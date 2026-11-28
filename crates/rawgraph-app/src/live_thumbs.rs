//! Independent, low-priority gallery pipeline. Camera JPEGs remain the fast fallback.
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use half::f16;
use rawgraph_core::{CaptureInfo, Graph, ImageBuf, ImageId, LensProfile, NodeId, NodeKind};
use rawgraph_engine::{CancelToken, Evaluator, Value};
use rawgraph_engine::ImageProvider;
use rawgraph_project::ThumbCache;
use rayon::prelude::*;

use crate::gpu_session::{gpu_tone, point_ops};
use crate::provider::{FileProvider, LruBytes};
use crate::collection::Captures;
use crate::session::{Session, ViewSettings, ViewerTarget};

pub(crate) const DEFAULT_GIB: u32 = 8;
const EDGE: u32 = 512;
// Increment for any change to linear decode settings or binary representation.
const CACHE_KEY: u32 = (1 << 16) | EDGE;
const INTERVAL: Duration = Duration::from_millis(100);
const HEAVY_BUDGET: Duration = Duration::from_millis(40);
const BATCH: usize = 4;

/// Half the cores (2..=8) at below-normal priority: the viewer keeps the rest and wins ties.
fn worker_threads() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get() / 2).clamp(2, 8)
}
type Pixels = (u32, u32, Vec<u8>);
type Base = (Arc<ImageBuf>, (u32, u32));
type Stage = (NodeId, Option<usize>);
type TextureKey = (ImageId, Stage, u64);

fn encode(base: &Base) -> Vec<u8> {
    let (img, (w, h)) = base;
    let mut bytes = Vec::with_capacity(16 + img.data().len() * 2);
    for n in [img.width(), img.height(), *w, *h] { bytes.extend(n.to_le_bytes()); }
    for &v in img.data() { bytes.extend(f16::from_f32(v).to_le_bytes()); }
    bytes
}

fn decode(bytes: &[u8]) -> Option<Base> {
    let mut dims = bytes.get(..16)?.as_chunks::<4>().0.iter().map(|s| u32::from_le_bytes(*s));
    let (w, h, fw, fh) = (dims.next()?, dims.next()?, dims.next()?, dims.next()?);
    if w == 0 || h == 0 || fw == 0 || fh == 0 || w.max(h) > EDGE || bytes.len() != 16 + (w * h) as usize * 8 { return None; }
    let data = bytes[16..].as_chunks::<2>().0.iter().map(|&v| f16::from_le_bytes(v).to_f32()).collect();
    Some((Arc::new(ImageBuf::from_rgba(w, h, data)?), (fw, fh)))
}

#[derive(Clone, Hash)]
struct File {
    path: PathBuf,
    hash: String,
}

type BaseKey = (ImageId, u64);
type CatalogSnapshot = (u64, Arc<Vec<ImageId>>, Arc<HashMap<ImageId, File>>, Captures);
struct Bases {
    memory: LruBytes<BaseKey>,
    sizes: HashMap<BaseKey, (u32, u32)>,
}

struct ThumbProvider {
    live: Arc<FileProvider>,
    files: Arc<HashMap<ImageId, File>>,
    ids: Arc<Vec<ImageId>>,
    captures: Captures,
    disk: Option<Arc<ThumbCache>>,
    bases: Arc<Mutex<Bases>>,
    document: u64,
    dragging: bool,
}

impl ImageProvider for ThumbProvider {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        let key = (id, self.revision(id));
        if let Some(hit) = self.bases.lock().unwrap().memory.get(&key) { return Ok(hit); }
        if self.dragging { return Err("editable thumbnail base deferred until release".into()); }
        let file = self.files.get(&id).ok_or_else(|| format!("no thumbnail source for {}", id.0))?;
        let disk = self.disk.as_ref().filter(|_| !file.hash.is_empty());
        let base = if let Some(base) = disk.and_then(|d| d.get(&file.hash, CACHE_KEY)).and_then(|b| decode(&b)) {
            base
        } else {
            let base = self.live.thumbnail_base(id, &file.path, EDGE)?;
            if let Some(disk) = disk {
                let bytes = encode(&base);
                let _ = disk.put(&file.hash, CACHE_KEY, &bytes);
                // Cold and warm renders use exactly the same pixels.
                decode(&bytes).unwrap_or(base)
            } else { base }
        };
        let mut bases = self.bases.lock().unwrap();
        bases.sizes.insert(key, base.1);
        bases.memory.insert(key, base.0.clone());
        if bases.sizes.len() > 256 {
            let Bases { memory, sizes } = &mut *bases;
            sizes.retain(|key, _| memory.contains(key));
        }
        Ok(base.0)
    }

    fn revision(&self, id: ImageId) -> u64 {
        let mut h = DefaultHasher::new();
        self.document.hash(&mut h);
        self.files.get(&id).hash(&mut h);
        h.finish()
    }

    fn capture(&self, id: ImageId) -> Option<CaptureInfo> { self.captures.get(&id).cloned() }
    fn images(&self) -> Vec<ImageId> { self.ids.as_ref().clone() }
    fn full_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.bases.lock().unwrap().sizes.get(&(id, self.revision(id))).copied()
    }
    fn lens_profile(&self, id: ImageId) -> Option<LensProfile> { self.live.lens_profile(id) }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct State {
    images: u64,
    stage: Stage,
    view: ViewSettings,
    dragging: bool,
    gpu: bool,
}

