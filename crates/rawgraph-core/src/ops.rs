//! CPU reference implementations of the pixel operations.
//!
//! All operations work on scene-referred linear ACEScg, never clamp, and leave
//! alpha untouched.

// Pixels are addressed as 4-float chunks of runtime-sized slices throughout; the
// `as_chunks` rewrite clippy suggests would not make that clearer.
#![allow(clippy::chunks_exact_to_as_chunks)]

use rayon::prelude::*;

use crate::color::AP1_LUMA;
use crate::image::{ImageBuf, MaskBuf};

/// Multiplies RGB by `gain` (exposure in linear light).
pub fn apply_gain(img: &mut ImageBuf, gain: f32) {
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        px[0] *= gain;
        px[1] *= gain;
        px[2] *= gain;
    });
}

/// Applies a 3x3 matrix to RGB.
pub fn apply_matrix(img: &mut ImageBuf, m: [[f32; 3]; 3]) {
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let (r, g, b) = (px[0], px[1], px[2]);
        px[0] = m[0][0] * r + m[0][1] * g + m[0][2] * b;
        px[1] = m[1][0] * r + m[1][1] * g + m[1][2] * b;
        px[2] = m[2][0] * r + m[2][1] * g + m[2][2] * b;
    });
}

/// Scales chroma around ACEScg luminance. 1.0 is neutral, 0.0 is monochrome.
pub fn saturation(img: &mut ImageBuf, amount: f32) {
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        for c in &mut px[..3] {
            *c = y + (*c - y) * amount;
        }
    });
}

/// Linear gradient mask. `angle_deg` 0 runs left to right, 90 top to bottom.
/// `center` is where the mask reaches 0.5 along that direction and `width`
/// the length of the transition, both as a fraction of the image.
pub fn linear_gradient_mask(width: u32, height: u32, angle_deg: f32, center: f32, transition: f32) -> MaskBuf {
    let (sin, cos) = angle_deg.to_radians().sin_cos();
    let mut mask = MaskBuf::new(width, height, 0.0);
    let (w, h) = (width as f32, height as f32);
    mask.data.par_chunks_mut(width.max(1) as usize).enumerate().for_each(|(y, row)| {
        let v = (y as f32 + 0.5) / h - 0.5;
        for (x, out) in row.iter_mut().enumerate() {
            let u = (x as f32 + 0.5) / w - 0.5;
            let along = u * cos + v * sin + 0.5;
            *out = if transition <= f32::EPSILON {
                if along >= center { 1.0 } else { 0.0 }
            } else {
                ((along - center) / transition + 0.5).clamp(0.0, 1.0)
            };
        }
    });
    mask
}

/// Width (in pixels) of the column strips used by the vertical pass. A strip of
/// the whole image height stays cache-resident while three box passes run on it.
const STRIP: usize = 16;

/// One box pass along a line of `len` samples, each made of `lanes` interleaved
/// values (clamped edges). `src`/`dst` address sample `i` at `off + i * stride`.
/// Sums are accumulated in `f64` so long running windows do not drift; storage
/// stays `f32`.
#[allow(clippy::too_many_arguments)]
fn box_pass(
    src: &[f32],
    src_stride: usize,
    src_off: usize,
    dst: &mut [f32],
    dst_stride: usize,
    dst_off: usize,
    lanes: usize,
    len: usize,
    radius: usize,
    acc: &mut Vec<f64>,
) {
    let n = (2 * radius + 1) as f64;
    let last = len - 1;
    acc.clear();
    acc.resize(lanes, 0.0);
    let at = |i: usize| &src[src_off + i * src_stride..src_off + i * src_stride + lanes];
    for (a, v) in acc.iter_mut().zip(at(0)) {
        *a = f64::from(*v) * (radius + 1) as f64;
    }
    for i in 1..=radius {
        for (a, v) in acc.iter_mut().zip(at(i.min(last))) {
            *a += f64::from(*v);
        }
    }
    for x in 0..len {
        let o = dst_off + x * dst_stride;
        for (d, a) in dst[o..o + lanes].iter_mut().zip(acc.iter()) {
            *d = (*a / n) as f32;
        }
        let (add, sub) = (at((x + radius + 1).min(last)), at(x.saturating_sub(radius)));
        for ((a, p), q) in acc.iter_mut().zip(add).zip(sub) {
            *a += f64::from(*p) - f64::from(*q);
        }
    }
}

/// Same as [`box_pass`] with a compile-time lane count, so the inner loops
/// unroll and vectorise. `src`/`dst` use the same addressing.
#[allow(clippy::too_many_arguments)]
fn box_pass_n<const L: usize>(
    src: &[f32],
    src_stride: usize,
    src_off: usize,
    dst: &mut [f32],
    dst_stride: usize,
    dst_off: usize,
    len: usize,
    radius: usize,
) {
    let n = (2 * radius + 1) as f64;
    let inv = 1.0 / n;
    let last = len - 1;
    let at = |i: usize| -> &[f32; L] {
        let o = src_off + i * src_stride;
        src[o..o + L].try_into().expect("lane count matches")
    };
    let mut acc = [0.0f64; L];
    for (a, v) in acc.iter_mut().zip(at(0)) {
        *a = f64::from(*v) * (radius + 1) as f64;
    }
    for i in 1..=radius {
        let v = at(i.min(last));
        for k in 0..L {
            acc[k] += f64::from(v[k]);
        }
    }
    for x in 0..len {
        let o = dst_off + x * dst_stride;
        let d: &mut [f32; L] = (&mut dst[o..o + L]).try_into().expect("lane count matches");
        for k in 0..L {
            d[k] = (acc[k] * inv) as f32;
        }
        let (add, sub) = (at((x + radius + 1).min(last)), at(x.saturating_sub(radius)));
        for k in 0..L {
            acc[k] += f64::from(add[k]) - f64::from(sub[k]);
        }
    }
}

type Range4 = ([f32; 4], [f32; 4]);

const EMPTY_RANGE: Range4 = ([f32::INFINITY; 4], [f32::NEG_INFINITY; 4]);

/// Per-channel value range of a run of RGBA pixels; a box blur can never leave it.
fn channel_range(data: &[f32]) -> Range4 {
    let (mut lo, mut hi) = EMPTY_RANGE;
    for px in data.chunks_exact(4) {
        for c in 0..4 {
            lo[c] = lo[c].min(px[c]);
            hi[c] = hi[c].max(px[c]);
        }
    }
    (lo, hi)
}

