//! Tone and colour effects that work on single pixels.

use rawgraph_core::ImageBuf;
use rayon::prelude::*;

use crate::util::{chroma_vec, finite_or, luma, perceptual, smoothstep, SQRT3_2};

/// Centres of the four tone bands on the perceptual axis.
const BAND_CENTRES: [f32; 4] = [0.125, 0.375, 0.625, 0.875];
/// Gaussian width of a band on the perceptual axis.
const BAND_SIGMA: f32 = 0.16;
/// Stops a band moves at +-1.
const BAND_STOPS: f32 = 1.5;
const LUT_SIZE: usize = 1024;

/// Log2 gain `g(t)` and its derivative for parameters `p` (before monotonic limiting).
fn band_gain(p: &[f32; 4], t: f32) -> (f32, f32) {
    let (mut g, mut dg) = (0.0, 0.0);
    for (pi, c) in p.iter().zip(BAND_CENTRES) {
        let d = t - c;
        let w = (-0.5 * (d / BAND_SIGMA) * (d / BAND_SIGMA)).exp();
        g += pi * w;
        dg += pi * w * (-d / (BAND_SIGMA * BAND_SIGMA));
    }
    (BAND_STOPS * g, BAND_STOPS * dg)
}

/// Scale `k <= 1` that keeps the luminance mapping monotonic for parameters `p`.
///
/// With `f(Y) = Y 2^g(t)` and `t = Y / (Y + 0.18)` one finds
/// `f'(Y) = 2^g (1 + ln2 * t (1 - t) * g'(t))`, so `f` is monotonic iff
/// `ln2 * t (1 - t) * (-g'(t)) <= 1` everywhere. Two neighbouring bands pushed in
/// opposite directions can violate that (a 3 stop step over ~1.5 stops of input), so
/// `g` is scaled by the largest factor that satisfies it (with 5 % margin). A single
/// band at +-1 is never limited.
fn monotonic_scale(p: &[f32; 4]) -> f32 {
    let mut worst = 0.0f32;
    for i in 0..=2048 {
        let t = i as f32 / 2048.0;
        let (_, dg) = band_gain(p, t);
        worst = worst.max(std::f32::consts::LN_2 * t * (1.0 - t) * (-dg));
    }
    if worst > 0.95 {
        0.95 / worst
    } else {
        1.0
    }
}

/// Gain LUT of the tone curve over the perceptual axis (exposed for the monotonicity test).
pub(crate) fn tone_lut(p: [f32; 4]) -> Vec<f32> {
    let k = monotonic_scale(&p);
    (0..LUT_SIZE).map(|i| (band_gain(&p, i as f32 / (LUT_SIZE - 1) as f32).0 * k).exp2()).collect()
}

/// Parametric tone curve on luminance with four bands.
///
/// The curve is a gain in stops over the perceptual axis `t = Y / (Y + 0.18)`:
/// `gain = 2^(1.5 * k * sum_i p_i * exp(-((t - c_i) / 0.16)^2 / 2))` with centres
/// `c = 0.125, 0.375, 0.625, 0.875` for shadows, darks, lights, highlights and
/// parameters clamped to -1..1, so one band at +-1 moves its centre by about +-1.5
/// stops. `k` (<= 1, usually 1) limits extreme opposing combinations so that the
/// mapping `Y -> Y'` stays monotonic (see `monotonic_scale`); the gain is read from a
/// 1024-entry table with linear interpolation. RGB is multiplied by the gain, hence
/// hue and saturation are preserved; pixels with `Y <= 0` are unchanged. All
/// parameters 0 is the bit-exact identity.
pub fn tone_curve(img: &mut ImageBuf, shadows: f32, darks: f32, lights: f32, highlights: f32) {
    let p = [shadows, darks, lights, highlights].map(|v| finite_or(v, 0.0).clamp(-1.0, 1.0));
    if p == [0.0; 4] {
        return;
    }
    let lut = tone_lut(p);
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let y = luma(px[0], px[1], px[2]);
        if y > 0.0 {
            let pos = perceptual(y) * (LUT_SIZE - 1) as f32;
            let i = (pos as usize).min(LUT_SIZE - 2);
            let gain = lut[i] + (lut[i + 1] - lut[i]) * (pos - i as f32).min(1.0);
            px[0] *= gain;
            px[1] *= gain;
            px[2] *= gain;
        }
    });
}

