//! Scope computations on display-referred R'G'B' (see [`crate::display`]).
//!
//! Every pixel is counted exactly once per channel; out-of-range values are
//! clamped into the first/last bin or row, never dropped. NaN counts as 0.

use crate::display::DisplayImage;
use rawgraph_core::color::REC709_LUMA;
use rayon::prelude::*;

/// Make a sample safe for binning: NaN -> 0, infinities -> large finite values.
#[inline]
fn clean(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(-1.0e6, 1.0e6)
    }
}

#[inline]
fn luma(r: f32, g: f32, b: f32) -> f32 {
    REC709_LUMA[0] * r + REC709_LUMA[1] * g + REC709_LUMA[2] * b
}

/// Fold row chunks into per-thread accumulators of `len` counters and sum them.
/// `f(acc, width, row)` is called once per image row.
fn accumulate<F>(img: &DisplayImage, len: usize, f: F) -> Vec<u32>
where
    F: Fn(&mut [u32], usize, &[f32]) + Sync,
{
    let w = img.width as usize;
    let h = img.height as usize;
    if w == 0 || h == 0 {
        return vec![0; len];
    }
    let rows = (h / 64).max(1);
    img.rgb
        .par_chunks(w * 3 * rows)
        .fold(
            || vec![0u32; len],
            |mut acc, chunk| {
                for row in chunk.chunks_exact(w * 3) {
                    f(&mut acc, w, row);
                }
                acc
            },
        )
        .reduce(
            || vec![0u32; len],
            |mut a, b| {
                for (x, y) in a.iter_mut().zip(b) {
                    *x += y;
                }
                a
            },
        )
}

/// Per-channel and luma histogram.
#[derive(Clone, Debug, PartialEq)]
pub struct Histogram {
    pub bins: usize,
    pub r: Vec<u32>,
    pub g: Vec<u32>,
    pub b: Vec<u32>,
    pub luma: Vec<u32>,
}

#[inline]
fn bin_of(v: f32, bins: usize) -> usize {
    let i = (clean(v) * bins as f32).floor() as i64;
    i.clamp(0, bins as i64 - 1) as usize
}

