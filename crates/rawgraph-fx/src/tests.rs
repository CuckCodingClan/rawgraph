//! Unit tests for every effect.

use rawgraph_core::color::{rec709_to_acescg, AP1_LUMA};
use rawgraph_core::{ops, ImageBuf, MaskBuf};

use crate::geometry::inscribed_scale;
use crate::util::hue_chroma;
use crate::*;

fn lum(px: [f32; 4]) -> f32 {
    AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2]
}

/// 4x2 image with zero, negative, bright and extreme values and distinct alphas.
fn hdr_img() -> ImageBuf {
    let px: [[f32; 3]; 8] = [
        [0.0, 0.0, 0.0],
        [-0.1, 0.2, 0.3],
        [16.0, 16.0, 16.0],
        [1.0e6, 5.0e5, 2.0e5],
        [1.0, 0.5, 0.2],
        [0.001, 0.002, 0.0005],
        [0.18, 0.18, 0.18],
        [-1.0, -1.0, -1.0],
    ];
    let mut data = Vec::new();
    for (i, p) in px.iter().enumerate() {
        data.extend_from_slice(&[p[0], p[1], p[2], 0.1 * (i + 1) as f32]);
    }
    ImageBuf::from_rgba(4, 2, data).unwrap()
}

/// Deterministic pseudo-random image with values in 0..2 and varying alpha.
fn noise_img(w: u32, h: u32) -> ImageBuf {
    let mut s = 12345u32;
    let mut next = move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (s >> 8) as f32 / (1u32 << 24) as f32
    };
    let mut data = Vec::new();
    for _ in 0..w * h {
        data.extend_from_slice(&[next() * 2.0, next() * 2.0, next() * 2.0, 0.25 + 0.5 * next()]);
    }
    ImageBuf::from_rgba(w, h, data).unwrap()
}

fn flat(w: u32, h: u32, rgb: [f32; 3]) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.set_pixel(x, y, [rgb[0], rgb[1], rgb[2], 1.0]);
        }
    }
    img
}

/// Image whose pixel value encodes its coordinates: r = x, g = y, b = x + y, alpha 1.
fn coord_img(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.set_pixel(x, y, [x as f32, y as f32, (x + y) as f32, 1.0]);
        }
    }
    img
}

fn all_finite(img: &ImageBuf) -> bool {
    img.data().iter().all(|v| v.is_finite())
}

fn alpha_same(a: &ImageBuf, b: &ImageBuf) -> bool {
    a.data().chunks_exact(4).zip(b.data().chunks_exact(4)).all(|(p, q)| p[3] == q[3])
}

