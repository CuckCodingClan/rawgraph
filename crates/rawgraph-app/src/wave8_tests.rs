//! Tests of the wave-8 plumbing that do not belong to one module: the tail operations on the GPU,
//! the provider's catalog methods, the curve channels, the Develop groups, the node-mode export
//! and the new command line flags.

use std::sync::Arc;

use rawgraph_core::{neutral_curve, CaptureInfo, CurveEntry, Graph, ImageBuf, ImageId, Node, NodeData, NodeId, NodeKind};
use rawgraph_engine::{CancelToken, Evaluator, ImageProvider, TailOp};
use rawgraph_gpu::{GpuContext, PointOp};
use rawgraph_scopes::{DisplayTransform, ToneMap};

use crate::gpu_path::{BaseKey, RendererMode};
use crate::gpu_real::RealBackend;
use crate::gpu_session::{point_ops, Begin, DisplayArgs, GpuSession, Shown};
use crate::provider::FileProvider;
use crate::session::tests::{png_project, provider};
use crate::session::Session;

const IMG: ImageId = ImageId(1);

// ------------------------------------------------------------------ TailOp -> PointOp

#[test]
fn the_new_tail_operations_map_to_the_matching_point_operations() {
    let ops = [
        TailOp::WhitesBlacks { whites: 0.3, blacks: -0.2 },
        TailOp::Vibrance(0.4),
        TailOp::PointCurve { points: neutral_curve(4), channel: 4 },
        TailOp::PointCurve { points: neutral_curve(5), channel: 5 },
        TailOp::PointCurve { points: vec![[0.0, 0.5], [0.4, 0.1], [1.0, 0.5]], channel: 6 },
    ];
    let mapped = point_ops(&ops);
    assert_eq!(mapped.len(), 5);
    assert!(matches!(mapped[0], PointOp::WhitesBlacks { whites, blacks } if whites == 0.3 && blacks == -0.2));
    assert!(matches!(mapped[1], PointOp::Vibrance(a) if a == 0.4));
    for (i, channel) in [(2, 4u8), (3, 5), (4, 6)] {
        assert!(matches!(&mapped[i], PointOp::PointCurve { channel: c, .. } if *c == channel), "channel {channel}");
    }
    assert!(matches!(&mapped[4], PointOp::PointCurve { points, .. } if points.len() == 3), "the points travel with the operation");
}

// ------------------------------------------------------------------ GPU == CPU

/// A scene-referred picture with dark, mid and very bright, saturated and grey areas.
fn synthetic(w: u32, h: u32) -> ImageBuf {
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let fy = y as f32 / h as f32;
            let grey = (x / 8 + y / 8) % 5 == 0;
            let (r, g, b) = if grey { (fx * 2.0, fx * 2.0, fx * 2.0) } else { (fx * fx * 5.0 + 0.01, 0.02 + fy * 1.2, (fx * 0.6 + fy * 0.4).powf(2.0) * 0.9) };
            data.extend_from_slice(&[r, g, b, 1.0]);
        }
    }
    ImageBuf::from_rgba(w, h, data).expect("buffer size")
}

struct Synth(Arc<ImageBuf>);

impl ImageProvider for Synth {
    fn image(&self, _id: ImageId) -> Result<Arc<ImageBuf>, String> {
        Ok(self.0.clone())
    }
    fn capture(&self, _id: ImageId) -> Option<CaptureInfo> {
        Some(CaptureInfo { raw: Some(false), ..Default::default() })
    }
    fn images(&self) -> Vec<ImageId> {
        vec![IMG]
    }
}

/// Source -> Develop -> Curves -> Output with `setup` applied to the Develop and Curves nodes.
fn chain(setup: impl FnOnce(&mut Node, &mut Node)) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let dev = g.add_node(NodeKind::Develop, [0.0, 0.0]);
    let curves = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
    let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
    g.connect(src, dev, 0).unwrap();
    g.connect(dev, curves, 0).unwrap();
    g.connect(curves, out, 0).unwrap();
    let mut d = g.node(dev).unwrap().clone();
    let mut c = g.node(curves).unwrap().clone();
    setup(&mut d, &mut c);
    *g.node_mut(dev).unwrap() = d;
    *g.node_mut(curves).unwrap() = c;
    (g, out)
}

