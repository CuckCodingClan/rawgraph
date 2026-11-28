//! Provisional SDR display transform.
//!
//! This is **not** an ACES Output Transform. It is a deliberately simple,
//! provisional SDR view transform: exposure gain, ACEScg -> linear Rec.709
//! (Bradford-adapted, via `rawgraph_core::color::acescg_to_rec709`), an optional
//! highlight roll-off (`SoftHighlights`) or an approximation of the ACES 1.x
//! rendering (`AcesFit`, not a certified ACES Output Transform) and the sRGB
//! OETF. ACES 2.0, a certified ACES Output Transform and HDR output are later
//! work.
//!
//! The resulting [`DisplayImage`] is intentionally *not* clamped so that scopes
//! can show values above 1.0 and below 0.0.
//!
//! # Output colour spaces
//!
//! [`DisplayTransform::apply_in_space`] and
//! [`DisplayTransform::apply_with_rgba8_in_space`] encode for one of the
//! [`OutputSpace`]s (sRGB, Display P3, Adobe RGB, Rec. 2020; all D65). `Clip` and
//! `SoftHighlights` convert ACEScg to the linear space (`acescg_to_output`), apply
//! the tone curve per channel and encode with the space's transfer function. `AcesFit`
//! keeps its maths up to the linear Rec.709 result (clamped to 0..1), converts that with
//! `rec709_to_output` and encodes; its result therefore always lies inside the Rec.709
//! gamut, in every space (the fit has no wide-gamut rendering). Neutral greys come out
//! identical in all spaces up to the transfer curve. Extreme values behave as in sRGB:
//! NaN counts as 0, huge and infinite neutral values come out white after clamping,
//! negative neutral values come out black.

use rawgraph_core::color::{
    acescg_to_output, acescg_to_rec709, invert, mul, mul_vec, rec709_to_output, srgb_eotf,
    srgb_oetf, to_f32, OutputSpace, Transfer,
};
use rawgraph_core::ImageBuf;
use rayon::prelude::*;
use std::sync::OnceLock;

/// Highlight handling of the display transform.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToneMap {
    /// No change: values stay unbounded (`to_rgba8` clamps).
    Clip,
    /// Extended Reinhard per channel in linear light, white point 8.0,
    /// negatives set to 0.
    SoftHighlights,
    /// Stephen Hill's fit of the ACES 1.x RRT + sRGB ODT (input matrix, rational
    /// curve per channel, output matrix, clamp to 0..1). Negatives into the
    /// curve are treated as 0.
    ///
    /// This is an approximation of the ACES 1.x reference rendering for SDR sRGB
    /// displays, NOT a certified ACES Output Transform. ACES 2.0 and HDR output
    /// transforms are later work.
    AcesFit,
}

/// Inverse of the zero-exposure sRGB viewer. Construct once per image.
///
/// Encoded components are clamped to 0..1 (NaN becomes zero). For ACES fit,
/// saturated colours outside the fit's invertible gamut are projected by
/// clamping the intermediate curve values; their round trip can change hue.
pub struct InverseDisplayTransform {
    tone_map: ToneMap,
    input_inverse: [[f64; 3]; 3],
    output_inverse: [[f64; 3]; 3],
}

