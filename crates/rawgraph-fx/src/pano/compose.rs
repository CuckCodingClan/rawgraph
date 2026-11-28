//! Exposure compensation and the largest valid rectangle.

use rawgraph_core::ImageBuf;

use super::align::Pair;
use super::ransac::solve_dense;
use super::warp::{edge_weight, sample_rgb, Warper};
use super::{PanoError, Rect};
use crate::util::luma;

/// Largest-area axis-aligned rectangle containing only non-zero cells of `mask` (`w x h`).
/// Polls `cancel` every 64 rows.
pub(super) fn largest_rect(mask: &[u8], w: usize, h: usize, cancel: &(dyn Fn() -> bool + Sync)) -> Result<Option<Rect>, PanoError> {
    let mut heights = vec![0u32; w];
    let mut best: Option<(u64, Rect)> = None;
    let mut stack: Vec<(usize, u32)> = Vec::new();
    for y in 0..h {
        if y % 64 == 0 && cancel() {
            return Err(PanoError::Cancelled);
        }
        for x in 0..w {
            heights[x] = if mask[y * w + x] != 0 { heights[x] + 1 } else { 0 };
        }
        stack.clear();
        for x in 0..=w {
            let cur = if x < w { heights[x] } else { 0 };
            let mut start = x;
            while let Some(&(sx, sh)) = stack.last() {
                if sh <= cur {
                    break;
                }
                stack.pop();
                let area = sh as u64 * (x - sx) as u64;
                if best.as_ref().is_none_or(|(a, _)| area > *a) {
                    best = Some((area, Rect { x: sx as u32, y: (y as u32 + 1) - sh, w: (x - sx) as u32, h: sh }));
                }
                start = sx;
            }
            if cur > 0 && stack.last().is_none_or(|&(_, sh)| sh < cur) {
                stack.push((start, cur));
            }
        }
    }
    Ok(best.map(|(_, r)| r))
}

/// Frame data needed for the exposure estimate.
pub(super) struct ExpFrame<'a> {
    pub small: &'a ImageBuf,
    pub sx: f64,
    pub sy: f64,
    pub full_w: f64,
    pub full_h: f64,
    pub warper: &'a Warper,
    /// Canvas bounding box `[x0, y0, x1, y1]`.
    pub bbox: [f64; 4],
}

/// Luminance above which the brightest 1 % of the frame lies (those samples are treated as clipped).
fn top_percent_luma(img: &ImageBuf) -> f32 {
    let mut v: Vec<f32> = img.data().chunks_exact(4).map(|p| luma(p[0], p[1], p[2])).filter(|y| y.is_finite()).collect();
    if v.is_empty() {
        return f32::INFINITY;
    }
    let k = ((v.len() as f64 * 0.99) as usize).min(v.len() - 1);
    *v.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
}

/// Median of a non-empty slice (reorders it).
fn median(v: &mut [f64]) -> f64 {
    let k = v.len() / 2;
    *v.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
}

