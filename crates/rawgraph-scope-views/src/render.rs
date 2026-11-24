//! Pure count -> pixel conversion (no egui types), so it is unit-testable.
//!
//! Output is premultiplied RGBA8 where the colour channels equal the intended colour
//! on a black background and alpha is the largest colour channel. Zero counts give
//! `[0, 0, 0, 0]`.

use rawgraph_scopes::WaveformColors;

/// Exponent applied to the log-normalised count; below 1 lifts sparse traces.
pub const BRIGHTNESS_GAMMA: f32 = 0.6;

/// One channel of trace data with the colour it is drawn in.
#[derive(Clone, Copy, Debug)]
pub struct Layer<'a> {
    pub counts: &'a [u32],
    pub tint: [f32; 3],
}

/// Largest count over all layers (the shared normalisation reference).
pub fn reference_of(layers: &[Layer<'_>]) -> u32 {
    layers
        .iter()
        .flat_map(|l| l.counts.iter().copied())
        .max()
        .unwrap_or(0)
}

/// Maps a count to brightness 0..=1: log-normalised against `reference`, gamma-lifted,
/// multiplied by `gain`. Monotonic in `count` and in `gain`.
pub fn brightness(count: u32, reference: u32, gain: f32) -> f32 {
    if count == 0 || reference == 0 {
        return 0.0;
    }
    let x = (count as f32).ln_1p() / (reference as f32).ln_1p();
    (x.clamp(0.0, 1.0).powf(BRIGHTNESS_GAMMA) * gain.max(0.0)).clamp(0.0, 1.0)
}

fn to_byte(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}

fn pack(rgb: [f32; 3]) -> [u8; 4] {
    let c = [to_byte(rgb[0]), to_byte(rgb[1]), to_byte(rgb[2])];
    let a = c[0].max(c[1]).max(c[2]);
    [c[0], c[1], c[2], a]
}

/// Converts counts to premultiplied RGBA8 of `width * height * 4` bytes.
/// Layers are added (clamped), so R+G+B layers overlap to white.
/// Missing counts (slices shorter than `width * height`) count as zero.
pub fn layers_to_rgba(layers: &[Layer<'_>], width: usize, height: usize, gain: f32) -> Vec<u8> {
    layers_to_rgba_with_reference(layers, width, height, gain, reference_of(layers))
}

/// Like [`layers_to_rgba`] with an explicit normalisation reference (used by the
/// parade so that the three channels share one scale). No glow.
pub fn layers_to_rgba_with_reference(
    layers: &[Layer<'_>],
    width: usize,
    height: usize,
    gain: f32,
    reference: u32,
) -> Vec<u8> {
    layers_to_rgba_glow(layers, width, height, gain, reference, false)
}

/// Soft glow of a brightness field: each texel is lifted to a fraction of its
/// vertical neighbours (+-1 row 0.35, +-2 rows 0.12) and its horizontal neighbours
/// (+-1 column 0.2). The reach in x is a single texel, so vertical step edges of
/// colour bars are not smeared; the original texel is never darkened.
pub fn glow_filter(b: &[f32], width: usize, height: usize) -> Vec<f32> {
    let mut out = b.to_vec();
    if width == 0 || height == 0 || b.len() < width * height {
        return out;
    }
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let mut v = b[i];
            if v > 0.0 {
                continue;
            }
            let mut take = |dx: isize, dy: isize, k: f32| {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < width && (ny as usize) < height {
                    v = v.max(k * b[ny as usize * width + nx as usize]);
                }
            };
            take(0, -1, 0.35);
            take(0, 1, 0.35);
            take(0, -2, 0.12);
            take(0, 2, 0.12);
            take(-1, 0, 0.2);
            take(1, 0, 0.2);
            out[i] = v;
        }
    }
    out
}

/// [`glow_filter`] that also says where each texel's value comes from: `src[i]` is the
/// texel whose (scaled) brightness `out[i]` is (`i` itself when it was not lifted).
pub fn glow_filter_src(b: &[f32], width: usize, height: usize) -> (Vec<f32>, Vec<u32>) {
    let mut out = b.to_vec();
    let mut src: Vec<u32> = (0..b.len() as u32).collect();
    if width == 0 || height == 0 || b.len() < width * height {
        return (out, src);
    }
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if b[i] > 0.0 {
                continue;
            }
            let (mut v, mut from) = (b[i], i);
            let mut take = |dx: isize, dy: isize, k: f32| {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < width && (ny as usize) < height {
                    let j = ny as usize * width + nx as usize;
                    let c = k * b[j];
                    if c > v {
                        v = c;
                        from = j;
                    }
                }
            };
            take(0, -1, 0.35);
            take(0, 1, 0.35);
            take(0, -2, 0.12);
            take(0, 2, 0.12);
            take(-1, 0, 0.2);
            take(1, 0, 0.2);
            out[i] = v;
            src[i] = from as u32;
        }
    }
    (out, src)
}

