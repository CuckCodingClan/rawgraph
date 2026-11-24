use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rawgraph_core::{ops, Graph, ImageBuf, ImageId, Mode, NodeId, NodeKind};
use rawgraph_engine::CancelToken;
use rawgraph_scopes::DisplayTransform;

use super::*;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Number of decode attempts made by `run_item` on this thread.
    pub(crate) static DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rawgraph-batch-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap_or_default();
        TempDir(p)
    }

    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn write_grey(path: &Path, w: u32, h: u32, v: u8) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_default();
    }
    let rgb = vec![v; (w * h * 3) as usize];
    rawgraph_io::save_srgb8(path, w, h, &rgb, 95, true).expect("write test source");
}

fn make_graph(ev: f32) -> (Graph, NodeId, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0; 2]);
    let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
    let out = g.add_node(NodeKind::Output, [0.0; 2]);
    g.connect(src, exp, 0).unwrap();
    g.connect(exp, out, 0).unwrap();
    g.node_mut(exp).unwrap().set_param("ev", ImageId(0), ev);
    (g, exp, out)
}

fn settings(out: &Path, format: ExportFormat) -> ExportSettings {
    ExportSettings {
        out_dir: out.to_path_buf(),
        format,
        name_template: "{name}".into(),
        long_edge: None,
        on_conflict: OnConflict::Skip,
        display: DisplayTransform::default(),
        // The pre-existing tests compare pixels only; profile and metadata have their own tests.
        embed_profile: false,
        copy_metadata: false,
        ..Default::default()
    }
}

pub(crate) fn item(id: u64, p: PathBuf) -> ExportItem {
    ExportItem { id: ImageId(id), source: p }
}

fn run(g: &Graph, n: NodeId, items: &[ExportItem], s: &ExportSettings) -> ExportReport {
    export(g, n, items, s, &CancelToken::new(), &mut |_| {}).expect("export")
}

pub(crate) fn load(path: &Path) -> ImageBuf {
    rawgraph_io::load(path, None).expect("load").image
}

pub(crate) fn mean_r(img: &ImageBuf) -> f32 {
    let n = (img.width() * img.height()) as f32;
    img.data().chunks(4).map(|p| p[0]).sum::<f32>() / n
}

pub(crate) fn files_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub(crate) fn written_path(s: &ItemStatus) -> PathBuf {
    match s {
        ItemStatus::Written { path, .. } => path.clone(),
        other => panic!("expected Written, got {other:?}"),
    }
}

#[test]
fn three_sources_exposure_plus_one() {
    let t = TempDir::new();
    let names = ["a.png", "b.png", "c.png"];
    let items: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let p = t.path(&format!("src/{n}"));
            write_grey(&p, 8, 6, 40 + 20 * i as u8);
            item(i as u64 + 1, p)
        })
        .collect();
    let (g, _, out) = make_graph(1.0);
    let s = settings(&t.path("out"), ExportFormat::Png);
    let report = run(&g, out, &items, &s);
    assert_eq!((report.written(), report.skipped(), report.failed(), report.cancelled()), (3, 0, 0, 0));
    for (i, (id, st)) in report.items.iter().enumerate() {
        assert_eq!(*id, items[i].id);
        let path = written_path(st);
        assert_eq!(path, t.path(&format!("out/{}", names[i])));

        // Independent expectation: x2 in linear light, display transform, 8-bit, same file writer.
        let mut lin = load(&items[i].source);
        ops::apply_gain(&mut lin, 2.0);
        let disp = DisplayTransform::default().apply(&lin);
        let rgba = disp.to_rgba8();
        let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
        let reference = t.path(&format!("ref{i}.png"));
        rawgraph_io::save_srgb8(&reference, 8, 6, &rgb, 95, true).unwrap();
        assert_eq!(load(&path), load(&reference), "export must equal the analytic result");

        let ratio = mean_r(&load(&path)) / mean_r(&load(&items[i].source));
        assert!((ratio - 2.0).abs() < 0.05, "brightness ratio {ratio}");
    }
}

#[test]
fn individual_mode_changes_only_its_image() {
    let t = TempDir::new();
    let items: Vec<_> = (1..=3)
        .map(|i| {
            let p = t.path(&format!("src/{i}.png"));
            write_grey(&p, 6, 6, 80);
            item(i, p)
        })
        .collect();
    let (mut g, exp, out) = make_graph(0.0);
    let n = g.node_mut(exp).unwrap();
    n.mode = Mode::Individual;
    n.set_param("ev", ImageId(2), 1.5);
    let report = run(&g, out, &items, &settings(&t.path("out"), ExportFormat::Png));
    assert_eq!(report.written(), 3);
    let m: Vec<f32> = report.items.iter().map(|(_, s)| mean_r(&load(&written_path(s)))).collect();
    assert!((m[0] - m[2]).abs() < 1e-6);
    assert!((m[1] / m[0] - 2f32.powf(1.5)).abs() < 0.1, "ratio {}", m[1] / m[0]);
}

#[test]
fn name_templates_and_planned_path() {
    let t = TempDir::new();
    let items: Vec<_> = ["x", "y"]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let p = t.path(&format!("src/{n}.png"));
            write_grey(&p, 4, 4, 100);
            item(i as u64 + 7, p)
        })
        .collect();
    let (g, _, out) = make_graph(0.0);
    let mut s = settings(&t.path("out"), ExportFormat::Png);
    s.name_template = "{name}_{index}_{index:04}_{id}".into();
    let report = run(&g, out, &items, &s);
    for (i, (_, st)) in report.items.iter().enumerate() {
        assert_eq!(written_path(st), planned_path(&items[i], i + 1, &s).unwrap());
    }
    assert_eq!(files_in(&t.path("out")), vec!["x_1_0001_7.png", "y_2_0002_8.png"]);

    for bad in ["", "{foo}", "{name", "name}", "a/{name}", "{name}?", "a\\b", "{index:x}"] {
        s.name_template = bad.into();
        assert!(matches!(planned_path(&items[0], 1, &s), Err(ExportError::InvalidSettings(_))), "{bad:?}");
        let r = export(&g, out, &items, &s, &CancelToken::new(), &mut |_| {});
        assert!(matches!(r, Err(ExportError::InvalidSettings(_))), "{bad:?}");
    }
}