fn mask_from(w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> MaskBuf {
    let mut m = MaskBuf::new(w, h, 0.0);
    for y in 0..h {
        for x in 0..w {
            m.data[(y * w + x) as usize] = f(x, y);
        }
    }
    m
}

fn close(a: f32, b: f32, tol: f32) -> bool {
    (a - b).abs() <= tol * (1.0 + b.abs())
}

const NEUTRAL_BANDS: [[f32; 3]; 6] = [[0.0; 3]; 6];

// ---------------------------------------------------------------- tone_curve

#[test]
fn tone_curve_identity_is_bit_exact() {
    let src = noise_img(16, 16);
    let mut img = src.clone();
    tone_curve(&mut img, 0.0, 0.0, 0.0, 0.0);
    assert_eq!(img, src);
}

#[test]
fn tone_curve_band_centres_move_about_one_and_a_half_stops() {
    // Perceptual centres t = 0.125 / 0.375 / 0.625 / 0.875 as grey luminance values.
    let centres = [0.125f32, 0.375, 0.625, 0.875].map(|t| 0.18 * t / (1.0 - t));
    for (band, &y) in centres.iter().enumerate() {
        for sign in [1.0f32, -1.0] {
            let mut p = [0.0f32; 4];
            p[band] = sign;
            let mut img = flat(1, 1, [y; 3]);
            tone_curve(&mut img, p[0], p[1], p[2], p[3]);
            let stops = (img.pixel(0, 0)[0] / y).log2();
            assert!((stops - sign * 1.5).abs() < 0.2, "band {band} sign {sign}: {stops} stops");
        }
    }
}

#[test]
fn tone_curve_preserves_hue_and_leaves_nonpositive_luminance() {
    let mut img = ImageBuf::new(2, 1);
    img.set_pixel(0, 0, [0.4, 0.2, 0.1, 0.5]);
    img.set_pixel(1, 0, [-0.5, -0.2, -0.1, 0.5]);
    let src = img.clone();
    tone_curve(&mut img, 0.3, 0.8, -0.4, 0.6);
    let (a, b) = (src.pixel(0, 0), img.pixel(0, 0));
    let g = b[0] / a[0];
    assert!(close(b[1], a[1] * g, 1e-5) && close(b[2], a[2] * g, 1e-5));
    assert_ne!(g, 1.0);
    assert_eq!(img.pixel(1, 0), src.pixel(1, 0));
}

#[test]
fn tone_curve_is_monotonic_over_parameter_grid() {
    // Grey ramp, 20 stops in 4000 steps.
    let n = 4000usize;
    let mut base = ImageBuf::new(n as u32, 1);
    for i in 0..n {
        let y = (2.0f32).powf(-14.0 + 24.0 * i as f32 / (n - 1) as f32);
        base.set_pixel(i as u32, 0, [y, y, y, 1.0]);
    }
    let vals = [-1.0f32, -0.75, -0.5, -0.25, 0.0, 0.25, 0.5, 0.75, 1.0];
    for &a in &vals {
        for &b in &vals {
            for &c in &vals {
                for &d in &vals {
                    let mut img = base.clone();
                    tone_curve(&mut img, a, b, c, d);
                    let mut prev = 0.0f32;
                    for i in 0..n {
                        let y = img.pixel(i as u32, 0)[1];
                        assert!(y >= prev, "not monotonic at {i} for {a} {b} {c} {d}: {y} < {prev}");
                        prev = y;
                    }
                }
            }
        }
    }
}

#[test]
fn tone_curve_analytic_monotonic_bound_holds_at_corners() {
    // f'(Y) >= 0 iff 1 + ln2 t(1-t) g'(t) >= 0; g' is linear in the parameters, so the
    // corners of the parameter box are the worst cases.
    for bits in 0..16u32 {
        let p: [f32; 4] = std::array::from_fn(|i| if bits >> i & 1 == 1 { 1.0 } else { -1.0 });
        let lut = crate::tone::tone_lut(p);
        // Y' = Y * lut(t): check d log2(Y') / d log2(Y) >= 0 on the table.
        let mut prev = f32::NEG_INFINITY;
        for (i, g) in lut.iter().enumerate().take(lut.len() - 1).skip(1) {
            let t = i as f32 / (lut.len() - 1) as f32;
            let y = 0.18 * t / (1.0 - t);
            let out = y * g;
            assert!(out >= prev, "corner {p:?} at t={t}");
            prev = out;
        }
    }
}

// ---------------------------------------------------------------- black_white

#[test]
fn black_white_uses_weights_as_given() {
    let mut img = ImageBuf::new(1, 1);
    img.set_pixel(0, 0, [1.0, 2.0, 3.0, 0.7]);
    black_white(&mut img, [0.5, 0.25, 0.1]);
    assert_eq!(img.pixel(0, 0), [1.3, 1.3, 1.3, 0.7]);
    let mut img = hdr_img();
    let src = img.clone();
    black_white(&mut img, AP1_LUMA);
    assert!(all_finite(&img) && alpha_same(&img, &src));
    let p = img.pixel(3, 0);
    assert!(p[0] > 1.0e5 && p[0] == p[1] && p[1] == p[2]);
}

// ---------------------------------------------------------------- hue_saturation

fn srgb_primary(i: usize) -> [f32; 3] {
    // red, yellow, green, cyan, blue, magenta in linear Rec.709, converted to ACEScg.
    let rgb = [[1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0], [0.0, 1.0, 1.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0]][i];
    let m = rec709_to_acescg();
    std::array::from_fn(|r| (0..3).map(|c| m[r][c] * rgb[c]).sum::<f64>() as f32)
}

#[test]
fn hue_saturation_primaries_fall_into_their_named_bands() {
    for i in 0..6 {
        let p = srgb_primary(i);
        let (hue, chroma) = hue_chroma(p[0], p[1], p[2]);
        assert!(chroma > 0.1);
        let d = (hue - 60.0 * i as f32 + 540.0).rem_euclid(360.0) - 180.0;
        assert!(d.abs() < 30.0, "primary {i} has hue {hue}");
        // And the band is really the one that acts: raise only band i's saturation.
        let mut bands = NEUTRAL_BANDS;
        bands[i][1] = 1.0;
        let mut img = ImageBuf::new(1, 1);
        img.set_pixel(0, 0, [p[0], p[1], p[2], 1.0]);
        hue_saturation(&mut img, &bands);
        let q = img.pixel(0, 0);
        let (_, c2) = hue_chroma(q[0], q[1], q[2]);
        assert!(c2 > chroma * 1.5, "primary {i}: chroma {chroma} -> {c2}");
    }
}

#[test]
fn hue_saturation_identity_and_neutrals() {
    let src = noise_img(8, 8);
    let mut img = src.clone();
    hue_saturation(&mut img, &NEUTRAL_BANDS);
    assert_eq!(img, src);

    let bands = [[0.7, -0.5, 0.9], [-1.0, 1.0, -0.8], [0.3, 0.2, 1.0], [1.0, -1.0, 0.5], [-0.4, 0.9, -1.0], [0.6, 0.1, 0.2]];
    let mut grey = ImageBuf::new(4, 1);
    for (x, v) in [0.0f32, 0.18, 16.0, 1.0e6].iter().enumerate() {
        grey.set_pixel(x as u32, 0, [*v, *v, *v, 0.5]);
    }
    let src = grey.clone();
    hue_saturation(&mut grey, &bands);
    assert_eq!(grey, src);
}

#[test]
fn hue_saturation_band_weights_sum_to_one() {
    // +1 saturation in every band doubles chroma of every hue, so the weights sum to 1
    // continuously over the circle; luminance is kept.
    let bands = [[0.0, 1.0, 0.0]; 6];
    for deg in (0..360).step_by(7) {
        let (s, c) = (deg as f32).to_radians().sin_cos();
        // Build an RGB with that hue: a = cos, b = sin (scaled), around grey 0.3.
        let a = 0.2 * c;
        let b = 0.2 * s;
        let r = 0.3 + a * 2.0 / 3.0;
        let g = 0.3 - a / 3.0 + b * 0.577_350_3;
        let bl = 0.3 - a / 3.0 - b * 0.577_350_3;
        let mut img = ImageBuf::new(1, 1);
        img.set_pixel(0, 0, [r, g, bl, 1.0]);
        let (_, c0) = hue_chroma(r, g, bl);
        hue_saturation(&mut img, &bands);
        let q = img.pixel(0, 0);
        let (_, c1) = hue_chroma(q[0], q[1], q[2]);
        assert!(close(c1, 2.0 * c0, 2e-3), "hue {deg}: {c0} -> {c1}");
        assert!(close(lum(q), lum([r, g, bl, 1.0]), 1e-4));
    }
}

#[test]
fn hue_saturation_desaturate_shift_and_lightness() {
    let p = srgb_primary(0);
    // Saturation -1 on all bands: grey with the same luminance.
    let mut img = ImageBuf::new(1, 1);
    img.set_pixel(0, 0, [p[0], p[1], p[2], 1.0]);
    let y = lum(img.pixel(0, 0));
    hue_saturation(&mut img, &[[0.0, -1.0, 0.0]; 6]);
    let q = img.pixel(0, 0);
    assert!(close(q[0], y, 1e-4) && close(q[1], y, 1e-4) && close(q[2], y, 1e-4));

    // Hue +1 on red moves red by about +30 degrees (towards yellow), luminance kept.
    let mut img = ImageBuf::new(1, 1);
    img.set_pixel(0, 0, [p[0], p[1], p[2], 1.0]);
    let (h0, c0) = hue_chroma(p[0], p[1], p[2]);
    let mut bands = NEUTRAL_BANDS;
    bands[0][0] = 1.0;
    hue_saturation(&mut img, &bands);
    let q = img.pixel(0, 0);
    let (h1, c1) = hue_chroma(q[0], q[1], q[2]);
    assert!((h1 - h0 - 30.0).abs() < 1.5, "{h0} -> {h1}");
    assert!(close(c1, c0, 1e-3));
    assert!(close(lum(q), y, 1e-4));

    // Lightness +1: about one stop for a saturated colour.
    let mut img = ImageBuf::new(1, 1);
    img.set_pixel(0, 0, [p[0], p[1], p[2], 1.0]);
    hue_saturation(&mut img, &[[0.0, 0.0, 1.0]; 6]);
    let gain = img.pixel(0, 0)[0] / p[0];
    assert!(gain > 1.8 && gain <= 2.0 + 1e-4, "{gain}");
}

#[test]
fn hue_saturation_is_continuous_around_the_circle() {
    let bands = [[0.4, 0.6, 0.3], [-0.9, 0.2, -0.6], [0.5, -0.8, 0.7], [0.1, 0.9, -0.2], [-0.3, -0.4, 0.5], [0.8, 0.3, -0.9]];
    let mut prev: Option<[f32; 3]> = None;
    for deg in 0..=720 {
        let (s, c) = (deg as f32 * 0.5).to_radians().sin_cos();
        let (a, b) = (0.2 * c, 0.2 * s);
        let px = [0.3 + a * 2.0 / 3.0, 0.3 - a / 3.0 + b * 0.577_350_3, 0.3 - a / 3.0 - b * 0.577_350_3];
        let mut img = ImageBuf::new(1, 1);
        img.set_pixel(0, 0, [px[0], px[1], px[2], 1.0]);
        hue_saturation(&mut img, &bands);
        let q = img.pixel(0, 0);
        if let Some(p) = prev {
            for k in 0..3 {
                assert!((q[k] - p[k]).abs() < 0.02, "jump at {deg}: {p:?} -> {q:?}");
            }
        }
        prev = Some([q[0], q[1], q[2]]);
    }
}

/// Exact reference of `hue_saturation` (per-pixel `atan2` / `cos` / `sin_cos`).
fn ref_hue_saturation(img: &mut ImageBuf, bands: &[[f32; 3]; 6]) {
    use rawgraph_core::color::AP1_LUMA as L;
    let b = bands.map(|band| band.map(|v| v.clamp(-1.0, 1.0)));
    if b == [[0.0; 3]; 6] {
        return;
    }
    let tau = std::f32::consts::TAU;
    use rayon::prelude::*;
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let (r, g, bl) = (px[0], px[1], px[2]);
        let a = r - 0.5 * (g + bl);
        let bb = 0.866_025_4 * (g - bl);
        let c = a.hypot(bb);
        if c <= 0.0 {
            return;
        }
        let x = bb.atan2(a).rem_euclid(tau) * (6.0 / tau);
        let i = (x as usize).min(5);
        let f = (x - i as f32).clamp(0.0, 1.0);
        let w1 = 0.5 * (1.0 - (std::f32::consts::PI * f).cos());
        let w0 = 1.0 - w1;
        let j = (i + 1) % 6;
        let mix = |k: usize| w0 * b[i][k] + w1 * b[j][k];
        let (dh, ds, dl) = (mix(0), mix(1), mix(2));
        let zm = |a: f32, b: f32| [a * (2.0 / 3.0), -a / 3.0 + b / 3.0f32.sqrt(), -a / 3.0 - b / 3.0f32.sqrt()];
        if dh != 0.0 || ds != 0.0 {
            let (s, co) = (dh * 30.0f32.to_radians()).sin_cos();
            let (na, nb) = ((a * co - bb * s) * (1.0 + ds), (a * s + bb * co) * (1.0 + ds));
            let (zo, zn) = (zm(a, bb), zm(na, nb));
            let mut d = [zn[0] - zo[0], zn[1] - zo[1], zn[2] - zo[2]];
            let dy = L[0] * d[0] + L[1] * d[1] + L[2] * d[2];
            d.iter_mut().for_each(|v| *v -= dy);
            px[0] = r + d[0];
            px[1] = g + d[1];
            px[2] = bl + d[2];
        }
        if dl != 0.0 {
            let y = (L[0] * r + L[1] * g + L[2] * bl).abs();
            let rel = c / (y + c);
            let t = (rel / 0.4).clamp(0.0, 1.0);
            let gain = (dl * t * t * (3.0 - 2.0 * t)).exp2();
            px[0] *= gain;
            px[1] *= gain;
            px[2] *= gain;
        }
    });
}

