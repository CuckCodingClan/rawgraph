//! Wave 8 tests: place_layer, blend_layer, warp_rectilinear, vignette_radial.

use rawgraph_core::{BlendMode, FitMode, ImageBuf, ResampleFilter, VignetteRadial, WarpRectilinear};

use crate::*;

const FILTERS: [ResampleFilter; 3] = [ResampleFilter::Linear, ResampleFilter::Bicubic, ResampleFilter::Nearest];
const MODES: [BlendMode; 9] = [
    BlendMode::Normal,
    BlendMode::Add,
    BlendMode::Subtract,
    BlendMode::Multiply,
    BlendMode::Screen,
    BlendMode::Overlay,
    BlendMode::Darken,
    BlendMode::Lighten,
    BlendMode::Difference,
];

fn filled(w: u32, h: u32, v: [f32; 4]) -> ImageBuf {
    let mut i = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            i.set_pixel(x, y, v);
        }
    }
    i
}

fn from_fn(w: u32, h: u32, mut f: impl FnMut(u32, u32) -> [f32; 4]) -> ImageBuf {
    let mut i = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            i.set_pixel(x, y, f(x, y));
        }
    }
    i
}

fn alpha_sum(i: &ImageBuf) -> f64 {
    i.data().chunks_exact(4).map(|p| p[3] as f64).sum()
}

/// Expected layer rectangle (x0, y0, w, h) on the canvas.
fn expected_rect(lw: f64, lh: f64, cw: f64, ch: f64, fit: FitMode, crop: f64) -> (f64, f64, f64, f64) {
    let (sx, sy) = match fit {
        FitMode::Fit => {
            let s = (cw / lw).min(ch / lh);
            (s, s)
        }
        FitMode::Fill => {
            let s = (cw / lw).max(ch / lh);
            (s, s)
        }
        FitMode::Stretch => (cw / lw, ch / lh),
        FitMode::Crop => (crop, crop),
    };
    let (w, h) = (lw * sx, lh * sy);
    ((cw - w) / 2.0, (ch - h) / 2.0, w, h)
}

fn rng(seed: u32) -> impl FnMut() -> f32 {
    let mut s = seed;
    move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (s >> 8) as f32 / (1u32 << 24) as f32
    }
}

// ---------------------------------------------------------------- place_layer

#[test]
fn place_layer_coverage_rectangles_for_every_fit() {
    let shapes: [(u32, u32); 3] = [(30, 20), (20, 30), (24, 24)];
    let canvases: [(u32, u32); 3] = [(60, 40), (40, 60), (40, 40)];
    for fit in [FitMode::Fit, FitMode::Fill, FitMode::Crop, FitMode::Stretch] {
        for &(lw, lh) in &shapes {
            for &(cw, ch) in &canvases {
                for filter in FILTERS {
                    let layer = filled(lw, lh, [0.5, 0.25, 0.1, 1.0]);
                    let out = place_layer(&layer, cw, ch, fit, filter, 0.75);
                    assert_eq!((out.width(), out.height()), (cw, ch));
                    let (x0, y0, w, h) = expected_rect(lw as f64, lh as f64, cw as f64, ch as f64, fit, 0.75);
                    // exact area coverage per pixel
                    for y in 0..ch {
                        for x in 0..cw {
                            let ox = ((x as f64 + 1.0).min(x0 + w) - (x as f64).max(x0)).clamp(0.0, 1.0);
                            let oy = ((y as f64 + 1.0).min(y0 + h) - (y as f64).max(y0)).clamp(0.0, 1.0);
                            let p = out.pixel(x, y);
                            assert!(
                                (p[3] as f64 - ox * oy).abs() < 1e-5,
                                "{fit:?} {lw}x{lh} on {cw}x{ch} at {x},{y}: {} vs {}",
                                p[3],
                                ox * oy
                            );
                            if p[3] > 0.0 {
                                assert!((p[0] - 0.5).abs() < 1e-5 && (p[1] - 0.25).abs() < 1e-5, "{p:?}");
                            } else {
                                assert_eq!(p, [0.0; 4]);
                            }
                        }
                    }
                    assert!((alpha_sum(&out) - w.min(cw as f64) * h.min(ch as f64)).abs() < 1e-2);
                }
            }
        }
    }
}

