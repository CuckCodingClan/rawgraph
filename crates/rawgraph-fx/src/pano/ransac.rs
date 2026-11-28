//! 3x3 matrix helpers, normalised DLT homography fitting and RANSAC.

pub(super) type Mat3 = [[f64; 3]; 3];

pub(super) const IDENT: Mat3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

pub(super) fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    let mut o = [[0.0; 3]; 3];
    for (i, row) in o.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
        }
    }
    o
}

pub(super) fn transpose(a: &Mat3) -> Mat3 {
    let mut o = [[0.0; 3]; 3];
    for (i, row) in o.iter_mut().enumerate() {
        for (j, v) in row.iter_mut().enumerate() {
            *v = a[j][i];
        }
    }
    o
}

pub(super) fn det(m: &Mat3) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

pub(super) fn inv(m: &Mat3) -> Option<Mat3> {
    let d = det(m);
    if !d.is_finite() || d.abs() < 1e-300 {
        return None;
    }
    let c = |r0: usize, r1: usize, c0: usize, c1: usize| m[r0][c0] * m[r1][c1] - m[r0][c1] * m[r1][c0];
    let o = [
        [c(1, 2, 1, 2), -c(0, 2, 1, 2), c(0, 1, 1, 2)],
        [-c(1, 2, 0, 2), c(0, 2, 0, 2), -c(0, 1, 0, 2)],
        [c(1, 2, 0, 1), -c(0, 2, 0, 1), c(0, 1, 0, 1)],
    ];
    let mut r = [[0.0; 3]; 3];
    for i in 0..3 {
        for j in 0..3 {
            r[i][j] = o[i][j] / d;
        }
    }
    r.iter().flatten().all(|v| v.is_finite()).then_some(r)
}

pub(super) fn diag(a: f64, b: f64) -> Mat3 {
    [[a, 0.0, 0.0], [0.0, b, 0.0], [0.0, 0.0, 1.0]]
}

/// Projective transform of a point; `None` when it maps to infinity.
pub(super) fn apply(h: &Mat3, x: f64, y: f64) -> Option<[f64; 2]> {
    let w = h[2][0] * x + h[2][1] * y + h[2][2];
    if !w.is_finite() || w.abs() < 1e-12 {
        return None;
    }
    let (u, v) = ((h[0][0] * x + h[0][1] * y + h[0][2]) / w, (h[1][0] * x + h[1][1] * y + h[1][2]) / w);
    (u.is_finite() && v.is_finite()).then_some([u, v])
}

/// Divides by `h22` (if usable).
pub(super) fn normalized(h: &Mat3) -> Mat3 {
    let s = h[2][2];
    if s.abs() < 1e-12 {
        return *h;
    }
    let mut o = *h;
    for v in o.iter_mut().flatten() {
        *v /= s;
    }
    o
}