#[test]
fn colliding_names_with_overwrite_are_rejected_before_any_work() {
    let t = TempDir::new();
    let a = t.path("one/x.png");
    let b = t.path("two/x.png");
    write_grey(&a, 4, 4, 100);
    write_grey(&b, 4, 4, 100);
    let items = [item(1, a), item(2, b)];
    let (g, _, out) = make_graph(0.0);
    let mut s = settings(&t.path("out"), ExportFormat::Png);
    s.on_conflict = OnConflict::Overwrite;
    let err = export(&g, out, &items, &s, &CancelToken::new(), &mut |_| panic!("no progress expected"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("one") && err.contains("two"), "{err}");
    assert!(!t.path("out").exists(), "no folder is created for invalid settings");
    s.name_template = "{name}_{index}".into();
    assert_eq!(run(&g, out, &items, &s).written(), 2);
}

/// Two same-named sources in different folders (grey 60 and 200), plus `extra` more.
fn same_named(t: &TempDir, n: usize) -> Vec<ExportItem> {
    (0..n)
        .map(|i| {
            let p = t.path(&format!("f{i}/x.png"));
            write_grey(&p, 4, 4, 60 + 70 * i as u8);
            item(i as u64 + 1, p)
        })
        .collect()
}

fn same_run_s(t: &TempDir, mode: OnConflict) -> ExportSettings {
    let mut s = settings(&t.path("out"), ExportFormat::Png);
    s.on_conflict = mode;
    s
}

#[test]
fn same_name_in_one_run_skip_rename_overwrite() {
    let (g, _, out) = make_graph(0.0);

    // Skip: the later item is skipped and points at the earlier file.
    let t = TempDir::new();
    let items = same_named(&t, 2);
    let r = run(&g, out, &items, &same_run_s(&t, OnConflict::Skip));
    assert_eq!(written_path(&r.items[0].1), t.path("out/x.png"));
    assert_eq!(r.items[1].1, ItemStatus::SkippedExisting(t.path("out/x.png")));
    assert_eq!(files_in(&t.path("out")), vec!["x.png"]);

    // Rename: both exist, each with the pixels of its own source.
    let t = TempDir::new();
    let items = same_named(&t, 2);
    let r = run(&g, out, &items, &same_run_s(&t, OnConflict::Rename));
    assert_eq!((r.written(), r.failed()), (2, 0), "{r:?}");
    assert_eq!(written_path(&r.items[0].1), t.path("out/x.png"));
    assert_eq!(written_path(&r.items[1].1), t.path("out/x (2).png"));
    assert_eq!(files_in(&t.path("out")), vec!["x (2).png", "x.png"]);
    for (i, name) in ["x.png", "x (2).png"].iter().enumerate() {
        let got = mean_r(&load(&t.path(&format!("out/{name}"))));
        let want = mean_r(&load(&items[i].source));
        assert!((got - want).abs() < 0.01, "{name}: {got} vs {want}");
    }

    // Overwrite: refused up front, nothing written.
    let t = TempDir::new();
    let items = same_named(&t, 2);
    assert!(export(&g, out, &items, &same_run_s(&t, OnConflict::Overwrite), &CancelToken::new(), &mut |_| {}).is_err());
    assert!(!t.path("out").exists());
}

#[test]
fn same_name_three_items_and_existing_file_with_rename() {
    let (g, _, out) = make_graph(0.0);
    let t = TempDir::new();
    let items = same_named(&t, 3);
    let r = run(&g, out, &items, &same_run_s(&t, OnConflict::Rename));
    let names: Vec<PathBuf> = r.items.iter().map(|(_, s)| written_path(s)).collect();
    assert_eq!(names, vec![t.path("out/x.png"), t.path("out/x (2).png"), t.path("out/x (3).png")]);

    // A file of that name already on disk: the two items get (2) and (3).
    let t = TempDir::new();
    let items = same_named(&t, 2);
    fs::create_dir_all(t.path("out")).unwrap();
    fs::write(t.path("out/x.png"), b"precious").unwrap();
    let r = run(&g, out, &items, &same_run_s(&t, OnConflict::Rename));
    assert_eq!(written_path(&r.items[0].1), t.path("out/x (2).png"));
    assert_eq!(written_path(&r.items[1].1), t.path("out/x (3).png"));
    assert_eq!(fs::read(t.path("out/x.png")).unwrap(), b"precious");
}

#[test]
fn same_name_retry_keeps_the_planned_name() {
    let (g, _, out) = make_graph(0.0);
    let t = TempDir::new();
    let mut items = same_named(&t, 2);
    let good = items[1].source.clone();
    // The second item cannot be decoded in the first run.
    fs::rename(&good, t.path("f1/x.bak")).unwrap();
    let s = same_run_s(&t, OnConflict::Rename);
    let r = run(&g, out, &items, &s);
    assert_eq!((r.written(), r.failed()), (1, 1), "{r:?}");
    assert_eq!(files_in(&t.path("out")), vec!["x.png"]);
    fs::rename(t.path("f1/x.bak"), &good).unwrap();
    let retry = r.retry_items(&items);
    assert_eq!(retry, vec![items.remove(1)]);
    let r2 = run(&g, out, &retry, &s);
    assert_eq!(written_path(&r2.items[0].1), t.path("out/x (2).png"));
    assert_eq!(files_in(&t.path("out")), vec!["x (2).png", "x.png"]);
}

#[test]
fn conflict_handling() {
    let t = TempDir::new();
    let src = t.path("src/a.png");
    write_grey(&src, 4, 4, 100);
    let items = [item(1, src)];
    let (g, _, out) = make_graph(1.0);
    let existing = t.path("out/a.png");
    fs::create_dir_all(t.path("out")).unwrap();
    fs::write(&existing, b"precious").unwrap();

    let mut s = settings(&t.path("out"), ExportFormat::Png);
    s.on_conflict = OnConflict::Skip;
    let r = run(&g, out, &items, &s);
    assert_eq!(r.items[0].1, ItemStatus::SkippedExisting(existing.clone()));
    assert_eq!(fs::read(&existing).unwrap(), b"precious");

    s.on_conflict = OnConflict::Rename;
    let r = run(&g, out, &items, &s);
    assert_eq!(written_path(&r.items[0].1), t.path("out/a (2).png"));
    let r = run(&g, out, &items, &s);
    assert_eq!(written_path(&r.items[0].1), t.path("out/a (3).png"));
    assert_eq!(fs::read(&existing).unwrap(), b"precious");

    s.on_conflict = OnConflict::Overwrite;
    let r = run(&g, out, &items, &s);
    assert_eq!(written_path(&r.items[0].1), existing);
    assert_ne!(fs::read(&existing).unwrap(), b"precious");
    assert_eq!(load(&existing).width(), 4);
    assert_eq!(files_in(&t.path("out")), vec!["a (2).png", "a (3).png", "a.png"]);
}

#[test]
fn sources_are_never_overwritten() {
    for mode in [OnConflict::Overwrite, OnConflict::Rename, OnConflict::Skip] {
        let t = TempDir::new();
        let s1 = t.path("dir/1.png");
        let s2 = t.path("dir/2.png");
        write_grey(&s1, 4, 4, 90);
        write_grey(&s2, 4, 4, 90);
        let (b1, b2) = (fs::read(&s1).unwrap(), fs::read(&s2).unwrap());
        let (g, _, out) = make_graph(1.0);
        let mut s = settings(&t.path("dir"), ExportFormat::Png);
        s.on_conflict = mode;
        // Item 1 targets its own source, item 2 targets the source of nobody ("{index}" -> 2.png is item 2's source).
        s.name_template = "{index}".into();
        let items = [item(1, s1.clone()), item(2, s2.clone())];
        let r = run(&g, out, &items, &s);
        assert_eq!(r.failed(), 2, "{mode:?}: {r:?}");
        assert!(matches!(&r.items[0].1, ItemStatus::Failed(m) if m.contains("source")));
        assert_eq!(fs::read(&s1).unwrap(), b1);
        assert_eq!(fs::read(&s2).unwrap(), b2);
        assert_eq!(files_in(&t.path("dir")), vec!["1.png", "2.png"]);
    }

    // An item may not hit another item's source either; its own neighbours still succeed.
    let t = TempDir::new();
    let s1 = t.path("dir/2.png");
    let s2 = t.path("dir/other.png");
    write_grey(&s1, 4, 4, 90);
    write_grey(&s2, 4, 4, 90);
    let before = fs::read(&s1).unwrap();
    let (g, _, out) = make_graph(1.0);
    let mut s = settings(&t.path("dir"), ExportFormat::Png);
    s.on_conflict = OnConflict::Overwrite;
    s.name_template = "{index}".into();
    let r = run(&g, out, &[item(1, s1.clone()), item(2, s2)], &s);
    assert_eq!(r.written(), 1);
    assert!(matches!(&r.items[1].1, ItemStatus::Failed(_)));
    assert_eq!(fs::read(&s1).unwrap(), before);
}

#[test]
fn partial_failure_and_retry() {
    let t = TempDir::new();
    let good1 = t.path("src/good1.png");
    let good2 = t.path("src/good2.png");
    let missing = t.path("src/missing.png");
    let garbage = t.path("src/garbage.jpg");
    write_grey(&good1, 4, 4, 100);
    write_grey(&good2, 4, 4, 100);
    fs::write(&garbage, b"this is not a jpeg").unwrap();
    let items = [item(1, good1), item(2, missing), item(3, garbage.clone()), item(4, good2)];
    let (g, _, out) = make_graph(0.0);
    let s = settings(&t.path("out"), ExportFormat::Jpeg { quality: 90 });
    let r = run(&g, out, &items, &s);
    assert_eq!((r.written(), r.failed()), (2, 2));
    assert!(matches!(&r.items[1].1, ItemStatus::Failed(m) if m.contains("missing.png")));
    assert!(matches!(&r.items[2].1, ItemStatus::Failed(m) if m.contains("decod")));
    let retry = r.retry_items(&items);
    assert_eq!(retry, vec![items[1].clone(), items[2].clone()]);
    assert_eq!(files_in(&t.path("out")), vec!["good1.jpg", "good2.jpg"]);

    write_grey(&garbage, 4, 4, 120);
    let r2 = run(&g, out, &retry, &s);
    assert!(matches!(r2.items[0].1, ItemStatus::Failed(_)));
    assert!(matches!(r2.items[1].1, ItemStatus::Written { .. }));
    assert_eq!(r2.retry_items(&retry), vec![items[1].clone()]);
    assert!(t.path("out/garbage.jpg").is_file());
}

#[test]
fn cancellation_mid_batch_and_before_start() {
    let t = TempDir::new();
    let items: Vec<_> = (1..=4)
        .map(|i| {
            let p = t.path(&format!("src/{i}.png"));
            write_grey(&p, 4, 4, 100);
            item(i, p)
        })
        .collect();
    let (g, _, out) = make_graph(0.0);
    let s = settings(&t.path("out"), ExportFormat::Png);
    let cancel = CancelToken::new();
    let r = export(&g, out, &items, &s, &cancel, &mut |p| {
        if p.done == 1 {
            cancel.cancel();
        }
    })
    .unwrap();
    assert!(matches!(r.items[0].1, ItemStatus::Written { .. }));
    assert_eq!((r.written(), r.cancelled()), (1, 3));
    assert_eq!(r.retry_items(&items), items[1..].to_vec());
    assert_eq!(files_in(&t.path("out")), vec!["1.png"]);

    let t2 = TempDir::new();
    let s2 = settings(&t2.path("out"), ExportFormat::Png);
    let c = CancelToken::new();
    c.cancel();
    let r = export(&g, out, &items, &s2, &c, &mut |_| {}).unwrap();
    assert_eq!(r.cancelled(), 4);
    assert!(files_in(&t2.path("out")).is_empty());
}

#[test]
fn progress_events() {
    let t = TempDir::new();
    let mut items: Vec<_> = (1..=3)
        .map(|i| {
            let p = t.path(&format!("src/{i}.png"));
            write_grey(&p, 4, 4, 100);
            item(i, p)
        })
        .collect();
    items.push(item(9, t.path("src/none.png")));
    let (g, _, out) = make_graph(0.0);
    let mut events = Vec::new();
    export(&g, out, &items, &settings(&t.path("out"), ExportFormat::Png), &CancelToken::new(), &mut |p| {
        events.push(p.clone())
    })
    .unwrap();
    assert_eq!(events.len(), items.len() + 1);
    assert_eq!((events[0].done, events[0].total, events[0].last.clone()), (0, 4, None));
    assert_eq!(events[0].current, items[0].source);
    for (i, e) in events.iter().enumerate().skip(1) {
        assert_eq!((e.done, e.total), (i, 4));
        let (id, st) = e.last.clone().unwrap();
        assert_eq!(id, items[i - 1].id);
        assert_eq!(e.current, items[i - 1].source);
        assert_eq!(matches!(st, ItemStatus::Failed(_)), i == 4);
    }
}

#[test]
fn resize_dimensions() {
    assert_eq!(target_size(300, 200, 100), (100, 67));
    assert_eq!(target_size(200, 300, 100), (67, 100));
    assert_eq!(target_size(1000, 667, 333), (333, 222));
    assert_eq!(target_size(50, 40, 100), (50, 40));
    assert_eq!(target_size(100, 40, 100), (100, 40));
    assert_eq!(target_size(10000, 3, 100), (100, 1));
    let img = ImageBuf::new(300, 200);
    assert_eq!(resize_to_long_edge(&img, 128).width(), 128);
    assert_eq!(resize_to_long_edge(&img, 128).height(), 85);
    assert!(matches!(resize_to_long_edge(&img, 4000), std::borrow::Cow::Borrowed(_)));
    let portrait = ImageBuf::new(37, 91);
    let r = resize_to_long_edge(&portrait, 30);
    assert_eq!((r.width(), r.height()), (12, 30));
}

#[test]
fn resize_preserves_values() {
    let mut flat = ImageBuf::new(37, 23);
    flat.data_mut().chunks_mut(4).for_each(|p| p.copy_from_slice(&[0.3, 4.5, 0.01, 1.0]));
    let small = resize_area(&flat, 10, 6);
    for p in small.data().chunks(4) {
        for (v, e) in p.iter().zip([0.3, 4.5, 0.01, 1.0]) {
            assert!((v - e).abs() < 1e-5, "{v} vs {e}");
        }
    }

    let mut ramp = ImageBuf::new(64, 8);
    for y in 0..8 {
        for x in 0..64 {
            ramp.set_pixel(x, y, [x as f32 / 63.0, 0.0, 0.0, 1.0]);
        }
    }
    let r = resize_area(&ramp, 20, 3);
    assert!((mean_r(&r) - mean_r(&ramp)).abs() < 1e-4);

    let mut img = ImageBuf::new(8, 6);
    for (i, v) in img.data_mut().iter_mut().enumerate() {
        *v = ((i * 37 % 101) as f32) / 10.0;
    }
    let a = resize_area(&img, 4, 3);
    let b = img.downscale_box(4);
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    for (x, y) in a.data().iter().zip(b.data()) {
        assert!((x - y).abs() < 1e-5, "{x} vs {y}");
    }
}

pub(crate) fn read_exr(path: &Path) -> (usize, usize, Vec<f32>) {
    let image = exr::prelude::read_first_rgba_layer_from_file(
        path,
        |res, _| (res.width(), res.height(), vec![0f32; res.width() * res.height() * 4]),
        |(w, _, v), pos, (r, g, b, a): (f32, f32, f32, f32)| {
            let i = (pos.y() * *w + pos.x()) * 4;
            v[i..i + 4].copy_from_slice(&[r, g, b, a]);
        },
    )
    .expect("read exr");
    image.layer_data.channel_data.pixels
}

#[test]
fn all_formats_read_back() {
    let t = TempDir::new();
    let src = t.path("src/a.png");
    write_grey(&src, 30, 20, 100);
    let items = [item(1, src.clone())];
    let (g, _, out) = make_graph(1.0);
    for (k, (fmt, ext)) in [
        (ExportFormat::Jpeg { quality: 85 }, "jpg"),
        (ExportFormat::Png, "png"),
        (ExportFormat::Tiff, "tif"),
        (ExportFormat::Png16, "png"),
        (ExportFormat::Tiff16, "tif"),
    ]
    .into_iter()
    .enumerate()
    {
        let mut s = settings(&t.path(&format!("out-{k}")), fmt);
        s.long_edge = Some(15);
        let r = run(&g, out, &items, &s);
        let p = written_path(&r.items[0].1);
        assert_eq!(p, t.path(&format!("out-{k}/a.{ext}")));
        assert!(matches!(r.items[0].1, ItemStatus::Written { width: 15, height: 10, .. }));
        let img = load(&p);
        assert_eq!((img.width(), img.height()), (15, 10), "{ext}");
    }

    let s = settings(&t.path("out"), ExportFormat::Exr);
    let r = run(&g, out, &items, &s);
    let (w, h, data) = read_exr(&written_path(&r.items[0].1));
    assert_eq!((w, h), (30, 20));
    let mut expected = load(&src);
    ops::apply_gain(&mut expected, 2.0);
    for (a, b) in data.iter().zip(expected.data()) {
        assert!((a - b).abs() < 1e-6, "{a} vs {b}");
    }

    let mut bad = settings(&t.path("out2"), ExportFormat::Jpeg { quality: 0 });
    assert!(matches!(export(&g, out, &items, &bad, &CancelToken::new(), &mut |_| {}), Err(ExportError::InvalidSettings(_))));
    bad.format = ExportFormat::Jpeg { quality: 101 };
    assert!(export(&g, out, &items, &bad, &CancelToken::new(), &mut |_| {}).is_err());
    bad.format = ExportFormat::Png;
    bad.long_edge = Some(0);
    assert!(export(&g, out, &items, &bad, &CancelToken::new(), &mut |_| {}).is_err());
    assert!(!t.path("out2").exists());
}

#[test]
fn invalid_node_is_rejected() {
    let t = TempDir::new();
    let (mut g, _, out) = make_graph(0.0);
    let mask = g.add_node(NodeKind::GradientMask, [0.0; 2]);
    let s = settings(&t.path("out"), ExportFormat::Png);
    for n in [NodeId(999), mask] {
        assert!(matches!(
            export(&g, n, &[], &s, &CancelToken::new(), &mut |_| {}),
            Err(ExportError::InvalidSettings(_))
        ));
    }
    assert!(export(&g, out, &[], &s, &CancelToken::new(), &mut |_| {}).unwrap().items.is_empty());
}

#[test]
fn evaluation_error_fails_every_item() {
    let t = TempDir::new();
    let items: Vec<_> = (1..=2)
        .map(|i| {
            let p = t.path(&format!("src/{i}.png"));
            write_grey(&p, 4, 4, 100);
            item(i, p)
        })
        .collect();
    let mut g = Graph::new();
    let out = g.add_node(NodeKind::Output, [0.0; 2]);
    let r = run(&g, out, &items, &settings(&t.path("out"), ExportFormat::Png));
    assert_eq!(r.failed(), 2);
    for (_, st) in &r.items {
        assert!(matches!(st, ItemStatus::Failed(m) if m.contains("not connected")), "{st:?}");
    }
    assert!(files_in(&t.path("out")).is_empty());
}

/// Minimal little-endian EXIF block with only PhotographicSensitivity (ISO).
fn exif_app1(iso: u16) -> Vec<u8> {
    let mut tiff: Vec<u8> = vec![b'I', b'I', 0x2A, 0, 8, 0, 0, 0];
    tiff.extend([1, 0, 0x69, 0x87, 4, 0, 1, 0, 0, 0, 26, 0, 0, 0, 0, 0, 0, 0]); // IFD0 -> Exif IFD at 26
    tiff.extend([1, 0, 0x27, 0x88, 3, 0, 1, 0, 0, 0]);
    tiff.extend(iso.to_le_bytes());
    tiff.extend([0, 0, 0, 0, 0, 0]);
    let mut seg = vec![0xFF, 0xE1];
    seg.extend(((2 + 6 + tiff.len()) as u16).to_be_bytes());
    seg.extend(b"Exif\0\0");
    seg.extend(tiff);
    seg
}

/// A real JPEG with an EXIF ISO tag.
fn write_jpeg_iso(path: &Path, v: u8, iso: u16) {
    write_grey(path, 16, 12, v);
    let bytes = fs::read(path).unwrap();
    let mut out = bytes[..2].to_vec();
    out.extend(exif_app1(iso));
    out.extend(&bytes[2..]);
    fs::write(path, out).unwrap();
}

pub(crate) fn set(g: &mut Graph, n: NodeId, name: &str, v: f32) {
    assert!(g.node_mut(n).unwrap().set_param(name, ImageId(0), v));
}

fn filter_graph(min: f32, max: f32, invert: bool) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0; 2]);
    let f = g.add_node(NodeKind::MetadataFilter, [0.0; 2]);
    let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
    let out = g.add_node(NodeKind::Output, [0.0; 2]);
    g.connect(src, f, 0).unwrap();
    g.connect(f, exp, 0).unwrap();
    g.connect(exp, out, 0).unwrap();
    set(&mut g, f, "min", min);
    set(&mut g, f, "max", max);
    set(&mut g, f, "invert", if invert { 1.0 } else { 0.0 });
    set(&mut g, exp, "ev", 1.0);
    (g, out)
}

