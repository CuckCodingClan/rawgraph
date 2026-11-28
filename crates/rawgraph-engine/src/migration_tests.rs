//! Rendering of migrated graphs: a format-2 graph with masks is loaded, rendered, and compared with
//! what the removed mask code produced. The expected values are computed here with the old formulas
//! (copied from the removed `rawgraph-fx` masks and the old engine) on the frame of the consumer.

use std::sync::Arc;

use rawgraph_core::{ops, Graph, ImageBuf, ImageId, MaskBuf, NodeId};
use rawgraph_fx as fx;
use serde_json::{json, Value};

use super::*;

const A: ImageId = ImageId(1);
const W: u32 = 240;
const H: u32 = 160;

/// Smooth, coloured, strictly positive test picture.
struct Picture;

impl ImageProvider for Picture {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        let mut img = ImageBuf::new(W, H);
        for y in 0..H {
            for x in 0..W {
                let (u, v) = (x as f32 / W as f32, y as f32 / H as f32);
                let l = 0.02 + 0.9 * (0.6 * u + 0.4 * v).powf(1.5) + id.0 as f32 * 0.01;
                img.set_pixel(x, y, [l, l * (0.6 + 0.3 * u) + 0.01, l * (0.5 + 0.4 * v) + 0.02, 1.0]);
            }
        }
        Ok(Arc::new(img))
    }

    fn images(&self) -> Vec<ImageId> {
        vec![ImageId(1), ImageId(2)]
    }
}

fn node(id: u64, kind: &str, all: Value) -> Value {
    json!({"id": id, "kind": kind, "mode": "All", "all": all, "individual": {}, "pos": [id as f64 * 100.0, 0.0]})
}

/// (from, from_port, to, to_port)
fn doc(nodes: Vec<Value>, edges: &[(u64, u64, u64, u64)]) -> String {
    let next = nodes.iter().map(|n| n["id"].as_u64().unwrap()).max().unwrap_or(0) + 1;
    let nodes: serde_json::Map<String, Value> = nodes.into_iter().map(|n| (n["id"].to_string(), n)).collect();
    let edges: Vec<Value> = edges
        .iter()
        .map(|&(f, fp, t, tp)| {
            let mut e = json!({"from": f, "to": t, "to_port": tp});
            if fp != 0 {
                e["from_port"] = json!(fp);
            }
            e
        })
        .collect();
    json!({"format_version": 2, "nodes": nodes, "edges": edges, "next_id": next}).to_string()
}

/// Source (1) -> consumer (2) -> Output (9), plus `masks` and the extra edges.
fn consumer_doc(consumer: &str, params: Value, masks: Vec<Value>, edges: &[(u64, u64, u64, u64)]) -> String {
    let mut nodes = vec![node(1, "Source", json!({})), node(2, consumer, params), node(9, "Output", json!({}))];
    nodes.extend(masks);
    let mut all = vec![(1, 0, 2, 0), (2, 0, 9, 0)];
    all.extend_from_slice(edges);
    doc(nodes, &all)
}

fn eval(g: &Graph, id: u64) -> Arc<ImageBuf> {
    Evaluator::new().eval_image(g, NodeId(id), A, &Picture, &CancelToken::new()).unwrap()
}

fn load(text: &str) -> Graph {
    let (g, notes) = Graph::from_json_with_notes(text).unwrap();
    assert!(g.validate().is_empty(), "{:?} {notes:?}", g.validate());
    g
}

/// Per-pixel Exposure strength `m` of `out` against `src` for ev = 1 (gain 2^m), red channel.
fn strength(out: &ImageBuf, src: &ImageBuf) -> Vec<f32> {
    out.data().as_chunks::<4>().0.iter().zip(src.data().as_chunks::<4>().0).map(|(o, s)| (o[0] / s[0]).log2()).collect()
}

fn stats(a: &[f32], b: &[f32]) -> (f32, f32) {
    assert_eq!(a.len(), b.len());
    let (mut sum, mut max) = (0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        let d = (x - y).abs();
        sum += d;
        max = max.max(d);
    }
    (sum / a.len() as f32, max)
}

