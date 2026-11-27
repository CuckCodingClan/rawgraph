//! Pure logic of brush-mask painting: brush settings, the mapping from viewer
//! points to normalised image coordinates, the point-density rule of a stroke
//! and the edit operations on the stroke list. No egui types.
//!
//! Strokes live in normalised image coordinates (0..1 in x and y). A stroke's
//! `radius` is a fraction of the image's long edge, so distances between points
//! are measured in the same unit: both axes are scaled by the long edge.

use rawgraph_core::graph::{BRUSH_RADIUS_MAX, BRUSH_RADIUS_MIN, MAX_STROKES, MAX_STROKE_POINTS};
use rawgraph_core::BrushStroke;

/// Brush settings of the UI (not part of the graph; every new stroke copies them).
#[derive(Clone, Debug, PartialEq)]
pub struct BrushSettings {
    /// Brush diameter in percent of the image's long edge.
    pub size_pct: f32,
    pub hardness: f32,
    pub flow: f32,
    /// The "Erase" toggle of the inspector (Alt while painting flips it).
    pub erase: bool,
    /// "Clear all strokes" waits for a second click.
    pub confirm_clear: bool,
}

impl Default for BrushSettings {
    fn default() -> Self {
        Self { size_pct: 6.0, hardness: 0.5, flow: 1.0, erase: false, confirm_clear: false }
    }
}

pub const SIZE_PCT_MIN: f32 = BRUSH_RADIUS_MIN * 200.0;
pub const SIZE_PCT_MAX: f32 = BRUSH_RADIUS_MAX * 200.0;
/// Size factor of one `[` / `]` key press.
pub const SIZE_STEP: f32 = 1.2;
/// Hardness change of one Shift+`[` / `]` press.
pub const HARDNESS_STEP: f32 = 0.1;

impl BrushSettings {
    /// Brush radius as a fraction of the long edge (what a stroke stores).
    pub fn radius(&self) -> f32 {
        (self.size_pct / 200.0).clamp(BRUSH_RADIUS_MIN, BRUSH_RADIUS_MAX)
    }

    pub fn scale_size(&mut self, factor: f32) {
        if factor.is_finite() && factor > 0.0 {
            self.size_pct = (self.size_pct * factor).clamp(SIZE_PCT_MIN, SIZE_PCT_MAX);
        }
    }

    pub fn nudge_hardness(&mut self, delta: f32) {
        self.hardness = ((self.hardness + delta) * 100.0).round().clamp(0.0, 100.0) / 100.0;
    }

    /// A new stroke with these settings; `alt` flips the erase toggle.
    pub fn new_stroke(&self, alt: bool, first: [f32; 2]) -> BrushStroke {
        BrushStroke { points: vec![first], radius: self.radius(), hardness: self.hardness.clamp(0.0, 1.0), flow: self.flow.clamp(0.0, 1.0), erase: self.erase != alt }
    }
}

/// Where the image sits in the viewer: its rectangle in widget points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImageView {
    pub min: [f32; 2],
    pub size: [f32; 2],
}

impl ImageView {
    /// Widget point to normalised image coordinates (not clamped: strokes may
    /// start slightly outside the picture).
    pub fn to_norm(self, p: [f32; 2]) -> Option<[f32; 2]> {
        if self.size[0] <= 0.0 || self.size[1] <= 0.0 {
            return None;
        }
        Some([(p[0] - self.min[0]) / self.size[0], (p[1] - self.min[1]) / self.size[1]])
    }

    #[cfg(test)]
    pub fn to_widget(self, n: [f32; 2]) -> [f32; 2] {
        [self.min[0] + n[0] * self.size[0], self.min[1] + n[1] * self.size[1]]
    }

    /// Length of the long edge in widget points.
    pub fn long_edge(self) -> f32 {
        self.size[0].max(self.size[1])
    }

    /// On-screen radius of a brush with radius `radius` (fraction of the long edge).
    pub fn screen_radius(self, radius: f32) -> f32 {
        radius * self.long_edge()
    }
}

/// How far the pointer travels in normalised units before a stroke gets the
/// next point: a quarter of the radius keeps strokes small and smooth.
pub fn spacing(radius: f32) -> f32 {
    radius * 0.25
}

/// Distance between two normalised points in long-edge units, for an image of
/// aspect `w / h` (x is the long edge when w >= h).
pub fn distance(a: [f32; 2], b: [f32; 2], aspect: f32) -> f32 {
    let (sx, sy) = if aspect >= 1.0 { (1.0, 1.0 / aspect) } else { (aspect, 1.0) };
    (((a[0] - b[0]) * sx).powi(2) + ((a[1] - b[1]) * sy).powi(2)).sqrt()
}

