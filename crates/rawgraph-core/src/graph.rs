//! Typed node graph with All/Individual parameters.
//!
//! A wire of type [`PortType::Images`] carries the whole collection; the
//! evaluator renders it one image identity at a time. Nodes compose strictly
//! in graph order and never flatten into each other, so every edit stays
//! adjustable and reversible.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

mod group;

pub use group::{GroupDef, GroupPort, MacroFile, Published, LevelEdge, End, MAX_GROUP_DEPTH, MACRO_FORMAT_VERSION};
pub use crate::capture::path_in_folder;
use crate::capture::{name_key, CaptureInfo, Measure};
use crate::color::OUTPUT_SPACES;
use crate::export::{
    ExportText, EXPORT_COLLISIONS, EXPORT_FORMATS, EXPORT_LONG_EDGE_MAX, EXPORT_LONG_EDGE_MIN, EXPORT_SIZES,
    MAX_EXPORT_TEXT_CHARS,
};
use crate::layer::{LAYER_BLEND_MODES, LAYER_FILTERS, LAYER_FITS, LAYER_MODES, MAX_LAYERS};
use crate::lens::LENS_MODES;

/// Bumped whenever the serialized graph layout changes incompatibly.
/// Version 2 added the Layer and Export kinds, Source folders, curve sets and
/// the Develop tone parameters; version 1 files still load. Version 5 added the Export "Keywords"
/// toggle. Version 6 added node groups (see [`GroupDef`]); version 5 files contain none and load as they are.
pub const GRAPH_FORMAT_VERSION: u32 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u64);

/// Stable identity of one image inside a project. It is assigned once by the
/// project and survives renames and moves (relinking is the project's job),
/// which is what lets Individual-mode values persist per image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ImageId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PortType {
    /// A collection of images (a single image is a collection of one).
    Images,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    /// One set of parameters for every incoming image.
    All,
    /// Parameters stored per image identity; images without a stored value
    /// inherit the node's All values.
    Individual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Source,
    CaptureEv,
    Exposure,
    WhiteBalance,
    Saturation,

    Blur,
    Output,
    Contrast,
    HighlightsShadows,
    NoiseReduction,
    /// Decides which images of the incoming collection stay in it; pixels pass through.
    MetadataFilter,
    /// Unites up to four collections; an image comes from the first input that has it.
    Merge,
    /// Absolute white balance (as shot / custom) plus baseline exposure.
    Develop,
    ToneCurve,
    HueSaturation,
    Sharpen,
    Vignette,
    BlackWhite,
    LensManual,
    /// Crop, straighten, rotate and flip.
    Transform,

    /// Curve through (input, output) points on the perceptual axis `t = Y / (Y + 0.18)`.
    PointCurve,
    /// Painted mask built from brush strokes. Created in Individual mode:
    /// a painted mask belongs to one image.
    Draw,
    /// Splits the collection into up to six named metadata ranges plus an
    /// "Unmatched" output.
    MetadataSplit,
    /// Lift / gamma / gain colour wheels (ASC-CDL style, see [`cdl_params`]).
    ColorWheels,
    /// Local-contrast ("clarity") and fine-detail ("texture") control.
    Clarity,
    /// Stacks up to [`MAX_LAYERS`] collections as layers; the layer with the
    /// most images drives the output collection.
    Layer,
    /// Writes the incoming images to files; passes the collection through.
    Export,
    /// Sort complete pixels over the whole image.
    PixelSort,
    /// Convert relative depth or luminance height to tangent-space normals.
    NormalMap,
    /// Relative depth estimated by an explicitly configured local worker.
    DepthMap,
    /// Generate pixels at the incoming image dimensions.
    Generator,
    /// Colour lookup table from a `.cube` file (path stored as node data).
    Lut,
    /// Structural node: a named set of other nodes shown as one box (see [`GroupDef`]).
    /// It has no ports or parameters of its own kind; the ports and published
    /// parameters live in its group definition. Never evaluated.
    Group,
}

/// How a parameter is edited and which values it accepts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    Continuous,
    /// Stored as an index into the option labels (0..len-1).
    Choice(&'static [&'static str]),
    /// Stored as 0.0 (off) or 1.0 (on).
    Toggle,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub kind: ParamKind,
    /// UI hint: the range is wide enough that a logarithmic slider fits better.
    pub logarithmic: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputSpec {
    pub label: &'static str,
    pub ty: PortType,
    pub required: bool,
}

/// One output port of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputSpec {
    pub label: &'static str,
    pub ty: PortType,
}

/// Number of range outputs of a Metadata Split (ports 0..=5).
pub const SPLIT_RANGES: usize = 6;
/// Output port of a Metadata Split that carries everything no range matched.
pub const SPLIT_UNMATCHED_PORT: usize = SPLIT_RANGES;

/// Limits of the structured node data (enforced by `Node::set_data` and when loading).
pub const MAX_CURVE_POINTS: usize = 32;
pub const MIN_CURVE_POINTS: usize = 2;
pub const MAX_STROKES: usize = 2_000;
pub const MAX_STROKE_POINTS: usize = 20_000;
pub const MAX_RANGES: usize = SPLIT_RANGES;
pub const MAX_RANGE_NAME_CHARS: usize = 40;
pub const BRUSH_RADIUS_MIN: f32 = 0.0005;
pub const BRUSH_RADIUS_MAX: f32 = 0.5;

/// Capture metadata lookup used for collection membership.
pub type CaptureFn<'a> = &'a dyn Fn(ImageId) -> Option<CaptureInfo>;

/// What collection queries know about the project: every image in project
/// order and the capture lookup (which should fill [`CaptureInfo::path`]).
#[derive(Clone, Copy)]
pub struct Catalog<'a> {
    /// All images of the project, in project order. May be empty: the `contains`
    /// family then answers per image without listing anything.
    pub images: &'a [ImageId],
    pub capture: CaptureFn<'a>,
}
/// Whether a Metadata Filter node accepts `image`. A present value is tested
/// against min..=max (inclusive) and then flipped by `invert`; a missing value
/// (or no capture info) follows the `missing` option and is not flipped.
pub fn metadata_filter_matches(node: &Node, image: ImageId, capture: Option<&CaptureInfo>) -> bool {
    let measure = Measure::from_index(node.choice("measure", image)).unwrap_or(Measure::Iso);
    if measure == Measure::Keyword {
        // Text measure: an image without keywords is not "missing", it simply contains nothing.
        let text = match node.data(image) {
            Some(NodeData::Keyword(t)) => t.as_str(),
            _ => "",
        };
        let has = capture.is_some_and(|c| crate::keywords::keywords_contain_text(&c.keywords, text));
        let matches = has != (node.choice("keyword_op", image) == 1);
        return matches != node.toggle("invert", image);
    }
    match capture.and_then(|c| c.measure(measure)) {
        Some(v) => {
            let inside = node.param("min", image) <= v && v <= node.param("max", image);
            inside != node.toggle("invert", image)
        }
        None => node.choice("missing", image) == 1,
    }
}

const fn p(name: &'static str, label: &'static str, unit: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec { name, label, unit, min, max, default, kind: ParamKind::Continuous, logarithmic: false }
}

const fn plog(name: &'static str, label: &'static str, unit: &'static str, min: f32, max: f32, default: f32) -> ParamSpec {
    ParamSpec { name, label, unit, min, max, default, kind: ParamKind::Continuous, logarithmic: true }
}

const fn choice(name: &'static str, label: &'static str, options: &'static [&'static str], default: usize) -> ParamSpec {
    ParamSpec {
        name,
        label,
        unit: "",
        min: 0.0,
        max: (options.len() - 1) as f32,
        default: default as f32,
        kind: ParamKind::Choice(options),
        logarithmic: false,
    }
}

const fn toggle(name: &'static str, label: &'static str, default: bool) -> ParamSpec {
    ParamSpec {
        name,
        label,
        unit: "",
        min: 0.0,
        max: 1.0,
        default: if default { 1.0 } else { 0.0 },
        kind: ParamKind::Toggle,
        logarithmic: false,
    }
}

const IMAGE_IN: InputSpec = InputSpec { label: "Images", ty: PortType::Images, required: true };

const IMAGE_INPUTS: &[InputSpec] = &[IMAGE_IN];

const OPTIONAL_IMAGE_INPUTS: &[InputSpec] = &[InputSpec { required: false, ..IMAGE_IN }];
const NORMALS_PARAMS: &[ParamSpec] = &[
    p("strength", "Strength", "", 0.0, 100.0, 1.0),
    p("smoothing", "Smoothing", "px", 0.0, 8.0, 0.0),
    toggle("flip_y", "Flip Y", false),
];

// Change to the capture-derived exposure value; +1 EV renders one stop brighter.
const PIXEL_SORT_PARAMS: &[ParamSpec] = &[
    choice("metric", "Sort by", &["Brightness", "Saturation", "Hue"], 0),
    toggle("reverse", "Reverse", false),
];
/// Input spaces of the LUT node (the choice index is stored; keep the order).
pub const LUT_SPACES: &[&str] = &["Display (view transform)", "sRGB / Rec.709 display (clamped)", "ACEScct (log)", "Linear ACEScg"];
const LUT_PARAMS: &[ParamSpec] = &[
    p("strength", "Strength", "", 0.0, 1.0, 1.0),
    choice("input_space", "Input space", LUT_SPACES, 0),
];
const GENERATOR_PARAMS: &[ParamSpec] = &[
    p("width", "Width", "px", 1.0, 8192.0, 1024.0),
    p("height", "Height", "px", 1.0, 8192.0, 1024.0),
    choice("type", "Type", &["Gradient", "Noise", "Pattern"], 0),
    toggle("vertical", "Vertical gradient (legacy)", false),
    choice("gradient_style", "Gradient style", &["Linear", "Radial", "Angle", "Reflected", "Diamond"], 0),
    p("gradient_angle", "Gradient angle", "deg", -180.0, 180.0, 0.0),
    p("gradient_scale", "Gradient scale", "%", 1.0, 400.0, 100.0),
    // Gradient origin in percent of the frame (50 / 50 = centre); may lie outside the frame.
    p("gradient_center_x", "Gradient center X", "%", -100.0, 200.0, 50.0),
    p("gradient_center_y", "Gradient center Y", "%", -100.0, 200.0, 50.0),
    toggle("gradient_reverse", "Reverse gradient", false),
    p("start_r", "Start red", "", -4.0, 4.0, 0.0), p("start_g", "Start green", "", -4.0, 4.0, 0.0), p("start_b", "Start blue", "", -4.0, 4.0, 0.0),
    p("end_r", "End red", "", -4.0, 4.0, 1.0), p("end_g", "End green", "", -4.0, 4.0, 1.0), p("end_b", "End blue", "", -4.0, 4.0, 1.0),
    toggle("ramp_middle_color", "Use middle colour stop", false),
    p("ramp_color_position", "Middle colour position", "%", 0.0, 100.0, 50.0),
    p("ramp_color_r", "Middle red", "", -4.0, 4.0, 0.5),
    p("ramp_color_g", "Middle green", "", -4.0, 4.0, 0.5),
    p("ramp_color_b", "Middle blue", "", -4.0, 4.0, 0.5),
    p("ramp_color_midpoint_1", "Colour midpoint 1", "%", 1.0, 99.0, 50.0),
    p("ramp_color_midpoint_2", "Colour midpoint 2", "%", 1.0, 99.0, 50.0),
    p("ramp_alpha_start", "Start opacity", "%", 0.0, 100.0, 100.0),
    toggle("ramp_middle_alpha", "Use middle opacity stop", false),
    p("ramp_alpha_position", "Middle opacity position", "%", 0.0, 100.0, 50.0),
    p("ramp_alpha_middle", "Middle opacity", "%", 0.0, 100.0, 100.0),
    p("ramp_alpha_end", "End opacity", "%", 0.0, 100.0, 100.0),
    p("ramp_alpha_midpoint_1", "Opacity midpoint 1", "%", 1.0, 99.0, 50.0),
    p("ramp_alpha_midpoint_2", "Opacity midpoint 2", "%", 1.0, 99.0, 50.0),
    choice("ramp_interpolation", "Ramp interpolation", &["Linear", "Smooth"], 0),
    p("ramp_smoothness", "Ramp smoothness", "%", 0.0, 100.0, 100.0),
    toggle("ramp_dither", "Ramp dither", false),
    p("seed", "Noise seed", "", 0.0, 65535.0, 0.0),
    p("cell_size", "Pattern cell size", "px", 1.0, 1024.0, 32.0),
    toggle("invert", "Invert pattern", false),
    choice("output", "Output", &["Display", "Data"], 0),
];
const CAPTURE_EV_PARAMS: &[ParamSpec] = &[p("delta", "EV change", "EV", -6.0, 6.0, 0.0)];
const EXPOSURE_PARAMS: &[ParamSpec] = &[p("ev", "Exposure", "stops", -6.0, 6.0, 0.0)];
const WHITE_BALANCE_PARAMS: &[ParamSpec] = &[
    p("temp", "Temperature shift", "mired", -150.0, 150.0, 0.0),
    p("tint", "Tint shift", "", -100.0, 100.0, 0.0),
];
const SATURATION_PARAMS: &[ParamSpec] = &[p("amount", "Saturation", "x", 0.0, 2.0, 1.0)];
// Radius as per-mille of the long edge, so preview and export agree.
const BLUR_PARAMS: &[ParamSpec] = &[
    plog("radius", "Radius", "\u{2030} of long edge", 0.0, 1000.0, 5.0),
    choice("pass", "Pass", &["Low pass", "High pass"], 0),
    choice("kernel", "Kernel", &["Gaussian", "Box", "Lens (disk)", "Motion", "Radial zoom", "Radial spin", "Shape (diamond)", "Median", "Bilateral"], 0),
    p("angle", "Motion angle", "deg", 0.0, 360.0, 0.0),
];
const CONTRAST_PARAMS: &[ParamSpec] = &[
    p("amount", "Amount", "", -1.0, 1.0, 0.0),
    plog("pivot", "Pivot", "scene linear", 0.02, 1.0, 0.18),
];
const HIGHLIGHTS_SHADOWS_PARAMS: &[ParamSpec] = &[
    p("highlights", "Highlights", "", -1.0, 1.0, 0.0),
    p("shadows", "Shadows", "", -1.0, 1.0, 0.0),
];
const NOISE_REDUCTION_PARAMS: &[ParamSpec] = &[
    p("luma", "Luma", "", 0.0, 1.0, 0.0),
    p("chroma", "Chroma", "", 0.0, 1.0, 0.0),
    p("radius", "Radius", "\u{2030} of long edge", 0.05, 2.0, 0.3),
    // Scale the strength with the capture ISO (see `iso_noise_scale`).
    toggle("iso_adaptive", "ISO adaptive", false),
];
const COLOR_WHEELS_PARAMS: &[ParamSpec] = &[
    p("lift", "Lift", "", -0.5, 0.5, 0.0),
    p("lift_r", "Lift red", "", -0.5, 0.5, 0.0),
    p("lift_g", "Lift green", "", -0.5, 0.5, 0.0),
    p("lift_b", "Lift blue", "", -0.5, 0.5, 0.0),
    p("gamma", "Gamma", "", -1.0, 1.0, 0.0),
    p("gamma_r", "Gamma red", "", -1.0, 1.0, 0.0),
    p("gamma_g", "Gamma green", "", -1.0, 1.0, 0.0),
    p("gamma_b", "Gamma blue", "", -1.0, 1.0, 0.0),
    p("gain", "Gain", "EV", -2.0, 2.0, 0.0),
    p("gain_r", "Gain red", "EV", -2.0, 2.0, 0.0),
    p("gain_g", "Gain green", "EV", -2.0, 2.0, 0.0),
    p("gain_b", "Gain blue", "EV", -2.0, 2.0, 0.0),
];
const CLARITY_PARAMS: &[ParamSpec] = &[
    p("clarity", "Clarity", "", -1.0, 1.0, 0.0),
    p("texture", "Texture", "", -1.0, 1.0, 0.0),
];
const MEASURE_OPTIONS: &[&str] = &Measure::LABELS;
/// The two tests of the Keyword measure.
pub const KEYWORD_OPS: [&str; 2] = ["contains", "does not contain"];
const KEYWORD_OP_OPTIONS: &[&str] = &KEYWORD_OPS;
const MISSING_OPTIONS: &[&str] = &["Exclude", "Include"];
const METADATA_FILTER_PARAMS: &[ParamSpec] = &[
    choice("measure", "Measure", MEASURE_OPTIONS, 0),
    plog("min", "Minimum", "", -100.0, 1_000_000.0, 0.0),
    plog("max", "Maximum", "", -100.0, 1_000_000.0, 1_000_000.0),
    toggle("invert", "Invert", false),
    choice("missing", "Missing metadata", MISSING_OPTIONS, 0),
    // Keyword measure only (appended): how the text is tested.
    choice("keyword_op", "Keyword test", KEYWORD_OP_OPTIONS, 0),
];
const IMAGE_OUTPUTS: &[OutputSpec] = &[OutputSpec { label: "Images", ty: PortType::Images }];
const NO_OUTPUTS: &[OutputSpec] = &[];
const fn range_out(label: &'static str) -> OutputSpec {
    OutputSpec { label, ty: PortType::Images }
}
const SPLIT_OUTPUTS: &[OutputSpec] = &[
    range_out("Range 1"),
    range_out("Range 2"),
    range_out("Range 3"),
    range_out("Range 4"),
    range_out("Range 5"),
    range_out("Range 6"),
    range_out("Unmatched"),
];
const MERGE_INPUTS: &[InputSpec] = &[
    InputSpec { label: "A", ty: PortType::Images, required: false },
    InputSpec { label: "B", ty: PortType::Images, required: false },
    InputSpec { label: "C", ty: PortType::Images, required: false },
    InputSpec { label: "D", ty: PortType::Images, required: false },
];

const WB_OPTIONS: &[&str] = &["As shot", "Custom"];
const DEVELOP_PARAMS: &[ParamSpec] = &[
    p("exposure", "Exposure", "EV", -5.0, 5.0, 0.0),
    p("contrast", "Contrast", "", -1.0, 1.0, 0.0),
    p("highlights", "Lights", "", -1.0, 1.0, 0.0),
    p("shadows", "Shadows", "", -1.0, 1.0, 0.0),
    p("whites", "White", "", -1.0, 1.0, 0.0),
    p("blacks", "Black", "", -1.0, 1.0, 0.0),
    choice("wb", "White balance", WB_OPTIONS, 0),
    plog("temperature", "Temperature", "K", 2000.0, 25000.0, 6500.0),
    p("tint", "Tint", "", -150.0, 150.0, 0.0),
    p("vibrance", "Dynamic", "", -1.0, 1.0, 0.0),
    p("saturation", "Saturation", "", -1.0, 1.0, 0.0),
    toggle("baseline", "Baseline exposure", true),
    p("baseline_default", "Default baseline", "EV", -2.0, 3.0, 0.35),
    // Rendering offset for RAW files, calibrated against embedded camera previews
    // (see examples/calibrate_brightness.rs).
    p("brightness", "Brightness", "EV", -3.0, 3.0, 1.6),
    // Off: local Lights/Shadows/White/Black (the default). On: the older pointwise curves.
    toggle("legacy_tone", "Legacy tone", false),
];
const TONE_CURVE_PARAMS: &[ParamSpec] = &[
    p("shadows", "Shadows", "", -1.0, 1.0, 0.0),
    p("darks", "Darks", "", -1.0, 1.0, 0.0),
    p("lights", "Lights", "", -1.0, 1.0, 0.0),
    p("highlights", "Highlights", "", -1.0, 1.0, 0.0),
];
const HUE_SATURATION_PARAMS: &[ParamSpec] = &[
    p("red_h", "Red hue", "", -1.0, 1.0, 0.0),
    p("red_s", "Red saturation", "", -1.0, 1.0, 0.0),
    p("red_l", "Red luminance", "", -1.0, 1.0, 0.0),
    p("yellow_h", "Yellow hue", "", -1.0, 1.0, 0.0),
    p("yellow_s", "Yellow saturation", "", -1.0, 1.0, 0.0),
    p("yellow_l", "Yellow luminance", "", -1.0, 1.0, 0.0),
    p("green_h", "Green hue", "", -1.0, 1.0, 0.0),
    p("green_s", "Green saturation", "", -1.0, 1.0, 0.0),
    p("green_l", "Green luminance", "", -1.0, 1.0, 0.0),
    p("cyan_h", "Cyan hue", "", -1.0, 1.0, 0.0),
    p("cyan_s", "Cyan saturation", "", -1.0, 1.0, 0.0),
    p("cyan_l", "Cyan luminance", "", -1.0, 1.0, 0.0),
    p("blue_h", "Blue hue", "", -1.0, 1.0, 0.0),
    p("blue_s", "Blue saturation", "", -1.0, 1.0, 0.0),
    p("blue_l", "Blue luminance", "", -1.0, 1.0, 0.0),
    p("magenta_h", "Magenta hue", "", -1.0, 1.0, 0.0),
    p("magenta_s", "Magenta saturation", "", -1.0, 1.0, 0.0),
    p("magenta_l", "Magenta luminance", "", -1.0, 1.0, 0.0),
];
const SHARPEN_PARAMS: &[ParamSpec] = &[
    p("amount", "Amount", "", 0.0, 3.0, 0.0),
    p("radius", "Radius", "\u{2030} of long edge", 0.05, 2.0, 0.15),
    p("threshold", "Threshold", "", 0.0, 1.0, 0.0),
];
const VIGNETTE_PARAMS: &[ParamSpec] = &[
    p("amount", "Amount", "", -1.0, 1.0, 0.0),
    p("midpoint", "Midpoint", "", 0.0, 1.0, 0.5),
    p("feather", "Feather", "", 0.0, 1.0, 0.5),
];
const BLACK_WHITE_PARAMS: &[ParamSpec] = &[
    p("red", "Red", "", -1.0, 2.0, 0.30),
    p("green", "Green", "", -1.0, 2.0, 0.59),
    p("blue", "Blue", "", -1.0, 2.0, 0.11),
];
const LENS_MODE_OPTIONS: &[&str] = &LENS_MODES;
const LENS_MANUAL_PARAMS: &[ParamSpec] = &[
    choice("mode", "Mode", LENS_MODE_OPTIONS, 0),
    p("distortion", "Distortion", "", -0.5, 0.5, 0.0),
    p("vignetting", "Vignetting", "EV at corner", 0.0, 3.0, 0.0),
    p("ca_red", "Red fringe", "\u{2030}", -5.0, 5.0, 0.0),
    p("ca_blue", "Blue fringe", "\u{2030}", -5.0, 5.0, 0.0),
];
const ROTATE_OPTIONS: &[&str] = &["0\u{b0}", "90\u{b0}", "180\u{b0}", "270\u{b0}"];
const TRANSFORM_PARAMS: &[ParamSpec] = &[
    p("left", "Crop left", "", 0.0, 1.0, 0.0),
    p("top", "Crop top", "", 0.0, 1.0, 0.0),
    p("right", "Crop right", "", 0.0, 1.0, 1.0),
    p("bottom", "Crop bottom", "", 0.0, 1.0, 1.0),
    p("angle", "Straighten", "deg", -45.0, 45.0, 0.0),
    choice("rotate", "Rotate", ROTATE_OPTIONS, 0),
    toggle("flip_h", "Flip horizontal", false),
    toggle("flip_v", "Flip vertical", false),
];

/// Channels a Curves node can edit, indexed by [`CurveEntry::channel`].
pub const CURVE_CHANNELS: [&str; 7] =
    ["Luminance", "Red", "Green", "Blue", "Sat vs Sat", "Hue vs Hue", "Hue vs Sat"];
/// Number of curve channels.
pub const CURVE_CHANNEL_COUNT: usize = CURVE_CHANNELS.len();
/// Order in which the channels of a curve set are applied.
const CURVE_APPLY_ORDER: [u8; 7] = [0, 1, 2, 3, 5, 6, 4];
const CURVE_CHANNEL_OPTIONS: &[&str] = &CURVE_CHANNELS;
const POINT_CURVE_PARAMS: &[ParamSpec] = &[choice("channel", "Channel", CURVE_CHANNEL_OPTIONS, 0)];

const SOURCE_PARAMS: &[ParamSpec] = &[toggle("subfolders", "Include subfolders", true)];
const MERGE_COLLISION_OPTIONS: &[&str] = &["Keep both", "First input wins", "Last input wins"];
const MERGE_PARAMS: &[ParamSpec] = &[choice("collision", "Same file name", MERGE_COLLISION_OPTIONS, 0)];

const LAYER_MODE_OPTIONS: &[&str] = &LAYER_MODES;
const LAYER_BLEND_OPTIONS: &[&str] = &LAYER_BLEND_MODES;
const LAYER_FIT_OPTIONS: &[&str] = &LAYER_FITS;
const LAYER_FILTER_OPTIONS: &[&str] = &LAYER_FILTERS;

macro_rules! layer_params {
    ($($i:literal),*) => {
        &[
            choice("mode", "Mode", LAYER_MODE_OPTIONS, 0),
            $(
                toggle(concat!("l", $i, "_on"), concat!("Layer ", $i, " on"), true),
                choice(concat!("l", $i, "_blend"), concat!("Layer ", $i, " blend"), LAYER_BLEND_OPTIONS, 0),
                p(concat!("l", $i, "_opacity"), concat!("Layer ", $i, " opacity"), "", 0.0, 1.0, 1.0),
                choice(concat!("l", $i, "_fit"), concat!("Layer ", $i, " fit"), LAYER_FIT_OPTIONS, 0),
                choice(concat!("l", $i, "_filter"), concat!("Layer ", $i, " filter"), LAYER_FILTER_OPTIONS, 0),
            )*
        ]
    };
}
const LAYER_PARAMS: &[ParamSpec] = layer_params!(1, 2, 3, 4, 5, 6, 7, 8);

const fn layer_in(label: &'static str) -> InputSpec {
    InputSpec { label, ty: PortType::Images, required: false }
}
const LAYER_INPUTS: &[InputSpec] = &[
    layer_in("Layer 1"),
    layer_in("Layer 2"),
    layer_in("Layer 3"),
    layer_in("Layer 4"),
    layer_in("Layer 5"),
    layer_in("Layer 6"),
    layer_in("Layer 7"),
    layer_in("Layer 8"),
];

