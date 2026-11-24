//! Pure layout chooser of the scope panel.

use egui::{pos2, vec2, Rect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Waveform,
    Parade,
    Vectorscope,
    Histogram,
}

impl ScopeKind {
    pub const ALL: [ScopeKind; 4] =
        [ScopeKind::Waveform, ScopeKind::Parade, ScopeKind::Vectorscope, ScopeKind::Histogram];

    pub fn title(self) -> &'static str {
        match self {
            ScopeKind::Waveform => "Waveform",
            ScopeKind::Parade => "RGB Parade",
            ScopeKind::Vectorscope => "Vectorscope",
            ScopeKind::Histogram => "Histogram",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            ScopeKind::Waveform => "Wave",
            ScopeKind::Parade => "Parade",
            ScopeKind::Vectorscope => "Vector",
            ScopeKind::Histogram => "Hist",
        }
    }

    /// Index into per-scope tables.
    pub fn index(self) -> usize {
        self as usize
    }

    /// The scope shown next to `self` when only two fit.
    pub fn companion(self) -> ScopeKind {
        match self {
            ScopeKind::Vectorscope => ScopeKind::Waveform,
            _ => ScopeKind::Vectorscope,
        }
    }
}

/// User-selectable arrangement. `Auto` picks one of the others from the area size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelLayout {
    Auto,
    /// All four scopes in a 2x2 grid.
    Grid2x2,
    /// Four scopes side by side.
    Row4,
    /// Four scopes stacked.
    Column4,
    /// One scope fills the panel.
    Single(ScopeKind),
}

/// Concrete arrangement after `Auto` has been resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Grid2x2,
    Row4,
    Column4,
    /// Two scopes side by side.
    Wide2,
    /// Two scopes stacked.
    Tall2,
    Single,
}

/// Height of the title strip above each scope.
pub const TITLE_H: f32 = 14.0;
/// Gap between cells.
pub const GAP: f32 = 6.0;

/// Chooses the arrangement for an area of `w` x `h` points.
pub fn auto_shape(w: f32, h: f32) -> Shape {
    if w >= 640.0 && h >= 440.0 {
        Shape::Grid2x2
    } else if w >= 800.0 && h >= 140.0 && w / h >= 2.5 {
        Shape::Row4
    } else if h >= 600.0 && w >= 200.0 && h / w >= 2.2 {
        Shape::Column4
    } else if w >= 400.0 && h >= 150.0 && w >= h {
        Shape::Wide2
    } else if h >= 330.0 && w >= 200.0 {
        Shape::Tall2
    } else if w >= 400.0 && h >= 150.0 {
        Shape::Wide2
    } else {
        Shape::Single
    }
}

/// Resolves a layout for an area to a shape.
pub fn resolve(layout: PanelLayout, w: f32, h: f32) -> Shape {
    match layout {
        PanelLayout::Auto => auto_shape(w, h),
        PanelLayout::Grid2x2 => Shape::Grid2x2,
        PanelLayout::Row4 => Shape::Row4,
        PanelLayout::Column4 => Shape::Column4,
        PanelLayout::Single(_) => Shape::Single,
    }
}

fn split(area: Rect, cols: usize, rows: usize) -> Vec<Rect> {
    let w = ((area.width() - GAP * (cols as f32 - 1.0)) / cols as f32).max(0.0);
    let h = ((area.height() - GAP * (rows as f32 - 1.0)) / rows as f32).max(0.0);
    let mut v = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            v.push(Rect::from_min_size(
                pos2(area.left() + c as f32 * (w + GAP), area.top() + r as f32 * (h + GAP)),
                vec2(w, h),
            ));
        }
    }
    v
}

/// Scope cells (title strip included) for `layout` in `area`. `focus` is the scope shown
/// when the arrangement has fewer than four cells and the layout does not name one.
pub fn arrange(layout: PanelLayout, area: Rect, focus: ScopeKind) -> Vec<(ScopeKind, Rect)> {
    let shape = resolve(layout, area.width(), area.height());
    let main = match layout {
        PanelLayout::Single(k) => k,
        _ => focus,
    };
    let four = ScopeKind::ALL;
    match shape {
        Shape::Grid2x2 => four.into_iter().zip(split(area, 2, 2)).collect(),
        Shape::Row4 => four.into_iter().zip(split(area, 4, 1)).collect(),
        Shape::Column4 => four.into_iter().zip(split(area, 1, 4)).collect(),
        Shape::Wide2 => [main, main.companion()].into_iter().zip(split(area, 2, 1)).collect(),
        Shape::Tall2 => [main, main.companion()].into_iter().zip(split(area, 1, 2)).collect(),
        Shape::Single => vec![(main, area)],
    }
}

/// The area a scope view gets inside its cell (below the title strip).
pub fn content_rect(cell: Rect) -> Rect {
    Rect::from_min_max(pos2(cell.left(), (cell.top() + TITLE_H).min(cell.bottom())), cell.max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::centred_square;

    fn area(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(5.0, 7.0), vec2(w, h))
    }

    #[test]
    fn auto_shape_table() {
        let t = [
            (260.0, 200.0, Shape::Single),
            (520.0, 380.0, Shape::Wide2),
            (520.0, 352.0, Shape::Wide2),
            (900.0, 300.0, Shape::Row4),
            (300.0, 900.0, Shape::Column4),
            (1200.0, 800.0, Shape::Grid2x2),
            (300.0, 400.0, Shape::Tall2),
            (260.0, 260.0, Shape::Single),
        ];
        for (w, h, want) in t {
            assert_eq!(auto_shape(w, h), want, "{w}x{h}");
        }
    }

    #[test]
    fn cells_stay_inside_area_and_vectorscope_square_fits() {
        let layouts = [
            PanelLayout::Auto,
            PanelLayout::Grid2x2,
            PanelLayout::Row4,
            PanelLayout::Column4,
            PanelLayout::Single(ScopeKind::Vectorscope),
        ];
        for (w, h) in [(260.0, 200.0), (520.0, 380.0), (900.0, 300.0), (300.0, 900.0), (1200.0, 800.0)] {
            for l in layouts {
                for focus in ScopeKind::ALL {
                    let a = area(w, h);
                    let cells = arrange(l, a, focus);
                    assert!(!cells.is_empty());
                    for (_, c) in &cells {
                        assert!(a.expand(1e-3).contains_rect(*c), "{l:?} {w}x{h}: {c:?}");
                        let content = content_rect(*c);
                        assert!(c.contains_rect(content));
                        let sq = centred_square(content);
                        assert!(c.contains_rect(sq) && sq.width() <= content.width() + 1e-3);
                        assert!((sq.width() - sq.height()).abs() < 1e-3);
                    }
                    for i in 0..cells.len() {
                        for j in i + 1..cells.len() {
                            assert!(!cells[i].1.shrink(0.01).intersects(cells[j].1), "overlap");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn four_up_layouts_show_every_scope_and_fallbacks_show_focus() {
        let a = area(1200.0, 800.0);
        assert_eq!(arrange(PanelLayout::Auto, a, ScopeKind::Waveform).len(), 4);
        let c = arrange(PanelLayout::Auto, area(260.0, 200.0), ScopeKind::Histogram);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, ScopeKind::Histogram);
        let c = arrange(PanelLayout::Auto, area(520.0, 380.0), ScopeKind::Parade);
        assert_eq!(c.iter().map(|x| x.0).collect::<Vec<_>>(), [ScopeKind::Parade, ScopeKind::Vectorscope]);
        let c = arrange(PanelLayout::Single(ScopeKind::Parade), a, ScopeKind::Waveform);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, ScopeKind::Parade);
    }
}
