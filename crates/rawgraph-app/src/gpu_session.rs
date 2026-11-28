//! The GPU preview session: which base images live on the GPU, the targets the viewer shows,
//! and what happens when something fails. It talks to the GPU through the small [`Backend`]
//! trait so every rule can be tested with a fake that fails on demand.

use std::sync::Arc;

use eframe::egui::TextureId;
use rawgraph_core::ops::LocalToneGrid;
use rawgraph_core::ImageBuf;
use rawgraph_engine::TailOp;
use rawgraph_gpu::{FilterOp, GpuError, GpuNode, GpuToneMap, LocalToneOp, PointOp, SharpenOp};
use rawgraph_scopes::{DisplayImage, ToneMap};

use crate::gpu_path::{Attempt, BaseKey, Fallback, Lru, RendererMode, ScopeScheduler};

/// At most this many base images stay on the GPU (the selected image and its neighbours).
pub const MAX_BASES: usize = 3;
/// At most this many (target, scratch) pairs, one per picture size in use.
pub const MAX_CLASSES: usize = 2;

/// What the session needs from the GPU. The real implementation is in `gpu_real`.
pub trait Backend {
    /// A base image on the GPU (float).
    type Image;
    /// The scratch image and the display target for one picture size, registered with egui.
    type Class;

    fn limits_ok(&self, width: u32, height: u32) -> bool;
    fn device_lost(&self) -> bool;
    fn upload(&mut self, image: &ImageBuf) -> Result<Self::Image, GpuError>;
    fn new_class(&mut self, width: u32, height: u32) -> Result<Self::Class, GpuError>;
    fn texture_id(&self, class: &Self::Class) -> TextureId;
    /// Unregisters the class from egui and frees it.
    fn release_class(&mut self, class: Self::Class);
    /// Runs the chain `nodes` on `base` and presents it in the class's target. The class keeps the
    /// scratch textures of the chain (pooled between frames, trimmed to what the chain needs) and
    /// where the float result lives.
    fn render(&mut self, base: &Self::Image, class: &mut Self::Class, nodes: &[GpuNode], exposure_ev: f32, tone: GpuToneMap) -> Result<(), GpuError>;
    /// The display picture sampled at every `step`-th pixel (after the ops, i.e. what is shown).
    fn samples(&mut self, base: &Self::Image, class: &Self::Class, ops_empty: bool, exposure_ev: f32, tone: GpuToneMap, step: u32) -> Result<DisplayImage, GpuError>;
    fn read_rgba8(&mut self, class: &Self::Class) -> Result<Vec<u8>, GpuError>;
    /// Blocks until the submitted work has finished.
    fn wait_idle(&self);
    /// Frees caches that live outside the images (uploaded grids): the GPU is being released.
    fn free_caches(&mut self) {}
}

/// `TailOp` -> the GPU crate's `PointOp` (same variants, same meaning; no wildcard on purpose).
pub fn point_ops(ops: &[TailOp]) -> Vec<PointOp> {
    ops.iter().filter_map(point_op).collect()
}

/// The pointwise `TailOp` as `PointOp`; `None` for the neighbourhood operations, which are
/// separate nodes of the chain ([`gpu_nodes`]).
fn point_op(op: &TailOp) -> Option<PointOp> {
    Some(match op {
        TailOp::Gain(g) => PointOp::Gain(*g),
        TailOp::Matrix(m) => PointOp::Matrix(*m),
        TailOp::Saturation(a) => PointOp::Saturation(*a),
        TailOp::Contrast { amount, pivot } => PointOp::Contrast { amount: *amount, pivot: *pivot },
        TailOp::HighlightsShadows { highlights, shadows } => PointOp::HighlightsShadows { highlights: *highlights, shadows: *shadows },
        TailOp::Cdl { slope, offset, power } => PointOp::Cdl { slope: *slope, offset: *offset, power: *power },
        TailOp::ToneCurve { shadows, darks, lights, highlights } => {
            PointOp::ToneCurve { shadows: *shadows, darks: *darks, lights: *lights, highlights: *highlights }
        }
        TailOp::PointCurve { points, channel } => PointOp::PointCurve { points: points.clone(), channel: *channel },
        TailOp::HueSaturation { bands } => PointOp::HueSaturation { bands: *bands },
        TailOp::Vignette { amount, midpoint, feather } => PointOp::Vignette { amount: *amount, midpoint: *midpoint, feather: *feather },
        TailOp::WhitesBlacks { whites, blacks } => PointOp::WhitesBlacks { whites: *whites, blacks: *blacks },
        TailOp::Vibrance(a) => PointOp::Vibrance(*a),
        TailOp::Lut { lut, space, strength } => PointOp::Lut { lut: Arc::clone(lut), space: *space, strength: *strength },
        TailOp::LocalTone { .. } | TailOp::Filter { .. } | TailOp::Sharpen { .. } => return None,
    })
}

/// The grids of the local tone operations of a tail: one entry per operation, `Some` for
/// [`TailOp::LocalTone`].
pub type TailGrids = [Option<Arc<LocalToneGrid>>];

