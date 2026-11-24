use std::fmt;

/// Everything that can go wrong on the GPU path. Nothing here panics on a machine without a GPU.
#[derive(Debug, Clone, PartialEq)]
pub enum GpuError {
    /// No adapter (GPU) could be found.
    NoAdapter(String),
    /// The adapter refused to create a device.
    DeviceRequest(String),
    /// The image has a zero width or height.
    EmptyImage { width: u32, height: u32 },
    /// The image is larger than the device's `max_texture_dimension_2d`.
    TooLarge { width: u32, height: u32, max_dimension: u32 },
    /// A destination texture passed to an `*_into` function has the wrong size.
    SizeMismatch { expected: (u32, u32), actual: (u32, u32) },
    /// A GPU allocation (texture or buffer) failed.
    OutOfMemory(String),
    /// The call itself is invalid (for example source and destination are the same image).
    InvalidArgument(String),
    /// Reading data back from the GPU failed.
    Readback(String),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GpuError::NoAdapter(e) => write!(f, "no GPU adapter: {e}"),
            GpuError::DeviceRequest(e) => write!(f, "GPU device request failed: {e}"),
            GpuError::EmptyImage { width, height } => {
                write!(f, "image has zero size ({width}x{height})")
            }
            GpuError::TooLarge { width, height, max_dimension } => write!(
                f,
                "image {width}x{height} exceeds the GPU's maximum texture dimension of {max_dimension}"
            ),
            GpuError::SizeMismatch { expected, actual } => write!(
                f,
                "destination is {}x{} but the source is {}x{}",
                actual.0, actual.1, expected.0, expected.1
            ),
            GpuError::OutOfMemory(e) => write!(f, "GPU out of memory: {e}"),
            GpuError::InvalidArgument(e) => write!(f, "invalid GPU call: {e}"),
            GpuError::Readback(e) => write!(f, "GPU readback failed: {e}"),
        }
    }
}

impl std::error::Error for GpuError {}