/// Per-frame gains (reference = 1) from the median ratio of the pair overlaps, solved by least
/// squares over all connected pairs. Samples in the brightest 1 % of either frame are excluded
/// (clipped highlights); with `clip` also every sample having a channel at or above that level
/// (display-referred data clipped at 1.0). Scene-linear data above 1.0 needs no fixed cut.
/// Frames without usable overlap keep gain 1.
pub(super) fn exposure_gains(
    frames: &[ExpFrame],
    pairs: &[Pair],
    members: &[usize],
    reference: usize,
    canvas_long: f64,
    clip: Option<f32>,
) -> Vec<f64> {
    let n = frames.len();
    let mut gains = vec![1.0; n];
    let step = (canvas_long / 800.0).max(1.0);
    let top: Vec<f32> = frames.iter().map(|f| top_percent_luma(f.small)).collect();
    // (i, j, ratio mean_i/mean_j, count)
    let mut eqs: Vec<(usize, usize, f64, f64)> = Vec::new();
    for p in pairs {
        let (a, b) = (&frames[p.i], &frames[p.j]);
        let (x0, y0) = (a.bbox[0].max(b.bbox[0]), a.bbox[1].max(b.bbox[1]));
        let (x1, y1) = (a.bbox[2].min(b.bbox[2]), a.bbox[3].min(b.bbox[3]));
        if x1 - x0 < step * 4.0 || y1 - y0 < step * 4.0 {
            continue;
        }
        let xs: Vec<f64> = (0..((x1 - x0) / step) as usize).map(|t| x0 + (t as f64 + 0.5) * step).collect();
        let ys: Vec<f64> = (0..((y1 - y0) / step) as usize).map(|t| y0 + (t as f64 + 0.5) * step).collect();
        let (pa, pb) = (a.warper.map_grid(&xs, &ys), b.warper.map_grid(&xs, &ys));
        let mut ratios: Vec<f64> = Vec::new();
        let margin_a = 0.03 * (a.full_w.min(a.full_h) as f32);
        let margin_b = 0.03 * (b.full_w.min(b.full_h) as f32);
        for k in 0..pa.len() {
            if edge_weight(pa[k], a.full_w as f32, a.full_h as f32) < margin_a || edge_weight(pb[k], b.full_w as f32, b.full_h as f32) < margin_b {
                continue;
            }
            let ca = sample_rgb(a.small, [pa[k][0] * a.sx as f32, pa[k][1] * a.sy as f32]);
            let cb = sample_rgb(b.small, [pb[k][0] * b.sx as f32, pb[k][1] * b.sy as f32]);
            if let Some(c) = clip {
                if ca.iter().chain(cb.iter()).any(|v| *v >= c) {
                    continue;
                }
            }
            let (ya, yb) = (luma(ca[0], ca[1], ca[2]), luma(cb[0], cb[1], cb[2]));
            if ya >= top[p.i] || yb >= top[p.j] || ya < 1e-4 || yb < 1e-4 {
                continue;
            }
            ratios.push(ya as f64 / yb as f64);
        }
        if ratios.len() >= 200 {
            let cnt = ratios.len() as f64;
            eqs.push((p.i, p.j, median(&mut ratios), cnt));
        }
    }
    let unknowns: Vec<usize> = members.iter().copied().filter(|&m| m != reference).collect();
    if unknowns.is_empty() || eqs.is_empty() {
        return gains;
    }
    let col = |f: usize| unknowns.iter().position(|&u| u == f);
    let m = unknowns.len();
    let mut a = vec![vec![0.0f64; m + 1]; m];
    for &(i, j, q, cnt) in &eqs {
        // g_i * q = g_j (q = median of y_i / y_j), normalised by the mean; a fixed gain (the
        // reference) moves to the right-hand side.
        let s = 0.5 * (q + 1.0);
        let (ci, cj) = (q / s, 1.0 / s);
        let w = cnt;
        let mut row = vec![0.0; m + 1];
        let mut rhs = 0.0;
        match col(i) {
            Some(c) => row[c] += ci,
            None => rhs -= ci,
        }
        match col(j) {
            Some(c) => row[c] -= cj,
            None => rhs += cj,
        }
        for r in 0..m {
            if row[r] == 0.0 {
                continue;
            }
            for c in 0..m {
                a[r][c] += w * row[r] * row[c];
            }
            a[r][m] += w * row[r] * rhs;
        }
    }
    // Weak prior towards 1 keeps frames with no equation well-posed.
    for r in 0..m {
        a[r][r] += 1e-3;
        a[r][m] += 1e-3;
    }
    if let Some(sol) = solve_dense(&mut a) {
        for (k, &u) in unknowns.iter().enumerate() {
            gains[u] = sol[k].clamp(0.1, 10.0);
        }
    }
    gains
}