struct Job {
    generation: u64,
    document: u64,
    state: State,
    graph: Arc<Graph>,
    files: Arc<HashMap<ImageId, File>>,
    ids: Arc<Vec<ImageId>>,
    captures: Captures,
    wanted: Vec<(ImageId, Stage)>,
    visible: usize,
    cancel: CancelToken,
    resident: HashMap<ImageId, TextureKey>,
}

struct Reply {
    generation: u64,
    id: ImageId,
    stale: bool,
    pixels: Option<Pixels>,
    key: TextureKey,
}

#[derive(Default)]
struct Mail {
    latest: Option<Job>,
    budget: Option<u32>,
    clear_disk: bool,
    closed: bool,
}

struct Photo {
    key: TextureKey,
    texture: egui::TextureHandle,
    used: u64,
}

pub(crate) struct LiveThumbs {
    mail: Arc<(Mutex<Mail>, Condvar)>,
    rx: mpsc::Receiver<Reply>,
    cancel: CancelToken,
    state: Option<State>,
    wanted_ids: Vec<ImageId>,
    visible: usize,
    catalog: Option<CatalogSnapshot>,
    graph: Option<Arc<Graph>>,
    textures: HashMap<ImageId, Photo>,
    stale: HashSet<ImageId>,
    generation: u64,
    document: u64,
    tick: u64,
    last: Option<Instant>,
    budget: u32,
    /// Only with a real window device: headless runs (tests, capture) stay on the CPU, like the viewer.
    gpu_allowed: bool,
}

impl LiveThumbs {
    pub(crate) fn new(ctx: egui::Context, provider: Arc<FileProvider>, path: PathBuf, budget: u32) -> Self {
        let mail = Arc::new((Mutex::new(Mail::default()), Condvar::new()));
        let (tx, rx) = mpsc::channel();
        let worker_mail = mail.clone();
        let budget = budget.min(128);
        std::thread::Builder::new().name("rawgraph-gallery".into()).spawn(move || {
            lower_priority();
            run_worker(&worker_mail, &ctx, &provider, path, budget, &tx);
        }).expect("gallery worker");
        Self { mail, rx, cancel: CancelToken::new(), state: None, wanted_ids: Vec::new(), visible: 0, catalog: None, graph: None,
            textures: HashMap::new(), stale: HashSet::new(), generation: 0, document: 0, tick: 0, last: None, budget, gpu_allowed: false }
    }

    pub(crate) fn budget(&self) -> u32 { self.budget }

    pub(crate) fn allow_gpu(&mut self, allowed: bool) { self.gpu_allowed = allowed; }

    pub(crate) fn set_budget(&mut self, budget: u32) {
        self.budget = budget.min(128);
        self.mail.0.lock().unwrap().budget = Some(self.budget);
        self.mail.1.notify_one();
    }

    pub(crate) fn clear_disk(&self) {
        self.mail.0.lock().unwrap().clear_disk = true;
        self.mail.1.notify_one();
    }

    pub(crate) fn reset(&mut self) {
        self.suspend();
        self.document += 1;
        self.graph = None;
        self.catalog = None;
        self.textures.clear();
        self.stale.clear();
    }

    /// Forgets the edited gallery picture of one image (it is rendered again when wanted).
    pub(crate) fn forget(&mut self, id: ImageId) {
        self.textures.remove(&id);
    }

    pub(crate) fn suspend(&mut self) {
        self.cancel.cancel();
        self.generation += 1;
        self.state = None;
        self.wanted_ids.clear();
    }

