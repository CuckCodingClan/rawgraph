//! HDR merge of an exposure bracket: white levels, exposure ratios, translation
//! alignment (Ward's median-threshold bitmaps), weighting, deghosting.
//!
//! Frames are scene-linear RGBA f32 of one size. The result is at the exposure
//! of the reference frame; values above 1 are kept.

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::ImageBuf;
use rayon::prelude::*;

use crate::util::smoothstep;

/// Most frames one merge accepts.
pub const HDR_MAX_FRAMES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HdrParams {
    /// Measure the exposure ratios from the pixels (falls back to metadata when
    /// the frames overlap too little); false prefers the metadata.
    pub measured: bool,
    pub align: bool,
    /// Largest alignment shift, percent of the longer side.
    pub max_shift_pct: f32,
    /// 0 = off, 1 = strictest.
    pub deghost: f32,
    /// Fraction of a frame's white level that counts as clipped.
    pub clip: f32,
    /// Express the result at the exposure of the frame with the median exposure instead of the
    /// exposure of `reference` (which still anchors alignment and deghosting). Used when the
    /// planned reference was not chosen by a known exposure value.
    pub median_exposure: bool,
}

impl Default for HdrParams {
    fn default() -> Self {
        HdrParams { measured: true, align: true, max_shift_pct: 3.0, deghost: 0.5, clip: 0.95, median_exposure: false }
    }
}

#[inline]
fn luma(p: &[f32]) -> f32 {
    AP1_LUMA[0] * p[0] + AP1_LUMA[1] * p[1] + AP1_LUMA[2] * p[2]
}

/// Largest channel; non-finite pixels give `None`.
#[inline]
fn max_channel(p: &[f32]) -> Option<f32> {
    if p[0].is_finite() && p[1].is_finite() && p[2].is_finite() {
        Some(p[0].max(p[1]).max(p[2]))
    } else {
        None
    }
}

/// A channel counts as clipped when this fraction of all pixels sits in the single histogram bin of
/// its white level and that bin holds more than three times the two bins below: clipped pixels are
/// exactly equal, while the brightest part of a scene is spread
/// out by noise.
const PLATEAU: f64 = 0.002;

/// 99.95th percentile of every channel separately, from histograms over the top 15
/// bits of the float representation (relative resolution 0.4 %), and per channel whether
/// that level is a clip plateau (see [`PLATEAU`]). A channel without positive finite samples
/// gives 0 / not clipped.
fn white_levels(img: &ImageBuf) -> ([f32; 3], [bool; 3]) {
    const BINS: usize = 1 << 15;
    type Hist = [Vec<u32>; 3];
    let empty = || -> Hist { [vec![0u32; BINS], vec![0u32; BINS], vec![0u32; BINS]] };
    let hist = img
        .data()
        .par_chunks(4 * 4096)
        .fold(empty, |mut h, chunk| {
            for p in chunk.chunks_exact(4) {
                for c in 0..3 {
                    let v = p[c];
                    if v.is_finite() && v > 0.0 {
                        h[c][(v.to_bits() >> 16) as usize & (BINS - 1)] += 1;
                    }
                }
            }
            h
        })
        .reduce(empty, |mut a, b| {
            for c in 0..3 {
                for (x, y) in a[c].iter_mut().zip(&b[c]) {
                    *x += *y;
                }
            }
            a
        });
    let mut out = [0.0f32; 3];
    let mut clipped = [false; 3];
    let total_px = (img.data().len() / 4) as f64;
    for c in 0..3 {
        let total: u64 = hist[c].iter().map(|&n| n as u64).sum();
        if total == 0 {
            continue;
        }
        let target = ((total as f64) * 0.9995).ceil() as u64;
        let mut acc = 0u64;
        for (bin, &n) in hist[c].iter().enumerate() {
            acc += n as u64;
            if acc >= target {
                out[c] = f32::from_bits(((bin as u32) << 16) | 0x8000);
                let below = hist[c][bin.saturating_sub(2)..bin].iter().copied().max().unwrap_or(0);
                clipped[c] = n as f64 >= PLATEAU * total_px && n > 3 * below;
                break;
            }
        }
    }
    (out, clipped)
}

