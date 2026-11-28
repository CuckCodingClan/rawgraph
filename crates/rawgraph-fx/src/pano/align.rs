//! Pairwise alignment, connectivity, reference choice, focal length and rotation recovery.

use super::features::Keypoint;
use super::matching::match_descriptors;
use super::ransac::{apply, det, diag, inv, mul, normalized, plausible_homography, ransac_homography, refine_homography, transpose, Mat3, IDENT};

pub(super) const MIN_INLIERS: usize = 30;
pub(super) const MIN_INLIER_RATIO: f64 = 0.3;
const RATIO_TEST: f32 = 0.8;
const RANSAC_ITERS: usize = 2000;
const RANSAC_THRESHOLD_PX: f64 = 3.0;

/// Work-image geometry of a frame: the work image is the full frame box-downscaled by the integer
/// factor `f` (covering `sw*f x sh*f` pixels from the top-left corner; a remainder is cut off).
#[derive(Clone, Copy)]
pub(super) struct Geo {
    /// Integer downscale factor (1 = the work image is the frame).
    pub f: f64,
    pub full_w: f64,
    pub full_h: f64,
}

impl Geo {
    /// Work-image index coordinates (pixel `i` centred at `i`) -> full-resolution coordinates
    /// centred on the frame centre. Exact for sizes not divisible by the factor.
    pub fn centred(&self, x: f32, y: f32) -> [f64; 2] {
        [(x as f64 + 0.5) * self.f - self.full_w * 0.5, (y as f64 + 0.5) * self.f - self.full_h * 0.5]
    }
}

/// Largest number of correspondences kept per pair for the global refinement.
const MAX_KEPT_CORR: usize = 160;

/// A connected frame pair; `h` maps centred full-resolution coordinates of `i` to those of `j`.
#[derive(Clone)]
pub(super) struct Pair {
    pub i: usize,
    pub j: usize,
    pub h: Mat3,
    pub inliers: usize,
    /// Inlier correspondences `(point in i, point in j)` in centred full-resolution coordinates
    /// (evenly thinned to at most `MAX_KEPT_CORR`).
    pub corr: Vec<([f64; 2], [f64; 2])>,
    /// Full-resolution size of the inlier threshold.
    pub thr_px: f64,
}

/// Matches and verifies one pair. `None` when the pair is not connected or its homography is
/// implausible (flipped, extreme scale or perspective).
pub(super) fn align_pair(ka: &[Keypoint], ga: &Geo, kb: &[Keypoint], gb: &Geo, i: usize, j: usize) -> Option<Pair> {
    let m = match_descriptors(ka, kb, RATIO_TEST);
    if m.len() < 12 {
        return None;
    }
    // Everything in full-resolution centred coordinates: the keypoints are sub-pixel, so the fit is
    // not limited by the work resolution.
    let src: Vec<[f64; 2]> = m.iter().map(|&(a, _)| ga.centred(ka[a].x, ka[a].y)).collect();
    let dst: Vec<[f64; 2]> = m.iter().map(|&(_, b)| gb.centred(kb[b].x, kb[b].y)).collect();
    let thr = RANSAC_THRESHOLD_PX * ga.f.max(gb.f);
    let (h0, inl) = ransac_homography(&src, &dst, thr, RANSAC_ITERS, (i as u64) << 32 | j as u64)?;
    let ratio = inl.len() as f64 / m.len() as f64;
    if inl.len() < MIN_INLIERS || ratio < MIN_INLIER_RATIO {
        return None;
    }
    // Robust geometric refinement on the inliers, then re-select the inliers and refine once more.
    let pick = |idx: &[usize]| -> (Vec<[f64; 2]>, Vec<[f64; 2]>) { (idx.iter().map(|&k| src[k]).collect(), idx.iter().map(|&k| dst[k]).collect()) };
    let (s1, d1) = pick(&inl);
    let mut h = refine_homography(&h0, &s1, &d1, 0.5 * thr);
    let mut inl2: Vec<usize> = (0..src.len())
        .filter(|&k| apply(&h, src[k][0], src[k][1]).is_some_and(|p| (p[0] - dst[k][0]).hypot(p[1] - dst[k][1]) < thr))
        .collect();
    if inl2.len() >= MIN_INLIERS {
        let (s2, d2) = pick(&inl2);
        h = refine_homography(&h, &s2, &d2, 0.5 * thr);
    } else {
        inl2 = inl;
    }
    let h = normalized(&h);
    let shared: Vec<[f64; 2]> = inl2.iter().map(|&k| src[k]).collect();
    if !plausible_homography(&h, ga.full_w, ga.full_h, &shared) {
        return None;
    }
    let step = inl2.len().div_ceil(MAX_KEPT_CORR).max(1);
    let corr = inl2.iter().step_by(step).map(|&k| (src[k], dst[k])).collect();
    Some(Pair { i, j, h, inliers: inl2.len(), corr, thr_px: thr })
}

