//! The GPU chain of the preview: neighbourhood operations (Develop local tone, Filter, Sharpen) next to
//! the pointwise ones. The first group runs on the fake backend (rules of the session: fusing,
//! fallback, cache), the second on the real GPU against the CPU frame (skipped without an adapter).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use rawgraph_core::ops::LocalToneGrid;
use rawgraph_core::{CaptureInfo, Graph, ImageBuf, ImageId, NodeId, NodeKind};
use rawgraph_engine::{CancelToken, EvalError, Evaluator, ImageProvider, TailOp};
use rawgraph_gpu::{GpuContext, GpuError, GpuNode, GpuToneMap, PointOp};
use rawgraph_scopes::{DisplayTransform, ToneMap};

use crate::gpu_path::{BaseKey, RendererMode};
use crate::gpu_real::RealBackend;
use crate::gpu_session::fake::FakeBackend;
use crate::gpu_session::{gpu_nodes, gpu_tone, Begin, DisplayArgs, GpuSession, Shown};

const IMG: ImageId = ImageId(1);

fn key(fingerprint: u64) -> BaseKey {
    BaseKey { image: IMG, fingerprint, preview: 64, revision: 0 }
}

fn display() -> DisplayArgs {
    DisplayArgs { exposure_ev: 0.0, tone: GpuToneMap::AcesFit }
}

fn grid() -> Arc<LocalToneGrid> {
    Arc::new(LocalToneGrid { width: 2, height: 2, coeff: vec![0.5; 16] })
}

fn local_tone(id: u64) -> TailOp {
    TailOp::LocalTone { highlights: -0.4, shadows: 0.3, whites: 0.0, blacks: 0.1, grid: id }
}

fn filter(radius: f32) -> TailOp {
    TailOp::Filter { radius, box_kernel: false, high: false }
}

// ------------------------------------------------------------------ the conversion

#[test]
fn pointwise_runs_are_fused_and_neighbourhood_operations_are_their_own_nodes() {
    let ops = vec![
        TailOp::Gain(2.0),
        TailOp::Saturation(1.2),
        filter(10.0),
        TailOp::Gain(0.5),
        TailOp::Sharpen { amount: 0.5, radius: 2.0, threshold: 0.1 },
        TailOp::Contrast { amount: 0.2, pivot: 0.18 },
        TailOp::Vibrance(0.3),
        local_tone(9),
    ];
    let grids = [None, None, None, None, None, None, None, Some(grid())];
    let nodes = gpu_nodes(&ops, &grids, (400, 250)).unwrap();
    assert_eq!(nodes.len(), 6, "{nodes:?}");
    assert!(matches!(&nodes[0], GpuNode::Point(p) if p.len() == 2 && p[0] == PointOp::Gain(2.0)));
    // Radii are per mille of the long edge of the base: 10 per mille of 400 px = 4 px.
    assert!(matches!(&nodes[1], GpuNode::Filter(f) if f.sigma == 4.0 && !f.box_kernel && !f.high));
    assert!(matches!(&nodes[2], GpuNode::Point(p) if p == &[PointOp::Gain(0.5)]));
    assert!(matches!(&nodes[3], GpuNode::Sharpen(s) if s.radius_px == 0.8 && s.amount == 0.5 && s.threshold == 0.1));
    assert!(matches!(&nodes[4], GpuNode::Point(p) if p.len() == 2));
    assert!(matches!(&nodes[5], GpuNode::LocalTone(l) if l.id == 9 && l.highlights == -0.4 && l.blacks == 0.1));
    // The long edge decides, not the width.
    let tall = gpu_nodes(&[filter(10.0)], &[None], (250, 400)).unwrap();
    assert!(matches!(&tall[0], GpuNode::Filter(f) if f.sigma == 4.0));
    assert!(gpu_nodes(&[], &[], (4, 4)).unwrap().is_empty());
}

#[test]
fn a_local_tone_operation_without_its_grid_is_an_error_not_a_wrong_picture() {
    for grids in [vec![None], vec![], vec![None, None]] {
        assert!(matches!(gpu_nodes(&[local_tone(1)], &grids, (8, 8)), Err(GpuError::InvalidArgument(_))), "{grids:?}");
    }
}

// ------------------------------------------------------------------ the session rules on a fake backend