fn merge_range((mut lo, mut hi): Range4, (l2, h2): Range4) -> Range4 {
    for c in 0..4 {
        lo[c] = lo[c].min(l2[c]);
        hi[c] = hi[c].max(h2[c]);
    }
    (lo, hi)
}

/// Three box passes in both directions at full resolution.
fn blur_exact(img: &ImageBuf, radius: usize) -> Vec<f32> {
    box_filter(img.data(), img.width() as usize, img.height() as usize, radius, 3)
}

/// `passes` (1 or 3) box passes in both directions over RGBA data.
fn box_filter(data: &[f32], w: usize, h: usize, radius: usize, passes: usize) -> Vec<f32> {
    debug_assert!(passes == 1 || passes == 3);
    let row = w * 4;

    // Horizontal: all three passes per row inside two thread-local row buffers.
    let mut out = vec![0.0f32; data.len()];
    let (lo, hi) = out
        .par_chunks_exact_mut(row)
        .zip(data.par_chunks_exact(row))
        .map_init(
            || (vec![0.0f32; row], vec![0.0f32; row]),
            |(t1, t2), (dst, src)| {
                if passes == 1 {
                    box_pass_n::<4>(src, 4, 0, dst, 4, 0, w, radius);
                } else {
                    box_pass_n::<4>(src, 4, 0, t1, 4, 0, w, radius);
                    box_pass_n::<4>(t1, 4, 0, t2, 4, 0, w, radius);
                    box_pass_n::<4>(t2, 4, 0, dst, 4, 0, w, radius);
                }
                channel_range(src)
            },
        )
        .reduce(|| EMPTY_RANGE, merge_range);

    // Vertical: column strips are gathered into a cache-resident buffer, get
    // three passes and are written back in place.
    struct Shared(*mut f32);
    // SAFETY: every task only touches the columns of its own strip (see below).
    unsafe impl Sync for Shared {}
    let shared = Shared(out.as_mut_ptr());
    let shared = &shared;
    let starts: Vec<usize> = (0..w).step_by(STRIP).collect();
    starts.par_iter().for_each_init(
        || (Vec::new(), Vec::new(), Vec::new()),
        |(l0, l1, acc), &x0| {
            let sw4 = STRIP.min(w - x0) * 4;
            l0.resize(h * sw4, 0.0f32);
            l1.resize(h * sw4, 0.0f32);
            // SAFETY: strips cover disjoint column ranges [x0, x0 + sw) of every
            // row and each strip is handled by exactly one task, so no element
            // is read or written by two tasks. Offsets stay inside `out`
            // (y < h, x0 * 4 + sw4 <= row). `out` is not otherwise used while
            // the tasks run.
            unsafe {
                for y in 0..h {
                    std::ptr::copy_nonoverlapping(shared.0.add(y * row + x0 * 4), l0.as_mut_ptr().add(y * sw4), sw4);
                }
            }
            if sw4 == STRIP * 4 {
                box_pass_n::<{ STRIP * 4 }>(l0, sw4, 0, l1, sw4, 0, h, radius);
                if passes == 3 {
                    box_pass_n::<{ STRIP * 4 }>(l1, sw4, 0, l0, sw4, 0, h, radius);
                    box_pass_n::<{ STRIP * 4 }>(l0, sw4, 0, l1, sw4, 0, h, radius);
                }
            } else {
                box_pass(l0, sw4, 0, l1, sw4, 0, sw4, h, radius, acc);
                if passes == 3 {
                    box_pass(l1, sw4, 0, l0, sw4, 0, sw4, h, radius, acc);
                    box_pass(l0, sw4, 0, l1, sw4, 0, sw4, h, radius, acc);
                }
            }
            for y in 0..h {
                let src = &l1[y * sw4..(y + 1) * sw4];
                // SAFETY: as above.
                let dst = unsafe { std::slice::from_raw_parts_mut(shared.0.add(y * row + x0 * 4), sw4) };
                for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    for c in 0..4 {
                        d[c] = s[c].max(lo[c]).min(hi[c]);
                    }
                }
            }
        },
    );
    out
}

/// Radii from this size on are blurred on a box-downscaled copy and sampled
/// back bilinearly; smaller radii use the exact full-resolution path.
const LOW_RES_MIN_RADIUS: usize = 16;

/// A blurred image that may still live at reduced resolution.
enum Blurred {
    Full(Vec<f32>),
    Low(LowRes),
}

struct LowRes {
    data: Vec<f32>,
    lw: usize,
    /// Per output column / row: the two low-res taps and the weight of the second.
    xt: Vec<(u32, u32, f32)>,
    yt: Vec<(u32, u32, f32)>,
}

impl LowRes {
    #[inline]
    fn sample(&self, x: usize, y: usize) -> [f32; 4] {
        let (x0, x1, fx) = self.xt[x];
        let (y0, y1, fy) = self.yt[y];
        let px = |xx: u32, yy: u32| -> &[f32] {
            let i = (yy as usize * self.lw + xx as usize) * 4;
            &self.data[i..i + 4]
        };
        let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
        let mut out = [0.0f32; 4];
        for k in 0..4 {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bot = c[k] + (d[k] - c[k]) * fx;
            out[k] = top + (bot - top) * fy;
        }
        out
    }
}

impl Blurred {
    #[inline]
    fn at(&self, x: usize, y: usize, w: usize) -> [f32; 4] {
        match self {
            Blurred::Full(data) => {
                let i = (y * w + x) * 4;
                [data[i], data[i + 1], data[i + 2], data[i + 3]]
            }
            Blurred::Low(l) => l.sample(x, y),
        }
    }
}

/// Bilinear taps mapping `n` full-resolution samples onto `ln` low-res samples
/// that are `scale` times coarser. A low-res sample sits at the centre of its
/// block, which for a partial block at the border is not `(i + 0.5) * scale`.
fn taps(n: usize, ln: usize, scale: usize) -> Vec<(u32, u32, f32)> {
    let centre = |i: usize| {
        let (start, end) = (i * scale, ((i + 1) * scale).min(n));
        (start + end - 1) as f32 * 0.5
    };
    let mut i0 = 0usize;
    (0..n)
        .map(|x| {
            while i0 + 1 < ln && centre(i0 + 1) <= x as f32 {
                i0 += 1;
            }
            let i1 = (i0 + 1).min(ln - 1);
            let span = centre(i1) - centre(i0);
            let f = if span > 0.0 { ((x as f32 - centre(i0)) / span).clamp(0.0, 1.0) } else { 0.0 };
            (i0 as u32, i1 as u32, f)
        })
        .collect()
}