/// Gauss elimination with partial pivoting on an `n x (n+1)` augmented matrix.
pub(super) fn solve_dense(a: &mut [Vec<f64>]) -> Option<Vec<f64>> {
    let n = a.len();
    for col in 0..n {
        let mut piv = col;
        for r in col + 1..n {
            if a[r][col].abs() > a[piv][col].abs() {
                piv = r;
            }
        }
        if a[piv][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, piv);
        for r in col + 1..n {
            let f = a[r][col] / a[col][col];
            if f != 0.0 {
                for c in col..=n {
                    let v = a[col][c];
                    a[r][c] -= f * v;
                }
            }
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        let mut s = a[r][n];
        for c in r + 1..n {
            s -= a[r][c] * x[c];
        }
        x[r] = s / a[r][r];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// Hartley normalisation: translation to the centroid, mean distance sqrt(2).
fn normalise(p: &[[f64; 2]]) -> Option<Mat3> {
    let n = p.len() as f64;
    let (cx, cy) = (p.iter().map(|q| q[0]).sum::<f64>() / n, p.iter().map(|q| q[1]).sum::<f64>() / n);
    let md = p.iter().map(|q| ((q[0] - cx).powi(2) + (q[1] - cy).powi(2)).sqrt()).sum::<f64>() / n;
    if md.is_nan() || md <= 1e-9 {
        return None;
    }
    let s = std::f64::consts::SQRT_2 / md;
    Some([[s, 0.0, -s * cx], [0.0, s, -s * cy], [0.0, 0.0, 1.0]])
}

/// Least-squares homography `dst ~ H src` (normalised DLT with `h22 = 1`); exact for 4 points.
pub(super) fn fit_homography(src: &[[f64; 2]], dst: &[[f64; 2]]) -> Option<Mat3> {
    let n = src.len();
    if n < 4 || dst.len() != n {
        return None;
    }
    let (ts, td) = (normalise(src)?, normalise(dst)?);
    let mut a = vec![vec![0.0f64; 9]; 8];
    for k in 0..n {
        let s = apply(&ts, src[k][0], src[k][1])?;
        let d = apply(&td, dst[k][0], dst[k][1])?;
        let (x, y, u, v) = (s[0], s[1], d[0], d[1]);
        let rows = [([x, y, 1.0, 0.0, 0.0, 0.0, -u * x, -u * y], u), ([0.0, 0.0, 0.0, x, y, 1.0, -v * x, -v * y], v)];
        for (row, b) in rows {
            for i in 0..8 {
                for j in 0..8 {
                    a[i][j] += row[i] * row[j];
                }
                a[i][8] += row[i] * b;
            }
        }
    }
    let h = solve_dense(&mut a)?;
    let hn = [[h[0], h[1], h[2]], [h[3], h[4], h[5]], [h[6], h[7], 1.0]];
    let out = mul(&mul(&inv(&td)?, &hn), &ts);
    let out = normalized(&out);
    out.iter().flatten().all(|v| v.is_finite()).then_some(out)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn collinear(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> bool {
    let cross = (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]);
    let scale = ((b[0] - a[0]).hypot(b[1] - a[1]) * (c[0] - a[0]).hypot(c[1] - a[1])).max(1e-9);
    cross.abs() < 0.02 * scale
}

fn degenerate(p: &[[f64; 2]; 4]) -> bool {
    collinear(p[0], p[1], p[2]) || collinear(p[0], p[1], p[3]) || collinear(p[0], p[2], p[3]) || collinear(p[1], p[2], p[3])
}

fn inliers_of(h: &Mat3, src: &[[f64; 2]], dst: &[[f64; 2]], thr: f64) -> (Vec<usize>, f64) {
    let t2 = thr * thr;
    let mut out = Vec::new();
    let mut err = 0.0;
    for (k, (s, d)) in src.iter().zip(dst).enumerate() {
        if let Some(p) = apply(h, s[0], s[1]) {
            let e = (p[0] - d[0]).powi(2) + (p[1] - d[1]).powi(2);
            if e < t2 {
                out.push(k);
                err += e;
            }
        }
    }
    (out, err)
}

/// RANSAC homography with a final least-squares refit on the inliers.
/// Returns the homography and the inlier indices.
pub(super) fn ransac_homography(
    src: &[[f64; 2]],
    dst: &[[f64; 2]],
    thr: f64,
    max_iter: usize,
    seed: u64,
) -> Option<(Mat3, Vec<usize>)> {
    let n = src.len();
    if n < 4 {
        return None;
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut best: Option<(Mat3, Vec<usize>, f64)> = None;
    let mut need = max_iter;
    let mut it = 0;
    while it < need.min(max_iter) {
        it += 1;
        let mut idx = [0usize; 4];
        let mut k = 0;
        let mut guard = 0;
        while k < 4 && guard < 100 {
            guard += 1;
            let c = (rng.next() % n as u64) as usize;
            if !idx[..k].contains(&c) {
                idx[k] = c;
                k += 1;
            }
        }
        if k < 4 {
            break;
        }
        let s4 = [src[idx[0]], src[idx[1]], src[idx[2]], src[idx[3]]];
        let d4 = [dst[idx[0]], dst[idx[1]], dst[idx[2]], dst[idx[3]]];
        if degenerate(&s4) || degenerate(&d4) {
            continue;
        }
        let Some(h) = fit_homography(&s4, &d4) else { continue };
        let (inl, err) = inliers_of(&h, src, dst, thr);
        let better = match &best {
            None => inl.len() >= 4,
            Some((_, b, be)) => inl.len() > b.len() || (inl.len() == b.len() && err < *be),
        };
        if better {
            let w = inl.len() as f64 / n as f64;
            let denom = (1.0 - w.powi(4)).max(1e-12).ln();
            if denom < -1e-12 {
                need = ((0.01f64).ln() / denom).ceil().clamp(1.0, max_iter as f64) as usize;
            }
            best = Some((h, inl, err));
        }
    }
    let (mut h, mut inl, _) = best?;
    for _ in 0..4 {
        let s: Vec<[f64; 2]> = inl.iter().map(|&i| src[i]).collect();
        let d: Vec<[f64; 2]> = inl.iter().map(|&i| dst[i]).collect();
        let Some(h2) = fit_homography(&s, &d) else { break };
        let (inl2, _) = inliers_of(&h2, src, dst, thr);
        if inl2.len() >= inl.len() {
            let same = inl2.len() == inl.len();
            h = h2;
            inl = inl2;
            if same {
                break;
            }
        } else {
            break;
        }
    }
    Some((h, inl))
}

/// Levenberg-Marquardt refinement of a homography on the (robust, Huber) transfer error, in the
/// coordinates given (use full-resolution coordinates for full-resolution accuracy). `huber` is the
/// residual size (in the same units) where the loss turns linear. Returns `h` unchanged when
/// nothing better is found.
pub(super) fn refine_homography(h: &Mat3, src: &[[f64; 2]], dst: &[[f64; 2]], huber: f64) -> Mat3 {
    let n = src.len();
    let (Some(ts), Some(td)) = (normalise(src), normalise(dst)) else { return *h };
    let (Some(tdi), true) = (inv(&td), n >= 4) else { return *h };
    let hn0 = normalized(&mul(&mul(&td, h), &inv(&ts).unwrap_or(IDENT)));
    if !hn0.iter().flatten().all(|v| v.is_finite()) {
        return *h;
    }
    let s: Vec<[f64; 2]> = src.iter().filter_map(|p| apply(&ts, p[0], p[1])).collect();
    let d: Vec<[f64; 2]> = dst.iter().filter_map(|p| apply(&td, p[0], p[1])).collect();
    if s.len() != n || d.len() != n {
        return *h;
    }
    let delta = (huber * td[0][0]).max(1e-6);
    let to_p = |m: &Mat3| [m[0][0], m[0][1], m[0][2], m[1][0], m[1][1], m[1][2], m[2][0], m[2][1]];
    let from_p = |p: &[f64; 8]| -> Mat3 { [[p[0], p[1], p[2]], [p[3], p[4], p[5]], [p[6], p[7], 1.0]] };
    let cost = |p: &[f64; 8]| -> f64 {
        let m = from_p(p);
        let mut c = 0.0;
        for k in 0..n {
            match apply(&m, s[k][0], s[k][1]) {
                Some(q) => {
                    let r = (q[0] - d[k][0]).hypot(q[1] - d[k][1]);
                    c += if r <= delta { 0.5 * r * r } else { delta * (r - 0.5 * delta) };
                }
                None => c += 1e6,
            }
        }
        c
    };
    let mut p = to_p(&hn0);
    let mut cur = cost(&p);
    let mut lambda = 1e-3;
    for _ in 0..30 {
        let m = from_p(&p);
        let mut a = vec![vec![0.0f64; 9]; 8];
        for k in 0..n {
            let (x, y) = (s[k][0], s[k][1]);
            let w = m[2][0] * x + m[2][1] * y + m[2][2];
            if w.abs() < 1e-12 {
                continue;
            }
            let (u, v) = ((m[0][0] * x + m[0][1] * y + m[0][2]) / w, (m[1][0] * x + m[1][1] * y + m[1][2]) / w);
            let (ru, rv) = (u - d[k][0], v - d[k][1]);
            let r = ru.hypot(rv);
            let wt = if r <= delta { 1.0 } else { delta / r };
            let iw = 1.0 / w;
            let ju = [x * iw, y * iw, iw, 0.0, 0.0, 0.0, -u * x * iw, -u * y * iw];
            let jv = [0.0, 0.0, 0.0, x * iw, y * iw, iw, -v * x * iw, -v * y * iw];
            for i in 0..8 {
                for j in 0..8 {
                    a[i][j] += wt * (ju[i] * ju[j] + jv[i] * jv[j]);
                }
                a[i][8] -= wt * (ju[i] * ru + jv[i] * rv);
            }
        }
        let mut improved = false;
        for _ in 0..8 {
            let mut aa = a.clone();
            for i in 0..8 {
                aa[i][i] += lambda * (a[i][i] + 1e-9);
            }
            let Some(step) = solve_dense(&mut aa) else {
                lambda *= 10.0;
                continue;
            };
            let mut q = p;
            for i in 0..8 {
                q[i] += step[i];
            }
            let c = cost(&q);
            if c < cur {
                let rel = (cur - c) / cur.max(1e-30);
                p = q;
                cur = c;
                lambda = (lambda * 0.3).max(1e-9);
                improved = rel > 1e-9;
                break;
            }
            lambda *= 10.0;
        }
        if !improved {
            break;
        }
    }
    let out = normalized(&mul(&mul(&tdi, &from_p(&p)), &ts));
    if out.iter().flatten().all(|v| v.is_finite()) {
        out
    } else {
        *h
    }
}

/// Checks that a pair homography (centred full-resolution coordinates, `w x h` source frame) is a
/// plausible image-to-image map: orientation preserving, local scale in `[0.5, 2]`, and no
/// perspective term that sends a point of the overlap behind or far in front of the plane.
///
/// The perspective term is checked at the corners of the bounding box of `points` (the inliers:
/// the part of the frame the pair actually shares), not at the frame corners: for a pure rotation
/// with a wide lens the far corner of a frame lies beyond 90 degrees from the other camera's axis
/// (w <= 0 there) although the overlap is perfectly fine. Without points the frame corners count.
/// Smallest bounding box of a pair's inliers, as a fraction of the frame area.
const MIN_INLIER_AREA: f64 = 0.02;

pub(super) fn plausible_homography(h: &Mat3, w: f64, hgt: f64, points: &[[f64; 2]]) -> bool {
    let n = normalized(h);
    if !n.iter().flatten().all(|v| v.is_finite()) {
        return false;
    }
    let d2 = n[0][0] * n[1][1] - n[0][1] * n[1][0];
    if d2 <= 0.0 || det(&n) <= 0.0 {
        return false;
    }
    let scale = d2.sqrt();
    if !(0.5..=2.0).contains(&scale) {
        return false;
    }
    let (x0, x1, y0, y1) = if points.is_empty() {
        (-w * 0.5, w * 0.5, -hgt * 0.5, hgt * 0.5)
    } else {
        points.iter().fold((f64::MAX, f64::MIN, f64::MAX, f64::MIN), |a, p| (a.0.min(p[0]), a.1.max(p[0]), a.2.min(p[1]), a.3.max(p[1])))
    };
    // A false match with clustered inliers (repetitive texture) covers a tiny patch: the shared part
    // of two frames is at least a few percent of a frame.
    if !points.is_empty() && (x1 - x0) * (y1 - y0) < MIN_INLIER_AREA * w * hgt {
        return false;
    }
    let (cx, cy) = (0.5 * (x0 + x1), 0.5 * (y0 + y1));
    for (x, y) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1), (cx, cy)] {
        let wv = n[2][0] * x + n[2][1] * y + n[2][2];
        if !(0.25..=4.0).contains(&wv) {
            return false;
        }
    }
    true
}