/// Builds one stroke from pointer samples.
#[derive(Clone, Debug)]
pub struct StrokeBuilder {
    pub stroke: BrushStroke,
    aspect: f32,
}

impl StrokeBuilder {
    pub fn new(settings: &BrushSettings, alt: bool, first: [f32; 2], aspect: f32) -> Self {
        Self { stroke: settings.new_stroke(alt, clamp_norm(first)), aspect: if aspect.is_finite() && aspect > 0.0 { aspect } else { 1.0 } }
    }

    /// Offers a pointer sample; it becomes a point when it moved at least a
    /// quarter of the radius from the last point. Returns whether the stroke changed.
    pub fn add(&mut self, p: [f32; 2]) -> bool {
        let p = clamp_norm(p);
        let Some(last) = self.stroke.points.last() else {
            self.stroke.points.push(p);
            return true;
        };
        if self.stroke.points.len() >= MAX_STROKE_POINTS || distance(*last, p, self.aspect) < spacing(self.stroke.radius) {
            return false;
        }
        self.stroke.points.push(p);
        true
    }

    /// Closes the stroke at the final pointer position (so the end is not cut short).
    pub fn finish(&mut self, p: [f32; 2]) -> bool {
        let p = clamp_norm(p);
        let moved = self.stroke.points.last().is_none_or(|l| *l != p);
        if moved && self.stroke.points.len() < MAX_STROKE_POINTS {
            self.stroke.points.push(p);
            return true;
        }
        false
    }
}

/// Points may lie slightly outside the picture (a stroke that starts at the border).
fn clamp_norm(p: [f32; 2]) -> [f32; 2] {
    let c = |v: f32| if v.is_finite() { v.clamp(-0.25, 1.25) } else { 0.0 };
    [c(p[0]), c(p[1])]
}

/// The stroke list with `current` as its last entry (the live state while painting).
pub fn with_stroke(base: &[BrushStroke], current: &BrushStroke) -> Vec<BrushStroke> {
    let mut v = base.to_vec();
    v.push(current.clone());
    v
}

/// Whether another stroke fits in the list.
pub fn can_add_stroke(count: usize) -> bool {
    count < MAX_STROKES
}

/// Removes the last stroke; returns whether there was one.
pub fn undo_last(strokes: &mut Vec<BrushStroke>) -> bool {
    strokes.pop().is_some()
}

