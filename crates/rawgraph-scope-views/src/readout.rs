//! Pure mapping functions behind the hover readouts, vectorscope margin and tick density.

use crate::geometry::{
    level_to_y, scope_uv_to_pos, vectorscope_target_pos, waveform_ticks, Tick, CODE_MAX,
};
use crate::style::ScaleKind;
use egui::{Pos2, Rect};
use rawgraph_scopes::vectorscope_targets;

/// Waveform ticks thinned for the available plot height: the full set from 110 pt
/// up, every second one from 60 pt, otherwise every fourth. The top tick is always kept.
pub fn waveform_ticks_for_height(scale: ScaleKind, height: f32) -> Vec<Tick> {
    let all = waveform_ticks(scale);
    let stride = if height >= 110.0 {
        1
    } else if height >= 60.0 {
        2
    } else {
        4
    };
    let last = all.len() - 1;
    all.into_iter()
        .enumerate()
        .filter(|(i, _)| i % stride == 0 || *i == last)
        .map(|(_, t)| t)
        .collect()
}

/// Fraction of the vectorscope square kept free around the Cb/Cr area so that dots
/// and 100 % targets on the +-0.5 border stay fully visible.
pub const VS_MARGIN: f32 = 0.05;

/// The rectangle that the Cb/Cr range (-0.5..=0.5) maps onto inside the scope square.
pub fn vectorscope_inner(square: Rect) -> Rect {
    square.shrink(square.width() * VS_MARGIN)
}

/// Inverse of `level_to_y`, clamped to 0..=1.
pub fn level_at_y(rect: Rect, y: f32) -> f32 {
    if rect.height() <= 0.0 {
        return 0.0;
    }
    ((rect.bottom() - y) / rect.height()).clamp(0.0, 1.0)
}

/// Readout text of a level: 10-bit code value (0..=1023) or percent with one decimal.
pub fn format_level(level: f32, scale: ScaleKind) -> String {
    match scale {
        ScaleKind::TenBit => format!("{}", (level.clamp(0.0, 1.0) * CODE_MAX).round() as u32),
        ScaleKind::Percent => format!("{:.1}", level.clamp(0.0, 1.0) * 100.0),
    }
}

/// Inverse of `scope_uv_to_pos`.
pub fn scope_pos_to_uv(rect: Rect, pos: Pos2, zoom: bool) -> (f32, f32) {
    let u = (pos.x - rect.left()) / rect.width().max(1e-6);
    let v = (pos.y - rect.top()) / rect.height().max(1e-6);
    if zoom {
        ((u - 0.5) / 2.0 + 0.5, (v - 0.5) / 2.0 + 0.5)
    } else {
        (u, v)
    }
}

/// The six 100 % targets as `(angle, Cb, Cr)`, sorted by angle.
fn gamut_points() -> Vec<(f32, f32, f32)> {
    let mut pts: Vec<(f32, f32, f32)> = vectorscope_targets()
        .iter()
        .map(|&(_, cb, cr)| (cr.atan2(cb), cb, cr))
        .collect();
    pts.sort_by(|a, b| a.0.total_cmp(&b.0));
    pts
}

/// Distance from the centre to the 100 % colour gamut hexagon (through the six 100 %
/// targets) along the direction `angle` (radians, atan2(Cr, Cb)).
pub fn gamut_radius(angle: f32) -> f32 {
    gamut_radius_in(&gamut_points(), angle)
}

fn gamut_radius_in(pts: &[(f32, f32, f32)], angle: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let a = angle.rem_euclid(TAU);
    let a = if a > PI { a - TAU } else { a };
    let n = pts.len();
    let (dx, dy) = (angle.cos(), angle.sin());
    for i in 0..n {
        let p1 = pts[i];
        let p2 = pts[(i + 1) % n];
        let (lo, mut hi) = (p1.0, p2.0);
        if hi < lo {
            hi += TAU;
        }
        let aa = if a < lo { a + TAU } else { a };
        if aa >= lo && aa <= hi {
            let (ex, ey) = (p2.1 - p1.1, p2.2 - p1.2);
            let den = dx * ey - dy * ex;
            if den.abs() > 1e-9 {
                return (p1.1 * ey - p1.2 * ex) / den;
            }
        }
    }
    0.5
}

/// Hue angle in degrees (0..360, counter-clockwise from +Cb, Cr up) and saturation in
/// percent (distance from the centre relative to the 100 % gamut edge at that hue).
pub fn hue_saturation(cb: f32, cr: f32) -> (f32, f32) {
    let ang = cr.atan2(cb);
    let r = gamut_radius(ang).max(1e-6);
    let sat = (cb * cb + cr * cr).sqrt() / r * 100.0;
    (ang.to_degrees().rem_euclid(360.0), sat)
}

