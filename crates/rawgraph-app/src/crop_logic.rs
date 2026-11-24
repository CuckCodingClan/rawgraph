//! Pure geometry of the interactive crop: handles, hit testing, aspect-ratio
//! constrained resizing, moving, clamping to the picture, and the orientation
//! (flips, quarter turns, straightening) the picture is shown in. No egui types.
//!
//! The crop rectangle uses the same coordinates as the `left` / `top` / `right`
//! / `bottom` parameters of a Crop / Rotate node: fractions (0..1) of the
//! straightened picture (the largest rectangle without empty corners inside the
//! rotated image, see `rawgraph_fx::transform_size`).

use rawgraph_fx::{transform_size, TransformParams};

/// A crop rectangle in fractions of the straightened picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropRect {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl CropRect {
    pub const FULL: CropRect = CropRect { l: 0.0, t: 0.0, r: 1.0, b: 1.0 };

    pub fn w(self) -> f32 {
        self.r - self.l
    }

    pub fn h(self) -> f32 {
        self.b - self.t
    }

    pub fn is_full(self) -> bool {
        self == Self::FULL
    }
}

/// Part of the crop rectangle the pointer grabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
    /// The inside of the rectangle: moves it.
    Move,
}

impl Handle {
    pub const ALL: [Handle; 8] = [Handle::TopLeft, Handle::Top, Handle::TopRight, Handle::Right, Handle::BottomRight, Handle::Bottom, Handle::BottomLeft, Handle::Left];

    /// Position of the handle on a rectangle `[x0, y0, x1, y1]`.
    pub fn position(self, rect: [f32; 4]) -> [f32; 2] {
        let [x0, y0, x1, y1] = rect;
        let (xm, ym) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        match self {
            Handle::TopLeft => [x0, y0],
            Handle::Top => [xm, y0],
            Handle::TopRight => [x1, y0],
            Handle::Right => [x1, ym],
            Handle::BottomRight => [x1, y1],
            Handle::Bottom => [xm, y1],
            Handle::BottomLeft => [x0, y1],
            Handle::Left => [x0, ym],
            Handle::Move => [xm, ym],
        }
    }

    fn moves_left(self) -> bool {
        matches!(self, Handle::TopLeft | Handle::Left | Handle::BottomLeft)
    }

    fn moves_right(self) -> bool {
        matches!(self, Handle::TopRight | Handle::Right | Handle::BottomRight)
    }

    fn moves_top(self) -> bool {
        matches!(self, Handle::TopLeft | Handle::Top | Handle::TopRight)
    }

    fn moves_bottom(self) -> bool {
        matches!(self, Handle::BottomLeft | Handle::Bottom | Handle::BottomRight)
    }

    pub fn is_corner(self) -> bool {
        matches!(self, Handle::TopLeft | Handle::TopRight | Handle::BottomRight | Handle::BottomLeft)
    }
}

/// The handle under `pos` for a rectangle `[x0, y0, x1, y1]` in widget points:
/// corners first, then edges, then the inside. `radius` is the grab distance.
pub fn hit_handle(rect: [f32; 4], pos: [f32; 2], radius: f32) -> Option<Handle> {
    let dist = |h: Handle| {
        let p = h.position(rect);
        ((p[0] - pos[0]).powi(2) + (p[1] - pos[1]).powi(2)).sqrt()
    };
    // Corners, then edge midpoints; a small rectangle prefers the corners.
    let best = Handle::ALL
        .iter()
        .filter(|h| h.is_corner())
        .chain(Handle::ALL.iter().filter(|h| !h.is_corner()))
        .copied()
        .filter(|h| dist(*h) <= radius)
        .min_by(|a, b| dist(*a).total_cmp(&dist(*b)));
    if best.is_some() {
        return best;
    }
    let [x0, y0, x1, y1] = rect;
    // The whole edge line grabs as well (not only its midpoint).
    let in_x = pos[0] >= x0 && pos[0] <= x1;
    let in_y = pos[1] >= y0 && pos[1] <= y1;
    if in_x && (pos[1] - y0).abs() <= radius * 0.6 {
        return Some(Handle::Top);
    }
    if in_x && (pos[1] - y1).abs() <= radius * 0.6 {
        return Some(Handle::Bottom);
    }
    if in_y && (pos[0] - x0).abs() <= radius * 0.6 {
        return Some(Handle::Left);
    }
    if in_y && (pos[0] - x1).abs() <= radius * 0.6 {
        return Some(Handle::Right);
    }
    (in_x && in_y).then_some(Handle::Move)
}

