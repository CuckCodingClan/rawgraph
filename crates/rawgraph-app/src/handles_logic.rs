//! Pure rules of the in-viewer handles (no egui types): where each handle of a node sits in
//! the node's own picture frame, and which parameter values a dragged handle stands for.
//!
//! Positions are normalised frame coordinates (0..1 across the picture the node produces,
//! y down), the same space the Generator gradient is defined in. `size` is the frame's pixel
//! size, needed where a node measures in pixels (Vignette distance, Filter direction).

use rawgraph_core::color::{self, Mat3};

/// A draggable part of a node's handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// Gradient origin: moves the whole gradient.
    Center,
    /// Linear gradient start (t = 0); the end stays.
    Start,
    /// Linear gradient end (t = 1); the start stays.
    End,
    /// Radial / Reflected / Diamond extent (t = 1), also sets the angle.
    Edge,
    /// Angle-style gradient direction (angle only).
    Direction,
    /// Vignette midpoint ring.
    Midpoint,
    /// Vignette inner feather ring (effect starts).
    FeatherInner,
    /// Vignette outer feather ring (full effect).
    FeatherOuter,
    /// Filter motion direction arrow.
    Motion,
}

// ------------------------------------------------------------------ gradient

/// Gradient settings as the Generator renders them (`rawgraph_core::generator`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    /// 0 Linear, 1 Radial, 2 Angle, 3 Reflected, 4 Diamond.
    pub style: u32,
    /// Degrees, -180..180 (the `gradient_angle` parameter).
    pub angle: f32,
    /// Fraction (the `gradient_scale` parameter / 100).
    pub scale: f32,
    /// Normalised origin (the `gradient_center_*` parameters / 100).
    pub center: [f32; 2],
    /// The legacy vertical toggle adds 90 degrees.
    pub vertical: bool,
}

/// Length of the Angle-style direction handle, in normalised units.
pub const ANGLE_HANDLE_LEN: f32 = 0.25;
pub const SCALE_MIN: f32 = 0.01;
pub const SCALE_MAX: f32 = 4.0;
pub const CENTER_MIN: f32 = -1.0;
pub const CENTER_MAX: f32 = 2.0;

fn wrap_deg(a: f32) -> f32 {
    let w = (a + 180.0).rem_euclid(360.0) - 180.0;
    if w.is_finite() { w } else { 0.0 }
}

impl Gradient {
    /// The rendered direction in radians (angle plus the legacy vertical quarter).
    pub fn radians(&self) -> f32 {
        (self.angle + if self.vertical { 90.0 } else { 0.0 }).to_radians()
    }

    fn dir(&self) -> [f32; 2] {
        let (s, c) = self.radians().sin_cos();
        [c, s]
    }

    fn at(&self, k: f32) -> [f32; 2] {
        let d = self.dir();
        [self.center[0] + d[0] * k, self.center[1] + d[1] * k]
    }