/// (largest difference in code values, share of differing values) between the GPU frame and the CPU
/// frame of `graph`; `None` without a GPU adapter. Also asserts the GPU really got a tail.
fn gpu_vs_cpu(graph: &Graph, out: NodeId, expect_tone_base: bool) -> Option<(u8, f64)> {
    let ctx = match GpuContext::new() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("note: no GPU adapter ({e}); the GPU comparison is skipped");
            return None;
        }
    };
    let provider = Synth(Arc::new(synthetic(97, 61)));
    let mut ev = Evaluator::new();
    let cancel = CancelToken::new();
    let cpu_img = ev.eval_image(graph, out, IMG, &provider, &cancel).expect("cpu frame");
    let transform = DisplayTransform { exposure_ev: 0.25, tone_map: ToneMap::AcesFit };
    let (_, cpu) = transform.apply_with_rgba8(&cpu_img);
    let plan = ev.plan_tail(graph, out, IMG, &provider).expect("plan");
    assert_eq!(plan.base_before_tone, expect_tone_base, "{:?}", plan.ops);
    let base = ev.eval_base(graph, &plan, IMG, &provider, &cancel).expect("base");
    let mut session = GpuSession::new(RealBackend::headless(ctx), RendererMode::Auto, None);
    let key = BaseKey { image: IMG, fingerprint: plan.base_fingerprint, preview: 97, revision: 0 };
    assert!(!matches!(session.begin(&key, 1, 0), Begin::Cpu { .. }));
    let display = DisplayArgs { exposure_ev: 0.25, tone: crate::gpu_session::gpu_tone(ToneMap::AcesFit) };
    let shown = session.show(&key, Some(&base), &plan.ops, display, 1, 0);
    assert!(matches!(shown, Shown::Gpu(_)), "{shown:?}");
    let gpu = session.read_current().expect("readback");
    assert_eq!(gpu.len(), cpu.len());
    let worst = gpu.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
    let differing = gpu.iter().zip(&cpu).filter(|(a, b)| a != b).count() as f64 / gpu.len() as f64;
    Some((worst, differing))
}

fn tone_values(dev: &mut Node) {
    for (name, v) in [("exposure", 0.5), ("contrast", 0.3), ("highlights", -0.35), ("shadows", 0.4), ("whites", 0.3), ("blacks", -0.25), ("vibrance", 0.45), ("saturation", 0.2)] {
        assert!(dev.set_param(name, IMG, v), "{name}");
    }
}

fn set_curve(c: &mut Node, channel: u8, points: Vec<[f32; 2]>) {
    let mut entries = match c.data(IMG) {
        Some(NodeData::CurveSet(e)) => e.clone(),
        _ => Vec::new(),
    };
    entries.push(CurveEntry { channel, points });
    assert!(c.set_data(IMG, Some(NodeData::CurveSet(entries))));
}

#[test]
fn develop_tone_values_run_on_the_gpu_like_on_the_cpu() {
    let (g, out) = chain(|d, _| tone_values(d));
    if let Some((worst, share)) = gpu_vs_cpu(&g, out, true) {
        assert!(worst <= 1, "Develop tone: largest difference {worst} code values ({share:.4} of the values differ)");
    }
}

#[test]
fn whites_blacks_and_dynamic_alone_run_on_the_gpu_like_on_the_cpu() {
    for (name, v) in [("whites", 0.5), ("blacks", -0.4), ("blacks", 0.4), ("vibrance", 0.6), ("vibrance", -0.5)] {
        let (g, out) = chain(|d, _| {
            d.set_param(name, IMG, v);
        });
        if let Some((worst, _)) = gpu_vs_cpu(&g, out, true) {
            assert!(worst <= 1, "{name} {v}: largest difference {worst}");
        }
    }
}

