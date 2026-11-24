//! Lens profile corrections with DNG 1.4 semantics: WarpRectilinear and FixVignetteRadial.

use rawgraph_core::{ImageBuf, VignetteRadial, WarpRectilinear};
use rayon::prelude::*;

use crate::util::transparent;

/// Replaces non-finite values.
#[inline]
fn fin(x: f64, fallback: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        fallback
    }
}

/// Centre in pixels and normalisation radius (distance from the centre to the farthest corner).
fn geometry(center: [f64; 2], w: usize, h: usize) -> (f64, f64, f64) {
    let cx = fin(center[0], 0.5) * w as f64;
    let cy = fin(center[1], 0.5) * h as f64;
    let m = (cx.max(w as f64 - cx)).hypot(cy.max(h as f64 - cy));
    (cx, cy, if m > 0.0 { m } else { 1.0 })
}

/// Catmull-Rom weights for the taps at -1, 0, 1, 2 with fraction `f`.
#[inline]
fn cr_weights(f: f32) -> [f32; 4] {
    let (f2, f3) = (f * f, f * f * f);
    [
        0.5 * (-f3 + 2.0 * f2 - f),
        0.5 * (3.0 * f3 - 5.0 * f2 + 2.0),
        0.5 * (-3.0 * f3 + 4.0 * f2 + f),
        0.5 * (f3 - f2),
    ]
}

/// Bicubic (Catmull-Rom) sample of channels `c0..c1` at continuous pixel-index coordinates
/// (pixel centres at integers), edge-clamped. The result is limited to the range of the four
/// nearest pixels, so it neither rings nor goes negative from non-negative input.
#[inline]
fn sample(data: &[f32], w: usize, h: usize, x: f32, y: f32, c0: usize, c1: usize) -> [f32; 4] {
    let (xf, yf) = (x.floor(), y.floor());
    let (fx, fy) = (x - xf, y - yf);
    let (wx, wy) = (cr_weights(fx), cr_weights(fy));
    let (xi, yi) = (xf as i64, yf as i64);
    let ix: [usize; 4] = std::array::from_fn(|i| (xi - 1 + i as i64).clamp(0, w as i64 - 1) as usize);
    let iy: [usize; 4] = std::array::from_fn(|j| (yi - 1 + j as i64).clamp(0, h as i64 - 1) as usize);
    let mut out = [0.0f32; 4];
    for c in c0..c1 {
        let mut acc = 0.0f32;
        for j in 0..4 {
            let base = iy[j] * w;
            let mut r = 0.0f32;
            for i in 0..4 {
                r += wx[i] * data[(base + ix[i]) * 4 + c];
            }
            acc += wy[j] * r;
        }
        let at = |i: usize, j: usize| data[(iy[j] * w + ix[i]) * 4 + c];
        let (a, b, cc, d) = (at(1, 1), at(2, 1), at(1, 2), at(2, 2));
        let lo = a.min(b).min(cc).min(d);
        let hi = a.max(b).max(cc).max(d);
        out[c] = acc.clamp(lo, hi);
    }
    out
}