/// Monochrome conversion: `Y = weights . RGB` written to all three channels. The
/// weights are used as given (not normalised).
pub fn black_white(img: &mut ImageBuf, weights: [f32; 3]) {
    let w = weights.map(|v| finite_or(v, 0.0));
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let y = w[0] * px[0] + w[1] * px[1] + w[2] * px[2];
        px[0] = y;
        px[1] = y;
        px[2] = y;
    });
}

const INV_SQRT3: f32 = 0.577_350_3;

/// Zero-mean RGB offsets whose chroma vector is `(a, b)`.
#[inline]
pub(crate) fn zero_mean(a: f32, b: f32) -> [f32; 3] {
    [a * (2.0 / 3.0), -a * (1.0 / 3.0) + b * INV_SQRT3, -a * (1.0 / 3.0) - b * INV_SQRT3]
}

/// Unit vectors (cos, sin) of the band centres 0, 60, ..., 300 degrees.
const BAND_DIRS: [[f32; 2]; 6] = [[1.0, 0.0], [0.5, SQRT3_2], [-0.5, SQRT3_2], [-1.0, 0.0], [-0.5, -SQRT3_2], [0.5, -SQRT3_2]];

/// Hue sector (0..=5, 60 degrees each) of the unit chroma vector `(x, y)` from three sign
/// tests, and the position inside the sector as `w1 = (1 - cos(3 phi)) / 2` where `phi` is
/// the angle from the sector's lower band centre; `None` only for rounding corner cases.
#[inline]
fn sector_weights(x: f32, y: f32) -> Option<(usize, f32)> {
    let a = y >= 0.0; // 0..180
    let b = 0.5 * y - SQRT3_2 * x >= 0.0; // 60..240
    let c = -0.5 * y - SQRT3_2 * x >= 0.0; // 120..300
    let i = match (a, b, c) {
        (true, false, false) => 0,
        (true, true, false) => 1,
        (true, true, true) => 2,
        (false, true, true) => 3,
        (false, false, true) => 4,
        (false, false, false) => 5,
        _ => return None,
    };
    let cos_phi = (x * BAND_DIRS[i][0] + y * BAND_DIRS[i][1]).min(1.0);
    // cos(3 phi) = 4 cos^3 - 3 cos
    let cos3 = cos_phi * (4.0 * cos_phi * cos_phi - 3.0);
    Some((i, (0.5 * (1.0 - cos3)).clamp(0.0, 1.0)))
}

/// `(sin, cos)` of `x` for `|x| <= 0.53` (Taylor series, error < 1e-7).
#[inline]
fn sin_cos_small(x: f32) -> (f32, f32) {
    let x2 = x * x;
    let s = x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))));
    let c = 1.0 - x2 / 2.0 * (1.0 - x2 / 12.0 * (1.0 - x2 / 30.0 * (1.0 - x2 / 56.0)));
    (s, c)
}

