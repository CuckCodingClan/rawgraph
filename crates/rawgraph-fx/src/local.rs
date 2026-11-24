//! Local contrast ("clarity" / "texture") on log luminance with an edge-aware base layer.

use rawgraph_core::{ops, ImageBuf};
use rayon::prelude::*;

use crate::util::{bilinear, finite_or, luma};

/// Offset of the log axis: `L = ln(max(Y, 0) + EPS)` (same as the unsharp mask, ~3.2 stops below grey).
const EPS: f32 = 0.02;
/// Sigma of the large scale as a fraction of the long edge.
const LARGE_FRAC: f32 = 0.02;
/// Sigma of the fine scale as a fraction of the long edge.
const FINE_FRAC: f32 = 0.0025;
/// Smallest sigma in pixels (tiny previews).
const MIN_SIGMA: f32 = 1.0;
/// Guided-filter regularisation (variance, `ln` units squared) of the large scale. A window whose
/// local standard deviation is well above `sqrt(0.004)` = 0.06 ln (a tenth of a stop) is treated as
/// containing an edge and keeps it in the base layer, so strong edges get no halo (measured below
/// 2 % of a 3-stop step). Larger values would give a smoother base but visible halos.
const GUIDE_EPS_LARGE: f32 = 0.004;
/// Same for the fine scale (std 0.17 ln, a quarter stop): a bigger value than for the large scale,
/// so that fine noise-like texture is not mistaken for edges and can really be softened.
const GUIDE_EPS_FINE: f32 = 0.03;
/// Gain of a positive `clarity` of 1 on the large-scale detail (detail becomes 1 + 1.2 = 2.2 times).
const K_CLARITY: f32 = 1.2;
/// Gain of a positive `texture` of 1 on the fine detail (detail becomes 3 times).
const K_TEXTURE: f32 = 2.0;
/// Negative values soften with factor 1 + k * amount on the detail: texture removes all of it at
/// -1, clarity keeps 25 % of it (full removal looks like plastic).
const K_CLARITY_NEG: f32 = 0.75;
const K_TEXTURE_NEG: f32 = 1.0;
/// Soft limit (ln units) of a positively amplified detail signal; slope 1 at 0.
const LIMIT_LARGE: f32 = 0.7;
const LIMIT_FINE: f32 = 0.35;
/// Weight of the second-order mean-luminance compensation (1 = exact to second order; 1.4 also
/// covers higher-order terms and the soft limit, tuned on strongly textured images).
const PRESERVE: f32 = 1.4;

/// Edge-aware base layer of `l` (self-guided filter, He et al.) with a Gaussian-like window of
/// `sigma` pixels. `l` must be centred (mean near 0) to keep the variance estimate precise.
///
/// Fast variant: the statistics are computed on a box-downscaled copy (factor `s = sigma / 4`,
/// 1..8, so the low-resolution window still has about 4 px or more), the coefficients `a`, `b`
/// are upsampled bilinearly and applied to the full-resolution guide, so edges stay sharp.
fn guided_base(l: &[f32], w: u32, h: u32, sigma: f32, eps: f32) -> Vec<f32> {
    let (wu, hu) = (w as usize, h as usize);
    let s = ((sigma / 4.0).round() as usize).clamp(1, 8);
    let (lw, lh) = (wu.div_ceil(s), hu.div_ceil(s));
    let mut lanes = ImageBuf::new(lw as u32, lh as u32);
    lanes.data_mut().par_chunks_exact_mut(lw * 4).enumerate().for_each(|(ly, row)| {
        let (y0, y1) = (ly * s, ((ly + 1) * s).min(hu));
        for (lx, px) in row.chunks_exact_mut(4).enumerate() {
            let (x0, x1) = (lx * s, ((lx + 1) * s).min(wu));
            let (mut m1, mut m2) = (0.0f32, 0.0f32);
            for y in y0..y1 {
                for &v in &l[y * wu + x0..y * wu + x1] {
                    m1 += v;
                    m2 += v * v;
                }
            }
            let inv = 1.0 / ((x1 - x0) * (y1 - y0)) as f32;
            px[0] = m1 * inv;
            px[1] = m2 * inv;
        }
    });
    let low_sigma = sigma / s as f32;
    let stats = ops::blur(&lanes, low_sigma);
    drop(lanes);
    let mut ab = ImageBuf::new(lw as u32, lh as u32);
    ab.data_mut().par_chunks_exact_mut(4).zip(stats.data().par_chunks_exact(4)).for_each(|(o, st)| {
        let var = (st[1] - st[0] * st[0]).max(0.0);
        let a = var / (var + eps);
        o[0] = a;
        o[1] = st[0] - a * st[0];
    });
    drop(stats);
    let mean_ab = ops::blur(&ab, low_sigma);
    drop(ab);
    let inv_s = 1.0 / s as f32;
    let mut base = vec![0.0f32; wu * hu];
    base.par_chunks_exact_mut(wu).zip(l.par_chunks_exact(wu)).enumerate().for_each(|(y, (out, lrow))| {
        let fy = (y as f32 + 0.5) * inv_s - 0.5;
        for (x, (o, &v)) in out.iter_mut().zip(lrow).enumerate() {
            let m = bilinear(mean_ab.data(), lw, lh, (x as f32 + 0.5) * inv_s - 0.5, fy);
            *o = m[0] * v + m[1];
        }
    });
    base
}

