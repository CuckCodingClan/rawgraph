//! Export writers. Files are written to a temporary sibling and renamed into place, so a failed
//! write never leaves a truncated target and an existing file is only replaced on success.

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::tiff::TiffEncoder;
use image::{ExtendedColorType, ImageEncoder};
use rawgraph_core::ImageBuf;

use rawgraph_core::color::OutputSpace;

use crate::icc::icc_profile;
use crate::meta::{exif_blob, tiff_add_exif, ExportMetadata};
use crate::IoError;

fn io_err(path: &Path, e: impl std::fmt::Display) -> IoError {
    IoError::Io(format!("{}: {e}", path.display()))
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(format!(".{}.rgtmp", std::process::id()));
    path.with_file_name(name)
}

/// Runs `write` against a temporary file, then moves it to `path`.
fn write_atomic(
    path: &Path,
    overwrite: bool,
    write: impl FnOnce(&Path) -> Result<(), IoError>,
) -> Result<(), IoError> {
    if !overwrite && path.exists() {
        return Err(IoError::Exists(path.to_path_buf()));
    }
    let tmp = tmp_path(path);
    let result = write(&tmp).and_then(|()| {
        if !overwrite && path.exists() {
            return Err(IoError::Exists(path.to_path_buf()));
        }
        fs::rename(&tmp, path).map_err(|e| io_err(path, e))
    });
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Options of [`save_srgb8_with`] / [`save_srgb16_with`].
#[derive(Clone, Debug, PartialEq)]
pub struct SaveOptions {
    /// JPEG quality, clamped to 1..=100. Ignored for other formats.
    pub jpeg_quality: u8,
    /// Replace an existing file; otherwise [`IoError::Exists`].
    pub overwrite: bool,
    /// Embed the ICC profile of [`SaveOptions::space`] ([`crate::icc_profile`]).
    pub embed_profile: bool,
    /// The colour space the pixels are encoded in (the caller converts; the writers only label):
    /// selects the embedded profile and the EXIF `ColorSpace` value.
    pub space: OutputSpace,
    /// EXIF to write (Orientation is always 1: the pixels are already upright). `None`: no EXIF.
    pub metadata: Option<ExportMetadata>,
}

impl Default for SaveOptions {
    fn default() -> Self {
        SaveOptions { jpeg_quality: 92, overwrite: false, embed_profile: true, space: OutputSpace::Srgb, metadata: None }
    }
}

fn check_size(width: u32, height: u32, len: usize) -> Result<(), IoError> {
    if width == 0 || height == 0 {
        return Err(IoError::Io("image has zero size".into()));
    }
    let expected = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(3));
    if expected != Some(len) {
        return Err(IoError::Io(format!(
            "buffer has {len} samples, expected {width}x{height}x3 = {}",
            expected.map_or("overflow".to_string(), |n| n.to_string())
        )));
    }
    Ok(())
}

fn extension_of(path: &Path) -> String {
    path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// Shared writer: `bytes` are 8-bit RGB or native-endian 16-bit RGB samples.
fn write_rgb(
    path: &Path,
    width: u32,
    height: u32,
    bytes: &[u8],
    color: ExtendedColorType,
    ext: &str,
    options: &SaveOptions,
) -> Result<(), IoError> {
    let srgb = options.space == OutputSpace::Srgb;
    write_atomic(path, options.overwrite, |tmp| {
        let file = File::create(tmp).map_err(|e| io_err(tmp, e))?;
        let mut w = BufWriter::new(file);
        macro_rules! encode {
            ($enc:expr, $exif:expr) => {{
                let mut enc = $enc;
                if options.embed_profile {
                    enc.set_icc_profile(icc_profile(options.space).to_vec()).map_err(|e| io_err(tmp, e))?;
                }
                if $exif {
                    if let Some(md) = &options.metadata {
                        enc.set_exif_metadata(exif_blob(md, srgb)).map_err(|e| io_err(tmp, e))?;
                    }
                }
                enc.write_image(bytes, width, height, color)
            }};
        }
        match ext {
            "png" => encode!(PngEncoder::new(&mut w), true),
            "tif" | "tiff" => encode!(TiffEncoder::new(&mut w), false),
            _ => encode!(JpegEncoder::new_with_quality(&mut w, options.jpeg_quality.clamp(1, 100)), true),
        }
        .map_err(|e| io_err(tmp, e))?;
        std::io::Write::flush(&mut w).map_err(|e| io_err(tmp, e))?;
        drop(w);
        if matches!(ext, "tif" | "tiff") {
            if let Some(md) = &options.metadata {
                tiff_add_exif(tmp, md, srgb)?;
            }
        }
        Ok(())
    })
}

/// Writes 8-bit sRGB. `rgb8` is RGB, 3 bytes per pixel. The format follows the extension:
/// jpg/jpeg (uses `options.jpeg_quality`), png, tif/tiff. `rgb8` must already be encoded in
/// `options.space` (despite the name the function writes any of the output spaces). With
/// `embed_profile` the ICC profile of `options.space` is embedded; with `metadata` EXIF is written
/// (JPEG APP1, PNG eXIf, TIFF EXIF IFD, including GPS and description).
/// Refuses to replace an existing file unless `options.overwrite`.
pub fn save_srgb8_with(path: &Path, width: u32, height: u32, rgb8: &[u8], options: &SaveOptions) -> Result<(), IoError> {
    let ext = extension_of(path);
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "tif" | "tiff") {
        return Err(IoError::Unsupported(format!("cannot write files with extension '{ext}'")));
    }
    check_size(width, height, rgb8.len())?;
    write_rgb(path, width, height, rgb8, ExtendedColorType::Rgb8, &ext, options)
}

