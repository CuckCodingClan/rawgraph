//! Colour lookup tables applied to an image (CPU reference; the GPU op mirrors it).

use rawgraph_core::color::{acescg_to_rec709, rec709_to_acescg, srgb_eotf, srgb_oetf, to_f32};
use rawgraph_core::lut::{acescct_decode, acescct_encode};
use rawgraph_core::{ImageBuf, Lut, LutSpace};
use rayon::prelude::*;

type M3 = [[f32; 3]; 3];

#[inline]
fn mat(m: &M3, v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|c| m[c][0] * v[0] + m[c][1] * v[1] + m[c][2] * v[2])
}

/// A working value made finite: NaN becomes 0, infinities become the largest finite value.
#[inline]
fn finite(v: f32) -> f32 {
    if v.is_nan() { 0.0 } else { v.clamp(-f32::MAX, f32::MAX) }
}

/// Strength is applied in the working space: `rgb + strength * (processed - rgb)`. At full strength
/// the processed value is returned exactly, so strength 1 is the plain LUT and the result moves
/// continuously with the strength (the colour-space round trip is never mixed into the input).
#[inline]
fn blend(rgb: [f32; 3], processed: [f32; 3], strength: f32) -> [f32; 3] {
    if strength >= 1.0 {
        return processed;
    }
    std::array::from_fn(|c| rgb[c] + (processed[c] - rgb[c]) * strength)
}

/// Applies `lut` to the RGB of `img` in `space`. `strength` (0..1) mixes the input and the LUT
/// result in the working space (see [`blend`]). Alpha is untouched. Strength 0 returns at once,
/// bit-identical. Input outside the table domain is clamped to it; NaN counts as 0 and an infinity as
/// the largest finite value.
pub fn lut_apply(img: &mut ImageBuf, lut: &Lut, space: LutSpace, strength: f32) {
    let strength = if strength.is_finite() { strength.clamp(0.0, 1.0) } else { 0.0 };
    if strength == 0.0 || img.width() == 0 || img.height() == 0 {
        return;
    }
    let to709 = to_f32(&acescg_to_rec709());
    let from709 = to_f32(&rec709_to_acescg());
    img.data_mut().par_chunks_exact_mut(4).for_each(|px| {
        let rgb = [finite(px[0]), finite(px[1]), finite(px[2])];
        let processed = match space {
            LutSpace::Linear => lut.sample(rgb),
            LutSpace::Srgb709 => {
                let lin = mat(&to709, rgb);
                let enc = lin.map(|v| srgb_oetf(v.clamp(0.0, 1.0)));
                mat(&from709, lut.sample(enc).map(srgb_eotf))
            }
            LutSpace::AcesCct => lut.sample(rgb.map(acescct_encode)).map(acescct_decode),
        };
        px[..3].copy_from_slice(&blend(rgb, processed, strength));
    });
}