/// Mean luminance over the finite pixels (orders frames by brightness).
/// True if at least 0.1 % of the bright pixels (above 30 % of the white level) are exactly grey: the
/// RAW loader's answer to a clipped channel. Noise makes real grey surfaces differ per channel.
fn has_neutralised_pixels(img: &ImageBuf, inv_w: &[f32; 3]) -> bool {
    let (bright, grey) = img
        .data()
        .par_chunks(4)
        .map(|p| {
            let level = (p[0] * inv_w[0]).max(p[1] * inv_w[1]).max(p[2] * inv_w[2]);
            if !(level.is_finite() && level >= 0.3) {
                return (0u64, 0u64);
            }
            let (mx, mn) = (p[0].max(p[1]).max(p[2]), p[0].min(p[1]).min(p[2]));
            (1, u64::from(mx > 0.0 && mx - mn <= 1e-4 * mx))
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    bright > 0 && grey * 1000 >= bright
}

fn mean_luma(img: &ImageBuf) -> f32 {
    let (sum, n) = img
        .data()
        .par_chunks(4 * 4096)
        .map(|chunk| {
            let (mut s, mut n) = (0.0f64, 0u64);
            for p in chunk.chunks_exact(4) {
                let y = luma(p);
                if y.is_finite() {
                    s += y as f64;
                    n += 1;
                }
            }
            (s, n)
        })
        .reduce(|| (0.0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    if n == 0 {
        0.0
    } else {
        (sum / n as f64) as f32
    }
}

// ---- alignment ---------------------------------------------------------------

/// One level of a median-threshold bitmap: 0 = excluded, 1 = below, 2 = above.
struct Bitmap {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

fn log_luminance(img: &ImageBuf) -> Vec<f32> {
    img.data()
        .par_chunks_exact(4)
        .map(|p| {
            let y = luma(p);
            if y.is_finite() {
                y.max(1e-5).ln()
            } else {
                0.0
            }
        })
        .collect()
}

fn halve(src: &[f32], w: usize, h: usize) -> (Vec<f32>, usize, usize) {
    let (w2, h2) = ((w / 2).max(1), (h / 2).max(1));
    let mut out = vec![0.0f32; w2 * h2];
    out.par_chunks_mut(w2).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let (x0, y0) = ((2 * x).min(w - 1), (2 * y).min(h - 1));
            let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
            *o = 0.25 * (src[y0 * w + x0] + src[y0 * w + x1] + src[y1 * w + x0] + src[y1 * w + x1]);
        }
    });
    (out, w2, h2)
}

fn bitmap(l: &[f32], w: usize, h: usize) -> Bitmap {
    let step = (l.len() / 65_536).max(1);
    let mut sample: Vec<f32> = l.iter().step_by(step).copied().collect();
    sample.sort_by(f32::total_cmp);
    let n = sample.len();
    let med = sample[n / 2];
    let (lo, hi) = (sample[n / 100], sample[(n - 1) - n / 100]);
    let band = 4.0 / 255.0 * (hi - lo);
    let data = l
        .par_iter()
        .map(|&v| {
            if (v - med).abs() <= band {
                0
            } else if v > med {
                2
            } else {
                1
            }
        })
        .collect();
    Bitmap { w, h, data }
}

/// Bitmaps of every pyramid level, finest first; coarsest edge about 64 px.
fn bitmap_pyramid(img: &ImageBuf) -> Vec<Bitmap> {
    let (mut w, mut h) = (img.width() as usize, img.height() as usize);
    let mut l = log_luminance(img);
    let mut out = Vec::new();
    loop {
        out.push(bitmap(&l, w, h));
        if w.max(h) <= 64 || w.min(h) < 16 {
            break;
        }
        let (n, w2, h2) = halve(&l, w, h);
        l = n;
        w = w2;
        h = h2;
    }
    out
}

/// Mismatch rate of two bitmaps when `b` is read at `(x + sx, y + sy)`.
fn mismatch(a: &Bitmap, b: &Bitmap, sx: i32, sy: i32) -> f32 {
    let (w, h) = (a.w as i32, a.h as i32);
    let (y0, y1) = (0.max(-sy), h.min(h - sy));
    let (x0, x1) = (0.max(-sx), w.min(w - sx));
    if y0 >= y1 || x0 >= x1 {
        return f32::MAX;
    }
    let (bad, used) = (y0..y1)
        .into_par_iter()
        .map(|y| {
            let ra = &a.data[y as usize * a.w..][..a.w];
            let rb = &b.data[(y + sy) as usize * b.w..][..b.w];
            let (mut bad, mut used) = (0u64, 0u64);
            for x in x0..x1 {
                let (p, q) = (ra[x as usize], rb[(x + sx) as usize]);
                if p != 0 && q != 0 {
                    used += 1;
                    bad += (p != q) as u64;
                }
            }
            (bad, used)
        })
        .reduce(|| (0, 0), |u, v| (u.0 + v.0, u.1 + v.1));
    if used == 0 {
        f32::MAX
    } else {
        bad as f32 / used as f32
    }
}

/// Alignment is only trusted when it is clearly better than not moving at all.
const SHIFT_GAIN: f32 = 0.8;

/// Why a search result was not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShiftVerdict {
    /// A shift (possibly zero) the bitmaps support.
    Found,
    /// The search ended on its bound or did not beat the unshifted match clearly.
    Unsure,
}

/// Translation (dx, dy) in full-resolution pixels such that the frame read at
/// `(x + dx, y + dy)` lines up with the reference at `(x, y)`, and whether the
/// result is trusted. A shift is kept only if the mismatch at the two finest
/// levels is below 0.8 times the mismatch without a shift, and it does not end
/// on the search bound; otherwise the answer is `(0, 0)` ("unsure" when a
/// shift was found but rejected).
fn find_shift_checked(reference: &[Bitmap], frame: &[Bitmap], max_px: f32) -> ((i32, i32), ShiftVerdict) {
    let levels = reference.len().min(frame.len());
    let mut s = (0i32, 0i32);
    // (shift, mismatch at that shift, mismatch at zero shift, bound) of the two finest levels.
    let mut fine: Vec<((i32, i32), f32, f32, i32)> = Vec::new();
    for lvl in (0..levels).rev() {
        if lvl + 1 != levels {
            s = (s.0 * 2, s.1 * 2);
        }
        let scale = (1u32 << lvl.min(30)) as f32;
        let bound = (max_px / scale).floor() as i32;
        let mut best = (s, f32::MAX);
        for (dx, dy) in [(0, 0), (-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, -1), (-1, 1), (1, 1)] {
            let c = (s.0 + dx, s.1 + dy);
            if c.0.abs() > bound || c.1.abs() > bound {
                continue;
            }
            let m = mismatch(&reference[lvl], &frame[lvl], c.0, c.1);
            if m < best.1 - 1e-6 {
                best = (c, m);
            }
        }
        s = best.0;
        if lvl < 2 {
            let zero = if s == (0, 0) { best.1 } else { mismatch(&reference[lvl], &frame[lvl], 0, 0) };
            fine.push((s, best.1, zero, bound));
        }
    }
    if s == (0, 0) {
        return ((0, 0), ShiftVerdict::Found);
    }
    let on_bound = fine.last().is_some_and(|&(sh, _, _, bound)| sh.0.abs() >= bound.max(1) || sh.1.abs() >= bound.max(1));
    let clearly_better = fine.iter().all(|&(sh, m, zero, _)| sh == (0, 0) || (m.is_finite() && m < SHIFT_GAIN * zero));
    if on_bound || !clearly_better {
        ((0, 0), ShiftVerdict::Unsure)
    } else {
        (s, ShiftVerdict::Found)
    }
}

/// [`find_shift_checked`] without the verdict.
#[cfg(test)]
fn find_shift(reference: &[Bitmap], frame: &[Bitmap], max_px: f32) -> (i32, i32) {
    find_shift_checked(reference, frame, max_px).0
}

// ---- merge ------------------------------------------------------------------------

/// Largest brightness (relative to the white level) at which a pixel of a darker
/// frame, scaled up, is checked against the brighter frame at all.
const CONSISTENCY_GATE: f32 = 0.2;
/// The brighter frame's own level (in units of its white level) from which its pixels can be
/// clipped or neutralised by the RAW loader (which starts blending at 90 % of the sensor range of a
/// channel). Below it a disagreement with the darker frame is motion or noise: left to deghosting.
const CONSISTENCY_OWN_GATE: f32 = 0.4;
/// Largest spread of the per-channel ratios of a block that still counts for exposure measuring.
const RATIO_AGREEMENT: f32 = 1.06;
/// A measured exposure ratio further than this (in stops) from the metadata is not trusted.
const MEASURED_VS_METADATA_EV: f32 = 0.5;
/// Relative disagreement (brighter frame vs the scaled darker frame) that starts / completes
/// dropping the brighter frame's pixel.
const CONSISTENCY_LO: f32 = 0.04;
const CONSISTENCY_HI: f32 = 0.12;

struct Frame<'a> {
    img: &'a ImageBuf,
    /// 1 / (clip * white level) per channel (scales the dark ramp and the checks).
    inv_w: [f32; 3],
    /// Like `inv_w` for channels whose white level is a clip plateau, else 0 (never clipped).
    inv_t: [f32; 3],
    /// Multiplier that brings the frame to the reference exposure.
    k: f32,
    shift: (i32, i32),
    /// The darkest frame is the last resort for highlights: it is never cut at its own white level.
    darkest: bool,
    /// The frame has clipped channels (a plateau) or pixels the RAW loader turned exactly grey: its
    /// bright pixels may not be what the scene was.
    may_clip: bool,
}

