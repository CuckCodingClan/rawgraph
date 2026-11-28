//! JPEG / PNG / TIFF input: already-developed images.
//!
//! What is done, in this order:
//! 1. decode with the `image` crate (8- or 16-bit, grey/RGB, with or without alpha);
//! 2. apply the EXIF/TIFF orientation, if the container carries one;
//! 3. interpret the code values as **sRGB** (`srgb_eotf` per colour channel, then the Rec.709 ->
//!    ACEScg matrix `rawgraph_core::color::rec709_to_acescg()`) unless the file carries an ICC
//!    profile that is a matrix/TRC RGB profile other than sRGB: then the code values are decoded
//!    with *that profile's* curves and its primaries (adapted from the D50 profile connection space
//!    to ACES white with Bradford) go to ACEScg. Profiles that cannot be used (LUT-based, CMYK,
//!    grey, broken) are ignored and the file is read as sRGB; `Loaded::profile` says what happened;
//! 4. alpha is passed through unchanged for files that have an alpha channel and set to 1.0
//!    otherwise. 32-bit float TIFFs are taken as already linear Rec.709.
//!
//! Nothing else happens: no tone curve is removed, no white balance is undone — the file keeps
//! its baked-in look and its limited latitude, which is why [`crate::SourceKind::Rendered`]
//! exists.

use std::fs::File;
use std::io::{BufReader, Seek};
use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader};
use rawgraph_core::color::{self, rec709_to_acescg};
use rawgraph_core::{CaptureInfo, ImageBuf};
use rayon::prelude::*;

use crate::icc::{resolve_input, InputTransform};
use crate::{InputProfile, IoError, Loaded, SourceKind};

/// Runs `f`, turning a panic inside a third-party decoder into `IoError::Decode`.
pub(crate) fn guard<T>(what: &str, f: impl FnOnce() -> Result<T, IoError>) -> Result<T, IoError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(IoError::Decode(format!("{what}: decoder panicked on malformed data"))),
    }
}

fn open_reader(path: &Path) -> Result<ImageReader<BufReader<File>>, IoError> {
    let file = File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => IoError::NotFound(path.to_path_buf()),
        _ => IoError::Io(format!("{}: {e}", path.display())),
    })?;
    let mut reader = ImageReader::new(BufReader::new(file)).with_guessed_format().map_err(|e| IoError::Io(e.to_string()))?;
    reader.no_limits();
    Ok(reader)
}

/// Like [`decode_oriented`], plus the embedded ICC profile bytes if there are any.
fn decode_oriented_with_icc(path: &Path) -> Result<(DynamicImage, Option<Vec<u8>>), IoError> {
    let reader = open_reader(path)?;
    let is_tiff = reader.format() == Some(image::ImageFormat::Tiff);
    let mut decoder = reader.into_decoder().map_err(|e| IoError::Decode(e.to_string()))?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    // `image` loses a TIFF's ICC profile once decoding limits have been applied to the decoder, so
    // TIFF profiles are read through a decoder of their own.
    let icc = if is_tiff {
        File::open(path)
            .ok()
            .and_then(|f| image::codecs::tiff::TiffDecoder::new(BufReader::new(f)).ok())
            .and_then(|mut t| t.icc_profile().ok().flatten())
    } else {
        decoder.icc_profile().ok().flatten()
    };
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| IoError::Decode(e.to_string()))?;
    img.apply_orientation(orientation);
    Ok((img, icc))
}

/// Image coded in sRGB (or in the profile `xf`) -> linear ACEScg.
fn to_acescg(img: &DynamicImage, xf: Option<&InputTransform>) -> Result<ImageBuf, IoError> {
    let (w, h) = (img.width(), img.height());
    let m = match xf {
        Some(x) => x.matrix,
        None => color::to_f32(&rec709_to_acescg()),
    };
    let lut_for = |n: usize| -> [Vec<f32>; 3] {
        match xf {
            Some(x) => [x.lut(0, n), x.lut(1, n), x.lut(2, n)],
            None => {
                let l: Vec<f32> = (0..n).map(|v| color::srgb_eotf(v as f32 / (n - 1) as f32)).collect();
                [l.clone(), l.clone(), l]
            }
        }
    };
    let mut out = vec![0.0f32; w as usize * h as usize * 4];
    let apply = |rgb: [f32; 3]| -> [f32; 3] {
        [
            m[0][0] * rgb[0] + m[0][1] * rgb[1] + m[0][2] * rgb[2],
            m[1][0] * rgb[0] + m[1][1] * rgb[1] + m[1][2] * rgb[2],
            m[2][0] * rgb[0] + m[2][1] * rgb[1] + m[2][2] * rgb[2],
        ]
    };
    let has_alpha = img.color().has_alpha();
    match img {
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_) => {
            let src = img.to_rgba32f();
            out.par_chunks_exact_mut(4).zip(src.as_raw().par_chunks_exact(4)).for_each(|(o, s)| {
                let c = apply([s[0], s[1], s[2]]);
                o.copy_from_slice(&[c[0], c[1], c[2], if has_alpha { s[3] } else { 1.0 }]);
            });
        }
        _ if img.color().bits_per_pixel() / img.color().channel_count() as u16 > 8 => {
            let src = img.to_rgba16();
            let lut = lut_for(65536);
            out.par_chunks_exact_mut(4).zip(src.as_raw().par_chunks_exact(4)).for_each(|(o, s)| {
                let c = apply([lut[0][s[0] as usize], lut[1][s[1] as usize], lut[2][s[2] as usize]]);
                o.copy_from_slice(&[c[0], c[1], c[2], if has_alpha { s[3] as f32 / 65535.0 } else { 1.0 }]);
            });
        }
        _ => {
            let src = img.to_rgba8();
            let lut = lut_for(256);
            out.par_chunks_exact_mut(4).zip(src.as_raw().par_chunks_exact(4)).for_each(|(o, s)| {
                let c = apply([lut[0][s[0] as usize], lut[1][s[1] as usize], lut[2][s[2] as usize]]);
                o.copy_from_slice(&[c[0], c[1], c[2], if has_alpha { s[3] as f32 / 255.0 } else { 1.0 }]);
            });
        }
    }
    ImageBuf::from_rgba(w, h, out).ok_or_else(|| IoError::Decode("internal buffer size mismatch".into()))
}

