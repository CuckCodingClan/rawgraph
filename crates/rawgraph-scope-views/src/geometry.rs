//! Pure mapping helpers between scope values and screen positions.

use crate::style::ScaleKind;
use egui::{pos2, vec2, Pos2, Rect, Vec2};
use rawgraph_scopes::{vectorscope_bin, SKIN_TONE_LINE_DEG};

/// Highest 10-bit code value.
pub const CODE_MAX: f32 = 1023.0;

/// A labelled horizontal scale line. `level` is 0.0 (bottom) ..= 1.0 (top).
#[derive(Clone, Debug, PartialEq)]
pub struct Tick {
    pub level: f32,
    pub label: String,
}

/// Screen y for a level (1.0 = top edge of `rect`, 0.0 = bottom edge).
pub fn level_to_y(rect: Rect, level: f32) -> f32 {
    rect.bottom() - level * rect.height()
}

/// Level of a 10-bit code value.
pub fn code_to_level(code: u32) -> f32 {
    code as f32 / CODE_MAX
}

/// Horizontal graticule lines of the waveform / parade for a scale kind.
pub fn waveform_ticks(scale: ScaleKind) -> Vec<Tick> {
    match scale {
        ScaleKind::TenBit => [0u32, 128, 256, 384, 512, 640, 768, 896, 1023]
            .into_iter()
            .map(|c| Tick { level: code_to_level(c), label: c.to_string() })
            .collect(),
        ScaleKind::Percent => (0..=10)
            .map(|i| Tick { level: i as f32 / 10.0, label: (i * 10).to_string() })
            .collect(),
    }
}

/// Indices of `ys` (given in drawing order) whose labels fit without overlapping:
/// a label is kept when it is at least `min_gap` away from the last kept one.
pub fn thin_labels(ys: &[f32], min_gap: f32) -> Vec<usize> {
    let mut kept = Vec::new();
    let mut last: Option<f32> = None;
    for (i, &y) in ys.iter().enumerate() {
        if last.is_none_or(|l| (l - y).abs() >= min_gap) {
            kept.push(i);
            last = Some(y);
        }
    }
    // The last label (the 100 % / 1023 line) is always kept.
    if let Some(n) = ys.len().checked_sub(1) {
        if kept.last() != Some(&n) {
            if kept.len() > 1 {
                kept.pop();
            }
            kept.push(n);
        }
    }
    kept
}

/// Splits the plot rect of a parade into three panels separated by `gap` points.
pub fn parade_rects(plot: Rect, gap: f32) -> [Rect; 3] {
    let v = parade_rects_n(plot, gap, 3);
    [v[0], v[1], v[2]]
}

/// Splits the plot rect of a parade into `n` panels (3 or 4) separated by `gap` points.
/// Every panel lies inside `plot`; widths are equal and never negative.
pub fn parade_rects_n(plot: Rect, gap: f32, n: usize) -> Vec<Rect> {
    let n = n.max(1);
    // A gap larger than the room shrinks so that every panel stays inside `plot`.
    let gap = if n > 1 { gap.min(plot.width().max(0.0) / (n as f32 - 1.0)) } else { gap };
    let w = ((plot.width() - gap * (n as f32 - 1.0)) / n as f32).max(0.0);
    (0..n)
        .map(|i| {
            let x0 = plot.left() + i as f32 * (w + gap);
            Rect::from_min_size(pos2(x0, plot.top()), vec2(w, plot.height()))
        })
        .collect()
}

/// Splits `plot` into `n` rows (top to bottom) separated by `gap` points, for the
/// stacked histogram. Every row lies inside `plot`.
pub fn stacked_rows(plot: Rect, n: usize, gap: f32) -> Vec<Rect> {
    let n = n.max(1);
    let gap = if n > 1 { gap.min(plot.height().max(0.0) / (n as f32 - 1.0)) } else { gap };
    let h = ((plot.height() - gap * (n as f32 - 1.0)) / n as f32).max(0.0);
    (0..n)
        .map(|i| {
            let y0 = plot.top() + i as f32 * (h + gap);
            Rect::from_min_size(pos2(plot.left(), y0), vec2(plot.width(), h))
        })
        .collect()
}

/// Largest square centred in `outer`.
pub fn centred_square(outer: Rect) -> Rect {
    let s = outer.width().min(outer.height()).max(0.0);
    Rect::from_center_size(outer.center(), vec2(s, s))
}