const EXPORT_FORMAT_OPTIONS: &[&str] = &EXPORT_FORMATS;
const EXPORT_SIZE_OPTIONS: &[&str] = &EXPORT_SIZES;
const EXPORT_COLLISION_OPTIONS: &[&str] = &EXPORT_COLLISIONS;
const OUTPUT_SPACE_OPTIONS: &[&str] = &OUTPUT_SPACES;
const EXPORT_PARAMS: &[ParamSpec] = &[
    choice("format", "Format", EXPORT_FORMAT_OPTIONS, 0),
    p("quality", "Quality", "", 1.0, 100.0, 92.0),
    toggle("webp_lossless", "WebP lossless", false),
    choice("size", "Size", EXPORT_SIZE_OPTIONS, 0),
    plog("long_edge", "Long edge", "px", EXPORT_LONG_EDGE_MIN as f32, EXPORT_LONG_EDGE_MAX as f32, 2048.0),
    p("percent", "Percent", "%", 1.0, 100.0, 100.0),
    choice("color_space", "Colour space", OUTPUT_SPACE_OPTIONS, 0),
    toggle("profile", "Embed colour profile", true),
    choice("collision", "If the file exists", EXPORT_COLLISION_OPTIONS, 0),
    toggle("meta_camera", "Camera data", true),
    toggle("meta_exposure", "Exposure data", true),
    toggle("meta_date", "Date", true),
    toggle("meta_gps", "GPS location", false),
    toggle("meta_author", "Author", true),
    toggle("meta_software", "Software", true),
    toggle("meta_keywords", "Keywords", true),
];
const DRAW_PARAMS: &[ParamSpec] = &[
    p("width", "Width", "px", 1.0, 8192.0, 1024.0),
    p("height", "Height", "px", 1.0, 8192.0, 1024.0),
];
const SPLIT_MISSING_OPTIONS: &[&str] = &["Unmatched", "Exclude"];
const METADATA_SPLIT_PARAMS: &[ParamSpec] = &[
    choice("measure", "Measure", MEASURE_OPTIONS, 0),
    choice("missing", "Missing metadata", SPLIT_MISSING_OPTIONS, 0),
    // Keyword measure only (appended): how each range name is tested.
    choice("keyword_op", "Keyword test", KEYWORD_OP_OPTIONS, 0),
];

/// ASC-CDL style parameters derived from a ColorWheels node for one image.
/// Meaning: `out = (in * slope + offset) ^ power` for positive values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CdlParams {
    pub slope: [f32; 3],
    pub offset: [f32; 3],
    pub power: [f32; 3],
}

impl CdlParams {
    /// Leaves every value unchanged.
    pub const IDENTITY: CdlParams = CdlParams { slope: [1.0; 3], offset: [0.0; 3], power: [1.0; 3] };

    /// True if the parameters change nothing (exactly the identity).
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}

/// CDL parameters of a ColorWheels node for `image`: per channel `c`,
/// slope = 2^(gain + gain_c), offset = 0.18 * (lift + lift_c) and
/// power = 2^-(gamma + gamma_c), so a positive gamma brightens the mid-tones.
/// Other node kinds give the identity.
pub fn cdl_params(node: &Node, image: ImageId) -> CdlParams {
    if node.kind != NodeKind::ColorWheels {
        return CdlParams::IDENTITY;
    }
    let mut out = CdlParams::IDENTITY;
    for (c, suffix) in ["r", "g", "b"].iter().enumerate() {
        let get = |base: &str| node.param(base, image) + node.param(&format!("{base}_{suffix}"), image);
        out.slope[c] = get("gain").exp2();
        out.offset[c] = 0.18 * get("lift");
        out.power[c] = (-get("gamma")).exp2();
    }
    out
}

/// Factor applied to the Noise Reduction strength when `iso_adaptive` is on:
/// `sqrt(iso / 2500)` clamped to 0.25..=2.5 (the strengths are tuned at ISO
/// 2500). Unknown or non-positive ISO gives 1.0.
pub fn iso_noise_scale(iso: Option<f32>) -> f32 {
    match iso {
        Some(v) if v.is_finite() && v > 0.0 => (v / 2500.0).sqrt().clamp(0.25, 2.5),
        _ => 1.0,
    }
}

/// The identity curve, the default of a Curve node: (0,0) and (1,1).
pub fn default_curve() -> NodeData {
    NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]])
}

/// The identity curve of a channel: `[[0,0],[1,1]]` for Luminance, Red, Green
/// and Blue (0..=3), `[[0,0.5],[1,0.5]]` for the three hue/saturation channels
/// (4..=6). Channels above 6 give the first form.
pub fn neutral_curve(channel: u8) -> Vec<[f32; 2]> {
    if (4..=6).contains(&channel) {
        vec![[0.0, 0.5], [1.0, 0.5]]
    } else {
        vec![[0.0, 0.0], [1.0, 1.0]]
    }
}

/// One curve of a Curves node: the channel (index in [`CURVE_CHANNELS`]) and its points.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurveEntry {
    pub channel: u8,
    pub points: Vec<[f32; 2]>,
}

/// The curves a Curves node applies to `image`, in application order
/// (channels 0, 1, 2, 3, 5, 6, 4). A legacy single `Curve` becomes one entry
/// on the channel of the `channel` parameter; a `CurveSet` gives its entries.
/// Entries equal to their neutral curve are dropped; no data (or another node
/// kind) gives an empty list.
pub fn curve_set(node: &Node, image: ImageId) -> Vec<CurveEntry> {
    if node.kind != NodeKind::PointCurve {
        return Vec::new();
    }
    let mut entries: Vec<CurveEntry> = match node.data(image) {
        Some(NodeData::Curve(points)) => {
            let channel = node.choice("channel", image).min(CURVE_CHANNEL_COUNT - 1) as u8;
            vec![CurveEntry { channel, points: points.clone() }]
        }
        Some(NodeData::CurveSet(entries)) => entries.clone(),
        _ => Vec::new(),
    };
    entries.retain(|e| (e.channel as usize) < CURVE_CHANNEL_COUNT && e.points != neutral_curve(e.channel));
    entries.sort_by_key(|e| CURVE_APPLY_ORDER.iter().position(|c| *c == e.channel).unwrap_or(usize::MAX));
    entries
}

impl NodeKind {
    pub const ALL: [NodeKind; 32] = [
        NodeKind::Source,
        NodeKind::CaptureEv,
        NodeKind::Exposure,
        NodeKind::WhiteBalance,
        NodeKind::Saturation,

        NodeKind::Blur,
        NodeKind::Output,
        NodeKind::Contrast,
        NodeKind::HighlightsShadows,
        NodeKind::NoiseReduction,
        NodeKind::MetadataFilter,
        NodeKind::Merge,
        NodeKind::Develop,
        NodeKind::ToneCurve,
        NodeKind::HueSaturation,
        NodeKind::Sharpen,
        NodeKind::Vignette,
        NodeKind::BlackWhite,
        NodeKind::LensManual,
        NodeKind::Transform,

        NodeKind::PointCurve,
        NodeKind::Draw,
        NodeKind::MetadataSplit,
        NodeKind::ColorWheels,
        NodeKind::Clarity,
        NodeKind::Layer,
        NodeKind::Export,
        NodeKind::PixelSort,
        NodeKind::NormalMap,
        NodeKind::DepthMap,
        NodeKind::Generator,
        NodeKind::Lut,
    ];

    /// Menu group of this kind.
    pub fn category(self) -> &'static str {
        match self {
            NodeKind::Source | NodeKind::Output | NodeKind::Export => "Input/Output",
            NodeKind::Develop | NodeKind::CaptureEv => "Develop",
            NodeKind::Exposure
            | NodeKind::Contrast
            | NodeKind::HighlightsShadows
            | NodeKind::ToneCurve
            | NodeKind::PointCurve => "Tone",
            NodeKind::WhiteBalance | NodeKind::Saturation | NodeKind::HueSaturation | NodeKind::BlackWhite => "Colour",
            NodeKind::ColorWheels | NodeKind::Lut => "Colour",
            NodeKind::NoiseReduction | NodeKind::Sharpen | NodeKind::Blur | NodeKind::Clarity => "Detail",
            NodeKind::Transform | NodeKind::LensManual => "Geometry",
            NodeKind::Vignette | NodeKind::PixelSort => "Effects",
            NodeKind::MetadataFilter | NodeKind::Merge | NodeKind::MetadataSplit | NodeKind::Layer => "Collection",
            NodeKind::Draw | NodeKind::DepthMap | NodeKind::NormalMap | NodeKind::Generator => "Generators",
            NodeKind::Group => "Structure",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NodeKind::Source => "Source",
            NodeKind::CaptureEv => "Capture EV",
            NodeKind::Exposure => "Exposure",
            NodeKind::WhiteBalance => "White Balance (relative)",
            NodeKind::Saturation => "Saturation",

            NodeKind::Blur => "Filter",
            NodeKind::Output => "Output",
            NodeKind::Contrast => "Contrast",
            NodeKind::HighlightsShadows => "Highlights / Shadows",
            NodeKind::NoiseReduction => "Noise Reduction",
            NodeKind::MetadataFilter => "Metadata Filter",
            NodeKind::Merge => "Merge",
            NodeKind::Develop => "Develop",
            NodeKind::ToneCurve => "Tone Curve",
            NodeKind::HueSaturation => "Hue / Saturation",
            NodeKind::Sharpen => "Sharpen",
            NodeKind::Vignette => "Vignette",
            NodeKind::BlackWhite => "Black & White",
            NodeKind::LensManual => "Lens Correction",
            NodeKind::Transform => "Crop / Rotate",

            NodeKind::PointCurve => "Curves",
            NodeKind::Draw => "Draw",
            NodeKind::MetadataSplit => "Split by Metadata",
            NodeKind::ColorWheels => "Lift / Gamma / Gain",
            NodeKind::Clarity => "Clarity / Texture",
            NodeKind::Layer => "Layer",
            NodeKind::Export => "Export",
            NodeKind::PixelSort => "Pixel Sort",
            NodeKind::NormalMap => "Normal Map",
            NodeKind::DepthMap => "Depth Map",
            NodeKind::Generator => "Generator",
            NodeKind::Lut => "LUT",
            NodeKind::Group => "Group",
        }
    }

    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            NodeKind::Output => &[],
            NodeKind::Source => SOURCE_PARAMS,
            NodeKind::Merge => MERGE_PARAMS,
            NodeKind::Layer => LAYER_PARAMS,
            NodeKind::Export => EXPORT_PARAMS,
            NodeKind::CaptureEv => CAPTURE_EV_PARAMS,
            NodeKind::Exposure => EXPOSURE_PARAMS,
            NodeKind::WhiteBalance => WHITE_BALANCE_PARAMS,
            NodeKind::Saturation => SATURATION_PARAMS,

            NodeKind::Blur => BLUR_PARAMS,
            NodeKind::Contrast => CONTRAST_PARAMS,
            NodeKind::HighlightsShadows => HIGHLIGHTS_SHADOWS_PARAMS,
            NodeKind::NoiseReduction => NOISE_REDUCTION_PARAMS,
            NodeKind::MetadataFilter => METADATA_FILTER_PARAMS,
            NodeKind::Develop => DEVELOP_PARAMS,
            NodeKind::ToneCurve => TONE_CURVE_PARAMS,
            NodeKind::HueSaturation => HUE_SATURATION_PARAMS,
            NodeKind::Sharpen => SHARPEN_PARAMS,
            NodeKind::Vignette => VIGNETTE_PARAMS,
            NodeKind::BlackWhite => BLACK_WHITE_PARAMS,
            NodeKind::LensManual => LENS_MANUAL_PARAMS,
            NodeKind::Transform => TRANSFORM_PARAMS,

            NodeKind::PointCurve => POINT_CURVE_PARAMS,
            NodeKind::Draw => DRAW_PARAMS,
            NodeKind::MetadataSplit => METADATA_SPLIT_PARAMS,
            NodeKind::ColorWheels => COLOR_WHEELS_PARAMS,
            NodeKind::Clarity => CLARITY_PARAMS,
            NodeKind::PixelSort => PIXEL_SORT_PARAMS,
            NodeKind::NormalMap => NORMALS_PARAMS,
            NodeKind::DepthMap | NodeKind::Group => &[],
            NodeKind::Generator => GENERATOR_PARAMS,
            NodeKind::Lut => LUT_PARAMS,
        }
    }

    pub fn inputs(self) -> &'static [InputSpec] {
        match self {
            NodeKind::Source | NodeKind::Group => &[],
            NodeKind::Generator | NodeKind::Draw => OPTIONAL_IMAGE_INPUTS,
            NodeKind::Merge => MERGE_INPUTS,
            NodeKind::Layer => LAYER_INPUTS,
            _ => IMAGE_INPUTS,
        }
    }

    /// All output ports: none for Output and Export, one for most kinds, seven for Split by Metadata.
    pub fn outputs(self) -> &'static [OutputSpec] {
        match self {
            NodeKind::Output | NodeKind::Export | NodeKind::Group => NO_OUTPUTS,
            NodeKind::MetadataSplit => SPLIT_OUTPUTS,
            _ => IMAGE_OUTPUTS,
        }
    }

    /// Type of the first output port (`None` for Output).
    pub fn output(self) -> Option<PortType> {
        self.outputs().first().map(|o| o.ty)
    }

    /// Kind of structured data this node stores besides its numeric parameters
    /// (the first of [`NodeKind::data_kinds`]).
    pub fn data_kind(self) -> Option<DataKind> {
        self.data_kinds().first().copied()
    }

    /// Every kind of structured data this node accepts. Curves take the legacy
    /// single `Curve` and the `CurveSet`.
    pub fn data_kinds(self) -> &'static [DataKind] {
        match self {
            NodeKind::PointCurve => &[DataKind::Curve, DataKind::CurveSet],
            NodeKind::Generator => &[DataKind::GradientRamp],
            NodeKind::Draw => &[DataKind::Strokes],
            NodeKind::MetadataSplit => &[DataKind::Ranges],
            NodeKind::MetadataFilter => &[DataKind::Keyword],
            NodeKind::Source => &[DataKind::Folder],
            NodeKind::Export => &[DataKind::Export],
            NodeKind::Lut => &[DataKind::LutFile],
            _ => &[],
        }
    }

    pub fn param_spec(self, name: &str) -> Option<&'static ParamSpec> {
        self.params().iter().find(|s| s.name == name)
    }
}

/// One brush stroke of a Draw. Points are normalised image coordinates
/// (0..1 in x and y, may slightly exceed), `radius` is a fraction of the long
/// edge (0.0005..0.5), `hardness` and `flow` are 0..1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrushStroke {
    pub points: Vec<[f32; 2]>,
    pub radius: f32,
    pub hardness: f32,
    pub flow: f32,
    pub erase: bool,
}

/// A named inclusive value range of a Split by Metadata node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedRange {
    pub name: String,
    pub min: f32,
    pub max: f32,
}

/// Structured per-node data beyond numeric parameters. Follows the same
/// All/Individual semantics as parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeData {
    GradientRamp(crate::generator::GradientRamp),
    /// (input, output) points on the perceptual axis, x strictly increasing.
    Curve(Vec<[f32; 2]>),
    Strokes(Vec<BrushStroke>),
    Ranges(Vec<NamedRange>),
    /// Source: absolute directory the images must lie in; "" = every image.
    Folder(String),
    /// Curves: several curves, one per channel.
    CurveSet(Vec<CurveEntry>),
    /// Export: folder, file name template and text metadata.
    Export(ExportText),
    /// LUT: absolute path of the `.cube` file; "" = none chosen.
    LutFile(String),
    /// Metadata Filter with the Keyword measure: the text to look for.
    Keyword(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataKind {
    GradientRamp,
    Curve,
    Strokes,
    Ranges,
    Folder,
    CurveSet,
    Export,
    LutFile,
    Keyword,
}

impl NodeData {
    pub fn kind(&self) -> DataKind {
        match self {
            NodeData::GradientRamp(_) => DataKind::GradientRamp,
            NodeData::Curve(_) => DataKind::Curve,
            NodeData::Strokes(_) => DataKind::Strokes,
            NodeData::Ranges(_) => DataKind::Ranges,
            NodeData::Folder(_) => DataKind::Folder,
            NodeData::CurveSet(_) => DataKind::CurveSet,
            NodeData::Export(_) => DataKind::Export,
            NodeData::LutFile(_) => DataKind::LutFile,
            NodeData::Keyword(_) => DataKind::Keyword,
        }
    }
}

/// Clamps brush parameters into their ranges (non-finite values stay and fail validation).
fn clamp_data(data: &mut NodeData) {
    if let NodeData::Strokes(strokes) = data {
        for s in strokes {
            s.radius = s.radius.clamp(BRUSH_RADIUS_MIN, BRUSH_RADIUS_MAX);
            s.hardness = s.hardness.clamp(0.0, 1.0);
            s.flow = s.flow.clamp(0.0, 1.0);
        }
    }
}

/// Checks `data` against the limits of the node kind.
fn validate_data(kind: NodeKind, data: &NodeData) -> Result<(), String> {
    match kind.data_kinds().first() {
        None => return Err(format!("{} takes no data", kind.label())),
        Some(first) if !kind.data_kinds().contains(&data.kind()) => {
            return Err(format!("{} takes {:?} data but got {:?}", kind.label(), first, data.kind()));
        }
        Some(_) => {}
    }
    match data {
        NodeData::GradientRamp(ramp) => ramp.validate().map_err(str::to_owned)?,
        NodeData::Curve(points) => validate_curve_points(points)?,
        NodeData::CurveSet(entries) => {
            if entries.len() > CURVE_CHANNEL_COUNT {
                return Err(format!("{} curves exceed the limit of {CURVE_CHANNEL_COUNT}", entries.len()));
            }
            for (i, e) in entries.iter().enumerate() {
                if e.channel as usize >= CURVE_CHANNEL_COUNT {
                    return Err(format!("curve channel {} does not exist", e.channel));
                }
                if entries[..i].iter().any(|o| o.channel == e.channel) {
                    return Err(format!("curve channel {} is used twice", e.channel));
                }
                validate_curve_points(&e.points)?;
            }
        }
        NodeData::Folder(path) => validate_text("folder", path)?,
        NodeData::LutFile(path) => validate_text("LUT file", path)?,
        NodeData::Keyword(text) => validate_text("keyword", text)?,
        NodeData::Export(t) => {
            validate_text("output folder", &t.out_dir)?;
            validate_text("name template", &t.name_template)?;
            validate_text("artist", &t.artist)?;
            validate_text("copyright", &t.copyright)?;
            validate_text("description", &t.description)?;
        }
        NodeData::Strokes(strokes) => {
            if strokes.len() > MAX_STROKES {
                return Err(format!("{} strokes exceed the limit of {MAX_STROKES}", strokes.len()));
            }
            for (i, s) in strokes.iter().enumerate() {
                if s.points.len() > MAX_STROKE_POINTS {
                    return Err(format!("stroke {i} has {} points, the limit is {MAX_STROKE_POINTS}", s.points.len()));
                }
                if !s.points.iter().all(|p| p[0].is_finite() && p[1].is_finite()) {
                    return Err(format!("stroke {i} has a non-finite point"));
                }
                let ok = |v: f32, lo: f32, hi: f32| v.is_finite() && (lo..=hi).contains(&v);
                if !ok(s.radius, BRUSH_RADIUS_MIN, BRUSH_RADIUS_MAX) {
                    return Err(format!("stroke {i} radius {} is outside {BRUSH_RADIUS_MIN}..{BRUSH_RADIUS_MAX}", s.radius));
                }
                if !ok(s.hardness, 0.0, 1.0) || !ok(s.flow, 0.0, 1.0) {
                    return Err(format!("stroke {i} hardness or flow is outside 0..1"));
                }
            }
        }
        NodeData::Ranges(ranges) => {
            if ranges.len() > MAX_RANGES {
                return Err(format!("{} ranges exceed the limit of {MAX_RANGES}", ranges.len()));
            }
            for (i, r) in ranges.iter().enumerate() {
                let n = r.name.chars().count();
                if n == 0 || n > MAX_RANGE_NAME_CHARS {
                    return Err(format!("range {i}: the name must have 1..={MAX_RANGE_NAME_CHARS} characters"));
                }
                if !r.min.is_finite() || !r.max.is_finite() || r.min > r.max {
                    return Err(format!("range \"{}\": min and max must be finite with min <= max", r.name));
                }
                if ranges[..i].iter().any(|o| o.name == r.name) {
                    return Err(format!("range name \"{}\" is used twice", r.name));
                }
            }
        }
    }
    Ok(())
}

fn validate_curve_points(points: &[[f32; 2]]) -> Result<(), String> {
    if !(MIN_CURVE_POINTS..=MAX_CURVE_POINTS).contains(&points.len()) {
        return Err(format!("curve has {} points, expected {MIN_CURVE_POINTS}..={MAX_CURVE_POINTS}", points.len()));
    }
    for p in points {
        if !p.iter().all(|v| v.is_finite() && (0.0..=1.0).contains(v)) {
            return Err(format!("curve point {p:?} is not finite or outside 0..1"));
        }
    }
    if points.windows(2).any(|w| w[0][0] >= w[1][0]) {
        return Err("curve x values must be strictly increasing".to_owned());
    }
    Ok(())
}

/// Text limits of the folder and Export fields: at most 1024 characters, no NUL.
fn validate_text(what: &str, text: &str) -> Result<(), String> {
    if text.chars().count() > MAX_EXPORT_TEXT_CHARS {
        return Err(format!("{what} is longer than {MAX_EXPORT_TEXT_CHARS} characters"));
    }
    if text.contains('\0') {
        return Err(format!("{what} contains a NUL character"));
    }
    Ok(())
}

fn is_zero(v: &usize) -> bool {
    *v == 0
}

/// An exposed image control input. These are not
/// collection inputs. Indices are stable slots, never reused after removal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposedInput {
    pub parameter: String,
    pub ty: PortType,
}

/// An additional output alias, routed to an existing built-in output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExposedOutput {
    pub label: String,
    pub source_port: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    pub mode: Mode,
    /// Values used in All mode and inherited by images without their own value.
    all: BTreeMap<String, f32>,
    /// Values keyed by image identity. Kept when the node is switched back to
    /// All mode, so toggling the mode never discards edits.
    individual: BTreeMap<ImageId, BTreeMap<String, f32>>,
    /// Structured data used in All mode and inherited by images without their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<NodeData>,
    /// Structured data keyed by image identity; kept across mode switches like `individual`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    individual_data: BTreeMap<ImageId, NodeData>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exposed_inputs: Vec<Option<ExposedInput>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exposed_outputs: Vec<Option<ExposedOutput>>,
    /// Position in the graph editor.
    pub pos: [f32; 2],
    /// The definition of a [`NodeKind::Group`] node; `None` for every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<GroupDef>,
}

impl Node {
    /// The group definition of a Group node.
    pub fn group(&self) -> Option<&GroupDef> { self.group.as_ref() }
    /// Number of input ports of the box (built-in, exposed and group ports).
    pub fn input_len(&self) -> usize {
        match &self.group { Some(g) => g.inputs.len(), None => self.kind.inputs().len() + self.exposed_inputs.len() }
    }
    /// Number of output ports of the box (built-in, exposed and group ports).
    pub fn output_len(&self) -> usize {
        match &self.group { Some(g) => g.outputs.len(), None => self.kind.outputs().len() + self.exposed_outputs.len() }
    }

    pub fn exposed_inputs(&self) -> &[Option<ExposedInput>] { &self.exposed_inputs }
    pub fn exposed_outputs(&self) -> &[Option<ExposedOutput>] { &self.exposed_outputs }

    /// Resolves either a built-in or exposed input, preserving its collection role.
    pub fn input_type(&self, port: usize) -> Option<PortType> {
        if let Some(g) = &self.group { return g.inputs.get(port).map(|_| PortType::Images); }
        if let Some(spec) = self.kind.inputs().get(port) { return Some(spec.ty); }
        self.exposed_inputs.get(port.checked_sub(self.kind.inputs().len())?)?.as_ref().map(|p| p.ty)
    }
    pub fn output_type(&self, port: usize) -> Option<PortType> {
        if let Some(g) = &self.group { return g.outputs.get(port).map(|_| PortType::Images); }
        self.kind.outputs().get(self.resolve_output(port)?).map(|p| p.ty)
    }
    pub fn resolve_output(&self, port: usize) -> Option<usize> {
        if port < self.kind.outputs().len() { return Some(port); }
        let alias = self.exposed_outputs.get(port.checked_sub(self.kind.outputs().len())?)?.as_ref()?;
        self.kind.outputs().get(alias.source_port).map(|_| alias.source_port)
    }
    pub fn is_collection_input(&self, port: usize) -> bool {
        if let Some(g) = &self.group { return port < g.inputs.len(); }
        port < self.kind.inputs().len()
    }
    /// Whether this auxiliary input controls a parameter or direction per pixel.
    pub fn is_control_input(&self, port: usize) -> bool {
        port.checked_sub(self.kind.inputs().len()).and_then(|i| self.exposed_inputs.get(i)).is_some_and(Option::is_some)
    }
    /// Effective value of a parameter for one image.
    pub fn param(&self, name: &str, image: ImageId) -> f32 {
        if self.mode == Mode::Individual {
            if let Some(v) = self.individual.get(&image).and_then(|m| m.get(name)) {
                return *v;
            }
        }
        self.all
            .get(name)
            .copied()
            .unwrap_or_else(|| self.kind.param_spec(name).map_or(0.0, |s| s.default))
    }

    /// Stores a value: for every image in All mode, for `image` only in
    /// Individual mode. The value is clamped to the parameter's range.
    /// Unknown parameter names are ignored and reported as `false`.
    pub fn set_param(&mut self, name: &str, image: ImageId, value: f32) -> bool {
        let Some(spec) = self.kind.param_spec(name) else { return false };
        let value = if !value.is_finite() {
            spec.default
        } else {
            match spec.kind {
                ParamKind::Continuous => value.clamp(spec.min, spec.max),
                // Nearest valid index / nearest of off and on.
                ParamKind::Choice(_) => value.round().clamp(spec.min, spec.max),
                ParamKind::Toggle => {
                    if value >= 0.5 {
                        1.0
                    } else {
                        0.0
                    }
                }
            }
        };
        match self.mode {
            Mode::All => self.all.insert(name.to_owned(), value),
            Mode::Individual => self.individual.entry(image).or_default().insert(name.to_owned(), value),
        };
        true
    }

    /// Selected option of a Choice parameter (0 for anything that is not one).
    pub fn choice(&self, name: &str, image: ImageId) -> usize {
        let v = self.param(name, image);
        if v.is_finite() && v > 0.0 {
            v.round() as usize
        } else {
            0
        }
    }

    /// State of a Toggle parameter.
    pub fn toggle(&self, name: &str, image: ImageId) -> bool {
        self.param(name, image) >= 0.5
    }

    /// The All value of a parameter, whatever the mode: collection-level
    /// settings (Source folder options, Merge collision, Layer mode) cannot
    /// differ per image. Non-finite values read as the default.
    pub(crate) fn all_value(&self, name: &str) -> f32 {
        let default = self.kind.param_spec(name).map_or(0.0, |s| s.default);
        match self.all.get(name).copied() {
            Some(v) if v.is_finite() => v,
            _ => default,
        }
    }

