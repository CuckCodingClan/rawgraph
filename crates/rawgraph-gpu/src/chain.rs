//! Chains of GPU nodes: runs of pointwise operations plus NEIGHBOURHOOD passes (a pass that reads
//! more than its own pixel and so needs a whole texture as input and a new one as output).
//!
//! The nodes run in order. The input of the first node is the base image, which is never written;
//! every pass writes one of a few scratch textures of the base's size (`ChainScratch`), chosen so a
//! pass never writes a texture another pass of the same node still has to read (ping-pong; a third
//! scratch exists only while an operation needs the original next to its blurred copy). Low
//! resolution intermediates of large blur radii live in a second small pair. Everything stays in
//! the pool between frames (a slider drag allocates nothing) and is freed as soon as a frame needs
//! less.
//!
//! Each operation reproduces one CPU function (see the node docs); the CPU stays the reference.

use std::sync::Arc;

use rawgraph_core::ops::{low_res_scale, LocalToneGrid, LOW_RES_MIN_RADIUS};
use rawgraph_core::ImageBuf;

use crate::context::GpuContext;
use crate::display::{display_into_view, DisplayTarget, GpuToneMap};
use crate::error::GpuError;
use crate::image::{check_size, GpuImage};
use crate::pass::{build_pass_ext, run_pass_ext, Pass};
use crate::point_ops::{apply_point_ops_into, PointOp};
use crate::shaders;

/// At most this many local tone grids stay uploaded.
const GRID_CACHE_MAX: usize = 4;
/// Most taps of one convolution pass (radius 16).
const MAX_TAPS: usize = 36;

/// Develop local tone, per-pixel part (`rawgraph_core::ops::develop_local_tone`): Lights, Shadows,
/// White and Black from the guided-filter coefficient grid, which is estimated on the CPU (it is a
/// 512 px grid) and uploaded once per `id`.
#[derive(Clone, Debug)]
pub struct LocalToneOp {
    /// Identity of the grid content; equal ids mean equal grids (the uploaded texture is reused).
    pub id: u64,
    pub grid: Arc<LocalToneGrid>,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
}

impl PartialEq for LocalToneOp {
    fn eq(&self, o: &Self) -> bool {
        self.id == o.id && self.highlights == o.highlights && self.shadows == o.shadows && self.whites == o.whites && self.blacks == o.blacks
    }
}

/// `rawgraph_core::ops::filter_low` (Gaussian approximation by three box passes, or one box pass),
/// or its high pass `0.5 + original - low`. `sigma` in pixels; the radius is `sigma.round()`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterOp {
    pub sigma: f32,
    pub box_kernel: bool,
    pub high: bool,
}

/// `rawgraph_fx::sharpen` without a control map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SharpenOp {
    pub amount: f32,
    pub radius_px: f32,
    pub threshold: f32,
}

/// One node of a GPU chain.
#[derive(Clone, Debug, PartialEq)]
pub enum GpuNode {
    /// Pointwise operations, applied in ONE pass.
    Point(Vec<PointOp>),
    LocalTone(LocalToneOp),
    Filter(FilterOp),
    Sharpen(SharpenOp),
}