fn session() -> GpuSession<FakeBackend> {
    GpuSession::new(FakeBackend::new(), RendererMode::Auto, None)
}

fn base() -> ImageBuf {
    ImageBuf::new(64, 4)
}

#[test]
fn the_chain_reaches_the_backend_and_the_base_stays_cached_while_the_tail_changes() {
    let mut s = session();
    let k = key(1);
    assert_eq!(s.begin(&k, 1, 0), Begin::NeedBase);
    let ops = [TailOp::Gain(2.0), filter(5.0), local_tone(3)];
    let grids = [None, None, Some(grid())];
    assert!(matches!(s.show_with(&k, Some(&base()), &ops, &grids, display(), 1, 0), Shown::Gpu(_)));
    // Dragging a slider of the tail: the base is on the GPU, nothing is uploaded again.
    assert_eq!(s.begin(&k, 2, 1), Begin::HaveBase);
    let ops2 = [TailOp::Gain(2.0), filter(9.0), local_tone(3)];
    assert!(matches!(s.show_with(&k, None, &ops2, &grids, display(), 2, 1), Shown::Gpu(_)));
    assert_eq!((s.stats.uploads, s.stats.renders), (1, 2));
    let renders = &s.backend().renders;
    assert_eq!(renders.len(), 2);
    assert_eq!(renders[1].1.len(), 3, "gain, filter, local tone: three nodes");
    // A changed base (different fingerprint) is a new base: evaluated and uploaded again.
    assert_eq!(s.begin(&key(2), 3, 2), Begin::NeedBase);
    assert!(matches!(s.show_with(&key(2), Some(&base()), &ops, &grids, display(), 3, 2), Shown::Gpu(_)));
    assert_eq!(s.stats.uploads, 2);
}

#[test]
fn a_missing_grid_falls_back_to_the_cpu_for_that_frame_and_the_gpu_stays_on() {
    let mut s = session();
    let k = key(1);
    assert_eq!(s.begin(&k, 1, 0), Begin::NeedBase);
    let shown = s.show_with(&k, Some(&base()), &[local_tone(3)], &[None], display(), 1, 0);
    assert!(matches!(&shown, Shown::Cpu { reason, .. } if reason.contains("GPU error") && reason.contains("grid")), "{shown:?}");
    // Not an out-of-memory case: the base stays, the next good frame is a GPU frame.
    let shown = s.show_with(&k, None, &[local_tone(3)], &[Some(grid())], display(), 2, 1);
    assert!(matches!(shown, Shown::Gpu(_)), "{shown:?}");
}

#[test]
fn out_of_memory_while_running_a_chain_uses_the_cpu_and_releases_everything() {
    let mut s = session();
    let k = key(1);
    assert_eq!(s.begin(&k, 1, 0), Begin::NeedBase);
    s.backend_mut().fail_render = Some(GpuError::OutOfMemory("scratch texture".into()));
    let shown = s.show_with(&k, Some(&base()), &[filter(5.0)], &[None], display(), 1, 0);
    assert!(matches!(&shown, Shown::Cpu { reason, .. } if reason.contains("out of memory")), "{shown:?}");
    assert!(s.cached_bases().is_empty(), "the rules of the pointwise path apply: everything is released");
    assert_eq!(s.backend().live_images.get(), 0);
    // The same image is then held on the CPU (blocked for this size), other pictures may retry.
    assert!(matches!(s.begin(&k, 2, 1), Begin::Cpu { .. }));
}

#[test]
fn strict_gpu_mode_reports_a_failed_chain_instead_of_falling_back() {
    let mut s = GpuSession::new(FakeBackend::new(), RendererMode::Gpu, None);
    let k = key(1);
    assert_eq!(s.begin(&k, 1, 0), Begin::NeedBase);
    let shown = s.show_with(&k, Some(&base()), &[local_tone(3)], &[None], display(), 1, 0);
    assert!(matches!(shown, Shown::Failed(_)), "{shown:?}");
}

// ------------------------------------------------------------------ real GPU against the CPU

struct Synth {
    image: std::sync::Mutex<Arc<ImageBuf>>,
    revision: AtomicU64,
}

