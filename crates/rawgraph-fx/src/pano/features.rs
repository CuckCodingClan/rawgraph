//! Corner detection (Harris, 3 octaves) and oriented BRIEF-like 256-bit descriptors on log luminance.

use rawgraph_core::ImageBuf;

use super::blend::{blur5, reduce, Plane};
use crate::util::luma;

/// A keypoint in work-image pixel index coordinates (sub-pixel, centre of pixel `i` = `i`) with its
/// 256-bit descriptor.
#[derive(Clone, Copy, Debug)]
pub(super) struct Keypoint {
    pub x: f32,
    pub y: f32,
    pub desc: [u64; 4],
}

const PATCH_R: f32 = 13.0;
const BORDER: usize = 16;
const OCTAVES: usize = 3;
/// Number of feature buckets along the long side of an image.
const BUCKETS: usize = 8;

/// Log luminance of an image (single channel plane). The log makes exposure differences a constant
/// offset, which the intensity comparisons of the descriptor ignore.
pub(super) fn log_luma_plane(img: &ImageBuf) -> Plane {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut p = Plane::new(w, h, 1);
    for (o, px) in p.d.iter_mut().zip(img.data().chunks_exact(4)) {
        *o = (luma(px[0], px[1], px[2]).max(0.0) + 0.002).ln();
    }
    p
}

fn bilinear(p: &Plane, x: f32, y: f32) -> f32 {
    let x = x.clamp(0.0, (p.w - 1) as f32);
    let y = y.clamp(0.0, (p.h - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(p.w - 1), (y0 + 1).min(p.h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let at = |xx: usize, yy: usize| p.d[yy * p.w + xx];
    let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
    let bot = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
    top * (1.0 - fy) + bot * fy
}

/// Deterministic pattern of 256 point pairs inside a disc of radius `PATCH_R`.
fn pattern() -> Vec<[f32; 4]> {
    let mut s: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut gauss = move |sigma: f64| loop {
        let (u1, u2) = (next().max(1e-12), next());
        let r = (-2.0 * u1.ln()).sqrt() * sigma;
        let (x, y) = (r * (std::f64::consts::TAU * u2).cos(), r * (std::f64::consts::TAU * u2).sin());
        if x.hypot(y) <= PATCH_R as f64 {
            return (x as f32, y as f32);
        }
    };
    (0..256)
        .map(|_| {
            let a = gauss(5.5);
            let b = gauss(5.5);
            [a.0, a.1, b.0, b.1]
        })
        .collect()
}

/// 5x5 box sum with replicated border.
fn box5(p: &Plane) -> Plane {
    let (w, h) = (p.w, p.h);
    let mut t = Plane::new(w, h, 1);
    for y in 0..h {
        for x in 0..w {
            let mut a = 0.0;
            for k in -2..=2isize {
                a += p.d[y * w + (x as isize + k).clamp(0, w as isize - 1) as usize];
            }
            t.d[y * w + x] = a;
        }
    }
    let mut o = Plane::new(w, h, 1);
    for y in 0..h {
        for x in 0..w {
            let mut a = 0.0;
            for k in -2..=2isize {
                a += t.d[(y as isize + k).clamp(0, h as isize - 1) as usize * w + x];
            }
            o.d[y * w + x] = a;
        }
    }
    o
}

/// A corner candidate: integer position, response and the sub-pixel offset of the response peak.
#[derive(Clone, Copy)]
struct Cand {
    x: usize,
    y: usize,
    r: f32,
    fx: f32,
    fy: f32,
}

/// Vertex offset of the parabola through `(-1, l)`, `(0, c)`, `(1, r)`; 0 when it is not a peak.
fn parabola_offset(l: f32, c: f32, r: f32) -> f32 {
    let denom = l - 2.0 * c + r;
    if denom >= -1e-20 {
        return 0.0;
    }
    (0.5 * (l - r) / denom).clamp(-0.5, 0.5)
}

/// Harris corner candidates (local maxima only) with a quadratic sub-pixel refinement of the peak.
fn harris(img: &Plane) -> Vec<Cand> {
    let (w, h) = (img.w, img.h);
    if w < 2 * BORDER + 3 || h < 2 * BORDER + 3 {
        return Vec::new();
    }
    let mut ixx = Plane::new(w, h, 1);
    let mut iyy = Plane::new(w, h, 1);
    let mut ixy = Plane::new(w, h, 1);
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let gx = 0.5 * (img.d[y * w + x + 1] - img.d[y * w + x - 1]);
            let gy = 0.5 * (img.d[(y + 1) * w + x] - img.d[(y - 1) * w + x]);
            ixx.d[y * w + x] = gx * gx;
            iyy.d[y * w + x] = gy * gy;
            ixy.d[y * w + x] = gx * gy;
        }
    }
    let (sxx, syy, sxy) = (box5(&ixx), box5(&iyy), box5(&ixy));
    let mut resp = vec![0.0f32; w * h];
    for i in 0..w * h {
        resp[i] = sxx.d[i] * syy.d[i] - sxy.d[i] * sxy.d[i] - 0.04 * (sxx.d[i] + syy.d[i]).powi(2);
    }
    // Threshold relative to the 99.9th percentile of the positive responses: a few extreme corners
    // (dead pixels, hard edges, the edge of a bright window in a merged HDR frame, whose gradient is
    // 10x that of any texture and its Harris response 10^4 times or more) must not hide all the others;
    // the per-cell cap of `select` keeps such edges from using up the quota.
    let mut pos: Vec<f32> = resp.iter().copied().filter(|v| *v > 0.0 && v.is_finite()).collect();
    if pos.is_empty() {
        return Vec::new();
    }
    let k = (((pos.len() - 1) as f64) * 0.999) as usize;
    let top = *pos.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1;
    let thr = top * 1e-7;
    let mut out = Vec::new();
    for y in BORDER..h - BORDER {
        for x in BORDER..w - BORDER {
            let r = resp[y * w + x];
            if r <= thr {
                continue;
            }
            let mut is_max = true;
            'n: for dy in -1..=1isize {
                for dx in -1..=1isize {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let n = resp[(y as isize + dy) as usize * w + (x as isize + dx) as usize];
                    // Row-major tie break: earlier neighbours must be strictly smaller.
                    let earlier = dy < 0 || (dy == 0 && dx < 0);
                    if n > r || (earlier && n == r) {
                        is_max = false;
                        break 'n;
                    }
                }
            }
            if is_max {
                let fx = parabola_offset(resp[y * w + x - 1], r, resp[y * w + x + 1]);
                let fy = parabola_offset(resp[(y - 1) * w + x], r, resp[(y + 1) * w + x]);
                out.push(Cand { x, y, r, fx, fy });
            }
        }
    }
    out
}