#[test]
fn place_layer_known_rectangles() {
    // landscape layer on portrait canvas: Fit leaves bars top and bottom.
    let layer = filled(30, 20, [1.0, 1.0, 1.0, 1.0]);
    let fit = place_layer(&layer, 40, 60, FitMode::Fit, ResampleFilter::Nearest, 1.0);
    assert_eq!(fit.pixel(20, 5)[3], 0.0);
    assert_eq!(fit.pixel(20, 30)[3], 1.0);
    // 40 x 26.67 rect: rows 16.67..43.33 -> row 16 partially covered
    assert!((fit.pixel(0, 16)[3] - 0.3333).abs() < 1e-3);
    let fill = place_layer(&layer, 40, 60, FitMode::Fill, ResampleFilter::Nearest, 1.0);
    assert!(fill.data().chunks_exact(4).all(|p| p[3] == 1.0));
    // Crop 1.0 on a canvas larger than the layer: exact pixels
    let crop = place_layer(&layer, 40, 40, FitMode::Crop, ResampleFilter::Nearest, 1.0);
    assert_eq!(alpha_sum(&crop), 600.0);
    assert_eq!(crop.pixel(4, 10)[3], 0.0);
    assert_eq!(crop.pixel(5, 10)[3], 1.0);
}

#[test]
fn place_layer_constant_stays_constant() {
    let layer = filled(37, 23, [0.7, 0.2, 3.0, 1.0]);
    for filter in FILTERS {
        for (cw, ch) in [(100, 80), (13, 9), (37, 23), (300, 7)] {
            let out = place_layer(&layer, cw, ch, FitMode::Stretch, filter, 1.0);
            for p in out.data().chunks_exact(4) {
                assert!((p[0] - 0.7).abs() < 1e-5 && (p[1] - 0.2).abs() < 1e-5 && (p[2] - 3.0).abs() < 1e-5, "{p:?}");
                assert!((p[3] - 1.0).abs() < 1e-6);
            }
        }
    }
}

#[test]
fn place_layer_nearest_keeps_exact_values() {
    let layer = from_fn(4, 4, |x, y| [(x * 4 + y) as f32 * 0.37, 0.0, 1.0, 1.0]);
    let out = place_layer(&layer, 12, 8, FitMode::Stretch, ResampleFilter::Nearest, 1.0);
    for y in 0..8 {
        for x in 0..12 {
            let src = layer.pixel(x / 3, y / 2);
            assert_eq!(out.pixel(x, y)[..3], src[..3], "at {x},{y}");
        }
    }
    // 1:1 placement is an exact copy for every filter
    for filter in FILTERS {
        let same = place_layer(&layer, 4, 4, FitMode::Fit, filter, 1.0);
        assert_eq!(same.data(), layer.data(), "{filter:?}");
    }
}

#[test]
fn place_layer_minifying_checkerboard_gives_grey() {
    let board = from_fn(64, 64, |x, y| {
        let v = if (x + y) % 2 == 0 { 1.0 } else { 0.0 };
        [v, v, v, 1.0]
    });
    for filter in FILTERS {
        for (cw, ch) in [(8, 8), (7, 7), (16, 16), (9, 5)] {
            let out = place_layer(&board, cw, ch, FitMode::Stretch, filter, 1.0);
            for p in out.data().chunks_exact(4) {
                assert!((p[0] - 0.5).abs() < 0.06, "{filter:?} {cw}x{ch}: {p:?}");
            }
        }
    }
    // exactly 8x: exact grey
    let out = place_layer(&board, 8, 8, FitMode::Stretch, ResampleFilter::Linear, 1.0);
    assert!(out.data().chunks_exact(4).all(|p| (p[0] - 0.5).abs() < 1e-6));
}