    /// [`Node::all_value`] as an option index.
    pub(crate) fn all_choice(&self, name: &str) -> usize {
        let v = self.all_value(name);
        if v > 0.0 {
            v.round() as usize
        } else {
            0
        }
    }

    /// [`Node::all_value`] as a toggle.
    pub(crate) fn all_toggle(&self, name: &str) -> bool {
        self.all_value(name) >= 0.5
    }

    /// The structured data of the All scope, whatever the mode.
    pub(crate) fn all_data(&self) -> Option<&NodeData> {
        self.data.as_ref()
    }

    /// Stores a value without any check (tests of how bad values are read).
    #[cfg(test)]
    pub(crate) fn set_raw(&mut self, name: &str, value: f32) {
        self.all.insert(name.to_owned(), value);
    }

    /// Effective structured data for one image: in Individual mode the image's
    /// own data if it has any, otherwise the All data.
    pub fn data(&self, image: ImageId) -> Option<&NodeData> {
        if self.mode == Mode::Individual {
            if let Some(d) = self.individual_data.get(&image) {
                return Some(d);
            }
        }
        self.data.as_ref()
    }

    /// Stores data for every image (All mode) or for `image` only (Individual
    /// mode); `None` removes it. Data of the wrong kind or outside the limits is
    /// rejected (`false`); brush radius, hardness and flow are clamped first.
    pub fn set_data(&mut self, image: ImageId, data: Option<NodeData>) -> bool {
        let Some(mut data) = data else {
            match self.mode {
                Mode::All => self.data = None,
                Mode::Individual => {
                    self.individual_data.remove(&image);
                }
            }
            return true;
        };
        clamp_data(&mut data);
        if validate_data(self.kind, &data).is_err() {
            return false;
        }
        match self.mode {
            Mode::All => self.data = Some(data),
            Mode::Individual => {
                self.individual_data.insert(image, data);
            }
        }
        true
    }

    /// True if `image` has its own structured data on this node.
    pub fn has_individual_data(&self, image: ImageId) -> bool {
        self.individual_data.contains_key(&image)
    }

    /// True if `image` has its own stored values or data on this node.
    pub fn has_individual(&self, image: ImageId) -> bool {
        self.individual.get(&image).is_some_and(|m| !m.is_empty()) || self.has_individual_data(image)
    }

    /// Removes the per-image values and data of `image`, so it inherits again.
    pub fn clear_individual(&mut self, image: ImageId) {
        self.individual.remove(&image);
        self.individual_data.remove(&image);
    }

    /// Number of images with their own stored values or data.
    pub fn individual_count(&self) -> usize {
        let mut images: BTreeSet<ImageId> =
            self.individual.iter().filter(|(_, m)| !m.is_empty()).map(|(i, _)| *i).collect();
        images.extend(self.individual_data.keys().copied());
        images.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub from: NodeId,
    /// Output port of `from`; absent in files written before nodes had several outputs.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub from_port: usize,
    pub to: NodeId,
    pub to_port: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    UnknownNode(NodeId),
    /// A bad input port, or a bad output port on the source node of a connection.
    NoSuchPort { node: NodeId, port: usize },
    NoOutput(NodeId),
    TypeMismatch { expected: PortType, found: PortType },
    WouldCycle,
    UnsupportedVersion(u32),
    /// A sub-graph (clipboard, recipe) of a newer format.
    UnsupportedSubgraphVersion(u32),
    Parse(String),
    /// A group operation that cannot be done (or a group structure that is not valid).
    Group(String),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::UnknownNode(id) => write!(f, "node {} does not exist", id.0),
            GraphError::NoSuchPort { node, port } => write!(f, "node {} has no input {}", node.0, port),
            GraphError::NoOutput(id) => write!(f, "node {} has no output", id.0),
            GraphError::TypeMismatch { expected, found } => {
                write!(f, "port expects {expected:?} but the connection carries {found:?}")
            }
            GraphError::WouldCycle => write!(f, "connection would create a cycle"),
            GraphError::UnsupportedVersion(v) => {
                write!(f, "graph format version {v} is newer than this build supports ({GRAPH_FORMAT_VERSION})")
            }
            GraphError::UnsupportedSubgraphVersion(v) => {
                write!(f, "sub-graph format version {v} is newer than this build supports ({SUBGRAPH_FORMAT_VERSION})")
            }
            GraphError::Parse(e) => write!(f, "graph file is not valid: {e}"),
            GraphError::Group(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GraphError {}

/// A problem that prevents a node from being evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub node: NodeId,
    pub message: String,
    /// Only some images are affected (a LUT file that cannot be read). The viewer passes those
    /// through and an export fails just the affected items, so it must not stop a whole run.
    pub per_item: bool,
}

/// Longest walk along a chain of nodes (graphs are acyclic, this only guards
/// against hand-built nonsense).
const MAX_WALK: usize = 512;

/// The last known state of the LUT file named by a LUT node's data, without touching the disk
/// (see [`crate::lut::peek`]); `None` when no file is chosen.
pub fn lut_file_status(path: &str) -> Option<crate::lut::LutState> {
    if path.trim().is_empty() {
        return None;
    }
    Some(crate::lut::peek(std::path::Path::new(path)))
}

/// The LUT file path a LUT node uses for `image` (`None` when no file is chosen).
pub fn lut_file(node: &Node, image: ImageId) -> Option<&str> {
    match node.data(image) {
        Some(NodeData::LutFile(p)) if !p.trim().is_empty() => Some(p.as_str()),
        _ => None,
    }
}

/// The folder a Source restricts its images to and whether subfolders count.
fn source_folder(n: &Node) -> Option<(&str, bool)> {
    match n.all_data() {
        Some(NodeData::Folder(f)) => Some((f.as_str(), n.all_toggle("subfolders"))),
        _ => None,
    }
}

/// The output port a Split by Metadata sends an image through, given the
/// image's capture (see [`Graph::split_port`]).
fn split_value(n: &Node, image: ImageId, cap: Option<&CaptureInfo>) -> Option<usize> {
    let measure = Measure::from_index(n.choice("measure", image)).unwrap_or(Measure::Iso);
    if measure == Measure::Keyword {
        // Each named range is a keyword test (first match wins); the rest is unmatched. "Missing
        // metadata" does not apply: an image without keywords contains nothing.
        let keywords: &[String] = cap.map_or(&[], |c| c.keywords.as_slice());
        let negate = n.choice("keyword_op", image) == 1;
        let port = match n.data(image) {
            Some(NodeData::Ranges(ranges)) => ranges.iter().position(|r| crate::keywords::keywords_contain_text(keywords, &r.name) != negate),
            _ => None,
        };
        return Some(port.unwrap_or(SPLIT_UNMATCHED_PORT));
    }
    let Some(value) = cap.and_then(|c| c.measure(measure)) else {
        return if n.choice("missing", image) == 1 { None } else { Some(SPLIT_UNMATCHED_PORT) };
    };
    let in_range = match n.data(image) {
        Some(NodeData::Ranges(ranges)) => ranges.iter().position(|r| r.min <= value && value <= r.max),
        _ => None,
    };
    Some(in_range.unwrap_or(SPLIT_UNMATCHED_PORT))
}

/// State of one collection query: the capture of every image is fetched at
/// most once, and collections already listed are remembered.
/// A shared list of images.
type Listing = Rc<Vec<ImageId>>;
/// The images of a Merge with the input port each comes from.
type MergeEntries = Vec<(ImageId, usize)>;

struct Ctx<'c> {
    catalog: Catalog<'c>,
    caps: RefCell<HashMap<ImageId, Option<Rc<CaptureInfo>>>>,
    members: RefCell<HashMap<(NodeId, usize), Listing>>,
    merges: RefCell<HashMap<NodeId, Rc<MergeEntries>>>,
    depth: Cell<usize>,
}

impl<'c> Ctx<'c> {
    fn new(catalog: &Catalog<'c>) -> Self {
        Ctx {
            catalog: *catalog,
            caps: RefCell::new(HashMap::new()),
            members: RefCell::new(HashMap::new()),
            merges: RefCell::new(HashMap::new()),
            depth: Cell::new(0),
        }
    }

    fn cap(&self, id: ImageId) -> Option<Rc<CaptureInfo>> {
        if let Some(c) = self.caps.borrow().get(&id) {
            return c.clone();
        }
        let c = (self.catalog.capture)(id).map(Rc::new);
        self.caps.borrow_mut().insert(id, c.clone());
        c
    }

    /// Counts one level of recursion; false when the walk got absurdly deep.
    fn enter(&self) -> bool {
        let d = self.depth.get();
        if d >= MAX_WALK {
            return false;
        }
        self.depth.set(d + 1);
        true
    }

    fn leave(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }

    /// A Merge that never needs to look at file names.
    fn merge_is_plain(&self, merge: &Node) -> bool {
        self.catalog.images.is_empty() || merge.all_choice("collision") == 0
    }
}

/// The node graph. Nodes are reference counted: cloning a graph (undo
/// snapshots, render copies) shares every node and only [`Graph::node_mut`]
/// copies the one node it changes (copy on write).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Graph {
    format_version: u32,
    #[serde(with = "shared_nodes")]
    nodes: BTreeMap<NodeId, Arc<Node>>,
    edges: Vec<Edge>,
    next_id: u64,
}

impl PartialEq for Graph {
    fn eq(&self, other: &Self) -> bool {
        self.format_version == other.format_version
            && self.next_id == other.next_id
            && self.edges == other.edges
            && self.nodes.len() == other.nodes.len()
            // A node shared by both graphs is equal without looking at it.
            && self.nodes.iter().zip(&other.nodes).all(|((ka, a), (kb, b))| ka == kb && (Arc::ptr_eq(a, b) || **a == **b))
    }
}

/// Serde adapter: the file format stores plain nodes.
mod shared_nodes {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use serde::{Deserialize, Deserializer, Serializer};

    use super::{Node, NodeId};

    pub fn serialize<S: Serializer>(nodes: &BTreeMap<NodeId, Arc<Node>>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(nodes.iter().map(|(k, v)| (k, &**v)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<NodeId, Arc<Node>>, D::Error> {
        let plain = BTreeMap::<NodeId, Node>::deserialize(d)?;
        Ok(plain.into_iter().map(|(k, v)| (k, Arc::new(v))).collect())
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self { format_version: GRAPH_FORMAT_VERSION, nodes: BTreeMap::new(), edges: Vec::new(), next_id: 1 }
    }
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, kind: NodeKind, pos: [f32; 2]) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        // Draw starts per image so painting can differ across a collection.
        let mode = if kind == NodeKind::Draw { Mode::Individual } else { Mode::All };
        self.nodes.insert(
            id,
            Arc::new(Node {
                id,
                kind,
                mode,
                all: BTreeMap::new(),
                individual: BTreeMap::new(),
                data: None,
                individual_data: BTreeMap::new(),
                exposed_inputs: Vec::new(),
                exposed_outputs: Vec::new(),
                pos,
                group: None,
            }),
        );
        id
    }

    /// Parameters with implemented per-pixel evaluation.
    pub fn supported_parameter_inputs(kind: NodeKind) -> &'static [&'static str] {
        match kind {
            NodeKind::Develop => &["exposure", "contrast", "highlights", "shadows", "whites", "blacks"],
            NodeKind::Exposure => &["ev"],
            NodeKind::Contrast | NodeKind::Saturation | NodeKind::Sharpen => &["amount"],
            NodeKind::Blur => &["radius", "direction"],
            _ => &[],
        }
    }

