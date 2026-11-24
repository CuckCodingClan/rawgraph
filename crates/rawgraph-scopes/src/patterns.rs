//! Generated display-referred test images used to prove scope accuracy.

use crate::display::DisplayImage;

fn build(width: u32, height: u32, f: impl Fn(u32) -> [f32; 3]) -> DisplayImage {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for _ in 0..height {
        for x in 0..width {
            rgb.extend_from_slice(&f(x));
        }
    }
    DisplayImage { width, height, rgb }
}

/// Eight vertical bars: white, yellow, cyan, green, magenta, red, blue, black.
/// `amplitude` scales every non-black bar (0.75 gives 75 % bars).
pub fn color_bars(width: u32, height: u32, amplitude: f32) -> DisplayImage {
    const BARS: [[f32; 3]; 8] = [
        [1.0, 1.0, 1.0],
        [1.0, 1.0, 0.0],
        [0.0, 1.0, 1.0],
        [0.0, 1.0, 0.0],
        [1.0, 0.0, 1.0],
        [1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0],
        [0.0, 0.0, 0.0],
    ];
    build(width, height, |x| {
        let bar = (x as usize * 8 / width as usize).min(7);
        BARS[bar].map(|v| v * amplitude)
    })
}

/// Encoded value 0 at the left edge to 1 at the right edge, all channels equal.
pub fn horizontal_ramp(width: u32, height: u32) -> DisplayImage {
    let denom = (width.max(2) - 1) as f32;
    build(width, height, |x| [x as f32 / denom; 3])
}

/// Uniform colour.
pub fn flat(width: u32, height: u32, rgb: [f32; 3]) -> DisplayImage {
    build(width, height, |_| rgb)
}