impl InverseDisplayTransform {
    pub fn new(tone_map: ToneMap) -> Self {
        let input = if tone_map == ToneMap::AcesFit {
            mul(&ACES_FIT_IN, &acescg_to_rec709())
        } else {
            acescg_to_rec709()
        };
        let output = ACES_FIT_OUT.map(|row| row.map(f64::from));
        Self { tone_map, input_inverse: invert(&input), output_inverse: invert(&output) }
    }

    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let linear = rgb.map(|v| f64::from(srgb_eotf(if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) })));
        let values = if self.tone_map == ToneMap::AcesFit {
            mul_vec(&self.output_inverse, linear)
        } else { linear };
        let scene = values.map(|y| match self.tone_map {
            ToneMap::Clip => y,
            ToneMap::SoftHighlights => {
                // y = x(1+x/64)/(1+x): x² + (64-64y)x - 64y = 0.
                let b = 64.0 * (1.0 - y);
                128.0 * y / ((b * b + 256.0 * y).sqrt() + b)
            }
            ToneMap::AcesFit => {
                // y = (x²+a*x-b)/(c*x²+d*x+e).
                // Positive root of (1-c*y)x² + (a-d*y)x - (b+e*y)=0.
                let y = y.clamp(0.0, 1.0);
                let a = 1.0 - 0.983729 * y;
                let b = 0.0245786 - 0.4329510 * y;
                let c = 0.000090537 + 0.238081 * y;
                (2.0 * c) / ((b * b + 4.0 * a * c).sqrt() + b)
            }
        });
        mul_vec(&self.input_inverse, scene).map(|v| v as f32)
    }
}

/// Exposure + tone map applied before the sRGB encoding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DisplayTransform {
    pub exposure_ev: f32,
    pub tone_map: ToneMap,
}

impl Default for DisplayTransform {
    fn default() -> Self {
        Self {
            exposure_ev: 0.0,
            tone_map: ToneMap::Clip,
        }
    }
}

/// Display-referred image: sRGB-encoded R'G'B', 3 floats per pixel, nominal
/// range 0..1. NOT clamped, so scopes can show values above 1.0 and below 0.0.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplayImage {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<f32>,
}

/// Extended Reinhard with white point 8.0 (linear light), negatives -> 0.
fn soft_highlights(x: f32) -> f32 {
    // NaN -> 0; the ceiling keeps `x * x / 64` finite (soft(1e15) is far above 1).
    let x = if x.is_nan() {
        0.0
    } else {
        x.clamp(0.0, SOFT_CEILING)
    };
    x * (1.0 + x / 64.0) / (1.0 + x)
}

/// Largest value fed into the soft-highlight curve; its square would overflow `f32` otherwise.
const SOFT_CEILING: f32 = 1.0e15;

/// Largest magnitude of a (gained) input channel. Infinite channels become +-INPUT_CEILING, so the
/// matrix products cannot form `inf - inf = NaN`; 1e30 is far above anything visible (white is 1).
const INPUT_CEILING: f32 = 1.0e30;

/// ACES fit input matrix (applied to linear Rec.709).
const ACES_FIT_IN: [[f64; 3]; 3] = [
    [0.59719, 0.35458, 0.04823],
    [0.07600, 0.90834, 0.01566],
    [0.02840, 0.13383, 0.83777],
];
/// ACES fit output matrix.
const ACES_FIT_OUT: [[f32; 3]; 3] = [
    [1.60475, -0.53108, -0.07367],
    [-0.10208, 1.10813, -0.00605],
    [-0.00327, -0.07276, 1.07602],
];

/// Per-channel rational curve of the ACES fit; negatives and NaN count as 0.
#[inline]
#[allow(clippy::excessive_precision)] // published constants of the fit, kept verbatim
fn aces_fit_curve(v: f32) -> f32 {
    let v = if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0e15)
    };
    (v * (v + 0.0245786) - 0.000090537) / (v * (0.983729 * v + 0.4329510) + 0.238081)
}

const LUT_SIZE: usize = 4096;

/// sRGB OETF sampled uniformly over 0..1 (`LUT_SIZE + 1` entries).
fn lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..=LUT_SIZE)
            .map(|i| srgb_oetf(i as f32 / LUT_SIZE as f32))
            .collect()
    })
}

/// Fast sRGB encoding: linear interpolation in a table inside 0..1 (absolute
/// error <= 1e-4), the exact function everywhere else (including NaN).
#[inline]
pub(crate) fn srgb_encode_fast(lut: &[f32], x: f32) -> f32 {
    if (0.0..1.0).contains(&x) {
        let t = x * LUT_SIZE as f32;
        let i = t as usize;
        let f = t - i as f32;
        lut[i] + (lut[i + 1] - lut[i]) * f
    } else {
        srgb_oetf(x)
    }
}