    /// Label of an implemented image control, including non-scalar inputs.
    pub fn parameter_input_label(kind: NodeKind, parameter: &str) -> Option<&'static str> {
        if !Self::supported_parameter_inputs(kind).contains(&parameter) { return None; }
        if kind == NodeKind::Blur && parameter == "direction" { return Some("Direction (RGB)"); }
        kind.param_spec(parameter).map(|p| p.label)
    }

    /// Expose a parameter as an auxiliary image control input.
    pub fn expose_parameter(&mut self, id: NodeId, parameter: &str, ty: PortType) -> Result<usize, GraphError> {
        let node = self.nodes.get_mut(&id).map(Arc::make_mut).ok_or(GraphError::UnknownNode(id))?;
        if !Self::supported_parameter_inputs(node.kind).contains(&parameter)
            || node.exposed_inputs.iter().flatten().any(|p| p.parameter == parameter) {
            return Err(GraphError::Parse(format!("invalid or already exposed parameter {parameter}")));
        }
        let port = node.kind.inputs().len() + node.exposed_inputs.len();
        node.exposed_inputs.push(Some(ExposedInput { parameter: parameter.to_owned(), ty }));
        Ok(port)
    }

    /// Expose an alias of a built-in output; aliases never point at aliases.
    pub fn expose_output(&mut self, id: NodeId, source_port: usize, label: &str) -> Result<usize, GraphError> {
        let node = self.nodes.get_mut(&id).map(Arc::make_mut).ok_or(GraphError::UnknownNode(id))?;
        if node.kind.outputs().get(source_port).is_none() { return Err(GraphError::NoSuchPort { node: id, port: source_port }); }
        if label.trim().is_empty() || label.chars().count() > 128 || label.contains('\0') { return Err(GraphError::Parse("invalid output label".into())); }
        let port = node.kind.outputs().len() + node.exposed_outputs.len();
        node.exposed_outputs.push(Some(ExposedOutput { label: label.to_owned(), source_port }));
        Ok(port)
    }
    pub fn remove_exposed_input(&mut self, id: NodeId, port: usize) -> bool {
        let Some(node) = self.nodes.get_mut(&id).map(Arc::make_mut) else { return false };
        let Some(slot) = port.checked_sub(node.kind.inputs().len()).and_then(|i| node.exposed_inputs.get_mut(i)) else { return false };
        if slot.take().is_none() { return false; }
        self.edges.retain(|e| e.to != id || e.to_port != port);
        self.drop_group_ports_of(id, true, port);
        true
    }
    pub fn remove_exposed_output(&mut self, id: NodeId, port: usize) -> bool {
        let Some(node) = self.nodes.get_mut(&id).map(Arc::make_mut) else { return false };
        let Some(slot) = port.checked_sub(node.kind.outputs().len()).and_then(|i| node.exposed_outputs.get_mut(i)) else { return false };
        if slot.take().is_none() { return false; }
        self.edges.retain(|e| e.from != id || e.from_port != port);
        self.drop_group_ports_of(id, false, port);
        true
    }

    /// Removes a node and its wires. Removing a group removes everything inside it.
    pub fn remove_node(&mut self, id: NodeId) {
        self.remove_node_tree(id);
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id).map(|n| &**n)
    }

    /// Mutable access to one node. A node that other graphs (undo snapshots)
    /// still share is copied first; the other nodes stay shared.
    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.get_mut(&id).map(Arc::make_mut)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values().map(|n| &**n)
    }

    /// Address of the allocation behind node `id`: two graphs that report the
    /// same value share that node's memory (undo history accounting, tests).
    pub fn node_identity(&self, id: NodeId) -> Option<usize> {
        self.nodes.get(&id).map(|n| Arc::as_ptr(n) as usize)
    }

    pub fn edges(&self) -> &[Edge] {
        &self.edges
    }

    /// The node feeding input `port` of `node`, if connected.
    pub fn input_of(&self, node: NodeId, port: usize) -> Option<NodeId> {
        let (node, port) = self.resolve_input(node, port);
        self.edges.iter().find(|e| e.to == node && e.to_port == port).map(|e| e.from)
    }

    /// True if `node` is `target` or depends on it through any input.
    pub fn depends_on(&self, node: NodeId, target: NodeId) -> bool {
        node == target || self.edges.iter().filter(|e| e.to == node).any(|e| self.depends_on(e.from, target))
    }

    /// The node feeding input `port` of `node` and the output port it leaves through.
    pub fn input_edge(&self, node: NodeId, port: usize) -> Option<(NodeId, usize)> {
        let (node, port) = self.resolve_input(node, port);
        self.edges.iter().find(|e| e.to == node && e.to_port == port).map(|e| (e.from, e.from_port))
    }

    /// All wires feeding an input, in graph insertion order.
    pub fn input_edges(&self, node: NodeId, port: usize) -> impl Iterator<Item = &Edge> {
        let (node, port) = self.resolve_input(node, port);
        self.edges.iter().filter(move |e| e.to == node && e.to_port == port)
    }

    /// Connects the first output of `from` to input `port` of `to`
    /// (see [`Graph::connect_ports`]).
    pub fn connect(&mut self, from: NodeId, to: NodeId, port: usize) -> Result<(), GraphError> {
        self.connect_ports(from, 0, to, port)
    }

    /// Connects output `from_port` of `from` to input `to_port` of `to`,
    /// adding distinct wires for ordinary image inputs; Merge, Layer and masks
    /// replace the existing input. Rejects unknown
    /// ports, type mismatches and cycles.
    pub fn connect_ports(
        &mut self,
        from: NodeId,
        from_port: usize,
        to: NodeId,
        to_port: usize,
    ) -> Result<(), GraphError> {
        // Group ports stand for the inner ports behind them: wires are always stored between real nodes.
        let (from, from_port) = self.resolve_output(from, from_port);
        let (to, to_port) = self.resolve_input(to, to_port);
        let src = self.node(from).ok_or(GraphError::UnknownNode(from))?;
        let dst = self.node(to).ok_or(GraphError::UnknownNode(to))?;
        let outputs = src.kind.outputs();
        if outputs.is_empty() && src.exposed_outputs.iter().all(Option::is_none) && src.group.is_none() {
            return Err(GraphError::NoOutput(from));
        }
        let found = src.output_type(from_port).ok_or(GraphError::NoSuchPort { node: from, port: from_port })?;
        let expected = dst.input_type(to_port).ok_or(GraphError::NoSuchPort { node: to, port: to_port })?;
        if expected != found && !dst.is_control_input(to_port) {
            return Err(GraphError::TypeMismatch { expected, found });
        }
        if self.depends_on(from, to) {
            return Err(GraphError::WouldCycle);
        }
        let multi = dst.is_collection_input(to_port) && !matches!(dst.kind, NodeKind::Merge | NodeKind::Layer);
        if multi {
            if self.edges.iter().any(|e| e.from == from && e.from_port == from_port && e.to == to && e.to_port == to_port) { return Ok(()); }
        } else {
            self.edges.retain(|e| !(e.to == to && e.to_port == to_port));
        }
        let edge = Edge { from, from_port, to, to_port };
        self.edges.push(edge);
        self.ensure_boundary_ports(edge);
        Ok(())
    }

    /// Label of an output port for display: the static label, or for a Split by
    /// Metadata the name of the range defined for `image` (its own ranges in
    /// Individual mode) and "Unmatched" for the last port. Empty for unknown ports.
    pub fn output_label(&self, node: NodeId, port: usize, image: ImageId) -> String {
        let Some(n) = self.node(node) else { return String::new() };
        if let Some(g) = n.group() { return g.outputs.get(port).map(|p| p.label.clone()).unwrap_or_default(); }
        if let Some(Some(alias)) = port.checked_sub(n.kind.outputs().len()).and_then(|i| n.exposed_outputs.get(i)) { return alias.label.clone(); }
        let Some(spec) = n.kind.outputs().get(port) else { return String::new() };
        if n.kind == NodeKind::MetadataSplit && port < SPLIT_RANGES {
            if let Some(NodeData::Ranges(ranges)) = n.data(image) {
                if let Some(r) = ranges.get(port) {
                    return r.name.clone();
                }
            }
        }
        spec.label.to_owned()
    }

    pub fn disconnect(&mut self, to: NodeId, port: usize) {
        let (to, port) = self.resolve_input(to, port);
        self.edges.retain(|e| !(e.to == to && e.to_port == port));
    }

    /// Disconnect exactly one wire, leaving other wires on the input untouched.
    pub fn disconnect_edge(&mut self, edge: Edge) -> bool {
        if let Some(index) = self.edges.iter().position(|e| *e == edge) {
            self.edges.remove(index);
            true
        } else {
            false
        }
    }

    /// Lists everything that keeps nodes from evaluating (missing required
    /// inputs, a Merge or Layer without any input, an Export without an output
    /// folder, a Lens Correction in Auto mode that cannot work, and the Layer
    /// mode that is not built yet).
    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        for node in self.nodes.values() {
            let mut add_with = |message: String, per_item: bool| issues.push(Issue { node: node.id, message, per_item });
            let label = node.kind.label();
            macro_rules! add {
                ($m:expr $(,)?) => {
                    add_with($m, false)
                };
            }
            if matches!(node.kind, NodeKind::Merge | NodeKind::Layer)
                && (0..node.kind.inputs().len()).all(|p| self.input_of(node.id, p).is_none())
            {
                add!(format!("{label}: no input is connected"));
            }
            for (port, spec) in node.kind.inputs().iter().enumerate() {
                if spec.required && self.input_of(node.id, port).is_none() {
                    add!(format!("{label}: input \"{}\" is not connected", spec.label));
                }
            }
            match node.kind {
                NodeKind::Layer if node.all_choice("mode") == 1 => {
                    add!("All combinations is not available yet; Match is used".to_owned());
                }
                NodeKind::Export => {
                    let dir_set = matches!(node.all_data(), Some(NodeData::Export(t)) if !t.out_dir.trim().is_empty());
                    if !dir_set {
                        add!("Export: no output folder set".to_owned());
                    }
                }
                NodeKind::Lut => {
                    // Only the data in effect: in All mode the All file; in Individual mode the own
                    // file of each image that has one, plus the All file the other images inherit.
                    // A problem is reported, the node passes its input through meanwhile. The UI
                    // calls this every frame, so it only reads the last known state of the files.
                    let all = node.data.iter().map(|d| (None, d));
                    let own = node.individual_data.iter().filter(|_| node.mode == Mode::Individual).map(|(i, d)| (Some(*i), d));
                    for (image, data) in all.chain(own) {
                        let NodeData::LutFile(path) = data else { continue };
                        if let Some(crate::lut::LutState::Failed(e)) = lut_file_status(path) {
                            let whose = image.map_or(String::new(), |i| format!(" (image {})", i.0));
                            add_with(format!("LUT{whose}: {path}: {e}; the image passes through unchanged"), true);
                        }
                    }
                }
                NodeKind::LensManual if node.all_choice("mode") == 1 && !self.lens_auto_usable(node.id) => {
                    add!(
                        "Lens Correction: Auto needs the uncropped frame of a source (no Crop / Rotate, Layer or \
                         other Lens Correction in between)"
                            .to_owned(),
                    );
                }
                _ => {}
            }
        }
        issues
    }

    /// The first LUT file problem that would hit `image` when it is evaluated at `node`: a LUT node
    /// upstream (strength above 0) whose file for this image cannot be read. Reads the files, so it
    /// is for export threads, not the UI. `None` when every LUT is fine or no file is chosen.
    pub fn lut_problem(&self, node: NodeId, image: ImageId) -> Option<String> {
        let mut upstream = BTreeSet::from([node]);
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            for e in self.edges.iter().filter(|e| e.to == n) {
                if upstream.insert(e.from) {
                    stack.push(e.from);
                }
            }
        }
        for id in upstream {
            let Some(n) = self.node(id).filter(|n| n.kind == NodeKind::Lut) else { continue };
            let strength = n.param("strength", image);
            if strength.is_nan() || strength <= 0.0 {
                continue;
            }
            let Some(path) = lut_file(n, image) else { continue };
            if let Err(e) = crate::lut::load_cached(std::path::Path::new(path)) {
                return Some(format!("LUT {path}: {e}"));
            }
        }
        None
    }

    /// True if the Lens Correction `node` can use the lens profile of its
    /// images: walking upstream from its image input reaches a Source through
    /// nodes that keep the frame. A Crop / Rotate, another Lens Correction or a
    /// Layer in between (the profile describes the full upright frame), a
    /// missing input or a node that is not a Lens Correction give `false`.
    /// At a Merge the first connected input counts.
    pub fn lens_auto_usable(&self, node: NodeId) -> bool {
        if !self.node(node).is_some_and(|n| n.kind == NodeKind::LensManual) {
            return false;
        }
        let Some(mut cur) = self.input_of(node, 0) else { return false };
        for _ in 0..MAX_WALK {
            let Some(n) = self.node(cur) else { return false };
            match n.kind {
                NodeKind::Source => return true,
                NodeKind::Transform | NodeKind::LensManual | NodeKind::Layer => return false,
                _ => {}
            }
            let next = n
                .kind
                .inputs()
                .iter()
                .enumerate()
                .filter(|(_, s)| s.ty == PortType::Images)
                .find_map(|(port, _)| self.input_of(cur, port));
            match next {
                Some(up) => cur = up,
                None => return false,
            }
        }
        false
    }

    /// Sum of all Capture EV changes that apply to `image` at `node`
    /// (the node itself and everything upstream along the image input).
    /// At a Merge the first connected input (in port order) counts, at a Layer
    /// the lowest connected layer. Only image ports carry the collection;
    /// optional mask inputs never count.
    pub fn capture_ev_delta(&self, node: NodeId, image: ImageId) -> f32 {
        let mut total = 0.0;
        let mut cur = node;
        for _ in 0..MAX_WALK {
            let Some(n) = self.node(cur) else { break };
            if n.kind == NodeKind::CaptureEv {
                let d = n.param("delta", image);
                if d.is_finite() {
                    total += d;
                }
            }
            let next = match n.kind.inputs().first() {
                Some(spec) if spec.ty == PortType::Images => n
                    .kind
                    .inputs()
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.ty == PortType::Images)
                    .find_map(|(port, _)| self.input_of(cur, port)),
                _ => None,
            };
            match next {
                Some(up) => cur = up,
                None => break,
            }
        }
        total
    }

    /// True if `image` is part of the collection that `node` outputs.
    ///
    /// Same as [`Graph::contains_in`] with an empty catalog: a Source without
    /// folder contains every image, a Merge keeps both of equally named
    /// images, a Layer contains what any of its inputs contains.
    pub fn contains(&self, node: NodeId, image: ImageId, capture: CaptureFn) -> bool {
        self.contains_in(node, image, &Catalog { images: &[], capture })
    }

    /// True if `image` is part of the collection that `node` outputs.
    ///
    /// A Source contains every image (with a folder set: the images whose
    /// capture path lies in it, see [`path_in_folder`]; an unknown path is not
    /// contained), mask producers count as containing everything, a Metadata
    /// Filter keeps what its input has and its rule accepts, a Merge unites its
    /// inputs (resolving same-named images by its `collision` option when the
    /// catalog lists images), a Layer's collection is its driver's (see
    /// [`Graph::layer_sources`]), and every other node passes its input
    /// through. Unknown nodes and unconnected required inputs give false.
    ///
    /// Every consumer follows its input edge including the output port: an
    /// input fed from range `k` of a Split by Metadata only contains the images
    /// that leave the split through `k`. For a Split itself this answers "is in
    /// the input collection" (any port).
    ///
    /// Collection-level settings (a Source's folder and `subfolders`, a Merge's
    /// `collision`, a Layer's `mode`) are read from the node's All values even
    /// in Individual mode.
    pub fn contains_in(&self, node: NodeId, image: ImageId, catalog: &Catalog) -> bool {
        let ctx = Ctx::new(catalog);
        self.contains_ctx(&ctx, node, image)
    }

    /// True if `image` is in the collection leaving output `port` of `node`.
    /// A Split by Metadata carries an image on exactly one port (see
    /// [`Graph::split_port`]); a range output without a defined range carries
    /// nothing. Single-output nodes only have port 0.
    pub fn contains_output(&self, node: NodeId, port: usize, image: ImageId, capture: CaptureFn) -> bool {
        self.contains_output_in(node, port, image, &Catalog { images: &[], capture })
    }

    /// [`Graph::contains_output`] with a catalog.
    pub fn contains_output_in(&self, node: NodeId, port: usize, image: ImageId, catalog: &Catalog) -> bool {
        let ctx = Ctx::new(catalog);
        self.contains_output_ctx(&ctx, node, port, image)
    }

    /// For a Split by Metadata: the output port `image` leaves through, the
    /// first range (in order) containing the measure value, else the
    /// Unmatched port. `None` if the image is not in the input collection, or
    /// it has no value for the measure and `missing` is "Exclude" (with
    /// "Unmatched" such images go to the Unmatched port).
    pub fn split_port(&self, node: NodeId, image: ImageId, capture: CaptureFn) -> Option<usize> {
        self.split_port_in(node, image, &Catalog { images: &[], capture })
    }

    /// [`Graph::split_port`] with a catalog.
    pub fn split_port_in(&self, node: NodeId, image: ImageId, catalog: &Catalog) -> Option<usize> {
        let ctx = Ctx::new(catalog);
        self.split_port_ctx(&ctx, node, image)
    }

    /// For a Merge node: the input port that supplies `image`, the first
    /// connected input in port order whose collection contains it.
    pub fn merge_source(&self, node: NodeId, image: ImageId, capture: CaptureFn) -> Option<usize> {
        self.merge_source_in(node, image, &Catalog { images: &[], capture })
    }

    /// [`Graph::merge_source`] with a catalog: after the `collision` option has
    /// removed the losing same-named images, the port of the entry that stays
    /// (`None` if the image is dropped or not in any input).
    pub fn merge_source_in(&self, node: NodeId, image: ImageId, catalog: &Catalog) -> Option<usize> {
        let n = self.node(node).filter(|n| n.kind == NodeKind::Merge)?;
        let ctx = Ctx::new(catalog);
        if ctx.merge_is_plain(n) {
            (0..n.kind.inputs().len()).find(|&port| self.input_contains_ctx(&ctx, node, port, image))
        } else {
            self.merge_entries(&ctx, node).iter().find(|(i, _)| *i == image).map(|(_, port)| *port)
        }
    }

    /// For a Merge node: how many connected inputs contain `image`
    /// (2 or more is a duplicate, resolved by port order).
    pub fn merge_candidates(&self, node: NodeId, image: ImageId, capture: CaptureFn) -> usize {
        let Some(n) = self.node(node).filter(|n| n.kind == NodeKind::Merge) else { return 0 };
        let catalog = Catalog { images: &[], capture };
        let ctx = Ctx::new(&catalog);
        (0..n.kind.inputs().len())
            .filter(|&port| self.input_contains_ctx(&ctx, node, port, image))
            .count()
    }

    /// The collection leaving output `port` of `node`, in order: a Source
    /// lists the catalog (filtered by its folder); pass-through nodes, filters
    /// and splits keep their input's order; a Merge lists input A first, then B
    /// and so on, each in its own order, skipping ids it already listed (and the
    /// losers of its `collision` option); a Layer lists its driver's images.
    /// Agrees with [`Graph::contains_in`] for every image of the catalog.
    /// Capture metadata is fetched at most once per image per call.
    pub fn members(&self, node: NodeId, port: usize, catalog: &Catalog) -> Vec<ImageId> {
        let ctx = Ctx::new(catalog);
        self.members_ctx(&ctx, node, port).as_ref().clone()
    }

    /// For a Layer node and an image of its output collection: the image each
    /// layer shows, indexed by layer (layer 1 = index 0). `None` for layers
    /// without a connected input or without images. The driver is the
    /// connected layer with the most images (tie: the lowest number); the
    /// output image at position `k` has the identity of the driver's `k`-th
    /// image and layer `j` shows its `k mod len`-th image. `None` if `node` is
    /// no Layer, `image` is not in the output, or the catalog is empty.
    pub fn layer_sources(
        &self,
        node: NodeId,
        image: ImageId,
        catalog: &Catalog,
    ) -> Option<[Option<ImageId>; MAX_LAYERS]> {
        if catalog.images.is_empty() {
            return None;
        }
        self.node(node).filter(|n| n.kind == NodeKind::Layer)?;
        let ctx = Ctx::new(catalog);
        let (driver, lists) = self.layer_plan(&ctx, node)?;
        let k = driver.iter().position(|i| *i == image)?;
        let mut out = [None; MAX_LAYERS];
        for (slot, list) in out.iter_mut().zip(&lists) {
            if let Some(list) = list.as_ref().filter(|l| !l.is_empty()) {
                *slot = Some(list[k % list.len()]);
            }
        }
        Some(out)
    }

    // ---- collection queries -------------------------------------------------

    fn input_contains_ctx(&self, ctx: &Ctx, node: NodeId, port: usize, image: ImageId) -> bool {
        self.input_edges(node, port)
            .any(|e| self.contains_output_ctx(ctx, e.from, e.from_port, image))
    }

    fn contains_output_ctx(&self, ctx: &Ctx, node: NodeId, port: usize, image: ImageId) -> bool {
        let Some(n) = self.node(node) else { return false };
        let port = if n.kind.outputs().is_empty() && port == 0 { 0 } else { let Some(p) = n.resolve_output(port) else { return false }; p };
        if n.kind == NodeKind::MetadataSplit {
            return self.split_port_ctx(ctx, node, image) == Some(port);
        }
        port == 0 && self.contains_ctx(ctx, node, image)
    }

    fn split_port_ctx(&self, ctx: &Ctx, node: NodeId, image: ImageId) -> Option<usize> {
        let n = self.node(node).filter(|n| n.kind == NodeKind::MetadataSplit)?;
        if !self.input_contains_ctx(ctx, node, 0, image) {
            return None;
        }
        let cap = ctx.cap(image);
        split_value(n, image, cap.as_deref())
    }

    fn contains_ctx(&self, ctx: &Ctx, node: NodeId, image: ImageId) -> bool {
        let Some(n) = self.node(node) else { return false };
        if !ctx.enter() {
            return false;
        }
        let result = match n.kind {
            NodeKind::Generator | NodeKind::Draw if self.input_edges(node, 0).next().is_none() => image == ImageId(u64::MAX - node.0) && !ctx.catalog.images.contains(&image),
            NodeKind::Source => match source_folder(n) {
                None => true,
                Some((folder, recursive)) => !folder.is_empty() && ctx
                    .cap(image)
                    .is_some_and(|c| c.path.as_deref().is_some_and(|p| path_in_folder(p, folder, recursive))),
            },
            NodeKind::Merge => {
                if ctx.merge_is_plain(n) {
                    (0..n.kind.inputs().len()).any(|port| self.input_contains_ctx(ctx, node, port, image))
                } else {
                    self.merge_entries(ctx, node).iter().any(|(i, _)| *i == image)
                }
            }
            NodeKind::Layer => {
                if ctx.catalog.images.is_empty() {
                    (0..MAX_LAYERS).any(|port| self.input_contains_ctx(ctx, node, port, image))
                } else {
                    self.members_ctx(ctx, node, 0).contains(&image)
                }
            }
            NodeKind::MetadataFilter => {
                self.input_contains_ctx(ctx, node, 0, image) && {
                    let cap = ctx.cap(image);
                    metadata_filter_matches(n, image, cap.as_deref())
                }
            }
            _ => self.input_contains_ctx(ctx, node, 0, image),
        };
        ctx.leave();
        result
    }

    fn input_members(&self, ctx: &Ctx, node: NodeId, port: usize) -> Rc<Vec<ImageId>> {
        let mut result = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for e in self.input_edges(node, port) {
            for &image in self.members_ctx(ctx, e.from, e.from_port).iter() {
                if seen.insert(image) { result.push(image); }
            }
        }
        Rc::new(result)
    }

    fn members_ctx(&self, ctx: &Ctx, node: NodeId, port: usize) -> Rc<Vec<ImageId>> {
        if let Some(v) = ctx.members.borrow().get(&(node, port)) {
            return Rc::clone(v);
        }
        if !ctx.enter() {
            return Rc::new(Vec::new());
        }
        let list = self.compute_members(ctx, node, port);
        ctx.leave();
        ctx.members.borrow_mut().insert((node, port), Rc::clone(&list));
        list
    }

    fn compute_members(&self, ctx: &Ctx, node: NodeId, port: usize) -> Rc<Vec<ImageId>> {
        let none = || Rc::new(Vec::new());
        let Some(n) = self.node(node) else { return none() };
        let port = if n.kind.outputs().is_empty() && port == 0 { 0 } else {
            let Some(port) = n.resolve_output(port) else { return none() };
            port
        };
        match n.kind {
            NodeKind::Generator | NodeKind::Draw if port == 0 && self.input_edges(node, 0).next().is_none() => {
                let image = ImageId(u64::MAX - node.0);
                if ctx.catalog.images.contains(&image) { none() } else { Rc::new(vec![image]) }
            }
            NodeKind::MetadataSplit => {
                if port >= n.kind.outputs().len() {
                    return none();
                }
                let input = self.input_members(ctx, node, 0);
                Rc::new(
                    input
                        .iter()
                        .copied()
                        .filter(|&i| {
                            let cap = ctx.cap(i);
                            split_value(n, i, cap.as_deref()) == Some(port)
                        })
                        .collect(),
                )
            }
            _ if port != 0 => none(),
            NodeKind::Source => match source_folder(n) {
                None => Rc::new(ctx.catalog.images.to_vec()),
                Some(("", _)) => none(),
                Some((folder, recursive)) => Rc::new(
                    ctx.catalog
                        .images
                        .iter()
                        .copied()
                        .filter(|&i| {
                            ctx.cap(i).is_some_and(|c| c.path.as_deref().is_some_and(|p| path_in_folder(p, folder, recursive)))
                        })
                        .collect(),
                ),
            },
            NodeKind::MetadataFilter => {
                let input = self.input_members(ctx, node, 0);
                Rc::new(
                    input
                        .iter()
                        .copied()
                        .filter(|&i| {
                            let cap = ctx.cap(i);
                            metadata_filter_matches(n, i, cap.as_deref())
                        })
                        .collect(),
                )
            }
            NodeKind::Merge => Rc::new(self.merge_entries(ctx, node).iter().map(|(i, _)| *i).collect()),
            NodeKind::Layer => match self.layer_plan(ctx, node) {
                Some((driver, _)) => driver,
                None => none(),
            },
            _ => self.input_members(ctx, node, 0),
        }
    }

    /// The images a Merge outputs with the input port each comes from. Identity
    /// duplicates keep the first input; then, unless the collision option is
    /// "Keep both" (or the catalog is empty), only the images of the winning
    /// input per file name stay.
    fn merge_entries(&self, ctx: &Ctx, node: NodeId) -> Rc<Vec<(ImageId, usize)>> {
        if let Some(v) = ctx.merges.borrow().get(&node) {
            return Rc::clone(v);
        }
        let Some(n) = self.node(node).filter(|n| n.kind == NodeKind::Merge) else { return Rc::new(Vec::new()) };
        let mut entries: Vec<(ImageId, usize)> = Vec::new();
        let mut seen: HashSet<ImageId> = HashSet::new();
        for port in 0..n.kind.inputs().len() {
            let list = self.input_members(ctx, node, port);
            for &id in list.iter() {
                if seen.insert(id) {
                    entries.push((id, port));
                }
            }
        }
        if !ctx.merge_is_plain(n) {
            let first_wins = n.all_choice("collision") == 1;
            let keys: Vec<Option<String>> = entries
                .iter()
                .map(|(id, _)| ctx.cap(*id).and_then(|c| c.path.as_deref().and_then(name_key)))
                .collect();
            let mut winner: HashMap<&str, usize> = HashMap::new();
            for ((_, port), key) in entries.iter().zip(&keys) {
                if let Some(key) = key {
                    winner
                        .entry(key.as_str())
                        .and_modify(|w| *w = if first_wins { (*w).min(*port) } else { (*w).max(*port) })
                        .or_insert(*port);
                }
            }
            entries = entries
                .iter()
                .zip(&keys)
                .filter(|((_, port), key)| key.as_deref().is_none_or(|k| winner.get(k) == Some(port)))
                .map(|(e, _)| *e)
                .collect();
        }
        let out = Rc::new(entries);
        ctx.merges.borrow_mut().insert(node, Rc::clone(&out));
        out
    }

    /// For a Layer: the driver's members and the members of every connected layer.
    #[allow(clippy::type_complexity)]
    fn layer_plan(&self, ctx: &Ctx, node: NodeId) -> Option<(Rc<Vec<ImageId>>, Vec<Option<Rc<Vec<ImageId>>>>)> {
        let mut lists: Vec<Option<Rc<Vec<ImageId>>>> = Vec::with_capacity(MAX_LAYERS);
        let mut driver: Option<(usize, Rc<Vec<ImageId>>)> = None;
        for port in 0..MAX_LAYERS {
            let list = self.input_edge(node, port).map(|(up, up_port)| self.members_ctx(ctx, up, up_port));
            if let Some(l) = &list {
                if driver.as_ref().is_none_or(|(len, _)| l.len() > *len) {
                    driver = Some((l.len(), Rc::clone(l)));
                }
            }
            lists.push(list);
        }
        driver.map(|(_, d)| (d, lists))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("graph serializes to JSON")
    }

    pub fn from_json(text: &str) -> Result<Self, GraphError> {
        Self::from_json_with_notes(text).map(|(graph, _)| graph)
    }

    /// Like [`Graph::from_json`], and also returns the notes of the migration: graphs of format 1 and 2
    /// (before the Mask category and the fixed mask ports were removed) are rewritten to the format-3
    /// nodes first, see `migrate`. Each note names a node and what could not be carried over exactly.
    /// The list is empty for a graph that needed no changes.
    pub fn from_json_with_notes(text: &str) -> Result<(Self, Vec<String>), GraphError> {
        Self::parse_json(text, false)
    }

    /// Like [`Graph::from_json_with_notes`] for graphs the user must be able to open (project files and
    /// their history snapshots): a damaged group structure is repaired instead of refused (dangling
    /// members and ports dropped, missing ports added, bad names cleaned, empty, cyclic or too deeply
    /// nested groups dissolved) and every repair adds a note. Everything else is checked as strictly
    /// as before.
    pub fn from_json_repaired(text: &str) -> Result<(Self, Vec<String>), GraphError> {
        Self::parse_json(text, true)
    }

    fn parse_json(text: &str, repair: bool) -> Result<(Self, Vec<String>), GraphError> {
        let mut value: serde_json::Value = serde_json::from_str(text).map_err(|e| GraphError::Parse(e.to_string()))?;
        // Before anything is parsed: a newer file may hold node kinds or data this build does not know.
        if let Some(v) = value.get("format_version").and_then(serde_json::Value::as_u64).filter(|v| *v > u64::from(GRAPH_FORMAT_VERSION)) {
            return Err(GraphError::UnsupportedVersion(u32::try_from(v).unwrap_or(u32::MAX)));
        }
        let mut notes = crate::migrate::migrate_graph(&mut value)?;
        let mut graph: Graph = serde_json::from_value(value).map_err(|e| GraphError::Parse(e.to_string()))?;
        if graph.format_version > GRAPH_FORMAT_VERSION {
            return Err(GraphError::UnsupportedVersion(graph.format_version));
        }
        if repair {
            notes.extend(graph.repair_groups());
        }
        graph.check_consistency()?;
        // Save successfully validated graphs using the current format.
        graph.format_version = GRAPH_FORMAT_VERSION;
        Ok((graph, notes))
    }

    /// Verifies everything the public API guarantees by construction, so a
    /// hand-edited file cannot smuggle in a state the editor could never make.
    fn check_consistency(&self) -> Result<(), GraphError> {
        let bad = |msg: String| Err(GraphError::Parse(msg));

        for (key, node) in &self.nodes {
            if *key != node.id {
                return bad(format!("node map key {} does not match the id {} stored in the node", key.0, node.id.0));
            }
            if node.id.0 >= self.next_id {
                return bad(format!("next_id {} is not greater than node id {}", self.next_id, node.id.0));
            }
            if (node.kind == NodeKind::Group) != node.group.is_some() {
                return bad(format!("node {}: a group definition belongs to Group nodes and only to them", node.id.0));
            }
            let mut exposed = BTreeSet::new();
            for input in node.exposed_inputs.iter().flatten() {
                if !Self::supported_parameter_inputs(node.kind).contains(&input.parameter.as_str()) || !exposed.insert(&input.parameter) {
                    return bad(format!("node {}: invalid or duplicate exposed parameter", node.id.0));
                }
            }
            for output in node.exposed_outputs.iter().flatten() {
                if node.kind.outputs().get(output.source_port).is_none() || output.label.trim().is_empty() || output.label.chars().count() > 128 || output.label.contains('\0') {
                    return bad(format!("node {}: invalid exposed output", node.id.0));
                }
            }
            let label = node.kind.label();
            let check_values = |values: &BTreeMap<String, f32>, scope: &str| -> Result<(), GraphError> {
                for (name, value) in values {
                    let Some(spec) = node.kind.param_spec(name) else {
                        return bad(format!("node {} ({label}){scope}: unknown parameter \"{name}\"", node.id.0));
                    };
                    let discrete = !matches!(spec.kind, ParamKind::Continuous);
                    if discrete && value.fract() != 0.0 {
                        return bad(format!(
                            "node {} ({label}){scope}: parameter \"{name}\" value {value} is not a whole option number",
                            node.id.0
                        ));
                    }
                    if !value.is_finite() || *value < spec.min || *value > spec.max {
                        return bad(format!(
                            "node {} ({label}){scope}: parameter \"{name}\" value {value} is outside {}..{}",
                            node.id.0, spec.min, spec.max
                        ));
                    }
                }
                Ok(())
            };
            check_values(&node.all, "")?;
            for (image, values) in &node.individual {
                check_values(values, &format!(" image {}", image.0))?;
            }
            let all_data = node.data.iter().map(|d| (d, String::new()));
            let image_data = node.individual_data.iter().map(|(i, d)| (d, format!(" image {}", i.0)));
            for (data, scope) in all_data.chain(image_data) {
                if let Err(msg) = validate_data(node.kind, data) {
                    return bad(format!("node {} ({label}){scope}: data: {msg}", node.id.0));
                }
            }
        }

        let mut wired = std::collections::BTreeSet::new();
        for (i, e) in self.edges.iter().enumerate() {
            let what = format!("edge {i} ({}:{} -> {}:{})", e.from.0, e.from_port, e.to.0, e.to_port);
            let Some(src) = self.nodes.get(&e.from) else {
                return bad(format!("{what}: source node {} does not exist", e.from.0));
            };
            let Some(dst) = self.nodes.get(&e.to) else {
                return bad(format!("{what}: target node {} does not exist", e.to.0));
            };
            if src.kind == NodeKind::Group || dst.kind == NodeKind::Group {
                return bad(format!("{what}: wires connect real nodes, never a group node"));
            }
            let outputs = src.kind.outputs();
            if outputs.is_empty() {
                return bad(format!("{what}: node {} ({}) has no output", e.from.0, src.kind.label()));
            }
            let Some(found) = src.output_type(e.from_port) else {
                return bad(format!("{what}: node {} ({}) has no output port {}", e.from.0, src.kind.label(), e.from_port));
            };
            let Some(expected) = dst.input_type(e.to_port) else {
                return bad(format!("{what}: node {} ({}) has no input {}", e.to.0, dst.kind.label(), e.to_port));
            };
            if expected != found && !dst.is_control_input(e.to_port) {
                return bad(format!("{what}: input expects {:?} but the source carries {found:?}", expected));
            }
            let multi = dst.is_collection_input(e.to_port) && !matches!(dst.kind, NodeKind::Merge | NodeKind::Layer);
            if (!multi && !wired.insert((e.to, e.to_port))) || self.edges.iter().take_while(|other| !std::ptr::eq(*other, e)).any(|other| other == e) {
                return bad(format!("{what}: input {} of node {} is connected more than once", e.to_port, e.to.0));
            }
        }

        // Kahn's algorithm: whatever cannot be ordered sits on or behind a cycle.
        let mut indegree: BTreeMap<NodeId, usize> = self.nodes.keys().map(|id| (*id, 0)).collect();
        for e in &self.edges {
            *indegree.get_mut(&e.to).expect("edge targets were checked") += 1;
        }
        let mut ready: Vec<NodeId> = indegree.iter().filter(|(_, d)| **d == 0).map(|(id, _)| *id).collect();
        let mut ordered = 0;
        while let Some(id) = ready.pop() {
            ordered += 1;
            for e in self.edges.iter().filter(|e| e.from == id) {
                let d = indegree.get_mut(&e.to).expect("edge targets were checked");
                *d -= 1;
                if *d == 0 {
                    ready.push(e.to);
                }
            }
        }
        if ordered != self.nodes.len() {
            let stuck: Vec<String> =
                indegree.iter().filter(|(_, d)| **d > 0).map(|(id, _)| id.0.to_string()).collect();
            return bad(format!("the connections form a cycle (nodes {})", stuck.join(", ")));
        }
        self.check_groups().map_err(GraphError::Parse)?;
        Ok(())
    }
}

/// Bumped whenever the serialized sub-graph layout changes incompatibly.
pub const SUBGRAPH_FORMAT_VERSION: u32 = 5;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SubNode {
    kind: NodeKind,
    mode: Mode,
    /// The All values only; per-image values never leave their image.
    all: BTreeMap<String, f32>,
    /// The All data only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<NodeData>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exposed_inputs: Vec<Option<ExposedInput>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    exposed_outputs: Vec<Option<ExposedOutput>>,
    pos: [f32; 2],
    /// The definition of a Group node, with members and ports as indices into the node list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<SubGroup>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SubPort {
    label: String,
    node: usize,
    port: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SubPublished {
    label: String,
    node: usize,
    param: String,
}

/// [`GroupDef`] of a sub-graph: node references are indices into the sub-graph's node list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SubGroup {
    label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<[u8; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    origin: Option<String>,
    members: Vec<usize>,
    inputs: Vec<SubPort>,
    outputs: Vec<SubPort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    published: Vec<SubPublished>,
}

impl SubGroup {
    /// Converts with `map` (sub-graph index to node id); `None` when a reference does not resolve.
    fn to_def(&self, map: &dyn Fn(usize) -> Option<NodeId>) -> Option<GroupDef> {
        let ports = |list: &[SubPort]| -> Option<Vec<GroupPort>> {
            list.iter().map(|p| Some(GroupPort { label: p.label.clone(), node: map(p.node)?, port: p.port })).collect()
        };
        Some(GroupDef {
            label: self.label.clone(),
            color: self.color,
            origin: self.origin.clone(),
            members: self.members.iter().map(|m| map(*m)).collect::<Option<Vec<_>>>()?,
            inputs: ports(&self.inputs)?,
            outputs: ports(&self.outputs)?,
            published: self
                .published
                .iter()
                .map(|p| Some(Published { label: p.label.clone(), node: map(p.node)?, param: p.param.clone() }))
                .collect::<Option<Vec<_>>>()?,
        })
    }

    fn from_def(def: &GroupDef, index: &dyn Fn(NodeId) -> Option<usize>) -> Option<SubGroup> {
        let ports = |list: &[GroupPort]| -> Option<Vec<SubPort>> {
            list.iter().map(|p| Some(SubPort { label: p.label.clone(), node: index(p.node)?, port: p.port })).collect()
        };
        Some(SubGroup {
            label: def.label.clone(),
            color: def.color,
            origin: def.origin.clone(),
            members: def.members.iter().map(|m| index(*m)).collect::<Option<Vec<_>>>()?,
            inputs: ports(&def.inputs)?,
            outputs: ports(&def.outputs)?,
            published: def
                .published
                .iter()
                .map(|p| Some(SubPublished { label: p.label.clone(), node: index(p.node)?, param: p.param.clone() }))
                .collect::<Option<Vec<_>>>()?,
        })
    }
}

/// Edge between two nodes of a sub-graph, by index into [`Subgraph`]'s node list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct SubEdge {
    from: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    from_port: usize,
    to: usize,
    to_port: usize,
}

/// A copy of some nodes and the edges between them, for copy/paste and recipes.
///
/// It carries kinds, modes, All parameters and All data and positions, but no
/// per-image (Individual) values or data: a recipe must not carry another
/// shoot's per-image edits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Subgraph {
    format_version: u32,
    nodes: Vec<SubNode>,
    edges: Vec<SubEdge>,
}

impl Subgraph {
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Kinds of the nodes, in the order `Graph::insert_subgraph` returns their new ids.
    pub fn kinds(&self) -> Vec<NodeKind> {
        self.nodes.iter().map(|n| n.kind).collect()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("sub-graph serializes to JSON")
    }

    /// Group definition of node 0 when that node is a group nothing else of the sub-graph contains.
    fn root_group(&self) -> Option<&SubGroup> {
        let first = self.nodes.first()?;
        let g = first.group.as_ref()?;
        let contained = self.nodes.iter().filter_map(|n| n.group.as_ref()).any(|d| d.members.contains(&0));
        (first.kind == NodeKind::Group && !contained).then_some(g)
    }

    /// Moves the sub-graph so its first node sits at the origin (macros are stored position independent).
    fn normalize_origin(&mut self) {
        let Some(first) = self.nodes.first() else { return };
        let [ox, oy] = first.pos;
        for n in &mut self.nodes {
            n.pos = [n.pos[0] - ox, n.pos[1] - oy];
        }
    }

