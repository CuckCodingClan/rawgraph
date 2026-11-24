//! Wave 8 tests: whites / blacks, vibrance, hue and saturation curves.

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::ImageBuf;

use crate::*;

fn lum(px: [f32; 4]) -> f32 {
    AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2]
}

fn one(c: [f32; 3]) -> ImageBuf {
    let mut i = ImageBuf::new(1, 1);
    i.set_pixel(0, 0, [c[0], c[1], c[2], 0.37]);
    i
}

fn rng(seed: u32) -> impl FnMut() -> f32 {
    let mut s = seed;
    move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (s >> 8) as f32 / (1u32 << 24) as f32
    }
}

fn noise(w: u32, h: u32, seed: u32) -> ImageBuf {
    let mut r = rng(seed);
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            img.set_pixel(x, y, [r() * 3.0 - 0.3, r() * 2.0, r() * 50.0, r()]);
        }
    }
    img
}

fn grey(v: f32) -> [f32; 3] {
    [v, v, v]
}

/// Hue (degrees) of the chroma vector, as documented in `point_curve`.
fn hue_deg(p: [f32; 4]) -> f32 {
    let a = p[0] - 0.5 * (p[1] + p[2]);
    let b = 0.866_025_4 * (p[1] - p[2]);
    b.atan2(a).to_degrees().rem_euclid(360.0)
}

fn sat(p: [f32; 4]) -> f32 {
    let a = p[0] - 0.5 * (p[1] + p[2]);
    let b = 0.866_025_4 * (p[1] - p[2]);
    let c = a.hypot(b);
    if c == 0.0 {
        0.0
    } else {
        c / (lum(p).abs() + c)
    }
}

// ---------------------------------------------------------------- whites / blacks

#[test]
fn whites_blacks_zero_is_bit_exact() {
    let img = noise(13, 9, 1);
    let mut a = img.clone();
    whites_blacks(&mut a, 0.0, 0.0);
    assert_eq!(a.data(), img.data());
    whites_blacks(&mut a, f32::NAN, f32::INFINITY);
    assert_eq!(a.data(), img.data());
}

#[test]
fn whites_affect_bright_not_dark() {
    let mut dark = one(grey(0.002));
    whites_blacks(&mut dark, 1.0, 0.0);
    assert_eq!(dark.pixel(0, 0)[0], 0.002);
    let mut bright = one(grey(4.0));
    whites_blacks(&mut bright, 1.0, 0.0);
    let v = bright.pixel(0, 0);
    assert!((v[0] - 8.0).abs() < 1e-3, "{v:?}");
    assert_eq!(v[3], 0.37);
    let mut bright = one(grey(4.0));
    whites_blacks(&mut bright, -1.0, 0.0);
    assert!((bright.pixel(0, 0)[1] - 2.0).abs() < 1e-3);
}

#[test]
fn blacks_affect_dark_not_bright() {
    let mut bright = one(grey(4.0));
    whites_blacks(&mut bright, 0.0, 1.0);
    assert_eq!(bright.pixel(0, 0)[0], 4.0);
    let mut dark = one(grey(0.0));
    whites_blacks(&mut dark, 0.0, 1.0);
    assert!((dark.pixel(0, 0)[0] - 0.03).abs() < 1e-7);
    let mut dark = one(grey(0.01));
    whites_blacks(&mut dark, 0.0, -1.0);
    // 0.01 - 0.03 * wb < 0 -> held at 0.0 minimum (min(rgb, 0) = 0)
    assert_eq!(dark.pixel(0, 0)[0], 0.0);
}

