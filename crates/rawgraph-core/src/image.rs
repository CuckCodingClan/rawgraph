//! 32-bit float pixel buffers.

/// RGBA image, interleaved, 32-bit float per channel.
///
/// Unless stated otherwise the pixels are scene-referred linear ACEScg (AP1
/// primaries, ACES white). Values are unbounded: highlights above 1.0 and
/// negative out-of-gamut values are legal and must survive every operation.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageBuf {
    width: u32,
    height: u32,
    data: Vec<f32>,
}

impl ImageBuf {
    pub const CHANNELS: usize = 4;

    /// Black, fully opaque image.
    pub fn new(width: u32, height: u32) -> Self {
        let mut data = vec![0.0; width as usize * height as usize * Self::CHANNELS];
        for alpha in data.iter_mut().skip(3).step_by(Self::CHANNELS) {
            *alpha = 1.0;
        }
        Self { width, height, data }
    }

    /// Wraps existing RGBA data. Returns `None` if the length does not match.
    pub fn from_rgba(width: u32, height: u32, data: Vec<f32>) -> Option<Self> {
        (data.len() == width as usize * height as usize * Self::CHANNELS)
            .then_some(Self { width, height, data })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn data(&self) -> &[f32] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [f32] {
        &mut self.data
    }

    pub fn pixel(&self, x: u32, y: u32) -> [f32; 4] {
        let i = (y as usize * self.width as usize + x as usize) * Self::CHANNELS;
        [self.data[i], self.data[i + 1], self.data[i + 2], self.data[i + 3]]
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, px: [f32; 4]) {
        let i = (y as usize * self.width as usize + x as usize) * Self::CHANNELS;
        self.data[i..i + 4].copy_from_slice(&px);
    }

    /// Box-filtered proxy whose longer edge is at most `max_dim` pixels.
    /// Uses an integer reduction factor, so a 1:1 request returns a clone.
    pub fn downscale_box(&self, max_dim: u32) -> ImageBuf {
        let long = self.width.max(self.height);
        let f = long.div_ceil(max_dim.max(1)).max(1);
        if f == 1 {
            return self.clone();
        }
        let (w, h) = ((self.width / f).max(1), (self.height / f).max(1));
        let mut out = ImageBuf::new(w, h);
        let src_w = self.width as usize;
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                let mut n = 0.0f32;
                for sy in (y * f)..((y * f + f).min(self.height)) {
                    let row = sy as usize * src_w;
                    for sx in (x * f)..((x * f + f).min(self.width)) {
                        let i = (row + sx as usize) * Self::CHANNELS;
                        for c in 0..4 {
                            acc[c] += self.data[i + c];
                        }
                        n += 1.0;
                    }
                }
                out.set_pixel(x, y, [acc[0] / n, acc[1] / n, acc[2] / n, acc[3] / n]);
            }
        }
        out
    }
}

/// Single-channel float map in 0..1, used to modulate effect parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct MaskBuf {
    pub width: u32,
    pub height: u32,
    pub data: Vec<f32>,
}

impl MaskBuf {
    pub fn new(width: u32, height: u32, value: f32) -> Self {
        Self { width, height, data: vec![value; width as usize * height as usize] }
    }

    pub fn get(&self, x: u32, y: u32) -> f32 {
        self.data[y as usize * self.width as usize + x as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_image_is_opaque_black() {
        let img = ImageBuf::new(2, 2);
        assert_eq!(img.pixel(1, 1), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn from_rgba_rejects_wrong_length() {
        assert!(ImageBuf::from_rgba(2, 2, vec![0.0; 15]).is_none());
        assert!(ImageBuf::from_rgba(2, 2, vec![0.0; 16]).is_some());
    }

    #[test]
    fn downscale_averages_blocks_and_keeps_highlights() {
        let mut img = ImageBuf::new(4, 2);
        for x in 0..4 {
            for y in 0..2 {
                img.set_pixel(x, y, [if x < 2 { 8.0 } else { 0.0 }, 1.0, 0.0, 1.0]);
            }
        }
        let small = img.downscale_box(2);
        assert_eq!((small.width(), small.height()), (2, 1));
        assert_eq!(small.pixel(0, 0), [8.0, 1.0, 0.0, 1.0]);
        assert_eq!(small.pixel(1, 0), [0.0, 1.0, 0.0, 1.0]);
    }
}