/// The tail as a GPU chain for a base of `size`: consecutive pointwise operations are fused into one
/// pass, every neighbourhood operation is its own node. Pixel radii follow the CPU: the parameter
/// is per mille of the long edge of the image the operation sees (the base size, as every tail
/// operation keeps it). A local tone operation without its grid is an error.
pub fn gpu_nodes(ops: &[TailOp], grids: &TailGrids, size: (u32, u32)) -> Result<Vec<GpuNode>, GpuError> {
    let long = size.0.max(size.1) as f32;
    let mut nodes: Vec<GpuNode> = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        if let Some(p) = point_op(op) {
            match nodes.last_mut() {
                Some(GpuNode::Point(run)) => run.push(p),
                _ => nodes.push(GpuNode::Point(vec![p])),
            }
            continue;
        }
        nodes.push(match op {
            TailOp::LocalTone { highlights, shadows, whites, blacks, grid } => {
                let Some(Some(data)) = grids.get(i) else {
                    return Err(GpuError::InvalidArgument("the local tone grid is missing".into()));
                };
                GpuNode::LocalTone(LocalToneOp {
                    id: *grid,
                    grid: Arc::clone(data),
                    highlights: *highlights,
                    shadows: *shadows,
                    whites: *whites,
                    blacks: *blacks,
                })
            }
            TailOp::Filter { radius, box_kernel, high } => {
                GpuNode::Filter(FilterOp { sigma: radius / 1000.0 * long, box_kernel: *box_kernel, high: *high })
            }
            TailOp::Sharpen { amount, radius, threshold } => {
                GpuNode::Sharpen(SharpenOp { amount: *amount, radius_px: radius / 1000.0 * long, threshold: *threshold })
            }
            _ => continue,
        });
    }
    Ok(nodes)
}

pub fn gpu_tone(tone: ToneMap) -> GpuToneMap {
    match tone {
        ToneMap::Clip => GpuToneMap::Clip,
        ToneMap::SoftHighlights => GpuToneMap::SoftHighlights,
        ToneMap::AcesFit => GpuToneMap::AcesFit,
    }
}

/// What the viewer needs to show a GPU picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuPicture {
    pub texture: TextureId,
    pub width: u32,
    pub height: u32,
}

/// The display settings of one frame.
#[derive(Clone, Copy, Debug)]
pub struct DisplayArgs {
    pub exposure_ev: f32,
    pub tone: GpuToneMap,
}

/// Counters the tests (and the benchmark) look at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub uploads: usize,
    pub renders: usize,
    pub classes_created: usize,
    /// Uploads that were made in the background for a neighbour image (also counted in `uploads`).
    pub preloads: usize,
}

/// Result of [`GpuSession::preload`].
#[derive(Debug, PartialEq, Eq)]
pub enum Preloaded {
    Uploaded,
    AlreadyThere,
    /// Not done, and why (the GPU is off for this image, no room without touching a protected
    /// image, ...). Not an error.
    Skipped(&'static str),
    /// The upload failed; further preloads are off for the session.
    Failed,
}

/// First answer of the session for a request.
#[derive(Debug, PartialEq, Eq)]
pub enum Begin {
    /// Use the CPU for this frame; the note (if any) is shown once in the status bar.
    Cpu { reason: String, notice: Option<String> },
    /// The base is on the GPU already: no CPU evaluation needed.
    HaveBase,
    /// The base has to be evaluated on the CPU and handed to [`GpuSession::show`].
    NeedBase,
}

/// Result of [`GpuSession::show`].
#[derive(Debug, PartialEq)]
pub enum Shown {
    Gpu(GpuPicture),
    Cpu { reason: String, notice: Option<String> },
    /// GPU-only mode: the frame could not be produced.
    Failed(String),
}

struct Held<I> {
    image: I,
    width: u32,
    height: u32,
}

/// The target and scratch image of the last shown frame, for scopes and the frame dump.
#[derive(Clone, Copy, Debug)]
struct Last {
    key: BaseKey,
    class: (u32, u32),
    ops_empty: bool,
    display: DisplayArgs,
}

pub struct GpuSession<B: Backend> {
    backend: B,
    bases: Lru<BaseKey, Held<B::Image>>,
    classes: Lru<(u32, u32), B::Class>,
    /// Classes no longer in use but possibly still painted until `shown >= tag`.
    graveyard: Vec<(B::Class, u64)>,
    pub fallback: Fallback,
    pub scopes: ScopeScheduler,
    last: Option<Last>,
    /// A background upload ran out of memory: no more speculative uploads in this session.
    preload_off: bool,
    pub stats: Stats,
}

impl<B: Backend> GpuSession<B> {
    /// `unavailable`: why the GPU cannot be used at all (for example no wgpu render state).
    pub fn new(backend: B, mode: RendererMode, unavailable: Option<String>) -> Self {
        Self {
            backend,
            bases: Lru::new(MAX_BASES),
            classes: Lru::new(MAX_CLASSES),
            graveyard: Vec::new(),
            fallback: Fallback::new(mode, unavailable),
            scopes: ScopeScheduler::default(),
            last: None,
            preload_off: false,
            stats: Stats::default(),
        }
    }

    /// Whether uploading `key` in the background is allowed now: the GPU is usable for it, it is
    /// not there yet, and speculative uploads have not been switched off.
    pub fn wants_preload(&self, key: &BaseKey) -> bool {
        !self.preload_off && !self.backend.device_lost() && matches!(self.fallback.decide(key), Attempt::Try) && !self.bases.contains(key)
    }