/// The display-wrapped form: `encoded` holds the view-transformed RGB of `img` (3 floats per
/// pixel); the table acts on it and `decode` (the inverse view transform) brings the result back
/// into the working space, where the strength mixes it with the input.
pub fn lut_apply_encoded(img: &mut ImageBuf, encoded: &[f32], lut: &Lut, strength: f32, decode: &(dyn Fn([f32; 3]) -> [f32; 3] + Sync)) {
    let strength = if strength.is_finite() { strength.clamp(0.0, 1.0) } else { 0.0 };
    if strength == 0.0 || encoded.len() != img.data().len() / 4 * 3 {
        return;
    }
    img.data_mut().par_chunks_exact_mut(4).zip(encoded.par_chunks_exact(3)).for_each(|(px, e)| {
        let rgb = [finite(px[0]), finite(px[1]), finite(px[2])];
        let processed = decode(lut.sample([finite(e[0]), finite(e[1]), finite(e[2])]));
        px[..3].copy_from_slice(&blend(rgb, processed, strength));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::lut::parse_cube;

    fn identity(n: usize) -> Lut {
        let mut s = format!("LUT_3D_SIZE {n}\n");
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    let d = (n - 1) as f32;
                    s.push_str(&format!("{} {} {}\n", r as f32 / d, g as f32 / d, b as f32 / d));
                }
            }
        }
        parse_cube(&s).unwrap()
    }

    /// Swaps red and blue: a non-identity cube whose result is easy to state.
    fn swap_rb() -> Lut {
        let mut s = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    s.push_str(&format!("{b} {g} {r}\n"));
                }
            }
        }
        parse_cube(&s).unwrap()
    }

    fn image() -> ImageBuf {
        let mut img = ImageBuf::new(4, 2);
        let px = [[0.0, 0.0, 0.0], [0.18, 0.18, 0.18], [0.5, 0.2, 0.1], [0.9, 0.7, 0.4], [0.02, 0.3, 0.6], [0.05, 0.05, 0.9], [0.3, 0.6, 0.1], [1.0, 1.0, 1.0]];
        for (i, p) in px.iter().enumerate() {
            img.set_pixel(i as u32 % 4, i as u32 / 4, [p[0], p[1], p[2], 0.5]);
        }
        img
    }

    #[test]
    fn identity_cube_is_a_no_op_in_every_space() {
        let lut = identity(17);
        for space in [LutSpace::Linear, LutSpace::Srgb709, LutSpace::AcesCct] {
            let mut src = image();
            if space == LutSpace::Srgb709 {
                // The sRGB mode clamps to the Rec.709 gamut, so the identity holds for colours inside it.
                let m = to_f32(&rec709_to_acescg());
                for (i, c) in [[0.0, 0.0, 0.0], [0.18, 0.18, 0.18], [0.5, 0.2, 0.1], [0.9, 0.7, 0.4], [0.02, 0.3, 0.6], [0.05, 0.05, 0.9], [0.3, 0.6, 0.1], [1.0, 1.0, 1.0]].iter().enumerate() {
                    let v = mat(&m, *c);
                    src.set_pixel(i as u32 % 4, i as u32 / 4, [v[0], v[1], v[2], 0.5]);
                }
            }
            let mut out = src.clone();
            lut_apply(&mut out, &lut, space, 1.0);
            for (a, b) in src.data().iter().zip(out.data()) {
                assert!((a - b).abs() < 1e-5, "{space:?}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn strength_zero_is_the_input_and_half_is_the_mean() {
        let lut = swap_rb();
        let src = image();
        let mut out = src.clone();
        lut_apply(&mut out, &lut, LutSpace::Linear, 0.0);
        assert_eq!(out, src);
        lut_apply(&mut out, &lut, LutSpace::Linear, 0.5);
        let p = src.pixel(2, 0);
        let o = out.pixel(2, 0);
        assert!((o[0] - (p[0] + p[2]) / 2.0).abs() < 1e-6 && (o[2] - (p[0] + p[2]) / 2.0).abs() < 1e-6);
        assert_eq!(o[3], 0.5, "alpha untouched");
    }

    #[test]
    fn strength_is_continuous_in_the_clamping_spaces() {
        // A highlight far above 1: the sRGB mode clips it inside the LUT, but a tiny strength must
        // only move the pixel a tiny bit, and non-finite input stays finite.
        let mut src = ImageBuf::new(3, 1);
        src.set_pixel(0, 0, [4.0, 3.0, 2.0, 1.0]);
        src.set_pixel(1, 0, [f32::INFINITY, f32::NAN, -f32::INFINITY, 1.0]);
        src.set_pixel(2, 0, [0.2, 0.3, 0.4, 1.0]);
        let lut = identity(9);
        for space in [LutSpace::Linear, LutSpace::Srgb709, LutSpace::AcesCct] {
            let mut full = src.clone();
            lut_apply(&mut full, &lut, space, 1.0);
            for strength in [0.001f32, 0.01, 0.5] {
                let mut out = src.clone();
                lut_apply(&mut out, &lut, space, strength);
                let (s, f, o) = (src.pixel(0, 0), full.pixel(0, 0), out.pixel(0, 0));
                for c in 0..3 {
                    let want = s[c] + (f[c] - s[c]) * strength;
                    assert!((o[c] - want).abs() <= 1e-4 * s[c].abs().max(1.0), "{space:?} {strength}: {} vs {want}", o[c]);
                }
                assert!(out.data().iter().all(|v| v.is_finite()), "{space:?} {strength}: {:?}", out.data());
            }
            assert!(full.data().iter().all(|v| v.is_finite()), "{space:?}: {:?}", full.data());
        }
    }

    #[test]
    fn full_strength_is_exactly_the_lut_result() {
        let lut = swap_rb();
        let mut a = image();
        lut_apply(&mut a, &lut, LutSpace::Linear, 1.0);
        for (i, p) in image().data().chunks_exact(4).enumerate() {
            assert_eq!(a.data()[i * 4..i * 4 + 3], lut.sample([p[0], p[1], p[2]]));
        }
    }

    #[test]
    fn swap_cube_swaps_channels_in_linear_space_and_clamps_out_of_domain() {
        let mut src = ImageBuf::new(2, 1);
        src.set_pixel(0, 0, [0.25, 0.5, 0.75, 1.0]);
        src.set_pixel(1, 0, [4.0, -2.0, f32::NAN, 1.0]);
        let mut out = src.clone();
        lut_apply(&mut out, &swap_rb(), LutSpace::Linear, 1.0);
        let a = out.pixel(0, 0);
        assert!((a[0] - 0.75).abs() < 1e-6 && (a[1] - 0.5).abs() < 1e-6 && (a[2] - 0.25).abs() < 1e-6, "{a:?}");
        // r=4 -> clamped to 1 -> blue output 1; g=-2 -> 0; b=NaN -> 0 -> red output 0.
        assert_eq!(out.pixel(1, 0), [0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn srgb_mode_clamps_highlights_and_acescct_keeps_them() {
        let mut src = ImageBuf::new(1, 1);
        src.set_pixel(0, 0, [4.0, 4.0, 4.0, 1.0]);
        let lut = identity(5);
        let mut a = src.clone();
        lut_apply(&mut a, &lut, LutSpace::Srgb709, 1.0);
        assert!((a.pixel(0, 0)[0] - 1.0).abs() < 0.01, "clamped to white: {:?}", a.pixel(0, 0));
        let mut b = src.clone();
        lut_apply(&mut b, &lut, LutSpace::AcesCct, 1.0);
        assert!((b.pixel(0, 0)[0] - 4.0).abs() < 1e-3, "{:?}", b.pixel(0, 0));
    }
}