/// Greedy selection by response with a minimum distance (grid hash) and a per-cell cap (spatial
/// bucketing: a textured area cannot use up the whole quota).
fn select(mut cand: Vec<Cand>, quota: usize, min_dist: usize, w: usize, h: usize) -> Vec<Cand> {
    cand.sort_by(|a, b| b.r.total_cmp(&a.r).then(a.y.cmp(&b.y)).then(a.x.cmp(&b.x)));
    let cell = min_dist.max(1);
    let mut grid: std::collections::HashMap<(usize, usize), Vec<(usize, usize)>> = std::collections::HashMap::new();
    // Buckets: about 8 cells along the long side.
    let bucket = (w.max(h) / BUCKETS).max(16);
    let (bw, bh) = (w.div_ceil(bucket), h.div_ceil(bucket));
    let cap = (3 * quota).div_ceil(bw * bh).max(3);
    let mut counts = vec![0usize; bw * bh];
    let mut out = Vec::new();
    let d2 = (min_dist * min_dist) as isize;
    for c in cand {
        if out.len() >= quota {
            break;
        }
        let bi = (c.y / bucket) * bw + c.x / bucket;
        if counts[bi] >= cap {
            continue;
        }
        let (x, y) = (c.x, c.y);
        let (cx, cy) = (x / cell, y / cell);
        let mut ok = true;
        'g: for gy in cy.saturating_sub(1)..=cy + 1 {
            for gx in cx.saturating_sub(1)..=cx + 1 {
                if let Some(v) = grid.get(&(gx, gy)) {
                    for &(px, py) in v {
                        let (dx, dy) = (px as isize - x as isize, py as isize - y as isize);
                        if dx * dx + dy * dy < d2 {
                            ok = false;
                            break 'g;
                        }
                    }
                }
            }
        }
        if ok {
            grid.entry((cx, cy)).or_default().push((x, y));
            counts[bi] += 1;
            out.push(c);
        }
    }
    out
}

