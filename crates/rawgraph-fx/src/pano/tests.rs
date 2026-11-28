//! Panorama tests on procedurally generated scenes (fixed seeds, no I/O).

use rawgraph_core::ImageBuf;
use rayon::prelude::*;

use super::blend::{blend_multiband, TileInput};
use super::align::Pair;
use super::bundle::{refine_rotations, BundleInput};
use super::compose::largest_rect;
use super::ransac::IDENT;
use super::ransac::{apply, fit_homography, inv, mul, ransac_homography, Mat3};
use super::*;

const NEVER: &(dyn Fn() -> bool + Sync) = &|| false;

// ---------------------------------------------------------------- scene

fn hash(ix: i32, iy: i32, seed: u32) -> f32 {
    let mut h = (ix as u32).wrapping_mul(0x27d4_eb2d) ^ (iy as u32).wrapping_mul(0x1656_67b1) ^ seed.wrapping_mul(0x9E37_79B9);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297a_2d39);
    h ^= h >> 15;
    (h & 0x00FF_FFFF) as f32 / 16_777_216.0
}

fn vnoise(x: f64, y: f64, cell: f64, seed: u32) -> f32 {
    let (gx, gy) = (x / cell, y / cell);
    let (ix, iy) = (gx.floor(), gy.floor());
    let (fx, fy) = ((gx - ix) as f32, (gy - iy) as f32);
    let (sx, sy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (ix, iy) = (ix as i32, iy as i32);
    let a = hash(ix, iy, seed) * (1.0 - sx) + hash(ix + 1, iy, seed) * sx;
    let b = hash(ix, iy + 1, seed) * (1.0 - sx) + hash(ix + 1, iy + 1, seed) * sx;
    a * (1.0 - sy) + b * sy
}

/// Textured scene: value noise (cells 96..12 px) plus soft-edged discs.
fn scene(x: f64, y: f64, seed: u32) -> [f32; 3] {
    let mut c = [0.0f32; 3];
    for (ch, cv) in c.iter_mut().enumerate() {
        let mut v = 0.0;
        let mut amp = 0.5;
        for (o, cell) in [96.0, 48.0, 24.0, 12.0].iter().enumerate() {
            v += amp * vnoise(x, y, *cell, seed + ch as u32 * 17 + o as u32 * 131);
            amp *= 0.8;
        }
        *cv = 0.08 + 0.75 * v / 1.476;
    }
    let (gx, gy) = ((x / 140.0).floor() as i32, (y / 140.0).floor() as i32);
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (cx, cy) = (gx + dx, gy + dy);
            let (px, py) = ((cx as f64 + hash(cx, cy, seed + 7) as f64) * 140.0, (cy as f64 + hash(cx, cy, seed + 8) as f64) * 140.0);
            let r = 10.0 + 22.0 * hash(cx, cy, seed + 9) as f64;
            let d = (x - px).hypot(y - py);
            let cov = ((r + 1.5 - d) / 3.0).clamp(0.0, 1.0) as f32;
            if cov > 0.0 {
                for (ch, cv) in c.iter_mut().enumerate() {
                    let col = 0.1 + 0.9 * hash(cx, cy, seed + 20 + ch as u32);
                    *cv = *cv * (1.0 - cov) + col * cov;
                }
            }
        }
    }
    c
}

fn render(w: u32, h: u32, gain: f32, f: impl Fn(f64, f64) -> [f32; 3] + Sync) -> ImageBuf {
    let mut data = vec![0.0f32; w as usize * h as usize * 4];
    data.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let c = f(x as f64 + 0.5 - w as f64 / 2.0, y as f64 + 0.5 - h as f64 / 2.0);
            row[x * 4] = c[0] * gain;
            row[x * 4 + 1] = c[1] * gain;
            row[x * 4 + 2] = c[2] * gain;
            row[x * 4 + 3] = 1.0;
        }
    });
    ImageBuf::from_rgba(w, h, data).unwrap()
}

/// Frame -> scene homography: rotation (deg), scale, translation, perspective terms.
fn frame_to_scene(rot_deg: f64, sc: f64, tx: f64, ty: f64, g: f64, h: f64) -> Mat3 {
    let (s, c) = rot_deg.to_radians().sin_cos();
    [[sc * c, -sc * s, tx], [sc * s, sc * c, ty], [g, h, 1.0]]
}

fn planar_frame(w: u32, h: u32, m: &Mat3, seed: u32, gain: f32) -> ImageBuf {
    let m = *m;
    render(w, h, gain, move |x, y| {
        let p = apply(&m, x, y).unwrap();
        scene(p[0], p[1], seed)
    })
}

fn cyl_frame(w: u32, h: u32, f: f64, yaw_deg: f64, seed: u32, gain: f32) -> ImageBuf {
    let (sy, cy) = yaw_deg.to_radians().sin_cos();
    render(w, h, gain, move |x, y| {
        let (rx, ry, rz) = (x / f, y / f, 1.0);
        let (wx, wz) = (rx * cy + rz * sy, -rx * sy + rz * cy);
        let theta = wx.atan2(wz);
        let hh = ry / wx.hypot(wz);
        scene(f * theta, f * hh, seed)
    })
}

fn psnr_vs(img: &ImageBuf, truth: impl Fn(f64, f64) -> [f32; 3] + Sync) -> f64 {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let d = img.data();
    let sse: f64 = (0..h)
        .into_par_iter()
        .map(|y| {
            let mut e = 0.0f64;
            for x in 0..w {
                let t = truth(x as f64 + 0.5, y as f64 + 0.5);
                for c in 0..3 {
                    e += ((d[(y * w + x) * 4 + c] - t[c]) as f64).powi(2);
                }
            }
            e
        })
        .sum();
    let mse = sse / (w * h * 3) as f64;
    10.0 * (1.0 / mse.max(1e-12)).log10()
}

fn refs(v: &[ImageBuf]) -> Vec<&ImageBuf> {
    v.iter().collect()
}

fn planar_set(gains: [f32; 3]) -> (Vec<ImageBuf>, Vec<Mat3>) {
    let ms = vec![
        frame_to_scene(0.8, 1.0, -380.0, 4.3, 0.0, 0.0),
        frame_to_scene(0.0, 1.0, 0.0, 0.0, 0.0, 0.0),
        frame_to_scene(-1.1, 1.015, 371.4, -9.2, 1.5e-5, -1.0e-5),
    ];
    let frames = ms.iter().zip(gains).map(|(m, g)| planar_frame(640, 420, m, 11, g)).collect();
    (frames, ms)
}

fn planar_params() -> PanoParams {
    PanoParams { projection: Projection::Planar, ..PanoParams::default() }
}

fn planar_psnr(pano: &Panorama, ms: &[Mat3], reference: usize, scale: f64) -> f64 {
    let m = ms[reference];
    let o = pano.origin;
    psnr_vs(&pano.image, move |x, y| {
        let p = apply(&m, o[0] as f64 + x, o[1] as f64 + y).unwrap();
        let c = scene(p[0], p[1], 11);
        [c[0] * scale as f32, c[1] * scale as f32, c[2] * scale as f32]
    })
}

// ---------------------------------------------------------------- maths

#[test]
fn homography_fit_is_exact_for_four_points() {
    let h: Mat3 = [[1.1, 0.05, 30.0], [-0.03, 0.95, -12.0], [1e-4, -2e-4, 1.0]];
    let src = [[-100.0, -80.0], [120.0, -70.0], [90.0, 110.0], [-60.0, 95.0]];
    let dst: Vec<[f64; 2]> = src.iter().map(|p| apply(&h, p[0], p[1]).unwrap()).collect();
    let fit = fit_homography(&src, &dst).unwrap();
    for p in [[0.0, 0.0], [50.0, -30.0], [-90.0, 70.0]] {
        let (a, b) = (apply(&h, p[0], p[1]).unwrap(), apply(&fit, p[0], p[1]).unwrap());
        assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
    }
}