/// Aspect ratios of the inspector combo.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Aspect {
    #[default]
    Free,
    /// The aspect ratio of the picture itself.
    Original,
    Square,
    R3x2,
    R4x3,
    R16x9,
    R4x5,
}

impl Aspect {
    pub const ALL: [Aspect; 7] = [Aspect::Free, Aspect::Original, Aspect::Square, Aspect::R3x2, Aspect::R4x3, Aspect::R16x9, Aspect::R4x5];

    pub fn label(self) -> &'static str {
        match self {
            Aspect::Free => "Free",
            Aspect::Original => "Original",
            Aspect::Square => "1:1",
            Aspect::R3x2 => "3:2",
            Aspect::R4x3 => "4:3",
            Aspect::R16x9 => "16:9",
            Aspect::R4x5 => "4:5",
        }
    }

    /// Width / height of the crop in pixels; `None` for Free. `picture` is the
    /// pixel size of the straightened picture, `swap` turns a landscape ratio into
    /// a portrait one and back (Original and 1:1 are not affected).
    pub fn ratio(self, picture: [f32; 2], swap: bool) -> Option<f32> {
        let base = match self {
            Aspect::Free => return None,
            Aspect::Original => return (picture[0] > 0.0 && picture[1] > 0.0).then(|| picture[0] / picture[1]),
            Aspect::Square => return Some(1.0),
            Aspect::R3x2 => 3.0 / 2.0,
            Aspect::R4x3 => 4.0 / 3.0,
            Aspect::R16x9 => 16.0 / 9.0,
            // 4:5 is named as a portrait ratio: width 4, height 5.
            Aspect::R4x5 => 4.0 / 5.0,
        };
        Some(if swap { 1.0 / base } else { base })
    }
}

/// Smallest crop side in pixels.
pub const MIN_SIDE_PX: f32 = 16.0;

fn min_frac(picture_side: f32) -> f32 {
    if picture_side <= 0.0 {
        0.5
    } else {
        (MIN_SIDE_PX / picture_side).min(0.5)
    }
}

/// Clamps a rectangle into the picture, keeping its size where possible, and
/// enforces the minimum size.
pub fn clamp_rect(r: CropRect, picture: [f32; 2]) -> CropRect {
    let (mw, mh) = (min_frac(picture[0]), min_frac(picture[1]));
    let fix = |lo: f32, hi: f32, min: f32| {
        let (lo, hi) = (if lo.is_finite() { lo } else { 0.0 }, if hi.is_finite() { hi } else { 1.0 });
        let size = (hi - lo).clamp(min, 1.0);
        if lo >= 0.0 && hi <= 1.0 && hi - lo >= min {
            // Already valid: no rounding drift.
            return (lo, hi);
        }
        let lo = lo.clamp(0.0, 1.0 - size);
        (lo, lo + size)
    };
    let (l, r2) = fix(r.l, r.r, mw);
    let (t, b) = fix(r.t, r.b, mh);
    CropRect { l, t, r: r2, b }
}

/// Moves the whole rectangle by `delta` (fractions), keeping its size inside the picture.
pub fn move_rect(start: CropRect, delta: [f32; 2]) -> CropRect {
    let (w, h) = (start.w(), start.h());
    let l = (start.l + delta[0]).clamp(0.0, (1.0 - w).max(0.0));
    let t = (start.t + delta[1]).clamp(0.0, (1.0 - h).max(0.0));
    CropRect { l, t, r: l + w, b: t + h }
}