fn expect_close(got: &[f32], want: &[f32], mean: f32, max: f32, what: &str) {
    let (m, x) = stats(got, want);
    assert!(m <= mean && x <= max, "{what}: mean error {m}, max error {x} (limits {mean}, {max})");
}

fn plane(w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> Vec<f32> {
    (0..h).flat_map(|y| (0..w).map(move |x| (x, y))).map(|(x, y)| f(x, y)).collect()
}

// ---- the removed mask formulas -------------------------------------------------------------

/// `ops::linear_gradient_mask` of the old code at pixel (x, y).
fn old_gradient(w: u32, h: u32, angle: f32, center: f32, width: f32, x: u32, y: u32) -> f32 {
    let (sin, cos) = angle.to_radians().sin_cos();
    let v = (y as f32 + 0.5) / h as f32 - 0.5;
    let u = (x as f32 + 0.5) / w as f32 - 0.5;
    let along = u * cos + v * sin + 0.5;
    if width <= f32::EPSILON {
        return if along >= center { 1.0 } else { 0.0 };
    }
    ((along - center) / width + 0.5).clamp(0.0, 1.0)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x >= e0 { 1.0 } else { 0.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `fx::radial_mask` of the old code at pixel (x, y).
#[allow(clippy::too_many_arguments)]
fn old_radial(w: u32, h: u32, cx: f32, cy: f32, rx: f32, ry: f32, feather: f32, angle: f32, invert: bool, x: u32, y: u32) -> f32 {
    let (wf, hf) = (w as f32, h as f32);
    let (ax, ay) = (rx * wf, ry * hf);
    let (sin, cos) = angle.to_radians().sin_cos();
    let dx = x as f32 + 0.5 - cx * wf;
    let dy = y as f32 + 0.5 - cy * hf;
    let (u, v) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    let d = (u / ax).hypot(v / ay);
    let m = if feather <= 0.0 {
        f32::from(d <= 1.0)
    } else {
        1.0 - smoothstep(1.0 - feather, 1.0, d)
    };
    if invert {
        1.0 - m
    } else {
        m
    }
}

fn old_luma(p: &[f32]) -> f32 {
    0.272_228_7 * p[0] + 0.674_081_8 * p[1] + 0.053_689_5 * p[2]
}

fn old_luminance(src: &ImageBuf, low: f32, high: f32, feather: f32, invert: bool) -> Vec<f32> {
    src.data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|px| {
            let y = old_luma(px).max(0.0);
            let t = y / (y + 0.18);
            let half = feather * 0.5;
            let m = if half <= 0.0 {
                f32::from(t >= low && t < high)
            } else {
                smoothstep(low - half, low + half, t) * (1.0 - smoothstep(high - half, high + half, t))
            };
            if invert {
                1.0 - m
            } else {
                m
            }
        })
        .collect()
}

fn old_adjust(m: f32, low: f32, high: f32, gamma: f32, invert: bool) -> f32 {
    let m = m.clamp(0.0, 1.0);
    let n = if high <= low {
        f32::from(m >= low)
    } else {
        let t = ((m - low) / (high - low)).clamp(0.0, 1.0);
        t.powf(gamma)
    };
    if invert {
        1.0 - n
    } else {
        n
    }
}

const GRADIENT: (f32, f32, f32) = (30.0, 0.4, 0.6);

fn gradient_node(id: u64) -> Value {
    node(id, "GradientMask", json!({"angle": GRADIENT.0, "center": GRADIENT.1, "width": GRADIENT.2}))
}

fn gradient_plane(w: u32, h: u32) -> Vec<f32> {
    plane(w, h, |x, y| old_gradient(w, h, GRADIENT.0, GRADIENT.1, GRADIENT.2, x, y))
}

fn exposure_doc(masks: Vec<Value>, edges: &[(u64, u64, u64, u64)]) -> Graph {
    load(&consumer_doc("Exposure", json!({"ev": 1.0}), masks, edges))
}

// ---- tests ----------------------------------------------------------------------------------

#[test]
fn gradient_mask_renders_like_the_old_mask() {
    let g = exposure_doc(vec![gradient_node(3)], &[(3, 0, 2, 1)]);
    let (out, src) = (eval(&g, 2), eval(&g, 1));
    assert_ne!(*out, *eval(&exposure_doc(vec![], &[]), 2), "the mask has an effect");
    expect_close(&strength(&out, &src), &gradient_plane(W, H), 0.003, 0.012, "gradient");
}

#[test]
fn gradient_angles_and_widths_match() {
    for (angle, center, width) in [(0.0, 0.5, 1.0), (90.0, 0.3, 0.4), (200.0, 0.6, 0.8), (315.0, 0.5, 0.25), (180.0, 0.1, 0.9)] {
        let mask = node(3, "GradientMask", json!({"angle": angle, "center": center, "width": width}));
        let g = exposure_doc(vec![mask], &[(3, 0, 2, 1)]);
        let want = plane(W, H, |x, y| old_gradient(W, H, angle, center, width, x, y));
        expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &want, 0.004, 0.02, &format!("angle {angle} center {center} width {width}"));
    }
}

#[test]
fn radial_mask_renders_like_the_old_mask() {
    for (feather, invert) in [(0.5, false), (0.2, false), (1.0, false), (0.4, true), (0.0, false)] {
        let all = json!({"center_x": 0.4, "center_y": 0.55, "radius_x": 0.3, "radius_y": 0.3, "feather": feather, "invert": f32::from(invert)});
        let g = exposure_doc(vec![node(3, "RadialMask", all)], &[(3, 0, 2, 1)]);
        let want = plane(W, H, |x, y| old_radial(W, H, 0.4, 0.55, 0.3, 0.3, feather, 0.0, invert, x, y));
        // The hard edge is a 1 / 1000 wide ramp in the ramp, the rest follows the old formula.
        let (mean, max) = if feather == 0.0 { (0.02, 1.0) } else { (0.005, 0.1) };
        expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &want, mean, max, &format!("feather {feather} invert {invert}"));
    }
}

