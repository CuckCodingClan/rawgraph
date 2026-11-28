//! Tests of the Panorama node (and of HDR Merge followed by Panorama) on procedurally generated
//! cylinder scenes: sizes, identity, focal length, caching, messages and the HDR panorama chain.

use std::collections::HashMap;
use std::sync::Mutex;

use rayon::prelude::*;

use super::*;

// ---------------------------------------------------------------- scene

fn hash(ix: i32, iy: i32, seed: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(0x27d4_eb2d) ^ (iy as u32).wrapping_mul(0x1656_67b1) ^ seed.wrapping_mul(0x9E37_79B9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h & 0x00FF_FFFF) as f32 / 16_777_216.0
}

fn vnoise(x: f64, y: f64, cell: f64, seed: u32) -> f32 {
    let (gx, gy) = (x / cell, y / cell);
    let (ix, iy) = (gx.floor(), gy.floor());
    let (fx, fy) = ((gx - ix) as f32, (gy - iy) as f32);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (ix, iy) = (ix as i32, iy as i32);
    let a = hash(ix, iy, seed) * (1.0 - sx) + hash(ix + 1, iy, seed) * sx;
    let b = hash(ix, iy + 1, seed) * (1.0 - sx) + hash(ix + 1, iy + 1, seed) * sx;
    a * (1.0 - sy) + b * sy
}

/// Textured scene: value noise (cells 96..12 px) plus soft-edged discs; channels in 0.08..~0.9.
fn scene(x: f64, y: f64, seed: u32) -> [f32; 3] {
    let mut c = [0.0f32; 3];
    for (ch, cv) in c.iter_mut().enumerate() {
        let mut v = 0.0;
        let mut amp = 0.5;
        for (o, cell) in [96.0, 48.0, 24.0, 12.0].iter().enumerate() {
            v += amp * vnoise(x, y, *cell, seed + ch as u32 * 17 + o as u32 * 131);
            amp *= 0.8;
        }
        *cv = 0.08 + 0.75 * v / 1.476;
    }
    let (gx, gy) = ((x / 140.0).floor() as i32, (y / 140.0).floor() as i32);
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (cx, cy) = (gx + dx, gy + dy);
            let (px, py) = ((cx as f64 + hash(cx, cy, seed + 7) as f64) * 140.0, (cy as f64 + hash(cx, cy, seed + 8) as f64) * 140.0);
            let r = 10.0 + 22.0 * hash(cx, cy, seed + 9) as f64;
            let d = (x - px).hypot(y - py);
            let cov = ((r + 1.5 - d) / 3.0).clamp(0.0, 1.0) as f32;
            if cov > 0.0 {
                for (ch, cv) in c.iter_mut().enumerate() {
                    let col = 0.1 + 0.9 * hash(cx, cy, seed + 20 + ch as u32);
                    *cv = *cv * (1.0 - cov) + col * cov;
                }
            }
        }
    }
    c
}

/// Scene radiance of the HDR panorama: the scene scaled to 0.5..~5 with a very bright window.
fn hdr_radiance(x: f64, y: f64, seed: u32) -> [f32; 3] {
    let mut c = scene(x, y, seed);
    for v in c.iter_mut() {
        *v *= 6.0;
    }
    // A bright window (soft edge of 12 px), 24 above the surroundings.
    let ramp = |d: f64, half: f64| {
        let t = ((half - d.abs()) / 12.0).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let window = (ramp(x - 150.0, 55.0) * ramp(y + 20.0, 60.0)) as f32;
    for v in c.iter_mut() {
        *v += 24.0 * window;
    }
    c
}

/// A pinhole camera at yaw `yaw_deg` looking at the cylinder scene `radiance(f * theta, f * height)`.
fn cyl_frame(w: u32, h: u32, f: f64, yaw_deg: f64, radiance: impl Fn(f64, f64) -> [f32; 3] + Sync) -> ImageBuf {
    let (sy, cy) = yaw_deg.to_radians().sin_cos();
    let mut data = vec![0.0f32; w as usize * h as usize * 4];
    data.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let (px, py) = (x as f64 + 0.5 - w as f64 / 2.0, y as f64 + 0.5 - h as f64 / 2.0);
            let (rx, ry, rz) = (px / f, py / f, 1.0);
            let (wx, wz) = (rx * cy + rz * sy, -rx * sy + rz * cy);
            let theta = wx.atan2(wz);
            let hh = ry / wx.hypot(wz);
            let c = radiance(f * theta, f * hh);
            row[x * 4..x * 4 + 3].copy_from_slice(&c);
            row[x * 4 + 3] = 1.0;
        }
    });
    ImageBuf::from_rgba(w, h, data).unwrap()
}