#[test]
fn whites_blacks_monotonic_and_never_negative() {
    let vals: Vec<f32> = (0..60).map(|i| 0.0005 * 1.25f32.powi(i)).collect();
    let mut prev_w = vec![f32::MIN; vals.len()];
    let mut prev_b = vec![f32::MIN; vals.len()];
    for step in 0..=20 {
        let p = -1.0 + 0.1 * step as f32;
        for (i, &v) in vals.iter().enumerate() {
            let mut a = one([v, v * 0.5, 0.0]);
            whites_blacks(&mut a, p, 0.0);
            let o = a.pixel(0, 0);
            assert!(o[0] >= prev_w[i] - 1e-6 * o[0].abs().max(1.0), "whites not monotonic");
            prev_w[i] = o[0];
            let mut a = one([v, v * 0.5, 0.0]);
            whites_blacks(&mut a, 0.0, p);
            let o = a.pixel(0, 0);
            assert!(o[0] >= prev_b[i] - 1e-6, "blacks not monotonic");
            prev_b[i] = o[0];
            let mut a = one([v, v * 0.5, 0.0]);
            whites_blacks(&mut a, p, p);
            assert!(a.pixel(0, 0)[..3].iter().all(|c| *c >= 0.0));
        }
    }
}

#[test]
fn blacks_negative_keeps_existing_negatives() {
    let mut a = one([-0.2, 0.0, 0.5]);
    whites_blacks(&mut a, 0.0, -1.0);
    let o = a.pixel(0, 0);
    assert!(o[0] >= -0.2 - 1e-7 && o[1] >= 0.0);
}

#[test]
fn whites_blacks_non_finite_parameters_are_zero() {
    let img = noise(5, 5, 2);
    let mut a = img.clone();
    whites_blacks(&mut a, f32::NAN, 0.5);
    let mut b = img.clone();
    whites_blacks(&mut b, 0.0, 0.5);
    assert_eq!(a.data(), b.data());
    let mut c = img.clone();
    whites_blacks(&mut c, 1e30, -1e30);
    assert!(c.data().iter().all(|v| v.is_finite()));
}

// ---------------------------------------------------------------- vibrance

#[test]
fn vibrance_zero_is_bit_exact() {
    let img = noise(11, 7, 3);
    let mut a = img.clone();
    vibrance(&mut a, 0.0);
    assert_eq!(a.data(), img.data());
    vibrance(&mut a, f32::NAN);
    assert_eq!(a.data(), img.data());
}

#[test]
fn vibrance_acts_more_on_weak_saturation() {
    let weak = [0.4, 0.3, 0.3];
    let strong = [0.9, 0.05, 0.0];
    let mut a = one(weak);
    let mut b = one(strong);
    let (sw0, ss0) = (chroma(a.pixel(0, 0)), chroma(b.pixel(0, 0)));
    vibrance(&mut a, 0.8);
    vibrance(&mut b, 0.8);
    let (sw1, ss1) = (chroma(a.pixel(0, 0)), chroma(b.pixel(0, 0)));
    assert!(sw1 / sw0 > 1.5, "weak {sw0} -> {sw1}");
    assert!(ss1 / ss0 < 1.15, "strong {ss0} -> {ss1}");
    assert!(sw1 / sw0 > ss1 / ss0);
}

#[test]
fn vibrance_grey_stays_grey_and_luminance_kept() {
    for amount in [-1.0, -0.4, 0.5, 1.0] {
        let mut g = one(grey(0.37));
        vibrance(&mut g, amount);
        assert_eq!(g.pixel(0, 0)[..3], [0.37, 0.37, 0.37]);
        let mut c = one([0.6, 0.2, 0.1]);
        let y0 = lum(c.pixel(0, 0));
        vibrance(&mut c, amount);
        assert!((lum(c.pixel(0, 0)) - y0).abs() < 1e-6);
        assert_eq!(c.pixel(0, 0)[3], 0.37);
    }
}