/// Mean of each `scale` x `scale` block (blocks at the border may be smaller).
fn box_downscale(img: &ImageBuf, scale: usize) -> ImageBuf {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (lw, lh) = (w.div_ceil(scale), h.div_ceil(scale));
    let src = img.data();
    let mut out = ImageBuf::new(lw as u32, lh as u32);
    out.data_mut().par_chunks_exact_mut(lw * 4).enumerate().for_each_init(
        || vec![0.0f32; lw * 4],
        |acc, (ly, row)| {
            let (y0, y1) = (ly * scale, ((ly + 1) * scale).min(h));
            acc.fill(0.0);
            for y in y0..y1 {
                let line = &src[y * w * 4..(y + 1) * w * 4];
                for (a, block) in acc.chunks_exact_mut(4).zip(line.chunks(scale * 4)) {
                    for p in block.chunks_exact(4) {
                        a[0] += p[0];
                        a[1] += p[1];
                        a[2] += p[2];
                        a[3] += p[3];
                    }
                }
            }
            for (lx, (px, a)) in row.chunks_exact_mut(4).zip(acc.chunks_exact(4)).enumerate() {
                let n = (((lx + 1) * scale).min(w) - lx * scale) * (y1 - y0);
                let inv = 1.0 / n as f32;
                for k in 0..4 {
                    px[k] = a[k] * inv;
                }
            }
        },
    );
    out
}

/// Blurs with the given box radius; `None` means the radius is zero (identity).
fn blurred(img: &ImageBuf, radius: usize, downscaled: &mut Vec<(usize, ImageBuf)>) -> Option<Blurred> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if radius == 0 || w == 0 || h == 0 {
        return None;
    }
    if radius < LOW_RES_MIN_RADIUS {
        return Some(Blurred::Full(blur_exact(img, radius)));
    }
    // Largest power of two that keeps at least 8 low-res pixels of radius.
    let scale = 1usize << (radius / 8).ilog2();
    if !downscaled.iter().any(|(s, _)| *s == scale) {
        downscaled.push((scale, box_downscale(img, scale)));
    }
    let low = &downscaled.iter().find(|(s, _)| *s == scale).expect("just inserted").1;
    let low_radius = (radius as f32 / scale as f32).round().max(1.0) as usize;
    let (lw, lh) = (low.width() as usize, low.height() as usize);
    let data = blur_exact(low, low_radius);
    Some(Blurred::Low(LowRes { data, lw, xt: taps(w, lw, scale), yt: taps(h, lh, scale) }))
}

/// Gaussian-like blur (three box passes). `sigma_px` is in pixels of `img`.
pub fn blur(img: &ImageBuf, sigma_px: f32) -> ImageBuf {
    let radius = sigma_px.round().max(0.0) as usize;
    let (w, h) = (img.width(), img.height());
    match blurred(img, radius, &mut Vec::new()) {
        None => img.clone(),
        Some(level) => {
            let w = w as usize;
            let mut out = vec![0.0f32; w * h as usize * 4];
            out.par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
                for (x, px) in row.chunks_exact_mut(4).enumerate() {
                    px.copy_from_slice(&level.at(x, y, w));
                }
            });
            ImageBuf::from_rgba(img.width(), img.height(), out).expect("blur keeps the image size")
        }
    }
}

/// Number of pre-blurred levels used for spatially varying blur.
const BLUR_LEVELS: usize = 4;

/// Blur whose radius varies per pixel: a mask value of 0 keeps the pixel
/// sharp, 1 blurs with `max_sigma_px`, linear in between. This modulates the
/// parameter itself and is not a blend between sharp and fully blurred.
pub fn blur_modulated(img: &ImageBuf, max_sigma_px: f32, mask: &MaskBuf) -> ImageBuf {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut out = ImageBuf::new(img.width(), img.height());
    if w == 0 || h == 0 {
        return out;
    }
    let mw = mask.width.max(1) as usize;
    let xs: Vec<usize> = (0..w).map(|x| (x as u64 * mask.width as u64 / w as u64) as usize).collect();
    let ys: Vec<usize> = (0..h).map(|y| (y as u64 * mask.height as u64 / h as u64) as usize).collect();
    let level_at = |x: usize, y: usize| -> (usize, usize, f32) {
        let t = mask.data[ys[y] * mw + xs[x]].clamp(0.0, 1.0) * BLUR_LEVELS as f32;
        let lo = (t.floor() as usize).min(BLUR_LEVELS);
        (lo, (lo + 1).min(BLUR_LEVELS), t - lo as f32)
    };

    // Only levels some pixel actually reads are blurred. Level 0 is the input.
    let needed = (0..h)
        .into_par_iter()
        .fold(
            || [false; BLUR_LEVELS + 1],
            |mut needed, y| {
                for x in 0..w {
                    let (lo, hi, f) = level_at(x, y);
                    needed[lo] = true;
                    if f > 0.0 {
                        needed[hi] = true;
                    }
                }
                needed
            },
        )
        .reduce(
            || [false; BLUR_LEVELS + 1],
            |mut a, b| {
                for k in 0..=BLUR_LEVELS {
                    a[k] |= b[k];
                }
                a
            },
        );
    let mut downscaled = Vec::new();
    let levels: Vec<Option<Blurred>> = (0..=BLUR_LEVELS)
        .map(|k| {
            let sigma = max_sigma_px * k as f32 / BLUR_LEVELS as f32;
            if needed[k] { blurred(img, sigma.round().max(0.0) as usize, &mut downscaled) } else { None }
        })
        .collect();
    let fetch = |k: usize, x: usize, y: usize| -> [f32; 4] {
        match &levels[k] {
            Some(level) => level.at(x, y, w),
            None => {
                let i = (y * w + x) * 4;
                let d = &img.data()[i..i + 4];
                [d[0], d[1], d[2], d[3]]
            }
        }
    };

    out.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let (lo, hi, f) = level_at(x, y);
            let a = fetch(lo, x, y);
            if f > 0.0 {
                let b = fetch(hi, x, y);
                for c in 0..4 {
                    px[c] = a[c] + (b[c] - a[c]) * f;
                }
            } else {
                px.copy_from_slice(&a);
            }
        }
    });
    out
}