/// Detail gain: positive amounts amplify by `k_pos * amount`, negative ones attenuate by
/// `k_neg * amount` (so the detail factor `1 + gain` stays >= 0 for `amount >= -1`).
#[inline]
fn gain(amount: f32, k_pos: f32, k_neg: f32) -> f32 {
    if amount >= 0.0 {
        amount * k_pos
    } else {
        amount * k_neg
    }
}

/// Added log-luminance of one detail layer: linear for softening, soft-limited for boosting.
#[inline]
fn shape(d: f32, g: f32, limit: f32) -> f32 {
    if g <= 0.0 {
        g * d
    } else {
        g * limit * d / (limit + d.abs())
    }
}

/// Local contrast on two scales. `clarity` works on large structures (sigma about 2 % of the
/// long edge), `texture` on fine detail (about 0.25 % of the long edge); each in -1..1 (clamped),
/// 0 = off, negative softens. Both 0 (or a zero-sized image) returns a clone.
///
/// Works on `L = ln(max(Y, 0) + 0.02)` (`Y` the ACEScg luminance). For each active scale an
/// edge-aware base `B` is computed with a self-guided filter (so strong edges stay in the base
/// layer and get no halo, unlike a Gaussian unsharp mask) and the detail is `d = L - B`:
///
/// * clarity: sigma `0.02 * long edge`, regularisation 0.004, `L += g * 0.7 d / (0.7 + |d|)` with
///   `g = 1.2 * clarity` for `clarity > 0` (soft-limited at 0.7 ln, slope 1 at 0), and
///   `L += 0.75 * clarity * d` for `clarity < 0`;
/// * texture: sigma `0.0025 * long edge` (at least 1 px), regularisation 0.03,
///   `g = 2.0 * texture` with limit 0.35 ln for `texture > 0`, `L += texture * d` for negative.
///
/// A second-order mean-luminance correction (weight 1.4, see `PRESERVE`) is subtracted per pixel
/// so that the average linear luminance of a textured image moves by less than about 2 % (log-space
/// gain on bright detail would otherwise raise it).
///
/// So +1 amplifies the detail of that scale to 2.2 times (clarity) / 3 times (texture), -1
/// reduces it to 25 % / 0 and never inverts it (the factor `1 + gain` stays >= 0). The two
/// contributions add, with both active each is computed against its own base.
///
/// RGB is multiplied by `exp(out_L - L)` (hue and saturation kept, no negative result, gain
/// bounded); pixels with `Y <= 0` stay unchanged, alpha is untouched. Fully scene-referred: values
/// above 1 and tiny values are handled on the log axis, nothing is clamped.
pub fn local_contrast(img: &ImageBuf, clarity: f32, texture: f32) -> ImageBuf {
    let clarity = finite_or(clarity, 0.0).clamp(-1.0, 1.0);
    let texture = finite_or(texture, 0.0).clamp(-1.0, 1.0);
    let (w, h) = (img.width(), img.height());
    if (clarity == 0.0 && texture == 0.0) || w == 0 || h == 0 {
        return img.clone();
    }
    let n = w as usize * h as usize;
    let mut log_y = vec![0.0f32; n];
    log_y.par_iter_mut().zip(img.data().par_chunks_exact(4)).for_each(|(l, px)| {
        *l = (luma(px[0], px[1], px[2]).max(0.0) + EPS).ln();
    });
    let mean = (log_y.par_iter().map(|&v| v as f64).sum::<f64>() / n as f64) as f32;
    let centred: Vec<f32> = log_y.par_iter().map(|&v| v - mean).collect();

    let long = w.max(h) as f32;
    // Per pixel: added log luminance `x` and the (first scale's) detail `d` for the mean correction.
    let mut delta = vec![0.0f32; n];
    let mut add = |frac: f32, eps: f32, g: f32, limit: f32| {
        let base = guided_base(&centred, w, h, (frac * long).max(MIN_SIGMA), eps);
        delta.par_iter_mut().zip(centred.par_iter().zip(base.par_iter())).for_each(|(o, (&l, &b))| {
            let d = l - b;
            let x = shape(d, g, limit);
            // Second-order mean preservation: exp(d + x) has mean exp(d) * (1 + (x^2 + 2 x d) / 2);
            // subtract that part (weighted by PRESERVE) so the linear mean luminance stays put.
            *o += x - PRESERVE * 0.5 * (x * x + 2.0 * x * d);
        });
    };
    if clarity != 0.0 {
        add(LARGE_FRAC, GUIDE_EPS_LARGE, gain(clarity, K_CLARITY, K_CLARITY_NEG), LIMIT_LARGE);
    }
    if texture != 0.0 {
        add(FINE_FRAC, GUIDE_EPS_FINE, gain(texture, K_TEXTURE, K_TEXTURE_NEG), LIMIT_FINE);
    }

    let mut out = img.clone();
    out.data_mut().par_chunks_exact_mut(4).zip(delta.par_iter()).for_each(|(px, &d)| {
        if luma(px[0], px[1], px[2]) <= 0.0 {
            return;
        }
        let g = d.exp();
        if g.is_finite() {
            px[0] *= g;
            px[1] *= g;
            px[2] *= g;
        }
    });
    out
}