#[test]
fn every_curve_channel_runs_on_the_gpu_like_on_the_cpu() {
    let curves: [(u8, Vec<[f32; 2]>); 7] = [
        (0, vec![[0.0, 0.0], [0.3, 0.2], [0.65, 0.8], [1.0, 1.0]]),
        (1, vec![[0.0, 0.05], [0.5, 0.6], [1.0, 1.0]]),
        (2, vec![[0.0, 0.0], [0.4, 0.5], [1.0, 0.9]]),
        (3, vec![[0.0, 0.1], [0.6, 0.4], [1.0, 1.0]]),
        (4, vec![[0.0, 0.5], [0.3, 0.8], [0.7, 0.2], [1.0, 0.5]]),
        (5, vec![[0.0, 0.6], [0.3, 0.8], [0.7, 0.2], [1.0, 0.6]]),
        (6, vec![[0.0, 0.5], [0.15, 0.1], [0.5, 0.9], [1.0, 0.5]]),
    ];
    for (channel, points) in curves {
        let (g, out) = chain(|_, c| set_curve(c, channel, points.clone()));
        if let Some((worst, share)) = gpu_vs_cpu(&g, out, false) {
            assert!(worst <= 1, "curve channel {channel}: largest difference {worst} code values ({share:.4} differ)");
        }
    }
}

#[test]
fn tone_values_and_all_curve_channels_together_run_on_the_gpu_like_on_the_cpu() {
    let (g, out) = chain(|d, c| {
        tone_values(d);
        set_curve(c, 0, vec![[0.0, 0.0], [0.4, 0.3], [1.0, 1.0]]);
        set_curve(c, 4, vec![[0.0, 0.5], [0.5, 0.7], [1.0, 0.5]]);
        set_curve(c, 5, vec![[0.0, 0.55], [0.5, 0.3], [1.0, 0.55]]);
        set_curve(c, 6, vec![[0.0, 0.5], [0.6, 0.2], [1.0, 0.5]]);
    });
    if let Some((worst, share)) = gpu_vs_cpu(&g, out, true) {
        assert!(worst <= 1, "combined: largest difference {worst} code values ({share:.4} differ)");
    }
}

// ------------------------------------------------------------------ provider

#[test]
fn the_provider_lists_the_catalog_and_answers_for_the_collection_queries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.png");
    image::RgbaImage::from_pixel(64, 48, image::Rgba([10, 20, 30, 255])).save(&path).unwrap();
    let p = FileProvider::new(1 << 30);
    assert!(p.images().is_empty());
    p.set_paths([(ImageId(7), path.clone()), (ImageId(3), dir.path().join("b.png"))]);
    p.set_captures([(ImageId(7), CaptureInfo { iso: Some(100.0), ..Default::default() })]);
    assert_eq!(p.images(), vec![ImageId(7), ImageId(3)], "project order, as the paths were registered");
    p.set_paths([(ImageId(7), path.clone()), (ImageId(9), dir.path().join("c.png"))]);
    assert_eq!(p.images(), vec![ImageId(7), ImageId(3), ImageId(9)], "known images keep their place, none is listed twice");
    let cap = p.capture(ImageId(7)).unwrap();
    assert_eq!(cap.iso, Some(100.0));
    assert_eq!(cap.path.as_deref(), Some(path.to_string_lossy().as_ref()), "the capture carries the source path");
    assert!(p.capture(ImageId(3)).is_none(), "no capture registered, none invented");
    // The full size is known once the file was decoded.
    assert_eq!(p.full_size(ImageId(7)), None);
    p.image(ImageId(7)).unwrap();
    assert_eq!(p.full_size(ImageId(7)), Some((64, 48)));
    // A plain PNG has no lens profile; asking is cached.
    assert_eq!(p.lens_known(ImageId(7)), None);
    assert_eq!(p.lens_profile(ImageId(7)), None);
    assert_eq!(p.lens_known(ImageId(7)), Some(None), "remembered: the file has none");
    p.reset_paths();
    assert!(p.images().is_empty() && p.lens_known(ImageId(7)).is_none() && p.capture(ImageId(7)).is_none());
}

