//! Global refinement of the per-frame rotations and the shared focal length (Levenberg-Marquardt on
//! the reprojection error of the inlier correspondences of ALL connected pairs, not only of the
//! spanning-tree edges). Used for the cylindrical and spherical projections.

use rayon::prelude::*;

use super::align::{Pair, FOCAL_MAX_REL, FOCAL_MIN_REL};
use super::ransac::{mul, solve_dense, transpose, Mat3};

/// Rotation matrix of a rotation vector (Rodrigues).
pub(super) fn rodrigues(w: [f64; 3]) -> Mat3 {
    let th = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
    let (a, b) = if th < 1e-9 { (1.0 - th * th / 6.0, 0.5 - th * th / 24.0) } else { (th.sin() / th, (1.0 - th.cos()) / (th * th)) };
    let k = [[0.0, -w[2], w[1]], [w[2], 0.0, -w[0]], [-w[1], w[0], 0.0]];
    let kk = mul(&k, &k);
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = (if i == j { 1.0 } else { 0.0 }) + a * k[i][j] + b * kk[i][j];
        }
    }
    r
}

fn rot_vec(r: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [
        r[0][0] * v[0] + r[0][1] * v[1] + r[0][2] * v[2],
        r[1][0] * v[0] + r[1][1] * v[1] + r[1][2] * v[2],
        r[2][0] * v[0] + r[2][1] * v[1] + r[2][2] * v[2],
    ]
}

/// Position in frame `to` of the ray through pixel `p` of frame `from` (rotations are
/// reference <- frame; `f_*` focal lengths in pixels). `None` behind the camera.
fn transfer(r_from: &Mat3, r_to_t: &Mat3, f_from: f64, f_to: f64, p: [f64; 2]) -> Option<[f64; 2]> {
    let ray = rot_vec(r_from, [p[0] / f_from, p[1] / f_from, 1.0]);
    let v = rot_vec(r_to_t, ray);
    if v[2] < 1e-3 {
        return None;
    }
    Some([f_to * v[0] / v[2], f_to * v[1] / v[2]])
}

/// Both-direction reprojection residual of one correspondence (4 values; 1e3 when invalid).
fn residual(ri: &Mat3, rj: &Mat3, fi: f64, fj: f64, a: [f64; 2], b: [f64; 2]) -> [f64; 4] {
    let fwd = transfer(ri, &transpose(rj), fi, fj, a).map_or([1e3, 1e3], |q| [q[0] - b[0], q[1] - b[1]]);
    let bwd = transfer(rj, &transpose(ri), fj, fi, b).map_or([1e3, 1e3], |q| [q[0] - a[0], q[1] - a[1]]);
    [fwd[0], fwd[1], bwd[0], bwd[1]]
}

/// Result of the refinement.
pub(super) struct Refined {
    pub rots: Vec<Mat3>,
    /// Shared focal length in units of the long side of each frame.
    pub f_rel: f64,
    pub rms_before: f64,
    pub rms_after: f64,
}

struct Problem<'a> {
    pairs: &'a [Pair],
    long: &'a [f64],
    /// Parameter slot of a frame (`None` = fixed reference / non-member).
    slot: Vec<Option<usize>>,
    fix_focal: bool,
    dim: usize,
}

fn huber_cost(r: &[f64; 4], delta: f64) -> f64 {
    let mut c = 0.0;
    for k in 0..2 {
        let e = r[2 * k].hypot(r[2 * k + 1]);
        c += if e <= delta { 0.5 * e * e } else { delta * (e - 0.5 * delta) };
    }
    c
}