/// `bin = floor(v * bins)` clamped to `0..bins`.
pub fn histogram(img: &DisplayImage, bins: usize) -> Histogram {
    let bins = bins.max(1);
    let all = accumulate(img, bins * 4, |acc, _w, row| {
        for px in row.as_chunks::<3>().0 {
            let (r, g, b) = (clean(px[0]), clean(px[1]), clean(px[2]));
            acc[bin_of(r, bins)] += 1;
            acc[bins + bin_of(g, bins)] += 1;
            acc[2 * bins + bin_of(b, bins)] += 1;
            acc[3 * bins + bin_of(luma(r, g, b), bins)] += 1;
        }
    });
    Histogram {
        bins,
        r: all[..bins].to_vec(),
        g: all[bins..2 * bins].to_vec(),
        b: all[2 * bins..3 * bins].to_vec(),
        luma: all[3 * bins..].to_vec(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaveformChannel {
    Luma,
    Red,
    Green,
    Blue,
}

/// `counts[row * columns + column]`; row 0 is the TOP and means level 1.0, the last row means 0.0.
#[derive(Clone, Debug, PartialEq)]
pub struct Waveform {
    pub columns: usize,
    pub levels: usize,
    pub counts: Vec<u32>,
}

#[inline]
fn level_row(v: f32, levels: usize) -> usize {
    let l = (clean(v) * (levels - 1) as f32).round() as i64;
    levels - 1 - l.clamp(0, levels as i64 - 1) as usize
}

fn waveform_multi(
    img: &DisplayImage,
    columns: usize,
    levels: usize,
    channels: &[WaveformChannel],
) -> Vec<Waveform> {
    let columns = columns.max(1);
    let levels = levels.max(1);
    let size = columns * levels;
    let w = img.width as usize;
    let col_of: Vec<usize> = (0..w).map(|x| x * columns / w).collect();
    let all = accumulate(img, size * channels.len(), |acc, _w, row| {
        for (x, px) in row.as_chunks::<3>().0.iter().enumerate() {
            let col = col_of[x];
            for (k, ch) in channels.iter().enumerate() {
                let v = match ch {
                    WaveformChannel::Red => px[0],
                    WaveformChannel::Green => px[1],
                    WaveformChannel::Blue => px[2],
                    WaveformChannel::Luma => luma(clean(px[0]), clean(px[1]), clean(px[2])),
                };
                acc[k * size + level_row(v, levels) * columns + col] += 1;
            }
        }
    });
    all.chunks(size)
        .map(|c| Waveform {
            columns,
            levels,
            counts: c.to_vec(),
        })
        .collect()
}

/// Waveform of one channel.
pub fn waveform(
    img: &DisplayImage,
    columns: usize,
    levels: usize,
    channel: WaveformChannel,
) -> Waveform {
    waveform_multi(img, columns, levels, &[channel]).remove(0)
}

/// RGB parade: R, G, B waveforms.
pub fn parade(img: &DisplayImage, columns: usize, levels: usize) -> [Waveform; 3] {
    let v = waveform_multi(
        img,
        columns,
        levels,
        &[
            WaveformChannel::Red,
            WaveformChannel::Green,
            WaveformChannel::Blue,
        ],
    );
    let mut it = v.into_iter();
    [it.next().unwrap(), it.next().unwrap(), it.next().unwrap()]
}

/// `counts[y * size + x]`; x grows with Cb, y = 0 is the top (Cr = +0.5).
#[derive(Clone, Debug, PartialEq)]
pub struct Vectorscope {
    pub size: usize,
    pub counts: Vec<u32>,
}

/// Rec.709 chroma difference signals of encoded R'G'B': Cb = (B'-Y')/1.8556, Cr = (R'-Y')/1.5748.
pub fn cbcr(rgb: [f32; 3]) -> (f32, f32) {
    let y = luma(rgb[0], rgb[1], rgb[2]);
    ((rgb[2] - y) / 1.8556, (rgb[0] - y) / 1.5748)
}

/// Bin position of a Cb/Cr pair, identical to what `vectorscope` uses.
pub fn vectorscope_bin(cb: f32, cr: f32, size: usize) -> (usize, usize) {
    let size = size.max(1);
    let s = (size - 1) as f32;
    let x = ((clean(cb) + 0.5) * s).round() as i64;
    let y = ((0.5 - clean(cr)) * s).round() as i64;
    (
        x.clamp(0, size as i64 - 1) as usize,
        y.clamp(0, size as i64 - 1) as usize,
    )
}

pub fn vectorscope(img: &DisplayImage, size: usize) -> Vectorscope {
    let size = size.max(1);
    let counts = accumulate(img, size * size, |acc, _w, row| {
        for px in row.as_chunks::<3>().0 {
            let (cb, cr) = cbcr([clean(px[0]), clean(px[1]), clean(px[2])]);
            let (x, y) = vectorscope_bin(cb, cr, size);
            acc[y * size + x] += 1;
        }
    });
    Vectorscope { size, counts }
}

/// (name, Cb, Cr) of 100 % R, Mg, B, Cy, G, Yl; multiply by 0.75 for 75 % targets.
pub fn vectorscope_targets() -> [(&'static str, f32, f32); 6] {
    let t = |name, rgb| {
        let (cb, cr) = cbcr(rgb);
        (name, cb, cr)
    };
    [
        t("R", [1.0, 0.0, 0.0]),
        t("Mg", [1.0, 0.0, 1.0]),
        t("B", [0.0, 0.0, 1.0]),
        t("Cy", [0.0, 1.0, 1.0]),
        t("G", [0.0, 1.0, 0.0]),
        t("Yl", [1.0, 1.0, 0.0]),
    ]
}

/// Skin-tone (I) line, degrees counter-clockwise from the +Cb axis with Cr up.
pub const SKIN_TONE_LINE_DEG: f32 = 123.0;

/// Layout of the parade scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ParadeMode {
    /// Three columns: R', G', B'.
    #[default]
    Rgb,
    /// Four columns: luma Y', R', G', B'.
    Yrgb,
    /// Three columns: Y', Cb, Cr (Rec.709). Cb and Cr are drawn with 0 at mid-height
    /// (level = chroma + 0.5, chroma in -0.5..=0.5).
    YCbCr,
}

impl ParadeMode {
    /// Number of columns (planes) of the parade.
    pub fn planes(self) -> usize {
        match self {
            ParadeMode::Rgb | ParadeMode::YCbCr => 3,
            ParadeMode::Yrgb => 4,
        }
    }

    /// Short names of the planes, left to right.
    pub fn plane_names(self) -> &'static [&'static str] {
        match self {
            ParadeMode::Rgb => &["R", "G", "B"],
            ParadeMode::Yrgb => &["Y", "R", "G", "B"],
            ParadeMode::YCbCr => &["Y", "Cb", "Cr"],
        }
    }

    /// Indices into the per-pixel plane values `[R, G, B, Y, Cb + 0.5, Cr + 0.5]`.
    fn plane_ids(self) -> &'static [usize] {
        match self {
            ParadeMode::Rgb => &[0, 1, 2],
            ParadeMode::Yrgb => &[3, 0, 1, 2],
            ParadeMode::YCbCr => &[3, 4, 5],
        }
    }
}

/// Radius (in samples) of the horizontal box filter used by the low-pass option: the
/// window is `2 * radius + 1` samples wide, about 1/100 of the image width. It is
/// measured in sampled pixels, so `step` shrinks it. 0 means no filtering.
pub fn lowpass_radius(width: usize, step: usize) -> usize {
    width / 200 / step.max(1)
}

/// Which scopes [`analyze`] computes in its single pass.
///
/// `waveform` is `(columns, levels)` of the waveform (of `waveform_channel`, luma by
/// default), `parade` is `(columns, levels)` per plane (planes per `parade_mode`),
/// `vectorscope` the size, `histogram` the bin count. `step` samples every n-th pixel
/// in x and y (1 = all, 0 is treated as 1).
///
/// The additions `waveform_channel`, `parade_mode`, `overlay`, `lowpass` and `clip_stats`
/// default (see [`Default`]) to the behaviour of the first version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScopeRequest {
    pub waveform: Option<(usize, usize)>,
    pub parade: Option<(usize, usize)>,
    pub vectorscope: Option<usize>,
    pub histogram: Option<usize>,
    pub step: usize,
    /// Channel shown by the `waveform` scope.
    pub waveform_channel: WaveformChannel,
    /// Planes of the `parade` scope.
    pub parade_mode: ParadeMode,
    /// Separate R, G, B waveforms `(columns, levels)` for an additive RGB overlay, for
    /// callers whose `parade` is not an RGB-based mode.
    pub overlay: Option<(usize, usize)>,
    /// Horizontal box filter (see [`lowpass_radius`]) over the samples of every scope
    /// column of waveform, parade and overlay before counting. Histogram, vectorscope
    /// and clipping statistics always see the unfiltered pixels.
    pub lowpass: bool,
    /// Also compute [`ClipStats`].
    pub clip_stats: bool,
    /// Also accumulate the colour of the pixels behind every `waveform` bin
    /// ([`ScopeSet::waveform_colors`]), so that views can colour the trace like the
    /// picture. Costs three extra additions per sampled pixel; only the `waveform` set is
    /// concerned (not parade or overlay).
    pub colorize: bool,
}

