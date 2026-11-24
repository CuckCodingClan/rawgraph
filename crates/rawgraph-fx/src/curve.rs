//! Point curve on the perceptual axis `t = v / (v + 0.18)`.

use rawgraph_core::ImageBuf;
use rayon::prelude::*;

use crate::tone::zero_mean;
use crate::util::{c01, chroma_vec, luma};

/// Which values a point curve acts on. Index order: Luminance, Red, Green, Blue, Sat vs Sat,
/// Hue vs Hue, Hue vs Sat (indices 0..=6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CurveChannel {
    Luminance,
    Red,
    Green,
    Blue,
    SatVsSat,
    HueVsHue,
    HueVsSat,
}

impl CurveChannel {
    /// `None` for an index above 6.
    pub fn from_index(i: usize) -> Option<CurveChannel> {
        [
            CurveChannel::Luminance,
            CurveChannel::Red,
            CurveChannel::Green,
            CurveChannel::Blue,
            CurveChannel::SatVsSat,
            CurveChannel::HueVsHue,
            CurveChannel::HueVsSat,
        ]
        .get(i)
        .copied()
    }
}

/// Lookup table intervals over t = 0..1.
const LUT_N: usize = 4096;
/// Linear value that `t = 1` maps back to (large but finite).
const V_MAX: f32 = 1.0e6;
/// Mid-grey of the perceptual axis.
const GREY: f32 = 0.18;

/// Monotone cubic (Fritsch-Carlson / PCHIP) interpolant through cleaned points.
struct Pchip {
    x: Vec<f32>,
    y: Vec<f32>,
    m: Vec<f32>,
}

impl Pchip {
    /// Cleans the points (non-finite dropped, clamped to 0..1, sorted by x, a later point with the
    /// same x replaces the earlier one). `None` when fewer than two distinct x remain.
    fn new(points: &[[f32; 2]]) -> Option<Pchip> {
        let mut pts: Vec<(f32, f32)> =
            points.iter().filter(|p| p[0].is_finite() && p[1].is_finite()).map(|p| (c01(p[0]), c01(p[1]))).collect();
        // Stable sort keeps the given order of equal x; the last of them wins below.
        pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut clean: Vec<(f32, f32)> = Vec::with_capacity(pts.len());
        for p in pts {
            match clean.last_mut() {
                Some(last) if last.0 == p.0 => *last = p,
                _ => clean.push(p),
            }
        }
        if clean.len() < 2 {
            return None;
        }
        let n = clean.len();
        let x: Vec<f32> = clean.iter().map(|p| p.0).collect();
        let y: Vec<f32> = clean.iter().map(|p| p.1).collect();
        let h: Vec<f64> = (0..n - 1).map(|k| (x[k + 1] - x[k]) as f64).collect();
        let d: Vec<f64> = (0..n - 1).map(|k| (y[k + 1] - y[k]) as f64 / h[k]).collect();
        let mut m = vec![0.0f64; n];
        if n == 2 {
            m[0] = d[0];
            m[1] = d[0];
        } else {
            for k in 1..n - 1 {
                if d[k - 1] * d[k] > 0.0 {
                    let w1 = 2.0 * h[k] + h[k - 1];
                    let w2 = h[k] + 2.0 * h[k - 1];
                    m[k] = (w1 + w2) / (w1 / d[k - 1] + w2 / d[k]);
                }
            }
            m[0] = end_slope(h[0], h[1], d[0], d[1]);
            m[n - 1] = end_slope(h[n - 2], h[n - 3], d[n - 2], d[n - 3]);
        }
        Some(Pchip { x, y, m: m.into_iter().map(|v| v as f32).collect() })
    }

    fn eval(&self, t: f32) -> f32 {
        let t = c01(t);
        let n = self.x.len();
        if t <= self.x[0] {
            return self.y[0];
        }
        if t >= self.x[n - 1] {
            return self.y[n - 1];
        }
        let k = (self.x.partition_point(|&v| v <= t) - 1).min(n - 2);
        let h = self.x[k + 1] - self.x[k];
        let s = (t - self.x[k]) / h;
        let (s2, s3) = (s * s, s * s * s);
        let v = (2.0 * s3 - 3.0 * s2 + 1.0) * self.y[k]
            + (s3 - 2.0 * s2 + s) * h * self.m[k]
            + (-2.0 * s3 + 3.0 * s2) * self.y[k + 1]
            + (s3 - s2) * h * self.m[k + 1];
        c01(v)
    }
}