#[test]
fn metadata_filter_excludes_without_decoding() {
    let t = TempDir::new();
    let lo = t.path("src/lo.jpg");
    let hi = t.path("src/hi.jpg");
    let garbage = t.path("src/garbage.jpg");
    let plain = t.path("src/plain.jpg");
    write_jpeg_iso(&lo, 90, 200);
    write_jpeg_iso(&hi, 90, 3200);
    fs::write(&garbage, b"junk").unwrap(); // unreadable metadata -> unknown -> excluded, not failed
    write_grey(&plain, 8, 8, 90); // no EXIF at all
    let items = [item(1, lo), item(2, hi), item(3, garbage), item(4, plain)];
    let (g, out) = filter_graph(0.0, 400.0, false);
    let s = settings(&t.path("out"), ExportFormat::Jpeg { quality: 90 });
    let mut calls = 0;
    DECODES.with(|c| c.set(0));
    let r = export(&g, out, &items, &s, &CancelToken::new(), &mut |_| calls += 1).unwrap();
    assert_eq!(r.items[0].1, ItemStatus::Written { path: t.path("out/lo.jpg"), width: 16, height: 12 });
    for (i, (_, st)) in r.items.iter().enumerate().skip(1) {
        assert_eq!(*st, ItemStatus::Excluded, "item {i}");
    }
    assert_eq!((r.written(), r.excluded(), r.failed(), r.cancelled(), r.skipped()), (1, 3, 0, 0, 0));
    assert_eq!(DECODES.with(|c| c.get()), 1, "only the included image is decoded");
    assert!(r.retry_items(&items).is_empty());
    assert_eq!(files_in(&t.path("out")), vec!["lo.jpg"]);
    assert_eq!(calls, items.len() + 1);

    // Invert: the high-ISO image is in, the low-ISO one out; missing metadata stays out.
    let (g, out) = filter_graph(0.0, 400.0, true);
    let s = settings(&t.path("out2"), ExportFormat::Jpeg { quality: 90 });
    DECODES.with(|c| c.set(0));
    let r = run(&g, out, &items, &s);
    assert_eq!(r.items[0].1, ItemStatus::Excluded);
    assert!(matches!(r.items[1].1, ItemStatus::Written { .. }));
    assert_eq!((r.written(), r.excluded()), (1, 3));
    assert_eq!(DECODES.with(|c| c.get()), 1);
    assert_eq!(files_in(&t.path("out2")), vec!["hi.jpg"]);

    // A retry list never contains excluded items, but does contain real failures.
    let broken = item(9, t.path("src/gone.jpg"));
    let all = [items[0].clone(), broken.clone()];
    let (g, out) = filter_graph(0.0, 400.0, false);
    let r = run(&g, out, &all, &settings(&t.path("out3"), ExportFormat::Png));
    // The missing file has unknown metadata, so the filter excludes it as well.
    assert_eq!((r.written(), r.excluded(), r.failed()), (1, 1, 0));
    assert!(r.retry_items(&all).is_empty());
}

