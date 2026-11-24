//! Background evaluation. A worker thread owns the [`Evaluator`]; the UI never
//! blocks on it. The latest request wins and older ones are cancelled.
//!
//! The worker has two ways to make a frame. The CPU path evaluates the whole graph and
//! converts it to an RGBA8 picture (the reference, the fallback and the only path for export).
//! The GPU path keeps the expensive, non-pointwise part of the graph (the "base") on the CPU,
//! uploads it once and runs the trailing pointwise nodes plus the display transform on the GPU
//! into a texture that egui shows directly (see `gpu_session`).

use eframe::egui;
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Instant;

use rawgraph_core::{Graph, ImageBuf, ImageId, NodeId, PortType};
use rawgraph_engine::{CancelToken, EvalError, Evaluator, ImageProvider, Value};
use rawgraph_scope_views::{PrepareSettings, PreparedScopes};
use rawgraph_scopes::DisplayImage;

use crate::frames::OverlayFrame;
use crate::gpu_path::{BaseKey, Preloader, RenderPath, RendererMode, REASON_CROP_TOOL, REASON_MASK_OVERLAY, REASON_MASK_VIEW};
use crate::gpu_real::{GpuSetup, RealBackend};
use crate::gpu_session::{gpu_tone, Backend, Begin, DisplayArgs, GpuPicture, GpuSession, Preloaded, Shown};
use crate::session::ViewSettings;

/// What the app knows that decides between the CPU and the GPU path.
#[derive(Clone, Debug, Default)]
pub struct GpuHints {
    /// The CPU has to be used for this request (renderer setting, compare view) and why.
    pub cpu_reason: Option<String>,
    /// Changes when the files or metadata behind the image change.
    pub revision: u64,
    /// Preview size (long edge) the image is decoded at.
    pub preview: u32,
    /// Neighbours of the image in the filtered list, most useful first: after a GPU frame has
    /// been shown they are uploaded in the background so that stepping needs no upload.
    pub neighbours: Vec<ImageId>,
    /// Curve node whose input gets a luminance histogram (sent after the frame).
    pub histogram: Option<NodeId>,
    /// How to frame the overlay of a brush mask (its consumer and the crops in between).
    pub overlay_frame: Option<OverlayFrame>,
}

/// One evaluation job.
pub struct Request {
    pub generation: u64,
    pub graph: Graph,
    pub node: NodeId,
    /// Output port of `node` to show; `None` is the node's normal result (for a
    /// Split by Metadata: its whole input collection).
    pub port: Option<usize>,
    pub image: ImageId,
    pub view: ViewSettings,
    /// Settings of the visible scopes; `None` skips the scope analysis.
    pub scopes: Option<PrepareSettings>,
    /// Mask node whose mask is shown as a red overlay (view only).
    pub overlay: Option<NodeId>,
    pub hints: GpuHints,
    pub cancel: CancelToken,
}

/// A mask rendered as a red, partly transparent overlay (unmultiplied RGBA8).
pub struct Overlay {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>,
}

/// Red where the mask is above 0, stronger where it is higher (rubylith).
pub fn mask_overlay(mask: &rawgraph_core::MaskBuf) -> Overlay {
    let mut rgba8 = Vec::with_capacity(mask.data.len() * 4);
    for v in &mask.data {
        let v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
        let alpha = if v > 0.002 { (40.0 + 110.0 * v).round() as u8 } else { 0 };
        rgba8.extend_from_slice(&[255, 36, 36, alpha]);
    }
    Overlay { width: mask.width, height: mask.height, rgba8 }
}

/// A finished preview.
pub struct Frame {
    pub generation: u64,
    #[allow(dead_code)] // wave 2: scopes/overlays need to know what the frame shows
    pub image: ImageId,
    #[allow(dead_code)]
    pub node: NodeId,
    pub width: u32,
    pub height: u32,
    /// Display-referred sRGB, 8 bit, for the texture. Empty for a GPU frame (the picture is
    /// in the GPU texture `gpu` and never read back).
    pub rgba8: Vec<u8>,
    /// The picture of a GPU frame.
    pub gpu: Option<GpuPicture>,
    /// Which path made this frame, and why the CPU did when it did.
    pub path: RenderPath,
    /// One-time note for the status bar (the GPU path switched itself off).
    pub notice: Option<String>,
    /// Unclamped display image; wave 2 feeds it to the scopes. Empty for a GPU frame.
    #[allow(dead_code)]
    pub display: Arc<DisplayImage>,
    /// Scope pixels prepared on the worker (the UI only uploads them).
    pub prepared: Option<PreparedScopes>,
    /// Pixel step the scopes were read with (1 = every pixel).
    pub scope_step: usize,
    /// Worker time for analysis + preparation of the scopes.
    pub scopes_ms: f32,
    pub eval_ms: f32,
    pub display_ms: f32,
    pub hits: u64,
    pub misses: u64,
    /// Mask overlay for the viewer (not part of the image, scopes or export).
    pub overlay: Option<Overlay>,
}

/// The histogram of the picture entering a Curve node (see `histogram`).
pub struct HistogramReply {
    pub generation: u64,
    pub node: NodeId,
    pub image: ImageId,
    /// 256 heights, 0..1 (scaled to the 99th percentile).
    pub bins: Vec<f32>,
}

/// Exact scopes of the resting GPU picture (sent once after the requests stopped).
pub struct Refined {
    pub generation: u64,
    pub prepared: PreparedScopes,
    pub step: usize,
    pub scopes_ms: f32,
}

pub enum Reply {
    Frame(Box<Frame>),
    Error { generation: u64, message: String },
    /// The image is not part of the viewed node's collection (not an error).
    Excluded { generation: u64, message: String },
    /// Exact scopes for the frame of that generation.
    Refined(Box<Refined>),
    /// The GPU picture was written to a PNG (`--dump-frame`).
    Dumped { path: PathBuf, result: Result<(), String> },
    /// The histogram behind the curve editor, for the frame of that generation.
    Histogram(Box<HistogramReply>),
}

impl Reply {
    pub fn generation(&self) -> u64 {
        match self {
            Reply::Frame(f) => f.generation,
            Reply::Refined(r) => r.generation,
            Reply::Error { generation, .. } | Reply::Excluded { generation, .. } => *generation,
            Reply::Dumped { .. } => 0,
            Reply::Histogram(h) => h.generation,
        }
    }
}

/// Decides which replies are still worth showing: anything older than what is
/// already on screen is dropped.
#[derive(Debug, Default)]
pub struct FrameGate {
    submitted: u64,
    shown: u64,
}

impl FrameGate {
    pub fn next_generation(&mut self) -> u64 {
        self.submitted += 1;
        self.submitted
    }

    /// True if a reply with this generation should be applied.
    pub fn accept(&mut self, generation: u64) -> bool {
        if generation > self.shown {
            self.shown = generation;
            true
        } else {
            false
        }
    }

    /// True while the newest request has not been answered yet.
    pub fn pending(&self) -> bool {
        self.shown < self.submitted
    }

    pub fn shown(&self) -> u64 {
        self.shown
    }
}

// A request is one message in a long-lived channel; boxing it would only add an allocation per slider move.
#[allow(clippy::large_enum_variant)]
enum Msg {
    Render(Request),
    /// Write the GPU picture shown last to this PNG.
    Dump(PathBuf),
    Shutdown,
}