    /// The checks of [`Subgraph::from_json`]: edge indices, the graph rules and the group rules.
    fn validate(&self) -> Result<(), GraphError> {
        let sub = self;
        for (i, e) in sub.edges.iter().enumerate() {
            if e.from >= sub.nodes.len() || e.to >= sub.nodes.len() {
                return Err(GraphError::Parse(format!(
                    "edge {i} refers to node {} or {} but the sub-graph has {} nodes",
                    e.from,
                    e.to,
                    sub.nodes.len()
                )));
            }
        }
        // Reuse the graph checks on a scratch graph whose ids are index + 1.
        let map = |i: usize| (i < sub.nodes.len()).then(|| NodeId(i as u64 + 1));
        let mut nodes = BTreeMap::new();
        for (i, n) in sub.nodes.iter().enumerate() {
            let id = NodeId(i as u64 + 1);
            let group = match &n.group {
                Some(g) => Some(g.to_def(&map).ok_or_else(|| GraphError::Parse(format!("node {i}: the group refers to a node that does not exist")))?),
                None => None,
            };
            let node = Node {
                id,
                kind: n.kind,
                mode: n.mode,
                all: n.all.clone(),
                individual: BTreeMap::new(),
                data: n.data.clone(),
                individual_data: BTreeMap::new(),
                exposed_inputs: n.exposed_inputs.clone(),
                exposed_outputs: n.exposed_outputs.clone(),
                pos: n.pos,
                group,
            };
            nodes.insert(id, Arc::new(node));
        }
        let edges = sub
            .edges
            .iter()
            .map(|e| Edge {
                from: NodeId(e.from as u64 + 1),
                from_port: e.from_port,
                to: NodeId(e.to as u64 + 1),
                to_port: e.to_port,
            })
            .collect();
        let scratch = Graph { format_version: GRAPH_FORMAT_VERSION, nodes, edges, next_id: sub.nodes.len() as u64 + 1 };
        scratch.check_consistency()
    }

    /// Parses and validates like [`Graph::from_json`]: node kinds, parameter
    /// ranges, data limits, edge ports and types, and no cycles.
    pub fn from_json(text: &str) -> Result<Subgraph, GraphError> {
        Self::from_json_with_notes(text).map(|(sub, _)| sub)
    }

    /// Like [`Subgraph::from_json`], and also returns the migration notes of a sub-graph of format 1
    /// (copied or saved as a recipe before the mask nodes were removed).
    pub fn from_json_with_notes(text: &str) -> Result<(Subgraph, Vec<String>), GraphError> {
        let mut value: serde_json::Value = serde_json::from_str(text).map_err(|e| GraphError::Parse(e.to_string()))?;
        if let Some(v) = value.get("format_version").and_then(serde_json::Value::as_u64).filter(|v| *v > u64::from(SUBGRAPH_FORMAT_VERSION)) {
            return Err(GraphError::UnsupportedSubgraphVersion(u32::try_from(v).unwrap_or(u32::MAX)));
        }
        let notes = crate::migrate::migrate_subgraph(&mut value)?;
        let sub: Subgraph = serde_json::from_value(value).map_err(|e| GraphError::Parse(e.to_string()))?;
        if sub.format_version > SUBGRAPH_FORMAT_VERSION {
            return Err(GraphError::UnsupportedSubgraphVersion(sub.format_version));
        }
        sub.validate()?;
        Ok((sub, notes))
    }
}

impl Graph {
    /// Copies the given nodes (unknown and repeated ids are skipped) and the
    /// edges between them. Per-image values and data are not included; the
    /// mode itself is kept.
    pub fn extract_subgraph(&self, nodes: &[NodeId]) -> Subgraph {
        let mut ids: Vec<NodeId> = Vec::new();
        for id in nodes {
            if self.nodes.contains_key(id) && !ids.contains(id) {
                ids.push(*id);
            }
        }
        // A group is copied with everything inside it.
        let mut at = 0;
        while at < ids.len() {
            if let Some(g) = self.nodes[&ids[at]].group.as_ref() {
                for m in &g.members {
                    if self.nodes.contains_key(m) && !ids.contains(m) {
                        ids.push(*m);
                    }
                }
            }
            at += 1;
        }
        let index = |id: NodeId| ids.iter().position(|x| *x == id);
        let sub_nodes = ids
            .iter()
            .map(|id| {
                let n = &self.nodes[id];
                SubNode { kind: n.kind, mode: n.mode, all: n.all.clone(), data: n.data.clone(), exposed_inputs: n.exposed_inputs.clone(), exposed_outputs: n.exposed_outputs.clone(), pos: n.pos, group: n.group.as_ref().and_then(|g| SubGroup::from_def(g, &index)) }
            })
            .collect();
        let sub_edges = self
            .edges
            .iter()
            .filter_map(|e| {
                Some(SubEdge { from: index(e.from)?, from_port: e.from_port, to: index(e.to)?, to_port: e.to_port })
            })
            .collect();
        Subgraph { format_version: SUBGRAPH_FORMAT_VERSION, nodes: sub_nodes, edges: sub_edges }
    }

    /// Inserts a copy of `sub` with fresh ids and positions shifted by
    /// `offset`; returns the new ids in the order of the sub-graph's nodes.
    /// Never connects to existing nodes.
    pub fn insert_subgraph(&mut self, sub: &Subgraph, offset: [f32; 2]) -> Vec<NodeId> {
        let ids: Vec<NodeId> = sub
            .nodes
            .iter()
            .map(|n| {
                let id = self.add_node(n.kind, [n.pos[0] + offset[0], n.pos[1] + offset[1]]);
                let node = self.nodes.get_mut(&id).map(Arc::make_mut).expect("node was just added");
                node.exposed_inputs = n.exposed_inputs.clone();
                node.exposed_outputs = n.exposed_outputs.clone();
                node.mode = n.mode;
                node.all = n.all.iter().filter(|(k, _)| n.kind.param_spec(k).is_some()).map(|(k, v)| (k.clone(), *v)).collect();
                node.data = n.data.clone().filter(|d| validate_data(n.kind, d).is_ok());
                id
            })
            .collect();
        for e in &sub.edges {
            if let (Some(&from), Some(&to)) = (ids.get(e.from), ids.get(e.to)) {
                // A sub-graph from `from_json` is valid; hand-built ones may lose a bad edge here.
                let _ = self.connect_ports(from, e.from_port, to, e.to_port);
            }
        }
        // Groups last: the wires between their members were made while they were still plain nodes.
        let map = |i: usize| ids.get(i).copied();
        for (i, n) in sub.nodes.iter().enumerate() {
            if let Some(def) = n.group.as_ref().and_then(|g| g.to_def(&map)) {
                if let Some(node) = self.nodes.get_mut(&ids[i]).map(Arc::make_mut) {
                    node.group = Some(def);
                }
            }
        }
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ImageId = ImageId(1);
    const B: ImageId = ImageId(2);

    #[test]
    fn connect_checks_ports_and_cycles() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let sat = g.add_node(NodeKind::Saturation, [0.0, 0.0]);
        let mask = g.add_node(NodeKind::Generator, [0.0, 0.0]);
        let blur = g.add_node(NodeKind::Blur, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);

        assert_eq!(g.connect(src, exp, 0), Ok(()));
        assert_eq!(g.connect(exp, sat, 0), Ok(()));
        assert_eq!(g.connect(sat, exp, 0), Err(GraphError::WouldCycle));
        assert_eq!(g.connect(exp, exp, 0), Err(GraphError::WouldCycle));
        assert_eq!(g.connect(mask, blur, 1), Err(GraphError::NoSuchPort { node: blur, port: 1 }));
        let radius = g.expose_parameter(blur, "radius", PortType::Images).unwrap();
        assert_eq!(g.connect(mask, blur, radius), Ok(()));
        assert_eq!(g.connect(out, sat, 0), Err(GraphError::NoOutput(out)));
        assert_eq!(g.connect(sat, out, 3), Err(GraphError::NoSuchPort { node: out, port: 3 }));
    }

    #[test]
    fn connecting_images_adds_distinct_inputs() {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(a, out, 0).unwrap();
        g.connect(b, out, 0).unwrap();
        assert_eq!(g.input_of(out, 0), Some(a));
        assert_eq!(g.edges().len(), 2);
        g.connect(a, out, 0).unwrap();
        assert_eq!(g.edges().len(), 2);
    }

    #[test]
    fn exact_disconnect_preserves_other_wires_and_their_order() {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        let first = Edge { from: a, from_port: 0, to: out, to_port: 0 };
        let second = Edge { from: b, from_port: 0, to: out, to_port: 0 };
        // Model a future fan-in without enabling it in connect or deserialization.
        g.edges.extend([first, second]);
        assert_eq!(g.input_edges(out, 0).copied().collect::<Vec<_>>(), vec![first, second]);
        assert!(g.disconnect_edge(first));
        assert!(!g.disconnect_edge(first));
        assert_eq!(g.input_edges(out, 0).copied().collect::<Vec<_>>(), vec![second]);
    }