/// Homography `from -> to` out of a pair list (inverting when stored the other way round).
pub(super) fn pair_h(p: &Pair, from: usize) -> Option<Mat3> {
    if p.i == from {
        Some(p.h)
    } else {
        inv(&p.h).map(|h| normalized(&h))
    }
}

/// Connected components; returns the members of the largest one (ties: the one with the lowest
/// first index) and the frames outside it. Without any connected pair the members are empty and
/// `rest` lists every frame.
pub(super) fn largest_component(n: usize, pairs: &[Pair]) -> (Vec<usize>, Vec<usize>) {
    let mut label: Vec<usize> = (0..n).collect();
    fn find(l: &mut [usize], mut x: usize) -> usize {
        while l[x] != x {
            l[x] = l[l[x]];
            x = l[x];
        }
        x
    }
    for p in pairs {
        let (a, b) = (find(&mut label, p.i), find(&mut label, p.j));
        if a != b {
            label[a.max(b)] = a.min(b);
        }
    }
    let roots: Vec<usize> = (0..n).map(|x| find(&mut label, x)).collect();
    let mut best_root = 0;
    let mut best_count = 0;
    for r in 0..n {
        let cnt = roots.iter().filter(|&&x| x == r).count();
        if cnt > best_count {
            best_count = cnt;
            best_root = r;
        }
    }
    if best_count < 2 {
        // No pair at all: every frame is unconnected.
        return (Vec::new(), (0..n).collect());
    }
    let members: Vec<usize> = (0..n).filter(|&x| roots[x] == best_root).collect();
    let rest: Vec<usize> = (0..n).filter(|&x| roots[x] != best_root).collect();
    (members, rest)
}

/// Spanning tree geometry rooted at the reference frame.
pub(super) struct Tree {
    pub reference: usize,
    /// `(frame, parent, pair index)` in breadth-first order (the reference itself is not listed).
    pub edges: Vec<(usize, usize, usize)>,
}

/// Maximum spanning tree on the inlier counts (Prim); reference = the most central node
/// (smallest eccentricity in hops, then the largest incident inlier sum, then the lowest index).
pub(super) fn build_tree(n: usize, members: &[usize], pairs: &[Pair]) -> Tree {
    let in_set = |x: usize| members.contains(&x);
    let mut tree_edges: Vec<(usize, usize, usize)> = Vec::new();
    let mut joined = vec![false; n];
    if let Some(&first) = members.first() {
        joined[first] = true;
    }
    while tree_edges.len() + 1 < members.len() {
        let mut best: Option<usize> = None;
        for (k, p) in pairs.iter().enumerate() {
            if !in_set(p.i) || !in_set(p.j) || joined[p.i] == joined[p.j] {
                continue;
            }
            if best.is_none_or(|bk| p.inliers > pairs[bk].inliers) {
                best = Some(k);
            }
        }
        let Some(k) = best else { break };
        let p = &pairs[k];
        tree_edges.push((p.i, p.j, k));
        joined[p.i] = true;
        joined[p.j] = true;
    }
    // Adjacency.
    let mut adj: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
    for &(a, b, k) in &tree_edges {
        adj[a].push((b, k));
        adj[b].push((a, k));
    }
    let bfs = |root: usize| -> (Vec<(usize, usize, usize)>, usize) {
        let mut dist = vec![usize::MAX; n];
        dist[root] = 0;
        let mut queue = std::collections::VecDeque::from([root]);
        let mut edges = Vec::new();
        let mut ecc = 0;
        while let Some(x) = queue.pop_front() {
            for &(y, k) in &adj[x] {
                if dist[y] == usize::MAX {
                    dist[y] = dist[x] + 1;
                    ecc = ecc.max(dist[y]);
                    edges.push((y, x, k));
                    queue.push_back(y);
                }
            }
        }
        (edges, ecc)
    };
    let mut reference = members.first().copied().unwrap_or(0);
    let mut best_key = (usize::MAX, 0usize);
    for &m in members {
        let (_, ecc) = bfs(m);
        let strength: usize = adj[m].iter().map(|&(_, k)| pairs[k].inliers).sum();
        if ecc < best_key.0 || (ecc == best_key.0 && strength > best_key.1) {
            best_key = (ecc, strength);
            reference = m;
        }
    }
    let (edges, _) = bfs(reference);
    Tree { reference, edges }
}

