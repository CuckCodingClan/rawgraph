//! Geometry: crop / rotate / flip transform and the manual lens model.

use rawgraph_core::ImageBuf;
use rayon::prelude::*;

use crate::util::{bilinear, finite_or};

/// Parameters of [`transform`]. Order of operations: flips, quarter turns (clockwise),
/// fine rotation, crop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransformParams {
    /// Crop edges as fractions (0..1) of the rotated result: left/top = 0 and
    /// right/bottom = 1 keep everything.
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    /// Fine rotation in degrees, positive = clockwise.
    pub angle_deg: f32,
    /// Clockwise quarter turns (taken modulo 4).
    pub quarter_turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
}

impl Default for TransformParams {
    fn default() -> Self {
        TransformParams {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
            angle_deg: 0.0,
            quarter_turns: 0,
            flip_h: false,
            flip_v: false,
        }
    }
}

/// Angles below this (degrees) count as no rotation.
const ANGLE_EPS: f32 = 1e-6;

impl TransformParams {
    /// True when [`transform`] returns the input unchanged.
    pub fn is_identity(&self) -> bool {
        self.fine_angle() == 0.0
            && self.quarter_turns.is_multiple_of(4)
            && !self.flip_h
            && !self.flip_v
            && self.crop_fractions() == [0.0, 0.0, 1.0, 1.0]
    }

    fn fine_angle(&self) -> f32 {
        let a = finite_or(self.angle_deg, 0.0);
        if a.abs() < ANGLE_EPS {
            0.0
        } else {
            a
        }
    }

    fn crop_fractions(&self) -> [f32; 4] {
        [
            finite_or(self.left, 0.0).clamp(0.0, 1.0),
            finite_or(self.top, 0.0).clamp(0.0, 1.0),
            finite_or(self.right, 1.0).clamp(0.0, 1.0),
            finite_or(self.bottom, 1.0).clamp(0.0, 1.0),
        ]
    }
}

/// Scale of the largest axis-aligned rectangle with the aspect ratio of a `w` x `h`
/// image that fits inside that image rotated by `angle_deg` (1 = no rotation).
pub(crate) fn inscribed_scale(w: f32, h: f32, angle_deg: f32) -> f32 {
    let (s, c) = angle_deg.to_radians().sin_cos();
    let (s, c) = (s.abs(), c.abs());
    let denom_w = w * c + h * s;
    let denom_h = w * s + h * c;
    (w / denom_w).min(h / denom_h).min(1.0)
}