/// Drags `handle` of `start` to the pointer position `p` (fractions of the
/// picture). `ratio` is the fixed width / height in pixels, `picture` the pixel
/// size of the straightened picture. `anchor_delta` is used by `Handle::Move`.
pub fn resize_rect(start: CropRect, handle: Handle, p: [f32; 2], grab_offset: [f32; 2], ratio: Option<f32>, picture: [f32; 2]) -> CropRect {
    if handle == Handle::Move {
        return move_rect(start, [p[0] - grab_offset[0], p[1] - grab_offset[1]]);
    }
    let p = [(p[0] - grab_offset[0]).clamp(0.0, 1.0), (p[1] - grab_offset[1]).clamp(0.0, 1.0)];
    let (mw, mh) = (min_frac(picture[0]), min_frac(picture[1]));
    let mut r = start;
    if handle.moves_left() {
        r.l = p[0].min(r.r - mw);
    }
    if handle.moves_right() {
        r.r = p[0].max(r.l + mw);
    }
    if handle.moves_top() {
        r.t = p[1].min(r.b - mh);
    }
    if handle.moves_bottom() {
        r.b = p[1].max(r.t + mh);
    }
    let Some(ratio) = ratio.filter(|q| q.is_finite() && *q > 0.0) else {
        return clamp_rect(r, picture);
    };
    if picture[0] <= 0.0 || picture[1] <= 0.0 {
        return clamp_rect(r, picture);
    }
    // Fixed ratio: work in pixels. `fx` converts a width fraction to a height fraction.
    let to_h = |w_frac: f32| w_frac * picture[0] / ratio / picture[1];
    let to_w = |h_frac: f32| h_frac * picture[1] * ratio / picture[0];
    if handle.is_corner() {
        // The corner opposite to the handle stays put.
        let ax = if handle.moves_left() { start.r } else { start.l };
        let ay = if handle.moves_top() { start.b } else { start.t };
        let dir_x = if handle.moves_left() { -1.0 } else { 1.0 };
        let dir_y = if handle.moves_top() { -1.0 } else { 1.0 };
        let room_w = if dir_x > 0.0 { 1.0 - ax } else { ax };
        let room_h = if dir_y > 0.0 { 1.0 - ay } else { ay };
        let want_w = ((p[0] - ax) * dir_x).max(0.0);
        let want_h = ((p[1] - ay) * dir_y).max(0.0);
        // The larger of the two requests wins, then it must fit into the free room.
        let mut w = want_w.max(to_w(want_h)).max(mw.max(to_w(mh)));
        w = w.min(room_w).min(to_w(room_h));
        let h = to_h(w);
        let (nx, ny) = (ax + dir_x * w, ay + dir_y * h);
        CropRect { l: ax.min(nx), t: ay.min(ny), r: ax.max(nx), b: ay.max(ny) }
    } else if handle.moves_left() || handle.moves_right() {
        // A side handle: the height follows, centred on the old vertical middle.
        let (ax, dir_x) = if handle.moves_left() { (start.r, -1.0) } else { (start.l, 1.0) };
        let room_w = if dir_x > 0.0 { 1.0 - ax } else { ax };
        let cy = (start.t + start.b) * 0.5;
        let max_h = 2.0 * cy.min(1.0 - cy);
        let want_w = ((p[0] - ax) * dir_x).max(mw);
        let w = want_w.min(room_w).min(to_w(max_h));
        let h = to_h(w);
        let nx = ax + dir_x * w;
        CropRect { l: ax.min(nx), r: ax.max(nx), t: cy - h * 0.5, b: cy + h * 0.5 }
    } else {
        let (ay, dir_y) = if handle.moves_top() { (start.b, -1.0) } else { (start.t, 1.0) };
        let room_h = if dir_y > 0.0 { 1.0 - ay } else { ay };
        let cx = (start.l + start.r) * 0.5;
        let max_w = 2.0 * cx.min(1.0 - cx);
        let want_h = ((p[1] - ay) * dir_y).max(mh);
        let h = want_h.min(room_h).min(to_h(max_w));
        let w = to_w(h);
        let ny = ay + dir_y * h;
        CropRect { t: ay.min(ny), b: ay.max(ny), l: cx - w * 0.5, r: cx + w * 0.5 }
    }
}

