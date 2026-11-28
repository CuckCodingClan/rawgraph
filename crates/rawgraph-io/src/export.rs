//! Export writers. Files are written to a temporary sibling and renamed into place, so a failed
//! write never leaves a truncated target and an existing file is only replaced on success.

use std::fs::{self, File};
use std::io::{BufWriter, Cursor, Write, Seek};
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
    write_atomic(path, options.overwrite, |tmp| {
        let mut writer = BufWriter::new(File::create(tmp).map_err(|e| io_err(tmp, e))?);
        encode_rgb_to(&mut writer, width, height, bytes, color, ext, options)?;
        writer.flush().map_err(|e| io_err(tmp, e))?;
        drop(writer);
        if matches!(ext, "tif" | "tiff") {
            if let Some(md) = &options.metadata {
                tiff_add_exif(tmp, md, options.space == OutputSpace::Srgb)?;
            }
        }
        Ok(())
    })
}

/// Encode RGB samples with the same image encoder as file export, entirely in memory.
/// TIFF EXIF is appended by the file writer and is not included in these bytes.
pub fn encode_rgb(
    width: u32, height: u32, bytes: &[u8], color: ExtendedColorType,
    ext: &str, options: &SaveOptions,
) -> Result<Vec<u8>, IoError> {
    let mut w = Cursor::new(Vec::new());
    encode_rgb_to(&mut w, width, height, bytes, color, ext, options)?;
    Ok(w.into_inner())
}

fn encode_rgb_to<W: Write + Seek>(mut w: W, width: u32, height: u32, bytes: &[u8], color: ExtendedColorType, ext: &str, options: &SaveOptions) -> Result<(), IoError> {
    let bytes_per_sample = match color {
        ExtendedColorType::Rgb8 => 1,
        ExtendedColorType::Rgb16 => 2,
        _ => return Err(IoError::Unsupported("expected RGB8 or RGB16".into())),
    };
    if !bytes.len().is_multiple_of(bytes_per_sample) {
        return Err(IoError::Io("invalid RGB sample bytes".into()));
    }
    check_size(width, height, bytes.len() / bytes_per_sample)?;
    let srgb = options.space == OutputSpace::Srgb;
    macro_rules! encode {
        ($enc:expr, $exif:expr) => {{
            let mut enc = $enc;
            if options.embed_profile {
                enc.set_icc_profile(icc_profile(options.space).to_vec()).map_err(|e| IoError::Io(format!("encoding {ext}: {e}")))?;
            }
            if $exif {
                if let Some(md) = &options.metadata {
                    enc.set_exif_metadata(exif_blob(md, srgb)).map_err(|e| IoError::Io(format!("encoding {ext}: {e}")))?;
                }
            }
            enc.write_image(bytes, width, height, color).map_err(|e| IoError::Io(format!("encoding {ext}: {e}")))?;
        }};
    }
    match ext {
        "png" => encode!(PngEncoder::new(&mut w), true),
        "tif" | "tiff" => encode!(TiffEncoder::new(&mut w), false),
        "jpg" | "jpeg" if color == ExtendedColorType::Rgb8 => encode!(JpegEncoder::new_with_quality(&mut w, options.jpeg_quality.clamp(1, 100)), true),
        _ => return Err(IoError::Unsupported(format!("cannot encode {color:?} as '{ext}'"))),
    }
    Ok(())
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

/// Encodes an 8-bit HEIC file in memory with the operating system's encoder (see [`crate::heic`]).
/// `rgb8` is RGB already encoded in `options.space`; `options.jpeg_quality` is the quality. With
/// `embed_profile` the ICC profile of the space is stored, with `metadata` EXIF is attempted
/// (best effort, see `heic`). [`IoError::Unsupported`] with the reason when no encoder exists.
pub fn encode_heic8(width: u32, height: u32, rgb8: &[u8], options: &SaveOptions) -> Result<Vec<u8>, IoError> {
    check_size(width, height, rgb8.len())?;
    crate::heic::heic_available().map_err(IoError::Unsupported)?;
    crate::heic::encode(width, height, rgb8, options.jpeg_quality.clamp(1, 100), Some(options.space), options.metadata.as_ref(), options.embed_profile)
}

/// Writes an 8-bit HEIC file (see [`encode_heic8`]); same overwrite rule and atomic write as the
/// other writers.
pub fn save_heic8(path: &Path, width: u32, height: u32, rgb8: &[u8], options: &SaveOptions) -> Result<(), IoError> {
    check_size(width, height, rgb8.len())?;
    if !options.overwrite && path.exists() {
        return Err(IoError::Exists(path.to_path_buf()));
    }
    let bytes = encode_heic8(width, height, rgb8, options)?;
    write_atomic(path, options.overwrite, |tmp| fs::write(tmp, &bytes).map_err(|e| io_err(tmp, e)))
}

/// Writes RGBA WebP. Pixels are already encoded in the selected output space. With `embed_profile`
/// the ICC profile of `options.space` is stored in an `ICCP` chunk (extended `VP8X` container,
/// lossy and lossless); EXIF is supported as well.
pub fn save_webp8(path: &Path, width: u32, height: u32, rgba: &[u8], lossless: bool, options: &SaveOptions) -> Result<(), IoError> {
    if !options.overwrite && path.exists() { return Err(IoError::Exists(path.to_path_buf())); }
    let bytes = encode_webp8(width, height, rgba, lossless, options)?;
    write_atomic(path, options.overwrite, |tmp| fs::write(tmp, &bytes).map_err(|e| io_err(tmp, e)))
}

/// The bytes [`save_webp8`] writes, in memory (Export preview, tests).
pub fn encode_webp8(width: u32, height: u32, rgba: &[u8], lossless: bool, options: &SaveOptions) -> Result<Vec<u8>, IoError> {
    let path = Path::new("(memory).webp");
    if width == 0 || height == 0 || width > 16383 || height > 16383 ||
        (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)) != Some(rgba.len()) {
        return Err(IoError::Io("invalid WebP dimensions or RGBA buffer".into()));
    }
    let img = webp_rust::ImageBuffer { width: width as usize, height: height as usize, rgba: rgba.to_vec() };
    let exif = options.metadata.as_ref().map(|m| exif_blob(m, options.space == OutputSpace::Srgb));
    let bytes = if lossless {
        webp_rust::encode_lossless_with_config(&img, &Default::default(), exif.as_deref())
    } else {
        let config = webp_rust::LossyEncodingConfig { quality: options.jpeg_quality.clamp(1, 100) as f32, ..Default::default() };
        webp_rust::encode_lossy_with_config(&img, &config, exif.as_deref())
    }.map_err(|e| io_err(path, e))?;
    if options.embed_profile { crate::webp_icc::with_icc(&bytes, icc_profile(options.space), width, height) } else { Ok(bytes) }
}

/// Decodes a WebP file to `(width, height, RGBA8)` (Export preview, tests).
pub fn decode_webp8(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), IoError> {
    let img = std::panic::catch_unwind(|| webp_rust::decode(bytes))
        .map_err(|_| IoError::Decode("WebP decoder panicked on malformed data".into()))?
        .map_err(|e| IoError::Decode(format!("WebP: {e}")))?;
    Ok((img.width as u32, img.height as u32, img.rgba))
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