struct Plan {
    /// Size after flips and quarter turns.
    w1: u32,
    h1: u32,
    /// Size after the fine rotation (before the crop).
    rw: u32,
    rh: u32,
    /// Crop box inside the rotated result: x0..x1, y0..y1.
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

fn plan(width: u32, height: u32, p: &TransformParams) -> Plan {
    let (w1, h1) = if p.quarter_turns % 2 == 1 { (height, width) } else { (width, height) };
    let angle = p.fine_angle();
    let (rw, rh) = if angle == 0.0 || w1 == 0 || h1 == 0 {
        (w1, h1)
    } else {
        let s = inscribed_scale(w1 as f32, h1 as f32, angle);
        (((s * w1 as f32 + 1e-3).floor() as u32).clamp(1, w1), ((s * h1 as f32 + 1e-3).floor() as u32).clamp(1, h1))
    };
    let [l, t, r, b] = p.crop_fractions();
    let edge = |lo: f32, hi: f32, n: u32| {
        if n == 0 {
            return (0, 0);
        }
        let a = ((lo * n as f32).round() as u32).min(n - 1);
        let z = ((hi * n as f32).round() as u32).max(a + 1).min(n);
        (a, z)
    };
    let (x0, x1) = edge(l, r, rw);
    let (y0, y1) = edge(t, b, rh);
    Plan { w1, h1, rw, rh, x0, y0, x1, y1 }
}

/// Size of the image [`transform`] produces for a `width` x `height` input.
pub fn transform_size(width: u32, height: u32, p: &TransformParams) -> (u32, u32) {
    let pl = plan(width, height, p);
    (pl.x1 - pl.x0, pl.y1 - pl.y0)
}

/// Maps a point given in normalised coordinates of the [`transform`] OUTPUT image (0..1 in x
/// and y, pixel edges at the integers of the pixel grid) to normalised coordinates of its INPUT
/// image (`width` x `height` is the size of the input). Same order as [`transform`]: crop, fine
/// rotation, quarter turns, flips undone in reverse. Works for any point, also outside 0..1
/// (extrapolating the same affine map). Degenerate (zero-sized) inputs return the point unchanged.
pub fn transform_output_to_input(width: u32, height: u32, p: &TransformParams, x: f32, y: f32) -> (f32, f32) {
    if width == 0 || height == 0 {
        return (x, y);
    }
    let pl = plan(width, height, p);
    let (w, h) = (width as f32, height as f32);
    let (w1, h1) = (pl.w1 as f32, pl.h1 as f32);
    // Output pixel space -> rotated-result space -> pre-rotation space (after flips and turns).
    let mut px = pl.x0 as f32 + x * (pl.x1 - pl.x0) as f32;
    let mut py = pl.y0 as f32 + y * (pl.y1 - pl.y0) as f32;
    let angle = p.fine_angle();
    if angle != 0.0 {
        let (sin, cos) = angle.to_radians().sin_cos();
        let (dx, dy) = (px - pl.rw as f32 * 0.5, py - pl.rh as f32 * 0.5);
        px = cos * dx + sin * dy + w1 * 0.5;
        py = -sin * dx + cos * dy + h1 * 0.5;
    }
    // Undo the quarter turns (clockwise), then the flips (applied first when transforming).
    let (mut sx, mut sy) = match p.quarter_turns % 4 {
        1 => (py, h - px),
        2 => (w - px, h - py),
        3 => (w - py, px),
        _ => (px, py),
    };
    if p.flip_v {
        sy = h - sy;
    }
    if p.flip_h {
        sx = w - sx;
    }
    (sx / w, sy / h)
}

/// The inverse of [`transform_output_to_input`]: a point in normalised coordinates of the input
/// image (`width` x `height`) -> normalised coordinates of the [`transform`] output image. The
/// result lies outside 0..1 when the point is cropped away (or in a rotated-out corner).
pub fn transform_input_to_output(width: u32, height: u32, p: &TransformParams, x: f32, y: f32) -> (f32, f32) {
    if width == 0 || height == 0 {
        return (x, y);
    }
    let pl = plan(width, height, p);
    let (w, h) = (width as f32, height as f32);
    let (w1, h1) = (pl.w1 as f32, pl.h1 as f32);
    let (mut sx, mut sy) = (x * w, y * h);
    if p.flip_h {
        sx = w - sx;
    }
    if p.flip_v {
        sy = h - sy;
    }
    let (mut px, mut py) = match p.quarter_turns % 4 {
        1 => (h - sy, sx),
        2 => (w - sx, h - sy),
        3 => (sy, w - sx),
        _ => (sx, sy),
    };
    let angle = p.fine_angle();
    if angle != 0.0 {
        let (sin, cos) = angle.to_radians().sin_cos();
        let (qx, qy) = (px - w1 * 0.5, py - h1 * 0.5);
        px = cos * qx - sin * qy + pl.rw as f32 * 0.5;
        py = sin * qx + cos * qy + pl.rh as f32 * 0.5;
    }
    ((px - pl.x0 as f32) / (pl.x1 - pl.x0) as f32, (py - pl.y0 as f32) / (pl.y1 - pl.y0) as f32)
}

/// Builds an image of `ow` x `oh` pixels, pixel (x, y) copied from `src(x, y)`.
fn remap_exact(img: &ImageBuf, ow: u32, oh: u32, src: impl Fn(u32, u32) -> (u32, u32) + Sync) -> ImageBuf {
    if ow == 0 || oh == 0 {
        return ImageBuf::new(ow, oh);
    }
    let w = img.width() as usize;
    let data = img.data();
    let mut out = vec![0.0f32; ow as usize * oh as usize * 4];
    out.par_chunks_exact_mut(ow as usize * 4).enumerate().for_each(|(y, row)| {
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let (sx, sy) = src(x as u32, y as u32);
            let i = (sy as usize * w + sx as usize) * 4;
            px.copy_from_slice(&data[i..i + 4]);
        }
    });
    ImageBuf::from_rgba(ow, oh, out).expect("size matches")
}

/// Crop, rotate and flip, in this order: flips; quarter turns (clockwise); fine
/// rotation by `angle_deg` (positive = clockwise) about the centre, shrunk to the
/// largest axis-aligned rectangle of the same aspect ratio that contains no empty
/// corner; then the crop fractions applied to that result (right > left and
/// bottom > top are enforced, at least 1 px remains). The fine rotation samples
/// bilinearly in linear light; flips, quarter turns and crops copy pixels exactly.
/// Identity parameters return a clone. The output size is [`transform_size`].
pub fn transform(img: &ImageBuf, p: &TransformParams) -> ImageBuf {
    if p.is_identity() || img.width() == 0 || img.height() == 0 {
        return img.clone();
    }
    let pl = plan(img.width(), img.height(), p);
    let (w, h) = (img.width(), img.height());

    // Flips and quarter turns (exact).
    let mut owned: Option<ImageBuf> = None;
    if p.flip_h {
        let src = owned.as_ref().unwrap_or(img);
        owned = Some(remap_exact(src, w, h, |x, y| (w - 1 - x, y)));
    }
    if p.flip_v {
        let src = owned.as_ref().unwrap_or(img);
        owned = Some(remap_exact(src, w, h, |x, y| (x, h - 1 - y)));
    }
    match p.quarter_turns % 4 {
        1 => {
            let src = owned.as_ref().unwrap_or(img);
            owned = Some(remap_exact(src, h, w, |x, y| (y, h - 1 - x)));
        }
        2 => {
            let src = owned.as_ref().unwrap_or(img);
            owned = Some(remap_exact(src, w, h, |x, y| (w - 1 - x, h - 1 - y)));
        }
        3 => {
            let src = owned.as_ref().unwrap_or(img);
            owned = Some(remap_exact(src, h, w, |x, y| (w - 1 - y, x)));
        }
        _ => {}
    }
    let base: &ImageBuf = owned.as_ref().unwrap_or(img);
    debug_assert_eq!((base.width(), base.height()), (pl.w1, pl.h1));

    let (ow, oh) = (pl.x1 - pl.x0, pl.y1 - pl.y0);
    let angle = p.fine_angle();
    if angle == 0.0 {
        return remap_exact(base, ow, oh, |x, y| (x + pl.x0, y + pl.y0));
    }

    // Fine rotation: output pixel centre offset from the rotated result's centre is
    // rotated back into the source (clockwise content rotation by `angle`).
    let (sin, cos) = angle.to_radians().sin_cos();
    let (bw, bh) = (pl.w1 as usize, pl.h1 as usize);
    let (scx, scy) = (pl.w1 as f32 * 0.5, pl.h1 as f32 * 0.5);
    let (rcx, rcy) = (pl.rw as f32 * 0.5, pl.rh as f32 * 0.5);
    let data = base.data();
    let mut out = vec![0.0f32; ow as usize * oh as usize * 4];
    out.par_chunks_exact_mut(ow as usize * 4).enumerate().for_each(|(y, row)| {
        let py = (y as u32 + pl.y0) as f32 + 0.5 - rcy;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let pxo = (x as u32 + pl.x0) as f32 + 0.5 - rcx;
            let qx = cos * pxo + sin * py;
            let qy = -sin * pxo + cos * py;
            px.copy_from_slice(&bilinear(data, bw, bh, qx + scx - 0.5, qy + scy - 0.5));
        }
    });
    ImageBuf::from_rgba(ow, oh, out).expect("size matches")
}