#[test]
fn merge_branches_match_single_branch_exports() {
    let t = TempDir::new();
    let lo = t.path("src/lo.jpg");
    let hi = t.path("src/hi.jpg");
    write_jpeg_iso(&lo, 70, 200);
    write_jpeg_iso(&hi, 110, 3200);
    let items = [item(1, lo), item(2, hi)];

    // Source -> [Filter <=400 -> Exposure +1] / [Filter >400 -> NR -> Exposure +2] -> Merge -> Output.
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0; 2]);
    let fa = g.add_node(NodeKind::MetadataFilter, [0.0; 2]);
    let ea = g.add_node(NodeKind::Exposure, [0.0; 2]);
    let fb = g.add_node(NodeKind::MetadataFilter, [0.0; 2]);
    let nr = g.add_node(NodeKind::NoiseReduction, [0.0; 2]);
    let eb = g.add_node(NodeKind::Exposure, [0.0; 2]);
    let merge = g.add_node(NodeKind::Merge, [0.0; 2]);
    let out = g.add_node(NodeKind::Output, [0.0; 2]);
    for (a, b) in [(src, fa), (fa, ea), (src, fb), (fb, nr), (nr, eb), (merge, out)] {
        g.connect(a, b, 0).unwrap();
    }
    g.connect(ea, merge, 0).unwrap();
    g.connect(eb, merge, 1).unwrap();
    set(&mut g, fa, "max", 400.0);
    set(&mut g, fb, "max", 400.0);
    set(&mut g, fb, "invert", 1.0);
    set(&mut g, ea, "ev", 1.0);
    set(&mut g, eb, "ev", 2.0);
    set(&mut g, nr, "luma", 0.5);
    set(&mut g, nr, "chroma", 0.7);

    let fmt = ExportFormat::Png;
    let r = run(&g, out, &items, &settings(&t.path("merged"), fmt));
    assert_eq!(r.written(), 2, "{r:?}");

    let alone = |nr_on: bool, ev: f32, dir: &str| {
        let mut g = Graph::new();
        let s = g.add_node(NodeKind::Source, [0.0; 2]);
        let n = g.add_node(NodeKind::NoiseReduction, [0.0; 2]);
        let e = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let o = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(s, n, 0).unwrap();
        g.connect(n, e, 0).unwrap();
        g.connect(e, o, 0).unwrap();
        if nr_on {
            set(&mut g, n, "luma", 0.5);
            set(&mut g, n, "chroma", 0.7);
        }
        set(&mut g, e, "ev", ev);
        let r = run(&g, o, &items[if ev == 1.0 { 0..1 } else { 1..2 }], &settings(&t.path(dir), fmt));
        written_path(&r.items[0].1)
    };
    let a = alone(false, 1.0, "a");
    let b = alone(true, 2.0, "b");
    assert_eq!(load(&written_path(&r.items[0].1)), load(&a), "ISO 200 takes branch A");
    assert_eq!(load(&written_path(&r.items[1].1)), load(&b), "ISO 3200 takes branch B");
    assert!(mean_r(&load(&a)) != mean_r(&load(&b)));

    // The graph itself says which branch each image takes.
    let cap = |iso: f32| CaptureInfo { iso: Some(iso), ..Default::default() };
    assert_eq!(g.merge_source(merge, ImageId(1), &|_| Some(cap(200.0))), Some(0));
    assert_eq!(g.merge_source(merge, ImageId(2), &|_| Some(cap(3200.0))), Some(1));
}