/// Readout for a pointer over the vectorscope: `(hue degrees, saturation percent)`;
/// `None` when the pointer is outside the Cb/Cr area `inner`.
pub fn vectorscope_readout(inner: Rect, pos: Pos2, zoom: bool) -> Option<(f32, f32)> {
    if !inner.contains(pos) {
        return None;
    }
    let (u, v) = scope_pos_to_uv(inner, pos, zoom);
    Some(hue_saturation(u - 0.5, 0.5 - v))
}

/// Mean saturation of the image in percent of the 100 % gamut edge, from vectorscope
/// counts (`counts[y * size + x]`, bin centres as `vectorscope_bin` places them).
/// `None` for empty data.
pub fn mean_saturation(counts: &[u32], size: usize) -> Option<f32> {
    if size == 0 || counts.len() < size * size {
        return None;
    }
    let pts = gamut_points();
    let s = (size.max(2) - 1) as f32;
    let (mut sum, mut total) = (0.0f64, 0u64);
    for (i, &n) in counts[..size * size].iter().enumerate() {
        if n == 0 {
            continue;
        }
        let cb = (i % size) as f32 / s - 0.5;
        let cr = 0.5 - (i / size) as f32 / s;
        let r = gamut_radius_in(&pts, cr.atan2(cb)).max(1e-6);
        let sat = ((cb * cb + cr * cr).sqrt() / r * 100.0).min(150.0);
        sum += f64::from(sat) * f64::from(n);
        total += u64::from(n);
    }
    (total > 0).then(|| (sum / total as f64) as f32)
}

/// What a span of the clipping / saturation readout row stands for (the view maps it
/// to a colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanTone {
    /// Words and separators.
    Label,
    /// A value of exactly zero.
    Zero,
    /// The "hi" marker word (colour of the top edge markers).
    High,
    /// The "lo" marker word (colour of the bottom edge markers).
    Low,
    /// Non-zero values of R, G, B and luma.
    Red,
    Green,
    Blue,
    Luma,
    /// The saturation value.
    Saturation,
}

/// A piece of text of the readout row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadoutSpan {
    pub text: String,
    pub tone: SpanTone,
}

fn span(text: impl Into<String>, tone: SpanTone) -> ReadoutSpan {
    ReadoutSpan { text: text.into(), tone }
}

/// One share as text: `1.2 %`; exactly `0.0 %` only when there is no such pixel, and
/// `<0.1 %` for a non-zero share that would round to zero. `compact` drops the unit.
pub fn format_clip_share(count: u64, samples: u64, compact: bool) -> String {
    let unit = if compact { "" } else { " %" };
    if count == 0 || samples == 0 {
        return format!("0.0{unit}");
    }
    let pct = count as f64 / samples as f64 * 100.0;
    if pct < 0.05 {
        format!("<0.1{unit}")
    } else {
        format!("{pct:.1}{unit}")
    }
}

/// The clipping part of the readout row, coloured per channel; empty when no value is
/// clipped. `compact` shortens it for narrow panels.
///
/// Full: `clip hi: R 1.2 % G 0.4 % B 0.0 % Y 0.0 % · lo: R 0.0 % ...`
/// Compact: `hi R1.2 G0.4 B0.0 Y0.0 · lo R0.0 ...`
pub fn clip_spans(stats: &rawgraph_scopes::ClipStats, compact: bool) -> Vec<ReadoutSpan> {
    if !stats.any_clipped() {
        return Vec::new();
    }
    const NAMES: [&str; 4] = ["R", "G", "B", "Y"];
    const TONES: [SpanTone; 4] = [SpanTone::Red, SpanTone::Green, SpanTone::Blue, SpanTone::Luma];
    let mut out = Vec::new();
    for (i, (word, counts, tone)) in
        [("hi", &stats.high, SpanTone::High), ("lo", &stats.low, SpanTone::Low)].into_iter().enumerate()
    {
        if i == 0 {
            out.push(span(if compact { "" } else { "clip " }, SpanTone::Label));
        } else {
            out.push(span(" \u{b7} ", SpanTone::Label));
        }
        out.push(span(if compact { word.to_owned() } else { format!("{word}:") }, tone));
        for c in 0..4 {
            out.push(span(format!(" {}{}", NAMES[c], if compact { "" } else { " " }), SpanTone::Label));
            let t = if counts[c] == 0 { SpanTone::Zero } else { TONES[c] };
            out.push(span(format_clip_share(counts[c], stats.samples, compact), t));
        }
    }
    out
}