impl Problem<'_> {
    fn cost(&self, rots: &[Mat3], f_rel: f64) -> f64 {
        self.pairs
            .par_iter()
            .map(|p| {
                let (fi, fj) = (f_rel * self.long[p.i], f_rel * self.long[p.j]);
                let delta = (0.5 * p.thr_px).max(1e-6);
                p.corr.iter().map(|&(a, b)| huber_cost(&residual(&rots[p.i], &rots[p.j], fi, fj, a, b), delta)).sum::<f64>()
            })
            .sum()
    }

    fn rms(&self, rots: &[Mat3], f_rel: f64) -> f64 {
        let (s, n) = self
            .pairs
            .par_iter()
            .map(|p| {
                let (fi, fj) = (f_rel * self.long[p.i], f_rel * self.long[p.j]);
                let mut s = 0.0;
                for &(a, b) in &p.corr {
                    let r = residual(&rots[p.i], &rots[p.j], fi, fj, a, b);
                    s += r.iter().map(|v| v * v).sum::<f64>();
                }
                (s, 4.0 * p.corr.len() as f64)
            })
            .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        if n > 0.0 {
            (s / n).sqrt()
        } else {
            0.0
        }
    }

    /// Normal equations (`dim x (dim + 1)`, last column = right-hand side) at the current estimate.
    fn normal(&self, rots: &[Mat3], f_rel: f64) -> Vec<Vec<f64>> {
        let dim = self.dim;
        let fslot = if self.fix_focal { None } else { Some(dim - 1) };
        self.pairs
            .par_iter()
            .map(|p| {
                let mut a = vec![vec![0.0f64; dim + 1]; dim];
                let (si, sj) = (self.slot[p.i].map(|s| 3 * s), self.slot[p.j].map(|s| 3 * s));
                let delta = (0.5 * p.thr_px).max(1e-6);
                let eval = |di: [f64; 3], dj: [f64; 3], df: f64, a_: [f64; 2], b_: [f64; 2]| {
                    let ri = mul(&rodrigues(di), &rots[p.i]);
                    let rj = mul(&rodrigues(dj), &rots[p.j]);
                    let f = f_rel * df.exp();
                    residual(&ri, &rj, f * self.long[p.i], f * self.long[p.j], a_, b_)
                };
                const EPS: f64 = 1e-6;
                let cols: [Option<usize>; 7] = [si, si.map(|s| s + 1), si.map(|s| s + 2), sj, sj.map(|s| s + 1), sj.map(|s| s + 2), fslot];
                for &(ca, cb) in &p.corr {
                    let r0 = eval([0.0; 3], [0.0; 3], 0.0, ca, cb);
                    let mut jac = [[0.0f64; 4]; 7];
                    for c in 0..7 {
                        if cols[c].is_none() {
                            continue;
                        }
                        let (mut di, mut dj, mut df) = ([0.0; 3], [0.0; 3], 0.0);
                        match c {
                            0..=2 => di[c] = EPS,
                            3..=5 => dj[c - 3] = EPS,
                            _ => df = EPS,
                        }
                        let r1 = eval(di, dj, df, ca, cb);
                        for k in 0..4 {
                            jac[c][k] = (r1[k] - r0[k]) / EPS;
                        }
                    }
                    // Two residual pairs (forward, backward), each with its own Huber weight.
                    for half in 0..2 {
                        let (ru, rv) = (r0[2 * half], r0[2 * half + 1]);
                        let e = ru.hypot(rv);
                        let wt = if e <= delta { 1.0 } else { delta / e };
                        for x in 0..7 {
                            let Some(cx) = cols[x] else { continue };
                            let (jxu, jxv) = (jac[x][2 * half], jac[x][2 * half + 1]);
                            for y in 0..7 {
                                let Some(cy) = cols[y] else { continue };
                                a[cx][cy] += wt * (jxu * jac[y][2 * half] + jxv * jac[y][2 * half + 1]);
                            }
                            a[cx][dim] -= wt * (jxu * ru + jxv * rv);
                        }
                    }
                }
                a
            })
            .reduce(
                || vec![vec![0.0f64; dim + 1]; dim],
                |mut x, y| {
                    for (rx, ry) in x.iter_mut().zip(&y) {
                        for (vx, vy) in rx.iter_mut().zip(ry) {
                            *vx += vy;
                        }
                    }
                    x
                },
            )
    }
}

