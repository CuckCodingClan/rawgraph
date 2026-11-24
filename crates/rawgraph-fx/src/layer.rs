//! Layer compositing: placing a layer on a canvas (fit modes, resampling) and blending it.

use rawgraph_core::{BlendMode, FitMode, ImageBuf, ResampleFilter};
use rayon::prelude::*;

use crate::util::transparent;

/// Largest canvas (pixels) that `place_layer` will allocate; larger requests give an empty image.
const MAX_CANVAS_PIXELS: u64 = 1 << 30;

/// One output coordinate of one axis: up to four source taps with weights and the coverage of
/// the output pixel by the layer rectangle.
#[derive(Clone, Copy, Default)]
struct Tap {
    idx: [u32; 4],
    w: [f32; 4],
    n: u8,
    cov: f32,
}

/// Catmull-Rom weights for the taps at -1, 0, 1, 2 around the sample position, fraction `f`.
#[inline]
fn catmull_rom(f: f64) -> [f64; 4] {
    let (f2, f3) = (f * f, f * f * f);
    [
        0.5 * (-f3 + 2.0 * f2 - f),
        0.5 * (3.0 * f3 - 5.0 * f2 + 2.0),
        0.5 * (-3.0 * f3 + 4.0 * f2 + f),
        0.5 * (f3 - f2),
    ]
}

/// Tap table of one axis.
///
/// * `n_out`: number of output pixels (canvas size on this axis),
/// * `off`, `len`: position and length of the layer on the canvas (canvas pixels, continuous),
/// * `scale`: canvas pixels per source pixel of the (possibly prefiltered) source,
/// * `src_len`: size of the source on this axis.
fn taps(n_out: u32, off: f64, len: f64, scale: f64, src_len: u32, filter: ResampleFilter) -> Vec<Tap> {
    let last = src_len as i64 - 1;
    let clampi = |i: i64| i.clamp(0, last) as u32;
    (0..n_out)
        .map(|k| {
            let lo = (k as f64).max(off);
            let hi = (k as f64 + 1.0).min(off + len);
            let cov = (hi - lo).clamp(0.0, 1.0) as f32;
            let mut t = Tap { cov, ..Tap::default() };
            if cov <= 0.0 {
                return t;
            }
            // Source position in pixel-index space (pixel centres at integers).
            let v = (k as f64 + 0.5 - off) / scale - 0.5;
            match filter {
                ResampleFilter::Nearest => {
                    t.idx[0] = clampi((v + 0.5).floor() as i64);
                    t.w[0] = 1.0;
                    t.n = 1;
                }
                ResampleFilter::Linear => {
                    let i0 = v.floor();
                    let f = v - i0;
                    let i0 = i0 as i64;
                    if f > 1e-7 {
                        t.idx[0] = clampi(i0);
                        t.idx[1] = clampi(i0 + 1);
                        t.w[0] = (1.0 - f) as f32;
                        t.w[1] = f as f32;
                        t.n = 2;
                    } else {
                        t.idx[0] = clampi(i0);
                        t.w[0] = 1.0;
                        t.n = 1;
                    }
                }
                ResampleFilter::Bicubic => {
                    let i0 = v.floor();
                    let f = v - i0;
                    let i0 = i0 as i64;
                    let w = catmull_rom(f);
                    for (j, wj) in w.iter().enumerate() {
                        if *wj != 0.0 {
                            let n = t.n as usize;
                            t.idx[n] = clampi(i0 - 1 + j as i64);
                            t.w[n] = *wj as f32;
                            t.n += 1;
                        }
                    }
                }
            }
            t
        })
        .collect()
}

/// Box-filter downscale by integer factors (area average of the available pixels).
fn box_down(src: &[f32], w: usize, h: usize, nx: usize, ny: usize) -> (Vec<f32>, usize, usize) {
    let (ow, oh) = (w.div_ceil(nx), h.div_ceil(ny));
    let mut out = vec![0.0f32; ow * oh * 4];
    out.par_chunks_exact_mut(ow * 4).enumerate().for_each(|(j, row)| {
        let (y0, y1) = (j * ny, ((j + 1) * ny).min(h));
        for (i, o) in row.chunks_exact_mut(4).enumerate() {
            let (x0, x1) = (i * nx, ((i + 1) * nx).min(w));
            let mut acc = [0.0f64; 4];
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = (y * w + x) * 4;
                    for c in 0..4 {
                        acc[c] += src[p + c] as f64;
                    }
                }
            }
            let inv = 1.0 / ((y1 - y0) * (x1 - x0)) as f64;
            for c in 0..4 {
                o[c] = (acc[c] * inv) as f32;
            }
        }
    });
    (out, ow, oh)
}

