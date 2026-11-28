//! Export of Panorama nodes (and of HDR Merge followed by Panorama): one file per panorama at its own
//! size, the long-edge limit applied to that size, every other member reported as merged into it.

use std::fs;
use std::path::Path;

use rawgraph_core::{CaptureInfo, ExportText, Graph, ImageId, NodeData, NodeId, NodeKind};
use rawgraph_engine::CancelToken;
use rawgraph_scopes::DisplayTransform;

use super::tests::{item, TempDir};
use super::*;

const A: ImageId = ImageId(0);

fn pano_hash(ix: i32, iy: i32, seed: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(0x27d4_eb2d) ^ (iy as u32).wrapping_mul(0x1656_67b1) ^ seed.wrapping_mul(0x9E37_79B9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h & 0x00FF_FFFF) as f32 / 16_777_216.0
}

/// Value-noise scene (cells 96..12 px), 0.08..0.83 per channel.
fn pano_scene(x: f64, y: f64) -> [f32; 3] {
    let noise = |cell: f64, seed: u32| {
        let (gx, gy) = (x / cell, y / cell);
        let (ix, iy) = (gx.floor(), gy.floor());
        let (fx, fy) = ((gx - ix) as f32, (gy - iy) as f32);
        let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
        let (ix, iy) = (ix as i32, iy as i32);
        let a = pano_hash(ix, iy, seed) * (1.0 - sx) + pano_hash(ix + 1, iy, seed) * sx;
        let b = pano_hash(ix, iy + 1, seed) * (1.0 - sx) + pano_hash(ix + 1, iy + 1, seed) * sx;
        a * (1.0 - sy) + b * sy
    };
    let mut c = [0.0f32; 3];
    for (ch, cv) in c.iter_mut().enumerate() {
        let (mut v, mut amp) = (0.0, 0.5);
        for (o, cell) in [96.0, 48.0, 24.0, 12.0].iter().enumerate() {
            v += amp * noise(*cell, 5 + ch as u32 * 17 + o as u32 * 131);
            amp *= 0.8;
        }
        *cv = 0.08 + 0.75 * v / 1.476;
    }
    c
}

/// An 8-bit sRGB PNG of the cylinder scene seen at `yaw_deg`, scaled by `gain` (clipped at 1).
fn write_cylinder_frame(path: &Path, yaw_deg: f64, gain: f32) {
    const F: f64 = 520.0;
    let (w, h) = (480u32, 320u32);
    let (sy, cy) = yaw_deg.to_radians().sin_cos();
    let mut data = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        for x in 0..w {
            let (px, py) = (x as f64 + 0.5 - w as f64 / 2.0, y as f64 + 0.5 - h as f64 / 2.0);
            let (rx, ry, rz) = (px / F, py / F, 1.0);
            let (wx, wz) = (rx * cy + rz * sy, -rx * sy + rz * cy);
            let c = pano_scene(F * wx.atan2(wz), F * ry / wx.hypot(wz));
            for v in c {
                data.push(((v * gain).clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0).round() as u8);
            }
        }
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_default();
    }
    rawgraph_io::save_srgb8(path, w, h, &data, 95, true).expect("write test frame");
}

const YAWS: [f64; 4] = [-30.0, -10.0, 10.0, 30.0];

/// Capture records by file name: `p{pos}_{k}.png` was shot at 3 s * pos + 0.1 s * k with the shutter
/// time `shutters[k]` and the 35-mm focal length that gives 520 px for a 480 x 320 frame.
fn pano_capture(shutters: &'static [f32]) -> impl Fn(&Path) -> Option<CaptureInfo> + Sync {
    move |path: &Path| {
        let name = path.file_stem()?.to_str()?;
        let (pos, k) = name.strip_prefix('p')?.split_once('_')?;
        let (pos, k): (i64, usize) = (pos.parse().ok()?, k.parse().ok()?);
        Some(CaptureInfo {
            captured_unix_ms: Some(pos * 3_000 + k as i64 * 100),
            shutter_s: Some(shutters[k]),
            iso: Some(100.0),
            aperture: Some(2.8),
            focal_35mm: Some((520.0 * 43.2666 / (480f64).hypot(320.0)) as f32),
            ..Default::default()
        })
    }
}