#[test]
fn the_lens_profile_is_read_on_a_worker_thread() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.png");
    image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 1, 1, 255])).save(&path).unwrap();
    let p = Arc::new(FileProvider::new(1 << 30));
    p.set_paths([(IMG, path)]);
    let ctx = eframe::egui::Context::default();
    assert_eq!(p.lens_known(IMG), None, "nothing has looked yet");
    p.request_lens(&ctx, IMG);
    let started = std::time::Instant::now();
    while p.lens_known(IMG).is_none() && started.elapsed().as_secs() < 10 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(p.lens_known(IMG), Some(None));
}

#[test]
fn a_provider_catalog_resolves_a_folder_source_without_the_ui() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(png_project(dir.path(), 4), provider());
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    assert!(crate::node_ui::set_source_folder(&mut g, src, Some(dir.path().to_string_lossy().into_owned())));
    let ev = Evaluator::new();
    assert_eq!(ev.members(&g, src, 0, &*s.provider).len(), 4, "the engine asks the provider's catalog");
    assert!(crate::node_ui::set_source_folder(&mut g, src, Some(dir.path().join("elsewhere").to_string_lossy().into_owned())));
    assert!(ev.members(&g, src, 0, &*s.provider).is_empty());
    let _ = s.id_captures();
}

// ------------------------------------------------------------------ curves and Develop

fn curves_node() -> (Graph, NodeId) {
    let mut g = Graph::new();
    let id = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
    (g, id)
}

#[test]
fn curve_channels_are_stored_as_a_set_and_old_single_curves_are_still_read() {
    let mut s = crate::session::tests::demo_session();
    let node = s.project.graph.add_node(NodeKind::PointCurve, [0.0, 0.0]);
    let img = s.current_image();
    let pts = vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]];
    crate::curve_editor::store_channel(&mut s, node, img, 1, pts.clone()).unwrap();
    let n = s.project.graph.node(node).unwrap();
    assert!(matches!(n.data(img), Some(NodeData::CurveSet(e)) if e.len() == 1 && e[0].channel == 1), "{:?}", n.data(img));
    assert_eq!(crate::curve_editor::curve_for(n, img, 1), pts);
    assert_eq!(crate::curve_editor::curve_for(n, img, 2), neutral_curve(2), "an untouched channel is neutral");
    assert_eq!(crate::curve_editor::curve_for(n, img, 5), vec![[0.0, 0.5], [1.0, 0.5]], "the colour channels are neutral at the middle");
    assert_eq!(rawgraph_core::curve_set(n, img).len(), 1, "the engine sees the same curve");

    // An older file: a single curve on the channel of the `channel` parameter.
    let (mut g, legacy) = curves_node();
    let n = g.node_mut(legacy).unwrap();
    n.set_param("channel", IMG, 2.0);
    assert!(n.set_data(IMG, Some(NodeData::Curve(pts.clone()))));
    let n = g.node(legacy).unwrap();
    assert_eq!(crate::curve_editor::curve_of(n, IMG), pts);
    assert_eq!(crate::curve_editor::modified_channels(n, IMG), [false, false, true, false, false, false, false]);
    assert_eq!(rawgraph_core::curve_set(n, IMG)[0].channel, 2);
    // Switching the tab keeps the old curve on its channel (it becomes a set first).
    s.project.graph = g;
    crate::curve_editor::select_channel(&mut s, legacy, IMG, 5);
    let n = s.project.graph.node(legacy).unwrap();
    assert!(matches!(n.data(IMG), Some(NodeData::CurveSet(_))));
    assert_eq!(crate::curve_editor::visible_channel(n, IMG), 5);
    assert_eq!(crate::curve_editor::curve_for(n, IMG, 2), pts, "still on the green channel");
    assert_eq!(crate::curve_editor::curve_of(n, IMG), neutral_curve(5));
}

