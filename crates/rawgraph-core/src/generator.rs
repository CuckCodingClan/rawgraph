//! CPU primitives for synthetic, scene-linear ACEScg images.
//! Used by the input-sized Generator graph node.

use crate::image::ImageBuf;

/// Each midpoint is relative to the segment starting at this stop; the last is unused.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ColorStop { pub position: f32, pub color: [f32; 3], pub midpoint: f32 }
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OpacityStop { pub position: f32, pub opacity: f32, pub midpoint: f32 }
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GradientRamp { pub colors: Vec<ColorStop>, pub opacities: Vec<OpacityStop> }
impl Default for GradientRamp {
    fn default() -> Self { Self { colors: vec![ColorStop { position: 0.0, color: [0.0; 3], midpoint: 0.5 }, ColorStop { position: 1.0, color: [1.0; 3], midpoint: 0.5 }], opacities: vec![OpacityStop { position: 0.0, opacity: 1.0, midpoint: 0.5 }, OpacityStop { position: 1.0, opacity: 1.0, midpoint: 0.5 }] } }
}
impl GradientRamp {
    /// Stop positions must be strictly increasing in [0,1]; endpoints extend outward.
    pub fn validate(&self) -> Result<(), &'static str> {
        fn positions(v: &[(f32, f32)]) -> bool { (2..=256).contains(&v.len()) && v.iter().all(|&(p,m)| p.is_finite() && (0.0..=1.0).contains(&p) && m.is_finite() && (0.01..=0.99).contains(&m)) && v.windows(2).all(|w| w[0].0 < w[1].0) }
        if !positions(&self.colors.iter().map(|s| (s.position,s.midpoint)).collect::<Vec<_>>()) || !positions(&self.opacities.iter().map(|s| (s.position,s.midpoint)).collect::<Vec<_>>()) || self.colors.iter().any(|s| s.color.iter().any(|v| !v.is_finite())) || self.opacities.iter().any(|s| !s.opacity.is_finite() || !(0.0..=1.0).contains(&s.opacity)) { return Err("invalid gradient ramp"); } Ok(())
    }
    /// Sample a validated ramp in scene-linear color and straight alpha.
    pub fn sample(&self, t: f32, smooth: f32) -> [f32; 4] {
        fn interp<const N: usize>(t: f32, stops: impl Iterator<Item=(f32,[f32;N],f32)>, smooth: f32) -> [f32;N] {
            let mut it = stops; let mut a = it.next().expect("validated ramp");
            if t <= a.0 { return a.1; }
            for b in it { if t <= b.0 { let f = segment((t-a.0)/(b.0-a.0), a.2, smooth); return std::array::from_fn(|i| a.1[i]*(1.0-f)+b.1[i]*f); } a=b; } a.1
        }
        let c = interp(t, self.colors.iter().map(|s| (s.position,s.color,s.midpoint)), smooth);
        let a = interp(t, self.opacities.iter().map(|s| (s.position,[s.opacity],s.midpoint)), smooth)[0]; [c[0],c[1],c[2],a]
    }
}

/// Allocation limit: at most 64 MiB of RGBA f32 pixels.
pub const MAX_PIXELS: u64 = 4_194_304;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    Solid { color: [f32; 3] },
    Gradient { start: [f32; 3], end: [f32; 3], vertical: bool },
    StyledGradient { start: [f32; 3], end: [f32; 3], style: u32, angle: f32, scale: f32, reverse: bool, vertical: bool, ramp: Ramp },
    /// Eight vertical reference bars in scene-linear ACEScg, not a calibrated chart.
    Chart,
    /// Reproducible monochrome uniform noise in [0, 1].
    Noise { seed: u32 },
    /// A linear scalar ramp, with optional value inversion (not a spatial flip).
    DataRamp { vertical: bool, invert: bool },
    /// Alternating square cells, useful as a repeatable scalar pattern.
    Checker { cell_size: u32, invert: bool },
}

/// Three independently positioned colour and opacity stops, with per-segment midpoints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ramp {
    pub color_middle: Option<(f32, [f32; 3])>,
    pub color_midpoints: [f32; 2],
    pub alpha: [f32; 3],
    pub alpha_middle: Option<f32>,
    pub alpha_midpoints: [f32; 2],
    pub smooth: f32,
    pub dither: bool,
}

fn segment(t: f32, midpoint: f32, smooth: f32) -> f32 {
    let m = midpoint.clamp(0.01, 0.99);
    let v = if t <= m { 0.5 * t / m } else { 0.5 + 0.5 * (t - m) / (1.0 - m) };
    v + (v * v * (3.0 - 2.0 * v) - v) * smooth.clamp(0.0, 1.0)
}

