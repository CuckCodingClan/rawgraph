//! RawGraph core: data model and math shared by every other crate.
//!
//! UI-independent and deterministic: 32-bit float image buffers in
//! scene-referred linear ACEScg, colour math, capture metadata, the typed
//! node graph with All/Individual parameters, and the CPU reference pixel
//! operations. Execution lives in `rawgraph-engine`, display transform and
//! scopes in `rawgraph-scopes`.

pub mod capture;
pub mod color;
pub mod develop;
pub mod export;
pub mod graph;
pub mod generator;
pub mod image;
pub mod layer;
pub mod lens;
mod migrate;
pub mod ops;

#[cfg(test)]
mod migrate_tests;
#[cfg(test)]
mod wave8_tests;

pub use capture::{path_in_folder, CaptureInfo, Measure};
pub use color::{OutputSpace, Transfer, OUTPUT_SPACES};
pub use develop::{develop_settings, develop_tone, BaselineSource, DevelopSettings, DevelopTone};
pub use export::{
    export_spec, ExportCollision, ExportFileFormat, ExportSize, ExportSpec, ExportText, MetaSelection, EXPORT_COLLISIONS,
    EXPORT_FORMATS, EXPORT_SIZES,
};
pub use graph::{
    cdl_params, curve_set, default_curve, iso_noise_scale, neutral_curve, BrushStroke, Catalog, CaptureFn, CdlParams,
    CurveEntry, DataKind, Edge, Graph, GraphError, ImageId, Issue, Mode, NamedRange, Node, NodeData, NodeId, NodeKind,
    OutputSpec, ParamKind, ParamSpec, PortType, Subgraph, CURVE_CHANNELS, GRAPH_FORMAT_VERSION,
};
pub use generator::{ColorStop, OpacityStop, GradientRamp};
pub use image::{ImageBuf, MaskBuf};
pub use layer::{
    layer_settings, BlendMode, FitMode, LayerSettings, ResampleFilter, LAYER_BLEND_MODES, LAYER_FILTERS, LAYER_FITS,
    MAX_LAYERS,
};
pub use lens::{LensProfile, VignetteRadial, WarpRectilinear, LENS_MODES};