#[inline]
fn quantize(v: f32) -> u8 {
    let v = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
    (v * 255.0).round() as u8
}

/// Transform one row of RGBA ACEScg into R'G'B'. `encode` does the sRGB encoding.
#[inline]
fn row_kernel(
    dst: &mut [f32],
    src: &[f32],
    m: &[[f32; 3]; 3],
    gain: f32,
    tone: ToneMap,
    post: Option<&[[f32; 3]; 3]>,
    encode: impl Fn(f32) -> f32,
) {
    for (d, s) in dst
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .zip(src.as_chunks::<{ ImageBuf::CHANNELS }>().0)
    {
        let scale = |v: f32| {
            let t = v * gain;
            if t.is_nan() {
                0.0
            } else {
                t.clamp(-INPUT_CEILING, INPUT_CEILING)
            }
        };
        let (r, g, b) = (scale(s[0]), scale(s[1]), scale(s[2]));
        if tone == ToneMap::AcesFit {
            // `m` is the combined (input matrix * ACEScg->Rec.709) matrix here.
            let f = |c: usize| aces_fit_curve(m[c][0] * r + m[c][1] * g + m[c][2] * b);
            let (fr, fg, fb) = (f(0), f(1), f(2));
            let mut o = [0.0f32; 3];
            for c in 0..3 {
                let v = ACES_FIT_OUT[c][0] * fr + ACES_FIT_OUT[c][1] * fg + ACES_FIT_OUT[c][2] * fb;
                o[c] = v.clamp(0.0, 1.0);
            }
            if let Some(p) = post {
                // Linear Rec.709 (inside the gamut) -> linear output space.
                let (a, b, c) = (o[0], o[1], o[2]);
                for k in 0..3 {
                    o[k] = (p[k][0] * a + p[k][1] * b + p[k][2] * c).clamp(0.0, 1.0);
                }
            }
            for c in 0..3 {
                d[c] = encode(o[c]);
            }
            continue;
        }
        for c in 0..3 {
            let lin = m[c][0] * r + m[c][1] * g + m[c][2] * b;
            let lin = match tone {
                ToneMap::Clip => lin,
                ToneMap::SoftHighlights => soft_highlights(lin),
                ToneMap::AcesFit => lin, // handled before this match
            };
            d[c] = encode(lin);
        }
    }
}

impl DisplayTransform {
    fn setup(&self) -> ([[f32; 3]; 3], f32) {
        let m709 = acescg_to_rec709();
        let m = if self.tone_map == ToneMap::AcesFit {
            // Fold the ACES fit input matrix into the colour matrix once per call.
            to_f32(&mul(&ACES_FIT_IN, &m709))
        } else {
            to_f32(&m709)
        };
        (m, 2f32.powf(self.exposure_ev))
    }

    /// Convert a scene-referred ACEScg image to display-encoded R'G'B'. Alpha is dropped.
    pub fn apply(&self, img: &ImageBuf) -> DisplayImage {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut rgb = vec![0.0f32; w * h * 3];
        if w == 0 || h == 0 {
            return DisplayImage {
                width: img.width(),
                height: img.height(),
                rgb,
            };
        }
        let (m, gain) = self.setup();
        let (tone, lut) = (self.tone_map, lut());
        rgb.par_chunks_mut(w * 3)
            .zip(img.data().par_chunks(w * ImageBuf::CHANNELS))
            .for_each(|(dst, src)| {
                row_kernel(dst, src, &m, gain, tone, None, |x| srgb_encode_fast(lut, x));
            });
        DisplayImage {
            width: img.width(),
            height: img.height(),
            rgb,
        }
    }