/// Exact reference of `hue_mask` (angle by `atan2` for every pixel). `old_gate` selects the
/// previous gate `(max - min) / max`, kept only for the timing comparison.
fn ref_hue_mask(img: &ImageBuf, hue_deg: f32, width_deg: f32, feather: f32, min_sat: f32, old_gate: bool) -> MaskBuf {
    let sm = |e0: f32, e1: f32, x: f32| {
        if e1 <= e0 {
            return (x >= e0) as u8 as f32;
        }
        let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    let mut m = MaskBuf::new(img.width(), img.height(), 0.0);
    let (centre, half) = (hue_deg.rem_euclid(360.0), width_deg * 0.5);
    use rayon::prelude::*;
    m.data.par_iter_mut().zip(img.data().par_chunks_exact(4)).for_each(|(o, px)| {
        let (r, g, b) = (px[0], px[1], px[2]);
        let (hue, c) = hue_chroma(r, g, b);
        if c <= 0.0 {
            return;
        }
        let gate = if old_gate {
            let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
            if mx <= 0.0 || mx <= mn {
                return;
            }
            sm(min_sat, min_sat + 0.1, (mx - mn) / mx)
        } else {
            crate::masks::hue_gate(c, lum([r, g, b, 1.0]), min_sat)
        };
        let mut d = (hue - centre).abs();
        if d > 180.0 {
            d = 360.0 - d;
        }
        let v = if feather <= 0.0 { (d <= half) as u8 as f32 } else { 1.0 - sm(half, half + feather, d) };
        *o = v * gate;
    });
    m
}

#[test]
fn hue_saturation_matches_exact_reference() {
    let cases = [
        [[0.4, 0.6, 0.3], [-0.9, 0.2, -0.6], [0.5, -0.8, 0.7], [0.1, 0.9, -0.2], [-0.3, -0.4, 0.5], [0.8, 0.3, -0.9]],
        [[0.0, 1.0, 0.0]; 6],
        [[1.0, 0.0, 0.0], [0.0; 3], [0.0; 3], [-1.0, 0.0, 0.0], [0.0; 3], [0.0, 0.0, 1.0]],
        [[0.0, 0.0, 0.5]; 6],
    ];
    for bands in &cases {
        for src in [noise_img(97, 61), hdr_img()] {
            let (mut a, mut b) = (src.clone(), src.clone());
            hue_saturation(&mut a, bands);
            ref_hue_saturation(&mut b, bands);
            for (p, q) in a.data().iter().zip(b.data()) {
                assert!((p - q).abs() <= 1e-4 * q.abs().max(1e-3) + 1e-6, "{p} vs {q} for {bands:?}");
            }
        }
    }
    // Pixels on the sector boundaries (exact 0, 60, ... degrees) agree as well.
    let mut edge = ImageBuf::new(6, 1);
    for (i, p) in [[1.0f32, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0], [0.0, 1.0, 1.0], [0.0, 0.0, 1.0], [1.0, 0.0, 1.0]].iter().enumerate() {
        edge.set_pixel(i as u32, 0, [p[0], p[1], p[2], 1.0]);
    }
    let (mut a, mut b) = (edge.clone(), edge.clone());
    hue_saturation(&mut a, &cases[0]);
    ref_hue_saturation(&mut b, &cases[0]);
    for (p, q) in a.data().iter().zip(b.data()) {
        assert!((p - q).abs() <= 1e-4 * q.abs().max(1e-3) + 1e-6, "{p} vs {q}");
    }
}

#[test]
fn hue_mask_matches_exact_reference_and_ignores_dark_noise() {
    let img = noise_img(113, 71);
    for (h, w, f, s) in [(30.0f32, 60.0f32, 20.0f32, 0.1f32), (350.0, 40.0, 0.0, 0.0), (200.0, 120.0, 90.0, 0.2), (10.0, 400.0, 10.0, 0.0), (0.0, 0.0, 30.0, 0.05)] {
        let a = hue_mask(&img, h, w, f, s);
        let b = ref_hue_mask(&img, h, w, f, s, false);
        for (p, q) in a.data.iter().zip(&b.data) {
            assert!((p - q).abs() <= 1e-4, "{p} vs {q} for {h} {w} {f} {s}");
        }
    }
    // Dark, nearly neutral noise (random hue, tiny chroma) is never selected.
    let mut dark = flat(64, 64, [0.02; 3]);
    let mut seed = 7u32;
    for px in dark.data_mut().chunks_exact_mut(4) {
        for c in px.iter_mut().take(3) {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *c += ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.008;
        }
    }
    let m = hue_mask(&dark, 180.0, 360.0, 0.0, 0.15);
    assert!(m.data.iter().all(|v| *v == 0.0), "dark noise selected");
}

// ---------------------------------------------------------------- vignette

#[test]
fn vignette_identity_centre_and_corners() {
    let src = noise_img(9, 7);
    let mut img = src.clone();
    vignette(&mut img, 0.0, 0.5, 0.5);
    assert_eq!(img, src);

    let mut img = flat(101, 101, [1.0, 0.5, 0.25]);
    vignette(&mut img, -1.0, 0.5, 0.5);
    assert_eq!(img.pixel(50, 50), [1.0, 0.5, 0.25, 1.0]);
    assert_eq!(img.pixel(0, 0)[0], 0.25); // -2 stops at the corner
    assert_eq!(img.pixel(100, 100)[1], 0.125);
    // Brighter outwards for a positive amount, monotonic along the diagonal.
    let mut img = flat(101, 101, [1.0; 3]);
    vignette(&mut img, 0.5, 0.3, 0.4);
    let mut prev = 1.0;
    for i in 50..101 {
        let v = img.pixel(i, i)[0];
        assert!(v >= prev);
        prev = v;
    }
    assert!(prev > 1.9);
}

#[test]
fn vignette_hdr_and_alpha() {
    let src = hdr_img();
    let mut img = src.clone();
    vignette(&mut img, -0.7, 0.4, 0.6);
    assert!(all_finite(&img) && alpha_same(&img, &src));
    let mut img = flat(8, 8, [16.0; 3]);
    vignette(&mut img, 1.0, 0.0, 1.0);
    assert!(img.pixel(7, 7)[0] > 16.0);
    let mut empty = ImageBuf::new(0, 0);
    vignette(&mut empty, 1.0, 0.5, 0.5);
}

// ---------------------------------------------------------------- masked ops

#[test]
fn masked_ops_all_zero_is_identity_all_one_matches_core() {
    let src = noise_img(12, 9);
    let zero = MaskBuf::new(12, 9, 0.0);
    let one = MaskBuf::new(12, 9, 1.0);

    let mut img = src.clone();
    gain_masked(&mut img, 1.3, &zero);
    contrast_masked(&mut img, 0.8, 0.18, &zero);
    saturation_masked(&mut img, 1.9, &zero);
    assert_eq!(img, src);

    let mut a = src.clone();
    gain_masked(&mut a, 1.3, &one);
    let mut b = src.clone();
    ops::apply_gain(&mut b, 1.3f32.exp2());
    assert_eq!(a, b);

    let mut a = src.clone();
    contrast_masked(&mut a, 0.8, 0.18, &one);
    let mut b = src.clone();
    ops::contrast(&mut b, 0.8, 0.18);
    for (p, q) in a.data().iter().zip(b.data()) {
        assert!(close(*p, *q, 1e-5));
    }

    let mut a = src.clone();
    saturation_masked(&mut a, 1.9, &one);
    let mut b = src.clone();
    ops::saturation(&mut b, 1.9);
    for (p, q) in a.data().iter().zip(b.data()) {
        assert!(close(*p, *q, 1e-5));
    }
}

#[test]
fn masked_ops_modulate_the_parameter() {
    let half = MaskBuf::new(1, 1, 0.5);
    // gain: 2^(2 * 0.5) = 2, not the average of 1 and 4.
    let mut img = flat(1, 1, [0.5; 3]);
    gain_masked(&mut img, 2.0, &half);
    assert!(close(img.pixel(0, 0)[0], 1.0, 1e-6));
    // saturation: 1 + (3 - 1) * 0.5 = 2.
    let mut img = ImageBuf::new(1, 1);
    img.set_pixel(0, 0, [0.4, 0.2, 0.1, 1.0]);
    let mut reference = img.clone();
    saturation_masked(&mut img, 3.0, &half);
    ops::saturation(&mut reference, 2.0);
    assert!(close(img.pixel(0, 0)[0], reference.pixel(0, 0)[0], 1e-6));
    // contrast: amount 2 * 0.5 = 1.
    let mut img = flat(1, 1, [0.36; 3]);
    let mut reference = img.clone();
    contrast_masked(&mut img, 2.0, 0.18, &half);
    ops::contrast(&mut reference, 1.0, 0.18);
    assert!(close(img.pixel(0, 0)[0], reference.pixel(0, 0)[0], 1e-5));
}

#[test]
fn masked_ops_hdr_alpha_and_mismatched_masks() {
    let masks = [
        MaskBuf::new(1, 1, 1.0),
        MaskBuf::new(3, 5, 0.7),
        mask_from(2, 2, |x, y| (x + y) as f32 * 0.5),
        MaskBuf::new(0, 0, 1.0),
        MaskBuf { width: 4, height: 4, data: vec![1.0; 3] },
        MaskBuf::new(9, 9, f32::NAN),
        MaskBuf::new(4, 2, 7.0),
        MaskBuf::new(4, 2, -3.0),
    ];
    for m in &masks {
        let src = hdr_img();
        let mut img = src.clone();
        gain_masked(&mut img, 1.5, m);
        contrast_masked(&mut img, 0.7, 0.18, m);
        saturation_masked(&mut img, 1.6, m);
        assert!(all_finite(&img), "mask {}x{}", m.width, m.height);
        assert!(alpha_same(&img, &src));
    }
    let mut img = flat(2, 2, [16.0; 3]);
    gain_masked(&mut img, 1.0, &MaskBuf::new(2, 2, 1.0));
    assert!(close(img.pixel(1, 1)[0], 32.0, 1e-6));
    // A smaller mask is sampled bilinearly: left-to-right ramp on a wider image.
    let ramp = mask_from(2, 1, |x, _| x as f32);
    let mut img = flat(8, 1, [1.0; 3]);
    gain_masked(&mut img, 1.0, &ramp);
    let v: Vec<f32> = (0..8).map(|x| img.pixel(x, 0)[0]).collect();
    assert!(v.windows(2).all(|w| w[1] >= w[0]) && v[0] < 1.3 && v[7] > 1.7, "{v:?}");
}

// ---------------------------------------------------------------- sharpen

fn step_edge(w: u32, h: u32, dark: f32, bright: f32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = if x < w / 2 { dark } else { bright };
            img.set_pixel(x, y, [v, v * 0.8, v * 0.6, 0.9]);
        }
    }
    img
}