#[test]
fn unequal_radii_are_only_approximated() {
    let all = json!({"radius_x": 0.4, "radius_y": 0.1, "feather": 0.5});
    let g = exposure_doc(vec![node(3, "RadialMask", all)], &[(3, 0, 2, 1)]);
    let got = strength(&eval(&g, 2), &eval(&g, 1));
    let want = plane(W, H, |x, y| old_radial(W, H, 0.5, 0.5, 0.4, 0.1, 0.5, 0.0, false, x, y));
    let (mean, _) = stats(&got, &want);
    assert!(mean > 0.02, "the shapes differ ({mean}), which the note says");
}

fn stroke(points: &[[f32; 2]], radius: f32, hardness: f32, erase: bool) -> Value {
    json!({"points": points, "radius": radius, "hardness": hardness, "flow": 1.0, "erase": erase})
}

fn brush_node(id: u64, invert: bool) -> Value {
    let strokes = json!({"Strokes": [stroke(&[[0.2, 0.3], [0.7, 0.6]], 0.12, 0.4, false), stroke(&[[0.5, 0.45]], 0.05, 1.0, true)]});
    let mut n = node(id, "BrushMask", json!({"invert": f32::from(invert)}));
    n["mode"] = json!("Individual");
    n["individual_data"] = json!({"1": strokes});
    n
}

fn old_brush(w: u32, h: u32, invert: bool) -> Vec<f32> {
    let strokes = vec![
        BrushStroke { points: vec![[0.2, 0.3], [0.7, 0.6]], radius: 0.12, hardness: 0.4, flow: 1.0, erase: false },
        BrushStroke { points: vec![[0.5, 0.45]], radius: 0.05, hardness: 1.0, flow: 1.0, erase: true },
    ];
    fx::stroke_mask(w, h, &strokes, invert).data
}

use rawgraph_core::BrushStroke;

#[test]
fn brush_mask_renders_exactly_like_the_old_mask() {
    let g = exposure_doc(vec![brush_node(3, false)], &[(3, 0, 2, 1)]);
    expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &old_brush(W, H, false), 1e-4, 1e-3, "brush");
}

#[test]
fn inverted_brush_mask_renders_like_the_old_mask() {
    let g = exposure_doc(vec![brush_node(3, true)], &[(3, 0, 2, 1)]);
    expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &old_brush(W, H, true), 5e-3, 0.03, "inverted brush");
}