impl GpuNode {
    /// Whether the node reads a neighbourhood (needs its own input texture).
    pub fn is_neighbourhood(&self) -> bool {
        !matches!(self, GpuNode::Point(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum NeighKind {
    Conv,
    Down,
    Resolve,
    LocalTone,
    SharpenLog,
    SharpenApply,
}

fn neigh_pass(ctx: &GpuContext, kind: NeighKind) -> Arc<Pass> {
    let mut cache = ctx.neigh_cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(p) = cache.get(&kind) {
        return Arc::clone(p);
    }
    let pre = shaders::prelude();
    let (label, wgsl, slots, extra) = match kind {
        NeighKind::Conv => ("rawgraph conv1d", shaders::CONV.to_string(), 10, 0),
        NeighKind::Down => ("rawgraph box downscale", shaders::DOWN.to_string(), 1, 0),
        NeighKind::Resolve => ("rawgraph blur resolve", shaders::RESOLVE.to_string(), 1, 1),
        NeighKind::LocalTone => ("rawgraph local tone", format!("{pre}{}", shaders::LOCAL_TONE), 2, 1),
        NeighKind::SharpenLog => ("rawgraph sharpen log", format!("{pre}{}", shaders::SHARPEN_LOG), 1, 0),
        NeighKind::SharpenApply => ("rawgraph sharpen apply", format!("{pre}{}", shaders::SHARPEN_APPLY), 2, 1),
    };
    let pass = Arc::new(build_pass_ext(ctx, label, &wgsl, wgpu::TextureFormat::Rgba32Float, slots * 16, None, extra));
    cache.insert(kind, Arc::clone(&pass));
    pass
}

/// The uploaded texture of a local tone grid (rewritten only when the id is new).
fn grid_image(ctx: &GpuContext, op: &LocalToneOp) -> Result<Arc<GpuImage>, GpuError> {
    let mut cache = ctx.grid_cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(pos) = cache.iter().position(|(id, _)| *id == op.id) {
        let entry = cache.remove(pos);
        let img = Arc::clone(&entry.1);
        cache.push(entry);
        return Ok(img);
    }
    let buf = ImageBuf::from_rgba(op.grid.width, op.grid.height, op.grid.coeff.clone())
        .ok_or_else(|| GpuError::InvalidArgument("local tone grid has the wrong size".into()))?;
    let img = Arc::new(GpuImage::upload(ctx, &buf)?);
    if cache.len() >= GRID_CACHE_MAX {
        cache.remove(0);
    }
    cache.push((op.id, Arc::clone(&img)));
    Ok(img)
}

/// Where an image of the chain lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loc {
    /// The base image (read only).
    Base,
    /// A scratch texture of the pool.
    Scratch(usize),
}

struct SmallPair {
    size: (u32, u32),
    a: GpuImage,
    b: GpuImage,
}

/// The textures a chain writes into: kept between frames, trimmed to what the last frame used.
#[derive(Default)]
pub struct ChainScratch {
    images: Vec<GpuImage>,
    small: Option<SmallPair>,
}

impl ChainScratch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Full-size scratch textures currently allocated.
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// Whether the low-resolution pair exists.
    pub fn has_small(&self) -> bool {
        self.small.is_some()
    }

    /// The image at `loc`.
    ///
    /// # Panics
    /// Never for a `Loc` that [`run_chain`] returned for this pool and base.
    pub fn resolve<'a>(&'a self, base: &'a GpuImage, loc: Loc) -> &'a GpuImage {
        match loc {
            Loc::Base => base,
            Loc::Scratch(i) => &self.images[i],
        }
    }
}

fn u32s(v: f32) -> u32 {
    v.to_bits()
}

fn box_taps(radius: usize) -> Vec<f32> {
    let n = 2 * radius + 1;
    vec![1.0 / n as f32; n]
}

/// `rawgraph_fx::sharpen`'s Gaussian taps for small radii (private there).
fn gaussian_taps(sigma: f32) -> Vec<f32> {
    let r = (3.0 * sigma).ceil().max(1.0) as i32;
    let mut taps: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// Radii below this sharpen with a true Gaussian (as `rawgraph_fx::sharpen`).
const GAUSS_MAX_SIGMA: f32 = 4.0;

struct Exec<'a> {
    ctx: &'a GpuContext,
    base: &'a GpuImage,
    sc: &'a mut ChainScratch,
    size: (u32, u32),
    used: usize,
    small_used: bool,
}

impl Exec<'_> {
    /// The lowest scratch texture not in `avoid` (allocated on first use).
    fn pick(&mut self, avoid: &[Loc]) -> Result<Loc, GpuError> {
        let mut i = 0;
        while avoid.contains(&Loc::Scratch(i)) {
            i += 1;
        }
        while self.sc.images.len() <= i {
            let img = GpuImage::new_empty(self.ctx, self.size.0, self.size.1)?;
            self.sc.images.push(img);
        }
        self.used = self.used.max(i + 1);
        Ok(Loc::Scratch(i))
    }

    fn image(&self, loc: Loc) -> &GpuImage {
        self.sc.resolve(self.base, loc)
    }

    fn ensure_small(&mut self, size: (u32, u32)) -> Result<(), GpuError> {
        self.small_used = true;
        if self.sc.small.as_ref().is_some_and(|s| s.size == size) {
            return Ok(());
        }
        // Free the old pair first so a change never needs both at once.
        self.sc.small = None;
        let a = GpuImage::new_empty(self.ctx, size.0, size.1)?;
        let b = GpuImage::new_empty(self.ctx, size.0, size.1)?;
        self.sc.small = Some(SmallPair { size, a, b });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        kind: NeighKind,
        src: &wgpu::TextureView,
        dst: &wgpu::TextureView,
        slots: &[[u32; 4]],
        extra: &[&wgpu::TextureView],
        size: (u32, u32),
    ) -> Result<(), GpuError> {
        let pass = neigh_pass(self.ctx, kind);
        run_pass_ext(self.ctx, &pass, src, dst, bytemuck::cast_slice(slots), None, extra, size)
    }

    fn conv_slots(axis: u32, taps: &[f32]) -> Vec<[u32; 4]> {
        debug_assert!(taps.len() <= MAX_TAPS && taps.len() % 2 == 1);
        let mut slots = vec![[axis, (taps.len() / 2) as u32, 0, 0]];
        for chunk in taps.chunks(4) {
            let mut s = [0u32; 4];
            for (d, t) in s.iter_mut().zip(chunk) {
                *d = u32s(*t);
            }
            slots.push(s);
        }
        slots.resize(10, [0; 4]);
        slots
    }

    fn conv(&self, src: Loc, dst: Loc, axis: u32, taps: &[f32]) -> Result<(), GpuError> {
        let slots = Self::conv_slots(axis, taps);
        self.dispatch(NeighKind::Conv, self.image(src).view(), self.image(dst).view(), &slots, &[], self.size)
    }

    /// Blurs `input` with a box (1 pass) or Gaussian-like (3 pass) kernel of integer `radius` >= 1 the way
    /// `rawgraph_core::ops::filter_low` does: exact at full resolution below `LOW_RES_MIN_RADIUS`,
    /// otherwise on a box-downscaled copy that is sampled back bilinearly. No texture in `keep` is
    /// written. With `high_of = Some(original)` the result is the high pass `0.5 + original - low`.
    fn blur(&mut self, input: Loc, keep: &[Loc], radius: usize, box_kernel: bool, high_of: Option<Loc>) -> Result<Loc, GpuError> {
        let passes = if box_kernel { 1 } else { 3 };
        let (w, h) = self.size;
        if radius < LOW_RES_MIN_RADIUS {
            let taps = box_taps(radius);
            let mut cur = input;
            for axis in [0u32, 1] {
                for _ in 0..passes {
                    let mut avoid = keep.to_vec();
                    avoid.push(cur);
                    let dst = self.pick(&avoid)?;
                    self.conv(cur, dst, axis, &taps)?;
                    cur = dst;
                }
            }
            return match high_of {
                None => Ok(cur),
                Some(orig) => self.resolve_full(cur, orig, keep),
            };
        }
        let scale = low_res_scale(radius) as u32;
        let low_radius = (radius as f32 / scale as f32).round().max(1.0) as usize;
        let (lw, lh) = (w.div_ceil(scale), h.div_ceil(scale));
        self.ensure_small((lw, lh))?;
        let taps = box_taps(low_radius);
        let final_is_a = {
            let (a, b) = match &self.sc.small {
                Some(s) => (&s.a, &s.b),
                None => return Err(GpuError::InvalidArgument("small pool missing".into())),
            };
            self.dispatch(NeighKind::Down, self.image(input).view(), a.view(), &[[scale, 0, 0, 0]], &[], (lw, lh))?;
            let mut in_a = true;
            for axis in [0u32, 1] {
                for _ in 0..passes {
                    let slots = Self::conv_slots(axis, &taps);
                    let (s, d) = if in_a { (a, b) } else { (b, a) };
                    self.dispatch(NeighKind::Conv, s.view(), d.view(), &slots, &[], (lw, lh))?;
                    in_a = !in_a;
                }
            }
            in_a
        };
        let mut avoid = keep.to_vec();
        if let Some(o) = high_of {
            avoid.push(o);
        }
        let out = self.pick(&avoid)?;
        let (a, b) = match &self.sc.small {
            Some(s) => (&s.a, &s.b),
            None => return Err(GpuError::InvalidArgument("small pool missing".into())),
        };
        let low = if final_is_a { a } else { b };
        let orig_view = match high_of {
            Some(o) => self.image(o).view(),
            None => low.view(),
        };
        let mode = u32::from(high_of.is_some());
        self.dispatch(NeighKind::Resolve, low.view(), self.image(out).view(), &[[scale, mode, 0, 0]], &[orig_view], (w, h))?;
        Ok(out)
    }

    /// High pass of an already full-size blurred image `low`: `0.5 + orig - low` into a new texture.
    fn resolve_full(&mut self, low: Loc, orig: Loc, keep: &[Loc]) -> Result<Loc, GpuError> {
        let mut avoid = keep.to_vec();
        avoid.extend([low, orig]);
        let out = self.pick(&avoid)?;
        self.dispatch(
            NeighKind::Resolve,
            self.image(low).view(),
            self.image(out).view(),
            &[[1, 1, 0, 0]],
            &[self.image(orig).view()],
            self.size,
        )?;
        Ok(out)
    }

    fn filter(&mut self, cur: Loc, f: &FilterOp) -> Result<Loc, GpuError> {
        // `ops::filter_low`: radius = sigma.round(), a radius of 0 is the identity.
        let radius = f.sigma.round().max(0.0) as usize;
        if radius == 0 {
            return if f.high { self.resolve_full(cur, cur, &[cur]) } else { Ok(cur) };
        }
        if f.high {
            self.blur(cur, &[cur], radius, f.box_kernel, Some(cur))
        } else {
            self.blur(cur, &[], radius, f.box_kernel, None)
        }
    }

    fn sharpen(&mut self, cur: Loc, s: &SharpenOp) -> Result<Loc, GpuError> {
        let amount = if s.amount.is_finite() { s.amount } else { 0.0 };
        let radius = if s.radius_px.is_finite() { s.radius_px } else { 0.0 };
        if amount == 0.0 || radius <= 0.0 {
            return Ok(cur);
        }
        let threshold = if s.threshold.is_finite() { s.threshold.max(0.0) } else { 0.0 };
        let log = self.pick(&[cur])?;
        self.dispatch(NeighKind::SharpenLog, self.image(cur).view(), self.image(log).view(), &[[0; 4]], &[], self.size)?;
        let blurred = if radius < GAUSS_MAX_SIGMA {
            let taps = gaussian_taps(radius);
            let mut at = log;
            for axis in [0u32, 1] {
                let dst = self.pick(&[cur, at])?;
                self.conv(at, dst, axis, &taps)?;
                at = dst;
            }
            at
        } else {
            self.blur(log, &[cur], radius.round().max(0.0) as usize, false, None)?
        };
        let out = self.pick(&[cur, blurred])?;
        let slots = [
            [u32s(amount), u32s(threshold), u32s(0.25 * threshold), u32s(0.75 * threshold)],
            [u32s(f32::INFINITY), 0, 0, 0],
        ];
        self.dispatch(
            NeighKind::SharpenApply,
            self.image(cur).view(),
            self.image(out).view(),
            &slots,
            &[self.image(blurred).view()],
            self.size,
        )?;
        Ok(out)
    }

    fn local_tone(&mut self, cur: Loc, op: &LocalToneOp) -> Result<Loc, GpuError> {
        let grid = grid_image(self.ctx, op)?;
        let out = self.pick(&[cur])?;
        let slots = [
            [u32s(op.highlights), u32s(op.shadows), u32s(op.whites), u32s(op.blacks)],
            [op.grid.width, op.grid.height, u32s(f32::INFINITY), 0],
        ];
        self.dispatch(NeighKind::LocalTone, self.image(cur).view(), self.image(out).view(), &slots, &[grid.view()], self.size)?;
        Ok(out)
    }

    fn node(&mut self, cur: Loc, node: &GpuNode) -> Result<Loc, GpuError> {
        match node {
            GpuNode::Point(ops) => {
                if ops.is_empty() {
                    return Ok(cur);
                }
                let out = self.pick(&[cur])?;
                apply_point_ops_into(self.ctx, self.image(cur), self.image(out), ops)?;
                Ok(out)
            }
            GpuNode::LocalTone(op) => self.local_tone(cur, op),
            GpuNode::Filter(f) => self.filter(cur, f),
            GpuNode::Sharpen(s) => self.sharpen(cur, s),
        }
    }
}

/// Runs `nodes` on `base` and returns where the result lives (`Loc::Base` when nothing changed).
/// The pool keeps only as many textures as this run needed.
pub fn run_chain(ctx: &GpuContext, base: &GpuImage, scratch: &mut ChainScratch, nodes: &[GpuNode]) -> Result<Loc, GpuError> {
    let (w, h) = base.size();
    check_size(ctx, w, h)?;
    let (cur, used, small_used) = {
        let mut ex = Exec { ctx, base, sc: scratch, size: (w, h), used: 0, small_used: false };
        let mut cur = Loc::Base;
        for node in nodes {
            cur = ex.node(cur, node)?;
        }
        (cur, ex.used, ex.small_used)
    };
    scratch.images.truncate(used);
    if !small_used {
        scratch.small = None;
    }
    Ok(cur)
}

/// Chain + display transform into `target` without a readback. Returns where the float result
/// (the scopes read it) lives.
pub fn render_chain_into(
    ctx: &GpuContext,
    base: &GpuImage,
    scratch: &mut ChainScratch,
    target: &DisplayTarget,
    nodes: &[GpuNode],
    exposure_ev: f32,
    tone: GpuToneMap,
) -> Result<Loc, GpuError> {
    let (w, h) = base.size();
    check_size(ctx, w, h)?;
    if target.size() != (w, h) {
        return Err(GpuError::SizeMismatch { expected: (w, h), actual: target.size() });
    }
    let loc = run_chain(ctx, base, scratch, nodes)?;
    display_into_view(ctx, scratch.resolve(base, loc).view(), target.egui_view(), (w, h), exposure_ev, tone)?;
    Ok(loc)
}

/// Runs `nodes` and reads the float result back (tests, tools).
pub fn apply_chain_to_cpu(ctx: &GpuContext, base: &GpuImage, nodes: &[GpuNode]) -> Result<ImageBuf, GpuError> {
    let mut scratch = ChainScratch::new();
    let loc = run_chain(ctx, base, &mut scratch, nodes)?;
    scratch.resolve(base, loc).download(ctx)
}