/// PSNR (peak 1) of the whole panorama against the cylinder truth; `theta0` is the world yaw of its centre.
fn cyl_psnr(img: &ImageBuf, origin: [i32; 2], f: f64, theta0: f64, truth: impl Fn(f64, f64) -> [f32; 3] + Sync) -> f64 {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let d = img.data();
    let sse: f64 = (0..h)
        .into_par_iter()
        .map(|y| {
            let mut e = 0.0f64;
            for x in 0..w {
                let theta = theta0 + (origin[0] as f64 + x as f64 + 0.5) / f;
                let hh = (origin[1] as f64 + y as f64 + 0.5) / f;
                let t = truth(f * theta, f * hh);
                for c in 0..3 {
                    e += ((d[(y * w + x) * 4 + c] - t[c]) as f64).powi(2);
                }
            }
            e
        })
        .sum();
    10.0 * (1.0 / (sse / (w * h * 3) as f64).max(1e-12)).log10()
}

// ---------------------------------------------------------------- provider

#[derive(Default)]
struct Prov {
    frames: HashMap<u64, Arc<ImageBuf>>,
    caps: Mutex<HashMap<u64, CaptureInfo>>,
    full: HashMap<u64, (u32, u32)>,
    order: Vec<ImageId>,
}

impl Prov {
    fn add(&mut self, n: u64, frame: ImageBuf, cap: CaptureInfo) {
        self.frames.insert(n, Arc::new(frame));
        self.caps.lock().unwrap().insert(n, cap);
        self.order.push(ImageId(n));
    }
}

impl ImageProvider for Prov {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        self.frames.get(&id.0).cloned().ok_or_else(|| format!("no image {}", id.0))
    }
    fn capture(&self, id: ImageId) -> Option<CaptureInfo> {
        self.caps.lock().unwrap().get(&id.0).cloned()
    }
    fn images(&self) -> Vec<ImageId> {
        self.order.clone()
    }
    fn full_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.full.get(&id.0).copied()
    }
}

const F: f64 = 520.0;
const W: u32 = 480;
const H: u32 = 320;

/// The 35-mm-equivalent focal length that makes a frame of W x H pixels have a focal length of `F` pixels.
fn f35() -> f32 {
    (F * FULL_FRAME_DIAGONAL_MM / (W as f64).hypot(H as f64)) as f32
}

fn caps_at(t_ms: i64, shutter: f32, focal: Option<f32>, name: &str) -> CaptureInfo {
    CaptureInfo {
        captured_unix_ms: Some(t_ms),
        shutter_s: Some(shutter),
        iso: Some(100.0),
        aperture: Some(1.0),
        focal_35mm: focal,
        focal_mm: focal.map(|f| f / 5.0),
        path: Some(format!("C:/shots/{name}")),
        ..Default::default()
    }
}

fn node_with(g: &mut Graph, kind: NodeKind) -> NodeId {
    g.add_node(kind, [0.0; 2])
}

fn pano_graph() -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::new();
    let src = node_with(&mut g, NodeKind::Source);
    let pano = node_with(&mut g, NodeKind::Panorama);
    let out = node_with(&mut g, NodeKind::Output);
    g.connect(src, pano, 0).unwrap();
    g.connect(pano, out, 0).unwrap();
    (g, src, pano, out)
}

fn set(g: &mut Graph, n: NodeId, name: &str, v: f32) {
    g.node_mut(n).unwrap().set_param(name, ImageId(0), v);
}

const YAWS: [f64; 4] = [-30.0, -10.0, 10.0, 30.0];

/// Four LDR frames of the cylinder scene, 3 s apart; `focal` is the recorded 35-mm focal length.
fn ldr_provider(focal: Option<f32>) -> Prov {
    let mut p = Prov::default();
    for (k, yaw) in YAWS.iter().enumerate() {
        let frame = cyl_frame(W, H, F, *yaw, |x, y| scene(x, y, 23));
        p.add(k as u64 + 1, frame, caps_at(k as i64 * 3_000, 0.01, focal, &format!("pano_{k}.jpg")));
    }
    p
}

// ---------------------------------------------------------------- focal length

fn cap_focal(real: Option<f32>, equiv: Option<f32>) -> Option<CaptureInfo> {
    Some(CaptureInfo { focal_mm: real, focal_35mm: equiv, ..Default::default() })
}

