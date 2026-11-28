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

/// Apply a different exposure value at each pixel.
pub fn exposure_mapped(img: &mut ImageBuf, ev: &[f32]) {
    img.data_mut().par_chunks_exact_mut(4).zip(ev).for_each(|(px, ev)| {
        let gain = ev.exp2();
        for c in &mut px[..3] { *c *= gain; }
    });
}

/// Apply a different saturation amount at each pixel.
pub fn saturation_mapped(img: &mut ImageBuf, amounts: &[f32]) {
    img.data_mut().par_chunks_exact_mut(4).zip(amounts).for_each(|(px, &amount)| {
        if amount == 1.0 { return; }
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        for c in &mut px[..3] { *c = y + (*c - y) * amount; }
    });
}

/// Apply a different contrast amount at each pixel around the same pivot.
pub fn contrast_mapped(img: &mut ImageBuf, amounts: &[f32], pivot: f32) {
    img.data_mut().par_chunks_exact_mut(4).zip(amounts).for_each(|(px, &amount)| {
        if amount == 0.0 { return; }
        let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
        if y > 0.0 {
            let gain = pivot * (y / pivot).powf(amount.exp2()) / y;
            for c in &mut px[..3] { *c *= gain; }
        }
    });
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
fn blur_exact(img: &ImageBuf, radius: usize, passes: usize) -> Vec<f32> {
    box_filter(img.data(), img.width() as usize, img.height() as usize, radius, passes)
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
fn blurred(img: &ImageBuf, radius: usize, passes: usize, downscaled: &mut Vec<(usize, ImageBuf)>) -> Option<Blurred> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if radius == 0 || w == 0 || h == 0 {
        return None;
    }
    if radius < LOW_RES_MIN_RADIUS {
        return Some(Blurred::Full(blur_exact(img, radius, passes)));
    }
    // Largest power of two that keeps at least 8 low-res pixels of radius.
    let scale = 1usize << (radius / 8).ilog2();
    if !downscaled.iter().any(|(s, _)| *s == scale) {
        downscaled.push((scale, box_downscale(img, scale)));
    }
    let low = &downscaled.iter().find(|(s, _)| *s == scale).expect("just inserted").1;
    let low_radius = (radius as f32 / scale as f32).round().max(1.0) as usize;
    let (lw, lh) = (low.width() as usize, low.height() as usize);
    let data = blur_exact(low, low_radius, passes);
    Some(Blurred::Low(LowRes { data, lw, xt: taps(w, lw, scale), yt: taps(h, lh, scale) }))
}

/// Gaussian-like blur (three box passes). `sigma_px` is in pixels of `img`.
pub fn blur(img: &ImageBuf, sigma_px: f32) -> ImageBuf {
    filter_low(img, sigma_px, false)
}

/// Low-pass using a single box pass, or the legacy Gaussian approximation.
pub fn filter_low(img: &ImageBuf, sigma_px: f32, box_kernel: bool) -> ImageBuf {
    let passes = if box_kernel { 1 } else { 3 };
    let radius = sigma_px.round().max(0.0) as usize;
    let (w, h) = (img.width(), img.height());
    match blurred(img, radius, passes, &mut Vec::new()) {
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
    filter_low_modulated(img, max_sigma_px, mask, false)
}

/// Radius-modulated low-pass using the selected kernel.
pub fn filter_low_modulated(img: &ImageBuf, max_sigma_px: f32, mask: &MaskBuf, box_kernel: bool) -> ImageBuf {
    let passes = if box_kernel { 1 } else { 3 };
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
            if needed[k] { blurred(img, sigma.round().max(0.0) as usize, passes, &mut downscaled) } else { None }
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

/// Bounded 25-tap spatial filters. Radius is in image pixels, and the mask
/// scales the sampling radius per output pixel. Sampling is bilinear and edges
/// clamp. Lens is a uniform disk; shape is a uniform diamond. Radial filters
/// are centered on the image. Bilateral uses a fixed 0.1 scene-linear RGB range.
/// The fixed tap budget bounds work independently of image size and radius;
/// large radii are therefore approximate, not exact dense convolutions.
pub fn filter_sampled(img: &ImageBuf, radius: f32, kernel: usize, angle: f32, mask: Option<&MaskBuf>) -> ImageBuf {
    filter_sampled_direction(img, radius, kernel, angle, mask, None)
}

/// Motion directions use linear normal-map R/G decoded as XY in [-1, 1].
/// Neutral (0.5, 0.5), non-finite, and near-zero vectors leave that pixel unchanged.
/// The map is nearest-neighbour scaled to the image; its blue and alpha are ignored.
pub fn filter_sampled_direction(img: &ImageBuf, radius: f32, kernel: usize, angle: f32, mask: Option<&MaskBuf>, direction: Option<&ImageBuf>) -> ImageBuf {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 || !radius.is_finite() || radius <= 0.0 { return img.clone(); }
    let radius = radius.min(w.max(h) as f32);
    let angle = if angle.is_finite() { angle.to_radians() } else { 0.0 };
    let src = img.data();
    let sample = |x: f32, y: f32| -> [f32; 4] {
        let x = x.clamp(0.0, (w - 1) as f32);
        let y = y.clamp(0.0, (h - 1) as f32);
        let (x0, y0) = (x as usize, y as usize);
        let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let mut out = [0.0; 4];
        for c in 0..4 {
            let a = src[(y0 * w + x0) * 4 + c] * (1.0 - fx) + src[(y0 * w + x1) * 4 + c] * fx;
            let b = src[(y1 * w + x0) * 4 + c] * (1.0 - fx) + src[(y1 * w + x1) * 4 + c] * fx;
            out[c] = a * (1.0 - fy) + b * fy;
        }
        out
    };
    let mut out = img.clone();
    out.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
            let m = mask.filter(|m| m.width > 0 && m.height > 0).map_or(1.0, |m| {
                let mx = x * m.width as usize / w;
                let my = y * m.height as usize / h;
                m.data[my * m.width as usize + mx].clamp(0.0, 1.0)
            });
            if !m.is_finite() || m <= 0.0 { continue; }
            let r = radius * m;
            let (cos, sin) = if kernel == 3 {
                if let Some(map) = direction.filter(|map| map.width() > 0 && map.height() > 0) {
                    let p = map.pixel((x * map.width() as usize / w) as u32, (y * map.height() as usize / h) as u32);
                    let (vx, vy) = (p[0] * 2.0 - 1.0, p[1] * 2.0 - 1.0);
                    let len = vx.hypot(vy);
                    if !len.is_finite() || len <= 1e-6 { continue; }
                    (vx / len, vy / len)
                } else { (angle.cos(), angle.sin()) }
            } else { (angle.cos(), angle.sin()) };
            let center = [src[(y * w + x) * 4], src[(y * w + x) * 4 + 1], src[(y * w + x) * 4 + 2], src[(y * w + x) * 4 + 3]];
            let (cx, cy) = ((w - 1) as f32 * 0.5, (h - 1) as f32 * 0.5);
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            let mut sums = [0.0; 4];
            let mut values = [[0.0f32; 25]; 4];
            let mut n = 0usize;
            let mut total = 0.0;
            // At five samples per axis the discrete disk and diamond are identical.
            let grid = if kernel == 2 { 7 } else { 5 };
            for j in 0..grid {
                for i in 0..grid {
                    let step = 2.0 / (grid - 1) as f32;
                    let (u, v) = (i as f32 * step - 1.0, j as f32 * step - 1.0);
                    if (kernel == 2 && u * u + v * v > 1.0) || (kernel == 6 && u.abs() + v.abs() > 1.0) { continue; }
                    let line = (j * 5 + i) as f32 / 12.0 - 1.0;
                    let (sx, sy) = match kernel {
                        3 => (x as f32 + line * r * cos, y as f32 + line * r * sin),
                        4 => { let t = line * r / w.max(h) as f32; (x as f32 + dx * t, y as f32 + dy * t) },
                        5 => { let t = line * r / w.max(h) as f32; (cx + dx * t.cos() - dy * t.sin(), cy + dx * t.sin() + dy * t.cos()) },
                        _ => (x as f32 + u * r, y as f32 + v * r),
                    };
                    let p = sample(sx, sy);
                    let weight = if kernel == 8 {
                        let d = (0..3).map(|c| (p[c] - center[c]).powi(2)).sum::<f32>();
                        (-d / (2.0 * 0.1f32.powi(2))).exp() * (-(u*u + v*v) * 2.0).exp()
                    } else { 1.0 };
                    for c in 0..4 {
                        sums[c] += p[c] * weight;
                        if kernel == 7 { values[c][n] = p[c]; }
                    }
                    total += weight;
                    n += 1;
                }
            }
            for c in 0..if kernel == 3 { 3 } else { 4 } {
                pixel[c] = if kernel == 7 {
                    values[c][..n].sort_by(f32::total_cmp);
                    values[c][n / 2]
                } else { sums[c] / total };
            }
        }
    });
    out
}