#[test]
fn sharpen_identity_cases() {
    let src = noise_img(20, 12);
    assert_eq!(sharpen(&src, 0.0, 3.0, 0.1, None), src);
    assert_eq!(sharpen(&src, 1.0, 0.0, 0.0, None), src);
    let zero = MaskBuf::new(20, 12, 0.0);
    assert_eq!(sharpen(&src, 1.5, 2.0, 0.0, Some(&zero)), src);
    let flat_img = flat(16, 16, [0.3, 0.2, 0.1]);
    let out = sharpen(&flat_img, 2.0, 2.0, 0.0, None);
    for (p, q) in out.data().iter().zip(flat_img.data()) {
        assert!(close(*p, *q, 1e-5));
    }
}

#[test]
fn sharpen_overshoot_is_monotonic_and_bounded() {
    // The change is a multiplicative gain exp(amount * d'), |d'| <= 0.4 ln: overshoot grows
    // monotonically with the amount (proportionally for small amounts) and never exceeds the bound.
    let src = step_edge(64, 8, 0.5, 1.0);
    let over = |amount: f32| {
        let out = sharpen(&src, amount, 1.5, 0.0, None);
        let bright = (32..64).map(|x| out.pixel(x, 4)[0]).fold(f32::MIN, f32::max) / 1.0 - 1.0;
        let dark = 1.0 - (0..32).map(|x| out.pixel(x, 4)[0]).fold(f32::MAX, f32::min) / 0.5;
        (bright, dark)
    };
    let amounts = [0.1f32, 0.2, 0.5, 1.0, 2.0, 4.0];
    let mut prev = (0.0f32, 0.0f32);
    for &a in &amounts {
        let (b, d) = over(a);
        assert!(b > prev.0 && d > prev.1, "amount {a}: {b} {d} after {prev:?}");
        assert!(b <= (a * 0.4).exp() - 1.0 + 1e-4, "bright bound at {a}: {b}");
        assert!(d <= 1.0 - (-a * 0.4).exp() + 1e-4, "dark bound at {a}: {d}");
        prev = (b, d);
    }
    let (b1, d1) = over(0.1);
    let (b2, d2) = over(0.2);
    assert!((1.9..2.1).contains(&(b2 / b1)) && (1.9..2.1).contains(&(d2 / d1)), "{b1} {b2} / {d1} {d2}");
    // Light and dark halos have a similar size in perceptual terms (log ratio).
    let (b, d) = over(1.0);
    let (lb, ld) = ((1.0 + b).ln(), -(1.0 - d).ln());
    assert!(lb / ld > 0.5 && lb / ld < 2.0, "{lb} {ld}");
    // Hue is kept (RGB ratios) and alpha untouched.
    let out = sharpen(&src, 1.0, 1.5, 0.0, None);
    let p = out.pixel(33, 4);
    assert!(close(p[1] / p[0], 0.8, 1e-5) && close(p[2] / p[0], 0.6, 1e-5));
    assert!(alpha_same(&out, &src));
}

#[test]
fn sharpen_fractional_radii_work_smoothly() {
    let src = step_edge(64, 8, 0.5, 1.0);
    let at = |r: f32| sharpen(&src, 1.0, r, 0.0, None);
    let diff = |a: &ImageBuf, b: &ImageBuf| a.data().iter().zip(b.data()).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
    let (r03, r07, r075, r15) = (at(0.3), at(0.7), at(0.75), at(1.5));
    for out in [&r03, &r07, &r15] {
        assert!(all_finite(out));
    }
    // Different radii give different results, and neighbouring radii give neighbouring results.
    assert!(diff(&r07, &r15) > 0.02 && diff(&r03, &r07) > 0.005);
    assert!(diff(&r07, &r075) < 0.25 * diff(&r07, &r15), "{} vs {}", diff(&r07, &r075), diff(&r07, &r15));
    // A tiny radius is a very weak effect, never a whole-pixel one.
    assert!(diff(&r03, &src) < diff(&r15, &src));
    // Large radii (box-blur path) are continuous with the Gaussian path.
    let (g, b) = (at(3.9), at(4.1));
    assert!(diff(&g, &b) < 0.1, "{}", diff(&g, &b));
}

#[test]
fn sharpen_threshold_and_mask() {
    let mut noisy = flat(32, 32, [0.2; 3]);
    for y in 0..32 {
        for x in 0..32 {
            let v = 0.2 + if (x + y) % 2 == 0 { 0.004 } else { -0.004 };
            noisy.set_pixel(x, y, [v, v, v, 1.0]);
        }
    }
    let plain = sharpen(&noisy, 2.0, 1.0, 0.0, None);
    let gated = sharpen(&noisy, 2.0, 1.0, 0.1, None);
    let dev = |im: &ImageBuf| im.data().iter().zip(noisy.data()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(dev(&plain) > 0.005);
    assert!(dev(&gated) < dev(&plain) * 0.2, "{} vs {}", dev(&gated), dev(&plain));
    // Strong detail passes the same threshold.
    let edge = step_edge(32, 8, 0.2, 1.0);
    let (e_plain, e_gated) = (sharpen(&edge, 1.0, 1.0, 0.0, None), sharpen(&edge, 1.0, 1.0, 0.2, None));
    assert!(close(e_gated.pixel(16, 4)[0], e_plain.pixel(16, 4)[0], 1e-3));

    // Half mask halves the amount (the gain is exp(amount * m * ...), so log gains halve).
    let src = step_edge(64, 8, 0.5, 1.0);
    let full = sharpen(&src, 1.0, 3.0, 0.0, None);
    let half = sharpen(&src, 1.0, 3.0, 0.0, Some(&MaskBuf::new(64, 8, 0.5)));
    let l_full = (full.pixel(32, 4)[0] / 1.0).ln();
    let l_half = (half.pixel(32, 4)[0] / 1.0).ln();
    assert!(l_full > 0.0 && close(l_half, l_full * 0.5, 1e-3), "{l_full} {l_half}");
}

#[test]
fn sharpen_never_goes_negative_and_survives_hdr() {
    let masks = [None, Some(MaskBuf::new(3, 3, 1.0)), Some(MaskBuf::new(0, 0, 1.0))];
    for m in &masks {
        let src = hdr_img();
        let out = sharpen(&src, 3.0, 2.0, 0.05, m.as_ref());
        assert!(all_finite(&out) && alpha_same(&out, &src));
        for (a, b) in out.data().chunks_exact(4).zip(src.data().chunks_exact(4)) {
            if lum([b[0], b[1], b[2], 1.0]) > 0.0 {
                assert!(lum([a[0], a[1], a[2], 1.0]) >= 0.0);
            } else {
                assert_eq!(a, b);
            }
        }
    }
    let mut hard = flat(32, 4, [1.0e-3; 3]);
    for y in 0..4 {
        for x in 16..32 {
            hard.set_pixel(x, y, [1.0e6, 1.0e6, 1.0e6, 1.0]);
        }
    }
    let out = sharpen(&hard, 4.0, 3.0, 0.0, None);
    assert!(all_finite(&out));
    assert!(out.data().chunks_exact(4).all(|p| p[0] >= 0.0));
    assert!(out.pixel(31, 1)[0] > 1.0e5);
}

// ---------------------------------------------------------------- transform

fn rot_cw(img: &ImageBuf) -> ImageBuf {
    let (w, h) = (img.width(), img.height());
    let mut out = ImageBuf::new(h, w);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(h - 1 - y, x, img.pixel(x, y));
        }
    }
    out
}

