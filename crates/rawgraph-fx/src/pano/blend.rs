//! Float planes, Gaussian/Laplacian pyramids and the tile blenders (multiband and feather).

/// Multi-channel float plane (interleaved).
#[derive(Clone)]
pub(super) struct Plane {
    pub w: usize,
    pub h: usize,
    pub c: usize,
    pub d: Vec<f32>,
}

impl Plane {
    pub fn new(w: usize, h: usize, c: usize) -> Self {
        Self { w, h, c, d: vec![0.0; w * h * c] }
    }
}

const K5: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];

/// 5-tap binomial blur (replicated border) without decimation.
pub(super) fn blur5(p: &Plane) -> Plane {
    let (w, h, c) = (p.w, p.h, p.c);
    let mut t = Plane::new(w, h, c);
    for y in 0..h {
        for x in 0..w {
            for ch in 0..c {
                let mut acc = 0.0;
                for (k, wt) in K5.iter().enumerate() {
                    let xi = (x as isize + k as isize - 2).clamp(0, w as isize - 1) as usize;
                    acc += wt * p.d[(y * w + xi) * c + ch];
                }
                t.d[(y * w + x) * c + ch] = acc;
            }
        }
    }
    let mut o = Plane::new(w, h, c);
    for y in 0..h {
        for x in 0..w {
            for ch in 0..c {
                let mut acc = 0.0;
                for (k, wt) in K5.iter().enumerate() {
                    let yi = (y as isize + k as isize - 2).clamp(0, h as isize - 1) as usize;
                    acc += wt * t.d[(yi * w + x) * c + ch];
                }
                o.d[(y * w + x) * c + ch] = acc;
            }
        }
    }
    o
}

/// Blur + decimation by two (coarse pixel `k` sits on fine pixel `2k`).
pub(super) fn reduce(p: &Plane) -> Plane {
    let (w, h, c) = (p.w, p.h, p.c);
    let (w2, h2) = (w.div_ceil(2), h.div_ceil(2));
    let mut t = Plane::new(w2, h, c);
    for y in 0..h {
        for x2 in 0..w2 {
            let cx = 2 * x2 as isize;
            for ch in 0..c {
                let mut acc = 0.0;
                for (k, wt) in K5.iter().enumerate() {
                    let xi = (cx + k as isize - 2).clamp(0, w as isize - 1) as usize;
                    acc += wt * p.d[(y * w + xi) * c + ch];
                }
                t.d[(y * w2 + x2) * c + ch] = acc;
            }
        }
    }
    let mut o = Plane::new(w2, h2, c);
    for y2 in 0..h2 {
        let cy = 2 * y2 as isize;
        for x in 0..w2 {
            for ch in 0..c {
                let mut acc = 0.0;
                for (k, wt) in K5.iter().enumerate() {
                    let yi = (cy + k as isize - 2).clamp(0, h as isize - 1) as usize;
                    acc += wt * t.d[(yi * w2 + x) * c + ch];
                }
                o.d[(y2 * w2 + x) * c + ch] = acc;
            }
        }
    }
    o
}

/// Bilinear upsampling to `w x h` (fine pixel `x` samples coarse position `x / 2`).
pub(super) fn expand(p: &Plane, w: usize, h: usize) -> Plane {
    let c = p.c;
    let mut t = Plane::new(w, p.h, c);
    for y in 0..p.h {
        for x in 0..w {
            let (a, b) = (x / 2, if x % 2 == 0 { x / 2 } else { (x / 2 + 1).min(p.w - 1) });
            let (a, b) = (a.min(p.w - 1), b.min(p.w - 1));
            for ch in 0..c {
                t.d[(y * w + x) * c + ch] = 0.5 * (p.d[(y * p.w + a) * c + ch] + p.d[(y * p.w + b) * c + ch]);
            }
        }
    }
    let mut o = Plane::new(w, h, c);
    for y in 0..h {
        let (a, b) = (y / 2, if y % 2 == 0 { y / 2 } else { (y / 2 + 1).min(p.h - 1) });
        let (a, b) = (a.min(p.h - 1), b.min(p.h - 1));
        for x in 0..w {
            for ch in 0..c {
                o.d[(y * w + x) * c + ch] = 0.5 * (t.d[(a * w + x) * c + ch] + t.d[(b * w + x) * c + ch]);
            }
        }
    }
    o
}

/// One frame rendered into a tile: colour (RGB, gain applied) and distance weight (0 = no coverage).
pub(super) struct TileInput {
    pub color: Vec<f32>,
    pub weight: Vec<f32>,
}

