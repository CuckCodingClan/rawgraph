//! Tests of colour balance, local contrast and the transform coordinate mapping.

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::{CdlParams, ImageBuf};

use crate::*;

fn lum(px: [f32; 4]) -> f32 {
    AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2]
}

fn solid(w: u32, h: u32, mut f: impl FnMut(u32, u32) -> [f32; 3]) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let c = f(x, y);
            img.set_pixel(x, y, [c[0], c[1], c[2], 0.5]);
        }
    }
    img
}

fn rng(seed: u32) -> impl FnMut() -> f32 {
    let mut s = seed;
    move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (s >> 8) as f32 / (1u32 << 24) as f32
    }
}

// ---------------------------------------------------------------- color_wheels

fn px3(vals: [f32; 3]) -> ImageBuf {
    let mut i = ImageBuf::new(1, 1);
    i.set_pixel(0, 0, [vals[0], vals[1], vals[2], 0.37]);
    i
}

#[test]
fn color_wheels_identity_is_bit_identical() {
    let mut r = rng(7);
    let img = solid(9, 7, |_, _| [r() * 3.0 - 0.5, r() * 3.0, r() * 100.0]);
    let mut a = img.clone();
    color_wheels(&mut a, &CdlParams::IDENTITY);
    assert_eq!(a.data(), img.data());
}

#[test]
fn color_wheels_slope_offset_power_hand_computed() {
    // slope only
    let mut a = px3([0.5, 1.0, 2.0]);
    color_wheels(&mut a, &CdlParams { slope: [2.0, 0.5, 1.5], ..CdlParams::IDENTITY });
    assert_eq!(a.pixel(0, 0), [1.0, 0.5, 3.0, 0.37]);
    // offset only (also lifts black)
    let mut a = px3([0.0, 0.5, 1.0]);
    color_wheels(&mut a, &CdlParams { offset: [0.1, 0.0, -0.25], ..CdlParams::IDENTITY });
    let p = a.pixel(0, 0);
    assert!((p[0] - 0.1).abs() < 1e-7 && p[1] == 0.5 && (p[2] - 0.75).abs() < 1e-7);
    // power only: 0.25^0.5 = 0.5, 4^0.5 = 2, 1 stays 1
    let mut a = px3([0.25, 4.0, 1.0]);
    color_wheels(&mut a, &CdlParams { power: [0.5, 0.5, 3.0], ..CdlParams::IDENTITY });
    let p = a.pixel(0, 0);
    assert!((p[0] - 0.5).abs() < 1e-6 && (p[1] - 2.0).abs() < 1e-6 && (p[2] - 1.0).abs() < 1e-7);
    // all three: (0.5 * 2 + 0.25)^2 = 1.5625
    let mut a = px3([0.5, 0.5, 0.5]);
    color_wheels(&mut a, &CdlParams { slope: [2.0; 3], offset: [0.25; 3], power: [2.0; 3] });
    for c in 0..3 {
        assert!((a.pixel(0, 0)[c] - 1.5625).abs() < 1e-6);
    }
}

#[test]
fn color_wheels_negative_zero_and_hdr_inputs() {
    let p = CdlParams { slope: [2.0; 3], offset: [-0.1; 3], power: [2.0, 0.5, 3.0] };
    // x = 0: v = -0.1 <= 0, kept at the linear value; x = -1: v = -2.1 likewise.
    let mut a = px3([0.0, -1.0, 0.05]);
    color_wheels(&mut a, &p);
    let o = a.pixel(0, 0);
    assert!((o[0] + 0.1).abs() < 1e-7, "{o:?}");
    assert!((o[1] + 2.1).abs() < 1e-6, "{o:?}");
    assert_eq!(o[2], 0.0); // v == 0 exactly: no power, value 0
    // above 1.0: power > 1 expands, power < 1 compresses (pivot at 1)
    let mut a = px3([8.0, 8.0, 1.0e6]);
    color_wheels(&mut a, &CdlParams { power: [2.0, 0.5, 1.5], ..CdlParams::IDENTITY });
    let o = a.pixel(0, 0);
    assert!((o[0] - 64.0).abs() < 1e-3 && (o[1] - 8.0f32.sqrt()).abs() < 1e-5);
    assert!(o[2].is_finite() && (o[2] / 1.0e9 - 1.0).abs() < 1e-3);
    // overflow is limited, not infinite
    let mut a = px3([1.0e30, 1.0, 1.0]);
    color_wheels(&mut a, &CdlParams { power: [4.0, 1.0, 1.0], ..CdlParams::IDENTITY });
    assert!(a.pixel(0, 0)[0].is_finite());
}