pub fn load(path: &Path, max_dim: Option<u32>) -> Result<Loaded, IoError> {
    let capture = read_capture_info(path)?;
    let (img, icc) = guard("image", || decode_oriented_with_icc(path))?;
    let full_size = (img.width(), img.height());
    // 32-bit float files are linear Rec.709 by definition; a profile does not change that.
    let is_float = matches!(img, DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_));
    let (xf, profile) = if is_float { (None, InputProfile::Untagged) } else { resolve_input(icc.as_deref()) };
    let mut image = to_acescg(&img, xf.as_ref())?;
    drop(img);
    if let Some(n) = max_dim {
        image = image.downscale_box(n);
    }
    Ok(Loaded { image, capture, kind: SourceKind::Rendered, full_size, profile })
}

/// Thumbnail in sRGB: pictures that carry a matrix/TRC profile (Adobe RGB, Display P3, ...) are
/// converted with it after the shrink (so the cost is per thumbnail pixel, not per source pixel).
pub fn load_thumbnail(path: &Path, max_dim: u32) -> Result<(u32, u32, Vec<u8>), IoError> {
    let (img, icc) = guard("image", || decode_oriented_with_icc(path))?;
    // 32-bit float files are linear Rec.709 by definition; a profile does not change that.
    let is_float = matches!(img, DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_));
    let (w, h, mut rgba) = crate::dynamic_to_thumb(img, max_dim);
    if !is_float {
        crate::icc::thumbnail_to_srgb(icc.as_deref(), &mut rgba);
    }
    Ok((w, h, rgba))
}

fn ascii(field: &exif::Field) -> Option<String> {
    if let exif::Value::Ascii(parts) = &field.value {
        let s = parts
            .iter()
            .map(|p| String::from_utf8_lossy(p).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let s = s.trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

fn rational(field: &exif::Field) -> Option<f32> {
    match &field.value {
        exif::Value::Rational(v) => v.first().filter(|r| r.denom != 0).map(|r| r.num as f32 / r.denom as f32),
        _ => None,
    }
}

pub fn read_capture_info(path: &Path) -> Result<CaptureInfo, IoError> {
    // Keep the same handle for validation and EXIF; an additional open can
    // be a network round trip on SMB. Borrow the reader only for validation.
    let reader = open_reader(path)?;
    let format = reader.format().ok_or_else(|| IoError::Decode("unknown image format".into()))?;
    let mut file = reader.into_inner();
    guard("image", || {
        let mut reader = ImageReader::with_format(&mut file, format);
        reader.no_limits();
        reader.into_decoder().map(|_| ()).map_err(|e| IoError::Decode(e.to_string()))
    })?;

    let mut info = CaptureInfo { raw: Some(false), ..Default::default() };
    if file.rewind().is_err() { return Ok(info); }
    let Ok(ex) = exif::Reader::new().read_from_container(&mut file) else { return Ok(info) };
    let get = |tag| ex.get_field(tag, exif::In::PRIMARY);

    info.captured_unix_ms = get(exif::Tag::DateTimeOriginal).and_then(ascii).as_deref().and_then(crate::exif_date);
    info.make = get(exif::Tag::Make).and_then(ascii);
    info.model = get(exif::Tag::Model).and_then(ascii);
    info.lens = get(exif::Tag::LensModel).and_then(ascii);
    let iso = get(exif::Tag::PhotographicSensitivity)
        .and_then(|f| f.value.get_uint(0))
        .filter(|v| *v != 0 && *v != 65535)
        .or_else(|| get(exif::Tag::ISOSpeed).and_then(|f| f.value.get_uint(0)).filter(|v| *v != 0))
        .or_else(|| get(exif::Tag::RecommendedExposureIndex).and_then(|f| f.value.get_uint(0)).filter(|v| *v != 0));
    info.iso = iso.map(|v| v as f32);
    info.shutter_s = get(exif::Tag::ExposureTime).and_then(rational).filter(|v| *v > 0.0);
    info.aperture = get(exif::Tag::FNumber).and_then(rational).filter(|v| *v > 0.0);
    info.flash_fired = get(exif::Tag::Flash).and_then(|f| f.value.get_uint(0)).map(|v| v & 1 == 1);
    Ok(info)
}