#[test]
fn the_focal_length_in_pixels_follows_the_35mm_equivalent_and_the_frame_diagonal() {
    let size = (6000, 4000);
    let diag = (6000f64).hypot(4000.0);
    let caps = vec![cap_focal(Some(24.0), Some(36.0)); 3];
    let (px, notes) = pano_focal(0, 0.0, &caps, size);
    assert!(notes.is_empty());
    assert!((px.unwrap() as f64 - 36.0 * diag / FULL_FRAME_DIAGONAL_MM).abs() < 0.5, "{px:?}");
    // A 36 mm lens on a 3:2 frame sees 36 mm over the long side: the focal length in pixels is the long side.
    assert!((px.unwrap() as f64 - 6000.0).abs() < 1.0, "{px:?}");
    // The typed value wins over the metadata and over "Estimate".
    let (px, notes) = pano_focal(0, 50.0, &caps, size);
    assert!(notes.is_empty() && (px.unwrap() as f64 - 50.0 * diag / FULL_FRAME_DIAGONAL_MM).abs() < 0.5);
    let (px, notes) = pano_focal(1, 50.0, &caps, size);
    assert!(notes.is_empty() && px.is_some());
    // "Estimate" without a typed value leaves it to the stitcher.
    assert_eq!(pano_focal(1, 0.0, &caps, size), (None, Vec::new()));
    // A non-positive or non-finite typed value does not count.
    assert_eq!(pano_focal(1, f32::NAN, &caps, size), (None, Vec::new()));
}

#[test]
fn missing_or_mixed_focal_lengths_fall_back_to_the_estimate_with_a_note() {
    let size = (4000, 3000);
    // No metadata at all.
    let (px, notes) = pano_focal(0, 0.0, &[None, None], size);
    assert_eq!(px, None);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("no focal length is recorded") && notes[0].contains("estimated"), "{notes:?}");
    // Only the real focal length (no 35-mm equivalent): named as such.
    let (px, notes) = pano_focal(0, 0.0, &[cap_focal(Some(24.0), None), cap_focal(Some(24.0), None)], size);
    assert_eq!(px, None);
    assert!(notes[0].contains("only the real focal length"), "{notes:?}");
    // One frame without the value.
    let (px, notes) = pano_focal(0, 0.0, &[cap_focal(None, Some(35.0)), cap_focal(None, None)], size);
    assert_eq!(px, None);
    assert_eq!(notes.len(), 1);
    // Different focal lengths (a zoom): estimated, and the note says which.
    let (px, notes) = pano_focal(0, 0.0, &[cap_focal(None, Some(24.0)), cap_focal(None, Some(35.0))], size);
    assert_eq!(px, None);
    assert!(notes[0].contains("different focal lengths") && notes[0].contains("24") && notes[0].contains("35"), "{notes:?}");
    // 1 % of rounding noise between frames is the same lens.
    let (px, notes) = pano_focal(0, 0.0, &[cap_focal(None, Some(35.0)), cap_focal(None, Some(35.2))], size);
    assert!(px.is_some() && notes.is_empty());
    // The 4:3 frame uses its own diagonal: the same 35-mm value gives a different pixel count than 3:2.
    let (a, _) = pano_focal(0, 35.0, &[], (4000, 3000));
    let (b, _) = pano_focal(0, 35.0, &[], (4000, 2667));
    assert!(a.unwrap() > b.unwrap());
}

// ---------------------------------------------------------------- the node in the engine