#[test]
fn color_wheels_channels_independent_and_alpha_untouched() {
    let mut r = rng(3);
    let img = solid(8, 8, |_, _| [r() * 2.0, r() * 2.0, r() * 2.0]);
    let p = CdlParams { slope: [1.3, 1.0, 1.0], offset: [0.0, 0.05, 0.0], power: [1.0, 1.0, 0.8] };
    let mut a = img.clone();
    color_wheels(&mut a, &p);
    for (o, i) in a.data().chunks_exact(4).zip(img.data().chunks_exact(4)) {
        assert_eq!(o[3], i[3]);
        assert!((o[0] - i[0] * 1.3).abs() < 1e-6, "red only sees slope");
        assert!((o[1] - (i[1] + 0.05)).abs() < 1e-6, "green only sees offset");
        assert!((o[2] - i[2].powf(0.8)).abs() < 1e-5, "blue only sees power");
    }
    // Non-finite parameters are neutral.
    let mut a = img.clone();
    color_wheels(&mut a, &CdlParams { slope: [f32::NAN; 3], offset: [f32::INFINITY; 3], power: [f32::NAN; 3] });
    assert_eq!(a.data(), img.data());
}

// ---------------------------------------------------------------- local_contrast

/// Textured test image: smooth gradient plus random fine texture, luminance 0.05..1.5.
fn textured(w: u32, h: u32, amp: f32) -> ImageBuf {
    let mut r = rng(99);
    solid(w, h, |x, y| {
        let base = 0.15 + 0.5 * (x as f32 / w as f32) + 0.2 * (y as f32 / h as f32);
        let v = base * (1.0 + amp * (r() - 0.5));
        [v, v * 0.9, v * 0.8]
    })
}

fn mean_lum(img: &ImageBuf) -> f64 {
    img.data().chunks_exact(4).map(|p| lum([p[0], p[1], p[2], 0.0]) as f64).sum::<f64>() / (img.data().len() / 4) as f64
}

/// Standard deviation of ln luminance over a window of the image.
fn fine_amplitude(img: &ImageBuf) -> f64 {
    let (w, h) = (img.width(), img.height());
    // difference to the 3x3 mean = fine detail
    let mut acc = 0.0f64;
    let mut n = 0.0f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let l = |dx: i32, dy: i32| (lum(img.pixel((x as i32 + dx) as u32, (y as i32 + dy) as u32)) + 0.02).ln() as f64;
            let mut m = 0.0;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    m += l(dx, dy);
                }
            }
            let d = l(0, 0) - m / 9.0;
            acc += d * d;
            n += 1.0;
        }
    }
    (acc / n).sqrt()
}

#[test]
fn local_contrast_zero_is_clone_and_flat_stays_flat() {
    let img = textured(40, 30, 0.5);
    assert_eq!(local_contrast(&img, 0.0, 0.0).data(), img.data());
    assert_eq!(local_contrast(&img, f32::NAN, f32::INFINITY.min(0.0)).data(), img.data());
    let flat = solid(64, 48, |_, _| [0.3, 0.2, 0.1]);
    for (c, t) in [(1.0, 0.0), (0.0, 1.0), (-1.0, -1.0), (1.0, 1.0)] {
        let o = local_contrast(&flat, c, t);
        for (a, b) in o.data().iter().zip(flat.data()) {
            assert!((a - b).abs() < 1e-5, "flat image changed for {c} {t}: {a} vs {b}");
        }
    }
}

#[test]
fn local_contrast_keeps_mean_luminance_and_alpha() {
    let img = textured(200, 140, 0.8);
    let m0 = mean_lum(&img);
    for (c, t) in [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (1.0, 1.0), (-0.6, 0.8)] {
        let o = local_contrast(&img, c, t);
        let m = mean_lum(&o);
        let rel = (m / m0 - 1.0).abs();
        println!("mean luminance clarity {c} texture {t}: {:+.3} %", (m / m0 - 1.0) * 100.0);
        assert!(rel < 0.02, "mean moved {rel} for {c} {t}");
        for (a, b) in o.data().chunks_exact(4).zip(img.data().chunks_exact(4)) {
            assert_eq!(a[3], b[3]);
        }
    }
}

