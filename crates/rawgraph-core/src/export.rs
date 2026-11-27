//! Export node: what to write and how, resolved per image.

use serde::{Deserialize, Serialize};

use crate::color::OutputSpace;
use crate::graph::{ImageId, Node, NodeData, NodeKind};

pub const EXPORT_FORMATS: [&str; 10] = ["JPEG", "PNG", "PNG 16-bit", "TIFF", "TIFF 16-bit", "EXR", "AVIF", "Adobe XMP sidecar", "PNG 16-bit data map", "WebP"];
pub const EXPORT_SIZES: [&str; 3] = ["Original", "Long edge", "Percent"];
pub const EXPORT_COLLISIONS: [&str; 3] = ["Skip", "Overwrite", "Rename"];

/// Limit of every Export text field, in characters.
pub const MAX_EXPORT_TEXT_CHARS: usize = 1024;
/// Limits of the `long_edge` parameter in pixels.
pub const EXPORT_LONG_EDGE_MIN: u32 = 16;
pub const EXPORT_LONG_EDGE_MAX: u32 = 32_000;

/// Text settings of an Export node (the node's structured data).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportText {
    /// Output folder; empty = not set.
    pub out_dir: String,
    /// File name template, default `{name}`.
    pub name_template: String,
    /// Empty = no override of the artist from the file.
    pub artist: String,
    /// Empty = no override.
    pub copyright: String,
    /// Empty = no override.
    pub description: String,
}

impl Default for ExportText {
    fn default() -> Self {
        ExportText {
            out_dir: String::new(),
            name_template: "{name}".to_owned(),
            artist: String::new(),
            copyright: String::new(),
            description: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportFileFormat {
    Jpeg,
    Png,
    Png16,
    Tiff,
    Tiff16,
    Exr,
    Avif,
    Xmp,
    Png16Data,
    Webp,
}

impl ExportFileFormat {
    pub const ALL: [ExportFileFormat; 10] = [
        ExportFileFormat::Jpeg,
        ExportFileFormat::Png,
        ExportFileFormat::Png16,
        ExportFileFormat::Tiff,
        ExportFileFormat::Tiff16,
        ExportFileFormat::Exr,
        ExportFileFormat::Avif,
        ExportFileFormat::Xmp,
        ExportFileFormat::Png16Data,
        ExportFileFormat::Webp,
    ];

    /// Format with this index in [`EXPORT_FORMATS`]; out of range gives JPEG.
    pub fn from_index(i: usize) -> ExportFileFormat {
        Self::ALL.get(i).copied().unwrap_or(ExportFileFormat::Jpeg)
    }

    pub fn label(self) -> &'static str {
        EXPORT_FORMATS[Self::ALL.iter().position(|f| *f == self).unwrap_or(0)]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExportSize {
    Original,
    /// Pixels of the long edge.
    LongEdge(u32),
    /// 1..=100.
    Percent(f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportCollision {
    Skip,
    Overwrite,
    Rename,
}

/// Which metadata groups an export keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MetaSelection {
    pub camera: bool,
    pub exposure: bool,
    pub date: bool,
    pub gps: bool,
    pub author: bool,
    pub software: bool,
}

impl Default for MetaSelection {
    /// The defaults of the Export node: everything except GPS.
    fn default() -> Self {
        MetaSelection { camera: true, exposure: true, date: true, gps: false, author: true, software: true }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportSpec {
    pub format: ExportFileFormat,
    /// 1..=100 (lossy formats).
    pub quality: u8,
    pub webp_lossless: bool,
    pub size: ExportSize,
    pub space: OutputSpace,
    pub embed_profile: bool,
    pub collision: ExportCollision,
    pub meta: MetaSelection,
    pub text: ExportText,
}

impl Default for ExportSpec {
    /// A fresh Export node.
    fn default() -> Self {
        ExportSpec {
            format: ExportFileFormat::Jpeg,
            quality: 92,
            webp_lossless: false,
            size: ExportSize::Original,
            space: OutputSpace::Srgb,
            embed_profile: true,
            collision: ExportCollision::Skip,
            meta: MetaSelection::default(),
            text: ExportText::default(),
        }
    }
}

/// Everything an Export node says about writing `image`. Other node kinds give
/// the defaults of a fresh Export node. Non-finite numbers read as the defaults.
pub fn export_spec(node: &Node, image: ImageId) -> ExportSpec {
    if node.kind != NodeKind::Export {
        return ExportSpec::default();
    }
    let finite = |name: &str, default: f32| {
        let v = node.param(name, image);
        if v.is_finite() {
            v
        } else {
            default
        }
    };
    let size = match node.choice("size", image) {
        1 => ExportSize::LongEdge(
            finite("long_edge", 2048.0).round().clamp(EXPORT_LONG_EDGE_MIN as f32, EXPORT_LONG_EDGE_MAX as f32) as u32,
        ),
        2 => ExportSize::Percent(finite("percent", 100.0).clamp(1.0, 100.0)),
        _ => ExportSize::Original,
    };
    let text = match node.data(image) {
        Some(NodeData::Export(t)) => t.clone(),
        _ => ExportText::default(),
    };
    ExportSpec {
        format: ExportFileFormat::from_index(node.choice("format", image)),
        quality: finite("quality", 92.0).round().clamp(1.0, 100.0) as u8,
        webp_lossless: node.toggle("webp_lossless", image),
        size,
        space: OutputSpace::from_index(node.choice("color_space", image)),
        embed_profile: node.toggle("profile", image),
        collision: match node.choice("collision", image) {
            1 => ExportCollision::Overwrite,
            2 => ExportCollision::Rename,
            _ => ExportCollision::Skip,
        },
        meta: MetaSelection {
            camera: node.toggle("meta_camera", image),
            exposure: node.toggle("meta_exposure", image),
            date: node.toggle("meta_date", image),
            gps: node.toggle("meta_gps", image),
            author: node.toggle("meta_author", image),
            software: node.toggle("meta_software", image),
        },
        text,
    }
}