#[test]
fn vibrance_minus_one_greys_low_saturation() {
    // s = 0 only for grey in the formula; k = 1 - (1 - s) = s, so weak colours nearly vanish.
    let mut a = one([0.3, 0.3, 0.3000001]);
    vibrance(&mut a, -1.0);
    let p = a.pixel(0, 0);
    assert!((p[0] - p[2]).abs() < 1e-6);
    let mut b = one([0.5, 0.45, 0.45]);
    let before = sat(b.pixel(0, 0));
    vibrance(&mut b, -1.0);
    assert!(sat(b.pixel(0, 0)) < before * 0.2);
    // a fully saturated colour (s = 1) is untouched
    let mut c = one([0.8, 0.0, 0.0]);
    vibrance(&mut c, -1.0);
    assert_eq!(c.pixel(0, 0)[..3], [0.8, 0.0, 0.0]);
}

#[test]
fn vibrance_finite_for_odd_input() {
    let mut a = one([-1.0, 5.0, 1e6]);
    vibrance(&mut a, 1.0);
    assert!(a.pixel(0, 0).iter().all(|v| v.is_finite()));
}

// ---------------------------------------------------------------- curve channels

#[test]
fn curve_channel_from_index() {
    use CurveChannel::*;
    let all = [Luminance, Red, Green, Blue, SatVsSat, HueVsHue, HueVsSat];
    for (i, c) in all.iter().enumerate() {
        assert_eq!(CurveChannel::from_index(i), Some(*c));
    }
    assert_eq!(CurveChannel::from_index(7), None);
    assert_eq!(CurveChannel::from_index(usize::MAX), None);
}

const NEUTRAL: [[f32; 2]; 2] = [[0.0, 0.5], [1.0, 0.5]];

#[test]
fn neutral_colour_curves_are_bit_exact() {
    let img = noise(15, 9, 4);
    for ch in [CurveChannel::SatVsSat, CurveChannel::HueVsHue, CurveChannel::HueVsSat] {
        let mut a = img.clone();
        point_curve(&mut a, &NEUTRAL, ch);
        assert_eq!(a.data(), img.data());
        point_curve(&mut a, &[[0.2, 0.5], [0.3, 0.5], [0.9, 0.5]], ch);
        assert_eq!(a.data(), img.data());
        point_curve(&mut a, &[[0.5, 0.9]], ch);
        assert_eq!(a.data(), img.data());
    }
}

#[test]
fn hue_curves_leave_grey_untouched() {
    let steep = [[0.0, 0.0], [0.5, 1.0], [1.0, 0.0]];
    for ch in [CurveChannel::SatVsSat, CurveChannel::HueVsHue, CurveChannel::HueVsSat] {
        let mut a = one(grey(0.5));
        point_curve(&mut a, &steep, ch);
        assert_eq!(a.pixel(0, 0)[..3], [0.5, 0.5, 0.5]);
    }
}

#[test]
fn hue_vs_hue_shifts_red_by_expected_angle() {
    // f = 0.75 everywhere -> +30 degrees; red (hue 0) -> hue 30.
    let mut a = one([0.8, 0.0, 0.0]);
    let y0 = lum(a.pixel(0, 0));
    point_curve(&mut a, &[[0.0, 0.75], [1.0, 0.75]], CurveChannel::HueVsHue);
    let p = a.pixel(0, 0);
    assert!((hue_deg(p) - 30.0).abs() < 1e-2, "{}", hue_deg(p));
    assert!((lum(p) - y0).abs() < 1e-6);
    // f = 0.25 -> -30 degrees = 330
    let mut a = one([0.8, 0.0, 0.0]);
    point_curve(&mut a, &[[0.0, 0.25], [1.0, 0.25]], CurveChannel::HueVsHue);
    assert!((hue_deg(a.pixel(0, 0)) - 330.0).abs() < 1e-2);
    // chroma magnitude is kept by a pure rotation
    let c = |p: [f32; 4]| (p[0] - 0.5 * (p[1] + p[2])).hypot(0.866_025_4 * (p[1] - p[2]));
    let mut b = one([0.5, 0.2, 0.1]);
    let c0 = c(b.pixel(0, 0));
    point_curve(&mut b, &[[0.0, 1.0], [1.0, 1.0]], CurveChannel::HueVsHue);
    assert!((c(b.pixel(0, 0)) - c0).abs() < 1e-5);
}