/// Contrast around a pivot, as a power curve on luminance: pixels keep their
/// chromaticity, `pivot` stays fixed and `amount` +1 doubles the exponent
/// (`Y' = pivot * (Y / pivot)^(2^amount)`). Pixels with Y <= 0 are unchanged,
/// and `amount` 0 is exactly the identity.
pub fn contrast(img: &mut ImageBuf, amount: f32, pivot: f32) {
    if amount == 0.0 {
        return;
    }
    let exponent = amount.exp2();
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        if y > 0.0 {
            let gain = pivot * (y / pivot).powf(exponent) / y;
            px[0] *= gain;
            px[1] *= gain;
            px[2] *= gain;
        }
    });
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Per-pixel highlight and shadow recovery. With `Y = max(luminance, 0)` and
/// `t = Y / (Y + 0.18)` (0.5 at middle grey), highlights act on `t` above 0.5
/// and shadows below it, both weights vanishing at middle grey:
/// `RGB *= 2^(2 * highlights * w_h + 2 * shadows * w_s)`. Both parameters at 0
/// is exactly the identity.
///
/// This is the global version: every pixel is treated on its own. Local
/// adaptation (weights from a blurred neighbourhood) is later work.
pub fn highlights_shadows(img: &mut ImageBuf, highlights: f32, shadows: f32) {
    if highlights == 0.0 && shadows == 0.0 {
        return;
    }
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let y = (AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2]).max(0.0);
        let t = y / (y + 0.18);
        let w_h = smoothstep(0.5, 1.0, t);
        let w_s = 1.0 - smoothstep(0.0, 0.5, t);
        let gain = (2.0 * highlights * w_h + 2.0 * shadows * w_s).exp2();
        px[0] *= gain;
        px[1] *= gain;
        px[2] *= gain;
    });
}

/// Noise level (in the square-root luminance domain) that `luma` 1.0 removes:
/// `eps = (LUMA_SIGMA * luma)^2`. Tuned on ISO 2500 files so that 0.5 clears
/// grain in flat areas while eyelashes, hair and lace texture survive.
const LUMA_SIGMA: f64 = 0.04;

/// Chroma noise is low-frequency, so its blur reaches this many times further
/// than the luminance filter: `sigma = chroma * radius_px * CHROMA_REACH`.
const CHROMA_REACH: f32 = 4.0;

/// Blur with a fractional sigma: the two neighbouring integer radii are
/// blended, so the effect grows smoothly with the strength instead of stepping.
fn blur_smooth(img: &ImageBuf, sigma: f32) -> ImageBuf {
    let sigma = sigma.max(0.0);
    let (lo, frac) = (sigma.floor(), sigma.fract());
    let a = blur(img, lo);
    if frac == 0.0 {
        return a;
    }
    let b = blur(img, lo + 1.0);
    let mut out = a;
    out.data_mut().par_iter_mut().zip(b.data().par_iter()).for_each(|(x, y)| *x += (*y - *x) * frac);
    out
}

