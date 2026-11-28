//! Resampling for internal scalar image planes.

use rawgraph_core::MaskBuf;
use rayon::prelude::*;

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
