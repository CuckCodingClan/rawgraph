//! Layer node: blend modes, fit modes, resampling filters and the settings of
//! one layer of a Layer node.

use serde::{Deserialize, Serialize};

use crate::graph::{ImageId, Node, NodeKind};

/// Number of layer inputs of a Layer node. Layer 1 is the bottom.
pub const MAX_LAYERS: usize = 8;

pub const LAYER_BLEND_MODES: [&str; 9] =
    ["Normal", "Add", "Subtract", "Multiply", "Screen", "Overlay", "Darken", "Lighten", "Difference"];
pub const LAYER_FITS: [&str; 4] = ["Fit", "Fill", "Crop", "Stretch"];
pub const LAYER_FILTERS: [&str; 3] = ["Linear", "Bicubic", "Nearest"];
pub const LAYER_MODES: [&str; 2] = ["Match", "All combinations"];

/// How a layer is combined with what lies below it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BlendMode {
    #[default]
    Normal,
    Add,
    Subtract,
    Multiply,
    Screen,
    Overlay,
    Darken,
    Lighten,
    Difference,
}

impl BlendMode {
    pub const ALL: [BlendMode; 9] = [
        BlendMode::Normal,
        BlendMode::Add,
        BlendMode::Subtract,
        BlendMode::Multiply,
        BlendMode::Screen,
        BlendMode::Overlay,
        BlendMode::Darken,
        BlendMode::Lighten,
        BlendMode::Difference,
    ];

    /// Mode with this index in [`LAYER_BLEND_MODES`]; out of range gives Normal.
    pub fn from_index(i: usize) -> BlendMode {
        Self::ALL.get(i).copied().unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        LAYER_BLEND_MODES[Self::ALL.iter().position(|m| *m == self).unwrap_or(0)]
    }
}

/// How a layer is fitted to the canvas (the canvas is the bottom layer's frame).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FitMode {
    /// Uniform scale until the whole layer is visible, centred.
    #[default]
    Fit,
    /// Uniform scale until the canvas is covered, centred, cropped.
    Fill,
    /// No scaling: one layer pixel per canvas pixel, centred.
    Crop,
    /// Non-uniform scale to the canvas.
    Stretch,
}

impl FitMode {
    pub const ALL: [FitMode; 4] = [FitMode::Fit, FitMode::Fill, FitMode::Crop, FitMode::Stretch];

    pub fn from_index(i: usize) -> FitMode {
        Self::ALL.get(i).copied().unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        LAYER_FITS[Self::ALL.iter().position(|m| *m == self).unwrap_or(0)]
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResampleFilter {
    #[default]
    Linear,
    Bicubic,
    Nearest,
}

impl ResampleFilter {
    pub const ALL: [ResampleFilter; 3] = [ResampleFilter::Linear, ResampleFilter::Bicubic, ResampleFilter::Nearest];

    pub fn from_index(i: usize) -> ResampleFilter {
        Self::ALL.get(i).copied().unwrap_or_default()
    }

    pub fn label(self) -> &'static str {
        LAYER_FILTERS[Self::ALL.iter().position(|m| *m == self).unwrap_or(0)]
    }
}

/// Settings of one layer of a Layer node for one output image.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerSettings {
    pub on: bool,
    pub blend: BlendMode,
    /// 0..=1.
    pub opacity: f32,
    pub fit: FitMode,
    pub filter: ResampleFilter,
}

impl Default for LayerSettings {
    fn default() -> Self {
        LayerSettings {
            on: true,
            blend: BlendMode::Normal,
            opacity: 1.0,
            fit: FitMode::Fit,
            filter: ResampleFilter::Linear,
        }
    }
}

/// Settings of layer `layer` (0-based; layer 1 is `0`) of a Layer node for the
/// output image `image`. Anything that is not a Layer node, or a layer number
/// of `MAX_LAYERS` or more, gives the defaults. A non-finite opacity reads as 1.
pub fn layer_settings(node: &Node, layer: usize, image: ImageId) -> LayerSettings {
    if node.kind != NodeKind::Layer || layer >= MAX_LAYERS {
        return LayerSettings::default();
    }
    let n = layer + 1;
    let opacity = node.param(&format!("l{n}_opacity"), image);
    LayerSettings {
        on: node.toggle(&format!("l{n}_on"), image),
        blend: BlendMode::from_index(node.choice(&format!("l{n}_blend"), image)),
        opacity: if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 },
        fit: FitMode::from_index(node.choice(&format!("l{n}_fit"), image)),
        filter: ResampleFilter::from_index(node.choice(&format!("l{n}_filter"), image)),
    }
}