fn ramp_value<const N: usize>(t: f32, start: [f32; N], end: [f32; N], middle: Option<(f32, [f32; N])>, midpoints: [f32; 2], smooth: f32) -> [f32; N] {
    let (a, b, local, midpoint) = match middle {
        Some((p, value)) if t < p && p > 0.0 => (start, value, t / p, midpoints[0]),
        Some((p, value)) if p < 1.0 => (value, end, (t - p).max(0.0) / (1.0 - p), midpoints[1]),
        Some((_, value)) if t >= 1.0 => (value, value, 0.0, midpoints[0]),
        _ => (start, end, t, midpoints[0]),
    };
    let f = segment(local.clamp(0.0, 1.0), midpoint, smooth);
    std::array::from_fn(|i| a[i] * (1.0 - f) + b[i] * f)
}

/// Reject invalid dimensions or non-finite colors before allocating.
pub fn generate(width: u32, height: u32, pattern: Pattern) -> Result<ImageBuf, &'static str> {
    generate_with_ramp(width, height, pattern, None)
}

/// Render with optional structured ramp.
pub fn generate_with_ramp(width: u32, height: u32, pattern: Pattern, structured: Option<&GradientRamp>) -> Result<ImageBuf, &'static str> {
    if let Some(r) = structured { r.validate()?; }
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err("generator dimensions exceed limits");
    }
    let valid = |c: [f32; 3]| c.iter().all(|v| v.is_finite());
    match pattern {
        Pattern::Solid { color } if !valid(color) => return Err("non-finite color"),
        Pattern::Gradient { start, end, .. } if !valid(start) || !valid(end) => return Err("non-finite color"),
        Pattern::StyledGradient { start, end, angle, scale, .. } if !valid(start) || !valid(end) || !angle.is_finite() || !scale.is_finite() || scale <= 0.0 => return Err("invalid gradient"),
        Pattern::Checker { cell_size: 0, .. } => return Err("checker cell size must be nonzero"),
        _ => {}
    }
    let bars = [[1.0, 1.0, 1.0], [1.0, 1.0, 0.0], [0.0, 1.0, 1.0], [0.0, 1.0, 0.0],
        [1.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0, 0.0]];
    let mut image = ImageBuf::new(width, height);
    for y in 0..height {
        for x in 0..width {
            let mut alpha = 1.0;
            let rgb = match pattern {
                Pattern::Solid { color } => color,
                Pattern::Gradient { start, end, vertical } => {
                    let (p, n) = if vertical { (y, height) } else { (x, width) };
                    let t = p as f32 / n.saturating_sub(1).max(1) as f32;
                    std::array::from_fn(|i| start[i] * (1.0 - t) + end[i] * t)
                }
                Pattern::StyledGradient { start, end, style, angle, scale, reverse, vertical, ramp } => {
                    // Normalized frame coordinates; the default remains the exact legacy ramp.
                    let u = x as f32 / width.saturating_sub(1).max(1) as f32;
                    let v = y as f32 / height.saturating_sub(1).max(1) as f32;
                    let radians = (angle + if vertical { 90.0 } else { 0.0 }).to_radians();
                    let (sin, cos) = radians.sin_cos();
                    let dx = u - 0.5;
                    let dy = v - 0.5;
                    let along = dx * cos + dy * sin;
                    let across = -dx * sin + dy * cos;
                    let raw = match style {
                        1 => (dx * dx + dy * dy).sqrt() * 2.0 / scale,
                        2 => ((dy.atan2(dx) - radians) / std::f32::consts::TAU).rem_euclid(1.0) / scale,
                        3 => along.abs() * 2.0 / scale,
                        4 => (along.abs() + across.abs()) * 2.0 / scale,
                        _ if angle == 0.0 && !vertical && scale == 1.0 => u,
                        _ if angle == 0.0 && vertical && scale == 1.0 => v,
                        _ => 0.5 + along / scale,
                    };
                    let t = raw.clamp(0.0, 1.0);
                    let t = if reverse { 1.0 - t } else { t };
                    let t = if ramp.dither {
                        let mut h = x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b);
                        h ^= h >> 16; h = h.wrapping_mul(0x7feb_352d); h ^= h >> 15;
                        (t + ((h & 65535) as f32 / 65535.0 - 0.5) / 255.0).clamp(0.0, 1.0)
                    } else { t };
                    if let Some(structured) = structured {
                        let rgba = structured.sample(t, ramp.smooth);
                        alpha = rgba[3];
                        [rgba[0], rgba[1], rgba[2]]
                    } else {
                    alpha = ramp_value(t, [ramp.alpha[0]], [ramp.alpha[2]], ramp.alpha_middle.map(|p| (p, [ramp.alpha[1]])), ramp.alpha_midpoints, ramp.smooth)[0];
                    ramp_value(t, start, end, ramp.color_middle, ramp.color_midpoints, ramp.smooth)
                    }
                }
                Pattern::DataRamp { vertical, invert } => {
                    let (p, n) = if vertical { (y, height) } else { (x, width) };
                    let t = p as f32 / n.saturating_sub(1).max(1) as f32;
                    [if invert { 1.0 - t } else { t }; 3]
                }
                Pattern::Checker { cell_size, invert } => {
                    let t = if (x / cell_size + y / cell_size) % 2 == 0 { 0.0 } else { 1.0 };
                    [if invert { 1.0 - t } else { t }; 3]
                }
                Pattern::Chart => bars[((u64::from(x) * 8) / u64::from(width)) as usize],
                Pattern::Noise { seed } => {
                    // Integer hash, independent of evaluation order and platform RNG.
                    let mut v = seed ^ x.wrapping_mul(0x9e37_79b9) ^ y.wrapping_mul(0x85eb_ca6b);
                    v ^= v >> 16;
                    v = v.wrapping_mul(0x7feb_352d);
                    v ^= v >> 15;
                    v = v.wrapping_mul(0x846c_a68b);
                    v ^= v >> 16;
                    [((v >> 8) as f32) / 16_777_215.0; 3]
                }
            };
            image.set_pixel(x, y, [rgb[0], rgb[1], rgb[2], alpha]);
        }
    }
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_ramp_validation_sampling_and_roundtrip() {
        let mut ramp = GradientRamp::default();
        ramp.colors.insert(1, ColorStop { position: 0.25, color: [1.0, 0.0, 0.0], midpoint: 0.25 });
        ramp.opacities.insert(1, OpacityStop { position: 0.5, opacity: 0.2, midpoint: 0.5 });
        assert!(ramp.validate().is_ok());
        assert_eq!(ramp.sample(0.25, 0.0), [1.0, 0.0, 0.0, 0.6]);
        let json = serde_json::to_string(&ramp).unwrap();
        assert_eq!(serde_json::from_str::<GradientRamp>(&json).unwrap(), ramp);
        ramp.colors[1].position = 0.0;
        assert!(ramp.validate().is_err());
    }
    #[test]
    fn bounds_and_nonfinite_colors_are_rejected() {
        assert!(generate(0, 1, Pattern::Chart).is_err());
        assert!(generate(u32::MAX, u32::MAX, Pattern::Chart).is_err());
        assert!(generate(1, 1, Pattern::Solid { color: [f32::NAN, 0.0, 0.0] }).is_err());
    }

    #[test]
    fn vertical_gradient_preserves_scene_linear_values_and_single_pixel_start() {
        let pattern = Pattern::Gradient { start: [-0.5, 0.0, 2.0], end: [1.5, 1.0, 4.0], vertical: true };
        let image = generate(2, 3, pattern).unwrap();
        assert_eq!(image.pixel(1, 0), [-0.5, 0.0, 2.0, 1.0]);
        assert_eq!(image.pixel(0, 1), [0.5, 0.5, 3.0, 1.0]);
        assert_eq!(image.pixel(1, 2), [1.5, 1.0, 4.0, 1.0]);
        assert_eq!(generate(1, 1, pattern).unwrap().pixel(0, 0), [-0.5, 0.0, 2.0, 1.0]);
    }

    #[test]
    fn data_ramp_is_linear_exact_and_inverts_values() {
        for vertical in [false, true] {
            let a = generate(5, 5, Pattern::DataRamp { vertical, invert: false }).unwrap();
            let b = generate(5, 5, Pattern::DataRamp { vertical, invert: true }).unwrap();
            for y in 0..5 {
                for x in 0..5 {
                    let expected = if vertical { y as f32 / 4.0 } else { x as f32 / 4.0 };
                    assert_eq!(a.pixel(x, y), [expected, expected, expected, 1.0]);
                    assert_eq!(b.pixel(x, y), [1.0 - expected, 1.0 - expected, 1.0 - expected, 1.0]);
                }
            }
        }
        assert_eq!(generate(1, 1, Pattern::DataRamp { vertical: false, invert: true }).unwrap().pixel(0, 0), [1.0; 4]);
    }

    #[test]
    fn checker_is_deterministic_and_validated() {
        assert!(generate(2, 2, Pattern::Checker { cell_size: 0, invert: false }).is_err());
        let a = generate(3, 2, Pattern::Checker { cell_size: 1, invert: false }).unwrap();
        let b = generate(3, 2, Pattern::Checker { cell_size: 1, invert: true }).unwrap();
        assert_eq!(a.pixel(0, 0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(a.pixel(1, 0), [1.0; 4]);
        assert_eq!(b.pixel(1, 0), [0.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn patterns_and_noise_are_reproducible() {
        let solid = generate(2, 1, Pattern::Solid { color: [0.2, 0.3, 0.4] }).unwrap();
        assert_eq!(solid.pixel(1, 0), [0.2, 0.3, 0.4, 1.0]);
        let gradient = generate(2, 1, Pattern::Gradient { start: [0.0; 3], end: [1.0; 3], vertical: false }).unwrap();
        assert_eq!(gradient.pixel(1, 0), [1.0; 4]);
        let chart = generate(8, 1, Pattern::Chart).unwrap();
        assert_eq!(chart.pixel(7, 0), [0.0, 0.0, 0.0, 1.0]);
        let a = generate(8, 8, Pattern::Noise { seed: 42 }).unwrap();
        assert_eq!(a, generate(8, 8, Pattern::Noise { seed: 42 }).unwrap());
        assert_ne!(a, generate(8, 8, Pattern::Noise { seed: 43 }).unwrap());
    }
}