    pub(crate) fn texture(&mut self, id: ImageId) -> Option<egui::TextureHandle> {
        self.tick += 1;
        self.textures.get_mut(&id).map(|p| { p.used = self.tick; p.texture.clone() })
    }

    pub(crate) fn stale(&self, id: ImageId) -> bool { self.stale.contains(&id) }

    /// The edited gallery picture of `id` for the current graph and view, if one is shown.
    pub(crate) fn current(&mut self, id: ImageId) -> Option<egui::TextureHandle> {
        if self.stale.contains(&id) || self.state.is_none() { return None; }
        self.texture(id)
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        // Bound per-frame uploads, leaving the viewer and input processing first claim on the frame.
        for reply in self.rx.try_iter().take(BATCH) {
            if reply.generation != self.generation { continue; }
            if reply.stale { self.stale.insert(reply.id); } else { self.stale.remove(&reply.id); }
            if let Some((w, h, rgba)) = reply.pixels {
                let image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                if let Some(photo) = self.textures.get_mut(&reply.id) {
                    photo.texture.set(image, egui::TextureOptions::LINEAR);
                    photo.used = self.tick;
                    photo.key = reply.key;
                } else {
                    let texture = ctx.load_texture(format!("live thumbnail {}", reply.id.0), image, egui::TextureOptions::LINEAR);
                    self.textures.insert(reply.id, Photo { texture, used: self.tick, key: reply.key });
                }
            }
            ctx.request_repaint();
        }
        if self.textures.len() > 150 {
            let mut old: Vec<_> = self.textures.iter().filter(|(id, _)| !self.wanted_ids.contains(id)).map(|(&id, p)| (p.used, id)).collect();
            old.sort_unstable();
            for (_, id) in old.into_iter().take(self.textures.len() - 150) { self.textures.remove(&id); self.stale.remove(&id); }
        }
    }

    pub(crate) fn request(&mut self, ctx: &egui::Context, session: &mut Session, target: ViewerTarget, wanted: (&[ImageId], usize), gpu: bool) {
        let (wanted, visible) = wanted;
        let Some(stage) = session.render_viewer_target(target) else { self.suspend(); return; };
        let dragging = ctx.input(|i| i.pointer.primary_down());
        let state = State { images: session.images_gen, stage, view: session.view, dragging, gpu: gpu && self.gpu_allowed };
        let graph_matches = self.graph.as_ref().is_some_and(|g| **g == session.project.graph);
        if self.state == Some(state) && graph_matches && self.wanted_ids == wanted && self.visible == visible { return; }
        let now = Instant::now();
        if let Some(wait) = throttled(dragging, self.last, now) {
            ctx.request_repaint_after(wait);
            return;
        }
        self.last = Some(now);
        self.state = Some(state);
        self.wanted_ids = wanted.to_vec();
        self.visible = visible;
        let (ids, files, captures) = if let Some((_, ids, files, captures)) = self.catalog.as_ref().filter(|(rev, ..)| *rev == state.images) {
            (ids.clone(), files.clone(), captures.clone())
        } else {
            let (ids, captures) = session.id_captures();
            let files = Arc::new(ids.iter().filter_map(|&id| session.entry(id).filter(|e| !e.missing).map(|e| (id, File { path: e.path.clone(), hash: e.content_hash.clone() }))).collect::<HashMap<_, _>>());
            self.catalog = Some((state.images, ids.clone(), files.clone(), captures.clone()));
            (ids, files, captures)
        };
        if !graph_matches { self.graph = Some(Arc::new(session.project.graph.clone())); }
        let graph = self.graph.as_ref().unwrap().clone();
        // Crop can route different collection members through different ports; never use the
        // pinned image to choose the input wire for every other image.
        let wanted: Vec<_> = wanted.iter().map(|&id| {
            let stage = if stage.1.is_some() { session.render_viewer_target(ViewerTarget { image: Some(id), ..target }).unwrap_or(stage) } else { stage };
            (id, stage)
        }).collect();
        self.cancel.cancel();
        self.cancel = CancelToken::new();
        self.generation += 1;
        self.mail.0.lock().unwrap().latest = Some(Job { generation: self.generation, document: self.document, state, graph, files, ids, captures, wanted, visible, cancel: self.cancel.clone(), resident: self.textures.iter().map(|(&id, p)| (id, p.key)).collect() });
        self.mail.1.notify_one();
    }
}

impl Drop for LiveThumbs {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.mail.0.lock().unwrap().closed = true;
        self.mail.1.notify_one();
    }
}