/// Maps normalised scope coordinates (`u`, `v` in 0..=1, v = 0 at the top) to a
/// screen position inside `rect`. With `zoom` the centre half is magnified 2x.
pub fn scope_uv_to_pos(rect: Rect, u: f32, v: f32, zoom: bool) -> Pos2 {
    let (u, v) = if zoom { ((u - 0.5) * 2.0 + 0.5, (v - 0.5) * 2.0 + 0.5) } else { (u, v) };
    pos2(rect.left() + u * rect.width(), rect.top() + v * rect.height())
}

/// UV sub-rectangle of the vectorscope texture to display.
pub fn vectorscope_uv(zoom: bool) -> Rect {
    if zoom {
        Rect::from_min_max(pos2(0.25, 0.25), pos2(0.75, 0.75))
    } else {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
    }
}

/// Normalised (u, v) of the centre of the bin the data uses for a (Cb, Cr) pair,
/// obtained from the same `vectorscope_bin` as the trace so targets sit exactly on it.
pub fn vectorscope_bin_uv(cb: f32, cr: f32, size: usize) -> (f32, f32) {
    let size = size.max(1);
    let (bx, by) = vectorscope_bin(cb, cr, size);
    ((bx as f32 + 0.5) / size as f32, (by as f32 + 0.5) / size as f32)
}

/// Screen position of a vectorscope target (`cb`, `cr` already scaled).
pub fn vectorscope_target_pos(rect: Rect, cb: f32, cr: f32, size: usize, zoom: bool) -> Pos2 {
    let (u, v) = vectorscope_bin_uv(cb, cr, size);
    scope_uv_to_pos(rect, u, v, zoom)
}

/// Screen-space unit direction of the skin-tone line (counter-clockwise from +Cb, Cr up).
pub fn skin_line_dir() -> Vec2 {
    let a = SKIN_TONE_LINE_DEG.to_radians();
    vec2(a.cos(), -a.sin())
}

