//! Tests for `stroke_mask` and `point_curve` / `curve_eval`.

use rawgraph_core::{BrushStroke, ImageBuf};

use crate::*;

fn stroke(points: &[[f32; 2]], radius: f32, hardness: f32, flow: f32, erase: bool) -> BrushStroke {
    BrushStroke { points: points.to_vec(), radius, hardness, flow, erase }
}

fn dab(x: f32, y: f32, radius: f32, hardness: f32) -> BrushStroke {
    stroke(&[[x, y]], radius, hardness, 1.0, false)
}

/// `n` wavy strokes of `pts` points each (about 500 px long on a 2560 px image), radius 2 %.
pub(crate) fn wavy_strokes(n: usize, pts: usize) -> Vec<BrushStroke> {
    (0..n)
        .map(|s| {
            let (ox, oy) = (0.05 + 0.6 * ((s * 7) % 13) as f32 / 13.0, 0.1 + 0.8 * ((s * 5) % 11) as f32 / 11.0);
            let points = (0..pts)
                .map(|i| {
                    let u = i as f32 / (pts - 1) as f32;
                    [ox + 0.2 * u, oy + 0.05 * (u * 9.0 + s as f32).sin()]
                })
                .collect();
            BrushStroke { points, radius: 0.02, hardness: 0.4 + 0.02 * (s % 20) as f32, flow: 0.8, erase: s % 7 == 6 }
        })
        .collect()
}

fn grey(w: u32, h: u32, v: f32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.set_pixel(x, y, [v, v, v, 0.5]);
        }
    }
    img
}

// ---------------------------------------------------------------- stroke_mask

#[test]
fn stroke_mask_empty_invert_and_degenerate_sizes() {
    let m = stroke_mask(8, 6, &[], false);
    assert_eq!((m.width, m.height), (8, 6));
    assert!(m.data.iter().all(|&v| v == 0.0));
    assert!(stroke_mask(8, 6, &[], true).data.iter().all(|&v| v == 1.0));
    assert!(stroke_mask(0, 5, &[dab(0.5, 0.5, 0.1, 1.0)], false).data.is_empty());
    // A stroke without points or with zero flow changes nothing.
    assert!(stroke_mask(8, 6, &[stroke(&[], 0.1, 1.0, 1.0, false), stroke(&[[0.5, 0.5]], 0.1, 1.0, 0.0, false)], false)
        .data
        .iter()
        .all(|&v| v == 0.0));
}

#[test]
fn stroke_mask_single_dab_profile() {
    let m = stroke_mask(400, 400, &[dab(0.5, 0.5, 0.1, 0.5)], false); // R = 40, inner = 20
    assert!(m.get(200, 200) > 0.999);
    assert!(m.get(200 + 15, 200) > 0.999); // inside the hard core
    // Mid radius (inner + R) / 2 = 30: pixel centre (229.5, 199.5) is 29.5 away -> about 0.5.
    let mid = m.get(229, 199);
    assert!((mid - 0.5).abs() < 0.1, "{mid}");
    assert_eq!(m.get(240, 200), 0.0); // 40.5 away
    assert_eq!(m.get(0, 0), 0.0);
    // Monotone falloff along a row.
    for x in 201..400 {
        assert!(m.get(x, 200) <= m.get(x - 1, 200) + 1e-6);
    }
    // Round: the same value in the four directions.
    assert!((m.get(200 + 25, 200) - m.get(200 - 26, 200)).abs() < 1e-5);
    assert!((m.get(200, 200 + 25) - m.get(200, 200 - 26)).abs() < 1e-5);
}

