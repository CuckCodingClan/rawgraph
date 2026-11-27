//! GPU pixel path prototype.
//!
//! Reproduces the CPU reference (`rawgraph_core::ops`, `rawgraph_scopes::DisplayTransform`)
//! with WGSL compute shaders on 32-bit float textures.
//!
//! * [`GpuContext`] owns the wgpu device/queue and the pipeline caches.
//! * [`GpuImage`] is an `Rgba32Float` texture; values above 1 and below 0 are kept.
//! * [`apply_point_ops`] runs a chain of pointwise operations in ONE pass; the shader is
//!   compiled once per distinct sequence of op kinds, parameters travel in a uniform buffer.
//! * [`DisplayTarget`] + [`render_into`] present the result in egui without a readback;
//!   [`display_samples`] reads a decimated float copy back for the scopes.
//! * [`display_rgba8`] / [`display_texture`] are the display transform (exposure, tone map,
//!   sRGB encoding), identical in meaning to the CPU `DisplayTransform`.

mod context;
mod display;
mod error;
mod image;
mod pass;
mod point_ops;
mod readback;

pub use context::GpuContext;
pub use display::{
    display_into, display_rgba8, display_samples, display_texture, read_display_batch, render_into, DisplayTarget, GpuToneMap,
};
pub use error::GpuError;
pub use image::GpuImage;
pub use point_ops::{apply_point_ops, apply_point_ops_into, PointOp};