#[cfg(test)]
mod sampled_filter_tests {
    use super::*;

    #[test]
    fn kernels_are_bounded_and_preserve_constants_and_zero_radius() {
        let constant = ImageBuf::from_rgba(9, 9, [0.25, 0.5, 1.5, 0.8].repeat(81)).unwrap();
        for kernel in 2..=8 {
            assert_eq!(filter_sampled(&constant, 0.0, kernel, 0.0, None), constant);
            let result = filter_sampled(&constant, 10000.0, kernel, 30.0, None);
            for (a, b) in result.data().iter().zip(constant.data()) { assert!((a - b).abs() < 1e-5); }
        }
    }

    #[test]
    fn normal_direction_vectors_and_neutral_preserve_alpha() {
        let mut img = ImageBuf::from_rgba(17, 17, [0.0, 0.0, 0.0, 0.25].repeat(17 * 17)).unwrap();
        img.set_pixel(8, 8, [1.0, 1.0, 1.0, 0.75]);
        let map = |r, g| ImageBuf::from_rgba(1, 1, vec![r, g, 1.0, 0.0]).unwrap();
        let run = |r, g| filter_sampled_direction(&img, 4.0, 3, 90.0, None, Some(&map(r, g)));
        let horizontal = run(1.0, 0.5);
        let vertical = run(0.5, 1.0);
        let diagonal = run(1.0, 1.0);
        assert!(horizontal.pixel(10, 8)[0] > 0.0);
        assert_eq!(horizontal.pixel(8, 10)[0], 0.0);
        assert!(vertical.pixel(8, 10)[0] > 0.0);
        assert_eq!(vertical.pixel(10, 8)[0], 0.0);
        assert!(diagonal.pixel(10, 10)[0] > 0.0);
        assert_eq!(run(0.5, 0.5), img);
        assert_eq!(run(f32::NAN, 0.5), img);
        for (a, b) in horizontal.data().chunks_exact(4).zip(img.data().chunks_exact(4)) { assert_eq!(a[3], b[3]); }
        assert_eq!(filter_sampled_direction(&img, 4.0, 3, 90.0, None, None), vertical);
    }