impl Frame<'_> {
    /// The pixel read for output position `(x, y)`; `None` when the shift moves it out of the
    /// image (such samples carry no weight: no edge streaks).
    #[inline]
    fn at(&self, x: usize, y: usize, w: usize, h: usize) -> Option<&[f32]> {
        let sx = x as i64 + self.shift.0 as i64;
        let sy = y as i64 + self.shift.1 as i64;
        if sx < 0 || sy < 0 || sx >= w as i64 || sy >= h as i64 {
            return None;
        }
        let i = (sy as usize * w + sx as usize) * 4;
        Some(&self.img.data()[i..i + 4])
    }

    /// Like [`Frame::at`], with the shift clamped to the image (only for the last-resort fallback).
    #[inline]
    fn at_clamped(&self, x: usize, y: usize, w: usize, h: usize) -> &[f32] {
        let sx = (x as i64 + self.shift.0 as i64).clamp(0, w as i64 - 1) as usize;
        let sy = (y as i64 + self.shift.1 as i64).clamp(0, h as i64 - 1) as usize;
        let i = (sy * w + sx) * 4;
        &self.img.data()[i..i + 4]
    }

    /// Largest channel in units of its clip threshold (1 = clipped); channels without a clip plateau count 0.
    #[inline]
    fn clip_x(&self, p: &[f32]) -> f32 {
        (p[0] * self.inv_t[0]).max(p[1] * self.inv_t[1]).max(p[2] * self.inv_t[2])
    }

    /// Largest channel in units of its white level.
    #[inline]
    fn level(&self, p: &[f32]) -> f32 {
        (p[0] * self.inv_w[0]).max(p[1] * self.inv_w[1]).max(p[2] * self.inv_w[2])
    }

    /// Weight of a pixel: 0 once clipped, a smooth falloff below, and a ramp up from black
    /// against noise. Returns (highlight part, total). Non-finite pixels give `None`.
    #[inline]
    fn quality(&self, p: &[f32]) -> Option<(f32, f32)> {
        max_channel(p)?;
        let x = self.clip_x(p);
        let hi = if self.darkest {
            1.0
        } else if x >= 1.0 {
            0.0
        } else {
            1.0 - smoothstep(0.85, 1.0, x)
        };
        let dark = (self.level(p) / 0.05).clamp(0.0, 1.0);
        Some((hi, hi * dark))
    }
}