/// Fits `rect` to a fixed `ratio`: keeps the centre and the larger part of the
/// area that still fits, then clamps into the picture. Used when the aspect combo changes.
pub fn fit_to_ratio(rect: CropRect, ratio: f32, picture: [f32; 2]) -> CropRect {
    if !(ratio.is_finite() && ratio > 0.0) || picture[0] <= 0.0 || picture[1] <= 0.0 {
        return rect;
    }
    let (wpx, hpx) = (rect.w() * picture[0], rect.h() * picture[1]);
    // Largest rectangle of this ratio inside the current one.
    let (nw, nh) = if wpx / hpx.max(1e-6) > ratio { (hpx * ratio, hpx) } else { (wpx, wpx / ratio) };
    let (fw, fh) = (nw / picture[0], nh / picture[1]);
    let (cx, cy) = ((rect.l + rect.r) * 0.5, (rect.t + rect.b) * 0.5);
    clamp_rect(CropRect { l: cx - fw * 0.5, r: cx + fw * 0.5, t: cy - fh * 0.5, b: cy + fh * 0.5 }, picture)
}

/// Pixel size of the straightened picture (before the crop) for a `w` x `h`
/// source: what the crop fractions refer to.
pub fn picture_size(w: u32, h: u32, angle_deg: f32, quarter_turns: u8) -> [f32; 2] {
    let p = TransformParams { angle_deg, quarter_turns, ..TransformParams::default() };
    let (pw, ph) = transform_size(w, h, &p);
    [pw as f32, ph as f32]
}

/// Size of the picture after the crop, as the engine computes it.
pub fn result_size(w: u32, h: u32, p: &TransformParams) -> (u32, u32) {
    transform_size(w, h, p)
}