/// Screen x of histogram bin `i` of `bins` (first bin on the left edge, last on the right).
pub fn histogram_x(rect: Rect, i: usize, bins: usize) -> f32 {
    if bins <= 1 {
        rect.center().x
    } else {
        rect.left() + rect.width() * i as f32 / (bins - 1) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_scopes::vectorscope_targets;

    fn rect() -> Rect {
        Rect::from_min_size(pos2(10.0, 20.0), vec2(200.0, 100.0))
    }

    #[test]
    fn level_one_is_top_zero_is_bottom() {
        let r = rect();
        assert_eq!(level_to_y(r, 1.0), r.top());
        assert_eq!(level_to_y(r, 0.0), r.bottom());
    }

    #[test]
    fn code_512_sits_at_512_over_1023() {
        let r = rect();
        let t = waveform_ticks(ScaleKind::TenBit);
        let t512 = t.iter().find(|t| t.label == "512").expect("512 tick");
        let expected = r.bottom() - r.height() * 512.0 / 1023.0;
        assert!((level_to_y(r, t512.level) - expected).abs() < 1e-4);
        assert_eq!(t.first().unwrap().label, "0");
        assert_eq!(t.last().unwrap().label, "1023");
        assert_eq!(t.last().unwrap().level, 1.0);
        assert_eq!(t.len(), 9);
    }

    #[test]
    fn percent_ticks_span_zero_to_hundred() {
        let t = waveform_ticks(ScaleKind::Percent);
        assert_eq!(t.first().unwrap().label, "0");
        assert_eq!(t.last().unwrap().label, "100");
        assert_eq!(t.last().unwrap().level, 1.0);
    }

    #[test]
    fn targets_sit_on_their_trace_bins() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(512.0, 512.0));
        let size = 256;
        for (name, cb, cr) in vectorscope_targets() {
            let (cb, cr) = (cb * 0.75, cr * 0.75);
            let (bx, by) = vectorscope_bin(cb, cr, size);
            let expect = pos2((bx as f32 + 0.5) / 256.0 * 512.0, (by as f32 + 0.5) / 256.0 * 512.0);
            let got = vectorscope_target_pos(r, cb, cr, size, false);
            assert!((got - expect).length() < 1e-3, "{name}");
        }
    }

    #[test]
    fn zoom_keeps_centre_and_doubles_offsets() {
        let r = rect();
        let c = scope_uv_to_pos(r, 0.5, 0.5, true);
        assert!((c - r.center()).length() < 1e-4);
        let a = scope_uv_to_pos(r, 0.6, 0.5, false);
        let b = scope_uv_to_pos(r, 0.6, 0.5, true);
        assert!(((b.x - c.x) - 2.0 * (a.x - c.x)).abs() < 1e-3);
        assert_eq!(vectorscope_uv(true).center(), pos2(0.5, 0.5));
    }

    #[test]
    fn label_thinning_respects_gap() {
        let ys = [100.0, 95.0, 90.0, 80.0];
        assert_eq!(thin_labels(&ys, 12.0), vec![0, 3]);
        assert_eq!(thin_labels(&[100.0, 95.0, 90.0, 89.0], 12.0), vec![0, 3]);
        assert_eq!(thin_labels(&[], 12.0), Vec::<usize>::new());
    }

    #[test]
    fn parade_rects_fit_inside_plot() {
        let r = rect();
        let p = parade_rects(r, 2.0);
        assert!(p[0].left() >= r.left() && p[2].right() <= r.right() + 1e-3);
        assert!(p[0].right() < p[1].left() && p[1].right() < p[2].left());
        let degenerate = parade_rects(Rect::from_min_size(pos2(0.0, 0.0), vec2(1.0, 1.0)), 2.0);
        assert_eq!(degenerate[0].width(), 0.0);
    }

    #[test]
    fn three_and_four_column_parades_stay_inside_the_scope_rect() {
        for (w, h) in [(300.0, 120.0), (60.0, 40.0), (17.0, 9.0), (4.0, 4.0), (0.0, 0.0)] {
            let plot = Rect::from_min_size(pos2(7.0, 3.0), vec2(w, h));
            for n in [3usize, 4] {
                let rects = parade_rects_n(plot, 2.0, n);
                assert_eq!(rects.len(), n);
                for r in &rects {
                    assert!(plot.expand(1e-3).contains_rect(*r), "{n} columns in {w}x{h}: {r:?}");
                    assert!(r.width() >= 0.0);
                    assert!((r.width() - rects[0].width()).abs() < 1e-4);
                }
                for pair in rects.windows(2) {
                    assert!(pair[0].right() <= pair[1].left() + 1e-4, "overlap {n} in {w}");
                }
            }
        }
        assert_eq!(parade_rects_n(rect(), 2.0, 3), parade_rects(rect(), 2.0).to_vec());
        let four = parade_rects_n(rect(), 2.0, 4);
        assert!((four[3].right() - rect().right()).abs() < 1e-3, "last panel reaches the right edge");
    }

    #[test]
    fn stacked_histogram_rows_tile_the_plot() {
        let plot = Rect::from_min_size(pos2(5.0, 9.0), vec2(200.0, 100.0));
        let rows = stacked_rows(plot, 4, 2.0);
        assert_eq!(rows.len(), 4);
        for r in &rows {
            assert!(plot.expand(1e-3).contains_rect(*r));
            assert_eq!(r.width(), plot.width());
            assert!((r.height() - 23.5).abs() < 1e-4);
        }
        assert_eq!(rows[0].top(), plot.top());
        assert!((rows[3].bottom() - plot.bottom()).abs() < 1e-3);
        for pair in rows.windows(2) {
            assert!((pair[1].top() - pair[0].bottom() - 2.0).abs() < 1e-4);
        }
        let small = Rect::from_min_size(pos2(0.0, 0.0), vec2(10.0, 3.0));
        let plot_contains = |r: &Rect| small.expand(1e-3).contains_rect(*r);
        let squeezed = stacked_rows(small, 4, 2.0);
        assert!(squeezed.iter().all(|r| r.height() >= 0.0 && plot_contains(r)));
    }

    #[test]
    fn square_and_histogram_helpers() {
        let sq = centred_square(rect());
        assert_eq!(sq.width(), 100.0);
        assert_eq!(sq.center(), rect().center());
        let r = rect();
        assert_eq!(histogram_x(r, 0, 256), r.left());
        assert!((histogram_x(r, 255, 256) - r.right()).abs() < 1e-3);
        assert_eq!(histogram_x(r, 0, 0), r.center().x);
    }

    #[test]
    fn skin_line_points_up_left() {
        let d = skin_line_dir();
        assert!(d.x < 0.0 && d.y < 0.0);
        assert!((d.length() - 1.0).abs() < 1e-5);
    }
}