/// Trace colour of a bin whose pixels averaged `mean` (display RGB, 0..=1): the mean
/// scaled so that its largest channel is 1 (hue and saturation of the picture content at
/// full brightness; the count sets the brightness). Black and unknown means give white.
pub fn trace_colour_of_mean(mean: Option<[f32; 3]>) -> [f32; 3] {
    let Some(m) = mean else { return [1.0; 3] };
    let top = m[0].max(m[1]).max(m[2]);
    if top < 1.0e-3 {
        return [1.0; 3];
    }
    m.map(|c| (c / top).clamp(0.0, 1.0))
}

/// One waveform trace coloured like the picture: the brightness follows the counts exactly
/// like [`layers_to_rgba_glow`] with a single layer, the colour of every texel is the
/// [`trace_colour_of_mean`] of the pixels behind it (`colors` holds their sums). Glow texels
/// take the colour of the texel they were lifted from. Missing data counts as zero / white.
pub fn colored_trace_to_rgba(
    counts: &[u32],
    colors: &WaveformColors,
    width: usize,
    height: usize,
    gain: f32,
    reference: u32,
    glow: bool,
) -> Vec<u8> {
    let n = width.saturating_mul(height);
    let b: Vec<f32> = (0..n)
        .map(|i| brightness(counts.get(i).copied().unwrap_or(0), reference, gain))
        .collect();
    let (b, src) = if glow {
        glow_filter_src(&b, width, height)
    } else {
        let src = (0..n as u32).collect();
        (b, src)
    };
    let mut out = Vec::with_capacity(n * 4);
    for (i, &v) in b.iter().enumerate() {
        if v <= 0.0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
            continue;
        }
        let j = src[i] as usize;
        let mean = counts.get(j).and_then(|&c| colors.mean(j, c));
        let t = trace_colour_of_mean(mean);
        out.extend_from_slice(&pack([v * t[0], v * t[1], v * t[2]]));
    }
    out
}

