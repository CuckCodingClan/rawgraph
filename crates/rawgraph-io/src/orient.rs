//! EXIF/TIFF orientation for interleaved RGBA float buffers.

use rayon::prelude::*;

/// True when the orientation swaps width and height (EXIF values 5..=8).
pub fn swaps_axes(orientation: u16) -> bool {
    (5..=8).contains(&orientation)
}

/// Oriented size for an `orientation` (EXIF values 1..=8; anything else is treated as 1).
pub fn oriented_size(w: u32, h: u32, orientation: u16) -> (u32, u32) {
    if swaps_axes(orientation) {
        (h, w)
    } else {
        (w, h)
    }
}

/// Source coordinates for destination pixel (x, y), for a source of size w x h.
#[inline]
fn src_coords(orientation: u16, w: usize, h: usize, x: usize, y: usize) -> (usize, usize) {
    match orientation {
        2 => (w - 1 - x, y),
        3 => (w - 1 - x, h - 1 - y),
        4 => (x, h - 1 - y),
        5 => (y, x),
        6 => (y, h - 1 - x),
        7 => (w - 1 - y, h - 1 - x),
        8 => (w - 1 - y, x),
        _ => (x, y),
    }
}

/// Applies an EXIF orientation (1..=8) to RGBA f32 data; returns the new size and data.
/// Orientation 1 (or an invalid value) returns the input unchanged.
pub fn orient_rgba(w: u32, h: u32, data: Vec<f32>, orientation: u16) -> (u32, u32, Vec<f32>) {
    if !(2..=8).contains(&orientation) {
        return (w, h, data);
    }
    let (ow, oh) = oriented_size(w, h, orientation);
    let (sw, sh) = (w as usize, h as usize);
    let mut out = vec![0.0f32; data.len()];
    out.par_chunks_mut(ow as usize * 4).enumerate().for_each(|(y, row)| {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (sx, sy) = src_coords(orientation, sw, sh, x, y);
            let i = (sy * sw + sx) * 4;
            px.copy_from_slice(&data[i..i + 4]);
        }
    });
    (ow, oh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 3x2 image whose pixel value encodes its own coordinates.
    fn sample() -> Vec<f32> {
        let mut d = Vec::new();
        for y in 0..2 {
            for x in 0..3 {
                d.extend_from_slice(&[x as f32, y as f32, 0.0, 1.0]);
            }
        }
        d
    }

    fn at(d: &[f32], w: usize, x: usize, y: usize) -> (f32, f32) {
        let i = (y * w + x) * 4;
        (d[i], d[i + 1])
    }

    #[test]
    fn rotate90_cw_moves_bottom_left_to_top_left() {
        let (w, h, d) = orient_rgba(3, 2, sample(), 6);
        assert_eq!((w, h), (2, 3));
        assert_eq!(at(&d, 2, 0, 0), (0.0, 1.0)); // was bottom-left
        assert_eq!(at(&d, 2, 1, 0), (0.0, 0.0)); // was top-left
        assert_eq!(at(&d, 2, 1, 2), (2.0, 0.0)); // was top-right
    }

    #[test]
    fn rotate270_and_mirrors() {
        let (w, h, d) = orient_rgba(3, 2, sample(), 8);
        assert_eq!((w, h), (2, 3));
        assert_eq!(at(&d, 2, 0, 0), (2.0, 0.0)); // top-right becomes top-left
        let (_, _, d) = orient_rgba(3, 2, sample(), 2);
        assert_eq!(at(&d, 3, 0, 0), (2.0, 0.0));
        let (_, _, d) = orient_rgba(3, 2, sample(), 3);
        assert_eq!(at(&d, 3, 0, 0), (2.0, 1.0));
        let (_, _, d) = orient_rgba(3, 2, sample(), 4);
        assert_eq!(at(&d, 3, 0, 0), (0.0, 1.0));
    }

    #[test]
    fn transpose_and_transverse() {
        let (w, h, d) = orient_rgba(3, 2, sample(), 5);
        assert_eq!((w, h), (2, 3));
        assert_eq!(at(&d, 2, 1, 0), (0.0, 1.0));
        let (_, _, d) = orient_rgba(3, 2, sample(), 7);
        assert_eq!(at(&d, 2, 0, 0), (2.0, 1.0));
    }

    #[test]
    fn identity_for_normal_and_invalid() {
        for o in [0u16, 1, 9] {
            let (w, h, d) = orient_rgba(3, 2, sample(), o);
            assert_eq!((w, h, d), (3, 2, sample()));
        }
    }
}