fn flip_h(img: &ImageBuf) -> ImageBuf {
    let (w, h) = (img.width(), img.height());
    let mut out = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(w - 1 - x, y, img.pixel(x, y));
        }
    }
    out
}

fn flip_v(img: &ImageBuf) -> ImageBuf {
    let (w, h) = (img.width(), img.height());
    let mut out = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(x, h - 1 - y, img.pixel(x, y));
        }
    }
    out
}

#[test]
fn transform_identity_and_exact_quarter_turns_flips() {
    let src = coord_img(7, 4);
    let ident = TransformParams::default();
    assert!(ident.is_identity());
    assert_eq!(transform(&src, &ident), src);

    let mut expect = src.clone();
    for q in 0..8u8 {
        let p = TransformParams { quarter_turns: q, ..ident };
        assert_eq!(transform(&src, &p), expect, "quarter_turns {q}");
        assert_eq!(transform_size(7, 4, &p), (expect.width(), expect.height()));
        expect = rot_cw(&expect);
    }
    let p = TransformParams { flip_h: true, ..ident };
    assert_eq!(transform(&src, &p), flip_h(&src));
    let p = TransformParams { flip_v: true, ..ident };
    assert_eq!(transform(&src, &p), flip_v(&src));
    // Flips come before the turn.
    let p = TransformParams { flip_h: true, quarter_turns: 1, ..ident };
    assert_eq!(transform(&src, &p), rot_cw(&flip_h(&src)));
    let p = TransformParams { flip_h: true, flip_v: true, quarter_turns: 3, ..ident };
    assert_eq!(transform(&src, &p), rot_cw(&rot_cw(&rot_cw(&flip_v(&flip_h(&src))))));
    assert!(!p.is_identity());
}

#[test]
fn transform_crops_are_exact_and_enforced() {
    let src = coord_img(20, 10);
    let p = TransformParams { left: 0.25, top: 0.2, right: 0.75, bottom: 0.9, ..Default::default() };
    let out = transform(&src, &p);
    assert_eq!((out.width(), out.height()), (10, 7));
    for y in 0..out.height() {
        for x in 0..out.width() {
            assert_eq!(out.pixel(x, y), src.pixel(x + 5, y + 2));
        }
    }
    // Inverted / degenerate crops keep at least one pixel.
    let p = TransformParams { left: 0.8, right: 0.2, top: 1.0, bottom: 0.0, ..Default::default() };
    let (w, h) = transform_size(20, 10, &p);
    assert_eq!((w, h), (1, 1));
    assert_eq!(transform(&src, &p).pixel(0, 0), src.pixel(16, 9));
    // Crop after a quarter turn works on the turned image.
    let p = TransformParams { quarter_turns: 1, left: 0.0, right: 0.5, ..Default::default() };
    assert_eq!(transform(&src, &p).width(), 5);
}

#[test]
fn transform_size_matches_output_for_a_table() {
    let sizes = [(1u32, 1u32), (1, 7), (7, 1), (5, 3), (3, 5), (64, 48), (101, 67), (2, 2), (33, 1)];
    let mut params = vec![TransformParams::default()];
    for angle in [0.0f32, 0.0000001, 0.5, 3.0, -12.5, 45.0, 90.0, 133.0, -179.0, 360.0] {
        for q in [0u8, 1, 2, 3, 5] {
            for (l, t, r, b) in [(0.0, 0.0, 1.0, 1.0), (0.1, 0.2, 0.9, 0.7), (0.5, 0.5, 0.5, 0.5), (0.9, 0.9, 0.1, 0.1), (-1.0, 2.0, 5.0, -3.0)] {
                params.push(TransformParams { left: l, top: t, right: r, bottom: b, angle_deg: angle, quarter_turns: q, flip_h: q == 2, flip_v: q == 3 });
            }
        }
    }
    params.push(TransformParams { angle_deg: f32::NAN, left: f32::NAN, ..Default::default() });
    for &(w, h) in &sizes {
        let img = noise_img(w, h);
        for p in &params {
            let out = transform(&img, p);
            assert_eq!((out.width(), out.height()), transform_size(w, h, p), "{w}x{h} {p:?}");
            assert!(out.width() >= 1 && out.height() >= 1);
            assert!(all_finite(&out));
        }
    }
}

#[test]
fn transform_rotation_keeps_flat_image_flat_without_empty_corners() {
    for (w, h) in [(64u32, 40u32), (40, 64), (50, 50), (101, 33)] {
        let img = flat(w, h, [0.25, 1.5, 16.0]);
        for angle in [-45.0f32, -17.3, -2.0, 0.3, 5.0, 17.0, 30.0, 44.9, 90.0, 120.0] {
            let p = TransformParams { angle_deg: angle, ..Default::default() };
            let out = transform(&img, &p);
            assert!(out.data().chunks_exact(4).all(|px| px == [0.25, 1.5, 16.0, 1.0]), "{w}x{h} @ {angle}");
            // The inscribed rectangle really lies inside the rotated image.
            let s = inscribed_scale(w as f32, h as f32, angle);
            let (ow, oh) = (out.width() as f32, out.height() as f32);
            assert!(ow <= s * w as f32 + 1e-2 && oh <= s * h as f32 + 1e-2);
            let (sn, cs) = angle.to_radians().sin_cos();
            for (cx, cy) in [(1.0f32, 1.0f32), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
                let (px, py) = (cx * ow * 0.5, cy * oh * 0.5);
                let (qx, qy) = (cs * px + sn * py, -sn * px + cs * py);
                assert!(qx.abs() <= w as f32 * 0.5 + 1e-3 && qy.abs() <= h as f32 * 0.5 + 1e-3, "{w}x{h} @ {angle}: corner outside");
            }
        }
    }
}

#[test]
fn transform_rotation_direction_and_quarter_equivalence() {
    // 90 degrees of fine rotation on a square image equals one clockwise quarter turn.
    let src = coord_img(16, 16);
    let a = transform(&src, &TransformParams { angle_deg: 90.0, ..Default::default() });
    let b = transform(&src, &TransformParams { quarter_turns: 1, ..Default::default() });
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    for (p, q) in a.data().iter().zip(b.data()) {
        assert!((p - q).abs() < 1e-2, "{p} vs {q}");
    }
    // Small positive angle turns the content clockwise: the top edge midpoint moves right.
    let mut dot = ImageBuf::new(41, 41);
    dot.set_pixel(20, 5, [10.0, 10.0, 10.0, 1.0]);
    let out = transform(&dot, &TransformParams { angle_deg: 20.0, ..Default::default() });
    let (cx, mut best) = (out.width() as f32 * 0.5, (0.0f32, 0u32, 0u32));
    for y in 0..out.height() {
        for x in 0..out.width() {
            if out.pixel(x, y)[0] > best.0 {
                best = (out.pixel(x, y)[0], x, y);
            }
        }
    }
    assert!(best.1 as f32 > cx, "dot moved to x={} (centre {cx})", best.1);
}

#[test]
fn transform_hdr_safety_and_alpha() {
    let src = hdr_img();
    for p in [
        TransformParams { angle_deg: 13.0, ..Default::default() },
        TransformParams { angle_deg: -30.0, left: 0.1, quarter_turns: 1, ..Default::default() },
    ] {
        let out = transform(&src, &p);
        assert!(all_finite(&out));
    }
    let mut big = flat(30, 20, [1.0e6; 3]);
    big.set_pixel(3, 3, [16.0, 16.0, 16.0, 0.5]);
    let out = transform(&big, &TransformParams { angle_deg: 10.0, ..Default::default() });
    assert!(all_finite(&out) && out.data().iter().any(|v| *v > 1.0e5));
}