    #[test]
    fn motion_direction_and_kernel_shapes_differ() {
        let mut data = vec![0.0; 17 * 17 * 4];
        let i = (8 * 17 + 8) * 4;
        data[i..i + 4].copy_from_slice(&[1.0; 4]);
        let img = ImageBuf::from_rgba(17, 17, data).unwrap();
        let horizontal = filter_sampled(&img, 4.0, 3, 0.0, None);
        let vertical = filter_sampled(&img, 4.0, 3, 90.0, None);
        assert!(horizontal.data()[(8 * 17 + 10) * 4] > vertical.data()[(8 * 17 + 10) * 4]);
        assert_ne!(filter_sampled(&img, 4.0, 2, 0.0, None), filter_sampled(&img, 4.0, 6, 0.0, None));
        assert_ne!(filter_sampled(&img, 4.0, 7, 0.0, None), filter_sampled(&img, 4.0, 8, 0.0, None));
    }
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

/// Per-pixel Develop controls in parameter units. Missing maps use scalar values.
/// Each supplied map must contain exactly one value per pixel.
#[derive(Default)]
pub struct DevelopControlMaps<'a> {
    pub exposure: Option<&'a [f32]>, pub contrast: Option<&'a [f32]>,
    pub highlights: Option<&'a [f32]>, pub shadows: Option<&'a [f32]>,
    pub whites: Option<&'a [f32]>, pub blacks: Option<&'a [f32]>,
}
impl DevelopControlMaps<'_> {
    fn at(map: Option<&[f32]>, i: usize, scalar: f32) -> f32 { map.map_or(scalar, |m| m[i]) }
    pub fn validate(&self, n: usize) { for m in [self.exposure, self.contrast, self.highlights, self.shadows, self.whites, self.blacks].into_iter().flatten() { assert_eq!(m.len(), n); } }
}

/// Apply mapped exposure, Legacy endpoint/recovery and contrast in Develop order.
pub fn develop_legacy_mapped(img: &mut ImageBuf, values: crate::DevelopTone, maps: &DevelopControlMaps<'_>) {
    maps.validate(img.width() as usize * img.height() as usize);
    img.data_mut().par_chunks_exact_mut(4).enumerate().for_each(|(i, px)| {
        let get = |m, v| DevelopControlMaps::at(m, i, v);
        let ev = get(maps.exposure, values.exposure);
        if ev != 0.0 { let g = ev.exp2(); for v in &mut px[..3] { *v *= g; } }
        let whites = get(maps.whites, values.whites); let blacks = get(maps.blacks, values.blacks);
        if whites != 0.0 || blacks != 0.0 {
            let y = AP1_LUMA[0]*px[0].max(0.0) + AP1_LUMA[1]*px[1].max(0.0) + AP1_LUMA[2]*px[2].max(0.0);
            if y.is_finite() { let t = y/(y+0.18); let gain = (whites*smoothstep(0.35,0.95,t)).exp2(); let add = blacks*0.03*(1.0-smoothstep(0.0,0.45,t));
                for v in &mut px[..3] { let mut o = *v*gain+add; if blacks < 0.0 { o=o.max(v.min(0.0)); } *v=o; }
            }
        }
        let hi=get(maps.highlights,values.highlights); let sh=get(maps.shadows,values.shadows);
        if hi != 0.0 || sh != 0.0 { let y=(AP1_LUMA[0]*px[0]+AP1_LUMA[1]*px[1]+AP1_LUMA[2]*px[2]).max(0.0); let t=y/(y+0.18); let g=(2.0*hi*smoothstep(0.5,1.0,t)+2.0*sh*(1.0-smoothstep(0.0,0.5,t))).exp2(); for v in &mut px[..3] { *v *= g; } }
        let c=get(maps.contrast,values.contrast);
        if c != 0.0 { let y=AP1_LUMA[0]*px[0]+AP1_LUMA[1]*px[1]+AP1_LUMA[2]*px[2]; if y>0.0 { let g=0.18*(y/0.18).powf(c.exp2())/y; for v in &mut px[..3] { *v *= g; } } }
    });
}