impl ImageProvider for Synth {
    fn image(&self, _id: ImageId) -> Result<Arc<ImageBuf>, String> {
        Ok(self.image.lock().unwrap().clone())
    }
    fn revision(&self, _id: ImageId) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }
    fn capture(&self, _id: ImageId) -> Option<CaptureInfo> {
        Some(CaptureInfo { raw: Some(false), ..Default::default() })
    }
    fn images(&self) -> Vec<ImageId> {
        vec![IMG]
    }
}

/// Detail, a bright sky over dark ground, an edge and saturated colours.
fn synthetic(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let area = if fy < 0.45 { 0.3 + 3.0 * (1.0 - fy) } else { 0.02 + 0.1 * fx };
            let edge = if fx > 0.55 && fy > 0.3 && fy < 0.8 { 4.0 } else { 1.0 };
            let texture = 1.0 + 0.3 * ((x as f32 * 0.9).sin() * (y as f32 * 1.1).cos());
            let v = area * edge * texture;
            img.set_pixel(x, y, [v, v * (0.4 + 0.6 * fx), v * (0.3 + 0.7 * fy), 1.0]);
        }
    }
    img
}

/// Source -> the given nodes -> Output.
fn graph(specs: &[(NodeKind, &[(&str, f32)])]) -> (Graph, Vec<NodeId>, NodeId) {
    let mut g = Graph::new();
    let mut ids = vec![g.add_node(NodeKind::Source, [0.0, 0.0])];
    for (kind, params) in specs {
        let n = g.add_node(*kind, [0.0, 0.0]);
        g.connect(*ids.last().unwrap(), n, 0).unwrap();
        for (name, v) in *params {
            assert!(g.node_mut(n).unwrap().set_param(name, IMG, *v), "{name}");
        }
        ids.push(n);
    }
    let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
    g.connect(*ids.last().unwrap(), out, 0).unwrap();
    (g, ids, out)
}

struct Rig {
    provider: Synth,
    ev: Evaluator,
    session: GpuSession<RealBackend>,
    generation: u64,
}

impl Rig {
    fn new(ctx: Arc<GpuContext>) -> Self {
        Self::sized(ctx, 200, 120)
    }

    fn sized(ctx: Arc<GpuContext>, w: u32, h: u32) -> Self {
        Self {
            provider: Synth { image: std::sync::Mutex::new(Arc::new(synthetic(w, h))), revision: AtomicU64::new(0) },
            ev: Evaluator::new(),
            session: GpuSession::new(RealBackend::headless(ctx), RendererMode::Auto, None),
            generation: 0,
        }
    }

    /// Renders `out` of `g` on the GPU the way the render worker does and returns the largest
    /// difference to the CPU frame in code values together with the plan.
    fn frame(&mut self, g: &Graph, out: NodeId) -> (u8, rawgraph_engine::TailPlan) {
        self.generation += 1;
        let cancel = CancelToken::new();
        let plan = self.ev.plan_tail_gpu(g, out, IMG, &self.provider).expect("plan");
        let k = BaseKey { image: IMG, fingerprint: plan.base_fingerprint, preview: self.provider.image.lock().unwrap().width(), revision: self.provider.revision.load(Ordering::Relaxed) };
        let base = match self.session.begin(&k, self.generation, 0) {
            Begin::Cpu { reason, .. } => panic!("unexpected CPU frame: {reason}"),
            Begin::HaveBase => None,
            Begin::NeedBase => Some(self.ev.eval_base(g, &plan, IMG, &self.provider, &cancel).expect("base")),
        };
        let mut grids = Vec::new();
        for (i, op) in plan.ops.iter().enumerate() {
            grids.push(match op {
                TailOp::LocalTone { .. } => Some(self.ev.tail_grid(g, &plan, i, IMG, &self.provider, &cancel).expect("grid")),
                _ => None,
            });
        }
        let args = DisplayArgs { exposure_ev: 0.25, tone: gpu_tone(ToneMap::AcesFit) };
        let shown = self.session.show_with(&k, base.as_deref(), &plan.ops, &grids, args, self.generation, 0);
        assert!(matches!(shown, Shown::Gpu(_)), "{shown:?}");
        let gpu = self.session.read_current().expect("readback");
        let cpu_img = Evaluator::new().eval_image(g, out, IMG, &self.provider, &cancel).expect("cpu frame");
        let (_, cpu) = DisplayTransform { exposure_ev: 0.25, tone_map: ToneMap::AcesFit }.apply_with_rgba8(&cpu_img);
        assert_eq!(gpu.len(), cpu.len());
        (gpu.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0), plan)
    }
}