#[test]
fn resetting_one_channel_keeps_the_others_and_the_tabs_show_which_are_changed() {
    let mut s = crate::session::tests::demo_session();
    let node = s.project.graph.add_node(NodeKind::PointCurve, [0.0, 0.0]);
    let img = s.current_image();
    let store = |s: &mut Session, c: u8, p: Vec<[f32; 2]>| crate::curve_editor::store_channel(s, node, img, c, p).unwrap();
    store(&mut s, 0, vec![[0.0, 0.0], [0.4, 0.3], [1.0, 1.0]]);
    store(&mut s, 3, vec![[0.0, 0.1], [1.0, 1.0]]);
    store(&mut s, 6, vec![[0.0, 0.5], [0.5, 0.1], [1.0, 0.5]]);
    let dots = |s: &Session| crate::curve_editor::modified_channels(s.project.graph.node(node).unwrap(), img);
    assert_eq!(dots(&s), [true, false, false, true, false, false, true]);
    // "Reset" of the visible curve: the neutral curve of that channel only.
    store(&mut s, 3, neutral_curve(3));
    assert_eq!(dots(&s), [true, false, false, false, false, false, true]);
    let n = s.project.graph.node(node).unwrap();
    let kept: Vec<u8> = rawgraph_core::curve_set(n, img).iter().map(|e| e.channel).collect();
    assert_eq!(kept, vec![0, 6], "the other curves are untouched");
    assert_eq!(crate::curve_editor::curve_for(n, img, 6)[1], [0.5, 0.1]);
    store(&mut s, 6, neutral_curve(6));
    store(&mut s, 0, neutral_curve(0));
    assert_eq!(dots(&s), [false; 7]);
    assert!(matches!(s.project.graph.node(node).unwrap().data(img), Some(NodeData::CurveSet(e)) if e.is_empty()), "an all-neutral set is stored empty");
    // The axis labels and readouts of the colour channels.
    assert_eq!(crate::curve_editor::y_axis_label(5), Some("\u{b1}60\u{b0}"));
    assert_eq!(crate::curve_editor::y_axis_label(4), Some("0 \u{2013} 2x"));
    assert_eq!(crate::curve_editor::y_axis_label(6), Some("0 \u{2013} 2x"));
    assert_eq!(crate::curve_editor::y_axis_label(0), None);
    assert_eq!(crate::curve_editor::y_readout(5, 0.75).as_deref(), Some("+30\u{b0}"));
    assert_eq!(crate::curve_editor::y_readout(6, 0.25).as_deref(), Some("x0.50"));
    assert_eq!(crate::curve_editor::y_readout(1, 0.25), None);
}

#[test]
fn develop_groups_reset_only_their_own_parameters() {
    let mut s = crate::session::tests::demo_session();
    let dev = s.project.graph.nodes().find(|n| n.kind == NodeKind::Develop).unwrap().id;
    let img = s.current_image();
    {
        let n = s.project.graph.node_mut(dev).unwrap();
        for (name, v) in [("exposure", 1.0), ("contrast", 0.5), ("whites", -0.4), ("saturation", 0.3), ("vibrance", 0.2), ("brightness", 2.0)] {
            assert!(n.set_param(name, img, v));
        }
    }
    let (groups, _) = crate::groups::param_groups(NodeKind::Develop);
    let tone = groups.iter().find(|g| g.title == "Tone").unwrap();
    assert!(crate::groups::group_changed(s.project.graph.node(dev).unwrap(), img, tone));
    crate::inspector::reset_group(&mut s, dev, img, tone);
    let n = s.project.graph.node(dev).unwrap();
    assert!(!crate::groups::group_changed(n, img, tone), "the Tone group is neutral again");
    assert_eq!((n.param("exposure", img), n.param("contrast", img), n.param("whites", img)), (0.0, 0.0, 0.0));
    assert_eq!((n.param("saturation", img), n.param("vibrance", img), n.param("brightness", img)), (0.3, 0.2, 2.0), "other groups keep their values");
    let colour = groups.iter().find(|g| g.title == "Colour").unwrap();
    crate::inspector::reset_group(&mut s, dev, img, colour);
    let n = s.project.graph.node(dev).unwrap();
    assert_eq!((n.param("saturation", img), n.param("vibrance", img), n.param("brightness", img)), (0.0, 0.0, 2.0));
    assert!(n.toggle("baseline", img));
}