fn develop_graph() -> (Graph, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0; 2]);
    let dev = g.add_node(NodeKind::Develop, [0.0; 2]);
    let out = g.add_node(NodeKind::Output, [0.0; 2]);
    g.connect(src, dev, 0).unwrap();
    g.connect(dev, out, 0).unwrap();
    (g, out)
}

#[test]
fn develop_uses_capture_metadata_for_every_item() {
    use std::sync::Mutex;

    let t = TempDir::new();
    let (a, b, c) = (t.path("a.png"), t.path("b.png"), t.path("c.png"));
    for p in [&a, &b, &c] {
        write_grey(p, 12, 8, 90);
    }
    let items = [item(1, a.clone()), item(2, b.clone()), item(3, c.clone())];
    let (g, out) = develop_graph();
    let s = settings(&t.path("out"), ExportFormat::Exr);
    let asked: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    // a: RAW with a 0.5 EV baseline in the file, b: RAW without a baseline tag (node default),
    // c: no metadata at all.
    let reader = |p: &Path| -> Option<CaptureInfo> {
        asked.lock().unwrap().push(p.to_path_buf());
        if p == a {
            Some(CaptureInfo { raw: Some(true), baseline_exposure: Some(0.5), ..Default::default() })
        } else if p == b {
            Some(CaptureInfo { raw: Some(true), ..Default::default() })
        } else {
            None
        }
    };
    let r = export_with(&g, out, &items, &s, &CancelToken::new(), &mut |_| {}, &reader, &|_| None).expect("export");
    assert_eq!(r.written(), 3);
    assert_eq!(*asked.lock().unwrap(), vec![a.clone(), b.clone(), c.clone()], "consulted once per item, in order");

    let base = load(&a);
    // a: tag 0.5 + brightness 1.6; b: baseline_default 0.35 + brightness 1.6; c: not RAW, no gain.
    for (i, gain) in [(0usize, 2.1f32), (1, 1.95), (2, 0.0)] {
        let (_, _, data) = read_exr(&written_path(&r.items[i].1));
        let mut expected = base.clone();
        ops::apply_gain(&mut expected, 2f32.powf(gain));
        for (x, y) in data.iter().zip(expected.data()) {
            assert!((x - y).abs() < 1e-5, "item {i}: {x} vs {y}");
        }
    }
}