/// Texture coordinates `[u, v]` of the four corners (top-left, top-right,
/// bottom-right, bottom-left) of the picture after the flips and the clockwise
/// quarter turns, which is the order the engine applies them in.
pub fn oriented_uv(quarter_turns: u8, flip_h: bool, flip_v: bool) -> [[f32; 2]; 4] {
    let corners = [[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    corners.map(|[a, b]| {
        // Coordinates in the flipped picture ...
        let (p, q) = match quarter_turns % 4 {
            1 => (b, 1.0 - a),
            2 => (1.0 - a, 1.0 - b),
            3 => (1.0 - b, a),
            _ => (a, b),
        };
        // ... and in the source texture.
        [if flip_h { 1.0 - p } else { p }, if flip_v { 1.0 - q } else { q }]
    })
}

/// Corners of a `w` x `h` rectangle centred at `centre`, rotated clockwise by
/// `angle_deg` (screen coordinates, y down): top-left, top-right, bottom-right, bottom-left.
pub fn rotated_corners(centre: [f32; 2], w: f32, h: f32, angle_deg: f32) -> [[f32; 2]; 4] {
    let (s, c) = angle_deg.to_radians().sin_cos();
    [[-w * 0.5, -h * 0.5], [w * 0.5, -h * 0.5], [w * 0.5, h * 0.5], [-w * 0.5, h * 0.5]].map(|[x, y]| [centre[0] + x * c - y * s, centre[1] + x * s + y * c])
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIC: [f32; 2] = [3000.0, 2000.0];

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn px_ratio(r: CropRect, pic: [f32; 2]) -> f32 {
        r.w() * pic[0] / (r.h() * pic[1])
    }

    #[test]
    fn handles_are_hit_in_the_right_order() {
        let rect = [100.0, 100.0, 300.0, 200.0];
        assert_eq!(hit_handle(rect, [101.0, 99.0], 10.0), Some(Handle::TopLeft));
        assert_eq!(hit_handle(rect, [299.0, 201.0], 10.0), Some(Handle::BottomRight));
        assert_eq!(hit_handle(rect, [200.0, 98.0], 10.0), Some(Handle::Top));
        assert_eq!(hit_handle(rect, [302.0, 150.0], 10.0), Some(Handle::Right));
        assert_eq!(hit_handle(rect, [100.0, 150.0], 10.0), Some(Handle::Left));
        assert_eq!(hit_handle(rect, [200.0, 202.0], 10.0), Some(Handle::Bottom));
        assert_eq!(hit_handle(rect, [150.0, 100.0], 10.0), Some(Handle::Top), "the whole edge grabs");
        assert_eq!(hit_handle(rect, [150.0, 140.0], 10.0), Some(Handle::Move));
        assert_eq!(hit_handle(rect, [20.0, 20.0], 10.0), None);
        // A tiny rectangle still offers its corners.
        let tiny = [100.0, 100.0, 108.0, 106.0];
        assert!(hit_handle(tiny, [100.0, 100.0], 10.0).is_some_and(|h| h.is_corner()));
    }

    #[test]
    fn each_free_handle_moves_only_its_edges() {
        let start = CropRect { l: 0.2, t: 0.2, r: 0.8, b: 0.8 };
        let g = [0.0, 0.0];
        let c = |h, p| resize_rect(start, h, p, g, None, PIC);
        assert_eq!(c(Handle::Left, [0.1, 0.5]), CropRect { l: 0.1, ..start });
        assert_eq!(c(Handle::Right, [0.9, 0.1]), CropRect { r: 0.9, ..start });
        assert_eq!(c(Handle::Top, [0.5, 0.05]), CropRect { t: 0.05, ..start });
        assert_eq!(c(Handle::Bottom, [0.5, 0.95]), CropRect { b: 0.95, ..start });
        assert_eq!(c(Handle::TopLeft, [0.1, 0.15]), CropRect { l: 0.1, t: 0.15, ..start });
        assert_eq!(c(Handle::TopRight, [0.95, 0.1]), CropRect { r: 0.95, t: 0.1, ..start });
        assert_eq!(c(Handle::BottomRight, [0.9, 0.9]), CropRect { r: 0.9, b: 0.9, ..start });
        assert_eq!(c(Handle::BottomLeft, [0.05, 0.99]), CropRect { l: 0.05, b: 0.99, ..start });
    }

    #[test]
    fn dragging_beyond_the_picture_or_the_opposite_edge_is_clamped() {
        let start = CropRect { l: 0.2, t: 0.2, r: 0.8, b: 0.8 };
        let g = [0.0, 0.0];
        let r = resize_rect(start, Handle::Right, [1.7, 0.5], g, None, PIC);
        assert_eq!(r.r, 1.0);
        let r = resize_rect(start, Handle::Left, [-3.0, 0.5], g, None, PIC);
        assert_eq!(r.l, 0.0);
        // Past the opposite edge: the minimum size stays.
        let r = resize_rect(start, Handle::Right, [0.0, 0.5], g, None, PIC);
        assert!(r.r > r.l && r.w() * PIC[0] >= MIN_SIDE_PX - 0.01, "{r:?}");
        let r = resize_rect(start, Handle::Bottom, [0.5, -1.0], g, None, PIC);
        assert!(r.b > r.t && r.h() * PIC[1] >= MIN_SIDE_PX - 0.01);
        assert!(resize_rect(start, Handle::TopLeft, [f32::NAN, 0.1], g, None, PIC).l.is_finite());
    }

    #[test]
    fn moving_keeps_the_size_and_stays_inside() {
        let start = CropRect { l: 0.2, t: 0.3, r: 0.6, b: 0.7 };
        let r = move_rect(start, [0.1, -0.1]);
        assert!(approx(r.l, 0.3) && approx(r.r, 0.7) && approx(r.t, 0.2) && approx(r.b, 0.6));
        let r = move_rect(start, [5.0, 5.0]);
        assert!(approx(r.r, 1.0) && approx(r.b, 1.0) && approx(r.w(), 0.4) && approx(r.h(), 0.4), "{r:?}");
        let r = move_rect(start, [-5.0, -5.0]);
        assert!(approx(r.l, 0.0) && approx(r.t, 0.0) && approx(r.w(), 0.4));
        // The grab offset keeps the rectangle from jumping to the pointer.
        let r = resize_rect(start, Handle::Move, [0.5, 0.5], [0.4, 0.5], None, PIC);
        assert!(approx(r.l, 0.3) && approx(r.t, 0.3), "{r:?}");
    }

    #[test]
    fn a_locked_corner_keeps_the_ratio_and_the_opposite_corner() {
        let start = CropRect { l: 0.1, t: 0.1, r: 0.5, b: 0.4 };
        for ratio in [1.0f32, 3.0 / 2.0, 4.0 / 5.0, 16.0 / 9.0] {
            for (h, p) in [(Handle::BottomRight, [0.7, 0.9]), (Handle::BottomRight, [0.3, 0.2]), (Handle::TopLeft, [0.0, 0.0])] {
                let r = resize_rect(start, h, p, [0.0, 0.0], Some(ratio), PIC);
                assert!((px_ratio(r, PIC) - ratio).abs() < 1e-3, "{h:?} {p:?} ratio {ratio}: {}", px_ratio(r, PIC));
                assert!(r.l >= -1e-6 && r.t >= -1e-6 && r.r <= 1.0 + 1e-6 && r.b <= 1.0 + 1e-6, "inside: {r:?}");
                match h {
                    Handle::BottomRight => assert!(approx(r.l, start.l) && approx(r.t, start.t), "anchor: {r:?}"),
                    _ => assert!(approx(r.r, start.r) && approx(r.b, start.b), "anchor: {r:?}"),
                }
            }
        }
    }

    #[test]
    fn a_locked_side_handle_changes_the_other_axis_around_the_centre() {
        let start = CropRect { l: 0.2, t: 0.3, r: 0.6, b: 0.7 };
        let r = resize_rect(start, Handle::Right, [0.8, 0.5], [0.0, 0.0], Some(1.0), PIC);
        assert!((px_ratio(r, PIC) - 1.0).abs() < 1e-3, "{r:?}");
        assert!(approx(r.l, 0.2) && approx(r.r, 0.8));
        assert!(approx((r.t + r.b) * 0.5, 0.5), "centred vertically: {r:?}");
        let r = resize_rect(start, Handle::Bottom, [0.4, 0.95], [0.0, 0.0], Some(3.0 / 2.0), PIC);
        assert!((px_ratio(r, PIC) - 1.5).abs() < 1e-3 && r.b <= 1.0 + 1e-6 && r.l >= -1e-6 && r.r <= 1.0 + 1e-6, "{r:?}");
        // Dragging a side far out cannot leave the picture on the other axis.
        let r = resize_rect(start, Handle::Right, [1.0, 0.5], [0.0, 0.0], Some(0.5), PIC);
        assert!(r.t >= -1e-6 && r.b <= 1.0 + 1e-6 && (px_ratio(r, PIC) - 0.5).abs() < 1e-3, "{r:?}");
    }

    #[test]
    fn the_minimum_size_holds_with_a_locked_ratio() {
        let start = CropRect { l: 0.4, t: 0.4, r: 0.6, b: 0.6 };
        let r = resize_rect(start, Handle::BottomRight, [0.0, 0.0], [0.0, 0.0], Some(1.0), PIC);
        assert!(r.w() * PIC[0] >= MIN_SIDE_PX - 0.01 && r.h() * PIC[1] >= MIN_SIDE_PX - 0.01, "{r:?}");
    }

    #[test]
    fn ratios_follow_the_combo_and_the_portrait_swap() {
        let p = [3000.0, 2000.0];
        assert_eq!(Aspect::Free.ratio(p, false), None);
        assert_eq!(Aspect::Original.ratio(p, false), Some(1.5));
        assert_eq!(Aspect::Original.ratio(p, true), Some(1.5), "Original keeps the picture's orientation");
        assert_eq!(Aspect::Square.ratio(p, false), Some(1.0));
        assert_eq!(Aspect::Square.ratio(p, true), Some(1.0));
        assert!(approx(Aspect::R3x2.ratio(p, false).unwrap(), 1.5));
        assert!(approx(Aspect::R3x2.ratio(p, true).unwrap(), 2.0 / 3.0));
        assert!(approx(Aspect::R4x3.ratio(p, false).unwrap(), 4.0 / 3.0));
        assert!(approx(Aspect::R16x9.ratio(p, true).unwrap(), 9.0 / 16.0));
        // 4:5 is a portrait ratio unless swapped.
        assert!(approx(Aspect::R4x5.ratio(p, false).unwrap(), 4.0 / 5.0));
        assert!(approx(Aspect::R4x5.ratio(p, true).unwrap(), 5.0 / 4.0));
        assert_eq!(Aspect::Original.ratio([0.0, 10.0], false), None);
    }

    #[test]
    fn changing_the_ratio_shrinks_the_rectangle_around_its_centre() {
        let start = CropRect { l: 0.1, t: 0.1, r: 0.9, b: 0.9 };
        let r = fit_to_ratio(start, 1.0, PIC);
        assert!((px_ratio(r, PIC) - 1.0).abs() < 1e-3, "{r:?}");
        assert!(approx((r.l + r.r) * 0.5, 0.5) && approx((r.t + r.b) * 0.5, 0.5));
        assert!(r.l >= start.l - 1e-6 && r.r <= start.r + 1e-6 && r.t >= start.t - 1e-6 && r.b <= start.b + 1e-6, "never grows: {r:?}");
        let wide = fit_to_ratio(CropRect::FULL, 16.0 / 9.0, PIC);
        assert!(approx(wide.w(), 1.0) && (px_ratio(wide, PIC) - 16.0 / 9.0).abs() < 1e-3, "{wide:?}");
        assert_eq!(fit_to_ratio(start, f32::NAN, PIC), start);
    }

    #[test]
    fn clamping_repairs_any_rectangle() {
        let r = clamp_rect(CropRect { l: -0.5, t: 0.9, r: 0.2, b: 0.95 }, PIC);
        assert!(r.l >= 0.0 && r.r <= 1.0 && r.t >= 0.0 && r.b <= 1.0 && r.w() > 0.0 && r.h() > 0.0, "{r:?}");
        let r = clamp_rect(CropRect { l: 0.8, t: 0.0, r: 2.0, b: 1.0 }, PIC);
        assert!(r.r <= 1.0 && approx(r.r - r.l, 1.0 - 0.8_f32.min(r.l)) || r.r <= 1.0, "{r:?}");
        let r = clamp_rect(CropRect { l: f32::NAN, t: 0.0, r: f32::NAN, b: 1.0 }, PIC);
        assert!(r.l.is_finite() && r.r.is_finite());
        assert!(CropRect::FULL.is_full());
    }

    #[test]
    fn the_picture_size_matches_the_engine() {
        assert_eq!(picture_size(2560, 1707, 0.0, 0), [2560.0, 1707.0]);
        assert_eq!(picture_size(2560, 1707, 0.0, 1), [1707.0, 2560.0], "a quarter turn swaps the sides");
        let [w, h] = picture_size(2560, 1707, 4.0, 0);
        assert!(w < 2560.0 && h < 1707.0 && (w / h - 2560.0 / 1707.0).abs() < 0.01, "{w}x{h}");
        // Cropping the straightened picture gives exactly the engine's size.
        let p = TransformParams { left: 0.1, top: 0.2, right: 0.9, bottom: 0.8, angle_deg: 4.0, ..Default::default() };
        let (rw, rh) = result_size(2560, 1707, &p);
        assert!((rw as f32 - w * 0.8).abs() <= 1.0 && (rh as f32 - h * 0.6).abs() <= 1.0, "{rw}x{rh} vs {w}x{h}");
    }

    #[test]
    fn orientation_uv_follows_flips_and_quarter_turns() {
        let id = oriented_uv(0, false, false);
        assert_eq!(id, [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
        assert_eq!(oriented_uv(0, true, false)[0], [1.0, 0.0], "mirrored");
        assert_eq!(oriented_uv(0, false, true)[0], [0.0, 1.0]);
        // One clockwise turn: the top-left of the result shows the source's bottom-left.
        assert_eq!(oriented_uv(1, false, false)[0], [0.0, 1.0]);
        assert_eq!(oriented_uv(2, false, false)[0], [1.0, 1.0]);
        assert_eq!(oriented_uv(3, false, false)[0], [1.0, 0.0]);
        assert_eq!(oriented_uv(4, false, false), id, "modulo 4");
        // The same mapping as the engine's remap for a 1-turn (x, y) <- (y, h-1-x) on a 3x2 picture.
        let (w, h) = (3.0f32, 2.0f32);
        let uv = oriented_uv(1, false, false);
        // Result pixel (0, 0) comes from source (0, h-1): bottom-left.
        assert_eq!([uv[0][0] * w, uv[0][1] * h], [0.0, 2.0]);
    }

    #[test]
    fn rotation_is_clockwise_on_screen() {
        let c = rotated_corners([100.0, 100.0], 100.0, 50.0, 0.0);
        assert_eq!(c[0], [50.0, 75.0]);
        let q = rotated_corners([0.0, 0.0], 2.0, 2.0, 90.0);
        // Top-left (-1, -1) turned 90 degrees clockwise (y down) lands on top-right (1, -1).
        assert!((q[0][0] - 1.0).abs() < 1e-5 && (q[0][1] + 1.0).abs() < 1e-5, "{q:?}");
    }
}