    #[test]
    fn validate_reports_missing_required_inputs_only() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let blur = g.add_node(NodeKind::Blur, [0.0, 0.0]);
        assert_eq!(g.validate().len(), 1);
        g.connect(src, blur, 0).unwrap();
        assert!(g.validate().is_empty(), "the optional mask input must not be an error");
    }

    #[test]
    fn individual_values_persist_per_image_and_survive_mode_changes() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();

        n.set_param("ev", A, 0.5);
        assert_eq!((n.param("ev", A), n.param("ev", B)), (0.5, 0.5));

        n.mode = Mode::Individual;
        n.set_param("ev", A, 2.0);
        assert_eq!(n.param("ev", A), 2.0);
        assert_eq!(n.param("ev", B), 0.5, "images without their own value inherit the All value");
        assert!(n.has_individual(A) && !n.has_individual(B));

        n.mode = Mode::All;
        assert_eq!(n.param("ev", A), 0.5);
        n.mode = Mode::Individual;
        assert_eq!(n.param("ev", A), 2.0, "switching modes must not discard per-image edits");

        n.clear_individual(A);
        assert_eq!(n.param("ev", A), 0.5);
    }

    #[test]
    fn set_param_clamps_and_rejects_unknown_names() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Saturation, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        assert_eq!(n.param("amount", A), 1.0);
        assert!(n.set_param("amount", A, 99.0));
        assert_eq!(n.param("amount", A), 2.0);
        assert!(n.set_param("amount", A, f32::NAN));
        assert_eq!(n.param("amount", A), 1.0);
        assert!(!n.set_param("nope", A, 1.0));
    }

    #[test]
    fn capture_ev_changes_add_up_along_the_chain() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let per_image = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        let offset = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        g.connect(src, per_image, 0).unwrap();
        g.connect(per_image, offset, 0).unwrap();
        let n = g.node_mut(per_image).unwrap();
        n.mode = Mode::Individual;
        n.set_param("delta", A, 1.5);
        g.node_mut(offset).unwrap().set_param("delta", A, -0.5);
        assert_eq!(g.capture_ev_delta(offset, A), 1.0);
        assert_eq!(g.capture_ev_delta(offset, B), -0.5);
        assert_eq!(g.capture_ev_delta(src, A), 0.0);
    }

    #[test]
    fn json_roundtrip_keeps_everything() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [10.0, 20.0]);
        let wb = g.add_node(NodeKind::WhiteBalance, [200.0, 20.0]);
        g.connect(src, wb, 0).unwrap();
        let n = g.node_mut(wb).unwrap();
        n.set_param("temp", A, 25.0);
        n.mode = Mode::Individual;
        n.set_param("tint", B, -10.0);
        let back = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(back, g);
        let id = back.clone().add_node(NodeKind::Output, [0.0, 0.0]);
        assert_eq!(id, NodeId(3), "ids must keep counting after a reload");
    }

    #[test]
    fn newer_format_versions_are_refused() {
        let text = Graph::new().to_json().replace(&format!("\"format_version\": {GRAPH_FORMAT_VERSION}"), "\"format_version\": 999");
        assert_eq!(Graph::from_json(&text), Err(GraphError::UnsupportedVersion(999)));
        assert!(matches!(Graph::from_json("not json"), Err(GraphError::Parse(_))));
    }

    fn sample_graph() -> Graph {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let cap = g.add_node(NodeKind::CaptureEv, [1.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [2.0, 0.0]);
        let wb = g.add_node(NodeKind::WhiteBalance, [3.0, 0.0]);
        let sat = g.add_node(NodeKind::Saturation, [4.0, 0.0]);
        let mask = g.add_node(NodeKind::Generator, [4.0, 1.0]);
        let blur = g.add_node(NodeKind::Blur, [5.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [6.0, 0.0]);
        for (a, b) in [(src, cap), (cap, exp), (exp, wb), (wb, sat), (sat, blur), (blur, out)] {
            g.connect(a, b, 0).unwrap();
        }
        let radius = g.expose_parameter(blur, "radius", PortType::Images).unwrap();
        g.connect(mask, blur, radius).unwrap();
        g.node_mut(exp).unwrap().set_param("ev", A, 1.5);
        let n = g.node_mut(sat).unwrap();
        n.mode = Mode::Individual;
        n.set_param("amount", A, 1.7);
        n.set_param("amount", B, 99.0); // clamped to the range
        g.node_mut(blur).unwrap().set_param("radius", A, f32::NAN); // stored as the default
        g
    }

    // Node ids in sample_graph: 1 Source, 2 CaptureEv, 3 Exposure, 4 WhiteBalance,
    // 5 Saturation, 6 Generator, 7 Blur, 8 Output.
    fn load_edited(edit: impl FnOnce(&mut serde_json::Value)) -> Result<Graph, GraphError> {
        let mut v: serde_json::Value = serde_json::from_str(&sample_graph().to_json()).unwrap();
        edit(&mut v);
        Graph::from_json(&v.to_string())
    }

    fn assert_rejected(result: Result<Graph, GraphError>, needle: &str) {
        match result {
            Err(GraphError::Parse(msg)) => assert!(msg.contains(needle), "message {msg:?} lacks {needle:?}"),
            other => panic!("expected a Parse error mentioning {needle:?}, got {other:?}"),
        }
    }

    fn push_edge(v: &mut serde_json::Value, from: u64, to: u64, port: usize) {
        v["edges"].as_array_mut().unwrap().push(serde_json::json!({"from": from, "to": to, "to_port": port}));
    }

    #[test]
    fn every_graph_built_through_the_api_round_trips() {
        let g = sample_graph();
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g.clone()));
        assert_eq!(Graph::from_json(&Graph::new().to_json()), Ok(Graph::new()));

        // Also after removals and re-connections.
        let mut g = g;
        let ids: Vec<NodeId> = g.nodes().map(|n| n.id).collect();
        g.remove_node(ids[2]);
        let extra = g.add_node(NodeKind::Exposure, [9.0, 9.0]);
        g.connect(ids[1], extra, 0).unwrap();
        g.disconnect(extra, 0);
        g.connect(ids[1], extra, 0).unwrap();
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g));
    }

    #[test]
    fn load_rejects_cycles() {
        // Blur (7) feeding back into Exposure (3) closes a loop through 3 -> 4 -> 5 -> 7.
        assert_rejected(load_edited(|v| {
            let edges = v["edges"].as_array_mut().unwrap();
            edges.retain(|e| !(e["to"] == 3));
            push_edge(v, 7, 3, 0);
        }), "cycle");
        assert_rejected(load_edited(|v| v["edges"] = serde_json::json!([{"from": 3, "to": 3, "to_port": 0}])), "cycle");
    }

    #[test]
    fn load_rejects_edges_to_unknown_nodes() {
        assert_rejected(load_edited(|v| v["edges"][0]["from"] = serde_json::json!(99)), "source node 99 does not exist");
        assert_rejected(load_edited(|v| v["edges"][0]["to"] = serde_json::json!(98)), "target node 98 does not exist");
    }

    #[test]
    fn load_rejects_edges_into_missing_ports() {
        assert_rejected(load_edited(|v| v["edges"][0]["to_port"] = serde_json::json!(5)), "has no input 5");
        // The Output node (8) has no output to connect from.
        assert_rejected(load_edited(|v| push_edge(v, 8, 3, 0)), "has no output");
    }

    #[test]
    fn load_rejects_node_keys_that_differ_from_the_node_id() {
        let r = load_edited(|v| v["nodes"]["3"]["id"] = serde_json::json!(33));
        assert_rejected(r, "node map key 3 does not match");
    }

    #[test]
    fn load_rejects_a_next_id_that_is_not_beyond_every_node_id() {
        assert_rejected(load_edited(|v| v["next_id"] = serde_json::json!(8)), "next_id 8");
        assert_rejected(load_edited(|v| v["next_id"] = serde_json::json!(0)), "next_id 0");
    }

    #[test]
    fn load_rejects_unknown_parameter_names() {
        let r = load_edited(|v| v["nodes"]["3"]["all"]["gamma"] = serde_json::json!(1.0));
        assert_rejected(r, "unknown parameter \"gamma\"");
        let r = load_edited(|v| v["nodes"]["5"]["individual"]["1"]["bogus"] = serde_json::json!(1.0));
        assert_rejected(r, "unknown parameter \"bogus\"");
    }

    #[test]
    fn load_rejects_out_of_range_and_non_finite_parameter_values() {
        let r = load_edited(|v| v["nodes"]["3"]["all"]["ev"] = serde_json::json!(6.5));
        assert_rejected(r, "outside -6..6");
        let r = load_edited(|v| v["nodes"]["5"]["individual"]["1"]["amount"] = serde_json::json!(-0.5));
        assert_rejected(r, "outside 0..2");
        // null is not a number at all; 1e39 overflows f32 to infinity.
        let r = load_edited(|v| v["nodes"]["3"]["all"]["ev"] = serde_json::Value::Null);
        assert!(matches!(r, Err(GraphError::Parse(_))), "{r:?}");
        let text = sample_graph().to_json().replace("\"ev\": 1.5", "\"ev\": 1e39");
        assert!(matches!(Graph::from_json(&text), Err(GraphError::Parse(_))));
    }

    #[test]
    fn parameter_kinds_round_clamp_and_expose_accessors() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();

        // Defaults.
        assert_eq!((n.choice("measure", A), n.choice("missing", A), n.toggle("invert", A)), (0, 0, false));
        assert_eq!((n.param("min", A), n.param("max", A)), (0.0, 1_000_000.0));

        // Choice: nearest index, clamped into 0..len-1.
        for (input, expect) in [(2.4, 2), (2.6, 3), (-3.0, 0), (99.0, 5), (f32::INFINITY, 0)] {
            n.set_param("measure", A, input);
            assert_eq!(n.choice("measure", A), expect, "input {input}");
        }
        n.set_param("measure", A, 3.0);
        n.set_param("measure", A, f32::NAN); // non-finite falls back to the default
        assert_eq!(n.choice("measure", A), 0);
        n.set_param("missing", A, 1.2);
        assert_eq!(n.choice("missing", A), 1);
        n.set_param("missing", A, 5.0);
        assert_eq!(n.choice("missing", A), 1, "clamped to the last option");

        // Toggle: 0.0 or 1.0.
        for (input, expect) in [(0.7, true), (0.5, true), (0.49, false), (-4.0, false), (9.0, true)] {
            n.set_param("invert", A, input);
            assert_eq!(n.toggle("invert", A), expect, "input {input}");
            assert_eq!(n.param("invert", A), if expect { 1.0 } else { 0.0 });
        }

        // Continuous parameters still clamp; wide ranges are flagged logarithmic.
        n.set_param("min", A, -5000.0);
        assert_eq!(n.param("min", A), -100.0);
        n.set_param("max", A, 5e9);
        assert_eq!(n.param("max", A), 1_000_000.0);
        let spec = NodeKind::MetadataFilter.param_spec("measure").unwrap();
        assert_eq!(spec.kind, ParamKind::Choice(&Measure::LABELS));
        assert!(NodeKind::MetadataFilter.param_spec("min").unwrap().logarithmic);
        assert!(NodeKind::Contrast.param_spec("pivot").unwrap().logarithmic);
        assert_eq!(NodeKind::MetadataFilter.param_spec("invert").unwrap().kind, ParamKind::Toggle);

        assert_eq!(NodeKind::Blur.param_spec("radius").unwrap().kind, ParamKind::Continuous);
        assert_eq!(NodeKind::Blur.param_spec("pass").unwrap().default, 0.0);
        assert_eq!(NodeKind::Blur.param_spec("kernel").unwrap().default, 0.0);
        // Legacy parameters remain continuous and linear.
        for kind in [
            NodeKind::CaptureEv,
            NodeKind::Exposure,
            NodeKind::WhiteBalance,
            NodeKind::Saturation,
        ] {
            for spec in kind.params() {
                assert_eq!((spec.kind, spec.logarithmic), (ParamKind::Continuous, false), "{}", spec.name);
            }
        }
    }

    #[test]
    fn individual_choice_and_toggle_values_are_per_image() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        n.mode = Mode::Individual;
        n.set_param("measure", A, 3.0);
        n.set_param("invert", A, 1.0);
        assert_eq!((n.choice("measure", A), n.toggle("invert", A)), (3, true));
        assert_eq!((n.choice("measure", B), n.toggle("invert", B)), (0, false));
    }

    #[test]
    fn load_rejects_invalid_choice_and_toggle_values() {
        let filter_graph = || {
            let mut g = Graph::new();
            let id = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
            g.node_mut(id).unwrap().set_param("measure", A, 2.0);
            serde_json::from_str::<serde_json::Value>(&g.to_json()).unwrap()
        };
        let load = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = filter_graph();
            edit(&mut v);
            Graph::from_json(&v.to_string())
        };
        assert!(load(&|_| {}).is_ok());
        assert_rejected(load(&|v| v["nodes"]["1"]["all"]["measure"] = serde_json::json!(1.5)), "not a whole option");
        assert_rejected(load(&|v| v["nodes"]["1"]["all"]["measure"] = serde_json::json!(6)), "outside 0..5");
        assert_rejected(load(&|v| v["nodes"]["1"]["all"]["measure"] = serde_json::json!(-1)), "outside 0..5");
        assert_rejected(load(&|v| v["nodes"]["1"]["all"]["invert"] = serde_json::json!(0.5)), "not a whole option");
        assert_rejected(load(&|v| v["nodes"]["1"]["all"]["invert"] = serde_json::json!(2)), "outside 0..1");
        assert_rejected(
            load(&|v| v["nodes"]["1"]["individual"] = serde_json::json!({"1": {"missing": 2}})),
            "outside 0..1",
        );
    }

    #[test]
    fn graphs_from_before_the_new_kinds_still_load() {
        let old = r#"{
            "format_version": 1,
            "nodes": {
                "1": {"id": 1, "kind": "Source", "mode": "All", "all": {}, "individual": {}, "pos": [0.0, 0.0]},
                "2": {"id": 2, "kind": "Exposure", "mode": "All", "all": {"ev": 1.0}, "individual": {}, "pos": [1.0, 0.0]}
            },
            "edges": [{"from": 1, "to": 2, "to_port": 0}],
            "next_id": 3
        }"#;
        let g = Graph::from_json(old).unwrap();
        assert_eq!(g.node(NodeId(2)).unwrap().kind, NodeKind::Exposure);
        assert_eq!(g.input_of(NodeId(2), 0), Some(NodeId(1)));
    }

    #[test]
    fn a_graph_with_every_node_kind_round_trips() {
        let mut g = Graph::new();
        let ids: Vec<NodeId> = NodeKind::ALL.iter().map(|k| g.add_node(*k, [0.0, 0.0])).collect();
        let id = |k: NodeKind| ids[NodeKind::ALL.iter().position(|x| *x == k).unwrap()];
        g.connect(id(NodeKind::Source), id(NodeKind::MetadataFilter), 0).unwrap();
        g.connect(id(NodeKind::MetadataFilter), id(NodeKind::Merge), 1).unwrap();
        g.connect(id(NodeKind::Source), id(NodeKind::Merge), 3).unwrap();
        g.connect(id(NodeKind::Merge), id(NodeKind::Contrast), 0).unwrap();
        g.connect(id(NodeKind::Contrast), id(NodeKind::HighlightsShadows), 0).unwrap();
        g.connect(id(NodeKind::HighlightsShadows), id(NodeKind::NoiseReduction), 0).unwrap();
        g.connect(id(NodeKind::NoiseReduction), id(NodeKind::Output), 0).unwrap();
        for kind in NodeKind::ALL {
            let node = g.node_mut(id(kind)).unwrap();
            let specs: Vec<ParamSpec> = kind.params().to_vec();
            for (i, spec) in specs.iter().enumerate() {
                node.set_param(spec.name, A, spec.min + (spec.max - spec.min) * (i as f32 + 1.0) / 7.0);
            }
        }
        let n = g.node_mut(id(NodeKind::MetadataFilter)).unwrap();
        n.mode = Mode::Individual;
        n.set_param("measure", B, 4.0);
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g));
    }

    // ---- wave 4 kinds ---------------------------------------------------

    const NEW_KINDS: [NodeKind; 8] = [
        NodeKind::Develop,
        NodeKind::ToneCurve,
        NodeKind::HueSaturation,
        NodeKind::Sharpen,
        NodeKind::Vignette,
        NodeKind::BlackWhite,
        NodeKind::LensManual,
        NodeKind::Transform,
    ];

    /// (name, min, max, default) of every parameter of `kind`.
    fn table(kind: NodeKind) -> Vec<(&'static str, f32, f32, f32)> {
        kind.params().iter().map(|s| (s.name, s.min, s.max, s.default)).collect()
    }

    #[test]
    fn develop_and_tone_parameter_tables() {
        let d = NodeKind::Develop;
        assert_eq!(d.label(), "Develop");
        let wb = d.param_spec("wb").unwrap();
        assert_eq!((wb.kind, wb.default), (ParamKind::Choice(&["As shot", "Custom"]), 0.0));
        let t = d.param_spec("temperature").unwrap();
        assert_eq!((t.min, t.max, t.default, t.unit, t.logarithmic), (2000.0, 25000.0, 6500.0, "K", true));
        assert_eq!(table(d)[8], ("tint", -150.0, 150.0, 0.0));
        assert_eq!(d.param_spec("baseline").unwrap().kind, ParamKind::Toggle);
        assert_eq!(d.param_spec("baseline").unwrap().default, 1.0);
        let b = d.param_spec("baseline_default").unwrap();
        assert_eq!((b.min, b.max, b.default, b.unit), (-2.0, 3.0, 0.35, "EV"));
        let br = d.param_spec("brightness").unwrap();
        assert_eq!((br.min, br.max, br.default, br.unit), (-3.0, 3.0, 1.6, "EV"));

        assert_eq!(
            table(NodeKind::ToneCurve),
            ["shadows", "darks", "lights", "highlights"].map(|n| (n, -1.0, 1.0, 0.0)).to_vec()
        );
    }

    #[test]
    fn hue_saturation_has_eighteen_band_parameters_in_order() {
        let t = table(NodeKind::HueSaturation);
        assert_eq!(t.len(), 18);
        let mut names = Vec::new();
        for band in ["red", "yellow", "green", "cyan", "blue", "magenta"] {
            for ch in ["h", "s", "l"] {
                names.push(format!("{band}_{ch}"));
            }
        }
        for (spec, name) in t.iter().zip(&names) {
            assert_eq!((spec.0, spec.1, spec.2, spec.3), (name.as_str(), -1.0, 1.0, 0.0));
        }
    }

    #[test]
    fn detail_colour_and_geometry_parameter_tables() {
        assert_eq!(
            table(NodeKind::Sharpen),
            vec![("amount", 0.0, 3.0, 0.0), ("radius", 0.05, 2.0, 0.15), ("threshold", 0.0, 1.0, 0.0)]
        );
        assert_eq!(NodeKind::Sharpen.param_spec("radius").unwrap().unit, "\u{2030} of long edge");
        assert_eq!(
            table(NodeKind::Vignette),
            vec![("amount", -1.0, 1.0, 0.0), ("midpoint", 0.0, 1.0, 0.5), ("feather", 0.0, 1.0, 0.5)]
        );
        assert_eq!(
            table(NodeKind::BlackWhite),
            vec![("red", -1.0, 2.0, 0.30), ("green", -1.0, 2.0, 0.59), ("blue", -1.0, 2.0, 0.11)]
        );
        assert_eq!(
            table(NodeKind::LensManual),
            vec![
                ("mode", 0.0, 1.0, 0.0),
                ("distortion", -0.5, 0.5, 0.0),
                ("vignetting", 0.0, 3.0, 0.0),
                ("ca_red", -5.0, 5.0, 0.0),
                ("ca_blue", -5.0, 5.0, 0.0)
            ]
        );
        assert_eq!(NodeKind::LensManual.param_spec("vignetting").unwrap().unit, "EV at corner");
        let t = table(NodeKind::Transform);
        assert_eq!(
            t,
            vec![
                ("left", 0.0, 1.0, 0.0),
                ("top", 0.0, 1.0, 0.0),
                ("right", 0.0, 1.0, 1.0),
                ("bottom", 0.0, 1.0, 1.0),
                ("angle", -45.0, 45.0, 0.0),
                ("rotate", 0.0, 3.0, 0.0),
                ("flip_h", 0.0, 1.0, 0.0),
                ("flip_v", 0.0, 1.0, 0.0)
            ]
        );
        assert_eq!(
            NodeKind::Transform.param_spec("rotate").unwrap().kind,
            ParamKind::Choice(&["0\u{b0}", "90\u{b0}", "180\u{b0}", "270\u{b0}"])
        );
        assert_eq!(NodeKind::Transform.param_spec("flip_h").unwrap().kind, ParamKind::Toggle);
    }

    #[test]
    fn labels_defaults_and_categories_of_every_kind() {
        let labels = [
            (NodeKind::Develop, "Develop", "Develop"),
            (NodeKind::ToneCurve, "Tone Curve", "Tone"),
            (NodeKind::HueSaturation, "Hue / Saturation", "Colour"),
            (NodeKind::Sharpen, "Sharpen", "Detail"),
            (NodeKind::Vignette, "Vignette", "Effects"),
            (NodeKind::BlackWhite, "Black & White", "Colour"),
            (NodeKind::LensManual, "Lens Correction", "Geometry"),
            (NodeKind::Transform, "Crop / Rotate", "Geometry"),
            (NodeKind::Source, "Source", "Input/Output"),
            (NodeKind::Output, "Output", "Input/Output"),
            (NodeKind::CaptureEv, "Capture EV", "Develop"),
            (NodeKind::Exposure, "Exposure", "Tone"),
            (NodeKind::Contrast, "Contrast", "Tone"),
            (NodeKind::HighlightsShadows, "Highlights / Shadows", "Tone"),
            (NodeKind::WhiteBalance, "White Balance (relative)", "Colour"),
            (NodeKind::Saturation, "Saturation", "Colour"),
            (NodeKind::NoiseReduction, "Noise Reduction", "Detail"),
            (NodeKind::Blur, "Filter", "Detail"),
            (NodeKind::PixelSort, "Pixel Sort", "Effects"),
            (NodeKind::NormalMap, "Normal Map", "Generators"),
            (NodeKind::Generator, "Generator", "Generators"),
            (NodeKind::Lut, "LUT", "Colour"),
            (NodeKind::DepthMap, "Depth Map", "Generators"),
            (NodeKind::MetadataFilter, "Metadata Filter", "Collection"),
            (NodeKind::Merge, "Merge", "Collection"),
            (NodeKind::PointCurve, "Curves", "Tone"),
            (NodeKind::Layer, "Layer", "Collection"),
            (NodeKind::Export, "Export", "Input/Output"),
            (NodeKind::Draw, "Draw", "Generators"),
            (NodeKind::MetadataSplit, "Split by Metadata", "Collection"),
            (NodeKind::ColorWheels, "Lift / Gamma / Gain", "Colour"),
            (NodeKind::Clarity, "Clarity / Texture", "Detail"),
        ];
        assert_eq!(labels.len(), NodeKind::ALL.len());
        for (kind, label, category) in labels {
            assert_eq!((kind.label(), kind.category()), (label, category), "{kind:?}");
        }
        // Defaults lie inside their ranges; a fresh node reports them.
        let mut g = Graph::new();
        for kind in NodeKind::ALL {
            let id = g.add_node(kind, [0.0, 0.0]);
            for s in kind.params() {
                assert!(s.min <= s.default && s.default <= s.max, "{kind:?}.{}", s.name);
                assert_eq!(g.node(id).unwrap().param(s.name, A), s.default);
            }
        }
        // Serialized names of existing kinds are unchanged.
        assert_eq!(serde_json::to_string(&NodeKind::HighlightsShadows).unwrap(), "\"HighlightsShadows\"");
        assert_eq!(serde_json::to_string(&NodeKind::Transform).unwrap(), "\"Transform\"");
    }

    #[test]
    fn develop_nodes_saved_without_brightness_get_the_default() {
        let old = r#"{
            "format_version": 1,
            "nodes": {"1": {"id": 1, "kind": "Develop", "mode": "All",
                "all": {"wb": 0, "baseline_default": 1.0}, "individual": {}, "pos": [0.0, 0.0]}},
            "edges": [], "next_id": 2
        }"#;
        let g = Graph::from_json(old).unwrap();
        let n = g.node(NodeId(1)).unwrap();
        assert_eq!(n.param("brightness", A), 1.6);
        assert_eq!(n.param("baseline_default", A), 1.0);
    }

    #[test]
    fn from_json_rejects_out_of_range_values_for_new_parameters() {
        let g = {
            let mut g = Graph::new();
            for k in NEW_KINDS {
                g.add_node(k, [0.0, 0.0]);
            }
            g
        };
        let load = |kind: NodeKind, param: &str, value: serde_json::Value| {
            let pos = NEW_KINDS.iter().position(|k| *k == kind).unwrap() + 1;
            let mut v: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
            v["nodes"][pos.to_string()]["all"][param] = value;
            Graph::from_json(&v.to_string())
        };
        use serde_json::json;
        assert!(load(NodeKind::Develop, "temperature", json!(6500.0)).is_ok());
        assert_rejected(load(NodeKind::Develop, "temperature", json!(1999.0)), "outside 2000..25000");
        assert_rejected(load(NodeKind::Develop, "tint", json!(151.0)), "outside -150..150");
        assert_rejected(load(NodeKind::Develop, "wb", json!(2)), "outside 0..1");
        assert_rejected(load(NodeKind::Develop, "baseline_default", json!(3.5)), "outside -2..3");
        assert_rejected(load(NodeKind::ToneCurve, "darks", json!(-1.5)), "outside -1..1");
        assert_rejected(load(NodeKind::HueSaturation, "magenta_l", json!(1.01)), "outside -1..1");
        assert_rejected(load(NodeKind::Sharpen, "radius", json!(0.01)), "outside 0.05..2");
        assert_rejected(load(NodeKind::Sharpen, "amount", json!(3.1)), "outside 0..3");
        assert_rejected(load(NodeKind::Vignette, "midpoint", json!(-0.1)), "outside 0..1");
        assert_rejected(load(NodeKind::BlackWhite, "blue", json!(2.5)), "outside -1..2");
        assert_rejected(load(NodeKind::LensManual, "ca_blue", json!(5.5)), "outside -5..5");
        assert_rejected(load(NodeKind::Transform, "angle", json!(46.0)), "outside -45..45");
        assert_rejected(load(NodeKind::Transform, "rotate", json!(4)), "outside 0..3");
        assert_rejected(load(NodeKind::Transform, "rotate", json!(1.5)), "not a whole option");
        assert_rejected(load(NodeKind::Transform, "flip_h", json!(2)), "outside 0..1");
    }

    // ---- collection membership ----------------------------------------

    fn caps(iso: &[(u64, Option<f32>)], flash: &[(u64, bool)]) -> impl Fn(ImageId) -> Option<CaptureInfo> {
        let iso: BTreeMap<u64, Option<f32>> = iso.iter().copied().collect();
        let flash: BTreeMap<u64, bool> = flash.iter().copied().collect();
        move |image: ImageId| {
            iso.get(&image.0).map(|iso| CaptureInfo {
                iso: *iso,
                flash_fired: flash.get(&image.0).copied(),
                ..Default::default()
            })
        }
    }

    // Images: 1 ISO 50, 2 ISO 100, 3 ISO 200, 4 ISO 400, 5 ISO 800, 6 no ISO, 7 unknown to the lookup.
    fn standard_caps() -> impl Fn(ImageId) -> Option<CaptureInfo> {
        caps(
            &[(1, Some(50.0)), (2, Some(100.0)), (3, Some(200.0)), (4, Some(400.0)), (5, Some(800.0)), (6, None)],
            &[(1, true), (2, false)],
        )
    }

    fn members(g: &Graph, node: NodeId, capture: CaptureFn) -> Vec<u64> {
        (1..=7).filter(|i| g.contains(node, ImageId(*i), capture)).collect()
    }

    fn filter_node(g: &mut Graph, input: NodeId, min: f32, max: f32) -> NodeId {
        let f = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        g.connect(input, f, 0).unwrap();
        let n = g.node_mut(f).unwrap();
        n.set_param("min", A, min);
        n.set_param("max", A, max);
        f
    }

    #[test]
    fn membership_follows_the_chain() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        let mask = g.add_node(NodeKind::Generator, [0.0, 0.0]);
        assert_eq!(members(&g, src, &capture), (1..=7).collect::<Vec<_>>());
        assert!(!g.contains(mask, A, &capture), "standalone generators have their own identity");
        assert!(!g.contains(exp, A, &capture), "unconnected required input");
        assert!(!g.contains(NodeId(999), A, &capture), "unknown node");
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, out, 0).unwrap();
        assert_eq!(members(&g, out, &capture), (1..=7).collect::<Vec<_>>());
    }

    #[test]
    fn iso_range_filter_with_and_without_invert() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let f = filter_node(&mut g, src, 100.0, 400.0);
        // Inclusive bounds; images 6 (no ISO) and 7 (no capture) are missing -> excluded.
        assert_eq!(members(&g, f, &capture), vec![2, 3, 4]);
        g.node_mut(f).unwrap().set_param("invert", A, 1.0);
        // Invert flips present values only; missing metadata still follows "missing" (Exclude).
        assert_eq!(members(&g, f, &capture), vec![1, 5]);
    }

    #[test]
    fn missing_metadata_follows_the_missing_option_and_ignores_invert() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let f = filter_node(&mut g, src, 100.0, 400.0);
        g.node_mut(f).unwrap().set_param("missing", A, 1.0); // Include
        assert_eq!(members(&g, f, &capture), vec![2, 3, 4, 6, 7]);
        g.node_mut(f).unwrap().set_param("invert", A, 1.0);
        assert_eq!(members(&g, f, &capture), vec![1, 5, 6, 7]);
    }

    #[test]
    fn flash_measure_selects_fired_unfired_and_unknown() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let f = filter_node(&mut g, src, 1.0, 1.0);
        g.node_mut(f).unwrap().set_param("measure", A, 4.0);
        assert_eq!(members(&g, f, &capture), vec![1], "fired only");
        g.node_mut(f).unwrap().set_param("min", A, 0.0);
        g.node_mut(f).unwrap().set_param("max", A, 0.0);
        assert_eq!(members(&g, f, &capture), vec![2], "not fired only");
        g.node_mut(f).unwrap().set_param("missing", A, 1.0);
        assert_eq!(members(&g, f, &capture), vec![2, 3, 4, 5, 6, 7], "unknown flash is included");
    }

    /// Images 1..=4 with keywords: 1 "Travel|Italy|Rome", 2 "travel" + "Food", 3 none, 4 "Ärger"; 5 and 6 have
    /// no keywords, 7 is unknown to the lookup.
    fn keyword_caps() -> impl Fn(ImageId) -> Option<CaptureInfo> {
        |image: ImageId| {
            let keywords: Vec<String> = match image.0 {
                1 => vec!["Travel|Italy|Rome".into()],
                2 => vec!["Food".into(), "travel".into()],
                4 => vec!["Ärger".into()],
                7 => return None,
                _ => vec![],
            };
            Some(CaptureInfo { keywords, ..Default::default() })
        }
    }

    #[test]
    fn keyword_measure_of_the_filter_contains_and_does_not_contain() {
        let capture = keyword_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let f = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        g.connect(src, f, 0).unwrap();
        g.node_mut(f).unwrap().set_param("measure", A, 5.0);
        assert!(g.node_mut(f).unwrap().set_data(A, Some(NodeData::Keyword("TRAVEL".into()))));
        assert_eq!(members(&g, f, &capture), vec![1, 2], "case-insensitive, any level of the path");
        g.node_mut(f).unwrap().set_param("keyword_op", A, 1.0);
        assert_eq!(members(&g, f, &capture), vec![3, 4, 5, 6, 7], "images without keywords do not contain it");
        g.node_mut(f).unwrap().set_param("invert", A, 1.0);
        assert_eq!(members(&g, f, &capture), vec![1, 2], "invert flips the test");
        g.node_mut(f).unwrap().set_param("invert", A, 0.0);
        assert!(g.node_mut(f).unwrap().set_data(A, Some(NodeData::Keyword("ärg".into()))));
        assert_eq!(members(&g, f, &capture), vec![1, 2, 3, 5, 6, 7]);
        // The numeric measures are unchanged and ignore the text.
        g.node_mut(f).unwrap().set_param("measure", A, 0.0);
        assert_eq!(members(&g, f, &capture), Vec::<u64>::new(), "no ISO anywhere, missing = exclude");
        // Saved and loaded again (the text is node data).
        let again = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(again, g);
    }

    #[test]
    fn keyword_measure_of_the_split_sends_images_to_the_first_matching_range() {
        let capture = keyword_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let s = split_node(&mut g, src, &[("food", 0.0, 0.0), ("travel", 0.0, 0.0)]);
        g.node_mut(s).unwrap().set_param("measure", A, 5.0);
        assert_eq!(port_members(&g, s, 0, &capture), vec![2]);
        assert_eq!(port_members(&g, s, 1, &capture), vec![1], "image 2 matches both and takes the first range");
        assert_eq!(port_members(&g, s, SPLIT_UNMATCHED_PORT, &capture), vec![3, 4, 5, 6, 7]);
        g.node_mut(s).unwrap().set_param("keyword_op", A, 1.0);
        assert_eq!(port_members(&g, s, 0, &capture), vec![1, 3, 4, 5, 6, 7], "does not contain food");
        assert_eq!(g.output_label(s, 0, A), "food");
        // "Missing metadata: Exclude" does not drop untagged images for the text measure.
        g.node_mut(s).unwrap().set_param("missing", A, 1.0);
        assert!(g.split_port(s, ImageId(3), &capture).is_some());
    }

    #[test]
    fn filters_chain_by_intersection_and_a_blocked_input_blocks_everything() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let a = filter_node(&mut g, src, 100.0, 800.0);
        let b = filter_node(&mut g, a, 0.0, 400.0);
        assert_eq!(members(&g, b, &capture), vec![2, 3, 4]);
        let lonely = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        assert_eq!(members(&g, lonely, &capture), Vec::<u64>::new());
    }

    #[test]
    fn merge_unites_branches_in_port_order() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let low = filter_node(&mut g, src, 0.0, 200.0); // images 1, 2, 3
        let high = filter_node(&mut g, src, 200.0, 1_000_000.0); // images 3, 4, 5
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        g.connect(low, merge, 0).unwrap();
        g.connect(high, merge, 1).unwrap();

        assert_eq!(members(&g, merge, &capture), vec![1, 2, 3, 4, 5], "6 and 7 are in neither branch");
        let source = |i: u64| g.merge_source(merge, ImageId(i), &capture);
        let candidates = |i: u64| g.merge_candidates(merge, ImageId(i), &capture);
        assert_eq!((source(1), candidates(1)), (Some(0), 1), "only in the first branch");
        assert_eq!((source(3), candidates(3)), (Some(0), 2), "in both: first port wins, flagged as duplicate");
        assert_eq!((source(5), candidates(5)), (Some(1), 1), "only in the second branch");
        assert_eq!((source(6), candidates(6)), (None, 0), "in none");
        assert_eq!(g.merge_source(src, A, &capture), None, "not a Merge");
        assert_eq!(g.merge_candidates(src, A, &capture), 0);
    }

    #[test]
    fn merge_with_gaps_and_no_inputs() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        assert!(!g.contains(merge, A, &capture));
        assert_eq!(g.merge_source(merge, A, &capture), None);
        let issues = g.validate();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].node, merge);

        g.connect(src, merge, 2).unwrap(); // port C only; A, B and D stay empty
        assert!(g.validate().is_empty(), "one connected input is enough");
        assert_eq!(g.merge_source(merge, A, &capture), Some(2));
        assert_eq!(g.merge_candidates(merge, A, &capture), 1);
        assert_eq!(g.connect(src, merge, 4), Err(GraphError::NoSuchPort { node: merge, port: 4 }));
    }

    #[test]
    fn individual_mode_thresholds_apply_per_image() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let f = filter_node(&mut g, src, 100.0, 400.0);
        let n = g.node_mut(f).unwrap();
        n.mode = Mode::Individual;
        n.set_param("max", ImageId(3), 150.0); // image 3 (ISO 200) now falls outside
        n.set_param("invert", ImageId(4), 1.0); // image 4 (ISO 400) is inverted -> out
        n.set_param("min", ImageId(1), 0.0); // image 1 (ISO 50) now inside
        assert_eq!(members(&g, f, &capture), vec![1, 2]);
    }

    #[test]
    fn capture_ev_delta_at_a_merge_follows_its_first_connected_input() {
        let mut g = Graph::new();
        let s1 = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let s2 = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let c1 = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        let c2 = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        g.node_mut(c1).unwrap().set_param("delta", A, 1.0);
        g.node_mut(c2).unwrap().set_param("delta", A, 2.0);
        g.connect(s1, c1, 0).unwrap();
        g.connect(s2, c2, 0).unwrap();
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        g.connect(c2, merge, 2).unwrap();
        g.connect(c1, merge, 3).unwrap();
        assert_eq!(g.capture_ev_delta(merge, A), 2.0);
        g.connect(c1, merge, 0).unwrap();
        assert_eq!(g.capture_ev_delta(merge, A), 1.0);
    }

    // ---- wave 5: several outputs, node data, split, sub-graphs ------------

    use serde_json::json;

    fn stroke(points: usize) -> BrushStroke {
        BrushStroke { points: vec![[0.5, 0.5]; points], radius: 0.05, hardness: 0.5, flow: 1.0, erase: false }
    }

    fn range(name: &str, min: f32, max: f32) -> NamedRange {
        NamedRange { name: name.to_owned(), min, max }
    }

    fn ranges(list: &[(&str, f32, f32)]) -> NodeData {
        NodeData::Ranges(list.iter().map(|(n, lo, hi)| range(n, *lo, *hi)).collect())
    }

    fn split_node(g: &mut Graph, input: NodeId, list: &[(&str, f32, f32)]) -> NodeId {
        let s = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        g.connect(input, s, 0).unwrap();
        assert!(g.node_mut(s).unwrap().set_data(A, Some(ranges(list))));
        s
    }

    fn port_members(g: &Graph, node: NodeId, port: usize, capture: CaptureFn) -> Vec<u64> {
        (1..=7).filter(|i| g.contains_output(node, port, ImageId(*i), capture)).collect()
    }

    /// Nodes: 1 PointCurve, 2 Draw, 3 MetadataSplit, 4 Exposure.
    fn data_graph() -> Graph {
        let mut g = Graph::new();
        let curve = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        let brush = g.add_node(NodeKind::Draw, [1.0, 0.0]);
        let split = g.add_node(NodeKind::MetadataSplit, [2.0, 0.0]);
        g.add_node(NodeKind::Exposure, [3.0, 0.0]);
        assert!(g.node_mut(curve).unwrap().set_data(A, Some(NodeData::Curve(vec![[0.0, 0.0], [0.5, 0.7], [1.0, 1.0]]))));
        assert!(g.node_mut(brush).unwrap().set_data(A, Some(NodeData::Strokes(vec![stroke(3)]))));
        assert!(g.node_mut(split).unwrap().set_data(A, Some(ranges(&[("dark", 0.0, 100.0), ("bright", 100.0, 800.0)]))));
        g
    }

    fn load_data_edited(edit: impl FnOnce(&mut serde_json::Value)) -> Result<Graph, GraphError> {
        let mut v: serde_json::Value = serde_json::from_str(&data_graph().to_json()).unwrap();
        edit(&mut v);
        Graph::from_json(&v.to_string())
    }

    #[test]
    fn old_files_load_and_equal_the_same_graph_built_through_the_api() {
        let old = r#"{
            "format_version": 1,
            "nodes": {
                "1": {"id": 1, "kind": "Source", "mode": "All", "all": {}, "individual": {}, "pos": [0.0, 0.0]},
                "2": {"id": 2, "kind": "Exposure", "mode": "All", "all": {"ev": 1.0}, "individual": {}, "pos": [1.0, 0.0]},
                "3": {"id": 3, "kind": "Saturation", "mode": "Individual", "all": {}, "individual": {"4": {"amount": 1.5}}, "pos": [2.0, 0.0]}
            },
            "edges": [{"from": 1, "to": 2, "to_port": 0}, {"from": 2, "to": 3, "to_port": 0}],
            "next_id": 4
        }"#;
        let loaded = Graph::from_json(old).unwrap();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [1.0, 0.0]);
        let sat = g.add_node(NodeKind::Saturation, [2.0, 0.0]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, sat, 0).unwrap();
        g.node_mut(exp).unwrap().set_param("ev", A, 1.0);
        let n = g.node_mut(sat).unwrap();
        n.mode = Mode::Individual;
        n.set_param("amount", ImageId(4), 1.5);
        assert_eq!(loaded, g);
        assert_eq!(loaded.edges()[0].from_port, 0);
        // Old features serialize without any of the new keys.
        let text = g.to_json();
        assert!(!text.contains("from_port") && !text.contains("\"data\"") && !text.contains("individual_data"), "{text}");
        assert_eq!(Graph::from_json(&text), Ok(g));
    }

    #[test]
    fn connect_is_connect_ports_with_output_zero() {
        let mut a = Graph::new();
        let mut b = Graph::new();
        for g in [&mut a, &mut b] {
            g.add_node(NodeKind::Source, [0.0, 0.0]);
            g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        }
        a.connect(NodeId(1), NodeId(2), 0).unwrap();
        b.connect_ports(NodeId(1), 0, NodeId(2), 0).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.input_edge(NodeId(2), 0), Some((NodeId(1), 0)));
        assert_eq!(a.input_edge(NodeId(2), 1), None);
        assert_eq!(a.input_of(NodeId(2), 0), Some(NodeId(1)));
    }

    #[test]
    fn output_ports_are_checked_when_connecting() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let sharpen = g.add_node(NodeKind::Sharpen, [0.0, 0.0]);
        let brush = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(src, split, 0).unwrap();

        assert_eq!(g.connect_ports(split, 7, exp, 0), Err(GraphError::NoSuchPort { node: split, port: 7 }));
        assert_eq!(g.connect_ports(src, 1, exp, 0), Err(GraphError::NoSuchPort { node: src, port: 1 }));
        assert_eq!(g.connect_ports(out, 0, exp, 0), Err(GraphError::NoOutput(out)));
        assert_eq!(g.connect_ports(brush, 1, sharpen, 1), Err(GraphError::NoSuchPort { node: brush, port: 1 }));
        for port in 0..7 {
            assert_eq!(g.connect_ports(split, port, exp, 0), Ok(()), "port {port}");
            assert_eq!(g.input_edges(exp, 0).last().map(|e| (e.from, e.from_port)), Some((split, port)));
            assert_eq!(g.edges().len(), port + 2);
        }
        let amount = g.expose_parameter(sharpen, "amount", PortType::Images).unwrap();
        assert_eq!(g.connect_ports(split, 3, sharpen, amount), Ok(()));
        assert_eq!(g.connect_ports(brush, 0, sharpen, amount), Ok(()));
        assert_eq!(g.connect_ports(brush, 0, sharpen, 0), Ok(()));
        // Feeding a split from its own output loops.
        assert_eq!(g.connect_ports(split, 2, split, 0), Err(GraphError::WouldCycle));
        g.connect_ports(split, 6, sharpen, 0).unwrap();
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g.clone()));
        assert!(g.to_json().contains("\"from_port\": 6"));
    }

    #[test]
    fn from_json_checks_output_ports() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let brush = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        g.connect(src, split, 0).unwrap();
        g.connect_ports(split, 5, exp, 0).unwrap();
        let ev = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(brush, exp, ev).unwrap();
        let load = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
            edit(&mut v);
            Graph::from_json(&v.to_string())
        };
        assert!(load(&|_| {}).is_ok());
        // Edge 1 is split(2) port 5 -> exposure(3); edge 2 is brush(4) -> exposure port 1.
        assert_rejected(load(&|v| v["edges"][1]["from_port"] = json!(7)), "no output port 7");
        assert_rejected(load(&|v| v["edges"][0]["from_port"] = json!(1)), "no output port 1");
        assert_rejected(load(&|v| v["edges"][2]["from_port"] = json!(1)), "no output port 1");
        assert_rejected(load(&|v| v["edges"][1]["from_port"] = json!(-1)), "");
    }

    #[test]
    fn outputs_are_consistent_with_output_for_every_kind() {
        for kind in NodeKind::ALL {
            let outs = kind.outputs();
            assert_eq!(kind.output(), outs.first().map(|o| o.ty), "{kind:?}");
            match kind {
                NodeKind::Output | NodeKind::Export => assert!(outs.is_empty()),
                NodeKind::MetadataSplit => {
                    assert_eq!(outs.len(), 7);
                    let labels: Vec<&str> = outs.iter().map(|o| o.label).collect();
                    assert_eq!(labels, ["Range 1", "Range 2", "Range 3", "Range 4", "Range 5", "Range 6", "Unmatched"]);
                    assert!(outs.iter().all(|o| o.ty == PortType::Images));
                }
                _ => assert_eq!(outs.len(), 1, "{kind:?}"),
            }
        }
        assert_eq!(NodeKind::Draw.output(), Some(PortType::Images));
        assert_eq!(NodeKind::PointCurve.output(), Some(PortType::Images));
    }

    #[test]
    fn data_kind_per_node_kind() {
        for kind in NodeKind::ALL {
            let expected = match kind {
                NodeKind::Generator => Some(DataKind::GradientRamp),
                NodeKind::PointCurve => Some(DataKind::Curve),
                NodeKind::Draw => Some(DataKind::Strokes),
                NodeKind::MetadataSplit => Some(DataKind::Ranges),
                NodeKind::Source => Some(DataKind::Folder),
                NodeKind::Export => Some(DataKind::Export),
                NodeKind::Lut => Some(DataKind::LutFile),
                NodeKind::MetadataFilter => Some(DataKind::Keyword),
                _ => None,
            };
            assert_eq!(kind.data_kind(), expected, "{kind:?}");
            assert_eq!(kind.data_kinds().first().copied(), expected, "{kind:?}");
        }
    }

    #[test]
    fn the_three_new_kinds_have_the_specified_shape() {
        let c = NodeKind::PointCurve;
        assert_eq!((c.label(), c.category()), ("Curves", "Tone"));
        assert_eq!(c.inputs().len(), 1);
        assert_eq!((c.inputs()[0].ty, c.inputs()[0].required), (PortType::Images, true));
        let ch = c.param_spec("channel").unwrap();
        assert_eq!(
            ch.kind,
            ParamKind::Choice(&["Luminance", "Red", "Green", "Blue", "Sat vs Sat", "Hue vs Hue", "Hue vs Sat"])
        );
        assert_eq!(ch.default, 0.0);
        assert_eq!(default_curve(), NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]]));
        let mut g = Graph::new();
        let curve_node = g.add_node(c, [0.0, 0.0]);
        assert!(g.node_mut(curve_node).unwrap().set_data(A, Some(default_curve())));

        let b = NodeKind::Draw;
        assert_eq!((b.label(), b.category()), ("Draw", "Generators"));
        assert_eq!(b.inputs(), OPTIONAL_IMAGE_INPUTS);
        assert_eq!(b.params(), DRAW_PARAMS);

        let s = NodeKind::MetadataSplit;
        assert_eq!((s.label(), s.category()), ("Split by Metadata", "Collection"));
        assert_eq!(s.inputs().len(), 1);
        assert_eq!(s.param_spec("measure").unwrap().kind, ParamKind::Choice(&Measure::LABELS));
        assert_eq!(s.param_spec("missing").unwrap().kind, ParamKind::Choice(&["Unmatched", "Exclude"]));
        assert_eq!(s.params().len(), 3);
        assert_eq!(s.param_spec("keyword_op").unwrap().kind, ParamKind::Choice(&["contains", "does not contain"]));
        assert!(NodeKind::ALL.contains(&c) && NodeKind::ALL.contains(&b) && NodeKind::ALL.contains(&s));
        assert_eq!(NodeKind::ALL.len(), 32);
    }

    #[test]
    fn set_data_enforces_the_curve_limits() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        let curve = |p: &[[f32; 2]]| Some(NodeData::Curve(p.to_vec()));
        assert!(n.data(A).is_none(), "no data means identity");
        assert!(n.set_data(A, curve(&[[0.0, 0.0], [1.0, 1.0]])));
        assert!(n.set_data(A, curve(&[[0.0, 1.0], [0.25, 0.0], [1.0, 0.5]])));
        let before = n.data(A).cloned();
        assert!(!n.set_data(A, curve(&[[0.5, 0.5]])), "one point");
        assert!(!n.set_data(A, curve(&[])), "no points");
        assert!(!n.set_data(A, curve(&[[0.0, 0.0], [0.5, 0.5], [0.5, 0.6]])), "x must strictly increase");
        assert!(!n.set_data(A, curve(&[[0.0, 0.0], [0.6, 0.5], [0.5, 0.6]])), "x going back");
        assert!(!n.set_data(A, curve(&[[0.0, 0.0], [1.1, 1.0]])), "x above 1");
        assert!(!n.set_data(A, curve(&[[-0.1, 0.0], [1.0, 1.0]])), "x below 0");
        assert!(!n.set_data(A, curve(&[[0.0, 0.0], [1.0, 1.5]])), "y above 1");
        assert!(!n.set_data(A, curve(&[[0.0, -0.5], [1.0, 1.0]])), "y below 0");
        assert!(!n.set_data(A, curve(&[[0.0, 0.0], [f32::NAN, 1.0]])));
        assert!(!n.set_data(A, curve(&[[0.0, f32::INFINITY], [1.0, 1.0]])));
        let many = |k: usize| (0..k).map(|i| [i as f32 / 40.0, 0.5]).collect::<Vec<_>>();
        assert!(n.set_data(A, curve(&many(32))));
        assert!(!n.set_data(A, curve(&many(33))), "33 points");
        assert_eq!(n.data(A), Some(&NodeData::Curve(many(32))), "a rejected call leaves the data alone");
        assert_ne!(before, n.data(A).cloned());
        // Wrong kinds.
        assert!(!n.set_data(A, Some(NodeData::Strokes(vec![]))));
        assert!(!n.set_data(A, Some(ranges(&[]))));
        // None removes.
        assert!(n.set_data(A, None));
        assert!(n.data(A).is_none());
    }

    #[test]
    fn set_data_enforces_the_stroke_limits_and_clamps() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        let strokes = |s: Vec<BrushStroke>| Some(NodeData::Strokes(s));
        assert!(n.set_data(A, strokes(vec![])));
        assert!(n.set_data(A, strokes(vec![stroke(0), stroke(1), stroke(20_000)])));
        assert!(!n.set_data(A, strokes(vec![stroke(20_001)])), "too many points in one stroke");
        assert!(n.set_data(A, strokes(vec![stroke(1); 2_000])));
        assert!(!n.set_data(A, strokes(vec![stroke(1); 2_001])), "too many strokes");

        let mut s = stroke(2);
        s.radius = 9.0;
        s.hardness = -1.0;
        s.flow = 3.0;
        assert!(n.set_data(B, strokes(vec![s.clone()])));
        let Some(NodeData::Strokes(stored)) = n.data(B) else { panic!("no strokes") };
        assert_eq!((stored[0].radius, stored[0].hardness, stored[0].flow), (0.5, 0.0, 1.0));
        s.radius = 0.0;
        assert!(n.set_data(B, strokes(vec![s.clone()])));
        let Some(NodeData::Strokes(stored)) = n.data(B) else { panic!("no strokes") };
        assert_eq!(stored[0].radius, 0.0005);

        // Points may slightly exceed the image but must be finite.
        s.radius = 0.1;
        s.points = vec![[-0.1, 1.2]];
        assert!(n.set_data(B, strokes(vec![s.clone()])));
        for bad in [[f32::NAN, 0.0], [0.0, f32::INFINITY]] {
            s.points = vec![bad];
            assert!(!n.set_data(B, strokes(vec![s.clone()])), "{bad:?}");
        }
        s.points = vec![[0.5, 0.5]];
        for set in [|s: &mut BrushStroke| s.radius = f32::NAN, |s: &mut BrushStroke| s.flow = f32::NAN, |s: &mut BrushStroke| s.hardness = f32::NAN] {
            let mut t = s.clone();
            set(&mut t);
            assert!(!n.set_data(B, strokes(vec![t])));
        }
        assert!(!n.set_data(B, Some(default_curve())), "wrong kind");
    }

    #[test]
    fn set_data_enforces_the_range_limits() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        assert!(n.set_data(A, Some(ranges(&[]))));
        assert!(n.set_data(A, Some(ranges(&[("a", 0.0, 0.0)]))), "min == max is a point range");
        let six = [("a", 0.0, 1.0), ("b", 1.0, 2.0), ("c", 2.0, 3.0), ("d", 3.0, 4.0), ("e", 4.0, 5.0), ("f", 5.0, 6.0)];
        assert!(n.set_data(A, Some(ranges(&six))));
        let mut seven = six.to_vec();
        seven.push(("g", 6.0, 7.0));
        assert!(!n.set_data(A, Some(ranges(&seven))), "7 ranges");
        assert!(!n.set_data(A, Some(ranges(&[("", 0.0, 1.0)]))), "empty name");
        assert!(n.set_data(A, Some(ranges(&[(&"x".repeat(40), 0.0, 1.0)]))));
        assert!(!n.set_data(A, Some(ranges(&[(&"x".repeat(41), 0.0, 1.0)]))), "41 characters");
        let umlauts = "\u{e4}".repeat(40);
        assert!(n.set_data(A, Some(ranges(&[(&umlauts, 0.0, 1.0)]))), "40 characters, not bytes");
        assert!(!n.set_data(A, Some(ranges(&[("a", 0.0, 1.0), ("a", 2.0, 3.0)]))), "names are unique");
        assert!(!n.set_data(A, Some(ranges(&[("a", 2.0, 1.0)]))), "min > max");
        assert!(!n.set_data(A, Some(ranges(&[("a", f32::NAN, 1.0)]))));
        assert!(!n.set_data(A, Some(ranges(&[("a", 0.0, f32::INFINITY)]))));
        assert!(!n.set_data(A, Some(NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]]))), "wrong kind");
        // Kinds without data refuse everything.
        let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        assert!(!g.node_mut(e).unwrap().set_data(A, Some(default_curve())));
        assert!(g.node(e).unwrap().data(A).is_none());
    }

    #[test]
    fn from_json_rejects_the_same_data_violations() {
        let bad = |node: &str, field: &str, image: bool, data: serde_json::Value, needle: &str| {
            let r = load_data_edited(|v| {
                if image {
                    v["nodes"][node][field]["1"] = data;
                } else {
                    v["nodes"][node][field] = data;
                }
            });
            assert_rejected(r, needle);
        };
        assert!(load_data_edited(|_| {}).is_ok());
        let pts = |n: usize| (0..n).map(|i| json!([i as f32 / 64.0, 0.5])).collect::<Vec<_>>();
        for (field, image) in [("data", false), ("individual_data", true)] {
            bad("1", field, image, json!({"Curve": [[0.5, 0.5]]}), "curve has 1 points");
            bad("1", field, image, json!({"Curve": pts(33)}), "curve has 33 points");
            bad("1", field, image, json!({"Curve": [[0.0, 0.0], [0.5, 0.5], [0.5, 0.6]]}), "strictly increasing");
            bad("1", field, image, json!({"Curve": [[0.0, 0.0], [1.5, 1.0]]}), "outside 0..1");
            bad("1", field, image, json!({"Curve": [[0.0, -0.2], [1.0, 1.0]]}), "outside 0..1");
            bad("1", field, image, json!({"Strokes": []}), "takes Curve data");
            let stroke_json = |points: usize, radius: f32, hardness: f32, flow: f32| {
                json!({"points": vec![json!([0.5, 0.5]); points], "radius": radius, "hardness": hardness, "flow": flow, "erase": false})
            };
            bad("2", field, image, json!({"Strokes": vec![stroke_json(1, 0.1, 0.5, 0.5); 2001]}), "2001 strokes exceed");
            bad("2", field, image, json!({"Strokes": [stroke_json(20_001, 0.1, 0.5, 0.5)]}), "20001 points");
            bad("2", field, image, json!({"Strokes": [stroke_json(1, 0.9, 0.5, 0.5)]}), "radius");
            bad("2", field, image, json!({"Strokes": [stroke_json(1, 0.0, 0.5, 0.5)]}), "radius");
            bad("2", field, image, json!({"Strokes": [stroke_json(1, 0.1, 1.5, 0.5)]}), "hardness or flow");
            bad("2", field, image, json!({"Strokes": [stroke_json(1, 0.1, 0.5, -0.5)]}), "hardness or flow");
            bad("2", field, image, json!({"Curve": [[0.0, 0.0], [1.0, 1.0]]}), "takes Strokes data");
            let r = |name: &str, lo: f32, hi: f32| json!({"name": name, "min": lo, "max": hi});
            bad("3", field, image, json!({"Ranges": (0..7).map(|i| r(&format!("r{i}"), 0.0, 1.0)).collect::<Vec<_>>()}), "7 ranges exceed");
            bad("3", field, image, json!({"Ranges": [r("", 0.0, 1.0)]}), "1..=40 characters");
            bad("3", field, image, json!({"Ranges": [r(&"y".repeat(41), 0.0, 1.0)]}), "1..=40 characters");
            bad("3", field, image, json!({"Ranges": [r("a", 0.0, 1.0), r("a", 1.0, 2.0)]}), "used twice");
            bad("3", field, image, json!({"Ranges": [r("a", 3.0, 1.0)]}), "min <= max");
            bad("3", field, image, json!({"Curve": [[0.0, 0.0], [1.0, 1.0]]}), "takes Ranges data");
            bad("4", field, image, json!({"Curve": [[0.0, 0.0], [1.0, 1.0]]}), "takes no data");
        }
        // Not representable at all.
        let r = load_data_edited(|v| v["nodes"]["1"]["data"] = json!({"Curve": [[0.0, 0.0], [1e39, 1.0]]}));
        assert!(matches!(r, Err(GraphError::Parse(_))), "{r:?}");
        let r = load_data_edited(|v| v["nodes"]["1"]["data"] = json!({"Blob": []}));
        assert!(matches!(r, Err(GraphError::Parse(_))), "{r:?}");
    }

    #[test]
    fn data_follows_the_all_individual_semantics() {
        let curve = |y: f32| Some(NodeData::Curve(vec![[0.0, 0.0], [0.5, y], [1.0, 1.0]]));
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        assert_eq!((n.individual_count(), n.has_individual(A)), (0, false));

        // All mode: one curve for everybody.
        assert!(n.set_data(A, curve(0.6)));
        assert_eq!(n.data(A), n.data(B));
        assert!(!n.has_individual_data(A) && !n.has_individual(A));

        // Individual mode: A gets its own, B inherits.
        n.mode = Mode::Individual;
        assert!(n.set_data(A, curve(0.8)));
        assert_eq!(n.data(A), curve(0.8).as_ref());
        assert_eq!(n.data(B), curve(0.6).as_ref());
        assert!(n.has_individual_data(A) && n.has_individual(A) && !n.has_individual(B));
        assert_eq!(n.individual_count(), 1);

        // Switching modes never discards anything.
        n.mode = Mode::All;
        assert_eq!(n.data(A), curve(0.6).as_ref());
        assert!(n.has_individual_data(A), "stored data survives while in All mode");
        n.mode = Mode::Individual;
        assert_eq!(n.data(A), curve(0.8).as_ref());

        // Parameters and data count as one set of per-image edits.
        n.set_param("channel", B, 2.0);
        assert_eq!(n.individual_count(), 2);
        n.set_param("channel", A, 1.0);
        assert_eq!(n.individual_count(), 2, "A has both a value and data but counts once");
        assert!(n.has_individual(B) && !n.has_individual_data(B));

        // Removing in Individual mode removes only the image's data.
        assert!(n.set_data(A, None));
        assert_eq!(n.data(A), curve(0.6).as_ref());
        assert!(n.has_individual(A), "A still has its parameter value");
        assert!(n.set_data(A, curve(0.9)));
        n.clear_individual(A);
        assert_eq!((n.data(A), n.param("channel", A)), (curve(0.6).as_ref(), 0.0));
        assert!(!n.has_individual(A) && !n.has_individual_data(A));
        assert_eq!(n.individual_count(), 1);
        n.clear_individual(B);
        assert_eq!(n.individual_count(), 0);

        // None in All mode drops the All data.
        n.mode = Mode::All;
        assert!(n.set_data(A, None));
        assert!(n.data(A).is_none() && n.data(B).is_none());
        // A data-only individual image is reported by the count.
        n.mode = Mode::Individual;
        assert!(n.set_data(B, curve(0.7)));
        assert_eq!((n.individual_count(), n.has_individual(B)), (1, true));
    }

    #[test]
    fn draw_starts_in_individual_mode() {
        let mut g = Graph::new();
        for kind in NodeKind::ALL {
            let id = g.add_node(kind, [0.0, 0.0]);
            let expected = if kind == NodeKind::Draw { Mode::Individual } else { Mode::All };
            assert_eq!(g.node(id).unwrap().mode, expected, "{kind:?}");
        }
        let brush = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        let n = g.node_mut(brush).unwrap();
        assert!(n.set_data(A, Some(NodeData::Strokes(vec![stroke(2)]))));
        assert!(n.data(A).is_some(), "painted for A");
        assert!(n.data(B).is_none(), "B is untouched");
        let capture = |_: ImageId| None;
        assert!(!g.contains(brush, B, &capture));
        assert!(g.contains(brush, ImageId(u64::MAX - brush.0), &capture));
        let source = g.add_node(NodeKind::Source, [0.0; 2]);
        g.connect(source, brush, 0).unwrap();
        assert!(g.contains(brush, B, &capture));
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g));
    }

    #[test]
    fn data_round_trips_through_json_in_both_scopes() {
        let mut g = data_graph();
        let curve = g.nodes().next().unwrap().id;
        let n = g.node_mut(curve).unwrap();
        n.set_data(A, Some(NodeData::Curve(vec![[0.0, 0.1], [1.0, 0.9]])));
        n.mode = Mode::Individual;
        n.set_data(B, Some(default_curve()));
        let text = g.to_json();
        assert!(text.contains("individual_data") && text.contains("\"data\""));
        assert_eq!(Graph::from_json(&text), Ok(g));
    }

    #[test]
    fn output_labels_name_split_ranges() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("Low", 0.0, 100.0), ("High", 100.0, 800.0)]);
        assert_eq!(g.output_label(split, 0, A), "Low");
        assert_eq!(g.output_label(split, 1, A), "High");
        assert_eq!(g.output_label(split, 2, A), "Range 3", "undefined ranges keep the static label");
        assert_eq!(g.output_label(split, 5, A), "Range 6");
        assert_eq!(g.output_label(split, 6, A), "Unmatched");
        assert_eq!(g.output_label(split, 7, A), "");
        assert_eq!(g.output_label(NodeId(99), 0, A), "");
        assert_eq!(g.output_label(src, 0, A), "Images");
        let brush = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        assert_eq!(g.output_label(brush, 0, A), "Images");
        // Individual ranges are per image.
        let n = g.node_mut(split).unwrap();
        n.mode = Mode::Individual;
        n.set_data(B, Some(ranges(&[("Only", 0.0, 1.0)])));
        assert_eq!((g.output_label(split, 0, B), g.output_label(split, 1, B)), ("Only".to_owned(), "Range 2".to_owned()));
        assert_eq!(g.output_label(split, 0, A), "Low");
    }

    #[test]
    fn split_sends_each_image_through_exactly_one_port() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("low", 0.0, 100.0), ("mid", 250.0, 400.0)]);
        // 1 (50) and 2 (100) low; 3 (200) lies in no range; 4 (400) mid; 5 (800) none; 6, 7 have no ISO.
        let port = |i: u64| g.split_port(split, ImageId(i), &capture);
        assert_eq!(
            (port(1), port(2), port(3), port(4), port(5), port(6), port(7)),
            (Some(0), Some(0), Some(6), Some(1), Some(6), Some(6), Some(6))
        );
        assert_eq!(port_members(&g, split, 0, &capture), vec![1, 2]);
        assert_eq!(port_members(&g, split, 1, &capture), vec![4]);
        assert_eq!(port_members(&g, split, 6, &capture), vec![3, 5, 6, 7]);
        for dead in 2..6 {
            assert!(port_members(&g, split, dead, &capture).is_empty(), "undefined range {dead}");
        }
        assert!(port_members(&g, split, 7, &capture).is_empty(), "no such port");
        assert_eq!(members(&g, split, &capture), (1..=7).collect::<Vec<_>>(), "contains(split) = in the input");
        let total: usize = (0..7).map(|p| port_members(&g, split, p, &capture).len()).sum();
        assert_eq!(total, 7);
        assert_eq!(g.split_port(src, A, &capture), None, "not a split");
        assert_eq!(g.split_port(NodeId(99), A, &capture), None);
        // Single-output nodes only have port 0.
        assert!(g.contains_output(src, 0, A, &capture));
        assert!(!g.contains_output(src, 1, A, &capture));
    }

    #[test]
    fn overlapping_ranges_first_one_wins() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("wide", 0.0, 400.0), ("narrow", 100.0, 200.0), ("rest", 100.0, 1000.0)]);
        assert_eq!(port_members(&g, split, 0, &capture), vec![1, 2, 3, 4]);
        assert!(port_members(&g, split, 1, &capture).is_empty(), "fully shadowed by the first range");
        assert_eq!(port_members(&g, split, 2, &capture), vec![5]);
        assert_eq!(g.split_port(split, ImageId(2), &capture), Some(0), "bounds are inclusive");
    }

    #[test]
    fn missing_values_follow_the_missing_option() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("all", 0.0, 1_000_000.0)]);
        assert_eq!(port_members(&g, split, 6, &capture), vec![6, 7], "Unmatched is the default");
        assert_eq!(g.split_port(split, ImageId(6), &capture), Some(6));
        g.node_mut(split).unwrap().set_param("missing", A, 1.0); // Exclude
        assert_eq!(g.split_port(split, ImageId(6), &capture), None, "no ISO");
        assert_eq!(g.split_port(split, ImageId(7), &capture), None, "no capture info");
        assert_eq!(port_members(&g, split, 0, &capture), vec![1, 2, 3, 4, 5]);
        assert!(port_members(&g, split, 6, &capture).is_empty());
        assert!((0..7).all(|p| !port_members(&g, split, p, &capture).contains(&6)));
    }

    #[test]
    fn images_outside_the_input_collection_leave_through_no_port() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let filter = filter_node(&mut g, src, 100.0, 400.0); // images 2, 3, 4
        let split = split_node(&mut g, filter, &[("a", 0.0, 200.0), ("b", 200.0, 1000.0)]);
        assert_eq!(port_members(&g, split, 0, &capture), vec![2, 3]);
        assert_eq!(port_members(&g, split, 1, &capture), vec![4]);
        assert!(port_members(&g, split, 6, &capture).is_empty(), "6 and 7 are not in the input at all");
        assert_eq!(g.split_port(split, ImageId(1), &capture), None);
        assert_eq!(g.split_port(split, ImageId(5), &capture), None);
        assert_eq!(members(&g, split, &capture), vec![2, 3, 4]);
        // An unconnected split carries nothing.
        let lonely = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        assert_eq!(g.split_port(lonely, A, &capture), None);
        assert!(!g.contains(lonely, A, &capture));
    }

    #[test]
    fn consumers_follow_their_input_port() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("low", 0.0, 100.0), ("high", 400.0, 1000.0)]);
        let low = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let high = g.add_node(NodeKind::Contrast, [0.0, 0.0]);
        let rest = g.add_node(NodeKind::Saturation, [0.0, 0.0]);
        let undefined = g.add_node(NodeKind::Blur, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect_ports(split, 0, low, 0).unwrap();
        g.connect_ports(split, 1, high, 0).unwrap();
        g.connect_ports(split, 6, rest, 0).unwrap();
        g.connect_ports(split, 4, undefined, 0).unwrap();
        g.connect(low, out, 0).unwrap();
        assert_eq!(members(&g, low, &capture), vec![1, 2]);
        assert_eq!(members(&g, high, &capture), vec![4, 5]);
        assert_eq!(members(&g, rest, &capture), vec![3, 6, 7]);
        assert_eq!(members(&g, undefined, &capture), Vec::<u64>::new(), "range 5 is not defined");
        assert_eq!(members(&g, out, &capture), vec![1, 2], "membership continues down the chain");
        // The same node on the other branch of a merge still resolves per edge.
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        g.connect(high, merge, 0).unwrap();
        g.connect(rest, merge, 1).unwrap();
        assert_eq!(members(&g, merge, &capture), vec![3, 4, 5, 6, 7]);
        assert!(g.validate().iter().all(|i| i.node == undefined || i.node == merge || i.node == out), "{:?}", g.validate());
    }

    #[test]
    fn branches_recombine_through_merge_with_every_image_once() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("low", 0.0, 100.0), ("mid", 250.0, 400.0)]);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        g.connect_ports(split, 0, merge, 0).unwrap();
        g.connect_ports(split, 1, exp, 0).unwrap();
        g.connect(exp, merge, 1).unwrap();
        g.connect_ports(split, 6, merge, 2).unwrap();
        assert_eq!(members(&g, merge, &capture), (1..=7).collect::<Vec<_>>());
        let mut seen = Vec::new();
        for i in 1..=7 {
            let id = ImageId(i);
            assert_eq!(g.merge_candidates(merge, id, &capture), 1, "image {i} must arrive exactly once");
            seen.push(g.merge_source(merge, id, &capture).unwrap());
        }
        assert_eq!(seen, vec![0, 0, 2, 1, 2, 2, 2]);
        let counts: Vec<usize> = [0, 1, 2].iter().map(|p| seen.iter().filter(|s| *s == p).count()).collect();
        assert_eq!(counts.iter().sum::<usize>(), 7, "counts add up: {counts:?}");
        // Excluding images without a value drops them from the recombined collection.
        g.node_mut(split).unwrap().set_param("missing", A, 1.0);
        assert_eq!(members(&g, merge, &capture), vec![1, 2, 3, 4, 5]);
        // Feeding the same range twice is a duplicate, resolved by port order.
        g.connect_ports(split, 0, merge, 3).unwrap();
        assert_eq!(g.merge_candidates(merge, ImageId(1), &capture), 2);
        assert_eq!(g.merge_source(merge, ImageId(1), &capture), Some(0));
    }

    #[test]
    fn split_ranges_can_differ_per_image_in_individual_mode() {
        let capture = standard_caps();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = split_node(&mut g, src, &[("shared", 0.0, 100.0)]);
        let n = g.node_mut(split).unwrap();
        n.mode = Mode::Individual;
        assert!(n.set_data(ImageId(3), Some(ranges(&[("none", 9000.0, 9001.0), ("own", 200.0, 200.0)]))));
        n.set_param("measure", ImageId(5), 2.0); // image 5 is measured by aperture, which it lacks
        let port = |i: u64| g.split_port(split, ImageId(i), &capture);
        assert_eq!(port(1), Some(0), "inherits the shared range");
        assert_eq!(port(2), Some(0));
        assert_eq!(port(3), Some(1), "its own second range");
        assert_eq!(port(4), Some(6), "outside the shared range");
        assert_eq!(port(5), Some(6), "no aperture value, missing = Unmatched");
        assert_eq!(port_members(&g, split, 1, &capture), vec![3]);
        assert_eq!(g.output_label(split, 1, ImageId(3)), "own");
        assert_eq!(g.output_label(split, 1, ImageId(4)), "Range 2");
    }

    #[test]
    fn capture_ev_delta_flows_through_split_ports() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let cap = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        g.node_mut(cap).unwrap().set_param("delta", A, 1.0);
        g.connect(src, cap, 0).unwrap();
        let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        g.connect(cap, split, 0).unwrap();
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        g.connect_ports(split, 6, exp, 0).unwrap();
        assert_eq!(g.capture_ev_delta(exp, A), 1.0);
    }

    // ---- sub-graphs ---------------------------------------------------------

    /// 1 Source, 2 Exposure, 3 Saturation, 4 Draw, 5 Output, 6 MetadataSplit, 7 Contrast.
    fn recipe_graph() -> Graph {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [10.0, 0.0]);
        let sat = g.add_node(NodeKind::Saturation, [20.0, 0.0]);
        let brush = g.add_node(NodeKind::Draw, [20.0, 10.0]);
        let out = g.add_node(NodeKind::Output, [30.0, 0.0]);
        let split = g.add_node(NodeKind::MetadataSplit, [10.0, 20.0]);
        let con = g.add_node(NodeKind::Contrast, [20.0, 20.0]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, sat, 0).unwrap();
        let amount = g.expose_parameter(sat, "amount", PortType::Images).unwrap();
        g.connect(brush, sat, amount).unwrap();
        g.connect(sat, out, 0).unwrap();
        g.connect(exp, split, 0).unwrap();
        g.connect_ports(split, 3, con, 0).unwrap();
        g.node_mut(exp).unwrap().set_param("ev", A, 1.5);
        // Saturation: All value 1.2 plus a per-image value for A.
        let n = g.node_mut(sat).unwrap();
        n.set_param("amount", A, 1.2);
        n.mode = Mode::Individual;
        n.set_param("amount", A, 1.8);
        // Draw: All data plus strokes for A.
        let n = g.node_mut(brush).unwrap();
        n.mode = Mode::All;
        assert!(n.set_data(A, Some(NodeData::Strokes(vec![stroke(1)]))));
        n.mode = Mode::Individual;
        assert!(n.set_data(A, Some(NodeData::Strokes(vec![stroke(5), stroke(6)]))));
        assert!(g.node_mut(split).unwrap().set_data(A, Some(ranges(&[("x", 0.0, 1.0)]))));
        g
    }

    #[test]
    fn extracting_keeps_internal_edges_and_drops_external_ones() {
        let g = recipe_graph();
        let (exp, sat, brush, split, con) = (NodeId(2), NodeId(3), NodeId(4), NodeId(6), NodeId(7));
        let sub = g.extract_subgraph(&[exp, sat, brush, split, con]);
        assert_eq!(sub.node_count(), 5);
        assert!(!sub.is_empty());
        assert_eq!(
            sub.kinds(),
            vec![NodeKind::Exposure, NodeKind::Saturation, NodeKind::Draw, NodeKind::MetadataSplit, NodeKind::Contrast]
        );

        let mut target = Graph::new();
        let ids = target.insert_subgraph(&sub, [0.0, 0.0]);
        assert_eq!(ids.len(), 5);
        // Source -> Exposure and Saturation -> Output left the selection and are gone.
        assert_eq!(target.edges().len(), 4);
        assert_eq!(target.input_edge(ids[1], 0), Some((ids[0], 0)));
        assert_eq!(target.input_edge(ids[1], 1), Some((ids[2], 0)));
        assert_eq!(target.input_edge(ids[3], 0), Some((ids[0], 0)));
        assert_eq!(target.input_edge(ids[4], 0), Some((ids[3], 3)), "the output port is kept");
        assert_eq!(target.input_edge(ids[0], 0), None);
        assert!(target.nodes().all(|n| n.kind != NodeKind::Source && n.kind != NodeKind::Output));
        assert_eq!(Graph::from_json(&target.to_json()), Ok(target.clone()));
    }

    #[test]
    fn extracting_leaves_per_image_values_and_data_behind() {
        let g = recipe_graph();
        let sub = g.extract_subgraph(&[NodeId(2), NodeId(3), NodeId(4), NodeId(6)]);
        let mut target = Graph::new();
        let ids = target.insert_subgraph(&sub, [0.0, 0.0]);
        let (exp, sat, brush, split) = (ids[0], ids[1], ids[2], ids[3]);
        assert_eq!(target.node(exp).unwrap().param("ev", B), 1.5, "All values come along");
        let n = target.node(sat).unwrap();
        assert_eq!(n.mode, Mode::Individual, "the mode is kept");
        assert_eq!(n.param("amount", A), 1.2, "the All value comes along, the per-image one does not");
        assert!(!n.has_individual(A));
        let n = target.node(brush).unwrap();
        assert_eq!(n.mode, Mode::Individual);
        assert_eq!(n.data(A), Some(&NodeData::Strokes(vec![stroke(1)])), "All data comes along");
        assert!(!n.has_individual_data(A));
        let n = target.node(split).unwrap();
        assert_eq!(n.mode, Mode::All);
        assert_eq!(n.data(A), Some(&ranges(&[("x", 0.0, 1.0)])));
        // Nothing of the original changed.
        assert!(g.node(NodeId(3)).unwrap().has_individual(A));
        assert!(g.node(NodeId(4)).unwrap().has_individual_data(A));
    }

    #[test]
    fn inserting_uses_fresh_ids_and_shifts_positions() {
        let mut g = recipe_graph();
        let before: Vec<NodeId> = g.nodes().map(|n| n.id).collect();
        let edges_before = g.edges().to_vec();
        let sub = g.extract_subgraph(&[NodeId(3), NodeId(2), NodeId(4)]);
        let ids = g.insert_subgraph(&sub, [100.0, 50.0]);
        assert_eq!(ids.len(), 3);
        assert!(ids.iter().all(|id| !before.contains(id)), "fresh ids");
        assert_eq!(ids, vec![NodeId(8), NodeId(9), NodeId(10)]);
        assert_eq!(g.node(ids[0]).unwrap().kind, NodeKind::Saturation, "the order of the extraction call");
        assert_eq!(g.node(ids[0]).unwrap().pos, [120.0, 50.0]);
        assert_eq!(g.node(ids[1]).unwrap().pos, [110.0, 50.0]);
        assert_eq!(g.node(ids[2]).unwrap().pos, [120.0, 60.0]);
        // Existing nodes and edges are untouched; the copy is wired to itself only.
        assert_eq!(&g.edges()[..edges_before.len()], &edges_before[..]);
        let new_edges: Vec<&Edge> = g.edges()[edges_before.len()..].iter().collect();
        assert_eq!(new_edges.len(), 2);
        assert!(new_edges.iter().all(|e| ids.contains(&e.from) && ids.contains(&e.to)));
        assert_eq!(g.nodes().filter(|n| n.kind == NodeKind::Source).count(), 1, "no second Source");
        // Inserting twice gives two independent copies.
        let again = g.insert_subgraph(&sub, [0.0, 0.0]);
        assert!(again.iter().all(|id| !ids.contains(id)));
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g));
    }

    #[test]
    fn a_source_is_only_copied_when_the_subgraph_contains_one() {
        let mut g = recipe_graph();
        let sub = g.extract_subgraph(&[NodeId(1), NodeId(2)]);
        let ids = g.insert_subgraph(&sub, [0.0, 100.0]);
        assert_eq!(g.nodes().filter(|n| n.kind == NodeKind::Source).count(), 2);
        assert_eq!(g.input_edge(ids[1], 0), Some((ids[0], 0)));
        let none = g.extract_subgraph(&[NodeId(2)]);
        g.insert_subgraph(&none, [0.0, 0.0]);
        assert_eq!(g.nodes().filter(|n| n.kind == NodeKind::Source).count(), 2);
    }

    #[test]
    fn empty_and_unknown_selections_give_an_empty_subgraph() {
        let mut g = recipe_graph();
        let sub = g.extract_subgraph(&[]);
        assert!(sub.is_empty());
        assert!(g.extract_subgraph(&[NodeId(99)]).is_empty());
        let count = g.nodes().count();
        assert!(g.insert_subgraph(&sub, [1.0, 1.0]).is_empty());
        assert_eq!(g.nodes().count(), count);
        assert_eq!(Subgraph::from_json(&sub.to_json()), Ok(sub));
        // Repeated ids count once.
        assert_eq!(g.extract_subgraph(&[NodeId(2), NodeId(2)]).node_count(), 1);
    }

    #[test]
    fn subgraph_json_round_trips() {
        let g = recipe_graph();
        let all: Vec<NodeId> = g.nodes().map(|n| n.id).collect();
        let sub = g.extract_subgraph(&all);
        assert_eq!(Subgraph::from_json(&sub.to_json()), Ok(sub.clone()));
        let text = sub.to_json();
        assert!(!text.contains("individual"), "{text}");
        let mut a = Graph::new();
        let mut b = Graph::new();
        a.insert_subgraph(&sub, [0.0, 0.0]);
        b.insert_subgraph(&Subgraph::from_json(&text).unwrap(), [0.0, 0.0]);
        assert_eq!(a, b);
    }

    #[test]
    fn invalid_subgraph_json_is_rejected() {
        let g = recipe_graph();
        // Order: 0 Exposure, 1 Saturation, 2 Draw, 3 MetadataSplit, 4 Contrast.
        let sub = g.extract_subgraph(&[NodeId(2), NodeId(3), NodeId(4), NodeId(6), NodeId(7)]);
        let base: serde_json::Value = serde_json::from_str(&sub.to_json()).unwrap();
        let load = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = base.clone();
            edit(&mut v);
            Subgraph::from_json(&v.to_string())
        };
        assert!(load(&|_| {}).is_ok());
        let parse_err = |r: Result<Subgraph, GraphError>, needle: &str| match r {
            Err(GraphError::Parse(m)) => assert!(m.contains(needle), "{m:?} lacks {needle:?}"),
            other => panic!("expected a Parse error mentioning {needle:?}, got {other:?}"),
        };
        assert!(matches!(Subgraph::from_json("nope"), Err(GraphError::Parse(_))));
        assert!(matches!(Subgraph::from_json("{}"), Err(GraphError::Parse(_))));
        assert_eq!(load(&|v| v["format_version"] = json!(99)), Err(GraphError::UnsupportedSubgraphVersion(99)));
        parse_err(load(&|v| v["nodes"][0]["kind"] = json!("Teleporter")), "Teleporter");
        parse_err(load(&|v| v["nodes"][0]["all"]["ev"] = json!(9.0)), "outside -6..6");
        parse_err(load(&|v| v["nodes"][0]["all"]["nope"] = json!(1.0)), "unknown parameter");
        parse_err(load(&|v| v["nodes"][1]["all"]["amount"] = json!(-1.0)), "outside 0..2");
        parse_err(load(&|v| v["edges"][0]["to"] = json!(40)), "has 5 nodes");
        parse_err(load(&|v| v["edges"][0]["to_port"] = json!(9)), "has no input 9");
        parse_err(load(&|v| v["edges"][0]["from_port"] = json!(3)), "no output port 3");
        parse_err(load(&|v| { let edge = v["edges"][1].clone(); v["edges"].as_array_mut().unwrap().push(edge); }), "connected more than once");
        // exposure -> saturation plus saturation -> exposure.
        parse_err(
            load(&|v| {
                v["edges"] = json!([{"from": 0, "to": 1, "to_port": 0}, {"from": 1, "to": 0, "to_port": 0}]);
            }),
            "cycle",
        );
        parse_err(load(&|v| v["nodes"][3]["data"] = json!({"Ranges": [{"name": "", "min": 0.0, "max": 1.0}]})), "characters");
        parse_err(load(&|v| v["nodes"][2]["data"] = json!({"Curve": [[0.0, 0.0], [1.0, 1.0]]})), "takes Strokes");
        parse_err(load(&|v| v["nodes"][0]["data"] = json!({"Curve": [[0.0, 0.0], [1.0, 1.0]]})), "takes no data");
    }

    #[test]
    fn a_graph_with_every_kind_data_and_ports_round_trips() {
        let mut g = Graph::new();
        let ids: Vec<NodeId> = NodeKind::ALL.iter().map(|k| g.add_node(*k, [1.0, 2.0])).collect();
        let id = |k: NodeKind| ids[NodeKind::ALL.iter().position(|x| *x == k).unwrap()];
        g.connect(id(NodeKind::Source), id(NodeKind::MetadataSplit), 0).unwrap();
        g.connect_ports(id(NodeKind::MetadataSplit), 0, id(NodeKind::PointCurve), 0).unwrap();
        g.connect_ports(id(NodeKind::MetadataSplit), 6, id(NodeKind::Merge), 1).unwrap();
        g.connect(id(NodeKind::PointCurve), id(NodeKind::Merge), 0).unwrap();
        g.connect(id(NodeKind::Merge), id(NodeKind::Exposure), 0).unwrap();
        let ev = g.expose_parameter(id(NodeKind::Exposure), "ev", PortType::Images).unwrap();
        g.connect(id(NodeKind::Draw), id(NodeKind::Exposure), ev).unwrap();
        g.connect(id(NodeKind::Exposure), id(NodeKind::Output), 0).unwrap();
        for kind in NodeKind::ALL {
            let node = g.node_mut(id(kind)).unwrap();
            let specs: Vec<ParamSpec> = kind.params().to_vec();
            for (i, spec) in specs.iter().enumerate() {
                node.set_param(spec.name, A, spec.min + (spec.max - spec.min) * (i as f32 + 1.0) / 7.0);
            }
        }
        let n = g.node_mut(id(NodeKind::PointCurve)).unwrap();
        assert!(n.set_data(A, Some(NodeData::Curve(vec![[0.0, 0.2], [0.4, 0.5], [1.0, 0.9]]))));
        n.mode = Mode::Individual;
        assert!(n.set_data(B, Some(default_curve())));
        let n = g.node_mut(id(NodeKind::Draw)).unwrap();
        assert!(n.set_data(A, Some(NodeData::Strokes(vec![stroke(4)]))));
        n.mode = Mode::All;
        assert!(n.set_data(A, Some(NodeData::Strokes(vec![stroke(2)]))));
        let n = g.node_mut(id(NodeKind::MetadataSplit)).unwrap();
        assert!(n.set_data(A, Some(ranges(&[("dark", 0.0, 100.0), ("bright", 100.0, 5000.0)]))));
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g));
    }

    // ---- wave 7: colour wheels, clarity, ISO-adaptive noise reduction -------

    #[test]
    fn colour_wheels_and_clarity_parameter_tables() {
        let cw = NodeKind::ColorWheels;
        let t = table(cw);
        let mut want = Vec::new();
        for (base, lo, hi) in [("lift", -0.5, 0.5), ("gamma", -1.0, 1.0), ("gain", -2.0, 2.0)] {
            want.push((base.to_owned(), lo, hi, 0.0));
            for c in ["r", "g", "b"] {
                want.push((format!("{base}_{c}"), lo, hi, 0.0));
            }
        }
        assert_eq!(t.len(), 12);
        for (got, w) in t.iter().zip(&want) {
            assert_eq!((got.0, got.1, got.2, got.3), (w.0.as_str(), w.1, w.2, w.3));
        }
        for s in cw.params() {
            assert_eq!(s.kind, ParamKind::Continuous);
            assert_eq!(s.unit, if s.name.starts_with("gain") { "EV" } else { "" }, "{}", s.name);
        }
        assert_eq!(table(NodeKind::Clarity), vec![("clarity", -1.0, 1.0, 0.0), ("texture", -1.0, 1.0, 0.0)]);
        for kind in [cw, NodeKind::Clarity] {
            let ins: Vec<_> = kind.inputs().iter().map(|i| (i.ty, i.required)).collect();
            assert_eq!(ins, vec![(PortType::Images, true)]);
            assert_eq!(kind.outputs(), IMAGE_OUTPUTS);
            assert_eq!(kind.data_kind(), None);
            assert!(NodeKind::ALL.contains(&kind));
        }
    }

    #[test]
    fn noise_reduction_gained_a_trailing_iso_adaptive_toggle() {
        let t = NodeKind::NoiseReduction.params();
        assert_eq!(t.len(), 4);
        assert_eq!(t[3].name, "iso_adaptive");
        assert_eq!((t[3].kind, t[3].min, t[3].max, t[3].default), (ParamKind::Toggle, 0.0, 1.0, 0.0));
        assert_eq!(t[0].name, "luma");
        assert_eq!(t[2].name, "radius");
    }

    #[test]
    fn graphs_saved_before_iso_adaptive_load_with_the_default() {
        let old = r#"{
            "format_version": 1,
            "nodes": {"1": {"id": 1, "kind": "NoiseReduction", "mode": "All",
                "all": {"luma": 0.4, "chroma": 0.6, "radius": 0.5}, "individual": {}, "pos": [0.0, 0.0]}},
            "edges": [], "next_id": 2
        }"#;
        let g = Graph::from_json(old).unwrap();
        let n = g.node(NodeId(1)).unwrap();
        assert!(!n.toggle("iso_adaptive", A));
        assert_eq!(n.param("luma", A), 0.4);
    }

    fn wheels() -> (Graph, NodeId) {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::ColorWheels, [0.0, 0.0]);
        (g, id)
    }

    fn close3(a: [f32; 3], b: [f32; 3]) {
        for c in 0..3 {
            assert!((a[c] - b[c]).abs() < 1e-6, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn cdl_params_default_is_exactly_identity() {
        let (g, id) = wheels();
        let p = cdl_params(g.node(id).unwrap(), A);
        assert_eq!(p, CdlParams::IDENTITY);
        assert!(p.is_identity());
        // Other kinds report identity too.
        let mut g2 = Graph::new();
        let e = g2.add_node(NodeKind::Exposure, [0.0, 0.0]);
        assert!(cdl_params(g2.node(e).unwrap(), A).is_identity());
    }

    #[test]
    fn cdl_params_master_and_channel_parameters() {
        let (mut g, id) = wheels();
        let n = g.node_mut(id).unwrap();
        n.set_param("gain", A, 1.0);
        let p = cdl_params(n, A);
        assert_eq!((p.slope, p.offset, p.power), ([2.0; 3], [0.0; 3], [1.0; 3]));
        assert!(!p.is_identity());
        n.set_param("gain", A, 0.0);

        n.set_param("lift", A, 0.5);
        close3(cdl_params(n, A).offset, [0.09; 3]);
        n.set_param("lift", A, 0.0);

        n.set_param("gamma", A, 1.0);
        close3(cdl_params(n, A).power, [0.5; 3]);
        n.set_param("gamma", A, -1.0);
        close3(cdl_params(n, A).power, [2.0; 3]);
        n.set_param("gamma", A, 0.0);

        // Each channel parameter touches only its own channel and quantity.
        n.set_param("gain_g", A, -1.0);
        let p = cdl_params(n, A);
        assert_eq!((p.slope, p.offset, p.power), ([1.0, 0.5, 1.0], [0.0; 3], [1.0; 3]));
        n.set_param("gain_g", A, 0.0);
        n.set_param("lift_b", A, -0.5);
        let p = cdl_params(n, A);
        assert_eq!((p.slope, p.power), ([1.0; 3], [1.0; 3]));
        close3(p.offset, [0.0, 0.0, -0.09]);
        n.set_param("lift_b", A, 0.0);
        n.set_param("gamma_r", A, 1.0);
        let p = cdl_params(n, A);
        close3(p.power, [0.5, 1.0, 1.0]);
        assert_eq!((p.slope, p.offset), ([1.0; 3], [0.0; 3]));
        n.set_param("gamma_r", A, 0.0);
        assert!(cdl_params(n, A).is_identity());
    }

    #[test]
    fn cdl_params_combine_master_and_channel() {
        let (mut g, id) = wheels();
        let n = g.node_mut(id).unwrap();
        n.set_param("gain", A, 1.0);
        n.set_param("gain_r", A, 0.5);
        n.set_param("gain_b", A, -1.0);
        n.set_param("lift", A, 0.2);
        n.set_param("lift_g", A, 0.1);
        n.set_param("gamma", A, 0.5);
        n.set_param("gamma_b", A, 0.5);
        let p = cdl_params(n, A);
        close3(p.slope, [2f32.powf(1.5), 2.0, 1.0]);
        close3(p.offset, [0.18 * 0.2, 0.18 * 0.3, 0.18 * 0.2]);
        close3(p.power, [2f32.powf(-0.5), 2f32.powf(-0.5), 0.5]);
    }

    #[test]
    fn cdl_params_follow_individual_mode() {
        let (mut g, id) = wheels();
        let n = g.node_mut(id).unwrap();
        n.set_param("gain", A, 1.0);
        n.mode = Mode::Individual;
        n.set_param("gain", B, -1.0);
        n.set_param("lift_r", B, 0.5);
        assert_eq!(cdl_params(n, A).slope, [2.0; 3], "A inherits the All values");
        let pb = cdl_params(n, B);
        assert_eq!(pb.slope, [0.5; 3]);
        close3(pb.offset, [0.09, 0.0, 0.0]);
        assert!(!cdl_params(n, A).is_identity());
    }

    #[test]
    fn iso_noise_scale_values() {
        assert_eq!(iso_noise_scale(Some(2500.0)), 1.0);
        assert!((iso_noise_scale(Some(10000.0)) - 2.0).abs() < 1e-6);
        assert_eq!(iso_noise_scale(Some(100.0)), 0.25, "sqrt(0.04) = 0.2 is clamped up");
        assert!((iso_noise_scale(Some(400.0)) - 0.4).abs() < 1e-6);
        assert_eq!(iso_noise_scale(Some(1_000_000.0)), 2.5);
        for bad in [None, Some(0.0), Some(-100.0), Some(f32::NAN), Some(f32::INFINITY), Some(f32::NEG_INFINITY)] {
            assert_eq!(iso_noise_scale(bad), 1.0, "{bad:?}");
        }
    }

    #[test]
    fn from_json_rejects_out_of_range_wave_7_values() {
        let mut g = Graph::new();
        let cw = g.add_node(NodeKind::ColorWheels, [0.0, 0.0]);
        let cl = g.add_node(NodeKind::Clarity, [0.0, 0.0]);
        let nr = g.add_node(NodeKind::NoiseReduction, [0.0, 0.0]);
        let load = |id: NodeId, param: &str, value: serde_json::Value| {
            let mut v: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
            v["nodes"][id.0.to_string()]["all"][param] = value;
            Graph::from_json(&v.to_string())
        };
        use serde_json::json;
        assert!(load(cw, "lift", json!(0.5)).is_ok());
        assert_rejected(load(cw, "lift", json!(0.51)), "outside -0.5..0.5");
        assert_rejected(load(cw, "lift_b", json!(-0.6)), "outside -0.5..0.5");
        assert_rejected(load(cw, "gamma", json!(1.1)), "outside -1..1");
        assert_rejected(load(cw, "gamma_g", json!(-1.1)), "outside -1..1");
        assert_rejected(load(cw, "gain", json!(2.5)), "outside -2..2");
        assert_rejected(load(cw, "gain_r", json!(-2.5)), "outside -2..2");
        assert_rejected(load(cl, "clarity", json!(1.5)), "outside -1..1");
        assert_rejected(load(cl, "texture", json!(-1.5)), "outside -1..1");
        assert!(load(nr, "iso_adaptive", json!(1)).is_ok());
        assert_rejected(load(nr, "iso_adaptive", json!(2)), "outside 0..1");
        assert_rejected(load(nr, "iso_adaptive", json!(0.5)), "not a whole option");
    }

    #[test]
    fn membership_of_wave_7_kinds_follows_the_image_input() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let filter = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        let cw = g.add_node(NodeKind::ColorWheels, [0.0, 0.0]);
        let cl = g.add_node(NodeKind::Clarity, [0.0, 0.0]);
        let loose = g.add_node(NodeKind::Clarity, [0.0, 0.0]);
        g.connect(src, filter, 0).unwrap();
        g.connect(filter, cw, 0).unwrap();
        g.connect(cw, cl, 0).unwrap();
        let f = g.node_mut(filter).unwrap();
        f.set_param("measure", A, 0.0);
        f.set_param("min", A, 0.0);
        f.set_param("max", A, 800.0);
        let iso = |i: ImageId| {
            Some(CaptureInfo { iso: Some(if i == A { 400.0 } else { 3200.0 }), ..Default::default() })
        };
        for n in [cw, cl] {
            assert!(g.contains(n, A, &iso));
            assert!(!g.contains(n, B, &iso));
        }
        assert!(!g.contains(loose, A, &iso), "unconnected required input");
    }
}

#[cfg(test)]
mod exposed_port_tests {
    use super::*;
    #[test]
    fn removed_node_kinds_are_rejected_by_name() {
        let mut g = Graph::new();
        g.add_node(NodeKind::Generator, [0.0; 2]);
        for kind in ["GradientMask", "RadialMask", "LuminanceMask", "HueMask", "MaskAdjust", "MaskCombine", "BrushMask", "DepthEstimate", "Normals"] {
            let text = g.to_json().replace("\"Generator\"", &format!("\"{kind}\""));
            let err = Graph::from_json(&text).unwrap_err();
            assert!(err.to_string().contains(&format!("unknown variant `{kind}`")), "{err}");
        }
    }

    #[test]
    fn fixed_ports_only_carry_images() {
        for kind in NodeKind::ALL {
            let count = match kind { NodeKind::Source => 0, NodeKind::Merge => 4, NodeKind::Layer => MAX_LAYERS, _ => 1 };
            assert_eq!(kind.inputs().len(), count, "{kind:?}");
            assert!(kind.inputs().iter().all(|p| p.ty == PortType::Images));
            assert!(kind.outputs().iter().all(|p| p.ty == PortType::Images));
        }
    }

    #[test]
    fn exposed_ports_are_stable_validated_and_auxiliary() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let other = g.add_node(NodeKind::Source, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let mask = g.add_node(NodeKind::Generator, [0.0; 2]);
        g.connect(src, exp, 0).unwrap();
        let p = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(other, exp, p).unwrap();
        assert_eq!(g.connect(mask, exp, p), Ok(()));
        let alias = g.expose_output(src, 0, "copy").unwrap();
        let ids = [ImageId(1), ImageId(2)];
        let catalog = Catalog { images: &ids, capture: &|_| None };
        assert_eq!(g.members(src, alias, &catalog), g.members(src, 0, &catalog));
        let sub = g.extract_subgraph(&[src, exp, other]);
        let sub = Subgraph::from_json(&sub.to_json()).unwrap();
        let mut copied = Graph::new();
        copied.insert_subgraph(&sub, [0.0; 2]);
        assert_eq!(Graph::from_json(&copied.to_json()), Ok(copied));
        assert_eq!(Graph::from_json(&g.to_json()), Ok(g.clone()));
        assert!(g.remove_exposed_input(exp, p));
        assert!(g.input_of(exp, p).is_none());
        let p2 = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        assert!(p2 > p);
        assert!(matches!(g.connect(other, exp, p), Err(GraphError::NoSuchPort { .. })));
        assert!(g.remove_exposed_output(src, alias));
        assert!(g.expose_output(src, 0, "again").unwrap() > alias);
        let mut json: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
        json["nodes"][exp.0.to_string()]["exposed_inputs"][p2 - g.node(exp).unwrap().kind.inputs().len()]["parameter"] = "not_a_parameter".into();
        assert!(Graph::from_json(&json.to_string()).is_err());
    }
}