#[test]
fn ransac_survives_forty_percent_outliers() {
    let h: Mat3 = [[0.98, -0.04, 55.0], [0.05, 1.02, -20.0], [2e-5, 1e-5, 1.0]];
    let mut s = 12345u64;
    let mut rnd = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (s >> 33) as f64 / (1u64 << 31) as f64
    };
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    for k in 0..200 {
        let p = [rnd() * 600.0 - 300.0, rnd() * 400.0 - 200.0];
        let q = apply(&h, p[0], p[1]).unwrap();
        src.push(p);
        if k % 5 < 2 {
            dst.push([rnd() * 600.0 - 300.0, rnd() * 400.0 - 200.0]);
        } else {
            dst.push([q[0] + (rnd() - 0.5), q[1] + (rnd() - 0.5)]);
        }
    }
    let (fit, inl) = ransac_homography(&src, &dst, 3.0, 2000, 5).unwrap();
    assert!(inl.len() >= 115, "inliers {}", inl.len());
    let (a, b) = (apply(&h, 10.0, 20.0).unwrap(), apply(&fit, 10.0, 20.0).unwrap());
    assert!((a[0] - b[0]).abs() < 1.0 && (a[1] - b[1]).abs() < 1.0);
}

#[test]
fn matrix_inverse_roundtrips() {
    let m: Mat3 = [[2.0, 0.3, 5.0], [-0.1, 1.5, 2.0], [1e-3, 2e-3, 1.0]];
    let p = mul(&m, &inv(&m).unwrap());
    for (i, row) in p.iter().enumerate() {
        for (j, v) in row.iter().enumerate() {
            assert!((v - if i == j { 1.0 } else { 0.0 }).abs() < 1e-9);
        }
    }
}

#[test]
fn largest_rect_finds_inscribed_rectangle() {
    let (w, h) = (20usize, 10usize);
    let mut m = vec![1u8; w * h];
    // Cut a corner and a notch.
    for y in 0..4 {
        for x in 0..6 {
            m[y * w + x] = 0;
        }
    }
    m[9 * w + 15] = 0;
    let r = largest_rect(&m, w, h, NEVER).unwrap().unwrap();
    for y in r.y..r.y + r.h {
        for x in r.x..r.x + r.w {
            assert_eq!(m[y as usize * w + x as usize], 1);
        }
    }
    assert_eq!(r.w * r.h, 14 * 9);
}

#[test]
fn multiband_blend_of_one_input_is_identity_and_two_inputs_cross_fade() {
    let (w, h) = (96usize, 80usize);
    let color: Vec<f32> = (0..w * h * 3).map(|i| ((i * 31 % 97) as f32) / 97.0).collect();
    let weight: Vec<f32> = (0..w * h).map(|i| 1.0 + (i % w) as f32).collect();
    let (rgb, cover) = blend_multiband(&[TileInput { color: color.clone(), weight }], w, h, 4);
    assert!(cover.iter().all(|c| *c));
    let err = rgb.iter().zip(&color).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    assert!(err < 1e-4, "identity error {err}");
    // Two flat inputs of different level: a smooth ramp between, nothing outside their range.
    let a = TileInput { color: vec![0.2; w * h * 3], weight: (0..w * h).map(|i| (w - (i % w)) as f32).collect() };
    let b = TileInput { color: vec![0.8; w * h * 3], weight: (0..w * h).map(|i| (i % w) as f32 + 1.0).collect() };
    let (rgb, _) = blend_multiband(&[a, b], w, h, 4);
    let row = |x: usize| rgb[(40 * w + x) * 3];
    assert!((row(2) - 0.2).abs() < 0.03 && (row(w - 3) - 0.8).abs() < 0.03);
    assert!(rgb.iter().all(|v| (0.19..=0.81).contains(v)));
    for x in 1..w {
        assert!(row(x) >= row(x - 1) - 0.01, "ramp not monotone at {x}");
    }
}

#[test]
fn size_limits_are_enforced() {
    assert!(check_size(1000.0, 800.0).is_ok());
    assert!(matches!(check_size(40_000.0, 100.0), Err(PanoError::TooLarge { .. })));
    assert!(matches!(check_size(25_000.0, 25_000.0), Err(PanoError::TooLarge { .. })));
    assert!(matches!(check_size(f64::NAN, 5.0), Err(PanoError::TooLarge { .. })));
}

// ---------------------------------------------------------------- stitching