/// Ratio (a over b: multiplying `b` by it matches `a`) as the median of block ratios where both
/// are unclipped and not nearly black.
fn measure_ratio(a: &Frame, b: &Frame, w: usize, h: usize) -> Option<f32> {
    const B: usize = 4;
    let (bw, bh) = (w / B, h / B);
    if bw == 0 || bh == 0 {
        return None;
    }
    let block = |f: &Frame, bx: usize, by: usize| -> Option<(f32, f32, f32, [f32; 3])> {
        let (mut ys, mut ms, mut mx) = (0.0f32, 0.0f32, 0.0f32);
        let mut cs = [0.0f32; 3];
        for y in by * B..by * B + B {
            for x in bx * B..bx * B + B {
                let p = f.at(x, y, w, h)?;
                max_channel(p)?;
                ys += luma(p);
                ms += f.level(p);
                mx = mx.max(f.clip_x(p));
                for c in 0..3 {
                    cs[c] += p[c];
                }
            }
        }
        let n = (B * B) as f32;
        Some((ys / n, ms / n, mx, cs))
    };
    // The RAW loader turns pixels with one clipped channel grey without reaching the frame's white
    // level: such a block has a different ratio per channel. Only blocks whose channels agree count.
    let agree = |ca: [f32; 3], cb: [f32; 3]| -> bool {
        let r: Vec<f32> = (0..3).filter(|&c| ca[c] > 1e-6 && cb[c] > 1e-6).map(|c| ca[c] / cb[c]).collect();
        let (lo, hi) = r.iter().fold((f32::INFINITY, 0.0f32), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        !r.is_empty() && hi <= lo * RATIO_AGREEMENT
    };
    let mut ratios: Vec<f32> = (0..bh)
        .into_par_iter()
        .flat_map_iter(|by| {
            let mut row = Vec::new();
            for bx in 0..bw {
                let (Some((ya, ma, xa, ca)), Some((yb, mb, xb, cb))) = (block(a, bx, by), block(b, bx, by)) else { continue };
                // Both blocks well below the level where the loader starts to blend clipped pixels to
                // grey (a block grey in both frames would agree per channel and still be wrong).
                let safe = |f: &Frame, m: f32| !f.may_clip || m < CONSISTENCY_OWN_GATE;
                if xa < 1.0 && xb < 1.0 && ma > 0.02 && mb > 0.02 && safe(a, ma) && safe(b, mb) && ya > 0.0 && yb > 0.0 && agree(ca, cb) {
                    row.push(ya / yb);
                }
            }
            row
        })
        .collect();
    if ratios.len() * 100 < bw * bh || ratios.is_empty() {
        return None;
    }
    let mid = ratios.len() / 2;
    let (_, m, _) = ratios.select_nth_unstable_by(mid, f32::total_cmp);
    (m.is_finite() && *m > 0.0).then_some(*m)
}

/// Per output pixel the mean of `sample` over the 3x3 neighbourhood (invalid samples, `None`,
/// do not count), turned into a byte by `reduce(mean, valid_count)`.
fn box3_map<const N: usize>(
    w: usize,
    h: usize,
    sample: &(dyn Fn(usize, usize) -> Option<[f32; N]> + Sync),
    reduce: &(dyn Fn([f32; N], f32) -> u8 + Sync),
) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    out.par_chunks_mut(w).enumerate().for_each_init(
        || (vec![[0.0f32; N]; 3 * w], vec![0.0f32; 3 * w]),
        |(vals, valid), (y, row)| {
            let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
            for (r, yy) in (y0..=y1).enumerate() {
                for x in 0..w {
                    match sample(x, yy) {
                        Some(v) => {
                            vals[r * w + x] = v;
                            valid[r * w + x] = 1.0;
                        }
                        None => {
                            vals[r * w + x] = [0.0; N];
                            valid[r * w + x] = 0.0;
                        }
                    }
                }
            }
            for (x, o) in row.iter_mut().enumerate() {
                let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
                let (mut sum, mut n) = ([0.0f32; N], 0.0f32);
                for r in 0..=(y1 - y0) {
                    for xx in x0..=x1 {
                        let i = r * w + xx;
                        if valid[i] > 0.0 {
                            for c in 0..N {
                                sum[c] += vals[i][c];
                            }
                            n += 1.0;
                        }
                    }
                }
                if n > 0.0 {
                    for s in sum.iter_mut() {
                        *s /= n;
                    }
                }
                *o = reduce(sum, n);
            }
        },
    );
    out
}