/// Shape-preserving three-point end slope (as in SciPy's `pchip`).
fn end_slope(h0: f64, h1: f64, d0: f64, d1: f64) -> f64 {
    let m = ((2.0 * h0 + h1) * d0 - h0 * d1) / (h0 + h1);
    if m * d0 <= 0.0 {
        0.0
    } else if d0 * d1 < 0.0 && m.abs() > 3.0 * d0.abs() {
        3.0 * d0
    } else {
        m
    }
}

/// `points` is the identity curve (nothing to do).
fn is_identity(points: &[[f32; 2]]) -> bool {
    points.len() < 2 || (points.len() == 2 && points[0] == [0.0, 0.0] && points[1] == [1.0, 1.0])
}

/// The curve through `points` (input, output) on the perceptual axis: maps t in 0..1 to the
/// output t (0..1). Monotone cubic (PCHIP) interpolation, so it never overshoots between
/// points; below the first / above the last point it continues with that point's value.
/// Points are clamped to 0..1 and sorted by x (a later point with the same x wins). Fewer
/// than two points give the identity.
pub fn curve_eval(points: &[[f32; 2]], t: f32) -> f32 {
    match Pchip::new(points) {
        Some(c) if !is_identity(points) => c.eval(t),
        _ => c01(t),
    }
}

/// Applies a curve through `points` (input, output) to the luminance, to a single channel, or
/// (channels 4..=6) to colour saturation / hue.
///
/// # Channels 0..=3: Luminance, Red, Green, Blue
///
/// Both axes are the perceptual axis `t = v / (v + 0.18)` (see [`curve_eval`]).
/// The curve result is converted back with `v = 0.18 t / (1 - t)`; `t = 1` maps to 1e6, so
/// values stay finite. Luminance: `Y` (ACEScg luminance) becomes `Y'` and RGB is scaled by
/// `Y' / Y` (so hue and chroma ratios are kept; `Y <= 0` is unchanged). Red/Green/Blue: that
/// channel alone goes through the curve (negative values unchanged). Non-finite values are
/// left alone. Fewer than two points, or exactly `[(0,0), (1,1)]`, is the identity (bit-exact).
///
/// # Channels 4..=6: Sat vs Sat, Hue vs Hue, Hue vs Sat
///
/// The curve maps x in 0..1 to y in 0..1 (same PCHIP interpolation as [`curve_eval`], points
/// clamped and sorted, flat beyond the end points); `y = 0.5` means "no change", so the neutral
/// curve is `[[0, 0.5], [1, 0.5]]`. Fewer than two usable points, or a curve whose points all
/// have `y == 0.5`, is a bit-exact no-op (`[[0,0],[1,1]]` is NOT neutral here). Alpha is
/// untouched and pixels with non-finite values are left alone.
///
/// Hue and saturation of a pixel `(r, g, b)` (ACEScg, the same decomposition as
/// [`crate::hue_saturation`]):
///
/// ```text
/// a  = r - (g + b) / 2        b' = (sqrt(3) / 2) * (g - b)       // chroma vector (a, b')
/// C  = sqrt(a^2 + b'^2)                                          // chroma
/// Y  = 0.2722287168 r + 0.6740817658 g + 0.0536895174 b          // AP1 luminance (AP1_LUMA)
/// h  = atan2(b', a) / (2 pi), taken modulo 1 into [0, 1)         // 0 red, 1/6 yellow, 1/3 green,
///                                                                // 1/2 cyan, 2/3 blue, 5/6 magenta
/// s  = C / (|Y| + C)                                             // 0 for grey, towards 1 for pure colours
/// ```
///
/// Pixels with `C == 0` (greys) are never changed by these three channels. Otherwise, with the
/// curve `f` read from a table of 4096 intervals over x (linear interpolation; hue is not
/// wrapped, `h` is always below 1):
///
/// * **Hue vs Hue**: `shift = (f(h) - 0.5) * 120` degrees. The chroma vector is rotated by
///   `shift` (positive = towards the next hue, red towards yellow):
///   `a2 = a cos(shift) - b' sin(shift)`, `b2 = a sin(shift) + b' cos(shift)`. The RGB change is
///   `d = Z(a2, b2) - Z(a, b')` with the zero-mean offsets
///   `Z(a, b') = (2a/3, -a/3 + b'/sqrt(3), -a/3 - b'/sqrt(3))`; the luminance of `d` is removed
///   (`d_c -= dot(d, AP1_LUMA)` on every channel) so luminance stays constant; `rgb += d`.
/// * **Hue vs Sat**: `factor = 2 f(h)`; `rgb = Y + (rgb - Y) * factor` (as `ops::saturation`).
/// * **Sat vs Sat**: `factor = 2 f(s)`; `rgb = Y + (rgb - Y) * factor`.
///
/// # Speed
///
/// The curve is sampled into a 4096-interval table over t with linear interpolation.
/// The table differs from [`curve_eval`] by < 1e-5 on smooth parts and by at most
/// `slope jump * 6e-5` (h / 4 of a table cell; up to about 2e-4 for the steepest end slope)
/// at the kinks where the curve meets its flat end extensions, all on the t axis; near t = 1 the round trip through f32 `t` limits the
/// relative accuracy of very bright values (about 3e-4 at v = 1000, 1e-5 at v = 16).
pub fn point_curve(img: &mut ImageBuf, points: &[[f32; 2]], channel: CurveChannel) {
    if matches!(channel, CurveChannel::SatVsSat | CurveChannel::HueVsHue | CurveChannel::HueVsSat) {
        colour_curve(img, points, channel);
        return;
    }
    if is_identity(points) {
        return;
    }
    let Some(curve) = Pchip::new(points) else { return };
    let lut: Vec<f32> = (0..=LUT_N).map(|i| curve.eval(i as f32 / LUT_N as f32)).collect();
    let lookup = |t: f32| -> f32 {
        let f = t * LUT_N as f32;
        let i = (f as usize).min(LUT_N - 1);
        let fr = f - i as f32;
        lut[i] + (lut[i + 1] - lut[i]) * fr
    };
    let t_max = V_MAX / (V_MAX + GREY);
    // value -> t on the perceptual axis (value >= 0)
    let to_t = |v: f32| v / (v + GREY);
    let to_v = |t: f32| {
        let t = t.min(t_max);
        GREY * t / (1.0 - t)
    };
    let data = img.data_mut();
    match channel {
        CurveChannel::Luminance => data.par_chunks_exact_mut(4).for_each(|px| {
            let y = luma(px[0], px[1], px[2]);
            if y > 0.0 && y.is_finite() {
                let y2 = to_v(lookup(to_t(y)));
                let gain = y2 / y;
                if gain.is_finite() {
                    px[0] *= gain;
                    px[1] *= gain;
                    px[2] *= gain;
                }
            }
        }),
        _ => {
            let c = match channel {
                CurveChannel::Red => 0,
                CurveChannel::Green => 1,
                _ => 2,
            };
            data.par_chunks_exact_mut(4).for_each(|px| {
                let v = px[c];
                if v >= 0.0 && v.is_finite() {
                    px[c] = to_v(lookup(to_t(v)));
                }
            });
        }
    }
}