fn rig() -> Option<(Rig, std::sync::MutexGuard<'static, ()>)> {
    let lock = crate::gpu_real::gpu_test_lock();
    match GpuContext::new() {
        Ok(ctx) => Some((Rig::new(Arc::new(ctx)), lock)),
        Err(e) => {
            eprintln!("note: no GPU adapter ({e}); the GPU comparison is skipped");
            None
        }
    }
}

#[test]
fn develop_local_tone_runs_on_the_gpu_and_sliders_do_not_touch_the_base() {
    let Some((mut rig, _lock)) = rig() else { return };
    let (mut g, ids, out) = graph(&[(
        NodeKind::Develop,
        &[("exposure", 0.4), ("highlights", -0.6), ("shadows", 0.5), ("whites", 0.3), ("blacks", -0.25), ("contrast", 0.2), ("saturation", 0.2)],
    )]);
    let (worst, plan) = rig.frame(&g, out);
    assert!(plan.base_before_tone && plan.ops.iter().any(|o| matches!(o, TailOp::LocalTone { .. })), "{:?}", plan.ops);
    assert!(worst <= 1, "local tone: largest difference {worst} code values");
    // Drag Lights and Shadows: the same base and grid, nothing is uploaded again, still the CPU picture.
    for (name, v) in [("highlights", -0.2), ("shadows", 0.9), ("highlights", 0.5), ("blacks", 0.6)] {
        assert!(g.node_mut(ids[1]).unwrap().set_param(name, IMG, v));
        let (worst, _) = rig.frame(&g, out);
        assert!(worst <= 1, "{name} {v}: largest difference {worst}");
    }
    assert_eq!(rig.session.stats.uploads, 1, "the base was uploaded once");
    // Exposure changes what the grid is estimated from: a new grid, the base is still the same.
    assert!(g.node_mut(ids[1]).unwrap().set_param("exposure", IMG, -0.5));
    let (worst, _) = rig.frame(&g, out);
    assert!(worst <= 1, "after an exposure change: {worst}");
    assert_eq!(rig.session.stats.uploads, 1);
    // A new source revision is a new base.
    rig.provider.revision.store(1, Ordering::Relaxed);
    let (worst, _) = rig.frame(&g, out);
    assert!(worst <= 1);
    assert_eq!(rig.session.stats.uploads, 2, "a changed base is uploaded again");
}

#[test]
fn filter_and_sharpen_after_a_local_tone_match_the_cpu() {
    let Some((mut rig, _lock)) = rig() else { return };
    let (g, _, out) = graph(&[
        (NodeKind::Develop, &[("highlights", -0.5), ("shadows", 0.4)]),
        (NodeKind::Blur, &[("radius", 40.0)]),
        (NodeKind::Sharpen, &[("amount", 0.8), ("radius", 2.0), ("threshold", 0.1)]),
        (NodeKind::Exposure, &[("ev", 0.3)]),
    ]);
    let (worst, plan) = rig.frame(&g, out);
    let kinds: Vec<bool> = plan.ops.iter().map(TailOp::is_neighbourhood).collect();
    assert_eq!(kinds, vec![true, true, true, false], "{:?}", plan.ops);
    assert!(worst <= 1, "largest difference {worst} code values");
}

#[test]
fn every_filter_variant_matches_the_cpu() {
    let Some((mut rig, _lock)) = rig() else { return };
    for (radius, kernel, pass) in [(10.0, 0.0, 0.0), (10.0, 1.0, 0.0), (10.0, 0.0, 1.0), (60.0, 0.0, 0.0), (60.0, 1.0, 1.0), (0.0, 0.0, 1.0)] {
        let (g, _, out) = graph(&[(NodeKind::Blur, &[("radius", radius), ("kernel", kernel), ("pass", pass)]), (NodeKind::Saturation, &[("amount", 1.2)])]);
        let (worst, plan) = rig.frame(&g, out);
        assert!(plan.ops.iter().any(|o| matches!(o, TailOp::Filter { .. })));
        assert!(worst <= 1, "radius {radius} kernel {kernel} pass {pass}: largest difference {worst} code values");
    }
}