/// Local Develop tone: Lights, Shadows, White and Black with an edge-aware base.
///
/// Luminance is taken in log2 stops around mid grey (`L = log2(Y / 0.18)`). A
/// two-scale guided filter (2 % and 5 % of the long edge, computed on a grid of
/// at most [`LOCAL_GRID`] px so every render size sees the same neighbourhood)
/// gives the base; the coarse scale is used where it agrees with the fine one,
/// so large flat areas get a wide, stable neighbourhood while strong edges fall
/// back to the fine, edge-aware scale (no halo). The weights then come from the
/// base, so texture inside a region keeps its contrast:
/// - Lights: `smoothstep(0, 5, base)` stops above grey (midtones stay put);
/// - Shadows: `smoothstep(0, -5, base)`, faded out again between -5 and -9 stops
///   so lifted shadows do not grey true blacks;
/// - White / Black: `smoothstep(±1, ±5, e)` on `e` = mean of the pixel and the
///   fine base, so the endpoints act mostly pointwise but keep some local texture.
///
/// Gains are applied to RGB equally (hue and ratios kept); where the result is
/// darkened, chroma is reduced slightly (`2^(0.06 * ev)`) around luminance so
/// recovered highlights do not turn more saturated than Camera Raw's. Each
/// slider stays monotonic. Signed/non-finite luminance is left untouched, no
/// display clamp is applied, alpha is kept.
pub fn develop_local_tone(img: &mut ImageBuf, highlights: f32, shadows: f32, whites: f32, blacks: f32) {
    develop_local_tone_mapped(img, highlights, shadows, whites, blacks, &DevelopControlMaps::default());
}

/// Long edge of the grid on which the local base is estimated. Larger images are
/// area-averaged onto it and the filter coefficients are sampled back bilinearly.
const LOCAL_GRID: usize = 512;
/// Guided filter radii as fractions of the long edge, and their regularisation (stops²).
const LOCAL_FINE: (f32, f32) = (0.02, 0.3);
const LOCAL_COARSE: (f32, f32) = (0.05, 0.4);
/// Where fine and coarse base differ by more than about this many stops, the fine one wins.
const LOCAL_SCALE_SIGMA: f32 = 0.6;
/// Luminance floor relative to mid grey (keeps log2 finite for black pixels).
const LOCAL_FLOOR: f32 = 1.0 / 4096.0;
/// Chroma kept per stop of darkening: `2^(LOCAL_CHROMA * ev)` for `ev < 0`.
const LOCAL_CHROMA: f32 = 0.06;

/// Area-weighted resampling taps: for each of `m` outputs, the first input index and its weights.
fn area_taps(n: usize, m: usize) -> Vec<(usize, Vec<f32>)> {
    let s = n as f64 / m as f64;
    (0..m)
        .map(|i| {
            let (x0, x1) = (i as f64 * s, (i + 1) as f64 * s);
            let (j0, j1) = (x0.floor() as usize, (x1.ceil() as usize).min(n));
            let w = (j0..j1).map(|j| (((j + 1) as f64).min(x1) - (j as f64).max(x0)).max(0.0) / s).map(|v| v as f32).collect();
            (j0, w)
        })
        .collect()
}

/// Area average of a `w` x `h` plane onto `lw` x `lh`.
fn area_resize(src: &[f32], w: usize, h: usize, lw: usize, lh: usize) -> Vec<f32> {
    let (xt, yt) = (area_taps(w, lw), area_taps(h, lh));
    let mut rows = vec![0.0f32; h * lw];
    rows.par_chunks_exact_mut(lw).zip(src.par_chunks_exact(w)).for_each(|(dst, row)| {
        for (d, (j0, wt)) in dst.iter_mut().zip(&xt) {
            *d = wt.iter().zip(&row[*j0..]).map(|(a, b)| a * b).sum();
        }
    });
    let mut out = vec![0.0f32; lw * lh];
    out.par_chunks_exact_mut(lw).zip(yt.par_iter()).for_each(|(dst, (j0, wt))| {
        for (k, a) in wt.iter().enumerate() {
            for (d, v) in dst.iter_mut().zip(&rows[(j0 + k) * lw..(j0 + k + 1) * lw]) {
                *d += a * v;
            }
        }
    });
    out
}

/// Bilinear taps from `n` output samples onto `ln` grid samples (pixel centres aligned).
fn centre_taps(n: usize, ln: usize) -> Vec<(u32, u32, f32)> {
    let s = ln as f32 / n as f32;
    (0..n)
        .map(|x| {
            let p = ((x as f32 + 0.5) * s - 0.5).max(0.0);
            let i0 = (p.floor() as usize).min(ln - 1);
            let i1 = (i0 + 1).min(ln - 1);
            (i0 as u32, i1 as u32, if i1 > i0 { p - i0 as f32 } else { 0.0 })
        })
        .collect()
}