/// The mean-saturation part of the readout row, e.g. ` · sat 56 %`.
pub fn saturation_spans(sat: f32, leading_separator: bool) -> Vec<ReadoutSpan> {
    let mut out = Vec::new();
    if leading_separator {
        out.push(span(" \u{b7} ", SpanTone::Label));
    }
    out.push(span("sat ", SpanTone::Label));
    out.push(span(format!("{:.0} %", sat.clamp(0.0, 999.0)), SpanTone::Saturation));
    out
}

/// Plain text of spans (tests, accessibility).
pub fn spans_text(spans: &[ReadoutSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// Histogram bin under screen x `x` (nearest, clamped).
pub fn histogram_bin_at(rect: Rect, x: f32, bins: usize) -> usize {
    if bins <= 1 || rect.width() <= 0.0 {
        return 0;
    }
    let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
    ((t * (bins - 1) as f32).round() as usize).min(bins - 1)
}

/// Level (0..=1) of a histogram bin.
pub fn histogram_bin_level(bin: usize, bins: usize) -> f32 {
    if bins <= 1 {
        0.0
    } else {
        bin as f32 / (bins - 1) as f32
    }
}

/// Screen y of a level (re-export for symmetry with [`level_at_y`]).
pub fn y_of_level(rect: Rect, level: f32) -> f32 {
    level_to_y(rect, level)
}

/// Screen position of a Cb/Cr pair inside `inner` for the given zoom (bin-exact).
pub fn chroma_pos(inner: Rect, cb: f32, cr: f32, size: usize, zoom: bool) -> Pos2 {
    vectorscope_target_pos(inner, cb, cr, size, zoom)
}

/// Screen position of normalised scope coordinates (thin wrapper kept next to the inverse).
pub fn uv_pos(inner: Rect, u: f32, v: f32, zoom: bool) -> Pos2 {
    scope_uv_to_pos(inner, u, v, zoom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, vec2};

    fn rect() -> Rect {
        Rect::from_min_size(pos2(10.0, 20.0), vec2(200.0, 100.0))
    }

    #[test]
    fn readout_waveform_levels() {
        let r = rect();
        assert_eq!(format_level(level_at_y(r, r.top()), ScaleKind::TenBit), "1023");
        assert_eq!(format_level(level_at_y(r, r.top()), ScaleKind::Percent), "100.0");
        assert_eq!(format_level(level_at_y(r, r.bottom()), ScaleKind::TenBit), "0");
        assert_eq!(format_level(level_at_y(r, r.top() - 50.0), ScaleKind::TenBit), "1023");
        assert_eq!(format_level(level_at_y(r, r.center().y), ScaleKind::Percent), "50.0");
    }

    #[test]
    fn readout_vectorscope_centre_and_red_target() {
        let sq = Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 400.0));
        let inner = vectorscope_inner(sq);
        let (_, sat) = vectorscope_readout(inner, inner.center(), false).unwrap();
        assert!(sat < 1.0, "centre saturation {sat}");
        let (_, cb, cr) = vectorscope_targets()[0];
        let (red_hue, _) = hue_saturation(cb, cr);
        for amp in [0.75f32, 1.0] {
            let pos = chroma_pos(inner, cb * amp, cr * amp, 256, false);
            let (hue, sat) = vectorscope_readout(inner, pos, false).unwrap();
            assert!((sat - amp * 100.0).abs() < 2.0, "sat {sat} for {amp}");
            assert!((hue - red_hue).abs() < 2.0, "hue {hue} vs {red_hue}");
        }
        assert!(vectorscope_readout(inner, pos2(1.0, 1.0), false).is_none());
        let p = inner.center() + vec2(100.0, 0.0);
        let a = vectorscope_readout(inner, p, false).unwrap().1;
        let b = vectorscope_readout(inner, p, true).unwrap().1;
        assert!((a / b - 2.0).abs() < 0.05);
    }

    #[test]
    fn gamut_radius_hits_all_targets() {
        for (_, cb, cr) in vectorscope_targets() {
            let want = (cb * cb + cr * cr).sqrt();
            assert!((gamut_radius(cr.atan2(cb)) - want).abs() < 1e-3);
        }
    }

    #[test]
    fn vectorscope_margin_keeps_border_bins_inside() {
        let sq = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 300.0));
        let inner = vectorscope_inner(sq);
        assert!(sq.contains_rect(inner) && inner.left() >= 14.9);
        let p = chroma_pos(inner, 0.0, 0.5, 256, false);
        assert!(p.y - sq.top() > 10.0);
        let _ = (uv_pos(inner, 0.5, 0.5, false), y_of_level(rect(), 1.0));
    }

    #[test]
    fn readout_histogram_bin() {
        let r = rect();
        assert_eq!(histogram_bin_at(r, r.left(), 256), 0);
        assert_eq!(histogram_bin_at(r, r.right(), 256), 255);
        assert_eq!(histogram_bin_at(r, r.center().x, 256), 128);
        assert_eq!(histogram_bin_at(r, 1e6, 256), 255);
        assert_eq!(histogram_bin_at(r, 5.0, 0), 0);
        assert!((histogram_bin_level(255, 256) - 1.0).abs() < 1e-6);
    }

    fn stats(high: [u64; 4], low: [u64; 4], samples: u64) -> rawgraph_scopes::ClipStats {
        rawgraph_scopes::ClipStats { samples, high, low, ..Default::default() }
    }

    #[test]
    fn clip_readout_text_formatting() {
        let s = stats([12, 4, 0, 0], [0, 0, 0, 0], 1000);
        assert_eq!(
            spans_text(&clip_spans(&s, false)),
            "clip hi: R 1.2 % G 0.4 % B 0.0 % Y 0.0 % \u{b7} lo: R 0.0 % G 0.0 % B 0.0 % Y 0.0 %"
        );
        assert_eq!(
            spans_text(&clip_spans(&s, true)),
            "hi R1.2 G0.4 B0.0 Y0.0 \u{b7} lo R0.0 G0.0 B0.0 Y0.0"
        );
        // Tiny non-zero shares are never shown as zero; full share is 100.0.
        assert_eq!(format_clip_share(1, 100_000, false), "<0.1 %");
        assert_eq!(format_clip_share(0, 100_000, false), "0.0 %");
        assert_eq!(format_clip_share(5, 1000, true), "0.5");
        assert_eq!(format_clip_share(1000, 1000, false), "100.0 %");
        assert_eq!(format_clip_share(3, 0, false), "0.0 %");
    }

    #[test]
    fn clip_readout_is_hidden_when_nothing_is_clipped_and_colours_values() {
        assert!(clip_spans(&stats([0; 4], [0; 4], 1000), false).is_empty());
        assert!(clip_spans(&rawgraph_scopes::ClipStats::default(), true).is_empty());
        let s = stats([12, 0, 0, 7], [0, 0, 3, 0], 1000);
        let spans = clip_spans(&s, false);
        let tone_of = |text: &str| spans.iter().find(|x| x.text == text).map(|x| x.tone);
        assert_eq!(tone_of("1.2 %"), Some(SpanTone::Red));
        assert_eq!(tone_of("0.7 %"), Some(SpanTone::Luma));
        assert_eq!(tone_of("0.3 %"), Some(SpanTone::Blue));
        // Zero values are dimmed, the hi / lo words carry the marker colours.
        assert!(spans.iter().any(|x| x.text == "0.0 %" && x.tone == SpanTone::Zero));
        assert_eq!(tone_of("hi:"), Some(SpanTone::High));
        assert_eq!(tone_of("lo:"), Some(SpanTone::Low));
        // A lone low-side clip still shows the row.
        assert!(!clip_spans(&stats([0; 4], [0, 0, 0, 1], 10), false).is_empty());
    }

    #[test]
    fn saturation_text_and_mean() {
        assert_eq!(spans_text(&saturation_spans(56.25, false)), "sat 56 %");
        assert_eq!(spans_text(&saturation_spans(3.4, true)), " \u{b7} sat 3 %");
        // One bin at the 75 % red target: 75 %. Centre bin: 0 %.
        let size = 256;
        let (_, cb, cr) = vectorscope_targets()[0];
        let (x, y) = rawgraph_scopes::vectorscope_bin(cb * 0.75, cr * 0.75, size);
        let mut c = vec![0u32; size * size];
        c[y * size + x] = 10;
        let sat = mean_saturation(&c, size).unwrap();
        assert!((sat - 75.0).abs() < 1.5, "{sat}");
        let mut c2 = vec![0u32; size * size];
        c2[(size / 2) * size + size / 2] = 3;
        c2[y * size + x] = 3;
        let both = mean_saturation(&c2, size).unwrap();
        assert!((both - 37.5).abs() < 2.0, "{both}");
        assert_eq!(mean_saturation(&vec![0; size * size], size), None);
        assert_eq!(mean_saturation(&[], 0), None);
        assert_eq!(mean_saturation(&[1, 2], 4), None);
    }

    #[test]
    fn tick_density_follows_height() {
        assert_eq!(waveform_ticks_for_height(ScaleKind::TenBit, 300.0).len(), 9);
        let mid = waveform_ticks_for_height(ScaleKind::TenBit, 80.0);
        let labels: Vec<&str> = mid.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["0", "256", "512", "768", "1023"]);
        let small = waveform_ticks_for_height(ScaleKind::Percent, 40.0);
        assert_eq!(small.first().unwrap().label, "0");
        assert_eq!(small.last().unwrap().label, "100");
        assert!(small.len() <= 4);
    }
}
