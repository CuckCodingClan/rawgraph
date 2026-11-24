//! Exact-size area-average (box) resampling in linear light.

use std::borrow::Cow;

use rawgraph_core::ImageBuf;
use rayon::prelude::*;

/// Output size for a target long edge: aspect ratio preserved, long edge exactly
/// `long_edge`, never larger than the source. Returns the source size when no
/// shrinking is needed.
pub fn target_size(width: u32, height: u32, long_edge: u32) -> (u32, u32) {
    let long = width.max(height);
    if long_edge == 0 || long <= long_edge || width == 0 || height == 0 {
        return (width, height);
    }
    let scale = long_edge as f64 / long as f64;
    let short = |v: u32| ((v as f64 * scale).round() as u32).clamp(1, long_edge);
    if width >= height {
        (long_edge, short(height))
    } else {
        (short(width), long_edge)
    }
}

/// Shrinks `img` so its long edge is exactly `long_edge`; borrows the input when it is already
/// that small (never upscales).
pub fn resize_to_long_edge(img: &ImageBuf, long_edge: u32) -> Cow<'_, ImageBuf> {
    let (w, h) = target_size(img.width(), img.height(), long_edge);
    if (w, h) == (img.width(), img.height()) {
        Cow::Borrowed(img)
    } else {
        Cow::Owned(resize_area(img, w, h))
    }
}

struct Span {
    start: usize,
    weights: Vec<f32>,
}

/// For each destination index the covered source range with fractional edge weights (sum = 1).
fn spans(src: usize, dst: usize) -> Vec<Span> {
    let ratio = src as f64 / dst as f64;
    (0..dst)
        .map(|d| {
            let a = d as f64 * ratio;
            let b = ((d + 1) as f64 * ratio).min(src as f64);
            let first = (a.floor() as usize).min(src - 1);
            let last = (b.ceil() as usize).clamp(first + 1, src);
            let mut w: Vec<f64> = (first..last)
                .map(|i| (b.min((i + 1) as f64) - a.max(i as f64)).max(0.0))
                .collect();
            let sum: f64 = w.iter().sum();
            if sum > 0.0 {
                w.iter_mut().for_each(|v| *v /= sum);
            } else {
                w = vec![1.0];
            }
            Span { start: first, weights: w.into_iter().map(|v| v as f32).collect() }
        })
        .collect()
}

/// Area-average resampling to exactly `dw` x `dh`: every destination pixel is the mean of the
/// source area it covers, with fractional weights at the edges. Works on linear float data, so
/// highlights above 1.0 are averaged correctly. Zero target sizes yield an empty-size buffer.
pub fn resize_area(src: &ImageBuf, dw: u32, dh: u32) -> ImageBuf {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    if dw == 0 || dh == 0 || sw == 0 || sh == 0 {
        return ImageBuf::new(dw, dh);
    }
    if (dw, dh) == (src.width(), src.height()) {
        return src.clone();
    }
    let (dwu, dhu) = (dw as usize, dh as usize);
    let xs = spans(sw, dwu);
    let ys = spans(sh, dhu);
    let data = src.data();
    let mut out = ImageBuf::new(dw, dh);
    out.data_mut().par_chunks_mut(dwu * 4).enumerate().for_each(|(y, dst)| {
        dst.fill(0.0);
        let yspan = &ys[y];
        for (k, &wy) in yspan.weights.iter().enumerate() {
            let sy = yspan.start + k;
            let row = &data[sy * sw * 4..(sy + 1) * sw * 4];
            for (x, xspan) in xs.iter().enumerate() {
                let mut acc = [0.0f32; 4];
                for (j, &wx) in xspan.weights.iter().enumerate() {
                    let i = (xspan.start + j) * 4;
                    for c in 0..4 {
                        acc[c] += row[i + c] * wx;
                    }
                }
                for c in 0..4 {
                    dst[x * 4 + c] += acc[c] * wy;
                }
            }
        }
    });
    out
}