#[test]
fn brush_strokes_belong_to_their_photo_and_follow_the_consumers_frame_after_a_crop() {
    // Source -> Crop (left 0.25) -> Exposure; the brush is evaluated for the cropped frame.
    let nodes = vec![
        node(1, "Source", json!({})),
        node(4, "Transform", json!({"left": 0.25, "top": 0.5})),
        node(2, "Exposure", json!({"ev": 1.0})),
        node(9, "Output", json!({})),
        brush_node(3, false),
    ];
    let g = load(&doc(nodes, &[(1, 0, 4, 0), (4, 0, 2, 0), (2, 0, 9, 0), (3, 0, 2, 1)]));
    let (out, cropped) = (eval(&g, 2), eval(&g, 4));
    let (w, h) = (cropped.width(), cropped.height());
    assert!(w < W && h < H);
    expect_close(&strength(&out, &cropped), &old_brush(w, h, false), 1e-4, 1e-3, "cropped brush");
    // Another photo has no strokes of its own: nothing is painted.
    let other = Evaluator::new().eval_image(&g, NodeId(2), ImageId(2), &Picture, &CancelToken::new()).unwrap();
    let src2 = Evaluator::new().eval_image(&g, NodeId(4), ImageId(2), &Picture, &CancelToken::new()).unwrap();
    assert!(strength(&other, &src2).iter().all(|m| m.abs() < 1e-5));
}

#[test]
fn luminance_mask_renders_like_the_old_mask() {
    for (low, high, feather, invert) in [(0.3, 0.8, 0.2, false), (0.0, 1.0, 0.1, false), (0.2, 0.6, 0.0, false), (0.4, 0.7, 0.3, true)] {
        let mask = node(3, "LuminanceMask", json!({"low": low, "high": high, "feather": feather, "invert": f32::from(invert)}));
        let g = load(&doc(
            vec![node(1, "Source", json!({})), node(2, "Exposure", json!({"ev": 1.0})), node(9, "Output", json!({})), mask],
            &[(1, 0, 2, 0), (2, 0, 9, 0), (1, 0, 3, 0), (3, 0, 2, 1)],
        ));
        let src = eval(&g, 1);
        let want = old_luminance(&src, low, high, feather, invert);
        let (mean, max) = if feather == 0.0 { (0.02, 1.0) } else { (0.01, 0.08) };
        expect_close(&strength(&eval(&g, 2), &src), &want, mean, max, &format!("luminance {low} {high} {feather} {invert}"));
    }
}

#[test]
fn mask_adjust_renders_like_the_old_mask() {
    for (low, high, gamma, invert) in [(0.2, 0.9, 1.0, false), (0.0, 1.0, 2.0, false), (0.0, 1.0, 1.0, true), (0.1, 0.8, 0.5, true), (0.5, 0.5, 1.0, false)] {
        let adjust = node(4, "MaskAdjust", json!({"low": low, "high": high, "gamma": gamma, "invert": f32::from(invert)}));
        let g = exposure_doc(vec![gradient_node(3), adjust], &[(3, 0, 4, 0), (4, 0, 2, 1)]);
        let want: Vec<f32> = gradient_plane(W, H).into_iter().map(|m| old_adjust(m, low, high, gamma, invert)).collect();
        let (mean, max) = if low == high { (0.03, 1.0) } else { (0.012, 0.12) };
        expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &want, mean, max, &format!("adjust {low} {high} {gamma} {invert}"));
    }
}

#[test]
fn inverting_a_mask_works_where_the_mask_is_exactly_zero() {
    // The brush is 0 outside its strokes; an inverting adjust must make that 1.
    let adjust = node(4, "MaskAdjust", json!({"invert": 1.0}));
    let g = exposure_doc(vec![brush_node(3, false), adjust], &[(3, 0, 4, 0), (4, 0, 2, 1)]);
    let got = strength(&eval(&g, 2), &eval(&g, 1));
    let want: Vec<f32> = old_brush(W, H, false).into_iter().map(|m| 1.0 - m).collect();
    expect_close(&got, &want, 5e-3, 0.03, "inverted by adjust");
    assert!(got[0] > 0.99, "a pixel without paint is fully exposed");
}