/// Detects up to `max_features` keypoints over three octaves.
pub(super) fn detect(base: &Plane, max_features: usize) -> Vec<Keypoint> {
    let pat = pattern();
    let quotas = [max_features / 2, max_features * 3 / 10, max_features / 5];
    let mut out = Vec::new();
    let mut img = base.clone();
    for (o, quota) in quotas.iter().enumerate().take(OCTAVES) {
        if img.w < 2 * BORDER + 4 || img.h < 2 * BORDER + 4 {
            break;
        }
        let smooth = blur5(&blur5(&img));
        let sel = select(harris(&img), *quota, 8, img.w, img.h);
        let scale = (1usize << o) as f32;
        for cand in sel {
            let (x, y) = (cand.x, cand.y);
            // Orientation: contrast-weighted centroid of the disc.
            let (cx, cy) = (x as f32, y as f32);
            let mut vals = Vec::with_capacity(500);
            for dy in -12..=12isize {
                for dx in -12..=12isize {
                    if dx * dx + dy * dy <= 144 {
                        vals.push((dx as f32, dy as f32, smooth.d[(y as isize + dy) as usize * smooth.w + (x as isize + dx) as usize]));
                    }
                }
            }
            let mean = vals.iter().map(|v| v.2).sum::<f32>() / vals.len() as f32;
            let (m10, m01) = vals.iter().fold((0.0f32, 0.0f32), |a, v| (a.0 + (v.2 - mean) * v.0, a.1 + (v.2 - mean) * v.1));
            let ang = if m10.abs() + m01.abs() > 1e-9 { m01.atan2(m10) } else { 0.0 };
            let (s, c) = ang.sin_cos();
            let mut desc = [0u64; 4];
            for (k, p) in pat.iter().enumerate() {
                let a = bilinear(&smooth, cx + c * p[0] - s * p[1], cy + s * p[0] + c * p[1]);
                let b = bilinear(&smooth, cx + c * p[2] - s * p[3], cy + s * p[2] + c * p[3]);
                if a < b {
                    desc[k / 64] |= 1u64 << (k % 64);
                }
            }
            out.push(Keypoint { x: (cx + cand.fx) * scale, y: (cy + cand.fy) * scale, desc });
        }
        img = reduce(&img);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parabola_offset_finds_the_vertex() {
        // y = -(x - 0.3)^2 sampled at -1, 0, 1.
        let f = |x: f32| -(x - 0.3) * (x - 0.3);
        assert!((parabola_offset(f(-1.0), f(0.0), f(1.0)) - 0.3).abs() < 1e-5);
        assert_eq!(parabola_offset(1.0, 1.0, 1.0), 0.0);
        assert!(parabola_offset(0.0, 1.0, 5.0).abs() <= 0.5);
    }

    #[test]
    fn bucketing_caps_features_per_cell() {
        // 3000 candidates crammed into one corner of an 800 x 800 image, plus a few elsewhere.
        let mut cand = Vec::new();
        for i in 0..3000usize {
            cand.push(Cand { x: 20 + (i % 55) * 2, y: 20 + (i / 55) * 2, r: 1000.0 - i as f32 * 0.1, fx: 0.0, fy: 0.0 });
        }
        for k in 0..20usize {
            cand.push(Cand { x: 400 + k * 15, y: 500, r: 1.0 + k as f32, fx: 0.0, fy: 0.0 });
        }
        let sel = select(cand, 750, 2, 800, 800);
        let corner = sel.iter().filter(|c| c.x < 160 && c.y < 160).count();
        let elsewhere = sel.len() - corner;
        // 100 px buckets, cap = ceil(3 * 750 / 64) = 36 per cell: at most 4 cells touched by the corner.
        assert!(corner <= 4 * 36, "corner keeps {corner}");
        assert!(elsewhere >= 15, "others {elsewhere}");
    }
}