#[test]
fn local_contrast_hard_edge_has_no_halo() {
    // Left 0.1, right 0.8 (3 stops, ln ratio 2.1), 400 x 100; edge at x = 200.
    let (lo, hi) = (0.1f32, 0.8f32);
    let img = solid(400, 100, |x, _| if x < 200 { [lo; 3] } else { [hi; 3] });
    let step = hi - lo;
    for (c, t) in [(1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
        let o = local_contrast(&img, c, t);
        // deviation from the plateau of either sign (overshoot or undershoot)
        let mut worst_abs = 0.0f32;
        for x in 0..400u32 {
            let v = o.pixel(x, 50)[1];
            let target = if x < 200 { lo } else { hi };
            worst_abs = worst_abs.max((v - target).abs() / step);
        }
        println!("halo clarity {c} texture {t}: max deviation from plateau {:.2} % of the step", worst_abs * 100.0);
        // The strict 3 % bound is the clarity requirement; the fine scale rings a little within a
        // pixel or two of a hard edge (texture is a 1-px-scale effect), measured about 10 %.
        let bound = if t == 0.0 { 0.03 } else { 0.15 };
        assert!(worst_abs < bound, "halo {worst_abs} for clarity {c}, texture {t}");
    }
}

#[test]
fn local_contrast_texture_amplifies_and_softens_without_inverting() {
    let img = textured(640, 400, 0.4);
    let a0 = fine_amplitude(&img);
    let plus = fine_amplitude(&local_contrast(&img, 0.0, 1.0));
    let minus_img = local_contrast(&img, 0.0, -1.0);
    let minus = fine_amplitude(&minus_img);
    println!("fine amplitude: base {a0:.4}, texture +1 {plus:.4}, texture -1 {minus:.4}");
    assert!(plus > 1.5 * a0, "texture +1 should raise fine amplitude clearly");
    assert!(minus < 0.5 * a0, "texture -1 should reduce it clearly");
    // never inverted: correlation of the output's fine detail with the input's stays positive.
    let (w, h) = (img.width(), img.height());
    let detail = |im: &ImageBuf, x: u32, y: u32| {
        let l = |xx: u32, yy: u32| (lum(im.pixel(xx, yy)) + 0.02).ln();
        let mut m = 0.0;
        for dy in 0..3 {
            for dx in 0..3 {
                m += l(x + dx - 1, y + dy - 1);
            }
        }
        l(x, y) - m / 9.0
    };
    let mut dot = 0.0f64;
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            dot += (detail(&img, x, y) * detail(&minus_img, x, y)) as f64;
        }
    }
    assert!(dot > 0.0, "softened detail must keep its sign");
    // clarity -1 also reduces, clarity +1 raises, mid-scale
    let mid = |im: &ImageBuf| fine_amplitude(im);
    assert!(mid(&local_contrast(&img, -1.0, 0.0)) < a0);
    assert!(mid(&local_contrast(&img, 1.0, 0.0)) > a0);
}

#[test]
fn local_contrast_hdr_safety() {
    let mut data = Vec::new();
    let vals: [[f32; 3]; 8] = [
        [0.0, 0.0, 0.0],
        [-0.1, 0.2, 0.3],
        [16.0, 16.0, 16.0],
        [1.0e6, 5.0e5, 2.0e5],
        [1.0, 0.5, 0.2],
        [0.001, 0.002, 0.0005],
        [0.18, 0.18, 0.18],
        [-1.0, -1.0, -1.0],
    ];
    for k in 0..(32 * 32) {
        let v = vals[(k * 7 + k / 5) % 8];
        data.extend_from_slice(&[v[0], v[1], v[2], 0.3]);
    }
    let img = ImageBuf::from_rgba(32, 32, data).unwrap();
    for (c, t) in [(1.0, 1.0), (-1.0, -1.0), (1.0, -1.0), (0.5, 0.0)] {
        let o = local_contrast(&img, c, t);
        for (a, b) in o.data().chunks_exact(4).zip(img.data().chunks_exact(4)) {
            assert!(a.iter().all(|v| v.is_finite()), "non-finite for {c} {t}: {a:?}");
            assert_eq!(a[3], b[3]);
            if lum([b[0], b[1], b[2], 0.0]) <= 0.0 {
                assert_eq!(a, b, "Y <= 0 pixels stay unchanged");
            }
        }
    }
}

// ---------------------------------------------------------------- transform mapping