    /// Uploads the base of a neighbour image into a free slot (or the slot of the least recently
    /// used image that is not in `protect`), never touching the picture on screen.
    ///
    /// The out-of-memory rules: nothing is tried for an image the fallback state has blocked or
    /// while the GPU is off; a failed upload switches speculative uploads off for the session
    /// without counting towards the three failures that switch the GPU off (the picture on
    /// screen was not affected).
    pub fn preload(&mut self, key: &BaseKey, base: &ImageBuf, protect: &[BaseKey]) -> Preloaded {
        if self.bases.contains(key) {
            return Preloaded::AlreadyThere;
        }
        if self.preload_off {
            return Preloaded::Skipped("background uploads are off for this session");
        }
        if self.backend.device_lost() || !matches!(self.fallback.decide(key), Attempt::Try) {
            return Preloaded::Skipped("the GPU is not available for this image");
        }
        if self.bases.keys_oldest_first().iter().any(|k| k.preview != key.preview) {
            return Preloaded::Skipped("the preview size is changing");
        }
        let (w, h) = (base.width(), base.height());
        if !self.backend.limits_ok(w, h) {
            return Preloaded::Skipped("the picture is larger than this GPU allows");
        }
        if self.bases.count() >= MAX_BASES {
            // Free a slot BEFORE allocating: the oldest image that nobody needs.
            match self.bases.evict_oldest_where(|k| k != key && !protect.contains(k)) {
                Some(old) => drop(old),
                None => return Preloaded::Skipped("every slot holds an image that is still needed"),
            }
        }
        match self.backend.upload(base) {
            Ok(image) => {
                self.stats.uploads += 1;
                self.stats.preloads += 1;
                drop(self.bases.insert(*key, Held { image, width: w, height: h }));
                Preloaded::Uploaded
            }
            Err(e) => {
                eprintln!("GPU preview: background upload failed ({e}); no more background uploads in this session");
                self.preload_off = true;
                Preloaded::Failed
            }
        }
    }

    #[cfg(test)]
    pub fn backend(&self) -> &B {
        &self.backend
    }

    #[cfg(test)]
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    #[cfg(test)]
    pub fn cached_bases(&self) -> Vec<BaseKey> {
        self.bases.keys()
    }


    /// Frees retired classes that no frame on screen can use any more.
    fn sweep(&mut self, shown: u64) {
        let (free, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.graveyard).into_iter().partition(|(_, tag)| shown >= *tag);
        self.graveyard = keep;
        for (class, _) in free {
            self.backend.release_class(class);
        }
    }

    fn retire(&mut self, class: B::Class, generation: u64) {
        self.graveyard.push((class, generation));
    }

    /// Drops the uploaded base images (they are uploaded again when needed); the frame on
    /// screen keeps its target. Used before a cold-start measurement.
    pub fn forget_bases(&mut self) {
        drop(self.bases.drain());
        self.last = None;
    }

    /// Drops every GPU image (out of memory, device lost, preview size change).
    fn release_all(&mut self, generation: u64) {
        self.bases.drain();
        self.backend.free_caches();
        for class in self.classes.drain() {
            self.retire(class, generation);
        }
        self.last = None;
    }

    fn lost(&mut self, generation: u64) -> Option<String> {
        self.release_all(generation);
        self.fallback.on_device_lost()
    }

    /// Decides whether this request can use the GPU and whether the base must be evaluated.
    pub fn begin(&mut self, key: &BaseKey, generation: u64, shown: u64) -> Begin {
        self.sweep(shown);
        if self.backend.device_lost() {
            let notice = self.lost(generation);
            let reason = self.fallback.permanent_reason().unwrap_or("GPU device lost").to_owned();
            return Begin::Cpu { reason, notice };
        }
        if let Attempt::Skip(reason) = self.fallback.decide(key) {
            return Begin::Cpu { reason, notice: None };
        }
        // Bases of another preview size are of no use any more: free them now.
        let stale = self.bases.remove_where(|k| k.preview != key.preview);
        drop(stale);
        if self.bases.contains(key) { Begin::HaveBase } else { Begin::NeedBase }
    }

    /// Renders the frame into the display target. `base` is needed when [`Begin::NeedBase`] was
    /// returned. Never panics; every failure becomes a CPU frame (or `Failed` in GPU-only mode).
    #[cfg(test)]
    pub fn show(&mut self, key: &BaseKey, base: Option<&ImageBuf>, ops: &[TailOp], display: DisplayArgs, generation: u64, shown: u64) -> Shown {
        self.show_with(key, base, ops, &[], display, generation, shown)
    }

    /// [`GpuSession::show`] for a tail with neighbourhood operations: `grids` has one entry per
    /// operation (`Some` for the local tone ones, see `Evaluator::tail_grid`); it may be empty for a
    /// pointwise tail.
    #[allow(clippy::too_many_arguments)]
    pub fn show_with(&mut self, key: &BaseKey, base: Option<&ImageBuf>, ops: &[TailOp], grids: &TailGrids, display: DisplayArgs, generation: u64, shown: u64) -> Shown {
        self.sweep(shown);
        match self.try_show(key, base, ops, grids, display, generation) {
            Ok(picture) => {
                self.fallback.on_success();
                Shown::Gpu(picture)
            }
            Err(e) => self.on_error(key, e, generation),
        }
    }