/// Linear interpolation in a table of `LUT_N + 1` entries over x = 0..1.
#[inline]
fn lut_lookup(lut: &[f32], x: f32) -> f32 {
    let f = c01(x) * LUT_N as f32;
    let i = (f as usize).min(LUT_N - 1);
    let fr = f - i as f32;
    lut[i] + (lut[i + 1] - lut[i]) * fr
}

/// Channels 4..=6 of [`point_curve`].
fn colour_curve(img: &mut ImageBuf, points: &[[f32; 2]], channel: CurveChannel) {
    let Some(curve) = Pchip::new(points) else { return };
    if curve.y.iter().all(|&v| v == 0.5) {
        return;
    }
    let lut: Vec<f32> = (0..=LUT_N).map(|i| curve.eval(i as f32 / LUT_N as f32)).collect();
    let lut = &lut;
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let (r, g, b) = (px[0], px[1], px[2]);
        let (a, bb) = chroma_vec(r, g, b);
        let c = a.hypot(bb);
        if c <= 0.0 || !c.is_finite() {
            return;
        }
        let y = luma(r, g, b);
        if !y.is_finite() {
            return;
        }
        let hue = || (bb.atan2(a) * (0.5 * std::f32::consts::FRAC_1_PI)).rem_euclid(1.0).min(1.0);
        let factor = match channel {
            CurveChannel::HueVsHue => {
                let shift = (lut_lookup(lut, hue()) - 0.5) * 120.0;
                let (s, co) = shift.to_radians().sin_cos();
                let (na, nb) = (a * co - bb * s, a * s + bb * co);
                let (zo, zn) = (zero_mean(a, bb), zero_mean(na, nb));
                let mut d = [zn[0] - zo[0], zn[1] - zo[1], zn[2] - zo[2]];
                let dy = luma(d[0], d[1], d[2]);
                d.iter_mut().for_each(|v| *v -= dy);
                px[0] = r + d[0];
                px[1] = g + d[1];
                px[2] = b + d[2];
                return;
            }
            CurveChannel::HueVsSat => 2.0 * lut_lookup(lut, hue()),
            _ => 2.0 * lut_lookup(lut, c / (y.abs() + c)),
        };
        px[0] = y + (r - y) * factor;
        px[1] = y + (g - y) * factor;
        px[2] = y + (b - y) * factor;
    });
}