/// Scale-normalises a homography-like matrix to determinant 1 (`None` when it flips orientation).
fn unit_det(m: &Mat3) -> Option<Mat3> {
    let d = det(m);
    if !d.is_finite() || d <= 1e-18 {
        return None;
    }
    let s = d.cbrt();
    let mut o = *m;
    for v in o.iter_mut().flatten() {
        *v /= s;
    }
    Some(o)
}

/// Projects a near-rotation matrix onto SO(3) by Newton iteration `R <- (R + R^-T) / 2`.
fn orthonormalise(m: &Mat3) -> Option<Mat3> {
    let mut r = *m;
    for _ in 0..12 {
        let it = transpose(&inv(&r)?);
        for i in 0..3 {
            for j in 0..3 {
                r[i][j] = 0.5 * (r[i][j] + it[i][j]);
            }
        }
    }
    Some(r)
}

/// Search range of the relative focal length (units of the long side).
pub(super) const FOCAL_MIN_REL: f64 = 0.25;
pub(super) const FOCAL_MAX_REL: f64 = 6.0;

/// True when `f` sits within 1.5 % of the end of the search range.
pub(super) fn at_focal_bound(f: f64) -> bool {
    f <= FOCAL_MIN_REL * 1.015 || f >= FOCAL_MAX_REL / 1.015
}

/// Departure of `K^-1 H K` from a rotation, for a relative focal length (units of the long side).
fn rotation_defect(h_rel: &Mat3, f: f64) -> Option<f64> {
    let k = diag(f, f);
    let r = unit_det(&mul(&mul(&inv(&k)?, h_rel), &k))?;
    let rrt = mul(&r, &transpose(&r));
    let mut e = 0.0;
    for i in 0..3 {
        for j in 0..3 {
            e += (rrt[i][j] - if i == j { 1.0 } else { 0.0 }).powi(2);
        }
    }
    Some(e)
}

/// Focal length (relative to the long side) that makes all pair homographies closest to pure
/// rotations (least-squares form of the Szeliski estimate). `None` without usable pairs; the flag is
/// set when the search ran into the boundary of its range.
pub(super) fn estimate_focal_rel(pairs: &[Pair], long: &[f64]) -> Option<(f64, bool)> {
    let edges: Vec<(Mat3, f64)> = pairs
        .iter()
        .filter_map(|p| {
            let hn = mul(&mul(&diag(1.0 / long[p.j], 1.0 / long[p.j]), &p.h), &diag(long[p.i], long[p.i]));
            hn.iter().flatten().all(|v| v.is_finite()).then_some((hn, (p.inliers as f64).sqrt()))
        })
        .collect();
    if edges.is_empty() {
        return None;
    }
    let cost = |f: f64| -> f64 {
        let mut c = 1e-6 * f.ln().powi(2);
        for (h, w) in &edges {
            c += w * rotation_defect(h, f).unwrap_or(10.0);
        }
        c
    };
    // Coarse log scan, then golden-section refinement.
    let (lo, hi) = (FOCAL_MIN_REL.ln(), FOCAL_MAX_REL.ln());
    let steps = 120;
    let mut best = (f64::INFINITY, lo);
    for s in 0..=steps {
        let l = lo + (hi - lo) * s as f64 / steps as f64;
        let c = cost(l.exp());
        if c < best.0 {
            best = (c, l);
        }
    }
    let (mut a, mut b) = (best.1 - (hi - lo) / steps as f64, best.1 + (hi - lo) / steps as f64);
    let g = 0.618_033_988_749_895;
    for _ in 0..40 {
        let (c, d) = (b - g * (b - a), a + g * (b - a));
        if cost(c.exp()) < cost(d.exp()) {
            b = d;
        } else {
            a = c;
        }
    }
    let f = (0.5 * (a + b)).exp();
    let at_bound = at_focal_bound(f);
    f.is_finite().then_some((f, at_bound))
}

/// Rotation reference-ward: `R_{c->p} = K_p^-1 H_{c->p} K_c`, orthonormalised.
pub(super) fn rotation_from_h(h: &Mat3, f_c: f64, f_p: f64) -> Option<Mat3> {
    let m = mul(&mul(&inv(&diag(f_p, f_p))?, h), &diag(f_c, f_c));
    orthonormalise(&unit_det(&m)?)
}

/// Chains `child -> reference` transforms along the tree. `step(child, parent, pair)` returns the
/// child -> parent transform.
pub(super) fn chain<F: Fn(usize, usize, usize) -> Option<Mat3>>(n: usize, tree: &Tree, step: F) -> Option<Vec<Mat3>> {
    let mut out = vec![IDENT; n];
    for &(c, p, k) in &tree.edges {
        let s = step(c, p, k)?;
        out[c] = mul(&out[p], &s);
    }
    Some(out)
}