// ------------------------------------------------------------------ node-mode export

#[test]
fn a_node_export_builds_the_catalog_and_the_selection_for_every_scope() {
    use crate::export::{node_members, node_plan, NodeExportState, Scope};
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    for i in 0..3 {
        image::RgbaImage::from_pixel(16, 16, image::Rgba([i * 40, 20, 30, 255])).save(a.join(format!("a{i}.png"))).unwrap();
    }
    for i in 0..2 {
        image::RgbaImage::from_pixel(16, 16, image::Rgba([i * 40, 90, 30, 255])).save(b.join(format!("b{i}.png"))).unwrap();
    }
    let mut project = rawgraph_project::Project::new();
    project.scan_folder(dir.path(), true, &rawgraph_io::is_supported, &|p| rawgraph_io::read_capture_info(p).ok());
    let mut s = Session::new(project, provider());
    let g = &mut s.project.graph;
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
    g.connect(src, export, 0).unwrap();
    assert!(crate::node_ui::set_source_folder(g, src, Some(a.to_string_lossy().into_owned())));
    let all_ids: Vec<ImageId> = s.project.images().iter().map(|e| e.id).collect();
    assert_eq!(all_ids.len(), 5);
    let in_a: Vec<ImageId> = s.project.images().iter().filter(|e| e.path.starts_with(&a)).map(|e| e.id).collect();
    let in_b: Vec<ImageId> = s.project.images().iter().filter(|e| e.path.starts_with(&b)).map(|e| e.id).collect();
    assert_eq!((in_a.len(), in_b.len()), (3, 2));

    assert_eq!(node_members(&mut s, export), in_a, "the node's collection is the folder of its Source");
    // Everything of the collection, whatever the filmstrip shows.
    s.set_rating(in_a[1], 5);
    s.set_rating(in_b[0], 5);
    s.filter.min_rating = 5;
    s.refresh_visible();
    s.select_id(in_a[1]);
    let (items, run) = node_plan(&mut s, export, Scope::All);
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), in_a);
    assert_eq!(run.catalog.len(), 5, "the catalog is the whole project (Layer and Merge need it)");
    assert_eq!(run.catalog.iter().map(|i| i.id).collect::<Vec<_>>(), all_ids, "in project order");
    assert!(items.iter().all(|i| run.catalog.iter().any(|c| c.id == i.id && c.source == i.source)), "items carry the source paths");
    // The filtered list: rated images of the filmstrip that are also in the collection.
    let (items, _) = node_plan(&mut s, export, Scope::Filtered);
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![in_a[1]], "the image of folder b is filtered in but not in the collection");
    // The selected image.
    let (items, _) = node_plan(&mut s, export, Scope::Selected);
    assert_eq!(items.iter().map(|i| i.id).collect::<Vec<_>>(), vec![in_a[1]]);
    s.select_id(in_b[0]);
    let (items, _) = node_plan(&mut s, export, Scope::Selected);
    assert!(items.is_empty(), "a selected image outside the node's collection is not exported");
    // The dialog's counts and its default scope.
    s.select_id(in_a[1]);
    let st = NodeExportState::open(&mut s, export);
    assert_eq!((st.counts, st.scope, st.count()), ((1, 1, 3), Scope::Selected, 1));
    s.filter.min_rating = 0;
    s.refresh_visible();
    s.select_id(in_b[0]);
    let st = NodeExportState::open(&mut s, export);
    assert_eq!((st.counts.0, st.scope), (0, Scope::Filtered), "no selected image in the collection: the filtered list is offered");
    assert_eq!(st.counts.1, 3);
    // The "Run Export nodes" list.
    let current = s.current_image();
    crate::node_ui::set_export_text(&mut s.project.graph, export, current, crate::node_ui::ExportField::OutDir, "D:/out").unwrap();
    let entries = crate::export::export_nodes(&s.project.graph, s.current_image());
    assert_eq!(entries.len(), 1);
    assert!(entries[0].run && entries[0].problem.is_none() && entries[0].label.contains("JPEG") && entries[0].label.contains("out"), "{entries:?}");
    let lonely = s.project.graph.add_node(NodeKind::Export, [0.0, 0.0]);
    let entries = crate::export::export_nodes(&s.project.graph, s.current_image());
    let e = entries.iter().find(|e| e.node == lonely).unwrap();
    assert!(!e.run && e.problem.as_deref() == Some("no output folder set"));
}