impl Default for ScopeRequest {
    fn default() -> Self {
        Self {
            waveform: None,
            parade: None,
            vectorscope: None,
            histogram: None,
            step: 1,
            waveform_channel: WaveformChannel::Luma,
            parade_mode: ParadeMode::Rgb,
            overlay: None,
            lowpass: false,
            clip_stats: false,
            colorize: false,
        }
    }
}

/// Flag of [`ScopeEdges`]: the column holds a value that lands in the top row (level 1.0).
pub const EDGE_HIGH: u8 = 1;
/// Flag of [`ScopeEdges`]: the column holds a value that lands in the bottom row (level 0.0).
pub const EDGE_LOW: u8 = 2;

/// Per column [`EDGE_HIGH`] / [`EDGE_LOW`] flags of the unfiltered pixels, for the scope
/// markers of clipped columns. Only produced when the low-pass filter is on: it moves
/// peaks away from the edge rows, so the counts alone would no longer show them.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ScopeEdges {
    /// Flags per column of the waveform.
    pub waveform: Vec<u8>,
    /// Flags per column for every parade plane, in plane order.
    pub parade: Vec<Vec<u8>>,
    /// Flags per column for the R, G, B planes of the `overlay` set.
    pub overlay: Vec<Vec<u8>>,
}

/// Number of channels of [`ClipStats`]: R, G, B, luma.
pub const CLIP_CHANNELS: usize = 4;

/// Clipping statistics of the analysed pixels, per channel in the order R, G, B, luma.
///
/// Values are the encoded (display-referred) values; NaN counts as 0 and infinities as
/// +-1e6 (so they are clipped and show up in `min` / `max`). A luma value counts as
/// clipped high from `1.0 - 1e-6` on, so that neutral white is not lost to rounding.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ClipStats {
    /// Number of analysed pixels (equals `ScopeSet::samples`).
    pub samples: u64,
    /// Pixels with value >= 1.0.
    pub high: [u64; CLIP_CHANNELS],
    /// Pixels with value <= 0.0.
    pub low: [u64; CLIP_CHANNELS],
    pub min: [f32; CLIP_CHANNELS],
    pub max: [f32; CLIP_CHANNELS],
}

