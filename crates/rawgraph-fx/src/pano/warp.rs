//! Projection of frames into the panorama canvas and tile rendering.

use rawgraph_core::ImageBuf;

use super::blend::TileInput;
use super::ransac::{apply, Mat3};
use super::Projection;

/// Maps between one frame and the canvas.
///
/// Canvas coordinates are in the output projection (units of pixels, origin at the reference frame
/// centre). Frame coordinates `pos` are pixel positions (`0..w`, `0..h`); "centred" ones are
/// relative to the frame centre.
#[derive(Clone)]
pub(super) struct Warper {
    pub kind: Projection,
    pub w: f64,
    pub h: f64,
    /// Focal length of this frame in pixels.
    pub f_in: f64,
    /// Focal length of the output projection in pixels.
    pub f_out: f64,
    /// Rotation reference <- frame (projections).
    pub rot: Mat3,
    /// Frame -> canvas (planar).
    pub m: Mat3,
    /// Canvas -> frame (planar).
    pub minv: Mat3,
}

impl Warper {
    /// Centred frame pixel -> canvas coordinates.
    pub fn forward(&self, xc: f64, yc: f64) -> Option<[f64; 2]> {
        match self.kind {
            Projection::Planar => {
                // Points on or behind the horizon (w <= 0) have no planar image.
                let w = self.m[2][0] * xc + self.m[2][1] * yc + self.m[2][2];
                if w <= 1e-9 {
                    return None;
                }
                apply(&self.m, xc, yc)
            }
            _ => {
                let v = [xc / self.f_in, yc / self.f_in, 1.0];
                let r = &self.rot;
                let ray = [
                    r[0][0] * v[0] + r[0][1] * v[1] + r[0][2] * v[2],
                    r[1][0] * v[0] + r[1][1] * v[1] + r[1][2] * v[2],
                    r[2][0] * v[0] + r[2][1] * v[1] + r[2][2] * v[2],
                ];
                let hz = ray[0].hypot(ray[2]);
                if hz < 1e-9 {
                    return None;
                }
                let theta = ray[0].atan2(ray[2]);
                let y = if self.kind == Projection::Cylindrical { ray[1] / hz } else { ray[1].atan2(hz) };
                Some([self.f_out * theta, self.f_out * y])
            }
        }
    }

    /// Frame positions for a grid of canvas coordinates (`ys` outer, `xs` inner). Invalid points
    /// (behind the camera, at infinity) have NaN coordinates; points outside the frame are kept.
    pub fn map_grid(&self, xs: &[f64], ys: &[f64]) -> Vec<[f32; 2]> {
        let mut out = vec![[f32::NAN; 2]; xs.len() * ys.len()];
        match self.kind {
            Projection::Planar => {
                let m = &self.minv;
                for (j, &y) in ys.iter().enumerate() {
                    for (i, &x) in xs.iter().enumerate() {
                        let z = m[2][0] * x + m[2][1] * y + m[2][2];
                        if z > 1e-9 {
                            out[j * xs.len() + i] = [
                                ((m[0][0] * x + m[0][1] * y + m[0][2]) / z + self.w * 0.5) as f32,
                                ((m[1][0] * x + m[1][1] * y + m[1][2]) / z + self.h * 0.5) as f32,
                            ];
                        }
                    }
                }
            }
            kind => {
                // rot is frame->ref; rays are rotated back with its transpose.
                let r = &self.rot;
                let rinv = [[r[0][0], r[1][0], r[2][0]], [r[0][1], r[1][1], r[2][1]], [r[0][2], r[1][2], r[2][2]]];
                let cols: Vec<(f64, f64)> = xs.iter().map(|&x| (x / self.f_out).sin_cos()).collect();
                for (j, &y) in ys.iter().enumerate() {
                    let (sv, cv) = if kind == Projection::Cylindrical {
                        (y / self.f_out, 1.0)
                    } else {
                        let phi = y / self.f_out;
                        if phi.abs() > std::f64::consts::FRAC_PI_2 {
                            continue;
                        }
                        phi.sin_cos()
                    };
                    for (i, &(st, ct)) in cols.iter().enumerate() {
                        let ray = [st * cv, sv, ct * cv];
                        let fx = rinv[0][0] * ray[0] + rinv[0][1] * ray[1] + rinv[0][2] * ray[2];
                        let fy = rinv[1][0] * ray[0] + rinv[1][1] * ray[1] + rinv[1][2] * ray[2];
                        let fz = rinv[2][0] * ray[0] + rinv[2][1] * ray[1] + rinv[2][2] * ray[2];
                        if fz > 1e-4 {
                            out[j * xs.len() + i] =
                                [(self.f_in * fx / fz + self.w * 0.5) as f32, (self.f_in * fy / fz + self.h * 0.5) as f32];
                        }
                    }
                }
            }
        }
        out
    }
}

