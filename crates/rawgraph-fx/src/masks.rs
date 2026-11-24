//! Mask generators and mask arithmetic.

use rawgraph_core::{ImageBuf, MaskBuf};
use rayon::prelude::*;

use crate::util::{c01, chroma_vec, finite_or, luma, perceptual, smoothstep};

/// Bilinear resize in normalised coordinates (pixel centres), edge-clamped. The same size
/// returns a clone. A zero-sized or malformed source gives an all-zero mask.
pub fn resize_mask(mask: &MaskBuf, width: u32, height: u32) -> MaskBuf {
    if mask.width == width && mask.height == height && mask.data.len() == width as usize * height as usize {
        return mask.clone();
    }
    let mut out = MaskBuf::new(width, height, 0.0);
    let (sw, sh) = (mask.width as usize, mask.height as usize);
    if width == 0 || height == 0 || sw == 0 || sh == 0 || mask.data.len() != sw * sh {
        return out;
    }
    let (sx, sy) = (sw as f32 / width as f32, sh as f32 / height as f32);
    out.data.par_chunks_mut(width as usize).enumerate().for_each(|(y, row)| {
        let fy = ((y as f32 + 0.5) * sy - 0.5).max(0.0).min((sh - 1) as f32);
        let (y0, fy_frac) = (fy as usize, fy - fy.floor());
        let y1 = (y0 + 1).min(sh - 1);
        for (x, o) in row.iter_mut().enumerate() {
            let fx = ((x as f32 + 0.5) * sx - 0.5).max(0.0).min((sw - 1) as f32);
            let (x0, fx_frac) = (fx as usize, fx - fx.floor());
            let x1 = (x0 + 1).min(sw - 1);
            let top = mask.data[y0 * sw + x0] + (mask.data[y0 * sw + x1] - mask.data[y0 * sw + x0]) * fx_frac;
            let bot = mask.data[y1 * sw + x0] + (mask.data[y1 * sw + x1] - mask.data[y1 * sw + x0]) * fx_frac;
            *o = top + (bot - top) * fy_frac;
        }
    });
    out
}

/// Elliptical mask. Centre and radii are fractions of width / height (the radii are
/// measured on their own axis, so equal values give an ellipse that follows the image
/// aspect); `angle_deg` rotates the ellipse clockwise. With `d` the elliptical distance
/// (1 on the boundary) the mask is 1 for `d <= 1 - feather` and falls to 0 at `d = 1`
/// with a smoothstep: `feather` 0 is a hard edge, 1 starts the falloff at the centre.
/// `invert` returns `1 - mask`. A radius <= 0 gives an empty ellipse.
#[allow(clippy::too_many_arguments)]
pub fn radial_mask(
    width: u32,
    height: u32,
    center_x: f32,
    center_y: f32,
    radius_x: f32,
    radius_y: f32,
    feather: f32,
    angle_deg: f32,
    invert: bool,
) -> MaskBuf {
    let mut mask = MaskBuf::new(width, height, 0.0);
    if width == 0 || height == 0 {
        return mask;
    }
    let (w, h) = (width as f32, height as f32);
    let (cx, cy) = (finite_or(center_x, 0.5) * w, finite_or(center_y, 0.5) * h);
    let (ax, ay) = (finite_or(radius_x, 0.0) * w, finite_or(radius_y, 0.0) * h);
    let feather = finite_or(feather, 0.0).clamp(0.0, 1.0);
    let (sin, cos) = finite_or(angle_deg, 0.0).to_radians().sin_cos();
    let empty = ax <= 0.0 || ay <= 0.0;
    mask.data.par_chunks_mut(width as usize).enumerate().for_each(|(y, row)| {
        let dy = y as f32 + 0.5 - cy;
        for (x, o) in row.iter_mut().enumerate() {
            let dx = x as f32 + 0.5 - cx;
            // Rotate into the ellipse frame (the ellipse is rotated clockwise by the angle).
            let u = cos * dx + sin * dy;
            let v = -sin * dx + cos * dy;
            let m = if empty {
                0.0
            } else {
                let d = (u / ax).hypot(v / ay);
                if feather <= 0.0 {
                    if d <= 1.0 {
                        1.0
                    } else {
                        0.0
                    }
                } else {
                    // 1 at d <= 1 - feather, 0 at d >= 1.
                    1.0 - smoothstep(1.0 - feather, 1.0, d)
                }
            };
            *o = if invert { 1.0 - m } else { m };
        }
    });
    mask
}