impl ClipStats {
    /// Share (0..=1) of pixels with value >= 1.0 in channel `c` (R, G, B, luma).
    pub fn high_share(&self, c: usize) -> f32 {
        share(self.high[c], self.samples)
    }

    /// Share (0..=1) of pixels with value <= 0.0 in channel `c`.
    pub fn low_share(&self, c: usize) -> f32 {
        share(self.low[c], self.samples)
    }

    /// True when any channel has a clipped pixel on either side.
    pub fn any_clipped(&self) -> bool {
        self.high.iter().chain(&self.low).any(|&n| n > 0)
    }
}

fn share(n: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (n as f64 / total as f64) as f32
    }
}

/// Colour sums of a waveform: for every bin the sum of the clamped (0..=1) display colours
/// of the pixels counted there, as integers of scale [`WaveformColors::SCALE`].
///
/// Use [`WaveformColors::sum`] / [`WaveformColors::mean`] with the row-major bin index of
/// `Waveform::counts` (`row * columns + column`); the sums themselves are stored per column
/// (`rgb_sum[(column * levels + row) * 3 + channel]`), the order they are produced in. The
/// mean colour of a bin is `sum / (count * SCALE)`. Integer sums keep the result independent
/// of the thread schedule; sums saturate instead of wrapping.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct WaveformColors {
    pub columns: usize,
    pub levels: usize,
    pub rgb_sum: Vec<u32>,
}

impl WaveformColors {
    /// Value of 1.0 in the sums.
    pub const SCALE: u32 = 255;

    /// Colour sum of bin `i` (row-major index as in `Waveform::counts`); `None` outside the data.
    pub fn sum(&self, i: usize) -> Option<[u32; 3]> {
        if self.columns == 0 {
            return None;
        }
        let (row, col) = (i / self.columns, i % self.columns);
        if row >= self.levels {
            return None;
        }
        let o = (col * self.levels + row) * 3;
        let s = self.rgb_sum.get(o..o + 3)?;
        Some([s[0], s[1], s[2]])
    }

    /// Mean display colour (0..=1 per channel, capped at 1) of bin `i`, which holds `count`
    /// pixels; `None` for an empty bin or an index outside the data.
    pub fn mean(&self, i: usize, count: u32) -> Option<[f32; 3]> {
        if count == 0 {
            return None;
        }
        let s = self.sum(i)?;
        let k = 1.0 / (count as f32 * Self::SCALE as f32);
        Some([s[0] as f32 * k, s[1] as f32 * k, s[2] as f32 * k].map(|v| v.min(1.0)))
    }
}

/// Result of [`analyze`]; `samples` is the number of pixels that were counted.
///
/// `parade` holds the RGB parade ([`ParadeMode::Rgb`]); for the other modes the planes are
/// in `parade_planes` instead. [`ScopeSet::parade_columns`] returns whichever is set.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ScopeSet {
    pub waveform: Option<Waveform>,
    pub parade: Option<[Waveform; 3]>,
    pub vectorscope: Option<Vectorscope>,
    pub histogram: Option<Histogram>,
    pub samples: u64,
    /// Parade planes of [`ParadeMode::Yrgb`] / [`ParadeMode::YCbCr`], left to right.
    pub parade_planes: Option<Vec<Waveform>>,
    /// R, G, B waveforms requested with `ScopeRequest::overlay`.
    pub overlay: Option<[Waveform; 3]>,
    /// Present when `ScopeRequest::clip_stats` was set.
    pub clipping: Option<ClipStats>,
    /// Edge flags of the unfiltered pixels; present only with `ScopeRequest::lowpass`.
    pub unfiltered_edges: Option<ScopeEdges>,
    /// Colour behind every bin of `waveform`; present with `ScopeRequest::colorize`.
    /// With the low-pass filter the colour is that of the unfiltered pixel.
    pub waveform_colors: Option<WaveformColors>,
}

impl ScopeSet {
    /// The parade planes in display order, whatever the mode.
    pub fn parade_columns(&self) -> Option<Vec<&Waveform>> {
        if let Some(p) = &self.parade_planes {
            Some(p.iter().collect())
        } else {
            self.parade.as_ref().map(|p| p.iter().collect())
        }
    }
}