#[test]
fn planar_stitch_matches_truth() {
    let (frames, ms) = planar_set([1.0; 3]);
    let pano = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    assert_eq!(pano.order, vec![0, 1, 2]);
    assert!(pano.image.width() > 800 && pano.image.height() > 300);
    assert_eq!(pano.valid_crop, Rect { x: 0, y: 0, w: pano.image.width(), h: pano.image.height() });
    assert!(pano.image.data().chunks_exact(4).all(|p| p[3] == 1.0));
    let psnr = planar_psnr(&pano, &ms, 1, 1.0);
    println!("planar PSNR {psnr:.2} dB, {}x{}", pano.image.width(), pano.image.height());
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

#[test]
fn planar_feather_and_uncropped_variants() {
    let (frames, ms) = planar_set([1.0; 3]);
    let p = PanoParams { blend: Blend::Feather, crop: false, ..planar_params() };
    let pano = stitch(&refs(&frames), &p, NEVER).unwrap();
    let img = &pano.image;
    // Uncropped: the canvas has empty corners, the valid rectangle lies fully inside the coverage.
    assert!(img.data().chunks_exact(4).any(|p| p[3] == 0.0));
    let r = pano.valid_crop;
    assert!(r.w > 400 && r.h > 250);
    for (x, y) in [(r.x, r.y), (r.x + r.w - 1, r.y), (r.x, r.y + r.h - 1), (r.x + r.w - 1, r.y + r.h - 1)] {
        assert_eq!(img.pixel(x, y)[3], 1.0);
    }
    // Inside the valid rectangle it still matches the truth.
    let m = ms[1];
    let o = pano.origin;
    let mut sse = 0.0f64;
    let mut n = 0.0f64;
    for y in r.y..r.y + r.h {
        for x in r.x..r.x + r.w {
            let p = apply(&m, o[0] as f64 + x as f64 + 0.5, o[1] as f64 + y as f64 + 0.5).unwrap();
            let t = scene(p[0], p[1], 11);
            let px = img.pixel(x, y);
            for c in 0..3 {
                sse += ((px[c] - t[c]) as f64).powi(2);
                n += 1.0;
            }
        }
    }
    let psnr = 10.0 * (1.0 / (sse / n)).log10();
    println!("planar feather PSNR {psnr:.2} dB");
    assert!(psnr >= 28.0, "PSNR {psnr}");
}

#[test]
fn shuffled_input_gives_panorama_order() {
    let (frames, _) = planar_set([1.0; 3]);
    let shuffled = [&frames[2], &frames[0], &frames[1]];
    let pano = stitch(&shuffled, &planar_params(), NEVER).unwrap();
    assert_eq!(pano.order, vec![1, 2, 0]);
}

#[test]
fn stitching_is_deterministic() {
    let (frames, _) = planar_set([1.0; 3]);
    let a = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    let b = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    assert_eq!(a.image, b.image);
    assert_eq!(a.origin, b.origin);
}

#[test]
fn exposure_differences_are_compensated() {
    // The middle (reference) frame keeps gain 1.
    let (frames, ms) = planar_set([0.55, 1.0, 1.6]);
    let on = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    let off = stitch(&refs(&frames), &PanoParams { exposure_comp: false, ..planar_params() }, NEVER).unwrap();
    let (p_on, p_off) = (planar_psnr(&on, &ms, 1, 1.0), planar_psnr(&off, &ms, 1, 1.0));
    println!("exposure: with {p_on:.2} dB, without {p_off:.2} dB");
    assert!(p_on >= 33.0, "with compensation {p_on}");
    assert!(p_on > p_off + 5.0, "compensation must help clearly: {p_on} vs {p_off}");
}

fn cyl_set(f: f64) -> Vec<ImageBuf> {
    [-22.0, 0.0, 22.0].iter().map(|&yaw| cyl_frame(640, 420, f, yaw, 23, 1.0)).collect()
}

fn cyl_psnr(pano: &Panorama, f_used: f64, f_true: f64) -> f64 {
    cyl_psnr_at(pano, f_used, f_true, 0.0)
}

/// PSNR against the cylinder truth; `theta0` is the world yaw of the projection centre.
fn cyl_psnr_at(pano: &Panorama, f_used: f64, f_true: f64, theta0: f64) -> f64 {
    let o = pano.origin;
    psnr_vs(&pano.image, move |x, y| {
        let theta = theta0 + (o[0] as f64 + x) / f_used;
        let hh = (o[1] as f64 + y) / f_used;
        scene(f_true * theta, f_true * hh, 23)
    })
}

#[test]
fn cylindrical_with_known_focal_matches_truth() {
    let frames = cyl_set(700.0);
    let p = PanoParams { focal_px: Some(700.0), focal_fixed: true, ..PanoParams::default() };
    let pano = stitch(&refs(&frames), &p, NEVER).unwrap();
    assert_eq!(pano.order, vec![0, 1, 2]);
    assert!((pano.focal_px - 700.0).abs() < 1e-3);
    // 44 deg of yaw plus one field of view of 49 deg.
    assert!(pano.image.width() > 900, "width {}", pano.image.width());
    let psnr = cyl_psnr(&pano, 700.0, 700.0);
    println!("cylindrical (known f) PSNR {psnr:.2} dB, {}x{}", pano.image.width(), pano.image.height());
    assert!(psnr >= 32.0, "PSNR {psnr}");
}

#[test]
fn cylindrical_estimates_the_focal_length() {
    let frames = cyl_set(700.0);
    let pano = stitch(&refs(&frames), &PanoParams::default(), NEVER).unwrap();
    let rel = pano.focal_px as f64 / 700.0;
    println!("estimated focal {:.1} px (truth 700), ratio {rel:.3}", pano.focal_px);
    assert!((rel - 1.0).abs() < 0.06, "focal ratio {rel}");
    assert!(pano.warnings.is_empty(), "{:?}", pano.warnings);
    let psnr = cyl_psnr(&pano, pano.focal_px as f64, 700.0);
    println!("cylindrical (estimated f) PSNR {psnr:.2} dB");
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

#[test]
fn spherical_projection_stitches_and_covers() {
    let frames = cyl_set(700.0);
    let p = PanoParams { projection: Projection::Spherical, focal_px: Some(700.0), ..PanoParams::default() };
    let pano = stitch(&refs(&frames), &p, NEVER).unwrap();
    assert_eq!(pano.order, vec![0, 1, 2]);
    assert!(pano.image.width() > 900 && pano.image.height() > 250);
    assert!(pano.image.data().chunks_exact(4).all(|p| p[3] == 1.0 && p[0].is_finite()));
    // Near the equator spherical and cylindrical agree: compare the centre rows with the truth.
    let o = pano.origin;
    let (w, h) = (pano.image.width(), pano.image.height());
    let (y0, y1) = (h / 2 - 20, h / 2 + 20);
    let mut sse = 0.0f64;
    let mut n = 0.0f64;
    for y in y0..y1 {
        for x in 0..w {
            let theta = (o[0] as f64 + x as f64 + 0.5) / 700.0;
            let phi = (o[1] as f64 + y as f64 + 0.5) / 700.0;
            let hh = phi.tan(); // cylinder height of the same ray
            let t = scene(700.0 * theta, 700.0 * hh, 23);
            let px = pano.image.pixel(x, y);
            for c in 0..3 {
                sse += ((px[c] - t[c]) as f64).powi(2);
                n += 1.0;
            }
        }
    }
    let psnr = 10.0 * (1.0 / (sse / n)).log10();
    println!("spherical centre-band PSNR {psnr:.2} dB");
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

#[test]
fn unconnected_frame_is_reported() {
    let (mut frames, _) = planar_set([1.0; 3]);
    // A frame of a completely different scene.
    let other = frame_to_scene(0.0, 1.0, 5000.0, 3000.0, 0.0, 0.0);
    frames.push(planar_frame(640, 420, &other, 999, 1.0));
    let err = stitch(&refs(&frames), &planar_params(), NEVER).unwrap_err();
    assert_eq!(err, PanoError::NotConnected { frames: vec![3] });
}

#[test]
fn input_errors() {
    let (frames, _) = planar_set([1.0; 3]);
    assert_eq!(stitch(&[&frames[0]], &planar_params(), NEVER).unwrap_err(), PanoError::TooFewFrames);
    let tiny = ImageBuf::new(8, 8);
    assert_eq!(stitch(&[&frames[0], &tiny], &planar_params(), NEVER).unwrap_err(), PanoError::InvalidFrame { frame: 1 });
    let many: Vec<&ImageBuf> = (0..26).map(|_| &frames[0]).collect();
    assert_eq!(stitch(&many, &planar_params(), NEVER).unwrap_err(), PanoError::TooManyFrames { count: 26 });
}

#[test]
fn cancel_ends_the_work() {
    let (frames, _) = planar_set([1.0; 3]);
    let always: &(dyn Fn() -> bool + Sync) = &|| true;
    assert_eq!(stitch(&refs(&frames), &planar_params(), always).unwrap_err(), PanoError::Cancelled);
    // Cancelling later (after some polls) also ends with Cancelled, never with a panic.
    let polls = std::sync::atomic::AtomicUsize::new(0);
    let later = || polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 10;
    assert_eq!(stitch(&refs(&frames), &planar_params(), &later).unwrap_err(), PanoError::Cancelled);
}

/// Timing of the 5 x 6000x4000 cylindrical case. Run with
/// `cargo test -p rawgraph-fx --release pano::tests::timing_five_6000x4000 -- --ignored --nocapture`.
#[test]
#[ignore]
fn timing_five_6000x4000() {
    let t0 = std::time::Instant::now();
    let f = 5000.0;
    let frames: Vec<ImageBuf> = [-44.0, -22.0, 0.0, 22.0, 44.0].iter().map(|&yaw| cyl_frame(6000, 4000, f, yaw, 31, 1.0)).collect();
    println!("frames rendered in {:.1} s", t0.elapsed().as_secs_f64());
    let t1 = std::time::Instant::now();
    let pano = stitch(&refs(&frames), &PanoParams::default(), NEVER).unwrap();
    println!(
        "stitch: {:.1} s, output {}x{}, focal {:.0} px (truth {f})",
        t1.elapsed().as_secs_f64(),
        pano.image.width(),
        pano.image.height(),
        pano.focal_px
    );
    assert!(pano.image.width() > 8000);
}

// ====================================================================== fix round 1

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
    fn sym(&mut self) -> f64 {
        self.next() * 2.0 - 1.0
    }
}

fn mat_vec(r: &Mat3, v: [f64; 3]) -> [f64; 3] {
    [
        r[0][0] * v[0] + r[0][1] * v[1] + r[0][2] * v[2],
        r[1][0] * v[0] + r[1][1] * v[1] + r[1][2] * v[2],
        r[2][0] * v[0] + r[2][1] * v[1] + r[2][2] * v[2],
    ]
}

fn rot_y(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, 0.0, s], [0.0, 1.0, 0.0], [-s, 0.0, c]]
}