#[test]
fn place_layer_crop_scale() {
    let layer = from_fn(8, 8, |x, y| [x as f32, y as f32, 0.0, 1.0]);
    let one = place_layer(&layer, 8, 8, FitMode::Crop, ResampleFilter::Nearest, 1.0);
    assert_eq!(one.data(), layer.data());
    // 2.0: 16 x 16 rect on a 16 canvas, every layer pixel becomes a 2 x 2 block
    let two = place_layer(&layer, 16, 16, FitMode::Crop, ResampleFilter::Nearest, 2.0);
    assert!(two.data().chunks_exact(4).all(|p| p[3] == 1.0));
    assert_eq!(two.pixel(5, 3)[..2], [2.0, 1.0]);
    // 2.0 on a smaller canvas crops the centre
    let two_small = place_layer(&layer, 8, 8, FitMode::Crop, ResampleFilter::Nearest, 2.0);
    assert_eq!(two_small.pixel(0, 0)[..2], [2.0, 2.0]);
    assert_eq!(two_small.pixel(7, 7)[..2], [5.0, 5.0]);
    // 0.5: 4 x 4 rect in the middle
    let half = place_layer(&layer, 8, 8, FitMode::Crop, ResampleFilter::Linear, 0.5);
    assert!((alpha_sum(&half) - 16.0).abs() < 1e-4);
    assert_eq!(half.pixel(1, 1)[3], 0.0);
    assert_eq!(half.pixel(2, 2)[3], 1.0);
    // non-finite or non-positive counts as 1
    for bad in [f32::NAN, f32::INFINITY, 0.0, -2.0] {
        let o = place_layer(&layer, 8, 8, FitMode::Crop, ResampleFilter::Nearest, bad);
        assert_eq!(o.data(), layer.data());
    }
}

#[test]
fn place_layer_degenerate_sizes_never_panic() {
    let layer = filled(5, 5, [1.0; 4]);
    for fit in [FitMode::Fit, FitMode::Fill, FitMode::Crop, FitMode::Stretch] {
        for filter in FILTERS {
            let o = place_layer(&layer, 0, 7, fit, filter, 1.0);
            assert_eq!((o.width(), o.height()), (0, 7));
            let o = place_layer(&layer, 7, 0, fit, filter, 1.0);
            assert_eq!((o.width(), o.height()), (7, 0));
            let empty = ImageBuf::new(0, 0);
            let o = place_layer(&empty, 4, 3, fit, filter, 1.0);
            assert_eq!(o.data(), &[0.0f32; 48][..]);
            let o = place_layer(&layer, 100_000, 100_000, fit, filter, 1.0);
            assert_eq!(o.data().len(), 0);
            // extreme ratios
            let o = place_layer(&layer, 1, 1, fit, filter, f32::MAX);
            assert!(o.data().iter().all(|v| v.is_finite()));
            let o = place_layer(&layer, 3, 3, fit, filter, 1e-30);
            assert!(o.data().iter().all(|v| v.is_finite()));
        }
    }
    let thin = filled(1, 4000, [1.0; 4]);
    let o = place_layer(&thin, 50, 10, FitMode::Fit, ResampleFilter::Bicubic, 1.0);
    assert!(o.data().iter().all(|v| v.is_finite()));
}

