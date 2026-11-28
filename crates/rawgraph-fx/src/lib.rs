//! RawGraph effects: CPU reference implementations of tone / colour effects, detail,
//! geometry and brush rasterization.
//!
//! All functions work on scene-referred linear ACEScg `ImageBuf`s and `MaskBuf`s, never clamp
//! the result to 0..1, preserve alpha (Pixel Sort moves it with its pixel) and give bit-identical output for neutral
//! parameters (so the engine may skip the work).

// Pixels are addressed as 4-float chunks of runtime-sized slices throughout.
#![allow(clippy::chunks_exact_to_as_chunks)]

mod brush;
mod color;
mod curve;
mod geometry;
mod layer;
mod lens;
mod local;
mod masks;
mod pixel_sort;
mod sharpen;
mod tone;
mod util;

pub use brush::stroke_mask;
pub use color::color_wheels;
pub use curve::{curve_eval, point_curve, CurveChannel};
pub use geometry::{
    lens_manual, transform, transform_input_to_output, transform_output_to_input, transform_size, TransformParams,
};
pub use layer::{blend_layer, place_layer};
pub use lens::{vignette_radial, warp_rectilinear};
pub use local::local_contrast;
pub use masks::resize_mask;
pub use pixel_sort::{pixel_sort, SortMetric};
pub use sharpen::sharpen;
pub use tone::{black_white, hue_saturation, tone_curve, vignette, vibrance, whites_blacks};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_brush_curve;
#[cfg(test)]
mod tests_wave7;
#[cfg(test)]
mod tests_wave8_tone;
#[cfg(test)]
mod tests_wave8_layer;