#[test]
fn a_panorama_group_becomes_one_wide_image_with_the_identity_of_its_middle_frame() {
    let p = ldr_provider(Some(f35()));
    let (g, _src, pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    // Four frames 3 s apart: one group; the lower middle (frame 2) stands for it.
    assert_eq!(ev.members(&g, out, 0, &p), vec![ImageId(2)]);
    let img = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    // The output is the panorama, not the size of the reference frame.
    assert!(img.width() > W + 200, "{}x{}", img.width(), img.height());
    assert!(img.height() <= H + 40, "{}", img.height());
    assert_ne!((img.width(), img.height()), (W, H));
    // A frame that is not the reference is not part of the output collection.
    assert_eq!(ev.eval(&g, pano, ImageId(1), &p, &c).unwrap_err(), EvalError::NotInCollection { node: pano });
    assert!(matches!(ev.eval(&g, out, ImageId(4), &p, &c), Err(EvalError::NotInCollection { .. })));
    // The focal length came from the metadata, so there is nothing to warn about.
    assert!(ev.notes().is_empty(), "{:?}", ev.notes());
}

#[test]
fn a_panorama_with_the_recorded_focal_length_matches_the_scene() {
    let p = ldr_provider(Some(f35()));
    let (g, _src, _pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    let img = ev.eval_image(&g, out, ImageId(2), &p, &CancelToken::new()).unwrap();
    // The same stitch directly with the maths gives the origin and the reference of the geometry.
    let frames: Vec<Arc<ImageBuf>> = (1..=4).map(|n| p.image(ImageId(n)).unwrap()).collect();
    let refs: Vec<&ImageBuf> = frames.iter().map(|f| f.as_ref()).collect();
    let params = fx::pano::PanoParams { focal_px: Some(F as f32), exposure_clip: None, ..Default::default() };
    let direct = fx::pano::stitch(&refs, &params, &|| false).unwrap();
    assert_eq!((img.width(), img.height()), (direct.image.width(), direct.image.height()));
    let worst = img.data().iter().zip(direct.image.data()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(worst < 1e-4, "the node differs from the stitch by {worst}");
    let theta0 = YAWS[direct.reference].to_radians();
    let psnr = cyl_psnr(&img, direct.origin, F, theta0, |x, y| scene(x, y, 23));
    println!("panorama node (recorded focal) PSNR {psnr:.2} dB, {}x{}", img.width(), img.height());
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

#[test]
fn without_a_recorded_focal_length_the_estimate_is_used_and_noted_once() {
    let p = ldr_provider(None);
    let (g, _src, _pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    let img = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    assert!(img.width() > W + 200);
    let notes = ev.take_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].1, ImageId(2));
    assert!(notes[0].2.contains("no focal length is recorded"), "{notes:?}");
    // The second evaluation is served from the cache: no new note.
    let again = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    assert!(Arc::ptr_eq(&img, &again));
    assert!(ev.notes().is_empty());
}

#[test]
fn the_typed_focal_length_replaces_the_metadata_and_changes_the_identity() {
    let p = ldr_provider(Some(f35()));
    let (mut g, _src, pano, _out) = pano_graph();
    let ev = Evaluator::new();
    let fp = |g: &Graph| ev.fingerprint(g, pano, ImageId(2), &p).unwrap();
    let base = fp(&g);
    set(&mut g, pano, "focal_mm_35", 30.0);
    let typed = fp(&g);
    assert_ne!(typed, base);
    set(&mut g, pano, "projection", 1.0);
    assert_ne!(fp(&g), typed, "the projection is part of the result");
    // The recorded focal length is part of the identity too.
    let g2 = pano_graph().0;
    let before = ev.fingerprint(&g2, pano, ImageId(2), &p).unwrap();
    p.caps.lock().unwrap().get_mut(&3).unwrap().focal_35mm = Some(50.0);
    assert_ne!(ev.fingerprint(&g2, pano, ImageId(2), &p).unwrap(), before);
}

#[test]
fn the_cache_serves_a_repeat_and_lean_mode_keeps_no_member_entries() {
    let p = ldr_provider(Some(f35()));
    let (g, _src, _pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    let first = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    let misses = ev.misses();
    let again = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    assert_eq!(ev.misses(), misses);
    assert!(Arc::ptr_eq(&first, &again));
    let mut lean = Evaluator::new();
    lean.set_keep_intermediates(false);
    lean.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    for (node, image, _) in lean.cache.keys() {
        assert!(*image == ImageId(2) && *node == out, "unexpected cache entry {node:?} / {image:?}");
    }
}

#[test]
fn a_cancelled_evaluation_ends_before_any_stitching() {
    let p = ldr_provider(Some(f35()));
    let (g, _src, _pano, out) = pano_graph();
    let c = CancelToken::new();
    c.cancel();
    assert_eq!(Evaluator::new().eval_image(&g, out, ImageId(2), &p, &c).unwrap_err(), EvalError::Cancelled);
}

#[test]
fn a_frame_that_does_not_overlap_is_named_by_file_in_the_error() {
    let mut p = ldr_provider(Some(f35()));
    // A fifth frame of another scene, shot in the same sweep: it cannot be connected.
    let other = cyl_frame(W, H, F, 0.0, |x, y| scene(x, y, 991));
    p.add(5, other, caps_at(12_000, 0.01, Some(f35()), "stranger.jpg"));
    let (g, _src, _pano, out) = pano_graph();
    let members = Evaluator::new().members(&g, out, 0, &p);
    assert_eq!(members.len(), 1);
    let err = Evaluator::new().eval_image(&g, out, members[0], &p, &CancelToken::new()).unwrap_err();
    match err {
        EvalError::Failed { message, .. } => {
            assert!(message.starts_with("Panorama: these images do not overlap"), "{message}");
            assert!(message.contains("stranger.jpg") && !message.contains("pano_1.jpg"), "{message}");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_lone_image_passes_through_and_the_budget_check_is_exact() {
    let mut p = ldr_provider(Some(f35()));
    let lone = cyl_frame(W, H, F, 0.0, |x, y| scene(x, y, 5));
    p.add(9, lone, caps_at(600_000, 0.01, Some(f35()), "alone.jpg"));
    let (g, _src, _pano, out) = pano_graph();
    let img = Evaluator::new().eval_image(&g, out, ImageId(9), &p, &CancelToken::new()).unwrap();
    assert!(Arc::ptr_eq(&img, &p.frames[&9]), "a single frame is not stitched");
}

// ---------------------------------------------------------------- Layer, sizes

#[test]
fn a_panorama_feeding_a_layer_sets_the_canvas_size_and_the_crop_scale_follows_the_frames() {
    let mut p = ldr_provider(Some(f35()));
    // The files are twice as large as the proxies the provider hands out.
    for n in 1..=4 {
        p.full.insert(n, (W * 2, H * 2));
    }
    let mut g = Graph::new();
    let src = node_with(&mut g, NodeKind::Source);
    let pano = node_with(&mut g, NodeKind::Panorama);
    let layer = node_with(&mut g, NodeKind::Layer);
    let out = node_with(&mut g, NodeKind::Output);
    g.connect(src, pano, 0).unwrap();
    g.connect(pano, layer, 0).unwrap(); // layer 1: the panorama (canvas)
    g.connect(pano, layer, 1).unwrap(); // layer 2: the same panorama
    g.connect(layer, out, 0).unwrap();
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    let img = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    let flat = ev.eval_image(&g, pano, ImageId(2), &p, &c).unwrap();
    assert_eq!((img.width(), img.height()), (flat.width(), flat.height()), "the canvas is the panorama, not the frame");
    // The full size that belongs to the panorama picture scales with the frames' proxy ratio.
    let size = (flat.width(), flat.height());
    let full = layer_full_size(&g, layer, 0, &p, ImageId(2), size).unwrap();
    assert_eq!(full, (size.0 * 2, size.1 * 2));
    // Behind a plain Source the file's own full size counts.
    let mut g2 = Graph::new();
    let s2 = node_with(&mut g2, NodeKind::Source);
    let l2 = node_with(&mut g2, NodeKind::Layer);
    g2.connect(s2, l2, 0).unwrap();
    assert_eq!(layer_full_size(&g2, l2, 0, &p, ImageId(2), (W, H)), Some((W * 2, H * 2)));
    // No full size known: none (the layer scale is 1).
    p.full.clear();
    assert_eq!(layer_full_size(&g, layer, 0, &p, ImageId(2), size), None);
}

// ---------------------------------------------------------------- HDR panorama

/// The RAW loader's white balance with clip neutralisation (rawgraph-io `white_balance_pixel`).
fn loader_pixel(p: [f32; 3], wb: [f32; 3]) -> [f32; 3] {
    const SOFT: f32 = 0.90;
    let v = [p[0] * wb[0], p[1] * wb[1], p[2] * wb[2]];
    let sensor_max = p[0].max(p[1]).max(p[2]);
    if sensor_max <= SOFT {
        return v;
    }
    let t = ((sensor_max - SOFT) / (1.0 - SOFT)).min(1.0);
    let w = t * t * (3.0 - 2.0 * t);
    let mx = v[0].max(v[1]).max(v[2]);
    [v[0] + w * (mx - v[0]), v[1] + w * (mx - v[1]), v[2] + w * (mx - v[2])]
}

/// Brightness of the scene at the reference exposure (0 EV): the darkest frame (-2 EV) of the 30x window stays below 1.
const S0: f32 = 0.12;
const WB: [f32; 3] = [2.0, 1.0, 1.5];
const BRACKET: [(f32, f32); 3] = [(0.0025, 0.25), (0.01, 1.0), (0.04, 4.0)];

/// Four positions x three exposures (EV -2, 0, +2, 0.1 s apart, shutter 1/400, 1/100, 1/25 s), positions 3 s apart.
/// Image numbers 3 * position + k + 1. The sensor clips at 1 and the loader neutralises clipped pixels.
fn hdr_provider() -> Prov {
    let mut p = Prov::default();
    for (pos, yaw) in YAWS.iter().enumerate() {
        let radiance = cyl_frame(W, H, F, *yaw, |x, y| hdr_radiance(x, y, 41));
        for (k, (shutter, gain)) in BRACKET.iter().enumerate() {
            let mut data = radiance.data().to_vec();
            for px in data.as_chunks_mut::<4>().0.iter_mut() {
                let mut sensor = [0.0f32; 3];
                for c in 0..3 {
                    sensor[c] = (gain * S0 * px[c] / WB[c]).clamp(0.0, 1.0);
                }
                let o = loader_pixel(sensor, WB);
                px[..3].copy_from_slice(&o);
            }
            let frame = ImageBuf::from_rgba(W, H, data).unwrap();
            let n = pos as u64 * 3 + k as u64 + 1;
            p.add(n, frame, caps_at(pos as i64 * 3_000 + k as i64 * 100, *shutter, Some(f35()), &format!("hdr_{pos}_{k}.dng")));
        }
    }
    p
}

fn chain_graph() -> (Graph, NodeId, NodeId, NodeId) {
    let mut g = Graph::new();
    let src = node_with(&mut g, NodeKind::Source);
    let hdr = node_with(&mut g, NodeKind::HdrMerge);
    let pano = node_with(&mut g, NodeKind::Panorama);
    let out = node_with(&mut g, NodeKind::Output);
    g.connect(src, hdr, 0).unwrap();
    g.connect(hdr, pano, 0).unwrap();
    g.connect(pano, out, 0).unwrap();
    (g, hdr, pano, out)
}

#[test]
fn hdr_merge_then_panorama_recovers_the_highlights_across_the_whole_panorama() {
    let p = hdr_provider();
    let (g, hdr, _pano, out) = chain_graph();
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    // Twelve frames: four brackets (HDR references 2, 5, 8, 11), one panorama with the second as reference.
    assert_eq!(ev.members(&g, hdr, 0, &p), vec![ImageId(2), ImageId(5), ImageId(8), ImageId(11)]);
    assert_eq!(ev.members(&g, out, 0, &p), vec![ImageId(5)]);
    let img = ev.eval_image(&g, out, ImageId(5), &p, &c).unwrap();
    assert!(img.width() > W + 200, "{}x{}", img.width(), img.height());
    // Direct computation: merge every bracket, stitch the merged frames with the same settings.
    let merged: Vec<Arc<ImageBuf>> = [2u64, 5, 8, 11].iter().map(|&r| ev.eval_image(&g, hdr, ImageId(r), &p, &c).unwrap()).collect();
    let refs: Vec<&ImageBuf> = merged.iter().map(|f| f.as_ref()).collect();
    let params = fx::pano::PanoParams { focal_px: Some(F as f32), exposure_clip: None, ..Default::default() };
    let direct = fx::pano::stitch(&refs, &params, &|| false).unwrap();
    assert_eq!((img.width(), img.height()), (direct.image.width(), direct.image.height()));
    let worst = img.data().iter().zip(direct.image.data()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(worst < 1e-3, "the chain differs from merge-then-stitch by {worst}");
    // Against the truth: the scene at the reference exposure (values above 1 where the window is).
    let theta0 = YAWS[direct.reference].to_radians();
    let truth = |x: f64, y: f64| {
        let r = hdr_radiance(x, y, 41);
        [S0 * r[0], S0 * r[1], S0 * r[2]]
    };
    let psnr = cyl_psnr(&img, direct.origin, F, theta0, truth);
    println!("HDR panorama PSNR {psnr:.2} dB, {}x{}", img.width(), img.height());
    assert!(psnr >= 30.0, "PSNR {psnr}");
    // The window is far above the clipping point of the 0 EV frame (peak ~ 0.12 * 30 = 3.6): recovered.
    let peak = img.data().as_chunks::<4>().0.iter().map(|p| p[1]).fold(0.0f32, f32::max);
    assert!(peak > 2.5, "highlights are not recovered: peak {peak}");
    let (mut sum, mut want, mut n) = (0.0f64, 0.0f64, 0u32);
    let (w, h) = (img.width() as usize, img.height() as usize);
    for y in 0..h {
        for x in 0..w {
            let theta = theta0 + (direct.origin[0] as f64 + x as f64 + 0.5) / F;
            let hh = (direct.origin[1] as f64 + y as f64 + 0.5) / F;
            let (sx, sy) = (F * theta, F * hh);
            if (sx - 150.0).abs() < 35.0 && (sy + 20.0).abs() < 40.0 {
                sum += img.data()[(y * w + x) * 4 + 1] as f64;
                want += truth(sx, sy)[1] as f64;
                n += 1;
            }
        }
    }
    assert!(n > 500, "the window must be in the picture ({n} px)");
    assert!((sum / want - 1.0).abs() < 0.05, "window mean {} vs truth {}", sum / n as f64, want / n as f64);
}

#[test]
fn the_chain_is_cached_by_its_inputs_and_a_changed_member_changes_the_panorama_identity() {
    let p = hdr_provider();
    let (mut g, hdr, pano, _out) = chain_graph();
    let ev = Evaluator::new();
    let fp = |g: &Graph, n: NodeId, i: u64| ev.fingerprint(g, n, ImageId(i), &p).unwrap();
    let before = fp(&g, pano, 5);
    set(&mut g, hdr, "deghost", 0.9);
    assert_ne!(fp(&g, pano, 5), before, "HDR settings reach the panorama");
    set(&mut g, hdr, "deghost", 0.5);
    assert_eq!(fp(&g, pano, 5), before);
    // A different ISO of a non-reference bracket frame changes how the bracket merges.
    p.caps.lock().unwrap().get_mut(&1).unwrap().iso = Some(200.0);
    assert_ne!(fp(&g, pano, 5), before);
}

// ---------------------------------------------------------------- preview timing

/// Preview timing: five frames of 2560 px (the default preview size). Run with `--ignored --nocapture`.
#[test]
#[ignore = "timing probe"]
fn timing_of_a_five_frame_panorama_at_preview_size() {
    let (w, h, f) = (2560u32, 1707u32, 2600.0);
    let mut p = Prov::default();
    for (k, yaw) in [-24.0, -12.0, 0.0, 12.0, 24.0].iter().enumerate() {
        p.add(k as u64 + 1, cyl_frame(w, h, f, *yaw, |x, y| scene(x, y, 23)), caps_at(k as i64 * 3_000, 0.01, None, "t.jpg"));
    }
    let (g, _src, _pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    let t = std::time::Instant::now();
    let img = ev.eval_image(&g, out, ImageId(3), &p, &CancelToken::new()).unwrap();
    println!("PANORAMA_5_FRAMES_2560_MS={} output {}x{} threads={}", t.elapsed().as_millis(), img.width(), img.height(), rayon::current_num_threads());
}

// ---------------------------------------------------------------- fix round 1

/// Frames of `w` x `h` px of the cylinder scene (focal length `F` px) with the matching 35-mm value.
fn shaped_provider(w: u32, h: u32, yaws: &[f64]) -> Prov {
    let f35 = (F * FULL_FRAME_DIAGONAL_MM / (w as f64).hypot(h as f64)) as f32;
    let mut p = Prov::default();
    for (k, yaw) in yaws.iter().enumerate() {
        p.add(k as u64 + 1, cyl_frame(w, h, F, *yaw, |x, y| scene(x, y, 23)), caps_at(k as i64 * 3_000, 0.01, Some(f35), &format!("shape_{k}.jpg")));
        p.full.insert(k as u64 + 1, (w, h));
    }
    p
}

fn direct_width(frames: &[Arc<ImageBuf>], focal: f32) -> u32 {
    let refs: Vec<&ImageBuf> = frames.iter().map(|f| f.as_ref()).collect();
    let params = fx::pano::PanoParams { focal_px: Some(focal), focal_fixed: true, ..Default::default() };
    fx::pano::stitch(&refs, &params, &|| false).unwrap().image.width()
}

fn close(a: u32, b: u32, tol: f64) -> bool {
    (a as f64 / b as f64 - 1.0).abs() <= tol
}

#[test]
fn the_recorded_focal_length_fits_16_9_and_square_frames() {
    for (w, h, yaws) in [(512u32, 288u32, [-26.0, -9.0, 8.0, 25.0]), (320, 320, [-22.0, -7.0, 8.0, 23.0])] {
        let p = shaped_provider(w, h, &yaws);
        let (g, _src, _pano, out) = pano_graph();
        let mut ev = Evaluator::new();
        let img = ev.eval_image(&g, out, ImageId(2), &p, &CancelToken::new()).unwrap();
        let frames: Vec<Arc<ImageBuf>> = (1..=4).map(|n| p.image(ImageId(n)).unwrap()).collect();
        let direct = direct_width(&frames, F as f32);
        assert!(close(img.width(), direct, 0.015), "{w}x{h}: node {} vs truth focal {direct}", img.width());
        assert!(ev.notes().is_empty(), "{:?}", ev.notes());
    }
}

#[test]
fn a_crop_before_the_panorama_does_not_shrink_the_focal_length() {
    let p = {
        let mut p = shaped_provider(W, H, &YAWS);
        for n in 1..=4 {
            p.full.insert(n, (W, H));
        }
        p
    };
    let mut g = Graph::new();
    let src = node_with(&mut g, NodeKind::Source);
    let t = node_with(&mut g, NodeKind::Transform);
    let pano = node_with(&mut g, NodeKind::Panorama);
    let out = node_with(&mut g, NodeKind::Output);
    g.connect(src, t, 0).unwrap();
    g.connect(t, pano, 0).unwrap();
    g.connect(pano, out, 0).unwrap();
    set(&mut g, t, "left", 0.1);
    set(&mut g, t, "right", 0.9);
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    let img = ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    let cropped: Vec<Arc<ImageBuf>> = (1..=4).map(|n| ev.eval_image(&g, t, ImageId(n), &p, &c).unwrap()).collect();
    assert!(cropped[0].width() < W, "the crop took effect");
    let direct = direct_width(&cropped, F as f32);
    assert!(close(img.width(), direct, 0.02), "node {} vs correct focal {direct}", img.width());
    // The focal length from the cropped size alone would be too short by the crop's diagonal ratio.
    let capt = p.capture(ImageId(1));
    let (wrong, _) = pano_focal(0, 0.0, std::slice::from_ref(&capt), (cropped[0].width(), cropped[0].height()));
    let (right, _) = pano_focal(0, 0.0, std::slice::from_ref(&capt), (W, H));
    assert!(wrong.unwrap() < right.unwrap() * 0.9);
    assert!((right.unwrap() as f64 / F - 1.0).abs() < 0.01);
}

#[test]
fn more_than_the_stitcher_takes_is_never_one_group_and_the_message_says_so() {
    let mut p = Prov::default();
    for k in 0..30u64 {
        p.add(k + 1, cyl_frame(64, 48, 90.0, 0.0, |x, y| scene(x, y, 1)), caps_at(k as i64 * 1_000, 0.01, None, &format!("many_{k}.jpg")));
    }
    let (mut g, _src, pano, out) = pano_graph();
    set(&mut g, pano, "max_size", 60.0);
    set(&mut g, pano, "gap_s", 60.0);
    let ev = Evaluator::new();
    let members = ev.members(&g, out, 0, &p);
    // 30 images in one sweep: cut at the node's limit of 25 (25 + 5), so two groups.
    assert_eq!(members.len(), 2, "{members:?}");
    let msg = pano_count_message(26);
    assert!(msg.contains("26 images") && msg.contains("25") && msg.contains("Max group size"), "{msg}");
}

#[test]
fn the_memory_budget_follows_the_machine_and_the_message_gives_numbers() {
    // 24 MP x 12 frames: 4.3 GB of inputs plus the output.
    assert!(pano_over(12, 6000, 4000, 8 << 30));
    assert!(!pano_over(12, 6000, 4000, 32u64 << 30));
    assert!(!pano_over(usize::MAX, 0, 0, 1 << 30), "no overflow, no pixels");
    let msg = pano_budget_message_for(12, 6000, 4000, 8 << 30);
    assert!(msg.starts_with("Panorama: 12 frames of 6000 x 4000 px need about"), "{msg}");
    assert!(msg.contains("fewer frames") && msg.contains("smaller long edge") && !msg.contains("preview"), "{msg}");
    let total = total_memory_bytes();
    assert!(total >= 1 << 30);
    assert_eq!(pano_memory_budget(), total / 10 * 7);
}

#[test]
fn notes_stay_bounded_and_are_taken_per_evaluation() {
    let p = ldr_provider(None);
    let (g, _src, _pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    ev.eval_image(&g, out, ImageId(2), &p, &CancelToken::new()).unwrap();
    assert_eq!(ev.take_notes().len(), 1);
    assert!(ev.take_notes().is_empty());
    for i in 0..200 {
        ev.push_note(NodeId(1), ImageId(i), "x".into());
    }
    assert!(ev.notes().len() <= 64);
    assert_eq!(ev.notes().last().unwrap().1, ImageId(199), "the newest notes stay");
}

/// Provider that counts decodes: the Layer scale of a panorama needs none when the proxy size is known.
struct SizedProv {
    inner: Prov,
    decodes: std::sync::atomic::AtomicUsize,
}

impl ImageProvider for SizedProv {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        self.decodes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.image(id)
    }
    fn capture(&self, id: ImageId) -> Option<CaptureInfo> {
        self.inner.capture(id)
    }
    fn images(&self) -> Vec<ImageId> {
        self.inner.images()
    }
    fn full_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.inner.full_size(id)
    }
    fn proxy_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.inner.frames.get(&id.0).map(|f| (f.width(), f.height()))
    }
}

#[test]
fn the_layer_scale_behind_a_panorama_needs_no_decode_and_follows_any_input() {
    let mut inner = ldr_provider(Some(f35()));
    for n in 1..=4 {
        inner.full.insert(n, (W * 2, H * 2));
    }
    let p = SizedProv { inner, decodes: Default::default() };
    // Panorama -> Merge (second input) -> Layer: the Panorama is not on the first input of the Merge.
    let mut g = Graph::new();
    let s1 = node_with(&mut g, NodeKind::Source);
    let s2 = node_with(&mut g, NodeKind::Source);
    let pano = node_with(&mut g, NodeKind::Panorama);
    let merge = node_with(&mut g, NodeKind::Merge);
    let layer = node_with(&mut g, NodeKind::Layer);
    g.connect(s2, pano, 0).unwrap();
    g.connect(s1, merge, 0).unwrap();
    g.connect(pano, merge, 1).unwrap();
    g.connect(merge, layer, 0).unwrap();
    let full = layer_full_size(&g, layer, 0, &p, ImageId(2), (1000, 300)).unwrap();
    assert_eq!(full, (2000, 600));
    assert_eq!(p.decodes.load(std::sync::atomic::Ordering::Relaxed), 0, "the scale comes from the sizes");
}

#[test]
fn the_findings_of_a_cached_panorama_stay_available_and_follow_the_graph() {
    let p = ldr_provider(None);
    let (mut g, _src, pano, out) = pano_graph();
    let mut ev = Evaluator::new();
    let c = CancelToken::new();
    ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    assert_eq!(ev.take_notes().len(), 1);
    // The second request is a cache hit: no new note, but the viewer still has the warning to show.
    ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    assert!(ev.take_notes().is_empty());
    let now = ev.current_notes(&g, out, ImageId(2));
    assert_eq!(now.len(), 1, "{now:?}");
    assert!(now[0].contains("no focal length is recorded"));
    // Another image of the sweep has none of its own.
    assert!(ev.current_notes(&g, out, ImageId(4)).is_empty());
    // A typed focal length recomputes the panorama; the old finding goes.
    set(&mut g, pano, "focal_mm_35", f35());
    ev.eval_image(&g, out, ImageId(2), &p, &c).unwrap();
    assert!(ev.current_notes(&g, out, ImageId(2)).is_empty());
    ev.clear();
    assert!(ev.current_notes(&g, out, ImageId(2)).is_empty());
}