fn throttled(dragging: bool, last: Option<Instant>, now: Instant) -> Option<Duration> {
    if dragging { last.and_then(|t| INTERVAL.checked_sub(now.duration_since(t))).filter(|d| !d.is_zero()) } else { None }
}

fn display_key(evaluator: &Evaluator, job: &Job, p: &dyn ImageProvider, id: ImageId, stage: Stage) -> Option<TextureKey> {
    let fp = evaluator.fingerprint(&job.graph, stage.0, id, p).ok()?;
    let mut hash = DefaultHasher::new();
    fp.hash(&mut hash);
    job.state.view.exposure_ev.to_bits().hash(&mut hash);
    (job.state.view.tone.tone_map() as u8).hash(&mut hash);
    Some((id, stage, hash.finish()))
}

fn lower_priority() {
    #[cfg(windows)]
    // The pseudo-handle identifies only this background thread.
    unsafe {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" { fn nice(increment: std::ffi::c_int) -> std::ffi::c_int; }
        // Linux nice is per-thread, and raising the nice value requires no privilege.
        unsafe { nice(8); }
    }
}

fn heavy_graph(graph: &Graph, node: NodeId) -> bool {
    let mut stack = vec![node];
    let mut seen = HashSet::new();
    while let Some(node) = stack.pop() {
        if !seen.insert(node) { continue; }
        if graph.node(node).is_some_and(|n| matches!(n.kind, NodeKind::DepthMap | NodeKind::PixelSort)) { return true; }
        stack.extend(graph.edges().iter().filter(|e| e.to == node).map(|e| e.from));
    }
    false
}

fn cpu(evaluator: &mut Evaluator, job: &Job, id: ImageId, (node, port): Stage, p: &dyn ImageProvider) -> Option<Pixels> {
    let Value::Images(buf) = match port {
        Some(port) => evaluator.eval_output(&job.graph, node, port, id, p, &job.cancel),
        None => evaluator.eval(&job.graph, node, id, p, &job.cancel),
    }.ok()?;
    if job.cancel.is_cancelled() { return None; }
    let downscaled;
    let buf = if buf.width().max(buf.height()) > EDGE { downscaled = buf.downscale_box(EDGE); &downscaled } else { &buf };
    let t = job.state.view.transform();
    let pixels = if let Some(n) = job.graph.node(node).filter(|n| n.kind == NodeKind::Export) {
        crate::render::codec_preview(buf, &rawgraph_core::export_spec(n, id), t).ok()??.image.to_rgba8()
    } else { t.apply_with_rgba8(buf).1 };
    Some((buf.width(), buf.height(), pixels))
}

struct GpuBatch {
    ctx: rawgraph_gpu::GpuContext,
    bases: crate::gpu_path::Lru<TextureKey, rawgraph_gpu::GpuImage>,
}

impl GpuBatch {
    fn new() -> Result<Self, rawgraph_gpu::GpuError> {
        Ok(Self { ctx: rawgraph_gpu::GpuContext::new()?, bases: crate::gpu_path::Lru::new(24) })
    }

    fn render(&mut self, job: &Job, p: &dyn ImageProvider, work: &[(ImageId, Stage, TextureKey)], eval: &mut Evaluator) -> Vec<Option<Pixels>> {
        use rawgraph_gpu::{render_into, DisplayTarget, GpuImage};
        let mut targets = Vec::new();
        for &(id, (node, port), _) in work {
            let mut render = || -> Option<DisplayTarget> {
                if job.cancel.is_cancelled() || port.is_some() || job.graph.node(node).is_some_and(|n| n.kind == NodeKind::Export) { return None; }
                let plan = eval.plan_tail(&job.graph, node, id, p).ok()?;
                if plan.ops.is_empty() { return None; }
                let key = (id, (plan.base, Some(plan.base_port)), plan.base_fingerprint);
                if !self.bases.contains(&key) {
                    let img = eval.eval_base(&job.graph, &plan, id, p, &job.cancel).ok()?;
                    if img.width().max(img.height()) > EDGE || job.cancel.is_cancelled() { return None; }
                    self.bases.insert(key, GpuImage::upload(&self.ctx, &img).ok()?);
                }
                let base = self.bases.get_mut(&key)?;
                let (w, h) = base.size();
                let scratch = GpuImage::new_empty(&self.ctx, w, h).ok()?;
                let target = DisplayTarget::new(&self.ctx, w, h).ok()?;
                let view = job.state.view;
                render_into(&self.ctx, base, &scratch, &target, &point_ops(&plan.ops), view.exposure_ev, gpu_tone(view.tone.tone_map())).ok()?;
                Some(target)
            };
            targets.push(render());
        }
        if job.cancel.is_cancelled() { return work.iter().map(|_| None).collect(); }
        let refs: Vec<_> = targets.iter().flatten().collect();
        let mut rgba = rawgraph_gpu::read_display_batch(&self.ctx, &refs).unwrap_or_default().into_iter();
        targets.into_iter().map(|target| {
            let (w, h) = target?.size();
            Some((w, h, rgba.next()?))
        }).collect()
    }
}