    /// Reference path with the exact per-pixel `powf` encoding (comparison only).
    #[cfg(test)]
    pub(crate) fn apply_exact(&self, img: &ImageBuf) -> DisplayImage {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut rgb = vec![0.0f32; w * h * 3];
        let (m, gain) = self.setup();
        let tone = self.tone_map;
        rgb.par_chunks_mut(w * 3)
            .zip(img.data().par_chunks(w * ImageBuf::CHANNELS))
            .for_each(|(dst, src)| {
                row_kernel(dst, src, &m, gain, tone, None, srgb_oetf);
            });
        DisplayImage {
            width: img.width(),
            height: img.height(),
            rgb,
        }
    }

    /// One pass producing both the display image and its 8-bit RGBA form;
    /// identical to `apply` followed by `to_rgba8`.
    pub fn apply_with_rgba8(&self, img: &ImageBuf) -> (DisplayImage, Vec<u8>) {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut rgb = vec![0.0f32; w * h * 3];
        let mut rgba = vec![255u8; w * h * 4];
        if w == 0 || h == 0 {
            return (
                DisplayImage {
                    width: img.width(),
                    height: img.height(),
                    rgb,
                },
                rgba,
            );
        }
        let (m, gain) = self.setup();
        let (tone, lut) = (self.tone_map, lut());
        rgb.par_chunks_mut(w * 3)
            .zip(rgba.par_chunks_mut(w * 4))
            .zip(img.data().par_chunks(w * ImageBuf::CHANNELS))
            .for_each(|((dst, out), src)| {
                row_kernel(dst, src, &m, gain, tone, None, |x| srgb_encode_fast(lut, x));
                for (o, d) in out
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(dst.as_chunks::<3>().0)
                {
                    o[0] = quantize(d[0]);
                    o[1] = quantize(d[1]);
                    o[2] = quantize(d[2]);
                }
            });
        (
            DisplayImage {
                width: img.width(),
                height: img.height(),
                rgb,
            },
            rgba,
        )
    }
}

/// Matrices of the transform into a non-sRGB space: the colour matrix the kernel applies
/// first (for `AcesFit` with the fit's input matrix folded in) and, for `AcesFit`, the
/// linear Rec.709 -> `space` matrix applied after the fit.
fn space_setup(tone: ToneMap, space: OutputSpace) -> ([[f32; 3]; 3], Option<[[f32; 3]; 3]>) {
    if tone == ToneMap::AcesFit {
        let m = to_f32(&mul(&ACES_FIT_IN, &acescg_to_rec709()));
        (m, Some(to_f32(&rec709_to_output(space))))
    } else {
        (to_f32(&acescg_to_output(space)), None)
    }
}

impl DisplayTransform {
    /// Like [`DisplayTransform::apply`], encoded for `space`: the values are R'G'B' of that
    /// space (see the module documentation). `OutputSpace::Srgb` is bit-identical to `apply`.
    /// Display P3 uses the same table-driven sRGB curve as `apply`; Adobe RGB (gamma
    /// 563/256) and Rec. 2020 (gamma 2.4) are encoded exactly per value.
    pub fn apply_in_space(&self, img: &ImageBuf, space: OutputSpace) -> DisplayImage {
        if space == OutputSpace::Srgb {
            return self.apply(img);
        }
        self.apply_space_impl(img, space, false).0
    }

    /// Like [`DisplayTransform::apply_with_rgba8`], encoded for `space`; the 8-bit form is
    /// identical to `apply_in_space` followed by `to_rgba8`. `OutputSpace::Srgb` is
    /// bit-identical to `apply_with_rgba8`.
    pub fn apply_with_rgba8_in_space(
        &self,
        img: &ImageBuf,
        space: OutputSpace,
    ) -> (DisplayImage, Vec<u8>) {
        if space == OutputSpace::Srgb {
            return self.apply_with_rgba8(img);
        }
        self.apply_space_impl(img, space, true)
    }

