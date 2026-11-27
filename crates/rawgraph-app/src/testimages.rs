//! Built-in synthetic test images in linear ACEScg, with values above 1.0.

use rawgraph_core::{CaptureInfo, ImageBuf, ImageId};
use rayon::prelude::*;

use rawgraph_project::{Flag, ImageEntry};

pub const WIDTH: u32 = 1920;
pub const HEIGHT: u32 = 1280;

/// Display names of the demo images, in order.
pub const NAMES: [&str; 3] = ["Colour bars + ramp", "Gradient + highlight", "Scene (aperture missing)"];

pub fn entries() -> Vec<ImageEntry> {
    let cap = |iso: f32, shutter: f32, aperture: Option<f32>, flash: bool| CaptureInfo {
        make: Some("Synthetic".into()),
        model: Some("Test generator".into()),
        lens: None,
        iso: Some(iso),
        shutter_s: Some(shutter),
        aperture,
        flash_fired: Some(flash),
        // Synthetic pixels: no camera, so Develop leaves them alone.
        raw: Some(false),
        as_shot_kelvin: None,
        as_shot_tint: None,
        baseline_exposure: None,
        ..Default::default()
    };
    let captures = [
        cap(100.0, 1.0 / 125.0, Some(8.0), false),
        cap(800.0, 1.0 / 60.0, Some(2.8), true),
        cap(400.0, 1.0 / 250.0, None, false),
    ];
    captures
        .into_iter()
        .enumerate()
        .map(|(i, capture)| ImageEntry {
            id: ImageId(i as u64 + 1),
            path: std::path::PathBuf::from(format!("demo:{}", i + 1)),
            content_hash: String::new(),
            file_size: 0,
            modified_unix_ms: None,
            capture,
            rating: 0,
            flag: Flag::None,
            color_label: None,
            missing: false,
            uuid: format!("{:032x}", 0xde30_0000u64 + i as u64 + 1),
        })
        .collect()
}

/// Name shown for an entry: the file name, or the descriptive name of a demo image.
pub fn display_name(e: &ImageEntry) -> String {
    if e.path.to_string_lossy().starts_with(crate::provider::DEMO_PREFIX) {
        return NAMES.get((e.id.0 as usize).wrapping_sub(1)).map_or_else(|| e.file_name(), |n| (*n).to_owned());
    }
    e.file_name()
}

/// Generates test image `index` (0, 1 or 2) at 1920x1280.
pub fn generate(index: usize) -> ImageBuf {
    generate_sized(index, WIDTH, HEIGHT)
}

pub fn generate_sized(index: usize, w: u32, h: u32) -> ImageBuf {
    let mut data = vec![0.0f32; w as usize * h as usize * 4];
    data.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let rgb = match index {
                0 => bars(x as u32, y as u32, w, h),
                1 => gradient(x as u32, y as u32, w, h),
                _ => scene(x as u32, y as u32, w, h),
            };
            px[..3].copy_from_slice(&rgb);
            px[3] = 1.0;
        }
    });
    ImageBuf::from_rgba(w, h, data).expect("buffer length matches the dimensions")
}

fn bars(x: u32, y: u32, w: u32, h: u32) -> [f32; 3] {
    const BARS: [[f32; 3]; 8] = [
        [0.8, 0.8, 0.8],
        [0.8, 0.8, 0.0],
        [0.0, 0.8, 0.8],
        [0.0, 0.8, 0.0],
        [0.8, 0.0, 0.8],
        [0.8, 0.0, 0.0],
        [0.0, 0.0, 0.8],
        [0.0, 0.0, 0.0],
    ];
    let u = x as f32 / w as f32;
    let v = y as f32 / h as f32;
    if v < 0.6 {
        BARS[((u * 8.0) as usize).min(7)]
    } else if v < 0.8 {
        // Ten-step grey wedge: 18 % grey scaled by powers of two, up to +4 stops.
        let step = ((u * 10.0) as i32).min(9);
        let g = 0.18 * 2f32.powi(step - 5);
        [g, g, g]
    } else {
        // Smooth linear ramp 0..2 with a fine tick every 1/16.
        let g = 2.0 * u;
        let tick = if (u * 16.0).fract() < 0.004 && v < 0.85 { 0.0 } else { 1.0 };
        [g * tick, g * tick, g * tick]
    }
}