/// Six-band hue / saturation / lightness adjustment.
///
/// Decomposition: the chroma vector is `(a, b) = (r - (g + b) / 2, sqrt(3)/2 (g - b))`
/// (invariant to adding grey, i.e. the colour around its ACEScg luminance axis), hue
/// is `atan2(b, a)` and chroma its length. Bands sit at 0, 60, 120, 180, 240 and 300
/// degrees (red, yellow, green, cyan, blue, magenta of that circle); a pixel blends
/// its two neighbouring bands with raised-cosine weights that are continuous and sum
/// to 1. Per band `[hue, saturation, lightness]`, each clamped to -1..1:
/// hue rotates the chroma vector by up to +-30 degrees (positive = towards the next
/// band, red -> yellow); saturation scales chroma by `1 + s` (-1 = grey, +1 = double);
/// lightness multiplies the pixel by `2^(l * wc)` (+-1 = +-1 stop), where
/// `wc = smoothstep(0, 0.4, C / (|Y| + C))` fades to 0 for neutrals. Luminance is
/// held by the hue and saturation steps. Neutral pixels (chroma exactly 0) are never
/// touched, and all-zero bands are the bit-exact identity.
///
/// No per-pixel trigonometry: the sector comes from sign tests on the normalised chroma
/// vector, the band weight from `cos(3 phi) = T3(cos phi)` (a dot product), and the hue
/// rotation from a short series (identical to the exact formulas within ~1e-6).
pub fn hue_saturation(img: &mut ImageBuf, bands: &[[f32; 3]; 6]) {
    let b = bands.map(|band| band.map(|v| finite_or(v, 0.0).clamp(-1.0, 1.0)));
    if b == [[0.0; 3]; 6] {
        return;
    }
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let (r, g, bl) = (px[0], px[1], px[2]);
        let (a, bb) = chroma_vec(r, g, bl);
        let c = (a * a + bb * bb).sqrt();
        if c <= 0.0 || !c.is_finite() {
            return;
        }
        let inv = 1.0 / c;
        let (x, y) = (a * inv, bb * inv);
        let (i, w1) = match sector_weights(x, y) {
            Some(v) => v,
            None => {
                // Rounding corner case: exact angle.
                let t = bb.atan2(a).rem_euclid(std::f32::consts::TAU) * (6.0 / std::f32::consts::TAU);
                let i = (t as usize).min(5);
                let f = (t - i as f32).clamp(0.0, 1.0);
                (i, 0.5 * (1.0 - (std::f32::consts::PI * f).cos()))
            }
        };
        let w0 = 1.0 - w1;
        let j = (i + 1) % 6;
        let mix = |k: usize| w0 * b[i][k] + w1 * b[j][k];
        let (dh, ds, dl) = (mix(0), mix(1), mix(2));

        if dh != 0.0 || ds != 0.0 {
            let (mut na, mut nb) = (a, bb);
            if dh != 0.0 {
                let (s, co) = sin_cos_small(dh * std::f32::consts::FRAC_PI_6);
                na = a * co - bb * s;
                nb = a * s + bb * co;
            }
            let sc = 1.0 + ds;
            na *= sc;
            nb *= sc;
            let (zo, zn) = (zero_mean(a, bb), zero_mean(na, nb));
            let mut d = [zn[0] - zo[0], zn[1] - zo[1], zn[2] - zo[2]];
            // Keep luminance: remove the luminance of the change.
            let dy = luma(d[0], d[1], d[2]);
            d[0] -= dy;
            d[1] -= dy;
            d[2] -= dy;
            px[0] = r + d[0];
            px[1] = g + d[1];
            px[2] = bl + d[2];
        }
        if dl != 0.0 {
            let ya = luma(r, g, bl).abs();
            let rel = c / (ya + c);
            let gain = (dl * smoothstep(0.0, 0.4, rel)).exp2();
            px[0] *= gain;
            px[1] *= gain;
            px[2] *= gain;
        }
    });
}

