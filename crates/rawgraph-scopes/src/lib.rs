//! RawGraph scopes: provisional SDR display transform, scope computations
//! (histogram, waveform, RGB parade, vectorscope) and generated test patterns.
//!
//! No UI code lives here; the crate only produces numbers and buffers that the
//! UI layers draw.

pub mod display;
pub mod patterns;
pub mod scopes;

pub use display::*;
pub use scopes::*;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_spaces;