#[test]
fn an_export_job_through_a_node_writes_the_files_of_its_collection() {
    use crate::export::{node_plan, ExportJob, ExportMsg, Scope};
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(png_project(dir.path(), 3), provider());
    let out_dir = tempfile::tempdir().unwrap();
    let g = &mut s.project.graph;
    let out = g.nodes().find(|n| n.kind == NodeKind::Output).unwrap().id;
    let upstream = g.input_of(out, 0).unwrap();
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
    g.connect(upstream, export, 0).unwrap();
    let img = s.current_image();
    crate::node_ui::set_export_text(&mut s.project.graph, export, img, crate::node_ui::ExportField::OutDir, &out_dir.path().to_string_lossy()).unwrap();
    s.project.graph.node_mut(export).unwrap().set_param("format", img, 1.0);
    let (items, run) = node_plan(&mut s, export, Scope::All);
    assert_eq!(items.len(), 3);
    let job = ExportJob::start_node(eframe::egui::Context::default(), s.project.graph.clone(), export, items, run);
    assert_eq!(job.settings.out_dir, out_dir.path());
    let started = std::time::Instant::now();
    let report = loop {
        match job.rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(ExportMsg::Done(r)) => break r.expect("export ran"),
            Ok(_) => {}
            Err(_) if started.elapsed().as_secs() > 60 => panic!("the export did not finish"),
            Err(_) => {}
        }
    };
    assert_eq!((report.written(), report.failed()), (3, 0), "{:?}", report.items);
    let mut names: Vec<String> = std::fs::read_dir(out_dir.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    assert_eq!(names, vec!["img_000.png", "img_001.png", "img_002.png"]);
}

// ------------------------------------------------------------------ command line