/// Denoises luminance and chroma separately.
///
/// The image is split into `Y` (AP1 luminance) and the chroma differences
/// `Cr = R - Y`, `Cb = B - Y`. Chroma is blurred with sigma
/// `chroma * radius_px * CHROMA_REACH` (chroma noise is low-frequency;
/// fractional sigmas are blended, so the strength is smooth). Luminance gets a
/// self-guided filter (He et al.) on `sqrt(max(Y, 0))` with box radius
/// `max(1, round(radius_px))` and `eps = (LUMA_SIGMA * luma)^2`, squared back
/// afterwards. `radius_px` is meant to be small (about 0.3 per mille of the long
/// edge, 2 px on a 7000 px image). Pixels with `Y <= 0`
/// keep their luminance. The result is recombined to RGB (G from the luma
/// weights). Alpha is untouched. `luma` and `chroma` 0 return a clone; with
/// `luma` 0 the luminance is unchanged up to float rounding of the
/// recombination.
pub fn noise_reduction(img: &ImageBuf, luma: f32, chroma: f32, radius_px: f32) -> ImageBuf {
    let (luma, chroma) = (luma.max(0.0), chroma.max(0.0));
    let (w, h) = (img.width() as usize, img.height() as usize);
    if (luma == 0.0 && chroma == 0.0) || w == 0 || h == 0 {
        return img.clone();
    }
    let n = w * h;
    let [wr, wg, wb] = AP1_LUMA;
    let src = img.data();
    let y_of = |px: &[f32]| wr * px[0] + wg * px[1] + wb * px[2];

    // Chroma differences, blurred.
    let chroma_planes: Option<Vec<f32>> = (chroma > 0.0).then(|| {
        let mut planes = vec![0.0f32; n * 4];
        planes.par_chunks_exact_mut(4).zip(src.par_chunks_exact(4)).for_each(|(c, px)| {
            let y = y_of(px);
            c[0] = px[0] - y;
            c[1] = px[2] - y;
        });
        let planes = ImageBuf::from_rgba(w as u32, h as u32, planes).expect("plane layout");
        blur_smooth(&planes, chroma * radius_px * CHROMA_REACH).data().to_vec()
    });

    // Luminance, filtered in the square-root domain.
    let luma_plane: Option<Vec<f32>> = (luma > 0.0).then(|| {
        let radius = (radius_px.round().max(1.0)) as usize;
        let k = LUMA_SIGMA * f64::from(luma);
        let eps = k * k;
        let mut moments = vec![0.0f32; n * 4];
        moments.par_chunks_exact_mut(4).zip(src.par_chunks_exact(4)).for_each(|(m, px)| {
            let g = y_of(px).max(0.0).sqrt();
            m[0] = g;
            m[1] = g * g;
        });
        let mean = box_filter(&moments, w, h, radius, 1);
        // a, b of the linear model g' = a * g + b.
        let mut coeff = vec![0.0f32; n * 4];
        coeff.par_chunks_exact_mut(4).zip(mean.par_chunks_exact(4)).for_each(|(c, m)| {
            let (mg, mgg) = (f64::from(m[0]), f64::from(m[1]));
            let var = (mgg - mg * mg).max(0.0);
            let a = var / (var + eps);
            c[0] = a as f32;
            c[1] = (mg * (1.0 - a)) as f32;
        });
        let mean_coeff = box_filter(&coeff, w, h, radius, 1);
        let mut out = vec![0.0f32; n];
        out.par_iter_mut().zip(src.par_chunks_exact(4)).zip(mean_coeff.par_chunks_exact(4)).for_each(
            |((o, px), c)| {
                let y = y_of(px);
                *o = if y > 0.0 {
                    let g = f64::from(y).sqrt();
                    let filtered = f64::from(c[0]) * g + f64::from(c[1]);
                    (filtered * filtered) as f32
                } else {
                    y
                };
            },
        );
        out
    });

    let mut out = img.clone();
    out.data_mut().par_chunks_exact_mut(4).enumerate().for_each(|(i, px)| {
        let y_old = y_of(px);
        let y = luma_plane.as_ref().map_or(y_old, |p| p[i]);
        let (cr, cb) = match &chroma_planes {
            Some(c) => (c[i * 4], c[i * 4 + 1]),
            None => (px[0] - y_old, px[2] - y_old),
        };
        let (r, b) = (cr + y, cb + y);
        px[0] = r;
        px[1] = (y - wr * r - wb * b) / wg;
        px[2] = b;
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(w: u32, h: u32, rgb: [f32; 3]) -> ImageBuf {
        let mut img = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.set_pixel(x, y, [rgb[0], rgb[1], rgb[2], 1.0]);
            }
        }
        img
    }

    #[test]
    fn gain_scales_rgb_without_clamping_and_keeps_alpha() {
        let mut img = flat(2, 2, [0.5, 1.0, 4.0]);
        apply_gain(&mut img, 4.0);
        assert_eq!(img.pixel(1, 1), [2.0, 4.0, 16.0, 1.0]);
    }

    #[test]
    fn saturation_keeps_luminance_and_neutrals() {
        let mut gray = flat(1, 1, [0.3, 0.3, 0.3]);
        saturation(&mut gray, 2.0);
        let p = gray.pixel(0, 0);
        assert!((p[0] - 0.3).abs() < 1e-6 && (p[2] - 0.3).abs() < 1e-6);

        let mut col = flat(1, 1, [0.8, 0.2, 0.1]);
        let luma = |p: [f32; 4]| AP1_LUMA[0] * p[0] + AP1_LUMA[1] * p[1] + AP1_LUMA[2] * p[2];
        let before = luma(col.pixel(0, 0));
        saturation(&mut col, 0.0);
        let p = col.pixel(0, 0);
        assert!((p[0] - before).abs() < 1e-6 && (p[1] - before).abs() < 1e-6 && (p[2] - before).abs() < 1e-6);
    }

    #[test]
    fn gradient_mask_runs_in_the_requested_direction() {
        let m = linear_gradient_mask(100, 10, 0.0, 0.5, 1.0);
        assert!(m.get(0, 5) < 0.02 && m.get(99, 5) > 0.98);
        assert!((m.get(50, 0) - m.get(50, 9)).abs() < 1e-6);
        let m = linear_gradient_mask(10, 100, 90.0, 0.5, 1.0);
        assert!(m.get(5, 0) < 0.02 && m.get(5, 99) > 0.98);
        let step = linear_gradient_mask(100, 1, 0.0, 0.5, 0.0);
        assert_eq!((step.get(10, 0), step.get(90, 0)), (0.0, 1.0));
    }

    #[test]
    fn blur_preserves_flat_areas_and_total_energy() {
        let img = flat(32, 16, [0.25, 2.0, 0.0]);
        let out = blur(&img, 3.0);
        for (a, b) in img.data().iter().zip(out.data()) {
            assert!((a - b).abs() < 1e-4);
        }

        let mut spot = ImageBuf::new(41, 41);
        spot.set_pixel(20, 20, [100.0, 100.0, 100.0, 1.0]);
        let out = blur(&spot, 2.0);
        let sum: f32 = out.data().iter().step_by(4).sum();
        assert!((sum - 100.0).abs() < 0.05, "{sum}");
        assert!(out.pixel(20, 20)[0] < 100.0 && out.pixel(22, 20)[0] > 0.0);
        let (p, q) = (out.pixel(20, 18)[0], out.pixel(18, 20)[0]);
        assert!((p - q).abs() <= 1e-5 * p.abs().max(q.abs()), "blur must be symmetric in x and y: {p} vs {q}");
    }

    #[test]
    fn zero_radius_blur_is_identity() {
        let mut img = ImageBuf::new(4, 4);
        img.set_pixel(1, 2, [3.0, 2.0, 1.0, 1.0]);
        assert_eq!(blur(&img, 0.2), img);
    }

    #[test]
    fn mask_modulates_blur_radius_per_pixel() {
        // Vertical edge in the middle; the mask is 0 on the top half, 1 on the bottom half.
        let mut img = ImageBuf::new(40, 40);
        for y in 0..40 {
            for x in 20..40 {
                img.set_pixel(x, y, [1.0, 1.0, 1.0, 1.0]);
            }
        }
        let mut mask = MaskBuf::new(40, 40, 0.0);
        for y in 20..40 {
            for x in 0..40 {
                mask.data[y * 40 + x] = 1.0;
            }
        }
        let out = blur_modulated(&img, 4.0, &mask);
        assert_eq!(out.pixel(18, 5)[0], 0.0, "mask 0 keeps the edge sharp");
        assert_eq!(out.pixel(21, 5)[0], 1.0);
        assert!(out.pixel(18, 35)[0] > 0.05, "mask 1 blurs across the edge");
        assert!(out.pixel(21, 35)[0] < 0.95);
        assert_eq!(out.pixel(18, 35), blur(&img, 4.0).pixel(18, 35), "mask 1 equals the full-radius blur");
    }

    #[test]
    fn constant_image_stays_constant_for_large_sigma() {
        let img = flat(300, 200, [0.37, 12.5, 0.001]);
        let out = blur(&img, 60.0);
        for (a, b) in img.data().iter().zip(out.data()) {
            assert!((a - b).abs() <= 1e-5 * a.abs().max(1.0), "{a} vs {b}");
        }
    }

    #[test]
    fn interior_impulse_keeps_its_energy() {
        for sigma in [3.0, 20.0, 60.0] {
            let mut spot = ImageBuf::new(801, 601);
            spot.set_pixel(400, 300, [100.0, 50.0, 25.0, 1.0]);
            let out = blur(&spot, sigma);
            for (c, expect) in [(0usize, 100.0f64), (1, 50.0), (2, 25.0)] {
                let sum: f64 = out.data().iter().skip(c).step_by(4).map(|&v| f64::from(v)).sum();
                assert!((sum - expect).abs() <= 1e-3 * expect, "sigma {sigma} channel {c}: {sum}");
            }
        }
    }

    #[test]
    fn hdr_impulse_blurs_without_negatives_or_nan() {
        let mut spot = ImageBuf::new(257, 129);
        spot.set_pixel(100, 60, [1000.0, 1000.0, 1000.0, 1.0]);
        for sigma in [2.0, 9.0, 60.0] {
            let out = blur(&spot, sigma);
            assert!(out.data().iter().all(|v| v.is_finite() && *v >= 0.0), "sigma {sigma}");
        }
    }

    #[test]
    fn modulated_blur_with_extreme_masks() {
        let mut img = ImageBuf::new(50, 40);
        for (i, v) in img.data_mut().iter_mut().enumerate() {
            *v = ((i * 37) % 101) as f32 / 10.0;
        }
        assert_eq!(blur_modulated(&img, 6.0, &MaskBuf::new(50, 40, 0.0)), img);
        assert_eq!(blur_modulated(&img, 6.0, &MaskBuf::new(25, 20, 1.0)), blur(&img, 6.0));
    }

    fn test_scene(w: u32, h: u32) -> ImageBuf {
        let mut img = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
                let mut v = 0.2 + 0.6 * fx + 0.3 * (fy * 9.0).sin();
                if (x / 97 + y / 61) % 2 == 0 {
                    v += 1.5; // hard-edged checker, HDR
                }
                if x > 8 && y > 8 && (x * 31 + y * 17) % 997 == 0 {
                    v += 40.0; // sparse highlights
                }
                let n = ((x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) % 100) as f32 / 100.0;
                img.set_pixel(x, y, [v + 0.2 * n, v * 0.8, v * 0.5 + 0.1 * n, 1.0]);
            }
        }
        img
    }

    #[test]
    fn low_res_path_stays_close_to_the_exact_path() {
        let img = test_scene(643, 431); // sizes that are not multiples of the scale
        for sigma in [16.0f32, 24.0, 40.0, 60.0] {
            let fast = blur(&img, sigma);
            let exact = ImageBuf::from_rgba(643, 431, blur_exact(&img, sigma as usize)).unwrap();
            let max_ref = exact.data().iter().cloned().fold(0.0f32, f32::max);
            let max_err = fast.data().iter().zip(exact.data()).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            eprintln!("sigma {sigma}: max abs diff {max_err:.5} (peak {max_ref:.2}, {:.3}%)", 100.0 * max_err / max_ref);
            assert!(max_err <= 0.03 * max_ref, "sigma {sigma}: {max_err}");
        }
    }

    fn luma_of(p: [f32; 4]) -> f32 {
        AP1_LUMA[0] * p[0] + AP1_LUMA[1] * p[1] + AP1_LUMA[2] * p[2]
    }

    /// Deterministic noise in -1..1.
    fn noise(i: u32) -> f32 {
        let mut x = i.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
        x = ((x >> ((x >> 28) + 4)) ^ x).wrapping_mul(277_803_737);
        x = (x >> 22) ^ x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn variance(values: &[f32]) -> f32 {
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / values.len() as f32
    }

    #[test]
    fn contrast_is_exact_identity_at_zero_and_fixes_the_pivot() {
        let mut img = flat(3, 2, [0.5, 0.25, 4.0]);
        let before = img.clone();
        contrast(&mut img, 0.0, 0.18);
        assert_eq!(img, before);

        let mut grey = flat(1, 1, [0.18, 0.18, 0.18]);
        contrast(&mut grey, 0.8, 0.18);
        let p = grey.pixel(0, 0);
        assert!((p[0] - 0.18).abs() < 1e-6 && (p[1] - 0.18).abs() < 1e-6 && (p[2] - 0.18).abs() < 1e-6);
    }

    #[test]
    fn contrast_doubles_the_exponent_at_plus_one_and_keeps_neutrals_and_alpha() {
        // 2 x pivot with amount +1: Y' = pivot * 2^2 = 4 x pivot.
        let mut img = flat(1, 1, [0.36, 0.36, 0.36]);
        contrast(&mut img, 1.0, 0.18);
        let p = img.pixel(0, 0);
        for c in &p[..3] {
            assert!((c - 0.72).abs() < 1e-5, "{c}");
        }
        assert_eq!(p[3], 1.0);
        // Amount -1 halves the exponent: 4 x pivot becomes 2 x pivot.
        let mut img = flat(1, 1, [0.72, 0.72, 0.72]);
        contrast(&mut img, -1.0, 0.18);
        assert!((img.pixel(0, 0)[0] - 0.36).abs() < 1e-5);
        // Chromaticity (channel ratios) is preserved.
        let mut col = flat(1, 1, [0.8, 0.4, 0.1]);
        contrast(&mut col, 0.6, 0.18);
        let c = col.pixel(0, 0);
        assert!((c[0] / c[1] - 2.0).abs() < 1e-4 && (c[1] / c[2] - 4.0).abs() < 1e-3);
    }

    #[test]
    fn contrast_leaves_non_positive_luminance_alone() {
        let mut img = ImageBuf::new(3, 1);
        img.set_pixel(0, 0, [-0.5, -0.2, -0.1, 1.0]);
        img.set_pixel(1, 0, [0.0, 0.0, 0.0, 1.0]);
        img.set_pixel(2, 0, [0.5, -2.0, 0.1, 1.0]); // negative luminance overall
        let before = img.clone();
        assert!(luma_of(before.pixel(2, 0)) < 0.0);
        contrast(&mut img, 1.0, 0.18);
        assert_eq!(img, before);
    }

    #[test]
    fn smoothstep_weights_vanish_at_middle_grey() {
        // The weights are exactly 0 at t = 0.5, so mid-grey cannot move.
        assert_eq!(smoothstep(0.5, 1.0, 0.5), 0.0);
        assert_eq!(1.0 - smoothstep(0.0, 0.5, 0.5), 0.0);
        let mut grey = flat(1, 1, [0.18, 0.18, 0.18]);
        let before = grey.clone();
        for (hl, sh) in [(1.0, 1.0), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
            let mut g = grey.clone();
            highlights_shadows(&mut g, hl, sh);
            for c in 0..3 {
                assert!((g.pixel(0, 0)[c] - before.pixel(0, 0)[c]).abs() < 1e-6, "{hl} {sh}");
            }
        }
        highlights_shadows(&mut grey, 0.0, 0.0);
        assert_eq!(grey, before);
    }

    #[test]
    fn highlights_darken_and_shadows_lift_by_about_two_stops() {
        let mut hi = flat(1, 1, [8.0, 8.0, 8.0]);
        highlights_shadows(&mut hi, -1.0, 0.0);
        let stops = (hi.pixel(0, 0)[1] / 8.0).log2();
        assert!((stops + 2.0).abs() < 0.05, "highlights -1 on 8.0: {stops} stops");

        let mut lo = flat(1, 1, [0.005, 0.005, 0.005]);
        highlights_shadows(&mut lo, 0.0, 1.0);
        let stops = (lo.pixel(0, 0)[1] / 0.005).log2();
        assert!((stops - 2.0).abs() < 0.05, "shadows +1 on 0.005: {stops} stops");

        // Shadows do not touch the highlight and vice versa; alpha is untouched.
        let mut hi2 = flat(1, 1, [8.0, 8.0, 8.0]);
        highlights_shadows(&mut hi2, 0.0, 1.0);
        assert!((hi2.pixel(0, 0)[0] / 8.0 - 1.0).abs() < 0.02);
        assert_eq!(hi.pixel(0, 0)[3], 1.0);
    }

    #[test]
    fn highlights_shadows_survive_zero_and_negative_pixels() {
        let mut img = ImageBuf::new(2, 1);
        img.set_pixel(0, 0, [-1.0, -1.0, -1.0, 1.0]);
        img.set_pixel(1, 0, [0.0, 0.0, 0.0, 1.0]);
        highlights_shadows(&mut img, 1.0, 1.0);
        assert!(img.data().iter().all(|v| v.is_finite()));
    }

    fn noisy_patch(w: u32, h: u32, base: [f32; 3], luma_noise: f32, chroma_noise: f32) -> ImageBuf {
        let mut img = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let l = 1.0 + luma_noise * noise(i * 3);
                let (cr, cb) = (chroma_noise * noise(i * 3 + 1), chroma_noise * noise(i * 3 + 2));
                img.set_pixel(x, y, [base[0] * l + cr, base[1] * l, base[2] * l + cb, 1.0]);
            }
        }
        img
    }

    fn interior(img: &ImageBuf, margin: u32, f: impl Fn([f32; 4]) -> f32) -> Vec<f32> {
        let mut v = Vec::new();
        for y in margin..img.height() - margin {
            for x in margin..img.width() - margin {
                v.push(f(img.pixel(x, y)));
            }
        }
        v
    }

    #[test]
    fn noise_reduction_identity_cases() {
        let img = noisy_patch(32, 24, [0.2, 0.3, 0.1], 0.2, 0.05);
        assert_eq!(noise_reduction(&img, 0.0, 0.0, 3.0), img);
        assert_eq!(noise_reduction(&ImageBuf::new(0, 0), 1.0, 1.0, 3.0), ImageBuf::new(0, 0));
    }

    #[test]
    fn luma_denoising_clearly_lowers_variance_and_keeps_the_mean() {
        // Realistic high-ISO grain (about 3.5 % rms) and the default radius at 2560 px (0.8 px).
        let img = noisy_patch(96, 96, [0.2, 0.3, 0.1], 0.06, 0.0);
        let out = noise_reduction(&img, 0.5, 0.0, 0.8);
        let before = interior(&img, 8, luma_of);
        let after = interior(&out, 8, luma_of);
        let factor = variance(&before) / variance(&after);
        eprintln!("luma variance reduction factor {factor:.1}x");
        assert!(factor > 4.0, "factor {factor}");
        let (mb, ma) = (before.iter().sum::<f32>() / before.len() as f32, after.iter().sum::<f32>() / after.len() as f32);
        assert!((mb - ma).abs() <= 0.01 * mb, "mean {mb} -> {ma}");
        assert!(out.data().iter().all(|v| v.is_finite()));
        // Alpha untouched.
        assert!(interior(&out, 0, |p| p[3]).iter().all(|a| *a == 1.0));
    }

    #[test]
    fn luma_denoising_keeps_hard_edges() {
        let (w, h) = (64u32, 32u32);
        let mut img = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 0.05 } else { 0.5 };
                img.set_pixel(x, y, [v, v, v, 1.0]);
            }
        }
        let out = noise_reduction(&img, 0.5, 0.0, 0.8);
        let step_before = img.pixel(w / 2, 10)[1] - img.pixel(w / 2 - 1, 10)[1];
        let step_after = out.pixel(w / 2, 10)[1] - out.pixel(w / 2 - 1, 10)[1];
        eprintln!("edge step kept: {:.1} %", 100.0 * step_after / step_before);
        assert!(step_after >= 0.9 * step_before, "{step_after} vs {step_before}");
        // Flat sides stay flat.
        assert!((out.pixel(5, 10)[1] - 0.05).abs() < 1e-4 && (out.pixel(w - 5, 10)[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn chroma_denoising_removes_colour_noise_and_keeps_luminance() {
        let img = noisy_patch(96, 96, [0.2, 0.3, 0.1], 0.0, 0.05);
        let cr = |p: [f32; 4]| p[0] - luma_of(p);
        let out = noise_reduction(&img, 0.0, 1.0, 0.8);
        let factor = variance(&interior(&img, 8, cr)) / variance(&interior(&out, 8, cr));
        eprintln!("chroma variance reduction factor {factor:.1}x");
        assert!(factor > 4.0, "factor {factor}");
        // Luminance is unchanged up to float rounding of the recombination.
        for (a, b) in img.data().chunks_exact(4).zip(out.data().chunks_exact(4)) {
            assert!((luma_of([a[0], a[1], a[2], 1.0]) - luma_of([b[0], b[1], b[2], 1.0])).abs() <= 1e-6 * a[1].abs().max(1.0));
        }
    }

    #[test]
    fn fine_lines_survive_while_flat_noise_is_removed_at_luma_half() {
        // One-pixel columns alternating 0.2 / 0.25 (a fine, coherent texture)...
        let (w, h) = (64u32, 48u32);
        let mut lines = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = if x % 2 == 0 { 0.2 } else { 0.25 };
                lines.set_pixel(x, y, [v, v, v, 1.0]);
            }
        }
        let out = noise_reduction(&lines, 0.5, 0.0, 0.8);
        let amplitude = |img: &ImageBuf| {
            let col = |x: u32| interior(img, 8, |p| p[1]).iter().skip(x as usize % 2).step_by(2).sum::<f32>();
            (col(1) - col(0)).abs() / (interior(img, 8, |p| p[1]).len() as f32 / 2.0)
        };
        let kept = amplitude(&out) / amplitude(&lines);
        eprintln!("fine line amplitude kept: {:.0} %", 100.0 * kept);
        assert!(kept >= 0.6, "kept {kept}");

        // ...while grain on a flat patch drops clearly at the same settings.
        let flat_noisy = noisy_patch(96, 96, [0.2, 0.3, 0.1], 0.06, 0.0);
        let out = noise_reduction(&flat_noisy, 0.5, 0.0, 0.8);
        let factor = variance(&interior(&flat_noisy, 8, luma_of)) / variance(&interior(&out, 8, luma_of));
        eprintln!("flat noise variance reduction at luma 0.5: {factor:.1}x");
        assert!(factor >= 4.0, "factor {factor}");
    }

    #[test]
    fn chroma_strength_scales_smoothly() {
        let img = noisy_patch(64, 64, [0.2, 0.3, 0.1], 0.0, 0.05);
        let a = noise_reduction(&img, 0.0, 0.30, 2.0);
        let b = noise_reduction(&img, 0.0, 0.35, 2.0);
        assert_ne!(a, b, "neighbouring strengths must differ (no stepping)");
        // The change between the two is small compared with the change from the original.
        let diff = |x: &ImageBuf, y: &ImageBuf| x.data().iter().zip(y.data()).map(|(p, q)| (p - q).abs()).sum::<f32>();
        assert!(diff(&a, &b) < 0.3 * diff(&img, &a), "{} vs {}", diff(&a, &b), diff(&img, &a));
        // Monotone: more strength, less chroma variance.
        let cr = |p: [f32; 4]| p[0] - luma_of(p);
        let mut last = f32::INFINITY;
        for c in [0.0f32, 0.1, 0.2, 0.3, 0.35, 0.5, 0.8] {
            let v = variance(&interior(&noise_reduction(&img, 0.0, c, 2.0), 8, cr));
            assert!(v <= last * 1.001, "chroma {c}: {v} after {last}");
            last = v;
        }
    }

    #[test]
    fn mean_is_preserved_at_default_settings() {
        let img = noisy_patch(96, 96, [0.2, 0.3, 0.1], 0.06, 0.03);
        let out = noise_reduction(&img, 0.5, 0.7, 0.8);
        for c in 0..3 {
            let mean = |i: &ImageBuf| interior(i, 8, |p| p[c]).iter().sum::<f32>() / (80.0 * 80.0);
            let (a, b) = (mean(&img), mean(&out));
            assert!((a - b).abs() <= 0.01 * a.abs(), "channel {c}: {a} -> {b}");
        }
    }

    #[test]
    fn chroma_zero_leaves_chroma_and_noise_reduction_survives_odd_values() {
        let img = noisy_patch(48, 48, [0.2, 0.3, 0.1], 0.2, 0.04);
        let cb = |p: [f32; 4]| p[2] - luma_of(p);
        let out = noise_reduction(&img, 1.0, 0.0, 0.8);
        for (a, b) in img.data().chunks_exact(4).zip(out.data().chunks_exact(4)) {
            let (ca, cb_) = (cb([a[0], a[1], a[2], 1.0]), cb([b[0], b[1], b[2], 1.0]));
            assert!((ca - cb_).abs() <= 1e-5, "{ca} vs {cb_}");
        }

        let mut odd = ImageBuf::new(40, 30);
        for y in 0..30 {
            for x in 0..40 {
                let v = match (x + y) % 5 {
                    0 => 0.0,
                    1 => -0.3,
                    2 => 1.0e6,
                    3 => 3.0e10,
                    _ => 0.18,
                };
                odd.set_pixel(x, y, [v, v * 0.5, -v, 1.0]);
            }
        }
        for (luma, chroma) in [(1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.3, 0.7)] {
            let out = noise_reduction(&odd, luma, chroma, 3.0);
            assert!(out.data().iter().all(|v| v.is_finite()), "luma {luma} chroma {chroma}");
        }
        // Pixels with Y <= 0 keep their luminance when only luma is filtered.
        let out = noise_reduction(&odd, 1.0, 0.0, 3.0);
        for y in 0..30 {
            for x in 0..40 {
                let (a, b) = (odd.pixel(x, y), out.pixel(x, y));
                if luma_of(a) <= 0.0 {
                    assert!((luma_of(a) - luma_of(b)).abs() <= 1e-5 * luma_of(a).abs().max(1.0));
                }
            }
        }
    }

    fn time_runs(label: &str, mut f: impl FnMut()) -> f64 {
        let mut ms: Vec<f64> = (0..10)
            .map(|_| {
                let t = std::time::Instant::now();
                f();
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(|a, b| a.total_cmp(b));
        println!("{label}: min {:.1} ms, median {:.1} ms", ms[0], (ms[4] + ms[5]) / 2.0);
        ms[0]
    }

    #[test]
    #[ignore = "timing; run with --release -- --ignored --nocapture"]
    fn timing_blur_2560x1707() {
        let (w, h) = (2560u32, 1707u32);
        let mut img = ImageBuf::new(w, h);
        for (i, v) in img.data_mut().iter_mut().enumerate() {
            *v = ((i * 2654435761) % 1000) as f32 / 500.0;
        }
        let mask = linear_gradient_mask(w, h, 0.0, 0.5, 1.0);
        time_runs("blur sigma 8", || drop(blur(&img, 8.0)));
        time_runs("blur sigma 60", || drop(blur(&img, 60.0)));
        time_runs("blur_modulated max 60", || drop(blur_modulated(&img, 60.0, &mask)));
        time_runs("noise_reduction luma 0.5 chroma 0.7 default radius 0.8 px", || drop(noise_reduction(&img, 0.5, 0.7, 0.8)));
        time_runs("noise_reduction luma 1.0 chroma 1.0 radius 5 px (heavy)", || drop(noise_reduction(&img, 1.0, 1.0, 5.0)));
        time_runs("noise_reduction luma only", || drop(noise_reduction(&img, 0.6, 0.0, 5.0)));
        time_runs("noise_reduction chroma only", || drop(noise_reduction(&img, 0.0, 0.6, 5.0)));
    }
}