#[test]
fn export_reads_capture_metadata_without_a_filter() {
    // A rendered file reports itself as not RAW, so the real reader never brightens it.
    let t = TempDir::new();
    let src = t.path("a.png");
    write_grey(&src, 12, 8, 90);
    let (g, out) = develop_graph();
    let r = run(&g, out, &[item(1, src.clone())], &settings(&t.path("out"), ExportFormat::Exr));
    let (_, _, data) = read_exr(&written_path(&r.items[0].1));
    let base = load(&src);
    for (x, y) in data.iter().zip(base.data()) {
        assert!((x - y).abs() < 1e-6);
    }
}

// ------------------------------------------------------------------ 16-bit, profile, metadata

mod deliverables {
    use super::*;
    use image::{ImageDecoder, ImageReader};

    const PROFILE_FLAGS: [(bool, bool); 4] = [(false, false), (true, false), (false, true), (true, true)];

    fn meta() -> rawgraph_io::ExportMetadata {
        rawgraph_io::ExportMetadata {
            make: Some("SONY".into()),
            model: Some("ILCE-7M4".into()),
            lens: Some("FE 35mm F1.4 GM".into()),
            iso: Some(640),
            exposure_time: Some((1, 250)),
            f_number: Some((28, 10)),
            focal_length: Some((35, 1)),
            datetime_original: Some("2026:09:26 14:03:09".into()),
            artist: Some("A. Photographer".into()),
            copyright: Some("(c) Test".into()),
            software: None,
            ..Default::default()
        }
    }