/// One counted set of waveform-like planes inside the shared accumulator.
struct Dataset {
    columns: usize,
    levels: usize,
    ids: &'static [usize],
    col_of: Vec<usize>,
    offset: usize,
    /// Start of this dataset's flags (one `columns`-long run per plane).
    flag_offset: usize,
}

impl Dataset {
    fn size(&self) -> usize {
        self.columns * self.levels
    }
    fn total(&self) -> usize {
        self.size() * self.ids.len()
    }
}

struct Acc {
    planes: Vec<u32>,
    /// Edge flags of the unfiltered pixels (low-pass only).
    flags: Vec<u8>,
    vs: Vec<u32>,
    hi: Vec<u32>,
    hi_clip: [u32; CLIP_CHANNELS],
    lo_clip: [u32; CLIP_CHANNELS],
    min: [f32; CLIP_CHANNELS],
    max: [f32; CLIP_CHANNELS],
    /// Filtered samples of the current row (3 floats per sample); empty without low-pass.
    scratch: Vec<f32>,
    n: u64,
}

fn add_into(a: &mut [u32], b: &[u32]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x += y;
    }
}

/// `[R, G, B, Y, Cb + 0.5, Cr + 0.5]` of cleaned encoded values.
#[inline]
fn plane_values(r: f32, g: f32, b: f32) -> [f32; 6] {
    let y = luma(r, g, b);
    let (cb, cr) = cbcr([r, g, b]);
    [r, g, b, y, cb + 0.5, cr + 0.5]
}

/// [`level_row`] without the library `round` call for the usual non-negative values
/// (identical results: round half away from zero is `trunc + (frac >= 0.5)` there).
#[inline]
fn level_row_fast(v: f32, levels: usize) -> usize {
    if (0.0..=1.0e6).contains(&v) {
        let t = v * (levels - 1) as f32;
        let i = t as u32;
        let l = if t - i as f32 >= 0.5 { i + 1 } else { i } as usize;
        levels - 1 - l.min(levels - 1)
    } else {
        level_row(v, levels)
    }
}

/// A raw display value as an integer of scale [`WaveformColors::SCALE`]: NaN and negatives are
/// 0, anything above 1 is 1.
#[inline]
fn quantize_raw(v: f32) -> u32 {
    let v = if v > 0.0 { v.min(1.0) } else { 0.0 };
    (v * WaveformColors::SCALE as f32 + 0.5) as u32
}

/// Colour sums of the bins of the waveform set `d` (second pass over the picture), per column.
///
/// The columns are split between the threads, so every bin is written by exactly one
/// thread: no per-thread copies of the sums to allocate, merge and free (which cost more
/// than the counting itself). The pixels visited, their bins (including the low-pass
/// filter) and therefore the counts agree with the first pass, up to the rounding of the
/// filter sums at the very edge between two levels.
fn colour_pass(img: &DisplayImage, step: usize, radius: usize, d: &Dataset) -> Vec<u32> {
    let (w, h) = (img.width as usize, img.height as usize);
    let (columns, levels) = (d.columns, d.levels);
    // Per column (`[column][level][rgb]`): the columns of one task are contiguous.
    let mut by_column = vec![0u32; columns * levels * 3];
    if w == 0 || h == 0 {
        return by_column;
    }
    let id = d.ids[0];
    // First pixel of every column (`columns + 1` entries; `col_of` never decreases).
    let col_start: Vec<usize> = (0..=columns)
        .map(|c| d.col_of.partition_point(|&k| k < c))
        .collect();
    let groups = rayon::current_num_threads().clamp(1, columns);
    let per = columns.div_ceil(groups);
    by_column
        .par_chunks_mut(per * levels * 3)
        .enumerate()
        .for_each(|(gi, local)| {
            let c0 = gi * per;
            let c1 = (c0 + per).min(columns);
            let (x_lo, x_hi) = (col_start[c0], col_start[c1]);
            if x_lo >= x_hi {
                return;
            }
            // Samples of the low-pass window around this task's pixels (a multiple of `step`
            // away from the left edge, like the samples of the full row).
            let pad = (radius + 1) * step;
            let s0 = x_lo.saturating_sub(pad) / step * step;
            let e = (x_hi + pad).min(w);
            let (mut scratch, mut pre) = (Vec::new(), Vec::new());
            for y in (0..h).step_by(step) {
                let row = &img.rgb[y * w * 3..(y + 1) * w * 3];
                if radius > 0 {
                    lowpass_row(
                        &row[s0 * 3..e * 3],
                        e - s0,
                        step,
                        radius,
                        &mut scratch,
                        &mut pre,
                    );
                }
                for col in c0..c1 {
                    let cell = &mut local[(col - c0) * levels * 3..(col - c0 + 1) * levels * 3];
                    let (xa, xb) = (col_start[col], col_start[col + 1]);
                    for x in (xa.div_ceil(step) * step..xb).step_by(step) {
                        let px = &row[x * 3..x * 3 + 3];
                        let (fr, fg, fb) = if radius > 0 {
                            let j = (x - s0) / step;
                            (scratch[j * 3], scratch[j * 3 + 1], scratch[j * 3 + 2])
                        } else {
                            (clean(px[0]), clean(px[1]), clean(px[2]))
                        };
                        let v = match id {
                            0 => fr,
                            1 => fg,
                            2 => fb,
                            3 => luma(fr, fg, fb),
                            _ => plane_values(fr, fg, fb)[id],
                        };
                        let o = level_row_fast(v, levels) * 3;
                        let c = &mut cell[o..o + 3];
                        c[0] = c[0].saturating_add(quantize_raw(px[0]));
                        c[1] = c[1].saturating_add(quantize_raw(px[1]));
                        c[2] = c[2].saturating_add(quantize_raw(px[2]));
                    }
                }
            }
        });
    by_column
}

