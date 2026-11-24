//! Unsharp mask on a logarithmic luminance axis.

use rawgraph_core::ops;
use rawgraph_core::{ImageBuf, MaskBuf};
use rayon::prelude::*;

use crate::util::{c01, finite_or, luma, mask_plane, smoothstep};

/// Offset of the log axis: `L = ln(max(Y, 0) + EPS)`. Keeps deep shadows (and their noise)
/// compressed and makes `L` defined at black. 0.02 is about 3.2 stops below middle grey.
const EPS: f32 = 0.02;
/// Soft limit of the detail signal, in `ln` units (0.4 is a little over half a stop).
const LIMIT: f32 = 0.4;
/// Gain of the detail signal before limiting (amount 1 = twice the plain `L - G(L)`).
const STRENGTH: f32 = 2.0;
/// Radii (sigma, pixels) below this use a true Gaussian; above, the core box-blur approximation.
const GAUSS_MAX_SIGMA: f32 = 4.0;

/// Normalised Gaussian taps for `sigma`, radius `ceil(3 sigma)` (at least 1).
fn gaussian_taps(sigma: f32) -> Vec<f32> {
    let r = (3.0 * sigma).ceil().max(1.0) as i32;
    let mut taps: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let sum: f32 = taps.iter().sum();
    taps.iter_mut().for_each(|t| *t /= sum);
    taps
}

/// Separable true Gaussian on a single-channel plane, edge-clamped.
fn gaussian_plane(src: &[f32], w: usize, h: usize, sigma: f32) -> Vec<f32> {
    let taps = gaussian_taps(sigma);
    let r = taps.len() / 2;
    // Horizontal pass: pad each row with replicated edges, then accumulate tap by tap.
    let mut tmp = vec![0.0f32; w * h];
    tmp.par_chunks_exact_mut(w).zip(src.par_chunks_exact(w)).for_each_init(
        || vec![0.0f32; w + 2 * r],
        |pad, (out, row)| {
            pad[..r].fill(row[0]);
            pad[r..r + w].copy_from_slice(row);
            pad[r + w..].fill(row[w - 1]);
            out.fill(0.0);
            for (k, &t) in taps.iter().enumerate() {
                for (o, p) in out.iter_mut().zip(&pad[k..k + w]) {
                    *o += t * p;
                }
            }
        },
    );
    // Vertical pass: each output row is a weighted sum of whole input rows.
    let mut out = vec![0.0f32; w * h];
    out.par_chunks_exact_mut(w).enumerate().for_each(|(y, row)| {
        for (k, &t) in taps.iter().enumerate() {
            let sy = (y + k).saturating_sub(r).min(h - 1);
            for (o, p) in row.iter_mut().zip(&tmp[sy * w..sy * w + w]) {
                *o += t * p;
            }
        }
    });
    out
}

/// Blur of a plane: true Gaussian for small sigma, the core three-pass box blur otherwise.
fn blur_plane(src: Vec<f32>, w: u32, h: u32, sigma: f32) -> Vec<f32> {
    if sigma < GAUSS_MAX_SIGMA {
        return gaussian_plane(&src, w as usize, h as usize, sigma);
    }
    let mut lanes = ImageBuf::new(w, h);
    lanes.data_mut().par_chunks_exact_mut(4).zip(src.par_iter()).for_each(|(l, &v)| l.fill(v));
    drop(src);
    let blurred = ops::blur(&lanes, sigma);
    blurred.data().par_chunks_exact(4).map(|p| p[0]).collect()
}

/// Unsharp mask on a logarithmic luminance axis.
///
/// With `L = ln(max(Y, 0) + 0.02)` (about 3 stops below middle grey, so shadow noise stays
/// compressed) the detail is `d = L - G(L)`, `G` a Gaussian of sigma `radius_px`. Fractional
/// radii work smoothly: below 4 px a true Gaussian (taps to 3 sigma) is used, above that the
/// core box-blur approximation (whole-pixel radius). The detail is then
///
/// * cored by the threshold: `w = smoothstep(0.25 t, 0.75 t, |d|)` with `t = threshold`
///   (`ln` units, i.e. 0.2 suppresses detail below about 0.05 ln = 7 % contrast and passes
///   above about 0.15 ln; `threshold` 0 = no coring), and
/// * soft-limited: `d' = 0.4 s / (0.4 + |s|)` with `s = 2 d`, which has slope 2 for small `d` and
///   saturates at +-0.4 ln, so strong edges get bounded halos while fine texture is
///   enhanced at full strength.
///
/// RGB of the pixel is multiplied by `exp(amount * m * w * d')` (`m` the mask value, 1
/// without mask), i.e. the change is a multiplicative gain on luminance with hue and
/// saturation kept. Light and dark halos are therefore symmetric in appearance, luminance
/// can never become negative and the gain is bounded by `exp(+-0.4 amount)`; the overshoot at
/// an edge grows monotonically with `amount` (proportional for small amounts). Pixels with
/// `Y <= 0` stay unchanged; alpha is untouched; `amount` 0 or `radius_px <= 0` returns a clone.
pub fn sharpen(img: &ImageBuf, amount: f32, radius_px: f32, threshold: f32, mask: Option<&MaskBuf>) -> ImageBuf {
    let amount = finite_or(amount, 0.0);
    let radius = finite_or(radius_px, 0.0);
    if amount == 0.0 || radius <= 0.0 || img.width() == 0 || img.height() == 0 {
        return img.clone();
    }
    let threshold = finite_or(threshold, 0.0).max(0.0);
    let (w, h) = (img.width(), img.height());

    let mut log_y = vec![0.0f32; w as usize * h as usize];
    log_y.par_iter_mut().zip(img.data().par_chunks_exact(4)).for_each(|(l, px)| {
        *l = (luma(px[0], px[1], px[2]).max(0.0) + EPS).ln();
    });
    let blurred = blur_plane(log_y.clone(), w, h, radius);

    let plane = mask.map(|m| mask_plane(m, w, h));
    let (t0, t1) = (0.25 * threshold, 0.75 * threshold);
    let mut out = img.clone();
    out.data_mut()
        .par_chunks_exact_mut(4)
        .zip(log_y.par_iter().zip(blurred.par_iter()))
        .enumerate()
        .for_each(|(i, (px, (&l, &b)))| {
            let amt = match &plane {
                Some(p) => amount * c01(p[i]),
                None => amount,
            };
            if amt == 0.0 || luma(px[0], px[1], px[2]) <= 0.0 {
                return;
            }
            let d = l - b;
            let wt = if threshold > 0.0 { smoothstep(t0, t1, d.abs()) } else { 1.0 };
            let scaled = STRENGTH * d;
            let limited = LIMIT * scaled / (LIMIT + scaled.abs());
            let gain = (amt * wt * limited).exp();
            if gain.is_finite() {
                px[0] *= gain;
                px[1] *= gain;
                px[2] *= gain;
            }
        });
    out
}
