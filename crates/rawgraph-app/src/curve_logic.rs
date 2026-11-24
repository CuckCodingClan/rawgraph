//! Pure logic of the point-curve editor: mapping between widget and curve
//! coordinates, hit testing, adding, moving and removing points. No egui types,
//! so everything is unit-testable.
//!
//! Curve points are `[input, output]` pairs in 0..1 on the perceptual axis; the
//! x values must stay strictly increasing and the curve keeps at least
//! [`MIN_POINTS`] and at most [`MAX_POINTS`] points (the limits of the core).

use rawgraph_core::graph::{MAX_CURVE_POINTS, MIN_CURVE_POINTS};

pub const MIN_POINTS: usize = MIN_CURVE_POINTS;
pub const MAX_POINTS: usize = MAX_CURVE_POINTS;
/// Smallest x distance between two neighbouring points.
pub const MIN_GAP: f32 = 0.002;

/// A rectangle in widget points (y grows downwards), the plotting area of the editor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlotRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PlotRect {
    /// Curve coordinates (x right, y up, 0..1) to widget points.
    pub fn to_widget(self, p: [f32; 2]) -> [f32; 2] {
        [self.x + p[0] * self.w, self.y + (1.0 - p[1]) * self.h]
    }

    /// Widget points to curve coordinates, clamped to 0..1.
    pub fn to_curve(self, p: [f32; 2]) -> [f32; 2] {
        if self.w <= 0.0 || self.h <= 0.0 {
            return [0.0, 0.0];
        }
        [((p[0] - self.x) / self.w).clamp(0.0, 1.0), (1.0 - (p[1] - self.y) / self.h).clamp(0.0, 1.0)]
    }
}

/// The identity curve of the Luminance, Red, Green and Blue channels (two end points).
#[cfg(test)]
pub fn identity() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [1.0, 1.0]]
}

#[cfg(test)]
pub fn is_identity(points: &[[f32; 2]]) -> bool {
    points == identity().as_slice()
}

/// Index of the point within `radius` widget points of `pos` (the nearest one).
pub fn hit_point(points: &[[f32; 2]], rect: PlotRect, pos: [f32; 2], radius: f32) -> Option<usize> {
    points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let w = rect.to_widget(*p);
            (i, ((w[0] - pos[0]).powi(2) + (w[1] - pos[1]).powi(2)).sqrt())
        })
        .filter(|(_, d)| *d <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(i, _)| i)
}

/// Allowed x range of point `idx`: strictly between its neighbours.
fn x_limits(points: &[[f32; 2]], idx: usize) -> (f32, f32) {
    let lo = if idx == 0 { 0.0 } else { points[idx - 1][0] + MIN_GAP };
    let hi = if idx + 1 >= points.len() { 1.0 } else { points[idx + 1][0] - MIN_GAP };
    (lo, hi)
}

/// Inserts a point at `p` (curve coordinates) keeping the x order. Returns its
/// index, or `None` when the curve is full or `p` is closer than [`MIN_GAP`] in x
/// to an existing point.
pub fn insert_point(points: &mut Vec<[f32; 2]>, p: [f32; 2]) -> Option<usize> {
    if points.len() >= MAX_POINTS {
        return None;
    }
    let p = [p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)];
    if points.iter().any(|q| (q[0] - p[0]).abs() < MIN_GAP) {
        return None;
    }
    let idx = points.iter().position(|q| q[0] > p[0]).unwrap_or(points.len());
    points.insert(idx, p);
    Some(idx)
}

/// Moves point `idx` to `target` (curve coordinates). The x value cannot pass
/// the neighbours, y stays within 0..1. Returns the position actually set.
pub fn move_point(points: &mut [[f32; 2]], idx: usize, target: [f32; 2]) -> Option<[f32; 2]> {
    if idx >= points.len() {
        return None;
    }
    let (lo, hi) = x_limits(points, idx);
    let x = if lo <= hi { target[0].clamp(lo, hi) } else { points[idx][0] };
    let y = target[1].clamp(0.0, 1.0);
    let x = if x.is_finite() { x } else { points[idx][0] };
    let y = if y.is_finite() { y } else { points[idx][1] };
    points[idx] = [x, y];
    Some([x, y])
}

