//! Painted brush strokes rasterised into a mask.

use rawgraph_core::{BrushStroke, MaskBuf};
use rayon::prelude::*;

use crate::util::{c01, finite_or};

/// Smallest and largest brush radius (fraction of the long edge) that is honoured.
const RADIUS_MIN: f32 = 0.0005;
const RADIUS_MAX: f32 = 0.5;
/// Points further out than this (normalised) are clamped; keeps the arithmetic finite.
const COORD_LIMIT: f32 = 16.0;

/// One segment of a stroke polyline in pixel coordinates (a dab is a zero-length segment).
struct Seg {
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
    /// `1 / |b|^2`, 0 for a zero-length segment.
    inv_len2: f32,
    min_y: f32,
    max_y: f32,
    min_x: f32,
    max_x: f32,
}

/// Rasterises brush strokes, in order, into a mask of the given size.
///
/// Each stroke is a polyline through its points (normalised image coordinates; a single
/// point is a dab). With `R = radius * max(width, height)` pixels (radius clamped to
/// 0.0005..0.5) and `d` the distance of a pixel centre to the polyline, the coverage is
///
/// * `1` for `d <= inner`, `0` for `d >= R`,
/// * `smoothstep` falling from 1 at `inner` to 0 at `R` in between,
///
/// with `inner = min(hardness * R, max(R - 1, 0))`, so even a hard brush (hardness 1) has a
/// one pixel antialiasing ramp and hardness 0 falls off from the centre. Because the
/// distance is taken to the segments (no stamped dabs) the coverage is uniform along the
/// stroke whatever the point spacing. The stroke's coverage `c` is combined with the mask
/// `m` in order: paint `m' = m + (1 - m) * flow * c`, erase `m' = m * (1 - flow * c)`.
/// The result lies in 0..1; `invert` returns `1 - m` at the end. Non-finite points are
/// skipped; hardness and flow are clamped to 0..1. Only each stroke's bounding box is
/// touched.
pub fn stroke_mask(width: u32, height: u32, strokes: &[BrushStroke], invert: bool) -> MaskBuf {
    let mut mask = MaskBuf::new(width, height, 0.0);
    if width > 0 && height > 0 {
        let w = width as usize;
        let long_edge = width.max(height) as f32;
        for stroke in strokes {
            paint_stroke(&mut mask.data, w, height as usize, long_edge, stroke);
        }
    }
    if invert {
        mask.data.par_iter_mut().for_each(|v| *v = 1.0 - c01(*v));
    }
    mask
}

fn paint_stroke(data: &mut [f32], w: usize, h: usize, long_edge: f32, stroke: &BrushStroke) {
    let r = finite_or(stroke.radius, 0.0).clamp(RADIUS_MIN, RADIUS_MAX) * long_edge;
    let hardness = c01(finite_or(stroke.hardness, 1.0));
    let flow = c01(finite_or(stroke.flow, 1.0));
    if flow <= 0.0 {
        return;
    }
    let inner = (hardness * r).min((r - 1.0).max(0.0));
    let inv_fall = 1.0 / (r - inner).max(1e-6);
    let r2 = r * r;

    let pts: Vec<(f32, f32)> = stroke
        .points
        .iter()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .map(|p| (p[0].clamp(-COORD_LIMIT, COORD_LIMIT) * w as f32, p[1].clamp(-COORD_LIMIT, COORD_LIMIT) * h as f32))
        .collect();
    if pts.is_empty() {
        return;
    }
    let make = |a: (f32, f32), b: (f32, f32)| {
        let (bx, by) = (b.0 - a.0, b.1 - a.1);
        let l2 = bx * bx + by * by;
        Seg {
            ax: a.0,
            ay: a.1,
            bx,
            by,
            inv_len2: if l2 > 1e-12 { 1.0 / l2 } else { 0.0 },
            min_y: a.1.min(b.1),
            max_y: a.1.max(b.1),
            min_x: a.0.min(b.0),
            max_x: a.0.max(b.0),
        }
    };
    let segs: Vec<Seg> = if pts.len() == 1 { vec![make(pts[0], pts[0])] } else { pts.windows(2).map(|p| make(p[0], p[1])).collect() };

    let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
    for s in &segs {
        min_y = min_y.min(s.min_y);
        max_y = max_y.max(s.max_y);
    }
    let y0 = (min_y - r - 1.0).floor().max(0.0) as usize;
    let y1 = ((max_y + r + 1.0).ceil().max(0.0) as usize).min(h);
    if y0 >= y1 {
        return;
    }
    let erase = stroke.erase;
    let segs = &segs;

    data[y0 * w..y1 * w].par_chunks_mut(w).enumerate().for_each_init(
        || (Vec::<f32>::new(), Vec::<(usize, usize, usize)>::new()),
        |(buf, active), (i, row)| {
            let yc = (y0 + i) as f32 + 0.5;
            active.clear();
            let (mut lo, mut hi) = (w, 0usize);
            for (si, s) in segs.iter().enumerate() {
                if yc < s.min_y - r || yc > s.max_y + r {
                    continue;
                }
                // x extent of the part of the segment within r of this row, widened by r.
                let (mut xa, mut xb) = (s.min_x, s.max_x);
                if s.by.abs() > 1e-4 {
                    let t0 = ((yc - r - s.ay) / s.by).clamp(0.0, 1.0);
                    let t1 = ((yc + r - s.ay) / s.by).clamp(0.0, 1.0);
                    let (x0, x1) = (s.ax + t0 * s.bx, s.ax + t1 * s.bx);
                    xa = x0.min(x1);
                    xb = x0.max(x1);
                }
                let xa = (xa - r - 1.0).floor().max(0.0) as usize;
                let xb = ((xb + r + 1.0).ceil().max(0.0) as usize).min(w);
                if xa >= xb {
                    continue;
                }
                active.push((si, xa, xb));
                lo = lo.min(xa);
                hi = hi.max(xb);
            }
            if lo >= hi {
                return;
            }
            if buf.len() < w {
                buf.resize(w, f32::INFINITY);
            }
            buf[lo..hi].fill(f32::INFINITY);
            for &(si, xa, xb) in active.iter() {
                let s = &segs[si];
                let py = yc - s.ay;
                let pyb = py * s.by;
                let (ax, bx, by, inv) = (s.ax, s.bx, s.by, s.inv_len2);
                for (k, b) in buf[xa..xb].iter_mut().enumerate() {
                    let px = (xa + k) as f32 + 0.5 - ax;
                    let t = ((px * bx + pyb) * inv).clamp(0.0, 1.0);
                    let dx = px - t * bx;
                    let dy = py - t * by;
                    *b = b.min(dx * dx + dy * dy);
                }
            }
            for (o, &d2) in row[lo..hi].iter_mut().zip(&buf[lo..hi]) {
                if d2 >= r2 {
                    continue;
                }
                let d = d2.sqrt();
                let cov = if d <= inner {
                    1.0
                } else {
                    let s = (r - d) * inv_fall;
                    s * s * (3.0 - 2.0 * s)
                };
                let a = flow * cov;
                *o = if erase { *o * (1.0 - a) } else { *o + (1.0 - *o) * a };
            }
        },
    );
}