/// Writes 16-bit sRGB: `rgb16` is RGB, 3 `u16` per pixel; png or tif/tiff by extension (JPEG has no
/// 16-bit mode: [`IoError::Unsupported`]). Profile, metadata and overwrite rule as in
/// [`save_srgb8_with`]; `jpeg_quality` is ignored.
pub fn save_srgb16_with(path: &Path, width: u32, height: u32, rgb16: &[u16], options: &SaveOptions) -> Result<(), IoError> {
    let ext = extension_of(path);
    if !matches!(ext.as_str(), "png" | "tif" | "tiff") {
        return Err(IoError::Unsupported(format!("cannot write 16-bit files with extension '{ext}'")));
    }
    check_size(width, height, rgb16.len())?;
    let mut bytes = Vec::with_capacity(rgb16.len() * 2);
    for v in rgb16 {
        bytes.extend_from_slice(&v.to_ne_bytes());
    }
    write_rgb(path, width, height, &bytes, ExtendedColorType::Rgb16, &ext, options)
}

/// Writes an 8-bit AVIF file (pure-Rust encoder `ravif`/rav1e, 4:4:4, 10-bit internal precision).
/// `rgb8` is RGB, 3 bytes per pixel, already encoded in `options.space`; `options.jpeg_quality`
/// (1..=100) is the AVIF quality. Colour is signalled with CICP, not with an ICC profile: sRGB
/// (1/13), Display P3 (12/13) and Rec. 2020 (9/14) are supported (the labels are written even when
/// `embed_profile` is off, AVIF cannot be untagged); Adobe RGB (1998) has no CICP code and gives
/// [`IoError::Unsupported`]. With `metadata` the EXIF block (incl. GPS and description) is stored
/// as the file's Exif item. Same overwrite rule and atomic write as the other writers; the
/// encoding itself takes noticeable time (about 8 s for a synthetic 24-megapixel image on 6 threads, see `avif::SPEED`).
pub fn save_avif8(path: &Path, width: u32, height: u32, rgb8: &[u8], options: &SaveOptions) -> Result<(), IoError> {
    check_size(width, height, rgb8.len())?;
    crate::avif::cicp(options.space)?;
    if !options.overwrite && path.exists() {
        return Err(IoError::Exists(path.to_path_buf()));
    }
    let bytes = crate::avif::encode(width, height, rgb8, options)?;
    write_atomic(path, options.overwrite, |tmp| fs::write(tmp, &bytes).map_err(|e| io_err(tmp, e)))
}

/// Writes 8-bit sRGB without profile and metadata; see [`save_srgb8_with`]. Refuses to replace an
/// existing file unless `overwrite` is true.
pub fn save_srgb8(path: &Path, width: u32, height: u32, rgb8: &[u8], jpeg_quality: u8, overwrite: bool) -> Result<(), IoError> {
    let options = SaveOptions { jpeg_quality, overwrite, embed_profile: false, space: OutputSpace::Srgb, metadata: None };
    save_srgb8_with(path, width, height, rgb8, &options)
}

/// Writes the float image as OpenEXR: RGBA, 32-bit float, lossless (ZIP) compression. The pixels
/// are linear ACEScg, so the file carries the AP1 primaries and ACES white as its
/// `chromaticities` attribute. Same overwrite rule as [`save_srgb8`].
pub fn save_exr(path: &Path, image: &ImageBuf, overwrite: bool) -> Result<(), IoError> {
    use exr::meta::attribute::Chromaticities;
    use exr::prelude::*;

    let (w, h) = (image.width() as usize, image.height() as usize);
    if w == 0 || h == 0 {
        return Err(IoError::Io("image has zero size".into()));
    }
    write_atomic(path, overwrite, |tmp| {
        let data = image.data();
        let layer = Layer::new(
            (w, h),
            LayerAttributes::named("rgba"),
            Encoding::SMALL_LOSSLESS,
            SpecificChannels::rgba(|p: Vec2<usize>| {
                let i = (p.y() * w + p.x()) * 4;
                (data[i], data[i + 1], data[i + 2], data[i + 3])
            }),
        );
        let mut img = Image::from_layer(layer);
        img.attributes.chromaticities = Some(Chromaticities {
            red: Vec2(0.713, 0.293),
            green: Vec2(0.165, 0.830),
            blue: Vec2(0.128, 0.044),
            white: Vec2(0.32168, 0.33767),
        });
        img.write().to_file(tmp).map_err(|e| io_err(tmp, e))
    })
}