    fn try_show(&mut self, key: &BaseKey, base: Option<&ImageBuf>, ops: &[TailOp], grids: &TailGrids, display: DisplayArgs, generation: u64) -> Result<GpuPicture, GpuError> {
        if !self.bases.contains(key) {
            let img = base.ok_or_else(|| GpuError::Readback("the base image was not provided".into()))?;
            let (w, h) = (img.width(), img.height());
            if !self.backend.limits_ok(w, h) {
                return Err(GpuError::TooLarge { width: w, height: h, max_dimension: 0 });
            }
            // Free the least recently used base BEFORE allocating the new one.
            drop(self.bases.make_room());
            let image = self.backend.upload(img)?;
            self.stats.uploads += 1;
            drop(self.bases.insert(*key, Held { image, width: w, height: h }));
        }
        let (w, h) = match self.bases.peek(key) {
            Some(h) => (h.width, h.height),
            None => return Err(GpuError::Readback("base image vanished".into())),
        };
        if !self.classes.contains(&(w, h)) {
            for old in self.classes.make_room() {
                self.retire(old, generation);
            }
            let class = self.backend.new_class(w, h)?;
            self.stats.classes_created += 1;
            drop(self.classes.insert((w, h), class));
        }
        let nodes = gpu_nodes(ops, grids, (w, h))?;
        let (Some(held), Some(class)) = (self.bases.get_mut(key), self.classes.get_mut(&(w, h))) else {
            return Err(GpuError::Readback("GPU images vanished".into()));
        };
        self.backend.render(&held.image, class, &nodes, display.exposure_ev, display.tone)?;
        self.stats.renders += 1;
        // The UI shows the picture as soon as it is told: make sure the work is done.
        self.backend.wait_idle();
        let texture = self.backend.texture_id(class);
        self.last = Some(Last { key: *key, class: (w, h), ops_empty: nodes.is_empty(), display });
        Ok(GpuPicture { texture, width: w, height: h })
    }

    fn on_error(&mut self, key: &BaseKey, error: GpuError, generation: u64) -> Shown {
        let strict = self.fallback.strict();
        let message = error.to_string();
        let (reason, notice) = if self.backend.device_lost() {
            let notice = self.lost(generation);
            (self.fallback.permanent_reason().unwrap_or("GPU device lost").to_owned(), notice)
        } else {
            match error {
                GpuError::OutOfMemory(m) => {
                    self.release_all(generation);
                    let notice = self.fallback.on_out_of_memory(key, &m);
                    eprintln!("GPU preview: {message}; this frame uses the CPU");
                    (format!("GPU out of memory ({m}); this image is shown on the CPU"), notice)
                }
                GpuError::TooLarge { .. } => (format!("the picture is larger than this GPU allows ({message})"), None),
                other => {
                    if self.fallback.on_other_error() {
                        eprintln!("GPU preview: {other}; the CPU shows this frame (logged once)");
                    }
                    (format!("GPU error: {other}"), None)
                }
            }
        };
        if strict {
            return Shown::Failed(format!("GPU renderer required (--renderer gpu), but: {message}"));
        }
        Shown::Cpu { reason, notice }
    }

    /// The scopes of the picture shown last, sampled at `step` (scopes of the GPU frame).
    pub fn samples(&mut self, step: u32) -> Result<DisplayImage, GpuError> {
        let Some(last) = self.last else {
            return Err(GpuError::Readback("no GPU frame yet".into()));
        };
        let (Some(held), Some(class)) = (self.bases.get_mut(&last.key), self.classes.get_mut(&last.class)) else {
            return Err(GpuError::Readback("the GPU frame is gone".into()));
        };
        self.backend.samples(&held.image, class, last.ops_empty, last.display.exposure_ev, last.display.tone, step)
    }

    /// Reads the picture shown last as RGBA8 (the frame dump).
    pub fn read_current(&mut self) -> Result<Vec<u8>, GpuError> {
        let Some(last) = self.last else {
            return Err(GpuError::Readback("no GPU frame yet".into()));
        };
        let Some(class) = self.classes.get_mut(&last.class) else {
            return Err(GpuError::Readback("the GPU frame is gone".into()));
        };
        self.backend.read_rgba8(class)
    }

    /// Size of the picture shown last.
    pub fn current_size(&self) -> Option<(u32, u32)> {
        self.last.map(|l| l.class)
    }