#[test]
fn place_layer_transparent_layer_pixels_are_premultiplied() {
    // Left half opaque red, right half fully transparent with garbage colour.
    let layer = from_fn(8, 2, |x, _| if x < 4 { [1.0, 0.0, 0.0, 1.0] } else { [0.0, 1.0, 0.0, 0.0] });
    let out = place_layer(&layer, 16, 4, FitMode::Stretch, ResampleFilter::Linear, 1.0);
    for p in out.data().chunks_exact(4) {
        if p[3] > 1e-4 {
            // never turns greenish: the transparent side contributes no colour
            assert!(p[1].abs() < 1e-4, "{p:?}");
            assert!((p[0] - 1.0).abs() < 1e-4);
        }
    }
    let half = filled(4, 4, [0.4, 0.5, 0.6, 0.5]);
    let o = place_layer(&half, 8, 8, FitMode::Stretch, ResampleFilter::Bicubic, 1.0);
    assert!(o.data().chunks_exact(4).all(|p| (p[3] - 0.5).abs() < 1e-5 && (p[1] - 0.5).abs() < 1e-5));
}

#[test]
fn place_layer_is_fast_enough_for_24_megapixels() {
    let (w, h) = (6000u32, 4000u32);
    let layer = from_fn(w, 8, |x, y| [x as f32 * 1e-4, y as f32, 0.5, 1.0]);
    // a 24 MP layer built cheaply by repeating rows
    let mut big = ImageBuf::new(w, h);
    for y in 0..h as usize {
        let row = &layer.data()[(y % 8) * w as usize * 4..(y % 8 + 1) * w as usize * 4];
        big.data_mut()[y * w as usize * 4..(y + 1) * w as usize * 4].copy_from_slice(row);
    }
    let t = std::time::Instant::now();
    let out = place_layer(&big, w, h, FitMode::Crop, ResampleFilter::Bicubic, 1.0001);
    let secs = t.elapsed().as_secs_f64();
    assert!(out.data().iter().all(|v| v.is_finite()));
    assert!(secs < 4.0, "took {secs:.2} s"); // debug-ish builds are slower; release is well under 1 s
    println!("place_layer 24 MP bicubic: {secs:.3} s");
}

// ---------------------------------------------------------------- blend_layer

fn blend_one(mode: BlendMode, base: f32, layer: f32, alpha: f32, opacity: f32) -> f32 {
    let mut c = filled(1, 1, [base, base, base, 0.9]);
    let p = filled(1, 1, [layer, layer, layer, alpha]);
    blend_layer(&mut c, &p, mode, opacity);
    assert_eq!(c.pixel(0, 0)[3], 0.9, "canvas alpha is unchanged");
    c.pixel(0, 0)[1]
}

#[test]
fn blend_every_mode_known_values() {
    let (b, l) = (0.5f32, 0.25f32);
    let expect = [
        (BlendMode::Normal, 0.25),
        (BlendMode::Add, 0.75),
        (BlendMode::Subtract, 0.25),
        (BlendMode::Multiply, 0.125),
        (BlendMode::Screen, 0.625),
        (BlendMode::Overlay, 0.25), // b = 0.5 -> l
        (BlendMode::Darken, 0.25),
        (BlendMode::Lighten, 0.5),
        (BlendMode::Difference, 0.25),
    ];
    for (mode, e) in expect {
        let v = blend_one(mode, b, l, 1.0, 1.0);
        assert!((v - e).abs() < 1e-6, "{mode:?}: {v} vs {e}");
    }
    // overlay both branches (classic values)
    assert!((blend_one(BlendMode::Overlay, 0.25, 0.5, 1.0, 1.0) - 0.25).abs() < 1e-6);
    assert!((blend_one(BlendMode::Overlay, 0.75, 0.5, 1.0, 1.0) - 0.75).abs() < 1e-6);
    assert!((blend_one(BlendMode::Overlay, 0.75, 0.25, 1.0, 1.0) - 0.625).abs() < 1e-6); // 1 - 2*.25*.75
    assert!((blend_one(BlendMode::Screen, 0.0, 0.3, 1.0, 1.0) - 0.3).abs() < 1e-6);
    assert!((blend_one(BlendMode::Screen, 1.0, 0.3, 1.0, 1.0) - 1.0).abs() < 1e-6);
}

