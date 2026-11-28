//! Brute-force Hamming matching with ratio test and cross check.

use rayon::prelude::*;

use super::features::Keypoint;

#[inline]
fn hamming(a: &[u64; 4], b: &[u64; 4]) -> u32 {
    (a[0] ^ b[0]).count_ones() + (a[1] ^ b[1]).count_ones() + (a[2] ^ b[2]).count_ones() + (a[3] ^ b[3]).count_ones()
}

/// For each descriptor of `a`: (index of best in `b`, best distance, second best distance).
fn best_two(a: &[Keypoint], b: &[Keypoint]) -> Vec<(usize, u32, u32)> {
    a.par_iter()
        .map(|ka| {
            let (mut bi, mut b1, mut b2) = (usize::MAX, u32::MAX, u32::MAX);
            for (j, kb) in b.iter().enumerate() {
                let d = hamming(&ka.desc, &kb.desc);
                if d < b1 {
                    b2 = b1;
                    b1 = d;
                    bi = j;
                } else if d < b2 {
                    b2 = d;
                }
            }
            (bi, b1, b2)
        })
        .collect()
}

/// Mutual best matches that also pass the ratio test `best < ratio * second` (a -> b).
pub(super) fn match_descriptors(a: &[Keypoint], b: &[Keypoint], ratio: f32) -> Vec<(usize, usize)> {
    if a.is_empty() || b.len() < 2 {
        return Vec::new();
    }
    let ab = best_two(a, b);
    let ba = best_two(b, a);
    let mut out = Vec::new();
    for (i, &(j, d1, d2)) in ab.iter().enumerate() {
        if j == usize::MAX || d2 == u32::MAX {
            continue;
        }
        if (d1 as f32) < ratio * d2 as f32 && ba[j].0 == i {
            out.push((i, j));
        }
    }
    out
}