/// Removes point `idx`; never leaves fewer than [`MIN_POINTS`]. Returns whether it was removed.
pub fn remove_point(points: &mut Vec<[f32; 2]>, idx: usize) -> bool {
    if points.len() <= MIN_POINTS || idx >= points.len() {
        return false;
    }
    points.remove(idx);
    true
}

/// Hue vs Hue: the hue wraps around, so the first and the last point keep the same height. After
/// point `moved` was set, the other end follows it (any other point leaves the ends alone).
pub fn wrap_ends(points: &mut [[f32; 2]], moved: usize) {
    let n = points.len();
    if n < 2 {
        return;
    }
    if moved == 0 {
        points[n - 1][1] = points[0][1];
    } else if moved == n - 1 {
        points[0][1] = points[n - 1][1];
    }
}

/// Makes the last point as high as the first (a curve of the hue channel from outside).
pub fn wrap_from_first(points: &mut [[f32; 2]]) {
    if let (Some(first), Some(last)) = (points.first().map(|p| p[1]), points.last_mut()) {
        last[1] = first;
    }
}

/// Samples the curve for drawing: `n` + 1 points `[x, y]` in curve coordinates.
pub fn sample(points: &[[f32; 2]], n: usize) -> Vec<[f32; 2]> {
    (0..=n)
        .map(|i| {
            let t = i as f32 / n.max(1) as f32;
            [t, rawgraph_fx::curve_eval(points, t).clamp(0.0, 1.0)]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: PlotRect = PlotRect { x: 10.0, y: 20.0, w: 200.0, h: 100.0 };

    #[test]
    fn widget_and_curve_coordinates_map_both_ways() {
        assert_eq!(R.to_widget([0.0, 0.0]), [10.0, 120.0], "origin is bottom left");
        assert_eq!(R.to_widget([1.0, 1.0]), [210.0, 20.0]);
        assert_eq!(R.to_curve([110.0, 70.0]), [0.5, 0.5]);
        for p in [[0.25, 0.8], [0.0, 1.0], [1.0, 0.0]] {
            let back = R.to_curve(R.to_widget(p));
            assert!((back[0] - p[0]).abs() < 1e-6 && (back[1] - p[1]).abs() < 1e-6);
        }
        assert_eq!(R.to_curve([-50.0, 500.0]), [0.0, 0.0], "outside is clamped");
        assert_eq!(PlotRect { w: 0.0, ..R }.to_curve([1.0, 1.0]), [0.0, 0.0], "degenerate rect never divides by zero");
    }

    #[test]
    fn hit_testing_picks_the_nearest_point_within_the_radius() {
        let pts = vec![[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]];
        let mid = R.to_widget([0.5, 0.5]);
        assert_eq!(hit_point(&pts, R, [mid[0] + 3.0, mid[1] - 2.0], 8.0), Some(1));
        assert_eq!(hit_point(&pts, R, [mid[0] + 30.0, mid[1]], 8.0), None);
        assert_eq!(hit_point(&pts, R, R.to_widget([0.0, 0.0]), 8.0), Some(0));
        // Two close points: the nearer one wins.
        let close = vec![[0.0, 0.0], [0.50, 0.5], [0.52, 0.5], [1.0, 1.0]];
        let near_second = R.to_widget([0.52, 0.5]);
        assert_eq!(hit_point(&close, R, [near_second[0] + 1.0, near_second[1]], 12.0), Some(2));
    }

    #[test]
    fn inserting_keeps_the_order_and_respects_the_limits() {
        let mut pts = identity();
        assert_eq!(insert_point(&mut pts, [0.7, 0.4]), Some(1));
        assert_eq!(insert_point(&mut pts, [0.3, 0.2]), Some(1));
        assert_eq!(pts, vec![[0.0, 0.0], [0.3, 0.2], [0.7, 0.4], [1.0, 1.0]]);
        assert_eq!(insert_point(&mut pts, [0.3005, 0.9]), None, "too close in x to an existing point");
        assert_eq!(insert_point(&mut pts, [2.0, -1.0]), None, "clamps to x = 1, which exists");
        let mut full: Vec<[f32; 2]> = (0..MAX_POINTS).map(|i| [i as f32 / (MAX_POINTS - 1) as f32, 0.5]).collect();
        assert_eq!(insert_point(&mut full, [0.0105, 0.5]), None, "the curve is full");
        assert!(pts.windows(2).all(|w| w[0][0] < w[1][0]));
    }

    #[test]
    fn a_point_cannot_pass_its_neighbours() {
        let mut pts = vec![[0.0, 0.0], [0.3, 0.3], [0.6, 0.6], [1.0, 1.0]];
        let set = move_point(&mut pts, 1, [0.9, 0.5]).unwrap();
        assert!(set[0] < 0.6 && set[0] > 0.59, "stops just before the neighbour: {set:?}");
        assert_eq!(set[1], 0.5);
        move_point(&mut pts, 2, [0.0, 0.2]);
        assert!(pts[2][0] > pts[1][0], "{pts:?}");
        assert!(pts.windows(2).all(|w| w[0][0] < w[1][0]), "order kept: {pts:?}");
        // The end points move in both axes but keep the order.
        move_point(&mut pts, 0, [0.2, 0.4]);
        assert!(pts[0][0] < pts[1][0] && pts[0][1] == 0.4, "{pts:?}");
        let last = pts.len() - 1;
        move_point(&mut pts, last, [0.1, 2.0]);
        assert!(pts[last][0] > pts[last - 1][0] && pts[last][1] == 1.0, "{pts:?}");
        assert_eq!(move_point(&mut pts, 99, [0.5, 0.5]), None);
        move_point(&mut pts, 1, [f32::NAN, f32::NAN]);
        assert!(pts.iter().all(|p| p[0].is_finite() && p[1].is_finite()));
    }

    #[test]
    fn there_are_never_fewer_than_two_points() {
        let mut pts = vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]];
        assert!(remove_point(&mut pts, 1));
        assert!(!remove_point(&mut pts, 0), "two points stay");
        assert!(!remove_point(&mut pts, 1));
        assert_eq!(pts.len(), 2);
        assert!(!remove_point(&mut pts, 7));
    }

    #[test]
    fn identity_is_recognised_and_resets() {
        let mut pts = vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]];
        assert!(!is_identity(&pts));
        pts = identity();
        assert!(is_identity(&pts));
    }

    #[test]
    fn the_hue_curve_keeps_its_ends_at_the_same_height() {
        let mut pts = vec![[0.0, 0.5], [0.4, 0.8], [1.0, 0.5]];
        move_point(&mut pts, 0, [0.0, 0.7]);
        wrap_ends(&mut pts, 0);
        assert_eq!((pts[0][1], pts[2][1]), (0.7, 0.7), "the last point follows the first");
        move_point(&mut pts, 2, [1.0, 0.2]);
        wrap_ends(&mut pts, 2);
        assert_eq!((pts[0][1], pts[2][1]), (0.2, 0.2), "and the first follows the last");
        move_point(&mut pts, 1, [0.4, 0.9]);
        wrap_ends(&mut pts, 1);
        assert_eq!((pts[0][1], pts[2][1], pts[1][1]), (0.2, 0.2, 0.9), "a middle point does not touch the ends");
        let mut odd = vec![[0.0, 0.3], [1.0, 0.9]];
        wrap_from_first(&mut odd);
        assert_eq!(odd[1][1], 0.3);
        wrap_ends(&mut [], 0);
    }

    #[test]
    fn sampling_follows_the_curve() {
        let s = sample(&[[0.0, 0.0], [0.5, 0.8], [1.0, 1.0]], 64);
        assert_eq!(s.len(), 65);
        assert!(s.iter().all(|p| (0.0..=1.0).contains(&p[1])));
        assert!((s[32][1] - 0.8).abs() < 1e-3, "passes through the middle point: {}", s[32][1]);
        let ident = sample(&identity(), 8);
        assert!(ident.iter().all(|p| (p[0] - p[1]).abs() < 1e-6));
    }

    #[test]
    fn what_the_logic_produces_is_accepted_by_the_core() {
        use rawgraph_core::{Graph, ImageId, NodeData, NodeKind};
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        let mut pts = identity();
        insert_point(&mut pts, [0.25, 0.1]);
        insert_point(&mut pts, [0.75, 0.9]);
        move_point(&mut pts, 0, [0.0, 0.05]);
        assert!(g.node_mut(id).unwrap().set_data(ImageId(1), Some(NodeData::Curve(pts))));
    }
}