/// Sums the counter arrays of all `accs` into the first one. The arrays are summed in
/// parallel over their bins (no tree of whole-array merges), which keeps the merge cheap
/// next to the counting; the sums are integers, so the result does not depend on the order.
fn merge_accs(mut accs: Vec<Acc>, empty: impl Fn() -> Acc) -> Acc {
    if accs.is_empty() {
        return empty();
    }
    let mut base = accs.remove(0);
    let others = &accs;
    const CHUNK: usize = 16 * 1024;
    base.planes
        .par_chunks_mut(CHUNK)
        .enumerate()
        .for_each(|(ci, ch)| {
            for o in others {
                add_into(ch, &o.planes[ci * CHUNK..]);
            }
        });
    for o in others {
        for (x, y) in base.flags.iter_mut().zip(&o.flags) {
            *x |= y;
        }
        add_into(&mut base.vs, &o.vs);
        add_into(&mut base.hi, &o.hi);
        for k in 0..CLIP_CHANNELS {
            base.hi_clip[k] += o.hi_clip[k];
            base.lo_clip[k] += o.lo_clip[k];
            base.min[k] = base.min[k].min(o.min[k]);
            base.max[k] = base.max[k].max(o.max[k]);
        }
        base.n += o.n;
    }
    // Freeing a dozen large buffers one after the other is slow; do it on the pool.
    accs.into_par_iter().for_each(drop);
    base
}

/// Box-filters the sampled pixels of one row (every `step`-th, cleaned) with the
/// symmetric window `2 * radius + 1` that shrinks symmetrically at the edges, so a
/// linear ramp stays exactly linear. Result in `out` (3 floats per sample).
fn lowpass_row(
    row: &[f32],
    w: usize,
    step: usize,
    radius: usize,
    out: &mut Vec<f32>,
    pre: &mut Vec<f64>,
) {
    let ns = w.div_ceil(step);
    // prefix sums per channel, interleaved: pre[i * 3 + c] = sum of the samples before i
    pre.clear();
    pre.resize((ns + 1) * 3, 0.0);
    for i in 0..ns {
        let px = &row[i * step * 3..i * step * 3 + 3];
        for c in 0..3 {
            pre[(i + 1) * 3 + c] = pre[i * 3 + c] + f64::from(clean(px[c]));
        }
    }
    out.clear();
    out.resize(ns * 3, 0.0);
    for i in 0..ns {
        let r = radius.min(i).min(ns - 1 - i);
        let (lo, hi) = (i - r, i + r + 1);
        let inv = 1.0 / (hi - lo) as f64;
        for c in 0..3 {
            out[i * 3 + c] = ((pre[hi * 3 + c] - pre[lo * 3 + c]) * inv) as f32;
        }
    }
}

