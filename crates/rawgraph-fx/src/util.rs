//! Shared helpers: smoothstep, mask resampling, hue/chroma decomposition, bilinear sampling.

use std::borrow::Cow;

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::MaskBuf;

/// ACEScg luminance.
#[inline]
pub(crate) fn luma(r: f32, g: f32, b: f32) -> f32 {
    AP1_LUMA[0] * r + AP1_LUMA[1] * g + AP1_LUMA[2] * b
}

/// Clamps to 0..1; NaN becomes 0 (`f32::clamp` would keep NaN).
#[allow(clippy::manual_clamp)]
#[inline]
pub(crate) fn c01(x: f32) -> f32 {
    x.max(0.0).min(1.0)
}

/// Replaces a non-finite parameter by `fallback`.
#[inline]
pub(crate) fn finite_or(x: f32, fallback: f32) -> f32 {
    if x.is_finite() {
        x
    } else {
        fallback
    }
}

/// Hermite smoothstep. Degenerate edges (`edge1 <= edge0`) give a hard step at `edge0`.
#[inline]
pub(crate) fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x >= edge0 { 1.0 } else { 0.0 };
    }
    let t = c01((x - edge0) / (edge1 - edge0));
    t * t * (3.0 - 2.0 * t)
}

/// Perceptual axis value `t = Y / (Y + 0.18)` of a luminance (negative luminance counts as 0).
#[inline]
pub(crate) fn perceptual(y: f32) -> f32 {
    let y = y.max(0.0);
    y / (y + 0.18)
}

/// Hue (degrees, 0..360) and chroma of an RGB triple.
///
/// Chroma vector: `a = r - (g + b) / 2`, `b = (sqrt(3) / 2) (g - b)`. Both are
/// unchanged when the same grey is added to all channels, so this is the
/// chroma of the colour around its luminance axis (`rgb - Y`) for any grey
/// reference. Hue is `atan2(b, a)`: 0 deg red, 60 yellow, 120 green, 180 cyan,
/// 240 blue, 300 magenta of the ACEScg primaries' hexagon. Neutrals have chroma
/// exactly 0 and hue 0.
#[cfg(test)]
#[inline]
pub(crate) fn hue_chroma(r: f32, g: f32, b: f32) -> (f32, f32) {
    let (a, bb) = chroma_vec(r, g, b);
    let c = a.hypot(bb);
    if c > 0.0 {
        let h = bb.atan2(a).to_degrees();
        (if h < 0.0 { h + 360.0 } else { h }, c)
    } else {
        (0.0, 0.0)
    }
}

pub(crate) const SQRT3_2: f32 = 0.866_025_4;

#[inline]
pub(crate) fn chroma_vec(r: f32, g: f32, b: f32) -> (f32, f32) {
    (r - 0.5 * (g + b), SQRT3_2 * (g - b))
}

/// Mask values for an image of `width` x `height`: borrowed when the size matches,
/// otherwise resampled bilinearly. A malformed mask (data length != size) counts as all zero.
pub(crate) fn mask_plane(mask: &MaskBuf, width: u32, height: u32) -> Cow<'_, [f32]> {
    if mask.width == width && mask.height == height && mask.data.len() == width as usize * height as usize {
        Cow::Borrowed(&mask.data)
    } else {
        Cow::Owned(crate::masks::resize_mask(mask, width, height).data)
    }
}

/// Bilinear sample of an interleaved RGBA f32 buffer at pixel-index coordinates
/// (pixel centres at integers), clamped to the edge.
#[inline]
pub(crate) fn bilinear(data: &[f32], w: usize, h: usize, x: f32, y: f32) -> [f32; 4] {
    let x = x.max(0.0).min((w - 1) as f32);
    let y = y.max(0.0).min((h - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let at = |xx: usize, yy: usize| {
        let i = (yy * w + xx) * 4;
        [data[i], data[i + 1], data[i + 2], data[i + 3]]
    };
    let (p00, p10, p01, p11) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
    let mut o = [0.0f32; 4];
    for c in 0..4 {
        let top = p00[c] + (p10[c] - p00[c]) * fx;
        let bot = p01[c] + (p11[c] - p01[c]) * fx;
        o[c] = top + (bot - top) * fy;
    }
    o
}

/// Fully transparent black image (`ImageBuf::new` is opaque black).
pub(crate) fn transparent(width: u32, height: u32) -> rawgraph_core::ImageBuf {
    rawgraph_core::ImageBuf::from_rgba(width, height, vec![0.0; width as usize * height as usize * 4])
        .unwrap_or_else(|| rawgraph_core::ImageBuf::new(0, 0))
}