    /// The parts this style offers, in hit-test priority order (small handles first).
    pub fn parts(&self) -> &'static [Part] {
        match self.style {
            0 => &[Part::Start, Part::End, Part::Center],
            2 => &[Part::Direction, Part::Center],
            _ => &[Part::Edge, Part::Center],
        }
    }

    /// Where a part sits (normalised frame coordinates).
    pub fn position(&self, part: Part) -> [f32; 2] {
        match part {
            Part::Start => self.at(-self.scale * 0.5),
            Part::End | Part::Edge => self.at(self.scale * 0.5),
            Part::Direction => self.at(ANGLE_HANDLE_LEN),
            _ => self.center,
        }
    }

    fn set_angle_from(&mut self, v: [f32; 2]) {
        if v[0].hypot(v[1]) > 1e-6 {
            self.angle = wrap_deg(v[1].atan2(v[0]).to_degrees() - if self.vertical { 90.0 } else { 0.0 });
        }
    }

    /// The gradient after dragging `part` to `p` (normalised frame coordinates).
    pub fn drag(mut self, part: Part, p: [f32; 2]) -> Gradient {
        let p = [p[0].clamp(CENTER_MIN, CENTER_MAX), p[1].clamp(CENTER_MIN, CENTER_MAX)];
        match part {
            Part::Center => self.center = p,
            Part::Start | Part::End => {
                let fixed = self.position(if part == Part::Start { Part::End } else { Part::Start });
                let (a, b) = if part == Part::Start { (p, fixed) } else { (fixed, p) };
                let v = [b[0] - a[0], b[1] - a[1]];
                self.set_angle_from(v);
                self.scale = v[0].hypot(v[1]).clamp(SCALE_MIN, SCALE_MAX);
                // Keep the fixed end where it was, also when the length was clamped.
                let d = self.dir();
                let half = self.scale * 0.5;
                self.center = if part == Part::Start { [fixed[0] - d[0] * half, fixed[1] - d[1] * half] } else { [fixed[0] + d[0] * half, fixed[1] + d[1] * half] };
            }
            Part::Edge => {
                let v = [p[0] - self.center[0], p[1] - self.center[1]];
                self.set_angle_from(v);
                self.scale = (2.0 * v[0].hypot(v[1])).clamp(SCALE_MIN, SCALE_MAX);
            }
            Part::Direction => self.set_angle_from([p[0] - self.center[0], p[1] - self.center[1]]),
            _ => {}
        }
        self
    }

    /// The t = 1 contour of Radial (an ellipse in normalised units) and Diamond, as a closed
    /// polyline; empty for the other styles.
    pub fn contour(&self, n: usize) -> Vec<[f32; 2]> {
        let r = self.scale * 0.5;
        let (s, c) = self.radians().sin_cos();
        match self.style {
            1 => (0..n).map(|i| {
                let a = i as f32 / n as f32 * std::f32::consts::TAU;
                [self.center[0] + r * a.cos(), self.center[1] + r * a.sin()]
            }).collect(),
            4 => [[r, 0.0], [0.0, r], [-r, 0.0], [0.0, -r]].iter().map(|[x, y]| [self.center[0] + x * c - y * s, self.center[1] + x * s + y * c]).collect(),
            _ => Vec::new(),
        }
    }
}

// ------------------------------------------------------------------ vignette

/// Vignette distance of a normalised point: `rawgraph_fx::vignette`'s metric (pixel distance
/// from the frame centre over the half diagonal).
pub fn vignette_distance(p: [f32; 2], size: [f32; 2]) -> f32 {
    let (hw, hh) = (size[0] * 0.5, size[1] * 0.5);
    let d = hw.hypot(hh);
    if d <= 0.0 { return 0.0; }
    ((p[0] - 0.5) * size[0]).hypot((p[1] - 0.5) * size[1]) / d
}

/// Radii (vignette distances) of the inner feather, midpoint and outer feather rings, exactly
/// the smoothstep edges `rawgraph_fx::vignette` uses.
pub fn vignette_rings(midpoint: f32, feather: f32) -> [f32; 3] {
    let feather = feather.max(0.0);
    let e0 = (midpoint - feather * 0.5).max(0.0);
    let e1 = (midpoint + feather * 0.5).max(e0);
    [e0, midpoint, e1]
}

/// Points of the ring at vignette distance `d` (a circle in pixels), normalised.
pub fn vignette_ring(d: f32, size: [f32; 2], n: usize) -> Vec<[f32; 2]> {
    let r = d * (size[0] * 0.5).hypot(size[1] * 0.5);
    if size[0] <= 0.0 || size[1] <= 0.0 { return Vec::new(); }
    (0..n).map(|i| {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        [0.5 + r * a.cos() / size[0], 0.5 + r * a.sin() / size[1]]
    }).collect()
}

/// Where the knob of a ring sits: on the diagonal towards the top-right corner (`upper`) or
/// the bottom-right corner, so the midpoint and feather knobs stay apart when the rings meet.
pub fn vignette_knob(d: f32, size: [f32; 2], upper: bool) -> [f32; 2] {
    let (hw, hh) = (size[0] * 0.5, size[1] * 0.5);
    let diag = hw.hypot(hh);
    if diag <= 0.0 { return [0.5, 0.5]; }
    let dy = d * hh / size[1];
    [0.5 + d * hw / size[0], if upper { 0.5 - dy } else { 0.5 + dy }]
}

/// (midpoint, feather) after dragging a ring to vignette distance `d`.
pub fn vignette_drag(part: Part, d: f32, midpoint: f32, feather: f32) -> (f32, f32) {
    match part {
        Part::Midpoint => (d.clamp(0.0, 1.0), feather),
        Part::FeatherInner | Part::FeatherOuter => (midpoint, (2.0 * (d - midpoint).abs()).clamp(0.0, 1.0)),
        _ => (midpoint, feather),
    }
}