#[test]
fn blend_opacity_and_alpha() {
    for mode in MODES {
        assert_eq!(blend_one(mode, 0.4, 0.9, 1.0, 0.0), 0.4, "{mode:?} opacity 0");
        assert_eq!(blend_one(mode, 0.4, 0.9, 0.0, 1.0), 0.4, "{mode:?} alpha 0");
    }
    // Normal, full: exactly the layer (even for awkward floats)
    for (b, l) in [(0.1f32, 0.7f32), (123.456, 0.001), (-3.3, 17.7)] {
        assert_eq!(blend_one(BlendMode::Normal, b, l, 1.0, 1.0), l);
    }
    // half opacity = lerp; alpha 0.5 x opacity 0.5 = 0.25
    assert!((blend_one(BlendMode::Normal, 0.2, 0.8, 1.0, 0.5) - 0.5).abs() < 1e-6);
    assert!((blend_one(BlendMode::Normal, 0.2, 0.8, 0.5, 0.5) - 0.35).abs() < 1e-6);
    // opacity clamped
    assert_eq!(blend_one(BlendMode::Normal, 0.2, 0.8, 1.0, 7.0), 0.8);
    assert_eq!(blend_one(BlendMode::Normal, 0.2, 0.8, 1.0, -1.0), 0.2);
    assert_eq!(blend_one(BlendMode::Normal, 0.2, 0.8, 1.0, f32::NAN), 0.8);
}

#[test]
fn blend_stays_finite_for_extreme_values() {
    let vals = [-5.0f32, -0.3, 0.0, 0.2, 0.5, 1.0, 7.0, 1.0e6, 3.0e38];
    for mode in MODES {
        for &b in &vals {
            for &l in &vals {
                let v = blend_one(mode, b, l, 0.7, 0.8);
                assert!(v.is_finite(), "{mode:?} {b} {l} -> {v}");
            }
        }
    }
    // Screen and Overlay keep rising with the base above 1
    let mut prev = f32::MIN;
    for i in 0..40 {
        let b = i as f32 * 0.25;
        let v = blend_one(BlendMode::Screen, b, 0.6, 1.0, 1.0);
        assert!(v >= prev, "screen not monotonic at {b}");
        prev = v;
    }
    let mut prev = f32::MIN;
    for i in 0..40 {
        let b = i as f32 * 0.25;
        let v = blend_one(BlendMode::Overlay, b, 0.6, 1.0, 1.0);
        assert!(v >= prev - 1e-6, "overlay not monotonic at {b}");
        prev = v;
    }
    // continuity of Overlay at 0.5 for layers above 1
    let a = blend_one(BlendMode::Overlay, 0.5, 3.0, 1.0, 1.0);
    let b = blend_one(BlendMode::Overlay, 0.500_001, 3.0, 1.0, 1.0);
    assert!((a - b).abs() < 1e-4);
}

#[test]
fn blend_size_mismatch_changes_nothing() {
    let mut c = filled(4, 4, [0.3; 4]);
    let before = c.clone();
    blend_layer(&mut c, &filled(3, 4, [1.0; 4]), BlendMode::Add, 1.0);
    assert_eq!(c.data(), before.data());
}

#[test]
fn place_then_blend_respects_coverage() {
    let mut canvas = filled(10, 10, [0.2, 0.2, 0.2, 1.0]);
    let placed = place_layer(&filled(5, 5, [1.0, 0.0, 0.0, 1.0]), 10, 10, FitMode::Crop, ResampleFilter::Nearest, 1.0);
    blend_layer(&mut canvas, &placed, BlendMode::Normal, 1.0);
    assert_eq!(canvas.pixel(5, 5)[..3], [1.0, 0.0, 0.0]);
    assert_eq!(canvas.pixel(0, 0)[..3], [0.2, 0.2, 0.2]);
}