#[test]
fn stroke_mask_hard_and_soft_edges() {
    let hard = stroke_mask(400, 400, &[dab(0.5, 0.5, 0.1, 1.0)], false);
    let soft = stroke_mask(400, 400, &[dab(0.5, 0.5, 0.1, 0.0)], false);
    // Hard: full up to ~1 px before the edge, then gone within a pixel or two.
    assert!(hard.get(200 + 37, 200) > 0.999);
    assert_eq!(hard.get(200 + 40, 200), 0.0);
    let ramp: Vec<f32> = (36..42).map(|d| hard.get(200 + d, 200)).collect();
    assert!(ramp.iter().filter(|&&v| v > 0.01 && v < 0.99).count() <= 2, "{ramp:?}");
    // Soft: falls from the centre, about 0.5 at R / 2, still partial 3/4 of the way out.
    assert!(soft.get(200, 200) > 0.999);
    assert!((soft.get(200 + 20, 200) - 0.5).abs() < 0.05);
    assert!(soft.get(200 + 30, 200) > 0.0 && soft.get(200 + 30, 200) < 0.2);
    assert!(soft.get(200 + 10, 200) > hard.get(200 + 10, 200) - 0.5);
    assert!(soft.get(200 + 10, 200) < 1.0);
}

#[test]
fn stroke_mask_coverage_is_uniform_along_a_straight_stroke() {
    let a = [0.2f32, 0.5];
    let b = [0.8f32, 0.5];
    let sparse = stroke(&[a, b], 0.04, 0.2, 1.0, false);
    let dense_pts: Vec<[f32; 2]> = (0..=300).map(|i| [a[0] + (b[0] - a[0]) * i as f32 / 300.0, 0.5]).collect();
    let dense = stroke(&dense_pts, 0.04, 0.2, 1.0, false);
    let ms = stroke_mask(500, 300, &[sparse], false);
    let md = stroke_mask(500, 300, &[dense], false);
    // Rows off the centre line have partial coverage; it must be constant along the stroke.
    for y in [150 + 3, 150 + 8, 150 + 12, 150 - 5] {
        let reference = ms.get(250, y);
        for x in 110..390 {
            assert!((ms.get(x, y) - reference).abs() < 1e-3, "sparse row {y} x {x}");
            assert!((md.get(x, y) - reference).abs() < 1e-3, "dense row {y} x {x}");
        }
    }
    let max_diff = ms.data.iter().zip(&md.data).map(|(p, q)| (p - q).abs()).fold(0.0f32, f32::max);
    assert!(max_diff < 1e-3, "{max_diff}");
    // Centre line is all (nearly) one.
    assert!((110..390).all(|x| ms.get(x, 150) > 0.999));
}

#[test]
fn stroke_mask_flow_accumulation_and_erase() {
    let two = stroke_mask(100, 100, &[stroke(&[[0.5, 0.5]], 0.2, 1.0, 0.5, false), stroke(&[[0.5, 0.5]], 0.2, 1.0, 0.5, false)], false);
    assert!((two.get(50, 50) - 0.75).abs() < 1e-5); // 0.5 + 0.5 * 0.5
    let one = stroke_mask(100, 100, &[stroke(&[[0.5, 0.5]], 0.2, 1.0, 0.3, false)], false);
    assert!((one.get(50, 50) - 0.3).abs() < 1e-5);
    // Paint at flow 0.8 then erase at flow 0.4 on the same dab: 0.8 * (1 - 0.4).
    let pe = stroke_mask(100, 100, &[stroke(&[[0.5, 0.5]], 0.2, 1.0, 0.8, false), stroke(&[[0.5, 0.5]], 0.2, 1.0, 0.4, true)], false);
    assert!((pe.get(50, 50) - 0.48).abs() < 1e-5);
    // Full erase clears; erase on empty does nothing; paint after erase comes back.
    let cleared = stroke_mask(100, 100, &[dab(0.5, 0.5, 0.2, 1.0), stroke(&[[0.5, 0.5]], 0.3, 1.0, 1.0, true)], false);
    assert!(cleared.data.iter().all(|&v| v == 0.0));
    let nothing = stroke_mask(100, 100, &[stroke(&[[0.5, 0.5]], 0.3, 1.0, 1.0, true)], false);
    assert!(nothing.data.iter().all(|&v| v == 0.0));
    // A soft erase through a hard stroke leaves a graded cut.
    let cut = stroke_mask(200, 100, &[stroke(&[[0.1, 0.5], [0.9, 0.5]], 0.1, 1.0, 1.0, false), stroke(&[[0.5, 0.0], [0.5, 1.0]], 0.05, 0.0, 1.0, true)], false);
    assert!(cut.get(100, 50) < 0.02);
    assert!(cut.get(100 - 6, 50) > 0.0 && cut.get(100 - 6, 50) < 1.0);
    assert!(cut.get(40, 50) > 0.999);
}