// --------------------------------------------------------------- filter arrow

/// Unit direction of the motion blur in normalised frame units for a `angle` in degrees
/// (pixel space, positive y down, as `filter_sampled_direction` samples) scaled to `len`
/// pixels.
pub fn motion_vector(angle_deg: f32, len_px: f32, size: [f32; 2]) -> [f32; 2] {
    let (s, c) = angle_deg.to_radians().sin_cos();
    if size[0] <= 0.0 || size[1] <= 0.0 { return [0.0, 0.0]; }
    [c * len_px / size[0], s * len_px / size[1]]
}

/// Motion angle (0..360 degrees) pointing from the frame centre to `p`.
pub fn motion_angle(p: [f32; 2], size: [f32; 2], fallback: f32) -> f32 {
    let (dx, dy) = ((p[0] - 0.5) * size[0], (p[1] - 0.5) * size[1]);
    if dx.hypot(dy) < 1e-3 { return fallback; }
    dy.atan2(dx).to_degrees().rem_euclid(360.0)
}

// -------------------------------------------------------------- white balance

/// Log-chroma residual of `m * c` (zero for a neutral result).
fn residual(m: &Mat3, c: [f64; 3]) -> Option<[f64; 2]> {
    let o = color::mul_vec(m, c);
    if o.iter().any(|v| !v.is_finite() || *v <= 0.0) { return None; }
    Some([(o[0] / o[1]).ln(), (o[2] / o[1]).ln()])
}

/// Finds the two parameters (within `lo..hi`) for which `matrix(params) * c` is neutral
/// (r = g = b), by damped Gauss-Newton with numerical derivatives from `start`. `c` must be
/// positive. Returns the parameters and the remaining log-chroma error.
pub fn solve_neutral(c: [f32; 3], start: [f64; 2], lo: [f64; 2], hi: [f64; 2], matrix: impl Fn([f64; 2]) -> Mat3) -> Option<([f64; 2], f64)> {
    let c = c.map(f64::from);
    if c.iter().any(|v| !v.is_finite() || *v <= 0.0) { return None; }
    let clamp = |p: [f64; 2]| [p[0].clamp(lo[0], hi[0]), p[1].clamp(lo[1], hi[1])];
    let err = |r: [f64; 2]| r[0].hypot(r[1]);
    let mut p = clamp(start);
    let mut r = residual(&matrix(p), c)?;
    let mut lambda = 1e-3;
    for _ in 0..60 {
        if err(r) < 1e-7 { break; }
        let h = [(hi[0] - lo[0]) * 1e-5, (hi[1] - lo[1]) * 1e-5];
        let mut jac = [[0.0; 2]; 2];
        for k in 0..2 {
            let mut q = p;
            q[k] += h[k];
            let rq = residual(&matrix(q), c)?;
            jac[0][k] = (rq[0] - r[0]) / h[k];
            jac[1][k] = (rq[1] - r[1]) / h[k];
        }
        // (J^T J + lambda diag) step = -J^T r
        let jtj = [
            [jac[0][0] * jac[0][0] + jac[1][0] * jac[1][0], jac[0][0] * jac[0][1] + jac[1][0] * jac[1][1]],
            [jac[0][1] * jac[0][0] + jac[1][1] * jac[1][0], jac[0][1] * jac[0][1] + jac[1][1] * jac[1][1]],
        ];
        let g = [jac[0][0] * r[0] + jac[1][0] * r[1], jac[0][1] * r[0] + jac[1][1] * r[1]];
        let mut improved = false;
        for _ in 0..12 {
            let a = [[jtj[0][0] * (1.0 + lambda), jtj[0][1]], [jtj[1][0], jtj[1][1] * (1.0 + lambda)]];
            let det = a[0][0] * a[1][1] - a[0][1] * a[1][0];
            if det.abs() < 1e-300 { lambda *= 10.0; continue; }
            let step = [-(a[1][1] * g[0] - a[0][1] * g[1]) / det, -(-a[1][0] * g[0] + a[0][0] * g[1]) / det];
            let q = clamp([p[0] + step[0], p[1] + step[1]]);
            if let Some(rq) = residual(&matrix(q), c) {
                if err(rq) < err(r) {
                    p = q;
                    r = rq;
                    lambda = (lambda * 0.3).max(1e-9);
                    improved = true;
                    break;
                }
            }
            lambda *= 10.0;
        }
        if !improved { break; }
    }
    Some((p, err(r)))
}

