//! RawGraph I/O: decode RAW/DNG and JPEG/PNG/TIFF into the engine's working
//! format and write exports.
//!
//! The working format is scene-referred linear ACEScg in a 32-bit float
//! [`rawgraph_core::ImageBuf`]. See [`raw`] for the exact RAW colour pipeline
//! and [`rendered`] for how already-developed files are treated.
//!
//! No function in this crate panics on file content: every failure (including
//! panics inside the third-party RAW decoder) is turned into an [`IoError`].

use std::fmt;
use std::path::{Path, PathBuf};

use rawgraph_core::{CaptureInfo, ImageBuf};

mod avif;
mod export;
mod heic;
// The HEIF box editing is only called by the Windows HEIC encoder; its tests run everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
mod heif_box;
mod icc;
mod lens;
mod meta;
mod webp_icc;
mod orient;
mod xmp;
pub mod raw;
pub mod rendered;

pub use export::{decode_webp8, encode_heic8, encode_heic8_detailed, encode_rgb, encode_webp8, save_avif8, save_heic8, save_webp8, save_exr, save_srgb16_with, save_srgb8, save_srgb8_with, SaveOptions};
pub use heic::{embedded_icc, heic_available, HeicEncoded, HeicImage, HEIC_MIN_SIDE, HEIC_MISSING_MESSAGE};
pub use icc::{icc_profile, srgb_icc_profile};
pub use lens::read_lens_profile;
/// Decodes a HEIC file with the operating system (Export preview and tests; not an import path).
pub fn decode_heic8(bytes: &[u8]) -> Result<HeicImage, IoError> {
    heic::decode(bytes)
}
pub use webp_icc::webp_icc_profile;
pub use meta::{read_export_metadata, read_xmp_packet, ExportMetadata, GpsInfo};

/// Whether a source file still carries sensor data or is already developed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Camera RAW / DNG: scene-referred data with full latitude.
    Raw,
    /// JPEG/PNG/TIFF: already developed (tone curve, baked-in white balance, clipped).
    Rendered,
}

/// How the colour profile embedded in a rendered input file was treated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputProfile {
    /// No embedded profile (and every RAW file): the code values are sRGB.
    Untagged,
    /// An embedded profile that is sRGB: same pixels as untagged.
    Srgb,
    /// An embedded matrix/TRC RGB profile (description): decoded with its own curves and
    /// primaries, converted to ACEScg.
    Matrix(String),
    /// An embedded profile that cannot be used (LUT-based, CMYK, grey, broken; the text says
    /// why): the file is decoded as sRGB.
    UnsupportedTreatedAsSrgb(String),
}

/// A decoded source image.
pub struct Loaded {
    /// Linear ACEScg, orientation applied.
    pub image: ImageBuf,
    pub capture: CaptureInfo,
    pub kind: SourceKind,
    /// Oriented full-resolution size, even if `image` is a proxy.
    pub full_size: (u32, u32),
    /// How the file's embedded colour profile was treated (RAW files: `Untagged`).
    pub profile: InputProfile,
}

#[derive(Debug)]
pub enum IoError {
    /// The file type (or a feature of the file) is not supported.
    Unsupported(String),
    NotFound(PathBuf),
    /// The file exists but its content could not be decoded.
    Decode(String),
    /// Operating-system I/O failure or invalid arguments for writing.
    Io(String),
    /// Refused to replace an existing file.
    Exists(PathBuf),
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IoError::Unsupported(s) => write!(f, "unsupported: {s}"),
            IoError::NotFound(p) => write!(f, "file not found: {}", p.display()),
            IoError::Decode(s) => write!(f, "decode error: {s}"),
            IoError::Io(s) => write!(f, "I/O error: {s}"),
            IoError::Exists(p) => write!(f, "file already exists: {}", p.display()),
        }
    }
}

impl std::error::Error for IoError {}