fn run_worker(mail: &(Mutex<Mail>, Condvar), ctx: &egui::Context, live: &Arc<FileProvider>, path: PathBuf, budget: u32, tx: &mpsc::Sender<Reply>) {
    let disk = ThumbCache::new(&path, u64::from(budget) << 30).ok().map(Arc::new);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(worker_threads()).start_handler(|_| lower_priority()).build().expect("gallery rayon pool");
    let mut bases = Arc::new(Mutex::new(Bases { memory: LruBytes::new(160 << 20), sizes: HashMap::new() }));
    let mut gpu = None;
    let mut gpu_attempted = false;
    let mut slow = HashSet::new();
    let mut document = None;
    loop {
        let (job, budget, clear_disk) = {
            let mut m = mail.0.lock().unwrap();
            while m.latest.is_none() && m.budget.is_none() && !m.clear_disk && !m.closed { m = mail.1.wait(m).unwrap(); }
            if m.closed { return; }
            (m.latest.take(), m.budget.take(), std::mem::take(&mut m.clear_disk))
        };
        if let (Some(d), Some(b)) = (&disk, budget) { d.set_budget(u64::from(b) << 30); }
        if let Some(d) = disk.as_ref().filter(|_| clear_disk) { d.clear(); }
        let Some(job) = job else { continue; };
        if document != Some(job.document) {
            document = Some(job.document);
            slow.clear();
            bases = Arc::new(Mutex::new(Bases { memory: LruBytes::new(160 << 20), sizes: HashMap::new() }));
        }
        let p = ThumbProvider { live: live.clone(), files: job.files.clone(), ids: job.ids.clone(), captures: job.captures.clone(), disk: disk.clone(), bases: bases.clone(), document: job.document, dragging: job.state.dragging };
        let mut eval = Evaluator::new();
        eval.set_view_tone(job.state.view.tone.tone_map());
        let mut pending = Vec::new();
        let wanted = if job.state.dragging { &job.wanted[..job.visible] } else { &job.wanted };
        for &(id, stage @ (node, _)) in wanted {
            if job.cancel.is_cancelled() { break; }
            let Some(key) = display_key(&eval, &job, &p, id, stage) else { continue; };
            if job.resident.get(&id) == Some(&key) {
                let _ = tx.send(Reply { generation: job.generation, id, stale: false, pixels: None, key });
                ctx.request_repaint();
                continue;
            }
            if job.state.dragging && (slow.contains(&stage) || heavy_graph(&job.graph, node)) {
                let _ = tx.send(Reply { generation: job.generation, id, stale: true, pixels: None, key });
                ctx.request_repaint();
                continue;
            }
            pending.push((id, stage, key));
        }
        // Lazy GPU initialization, on a separate queue, never while the first interaction is active.
        if job.state.gpu && !gpu_attempted && !job.state.dragging && !pending.is_empty() && !job.cancel.is_cancelled() {
            gpu_attempted = true;
            gpu = GpuBatch::new().ok();
        }
        let split = pending.iter().take_while(|(id, _, _)| job.wanted.iter().take(job.visible).any(|(visible, _)| visible == id)).count();
        let (visible, nearby) = pending.split_at(split);
        for work in visible.chunks(BATCH).chain(nearby.chunks(BATCH)) {
            // The viewer's first decode of a new photo has priority over the gallery.
            live.yield_to_foreground(|| job.cancel.is_cancelled());
            if job.cancel.is_cancelled() { break; }
            // RAW/cache I/O is not graph latency. Warm the visible sources first so a
            // cold decode does not permanently classify Develop as a heavy stage.
            if !job.state.dragging {
                pool.install(|| work.par_iter().for_each(|&(id, _, _)| {
                    if !job.cancel.is_cancelled() && p.files.contains_key(&id) { let _ = p.image(id); }
                }));
            }
            if job.cancel.is_cancelled() { break; }
            let started = Instant::now();
            let mut gpu_pixels = if let Some(gpu) = gpu.as_mut().filter(|_| job.state.gpu) {
                pool.install(|| gpu.render(&job, &p, work, &mut eval))
            } else { work.iter().map(|_| None).collect() };
            let gpu_over_budget = started.elapsed() > HEAVY_BUDGET * work.len() as u32;
            let results: Vec<_> = pool.install(|| work.par_iter().zip(gpu_pixels.par_iter_mut()).map(|(&(id, stage, key), gpu)| {
                let start = Instant::now();
                let used_gpu = gpu.is_some();
                let pixels = gpu.take().or_else(|| {
                    if job.cancel.is_cancelled() { return None; }
                    let mut eval = Evaluator::new();
                    eval.set_view_tone(job.state.view.tone.tone_map());
                    cpu(&mut eval, &job, id, stage, &p)
                });
                (id, stage, key, pixels, if used_gpu { gpu_over_budget } else { start.elapsed() > HEAVY_BUDGET })
            }).collect());
            for (id, stage, key, pixels, over_budget) in results {
                if job.cancel.is_cancelled() { break; }
                if over_budget { slow.insert(stage); }
                let stale = pixels.is_none();
                let key = display_key(&eval, &job, &p, id, stage).unwrap_or(key);
                let _ = tx.send(Reply { generation: job.generation, id, stale, pixels, key });
            }
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::Mode;

    struct Synthetic {
        source: Arc<ImageBuf>,
        ids: Vec<ImageId>,
    }
    impl ImageProvider for Synthetic {
        fn images(&self) -> Vec<ImageId> { self.ids.clone() }
        fn image(&self, _: ImageId) -> Result<Arc<ImageBuf>, String> { Ok(self.source.clone()) }
    }

    fn fixture(n: u64, size: u32) -> (Synthetic, Job) {
        let mut source = ImageBuf::new(size, size);
        for (i, p) in source.data_mut().as_chunks_mut::<4>().0.iter_mut().enumerate() {
            *p = [i as f32 / ((size * size) as f32), 0.2, 0.1, 1.0];
        }
        let mut graph = Graph::new();
        let input = graph.add_node(NodeKind::Source, [0.0; 2]);
        let develop = graph.add_node(NodeKind::Develop, [0.0; 2]);
        graph.connect(input, develop, 0).unwrap();
        let stage = (develop, None);
        let ids: Vec<_> = (1..=n).map(ImageId).collect();
        let job = Job { generation: 1, document: 0, state: State { images: 0, stage, view: ViewSettings::default(), dragging: false, gpu: false },
            graph: Arc::new(graph), files: Arc::default(), ids: Arc::new(ids.clone()), captures: Arc::default(),
            wanted: ids.iter().map(|&id| (id, stage)).collect(), visible: ids.len(), cancel: CancelToken::new(), resident: HashMap::new() };
        (Synthetic { source: Arc::new(source), ids }, job)
    }

    fn set_ev(job: &mut Job, id: ImageId, ev: f32, mode: Mode) {
        let node = Arc::make_mut(&mut job.graph).node_mut(job.state.stage.0).unwrap();
        node.mode = mode;
        assert!(node.set_param("exposure", id, ev));
    }

    #[test]
    fn gallery_fingerprints_follow_group_and_individual_exposure() {
        let (p, mut job) = fixture(2, 16);
        let stage = job.state.stage;
        let keys = |job: &Job| p.ids.iter().map(|&id| display_key(&Evaluator::new(), job, &p, id, stage).unwrap()).collect::<Vec<_>>();
        let initial = keys(&job);
        set_ev(&mut job, p.ids[0], 1.0, Mode::All);
        let group = keys(&job);
        assert_ne!(initial[0], group[0]);
        assert_ne!(initial[1], group[1]);
        set_ev(&mut job, p.ids[0], 2.0, Mode::Individual);
        let single = keys(&job);
        assert_ne!(group[0], single[0]);
        assert_eq!(group[1], single[1]);
        let bright = cpu(&mut Evaluator::new(), &job, p.ids[0], stage, &p).unwrap();
        let other = cpu(&mut Evaluator::new(), &job, p.ids[1], stage, &p).unwrap();
        assert_ne!(bright.2, other.2);
    }

    #[test]
    fn gallery_base_key_format_and_oldest_first_budget() {
        let (p, _) = fixture(1, 16);
        let mut src = (*p.source).clone();
        src.data_mut()[..4].copy_from_slice(&[-0.25, 4.5, 0.5, 1.0]);
        let bytes = encode(&(Arc::new(src), (3000, 2000)));
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.1, (3000, 2000));
        assert_eq!(&decoded.0.data()[..4], &[-0.25, 4.5, 0.5, 1.0]);
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::new(dir.path(), (bytes.len() * 2) as u64).unwrap();
        cache.put("ab", CACHE_KEY, &bytes).unwrap();
        cache.put("cd", CACHE_KEY, &bytes).unwrap();
        assert!(cache.get("ab", CACHE_KEY + (1 << 16)).is_none());
        assert!(cache.get("ef", CACHE_KEY).is_none());
        for (hash, sec) in [("ab", 10), ("cd", 11)] {
            let path = dir.path().join(format!("{hash}_{CACHE_KEY}.thumb"));
            std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(sec)).unwrap();
        }
        cache.set_budget(bytes.len() as u64);
        assert!(cache.get("ab", CACHE_KEY).is_none());
        assert_eq!(cache.get("cd", CACHE_KEY), Some(bytes.clone()));
        cache.set_budget(0);
        cache.put("ab", CACHE_KEY, &bytes).unwrap();
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn gallery_drag_throttle_and_heavy_detection() {
        let now = Instant::now();
        assert_eq!(throttled(true, Some(now - Duration::from_millis(30)), now), Some(Duration::from_millis(70)));
        assert_eq!(throttled(false, Some(now), now), None);
        let (_, mut job) = fixture(1, 16);
        let graph = Arc::make_mut(&mut job.graph);
        let sort = graph.add_node(NodeKind::PixelSort, [0.0; 2]);
        graph.connect(job.state.stage.0, sort, 0).unwrap();
        assert!(heavy_graph(graph, sort));
        assert!(!heavy_graph(graph, job.state.stage.0));
    }

    #[test]
    fn gallery_keeps_texture_for_stale_and_rejects_superseded_replies() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut gallery = LiveThumbs::new(ctx.clone(), Arc::new(FileProvider::new(0)), dir.path().to_owned(), 0);
        let (tx, rx) = mpsc::channel();
        gallery.rx = rx;
        let (a, b) = (ImageId(1), ImageId(2));
        let stage = (NodeId(1), None);
        let send = |generation, id, stale, pixels| tx.send(Reply { generation, id, stale, pixels, key: (id, stage, generation) }).unwrap();
        send(0, a, false, Some((1, 1, vec![0, 0, 0, 255])));
        gallery.poll(&ctx);
        let tex = gallery.texture(a).unwrap().id();
        gallery.generation = 1;
        send(0, b, false, Some((1, 1, vec![255, 0, 0, 255])));
        send(1, a, true, None);
        gallery.poll(&ctx);
        assert_eq!(gallery.texture(a).unwrap().id(), tex);
        assert!(gallery.stale(a));
        assert!(gallery.texture(b).is_none());
        send(1, a, false, Some((1, 1, vec![255, 0, 0, 255])));
        gallery.poll(&ctx);
        assert_eq!(gallery.texture(a).unwrap().id(), tex);
        assert!(!gallery.stale(a));
    }

    #[test]
    fn gallery_renders_every_images_own_pixels_under_a_primary_image_pin() {
        let dir = tempfile::tempdir().unwrap();
        let live = Arc::new(FileProvider::new(0));
        let mut session = Session::demo(live.clone());
        let ctx = egui::Context::default();
        let mut gallery = LiveThumbs::new(ctx.clone(), live, dir.path().to_owned(), 0);
        let wanted = session.visible().to_vec();
        let develop = session.project.graph.nodes().find(|n| n.kind == NodeKind::Develop).unwrap().id;
        let target = ViewerTarget { node: Some(develop), image: Some(wanted[0]), image_pinned: true };
        let epoch = |gallery: &mut LiveThumbs, session: &mut Session| {
            gallery.request(&ctx, session, target, (&wanted, wanted.len()), false);
            let timeout = Instant::now() + Duration::from_secs(15);
            let mut out = HashMap::new();
            while out.len() < wanted.len() {
                let reply = gallery.rx.recv_timeout(timeout.saturating_duration_since(Instant::now())).unwrap();
                if reply.generation == gallery.generation { out.insert(reply.id, reply); }
            }
            out
        };
        let original = epoch(&mut gallery, &mut session);
        let node = session.project.graph.node_mut(develop).unwrap();
        node.mode = Mode::All;
        assert!(node.set_param("exposure", wanted[0], 2.0));
        let changed = epoch(&mut gallery, &mut session);
        for id in &wanted {
            assert_eq!(changed[id].key.1, (develop, None));
            assert!(changed[id].pixels.is_some());
            assert_ne!(changed[id].pixels, original[id].pixels);
        }
        assert_ne!(changed[&wanted[0]].pixels, changed[&wanted[1]].pixels);
    }

    #[test]
    fn gallery_mailbox_does_not_gate_primary_renderer() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let (provider, job) = fixture(1, 8);
        let mut viewer = crate::render::Renderer::new(ctx.clone(), Arc::new(provider));
        let gallery = LiveThumbs::new(ctx, Arc::new(FileProvider::new(0)), dir.path().to_owned(), 0);
        // Deliberately block the gallery before the viewer request is submitted.
        let _busy_gallery = gallery.mail.0.lock().unwrap();
        let generation = viewer.submit((*job.graph).clone(), job.state.stage.0, None, ImageId(1), job.state.view, None, None);
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(reply) = viewer.poll() {
                assert_eq!(reply.generation(), generation);
                assert!(matches!(reply, crate::render::Reply::Frame(_)));
                break;
            }
            assert!(Instant::now() < until, "viewer waited for the gallery");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "synthetic CPU and GPU performance probe"]
    fn gallery_benchmark() {
        let count = 100;
        let (p, mut job) = fixture(count, EDGE);
        let (large, _) = fixture(1, 2048);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.png");
        let rgba = large.source.data().iter().map(|v| (255.0 * v) as u8).collect::<Vec<_>>();
        image::save_buffer(&path, &rgba, 2048, 2048, image::ColorType::Rgba8).unwrap();
        let disk = ThumbCache::new(dir.path(), 1 << 30).unwrap();
        let file = FileProvider::new(0);
        let start = Instant::now();
        for i in 0..10 {
            let base = file.thumbnail_base(ImageId(i), &path, EDGE).unwrap();
            let bytes = encode(&base);
            disk.put(&format!("{i:x}"), CACHE_KEY, &bytes).unwrap();
            assert!(decode(&bytes).is_some());
        }
        println!("synthetic 2048 px PNG decode, downscale, f16 round-trip and disk write: {:.2} ms/image", start.elapsed().as_secs_f64() * 100.0);
        let pool = rayon::ThreadPoolBuilder::new().num_threads(worker_threads()).build().unwrap();
        println!("gallery worker threads: {}", worker_threads());
        let cpu_pass = |job: &Job| pool.install(|| job.wanted.par_iter().map(|&(id, stage)| cpu(&mut Evaluator::new(), job, id, stage, &p).unwrap()).collect::<Vec<_>>());
        set_ev(&mut job, p.ids[0], 0.5, Mode::All);
        let before = cpu_pass(&job);
        set_ev(&mut job, p.ids[0], 1.5, Mode::All);
        let start = Instant::now();
        let after = cpu_pass(&job);
        println!("synthetic warm-base CPU exposure re-render {count} × {EDGE}: {:.2} ms", start.elapsed().as_secs_f64() * 1000.0);
        assert_ne!(before[0], after[0]);
        let _gpu = crate::gpu_real::gpu_test_lock();
        match GpuBatch::new() {
            Err(error) => println!("GPU unavailable: {error}"),
            Ok(mut gpu) => {
                let mut eval = Evaluator::new();
                let work: Vec<_> = job.wanted.iter().map(|&(id, stage)| (id, stage, (id, stage, 0))).collect();
                for batch in work.chunks(BATCH) { assert!(gpu.render(&job, &p, batch, &mut eval).iter().all(Option::is_some)); }
                set_ev(&mut job, p.ids[0], 1.6, Mode::All);
                let start = Instant::now();
                let results: Vec<_> = work.chunks(BATCH).flat_map(|batch| gpu.render(&job, &p, batch, &mut eval)).collect();
                println!("synthetic GPU exposure re-render {count} × {EDGE}, batched readback: {:.2} ms; {}", start.elapsed().as_secs_f64() * 1000.0, gpu.ctx.adapter_summary());
                let cpu = cpu_pass(&job);
                for (gpu, cpu) in results.into_iter().zip(cpu) {
                    let gpu = gpu.expect("GPU render, not CPU fallback");
                    assert!(gpu.2.iter().zip(cpu.2).all(|(a, b)| a.abs_diff(b) <= 1));
                }
            }
        }
    }
}