#[test]
fn stroke_mask_invert_and_range() {
    let s = [stroke(&[[0.2, 0.2], [0.8, 0.7]], 0.1, 0.3, 0.9, false), stroke(&[[0.3, 0.6]], 0.15, 0.0, 1.0, true)];
    let m = stroke_mask(120, 90, &s, false);
    let i = stroke_mask(120, 90, &s, true);
    assert!(m.data.iter().all(|&v| (0.0..=1.0).contains(&v)));
    for (a, b) in m.data.iter().zip(&i.data) {
        assert!((a + b - 1.0).abs() < 1e-6);
    }
}

#[test]
fn stroke_mask_outside_degenerate_and_extreme_input() {
    // Entirely outside.
    let m = stroke_mask(100, 80, &[dab(-1.0, -1.0, 0.1, 1.0), dab(3.0, 0.5, 0.2, 1.0), dab(0.5, 1e30, 0.1, 1.0)], false);
    assert!(m.data.iter().all(|&v| v == 0.0));
    // Partly outside: the visible part is still painted.
    let m = stroke_mask(100, 100, &[dab(-0.02, 0.5, 0.1, 1.0)], false);
    assert!(m.get(0, 50) > 0.99 && m.get(6, 50) > 0.99 && m.get(15, 50) == 0.0);
    // Zero-length segments equal the dab.
    let one = stroke_mask(60, 60, &[dab(0.4, 0.6, 0.1, 0.5)], false);
    let rep = stroke_mask(60, 60, &[stroke(&[[0.4, 0.6]; 5], 0.1, 0.5, 1.0, false)], false);
    assert_eq!(one, rep);
    // Non-finite points are skipped, NaN parameters do not poison the mask.
    let junk = stroke(&[[f32::NAN, 0.5], [0.4, 0.6], [f32::INFINITY, 0.1]], 0.1, f32::NAN, f32::NAN, false);
    let m = stroke_mask(60, 60, &[junk], false);
    assert!(m.data.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
    assert!(m.get(24, 36) > 0.99);
}

#[test]
fn stroke_mask_clamps_the_radius() {
    let tiny = stroke_mask(200, 100, &[dab(0.5, 0.5, 0.0, 1.0)], false);
    let min = stroke_mask(200, 100, &[dab(0.5, 0.5, 0.0005, 1.0)], false);
    assert_eq!(tiny, min);
    let neg = stroke_mask(200, 100, &[dab(0.5, 0.5, -3.0, 1.0)], false);
    assert_eq!(neg, min);
    let huge = stroke_mask(200, 100, &[dab(0.5, 0.5, 100.0, 1.0)], false);
    let max = stroke_mask(200, 100, &[dab(0.5, 0.5, 0.5, 1.0)], false);
    assert_eq!(huge, max);
    assert!(max.get(100, 50) > 0.999 && max.get(10, 50) > 0.999 && max.get(0, 0) == 0.0); // R = 100 px
}

#[test]
fn stroke_mask_survives_huge_stroke_counts() {
    let mut strokes = wavy_strokes(3000, 20);
    strokes.push(stroke(&(0..5000).map(|i| [(i % 97) as f32 / 97.0, (i % 89) as f32 / 89.0]).collect::<Vec<_>>(), 0.01, 0.5, 0.3, false));
    let m = stroke_mask(160, 100, &strokes, false);
    assert!(m.data.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)));
    assert!(m.data.iter().any(|&v| v > 0.5));
}