fn pano_rig(t: &TempDir, exposures: &[f32]) -> Vec<ExportItem> {
    let mut catalog = Vec::new();
    for (pos, yaw) in YAWS.iter().enumerate() {
        for (k, gain) in exposures.iter().enumerate() {
            let p = t.path(&format!("src/p{pos}_{k}.png"));
            write_cylinder_frame(&p, *yaw, *gain);
            catalog.push(item((pos * exposures.len() + k) as u64 + 1, p));
        }
    }
    catalog
}

/// Source -> `kinds` in a chain -> Export (PNG) writing to `out`.
fn export_graph(kinds: &[NodeKind], out: &Path) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let mut prev = g.add_node(NodeKind::Source, [0.0; 2]);
    for kind in kinds {
        let n = g.add_node(*kind, [0.0; 2]);
        g.connect(prev, n, 0).expect("connect");
        prev = n;
    }
    let exp = g.add_node(NodeKind::Export, [0.0; 2]);
    g.connect(prev, exp, 0).expect("connect");
    let node = g.node_mut(exp).expect("export node");
    assert!(node.set_param("format", A, 1.0));
    let tx = ExportText { out_dir: out.to_string_lossy().into_owned(), ..Default::default() };
    assert!(node.set_data(A, Some(NodeData::Export(tx))));
    (g, exp)
}

fn run(g: &Graph, exp: NodeId, catalog: &[ExportItem], capture: &(dyn Fn(&Path) -> Option<CaptureInfo> + Sync)) -> ExportReport {
    let ids: Vec<ImageId> = catalog.iter().map(|i| i.id).collect();
    export_node_with(g, exp, catalog, &ids, DisplayTransform::default(), &CancelToken::new(), &mut |_| {}, capture, &|_| None).expect("export")
}

#[test]
fn a_panorama_is_written_once_at_its_own_size_and_the_long_edge_applies_to_that_size() {
    let t = TempDir::new();
    let catalog = pano_rig(&t, &[1.0]);
    let (mut g, exp) = export_graph(&[NodeKind::Panorama], &t.path("out"));
    let capture = pano_capture(&[0.01]);
    // Original size: the panorama, wider than any frame (four frames 20 degrees apart: about 990 px).
    let rep = run(&g, exp, &catalog, &capture);
    assert_eq!((rep.written(), rep.merged(), rep.failed(), rep.excluded()), (1, 3, 0, 0), "{:?}", rep.items);
    let (path, w, h) = match &rep.items[1].1 {
        ItemStatus::Written { path, width, height } => (path.clone(), *width, *height),
        other => panic!("the reference (image 2) is written, got {other:?}"),
    };
    assert!(w > 900 && h <= 330, "{w} x {h}");
    assert_eq!(image::open(&path).expect("read back").width(), w);
    for k in [0usize, 2, 3] {
        assert_eq!(rep.items[k].1, ItemStatus::MergedInto(ImageId(2)), "image {}", k + 1);
    }
    // A long edge of 600 px shrinks the panorama (not a frame): the long edge is the panorama's width.
    set_out(&mut g, exp, &t.path("out2"));
    assert!(g.node_mut(exp).expect("export").set_param("size", A, 1.0));
    assert!(g.node_mut(exp).expect("export").set_param("long_edge", A, 600.0));
    let small = run(&g, exp, &catalog, &capture);
    match &small.items[1].1 {
        ItemStatus::Written { width, height, .. } => {
            assert_eq!(*width, 600);
            assert_eq!(*height, ((h as f64 * 600.0 / w as f64).round() as u32).max(1), "the aspect of the panorama is kept");
        }
        other => panic!("{other:?}"),
    }
    // A limit above the panorama's size never enlarges it.
    set_out(&mut g, exp, &t.path("out3"));
    assert!(g.node_mut(exp).expect("export").set_param("long_edge", A, 4000.0));
    let big = run(&g, exp, &catalog, &capture);
    assert!(matches!(&big.items[1].1, ItemStatus::Written { width, .. } if *width == w), "{:?}", big.items[1]);
}