/// Luminance range mask on the perceptual axis `t = Y / (Y + 0.18)` (0 black, 0.5 mid
/// grey): `smoothstep(low - f/2, low + f/2, t) * (1 - smoothstep(high - f/2, high + f/2, t))`
/// with `f = feather`; feather 0 gives hard edges (`low <= t < high`). `invert` returns
/// `1 - mask`. `low >= high` selects nothing.
pub fn luminance_mask(img: &ImageBuf, low: f32, high: f32, feather: f32, invert: bool) -> MaskBuf {
    let (w, h) = (img.width(), img.height());
    let mut mask = MaskBuf::new(w, h, 0.0);
    let (low, high) = (finite_or(low, 0.0), finite_or(high, 1.0));
    let half = finite_or(feather, 0.0).max(0.0) * 0.5;
    mask.data.par_iter_mut().zip(img.data().par_chunks_exact(4)).for_each(|(o, px)| {
        let t = perceptual(luma(px[0], px[1], px[2]));
        let m = if half <= 0.0 {
            if t >= low && t < high {
                1.0
            } else {
                0.0
            }
        } else {
            smoothstep(low - half, low + half, t) * (1.0 - smoothstep(high - half, high + half, t))
        };
        *o = if invert { 1.0 - m } else { m };
    });
    mask
}

/// Floor added to the luminance in the saturation gate of [`hue_mask`].
const GATE_FLOOR: f32 = 0.05;
/// Width of the gate ramp above `min_saturation`.
const GATE_RAMP: f32 = 0.2;

/// Saturation gate of [`hue_mask`]: relative chroma `S = C / (max(Y, 0) + C + 0.05)` (C the
/// chroma-vector length of `hue_saturation`) put through `smoothstep(min_sat, min_sat + 0.2, S)`.
/// Neutral pixels (C = 0) give 0; dark, nearly neutral pixels give 0 because the floor keeps
/// their `S` small however noisy their hue is.
#[inline]
pub(crate) fn hue_gate(c: f32, y: f32, min_sat: f32) -> f32 {
    smoothstep(min_sat, min_sat + GATE_RAMP, c / (y.max(0.0) + c + GATE_FLOOR))
}