// ---------------------------------------------------------------- warp / vignette

fn warp(planes: Vec<[f64; 6]>) -> WarpRectilinear {
    WarpRectilinear { planes, center: [0.5, 0.5] }
}

const ID: [f64; 6] = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0];

#[test]
fn warp_identity_is_a_no_op() {
    let mut r = rng(11);
    let img = from_fn(31, 23, |_, _| [r() * 3.0, r(), r() * 9.0, 0.5 + r() * 0.5]);
    assert_eq!(warp_rectilinear(&img, &warp(vec![ID])).data(), img.data());
    assert_eq!(warp_rectilinear(&img, &warp(vec![ID, ID, ID])).data(), img.data());
    // almost identity goes through the sampler and stays within 1e-5
    let near = [1.0 + 1e-12, 0.0, 0.0, 0.0, 0.0, 0.0];
    let out = warp_rectilinear(&img, &warp(vec![near]));
    for (a, b) in out.data().iter().zip(img.data()) {
        assert!((a - b).abs() < 1e-5 * b.abs().max(1.0), "{a} vs {b}");
    }
    // degenerate inputs
    assert_eq!(warp_rectilinear(&ImageBuf::new(0, 0), &warp(vec![ID])).data().len(), 0);
    assert_eq!(warp_rectilinear(&img, &warp(vec![])).data(), img.data());
}

#[test]
fn warp_matches_the_dng_formula_at_several_points() {
    let (w, h) = (200u32, 120u32);
    // linear ramp: bicubic reproduces it exactly, so the output shows the source position
    let img = from_fn(w, h, |x, y| [x as f32 + 0.5, y as f32 + 0.5, 0.0, 1.0]);
    let k = [0.97, -0.04, 0.01, 0.0, 0.002, -0.003];
    let out = warp_rectilinear(&img, &warp(vec![k]));
    let (cx, cy) = (100.0f64, 60.0f64);
    let m = (100.0f64).hypot(60.0);
    for &(x, y) in &[(100u32, 60u32), (30, 20), (170, 100), (60, 90), (140, 15), (10, 60)] {
        let dx = (x as f64 + 0.5 - cx) / m;
        let dy = (y as f64 + 0.5 - cy) / m;
        let r2 = dx * dx + dy * dy;
        let f = k[0] + k[1] * r2 + k[2] * r2 * r2 + k[3] * r2 * r2 * r2;
        let sx = cx + m * (f * dx + 2.0 * k[4] * dx * dy + k[5] * (r2 + 2.0 * dx * dx));
        let sy = cy + m * (f * dy + k[4] * (r2 + 2.0 * dy * dy) + 2.0 * k[5] * dx * dy);
        let p = out.pixel(x, y);
        assert!((p[0] as f64 - sx).abs() < 2e-3, "x at {x},{y}: {} vs {sx}", p[0]);
        assert!((p[1] as f64 - sy).abs() < 2e-3, "y at {x},{y}: {} vs {sy}", p[1]);
        assert_eq!(p[3], 1.0);
    }
}

#[test]
fn warp_three_planes_shift_red_and_blue_differently() {
    let img = from_fn(200, 100, |x, _| [x as f32 + 0.5; 4]);
    let planes = vec![[1.01, 0.0, 0.0, 0.0, 0.0, 0.0], ID, [0.99, 0.0, 0.0, 0.0, 0.0, 0.0]];
    let out = warp_rectilinear(&img, &warp(planes));
    // right of the centre: red samples further right, blue further left, green unchanged
    let p = out.pixel(150, 50);
    assert!(p[0] > p[1] + 0.3 && p[2] < p[1] - 0.3, "{p:?}");
    assert!((p[1] - 150.5).abs() < 1e-3);
    // left of the centre the order flips
    let q = out.pixel(50, 50);
    assert!(q[0] < q[1] - 0.3 && q[2] > q[1] + 0.3, "{q:?}");
    // expected: source x = cx + 1.01 * (x + 0.5 - cx)
    assert!((p[0] - (100.0 + 1.01 * 50.5)).abs() < 2e-3, "{}", p[0]);
    assert!((p[2] - (100.0 + 0.99 * 50.5)).abs() < 2e-3);
}