fn rot_x(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[1.0, 0.0, 0.0], [0.0, c, -s], [0.0, s, c]]
}

fn rot_z(a: f64) -> Mat3 {
    let (s, c) = a.sin_cos();
    [[c, -s, 0.0], [s, c, 0.0], [0.0, 0.0, 1.0]]
}

/// Angle (radians) of the rotation `a^T b`.
fn rot_angle(a: &Mat3, b: &Mat3) -> f64 {
    let m = mul(&ransac::transpose(a), b);
    ((m[0][0] + m[1][1] + m[2][2] - 1.0) * 0.5).clamp(-1.0, 1.0).acos()
}

// ---------------------------------------------------------------- 1. full-resolution accuracy

/// Planar frames of `w x h` with shifts given as fractions of the width.
fn planar_set_sized(w: u32, h: u32) -> (Vec<ImageBuf>, Vec<Mat3>) {
    let fw = w as f64;
    let ms = vec![
        frame_to_scene(0.8, 1.0, -0.59 * fw, 0.011 * h as f64, 0.0, 0.0),
        frame_to_scene(0.0, 1.0, 0.0, 0.0, 0.0, 0.0),
        frame_to_scene(-1.1, 1.015, 0.58 * fw, -0.022 * h as f64, 0.2 / fw / 1000.0, -0.1 / fw / 1000.0),
    ];
    let frames = ms.iter().map(|m| planar_frame(w, h, m, 11, 1.0)).collect();
    (frames, ms)
}

fn planar_valid_psnr(pano: &Panorama, ms: &[Mat3]) -> f64 {
    let (m, o, r) = (ms[1], pano.origin, pano.valid_crop);
    let (mut sse, mut n) = (0.0f64, 0.0f64);
    for y in (r.y..r.y + r.h).step_by(2) {
        for x in (r.x..r.x + r.w).step_by(2) {
            let p = apply(&m, o[0] as f64 + x as f64 + 0.5, o[1] as f64 + y as f64 + 0.5).unwrap();
            let t = scene(p[0], p[1], 11);
            let px = pano.image.pixel(x, y);
            for c in 0..3 {
                sse += ((px[c] - t[c]) as f64).powi(2);
                n += 1.0;
            }
        }
    }
    10.0 * (1.0 / (sse / n)).log10()
}

#[test]
fn planar_fullres_1600_accuracy() {
    let (frames, ms) = planar_set_sized(1600, 1066);
    let pano = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    let psnr = planar_valid_psnr(&pano, &ms);
    println!("planar 1600 px (work factor 2): PSNR {psnr:.2} dB, {}x{}", pano.image.width(), pano.image.height());
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

#[test]
#[ignore]
fn planar_fullres_3200_accuracy() {
    let (frames, ms) = planar_set_sized(3200, 2132);
    let t = std::time::Instant::now();
    let pano = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    let psnr = planar_valid_psnr(&pano, &ms);
    println!("planar 3200 px (work factor 3): PSNR {psnr:.2} dB, {:.1} s", t.elapsed().as_secs_f64());
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

#[test]
fn odd_sizes_map_work_to_full_exactly() {
    // 1003 x 677 with work_px 520 -> factor 2 -> the work image covers 1002 x 676 only.
    let (frames, ms) = planar_set_sized(1003, 677);
    let p = PanoParams { work_px: 520, ..planar_params() };
    let pano = stitch(&refs(&frames), &p, NEVER).unwrap();
    let psnr = planar_valid_psnr(&pano, &ms);
    println!("odd sizes (factor 2): PSNR {psnr:.2} dB");
    assert!(psnr >= 30.0, "PSNR {psnr}");
    let g = align::Geo { f: 4.0, full_w: 1003.0, full_h: 677.0 };
    let c = g.centred(0.0, 0.0);
    assert!((c[0] - (2.0 - 501.5)).abs() < 1e-9 && (c[1] - (2.0 - 338.5)).abs() < 1e-9);
    let c = g.centred(249.0, 168.0);
    assert!((c[0] - (249.5 * 4.0 - 501.5)).abs() < 1e-9);
}

#[test]
fn keypoints_are_subpixel() {
    // The same scene shifted by half a pixel: matched keypoints move by about 0.5 px, which an
    // integer-position detector cannot show.
    let a = render(400, 300, 1.0, |x, y| scene(x, y, 5));
    let b = render(400, 300, 1.0, |x, y| scene(x + 0.5, y, 5));
    let ka = features::detect(&features::log_luma_plane(&a), 1500);
    let kb = features::detect(&features::log_luma_plane(&b), 1500);
    let m = matching::match_descriptors(&ka, &kb, 0.8);
    assert!(m.len() > 60, "matches {}", m.len());
    let mut dx: Vec<f32> = m.iter().map(|&(i, j)| ka[i].x - kb[j].x).collect();
    dx.sort_by(f32::total_cmp);
    let med = dx[dx.len() / 2];
    println!("median keypoint shift {med:.3} px over {} matches (truth 0.5)", dx.len());
    assert!((med - 0.5).abs() < 0.15, "median shift {med}");
    assert!(ka.iter().any(|k| (k.x - k.x.round()).abs() > 0.05), "positions are integers");
}

#[test]
fn homography_refinement_beats_dlt_on_noisy_points() {
    let h: Mat3 = [[1.05, 0.04, 40.0], [-0.02, 0.97, -15.0], [3e-5, -2e-5, 1.0]];
    let mut r = Lcg(77);
    let (mut src, mut dst) = (Vec::new(), Vec::new());
    for _ in 0..300 {
        let p = [r.sym() * 800.0, r.sym() * 500.0];
        let q = apply(&h, p[0], p[1]).unwrap();
        src.push(p);
        dst.push([q[0] + 0.4 * r.sym(), q[1] + 0.4 * r.sym()]);
    }
    // One gross outlier: the robust refinement must shrug it off.
    dst[0] = [dst[0][0] + 80.0, dst[0][1] - 60.0];
    let start = fit_homography(&src[1..], &dst[1..]).unwrap();
    let fit = ransac::refine_homography(&start, &src, &dst, 1.5);
    let err = |m: &Mat3| {
        let mut e = 0.0f64;
        for p in [[-700.0, -400.0], [700.0, -400.0], [-700.0, 400.0], [700.0, 400.0], [0.0, 0.0]] {
            let (a, b) = (apply(&h, p[0], p[1]).unwrap(), apply(m, p[0], p[1]).unwrap());
            e = e.max((a[0] - b[0]).hypot(a[1] - b[1]));
        }
        e
    };
    println!("homography corner error: dlt {:.3} px, refined {:.3} px", err(&start), err(&fit));
    assert!(err(&fit) < 0.35, "refined error {}", err(&fit));
}

// ---------------------------------------------------------------- 2. global rotation refinement

/// Synthetic pair set for cameras with the given true rotations (reference <- frame), `f` pixels,
/// frames of `w x h`; correspondences carry `noise` px of noise.
fn synth_pairs(rots: &[Mat3], f: f64, w: f64, h: f64, noise: f64, seed: u64) -> Vec<Pair> {
    let mut rng = Lcg(seed);
    let n = rots.len();
    let mut pairs = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            let mut corr = Vec::new();
            for _ in 0..4000 {
                if corr.len() >= 120 {
                    break;
                }
                let a = [rng.sym() * w * 0.5, rng.sym() * h * 0.5];
                let ray = mat_vec(&rots[i], [a[0] / f, a[1] / f, 1.0]);
                let v = mat_vec(&ransac::transpose(&rots[j]), ray);
                if v[2] < 0.2 {
                    continue;
                }
                let b = [f * v[0] / v[2], f * v[1] / v[2]];
                if b[0].abs() < w * 0.5 && b[1].abs() < h * 0.5 {
                    corr.push((a, [b[0] + noise * rng.sym(), b[1] + noise * rng.sym()]));
                }
            }
            if corr.len() >= 40 {
                pairs.push(Pair { i, j, h: IDENT, inliers: corr.len(), corr, thr_px: 3.0 });
            }
        }
    }
    pairs
}