#[test]
fn stroke_mask_is_resolution_independent() {
    let strokes = {
        let mut s = wavy_strokes(6, 40);
        s.push(stroke(&[[0.1, 0.9], [0.5, 0.2], [0.9, 0.8]], 0.03, 1.0, 1.0, false));
        s.push(stroke(&[[0.3, 0.5]], 0.05, 0.0, 0.7, true));
        s
    };
    let small = stroke_mask(800, 600, &strokes, false);
    let big = stroke_mask(1600, 1200, &strokes, false);
    let back = resize_mask(&big, 800, 600);
    let mean: f32 = small.data.iter().zip(&back.data).map(|(a, b)| (a - b).abs()).sum::<f32>() / small.data.len() as f32;
    assert!(mean <= 0.01, "mean abs difference {mean}");
    assert!(small.data.iter().any(|&v| v > 0.5));
}

// ---------------------------------------------------------------- curves

const S_CURVE: [[f32; 2]; 5] = [[0.0, 0.0], [0.25, 0.1], [0.5, 0.5], [0.75, 0.9], [1.0, 1.0]];

#[test]
fn curve_channel_index_order() {
    assert_eq!(CurveChannel::from_index(0), Some(CurveChannel::Luminance));
    assert_eq!(CurveChannel::from_index(1), Some(CurveChannel::Red));
    assert_eq!(CurveChannel::from_index(2), Some(CurveChannel::Green));
    assert_eq!(CurveChannel::from_index(3), Some(CurveChannel::Blue));
    assert_eq!(CurveChannel::from_index(7), None); // 4..=6 are the colour channels since wave 8
}

#[test]
fn point_curve_identity_cases_are_bit_identical() {
    let mut src = ImageBuf::new(4, 2);
    let vals = [0.0, -0.1, 16.0, 1.0e6, 0.18, 0.001, 3.3, -2.0];
    for (i, v) in vals.iter().enumerate() {
        src.set_pixel(i as u32 % 4, i as u32 / 4, [*v, v * 0.5, 0.2, 0.3]);
    }
    for pts in [&[][..], &[[0.5, 0.7]][..], &[[0.0, 0.0], [1.0, 1.0]][..]] {
        for ch in [CurveChannel::Luminance, CurveChannel::Red, CurveChannel::Green, CurveChannel::Blue] {
            let mut img = src.clone();
            point_curve(&mut img, pts, ch);
            assert_eq!(img.data(), src.data());
        }
    }
    assert_eq!(curve_eval(&[], 0.37), 0.37);
    assert!((curve_eval(&[[0.0, 0.0], [1.0, 1.0]], 0.37) - 0.37).abs() < 1e-6);
}

#[test]
fn curve_passes_through_its_points_and_extends_flat() {
    let pts = [[0.2, 0.1], [0.4, 0.5], [0.7, 0.55], [0.9, 0.95]];
    for p in &pts {
        assert!((curve_eval(&pts, p[0]) - p[1]).abs() < 1e-6, "{p:?}");
    }
    assert_eq!(curve_eval(&pts, 0.0), 0.1);
    assert_eq!(curve_eval(&pts, 0.1), 0.1);
    assert_eq!(curve_eval(&pts, 1.0), 0.95);
    // Unsorted input and out-of-range points are cleaned up.
    let messy = [[0.9, 0.95], [0.2, 0.1], [0.4, 0.5], [0.7, 0.55], [f32::NAN, 0.3]];
    for t in [0.0, 0.3, 0.55, 0.8, 1.0] {
        assert_eq!(curve_eval(&messy, t), curve_eval(&pts, t));
    }
    // A later point with the same x wins.
    assert_eq!(curve_eval(&[[0.0, 0.0], [0.5, 0.2], [0.5, 0.6], [1.0, 1.0]], 0.5), 0.6);
    assert_eq!(curve_eval(&S_CURVE, -1.0), 0.0);
    assert_eq!(curve_eval(&S_CURVE, 2.0), 1.0);
}