/// Manual lens correction with a radial model. `r` is the distance from the image centre
/// normalised to the half-diagonal (corners at 1).
///
/// - Distortion: the output pixel at radius `r` takes its colour from the source at
///   `r_src = s * r * (1 + distortion * r^2)`, `distortion` clamped to -0.3..1. Negative
///   values pull edge content outwards (stretch the edges): this **corrects barrel
///   distortion**; positive values compress the edges (adds barrel, corrects pincushion).
///   `s = 1 / (1 + distortion)` for positive values zooms in just enough that no empty
///   border appears; for negative values the source stays inside the image (`s = 1`).
/// - Vignetting: the pixel is brightened by `vignetting_ev * r^2` stops (full value in the
///   corners, so a positive value corrects a darkening lens).
/// - Chromatic aberration: the red / blue channel image is magnified about the centre by
///   `1 + ca_red / 1000` / `1 + ca_blue / 1000` relative to green.
///
/// Sampling is bilinear and edge-clamped. All parameters 0 returns a clone.
pub fn lens_manual(img: &ImageBuf, distortion: f32, vignetting_ev: f32, ca_red: f32, ca_blue: f32) -> ImageBuf {
    let dist = finite_or(distortion, 0.0).clamp(-0.3, 1.0);
    let vig = finite_or(vignetting_ev, 0.0);
    let (ca_r, ca_b) = (finite_or(ca_red, 0.0), finite_or(ca_blue, 0.0));
    if (dist == 0.0 && vig == 0.0 && ca_r == 0.0 && ca_b == 0.0) || img.width() == 0 || img.height() == 0 {
        return img.clone();
    }
    let (w, h) = (img.width() as usize, img.height() as usize);
    let (hw, hh) = (w as f32 * 0.5, h as f32 * 0.5);
    let inv_r2 = 1.0 / (hw * hw + hh * hh);
    let zoom = if dist > 0.0 { 1.0 / (1.0 + dist) } else { 1.0 };
    let inv_mag_r = 1.0 / (1.0 + ca_r / 1000.0);
    let inv_mag_b = 1.0 / (1.0 + ca_b / 1000.0);
    let resample = dist != 0.0 || ca_r != 0.0 || ca_b != 0.0;
    let data = img.data();
    let mut out = if resample { vec![0.0f32; w * h * 4] } else { data.to_vec() };
    out.par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        let dy = y as f32 + 0.5 - hh;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let dx = x as f32 + 0.5 - hw;
            let r2 = (dx * dx + dy * dy) * inv_r2;
            if resample {
                let f = zoom * (1.0 + dist * r2);
                let at = |k: f32| bilinear(data, w, h, dx * f * k + hw - 0.5, dy * f * k + hh - 0.5);
                if ca_r == 0.0 && ca_b == 0.0 {
                    px.copy_from_slice(&at(1.0));
                } else {
                    let g = at(1.0);
                    px[0] = if ca_r == 0.0 { g[0] } else { at(inv_mag_r)[0] };
                    px[1] = g[1];
                    px[2] = if ca_b == 0.0 { g[2] } else { at(inv_mag_b)[2] };
                    px[3] = g[3];
                }
            }
            if vig != 0.0 {
                let gain = (vig * r2).exp2();
                px[0] *= gain;
                px[1] *= gain;
                px[2] *= gain;
            }
        }
    });
    ImageBuf::from_rgba(img.width(), img.height(), out).expect("size matches")
}