#[test]
fn warp_outside_source_is_black_and_transparent() {
    let img = filled(100, 60, [1.0, 1.0, 1.0, 1.0]);
    // kr0 = 1.5 samples beyond the borders at the edge pixels
    let out = warp_rectilinear(&img, &warp(vec![[1.5, 0.0, 0.0, 0.0, 0.0, 0.0]]));
    assert_eq!(out.pixel(0, 0), [0.0; 4]);
    assert_eq!(out.pixel(99, 59), [0.0; 4]);
    assert_eq!(out.pixel(50, 30), [1.0, 1.0, 1.0, 1.0]);
    // non-finite coefficients do not panic
    let w = warp(vec![[f64::NAN, f64::INFINITY, 0.0, 0.0, f64::NAN, 0.0]]);
    let o = warp_rectilinear(&img, &w);
    assert_eq!(o.data(), img.data());
}

#[test]
fn vignette_identity_and_gain() {
    let img = filled(101, 101, [1.0, 2.0, 0.5, 0.8]);
    let mut a = img.clone();
    vignette_radial(&mut a, &VignetteRadial { k: [0.0; 5], center: [0.5, 0.5] });
    assert_eq!(a.data(), img.data());
    let mut a = img.clone();
    vignette_radial(&mut a, &VignetteRadial { k: [f64::NAN; 5], center: [0.5, 0.5] });
    assert_eq!(a.data(), img.data());

    let k = [0.4, -0.1, 0.2, 0.05, 0.1];
    let mut a = img.clone();
    vignette_radial(&mut a, &VignetteRadial { k, center: [0.5, 0.5] });
    let m = (50.5f64).hypot(50.5);
    for &(x, y) in &[(50u32, 50u32), (0, 0), (100, 100), (20, 70), (90, 10)] {
        let r2 = ((x as f64 + 0.5 - 50.5).powi(2) + (y as f64 + 0.5 - 50.5).powi(2)) / (m * m);
        let g = 1.0 + k[0] * r2 + k[1] * r2.powi(2) + k[2] * r2.powi(3) + k[3] * r2.powi(4) + k[4] * r2.powi(5);
        let p = a.pixel(x, y);
        assert!((p[0] as f64 - g).abs() < 1e-5, "{x},{y}: {} vs {g}", p[0]);
        assert!((p[1] as f64 - 2.0 * g).abs() < 2e-5);
        assert_eq!(p[3], 0.8, "alpha untouched");
    }
    // centre = 1, corner of a big image = 1 + sum(k)
    let big = filled(2001, 1001, [1.0, 1.0, 1.0, 1.0]);
    let mut b = big.clone();
    vignette_radial(&mut b, &VignetteRadial { k, center: [0.5, 0.5] });
    assert!((b.pixel(1000, 500)[0] - 1.0).abs() < 1e-6);
    let sum: f32 = k.iter().sum::<f64>() as f32;
    assert!((b.pixel(0, 0)[0] - (1.0 + sum)).abs() < 2e-3, "{}", b.pixel(0, 0)[0]);
    assert!((b.pixel(2000, 1000)[0] - (1.0 + sum)).abs() < 2e-3);
    // an off-centre centre moves the far corner
    let mut c = filled(100, 100, [1.0; 4]);
    vignette_radial(&mut c, &VignetteRadial { k: [1.0, 0.0, 0.0, 0.0, 0.0], center: [0.0, 0.0] });
    assert!((c.pixel(0, 0)[0] - 1.0).abs() < 1e-3);
    assert!((c.pixel(99, 99)[0] - 2.0).abs() < 1e-2);
}