/// Tells the worker whether the decoded proxy of an image is in memory (cheap to preload).
pub type ReadyFn = Arc<dyn Fn(ImageId) -> bool + Send + Sync>;

/// What the app hands the worker to run the GPU path.
pub struct GpuConfig {
    pub setup: GpuSetup,
    pub mode: RendererMode,
    /// `None`: no background uploads of neighbour images.
    pub ready: Option<ReadyFn>,
}

pub struct Renderer {
    tx: Sender<Msg>,
    rx: Receiver<Reply>,
    gate: FrameGate,
    cancel: CancelToken,
    handle: Option<JoinHandle<()>>,
    /// Generation of the frame on screen, for the worker (it frees retired GPU targets with it).
    shown: Arc<AtomicU64>,
    /// Exact scopes that arrived for the frame on screen.
    refined: Option<Box<Refined>>,
    dumped: Option<(PathBuf, Result<(), String>)>,
    histogram: Option<Box<HistogramReply>>,
}

impl Renderer {
    /// A CPU-only renderer (compare view, tests).
    pub fn new(ctx: egui::Context, provider: Arc<dyn ImageProvider>) -> Self {
        Self::spawn::<RealBackend>(ctx, provider, None, "this view always uses the CPU".to_owned(), RendererMode::Cpu, None)
    }

    /// A renderer with the GPU path on the device of `config.setup`.
    pub fn with_gpu(ctx: egui::Context, provider: Arc<dyn ImageProvider>, config: GpuConfig) -> Self {
        match RealBackend::from_setup(&config.setup) {
            Some(backend) => {
                eprintln!("GPU preview path: {}", backend.summary());
                let session = GpuSession::new(backend, config.mode, None);
                Self::spawn(ctx, provider, Some(session), String::new(), config.mode, config.ready)
            }
            None => {
                let why = "eframe has no wgpu render state, so there is no GPU to use".to_owned();
                Self::spawn::<RealBackend>(ctx, provider, None, why, config.mode, None)
            }
        }
    }

    fn spawn<B: Backend + Send + 'static>(
        ctx: egui::Context,
        provider: Arc<dyn ImageProvider>,
        session: Option<GpuSession<B>>,
        no_gpu_reason: String,
        mode: RendererMode,
        ready: Option<ReadyFn>,
    ) -> Self
    where
        B::Image: Send,
        B::Class: Send,
    {
        let (tx, worker_rx) = channel::<Msg>();
        let (worker_tx, rx) = channel::<Reply>();
        let shown = Arc::new(AtomicU64::new(0));
        let shown_for_worker = shown.clone();
        let handle = std::thread::Builder::new()
            .name("rawgraph-render".into())
            .spawn(move || worker(worker_rx, worker_tx, provider, ctx, session, no_gpu_reason, mode, shown_for_worker, ready))
            .ok();
        Self { tx, rx, gate: FrameGate::default(), cancel: CancelToken::new(), handle, shown, refined: None, dumped: None, histogram: None }
    }

    /// Queues a CPU render (the compare view); any evaluation still running for an older request
    /// is cancelled. Returns the generation number.
    #[allow(clippy::too_many_arguments)]
    pub fn submit(&mut self, graph: Graph, node: NodeId, port: Option<usize>, image: ImageId, view: ViewSettings, scopes: Option<PrepareSettings>, overlay: Option<NodeId>) -> u64 {
        self.submit_with(graph, node, port, image, view, scopes, overlay, GpuHints { cpu_reason: Some("this view always uses the CPU".to_owned()), ..GpuHints::default() })
    }

    /// Like [`Renderer::submit`] with the information the GPU path needs.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_with(&mut self, graph: Graph, node: NodeId, port: Option<usize>, image: ImageId, view: ViewSettings, scopes: Option<PrepareSettings>, overlay: Option<NodeId>, hints: GpuHints) -> u64 {
        self.cancel.cancel();
        self.cancel = CancelToken::new();
        let generation = self.gate.next_generation();
        let request = Request { generation, graph, node, port, image, view, scopes, overlay, hints, cancel: self.cancel.clone() };
        // A dead worker is reported through `is_alive`; nothing to do here.
        let _ = self.tx.send(Msg::Render(request));
        generation
    }

    /// Asks the worker to write the GPU picture on screen to a PNG; the answer comes with
    /// [`Renderer::take_dumped`].
    pub fn dump_gpu_frame(&self, path: PathBuf) {
        let _ = self.tx.send(Msg::Dump(path));
    }

    /// Newest acceptable reply since the last call, if any.
    pub fn poll(&mut self) -> Option<Reply> {
        let mut latest = None;
        while let Ok(reply) = self.rx.try_recv() {
            match reply {
                Reply::Refined(r) => {
                    // Exact scopes only belong to the frame that is on screen.
                    if r.generation == self.gate.shown() {
                        self.refined = Some(r);
                    }
                }
                Reply::Dumped { path, result } => self.dumped = Some((path, result)),
                Reply::Histogram(h) => {
                    if h.generation == self.gate.shown() {
                        self.histogram = Some(h);
                    }
                }
                other => {
                    if self.gate.accept(other.generation()) {
                        self.shown.store(self.gate.shown(), Ordering::Relaxed);
                        self.refined = None;
                        latest = Some(other);
                    }
                }
            }
        }
        latest
    }

    /// The exact scopes that arrived after the frame on screen.
    pub fn take_refined(&mut self) -> Option<Box<Refined>> {
        self.refined.take().filter(|r| r.generation == self.gate.shown())
    }

    /// The curve histogram that arrived for the frame on screen.
    pub fn take_histogram(&mut self) -> Option<Box<HistogramReply>> {
        self.histogram.take().filter(|h| h.generation == self.gate.shown())
    }

    pub fn take_dumped(&mut self) -> Option<(PathBuf, Result<(), String>)> {
        self.dumped.take()
    }

    pub fn pending(&self) -> bool {
        self.gate.pending()
    }

    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// What the worker does next.
enum Next {
    Render(Box<Request>),
    Dump(PathBuf),
    /// Nothing arrived until the scope refinement was due.
    Refine,
    Shutdown,
}

/// Blocks for one message (until `deadline` when given), then drains the queue and keeps only
/// the newest render request. A dump request that follows stays queued in `stash`.
fn next_message(rx: &Receiver<Msg>, stash: &mut Option<Msg>, deadline: Option<Instant>) -> Next {
    let first = match stash.take() {
        Some(m) => m,
        None => match deadline {
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => return Next::Shutdown,
            },
            Some(t) => match rx.recv_timeout(t.saturating_duration_since(Instant::now())) {
                Ok(m) => m,
                Err(RecvTimeoutError::Timeout) => return Next::Refine,
                Err(RecvTimeoutError::Disconnected) => return Next::Shutdown,
            },
        },
    };
    match first {
        Msg::Shutdown => Next::Shutdown,
        Msg::Dump(path) => Next::Dump(path),
        Msg::Render(mut latest) => {
            while let Ok(next) = rx.try_recv() {
                match next {
                    Msg::Render(r) => latest = r,
                    Msg::Shutdown => return Next::Shutdown,
                    dump @ Msg::Dump(_) => {
                        *stash = Some(dump);
                        break;
                    }
                }
            }
            Next::Render(Box::new(latest))
        }
    }
}