#[test]
fn global_refinement_reduces_error_and_fixes_the_focal() {
    let (f_true, w, h) = (700.0, 640.0, 420.0);
    let truth: Vec<Mat3> = (0..7).map(|k| mul(&rot_y((k as f64 - 3.0).to_radians() * 12.0), &rot_x(0.01 * (k % 3) as f64))).collect();
    // The reference (frame 3) is the identity in the convention of the solver.
    let r3 = ransac::transpose(&truth[3]);
    let truth: Vec<Mat3> = truth.iter().map(|r| mul(&r3, r)).collect();
    let pairs = synth_pairs(&truth, f_true, w, h, 0.3, 9);
    assert!(pairs.len() >= 10, "pairs {}", pairs.len());
    let mut rng = Lcg(3);
    let init: Vec<Mat3> = truth
        .iter()
        .enumerate()
        .map(|(k, r)| if k == 3 { *r } else { mul(&bundle::rodrigues([0.012 * rng.sym(), 0.012 * rng.sym(), 0.008 * rng.sym()]), r) })
        .collect();
    let long = vec![w; 7];
    let members: Vec<usize> = (0..7).collect();
    let input = BundleInput { pairs: &pairs, long: &long, members: &members, reference: 3, rots: &init, f_rel: f_true * 1.06 / w, fix_focal: false, focal_tol: None };
    let r = refine_rotations(&input, NEVER).unwrap();
    println!("refinement: rms {:.2} -> {:.2} px, focal {:.1} (truth {f_true}, start {:.1})", r.rms_before, r.rms_after, r.f_rel * w, f_true * 1.06);
    assert!(r.rms_before > 4.0, "start should be clearly off, got {}", r.rms_before);
    assert!(r.rms_after < 0.6 && r.rms_after < r.rms_before / 5.0);
    assert!((r.f_rel * w / f_true - 1.0).abs() < 0.01, "focal {}", r.f_rel * w);
    for k in 0..7 {
        assert!(rot_angle(&r.rots[k], &truth[k]).to_degrees() < 0.12, "frame {k}");
    }
    // A fixed focal stays fixed.
    let fixed = BundleInput { fix_focal: true, f_rel: f_true / w, ..input };
    let rf = refine_rotations(&fixed, NEVER).unwrap();
    assert!((rf.f_rel - f_true / w).abs() < 1e-12 && rf.rms_after < 0.6);
}

#[test]
fn drift_over_ten_frames_is_bounded_by_the_global_refinement() {
    let (f_true, w, h) = (700.0, 640.0, 420.0);
    let yaw = |k: usize| (k as f64 - 4.0) * 11.0_f64.to_radians();
    let truth: Vec<Mat3> = (0..10).map(|k| rot_y(yaw(k) - yaw(4))).collect();
    let pairs = synth_pairs(&truth, f_true, w, h, 0.3, 21);
    // Initial rotations chained along the tree edges with a random error per edge: classic drift.
    let mut rng = Lcg(5);
    let mut init = vec![truth[4]; 10];
    for k in (5..10).chain((0..4).rev()) {
        let from = if k > 4 { k - 1 } else { k + 1 };
        let rel = mul(&ransac::transpose(&truth[from]), &truth[k]);
        let noisy = mul(&rel, &bundle::rodrigues([0.007 * rng.sym(), 0.007 * rng.sym(), 0.005 * rng.sym()]));
        init[k] = mul(&init[from], &noisy);
    }
    let drift_before = rot_angle(&init[9], &truth[9]).max(rot_angle(&init[0], &truth[0])).to_degrees();
    let long = vec![w; 10];
    let members: Vec<usize> = (0..10).collect();
    let input = BundleInput { pairs: &pairs, long: &long, members: &members, reference: 4, rots: &init, f_rel: f_true / w, fix_focal: false, focal_tol: None };
    let r = refine_rotations(&input, NEVER).unwrap();
    let drift_after = r.rots.iter().zip(&truth).map(|(a, b)| rot_angle(a, b)).fold(0.0, f64::max).to_degrees();
    println!("drift over 10 frames: {drift_before:.3} deg chained -> {drift_after:.3} deg refined; rms {:.2} -> {:.2} px", r.rms_before, r.rms_after);
    assert!(drift_after < 0.1, "drift {drift_after}");
    assert!(drift_after < drift_before / 2.0);
}

#[test]
fn ten_frame_cylinder_stitches_without_drift() {
    let f = 700.0;
    let yaws: Vec<f64> = (0..10).map(|k| (k as f64 - 4.5) * 11.0).collect();
    let frames: Vec<ImageBuf> = yaws.iter().map(|&y| cyl_frame(640, 420, f, y, 23, 1.0)).collect();
    let pano = stitch(&refs(&frames), &PanoParams::default(), NEVER).unwrap();
    let theta0 = yaws[pano.reference].to_radians();
    let psnr = cyl_psnr_at(&pano, pano.focal_px as f64, f, theta0);
    println!("10 frames: focal {:.1}, PSNR {psnr:.2} dB, {}x{}, warnings {:?}", pano.focal_px, pano.image.width(), pano.image.height(), pano.warnings);
    assert!((pano.focal_px as f64 / f - 1.0).abs() < 0.03);
    assert!(psnr >= 28.0, "PSNR {psnr}");
}

// ---------------------------------------------------------------- 3. horizon straightening

#[test]
fn wave_correction_finds_the_up_vector() {
    // Cameras panning around a tilted axis: the up vector is the tilted axis, pointing down.
    let q = mul(&rot_x(0.2), &rot_z(0.1));
    let rots: Vec<Mat3> = (0..6).map(|k| mul(&q, &rot_y((k as f64 - 2.5) * 0.3))).collect();
    let members: Vec<usize> = (0..6).collect();
    let w = bundle::wave_correction(&rots, &members).unwrap();
    let qy = mat_vec(&q, [0.0, 1.0, 0.0]);
    let dot = w[1][0] * qy[0] + w[1][1] * qy[1] + w[1][2] * qy[2];
    assert!(dot > 1.0 - 1e-9, "up vector off, dot {dot}");
    // A single column of frames (no spread of the x axes) gives no correction.
    let col: Vec<Mat3> = (0..4).map(|k| rot_x(k as f64 * 0.2)).collect();
    assert!(bundle::wave_correction(&col, &[0, 1, 2, 3]).is_none());
}

