//! Lens profile data (DNG opcodes) that a Lens Correction node in Auto mode applies.

use serde::{Deserialize, Serialize};

/// Choices of the Lens Correction `mode` parameter.
pub const LENS_MODES: [&str; 2] = ["Manual", "Auto"];

/// DNG WarpRectilinear: one plane (all channels) or three (R, G, B); per plane
/// `kr0, kr1, kr2, kr3` (radial) and `kt0, kt1` (tangential). `center` is the
/// optical centre as a fraction of the image (0.5, 0.5 = middle).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarpRectilinear {
    pub planes: Vec<[f64; 6]>,
    pub center: [f64; 2],
}

/// DNG FixVignetteRadial: gain `1 + k0 r^2 + k1 r^4 + k2 r^6 + k3 r^8 + k4 r^10`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VignetteRadial {
    pub k: [f64; 5],
    pub center: [f64; 2],
}

/// What a file knows about its lens, in the upright image frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LensProfile {
    pub warp: Option<WarpRectilinear>,
    pub vignette: Option<VignetteRadial>,
}