    fn apply_space_impl(
        &self,
        img: &ImageBuf,
        space: OutputSpace,
        want_rgba: bool,
    ) -> (DisplayImage, Vec<u8>) {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut rgb = vec![0.0f32; w * h * 3];
        let mut rgba = if want_rgba {
            vec![255u8; w * h * 4]
        } else {
            Vec::new()
        };
        if w > 0 && h > 0 {
            let (m, post) = space_setup(self.tone_map, space);
            let gain = 2f32.powf(self.exposure_ev);
            let tone = self.tone_map;
            let post = post.as_ref();
            let rows = rgb
                .par_chunks_mut(w * 3)
                .zip(img.data().par_chunks(w * ImageBuf::CHANNELS));
            let to8 = |out: &mut [u8], dst: &[f32]| {
                for (o, d) in out
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(dst.as_chunks::<3>().0)
                {
                    o[0] = quantize(d[0]);
                    o[1] = quantize(d[1]);
                    o[2] = quantize(d[2]);
                }
            };
            // The same table-driven curve as the sRGB path for the sRGB-curve spaces, the exact
            // power law for the gamma spaces.
            match space.transfer() {
                Transfer::Srgb => {
                    let lut = lut();
                    if want_rgba {
                        rows.zip(rgba.par_chunks_mut(w * 4))
                            .for_each(|((dst, src), out)| {
                                row_kernel(dst, src, &m, gain, tone, post, |x| {
                                    srgb_encode_fast(lut, x)
                                });
                                to8(out, dst);
                            });
                    } else {
                        rows.for_each(|(dst, src)| {
                            row_kernel(dst, src, &m, gain, tone, post, |x| {
                                srgb_encode_fast(lut, x)
                            });
                        });
                    }
                }
                Transfer::Gamma(_) => {
                    if want_rgba {
                        rows.zip(rgba.par_chunks_mut(w * 4))
                            .for_each(|((dst, src), out)| {
                                row_kernel(dst, src, &m, gain, tone, post, |x| space.encode(x));
                                to8(out, dst);
                            });
                    } else {
                        rows.for_each(|(dst, src)| {
                            row_kernel(dst, src, &m, gain, tone, post, |x| space.encode(x));
                        });
                    }
                }
            }
        }
        (
            DisplayImage {
                width: img.width(),
                height: img.height(),
                rgb,
            },
            rgba,
        )
    }
}

impl DisplayImage {
    /// 8-bit RGBA: clamp 0..1, `round(x * 255)`, alpha 255.
    pub fn to_rgba8(&self) -> Vec<u8> {
        let mut out = vec![255u8; self.rgb.len() / 3 * 4];
        out.par_chunks_mut(4 * 4096)
            .zip(self.rgb.par_chunks(3 * 4096))
            .for_each(|(dst, src)| {
                for (d, s) in dst
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(src.as_chunks::<3>().0)
                {
                    d[0] = quantize(s[0]);
                    d[1] = quantize(s[1]);
                    d[2] = quantize(s[2]);
                }
            });
        out
    }
}

#[cfg(test)]
pub(crate) fn lut_for_tests() -> &'static [f32] {
    lut()
}

#[cfg(test)]
mod inverse_tests {
    use super::*;

    #[test]
    fn neutral_codes_round_trip_through_actual_view() {
        for tone_map in [ToneMap::Clip, ToneMap::SoftHighlights, ToneMap::AcesFit] {
            let inverse = InverseDisplayTransform::new(tone_map);
            let mut image = ImageBuf::new(1024, 1);
            for (i, pixel) in image.data_mut().as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let t = i as f32 / 1023.0;
                let rgb = inverse.apply([t; 3]);
                pixel[..3].copy_from_slice(&rgb);
                pixel[3] = 1.0;
            }
            let output = DisplayTransform { exposure_ev: 0.0, tone_map }.apply(&image);
            for (i, pixel) in output.rgb.as_chunks::<3>().0.iter().enumerate() {
                let t = i as f32 / 1023.0;
                for value in pixel {
                    assert!((value - t).abs() <= 0.5 / 1023.0, "{tone_map:?} {i}: {value}");
                }
            }
        }
    }
}