/// Places `layer` on a transparent `canvas_w` x `canvas_h` canvas.
///
/// Returns an RGBA image whose alpha is `coverage * layer alpha`: coverage is the exact area
/// fraction of each canvas pixel inside the layer rectangle (so rectangle edges are
/// anti-aliased), 0 outside; uncovered pixels are transparent black.
///
/// Layer rectangle (centred on the canvas), with `s` = canvas pixels per layer pixel:
///
/// * `Fit`: `s = min(cw / lw, ch / lh)` (whole layer visible, bars at the sides),
/// * `Fill`: `s = max(cw / lw, ch / lh)` (canvas fully covered, the layer is cropped),
/// * `Stretch`: `sx = cw / lw`, `sy = ch / lh`,
/// * `Crop`: `s = crop_scale` (1.0 = one layer pixel per canvas pixel; non-finite or `<= 0` counts as 1).
///
/// Filters: `Nearest` (keeps the exact layer values), `Linear` (bilinear), `Bicubic`
/// (Catmull-Rom). Edge pixels clamp to the layer border. When the layer is minified by more
/// than 2x on an axis it is first box-filtered by the integer factor `floor(1 / s)` on that
/// axis (area average), so a fine pattern averages to grey instead of aliasing. Layers with
/// alpha are filtered premultiplied.
///
/// A zero-sized layer or canvas, or a canvas of more than 2^30 pixels, gives an empty or
/// transparent image (never a panic).
pub fn place_layer(
    layer: &ImageBuf,
    canvas_w: u32,
    canvas_h: u32,
    fit: FitMode,
    filter: ResampleFilter,
    crop_scale: f32,
) -> ImageBuf {
    if canvas_w as u64 * canvas_h as u64 > MAX_CANVAS_PIXELS {
        return ImageBuf::new(0, 0);
    }
    let (lw, lh) = (layer.width() as usize, layer.height() as usize);
    if canvas_w == 0 || canvas_h == 0 || lw == 0 || lh == 0 || layer.data().len() < lw * lh * 4 {
        return transparent(canvas_w, canvas_h);
    }
    let (cw, ch) = (canvas_w as f64, canvas_h as f64);
    let (sx0, sy0) = match fit {
        FitMode::Fit => {
            let s = (cw / lw as f64).min(ch / lh as f64);
            (s, s)
        }
        FitMode::Fill => {
            let s = (cw / lw as f64).max(ch / lh as f64);
            (s, s)
        }
        FitMode::Stretch => (cw / lw as f64, ch / lh as f64),
        FitMode::Crop => {
            let s = if crop_scale.is_finite() && crop_scale > 0.0 { crop_scale as f64 } else { 1.0 };
            (s, s)
        }
    };
    let (rect_w, rect_h) = (lw as f64 * sx0, lh as f64 * sy0);
    let (ox, oy) = ((cw - rect_w) * 0.5, (ch - rect_h) * 0.5);

    // Premultiply only when the layer is not fully opaque.
    let opaque = layer.data().chunks_exact(4).all(|p| p[3] == 1.0);
    let premult: Vec<f32>;
    let src: &[f32] = if opaque {
        layer.data()
    } else {
        premult = layer.data().chunks_exact(4).flat_map(|p| [p[0] * p[3], p[1] * p[3], p[2] * p[3], p[3]]).collect();
        &premult
    };

    // Prefilter strong minification.
    let nx = if sx0 < 0.5 { ((1.0 / sx0).floor() as usize).clamp(1, lw) } else { 1 };
    let ny = if sy0 < 0.5 { ((1.0 / sy0).floor() as usize).clamp(1, lh) } else { 1 };
    let down;
    let (src, sw, sh): (&[f32], usize, usize) = if nx > 1 || ny > 1 {
        down = box_down(src, lw, lh, nx, ny);
        (&down.0, down.1, down.2)
    } else {
        (src, lw, lh)
    };
    let (sx, sy) = (sx0 * nx as f64, sy0 * ny as f64);
    let xt = taps(canvas_w, ox, rect_w, sx, sw as u32, filter);
    let yt = taps(canvas_h, oy, rect_h, sy, sh as u32, filter);

    let mut out = transparent(canvas_w, canvas_h);
    let cwu = canvas_w as usize;
    out.data_mut().par_chunks_exact_mut(cwu * 4).enumerate().for_each(|(y, row)| {
        let ty = &yt[y];
        if ty.cov <= 0.0 {
            return;
        }
        for (x, o) in row.chunks_exact_mut(4).enumerate() {
            let tx = &xt[x];
            let cov = tx.cov * ty.cov;
            if cov <= 0.0 {
                continue;
            }
            let mut acc = [0.0f32; 4];
            for j in 0..ty.n as usize {
                let base = ty.idx[j] as usize * sw;
                let mut racc = [0.0f32; 4];
                for i in 0..tx.n as usize {
                    let p = (base + tx.idx[i] as usize) * 4;
                    let w = tx.w[i];
                    racc[0] += w * src[p];
                    racc[1] += w * src[p + 1];
                    racc[2] += w * src[p + 2];
                    racc[3] += w * src[p + 3];
                }
                let wy = ty.w[j];
                for c in 0..4 {
                    acc[c] += wy * racc[c];
                }
            }
            if opaque {
                o[0] = acc[0];
                o[1] = acc[1];
                o[2] = acc[2];
                o[3] = cov;
            } else {
                let a = acc[3];
                if a > 1e-6 {
                    let inv = 1.0 / a;
                    o[0] = acc[0] * inv;
                    o[1] = acc[1] * inv;
                    o[2] = acc[2] * inv;
                    o[3] = a.min(1.0) * cov;
                }
            }
        }
    });
    out
}