#[test]
fn the_new_command_line_flags_parse() {
    use crate::cli::parse_args;
    let parse = |s: &str| parse_args(s.split_whitespace().map(str::to_owned));
    let cli = parse("--curve 7:huesat:0,0.5;0.5,0.1;1,0.5 --curve 7:0,0;1,1 --curve 3:5:0,0.5;1,0.5 --set-folder 0:D:/photos/a --set-text 9:folder=D:/out --set-text 9:description=a=b --run-export-node 9 --inspector-scroll 600").unwrap();
    assert_eq!(cli.curves.len(), 2);
    assert_eq!((cli.curves[0].0, cli.curves[0].1, cli.curves[0].2.len()), (7, 6, 3));
    assert_eq!((cli.curves[1].0, cli.curves[1].1), (3, 5));
    assert_eq!(cli.curve.len(), 1, "the older form still sets one curve");
    assert_eq!(cli.set_folder, vec![(0, std::path::PathBuf::from("D:/photos/a"))]);
    assert_eq!(cli.set_text, vec![(9, "folder".to_owned(), "D:/out".to_owned()), (9, "description".to_owned(), "a=b".to_owned())]);
    assert_eq!(cli.run_export_node, Some(9));
    assert_eq!(cli.inspector_scroll, Some(600.0));
    for bad in ["--curve 7:nope:0,0;1,1", "--curve 7:9:0,0;1,1", "--set-folder 0", "--set-folder x:D:/a", "--set-text 1:colour=red", "--set-text 1:folder", "--run-export-node", "--inspector-scroll -3"] {
        assert!(parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_new_flags_set_the_state_of_the_graph() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(png_project(dir.path(), 2), provider());
    let n = s.project.graph.nodes().count();
    let cli = crate::cli::parse_args(
        [
            "--add-node".to_owned(),
            "Curves".to_owned(),
            "--add-node".to_owned(),
            "Export".to_owned(),
            "--curve".to_owned(),
            format!("{n}:huehue:0,0.7;0.5,0.2;1,0.4"),
            "--set-text".to_owned(),
            format!("{}:folder=D:/out", n + 1),
            "--set-folder".to_owned(),
            format!("0:{}", dir.path().display()),
        ],
    )
    .unwrap();
    crate::app::apply_cli_state(&mut s, &cli);
    let curves = s.project.graph.node_by_kind(NodeKind::PointCurve);
    let img = s.current_image();
    let c = s.project.graph.node(curves).unwrap();
    assert_eq!(crate::curve_editor::visible_channel(c, img), 5, "that tab is shown");
    let pts = crate::curve_editor::curve_for(c, img, 5);
    assert_eq!(pts.first().map(|p| p[1]), pts.last().map(|p| p[1]), "the hue curve wraps: both ends at the height of the first");
    let export = s.project.graph.node_by_kind(NodeKind::Export);
    assert_eq!(rawgraph_core::export_spec(s.project.graph.node(export).unwrap(), img).text.out_dir, "D:/out");
    let src = s.project.graph.node_by_kind(NodeKind::Source);
    assert_eq!(crate::node_ui::source_folder(s.project.graph.node(src).unwrap()).as_deref(), Some(dir.path().to_string_lossy().as_ref()));
}

trait ByKind {
    fn node_by_kind(&self, kind: NodeKind) -> NodeId;
}

impl ByKind for Graph {
    fn node_by_kind(&self, kind: NodeKind) -> NodeId {
        self.nodes().find(|n| n.kind == kind).map(|n| n.id).expect("a node of that kind")
    }
}

#[test]
fn an_export_never_targets_a_folder_of_source_photos() {
    use crate::export::{node_blocker, writes_into_sources};
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(png_project(dir.path(), 2), provider());
    let catalog = crate::export::catalog_items(&s);
    let folder = dir.path().to_string_lossy().into_owned();
    assert!(writes_into_sources(&catalog, &folder));
    assert!(writes_into_sources(&catalog, &format!("{}\\", folder.to_uppercase())), "case and a trailing separator do not matter");
    assert!(!writes_into_sources(&catalog, &dir.path().join("out").to_string_lossy()), "a sub-folder of the sources is another folder");
    assert!(!writes_into_sources(&catalog, ""));
    let export = {
        let g = &mut s.project.graph;
        let out = g.nodes().find(|n| n.kind == NodeKind::Output).unwrap().id;
        let up = g.input_of(out, 0).unwrap();
        let e = g.add_node(NodeKind::Export, [0.0, 0.0]);
        g.connect(up, e, 0).unwrap();
        e
    };
    assert_eq!(node_blocker(&s, export).as_deref(), Some("The Export node has no output folder"));
    let img = s.current_image();
    crate::node_ui::set_export_text(&mut s.project.graph, export, img, crate::node_ui::ExportField::OutDir, &folder).unwrap();
    assert!(node_blocker(&s, export).is_some_and(|m| m.contains("source photos")));
    crate::node_ui::set_export_text(&mut s.project.graph, export, img, crate::node_ui::ExportField::OutDir, &dir.path().join("out").to_string_lossy()).unwrap();
    assert_eq!(node_blocker(&s, export), None);
}