#[test]
fn an_unsupported_node_ends_the_chain_and_the_frame_still_matches() {
    let Some((mut rig, _lock)) = rig() else { return };
    // Clarity is a CPU base; the Filter and Sharpen above it are the GPU tail.
    let (g, ids, out) = graph(&[
        (NodeKind::Develop, &[("highlights", -0.5), ("shadows", 0.4)]),
        (NodeKind::Clarity, &[("clarity", 0.5)]),
        (NodeKind::Blur, &[("radius", 30.0)]),
        (NodeKind::Sharpen, &[("amount", 0.5), ("radius", 2.0)]),
    ]);
    let (worst, plan) = rig.frame(&g, out);
    assert_eq!(plan.base, ids[2], "Clarity is the base");
    assert_eq!(plan.ops.len(), 2);
    assert!(worst <= 1, "largest difference {worst} code values");
}

/// Slider -> frame, CPU against GPU, without the window: `RG_BENCH_EDGE=1280 cargo test -p
/// rawgraph-app --release slider_to_frame -- --ignored --nocapture`. The CPU time is the evaluator
/// recomputing from the changed node on (cached upstream), the GPU time is plan + grid lookup + the
/// chain + display + `wait_idle` (no readback).
#[test]
#[ignore = "timing report"]
fn slider_to_frame_cpu_against_gpu() {
    let edge: u32 = std::env::var("RG_BENCH_EDGE").ok().and_then(|v| v.parse().ok()).unwrap_or(1280);
    let Ok(ctx) = GpuContext::new() else { return };
    let _lock = crate::gpu_real::gpu_test_lock();
    let mut rig = Rig::sized(Arc::new(ctx), edge, edge * 2 / 3);
    let median = |mut v: Vec<f32>| {
        v.sort_by(f32::total_cmp);
        v[v.len() / 2]
    };
    type Spec<'a> = (&'a str, Vec<(NodeKind, &'a [(&'a str, f32)])>, usize, &'a str, [f32; 2]);
    let cases: Vec<Spec> = vec![
        ("Develop Lights", vec![(NodeKind::Develop, &[("shadows", 0.3)])], 1, "highlights", [-0.8, 0.8]),
        ("Filter radius", vec![(NodeKind::Blur, &[("radius", 8.0)])], 1, "radius", [6.0, 24.0]),
        ("Sharpen amount", vec![(NodeKind::Sharpen, &[("amount", 0.3), ("radius", 1.0)])], 1, "amount", [0.2, 1.0]),
    ];
    for (name, specs, node, param, [lo, hi]) in cases {
        let (mut g, ids, out) = graph(&specs);
        rig.frame(&g, out);
        let (mut cpu, mut gpu) = (Vec::new(), Vec::new());
        let mut ev = Evaluator::new();
        let cancel = CancelToken::new();
        ev.eval_image(&g, out, IMG, &rig.provider, &cancel).unwrap();
        for i in 0..9 {
            let v = lo + (hi - lo) * i as f32 / 8.0;
            assert!(g.node_mut(ids[node]).unwrap().set_param(param, IMG, v));
            let t = std::time::Instant::now();
            let _ = ev.eval_image(&g, out, IMG, &rig.provider, &cancel).unwrap();
            cpu.push(t.elapsed().as_secs_f32() * 1000.0);
            let t = std::time::Instant::now();
            rig.generation += 1;
            let plan = rig.ev.plan_tail_gpu(&g, out, IMG, &rig.provider).unwrap();
            let k = BaseKey { image: IMG, fingerprint: plan.base_fingerprint, preview: edge, revision: 0 };
            let mut grids = Vec::new();
            for (i, op) in plan.ops.iter().enumerate() {
                grids.push(matches!(op, TailOp::LocalTone { .. }).then(|| rig.ev.tail_grid(&g, &plan, i, IMG, &rig.provider, &cancel).unwrap()));
            }
            let args = DisplayArgs { exposure_ev: 0.0, tone: gpu_tone(ToneMap::AcesFit) };
            let _ = rig.session.begin(&k, rig.generation, 0);
            let shown = rig.session.show_with(&k, None, &plan.ops, &grids, args, rig.generation, 0);
            assert!(matches!(shown, Shown::Gpu(_)), "{shown:?}");
            gpu.push(t.elapsed().as_secs_f32() * 1000.0);
        }
        println!("{edge} px {name}: CPU median {:.1} ms, GPU median {:.1} ms", median(cpu), median(gpu));
    }
}