/// Frames of a world cylinder (with a bright horizontal line at height `h0`) seen by a camera panning
/// about the vertical axis, pitched up by `pitch_deg` and rolled by `roll_deg` (this frame).
fn tilted_frame(w: u32, h: u32, f: f64, yaw_deg: f64, roll_deg: f64, pitch_deg: f64, h0: f64) -> ImageBuf {
    let mount = mul(&rot_x(pitch_deg.to_radians()), &rot_z(roll_deg.to_radians()));
    let r = mul(&rot_y(yaw_deg.to_radians()), &mount);
    render(w, h, 1.0, move |x, y| {
        let v = mat_vec(&r, [x / f, y / f, 1.0]);
        let theta = v[0].atan2(v[2]);
        let hh = v[1] / v[0].hypot(v[2]);
        let mut c = scene(f * theta, f * hh, 41);
        let d = (f * (hh - h0)).abs();
        let cov = ((2.0 - d) / 1.5).clamp(0.0, 1.0) as f32;
        for v in c.iter_mut() {
            *v = *v * (1.0 - cov) + 3.0 * cov;
        }
        c
    })
}

#[test]
fn rolled_and_pitched_camera_gives_a_straight_horizon() {
    let f = 700.0;
    // The reference frame (yaw 0) is rolled by 5 deg and everything is pitched by 8 deg; the other
    // rolls cancel in the plane fit of the x axes, as a roll that is not the same in every frame.
    let rolls = [0.0, -2.66, 5.0, -2.66, 0.0];
    let frames: Vec<ImageBuf> =
        [-40.0, -20.0, 0.0, 20.0, 40.0].iter().zip(rolls).map(|(&yaw, roll)| tilted_frame(640, 420, f, yaw, roll, 8.0, 0.1)).collect();
    let p = PanoParams { focal_px: Some(700.0), crop: false, ..PanoParams::default() };
    let pano = stitch(&refs(&frames), &p, NEVER).unwrap();
    let (w, h) = (pano.image.width() as usize, pano.image.height() as usize);
    let d = pano.image.data();
    let luma_at = |x: usize, y: usize| d[(y * w + x) * 4 + 1];
    let covered = |x: usize, y: usize| d[(y * w + x) * 4 + 3] > 0.5;
    let mut ys = Vec::new();
    for x in (4..w - 4).step_by(4) {
        // Brightest row, then the centroid of the line around it.
        let (mut best, mut by) = (0.0f32, 0usize);
        for y in 6..h - 6 {
            if luma_at(x, y) > best && (y - 5..=y + 5).all(|yy| covered(x, yy)) {
                best = luma_at(x, y);
                by = y;
            }
        }
        if best > 1.5 {
            let (mut sw, mut sy) = (0.0f64, 0.0f64);
            for y in by - 4..=by + 4 {
                let v = (luma_at(x, y) - 0.9).max(0.0) as f64;
                sw += v;
                sy += v * y as f64;
            }
            ys.push(sy / sw);
        }
    }
    let n_cols = (w - 8).div_ceil(4);
    assert!(ys.len() * 10 >= n_cols * 7, "line found in {} of {n_cols} columns", ys.len());
    let (lo, hi) = ys.iter().fold((f64::MAX, f64::MIN), |a, &y| (a.0.min(y), a.1.max(y)));
    println!("horizon line: y from {lo:.2} to {hi:.2} over {} columns, {}x{}", ys.len(), w, h);
    assert!(hi - lo < 2.5, "horizon not straight: spread {}", hi - lo);
}

// ---------------------------------------------------------------- 4. bad pairs

#[test]
fn implausible_homographies_are_rejected() {
    use ransac::plausible_homography as ok;
    let (w, h) = (640.0, 420.0);
    let ok = |m: &Mat3, w: f64, h: f64| ok(m, w, h, &[]);
    let rot: Mat3 = [[0.99, -0.02, 30.0], [0.02, 0.99, 5.0], [1e-5, 0.0, 1.0]];
    assert!(ok(&rot, w, h));
    assert!(!ok(&[[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], w, h), "mirror");
    assert!(!ok(&[[3.0, 0.0, 0.0], [0.0, 3.0, 0.0], [0.0, 0.0, 1.0]], w, h), "3x scale");
    assert!(!ok(&[[0.3, 0.0, 0.0], [0.0, 0.3, 0.0], [0.0, 0.0, 1.0]], w, h), "0.3x scale");
    assert!(!ok(&[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.004, 0.0, 1.0]], w, h), "corner behind the plane");
    assert!(!ok(&[[1.0, 0.0, f64::NAN], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], w, h), "NaN");
}