/// Image whose R is the source pixel-centre x and G the centre y, both normalised to 0..1.
fn coord_norm(w: u32, h: u32) -> ImageBuf {
    solid(w, h, |x, y| [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32, 0.0])
}

fn cases() -> Vec<TransformParams> {
    let d = TransformParams::default();
    let mut v = vec![
        d,
        TransformParams { flip_h: true, ..d },
        TransformParams { flip_v: true, ..d },
        TransformParams { flip_h: true, flip_v: true, quarter_turns: 1, ..d },
        TransformParams { quarter_turns: 1, ..d },
        TransformParams { quarter_turns: 2, ..d },
        TransformParams { quarter_turns: 3, ..d },
        TransformParams { left: 0.1, top: 0.2, right: 0.85, bottom: 0.7, ..d },
        TransformParams { angle_deg: 7.0, ..d },
        TransformParams { angle_deg: -12.5, left: 0.1, right: 0.9, top: 0.05, bottom: 0.8, ..d },
        TransformParams { angle_deg: 20.0, quarter_turns: 1, flip_h: true, left: 0.15, top: 0.1, right: 0.9, bottom: 0.95, ..d },
    ];
    for q in 0..4u8 {
        for fh in [false, true] {
            v.push(TransformParams {
                angle_deg: 9.0,
                quarter_turns: q,
                flip_h: fh,
                flip_v: !fh,
                left: 0.07,
                top: 0.12,
                right: 0.92,
                bottom: 0.88,
            });
        }
    }
    // 90 / 270 turn + rotation + crop explicitly
    v.push(TransformParams { quarter_turns: 1, angle_deg: -6.0, left: 0.2, top: 0.0, right: 1.0, bottom: 0.6, ..d });
    v.push(TransformParams { quarter_turns: 3, angle_deg: 15.0, left: 0.05, top: 0.3, right: 0.75, bottom: 1.0, ..d });
    v
}

#[test]
fn transform_mapping_matches_pixels() {
    for (w, h) in [(240u32, 160u32), (97, 211), (64, 64)] {
        let src = coord_norm(w, h);
        for (k, p) in cases().iter().enumerate() {
            let out = transform(&src, p);
            let (ow, oh) = (out.width(), out.height());
            assert_eq!((ow, oh), transform_size(w, h, p));
            let mut worst = 0.0f32;
            for iy in 0..8 {
                for ix in 0..8 {
                    let (px, py) = ((ix * (ow - 1)) / 7, (iy * (oh - 1)) / 7);
                    let (u, v) = ((px as f32 + 0.5) / ow as f32, (py as f32 + 0.5) / oh as f32);
                    let (mx, my) = transform_output_to_input(w, h, p, u, v);
                    let s = out.pixel(px, py);
                    let dx = (s[0] - mx).abs() * w as f32;
                    let dy = (s[1] - my).abs() * h as f32;
                    worst = worst.max(dx).max(dy);
                }
            }
            assert!(worst <= 1.5, "case {k} {w}x{h}: worst error {worst} source pixels");
        }
    }
}

#[test]
fn transform_mappings_are_inverse() {
    for (w, h) in [(240u32, 160u32), (97, 211)] {
        for p in cases() {
            for &(x, y) in &[(0.0, 0.0), (0.5, 0.5), (1.0, 1.0), (0.13, 0.77), (-0.4, 0.3), (1.6, -0.2), (0.9, 1.4)] {
                let (a, b) = transform_output_to_input(w, h, &p, x, y);
                let (x2, y2) = transform_input_to_output(w, h, &p, a, b);
                assert!((x - x2).abs() < 1e-4 && (y - y2).abs() < 1e-4, "{p:?} out->in->out ({x},{y}) -> ({x2},{y2})");
                let (c, d) = transform_input_to_output(w, h, &p, x, y);
                let (x3, y3) = transform_output_to_input(w, h, &p, c, d);
                assert!((x - x3).abs() < 1e-4 && (y - y3).abs() < 1e-4, "{p:?} in->out->in ({x},{y}) -> ({x3},{y3})");
            }
        }
    }
}

#[test]
fn transform_mapping_identity_maps_to_itself() {
    let p = TransformParams::default();
    for &(x, y) in &[(0.0, 0.0), (0.3, 0.8), (1.0, 1.0), (-2.0, 5.5)] {
        assert_eq!(transform_output_to_input(123, 77, &p, x, y), (x, y));
        assert_eq!(transform_input_to_output(123, 77, &p, x, y), (x, y));
    }
}