/// Text under the stroke count of the inspector.
pub fn count_text(n: usize) -> String {
    format!("{n} stroke{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(min: [f32; 2], size: [f32; 2]) -> ImageView {
        ImageView { min, size }
    }

    #[test]
    fn normalised_coordinates_are_right_at_any_zoom_and_pan() {
        // Fit: 1000 x 500 points at the origin.
        let v = view([0.0, 0.0], [1000.0, 500.0]);
        assert_eq!(v.to_norm([500.0, 250.0]), Some([0.5, 0.5]));
        // Zoomed in (x2) and panned: the same image point gives the same coordinates.
        let zoomed = view([-300.0, 40.0], [2000.0, 1000.0]);
        let image_point = [0.25, 0.8];
        for vw in [v, zoomed, view([123.0, -77.0], [333.0, 111.0])] {
            let w = vw.to_widget(image_point);
            let back = vw.to_norm(w).unwrap();
            assert!((back[0] - 0.25).abs() < 1e-5 && (back[1] - 0.8).abs() < 1e-5, "{vw:?} {back:?}");
        }
        assert_eq!(view([0.0, 0.0], [0.0, 10.0]).to_norm([1.0, 1.0]), None, "a degenerate image never divides by zero");
    }

    #[test]
    fn the_radius_is_a_fraction_of_the_long_edge() {
        let s = BrushSettings { size_pct: 10.0, ..Default::default() };
        assert!((s.radius() - 0.05).abs() < 1e-6, "diameter 10 % = radius 0.05");
        let v = view([0.0, 0.0], [1000.0, 500.0]);
        assert!((v.screen_radius(s.radius()) - 50.0).abs() < 1e-3);
        let portrait = view([0.0, 0.0], [500.0, 1000.0]);
        assert!((portrait.screen_radius(s.radius()) - 50.0).abs() < 1e-3, "the long edge counts, not the width");
    }

    #[test]
    fn size_keys_stay_in_range_and_hardness_steps() {
        let mut s = BrushSettings::default();
        for _ in 0..200 {
            s.scale_size(SIZE_STEP);
        }
        assert_eq!(s.size_pct, SIZE_PCT_MAX);
        for _ in 0..400 {
            s.scale_size(1.0 / SIZE_STEP);
        }
        assert_eq!(s.size_pct, SIZE_PCT_MIN);
        s.scale_size(f32::NAN);
        s.scale_size(-1.0);
        assert_eq!(s.size_pct, SIZE_PCT_MIN, "bad factors are ignored");
        s.hardness = 0.5;
        s.nudge_hardness(HARDNESS_STEP);
        assert!((s.hardness - 0.6).abs() < 1e-6);
        for _ in 0..20 {
            s.nudge_hardness(HARDNESS_STEP);
        }
        assert_eq!(s.hardness, 1.0);
        for _ in 0..30 {
            s.nudge_hardness(-HARDNESS_STEP);
        }
        assert_eq!(s.hardness, 0.0);
    }

    #[test]
    fn a_point_is_added_only_after_a_quarter_radius() {
        let s = BrushSettings { size_pct: 20.0, ..Default::default() }; // radius 0.1
        let mut b = StrokeBuilder::new(&s, false, [0.5, 0.5], 1.0);
        assert_eq!(b.stroke.points.len(), 1);
        assert!(!b.add([0.51, 0.5]), "0.01 < 0.025");
        assert!(!b.add([0.524, 0.5]));
        assert!(b.add([0.526, 0.5]), "0.026 >= 0.025");
        assert_eq!(b.stroke.points.len(), 2);
        // A dense pointer path of 1000 samples gives few points.
        let mut b = StrokeBuilder::new(&s, false, [0.0, 0.5], 1.0);
        for i in 1..=1000 {
            b.add([i as f32 / 1000.0, 0.5]);
        }
        let n = b.stroke.points.len();
        assert!((38..=42).contains(&n), "1.0 long / 0.025 = 40 points, got {n}");
        assert!(b.finish([1.0, 0.5]) || b.stroke.points.last() == Some(&[1.0, 0.5]));
    }

    #[test]
    fn distance_uses_the_long_edge_as_unit() {
        // Landscape 2:1: a step of 0.1 in y is 0.05 of the long edge.
        assert!((distance([0.0, 0.0], [0.0, 0.1], 2.0) - 0.05).abs() < 1e-6);
        assert!((distance([0.0, 0.0], [0.1, 0.0], 2.0) - 0.1).abs() < 1e-6);
        // Portrait 1:2: x is the short axis.
        assert!((distance([0.0, 0.0], [0.1, 0.0], 0.5) - 0.05).abs() < 1e-6);
    }

    #[test]
    fn erase_comes_from_the_toggle_xor_alt() {
        let mut s = BrushSettings::default();
        assert!(!StrokeBuilder::new(&s, false, [0.1, 0.1], 1.0).stroke.erase);
        assert!(StrokeBuilder::new(&s, true, [0.1, 0.1], 1.0).stroke.erase, "Alt erases");
        s.erase = true;
        assert!(StrokeBuilder::new(&s, false, [0.1, 0.1], 1.0).stroke.erase);
        assert!(!StrokeBuilder::new(&s, true, [0.1, 0.1], 1.0).stroke.erase, "Alt flips the toggle");
    }

    #[test]
    fn strokes_copy_the_settings_and_clamp_wild_points() {
        let s = BrushSettings { size_pct: 4.0, hardness: 0.25, flow: 0.7, ..Default::default() };
        let mut b = StrokeBuilder::new(&s, false, [5.0, -9.0], 1.5);
        assert_eq!(b.stroke.points[0], [1.25, -0.25]);
        b.add([f32::NAN, 0.5]);
        assert!(b.stroke.points.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
        assert_eq!((b.stroke.hardness, b.stroke.flow), (0.25, 0.7));
        assert!((b.stroke.radius - 0.02).abs() < 1e-6);
    }

    #[test]
    fn stroke_list_helpers() {
        let s = BrushSettings::default();
        let a = s.new_stroke(false, [0.1, 0.1]);
        let b = s.new_stroke(false, [0.9, 0.9]);
        let live = with_stroke(std::slice::from_ref(&a), &b);
        assert_eq!(live.len(), 2);
        let mut v = live;
        assert!(undo_last(&mut v));
        assert_eq!(v, vec![a]);
        assert!(undo_last(&mut v) && !undo_last(&mut v));
        assert_eq!(count_text(1), "1 stroke");
        assert_eq!(count_text(30), "30 strokes");
        assert!(can_add_stroke(MAX_STROKES - 1) && !can_add_stroke(MAX_STROKES));
    }

    #[test]
    fn what_the_logic_produces_is_accepted_by_the_core() {
        use rawgraph_core::{Graph, ImageId, NodeData, NodeKind};
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        let s = BrushSettings::default();
        let mut b = StrokeBuilder::new(&s, false, [0.2, 0.2], 1.5);
        b.add([0.4, 0.3]);
        assert!(g.node_mut(id).unwrap().set_data(ImageId(7), Some(NodeData::Strokes(vec![b.stroke]))));
    }
}