#[test]
fn monotone_points_give_a_monotone_curve_without_overshoot() {
    let pts = [[0.0, 0.0], [0.05, 0.0], [0.3, 0.02], [0.35, 0.6], [0.7, 0.62], [0.72, 1.0], [1.0, 1.0]]; // very steep steps
    let mut prev = -1.0;
    for i in 0..=4000 {
        let t = i as f32 / 4000.0;
        let v = curve_eval(&pts, t);
        assert!(v >= prev - 1e-6, "not monotone at {t}");
        assert!((0.0..=1.0).contains(&v));
        prev = v;
    }
    // Between neighbouring points the value stays between their outputs.
    for w in pts.windows(2) {
        for i in 0..=50 {
            let t = w[0][0] + (w[1][0] - w[0][0]) * i as f32 / 50.0;
            let v = curve_eval(&pts, t);
            assert!(v >= w[0][1].min(w[1][1]) - 1e-6 && v <= w[0][1].max(w[1][1]) + 1e-6, "overshoot at {t}: {v}");
        }
    }
}

#[test]
fn non_monotone_requests_are_followed_without_overshoot() {
    let pts = [[0.0, 0.0], [0.3, 0.8], [0.6, 0.2], [1.0, 1.0]];
    for p in &pts {
        assert!((curve_eval(&pts, p[0]) - p[1]).abs() < 1e-6);
    }
    for w in pts.windows(2) {
        for i in 0..=100 {
            let t = w[0][0] + (w[1][0] - w[0][0]) * i as f32 / 100.0;
            let v = curve_eval(&pts, t);
            assert!(v >= w[0][1].min(w[1][1]) - 1e-6 && v <= w[0][1].max(w[1][1]) + 1e-6, "overshoot at {t}: {v}");
        }
    }
    // The dip really happens.
    assert!(curve_eval(&pts, 0.5) < curve_eval(&pts, 0.3));
}

#[test]
fn point_curve_hdr_safety() {
    let mut img = ImageBuf::new(6, 1);
    let vals = [0.0f32, -0.5, 16.0, 1.0e6, 3.0e38, 0.18];
    for (i, v) in vals.iter().enumerate() {
        img.set_pixel(i as u32, 0, [*v, *v, *v, 0.25]);
    }
    for ch in [CurveChannel::Luminance, CurveChannel::Red, CurveChannel::Green, CurveChannel::Blue] {
        for pts in [&S_CURVE[..], &[[0.0, 0.3], [1.0, 1.0]][..], &[[0.0, 0.0], [0.5, 1.0], [1.0, 1.0]][..]] {
            let mut out = img.clone();
            point_curve(&mut out, pts, ch);
            assert!(out.data().iter().all(|v| v.is_finite()), "{ch:?}");
            for x in 0..6 {
                assert_eq!(out.pixel(x, 0)[3], 0.25);
            }
        }
    }
    let mut out = img.clone();
    point_curve(&mut out, &S_CURVE, CurveChannel::Luminance);
    assert_eq!(out.pixel(0, 0)[..3], [0.0; 3]); // black stays black
    assert_eq!(out.pixel(1, 0)[..3], [-0.5; 3]); // negative luminance unchanged
    assert!(out.pixel(2, 0)[0] > 10.0 && out.pixel(3, 0)[0] > 1.0e5); // highlights survive, not clamped
    // A curve that maps the top to 1 sends the brightest values to the large finite maximum.
    let mut out = img.clone();
    point_curve(&mut out, &[[0.0, 0.0], [0.5, 1.0], [1.0, 1.0]], CurveChannel::Green);
    assert!(out.pixel(3, 0)[1].is_finite() && out.pixel(3, 0)[1] > 1.0e5);
    assert_eq!(out.pixel(3, 0)[0], 1.0e6);
    assert_eq!(out.pixel(1, 0)[1], -0.5);
}

#[test]
fn brightening_curve_raises_mid_grey_by_the_expected_amount() {
    // Mid-grey 0.18 sits at t = 0.5; the curve sends 0.5 -> 0.7, i.e. v = 0.18 * 0.7 / 0.3 = 0.42.
    let pts = [[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]];
    let expected = 0.18 * 0.7 / 0.3;
    for ch in [CurveChannel::Luminance, CurveChannel::Red] {
        let mut img = grey(2, 2, 0.18);
        point_curve(&mut img, &pts, ch);
        let v = img.pixel(1, 1)[0];
        assert!((v - expected).abs() < 2e-4, "{ch:?}: {v} vs {expected}");
    }
}