#[test]
fn mask_combine_renders_like_the_old_mask() {
    let radial = node(4, "RadialMask", json!({"center_x": 0.6, "center_y": 0.5, "radius_x": 0.35, "radius_y": 0.35, "feather": 0.6}));
    for mode in 0..5 {
        let combine = node(5, "MaskCombine", json!({"mode": mode as f32}));
        let g = exposure_doc(vec![gradient_node(3), radial.clone(), combine], &[(3, 0, 5, 0), (4, 0, 5, 1), (5, 0, 2, 1)]);
        let want = plane(W, H, |x, y| {
            let a = old_gradient(W, H, GRADIENT.0, GRADIENT.1, GRADIENT.2, x, y);
            let b = old_radial(W, H, 0.6, 0.5, 0.35, 0.35, 0.6, 0.0, false, x, y);
            match mode {
                0 => a * b,
                1 => (a + b).clamp(0.0, 1.0),
                2 => (a - b).clamp(0.0, 1.0),
                3 => a.max(b),
                _ => a.min(b),
            }
        });
        expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &want, 0.006, 0.05, &format!("combine mode {mode}"));
    }
}

#[test]
fn a_chain_of_masks_matches_the_old_chain() {
    // gradient, radial -> combine (maximum) -> adjust (gamma 2) -> Exposure
    let radial = node(4, "RadialMask", json!({"radius_x": 0.25, "radius_y": 0.25}));
    let combine = node(5, "MaskCombine", json!({"mode": 3.0}));
    let adjust = node(6, "MaskAdjust", json!({"gamma": 2.0}));
    let g = exposure_doc(vec![gradient_node(3), radial, combine, adjust], &[(3, 0, 5, 0), (4, 0, 5, 1), (5, 0, 6, 0), (6, 0, 2, 1)]);
    let want = plane(W, H, |x, y| {
        let a = old_gradient(W, H, GRADIENT.0, GRADIENT.1, GRADIENT.2, x, y);
        let b = old_radial(W, H, 0.5, 0.5, 0.25, 0.25, 0.5, 0.0, false, x, y);
        a.max(b).powf(2.0)
    });
    expect_close(&strength(&eval(&g, 2), &eval(&g, 1)), &want, 0.006, 0.06, "chain");
}

#[test]
fn a_removed_hue_mask_leaves_the_slider_in_control() {
    let hue = node(3, "HueMask", json!({"hue": 30.0}));
    let g = load(&doc(
        vec![node(1, "Source", json!({})), node(2, "Exposure", json!({"ev": 1.0})), node(9, "Output", json!({})), hue],
        &[(1, 0, 2, 0), (2, 0, 9, 0), (1, 0, 3, 0), (3, 0, 2, 1)],
    ));
    let got = strength(&eval(&g, 2), &eval(&g, 1));
    assert!(got.iter().all(|m| (m - 1.0).abs() < 1e-4), "the full slider value everywhere");
}