/// Blocks for one message, then drains the queue and keeps only the newest render request.
/// Returns `None` on shutdown or a closed channel.
#[cfg(test)]
fn take_latest(rx: &Receiver<Msg>) -> Option<Request> {
    let mut stash = None;
    loop {
        match next_message(rx, &mut stash, None) {
            Next::Render(r) => return Some(*r),
            Next::Shutdown => return None,
            Next::Dump(_) | Next::Refine => {}
        }
    }
}

/// The scopes of the last GPU frame, so the exact reading can be made when things rest.
struct RestingScopes {
    generation: u64,
    settings: PrepareSettings,
}

#[allow(clippy::too_many_arguments)]
fn worker<B: Backend>(
    rx: Receiver<Msg>,
    tx: Sender<Reply>,
    provider: Arc<dyn ImageProvider>,
    ctx: egui::Context,
    mut gpu: Option<GpuSession<B>>,
    no_gpu_reason: String,
    mode: RendererMode,
    shown: Arc<AtomicU64>,
    ready: Option<ReadyFn>,
) {
    let mut evaluator = Evaluator::new();
    let mut stash = None;
    let mut resting: Option<RestingScopes> = None;
    let base_ms: Cell<Option<f32>> = Cell::new(None);
    let mut preloader = Preloader::default();
    let mut job: Option<PreloadJob> = None;
    loop {
        let refine = gpu.as_ref().and_then(|g| g.scopes.refine_due());
        let preload = preloader.due();
        let due = match (refine, preload) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        match next_message(&rx, &mut stash, due) {
            Next::Shutdown => return,
            Next::Render(request) => {
                // Whatever was still to be preloaded is abandoned: the request comes first.
                preloader.on_request();
                job = None;
                if request.cancel.is_cancelled() {
                    continue;
                }
                let env = Env { no_gpu_reason: &no_gpu_reason, mode, shown: shown.load(Ordering::Relaxed), base_ms: &base_ms };
                let reply = run_request(&mut evaluator, &request, provider.as_ref(), gpu.as_mut(), &env, &mut resting);
                if let Some(reply) = reply {
                    let gpu_frame = matches!(&reply, Reply::Frame(f) if f.path.is_gpu());
                    let hist = request.hints.histogram.filter(|_| matches!(&reply, Reply::Frame(_)));
                    if tx.send(reply).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                    if let Some(node) = hist {
                        if let Some(h) = histogram_reply(&mut evaluator, &request, node, provider.as_ref()) {
                            if tx.send(Reply::Histogram(Box::new(h))).is_err() {
                                return;
                            }
                            ctx.request_repaint();
                        }
                    }
                    if gpu_frame && ready.is_some() && !request.hints.neighbours.is_empty() {
                        preloader.on_gpu_frame(Instant::now(), &request.hints.neighbours);
                        job = Some(PreloadJob { graph: request.graph.clone(), node: request.node, image: request.image, hints: request.hints.clone(), cancel: request.cancel.clone() });
                    }
                }
            }
            Next::Dump(path) => {
                let result = match gpu.as_mut() {
                    Some(g) => dump_gpu_picture(g, &path),
                    None => Err("there is no GPU picture to write".to_owned()),
                };
                if tx.send(Reply::Dumped { path, result }).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            Next::Refine => {
                let now = Instant::now();
                let refine_now = gpu.as_ref().is_some_and(|g| g.scopes.refine_due().is_some_and(|t| now >= t));
                if !refine_now {
                    // Nothing else is waiting: upload one neighbour in the background (the next
                    // wake-up takes the following one, so a request is never kept waiting for more).
                    if let (Some(g), Some(j), Some(ready)) = (gpu.as_mut(), job.as_ref(), ready.as_ref()) {
                        if let Some(id) = preloader.take_next(now) {
                            let outcome = preload_step(&mut evaluator, g, j, id, provider.as_ref(), ready.as_ref(), base_ms.get());
                            match outcome {
                                PreloadOutcome::Abandoned => preloader.on_request(),
                                // The proxy is still being decoded: look again a little later.
                                PreloadOutcome::NotReady => preloader.defer(Instant::now(), id),
                                PreloadOutcome::Uploaded | PreloadOutcome::Skipped(_) => {}
                            }
                        }
                    }
                    continue;
                }
                let (Some(g), Some(rest)) = (gpu.as_mut(), resting.as_ref()) else { continue };
                if !g.scopes.take_refine(Instant::now()) {
                    continue;
                }
                let t = Instant::now();
                let step = rest.settings.step.max(1);
                if let Some(prepared) = read_scopes(g, &rest.settings, step, rest.generation) {
                    let refined = Refined { generation: rest.generation, prepared, step, scopes_ms: t.elapsed().as_secs_f32() * 1000.0 };
                    if tx.send(Reply::Refined(Box::new(refined))).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            }
        }
    }
}

fn dump_gpu_picture<B: Backend>(gpu: &mut GpuSession<B>, path: &std::path::Path) -> Result<(), String> {
    let (w, h) = gpu.current_size().ok_or("no GPU picture has been shown yet")?;
    let rgba = gpu.read_current().map_err(|e| e.to_string())?;
    write_png(path, w, h, &rgba)
}

/// Writes RGBA8 pixels as a PNG.
pub fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    image::save_buffer(path, rgba, width, height, image::ColorType::Rgba8).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads the scopes of the GPU picture at `step` and prepares them. `None` when the reading
/// failed; the picture itself is fine then.
fn read_scopes<B: Backend>(gpu: &mut GpuSession<B>, settings: &PrepareSettings, step: usize, generation: u64) -> Option<PreparedScopes> {
    match gpu.samples(sample_step(step) as u32) {
        Ok(samples) => {
            // The samples are the pixels `analyze` would visit at `step`: analyse them as they are.
            let mut request = settings.request();
            request.step = 1;
            let set = rawgraph_scopes::analyze(&samples, &request);
            Some(rawgraph_scope_views::prepare(&set, settings))
        }
        Err(e) => {
            eprintln!("GPU preview: scopes could not be read ({e}); the previous scopes stay");
            gpu.note_samples_error(generation);
            None
        }
    }
}

/// Per-request facts that are not part of the request.
struct Env<'a> {
    no_gpu_reason: &'a str,
    mode: RendererMode,
    /// Generation of the frame the UI shows (retired GPU targets are freed with it).
    shown: u64,
    /// How long the last base evaluation on the CPU took (decides whether preloading is cheap).
    base_ms: &'a Cell<Option<f32>>,
}

/// Why a request must use the CPU regardless of the GPU, if it must.
fn forced_cpu_reason(req: &Request) -> Option<String> {
    if let Some(r) = &req.hints.cpu_reason {
        return Some(r.clone());
    }
    if req.port.is_some() {
        return Some(REASON_CROP_TOOL.to_owned());
    }
    if req.overlay.is_some() {
        return Some(REASON_MASK_OVERLAY.to_owned());
    }
    if req.graph.node(req.node).is_some_and(|n| n.kind.output() == Some(PortType::Mask)) {
        return Some(REASON_MASK_VIEW.to_owned());
    }
    None
}

/// Turns an evaluation error into the reply the UI shows (`None`: cancelled, no reply).
fn error_reply(req: &Request, e: EvalError) -> Option<Reply> {
    match e {
        EvalError::Cancelled => None,
        EvalError::NotInCollection { node } => Some(Reply::Excluded { generation: req.generation, message: excluded_message(&req.graph, node) }),
        e => Some(Reply::Error { generation: req.generation, message: describe_error(&req.graph, &e) }),
    }
}

/// Evaluates one request. `None` means it was cancelled (no reply is sent).
fn run_request<B: Backend>(
    evaluator: &mut Evaluator,
    req: &Request,
    provider: &dyn ImageProvider,
    gpu: Option<&mut GpuSession<B>>,
    env: &Env<'_>,
    resting: &mut Option<RestingScopes>,
) -> Option<Reply> {
    evaluator.retain_image(req.image);
    let started = Instant::now();
    let forced = forced_cpu_reason(req);
    let (path, notice) = match (forced, gpu) {
        (Some(reason), g) => {
            if let Some(g) = g {
                g.scopes.on_request_without_scopes(Instant::now());
            }
            (RenderPath::cpu(reason), None)
        }
        (None, Some(g)) => match run_gpu(evaluator, req, provider, g, env, started, resting) {
            GpuStep::Done(reply) => return reply,
            GpuStep::Cpu { reason, notice } => {
                g.scopes.on_request_without_scopes(Instant::now());
                *resting = None;
                (RenderPath::fallback(reason), notice)
            }
        },
        (None, None) => {
            let reason = env.no_gpu_reason.to_owned();
            if env.mode == RendererMode::Gpu {
                return Some(Reply::Error { generation: req.generation, message: format!("GPU renderer required (--renderer gpu), but: {reason}") });
            }
            (RenderPath::fallback(reason), None)
        }
    };
    run_cpu(evaluator, req, provider, started, path, notice)
}

/// The GPU sampler agrees with the CPU `analyze` only for steps 1, 2 and 4.
fn sample_step(step: usize) -> usize {
    match step {
        0 | 1 => 1,
        2 => 2,
        _ => 4,
    }
}

enum GpuStep {
    /// The GPU path answered (a frame, an error, or cancelled).
    Done(Option<Reply>),
    /// Use the CPU for this frame.
    Cpu { reason: String, notice: Option<String> },
}

fn run_gpu<B: Backend>(
    evaluator: &mut Evaluator,
    req: &Request,
    provider: &dyn ImageProvider,
    gpu: &mut GpuSession<B>,
    env: &Env<'_>,
    started: Instant,
    resting: &mut Option<RestingScopes>,
) -> GpuStep {
    let plan = match evaluator.plan_tail(&req.graph, req.node, req.image, provider) {
        Ok(p) => p,
        Err(e) => return GpuStep::Done(error_reply(req, e)),
    };
    let key = BaseKey { image: req.image, fingerprint: plan.base_fingerprint, preview: req.hints.preview, revision: req.hints.revision };
    let base = match gpu.begin(&key, req.generation, env.shown) {
        Begin::Cpu { reason, notice } => return GpuStep::Cpu { reason, notice },
        Begin::HaveBase => None,
        Begin::NeedBase => {
            let t = Instant::now();
            match evaluator.eval_base(&req.graph, &plan, req.image, provider, &req.cancel) {
                Ok(b) => {
                    env.base_ms.set(Some(t.elapsed().as_secs_f32() * 1000.0));
                    Some(b)
                }
                Err(e) => return GpuStep::Done(error_reply(req, e)),
            }
        }
    };
    if req.cancel.is_cancelled() {
        return GpuStep::Done(None);
    }
    let eval_ms = started.elapsed().as_secs_f32() * 1000.0;
    let display = DisplayArgs { exposure_ev: req.view.exposure_ev, tone: gpu_tone(req.view.tone.tone_map()) };
    let t = Instant::now();
    let picture = match gpu.show(&key, base.as_deref(), &plan.ops, display, req.generation, env.shown) {
        Shown::Gpu(p) => p,
        Shown::Cpu { reason, notice } => return GpuStep::Cpu { reason, notice },
        Shown::Failed(message) => return GpuStep::Done(Some(Reply::Error { generation: req.generation, message })),
    };
    let display_ms = t.elapsed().as_secs_f32() * 1000.0;
    // Scopes: coarse while requests keep arriving, exact when they rest (see `ScopeScheduler`).
    let t = Instant::now();
    let now = Instant::now();
    let (prepared, scope_step) = match &req.scopes {
        Some(settings) => {
            let step = gpu.scopes.on_request(now, settings.step);
            *resting = Some(RestingScopes { generation: req.generation, settings: settings.clone() });
            (read_scopes(gpu, settings, step, req.generation), step)
        }
        None => {
            gpu.scopes.on_request_without_scopes(now);
            *resting = None;
            (None, 1)
        }
    };
    let scopes_ms = t.elapsed().as_secs_f32() * 1000.0;
    if req.cancel.is_cancelled() {
        return GpuStep::Done(None);
    }
    GpuStep::Done(Some(Reply::Frame(Box::new(Frame {
        generation: req.generation,
        image: req.image,
        node: req.node,
        width: picture.width,
        height: picture.height,
        rgba8: Vec::new(),
        gpu: Some(picture),
        path: RenderPath::Gpu,
        notice: None,
        display: Arc::new(DisplayImage { width: 0, height: 0, rgb: Vec::new() }),
        prepared,
        scope_step,
        scopes_ms,
        eval_ms,
        display_ms,
        hits: evaluator.hits(),
        misses: evaluator.misses(),
        overlay: None,
    }))))
}

/// What the worker remembers of the last GPU frame to preload the neighbours of its image.
pub struct PreloadJob {
    graph: Graph,
    node: NodeId,
    image: ImageId,
    hints: GpuHints,
    /// The token of that request: it is cancelled as soon as the UI submits a newer one.
    cancel: CancelToken,
}

/// What one background upload did.
#[derive(Debug, PartialEq, Eq)]
pub enum PreloadOutcome {
    Uploaded,
    /// The decoded proxy of the neighbour is not in memory yet: worth another look soon.
    NotReady,
    /// Nothing to do for this neighbour (already there, not allowed, off).
    Skipped(&'static str),
    /// A request arrived or the evaluation failed: stop preloading.
    Abandoned,
}

/// Evaluations of the base up to this cost count as cheap enough to be made for a neighbour.
const CHEAP_BASE_MS: f32 = 60.0;

/// The tail plan and the key of the base of `image` for the graph and node of `job`.
fn preload_key(evaluator: &Evaluator, job: &PreloadJob, image: ImageId, provider: &dyn ImageProvider) -> Option<(rawgraph_engine::TailPlan, BaseKey)> {
    let plan = evaluator.plan_tail(&job.graph, job.node, image, provider).ok()?;
    let key = BaseKey { image, fingerprint: plan.base_fingerprint, preview: job.hints.preview, revision: provider.revision(image) };
    Some((plan, key))
}

/// Uploads the base of neighbour `id` to the GPU if that is cheap and allowed. Never touches the
/// picture on screen; stops at once when the request it follows has been replaced.
pub fn preload_step<B: Backend>(
    evaluator: &mut Evaluator,
    gpu: &mut GpuSession<B>,
    job: &PreloadJob,
    id: ImageId,
    provider: &dyn ImageProvider,
    ready: &(dyn Fn(ImageId) -> bool + Send + Sync),
    last_base_ms: Option<f32>,
) -> PreloadOutcome {
    if job.cancel.is_cancelled() {
        return PreloadOutcome::Abandoned;
    }
    if !ready(id) {
        return PreloadOutcome::NotReady;
    }
    let Some((plan, key)) = preload_key(evaluator, job, id, provider) else {
        return PreloadOutcome::Skipped("the neighbour is not part of the viewed collection");
    };
    if !gpu.wants_preload(&key) {
        return PreloadOutcome::Skipped("not wanted");
    }
    // The base must be cheap: the decoded proxy itself, or something the CPU made quickly before.
    let is_source = job.graph.node(plan.base).is_some_and(|n| n.kind == rawgraph_core::NodeKind::Source);
    if !is_source && !last_base_ms.is_some_and(|ms| ms <= CHEAP_BASE_MS) {
        return PreloadOutcome::Skipped("the base needs more CPU work than a background upload may cost");
    }
    // Protected: the picture on screen and both neighbours.
    let mut protect = Vec::new();
    if let Some((_, k)) = preload_key(evaluator, job, job.image, provider) {
        protect.push(k);
    }
    for n in &job.hints.neighbours {
        if let Some((_, k)) = preload_key(evaluator, job, *n, provider) {
            protect.push(k);
        }
    }
    let base = match evaluator.eval_base(&job.graph, &plan, id, provider, &job.cancel) {
        Ok(b) => b,
        Err(_) => return PreloadOutcome::Abandoned,
    };
    if job.cancel.is_cancelled() {
        return PreloadOutcome::Abandoned;
    }
    match gpu.preload(&key, &base, &protect) {
        Preloaded::Uploaded => PreloadOutcome::Uploaded,
        Preloaded::AlreadyThere => PreloadOutcome::Skipped("already on the GPU"),
        Preloaded::Skipped(why) => PreloadOutcome::Skipped(why),
        Preloaded::Failed => PreloadOutcome::Skipped("the upload failed"),
    }
}

/// The histogram of the picture entering Curve node `node`, evaluated through the evaluator's
/// cache (cheap while the curve is dragged: nothing upstream changes).
fn histogram_reply(evaluator: &mut Evaluator, req: &Request, node: NodeId, provider: &dyn ImageProvider) -> Option<HistogramReply> {
    if req.cancel.is_cancelled() {
        return None;
    }
    let (up, port) = req.graph.input_edge(node, 0)?;
    let value = evaluator.eval_output(&req.graph, up, port, req.image, provider, &req.cancel).ok()?;
    let Value::Images(img) = value else { return None };
    if req.cancel.is_cancelled() {
        return None;
    }
    Some(HistogramReply { generation: req.generation, node, image: req.image, bins: crate::histogram::curve_histogram(&img) })
}

/// The CPU path: evaluates the whole graph and converts it to RGBA8.
fn run_cpu(evaluator: &mut Evaluator, req: &Request, provider: &dyn ImageProvider, started: Instant, path: RenderPath, notice: Option<String>) -> Option<Reply> {
    let result = match req.port {
        Some(port) => evaluator.eval_output(&req.graph, req.node, port, req.image, provider, &req.cancel),
        None => evaluator.eval(&req.graph, req.node, req.image, provider, &req.cancel),
    };
    let eval_ms = started.elapsed().as_secs_f32() * 1000.0;
    let value = match result {
        Ok(v) => v,
        Err(e) => return error_reply(req, e),
    };
    if req.cancel.is_cancelled() {
        return None;
    }
    let buf: Arc<ImageBuf> = match value {
        Value::Images(img) => img,
        Value::Mask(mask) => Arc::new(mask_to_image(&mask)),
    };
    let t = Instant::now();
    let (display, rgba8) = req.view.transform().apply_with_rgba8(&buf);
    let display_ms = t.elapsed().as_secs_f32() * 1000.0;
    let t = Instant::now();
    let prepared = req.scopes.as_ref().map(|settings| {
        let set = rawgraph_scopes::analyze(&display, &settings.request());
        rawgraph_scope_views::prepare(&set, settings)
    });
    let scopes_ms = t.elapsed().as_secs_f32() * 1000.0;
    let scope_step = req.scopes.as_ref().map_or(1, |s| s.step.max(1));
    // The overlay is best effort: a mask that cannot be computed just shows nothing.
    let overlay = req.overlay.and_then(|n| match &req.hints.overlay_frame {
        // A brush mask lives in the frame of the node that uses it; carry it through the crops
        // between that node and the picture shown.
        Some(frame) => {
            let source = provider.image(req.image).ok().map(|i| (i.width(), i.height()))?;
            crate::frames::brush_overlay_mask(&req.graph, n, frame, req.image, source).map(|m| mask_overlay(&m))
        }
        None => match evaluator.eval(&req.graph, n, req.image, provider, &req.cancel) {
            Ok(Value::Mask(m)) => Some(mask_overlay(&m)),
            _ => None,
        },
    });
    if req.cancel.is_cancelled() {
        return None;
    }
    Some(Reply::Frame(Box::new(Frame {
        generation: req.generation,
        image: req.image,
        node: req.node,
        width: display.width,
        height: display.height,
        rgba8,
        gpu: None,
        path,
        notice,
        display: Arc::new(display),
        prepared,
        scope_step,
        scopes_ms,
        eval_ms,
        display_ms,
        hits: evaluator.hits(),
        misses: evaluator.misses(),
        overlay,
    })))
}

/// Shows a mask as a grey image.
fn mask_to_image(mask: &rawgraph_core::MaskBuf) -> ImageBuf {
    let mut data = Vec::with_capacity(mask.data.len() * 4);
    for v in &mask.data {
        data.extend_from_slice(&[*v, *v, *v, 1.0]);
    }
    ImageBuf::from_rgba(mask.width, mask.height, data).unwrap_or_else(|| ImageBuf::new(1, 1))
}

/// Message for an image that a node's collection does not contain.
pub fn excluded_message(graph: &Graph, node: NodeId) -> String {
    let label = graph.node(node).map_or_else(|| format!("node {}", node.0), |n| n.kind.label().to_owned());
    format!("This image is not part of the collection at {label}")
}

/// Human-readable text for an evaluation error, using node labels.
pub fn describe_error(graph: &Graph, e: &EvalError) -> String {
    let label = |node: NodeId| graph.node(node).map_or_else(|| format!("Node {}", node.0), |n| n.kind.label().to_owned());
    let port_label = |node: NodeId, port: usize| {
        graph
            .node(node)
            .and_then(|n| n.kind.inputs().get(port))
            .map_or_else(|| format!("#{port}"), |s| s.label.to_owned())
    };
    match e {
        EvalError::MissingInput { node, port } => {
            format!("{}: input \"{}\" is not connected", label(*node), port_label(*node, *port))
        }
        EvalError::WrongType { node, port } => {
            format!("{}: input \"{}\" received the wrong type of data", label(*node), port_label(*node, *port))
        }
        EvalError::NotInCollection { node } => excluded_message(graph, *node),
        EvalError::UnknownNode(n) => format!("Node {} does not exist", n.0),
        EvalError::Provider(msg) if msg.starts_with("File not found") => msg.clone(),
        EvalError::Provider(msg) => format!("Could not load the image: {msg}"),
        EvalError::Cancelled => "Evaluation was cancelled".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::NodeKind;

    #[test]
    fn stale_replies_are_dropped() {
        let mut gate = FrameGate::default();
        let g1 = gate.next_generation();
        let g2 = gate.next_generation();
        let g3 = gate.next_generation();
        assert!(gate.pending());
        assert!(gate.accept(g2), "a newer reply is shown even if an even newer one is pending");
        assert!(gate.pending());
        assert!(!gate.accept(g1), "an older reply arriving late is dropped");
        assert!(!gate.accept(g2), "a duplicate is dropped");
        assert!(gate.accept(g3));
        assert!(!gate.pending());
    }

    fn mk(generation: u64) -> Msg {
        Msg::Render(Request {
            generation,
            graph: Graph::new(),
            node: NodeId(1),
            port: None,
            image: ImageId(1),
            view: ViewSettings::default(),
            scopes: None,
            overlay: None,
            hints: GpuHints::default(),
            cancel: CancelToken::new(),
        })
    }

    #[test]
    fn worker_keeps_only_the_newest_queued_request() {
        let (tx, rx) = channel::<Msg>();
        tx.send(mk(1)).unwrap();
        tx.send(mk(2)).unwrap();
        tx.send(mk(3)).unwrap();
        assert_eq!(take_latest(&rx).unwrap().generation, 3);
        tx.send(mk(4)).unwrap();
        tx.send(Msg::Shutdown).unwrap();
        assert!(take_latest(&rx).is_none());
    }

    #[test]
    fn a_dump_request_is_not_lost_behind_render_requests() {
        let (tx, rx) = channel::<Msg>();
        let mut stash = None;
        tx.send(mk(1)).unwrap();
        tx.send(mk(2)).unwrap();
        tx.send(Msg::Dump(PathBuf::from("x.png"))).unwrap();
        tx.send(mk(3)).unwrap();
        assert!(matches!(next_message(&rx, &mut stash, None), Next::Render(r) if r.generation == 2));
        assert!(matches!(next_message(&rx, &mut stash, None), Next::Dump(_)));
        assert!(matches!(next_message(&rx, &mut stash, None), Next::Render(r) if r.generation == 3));
    }

    #[test]
    fn the_worker_wakes_up_for_the_scope_refinement() {
        let (_tx, rx) = channel::<Msg>();
        let mut stash = None;
        let started = Instant::now();
        let due = started + std::time::Duration::from_millis(40);
        assert!(matches!(next_message(&rx, &mut stash, Some(due)), Next::Refine));
        assert!(started.elapsed() >= std::time::Duration::from_millis(35));
    }

    #[test]
    fn the_overlay_is_red_where_the_mask_is_above_zero() {
        let mask = rawgraph_core::MaskBuf { width: 4, height: 1, data: vec![0.0, 0.001, 0.5, 1.0] };
        let o = mask_overlay(&mask);
        assert_eq!((o.width, o.height, o.rgba8.len()), (4, 1, 16));
        let alpha = |i: usize| o.rgba8[i * 4 + 3];
        assert_eq!(alpha(0), 0, "mask 0 stays clear");
        assert_eq!(alpha(1), 0, "noise level stays clear");
        assert!(alpha(2) > 80 && alpha(3) > alpha(2), "stronger where the mask is higher");
        assert_eq!(&o.rgba8[8..11], &[255, 36, 36], "red");
        let bad = rawgraph_core::MaskBuf { width: 1, height: 1, data: vec![f32::NAN] };
        assert_eq!(mask_overlay(&bad).rgba8[3], 0, "NaN never shows");
    }

    #[test]
    fn excluded_images_get_a_neutral_message() {
        let mut g = Graph::new();
        let f = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        assert_eq!(excluded_message(&g, f), "This image is not part of the collection at Metadata Filter");
        assert_eq!(describe_error(&g, &EvalError::NotInCollection { node: f }), excluded_message(&g, f));
    }

    #[test]
    fn errors_use_node_and_port_labels() {
        let mut g = Graph::new();
        let blur = g.add_node(NodeKind::Blur, [0.0, 0.0]);
        let text = describe_error(&g, &EvalError::MissingInput { node: blur, port: 0 });
        assert_eq!(text, "Blur: input \"Images\" is not connected");
        assert_eq!(g.validate()[0].message, text, "same wording as Graph::validate");
    }

    #[test]
    fn renderer_evaluates_in_the_background() {
        use crate::session::demo_graph;
        let fp = Arc::new(crate::provider::FileProvider::new(1 << 30));
        fp.set_paths([(ImageId(1), std::path::PathBuf::from("demo:1"))]);
        fp.set_preview_size(1280);
        let provider: Arc<dyn ImageProvider> = fp;
        let mut r = Renderer::new(egui::Context::default(), provider);
        let g = demo_graph();
        let out = g.nodes().find(|n| n.kind == NodeKind::Output).unwrap().id;
        // Two quick requests: only the newest must be answered with a frame.
        r.submit(g.clone(), out, None, ImageId(1), ViewSettings::default(), None, None);
        let gen2 = r.submit(g, out, None, ImageId(1), ViewSettings::default(), Some(PrepareSettings::default()), None);
        let started = Instant::now();
        let mut got = None;
        while started.elapsed().as_secs() < 60 {
            if let Some(reply) = r.poll() {
                let done = reply.generation() == gen2;
                got = Some(reply);
                if done {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        match got {
            Some(Reply::Frame(f)) => {
                assert_eq!(f.generation, gen2);
                assert_eq!((f.width, f.height), (960, 640));
                assert_eq!(f.rgba8.len(), 960 * 640 * 4);
                assert!(f.prepared.is_some(), "scopes are prepared on the worker");
                assert_eq!(f.path.short(), "CPU");
            }
            Some(Reply::Error { message, .. } | Reply::Excluded { message, .. }) => panic!("unexpected error: {message}"),
            Some(_) => panic!("unexpected reply"),
            None => panic!("no frame arrived"),
        }
    }

    // ---- the GPU path of the worker, with a fake GPU

    use crate::gpu_session::fake::FakeBackend;

    fn demo_provider() -> Arc<dyn ImageProvider> {
        let fp = Arc::new(crate::provider::FileProvider::new(1 << 30));
        fp.set_paths([(ImageId(1), std::path::PathBuf::from("demo:1"))]);
        fp.set_preview_size(640);
        fp
    }

    fn request(g: &Graph, node: NodeId, generation: u64, ev: f32, scopes: bool) -> Request {
        request_for(g, node, ImageId(1), generation, ev, scopes)
    }

    fn request_for(g: &Graph, node: NodeId, image: ImageId, generation: u64, ev: f32, scopes: bool) -> Request {
        Request {
            generation,
            graph: g.clone(),
            node,
            port: None,
            image,
            view: ViewSettings { exposure_ev: ev, ..ViewSettings::default() },
            scopes: scopes.then(PrepareSettings::default),
            overlay: None,
            hints: GpuHints { cpu_reason: None, revision: 0, preview: 640, ..GpuHints::default() },
            cancel: CancelToken::new(),
        }
    }

    fn frame_of(reply: Option<Reply>) -> Box<Frame> {
        match reply {
            Some(Reply::Frame(f)) => f,
            Some(Reply::Error { message, .. } | Reply::Excluded { message, .. }) => panic!("error: {message}"),
            _ => panic!("no frame"),
        }
    }

    fn run(session: &mut Option<GpuSession<FakeBackend>>, mode: RendererMode, req: &Request, ev: &mut Evaluator, prov: &dyn ImageProvider) -> Option<Reply> {
        let base_ms = Cell::new(None);
        let env = Env { no_gpu_reason: "no GPU in this test", mode, shown: req.generation.saturating_sub(1), base_ms: &base_ms };
        run_request(ev, req, prov, session.as_mut(), &env, &mut None)
    }

    fn output_node(g: &Graph) -> NodeId {
        g.nodes().find(|n| n.kind == NodeKind::Output).unwrap().id
    }

    #[test]
    fn the_default_graph_is_shown_from_the_gpu_without_pixels_and_uploads_once() {
        let prov = demo_provider();
        let g = crate::session::demo_graph();
        let out = output_node(&g);
        let mut ev = Evaluator::new();
        let mut session = Some(GpuSession::new(FakeBackend::new(), RendererMode::Auto, None));
        let f1 = frame_of(run(&mut session, RendererMode::Auto, &request(&g, out, 1, 0.0, true), &mut ev, prov.as_ref()));
        assert!(f1.path.is_gpu());
        assert!(f1.rgba8.is_empty() && f1.gpu.is_some(), "no pixels are read back for the picture");
        assert!(f1.prepared.is_some());
        assert_eq!(f1.scope_step, 1, "the first request of a gesture is exact");
        // A slider drag: the tail changes, the base stays on the GPU.
        let f2 = frame_of(run(&mut session, RendererMode::Auto, &request(&g, out, 2, 0.5, true), &mut ev, prov.as_ref()));
        assert!(f2.path.is_gpu());
        assert_eq!(f2.scope_step, 2, "scopes read every 2nd pixel while requests keep arriving");
        let s = session.as_ref().unwrap();
        assert_eq!(s.stats.uploads, 1);
        assert_eq!(s.stats.renders, 2);
        assert!(s.scopes.refine_due().is_some(), "the exact reading is scheduled");
    }

    #[test]
    fn frames_that_need_cpu_pixels_say_why() {
        let prov = demo_provider();
        let g = crate::session::demo_graph();
        let out = output_node(&g);
        let mut ev = Evaluator::new();
        let mut session = Some(GpuSession::new(FakeBackend::new(), RendererMode::Auto, None));
        let mut r = request(&g, out, 1, 0.0, false);
        r.overlay = Some(out);
        let f = frame_of(run(&mut session, RendererMode::Auto, &r, &mut ev, prov.as_ref()));
        assert!(matches!(&f.path, RenderPath::Cpu { reason, .. } if reason.contains("overlay")), "{:?}", f.path);
        assert!(!f.rgba8.is_empty() && f.gpu.is_none());
        let mut r = request(&g, out, 2, 0.0, false);
        r.hints.cpu_reason = Some("the renderer is set to CPU only".into());
        let f = frame_of(run(&mut session, RendererMode::Auto, &r, &mut ev, prov.as_ref()));
        assert!(matches!(&f.path, RenderPath::Cpu { reason, .. } if reason.contains("CPU only")));
        assert_eq!(session.as_ref().unwrap().stats.uploads, 0, "the GPU was not touched");
        // The crop tool's input view and a mask node.
        let mut r = request(&g, out, 3, 0.0, false);
        r.port = Some(0);
        assert!(forced_cpu_reason(&r).is_some_and(|m| m.contains("crop")));
        let mut g2 = g.clone();
        let mask = g2.add_node(NodeKind::RadialMask, [0.0, 0.0]);
        assert!(forced_cpu_reason(&request(&g2, mask, 4, 0.0, false)).is_some_and(|m| m.contains("mask")));
        assert!(forced_cpu_reason(&request(&g, out, 5, 0.0, false)).is_none());
    }

    #[test]
    fn without_a_gpu_the_cpu_frame_carries_the_reason_and_gpu_only_mode_errors() {
        let prov = demo_provider();
        let g = crate::session::demo_graph();
        let out = output_node(&g);
        let mut ev = Evaluator::new();
        let mut none: Option<GpuSession<FakeBackend>> = None;
        let f = frame_of(run(&mut none, RendererMode::Auto, &request(&g, out, 1, 0.0, false), &mut ev, prov.as_ref()));
        assert!(matches!(&f.path, RenderPath::Cpu { reason, .. } if reason == "no GPU in this test"));
        let r = run(&mut none, RendererMode::Gpu, &request(&g, out, 2, 0.0, false), &mut ev, prov.as_ref());
        assert!(matches!(r, Some(Reply::Error { message, .. }) if message.contains("--renderer gpu")));
    }

    #[test]
    fn injected_out_of_memory_still_shows_a_picture_and_is_not_retried_for_the_same_image() {
        let prov = demo_provider();
        let g = crate::session::demo_graph();
        let out = output_node(&g);
        let mut ev = Evaluator::new();
        let mut backend = FakeBackend::new();
        backend.fail_upload = Some(rawgraph_gpu::GpuError::OutOfMemory("injected".into()));
        let mut session = Some(GpuSession::new(backend, RendererMode::Auto, None));
        for i in 1..=3u64 {
            let f = frame_of(run(&mut session, RendererMode::Auto, &request(&g, out, i, 0.1 * i as f32, true), &mut ev, prov.as_ref()));
            assert!(!f.rgba8.is_empty() && f.gpu.is_none(), "a picture is always there");
            assert!(f.prepared.is_some(), "the CPU frame has its scopes");
            assert!(matches!(&f.path, RenderPath::Cpu { reason, .. } if reason.contains("memory")), "{:?}", f.path);
        }
        assert_eq!(session.as_ref().unwrap().fallback.consecutive_oom(), 1, "one attempt for the image");
    }

    #[test]
    fn a_dumped_frame_is_a_png_with_the_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.png");
        let px: Vec<u8> = (0..32u8).map(|i| i.wrapping_mul(7)).collect();
        write_png(&path, 4, 2, &px).unwrap();
        let back = image::open(&path).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (4, 2));
        assert_eq!(back.as_raw(), &px);
        assert!(write_png(&dir.path().join("no/such/dir/x.png"), 1, 1, &[0, 0, 0, 255]).is_err());
    }

    // ---- neighbour preload, the histogram and the brush overlay of the worker

    /// As the app does it: the revision of the request is the provider's revision of the image.
    fn with_revision(mut r: Request, prov: &dyn ImageProvider) -> Request {
        r.hints.revision = prov.revision(r.image);
        r
    }

    fn three_images() -> Arc<dyn ImageProvider> {
        let fp = Arc::new(crate::provider::FileProvider::new(1 << 30));
        fp.set_paths((1..=3u64).map(|i| (ImageId(i), std::path::PathBuf::from(format!("demo:{i}")))));
        fp.set_preview_size(640);
        fp
    }

    /// The graph, a GPU session that has shown image 2, and the job a worker would keep for it.
    fn shown_image_two() -> (Graph, NodeId, Evaluator, GpuSession<FakeBackend>, PreloadJob, Arc<dyn ImageProvider>) {
        let prov = three_images();
        let g = crate::session::demo_graph_neutral();
        let out = output_node(&g);
        let mut ev = Evaluator::new();
        let mut session = Some(GpuSession::new(FakeBackend::new(), RendererMode::Auto, None));
        let req = with_revision(request_for(&g, out, ImageId(2), 1, 0.0, false), prov.as_ref());
        let f = frame_of(run(&mut session, RendererMode::Auto, &req, &mut ev, prov.as_ref()));
        assert!(f.path.is_gpu());
        let hints = GpuHints { preview: 640, neighbours: vec![ImageId(3), ImageId(1)], ..GpuHints::default() };
        let job = PreloadJob { graph: g.clone(), node: out, image: ImageId(2), hints, cancel: req.cancel.clone() };
        (g, out, ev, session.unwrap(), job, prov)
    }

    #[test]
    fn neighbours_are_uploaded_when_idle_and_stepping_to_one_needs_no_upload() {
        let (g, out, mut ev, mut gpu, job, prov) = shown_image_two();
        let ready = |_: ImageId| true;
        // The policy with an injected clock: not before the worker has been idle for a while.
        let t0 = Instant::now();
        let mut pre = Preloader::default();
        pre.on_gpu_frame(t0, &job.hints.neighbours);
        assert!(pre.take_next(t0 + std::time::Duration::from_millis(10)).is_none(), "not idle yet");
        let later = t0 + std::time::Duration::from_millis(200);
        while let Some(id) = pre.take_next(later) {
            let r = preload_step(&mut ev, &mut gpu, &job, id, prov.as_ref(), &ready, Some(5.0));
            assert_eq!(r, PreloadOutcome::Uploaded, "image {id:?}");
        }
        assert_eq!((gpu.stats.uploads, gpu.stats.preloads), (3, 2));
        // Step to the next image: the base is on the GPU, nothing is uploaded.
        let mut session = Some(gpu);
        let before = session.as_ref().unwrap().stats.uploads;
        let f = frame_of(run(&mut session, RendererMode::Auto, &with_revision(request_for(&g, out, ImageId(3), 2, 0.0, false), prov.as_ref()), &mut ev, prov.as_ref()));
        assert!(f.path.is_gpu());
        assert_eq!(session.as_ref().unwrap().stats.uploads, before, "stepping needs no upload");
        // And back: the other neighbour too.
        frame_of(run(&mut session, RendererMode::Auto, &with_revision(request_for(&g, out, ImageId(1), 3, 0.0, false), prov.as_ref()), &mut ev, prov.as_ref()));
        assert_eq!(session.as_ref().unwrap().stats.uploads, before);
    }

    #[test]
    fn a_new_request_abandons_the_preload_and_the_cancelled_token_stops_a_step() {
        let (_g, _out, mut ev, mut gpu, job, prov) = shown_image_two();
        let ready = |_: ImageId| true;
        // The UI submits a newer request: the token of the shown one is cancelled.
        job.cancel.cancel();
        let r = preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &ready, Some(5.0));
        assert_eq!(r, PreloadOutcome::Abandoned);
        assert_eq!(gpu.stats.preloads, 0, "nothing was uploaded");
        // The worker's policy drops the rest of the queue as soon as a request is taken.
        let t0 = Instant::now();
        let mut pre = Preloader::default();
        pre.on_gpu_frame(t0, &[ImageId(3), ImageId(1)]);
        pre.on_request();
        assert!(pre.take_next(t0 + std::time::Duration::from_secs(5)).is_none());
    }

    #[test]
    fn a_neighbour_without_a_decoded_proxy_or_with_an_expensive_base_is_not_preloaded() {
        let (_g, _out, mut ev, mut gpu, job, prov) = shown_image_two();
        let not_ready = |_: ImageId| false;
        assert_eq!(preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &not_ready, Some(5.0)), PreloadOutcome::NotReady);
        let ready = |_: ImageId| true;
        // The demo graph ends in a Blur: its base is not the decoded proxy, so the CPU would have to work.
        assert!(matches!(preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &ready, None), PreloadOutcome::Skipped(why) if why.contains("CPU work")));
        assert!(matches!(preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &ready, Some(400.0)), PreloadOutcome::Skipped(_)));
        assert_eq!(gpu.stats.preloads, 0);
        assert_eq!(preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &ready, Some(30.0)), PreloadOutcome::Uploaded);
        assert!(matches!(preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &ready, Some(30.0)), PreloadOutcome::Skipped(_)), "already there");
    }

    #[test]
    fn an_out_of_memory_failure_in_the_background_costs_nothing_on_screen() {
        let (g, out, mut ev, mut gpu, job, prov) = shown_image_two();
        let ready = |_: ImageId| true;
        gpu.backend_mut().fail_upload = Some(rawgraph_gpu::GpuError::OutOfMemory("injected".into()));
        assert!(matches!(preload_step(&mut ev, &mut gpu, &job, ImageId(3), prov.as_ref(), &ready, Some(5.0)), PreloadOutcome::Skipped(_)));
        gpu.backend_mut().fail_upload = None;
        assert!(matches!(preload_step(&mut ev, &mut gpu, &job, ImageId(1), prov.as_ref(), &ready, Some(5.0)), PreloadOutcome::Skipped(_)), "no further background uploads");
        assert_eq!(gpu.stats.preloads, 0);
        // The picture on screen keeps rendering from the GPU.
        let mut session = Some(gpu);
        let f = frame_of(run(&mut session, RendererMode::Auto, &with_revision(request_for(&g, out, ImageId(2), 2, 0.5, false), prov.as_ref()), &mut ev, prov.as_ref()));
        assert!(f.path.is_gpu());
    }

    #[test]
    fn the_curve_histogram_is_made_from_the_input_of_the_curve_node() {
        let prov = demo_provider();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let curve = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(src, curve, 0).unwrap();
        g.connect(curve, out, 0).unwrap();
        let mut ev = Evaluator::new();
        let req = request(&g, out, 4, 0.0, false);
        let h = histogram_reply(&mut ev, &req, curve, prov.as_ref()).expect("a histogram");
        assert_eq!((h.generation, h.node, h.image), (4, curve, ImageId(1)));
        assert_eq!(h.bins.len(), 256);
        assert!(h.bins.iter().all(|v| (0.0..=1.0).contains(v)) && h.bins.iter().any(|v| *v > 0.99), "scaled to the 99th percentile");
        // A curve without an input has nothing to show; a cancelled request makes no histogram.
        let lone = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        assert!(histogram_reply(&mut ev, &req, lone, prov.as_ref()).is_none());
        let cancelled = request(&g, out, 5, 0.0, false);
        cancelled.cancel.cancel();
        assert!(histogram_reply(&mut ev, &cancelled, curve, prov.as_ref()).is_none());
    }
}