/// Hue range mask. Hue is the angle of the chroma vector (see `hue_saturation`), in
/// degrees. The mask is 1 within `width_deg / 2` of `hue_deg` and falls to 0 over a further
/// `feather` degrees (smoothstep, angular distance wrapped). It is multiplied by the
/// saturation gate `hue_gate` (`smoothstep(min_saturation, min_saturation + 0.2, S)` with
/// `S = C / (Y + C + 0.05)`), so neutrals and dark near-neutral pixels (whose hue is noise)
/// are never selected, even with `min_saturation` 0.
///
/// The angle is only computed for pixels inside the feather zone; everything else is decided
/// from the cosine of the angular distance (a dot product).
pub fn hue_mask(img: &ImageBuf, hue_deg: f32, width_deg: f32, feather: f32, min_saturation: f32) -> MaskBuf {
    let (w, h) = (img.width(), img.height());
    let mut mask = MaskBuf::new(w, h, 0.0);
    let centre = finite_or(hue_deg, 0.0).rem_euclid(360.0);
    let half = (finite_or(width_deg, 0.0) * 0.5).max(0.0);
    let feather = finite_or(feather, 0.0).max(0.0);
    let min_sat = finite_or(min_saturation, 0.0).max(0.0);
    let outer = half + feather;
    let (sin0, cos0) = centre.to_radians().sin_cos();
    // Cosines of the inner / outer angles; None = never reached (>= 180 degrees).
    let cos_in = (half < 180.0).then(|| half.to_radians().cos());
    let cos_out = (outer < 180.0).then(|| outer.to_radians().cos());
    mask.data.par_iter_mut().zip(img.data().par_chunks_exact(4)).for_each(|(o, px)| {
        let (r, g, b) = (px[0], px[1], px[2]);
        let (a, bb) = chroma_vec(r, g, b);
        let c = (a * a + bb * bb).sqrt();
        if c <= 0.0 || !c.is_finite() {
            return;
        }
        let gate = hue_gate(c, luma(r, g, b), min_sat);
        if gate <= 0.0 {
            return;
        }
        let inv = 1.0 / c;
        let (x, y) = (a * inv, bb * inv);
        let dot = x * cos0 + y * sin0;
        let m = match cos_in {
            None => 1.0,
            Some(ci) if dot >= ci => 1.0,
            Some(_) if feather <= 0.0 => 0.0,
            Some(_) => match cos_out {
                Some(co) if dot <= co => 0.0,
                _ => {
                    let cross = x * sin0 - y * cos0;
                    let d = cross.abs().atan2(dot).to_degrees();
                    1.0 - smoothstep(half, outer, d)
                }
            },
        };
        *o = m * gate;
    });
    mask
}

/// Remaps mask values: `m' = clamp((m - low) / (high - low), 0, 1) ^ gamma`, then
/// optionally `1 - m'`. `low >= high` is a hard threshold (`m >= low` gives 1). `gamma`
/// below 1e-4 is raised to 1e-4. Input values are clamped to 0..1 first.
pub fn mask_adjust(mask: &MaskBuf, low: f32, high: f32, gamma: f32, invert: bool) -> MaskBuf {
    let (low, high) = (finite_or(low, 0.0), finite_or(high, 1.0));
    let gamma = finite_or(gamma, 1.0).max(1e-4);
    let mut out = mask.clone();
    out.data.par_iter_mut().for_each(|v| {
        let m = c01(*v);
        let n = if high <= low {
            if m >= low {
                1.0
            } else {
                0.0
            }
        } else {
            let t = c01((m - low) / (high - low));
            if gamma == 1.0 {
                t
            } else {
                t.powf(gamma)
            }
        };
        *v = if invert { 1.0 - n } else { n };
    });
    out
}

/// How [`mask_combine`] merges two masks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombineMode {
    Multiply,
    Add,
    Subtract,
    Maximum,
    Minimum,
}

impl CombineMode {
    /// Index order: Multiply, Add, Subtract, Maximum, Minimum.
    pub fn from_index(i: usize) -> Option<CombineMode> {
        [CombineMode::Multiply, CombineMode::Add, CombineMode::Subtract, CombineMode::Maximum, CombineMode::Minimum]
            .get(i)
            .copied()
    }
}

/// Combines two masks (`a * b`, `a + b`, `a - b`, max, min) after clamping both to 0..1;
/// the result is clamped to 0..1 and has the size of `a` (`b` is resampled bilinearly if
/// its size differs).
pub fn mask_combine(a: &MaskBuf, b: &MaskBuf, mode: CombineMode) -> MaskBuf {
    let resized;
    let bd: &[f32] = if b.width == a.width && b.height == a.height && b.data.len() == a.data.len() {
        &b.data
    } else {
        resized = resize_mask(b, a.width, a.height);
        &resized.data
    };
    let mut out = a.clone();
    out.data.par_iter_mut().zip(bd.par_iter()).for_each(|(x, &y)| {
        let (p, q) = (c01(*x), c01(y));
        *x = c01(match mode {
            CombineMode::Multiply => p * q,
            CombineMode::Add => p + q,
            CombineMode::Subtract => p - q,
            CombineMode::Maximum => p.max(q),
            CombineMode::Minimum => p.min(q),
        });
    });
    out
}