    /// A source with EXIF: written by the io crate's own writer.
    fn source_with_exif(path: &Path, w: u32, h: u32) {
        fs::create_dir_all(path.parent().expect("parent")).unwrap_or_default();
        let rgb: Vec<u8> = (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, 90, 160]).collect();
        let o = rawgraph_io::SaveOptions { jpeg_quality: 98, overwrite: true, embed_profile: false, metadata: Some(meta()), ..Default::default() };
        rawgraph_io::save_srgb8_with(path, w, h, &rgb, &o).expect("write source");
    }

    /// A 16-bit source whose red channel uses far more than 256 distinct values.
    fn source_16bit(path: &Path, w: u32, h: u32) {
        fs::create_dir_all(path.parent().expect("parent")).unwrap_or_default();
        let rgb: Vec<u16> = (0..w * h).flat_map(|i| [((i % w) * 65535 / (w - 1)) as u16, 20000, 50000 - (i % w) as u16 * 3]).collect();
        let o = rawgraph_io::SaveOptions { overwrite: true, embed_profile: false, ..Default::default() };
        rawgraph_io::save_srgb16_with(path, w, h, &rgb, &o).expect("write 16-bit source");
    }

    fn icc_of(path: &Path) -> Option<Vec<u8>> {
        // `image` drops a TIFF's profile once limits are applied; use the TIFF decoder directly.
        if matches!(path.extension().and_then(|e| e.to_str()), Some("tif")) {
            let mut t = image::codecs::tiff::TiffDecoder::new(std::io::BufReader::new(fs::File::open(path).ok()?)).ok()?;
            return t.icc_profile().ok().flatten();
        }
        let mut d = ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
        d.icc_profile().ok().flatten()
    }

    fn exif_of(path: &Path) -> Option<exif::Exif> {
        exif::Reader::new().read_from_container(&mut std::io::BufReader::new(fs::File::open(path).ok()?)).ok()
    }

    fn ascii(ex: &exif::Exif, tag: exif::Tag) -> Option<String> {
        match &ex.get_field(tag, exif::In::PRIMARY)?.value {
            exif::Value::Ascii(v) => v.first().map(|s| String::from_utf8_lossy(s).to_string()),
            _ => None,
        }
    }

    #[test]
    fn settings_default_is_sensible() {
        let s = ExportSettings::default();
        assert_eq!(s.format, ExportFormat::Jpeg { quality: 92 });
        assert_eq!((s.embed_profile, s.copy_metadata, s.long_edge), (true, true, None));
        assert_eq!((s.name_template.as_str(), s.on_conflict), ("{name}", OnConflict::Skip));
        let s = ExportSettings { out_dir: PathBuf::from("x"), ..Default::default() };
        assert_eq!(s.out_dir, PathBuf::from("x"));
    }

    #[test]
    fn sixteen_bit_files_match_the_eight_bit_export_within_quantisation() {
        let t = TempDir::new();
        let src = t.path("src/g.png");
        source_16bit(&src, 300, 20);
        let items = [item(1, src)];
        let (g, _, out) = make_graph(0.5);
        for (fmt8, fmt16, ext) in [(ExportFormat::Png, ExportFormat::Png16, "png"), (ExportFormat::Tiff, ExportFormat::Tiff16, "tif")] {
            let r8 = run(&g, out, &items, &settings(&t.path(&format!("o8{ext}")), fmt8));
            let r16 = run(&g, out, &items, &settings(&t.path(&format!("o16{ext}")), fmt16));
            let (p8, p16) = (written_path(&r8.items[0].1), written_path(&r16.items[0].1));
            assert!(matches!(r16.items[0].1, ItemStatus::Written { width: 300, height: 20, .. }));
            let a = image::open(&p8).expect("8-bit file");
            let b = image::open(&p16).expect("16-bit file");
            assert_eq!(a.color(), image::ColorType::Rgb8, "{ext}");
            assert_eq!(b.color(), image::ColorType::Rgb16, "{ext}");
            let (a, b) = (a.to_rgb8(), b.to_rgb16());
            assert_eq!(a.dimensions(), b.dimensions());
            let mut worst = 0.0f32;
            for (x, y) in a.as_raw().iter().zip(b.as_raw()) {
                worst = worst.max((*y as f32 / 65535.0 * 255.0 - *x as f32).abs());
            }
            let distinct: std::collections::BTreeSet<u16> = b.as_raw().chunks(3).map(|px| px[0]).collect();
            // The 8-bit value is the rounded 16-bit value (within half an 8-bit step plus float noise).
            assert!(worst <= 0.5 + 1e-3, "{ext}: 16-bit differs from 8-bit by {worst} steps");
            assert!(distinct.len() > 256, "{ext}: only {} distinct red values, not really 16-bit", distinct.len());
        }
    }

    #[test]
    fn profile_and_metadata_follow_the_flags_in_every_format() {
        let t = TempDir::new();
        let src = t.path("src/photo.jpg");
        source_with_exif(&src, 48, 32);
        let items = [item(1, src)];
        let (g, _, out) = make_graph(0.0);
        let formats = [
            (ExportFormat::Jpeg { quality: 90 }, "jpg"),
            (ExportFormat::Png, "png"),
            (ExportFormat::Tiff, "tif"),
            (ExportFormat::Png16, "png"),
            (ExportFormat::Tiff16, "tif"),
        ];
        for (k, (fmt, ext)) in formats.into_iter().enumerate() {
            for (n, (profile, metadata)) in PROFILE_FLAGS.into_iter().enumerate() {
                let mut s = settings(&t.path(&format!("out-{k}-{ext}-{n}")), fmt);
                s.embed_profile = profile;
                s.copy_metadata = metadata;
                let r = run(&g, out, &items, &s);
                let p = written_path(&r.items[0].1);
                let label = format!("{fmt:?} profile={profile} metadata={metadata}");
                assert_eq!(icc_of(&p).is_some(), profile, "{label}: profile");
                if profile {
                    assert_eq!(icc_of(&p).as_deref(), Some(rawgraph_io::srgb_icc_profile()), "{label}");
                }
                let ex = exif_of(&p).filter(|e| e.get_field(exif::Tag::Software, exif::In::PRIMARY).is_some());
                assert_eq!(ex.is_some(), metadata, "{label}: EXIF");
                if let Some(ex) = ex {
                    assert_eq!(ascii(&ex, exif::Tag::Make).as_deref(), Some("SONY"), "{label}");
                    assert_eq!(ascii(&ex, exif::Tag::Model).as_deref(), Some("ILCE-7M4"), "{label}");
                    assert_eq!(ascii(&ex, exif::Tag::LensModel).as_deref(), Some("FE 35mm F1.4 GM"), "{label}");
                    assert_eq!(ascii(&ex, exif::Tag::DateTimeOriginal).as_deref(), Some("2026:09:26 14:03:09"), "{label}");
                    assert_eq!(ascii(&ex, exif::Tag::Software).as_deref(), Some("RawGraph"), "{label}");
                    assert_eq!(ex.get_field(exif::Tag::Orientation, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)), Some(1));
                    assert_eq!(ex.get_field(exif::Tag::PhotographicSensitivity, exif::In::PRIMARY).and_then(|f| f.value.get_uint(0)), Some(640));
                }
            }
        }
    }

    #[test]
    fn tagged_export_decodes_like_the_untagged_one() {
        let t = TempDir::new();
        let src = t.path("src/photo.jpg");
        source_with_exif(&src, 48, 32);
        let items = [item(1, src)];
        let (g, _, out) = make_graph(0.0);
        let plain = settings(&t.path("plain"), ExportFormat::Png);
        let mut tagged = settings(&t.path("tagged"), ExportFormat::Png);
        tagged.embed_profile = true;
        tagged.copy_metadata = true;
        let (a, b) = (run(&g, out, &items, &plain), run(&g, out, &items, &tagged));
        let (pa, pb) = (written_path(&a.items[0].1), written_path(&b.items[0].1));
        // Our own profile is read back as sRGB, so the pixels do not change.
        let (la, lb) = (rawgraph_io::load(&pa, None).expect("plain"), rawgraph_io::load(&pb, None).expect("tagged"));
        assert_eq!(lb.profile, rawgraph_io::InputProfile::Srgb);
        assert_eq!(la.profile, rawgraph_io::InputProfile::Untagged);
        assert_eq!(la.image, lb.image);
    }

    #[test]
    fn source_without_exif_does_not_fail_the_item() {
        let t = TempDir::new();
        let src = t.path("src/plain.png");
        write_grey(&src, 16, 16, 90); // saved without any metadata
        let items = [item(1, src)];
        let (g, _, out) = make_graph(0.0);
        let all = [ExportFormat::Jpeg { quality: 90 }, ExportFormat::Png, ExportFormat::Tiff, ExportFormat::Png16, ExportFormat::Tiff16];
        for (k, fmt) in all.into_iter().enumerate() {
            let mut s = settings(&t.path(&format!("out-{k}")), fmt);
            s.embed_profile = true;
            s.copy_metadata = true;
            let r = run(&g, out, &items, &s);
            assert_eq!((r.written(), r.failed()), (1, 0), "{fmt:?}");
            let p = written_path(&r.items[0].1);
            let ex = exif_of(&p).expect("exif block");
            assert_eq!(ascii(&ex, exif::Tag::Software).as_deref(), Some("RawGraph"), "{fmt:?}");
            assert!(ascii(&ex, exif::Tag::Make).is_none(), "{fmt:?}");
            assert!(icc_of(&p).is_some(), "{fmt:?}");
        }
    }

    #[test]
    fn exr_ignores_profile_and_metadata_flags() {
        let t = TempDir::new();
        let src = t.path("src/photo.jpg");
        source_with_exif(&src, 16, 16);
        let items = [item(1, src)];
        let (g, _, out) = make_graph(0.0);
        let mut s = settings(&t.path("out"), ExportFormat::Exr);
        s.embed_profile = true;
        s.copy_metadata = true;
        let r = run(&g, out, &items, &s);
        let (w, h, _) = read_exr(&written_path(&r.items[0].1));
        assert_eq!((w, h), (16, 16));
    }
}