#[test]
fn wide_angle_rotations_are_plausible_where_the_frames_overlap() {
    use ransac::plausible_homography;
    // 6000 x 4000 frames; pure rotation about the vertical axis, H = K R^T K^-1 (normalised).
    let (w, h) = (6000.0f64, 4000.0f64);
    for (mm, overlap) in [(24.0f64, 0.35f64), (20.0, 0.5), (16.0, 0.5), (14.0, 0.4)] {
        let f = mm / 36.0 * w;
        let hfov = 2.0 * (0.5 * w / f).atan();
        let theta = hfov * (1.0 - overlap);
        let (c, t) = (theta.cos(), theta.tan());
        let hm: Mat3 = [[1.0, 0.0, -f * t], [0.0, 1.0 / c, 0.0], [t / f, 0.0, 1.0]];
        // Inliers: the part of frame a that the rotated camera sees (angles beyond theta - hfov/2).
        let x_min = f * (theta - 0.5 * hfov).tan() + 50.0;
        let pts: Vec<[f64; 2]> = (0..40).map(|k| [x_min + (0.5 * w - 50.0 - x_min) * (k % 8) as f64 / 7.0, -1800.0 + 3600.0 * (k / 8) as f64 / 4.0]).collect();
        assert!(plausible_homography(&hm, w, h, &pts), "{mm} mm, {overlap} overlap");
        // The same map checked at the frame corners (the old rule) can fail for the widest lenses;
        // a mirrored map stays implausible with any points.
        let mirror: Mat3 = [[-1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
        assert!(!plausible_homography(&mirror, w, h, &pts));
    }
}

#[test]
fn nothing_connected_lists_all_frames() {
    let a = planar_frame(640, 420, &frame_to_scene(0.0, 1.0, 0.0, 0.0, 0.0, 0.0), 1, 1.0);
    let b = planar_frame(640, 420, &frame_to_scene(0.0, 1.0, 5000.0, 3000.0, 0.0, 0.0), 2, 1.0);
    let err = stitch(&[&a, &b], &planar_params(), NEVER).unwrap_err();
    assert_eq!(err, PanoError::NotConnected { frames: vec![0, 1] });
}

// ---------------------------------------------------------------- 5. memory

#[test]
fn byte_budget_and_failed_allocation_are_errors() {
    // 15000 x 15000 is 3.8 GB with the coverage mask, 16000 x 16000 exceeds the 4 GiB budget.
    assert!(check_size(15_000.0, 15_000.0).is_ok());
    assert!(matches!(check_size(16_000.0, 16_000.0), Err(PanoError::TooLarge { .. })));
    assert!(matches!(try_vec(usize::MAX / 2, 0.0f32, 7, 9), Err(PanoError::TooLarge { width: 7, height: 9 })));
    assert_eq!(try_vec(5, 1u8, 1, 1).unwrap(), vec![1u8; 5]);
}

// ---------------------------------------------------------------- 7. non-finite pixels

#[test]
fn non_finite_pixels_do_not_spread() {
    let (mut frames, ms) = planar_set([1.0; 3]);
    // Patches in the overlap of frames 0 and 1.
    {
        let mut data = frames[0].data().to_vec();
        for (px, py, v) in [(540usize, 200usize, f32::NAN), (500, 120, f32::INFINITY), (580, 300, f32::NEG_INFINITY)] {
            for y in py..py + 6 {
                for x in px..px + 6 {
                    let o = (y * 640 + x) * 4;
                    data[o] = v;
                    data[o + 1] = v;
                    data[o + 2] = v;
                }
            }
        }
        frames[0] = ImageBuf::from_rgba(640, 420, data).unwrap();
    }
    let pano = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    assert!(pano.image.data().iter().all(|v| v.is_finite()), "non-finite output");
    let psnr = planar_psnr(&pano, &ms, 1, 1.0);
    println!("with NaN/Inf patches: PSNR {psnr:.2} dB");
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

// ---------------------------------------------------------------- 8. exposure on scene-linear data

#[test]
fn exposure_compensation_works_above_one() {
    // Scene-linear: values up to ~4, nothing is clipped at 0.98.
    let (frames, ms) = planar_set([1.5, 3.0, 4.8]);
    let on = stitch(&refs(&frames), &planar_params(), NEVER).unwrap();
    let off = stitch(&refs(&frames), &PanoParams { exposure_comp: false, ..planar_params() }, NEVER).unwrap();
    let (p_on, p_off) = (planar_psnr(&on, &ms, 1, 3.0), planar_psnr(&off, &ms, 1, 3.0));
    println!("scene-linear exposure: with {p_on:.2} dB, without {p_off:.2} dB (peak 1.0 reference)");
    assert!(p_on >= 25.0, "with compensation {p_on}");
    assert!(p_on > p_off + 5.0);
}

#[test]
fn exposure_clip_parameter_keeps_a_clean_case_working() {
    let (frames, ms) = planar_set([0.6, 1.0, 1.5]);
    let on = stitch(&refs(&frames), &PanoParams { exposure_clip: Some(0.98), ..planar_params() }, NEVER).unwrap();
    let psnr = planar_psnr(&on, &ms, 1, 1.0);
    println!("exposure with clip level: {psnr:.2} dB");
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

// ---------------------------------------------------------------- 9. blend clamp

#[test]
fn multiband_output_is_never_negative() {
    let (w, h) = (128usize, 96usize);
    let hard: Vec<f32> = (0..w * h * 3).map(|i| if (i / 3 / 5 + i / 3 / w / 5) % 2 == 0 { 0.0 } else { 1.0 }).collect();
    let a = TileInput { color: hard, weight: (0..w * h).map(|i| (w - (i % w)) as f32).collect() };
    let b = TileInput { color: vec![0.0; w * h * 3], weight: (0..w * h).map(|i| (i % w) as f32 + 1.0).collect() };
    let (rgb, _) = blend_multiband(&[a, b], w, h, 5);
    assert!(rgb.iter().all(|v| *v >= 0.0 && v.is_finite()));
}

// ---------------------------------------------------------------- 10. cancellation

#[test]
fn cancellation_in_the_late_passes() {
    let m = vec![1u8; 512 * 512];
    assert_eq!(largest_rect(&m, 512, 512, &|| true).unwrap_err(), PanoError::Cancelled);
    assert!(largest_rect(&m, 512, 512, NEVER).unwrap().is_some());
    // Cancel at many different moments: always Cancelled or a result, never another error or a panic.
    let (frames, _) = planar_set([1.0; 3]);
    let total = std::sync::atomic::AtomicUsize::new(0);
    let count = || {
        total.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    };
    stitch(&refs(&frames), &planar_params(), &count).unwrap();
    let total = total.load(std::sync::atomic::Ordering::SeqCst);
    assert!(total > 30, "only {total} polls");
    for after in (0..total - 2).step_by((total / 40).max(1)) {
        let polls = std::sync::atomic::AtomicUsize::new(0);
        let c = || polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= after;
        assert_eq!(stitch(&refs(&frames), &planar_params(), &c).unwrap_err(), PanoError::Cancelled, "after {after} of {total} polls");
    }
}

// ---------------------------------------------------------------- 11. planar horizon

#[test]
fn planar_forward_rejects_points_behind_the_horizon() {
    let wp = Warper {
        kind: Projection::Planar,
        w: 640.0,
        h: 420.0,
        f_in: 700.0,
        f_out: 700.0,
        rot: IDENT,
        m: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.005, 0.0, 1.0]],
        minv: IDENT,
    };
    assert!(wp.forward(100.0, 0.0).is_some());
    assert!(wp.forward(-300.0, 0.0).is_none(), "w = 1 - 1.5 < 0");
}

// ---------------------------------------------------------------- 13. focal bound, warnings

#[test]
fn focal_search_flags_its_range_limit() {
    let long = vec![640.0, 640.0];
    let shift = |tx: f64| Pair { i: 0, j: 1, h: [[1.0, 0.0, tx], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], inliers: 100, corr: Vec::new(), thr_px: 3.0 };
    // A pure translation is a rotation about an infinite focal length: the search ends at its top.
    let (f, at_bound) = align::estimate_focal_rel(&[shift(120.0)], &long).unwrap();
    assert!(at_bound && f > 5.8, "f {f}, bound {at_bound}");
    // A genuine rotation pair is not at the bound.
    let fr: f64 = 1.1;
    let r = rot_y(0.3);
    let k = ransac::diag(fr * 640.0, fr * 640.0);
    let hh = mul(&mul(&k, &r), &ransac::inv(&k).unwrap());
    let pair = Pair { i: 0, j: 1, h: ransac::normalized(&hh), inliers: 100, corr: Vec::new(), thr_px: 3.0 };
    let (f, at_bound) = align::estimate_focal_rel(&[pair], &long).unwrap();
    assert!(!at_bound && (f - fr).abs() < 0.02, "f {f}, bound {at_bound}");
}

#[test]
fn planar_frames_in_a_cylinder_raise_a_warning() {
    let (frames, _) = planar_set([1.0; 3]);
    match stitch(&refs(&frames), &PanoParams::default(), NEVER) {
        Ok(p) => assert!(p.warnings.iter().any(|w| w.contains("limit")), "warnings {:?}", p.warnings),
        Err(e) => println!("cylinder from planar frames refused: {e}"),
    }
}

// ---------------------------------------------------------------- 14. high dynamic range

/// Cylinder frame of a scene with a very bright window with hard edges (merged HDR frames of a room
/// with a window: the window edge is far stronger than any texture).
fn cyl_frame_window(w: u32, h: u32, f: f64, yaw_deg: f64, seed: u32, window: f32) -> ImageBuf {
    let (sy, cy) = yaw_deg.to_radians().sin_cos();
    render(w, h, 1.0, move |x, y| {
        let (rx, ry, rz) = (x / f, y / f, 1.0);
        let (wx, wz) = (rx * cy + rz * sy, -rx * sy + rz * cy);
        let (sx, sy_) = (f * wx.atan2(wz), f * ry / wx.hypot(wz));
        let mut c = scene(sx, sy_, seed);
        if (sx - 150.0).abs() < 55.0 && (sy_ + 20.0).abs() < 60.0 {
            c = c.map(|v| v + window);
        }
        c
    })
}

#[test]
fn a_hard_edged_bright_window_does_not_hide_the_texture_corners() {
    // Window 40 times brighter than the scene: its edge gradient is two orders above the texture's.
    let frames: Vec<ImageBuf> = [-30.0, -10.0, 10.0, 30.0].iter().map(|&yaw| cyl_frame_window(480, 320, 520.0, yaw, 41, 12.0)).collect();
    let p = PanoParams { focal_px: Some(520.0), ..PanoParams::default() };
    let pano = stitch(&refs(&frames), &p, NEVER).expect("frames with a hard-edged window stitch");
    assert_eq!(pano.order, vec![0, 1, 2, 3]);
    assert!(pano.image.width() > 900, "width {}", pano.image.width());
    // Away from the window edges the picture is the scene (the edges themselves are one-pixel steps).
    let o = pano.origin;
    let theta0 = [-30.0f64, -10.0, 10.0, 30.0][pano.reference].to_radians();
    let (w, h) = (pano.image.width() as usize, pano.image.height() as usize);
    let (mut sse, mut n) = (0.0f64, 0u32);
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = (520.0 * (theta0 + (o[0] as f64 + x as f64 + 0.5) / 520.0), o[1] as f64 + y as f64 + 0.5);
            let near = ((sx - 150.0).abs() - 55.0).abs() < 14.0 && (sy + 20.0).abs() < 75.0 || ((sy + 20.0).abs() - 60.0).abs() < 14.0 && (sx - 150.0).abs() < 70.0;
            if near {
                continue;
            }
            let t = scene(sx, sy, 41);
            let inside = (sx - 150.0).abs() < 55.0 && (sy + 20.0).abs() < 60.0;
            for c in 0..3 {
                let want = t[c] + if inside { 12.0 } else { 0.0 };
                sse += ((pano.image.data()[(y * w + x) * 4 + c] - want) as f64).powi(2);
            }
            n += 3;
        }
    }
    let psnr = 10.0 * (1.0 / (sse / n as f64).max(1e-12)).log10();
    println!("window scene PSNR away from the edges {psnr:.2} dB");
    assert!(psnr >= 30.0, "PSNR {psnr}");
}