/// Compute every requested scope in ONE pass over the pixels. With `step = 1` and
/// default additions each result equals the corresponding single-scope function.
pub fn analyze(img: &DisplayImage, request: &ScopeRequest) -> ScopeSet {
    let step = request.step.max(1);
    let w = img.width as usize;
    let h = img.height as usize;
    let wf_dim = request.waveform.map(|(c, l)| (c.max(1), l.max(1)));
    let pa_dim = request.parade.map(|(c, l)| (c.max(1), l.max(1)));
    let ov_dim = request.overlay.map(|(c, l)| (c.max(1), l.max(1)));
    let vs_size = request.vectorscope.map(|s| s.max(1));
    let bins = request.histogram.map(|b| b.max(1));
    let wf_ids: &'static [usize] = match request.waveform_channel {
        WaveformChannel::Red => &[0],
        WaveformChannel::Green => &[1],
        WaveformChannel::Blue => &[2],
        WaveformChannel::Luma => &[3],
    };
    let pa_ids = request.parade_mode.plane_ids();

    // Waveform, parade and overlay share one flat counter array.
    let mut sets: Vec<Dataset> = Vec::new();
    let mut offset = 0;
    let mut flag_offset = 0;
    let mut add_set = |dim: Option<(usize, usize)>, ids: &'static [usize]| -> Option<usize> {
        let (columns, levels) = dim?;
        let col_of = (0..w).map(|x| x * columns / w).collect();
        let d = Dataset {
            columns,
            levels,
            ids,
            col_of,
            offset,
            flag_offset,
        };
        offset += d.total();
        flag_offset += columns * ids.len();
        sets.push(d);
        Some(sets.len() - 1)
    };
    let wf_ix = add_set(wf_dim, wf_ids);
    let pa_ix = add_set(pa_dim, pa_ids);
    let ov_ix = add_set(ov_dim, &[0, 1, 2]);
    let plane_total = offset;
    let flag_total = flag_offset;
    let radius = if request.lowpass && !sets.is_empty() {
        lowpass_radius(w, step)
    } else {
        0
    };
    let clip = request.clip_stats;
    let new_acc = || Acc {
        planes: vec![0; plane_total],
        flags: vec![0; if radius > 0 { flag_total } else { 0 }],
        vs: vec![0; vs_size.map_or(0, |s| s * s)],
        hi: vec![0; bins.map_or(0, |b| b * 4)],
        hi_clip: [0; CLIP_CHANNELS],
        lo_clip: [0; CLIP_CHANNELS],
        min: [f32::INFINITY; CLIP_CHANNELS],
        max: [f32::NEG_INFINITY; CLIP_CHANNELS],
        scratch: Vec::new(),
        n: 0,
    };
    let count_pass = || {
        if w == 0 || h == 0 {
            new_acc()
        } else {
            let rows = h.div_ceil(rayon::current_num_threads()).max(1);
            let accs: Vec<Acc> = img
                .rgb
                .par_chunks(w * 3 * rows)
                .enumerate()
                .fold(new_acc, |mut a, (ci, chunk)| {
                    let mut pre: Vec<f64> = Vec::new();
                    for (ri, row) in chunk.chunks_exact(w * 3).enumerate() {
                        if !(ci * rows + ri).is_multiple_of(step) {
                            continue;
                        }
                        if radius > 0 {
                            let mut scratch = std::mem::take(&mut a.scratch);
                            lowpass_row(row, w, step, radius, &mut scratch, &mut pre);
                            a.scratch = scratch;
                        }
                        for (xi, x) in (0..w).step_by(step).enumerate() {
                            let px = &row[x * 3..x * 3 + 3];
                            let (r, g, b) = (clean(px[0]), clean(px[1]), clean(px[2]));
                            let y = luma(r, g, b);
                            a.n += 1;
                            if !sets.is_empty() {
                                let v = if radius > 0 {
                                    let f = &a.scratch[xi * 3..xi * 3 + 3];
                                    plane_values(f[0], f[1], f[2])
                                } else {
                                    plane_values(r, g, b)
                                };
                                if radius > 0 {
                                    // The filter moves peaks off the edge rows: flag the
                                    // columns from the unfiltered values.
                                    let raw = plane_values(r, g, b);
                                    for d in &sets {
                                        let col = d.col_of[x];
                                        for (k, &id) in d.ids.iter().enumerate() {
                                            let row = level_row(raw[id], d.levels);
                                            let f =
                                                &mut a.flags[d.flag_offset + k * d.columns + col];
                                            if row == 0 {
                                                *f |= EDGE_HIGH;
                                            }
                                            if row == d.levels - 1 {
                                                *f |= EDGE_LOW;
                                            }
                                        }
                                    }
                                }
                                for d in &sets {
                                    let col = d.col_of[x];
                                    for (k, &id) in d.ids.iter().enumerate() {
                                        let i = d.offset
                                            + k * d.size()
                                            + level_row(v[id], d.levels) * d.columns
                                            + col;
                                        a.planes[i] += 1;
                                    }
                                }
                            }
                            if let Some(s) = vs_size {
                                let (cb, cr) = cbcr([r, g, b]);
                                let (vx, vy) = vectorscope_bin(cb, cr, s);
                                a.vs[vy * s + vx] += 1;
                            }
                            if let Some(n) = bins {
                                a.hi[bin_of(r, n)] += 1;
                                a.hi[n + bin_of(g, n)] += 1;
                                a.hi[2 * n + bin_of(b, n)] += 1;
                                a.hi[3 * n + bin_of(y, n)] += 1;
                            }
                            if clip {
                                for (k, v) in [r, g, b, y].into_iter().enumerate() {
                                    let top = if k == 3 { 1.0 - 1.0e-6 } else { 1.0 };
                                    a.hi_clip[k] += u32::from(v >= top);
                                    a.lo_clip[k] += u32::from(v <= 0.0);
                                    a.min[k] = a.min[k].min(v);
                                    a.max[k] = a.max[k].max(v);
                                }
                            }
                        }
                    }
                    a
                })
                .collect();
            merge_accs(accs, new_acc)
        }
    };
    let colour_task = || match (request.colorize, wf_ix) {
        (true, Some(ix)) => Some(WaveformColors {
            columns: sets[ix].columns,
            levels: sets[ix].levels,
            rgb_sum: colour_pass(img, step, radius, &sets[ix]),
        }),
        _ => None,
    };
    // The colour pass is independent of the counting: running both on the pool lets it use the
    // threads that would otherwise idle while the counting merges its partial results.
    let (acc, waveform_colors) = rayon::join(count_pass, colour_task);
    let wave = |d: &Dataset, k: usize| Waveform {
        columns: d.columns,
        levels: d.levels,
        counts: acc.planes[d.offset + k * d.size()..d.offset + (k + 1) * d.size()].to_vec(),
    };
    let rgb_triplet = |ix: usize| [wave(&sets[ix], 0), wave(&sets[ix], 1), wave(&sets[ix], 2)];
    let planes_of = |ix: usize| {
        (0..sets[ix].ids.len())
            .map(|k| wave(&sets[ix], k))
            .collect::<Vec<_>>()
    };
    let flags_of = |ix: usize| -> Vec<Vec<u8>> {
        let d = &sets[ix];
        (0..d.ids.len())
            .map(|k| {
                acc.flags[d.flag_offset + k * d.columns..d.flag_offset + (k + 1) * d.columns]
                    .to_vec()
            })
            .collect()
    };
    let unfiltered_edges = (radius > 0).then(|| ScopeEdges {
        waveform: wf_ix.map(|ix| flags_of(ix).remove(0)).unwrap_or_default(),
        parade: pa_ix.map(flags_of).unwrap_or_default(),
        overlay: ov_ix.map(flags_of).unwrap_or_default(),
    });
    let (parade, parade_planes) = match pa_ix {
        Some(ix) if request.parade_mode == ParadeMode::Rgb => (Some(rgb_triplet(ix)), None),
        Some(ix) => (None, Some(planes_of(ix))),
        None => (None, None),
    };
    ScopeSet {
        waveform: wf_ix.map(|ix| wave(&sets[ix], 0)),
        parade,
        vectorscope: vs_size.map(|size| Vectorscope {
            size,
            counts: acc.vs.clone(),
        }),
        histogram: bins.map(|b| Histogram {
            bins: b,
            r: acc.hi[..b].to_vec(),
            g: acc.hi[b..2 * b].to_vec(),
            b: acc.hi[2 * b..3 * b].to_vec(),
            luma: acc.hi[3 * b..].to_vec(),
        }),
        samples: acc.n,
        parade_planes,
        overlay: ov_ix.map(rgb_triplet),
        unfiltered_edges,
        waveform_colors,
        clipping: clip.then(|| {
            // No sample leaves the +-inf start values in place.
            let fix = |v: [f32; CLIP_CHANNELS]| v.map(|x| if x.is_finite() { x } else { 0.0 });
            ClipStats {
                samples: acc.n,
                high: acc.hi_clip.map(u64::from),
                low: acc.lo_clip.map(u64::from),
                min: fix(acc.min),
                max: fix(acc.max),
            }
        }),
    }
}