#[test]
fn hue_vs_hue_only_where_the_curve_says() {
    // shift only near red (h = 0), none at green (h = 1/3).
    let pts = [[0.0, 0.75], [0.1, 0.75], [0.2, 0.5], [1.0, 0.5]];
    let mut red = one([0.8, 0.0, 0.0]);
    let mut green = one([0.0, 0.8, 0.0]);
    point_curve(&mut red, &pts, CurveChannel::HueVsHue);
    point_curve(&mut green, &pts, CurveChannel::HueVsHue);
    assert!((hue_deg(red.pixel(0, 0)) - 30.0).abs() < 0.1);
    assert_eq!(green.pixel(0, 0)[..3], [0.0, 0.8, 0.0]);
}

#[test]
fn hue_vs_sat_zero_greys_that_hue_only() {
    // factor 0 around green (h = 1/3), factor 1 at the opposite hue magenta (h = 5/6).
    let pts = [[0.0, 0.5], [0.2, 0.5], [0.3, 0.0], [0.37, 0.0], [0.5, 0.5], [1.0, 0.5]];
    let mut green = one([0.1, 0.8, 0.1]);
    point_curve(&mut green, &pts, CurveChannel::HueVsSat);
    let g = green.pixel(0, 0);
    assert!((g[0] - g[1]).abs() < 1e-5 && (g[1] - g[2]).abs() < 1e-5, "{g:?}");
    assert!((lum(g) - lum([0.1, 0.8, 0.1, 0.0])).abs() < 1e-6);
    let mut mag = one([0.8, 0.1, 0.8]);
    point_curve(&mut mag, &pts, CurveChannel::HueVsSat);
    let m = mag.pixel(0, 0);
    assert!((m[0] - 0.8).abs() < 1e-5 && (m[1] - 0.1).abs() < 1e-5 && (m[2] - 0.8).abs() < 1e-5, "{m:?}");
    // factor 2 doubles the chroma
    let mut a = one([0.5, 0.3, 0.2]);
    let s0 = chroma(a.pixel(0, 0));
    point_curve(&mut a, &[[0.0, 1.0], [1.0, 1.0]], CurveChannel::HueVsSat);
    assert!(chroma(a.pixel(0, 0)) > s0 * 1.5);
}

#[test]
fn sat_vs_sat_boosts_low_saturation() {
    // s small -> factor 2, s large -> factor 1.
    let pts = [[0.0, 1.0], [0.3, 0.5], [1.0, 0.5]];
    let mut weak = one([0.4, 0.35, 0.35]);
    let mut strong = one([0.9, 0.02, 0.02]);
    let (w0, s0) = (chroma(weak.pixel(0, 0)), chroma(strong.pixel(0, 0)));
    point_curve(&mut weak, &pts, CurveChannel::SatVsSat);
    point_curve(&mut strong, &pts, CurveChannel::SatVsSat);
    assert!(chroma(weak.pixel(0, 0)) > w0 * 1.3, "{w0} -> {}", chroma(weak.pixel(0, 0)));
    assert!((chroma(strong.pixel(0, 0)) - s0).abs() < 1e-3);
    assert_eq!(weak.pixel(0, 0)[3], 0.37);
}

#[test]
fn colour_curves_stay_finite() {
    let img = noise(20, 20, 5);
    for ch in [CurveChannel::SatVsSat, CurveChannel::HueVsHue, CurveChannel::HueVsSat] {
        let mut a = img.clone();
        point_curve(&mut a, &[[0.0, 1.0], [0.5, 0.0], [1.0, 1.0]], ch);
        assert!(a.data().iter().all(|v| v.is_finite()));
    }
}

fn chroma(p: [f32; 4]) -> f32 {
    (p[0] - 0.5 * (p[1] + p[2])).hypot(0.866_025_4 * (p[1] - p[2]))
}