#[test]
fn an_hdr_panorama_chain_writes_one_image_and_reports_every_bracket_frame_as_merged_into_it() {
    let t = TempDir::new();
    // Brackets of three (gain 0.35, 1, 2 with matching shutter times), four positions.
    let catalog = pano_rig(&t, &[0.35, 1.0, 2.0]);
    let (g, exp) = export_graph(&[NodeKind::HdrMerge, NodeKind::Panorama], &t.path("out"));
    let capture = pano_capture(&[0.0035, 0.01, 0.02]);
    let rep = run(&g, exp, &catalog, &capture);
    // HDR references are the middle exposures 2, 5, 8, 11; the panorama's is the second of them (5).
    assert_eq!((rep.written(), rep.failed(), rep.excluded()), (1, 0, 0), "{:?}", rep.items);
    assert_eq!(rep.merged(), 11);
    assert!(matches!(rep.items[4].1, ItemStatus::Written { .. }), "{:?}", rep.items[4]);
    for (k, (id, status)) in rep.items.iter().enumerate() {
        if k != 4 {
            assert_eq!(*status, ItemStatus::MergedInto(ImageId(5)), "image {}", id.0);
        }
    }
    assert!(rep.retry_items(&catalog).is_empty(), "merged images are not retried");
}

#[test]
fn a_panorama_that_cannot_be_stitched_fails_its_reference_with_the_file_names() {
    let t = TempDir::new();
    let mut catalog = pano_rig(&t, &[1.0]);
    // A fifth frame of the same sweep that shows a different place.
    let stranger = t.path("src/p4_0.png");
    write_cylinder_frame(&stranger, 130.0, 1.0);
    catalog.push(item(5, stranger));
    let (g, exp) = export_graph(&[NodeKind::Panorama], &t.path("out"));
    let rep = run(&g, exp, &catalog, &pano_capture(&[0.01]));
    assert_eq!(rep.written(), 0);
    let failed: Vec<&(ImageId, ItemStatus)> = rep.items.iter().filter(|(_, s)| matches!(s, ItemStatus::Failed(_))).collect();
    assert_eq!(failed.len(), 1, "{:?}", rep.items);
    match &failed[0].1 {
        ItemStatus::Failed(m) => assert!(m.contains("do not overlap") && m.contains("p4_0.png"), "{m}"),
        other => panic!("{other:?}"),
    }
}

/// Points the Export node at another output folder (a second run must not find the first one's files).
fn set_out(g: &mut Graph, exp: NodeId, out: &Path) {
    let tx = ExportText { out_dir: out.to_string_lossy().into_owned(), ..Default::default() };
    assert!(g.node_mut(exp).expect("export node").set_data(A, Some(NodeData::Export(tx))));
}

#[test]
fn stitching_findings_are_listed_in_the_export_report() {
    let t = TempDir::new();
    let catalog = pano_rig(&t, &[1.0]);
    let (g, exp) = export_graph(&[NodeKind::Panorama], &t.path("out"));
    // No focal length recorded: the stitcher estimates it and says so.
    let inner = pano_capture(&[0.01]);
    let without_focal = move |p: &Path| inner(p).map(|c| CaptureInfo { focal_35mm: None, focal_mm: None, ..c });
    let rep = run(&g, exp, &catalog, &without_focal);
    assert_eq!(rep.written(), 1, "{:?}", rep.items);
    assert!(rep.warnings.iter().any(|(id, w)| *id == ImageId(2) && w.contains("no focal length is recorded")), "{:?}", rep.warnings);
    // With the focal length there is nothing to report.
    let mut g = g;
    set_out(&mut g, exp, &t.path("out2"));
    let ok = run(&g, exp, &catalog, &pano_capture(&[0.01]));
    assert!(ok.warnings.iter().all(|(_, w)| !w.contains("focal")), "{:?}", ok.warnings);
}