/// Byte map (255 = consistent) of how well the pixels of `frame` agree with the next darker
/// frame `darker` scaled up by `r` (the exposure ratio between them). Where the darker frame
/// predicts a bright value but the brighter frame shows something else, a sensor channel of the
/// brighter frame has clipped (also when the RAW loader neutralised the colour of the pixel, so
/// its largest channel still looks harmless). Dark pixels are not checked.
fn consistency_map(frame: &Frame, darker: &Frame, r: f32, w: usize, h: usize) -> Vec<u8> {
    let t: [f32; 3] = std::array::from_fn(|c| if frame.inv_w[c] > 0.0 { 1.0 / frame.inv_w[c] } else { 0.0 });
    let sample = |x: usize, y: usize| -> Option<[f32; 6]> {
        let (pi, pd) = (frame.at(x, y, w, h)?, darker.at(x, y, w, h)?);
        max_channel(pi)?;
        max_channel(pd)?;
        let pred = [pd[0] * r, pd[1] * r, pd[2] * r];
        let bright = (0..3).any(|c| pred[c] * frame.inv_w[c] >= CONSISTENCY_GATE);
        let own = frame.level(pi) >= CONSISTENCY_OWN_GATE;
        if !bright || !own {
            return Some([0.0, 0.0, 0.0, 1.0, 1.0, 1.0]);
        }
        let d = [pi[0] - pred[0], pi[1] - pred[1], pi[2] - pred[2]];
        let s = [0.5 * (pi[0] + pred[0]), 0.5 * (pi[1] + pred[1]), 0.5 * (pi[2] + pred[2])];
        Some([d[0], d[1], d[2], s[0], s[1], s[2]])
    };
    let reduce = |m: [f32; 6], n: f32| -> u8 {
        if n < 1.0 {
            return 255;
        }
        let mut err = 0.0f32;
        for c in 0..3 {
            if t[c] > 0.0 {
                err = err.max(m[c].abs() / m[3 + c].max(0.03 * t[c]));
            }
        }
        ((1.0 - smoothstep(CONSISTENCY_LO, CONSISTENCY_HI, err)) * 255.0).round() as u8
    };
    box3_map::<6>(w, h, &sample, &reduce)
}