/// Temperature / tint shift (mired, tint units) of a White Balance node that makes the
/// ACEScg colour `c` neutral.
pub fn neutral_shift(c: [f32; 3], start: (f32, f32)) -> Option<((f32, f32), f64)> {
    let (p, e) = solve_neutral(c, [f64::from(start.0), f64::from(start.1)], [-150.0, -100.0], [150.0, 100.0], |p| color::white_balance_matrix(p[0], p[1]))?;
    Some(((p[0] as f32, p[1] as f32), e))
}

/// Absolute temperature (K) and tint of a Develop node (Custom white balance, `as_shot`
/// reference) that make the ACEScg colour `c` of its input neutral. Solved in mired.
pub fn neutral_absolute(c: [f32; 3], as_shot: (f32, f32), start: (f32, f32)) -> Option<((f32, f32), f64)> {
    let as_shot = (f64::from(as_shot.0), f64::from(as_shot.1));
    let to_mired = |k: f64| 1.0e6 / k;
    let start = [to_mired(f64::from(start.0).clamp(2000.0, 25000.0)), f64::from(start.1)];
    let (p, e) = solve_neutral(c, start, [to_mired(25000.0), -150.0], [to_mired(2000.0), 150.0], |p| color::absolute_white_balance_matrix(as_shot, (1.0e6 / p[0], p[1])))?;
    Some((((1.0e6 / p[0]) as f32, p[1] as f32), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4
    }

    fn g(style: u32) -> Gradient {
        Gradient { style, angle: 30.0, scale: 0.8, center: [0.4, 0.55], vertical: false }
    }

    #[test]
    fn linear_start_and_end_drag_round_trip_and_keep_the_other_end() {
        let a = g(0);
        let end = a.position(Part::End);
        let target = [0.9, 0.2];
        let b = a.drag(Part::Start, target);
        assert!(close(b.position(Part::Start), target), "start lands under the pointer");
        assert!(close(b.position(Part::End), end), "end stays");
        let c = b.drag(Part::End, [0.1, 0.9]);
        assert!(close(c.position(Part::End), [0.1, 0.9]));
        assert!(close(c.position(Part::Start), target));
        // Dragging a handle onto itself changes nothing.
        let same = a.drag(Part::End, end);
        assert!((same.angle - a.angle).abs() < 1e-3 && (same.scale - a.scale).abs() < 1e-5 && close(same.center, a.center));
    }

    #[test]
    fn linear_handles_are_where_the_ramp_is_zero_and_one() {
        // t = 0.5 + along / scale (generator.rs): t(start) = 0, t(end) = 1.
        let a = g(0);
        let t = |p: [f32; 2]| {
            let (s, c) = a.radians().sin_cos();
            0.5 + ((p[0] - a.center[0]) * c + (p[1] - a.center[1]) * s) / a.scale
        };
        assert!(t(a.position(Part::Start)).abs() < 1e-5);
        assert!((t(a.position(Part::End)) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn radial_edge_sets_radius_and_centre_moves_everything() {
        let a = g(1);
        let b = a.drag(Part::Edge, [0.4, 0.95]);
        assert!((b.scale - 0.8).abs() < 1e-5, "|d| * 2 / scale = 1 at the edge");
        assert!((b.angle - 90.0).abs() < 1e-3);
        assert!(close(b.position(Part::Edge), [0.4, 0.95]));
        let c = b.drag(Part::Center, [0.6, 0.6]);
        assert_eq!(c.scale, b.scale);
        assert!(close(c.position(Part::Edge), [0.6, 1.0]));
        assert_eq!(c.contour(4).len(), 4);
    }

    #[test]
    fn angle_style_direction_changes_only_the_angle_and_vertical_is_respected() {
        let mut a = g(2);
        a.vertical = true;
        let b = a.drag(Part::Direction, [a.center[0] + 0.1, a.center[1]]);
        assert!((b.radians()).abs() < 1e-4, "direction points right");
        assert!((b.angle + 90.0).abs() < 1e-3, "vertical adds 90 degrees");
        assert_eq!((b.scale, b.center), (a.scale, a.center));
    }

    #[test]
    fn gradient_values_stay_in_parameter_ranges() {
        let a = g(0).drag(Part::End, [5.0, -5.0]);
        assert!((-180.0..=180.0).contains(&a.angle));
        assert!((SCALE_MIN..=SCALE_MAX).contains(&a.scale));
        assert!(a.center.iter().all(|c| (CENTER_MIN..=CENTER_MAX).contains(c)));
        let b = g(1).drag(Part::Edge, [0.4, 0.55]);
        assert_eq!(b.scale, SCALE_MIN);
    }

    #[test]
    fn vignette_rings_use_the_effect_metric() {
        let size = [300.0, 200.0];
        assert_eq!(vignette_distance([0.5, 0.5], size), 0.0);
        assert!((vignette_distance([1.0, 1.0], size) - 1.0).abs() < 1e-6, "corner is 1");
        for d in [0.2, 0.5, 0.9] {
            for p in vignette_ring(d, size, 16) {
                assert!((vignette_distance(p, size) - d).abs() < 1e-5);
            }
            for upper in [true, false] {
                assert!((vignette_distance(vignette_knob(d, size, upper), size) - d).abs() < 1e-5);
            }
        }
        assert_eq!(vignette_rings(0.5, 0.4), [0.3, 0.5, 0.7]);
        assert_eq!(vignette_rings(0.1, 0.6), [0.0, 0.1, 0.4]);
        assert_eq!(vignette_drag(Part::Midpoint, 0.7, 0.5, 0.4), (0.7, 0.4));
        let (m, f) = vignette_drag(Part::FeatherOuter, 0.8, 0.5, 0.4);
        assert!((m - 0.5).abs() < 1e-6 && (f - 0.6).abs() < 1e-6);
        assert_eq!(vignette_drag(Part::FeatherInner, 2.0, 0.5, 0.4).1, 1.0);
    }

    #[test]
    fn motion_arrow_angle_round_trips() {
        let size = [300.0, 200.0];
        for angle in [0.0f32, 30.0, 90.0, 200.0, 315.0] {
            let v = motion_vector(angle, 50.0, size);
            let back = motion_angle([0.5 + v[0], 0.5 + v[1]], size, 0.0);
            assert!((back - angle).abs() < 1e-3, "{angle} -> {back}");
        }
        assert_eq!(motion_angle([0.5, 0.5], size, 42.0), 42.0, "the centre keeps the angle");
    }

    #[test]
    fn white_balance_picker_neutralises_a_tinted_patch() {
        let grey = [0.18f32; 3];
        // A patch tinted by a known White Balance node setting: the picker finds the inverse.
        let tint_m = color::to_f32(&color::white_balance_matrix(-60.0, 35.0));
        let tinted: [f32; 3] = std::array::from_fn(|i| (0..3).map(|j| tint_m[i][j] * grey[j]).sum());
        let ((t, n), e) = neutral_shift(tinted, (0.0, 0.0)).unwrap();
        assert!(e < 1e-3, "residual {e}");
        let m = color::white_balance_matrix(f64::from(t), f64::from(n));
        let out = color::mul_vec(&m, tinted.map(f64::from));
        assert!((out[0] / out[1] - 1.0).abs() < 2e-3 && (out[2] / out[1] - 1.0).abs() < 2e-3, "{out:?}");
        // Develop: absolute temperature / tint against an as-shot reference.
        let warm = [0.24f32, 0.18, 0.12];
        let ((k, tn), e) = neutral_absolute(warm, (5000.0, 0.0), (5000.0, 0.0)).unwrap();
        assert!(e < 1e-2, "residual {e}");
        let out = color::mul_vec(&color::absolute_white_balance_matrix((5000.0, 0.0), (f64::from(k), f64::from(tn))), warm.map(f64::from));
        assert!((out[0] / out[1] - 1.0).abs() < 1e-2 && (out[2] / out[1] - 1.0).abs() < 1e-2, "{k} K {tn}: {out:?}");
        assert!(neutral_shift([0.0, 0.1, 0.1], (0.0, 0.0)).is_none(), "black channels cannot be balanced");
    }
}