// ---------------------------------------------------------------- lens_manual

#[test]
fn lens_identity_and_vignetting() {
    let src = noise_img(10, 6);
    assert_eq!(lens_manual(&src, 0.0, 0.0, 0.0, 0.0), src);

    let img = flat(101, 61, [1.0, 0.5, 0.25]);
    let out = lens_manual(&img, 0.0, 1.0, 0.0, 0.0);
    // Corner: about +1 stop; centre pixel almost unchanged; monotonic outwards.
    let corner = out.pixel(0, 0)[0];
    assert!(corner > 1.9 && corner <= 2.0, "{corner}");
    assert!(out.pixel(50, 30)[0] < 1.001);
    assert!(out.pixel(80, 30)[0] > out.pixel(60, 30)[0]);
    assert_eq!(out.pixel(0, 0)[3], 1.0);
    // Negative value darkens.
    assert!(lens_manual(&img, 0.0, -1.0, 0.0, 0.0).pixel(0, 0)[0] < 0.55);
}

/// Horizontal ramp image: all colour channels hold the x index.
fn ramp_img(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.set_pixel(x, y, [x as f32, x as f32, x as f32, 1.0]);
        }
    }
    img
}

#[test]
fn lens_distortion_follows_the_model_and_leaves_no_empty_border() {
    let (w, h) = (200u32, 120u32);
    let img = ramp_img(w, h);
    let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
    let r2_of = |x: u32, y: u32| {
        let (dx, dy) = (x as f32 + 0.5 - hw, y as f32 + 0.5 - hh);
        (dx * dx + dy * dy) / (hw * hw + hh * hh)
    };
    for d in [0.3f32, 0.15, -0.1, -0.25] {
        let out = lens_manual(&img, d, 0.0, 0.0, 0.0);
        let zoom = if d > 0.0 { 1.0 / (1.0 + d) } else { 1.0 };
        for &(x, y) in &[(120u32, 60u32), (150, 30), (60, 90), (30, 10)] {
            let dx = x as f32 + 0.5 - hw;
            let expect = dx * zoom * (1.0 + d * r2_of(x, y)) + hw - 0.5;
            assert!((out.pixel(x, y)[0] - expect).abs() < 1e-3, "d {d} at {x},{y}");
        }
        // Every border pixel samples strictly inside the image.
        for y in 0..h {
            for x in [0, w - 1] {
                let v = out.pixel(x, y)[0];
                assert!(v > 0.05 && v < (w - 1) as f32 - 0.05, "d {d} border value {v} at {x},{y}");
            }
        }
        for x in 0..w {
            for y in [0, h - 1] {
                let v = out.pixel(x, y)[0];
                assert!(v >= 0.0 && v <= (w - 1) as f32);
            }
        }
    }
}

#[test]
fn lens_chromatic_aberration_scales_channels_radially() {
    let img = ramp_img(100, 40);
    let out = lens_manual(&img, 0.0, 0.0, 10.0, -10.0);
    // Right of centre: red (magnified) shows content from nearer the centre, blue from farther.
    let p = out.pixel(90, 20);
    assert!(p[0] < p[1] && p[1] < p[2], "{p:?}");
    // Left of centre mirrors it.
    let q = out.pixel(9, 20);
    assert!(q[0] > q[1] && q[1] > q[2], "{q:?}");
    // Expected value for red: dx / 1.01.
    let dx = 90.5 - 50.0;
    assert!((p[0] - (dx / 1.01 + 49.5)).abs() < 1e-3);
    assert_eq!(p[1], 90.0);
    // Only red: blue equals green.
    let out = lens_manual(&img, 0.0, 0.0, 5.0, 0.0);
    let p = out.pixel(90, 20);
    assert_eq!(p[2], p[1]);
    assert_ne!(p[0], p[1]);
}

#[test]
fn lens_hdr_and_alpha() {
    let src = hdr_img();
    for params in [(0.2, 0.5, 3.0, -3.0), (-0.2, -1.0, 0.0, 8.0), (1.0, 2.0, -10.0, 10.0), (f32::NAN, 0.0, 0.0, 0.0)] {
        let out = lens_manual(&src, params.0, params.1, params.2, params.3);
        assert!(all_finite(&out), "{params:?}");
        assert_eq!(out.width(), 4);
    }
    let out = lens_manual(&flat(8, 8, [16.0; 3]), 0.0, 1.0, 0.0, 0.0);
    assert!(out.pixel(0, 0)[0] > 16.0);
    let mut a = flat(10, 10, [1.0; 3]);
    a.set_pixel(5, 5, [1.0, 1.0, 1.0, 0.3]);
    assert_eq!(lens_manual(&a, 0.0, 0.7, 0.0, 0.0).pixel(5, 5)[3], 0.3);
}

// ---------------------------------------------------------------- masks

#[test]
fn radial_mask_values() {
    let m = radial_mask(100, 100, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0, false);
    assert_eq!((m.width, m.height), (100, 100));
    assert_eq!(m.get(50, 50), 1.0);
    assert_eq!(m.get(70, 50), 1.0);
    assert_eq!(m.get(80, 50), 0.0);
    assert_eq!(m.get(0, 0), 0.0);
    let inv = radial_mask(100, 100, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0, true);
    assert_eq!(inv.get(50, 50), 0.0);
    assert_eq!(inv.get(0, 0), 1.0);

    // Feather 1: falloff starts at the centre; half way out the value is 0.5.
    let f = radial_mask(100, 100, 0.5, 0.5, 0.25, 0.25, 1.0, 0.0, false);
    assert!(f.get(50, 50) > 0.99);
    assert!((f.get(62, 50) - 0.5).abs() < 1e-3, "{}", f.get(62, 50));
    // Feather 0.5: full inside half the radius, zero at the edge.
    let f = radial_mask(100, 100, 0.5, 0.5, 0.25, 0.25, 0.5, 0.0, false);
    assert_eq!(f.get(55, 50), 1.0);
    assert!(f.get(73, 50) < 0.1 && f.get(65, 50) > 0.8 && f.get(65, 50) < 1.0);
    assert!(f.data.iter().all(|v| (0.0..=1.0).contains(v)));

    // Rotation by 90 degrees turns a wide ellipse into a tall one.
    let wide = radial_mask(100, 100, 0.5, 0.5, 0.4, 0.1, 0.0, 0.0, false);
    let tall = radial_mask(100, 100, 0.5, 0.5, 0.4, 0.1, 0.0, 90.0, false);
    assert_eq!((wide.get(75, 50), wide.get(50, 75)), (1.0, 0.0));
    assert_eq!((tall.get(75, 50), tall.get(50, 75)), (0.0, 1.0));
    // Radii are fractions of each axis.
    let ell = radial_mask(200, 100, 0.5, 0.5, 0.25, 0.25, 0.0, 0.0, false);
    assert_eq!((ell.get(140, 50), ell.get(100, 70)), (1.0, 1.0));
    assert_eq!((ell.get(160, 50), ell.get(100, 80)), (0.0, 0.0));
    // Degenerate cases.
    assert!(radial_mask(10, 10, 0.5, 0.5, 0.0, 0.3, 0.2, 0.0, false).data.iter().all(|v| *v == 0.0));
    assert!(radial_mask(0, 5, 0.5, 0.5, 0.3, 0.3, 0.2, 0.0, false).data.is_empty());
    assert!(radial_mask(10, 10, f32::NAN, 0.5, 0.3, 0.3, f32::NAN, f32::NAN, false).data.iter().all(|v| v.is_finite()));
}