/// Total quality of a pixel of frame `f` including its consistency byte (`None` map = 1).
#[inline]
fn full_quality(f: &Frame, px: &[f32], cons: Option<&[u8]>, idx: usize) -> f32 {
    let Some((_, q)) = f.quality(px) else { return 0.0 };
    q * cons.map_or(1.0, |c| c[idx] as f32 / 255.0)
}

/// Per frame (except the reference) a 0..255 ghost map: 255 = the frame
/// disagrees with the reference there. Compared on the 3x3 mean of the log
/// luminance ratio, so single noisy pixels do not trigger it. Pixels where
/// either frame is clipped, inconsistent or dark are not compared.
fn ghost_map(
    reference: &Frame,
    ref_cons: Option<&[u8]>,
    frame: &Frame,
    frame_cons: Option<&[u8]>,
    deghost: f32,
    w: usize,
    h: usize,
) -> Vec<u8> {
    let thr = 1.0 + (0.15 - 1.0) * deghost.clamp(0.0, 1.0);
    let sample = |x: usize, y: usize| -> Option<[f32; 1]> {
        let (pr, pf) = (reference.at(x, y, w, h)?, frame.at(x, y, w, h)?);
        let idx = y * w + x;
        if full_quality(reference, pr, ref_cons, idx) <= 0.2 || full_quality(frame, pf, frame_cons, idx) <= 0.2 {
            return None;
        }
        let (yr, yf) = (luma(pr), luma(pf) * frame.k);
        if yr <= 0.0 || yf <= 0.0 {
            return None;
        }
        Some([(yf / yr).log2()])
    };
    let reduce = |m: [f32; 1], n: f32| -> u8 {
        if n < 3.0 {
            return 0;
        }
        (smoothstep(thr, thr * 1.5, m[0].abs()) * 255.0).round() as u8
    };
    let raw = box3_map::<1>(w, h, &sample, &reduce);
    // 3x3 max dilation, separable.
    let mut tmp = vec![0u8; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for (x, o) in row.iter_mut().enumerate() {
            let (a, b) = (x.saturating_sub(1), (x + 1).min(w - 1));
            *o = raw[y * w + a].max(raw[y * w + x]).max(raw[y * w + b]);
        }
    });
    let mut out = vec![0u8; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let (a, b) = (y.saturating_sub(1), (y + 1).min(h - 1));
        for (x, o) in row.iter_mut().enumerate() {
            *o = tmp[a * w + x].max(tmp[y * w + x]).max(tmp[b * w + x]);
        }
    });
    out
}

/// Why a merge did not produce an image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HdrError {
    Cancelled,
    Failed(String),
}

fn fail<T>(s: impl Into<String>) -> Result<T, HdrError> {
    Err(HdrError::Failed(s.into()))
}

/// Merges `frames` into one image at the exposure of `frames[reference]`.
/// `meta_ev[i]` is the exposure value of frame `i` from its metadata (see
/// `rawgraph_core::exposure_ev`; a larger value means more light).
pub fn merge_hdr(
    frames: &[&ImageBuf],
    reference: usize,
    meta_ev: &[Option<f32>],
    p: &HdrParams,
) -> Result<ImageBuf, String> {
    match merge_hdr_ex(frames, reference, meta_ev, p, &|| false, &mut Vec::new()) {
        Ok(img) => Ok(img),
        Err(HdrError::Failed(m)) => Err(m),
        Err(HdrError::Cancelled) => Err("HDR Merge: cancelled".to_owned()),
    }
}