    /// Errors of the scope reading: the picture itself is fine, so only a lost device matters.
    pub fn note_samples_error(&mut self, generation: u64) -> Option<String> {
        if self.backend.device_lost() { self.lost(generation) } else { None }
    }
}

impl<B: Backend> Drop for GpuSession<B> {
    fn drop(&mut self) {
        for class in self.classes.drain() {
            self.backend.release_class(class);
        }
        for (class, _) in std::mem::take(&mut self.graveyard) {
            self.backend.release_class(class);
        }
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A backend that records what happens and fails on demand.
    #[derive(Default)]
    pub struct FakeBackend {
        pub live_images: Rc<Cell<usize>>,
        pub live_classes: Rc<Cell<usize>>,
        pub renders: Vec<(usize, Vec<GpuNode>)>,
        pub fail_upload: Option<GpuError>,
        pub fail_class: Option<GpuError>,
        pub fail_render: Option<GpuError>,
        pub lost: Cell<bool>,
        pub max_dim: u32,
        pub next_id: u64,
    }

    pub struct FakeImage {
        live: Rc<Cell<usize>>,
        pub id: usize,
    }

    impl Drop for FakeImage {
        fn drop(&mut self) {
            self.live.set(self.live.get() - 1);
        }
    }

    pub struct FakeClass {
        pub id: u64,
        pub size: (u32, u32),
    }

    impl FakeBackend {
        pub fn new() -> Self {
            Self { max_dim: 8192, ..Self::default() }
        }
    }

    impl Backend for FakeBackend {
        type Image = FakeImage;
        type Class = FakeClass;

        fn limits_ok(&self, w: u32, h: u32) -> bool {
            w <= self.max_dim && h <= self.max_dim
        }
        fn device_lost(&self) -> bool {
            self.lost.get()
        }
        fn upload(&mut self, image: &ImageBuf) -> Result<FakeImage, GpuError> {
            if let Some(e) = &self.fail_upload {
                return Err(e.clone());
            }
            self.live_images.set(self.live_images.get() + 1);
            Ok(FakeImage { live: self.live_images.clone(), id: image.width() as usize })
        }
        fn new_class(&mut self, w: u32, h: u32) -> Result<FakeClass, GpuError> {
            if let Some(e) = &self.fail_class {
                return Err(e.clone());
            }
            self.next_id += 1;
            self.live_classes.set(self.live_classes.get() + 1);
            Ok(FakeClass { id: self.next_id, size: (w, h) })
        }
        fn texture_id(&self, class: &FakeClass) -> TextureId {
            TextureId::User(class.id)
        }
        fn release_class(&mut self, _class: FakeClass) {
            self.live_classes.set(self.live_classes.get() - 1);
        }
        fn render(&mut self, base: &FakeImage, _c: &mut FakeClass, ops: &[GpuNode], _ev: f32, _t: GpuToneMap) -> Result<(), GpuError> {
            if let Some(e) = &self.fail_render {
                return Err(e.clone());
            }
            self.renders.push((base.id, ops.to_vec()));
            Ok(())
        }
        fn samples(&mut self, _b: &FakeImage, c: &FakeClass, _e: bool, _ev: f32, _t: GpuToneMap, step: u32) -> Result<DisplayImage, GpuError> {
            let (w, h) = (c.size.0.div_ceil(step), c.size.1.div_ceil(step));
            Ok(DisplayImage { width: w, height: h, rgb: vec![0.5; w as usize * h as usize * 3] })
        }
        fn read_rgba8(&mut self, c: &FakeClass) -> Result<Vec<u8>, GpuError> {
            Ok(vec![7; c.size.0 as usize * c.size.1 as usize * 4])
        }
        fn wait_idle(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;
    use rawgraph_core::ImageId;

    fn key(image: u64, fp: u64, preview: u32) -> BaseKey {
        BaseKey { image: ImageId(image), fingerprint: fp, preview, revision: 0 }
    }

    fn img(w: u32) -> ImageBuf {
        ImageBuf::new(w, 4)
    }

    fn disp() -> DisplayArgs {
        DisplayArgs { exposure_ev: 0.0, tone: GpuToneMap::AcesFit }
    }

    fn session() -> GpuSession<FakeBackend> {
        GpuSession::new(FakeBackend::new(), RendererMode::Auto, None)
    }

    /// One full request the way the worker makes it.
    fn go(s: &mut GpuSession<FakeBackend>, k: BaseKey, w: u32, ops: &[TailOp], generation: u64) -> Shown {
        match s.begin(&k, generation, generation.saturating_sub(1)) {
            Begin::Cpu { reason, notice } => Shown::Cpu { reason, notice },
            Begin::HaveBase => s.show(&k, None, ops, disp(), generation, generation.saturating_sub(1)),
            Begin::NeedBase => s.show(&k, Some(&img(w)), ops, disp(), generation, generation.saturating_sub(1)),
        }
    }

    #[test]
    fn tail_ops_map_one_to_one() {
        let m = [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0]];
        let mut bands = [[0.0f32; 3]; 6];
        bands[0] = [0.1, -0.2, 0.3];
        bands[4] = [-0.4, 0.5, -0.6];
        let pts = vec![[0.0, 0.0], [0.3, 0.2], [1.0, 1.0]];
        let ops = [
            TailOp::Gain(1.5),
            TailOp::Matrix(m),
            TailOp::Saturation(1.3),
            TailOp::Contrast { amount: 0.3, pivot: 0.18 },
            TailOp::HighlightsShadows { highlights: -0.2, shadows: 0.4 },
            TailOp::Cdl { slope: [1.1, 1.2, 1.3], offset: [0.01, 0.02, 0.03], power: [0.9, 1.0, 1.1] },
            TailOp::ToneCurve { shadows: 0.1, darks: -0.2, lights: 0.3, highlights: -0.4 },
            TailOp::PointCurve { points: pts.clone(), channel: 2 },
            TailOp::HueSaturation { bands },
            TailOp::Vignette { amount: -0.5, midpoint: 0.4, feather: 0.6 },
        ];
        assert_eq!(
            point_ops(&ops),
            vec![
                PointOp::Gain(1.5),
                PointOp::Matrix(m),
                PointOp::Saturation(1.3),
                PointOp::Contrast { amount: 0.3, pivot: 0.18 },
                PointOp::HighlightsShadows { highlights: -0.2, shadows: 0.4 },
                PointOp::Cdl { slope: [1.1, 1.2, 1.3], offset: [0.01, 0.02, 0.03], power: [0.9, 1.0, 1.1] },
                PointOp::ToneCurve { shadows: 0.1, darks: -0.2, lights: 0.3, highlights: -0.4 },
                PointOp::PointCurve { points: pts, channel: 2 },
                PointOp::HueSaturation { bands },
                PointOp::Vignette { amount: -0.5, midpoint: 0.4, feather: 0.6 },
            ]
        );
        // Every single variant on its own, so a wrong arm cannot hide behind a right neighbour.
        for op in &ops {
            assert_eq!(point_ops(std::slice::from_ref(op)).len(), 1);
        }
        assert!(point_ops(&[]).is_empty());
        assert_eq!(gpu_tone(ToneMap::AcesFit), GpuToneMap::AcesFit);
        assert_eq!(gpu_tone(ToneMap::Clip), GpuToneMap::Clip);
        assert_eq!(gpu_tone(ToneMap::SoftHighlights), GpuToneMap::SoftHighlights);
    }

    // ---- neighbour preload

    fn protect(keys: &[BaseKey]) -> Vec<BaseKey> {
        keys.to_vec()
    }

    #[test]
    fn a_preloaded_neighbour_needs_no_upload_when_it_is_stepped_to() {
        let mut s = session();
        let (cur, next, prev) = (key(2, 1, 2560), key(3, 1, 2560), key(1, 1, 2560));
        go(&mut s, cur, 64, &[], 1);
        let all = protect(&[cur, next, prev]);
        assert_eq!(s.preload(&next, &img(65), &all), Preloaded::Uploaded);
        assert_eq!(s.preload(&prev, &img(66), &all), Preloaded::Uploaded);
        assert_eq!(s.stats.preloads, 2);
        assert_eq!(s.backend().live_images.get(), 3);
        let before = s.stats.uploads;
        // Stepping to the next image: the base is there, begin() says so and no pixels are needed.
        assert_eq!(s.begin(&next, 2, 1), Begin::HaveBase);
        assert!(matches!(s.show(&next, None, &[TailOp::Gain(2.0)], disp(), 2, 1), Shown::Gpu(_)));
        assert_eq!(s.stats.uploads, before, "no upload when stepping");
        assert_eq!(s.preload(&next, &img(65), &all), Preloaded::AlreadyThere);
    }

    #[test]
    fn preloading_stays_within_three_images_and_never_evicts_what_is_protected() {
        let mut s = session();
        let live = s.backend().live_images.clone();
        // Old images 8 and 9 and the current one 2 fill the three slots.
        for (i, k) in [key(8, 1, 2560), key(9, 1, 2560), key(2, 1, 2560)].into_iter().enumerate() {
            go(&mut s, k, 64, &[], 1 + i as u64);
        }
        let (cur, next, prev) = (key(2, 1, 2560), key(3, 1, 2560), key(1, 1, 2560));
        let all = protect(&[cur, next, prev]);
        assert_eq!(s.preload(&next, &img(65), &all), Preloaded::Uploaded);
        assert_eq!(s.preload(&prev, &img(66), &all), Preloaded::Uploaded);
        assert_eq!(live.get(), 3, "never more than three images on the GPU");
        let kept: Vec<u64> = s.cached_bases().iter().map(|k| k.image.0).collect();
        assert!(kept.contains(&1) && kept.contains(&2) && kept.contains(&3), "{kept:?}");
        // When everything left is protected there is no room for a fourth image.
        let other = key(7, 1, 2560);
        let r = s.preload(&other, &img(67), &protect(&[cur, next, prev, other]));
        assert!(matches!(r, Preloaded::Skipped(_)), "{r:?}");
        assert_eq!(live.get(), 3);
    }

    #[test]
    fn preloading_obeys_the_failure_rules() {
        // Out of memory in the background: switched off for the session, the shown image is fine.
        let mut s = session();
        let cur = key(2, 1, 2560);
        go(&mut s, cur, 64, &[], 1);
        s.backend.fail_upload = Some(GpuError::OutOfMemory("fake".into()));
        let next = key(3, 1, 2560);
        assert_eq!(s.preload(&next, &img(65), &[cur, next]), Preloaded::Failed);
        assert_eq!(s.fallback.consecutive_oom(), 0, "a speculative failure is not one of the three strikes");
        s.backend.fail_upload = None;
        assert!(matches!(s.preload(&next, &img(65), &[cur, next]), Preloaded::Skipped(_)), "no more background uploads");
        assert!(!s.wants_preload(&next));
        assert!(matches!(go(&mut s, cur, 64, &[], 2), Shown::Gpu(_)), "the picture on screen keeps working");

        // An image blocked by an earlier out-of-memory failure is not tried.
        let mut s = session();
        s.backend.fail_upload = Some(GpuError::OutOfMemory("fake".into()));
        assert!(matches!(go(&mut s, key(5, 1, 2560), 64, &[], 1), Shown::Cpu { .. }));
        s.backend.fail_upload = None;
        assert!(!s.wants_preload(&key(5, 2, 2560)));
        assert!(matches!(s.preload(&key(5, 2, 2560), &img(65), &[]), Preloaded::Skipped(_)));
        // A lost device and a GPU that is off stay untouched.
        let mut s = session();
        s.backend.lost.set(true);
        assert!(matches!(s.preload(&key(1, 1, 2560), &img(65), &[]), Preloaded::Skipped(_)));
        let mut off = GpuSession::new(FakeBackend::new(), RendererMode::Auto, Some("no GPU".into()));
        assert!(matches!(off.preload(&key(1, 1, 2560), &img(65), &[]), Preloaded::Skipped(_)));
        assert_eq!(off.stats.uploads, 0);
    }

    #[test]
    fn a_preview_size_change_is_left_to_the_real_request() {
        let mut s = session();
        go(&mut s, key(1, 1, 1280), 32, &[], 1);
        let r = s.preload(&key(2, 1, 2560), &img(64), &[]);
        assert!(matches!(r, Preloaded::Skipped(_)), "{r:?}");
        assert_eq!(s.stats.preloads, 0);
    }

    #[test]
    fn same_fingerprint_and_tail_only_changes_upload_nothing() {
        let mut s = session();
        let k = key(1, 100, 2560);
        assert!(matches!(go(&mut s, k, 64, &[TailOp::Gain(1.0)], 1), Shown::Gpu(_)));
        assert_eq!(s.stats.uploads, 1);
        // The user drags exposure: only the ops change, the fingerprint stays.
        for (i, g) in [1.1f32, 1.2, 1.3].into_iter().enumerate() {
            assert!(matches!(go(&mut s, k, 64, &[TailOp::Gain(g)], 2 + i as u64), Shown::Gpu(_)));
        }
        assert_eq!(s.stats.uploads, 1, "no upload for tail-only changes");
        assert_eq!(s.stats.renders, 4);
        assert_eq!(s.stats.classes_created, 1, "the target is reused");
        // A base change uploads again.
        assert!(matches!(go(&mut s, key(1, 101, 2560), 64, &[], 6), Shown::Gpu(_)));
        assert_eq!(s.stats.uploads, 2);
    }

    #[test]
    fn at_most_three_bases_stay_on_the_gpu() {
        let mut s = session();
        let live = s.backend().live_images.clone();
        for i in 1..=3 {
            go(&mut s, key(i, 1, 2560), 64, &[], i);
        }
        assert_eq!(live.get(), 3);
        // Stepping back to image 1 is free and makes image 2 the oldest.
        go(&mut s, key(1, 1, 2560), 64, &[], 4);
        assert_eq!(s.stats.uploads, 3);
        go(&mut s, key(4, 1, 2560), 64, &[], 5);
        assert_eq!(live.get(), 3, "the fourth image evicts the least recently used one");
        assert_eq!(s.stats.uploads, 4);
        let keys: Vec<u64> = s.cached_bases().iter().map(|k| k.image.0).collect();
        assert!(!keys.contains(&2) && keys.contains(&1) && keys.contains(&3) && keys.contains(&4), "{keys:?}");
        // Image 2 has to be uploaded again.
        go(&mut s, key(2, 1, 2560), 64, &[], 6);
        assert_eq!(s.stats.uploads, 5);
    }

    #[test]
    fn a_new_preview_size_uploads_again_and_drops_the_old_bases() {
        let mut s = session();
        let live = s.backend().live_images.clone();
        go(&mut s, key(1, 1, 1280), 32, &[], 1);
        go(&mut s, key(2, 1, 1280), 32, &[], 2);
        assert_eq!(live.get(), 2);
        go(&mut s, key(1, 1, 2560), 64, &[], 3);
        assert_eq!(s.stats.uploads, 3);
        assert_eq!(live.get(), 1, "bases of the old preview size are freed");
        assert_eq!(s.stats.classes_created, 2, "a new picture size gets its own target");
    }

    #[test]
    fn revision_change_uploads_again() {
        let mut s = session();
        go(&mut s, key(1, 1, 2560), 64, &[], 1);
        let mut k = key(1, 1, 2560);
        k.revision = 9;
        go(&mut s, k, 64, &[], 2);
        assert_eq!(s.stats.uploads, 2);
    }

    #[test]
    fn retired_targets_are_freed_only_after_the_new_frame_was_shown() {
        let mut s = session();
        let classes = s.backend().live_classes.clone();
        go(&mut s, key(1, 1, 1280), 32, &[], 1);
        go(&mut s, key(1, 1, 2560), 64, &[], 2);
        go(&mut s, key(1, 2, 640), 16, &[], 3); // third size: the first class is evicted (cap 2)
        assert_eq!(classes.get(), 3, "the evicted one is still held until its replacement is shown");
        // Generation 3 is acknowledged: the next request sweeps.
        s.begin(&key(1, 2, 640), 4, 3);
        assert_eq!(classes.get(), 2);
    }

    #[test]
    fn out_of_memory_falls_back_once_per_image_and_then_for_good() {
        let mut s = session();
        s.backend.fail_upload = Some(GpuError::OutOfMemory("fake".into()));
        let live = s.backend().live_images.clone();
        // Image 1: the attempt fails, the frame is a CPU frame.
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        assert!(matches!(r, Shown::Cpu { ref reason, .. } if reason.contains("out of memory")), "{r:?}");
        assert_eq!(live.get(), 0);
        // Same image again (a slider moved): no new GPU attempt.
        let before = s.stats.uploads;
        assert!(matches!(go(&mut s, key(1, 2, 2560), 64, &[], 2), Shown::Cpu { .. }));
        assert_eq!(s.stats.uploads, before);
        assert_eq!(s.fallback.consecutive_oom(), 1, "no second attempt for the same image");
        // Image 2 and 3 fail as well: after three in a row the GPU stays off.
        assert!(matches!(go(&mut s, key(2, 1, 2560), 64, &[], 3), Shown::Cpu { notice: None, .. }));
        let third = go(&mut s, key(3, 1, 2560), 64, &[], 4);
        assert!(matches!(third, Shown::Cpu { notice: Some(ref n), .. } if n.contains("switched off")), "{third:?}");
        // Even when memory is back, the session stays on the CPU.
        s.backend.fail_upload = None;
        let r = go(&mut s, key(4, 1, 2560), 64, &[], 5);
        assert!(matches!(r, Shown::Cpu { ref reason, .. } if reason.contains("switched off")), "{r:?}");
    }

    #[test]
    fn a_target_allocation_failure_counts_like_an_upload_failure() {
        let mut s = session();
        s.backend.fail_class = Some(GpuError::OutOfMemory("target".into()));
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        assert!(matches!(r, Shown::Cpu { .. }));
        assert_eq!(s.backend().live_images.get(), 0, "the uploaded base was dropped again");
        s.backend.fail_class = None;
        // A new image gets a fresh attempt and works.
        assert!(matches!(go(&mut s, key(2, 1, 2560), 64, &[], 2), Shown::Gpu(_)));
        assert_eq!(s.fallback.consecutive_oom(), 0, "a success resets the count");
    }

    #[test]
    fn a_lost_device_means_cpu_for_the_rest_of_the_session() {
        let mut s = session();
        assert!(matches!(go(&mut s, key(1, 1, 2560), 64, &[], 1), Shown::Gpu(_)));
        s.backend.lost.set(true);
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 2);
        assert!(matches!(r, Shown::Cpu { notice: Some(ref n), .. } if n.contains("lost")), "{r:?}");
        assert_eq!(s.backend().live_images.get(), 0);
        s.backend.lost.set(false);
        assert!(matches!(go(&mut s, key(1, 1, 2560), 64, &[], 3), Shown::Cpu { notice: None, .. }), "stays on the CPU");
    }

    #[test]
    fn a_loss_during_rendering_is_detected_too() {
        let mut s = session();
        s.backend.fail_render = Some(GpuError::Readback("device lost".into()));
        s.backend.lost.set(true);
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        // begin() already sees the lost device.
        assert!(matches!(r, Shown::Cpu { notice: Some(_), .. }));
    }

    #[test]
    fn another_error_costs_one_frame_and_is_logged_once() {
        let mut s = session();
        s.backend.fail_render = Some(GpuError::Readback("boom".into()));
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        assert!(matches!(r, Shown::Cpu { ref reason, notice: None } if reason.contains("boom")), "{r:?}");
        assert!(matches!(go(&mut s, key(1, 1, 2560), 64, &[], 2), Shown::Cpu { .. }));
        s.backend.fail_render = None;
        assert!(matches!(go(&mut s, key(1, 1, 2560), 64, &[], 3), Shown::Gpu(_)), "the next frame tries the GPU again");
        assert_eq!(s.fallback.consecutive_oom(), 0);
    }

    #[test]
    fn a_picture_beyond_the_limits_uses_the_cpu_without_counting_as_a_failure() {
        let mut s = session();
        s.backend.max_dim = 32;
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        assert!(matches!(r, Shown::Cpu { ref reason, .. } if reason.contains("larger than this GPU")), "{r:?}");
        assert_eq!(s.fallback.consecutive_oom(), 0);
        assert_eq!(s.stats.uploads, 0);
        // A smaller picture works on the same session.
        assert!(matches!(go(&mut s, key(2, 1, 1280), 16, &[], 2), Shown::Gpu(_)));
    }

    #[test]
    fn no_render_state_means_cpu_from_the_start() {
        let mut s = GpuSession::new(FakeBackend::new(), RendererMode::Auto, Some("eframe gave no wgpu render state".into()));
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        assert!(matches!(r, Shown::Cpu { ref reason, .. } if reason.contains("no wgpu render state")), "{r:?}");
        assert_eq!(s.stats.uploads, 0);
    }

    #[test]
    fn gpu_only_mode_fails_loudly_instead_of_falling_back() {
        let mut s = GpuSession::new(FakeBackend::new(), RendererMode::Gpu, None);
        s.backend.fail_upload = Some(GpuError::OutOfMemory("fake".into()));
        let r = go(&mut s, key(1, 1, 2560), 64, &[], 1);
        assert!(matches!(r, Shown::Failed(ref m) if m.contains("--renderer gpu")), "{r:?}");
        // It keeps trying instead of switching itself off.
        let r = go(&mut s, key(2, 1, 2560), 64, &[], 2);
        assert!(matches!(r, Shown::Failed(_)));
    }

    #[test]
    fn samples_and_dump_read_the_picture_shown_last() {
        let mut s = session();
        assert!(s.samples(2).is_err(), "nothing shown yet");
        go(&mut s, key(1, 1, 2560), 64, &[TailOp::Gain(2.0)], 1);
        let d = s.samples(2).unwrap();
        assert_eq!((d.width, d.height), (32, 2));
        let px = s.read_current().unwrap();
        assert_eq!(px.len(), 64 * 4 * 4);
    }
}