#[test]
fn luminance_mask_values() {
    let ys = [0.0f32, 0.02, 0.18, 5.0, -1.0, 1.0e6];
    let mut img = ImageBuf::new(ys.len() as u32, 1);
    for (i, y) in ys.iter().enumerate() {
        img.set_pixel(i as u32, 0, [*y, *y, *y, 0.5]);
    }
    // t: 0, 0.1, 0.5, 0.965, 0, ~1.
    let hard = luminance_mask(&img, 0.3, 0.7, 0.0, false);
    assert_eq!(hard.data, vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    let inv = luminance_mask(&img, 0.3, 0.7, 0.0, true);
    assert_eq!(inv.data, vec![1.0, 1.0, 0.0, 1.0, 1.0, 1.0]);
    let all = luminance_mask(&img, 0.0, 1.0, 0.0, false);
    assert_eq!(all.data[0], 1.0);
    assert_eq!(all.data[5], 1.0);
    // Soft edge: the edge value at t == low is 0.5.
    let mut e = ImageBuf::new(1, 1);
    let y = 0.18 * 0.3 / 0.7;
    e.set_pixel(0, 0, [y, y, y, 1.0]);
    let soft = luminance_mask(&e, 0.3, 0.8, 0.2, false);
    assert!((soft.data[0] - 0.5).abs() < 1e-3, "{}", soft.data[0]);
    // Fully inside the soft zone's flat part.
    let soft = luminance_mask(&img, 0.3, 0.7, 0.2, false);
    assert_eq!(soft.data[2], 1.0);
    assert!(luminance_mask(&hdr_img(), 0.2, 0.9, 0.3, false).data.iter().all(|v| (0.0..=1.0).contains(v)));
}

#[test]
fn hue_mask_values() {
    let mut img = ImageBuf::new(5, 1);
    let red = srgb_primary(0);
    let blue = srgb_primary(4);
    img.set_pixel(0, 0, [red[0], red[1], red[2], 1.0]);
    img.set_pixel(1, 0, [blue[0], blue[1], blue[2], 1.0]);
    img.set_pixel(2, 0, [0.5, 0.5, 0.5, 1.0]);
    img.set_pixel(3, 0, [0.5, 0.48, 0.47, 1.0]); // barely red, saturation 0.06
    img.set_pixel(4, 0, [-0.2, -0.2, -0.2, 1.0]);
    let (h_red, _) = hue_chroma(red[0], red[1], red[2]);
    let m = hue_mask(&img, 0.0, 40.0, 0.0, 0.0);
    assert!(h_red < 20.0);
    assert_eq!(m.data[0], 1.0);
    assert_eq!(m.data[1], 0.0);
    assert_eq!(m.data[2], 0.0); // neutral never selected
    assert_eq!(m.data[4], 0.0);
    // Wrap-around: 355 degrees +- 10 contains the red primary (hue ~ 4 degrees).
    let m = hue_mask(&img, 355.0, 20.0 + 2.0 * (h_red - 5.0).max(0.0), 0.0, 0.0);
    assert_eq!(m.data[0], 1.0);
    // Saturation gate.
    let m = hue_mask(&img, 0.0, 60.0, 0.0, 0.0);
    assert!(m.data[3] > 0.0 && m.data[3] < 1.0);
    let m = hue_mask(&img, 0.0, 60.0, 0.0, 0.2);
    assert_eq!(m.data[3], 0.0);
    assert!(m.data[0] > 0.99);
    // Feather: outside the hard width the value falls smoothly from 1 to 0.
    let (hh, _) = hue_chroma(red[0], red[1], red[2]);
    let m = hue_mask(&img, hh - 25.0, 20.0, 30.0, 0.0);
    // distance 25, inner 10, feather 30 -> 1 - smoothstep(10, 40, 25) = 0.5
    assert!((m.data[0] - 0.5).abs() < 1e-3, "{}", m.data[0]);
    assert!(hue_mask(&hdr_img(), 30.0, 90.0, 20.0, 0.1).data.iter().all(|v| (0.0..=1.0).contains(v)));
}

#[test]
fn mask_adjust_table() {
    let m = mask_from(5, 1, |x, _| x as f32 * 0.25); // 0, .25, .5, .75, 1
    assert_eq!(mask_adjust(&m, 0.0, 1.0, 1.0, false), m);
    assert_eq!(mask_adjust(&m, 0.0, 1.0, 1.0, true).data, vec![1.0, 0.75, 0.5, 0.25, 0.0]);
    let a = mask_adjust(&m, 0.25, 0.75, 1.0, false);
    assert_eq!(a.data, vec![0.0, 0.0, 0.5, 1.0, 1.0]);
    let g = mask_adjust(&m, 0.0, 1.0, 2.0, false);
    assert!(g.data.iter().zip([0.0, 0.0625, 0.25, 0.5625, 1.0]).all(|(a, b)| (a - b).abs() < 1e-6));
    let g = mask_adjust(&m, 0.0, 1.0, 0.5, false);
    assert!((g.data[2] - 0.5f32.sqrt()).abs() < 1e-6);
    // low >= high: hard threshold at low.
    assert_eq!(mask_adjust(&m, 0.5, 0.5, 1.0, false).data, vec![0.0, 0.0, 1.0, 1.0, 1.0]);
    assert_eq!(mask_adjust(&m, 0.6, 0.2, 1.0, false).data, vec![0.0, 0.0, 0.0, 1.0, 1.0]);
    assert_eq!(mask_adjust(&m, 0.5, 0.5, 1.0, true).data, vec![1.0, 1.0, 0.0, 0.0, 0.0]);
    // Out-of-range input values are clamped first; NaN counts as 0.
    let wild = MaskBuf { width: 4, height: 1, data: vec![-2.0, 3.0, f32::NAN, 0.5] };
    assert_eq!(mask_adjust(&wild, 0.0, 1.0, 1.0, false).data, vec![0.0, 1.0, 0.0, 0.5]);
    assert!(mask_adjust(&wild, 0.2, 0.9, 0.0, false).data.iter().all(|v| v.is_finite()));
}

#[test]
fn mask_combine_table() {
    let a = mask_from(4, 1, |x, _| [0.0, 0.25, 0.5, 1.0][x as usize]);
    let b = mask_from(4, 1, |x, _| [1.0, 0.5, 0.5, 0.25][x as usize]);
    assert_eq!(mask_combine(&a, &b, CombineMode::Multiply).data, vec![0.0, 0.125, 0.25, 0.25]);
    assert_eq!(mask_combine(&a, &b, CombineMode::Add).data, vec![1.0, 0.75, 1.0, 1.0]);
    assert_eq!(mask_combine(&a, &b, CombineMode::Subtract).data, vec![0.0, 0.0, 0.0, 0.75]);
    assert_eq!(mask_combine(&a, &b, CombineMode::Maximum).data, vec![1.0, 0.5, 0.5, 1.0]);
    assert_eq!(mask_combine(&a, &b, CombineMode::Minimum).data, vec![0.0, 0.25, 0.5, 0.25]);
    let order = [CombineMode::Multiply, CombineMode::Add, CombineMode::Subtract, CombineMode::Maximum, CombineMode::Minimum];
    for (i, m) in order.iter().enumerate() {
        assert_eq!(CombineMode::from_index(i), Some(*m));
    }
    assert_eq!(CombineMode::from_index(5), None);
    // b with another size is resampled; result has a's size.
    let small = MaskBuf::new(1, 1, 0.5);
    let r = mask_combine(&a, &small, CombineMode::Multiply);
    assert_eq!((r.width, r.height), (4, 1));
    assert_eq!(r.data, vec![0.0, 0.125, 0.25, 0.5]);
    let r = mask_combine(&small, &a, CombineMode::Maximum);
    assert_eq!((r.width, r.height), (1, 1));
    assert!(r.data[0] >= 0.5);
    // Malformed / empty b does not panic.
    let _ = mask_combine(&a, &MaskBuf::new(0, 0, 1.0), CombineMode::Add);
    let _ = mask_combine(&a, &MaskBuf { width: 3, height: 3, data: vec![1.0] }, CombineMode::Add);
    // Out-of-range values are clamped.
    let wild = MaskBuf { width: 2, height: 1, data: vec![5.0, -5.0] };
    assert_eq!(mask_combine(&wild, &wild, CombineMode::Add).data, vec![1.0, 0.0]);
}

#[test]
fn resize_mask_bilinear() {
    let m = mask_from(2, 2, |x, y| (x + 2 * y) as f32 / 3.0);
    assert_eq!(resize_mask(&m, 2, 2), m);
    let up = resize_mask(&m, 4, 4);
    assert_eq!((up.width, up.height), (4, 4));
    // Corners keep their values (edge clamped), the middle blends.
    assert_eq!(up.get(0, 0), 0.0);
    assert!((up.get(3, 3) - 1.0).abs() < 1e-6);
    assert!((up.get(1, 0) - 0.25 / 3.0).abs() < 1e-6, "{}", up.get(1, 0));
    let down = resize_mask(&up, 1, 1);
    assert!((down.data[0] - 0.5).abs() < 0.05);
    assert!(resize_mask(&m, 0, 3).data.is_empty());
    assert_eq!(resize_mask(&MaskBuf::new(0, 0, 0.0), 3, 2).data, vec![0.0; 6]);
    assert_eq!(resize_mask(&MaskBuf { width: 2, height: 2, data: vec![1.0] }, 3, 2).data, vec![0.0; 6]);
}

// ---------------------------------------------------------------- cross-cutting

#[test]
fn pointwise_effects_survive_hdr_without_clamping() {
    let src = hdr_img();
    let mut img = src.clone();
    tone_curve(&mut img, 0.5, -0.5, 1.0, 0.3);
    assert!(all_finite(&img) && alpha_same(&img, &src));
    assert!(img.pixel(3, 0)[0] > 1.0e5 && img.pixel(2, 0)[0] > 1.0);
    let mut img = src.clone();
    hue_saturation(&mut img, &[[0.5, 1.0, 1.0], [0.2, -1.0, -1.0], [1.0, 0.3, 0.2], [-1.0, 1.0, 1.0], [0.1, 0.4, 0.9], [0.9, -0.4, 0.1]]);
    assert!(all_finite(&img) && alpha_same(&img, &src));
    assert!(img.pixel(3, 0)[0] > 1.0e5);
    assert_eq!(img.pixel(2, 0)[..3], [16.0; 3]);
    let mut img = src.clone();
    black_white(&mut img, [1.0, 1.0, 1.0]);
    assert!(all_finite(&img));
    let m = luminance_mask(&src, 0.1, 0.9, 0.1, false);
    assert!(m.data.iter().all(|v| v.is_finite()));
}

// ---------------------------------------------------------------- timing

fn timed(name: &str, mut f: impl FnMut()) {
    let mut ms: Vec<f64> = (0..10)
        .map(|_| {
            let t = std::time::Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("{name:<28} min {:7.2} ms   median {:7.2} ms", ms[0], (ms[4] + ms[5]) / 2.0);
}

/// `cargo test -p rawgraph-fx --release -- --ignored --nocapture timing`
#[test]
#[ignore]
fn timing_2560x1707() {
    let (w, h) = (2560, 1707);
    let base = noise_img(w, h);
    let mask = radial_mask(w, h, 0.5, 0.5, 0.3, 0.3, 0.5, 0.0, false);
    println!("rayon threads: {}", rayon::current_num_threads());
    timed("tone_curve", || {
        let mut i = base.clone();
        tone_curve(&mut i, 0.5, -0.3, 0.4, -0.6);
    });
    timed("(clone only, for reference)", || {
        let _ = base.clone();
    });
    timed("black_white", || {
        let mut i = base.clone();
        black_white(&mut i, [0.3, 0.6, 0.1]);
    });
    timed("hue_saturation", || {
        let mut i = base.clone();
        hue_saturation(&mut i, &[[0.3, 0.5, 0.2], [0.0, -0.4, 0.1], [0.2, 0.3, 0.0], [0.0; 3], [-0.3, 0.6, -0.2], [0.1, 0.1, 0.1]]);
    });
    timed("hue_saturation (exact ref)", || {
        let mut i = base.clone();
        ref_hue_saturation(&mut i, &[[0.3, 0.5, 0.2], [0.0, -0.4, 0.1], [0.2, 0.3, 0.0], [0.0; 3], [-0.3, 0.6, -0.2], [0.1, 0.1, 0.1]]);
    });
    timed("hue_mask (old: atan2 each)", || {
        let _ = ref_hue_mask(&base, 30.0, 60.0, 20.0, 0.1, true);
    });
    {
        let bands = [[0.3, 0.5, 0.0], [0.0, -0.4, 0.0], [0.2, 0.3, 0.0], [0.0; 3], [-0.3, 0.6, 0.0], [0.1, 0.1, 0.0]];
        let mut buf = base.clone();
        timed("hue_saturation in place", || hue_saturation(&mut buf, &bands));
        let mut buf = base.clone();
        timed("hue_saturation exact in place", || ref_hue_saturation(&mut buf, &bands));
    }
    timed("vignette", || {
        let mut i = base.clone();
        vignette(&mut i, -0.6, 0.5, 0.5);
    });
    timed("gain_masked", || {
        let mut i = base.clone();
        gain_masked(&mut i, 1.0, &mask);
    });
    timed("contrast_masked", || {
        let mut i = base.clone();
        contrast_masked(&mut i, 0.5, 0.18, &mask);
    });
    timed("saturation_masked", || {
        let mut i = base.clone();
        saturation_masked(&mut i, 1.5, &mask);
    });
    timed("(core ops::blur sigma 2)", || {
        let _ = ops::blur(&base, 2.0);
    });
    timed("sharpen (r=2)", || {
        let _ = sharpen(&base, 0.8, 2.0, 0.02, None);
    });
    timed("sharpen (r=8, mask)", || {
        let _ = sharpen(&base, 0.8, 8.0, 0.02, Some(&mask));
    });
    timed("transform rotate 3.5 deg", || {
        let _ = transform(&base, &TransformParams { angle_deg: 3.5, ..Default::default() });
    });
    timed("transform rotate + crop", || {
        let _ = transform(&base, &TransformParams { angle_deg: -7.0, left: 0.1, right: 0.9, ..Default::default() });
    });
    timed("transform crop only", || {
        let _ = transform(&base, &TransformParams { left: 0.1, right: 0.9, top: 0.1, bottom: 0.8, ..Default::default() });
    });
    timed("transform quarter turn", || {
        let _ = transform(&base, &TransformParams { quarter_turns: 1, ..Default::default() });
    });
    timed("local_contrast clarity+texture", || {
        let _ = local_contrast(&base, 0.7, 0.7);
    });
    timed("local_contrast clarity only", || {
        let _ = local_contrast(&base, 0.7, 0.0);
    });
    timed("local_contrast texture only", || {
        let _ = local_contrast(&base, 0.0, 0.7);
    });
    timed("color_wheels (slope+offset+power)", || {
        let mut i = base.clone();
        color_wheels(&mut i, &rawgraph_core::CdlParams { slope: [1.1, 1.0, 0.9], offset: [0.0, 0.0, 0.02], power: [0.9; 3] });
    });
    timed("transform mapping x1e6", || {
        let p = TransformParams { angle_deg: 5.0, left: 0.1, ..Default::default() };
        let mut s = 0.0;
        for k in 0..1_000_000 {
            s += transform_output_to_input(w, h, &p, k as f32 * 1e-6, 0.5).0;
        }
        std::hint::black_box(s);
    });
    timed("lens distortion", || {
        let _ = lens_manual(&base, 0.1, 0.0, 0.0, 0.0);
    });
    timed("lens all (dist+vig+ca)", || {
        let _ = lens_manual(&base, 0.1, 0.5, 2.0, -2.0);
    });
    timed("lens vignetting only", || {
        let _ = lens_manual(&base, 0.0, 0.5, 0.0, 0.0);
    });
    timed("radial_mask", || {
        let _ = radial_mask(w, h, 0.5, 0.5, 0.3, 0.3, 0.5, 20.0, false);
    });
    timed("luminance_mask", || {
        let _ = luminance_mask(&base, 0.3, 0.7, 0.2, false);
    });
    timed("hue_mask", || {
        let _ = hue_mask(&base, 30.0, 60.0, 20.0, 0.1);
    });
    timed("mask_adjust", || {
        let _ = mask_adjust(&mask, 0.1, 0.9, 1.5, false);
    });
    timed("mask_combine", || {
        let _ = mask_combine(&mask, &mask, CombineMode::Multiply);
    });
    {
        let strokes = crate::tests_brush_curve::wavy_strokes(30, 200);
        timed("stroke_mask 30x200 pts", || {
            let _ = stroke_mask(w, h, &strokes, false);
        });
        let one = &strokes[..1];
        timed("stroke_mask 1x200 pts", || {
            let _ = stroke_mask(w, h, one, false);
        });
        let s2 = crate::tests_brush_curve::wavy_strokes(60, 200);
        timed("stroke_mask 60x200 pts", || {
            let _ = stroke_mask(w, h, &s2, false);
        });
        let pts = [[0.0, 0.0], [0.25, 0.1], [0.5, 0.5], [0.75, 0.9], [1.0, 1.0]];
        for (name, ch) in [("point_curve luminance", CurveChannel::Luminance), ("point_curve green", CurveChannel::Green)] {
            let mut buf = base.clone();
            timed(name, || point_curve(&mut buf, &pts, ch));
        }
    }
    timed("resize_mask 1/2", || {
        let _ = resize_mask(&mask, w / 2, h / 2);
    });
}
