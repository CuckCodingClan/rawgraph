//! Effects whose parameter is modulated per pixel by a mask.
//!
//! This is parameter modulation, not a blend between original and processed output.
//! A mask of another size is sampled bilinearly in normalised coordinates; mask values
//! are clamped to 0..1.

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::{ImageBuf, MaskBuf};
use rayon::prelude::*;

use crate::util::{c01, finite_or, mask_plane};

/// Exposure whose strength follows the mask: per-pixel gain `2^(ev * m)`.
/// Mask 1 everywhere equals `ops::apply_gain(img, 2^ev)`, mask 0 is the identity.
pub fn gain_masked(img: &mut ImageBuf, ev: f32, mask: &MaskBuf) {
    let ev = finite_or(ev, 0.0);
    if ev == 0.0 {
        return;
    }
    let plane = mask_plane(mask, img.width(), img.height());
    img.data_mut().par_chunks_exact_mut(4).zip(plane.par_iter()).for_each(|(px, &m)| {
        let m = c01(m);
        if m > 0.0 {
            let gain = (ev * m).exp2();
            px[0] *= gain;
            px[1] *= gain;
            px[2] *= gain;
        }
    });
}

/// Contrast whose amount follows the mask: per-pixel amount `amount * m` of
/// `ops::contrast` (power curve on luminance around `pivot`). Mask 1 everywhere equals
/// `ops::contrast`, mask 0 is the identity. A pivot <= 0 is raised to 1e-6.
pub fn contrast_masked(img: &mut ImageBuf, amount: f32, pivot: f32, mask: &MaskBuf) {
    let amount = finite_or(amount, 0.0);
    if amount == 0.0 {
        return;
    }
    let pivot = finite_or(pivot, 0.18).max(1e-6);
    let plane = mask_plane(mask, img.width(), img.height());
    img.data_mut().par_chunks_exact_mut(4).zip(plane.par_iter()).for_each(|(px, &m)| {
        let a = amount * c01(m);
        if a == 0.0 {
            return;
        }
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        if y > 0.0 {
            let gain = pivot * (y / pivot).powf(a.exp2()) / y;
            if gain.is_finite() {
                px[0] *= gain;
                px[1] *= gain;
                px[2] *= gain;
            }
        }
    });
}

/// Saturation whose amount follows the mask: per-pixel amount `1 + (amount - 1) * m` of
/// `ops::saturation` (chroma scaled around ACEScg luminance). Mask 1 everywhere equals
/// `ops::saturation(amount)`, mask 0 is the identity.
pub fn saturation_masked(img: &mut ImageBuf, amount: f32, mask: &MaskBuf) {
    let amount = finite_or(amount, 1.0);
    if amount == 1.0 {
        return;
    }
    let plane = mask_plane(mask, img.width(), img.height());
    img.data_mut().par_chunks_exact_mut(4).zip(plane.par_iter()).for_each(|(px, &m)| {
        let s = 1.0 + (amount - 1.0) * c01(m);
        if s == 1.0 {
            return;
        }
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        for c in &mut px[..3] {
            *c = y + (*c - y) * s;
        }
    });
}