/// Distance to the nearest frame edge, 0 outside the frame or for invalid points.
#[inline]
pub(super) fn edge_weight(pos: [f32; 2], w: f32, h: f32) -> f32 {
    let (x, y) = (pos[0], pos[1]);
    if x.is_nan() || y.is_nan() {
        return 0.0;
    }
    x.min(w - x).min(y).min(h - y).max(0.0)
}

/// Clamped bilinear sample (RGB) at a pixel position. Non-finite taps count as 0.
#[inline]
pub(super) fn sample_rgb(img: &ImageBuf, pos: [f32; 2]) -> [f32; 3] {
    sample_rgb_checked(img, pos).0
}

/// Like [`sample_rgb`], also telling whether every tap was finite.
#[inline]
pub(super) fn sample_rgb_checked(img: &ImageBuf, pos: [f32; 2]) -> ([f32; 3], bool) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let x = (pos[0] - 0.5).clamp(0.0, (w - 1) as f32);
    let y = (pos[1] - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x as usize, y as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let d = img.data();
    let mut ok = true;
    let mut at = |xx: usize, yy: usize, c: usize| {
        let v = d[(yy * w + xx) * 4 + c];
        if v.is_finite() {
            v
        } else {
            ok = false;
            0.0
        }
    };
    let mut o = [0.0; 3];
    for (c, v) in o.iter_mut().enumerate() {
        let top = at(x0, y0, c) * (1.0 - fx) + at(x1, y0, c) * fx;
        let bot = at(x0, y1, c) * (1.0 - fx) + at(x1, y1, c) * fx;
        *v = top * (1.0 - fy) + bot * fy;
    }
    (o, ok)
}

/// Renders one frame into the canvas region `[x0, x0+w) x [y0, y0+h)` (integer canvas
/// coordinates, pixel centres at +0.5). Colours beyond the frame edge are replicated so the
/// pyramids have valid data there; the weight is zero outside.
pub(super) fn render_frame_tile(img: &ImageBuf, wp: &Warper, gain: f32, x0: i64, y0: i64, w: usize, h: usize) -> TileInput {
    let xs: Vec<f64> = (0..w).map(|i| (x0 + i as i64) as f64 + 0.5).collect();
    let ys: Vec<f64> = (0..h).map(|j| (y0 + j as i64) as f64 + 0.5).collect();
    let pos = wp.map_grid(&xs, &ys);
    let (fw, fh) = (img.width() as f32, img.height() as f32);
    let mut color = vec![0.0f32; w * h * 3];
    let mut weight = vec![0.0f32; w * h];
    for (p, &ps) in pos.iter().enumerate() {
        if ps[0].is_nan() {
            continue;
        }
        let (c, ok) = sample_rgb_checked(img, ps);
        if !ok {
            // NaN / Inf in the source: no contribution (colour 0, weight 0).
            continue;
        }
        weight[p] = edge_weight(ps, fw, fh);
        for ch in 0..3 {
            color[p * 3 + ch] = c[ch] * gain;
        }
    }
    TileInput { color, weight }
}