/// Everything the refinement needs.
pub(super) struct BundleInput<'a> {
    pub pairs: &'a [Pair],
    /// Long side of each frame in pixels.
    pub long: &'a [f64],
    pub members: &'a [usize],
    pub reference: usize,
    /// Initial rotations (reference <- frame).
    pub rots: &'a [Mat3],
    pub f_rel: f64,
    pub fix_focal: bool,
    /// Relative range the focal length may move around the start value (`Some(0.1)` = +-10 %);
    /// `None` = the whole search range. Ignored when `fix_focal`.
    pub focal_tol: Option<f64>,
}

/// Refines the rotations of the members (the reference stays fixed) and, unless `fix_focal`, the
/// shared relative focal length. `None` when there is nothing to refine, the problem is singular,
/// the run was cancelled, or the result is not better than the start.
pub(super) fn refine_rotations(inp: &BundleInput, cancel: &(dyn Fn() -> bool + Sync)) -> Option<Refined> {
    let n = inp.rots.len();
    let mut slot: Vec<Option<usize>> = vec![None; n];
    let mut next = 0;
    for &m in inp.members {
        if m != inp.reference && m < n {
            slot[m] = Some(next);
            next += 1;
        }
    }
    let dim = 3 * next + usize::from(!inp.fix_focal);
    if dim == 0 || inp.pairs.iter().all(|p| p.corr.is_empty()) {
        return None;
    }
    let fix_focal = inp.fix_focal;
    let prob = Problem { pairs: inp.pairs, long: inp.long, slot, fix_focal, dim };
    let mut rots = inp.rots.to_vec();
    // A given focal length (metadata or typed) may lie outside the range an estimate searches (long
    // lenses): its own band counts, not the estimate range, and a fixed one is taken as it is.
    let (f_lo, f_hi) = match inp.focal_tol {
        Some(t) if t.is_finite() && t > 0.0 => (inp.f_rel * (1.0 - t), inp.f_rel * (1.0 + t)),
        _ if fix_focal => (inp.f_rel, inp.f_rel),
        _ => (FOCAL_MIN_REL, FOCAL_MAX_REL),
    };
    let (f_lo, f_hi) = (f_lo.min(f_hi), f_hi.max(f_lo));
    let mut f_rel = if fix_focal { inp.f_rel } else { inp.f_rel.clamp(f_lo, f_hi) };
    let rms_before = prob.rms(&rots, f_rel);
    let mut cur = prob.cost(&rots, f_rel);
    let cost_before = cur;
    let mut lambda = 1e-3;
    for _ in 0..40 {
        if cancel() {
            return None;
        }
        let a = prob.normal(&rots, f_rel);
        let mut accepted = false;
        let mut small_step = false;
        for _ in 0..10 {
            let mut aa = a.clone();
            for i in 0..dim {
                aa[i][i] += lambda * (a[i][i] + 1e-9);
            }
            let Some(step) = solve_dense(&mut aa) else {
                lambda *= 10.0;
                continue;
            };
            let mut nr = rots.clone();
            for k in 0..n {
                if let Some(s) = prob.slot[k] {
                    nr[k] = mul(&rodrigues([step[3 * s], step[3 * s + 1], step[3 * s + 2]]), &rots[k]);
                }
            }
            let nf = if fix_focal { f_rel } else { (f_rel * step[dim - 1].clamp(-0.5, 0.5).exp()).clamp(f_lo, f_hi) };
            let c = prob.cost(&nr, nf);
            if c < cur {
                small_step = step.iter().fold(0.0f64, |m, v| m.max(v.abs())) < 1e-8 || (cur - c) < 1e-10 * cur.max(1e-30);
                rots = nr;
                f_rel = nf;
                cur = c;
                lambda = (lambda * 0.3).max(1e-9);
                accepted = true;
                break;
            }
            lambda *= 10.0;
        }
        if !accepted || small_step {
            break;
        }
    }
    let rms_after = prob.rms(&rots, f_rel);
    // The robust (Huber) cost is what the iteration minimises, so it decides; the plain RMS may rise
    // when gross outliers lose their weight.
    if !rms_after.is_finite() || !cur.is_finite() || cur > cost_before {
        return None;
    }
    Some(Refined { rots, f_rel, rms_before, rms_after })
}