/// Guided-filter coefficients `[a_fine, b_fine, a_coarse, b_coarse]` of a log
/// luminance grid, self-guided (`base = a * L + b`).
fn local_tone_coefficients(l: &[f32], lw: usize, lh: usize) -> Vec<f32> {
    let long = lw.max(lh) as f32;
    let moments: Vec<f32> = l.iter().flat_map(|&v| [v, v * v, 0.0, 0.0]).collect();
    let scale = |(r, eps): (f32, f32)| {
        let radius = ((long * r).round() as usize).max(1);
        let mean = box_filter(&moments, lw, lh, radius, 1);
        let ab: Vec<f32> = mean
            .chunks_exact(4)
            .flat_map(|m| {
                let var = (m[1] - m[0] * m[0]).max(0.0);
                let a = var / (var + eps);
                [a, m[0] * (1.0 - a), 0.0, 0.0]
            })
            .collect();
        box_filter(&ab, lw, lh, radius, 1)
    };
    let (fine, coarse) = rayon::join(|| scale(LOCAL_FINE), || scale(LOCAL_COARSE));
    fine.chunks_exact(4).zip(coarse.chunks_exact(4)).flat_map(|(f, c)| [f[0], f[1], c[0], c[1]]).collect()
}

/// Local tone with optional spatial control values; the base is computed once.
pub fn develop_local_tone_mapped(img: &mut ImageBuf, highlights: f32, shadows: f32, whites: f32, blacks: f32, maps: &DevelopControlMaps<'_>) {
    maps.validate(img.width() as usize * img.height() as usize);
    if highlights == 0.0 && shadows == 0.0 && whites == 0.0 && blacks == 0.0 && maps.highlights.is_none() && maps.shadows.is_none() && maps.whites.is_none() && maps.blacks.is_none() { return; }
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 { return; }
    let lum: Vec<f32> = img.data().par_chunks_exact(4).map(|px| {
        let y = AP1_LUMA[0]*px[0] + AP1_LUMA[1]*px[1] + AP1_LUMA[2]*px[2];
        (if y.is_finite() && y > 0.0 { y / 0.18 } else { 0.0 } + LOCAL_FLOOR).log2()
    }).collect();
    let long = w.max(h);
    let (lw, lh) = if long > LOCAL_GRID {
        let s = LOCAL_GRID as f32 / long as f32;
        (((w as f32 * s).round() as usize).max(1), ((h as f32 * s).round() as usize).max(1))
    } else { (w, h) };
    let coeff = if (lw, lh) == (w, h) {
        local_tone_coefficients(&lum, w, h)
    } else {
        local_tone_coefficients(&area_resize(&lum, w, h, lw, lh), lw, lh)
    };
    let (xt, yt) = (centre_taps(w, lw), centre_taps(h, lh));
    img.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        let (y0, y1, fy) = yt[y];
        let (r0, r1) = (&coeff[y0 as usize * lw * 4..][..lw * 4], &coeff[y1 as usize * lw * 4..][..lw * 4]);
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let i = y * w + x;
            let hi = DevelopControlMaps::at(maps.highlights, i, highlights); let sh = DevelopControlMaps::at(maps.shadows, i, shadows);
            let wh = DevelopControlMaps::at(maps.whites, i, whites); let bl = DevelopControlMaps::at(maps.blacks, i, blacks);
            if hi == 0.0 && sh == 0.0 && wh == 0.0 && bl == 0.0 { continue; }
            let yl = AP1_LUMA[0]*px[0] + AP1_LUMA[1]*px[1] + AP1_LUMA[2]*px[2];
            if !yl.is_finite() || yl <= 0.0 { continue; }
            let (x0, x1, fx) = xt[x];
            let (x0, x1) = (x0 as usize * 4, x1 as usize * 4);
            let mut c = [0.0f32; 4];
            for (k, v) in c.iter_mut().enumerate() {
                let top = r0[x0 + k] + (r0[x1 + k] - r0[x0 + k]) * fx;
                let bot = r1[x0 + k] + (r1[x1 + k] - r1[x0 + k]) * fx;
                *v = top + (bot - top) * fy;
            }
            let l = lum[i];
            let fine = c[0] * l + c[1];
            let d = c[2] * l + c[3] - fine;
            let base = fine + d * (-0.5 * (d / LOCAL_SCALE_SIGMA).powi(2)).exp();
            let lights = smoothstep(0.0, 5.0, base);
            let darks = smoothstep(0.0, -5.0, base) * (1.0 - smoothstep(-5.0, -9.0, base));
            let e = 0.5 * (l + fine);
            let ev = 2.0 * (hi * lights + sh * darks) + wh * smoothstep(1.0, 5.0, e) + bl * smoothstep(-1.0, -5.0, e);
            if ev == 0.0 { continue; }
            let gain = ev.exp2();
            for v in &mut px[..3] { *v *= gain; }
            if ev < 0.0 {
                // Darkened colours: pull chroma slightly towards the new luminance.
                let (k, yo) = ((LOCAL_CHROMA * ev).exp2(), yl * gain);
                for v in &mut px[..3] { *v = yo + (*v - yo) * k; }
            }
        }
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
            let exact = ImageBuf::from_rgba(643, 431, blur_exact(&img, sigma as usize, 3)).unwrap();
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
        let mask = MaskBuf { width: w, height: h, data: (0..w*h).map(|i| (i % w) as f32 / (w - 1) as f32).collect() };
        time_runs("blur sigma 8", || drop(blur(&img, 8.0)));
        time_runs("blur sigma 60", || drop(blur(&img, 60.0)));
        time_runs("blur_modulated max 60", || drop(blur_modulated(&img, 60.0, &mask)));
        time_runs("noise_reduction luma 0.5 chroma 0.7 default radius 0.8 px", || drop(noise_reduction(&img, 0.5, 0.7, 0.8)));
        time_runs("noise_reduction luma 1.0 chroma 1.0 radius 5 px (heavy)", || drop(noise_reduction(&img, 1.0, 1.0, 5.0)));
        time_runs("noise_reduction luma only", || drop(noise_reduction(&img, 0.6, 0.0, 5.0)));
        time_runs("noise_reduction chroma only", || drop(noise_reduction(&img, 0.0, 0.6, 5.0)));
    }
}