/// Blend function of one channel: `b` = canvas (base), `l` = layer.
///
/// * Normal `l`, Add `b + l`, Subtract `b - l`, Multiply `b * l`, Darken `min`, Lighten `max`,
///   Difference `|b - l|`.
/// * Screen: `b + l - m(b) m(l)` with `m(x) = clamp(x, 0, 1)`. For values in 0..1 this is the
///   classic `1 - (1 - b)(1 - l)`; above 1 the product stops growing, so the result keeps rising
///   instead of collapsing, and below 0 it is plain addition. Continuous and monotonic in both
///   arguments.
/// * Overlay: `2 b l` for `b <= 0.5`, otherwise `l + (2 b - 1)(1 - m(l))`. For `b, l` in 0..1
///   the upper branch equals the classic `1 - 2 (1 - b)(1 - l)`; both branches give `l` at
///   `b = 0.5` for every `l`, so it is continuous, and it is finite for every finite input
///   (a layer value above 1 behaves like 1 in the upper branch).
#[inline]
fn blend_channel(mode: BlendMode, b: f32, l: f32) -> f32 {
    let m = |x: f32| x.clamp(0.0, 1.0);
    match mode {
        BlendMode::Normal => l,
        BlendMode::Add => b + l,
        BlendMode::Subtract => b - l,
        BlendMode::Multiply => b * l,
        BlendMode::Screen => b + l - m(b) * m(l),
        BlendMode::Overlay => {
            if b <= 0.5 {
                2.0 * b * l
            } else {
                l + (2.0 * b - 1.0) * (1.0 - m(l))
            }
        }
        BlendMode::Darken => b.min(l),
        BlendMode::Lighten => b.max(l),
        BlendMode::Difference => (b - l).abs(),
    }
}

/// Blends `placed` (from [`place_layer`]) onto `canvas`.
///
/// Per colour channel `out = base + a * (B(base, layer) - base)` with `a = placed.alpha *
/// opacity` (opacity clamped to 0..1, non-finite = 1, alpha clamped to 0..1); at `a >= 1` the
/// result is exactly `B(base, layer)` (so Normal at full opacity and coverage gives the layer
/// bit for bit). The canvas alpha is unchanged. A result that is not finite keeps the base value.
/// `B` is documented at the private `blend_channel` (Screen `b + l - m(b) m(l)`, Overlay
/// `2 b l` below 0.5, otherwise `l + (2 b - 1)(1 - m(l))`, `m` = clamp to 0..1).
///
/// If the two images differ in size nothing is blended.
pub fn blend_layer(canvas: &mut ImageBuf, placed: &ImageBuf, blend: BlendMode, opacity: f32) {
    if canvas.width() != placed.width() || canvas.height() != placed.height() {
        return;
    }
    let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
    if opacity == 0.0 {
        return;
    }
    let src = placed.data();
    canvas.data_mut().par_chunks_exact_mut(4).zip(src.par_chunks_exact(4)).for_each(|(px, lp)| {
        let a = lp[3].clamp(0.0, 1.0) * opacity;
        if a.is_nan() || a <= 0.0 {
            return;
        }
        for c in 0..3 {
            let base = px[c];
            let b = blend_channel(blend, base, lp[c]);
            let out = if a >= 1.0 { b } else { base + a * (b - base) };
            if out.is_finite() {
                px[c] = out;
            }
        }
    });
}