#[test]
fn every_consumer_port_applies_the_map_like_the_old_port() {
    let m = |x, y| old_gradient(W, H, GRADIENT.0, GRADIENT.1, GRADIENT.2, x, y);
    let mask = MaskBuf { width: W, height: H, data: plane(W, H, m) };
    let src = Picture.image(A).unwrap();

    // Contrast: amount * m through the old power curve around the pivot.
    let g = load(&consumer_doc("Contrast", json!({"amount": 0.8}), vec![gradient_node(3)], &[(3, 0, 2, 1)]));
    let mut want = (*src).clone();
    let pivot = g.node(NodeId(2)).unwrap().param("pivot", A);
    for (px, &m) in want.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(&mask.data) {
        let a = 0.8 * m;
        let y = 0.272_228_7 * px[0] + 0.674_081_8 * px[1] + 0.053_689_5 * px[2];
        if a != 0.0 && y > 0.0 {
            let gain = pivot * (y / pivot).powf(a.exp2()) / y;
            px[..3].iter_mut().for_each(|c| *c *= gain);
        }
    }
    assert!(max_diff(&eval(&g, 2), &want) < 0.01, "contrast");

    // Saturation: 1 + (amount - 1) * m.
    let g = load(&consumer_doc("Saturation", json!({"amount": 1.8}), vec![gradient_node(3)], &[(3, 0, 2, 1)]));
    let mut want = (*src).clone();
    for (px, &m) in want.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(&mask.data) {
        let s = 1.0 + 0.8 * m;
        let y = 0.272_228_7 * px[0] + 0.674_081_8 * px[1] + 0.053_689_5 * px[2];
        px[..3].iter_mut().for_each(|c| *c = y + (*c - y) * s);
    }
    assert!(max_diff(&eval(&g, 2), &want) < 0.01, "saturation");

    // Sharpen: the old call with the mask.
    let g = load(&consumer_doc("Sharpen", json!({"amount": 2.0, "radius": 1.0}), vec![gradient_node(3)], &[(3, 0, 2, 1)]));
    let want = fx::sharpen(&src, 2.0, 1.0 / 1000.0 * W.max(H) as f32, 0.0, Some(&mask));
    assert!(max_diff(&eval(&g, 2), &want) < 0.02, "sharpen");

    // Filter radius: the old modulated blur with the slider as the maximum.
    let g = load(&consumer_doc("Blur", json!({"radius": 30.0}), vec![gradient_node(3)], &[(3, 0, 2, 1)]));
    let want = ops::blur_modulated(&src, 30.0 / 1000.0 * W.max(H) as f32, &mask);
    assert!(max_diff(&eval(&g, 2), &want) < 0.02, "filter radius");
    assert_ne!(*eval(&g, 2), *eval(&load(&consumer_doc("Blur", json!({"radius": 30.0}), vec![], &[])), 2));
}

fn max_diff(a: &ImageBuf, b: &ImageBuf) -> f32 {
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    a.data().iter().zip(b.data()).map(|(x, y)| (x - y).abs()).fold(0.0, f32::max)
}

#[test]
fn an_old_exposed_filter_radius_keeps_its_full_range() {
    // Old: the exposed radius map gave sigma = max radius * m.
    let mut blur = node(2, "Blur", json!({"radius": 7.0}));
    blur["exposed_inputs"] = json!([{"parameter": "radius", "ty": "Mask"}]);
    let nodes = vec![node(1, "Source", json!({})), blur, gradient_node(3), node(9, "Output", json!({}))];
    // Old ports: image 0, mask 1, direction 2, first exposed input 3.
    let g = load(&doc(nodes, &[(1, 0, 2, 0), (3, 0, 2, 3), (2, 0, 9, 0)]));
    let src = Picture.image(A).unwrap();
    let mask = MaskBuf { width: W, height: H, data: gradient_plane(W, H) };
    let want = ops::blur_modulated(&src, 1000.0 / 1000.0 * W.max(H) as f32, &mask);
    assert!(max_diff(&eval(&g, 2), &want) < 0.03);
}

#[test]
fn the_version_1_fixture_renders_through_its_brush_mask() {
    let mut g = load(include_str!("../../rawgraph-core/tests/fixtures/graph_v1.json"));
    let mut plain = {
        let mut doc: Value = serde_json::from_str(include_str!("../../rawgraph-core/tests/fixtures/graph_v1.json")).unwrap();
        doc["edges"].as_array_mut().unwrap().retain(|e| !(e["from"] == 8));
        load(&doc.to_string())
    };
    for graph in [&mut g, &mut plain] {
        graph.node_mut(NodeId(9)).unwrap().set_param("radius", A, 60.0);
    }
    struct Iso;
    impl ImageProvider for Iso {
        fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
            Picture.image(id)
        }
        fn capture(&self, id: ImageId) -> Option<CaptureInfo> {
            (id.0 > 0).then(|| CaptureInfo { iso: Some(100.0), ..Default::default() })
        }
    }
    let run = |g: &Graph| Evaluator::new().eval_image(g, NodeId(9), A, &Iso, &CancelToken::new()).unwrap();
    assert!(max_diff(&run(&g), &run(&plain)) > 1e-4, "the brush strokes change the filter result");
}