#[test]
fn colour_channels_touch_only_their_channel() {
    let mut src = ImageBuf::new(3, 1);
    src.set_pixel(0, 0, [0.3, 0.5, 0.1, 0.9]);
    src.set_pixel(1, 0, [2.0, 0.2, 4.0, 0.8]);
    src.set_pixel(2, 0, [-0.2, 0.2, 0.4, 0.7]);
    for (ch, c) in [(CurveChannel::Red, 0), (CurveChannel::Green, 1), (CurveChannel::Blue, 2)] {
        let mut img = src.clone();
        point_curve(&mut img, &S_CURVE, ch);
        for x in 0..3 {
            let (a, b) = (src.pixel(x, 0), img.pixel(x, 0));
            for k in [0, 1, 2, 3] {
                if k != c {
                    assert_eq!(a[k], b[k], "{ch:?} changed channel {k}");
                }
            }
        }
        assert_ne!(img.pixel(0, 0)[c], src.pixel(0, 0)[c]);
    }
    // Negative values are unchanged.
    let mut img = src.clone();
    point_curve(&mut img, &S_CURVE, CurveChannel::Red);
    assert_eq!(img.pixel(2, 0)[0], -0.2);
}

#[test]
fn luminance_curve_keeps_neutral_neutral_and_scales_rgb() {
    let mut img = ImageBuf::new(3, 1);
    img.set_pixel(0, 0, [0.05, 0.05, 0.05, 1.0]);
    img.set_pixel(1, 0, [0.7, 0.7, 0.7, 1.0]);
    img.set_pixel(2, 0, [0.4, 0.2, 0.1, 1.0]);
    let src = img.clone();
    point_curve(&mut img, &S_CURVE, CurveChannel::Luminance);
    for x in 0..2 {
        let p = img.pixel(x, 0);
        assert!(p[0] == p[1] && p[1] == p[2], "{p:?}");
    }
    assert!(img.pixel(0, 0)[0] < 0.05 && img.pixel(1, 0)[0] > 0.7); // S-curve: darker shadows, brighter lights
    let (a, b) = (src.pixel(2, 0), img.pixel(2, 0));
    let g = b[0] / a[0];
    assert!((b[1] / a[1] - g).abs() < 1e-5 && (b[2] / a[2] - g).abs() < 1e-5);
    // The new luminance is the curve's value.
    let y = |p: [f32; 4]| crate::util::luma(p[0], p[1], p[2]);
    let t = y(a) / (y(a) + 0.18);
    let t2 = curve_eval(&S_CURVE, t);
    assert!((y(b) - 0.18 * t2 / (1.0 - t2)).abs() < 1e-4);
}

#[test]
fn lookup_table_matches_direct_evaluation() {
    let curves: [&[[f32; 2]]; 3] = [&S_CURVE, &[[0.0, 0.2], [0.3, 0.6], [1.0, 0.9]], &[[0.1, 0.05], [0.5, 0.8], [0.8, 0.82]]];
    let n = 5000;
    for (ci, pts) in curves.into_iter().enumerate() {
        let mut img = ImageBuf::new(n, 1);
        for i in 0..n {
            // Spread over the perceptual axis (t = 0.0005 .. 0.97) via the linear value.
            let t = 0.0005 + 0.9695 * i as f32 / (n - 1) as f32;
            let v = 0.18 * t / (1.0 - t);
            img.set_pixel(i, 0, [v, v, v, 1.0]);
        }
        let src = img.clone();
        point_curve(&mut img, pts, CurveChannel::Green);
        let mut worst = 0.0f32;
        for i in 0..n {
            let v = src.pixel(i, 0)[1];
            let t = v / (v + 0.18);
            let direct_t = curve_eval(pts, t);
            let got = img.pixel(i, 0)[1];
            let got_t = got / (got + 0.18);
            worst = worst.max((got_t - direct_t).abs());
        }
        // Smooth curve: table error is tiny; curves with flat extensions have kinks (slope jump up to 3).
        let tol = if ci == 0 { 1e-5 } else { 3e-4 };
        assert!(worst < tol, "curve {ci}: worst t error {worst}");
    }
}