/// Geometric lens correction as in DNG 1.4 `WarpRectilinear`.
///
/// Coordinates are normalised to the image centre `warp.center` (fractions of width / height,
/// `[0.5, 0.5]` = middle): `(dx, dy) = (pixel - centre) / m` where `m` is the distance from the
/// centre to the farthest image corner, so `r = 1` at that corner. For every output
/// (destination) pixel the source position is
///
/// ```text
/// r2 = dx^2 + dy^2
/// f  = kr0 + kr1 r2 + kr2 r2^2 + kr3 r2^3
/// dx' = f dx + 2 kt0 dx dy + kt1 (r2 + 2 dx^2)
/// dy' = f dy + kt0 (r2 + 2 dy^2) + 2 kt1 dx dy
/// source = centre + m (dx', dy')
/// ```
///
/// with coefficients `[kr0, kr1, kr2, kr3, kt0, kt1]` of a plane. One plane applies to all
/// channels; three planes are used for R, G and B (lateral chromatic aberration); alpha follows
/// the single plane or the green plane. Sampling is bicubic (Catmull-Rom, limited to the range
/// of the four nearest pixels). Source positions outside the image give black with alpha 0
/// (a half pixel wide soft edge keeps the border smooth); the output has the size of the input.
///
/// `kr0 = 1` with all other coefficients 0 is the identity (a clone of the input). Fewer than one
/// plane, or a plane count other than 1 / 3 (other counts use the first plane), and non-finite
/// coefficients (counted as the identity value) never panic.
pub fn warp_rectilinear(img: &ImageBuf, warp: &WarpRectilinear) -> ImageBuf {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w == 0 || h == 0 || img.data().len() < w * h * 4 || warp.planes.is_empty() {
        return img.clone();
    }
    let clean = |p: &[f64; 6]| -> [f64; 6] {
        [fin(p[0], 1.0), fin(p[1], 0.0), fin(p[2], 0.0), fin(p[3], 0.0), fin(p[4], 0.0), fin(p[5], 0.0)]
    };
    let identity = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let planes: Vec<[f64; 6]> = if warp.planes.len() == 3 {
        warp.planes.iter().map(clean).collect()
    } else {
        vec![clean(&warp.planes[0])]
    };
    if planes.iter().all(|p| *p == identity) {
        return img.clone();
    }
    let (cx, cy, m) = geometry(warp.center, w, h);
    let three = planes.len() == 3;
    let src = img.data();
    let mut out = transparent(img.width(), img.height());
    out.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        let dy = (y as f64 + 0.5 - cy) / m;
        for (x, o) in row.chunks_exact_mut(4).enumerate() {
            let dx = (x as f64 + 0.5 - cx) / m;
            let r2 = dx * dx + dy * dy;
            let map = |k: &[f64; 6]| -> (f64, f64) {
                let f = k[0] + r2 * (k[1] + r2 * (k[2] + r2 * k[3]));
                let sx = f * dx + 2.0 * k[4] * dx * dy + k[5] * (r2 + 2.0 * dx * dx);
                let sy = f * dy + k[4] * (r2 + 2.0 * dy * dy) + 2.0 * k[5] * dx * dy;
                (cx + m * sx, cy + m * sy)
            };
            // Coverage of one source position (continuous edge coordinates).
            let cover = |(px, py): (f64, f64)| -> f32 {
                let ex = px.min(w as f64 - px) + 0.5;
                let ey = py.min(h as f64 - py) + 0.5;
                if ex.is_nan() || ey.is_nan() {
                    0.0
                } else {
                    ex.min(ey).clamp(0.0, 1.0) as f32
                }
            };
            if three {
                let mut v = [0.0f32; 4];
                let mut cov_g = 0.0f32;
                for (c, k) in planes.iter().enumerate() {
                    let p = map(k);
                    let cv = cover(p);
                    if c == 1 {
                        cov_g = cv;
                    }
                    if cv > 0.0 {
                        let s = sample(src, w, h, p.0 as f32 - 0.5, p.1 as f32 - 0.5, c, c + 1);
                        v[c] = s[c];
                        if c == 1 {
                            let a = sample(src, w, h, p.0 as f32 - 0.5, p.1 as f32 - 0.5, 3, 4);
                            v[3] = a[3];
                        }
                    }
                }
                if cov_g > 0.0 {
                    o[0] = v[0];
                    o[1] = v[1];
                    o[2] = v[2];
                    o[3] = v[3] * cov_g;
                }
            } else {
                let p = map(&planes[0]);
                let cv = cover(p);
                if cv > 0.0 {
                    let s = sample(src, w, h, p.0 as f32 - 0.5, p.1 as f32 - 0.5, 0, 4);
                    o[0] = s[0];
                    o[1] = s[1];
                    o[2] = s[2];
                    o[3] = s[3] * cv;
                }
            }
        }
    });
    out
}

/// Radial vignette correction as in DNG 1.4 `FixVignetteRadial`.
///
/// With the normalised radius `r` of [`warp_rectilinear`] (1 = farthest corner from
/// `v.center`, pixel centres), RGB is multiplied by the gain
/// `1 + k0 r^2 + k1 r^4 + k2 r^6 + k3 r^8 + k4 r^10`, so the centre keeps gain 1 and the farthest
/// corner gets `1 + k0 + k1 + k2 + k3 + k4`. Alpha is untouched, the gain never goes below 0,
/// non-finite coefficients count as 0 and all-zero coefficients are the bit-exact identity.
pub fn vignette_radial(img: &mut ImageBuf, v: &VignetteRadial) {
    let k: [f64; 5] = v.k.map(|x| fin(x, 0.0));
    let (w, h) = (img.width() as usize, img.height() as usize);
    if k == [0.0; 5] || w == 0 || h == 0 {
        return;
    }
    let (cx, cy, m) = geometry(v.center, w, h);
    img.data_mut().par_chunks_exact_mut(w * 4).enumerate().for_each(|(y, row)| {
        let dy = (y as f64 + 0.5 - cy) / m;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let dx = (x as f64 + 0.5 - cx) / m;
            let r2 = dx * dx + dy * dy;
            let g = 1.0 + r2 * (k[0] + r2 * (k[1] + r2 * (k[2] + r2 * (k[3] + r2 * k[4]))));
            let g = g.max(0.0) as f32;
            if g.is_finite() {
                px[0] *= g;
                px[1] *= g;
                px[2] *= g;
            }
        }
    });
}