fn yaw_truth(n: usize, centre: usize, step_deg: f64) -> Vec<Mat3> {
    let yaw = |k: usize| (k as f64 - centre as f64) * step_deg.to_radians();
    (0..n).map(|k| rot_y(yaw(k))).collect()
}

#[test]
fn a_given_focal_length_is_refined_only_within_its_tolerance() {
    let (f_true, w, h) = (700.0, 640.0, 420.0);
    let truth = yaw_truth(7, 3, 11.0);
    let pairs = synth_pairs(&truth, f_true, w, h, 0.3, 4);
    let long = vec![w; 7];
    let members: Vec<usize> = (0..7).collect();
    let start = f_true * 1.25 / w;
    let base = BundleInput { pairs: &pairs, long: &long, members: &members, reference: 3, rots: &truth, f_rel: start, fix_focal: false, focal_tol: None };
    let free = refine_rotations(&base, NEVER).unwrap();
    assert!((free.f_rel * w / f_true - 1.0).abs() < 0.02, "free focal {}", free.f_rel * w);
    let tol = BundleInput { focal_tol: Some(0.1), ..base };
    let r = refine_rotations(&tol, NEVER).unwrap();
    let (lo, hi) = (start * 0.9, start * 1.1);
    assert!(r.f_rel >= lo - 1e-9 && r.f_rel <= hi + 1e-9, "focal {} outside {lo}..{hi}", r.f_rel);
    assert!(r.f_rel < start * 0.95, "the refinement moves towards the truth: {}", r.f_rel);
    // A start that is only 6 % off is corrected inside the range.
    let near = BundleInput { f_rel: f_true * 1.06 / w, focal_tol: Some(0.1), ..base };
    let rn = refine_rotations(&near, NEVER).unwrap();
    assert!((rn.f_rel * w / f_true - 1.0).abs() < 0.01, "focal {}", rn.f_rel * w);
}

#[test]
fn a_robust_improvement_is_kept_when_outliers_dominate_the_plain_rms() {
    let (f_true, w, h) = (700.0, 640.0, 420.0);
    let truth = yaw_truth(6, 2, 11.0);
    let mut pairs = synth_pairs(&truth, f_true, w, h, 0.3, 8);
    let mut rng = Lcg(77);
    // Gross outliers (about 80 px off): the Huber cost bounds them, the plain RMS does not.
    for p in pairs.iter_mut() {
        for k in 0..6 {
            let (a, b) = p.corr[k];
            p.corr[k] = (a, [b[0] + 80.0 * (0.5 + rng.sym().abs()), b[1] - 60.0 * (0.5 + rng.sym().abs())]);
        }
    }
    let mut rng = Lcg(5);
    let init: Vec<Mat3> = truth
        .iter()
        .enumerate()
        .map(|(k, r)| if k == 2 { *r } else { mul(&bundle::rodrigues([0.01 * rng.sym(), 0.01 * rng.sym(), 0.006 * rng.sym()]), r) })
        .collect();
    let long = vec![w; 6];
    let members: Vec<usize> = (0..6).collect();
    let input = BundleInput { pairs: &pairs, long: &long, members: &members, reference: 2, rots: &init, f_rel: f_true / w, fix_focal: true, focal_tol: None };
    let r = refine_rotations(&input, NEVER).expect("the robust cost falls, so the refinement is kept");
    let before = init.iter().zip(&truth).map(|(a, b)| rot_angle(a, b)).fold(0.0, f64::max);
    let after = r.rots.iter().zip(&truth).map(|(a, b)| rot_angle(a, b)).fold(0.0, f64::max);
    assert!(after < before, "{after} vs {before}");
    assert!(after.to_degrees() < 0.2, "after {} deg", after.to_degrees());
}

#[test]
fn stitch_refines_a_slightly_wrong_given_focal_unless_it_is_fixed() {
    let frames = cyl_set(700.0);
    let mut p = PanoParams { focal_px: Some(742.0), ..PanoParams::default() };
    let pano = stitch(&refs(&frames), &p, NEVER).unwrap();
    assert!((pano.focal_px as f64 / 700.0 - 1.0).abs() < 0.03, "refined focal {}", pano.focal_px);
    assert!((pano.focal_px - 742.0).abs() > 10.0);
    p.focal_fixed = true;
    let fixed = stitch(&refs(&frames), &p, NEVER).unwrap();
    assert!((fixed.focal_px - 742.0).abs() < 1e-3);
}

// ---------------------------------------------------------------- round 2 (lead)

#[test]
fn clustered_inliers_are_not_a_plausible_pair() {
    use ransac::plausible_homography;
    let (w, h) = (6000.0f64, 4000.0f64);
    let ident: Mat3 = [[1.0, 0.0, 200.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let spread: Vec<[f64; 2]> = (0..30).map(|k| [-2000.0 + 150.0 * k as f64, -1500.0 + 100.0 * k as f64]).collect();
    let cluster: Vec<[f64; 2]> = (0..30).map(|k| [100.0 + (k % 6) as f64 * 20.0, 50.0 + (k / 6) as f64 * 20.0]).collect();
    assert!(plausible_homography(&ident, w, h, &spread));
    assert!(!plausible_homography(&ident, w, h, &cluster), "30 inliers in a 100 x 80 px patch");
}

#[test]
fn the_output_budget_given_by_the_caller_limits_the_canvas_and_says_what_helps() {
    let (frames, _) = planar_set([1.0; 3]);
    let tight = PanoParams { max_output_bytes: Some(1 << 20), ..planar_params() };
    let err = stitch(&refs(&frames), &tight, NEVER).unwrap_err();
    assert!(matches!(err, PanoError::TooLarge { .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("GB") && text.contains("fewer frames"), "{text}");
    assert!(stitch(&refs(&frames), &planar_params(), NEVER).is_ok());
}