fn gradient(x: u32, y: u32, w: u32, h: u32) -> [f32; 3] {
    let u = x as f32 / w as f32;
    let v = y as f32 / h as f32;
    let r = 0.04 + 0.9 * u * (1.0 - 0.5 * v);
    let g = 0.04 + 0.75 * v * (0.4 + 0.6 * u);
    let b = 0.04 + 0.9 * (1.0 - u) * (0.4 + 0.6 * v);
    // Bright specular-like highlight reaching about 4.0.
    let (dx, dy) = ((u - 0.7) * 1.5, v - 0.35);
    let s = 3.2 * (-(dx * dx + dy * dy) / (2.0 * 0.06 * 0.06)).exp();
    [r + s * 1.0, g + s * 0.92, b + s * 0.78]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

fn scene(x: u32, y: u32, w: u32, h: u32) -> [f32; 3] {
    let u = x as f32 / w as f32;
    let v = y as f32 / h as f32;
    const HORIZON: f32 = 0.52;

    // Sky with a sun glow and a clipped sun disc.
    if v < HORIZON {
        let t = (v / HORIZON).powf(1.6);
        let mut c = lerp3([0.04, 0.10, 0.42], [0.55, 0.66, 0.85], t);
        let (dx, dy) = ((u - 0.78) * 1.5, v - 0.22);
        let d2 = dx * dx + dy * dy;
        let glow = 1.8 * (-d2 / 0.012).exp();
        let disc = if d2 < 0.0009 { 6.0 } else { 0.0 };
        c[0] += glow * 1.0 + disc;
        c[1] += glow * 0.8 + disc * 0.9;
        c[2] += glow * 0.45 + disc * 0.6;

        // A skyline of buildings with fine window grids.
        let bx = (u * 40.0) as u32;
        let height = 0.10 + 0.22 * hash01(bx);
        if v > HORIZON - height {
            let window = (x / 5 + y / 5).is_multiple_of(2) && x % 12 < 8 && y % 14 < 9;
            let lit = hash01(bx * 31 + x / 12 + (y / 14) * 97) > 0.55;
            return if window && lit { [0.9, 0.7, 0.35] } else if window { [0.04, 0.05, 0.07] } else { [0.07, 0.07, 0.09] };
        }
        return c;
    }

    // Ground: grass fading into a fine checkered pavement at the bottom.
    let gv = (v - HORIZON) / (1.0 - HORIZON);
    if gv > 0.62 {
        let cell = ((x / 6) + (y / 6)).is_multiple_of(2);
        let base = if cell { [0.55, 0.52, 0.48] } else { [0.10, 0.09, 0.09] };
        // Strong detail in the lower third; single-pixel lines add the finest detail.
        let line = x.is_multiple_of(48) || y.is_multiple_of(48);
        return if line { [1.4, 1.4, 1.4] } else { base };
    }
    let mut c = lerp3([0.05, 0.16, 0.04], [0.12, 0.26, 0.07], gv);

    // Coloured patches on the lawn.
    let patches: [([f32; 4], [f32; 3]); 5] = [
        ([0.06, 0.10, 0.62, 0.78], [0.65, 0.03, 0.03]),
        ([0.22, 0.36, 0.62, 0.78], [0.80, 0.62, 0.04]),
        ([0.38, 0.52, 0.62, 0.78], [0.03, 0.12, 0.70]),
        ([0.54, 0.68, 0.62, 0.78], [0.46, 0.25, 0.18]),
        ([0.70, 0.84, 0.62, 0.78], [1.8, 1.8, 1.7]),
    ];
    for ([x0, x1, y0, y1], col) in patches {
        if u >= x0 && u < x1 && v >= y0 && v < y1 {
            c = col;
        }
    }
    // Fine vertical blades.
    if x.is_multiple_of(7) {
        c[1] *= 1.25;
    }
    c
}

/// Deterministic pseudo-random value in 0..1.
fn hash01(n: u32) -> f32 {
    let mut h = n.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0xFFFF) as f32 / 65535.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_images_with_expected_sizes() {
        let e = entries();
        assert_eq!(e.len(), 3);
        for i in 0..3 {
            let img = generate(i);
            assert_eq!((img.width(), img.height()), (1920, 1280));
        }
    }

    #[test]
    fn values_above_one_and_no_nan() {
        for i in 0..3 {
            let img = generate(i);
            let mut max = 0.0f32;
            for v in img.data() {
                assert!(v.is_finite(), "image {i} contains a non-finite value");
                max = max.max(*v);
            }
            assert!(max > 1.0, "image {i} must contain scene-referred values above 1.0, max {max}");
        }
        let hi = generate(1).data().iter().cloned().fold(0.0f32, f32::max);
        assert!(hi > 3.5 && hi < 6.0, "highlight should reach about 4, got {hi}");
    }

    #[test]
    fn capture_metadata_covers_the_edge_cases() {
        let e = entries();
        assert!(e.iter().any(|x| x.capture.flash_fired == Some(true)));
        assert!(e.iter().any(|x| x.capture.aperture.is_none() && x.capture.ev100().is_none()));
        let evs: Vec<_> = e.iter().filter_map(|x| x.capture.ev100()).collect();
        assert_eq!(evs.len(), 2);
        assert!((evs[0] - evs[1]).abs() > 0.5, "images differ in capture EV");
    }
}