/// Eigen-decomposition of a symmetric 3x3 matrix (cyclic Jacobi). Returns eigenvalues ascending and
/// the matching unit eigenvectors.
fn eigen_sym3(m: [[f64; 3]; 3]) -> ([f64; 3], [[f64; 3]; 3]) {
    let mut a = m;
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..50 {
        let off = a[0][1].powi(2) + a[0][2].powi(2) + a[1][2].powi(2);
        if off < 1e-30 {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if a[p][q].abs() < 1e-300 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let t = if theta == 0.0 { 1.0 } else { t };
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for k in 0..3 {
                let (akp, akq) = (a[k][p], a[k][q]);
                a[k][p] = c * akp - s * akq;
                a[k][q] = s * akp + c * akq;
            }
            for k in 0..3 {
                let (apk, aqk) = (a[p][k], a[q][k]);
                a[p][k] = c * apk - s * aqk;
                a[q][k] = s * apk + c * aqk;
            }
            for k in 0..3 {
                let (vkp, vkq) = (v[k][p], v[k][q]);
                v[k][p] = c * vkp - s * vkq;
                v[k][q] = s * vkp + c * vkq;
            }
        }
    }
    let mut idx = [0usize, 1, 2];
    idx.sort_by(|&x, &y| a[x][x].total_cmp(&a[y][y]));
    let vals = [a[idx[0]][idx[0]], a[idx[1]][idx[1]], a[idx[2]][idx[2]]];
    let vecs = [0, 1, 2].map(|r| [v[0][idx[r]], v[1][idx[r]], v[2][idx[r]]]);
    (vals, vecs)
}

/// Horizon straightening (wave correction, Brown and Lowe): the up vector is the direction that is
/// most nearly perpendicular to the x axes of all cameras. Returns the rotation `W` (rows = new x, y,
/// z axes in reference coordinates; y points down like the image y axis) that makes the camera x axes
/// lie in the equator plane, keeping the reference camera's viewing direction as the projection
/// centre. `None` when the cameras do not span a plane (e.g. a single column or one frame).
pub(super) fn wave_correction(rots: &[Mat3], members: &[usize]) -> Option<Mat3> {
    if members.len() < 2 {
        return None;
    }
    let mut c = [[0.0f64; 3]; 3];
    let mut ysum = [0.0f64; 3];
    for &k in members {
        let r = &rots[k];
        let x = [r[0][0], r[1][0], r[2][0]];
        let y = [r[0][1], r[1][1], r[2][1]];
        for i in 0..3 {
            ysum[i] += y[i];
            for j in 0..3 {
                c[i][j] += x[i] * x[j];
            }
        }
    }
    let (vals, vecs) = eigen_sym3(c);
    let total = vals[0] + vals[1] + vals[2];
    // The camera x axes must spread over a plane, not along one direction.
    if total <= 0.0 || vals[1] < 0.02 * total {
        return None;
    }
    let mut u = vecs[0];
    if u[0] * ysum[0] + u[1] * ysum[1] + u[2] * ysum[2] < 0.0 {
        u = [-u[0], -u[1], -u[2]];
    }
    // Projection centre: the reference viewing direction made horizontal.
    let zr = [0.0, 0.0, 1.0];
    let dotz = u[2];
    let mut zw = [zr[0] - dotz * u[0], zr[1] - dotz * u[1], zr[2] - dotz * u[2]];
    let nz = (zw[0] * zw[0] + zw[1] * zw[1] + zw[2] * zw[2]).sqrt();
    if nz < 0.2 {
        return None;
    }
    for v in zw.iter_mut() {
        *v /= nz;
    }
    let xw = [u[1] * zw[2] - u[2] * zw[1], u[2] * zw[0] - u[0] * zw[2], u[0] * zw[1] - u[1] * zw[0]];
    Some([xw, u, zw])
}