#[cfg(test)]
mod local_develop_tests {
    use super::*;
    #[test]
    fn local_context_identity_hdr_and_endpoints() {
        let mut a = ImageBuf::new(9, 1);
        let mut b = ImageBuf::new(9, 1);
        for x in 0..9 { a.set_pixel(x, 0, [0.12, 0.12, 0.12, 0.7]); b.set_pixel(x, 0, [0.3, 0.3, 0.3, 0.7]); }
        a.set_pixel(4, 0, [0.2, 0.2, 0.2, 0.7]); b.set_pixel(4, 0, [0.2, 0.2, 0.2, 0.7]);
        let orig = a.clone(); develop_local_tone(&mut a, 0.0, 0.0, 0.0, 0.0); assert_eq!(a, orig);
        develop_local_tone(&mut a, -1.0, 0.0, 0.0, 0.0); develop_local_tone(&mut b, -1.0, 0.0, 0.0, 0.0);
        assert_ne!(a.pixel(4, 0)[0], b.pixel(4, 0)[0]);
        assert_eq!(a.pixel(4, 0)[3], 0.7);
        let mut hdr = ImageBuf::from_rgba(1, 1, vec![8.0, 8.0, 8.0, 1.0]).unwrap();
        develop_local_tone(&mut hdr, 0.0, 0.0, 0.5, 0.0); assert!(hdr.pixel(0, 0)[0] > 8.0);
        let mut black = ImageBuf::from_rgba(1, 1, vec![0.001, 0.001, 0.001, 1.0]).unwrap();
        develop_local_tone(&mut black, 0.0, 0.0, 0.0, 0.5); assert!(black.pixel(0, 0)[0] > 0.001);
    }