// ------------------------------------------------------------------ memory rules

#[test]
fn an_idle_display_class_gives_its_scratch_textures_back_and_the_backend_is_warmed_up() {
    let mut s = session();
    assert!(!s.backend().warmed_up, "a new session compiles nothing (it may be made on the UI thread)");
    s.warm_up();
    assert!(s.backend().warmed_up, "the render worker compiles the pipelines when it starts");
    let (a, b) = (key(1), BaseKey { image: ImageId(2), fingerprint: 2, preview: 64, revision: 0 });
    let wide = ImageBuf::new(80, 4);
    assert_eq!(s.begin(&a, 1, 0), Begin::NeedBase);
    assert!(matches!(s.show_with(&a, Some(&base()), &[filter(5.0)], &[None], display(), 1, 0), Shown::Gpu(_)));
    assert!(s.backend().trims.is_empty(), "the only class is the one that renders");
    assert_eq!(s.begin(&b, 2, 1), Begin::NeedBase);
    assert!(matches!(s.show_with(&b, Some(&wide), &[filter(5.0)], &[None], display(), 2, 1), Shown::Gpu(_)));
    assert_eq!(s.backend().trims, vec![(64, 4)], "the 64 px class was idle while the 80 px one rendered");
    // Rendering the 64 px picture again trims the 80 px class in turn.
    assert!(matches!(s.show_with(&a, None, &[filter(5.0)], &[None], display(), 3, 2), Shown::Gpu(_)));
    assert_eq!(s.backend().trims.last(), Some(&(80, 4)));
}

#[test]
fn stepping_a_b_a_and_changing_the_preview_size_keep_the_local_tone_right() {
    let Some((mut rig, _lock)) = rig() else { return };
    let (g, _, out) = graph(&[(NodeKind::Develop, &[("highlights", -0.6), ("shadows", 0.5), ("exposure", 0.3)])]);
    let (a, b) = (Arc::new(synthetic(200, 120)), Arc::new(synthetic(160, 96)));
    let set = |rig: &Rig, img: &Arc<ImageBuf>, rev: u64| {
        *rig.provider.image.lock().unwrap() = img.clone();
        rig.provider.revision.store(rev, Ordering::Relaxed);
    };
    for (img, rev) in [(&a, 0), (&b, 1), (&a, 2), (&b, 3)] {
        set(&rig, img, rev);
        let (worst, plan) = rig.frame(&g, out);
        assert!(plan.ops.iter().any(|o| matches!(o, TailOp::LocalTone { .. })));
        assert!(worst <= 1, "preview {} px: largest difference {worst}", img.width());
    }
    // The evaluator kept the grid of the first picture across the steps: asking again costs no evaluation.
    set(&rig, &a, 0);
    let plan = rig.ev.plan_tail_gpu(&g, out, IMG, &rig.provider).unwrap();
    let misses = rig.ev.misses();
    let i = plan.ops.iter().position(|o| matches!(o, TailOp::LocalTone { .. })).unwrap();
    rig.ev.tail_grid(&g, &plan, i, IMG, &rig.provider, &CancelToken::new()).unwrap();
    assert_eq!(rig.ev.misses(), misses, "the grid of A came from the cache");
}
#[test]
fn a_filter_with_a_connected_radius_map_is_a_cpu_base() {
    // No GPU needed: this is the planner.
    let (mut g, ids, out) = graph(&[(NodeKind::Blur, &[("radius", 30.0)])]);
    let map = g.add_node(NodeKind::Generator, [0.0, 0.0]);
    let port = g.expose_parameter(ids[1], "radius", rawgraph_core::PortType::Images).unwrap();
    g.connect(map, ids[1], port).unwrap();
    let provider = Synth { image: std::sync::Mutex::new(Arc::new(synthetic(32, 32))), revision: AtomicU64::new(0) };
    let plan = Evaluator::new().plan_tail_gpu(&g, out, IMG, &provider).unwrap();
    assert_eq!((plan.base, plan.ops.len()), (ids[1], 0));
    // And asking for the grid of an operation that is not a local tone is an error.
    let err = Evaluator::new().tail_grid(&g, &plan, 0, IMG, &provider, &CancelToken::new());
    assert!(matches!(err, Err(EvalError::WrongType { .. })));
}