/// Full-featured form of [`layers_to_rgba`]: explicit reference and optional glow.
pub fn layers_to_rgba_glow(
    layers: &[Layer<'_>],
    width: usize,
    height: usize,
    gain: f32,
    reference: u32,
    glow: bool,
) -> Vec<u8> {
    let n = width.saturating_mul(height);
    let fields: Vec<Vec<f32>> = layers
        .iter()
        .map(|l| {
            let b: Vec<f32> = (0..n)
                .map(|i| brightness(l.counts.get(i).copied().unwrap_or(0), reference, gain))
                .collect();
            if glow {
                glow_filter(&b, width, height)
            } else {
                b
            }
        })
        .collect();
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n {
        let mut rgb = [0.0f32; 3];
        for (f, l) in fields.iter().zip(layers) {
            let b = f[i];
            if b > 0.0 {
                for k in 0..3 {
                    rgb[k] += b * l.tint[k];
                }
            }
        }
        out.extend_from_slice(&pack(rgb));
    }
    out
}

/// Colour for the vectorscope position (`cb`, `cr` in -0.5..=0.5): the hue at that
/// position, normalised and diluted with white so desaturated areas stay visible.
pub fn hue_tint(cb: f32, cr: f32) -> [f32; 3] {
    let y = 0.5;
    let r = (y + 1.5748 * cr).max(0.0);
    let g = (y - 0.1873 * cb - 0.4681 * cr).max(0.0);
    let b = (y + 1.8556 * cb).max(0.0);
    let m = r.max(g).max(b).max(1e-6);
    [r, g, b].map(|c| 0.65 * (c / m) + 0.35)
}

/// Radius (in Cb / Cr units) from which the vectorscope tint is fully saturated; the 75 %
/// targets lie between 0.38 and 0.45.
const TINT_FULL_RADIUS: f32 = 0.25;

/// Colour of the vectorscope position (`cb`, `cr` in -0.5..=0.5) in the Colorize look: the
/// hue of the position at full brightness, as saturated as the position is far from the
/// centre (white at the centre, pure hue from [`TINT_FULL_RADIUS`] on). The count sets the
/// brightness, so the trace reads like the picture's colours.
pub fn vectorscope_tint(cb: f32, cr: f32) -> [f32; 3] {
    let y = 0.5;
    let r = (y + 1.5748 * cr).max(0.0);
    let g = (y - 0.1873 * cb - 0.4681 * cr).max(0.0);
    let b = (y + 1.8556 * cb).max(0.0);
    let m = r.max(g).max(b).max(1e-6);
    let radius = (cb * cb + cr * cr).sqrt();
    let s = 0.12 + 0.88 * (radius / TINT_FULL_RADIUS).clamp(0.0, 1.0).powf(0.8);
    [r, g, b].map(|c| 1.0 - s * (1.0 - c / m))
}

/// Vectorscope pixels: `counts[y * size + x]`, `y = 0` is the top (Cr = +0.5).
/// Each bin is slightly bloomed into its eight neighbours (3x3) so single-bin dots stay visible.
pub fn vectorscope_to_rgba(counts: &[u32], size: usize, gain: f32, colourise: bool) -> Vec<u8> {
    let n = size.saturating_mul(size);
    let reference = counts.iter().copied().take(n).max().unwrap_or(0);
    let mut b = vec![0.0f32; n];
    for (i, v) in b.iter_mut().enumerate() {
        *v = brightness(counts.get(i).copied().unwrap_or(0), reference, gain);
    }
    let mut out = Vec::with_capacity(n * 4);
    for y in 0..size {
        for x in 0..size {
            let at = |dx: isize, dy: isize| -> f32 {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx < 0 || ny < 0 || nx >= size as isize || ny >= size as isize {
                    0.0
                } else {
                    b[ny as usize * size + nx as usize]
                }
            };
            let neigh = at(-1, 0).max(at(1, 0)).max(at(0, -1)).max(at(0, 1));
            let diag = at(-1, -1).max(at(1, -1)).max(at(-1, 1)).max(at(1, 1));
            let v = b[y * size + x].max(0.75 * neigh).max(0.45 * diag);
            if v <= 0.0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let tint = if colourise {
                let cb = (x as f32 + 0.5) / size as f32 - 0.5;
                let cr = 0.5 - (y as f32 + 0.5) / size as f32;
                vectorscope_tint(cb, cr)
            } else {
                crate::style::LUMA_TINT
            };
            out.extend_from_slice(&pack([v * tint[0], v * tint[1], v * tint[2]]));
        }
    }
    out
}

/// Fill colours of the R, G, B histogram areas of the plain look (Colorize off).
pub const HISTOGRAM_FILL_PLAIN: [[u8; 3]; 3] = [[170, 28, 28], [28, 150, 40], [40, 70, 190]];
/// Fill colours of the Colorize look: the primaries are bright enough for any overlap of
/// two to read as the secondary colour and for all three to add up to white.
pub const HISTOGRAM_FILL_COLOUR: [[u8; 3]; 3] = [[205, 26, 26], [26, 205, 26], [26, 26, 205]];
/// Colour of the luminance outline of the histogram.
pub const HISTOGRAM_OUTLINE: [u8; 3] = [225, 225, 225];

/// The fill colours of the R, G, B areas for the Colorize switch.
pub fn histogram_fills(colourise: bool) -> [[u8; 3]; 3] {
    if colourise {
        HISTOGRAM_FILL_COLOUR
    } else {
        HISTOGRAM_FILL_PLAIN
    }
}

/// Software raster of the overlay histogram (the same composition the widget draws with
/// meshes): R, G, B areas added on black ([`histogram_fills`]), the luminance as a thin
/// outline on top. Premultiplied RGBA8 like the other renderers, alpha = largest channel.
/// Used for proof pictures and tests; `luma` etc. are the bin counts of the histogram.
#[allow(clippy::too_many_arguments)]
pub fn histogram_to_rgba(
    r: &[u32],
    g: &[u32],
    b: &[u32],
    luma: &[u32],
    width: usize,
    height: usize,
    colourise: bool,
    log: bool,
    gain: f32,
) -> Vec<u8> {
    let bins = r.len().min(g.len()).min(b.len()).min(luma.len());
    let mut out = vec![0u8; width.saturating_mul(height).saturating_mul(4)];
    if bins == 0 || width == 0 || height == 0 {
        return out;
    }
    let max = [r, g, b, luma].iter().flat_map(|v| v.iter().copied()).max().unwrap_or(0);
    let fills = histogram_fills(colourise);
    let hf = height as f32;
    let top_of = |v: u32| histogram_height(v, max, log, gain) * hf;
    let mut acc = vec![[0.0f32; 3]; width * height];
    let mut prev_luma = None;
    for x in 0..width {
        let bin = (x * bins / width).min(bins - 1);
        let heights = [top_of(r[bin]), top_of(g[bin]), top_of(b[bin])];
        let hl = top_of(luma[bin]);
        // Outline span of this column: from the previous column's level to this one's, at
        // least 1.2 px thick.
        let lo = prev_luma.map_or(hl, |p: f32| p.min(hl)) - 0.6;
        let hi = prev_luma.map_or(hl, |p: f32| p.max(hl)) + 0.6;
        prev_luma = Some(hl);
        for row in 0..height {
            let below = (height - 1 - row) as f32; // pixel covers [below, below + 1]
            let px = &mut acc[row * width + x];
            for (c, &h) in heights.iter().enumerate() {
                let cover = (h - below).clamp(0.0, 1.0);
                for (k, v) in px.iter_mut().enumerate() {
                    *v += cover * f32::from(fills[c][k]);
                }
            }
            let line = (hi.min(below + 1.0) - lo.max(below)).clamp(0.0, 1.0);
            if line > 0.0 {
                for (k, v) in px.iter_mut().enumerate() {
                    // The outline covers the fills underneath.
                    *v = *v * (1.0 - line) + line * f32::from(HISTOGRAM_OUTLINE[k]);
                }
            }
        }
    }
    for (o, px) in out.as_chunks_mut::<4>().0.iter_mut().zip(&acc) {
        let c = px.map(|v| (v + 0.5).clamp(0.0, 255.0) as u8);
        o.copy_from_slice(&[c[0], c[1], c[2], c[0].max(c[1]).max(c[2])]);
    }
    out
}

/// Flag of [`edge_flags`]: the column has counts in its top row (level 1.0).
pub const EDGE_HIGH: u8 = rawgraph_scopes::EDGE_HIGH;
/// Flag of [`edge_flags`]: the column has counts in its bottom row (level 0.0).
pub const EDGE_LOW: u8 = rawgraph_scopes::EDGE_LOW;

/// Per column of a waveform-like count grid (`counts[row * columns + column]`, row 0 is
/// the top): [`EDGE_HIGH`] / [`EDGE_LOW`] when the top / bottom row holds counts, i.e.
/// when the column contains values clipped at (or within half a level step of) 1.0 / 0.0.
pub fn edge_flags(counts: &[u32], columns: usize, levels: usize) -> Vec<u8> {
    let mut out = vec![0u8; columns];
    if columns == 0 || levels == 0 || counts.len() < columns * levels {
        return out;
    }
    let last = (levels - 1) * columns;
    for (c, f) in out.iter_mut().enumerate() {
        if counts[c] > 0 {
            *f |= EDGE_HIGH;
        }
        if counts[last + c] > 0 {
            *f |= EDGE_LOW;
        }
    }
    out
}

/// Column-wise OR of several flag vectors (the result has the length of the longest).
pub fn merge_edges(sets: &[&[u8]]) -> Vec<u8> {
    let n = sets.iter().map(|s| s.len()).max().unwrap_or(0);
    (0..n)
        .map(|i| sets.iter().fold(0, |a, s| a | s.get(i).copied().unwrap_or(0)))
        .collect()
}

/// Runs of consecutive columns that carry `flag`: `(first, one past the last)`.
pub fn flag_runs(flags: &[u8], flag: u8) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, f) in flags.iter().enumerate() {
        match (f & flag != 0, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                runs.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        runs.push((s, flags.len()));
    }
    runs
}