    fn luma(px: [f32; 4]) -> f32 {
        AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2]
    }

    fn grey(w: u32, h: u32, f: impl Fn(u32, u32) -> f32) -> ImageBuf {
        let mut img = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = f(x, y);
                img.set_pixel(x, y, [v, v, v, 1.0]);
            }
        }
        img
    }

    /// Largest deviation (stops) of the output luminance from the far-field
    /// level on either side of a vertical step edge: 0 means no halo.
    fn halo(lo: f32, hi: f32, s: [f32; 4]) -> f32 {
        let (w, h) = (1024u32, 32u32);
        let mut img = grey(w, h, |x, _| if x < w / 2 { lo } else { hi });
        develop_local_tone(&mut img, s[0], s[1], s[2], s[3]);
        let row: Vec<f32> = (0..w).map(|x| luma(img.pixel(x, h / 2)).log2()).collect();
        let (left, right) = row.split_at(w as usize / 2);
        let far = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
        let (fl, fr) = (far(&left[..w as usize / 8]), far(&right[right.len() - w as usize / 8..]));
        let dl = left.iter().map(|v| (v - fl).abs()).fold(0.0, f32::max);
        let dr = right.iter().map(|v| (v - fr).abs()).fold(0.0, f32::max);
        dl.max(dr)
    }

    #[test]
    fn local_tone_halo_across_a_step_edge() {
        let mut worst = 0.0f32;
        for (lo, hi) in [(0.02, 2.0), (0.09, 0.36), (0.005, 0.5)] {
            for s in [[0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [-1.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 1.0], [0.0, 0.0, -1.0, -1.0]] {
                let v = halo(lo, hi, s);
                println!("halo {lo}->{hi} {s:?}: {v:.3} stops");
                worst = worst.max(v);
            }
        }
        println!("halo worst: {worst:.3} stops");
        assert!(worst < 0.2, "halo {worst}");
    }

    /// 4x4 supersampled analytic scene: a -4..+4 stop ramp, dark/bright blocks,
    /// a bright disc, a dark disc and a fine texture band. `w` = 2 * height.
    fn scene(w: u32) -> ImageBuf {
        let h = w / 2;
        let mut img = ImageBuf::new(w, h);
        let n = w as f32;
        let f = |x: f32, y: f32| -> f32 {
            let mut v = 0.18 * (-4.0 + 8.0 * x).exp2();
            if y > 0.25 { v = if x < 0.5 { 0.02 } else { 2.5 }; }
            if (x - 0.3).powi(2) + (y - 0.25).powi(2) < 0.01 { v = 1.5; }
            if (x - 0.75).powi(2) + (y - 0.25).powi(2) < 0.006 { v = 0.01; }
            if y > 0.4 { v *= 1.0 + 0.4 * (x * 120.0).sin() * (y * 90.0).sin(); }
            v
        };
        for py in 0..h {
            for px in 0..w {
                let mut s = 0.0;
                for sy in 0..4 {
                    for sx in 0..4 {
                        s += f((px as f32 + (sx as f32 + 0.5) / 4.0) / n, (py as f32 + (sy as f32 + 0.5) / 4.0) / n);
                    }
                }
                let v = s / 16.0;
                img.set_pixel(px, py, [v, 0.8 * v, 0.6 * v, 1.0]);
            }
        }
        img
    }

    /// Mean and 99th percentile of |gain(512) - downscaled gain(2048)| in stops.
    fn resolution_difference(s: [f32; 4]) -> (f32, f32) {
        let big = scene(2048);
        let small = scene(512);
        let gain = |img: &ImageBuf| {
            let mut out = img.clone();
            develop_local_tone(&mut out, s[0], s[1], s[2], s[3]);
            out.data().chunks_exact(4).zip(img.data().chunks_exact(4)).map(|(o, i)| (luma([o[0], o[1], o[2], 0.0]) / luma([i[0], i[1], i[2], 0.0])).log2()).collect::<Vec<f32>>()
        };
        let (gb, gs) = (gain(&big), gain(&small));
        let mut d: Vec<f32> = (0..256usize)
            .flat_map(|y| (0..512usize).map(move |x| (x, y)))
            .map(|(x, y)| {
                let mut m = 0.0;
                for yy in 0..4 { for xx in 0..4 { m += gb[(y * 4 + yy) * 2048 + x * 4 + xx]; } }
                (m / 16.0 - gs[y * 512 + x]).abs()
            })
            .collect();
        d.sort_by(f32::total_cmp);
        (d.iter().sum::<f32>() / d.len() as f32, d[d.len() * 99 / 100])
    }

    #[test]
    fn local_tone_is_resolution_independent() {
        for s in [[-1.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, -1.0]] {
            let (mean, p99) = resolution_difference(s);
            println!("512 vs 2048 {s:?}: mean {mean:.4} p99 {p99:.4} stops");
            assert!(mean < 0.01 && p99 < 0.03, "{s:?}: {mean} {p99}");
        }
    }

    #[test]
    fn neutral_sliders_and_zero_maps_are_bit_exact() {
        let mut img = scene(256);
        img.set_pixel(3, 3, [-0.2, 0.1, 0.4, 0.5]);
        img.set_pixel(4, 3, [f32::NAN, 0.1, f32::INFINITY, 1.0]);
        img.set_pixel(5, 3, [0.0, 0.0, 0.0, 0.0]);
        let orig = img.clone();
        develop_local_tone(&mut img, 0.0, 0.0, 0.0, 0.0);
        assert!(img.data().iter().zip(orig.data()).all(|(a, b)| a.to_bits() == b.to_bits()));
        // Maps that are 0 everywhere change nothing either, whatever the sliders say.
        let zeros = vec![0.0f32; 256 * 128];
        let maps = DevelopControlMaps { highlights: Some(&zeros), shadows: Some(&zeros), whites: Some(&zeros), blacks: Some(&zeros), ..Default::default() };
        develop_local_tone_mapped(&mut img, -1.0, 1.0, 1.0, 1.0, &maps);
        assert!(img.data().iter().zip(orig.data()).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    #[test]
    fn more_shadows_never_darkens_and_more_recovery_never_brightens() {
        let img = scene(512);
        let run = |s: [f32; 4]| { let mut o = img.clone(); develop_local_tone(&mut o, s[0], s[1], s[2], s[3]); o };
        let steps = [0.0, 0.25, 0.5, 0.75, 1.0];
        for pair in steps.windows(2) {
            let l = |p: &[f32]| luma([p[0], p[1], p[2], 0.0]);
            for (a, b) in run([0.0, pair[0], 0.0, 0.0]).data().chunks_exact(4).zip(run([0.0, pair[1], 0.0, 0.0]).data().chunks_exact(4)) {
                assert!(l(b) >= l(a), "shadows {pair:?}");
            }
            for (a, b) in run([-pair[0], 0.0, 0.0, 0.0]).data().chunks_exact(4).zip(run([-pair[1], 0.0, 0.0, 0.0]).data().chunks_exact(4)) {
                assert!(l(b) <= l(a), "highlights {pair:?}");
            }
        }
        // Pointwise, the tone response stays monotonic for every slider extreme:
        // a brighter flat field never comes out darker.
        for s in [[-1.0, 1.0, -1.0, 1.0], [1.0, -1.0, 1.0, -1.0], [-1.0, -1.0, -1.0, -1.0], [1.0, 1.0, 1.0, 1.0]] {
            let mut prev = 0.0f32;
            for k in 0..=200 {
                let v = 0.18 * (-12.0 + 20.0 * k as f32 / 200.0).exp2();
                let mut px = grey(8, 8, |_, _| v);
                develop_local_tone(&mut px, s[0], s[1], s[2], s[3]);
                let o = luma(px.pixel(4, 4));
                assert!(o >= prev, "{s:?} at {v}: {o} < {prev}");
                prev = o;
            }
        }
    }

    /// Display saturation (max-min)/max of pixel (4, 4): ACEScg -> Rec.709, the
    /// per-channel ACES fit curve of the viewer, sRGB encoding.
    fn display_saturation(img: &ImageBuf) -> f32 {
        let m = crate::color::to_f32(&crate::color::acescg_to_rec709());
        let px = img.pixel(4, 4);
        let p: Vec<f32> = (0..3).map(|r| {
            let v = (m[r][0] * px[0] + m[r][1] * px[1] + m[r][2] * px[2]).max(0.0);
            let t = (v * (v + 0.024_578_6) - 0.000_090_537) / (v * (0.983_729 * v + 0.432_951) + 0.238_081);
            crate::color::srgb_oetf(t.clamp(0.0, 1.0))
        }).collect();
        let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
        (mx - mn) / mx.max(1e-9)
    }

    #[test]
    fn coloured_patches_keep_their_saturation_under_strong_pushes() {
        let mut worst = 0.0f32;
        for rgb in [[0.05, 0.01, 0.005], [0.02, 0.03, 0.12], [2.0, 0.6, 0.1], [0.1, 0.2, 3.0], [1.2, 1.0, 0.2], [0.6, 0.15, 0.4]] {
            for s in [[0.0, 1.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [0.0, 0.0, -1.0, 0.0], [0.0, 0.0, 0.0, 1.0]] {
                let mut img = ImageBuf::new(16, 16);
                for y in 0..16 { for x in 0..16 { img.set_pixel(x, y, [rgb[0], rgb[1], rgb[2], 1.0]); } }
                let before = display_saturation(&img);
                let y0 = luma(img.pixel(4, 4));
                develop_local_tone(&mut img, s[0], s[1], s[2], s[3]);
                if (luma(img.pixel(4, 4)) / y0).log2().abs() < 0.05 { continue; }
                let d = display_saturation(&img) - before;
                println!("saturation {rgb:?} {s:?}: {before:.3} -> {:.3}", before + d);
                worst = worst.max(d.abs());
            }
        }
        println!("saturation worst change: {worst:.3}");
        assert!(worst < 0.05, "{worst}");
    }

    #[test]
    fn constant_maps_match_the_scalar_sliders() {
        let img = scene(256);
        let n = 256 * 128;
        for (k, s) in [[-0.8f32, 0.0, 0.0, 0.0], [0.0, 0.6, 0.0, 0.0], [0.0, 0.0, 0.7, 0.0], [0.0, 0.0, 0.0, -0.5]].iter().enumerate() {
            let mut scalar = img.clone();
            develop_local_tone(&mut scalar, s[0], s[1], s[2], s[3]);
            let map = vec![s[k]; n];
            let mut maps = DevelopControlMaps::default();
            match k { 0 => maps.highlights = Some(&map), 1 => maps.shadows = Some(&map), 2 => maps.whites = Some(&map), _ => maps.blacks = Some(&map) }
            let mut mapped = img.clone();
            develop_local_tone_mapped(&mut mapped, 0.0, 0.0, 0.0, 0.0, &maps);
            assert_eq!(mapped, scalar, "slider {k}");
            // Half the map value: an effect between neutral and the full slider.
            let half = vec![s[k] * 0.5; n];
            let mut maps = DevelopControlMaps::default();
            match k { 0 => maps.highlights = Some(&half), 1 => maps.shadows = Some(&half), 2 => maps.whites = Some(&half), _ => maps.blacks = Some(&half) }
            let mut halfway = img.clone();
            develop_local_tone_mapped(&mut halfway, 0.0, 0.0, 0.0, 0.0, &maps);
            let mut moved = 0;
            for ((h, f), o) in halfway.data().chunks_exact(4).zip(scalar.data().chunks_exact(4)).zip(img.data().chunks_exact(4)) {
                let (lh, lf, lo) = (luma([h[0], h[1], h[2], 0.0]), luma([f[0], f[1], f[2], 0.0]), luma([o[0], o[1], o[2], 0.0]));
                assert!((lh - lo) * (lf - lo) >= 0.0 && (lh - lo).abs() <= (lf - lo).abs() * 1.0001 + 1e-7);
                if (lf - lo).abs() > 1e-4 { moved += 1; }
            }
            assert!(moved > 0);
        }
    }

    #[test]
    #[ignore = "timing; run with --release -- --ignored --nocapture"]
    fn timing_local_tone_2560x1707() {
        let (w, h) = (2560u32, 1707u32);
        let mut img = ImageBuf::new(w, h);
        for (i, v) in img.data_mut().iter_mut().enumerate() {
            *v = ((i * 2654435761) % 1000) as f32 / 500.0;
        }
        let mut ms: Vec<f64> = (0..15)
            .map(|_| {
                let mut a = img.clone();
                let t = std::time::Instant::now();
                develop_local_tone(&mut a, -0.6, 0.5, 0.2, -0.2);
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        ms.sort_by(f64::total_cmp);
        println!("develop_local_tone 2560x1707: min {:.1} ms, median {:.1} ms", ms[0], ms[7]);
    }
}