/// [`merge_hdr`] that polls `cancel` between its stages and reports what it could not do
/// (for example "alignment unsure") in `notes`.
pub fn merge_hdr_ex(
    frames: &[&ImageBuf],
    reference: usize,
    meta_ev: &[Option<f32>],
    p: &HdrParams,
    cancel: &dyn Fn() -> bool,
    notes: &mut Vec<String>,
) -> Result<ImageBuf, HdrError> {
    let check = || if cancel() { Err(HdrError::Cancelled) } else { Ok(()) };
    if frames.is_empty() || reference >= frames.len() {
        return fail("HDR Merge: no images to merge");
    }
    if frames.len() > HDR_MAX_FRAMES {
        return fail(format!("HDR Merge: at most {HDR_MAX_FRAMES} images per group"));
    }
    let (w, h) = (frames[0].width(), frames[0].height());
    if let Some(f) = frames.iter().find(|f| (f.width(), f.height()) != (w, h)) {
        return fail(format!(
            "HDR Merge: the images of a group differ in size ({w}x{h} vs {}x{})",
            f.width(),
            f.height()
        ));
    }
    if frames.len() == 1 {
        return Ok(frames[0].clone());
    }
    let (wu, hu) = (w as usize, h as usize);
    if wu == 0 || hu == 0 {
        return Ok(frames[reference].clone());
    }
    check()?;
    let n = frames.len();
    let clip = if p.clip.is_finite() { p.clip.clamp(0.5, 1.0) } else { 0.95 };

    let mut set: Vec<Frame> = Vec::with_capacity(n);
    for img in frames {
        let (wl, plateau) = white_levels(img);
        let inv_w: [f32; 3] = std::array::from_fn(|c| if wl[c] > 0.0 { 1.0 / (clip * wl[c]) } else { 0.0 });
        let inv_t: [f32; 3] = std::array::from_fn(|c| if plateau[c] { inv_w[c] } else { 0.0 });
        let may_clip = plateau.iter().any(|p| *p) || has_neutralised_pixels(img, &inv_w);
        set.push(Frame { img, inv_w, inv_t, k: 1.0, shift: (0, 0), darkest: false, may_clip });
        check()?;
    }

    // Alignment (translation only, reference stays).
    if p.align && wu >= 16 && hu >= 16 {
        let max_px = (p.max_shift_pct.clamp(0.0, 10.0) / 100.0) * wu.max(hu) as f32;
        if max_px >= 1.0 {
            let ref_pyr = bitmap_pyramid(frames[reference]);
            for i in 0..n {
                if i == reference {
                    continue;
                }
                check()?;
                let pyr = bitmap_pyramid(frames[i]);
                let (shift, verdict) = find_shift_checked(&ref_pyr, &pyr, max_px);
                set[i].shift = shift;
                if verdict == ShiftVerdict::Unsure {
                    notes.push(format!("HDR Merge: alignment of image {} unsure, not shifted", i + 1));
                }
            }
        }
    }

    // Exposure factors.
    check()?;
    let meta = |a: usize, b: usize| -> Option<f32> {
        // Multiplying frame `b` by the result matches frame `a`.
        let (ea, eb) = (meta_ev.get(a).copied().flatten()?, meta_ev.get(b).copied().flatten()?);
        let k = (ea - eb).exp2();
        (ea.is_finite() && eb.is_finite() && k.is_finite() && k > 0.0).then_some(k)
    };
    let brightness: Vec<f32> = frames.iter().map(|f| mean_luma(f)).collect();
    let mut by_brightness: Vec<usize> = (0..n).collect();
    by_brightness.sort_by(|&a, &b| brightness[a].total_cmp(&brightness[b]).then(a.cmp(&b)));
    let mut steps: std::collections::HashMap<(usize, usize), Option<f32>> = std::collections::HashMap::new();
    let mut step = |set: &[Frame], a: usize, b: usize| -> Option<f32> {
        *steps.entry((a, b)).or_insert_with(|| measure_ratio(&set[a], &set[b], wu, hu).or_else(|| meta(a, b)))
    };
    // Exposure ratio through the neighbours (sorted by brightness) between the reference and `i`.
    let mut chain = |set: &[Frame], i: usize| -> Option<f32> {
        let pos = |f: usize| by_brightness.iter().position(|&x| x == f).unwrap_or(0);
        let (from, to) = (pos(reference), pos(i));
        let mut k = 1.0f32;
        let mut at = from;
        while at != to {
            let next = if to > at { at + 1 } else { at - 1 };
            k *= step(set, by_brightness[at], by_brightness[next])?;
            at = next;
        }
        (k.is_finite() && k > 0.0).then_some(k)
    };
    let mut dropped = vec![false; n];
    for i in 0..n {
        if i == reference {
            continue;
        }
        check()?;
        let direct = || measure_ratio(&set[reference], &set[i], wu, hu);
        let k = if p.measured {
            let measured = direct().or_else(|| chain(&set, i));
            match (measured, meta(reference, i)) {
                (Some(m), Some(md)) if (m / md).log2().abs() > MEASURED_VS_METADATA_EV => {
                    notes.push(format!(
                        "HDR Merge: the measured exposure of image {} is {:+.1} EV off the metadata; the metadata is used",
                        i + 1,
                        (m / md).log2()
                    ));
                    Some(md)
                }
                (m, md) => m.or(md),
            }
        } else {
            meta(reference, i).or_else(direct).or_else(|| chain(&set, i))
        };
        match k {
            Some(k) => set[i].k = k,
            None => {
                notes.push(format!("HDR Merge: the exposure of image {} cannot be told (no metadata, too little overlap); it is left out", i + 1));
                dropped[i] = true;
            }
        }
    }
    if dropped.iter().filter(|d| !**d).count() < 2 {
        return fail("HDR Merge: cannot tell the exposures apart (no metadata and too little overlap)");
    }
    let reference = reference - dropped[..reference].iter().filter(|d| **d).count();
    let mut set: Vec<Frame> = set.into_iter().zip(&dropped).filter(|(_, d)| !**d).map(|(f, _)| f).collect();
    let n = set.len();

    // Brightest first (smallest k); the last one is the darkest exposure.
    let mut by_k: Vec<usize> = (0..n).collect();
    by_k.sort_by(|&a, &b| set[a].k.total_cmp(&set[b].k).then(a.cmp(&b)));
    set[by_k[n - 1]].darkest = true;

    // Frame consistency: a brighter frame is trusted only where the next darker frame, scaled up, agrees.
    let mut cons: Vec<Option<Vec<u8>>> = vec![None; n];
    for (pos, &i) in by_k.iter().enumerate().take(n - 1) {
        check()?;
        let d = by_k[pos + 1];
        let r = set[d].k / set[i].k;
        if r.is_finite() && r > 0.0 {
            cons[i] = Some(consistency_map(&set[i], &set[d], r, wu, hu));
        }
    }

    // Deghost maps.
    let mut ghosts: Vec<Option<Vec<u8>>> = vec![None; n];
    if p.deghost > 0.0 {
        for i in 0..n {
            if i == reference {
                continue;
            }
            check()?;
            ghosts[i] =
                Some(ghost_map(&set[reference], cons[reference].as_deref(), &set[i], cons[i].as_deref(), p.deghost, wu, hu));
        }
    }

    check()?;
    let darkest = by_k[n - 1];
    let mut out = vec![1.0f32; wu * hu * 4];
    out.par_chunks_mut(wu * 4).enumerate().for_each(|(y, row)| {
        for x in 0..wu {
            let idx = y * wu + x;
            let mut num = [0.0f32; 3];
            let mut den = 0.0f32;
            // Fallback when every weight is 0: the frame that is most trustworthy in the highlights
            // (highlight part times consistency), the darker one on ties (it is the least clipped).
            let mut best: Option<(f32, f32, usize)> = None;
            for (i, f) in set.iter().enumerate() {
                let Some(px) = f.at(x, y, wu, hu) else { continue };
                let Some((hi, mut q)) = f.quality(px) else { continue };
                let c = cons[i].as_ref().map_or(1.0, |c| c[idx] as f32 / 255.0);
                q *= c;
                if let Some(g) = ghosts[i].as_ref() {
                    q *= 1.0 - g[idx] as f32 / 255.0;
                }
                let score = hi * c;
                if best.is_none_or(|(s, k, _)| score > s || (score == s && f.k > k)) {
                    best = Some((score, f.k, i));
                }
                let wgt = q / f.k;
                if wgt > 0.0 {
                    num[0] += wgt * f.k * px[0];
                    num[1] += wgt * f.k * px[1];
                    num[2] += wgt * f.k * px[2];
                    den += wgt;
                }
            }
            let o = &mut row[x * 4..x * 4 + 4];
            if den > 1e-12 {
                o[0] = num[0] / den;
                o[1] = num[1] / den;
                o[2] = num[2] / den;
            } else {
                // Nothing usable here. A clipped frame is no answer: where the darkest frame was shifted
                // out of the picture (a border column), its nearest pixel is closer to the truth.
                let pick = match best {
                    Some((score, _, i)) if score > 0.0 => i,
                    _ => darkest,
                };
                let f = &set[pick];
                let px = f.at(x, y, wu, hu).or_else(|| (pick == darkest).then(|| f.at_clamped(x, y, wu, hu)));
                for c in 0..3 {
                    o[c] = px.map_or(0.0, |px| if px[c].is_finite() { px[c] * f.k } else { 0.0 });
                }
            }
            o[3] = 1.0;
        }
    });

    if p.median_exposure {
        // Ascending exposure value = descending k; the lower-middle one.
        let mid = by_k[n - 1 - (n - 1) / 2];
        let s = 1.0 / set[mid].k;
        if s.is_finite() && s > 0.0 && (s - 1.0).abs() > 1e-6 {
            out.par_chunks_mut(4).for_each(|px| {
                px[0] *= s;
                px[1] *= s;
                px[2] *= s;
            });
        }
    }
    match ImageBuf::from_rgba(w, h, out) {
        Some(img) => Ok(img),
        None => fail("HDR Merge: internal size error"),
    }
}

#[cfg(test)]
mod tests;