/// Height 0..=1 of a histogram bin value relative to `max`.
pub fn histogram_height(value: u32, max: u32, log: bool, gain: f32) -> f32 {
    if value == 0 || max == 0 {
        return 0.0;
    }
    let h = if log {
        (value as f32).ln_1p() / (max as f32).ln_1p()
    } else {
        value as f32 / max as f32
    };
    (h * gain.max(0.0)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_counts_are_transparent() {
        let counts = vec![0u32; 12];
        let px = layers_to_rgba(&[Layer { counts: &counts, tint: [1.0; 3] }], 4, 3, 1.0);
        assert_eq!(px.len(), 4 * 3 * 4);
        assert!(px.iter().all(|&b| b == 0));
    }

    #[test]
    fn higher_counts_never_darker() {
        let counts: Vec<u32> = (0..200).collect();
        let px = layers_to_rgba(&[Layer { counts: &counts, tint: [0.7, 1.0, 0.8] }], 200, 1, 1.0);
        for i in 1..200 {
            assert!(px[i * 4 + 3] >= px[(i - 1) * 4 + 3], "alpha not monotonic at {i}");
            assert!(px[i * 4 + 1] >= px[(i - 1) * 4 + 1]);
        }
        assert!(px[199 * 4 + 3] > px[4 + 3]);
    }

    #[test]
    fn gain_increases_brightness() {
        let counts = vec![1u32, 5, 100];
        let lo = layers_to_rgba(&[Layer { counts: &counts, tint: [1.0; 3] }], 3, 1, 0.25);
        let hi = layers_to_rgba(&[Layer { counts: &counts, tint: [1.0; 3] }], 3, 1, 1.0);
        for i in 0..2 {
            assert!(hi[i * 4 + 3] > lo[i * 4 + 3]);
        }
        assert!(hi[11] >= lo[11]);
    }

    #[test]
    fn output_size_matches() {
        let counts = vec![3u32; 512 * 256];
        let px = layers_to_rgba(&[Layer { counts: &counts, tint: [1.0; 3] }], 512, 256, 1.0);
        assert_eq!(px.len(), 512 * 256 * 4);
    }

    #[test]
    fn empty_and_short_data_do_not_panic() {
        assert!(layers_to_rgba(&[], 0, 0, 1.0).is_empty());
        assert!(layers_to_rgba(&[Layer { counts: &[], tint: [1.0; 3] }], 0, 5, 1.0).is_empty());
        // Short slice: missing counts are zero.
        let px = layers_to_rgba(&[Layer { counts: &[9], tint: [1.0; 3] }], 2, 2, 1.0);
        assert_eq!(px.len(), 16);
        assert!(px[3] > 0 && px[7] == 0);
        assert!(vectorscope_to_rgba(&[], 0, 1.0, true).is_empty());
        assert_eq!(vectorscope_to_rgba(&[], 4, 1.0, true).len(), 64);
        assert_eq!(brightness(5, 0, 1.0), 0.0);
        assert_eq!(histogram_height(0, 0, true, 1.0), 0.0);
    }

    #[test]
    fn rgb_layers_add_to_white() {
        let one = [1u32];
        let layers = [
            Layer { counts: &one, tint: [1.0, 0.0, 0.0] },
            Layer { counts: &one, tint: [0.0, 1.0, 0.0] },
            Layer { counts: &one, tint: [0.0, 0.0, 1.0] },
        ];
        assert_eq!(layers_to_rgba(&layers, 1, 1, 1.0), vec![255, 255, 255, 255]);
    }

    #[test]
    fn vectorscope_single_bin_blooms_but_stays_local() {
        let mut c = vec![0u32; 9 * 9];
        c[4 * 9 + 4] = 50;
        let px = vectorscope_to_rgba(&c, 9, 1.0, false);
        let a = |x: usize, y: usize| px[(y * 9 + x) * 4 + 3];
        assert_eq!(a(4, 4), 255);
        assert!(a(3, 4) > 0 && a(3, 4) < 255);
        assert!(a(3, 3) > 0 && a(3, 3) < a(3, 4));
        assert_eq!(a(2, 4), 0);
        assert_eq!(a(2, 2), 0);
    }

    #[test]
    fn glow_lifts_neighbours_only_vertically_and_one_column() {
        let mut c = vec![0u32; 9 * 9];
        c[4 * 9 + 4] = 50;
        let l = [Layer { counts: &c, tint: [1.0; 3] }];
        let plain = layers_to_rgba_glow(&l, 9, 9, 1.0, 50, false);
        assert_eq!(plain, layers_to_rgba(&l, 9, 9, 1.0));
        let g = layers_to_rgba_glow(&l, 9, 9, 1.0, 50, true);
        let a = |p: &[u8], x: usize, y: usize| p[(y * 9 + x) * 4 + 3];
        assert_eq!(a(&g, 4, 4), a(&plain, 4, 4));
        assert!(a(&g, 4, 3) > 0 && a(&g, 4, 2) > 0 && a(&g, 3, 4) > 0);
        assert_eq!(a(&g, 2, 4), 0, "no smear two columns away");
        assert_eq!(a(&g, 3, 3), 0);
    }

    #[test]
    fn hue_tint_is_in_range() {
        for &(cb, cr) in &[(0.0, 0.0), (0.5, 0.0), (-0.5, -0.5), (0.0, 0.5)] {
            for c in hue_tint(cb, cr) {
                assert!((0.0..=1.0).contains(&c));
            }
        }
    }

    #[test]
    fn edge_flags_follow_top_and_bottom_rows() {
        // 3 columns x 4 rows; column 0 has a top count, column 2 a bottom count, 1 both.
        let mut c = vec![0u32; 12];
        c[0] = 5; // row 0, col 0
        c[1] = 1; // row 0, col 1
        c[3 * 3 + 1] = 2; // row 3, col 1
        c[3 * 3 + 2] = 7; // row 3, col 2
        c[3 + 2] = 9; // row 1: ignored
        let f = edge_flags(&c, 3, 4);
        assert_eq!(f, [EDGE_HIGH, EDGE_HIGH | EDGE_LOW, EDGE_LOW]);
        assert_eq!(edge_flags(&[], 0, 0), Vec::<u8>::new());
        assert_eq!(edge_flags(&[1], 3, 3), [0, 0, 0], "short data is ignored");
        assert_eq!(merge_edges(&[&[1, 0], &[0, 2, 1]]), [1, 2, 1]);
        assert_eq!(flag_runs(&[1, 1, 0, 3, 3], EDGE_HIGH), [(0, 2), (3, 5)]);
        assert_eq!(flag_runs(&[1, 1, 0, 3, 3], EDGE_LOW), [(3, 5)]);
        assert!(flag_runs(&[], EDGE_LOW).is_empty());
    }

    #[test]
    fn histogram_height_log_lifts_small_values() {
        let lin = histogram_height(10, 10_000, false, 1.0);
        let log = histogram_height(10, 10_000, true, 1.0);
        assert!(log > lin);
        assert_eq!(histogram_height(10_000, 10_000, false, 1.0), 1.0);
    }
}