/// Radial vignette. The distance `d` of a pixel from the image centre is normalised so
/// the corners are at 1. Gain in stops is `2 * amount * smoothstep(e0, e1, d)` with
/// `e0 = max(midpoint - feather / 2, 0)` and `e1 = max(midpoint + feather / 2, e0)`:
/// negative `amount` darkens the corners (-1 = -2 stops), positive brightens. The
/// centre (`d = 0`) is unchanged and `amount` 0 is the bit-exact identity.
pub fn vignette(img: &mut ImageBuf, amount: f32, midpoint: f32, feather: f32) {
    let amount = finite_or(amount, 0.0);
    if amount == 0.0 {
        return;
    }
    let (mid, feather) = (finite_or(midpoint, 0.5), finite_or(feather, 0.5).max(0.0));
    let e0 = (mid - feather * 0.5).max(0.0);
    let e1 = (mid + feather * 0.5).max(e0);
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 {
        return;
    }
    let (hw, hh) = (w as f32 * 0.5, h as f32 * 0.5);
    let inv_diag = 1.0 / hw.hypot(hh);
    img.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        let dy = y as f32 + 0.5 - hh;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let dx = x as f32 + 0.5 - hw;
            let d = dx.hypot(dy) * inv_diag;
            let s = smoothstep(e0, e1, d);
            if s > 0.0 {
                let gain = (2.0 * amount * s).exp2();
                px[0] *= gain;
                px[1] *= gain;
                px[2] *= gain;
            }
        }
    });
}

/// Whites and blacks. Per pixel (alpha untouched), with `Y = AP1_LUMA . max(rgb, 0)` and
/// `t = Y / (Y + 0.18)`:
///
/// ```text
/// wh = smoothstep(0.35, 0.95, t)        wb = 1 - smoothstep(0.0, 0.45, t)
/// out_c = rgb_c * 2^(whites * wh) + blacks * 0.03 * wb
/// blacks < 0:  out_c = max(out_c, min(rgb_c, 0))     // never pushes a channel below 0
/// ```
///
/// `whites` scales bright pixels by up to `2^whites` (+-1 = +-1 stop at the top, none in the
/// shadows); `blacks` adds / subtracts up to 0.03 scene-linear in the shadows (none in the
/// highlights). Both are clamped to -1..1, a non-finite one counts as 0, both 0 is the
/// bit-exact identity. Non-negative input never becomes negative.
pub fn whites_blacks(img: &mut ImageBuf, whites: f32, blacks: f32) {
    let whites = finite_or(whites, 0.0).clamp(-1.0, 1.0);
    let blacks = finite_or(blacks, 0.0).clamp(-1.0, 1.0);
    if whites == 0.0 && blacks == 0.0 {
        return;
    }
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let y = luma(px[0].max(0.0), px[1].max(0.0), px[2].max(0.0));
        if !y.is_finite() {
            return;
        }
        let t = y / (y + 0.18);
        let wh = smoothstep(0.35, 0.95, t);
        let wb = 1.0 - smoothstep(0.0, 0.45, t);
        let gain = (whites * wh).exp2();
        let add = blacks * 0.03 * wb;
        for v in &mut px[..3] {
            let mut o = *v * gain + add;
            if blacks < 0.0 {
                o = o.max(v.min(0.0));
            }
            *v = o;
        }
    });
}

/// Vibrance: saturation that acts more on weakly saturated pixels. Per pixel (alpha untouched):
///
/// ```text
/// c = max(rgb, 0);  s = (max(c) > 1e-6) ? (max(c) - min(c)) / max(c) : 0
/// k = 1 + amount * (1 - s);   Y = AP1_LUMA . rgb;   out = Y + (rgb - Y) * k
/// ```
///
/// `amount` is clamped to -1..1 (non-finite = 0); 0 is the bit-exact identity. Greys stay grey
/// and luminance is preserved; at -1 a pixel with `s = 0` becomes grey.
pub fn vibrance(img: &mut ImageBuf, amount: f32) {
    let amount = finite_or(amount, 0.0).clamp(-1.0, 1.0);
    if amount == 0.0 {
        return;
    }
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let (r, g, b) = (px[0].max(0.0), px[1].max(0.0), px[2].max(0.0));
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let s = if mx > 1e-6 { (mx - mn) / mx } else { 0.0 };
        let k = 1.0 + amount * (1.0 - s);
        let y = luma(px[0], px[1], px[2]);
        if !y.is_finite() || !k.is_finite() {
            return;
        }
        for v in &mut px[..3] {
            *v = y + (*v - y) * k;
        }
    });
}