/// Plain distance-weighted average. Returns RGB and the coverage flags.
pub(super) fn blend_feather(inputs: &[TileInput], w: usize, h: usize) -> (Vec<f32>, Vec<bool>) {
    let mut rgb = vec![0.0f32; w * h * 3];
    let mut cover = vec![false; w * h];
    for p in 0..w * h {
        let (mut sw, mut acc) = (0.0f32, [0.0f32; 3]);
        for inp in inputs {
            let wt = inp.weight[p];
            if wt > 0.0 {
                sw += wt;
                for (ch, a) in acc.iter_mut().enumerate() {
                    *a += wt * inp.color[p * 3 + ch];
                }
            }
        }
        if sw > 0.0 {
            cover[p] = true;
            for ch in 0..3 {
                rgb[p * 3 + ch] = acc[ch] / sw;
            }
        }
    }
    (rgb, cover)
}

fn level_dims(w: usize, h: usize, levels: usize) -> Vec<(usize, usize)> {
    let mut v = vec![(w, h)];
    for _ in 0..levels {
        let (pw, ph) = *v.last().unwrap_or(&(w, h));
        v.push((pw.div_ceil(2), ph.div_ceil(2)));
    }
    v
}

/// Multiband (Laplacian pyramid) blend: every pixel is assigned to the frame with the largest
/// distance weight, the assignment masks are blurred per band.
pub(super) fn blend_multiband(inputs: &[TileInput], w: usize, h: usize, levels: usize) -> (Vec<f32>, Vec<bool>) {
    let mut winner = vec![usize::MAX; w * h];
    let mut best = vec![0.0f32; w * h];
    for (i, inp) in inputs.iter().enumerate() {
        for p in 0..w * h {
            if inp.weight[p] > best[p] {
                best[p] = inp.weight[p];
                winner[p] = i;
            }
        }
    }
    let cover: Vec<bool> = winner.iter().map(|&x| x != usize::MAX).collect();
    // As deep as the tile allows.
    let mut levels = levels;
    while levels > 0 && (w >> levels < 4 || h >> levels < 4) {
        levels -= 1;
    }
    let dims = level_dims(w, h, levels);
    let mut acc_col: Vec<Plane> = dims.iter().map(|&(lw, lh)| Plane::new(lw, lh, 3)).collect();
    let mut acc_w: Vec<Plane> = dims.iter().map(|&(lw, lh)| Plane::new(lw, lh, 1)).collect();
    for (i, inp) in inputs.iter().enumerate() {
        let mut mask = Plane::new(w, h, 1);
        let mut any = false;
        for p in 0..w * h {
            if winner[p] == i {
                mask.d[p] = 1.0;
                any = true;
            }
        }
        if !any {
            continue;
        }
        let mut gc = vec![Plane { w, h, c: 3, d: inp.color.clone() }];
        let mut gm = vec![mask];
        for l in 0..levels {
            let (nc, nm) = (reduce(&gc[l]), reduce(&gm[l]));
            gc.push(nc);
            gm.push(nm);
        }
        for l in 0..=levels {
            let band: Vec<f32> = if l == levels {
                gc[l].d.clone()
            } else {
                let up = expand(&gc[l + 1], dims[l].0, dims[l].1);
                gc[l].d.iter().zip(&up.d).map(|(a, b)| a - b).collect()
            };
            let m = &gm[l].d;
            for (p, mv) in m.iter().enumerate() {
                if *mv != 0.0 {
                    for ch in 0..3 {
                        acc_col[l].d[p * 3 + ch] += band[p * 3 + ch] * mv;
                    }
                    acc_w[l].d[p] += mv;
                }
            }
        }
    }
    // Normalise each band and collapse.
    for l in 0..=levels {
        for p in 0..dims[l].0 * dims[l].1 {
            let wt = acc_w[l].d[p];
            for ch in 0..3 {
                let v = &mut acc_col[l].d[p * 3 + ch];
                *v = if wt > 1e-6 { *v / wt } else { 0.0 };
            }
        }
    }
    let mut cur = acc_col.pop().unwrap_or_else(|| Plane::new(w, h, 3));
    while let Some(mut band) = acc_col.pop() {
        let up = expand(&cur, band.w, band.h);
        for (b, u) in band.d.iter_mut().zip(&up.d) {
            *b += u;
        }
        cur = band;
    }
    let mut rgb = cur.d;
    // The Laplacian recombination can ring below zero; light is never negative.
    for v in rgb.iter_mut() {
        *v = v.max(0.0);
    }
    for (p, cv) in cover.iter().enumerate() {
        if !cv {
            rgb[p * 3..p * 3 + 3].fill(0.0);
        }
    }
    (rgb, cover)
}