/// Lower-case, without dot. Everything the RAW decoder (`rawler`) has a decoder for and
/// that the pipeline in [`raw`] can develop (Bayer / X-Trans / linear RGB sensors).
/// Verified on real files so far: Sony ARW (ILCE-7M4) and DNG (Sony A7 IV, Canon EOS R5/R6).
pub const RAW_EXTENSIONS: &[&str] = &[
    "arw", "srf", "sr2", "dng", "cr2", "cr3", "crw", "nef", "nrw", "orf", "raf", "rw2", "rwl", "pef", "srw", "mrw",
    "erf", "kdc", "dcr", "mef", "mos", "iiq", "3fr", "ari", "qtk",
];

pub const RENDERED_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "tif", "tiff"];

/// Classifies a file by extension, case-insensitively.
pub fn source_kind(path: &Path) -> Option<SourceKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if RAW_EXTENSIONS.contains(&ext.as_str()) {
        Some(SourceKind::Raw)
    } else if RENDERED_EXTENSIONS.contains(&ext.as_str()) {
        Some(SourceKind::Rendered)
    } else {
        None
    }
}

pub fn is_supported(path: &Path) -> bool {
    source_kind(path).is_some()
}

fn kind_or_err(path: &Path) -> Result<SourceKind, IoError> {
    let kind = source_kind(path).ok_or_else(|| {
        IoError::Unsupported(format!(
            "unknown file extension: {}",
            path.extension().and_then(|e| e.to_str()).unwrap_or("(none)")
        ))
    })?;
    if !path.is_file() {
        return Err(IoError::NotFound(path.to_path_buf()));
    }
    Ok(kind)
}

/// Options for [`load_with`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadOptions {
    /// `Some(n)`: proxy whose long edge is <= n; `None`: full resolution.
    pub max_dim: Option<u32>,
    /// RAW only: turn pixels in which a camera channel reached the sensor clip level neutral
    /// instead of leaving the white-balance colour cast (see [`raw`]). Ignored for rendered files.
    pub neutralize_clipped_highlights: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        LoadOptions { max_dim: None, neutralize_clipped_highlights: true }
    }
}

/// Full decode with options.
pub fn load_with(path: &Path, options: &LoadOptions) -> Result<Loaded, IoError> {
    match kind_or_err(path)? {
        SourceKind::Raw => raw::load(path, options),
        SourceKind::Rendered => rendered::load(path, options.max_dim),
    }
}

/// Full decode. `max_dim = Some(n)`: return a box-filtered proxy whose long edge is <= n
/// (see `ImageBuf::downscale_box`); `None`: full resolution.
pub fn load(path: &Path, max_dim: Option<u32>) -> Result<Loaded, IoError> {
    load_with(path, &LoadOptions { max_dim, ..Default::default() })
}

/// Metadata only; no pixel decoding.
pub fn read_capture_info(path: &Path) -> Result<CaptureInfo, IoError> {
    match kind_or_err(path)? {
        SourceKind::Raw => raw::read_capture_info(path),
        SourceKind::Rendered => rendered::read_capture_info(path),
    }
}

/// EXIF typically omits a timezone; sort using the camera's recorded local clock.
fn exif_date(s: &str) -> Option<i64> {
    chrono::NaiveDateTime::parse_from_str(s.trim_matches('\0').trim(), "%Y:%m:%d %H:%M:%S")
        .ok().map(|date| date.and_utc().timestamp_millis())
}

/// Small sRGB preview for filmstrips: the embedded preview if the file has one, otherwise a
/// decoded proxy. Returns (width, height, RGBA8), orientation applied, long edge <= `max_dim`.
pub fn load_thumbnail(path: &Path, max_dim: u32) -> Result<(u32, u32, Vec<u8>), IoError> {
    let max_dim = max_dim.max(1);
    match kind_or_err(path)? {
        SourceKind::Raw => raw::load_thumbnail(path, max_dim),
        SourceKind::Rendered => rendered::load_thumbnail(path, max_dim),
    }
}

/// Shrinks (box/triangle filter) to `max_dim` and returns RGBA8 — shared by thumbnail paths.
pub(crate) fn dynamic_to_thumb(img: image::DynamicImage, max_dim: u32) -> (u32, u32, Vec<u8>) {
    let img = if img.width().max(img.height()) > max_dim { img.thumbnail(max_dim, max_dim) } else { img };
    let rgba = img.to_rgba8();
    (rgba.width(), rgba.height(), rgba.into_raw())
}
