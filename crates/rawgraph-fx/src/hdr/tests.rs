//! Synthetic tests of the HDR merge: a radiance scene rendered at several
//! exposures with clipping and noise.

use super::*;

const W: u32 = 200;
const H: u32 = 150;
/// Scale that keeps the darkest frame (-2 EV) of the 40x window just below 1.
const S0: f32 = 0.09;

fn hash(x: i64, y: i64, seed: u32) -> f32 {
    let mut v = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F)
        ^ (seed as u64).wrapping_mul(0x1656_67B1_9E37_79F9);
    v ^= v >> 29;
    v = v.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    v ^= v >> 32;
    ((v & 0xFFFF) as f32 / 32768.0) - 1.0
}

fn in_window(x: i64, y: i64) -> bool {
    (140..180).contains(&x) && (30..75).contains(&y)
}

/// Scene radiance of one pixel: gradient in both directions, texture and a 40x window.
fn radiance(x: i64, y: i64) -> f32 {
    if in_window(x, y) {
        return 40.0;
    }
    let (u, v) = (x as f32 / W as f32, y as f32 / H as f32);
    let tex = 0.5 * (0.35 * x as f32 + 0.5).sin() * (0.29 * y as f32 + 1.0).sin() + 0.3 * (0.11 * x as f32 + 0.7 * y as f32).sin();
    0.05 * (3.5 * u + 1.6 * v + tex).exp()
}

const COLOUR: [f32; 3] = [1.0, 0.8, 0.6];

struct Shot {
    ev: f32,
    shift: (i64, i64),
    noise: bool,
    seed: u32,
    /// Rectangle (x0, y0, x1, y1) in which the scene is 4x brighter (a moving object).
    block: Option<(i64, i64, i64, i64)>,
}

impl Shot {
    fn new(ev: f32, seed: u32) -> Shot {
        Shot { ev, shift: (0, 0), noise: true, seed, block: None }
    }
}

fn render(s: &Shot) -> ImageBuf {
    let mut img = ImageBuf::new(W, H);
    for y in 0..H as i64 {
        for x in 0..W as i64 {
            let (sx, sy) = (x + s.shift.0, y + s.shift.1);
            let mut l = radiance(sx, sy);
            if let Some((x0, y0, x1, y1)) = s.block {
                if (x0..x1).contains(&x) && (y0..y1).contains(&y) {
                    l *= 4.0;
                }
            }
            let mut px = [0.0f32; 4];
            for c in 0..3 {
                let mut v = S0 * s.ev.exp2() * l * COLOUR[c];
                if s.noise {
                    let n = 0.5 * (hash(x, y, s.seed * 7 + c as u32) + hash(x, y, s.seed * 7 + 3 + c as u32));
                    v = v * (1.0 + 0.012 * n) + 2e-4 * n;
                }
                px[c] = v.clamp(0.0, 1.0);
            }
            px[3] = 1.0;
            img.set_pixel(x as u32, y as u32, px);
        }
    }
    img
}

fn truth(x: i64, y: i64, c: usize) -> f32 {
    S0 * radiance(x, y) * COLOUR[c]
}

/// |mean(merged) / mean(truth) - 1| over the pixels selected by `sel`, channel `c`.
fn region_error(img: &ImageBuf, c: usize, sel: impl Fn(i64, i64) -> bool) -> f32 {
    let (mut a, mut b, mut n) = (0.0f64, 0.0f64, 0);
    for y in 0..H as i64 {
        for x in 0..W as i64 {
            if sel(x, y) {
                a += img.pixel(x as u32, y as u32)[c] as f64;
                b += truth(x, y, c) as f64;
                n += 1;
            }
        }
    }
    assert!(n > 50, "region too small");
    ((a / b) - 1.0).abs() as f32
}

fn near_window(x: i64, y: i64, d: i64) -> bool {
    (140 - d..180 + d).contains(&x) && (30 - d..75 + d).contains(&y)
}

fn interior(x: i64, y: i64) -> bool {
    (10..W as i64 - 10).contains(&x) && (10..H as i64 - 10).contains(&y)
}

type Region = (&'static str, Box<dyn Fn(i64, i64) -> bool>);

fn regions() -> Vec<Region> {
    vec![
        ("dark", Box::new(|x, y| interior(x, y) && x < 40 && !near_window(x, y, 3))),
        ("mid", Box::new(|x, y| interior(x, y) && (40..110).contains(&x) && !near_window(x, y, 3))),
        ("bright", Box::new(|x, y| interior(x, y) && x >= 110 && !near_window(x, y, 4))),
        ("window", Box::new(|x, y| near_window(x, y, -3) && interior(x, y))),
    ]
}

fn bracket(shifts: [(i64, i64); 3]) -> Vec<ImageBuf> {
    [(-2.0, 1), (0.0, 2), (2.0, 3)]
        .iter()
        .zip(shifts)
        .map(|(&(ev, seed), shift)| render(&Shot { shift, ..Shot::new(ev, seed) }))
        .collect()
}

fn refs(v: &[ImageBuf]) -> Vec<&ImageBuf> {
    v.iter().collect()
}

fn check_all_regions(img: &ImageBuf, tol: f32) {
    for (name, sel) in regions() {
        for c in 0..3 {
            let e = region_error(img, c, &sel);
            assert!(e < tol, "region {name} channel {c}: error {e}");
        }
    }
}

#[test]
fn bracket_merges_to_the_scene_within_three_percent_measured() {
    let frames = bracket([(0, 0); 3]);
    let out = merge_hdr(&refs(&frames), 1, &[None, None, None], &HdrParams::default()).unwrap();
    assert_eq!((out.width(), out.height()), (W, H));
    check_all_regions(&out, 0.03);
    // The window is far above 1: the float result keeps it.
    assert!(out.pixel(160, 50)[0] > 3.0);
    assert_eq!(out.pixel(5, 5)[3], 1.0);
}

#[test]
fn metadata_only_exposure_matches_too() {
    let frames = bracket([(0, 0); 3]);
    let p = HdrParams { measured: false, ..Default::default() };
    // EV from metadata: more light = larger value.
    let out = merge_hdr(&refs(&frames), 1, &[Some(-2.0), Some(0.0), Some(2.0)], &p).unwrap();
    check_all_regions(&out, 0.03);
    // Wrong metadata is visible in the result (so the test above really used it).
    let nog = HdrParams { deghost: 0.0, ..p };
    let bad = merge_hdr(&refs(&frames), 1, &[Some(-1.0), Some(0.0), Some(1.0)], &nog).unwrap();
    // (without deghosting: with it, frames that disagree with the reference are dropped, which hides the error)
    assert!(region_error(&bad, 0, |x, y| interior(x, y) && x < 40) > 0.05);
}

#[test]
fn metadata_is_used_when_the_overlap_is_too_small_to_measure() {
    // The first frame is black: nothing to measure against, metadata decides.
    let a = ImageBuf::new(8, 8);
    let mut b = ImageBuf::new(8, 8);
    for y in 0..8 {
        for x in 0..8 {
            let v = 0.1 + 0.04 * x as f32; // a ramp: a flat image would be one clip plateau
            b.set_pixel(x, y, [v, v, v, 1.0]);
        }
    }
    let p = HdrParams { align: false, deghost: 0.0, ..Default::default() };
    let out = merge_hdr(&[&a, &b], 1, &[Some(-1.0), Some(0.0)], &p).unwrap();
    assert!((out.pixel(3, 3)[0] - b.pixel(3, 3)[0]).abs() < 1e-5, "{}", out.pixel(3, 3)[0]);
    let err = merge_hdr(&[&a, &b], 1, &[None, None], &p).unwrap_err();
    assert_eq!(err, "HDR Merge: cannot tell the exposures apart (no metadata and too little overlap)");
}

#[test]
fn shifted_frames_are_aligned() {
    let frames = bracket([(3, -2), (0, 0), (-2, 1)]);
    let aligned = merge_hdr(&refs(&frames), 1, &[None; 3], &HdrParams::default()).unwrap();
    check_all_regions(&aligned, 0.04);
    let off = HdrParams { align: false, ..Default::default() };
    let unaligned = merge_hdr(&refs(&frames), 1, &[None; 3], &off).unwrap();
    let sel = |x: i64, y: i64| interior(x, y) && x >= 40 && !near_window(x, y, 6);
    assert!(region_error(&aligned, 0, sel) < 0.03);
    // Without alignment the same gradient is smeared across frames by 2-3 px (about 8 % per frame).
    let (e_off, e_on) = (region_error(&unaligned, 0, sel), region_error(&aligned, 0, sel));
    assert!(e_off > e_on, "{e_off} vs {e_on}");
}

#[test]
fn shifts_are_limited_by_max_shift() {
    let frames = bracket([(0, 0), (0, 0), (5, 0)]);
    let tight = HdrParams { max_shift_pct: 1.0, ..Default::default() }; // 2 px
    let a = merge_hdr(&refs(&frames), 1, &[None; 3], &tight).unwrap();
    let wide = HdrParams { max_shift_pct: 5.0, ..Default::default() }; // 10 px
    let b = merge_hdr(&refs(&frames), 1, &[None; 3], &wide).unwrap();
    let sel = |x: i64, y: i64| interior(x, y) && x >= 40 && !near_window(x, y, 7);
    assert!(region_error(&b, 0, sel) < region_error(&a, 0, sel));
}

#[test]
fn a_moving_block_ghosts_without_deghost_and_not_with_it() {
    let block = (30, 60, 50, 80);
    let mut frames = bracket([(0, 0); 3]);
    frames[2] = render(&Shot { block: Some(block), ..Shot::new(2.0, 3) });
    let inside = |x: i64, y: i64| (block.0 + 2..block.2 - 2).contains(&x) && (block.1 + 2..block.3 - 2).contains(&y);
    let off = HdrParams { deghost: 0.0, ..Default::default() };
    let ghosted = merge_hdr(&refs(&frames), 1, &[None; 3], &off).unwrap();
    assert!(region_error(&ghosted, 0, inside) > 0.5, "the object shows up without deghosting");
    let on = HdrParams { deghost: 0.5, ..Default::default() };
    let clean = merge_hdr(&refs(&frames), 1, &[None; 3], &on).unwrap();
    assert!(region_error(&clean, 0, inside) < 0.06, "{}", region_error(&clean, 0, inside));
    // Elsewhere deghosting changes nothing noticeable.
    check_all_regions(&clean, 0.03);
}

#[test]
fn deghost_keeps_other_frames_where_the_reference_is_clipped() {
    // Reference is the brightest frame here; the window is clipped in it, the darker frame supplies it.
    let frames = bracket([(0, 0); 3]);
    let out = merge_hdr(&[&frames[2], &frames[0]], 0, &[None, None], &HdrParams::default()).unwrap();
    // The result is at the reference exposure (+2 EV = 4x).
    let e = (out.pixel(160, 50)[0] / (S0 * 40.0 * 4.0) - 1.0).abs();
    assert!(e < 0.03, "{e}");
}

#[test]
fn identical_frames_give_the_frame() {
    let a = render(&Shot { noise: false, ..Shot::new(0.0, 1) });
    let out = merge_hdr(&[&a, &a, &a], 1, &[None; 3], &HdrParams::default()).unwrap();
    for y in (0..H).step_by(7) {
        for x in (0..W).step_by(5) {
            let (p, q) = (a.pixel(x, y), out.pixel(x, y));
            for c in 0..3 {
                assert!((p[c] - q[c]).abs() <= 1e-4 * p[c].max(1e-3), "{x},{y},{c}: {} vs {}", p[c], q[c]);
            }
        }
    }
}

#[test]
fn different_sizes_are_an_error() {
    let a = ImageBuf::new(10, 8);
    let b = ImageBuf::new(10, 9);
    assert_eq!(
        merge_hdr(&[&a, &b], 0, &[None, None], &HdrParams::default()).unwrap_err(),
        "HDR Merge: the images of a group differ in size (10x8 vs 10x9)"
    );
    assert!(merge_hdr(&[], 0, &[], &HdrParams::default()).is_err());
    assert!(merge_hdr(&[&a], 3, &[None], &HdrParams::default()).is_err());
}

#[test]
fn a_single_frame_passes_through_bit_exact() {
    let a = render(&Shot::new(0.0, 5));
    let out = merge_hdr(&[&a], 0, &[None], &HdrParams::default()).unwrap();
    assert_eq!(out, a);
}

#[test]
fn non_finite_pixels_do_not_leak() {
    let mut frames = bracket([(0, 0); 3]);
    frames[0].set_pixel(20, 20, [f32::NAN, f32::INFINITY, 0.5, 1.0]);
    frames[2].set_pixel(30, 30, [f32::NEG_INFINITY, 0.1, 0.1, 1.0]);
    let out = merge_hdr(&refs(&frames), 1, &[None; 3], &HdrParams::default()).unwrap();
    assert!(out.data().iter().all(|v| v.is_finite()));
}

#[test]
fn white_levels_are_the_clip_value_per_channel_when_enough_pixels_clip() {
    let frames = bracket([(0, 0); 3]);
    let (w, clipped) = white_levels(&frames[2]);
    assert!(w.iter().all(|v| (v - 1.0).abs() < 0.01), "{w:?}");
    assert_eq!(clipped, [true; 3]);
    // The darkest frame does not clip: every channel keeps its own level (colour 1.0 / 0.8 / 0.6),
    // spread by noise, so it is no plateau.
    let (dark, dark_clipped) = white_levels(&frames[0]);
    assert!(dark[0] > 0.7 && dark[0] < 1.0, "{dark:?}");
    assert!(dark[2] < dark[1] && dark[1] < dark[0], "{dark:?}");
    assert_eq!(dark_clipped, [false; 3]);
    assert_eq!(white_levels(&ImageBuf::new(4, 4)), ([0.0; 3], [false; 3]));
}

#[test]
fn bitmaps_find_a_known_shift() {
    let a = render(&Shot { noise: false, ..Shot::new(0.0, 1) });
    let b = render(&Shot { noise: false, shift: (4, -3), ..Shot::new(0.0, 1) });
    let (pa, pb) = (bitmap_pyramid(&a), bitmap_pyramid(&b));
    // b(x, y) = scene(x + 4, y - 3), so reading b at (x - 4, y + 3) lines up with a.
    assert_eq!(find_shift(&pa, &pb, 10.0), (-4, 3));
    assert_eq!(find_shift(&pa, &pa, 10.0), (0, 0));
    // Limit: the true shift is beyond reach.
    let (sx, sy) = find_shift(&pa, &pb, 2.0);
    assert!(sx.abs() <= 2 && sy.abs() <= 2);
}

#[test]
#[ignore = "timing: run in release with --ignored --nocapture"]
fn merge_24mp_five_frames_timing() {
    let (w, h) = (6000u32, 4000u32);
    let frames: Vec<ImageBuf> = (0..5)
        .map(|f| {
            let ev = f as f32 - 2.0;
            let shift = (f as i64 - 2, 2 - f as i64);
            let mut data = vec![0.0f32; w as usize * h as usize * 4];
            data.par_chunks_mut(w as usize * 4).enumerate().for_each(|(y, row)| {
                for x in 0..w as usize {
                    let (u, v) = ((x as i64 + shift.0) as f32 / w as f32, (y as i64 + shift.1) as f32 / h as f32);
                    let tex = 0.5 * (0.05 * x as f32).sin() * (0.043 * y as f32).sin();
                    let l = 0.05 * (3.5 * u + 1.6 * v + tex).exp() * if (0.7..0.9).contains(&u) && (0.2..0.5).contains(&v) { 40.0 } else { 1.0 };
                    let n = 1.0 + 0.01 * hash(x as i64, y as i64, f as u32);
                    for c in 0..3 {
                        row[x * 4 + c] = (S0 * ev.exp2() * l * COLOUR[c] * n).clamp(0.0, 1.0);
                    }
                    row[x * 4 + 3] = 1.0;
                }
            });
            ImageBuf::from_rgba(w, h, data).unwrap()
        })
        .collect();
    let r: Vec<&ImageBuf> = frames.iter().collect();
    let t = std::time::Instant::now();
    let out = merge_hdr(&r, 2, &[None; 5], &HdrParams::default()).unwrap();
    let ms = t.elapsed().as_millis();
    println!("HDR_MERGE_24MP_5FRAMES_MS={ms} threads={}", rayon::current_num_threads());
    assert_eq!((out.width(), out.height()), (w, h));
}

// ---- fix round 1 -------------------------------------------------------------------

/// The RAW loader's white balance with clip neutralisation (rawgraph-io `white_balance_pixel`).
fn loader_pixel(p: [f32; 3], wb: [f32; 3]) -> [f32; 3] {
    const SOFT: f32 = 0.90;
    let v = [p[0] * wb[0], p[1] * wb[1], p[2] * wb[2]];
    let sensor_max = p[0].max(p[1]).max(p[2]);
    if sensor_max <= SOFT {
        return v;
    }
    let t = ((sensor_max - SOFT) / (1.0 - SOFT)).min(1.0);
    let w = t * t * (3.0 - 2.0 * t);
    let mx = v[0].max(v[1]).max(v[2]);
    [v[0] + w * (mx - v[0]), v[1] + w * (mx - v[1]), v[2] + w * (mx - v[2])]
}

/// Sky ramp (scene colour (1, 0.8, 0.6) times a brightness rising from 0.05 to 3.05 over the width)
/// shot at gain `g` by a sensor whose channels clip at 1 and are white balanced by `wb`, with the
/// loader's neutralisation of clipped pixels.
fn sky_frame(g: f32, wb: [f32; 3], seed: u32) -> ImageBuf {
    let mut img = ImageBuf::new(W, H);
    for y in 0..H as i64 {
        for x in 0..W as i64 {
            let s = 0.05 + 3.0 * x as f32 / W as f32;
            let mut sensor = [0.0f32; 3];
            for c in 0..3 {
                let n = 0.5 * (hash(x, y, seed * 5 + c as u32) + hash(x, y, seed * 5 + 3 + c as u32));
                sensor[c] = (g * s * COLOUR[c] / wb[c] * (1.0 + 0.01 * n)).clamp(0.0, 1.0);
            }
            let o = loader_pixel(sensor, wb);
            img.set_pixel(x as u32, y as u32, [o[0], o[1], o[2], 1.0]);
        }
    }
    img
}

fn sky_frames() -> Vec<ImageBuf> {
    let wb = [2.0, 1.0, 1.5];
    [(0.25, 1), (1.0, 2), (4.0, 3)].iter().map(|&(g, s)| sky_frame(g, wb, s)).collect()
}

/// Worst error of mean(out) / mean(truth) over column groups of 10 and the three channels, for the
/// columns `from..to`; truth is the sky at exposure gain `gain`.
fn sky_error(out: &ImageBuf, gain: f32, from: i64, to: i64) -> (f64, i64, usize) {
    let mut worst = (0.0f64, 0, 0);
    for x0 in (from..to).step_by(10) {
        for (c, col) in COLOUR.iter().enumerate() {
            let (mut a, mut b) = (0.0f64, 0.0f64);
            for x in x0..(x0 + 10).min(to) {
                let s = 0.05 + 3.0 * x as f32 / W as f32;
                for y in 0..H as i64 {
                    a += out.pixel(x as u32, y as u32)[c] as f64;
                    b += (gain * s * col) as f64;
                }
            }
            let e = ((a / b) - 1.0).abs();
            if e > worst.0 {
                worst = (e, x0, c);
            }
        }
    }
    worst
}

#[test]
fn neutralised_clipped_sky_from_a_raw_loader_is_recovered() {
    let frames = sky_frames();
    // The frames really contain neutralised pixels (the brightest frame is grey at the cap).
    let p = frames[2].pixel(W - 2, 40);
    assert!(p[0] > 1.5 && (p[0] - p[1]).abs() < 1e-3 && (p[1] - p[2]).abs() < 1e-3, "{p:?}");
    for (reference, gain) in [(1usize, 1.0f32), (2, 4.0), (0, 0.25)] {
        let out = merge_hdr(&refs(&frames), reference, &[None; 3], &HdrParams::default()).unwrap();
        let (e, x0, c) = sky_error(&out, gain, 0, W as i64);
        assert!(e < 0.03, "reference {reference}: columns {x0}.., channel {c}: error {e}");
    }
}

#[test]
fn a_half_clipped_reference_does_not_beat_the_consistent_darker_frame_in_deghosting() {
    let frames = sky_frames();
    for deghost in [0.0, 0.5, 1.0] {
        let p = HdrParams { deghost, ..Default::default() };
        // The reference is the brightest frame, neutralised in the right part.
        let out = merge_hdr(&refs(&frames), 2, &[None; 3], &p).unwrap();
        let (e, x0, c) = sky_error(&out, 4.0, 100, W as i64);
        assert!(e < 0.03, "deghost {deghost}: columns {x0}.., channel {c}: error {e}");
    }
}

fn xorshift(s: &mut u64) -> f32 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    ((*s >> 40) as f32 / (1u64 << 24) as f32) - 0.5
}

/// A scene of flat (or softly graded) texture-less areas with a bright patch, at gains 1/4, 1, 4.
fn lowtexture_frames(sigma: f32, grad: f32) -> (Vec<ImageBuf>, impl Fn(usize, usize) -> f32) {
    let (w, h) = (300usize, 200usize);
    let truth = move |x: usize, y: usize| -> f32 {
        if (140..160).contains(&x) && (90..110).contains(&y) {
            3.0
        } else {
            0.2 * (1.0 + grad * x as f32 / w as f32)
        }
    };
    let mut seed = 0x1234_5678_9abc_def1u64;
    let mut mk = |gain: f32| {
        let mut d = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let v = ((truth(x, y) * (1.0 + sigma * 2.0 * xorshift(&mut seed))) * gain).min(1.0);
                d.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        ImageBuf::from_rgba(w as u32, h as u32, d).unwrap()
    };
    (vec![mk(0.25), mk(1.0), mk(4.0)], truth)
}

#[test]
fn low_texture_frames_are_not_shifted_and_the_merge_is_not_damaged() {
    for (label, sigma, grad) in [("flat+noise", 0.02f32, 0.0f32), ("soft gradient+noise", 0.02, 0.1), ("flat, no noise", 0.0, 0.0)] {
        let (frames, truth) = lowtexture_frames(sigma, grad);
        // Shift search itself: nothing to hold on to.
        let ref_pyr = bitmap_pyramid(&frames[1]);
        for f in [&frames[0], &frames[2]] {
            let (s, _) = find_shift_checked(&ref_pyr, &bitmap_pyramid(f), 9.0);
            assert_eq!(s, (0, 0), "{label}");
        }
        let p = HdrParams { align: true, deghost: 0.5, ..Default::default() };
        let mut notes = Vec::new();
        let m = merge_hdr_ex(&refs(&frames), 1, &[Some(-2.0), Some(0.0), Some(2.0)], &p, &|| false, &mut notes).unwrap();
        let mut bad = 0;
        for y in 85..115 {
            for x in 135..165 {
                let t = truth(x, y);
                if ((m.pixel(x as u32, y as u32)[0] - t) / t).abs() > 0.3 {
                    bad += 1;
                }
            }
        }
        assert_eq!(bad, 0, "{label}: wrong pixels around the patch (notes {notes:?})");
    }
}

#[test]
fn a_shift_ending_on_the_search_bound_is_rejected_and_noted() {
    let a = render(&Shot { noise: false, ..Shot::new(0.0, 1) });
    let b = render(&Shot { noise: false, shift: (4, -3), ..Shot::new(0.0, 1) });
    let (pa, pb) = (bitmap_pyramid(&a), bitmap_pyramid(&b));
    assert_eq!(find_shift_checked(&pa, &pb, 10.0), ((-4, 3), ShiftVerdict::Found));
    // Limit below the true shift: the search ends on its bound, which is not trusted.
    assert_eq!(find_shift_checked(&pa, &pb, 2.0), ((0, 0), ShiftVerdict::Unsure));
    // Through the merge the note is reported and the frame stays where it is.
    let frames = bracket([(0, 0), (0, 0), (9, 0)]);
    let p = HdrParams { max_shift_pct: 2.0, ..Default::default() }; // 4 px
    let mut notes = Vec::new();
    merge_hdr_ex(&refs(&frames), 1, &[None; 3], &p, &|| false, &mut notes).unwrap();
    assert!(notes.iter().any(|n| n.contains("alignment") && n.contains("unsure")), "{notes:?}");
}

#[test]
fn samples_outside_a_shifted_frame_carry_no_weight() {
    let img = render(&Shot::new(0.0, 1));
    let f = Frame { img: &img, inv_w: [1.0; 3], inv_t: [0.0; 3], k: 1.0, shift: (3, -2), darkest: false, may_clip: false };
    let (w, h) = (W as usize, H as usize);
    assert!(f.at(0, 2, w, h).is_some());
    assert!(f.at(w - 3, 2, w, h).is_none(), "reads x = width");
    assert!(f.at(5, 1, w, h).is_none(), "reads y = -1");
    assert!(f.at(w - 4, h - 1, w, h).is_some());
    // In a merge the border columns follow the scene (no replicated edge streak).
    let frames = bracket([(0, 0), (0, 0), (-3, 0)]);
    let out = merge_hdr(&refs(&frames), 1, &[None; 3], &HdrParams { align: true, deghost: 0.0, ..Default::default() }).unwrap();
    for x in W - 3..W {
        for y in 100..H - 5 {
            let t = truth(x as i64, y as i64, 0);
            let e = ((out.pixel(x, y)[0] - t) / t).abs();
            assert!(e < 0.08, "border pixel {x},{y}: {e}");
        }
    }
}

#[test]
fn a_hot_pixel_is_no_ghost() {
    let (w, h) = (40usize, 30usize);
    let ramp = |x: usize, gain: f32| gain * 0.1 * (1.0 + x as f32 / w as f32);
    let make = |gain: f32, spike: bool| {
        let mut d = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let v = if spike && (x, y) == (20, 15) { ramp(x, gain) * 4.0 } else { ramp(x, gain) };
                d.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        ImageBuf::from_rgba(w as u32, h as u32, d).unwrap()
    };
    let (a, b) = (make(1.0, false), make(2.0, true));
    let mk = |img, k| Frame { img, inv_w: [1.0 / 0.4; 3], inv_t: [0.0; 3], k, shift: (0, 0), darkest: false, may_clip: false };
    let (fa, fb) = (mk(&a, 1.0), mk(&b, 0.5));
    let map = ghost_map(&fa, None, &fb, None, 0.5, w, h);
    assert_eq!(map[15 * w + 20], 0, "a single pixel does not drop the frame");
    // A 5x5 block of the same strength does.
    let mut big = b.clone();
    for y in 10..15 {
        for x in 15..20 {
            let v = ramp(x, 2.0) * 4.0;
            big.set_pixel(x as u32, y as u32, [v, v, v, 1.0]);
        }
    }
    let fbig = mk(&big, 0.5);
    let map = ghost_map(&fa, None, &fbig, None, 0.5, w, h);
    assert_eq!(map[12 * w + 17], 255);
}

#[test]
fn far_apart_exposures_are_chained_through_their_neighbours() {
    // Scene brightness 3e-4 .. 0.9 (log ramp); frames at +0, +3.5, +7 EV: the first and the last
    // share no unclipped pixels, so the ratio between them can only come through the middle one.
    let (w, h) = (240u32, 40u32);
    let scene = |x: u32| 3e-4f32 * (3000f32).powf(x as f32 / w as f32);
    let make = |ev: f32, seed: u32| {
        let mut d = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let n = 1.0 + 0.01 * hash(x as i64, y as i64, seed);
                let v = (ev.exp2() * scene(x) * n).clamp(0.0, 1.0);
                d.extend_from_slice(&[v, v, v, 1.0]);
            }
        }
        ImageBuf::from_rgba(w, h, d).unwrap()
    };
    let frames = [make(0.0, 1), make(3.5, 2), make(7.0, 3)];
    let p = HdrParams { align: false, deghost: 0.0, ..Default::default() };
    let out = merge_hdr(&refs(&frames), 0, &[None; 3], &p).unwrap();
    for x0 in (30..w).step_by(30) {
        let (mut a, mut b) = (0.0f64, 0.0f64);
        for x in x0..(x0 + 10).min(w) {
            for y in 0..h {
                a += out.pixel(x, y)[0] as f64;
                b += scene(x) as f64;
            }
        }
        assert!(((a / b) - 1.0).abs() < 0.03, "columns {x0}..: {}", a / b);
    }
    // The metadata-free overlap between 0 and +7 EV is really too small for a direct measure.
    let set: Vec<Frame> = frames
        .iter()
        .map(|img| {
            let (wl, plateau) = white_levels(img);
            Frame {
                img,
                inv_w: std::array::from_fn(|c| 1.0 / (0.95 * wl[c])),
                inv_t: std::array::from_fn(|c| if plateau[c] { 1.0 / (0.95 * wl[c]) } else { 0.0 }),
                k: 1.0,
                shift: (0, 0),
                darkest: false,
                may_clip: true,
            }
        })
        .collect();
    assert!(measure_ratio(&set[0], &set[2], w as usize, h as usize).is_none());
    assert!(measure_ratio(&set[0], &set[1], w as usize, h as usize).is_some());
}

#[test]
fn median_exposure_expresses_the_result_at_the_middle_frame() {
    let frames = bracket([(0, 0); 3]);
    // The planned reference is the darkest frame; with the option the median frame (0 EV) sets the exposure.
    let plain = merge_hdr(&refs(&frames), 0, &[None; 3], &HdrParams::default()).unwrap();
    let mid = merge_hdr(&refs(&frames), 0, &[None; 3], &HdrParams { median_exposure: true, ..Default::default() }).unwrap();
    let (a, b) = (plain.pixel(20, 100)[0], mid.pixel(20, 100)[0]);
    assert!((b / a - 4.0).abs() < 0.1, "darkest (-2 EV) to middle (0 EV) is 4x: {a} -> {b}");
    // The planned reference being the median already changes nothing.
    let same = merge_hdr(&refs(&frames), 1, &[None; 3], &HdrParams { median_exposure: true, ..Default::default() }).unwrap();
    let base = merge_hdr(&refs(&frames), 1, &[None; 3], &HdrParams::default()).unwrap();
    assert_eq!(same, base);
}

#[test]
fn merge_polls_the_cancel_callback_between_its_stages() {
    use std::cell::Cell;
    let frames = bracket([(0, 0); 3]);
    let polls = Cell::new(0usize);
    let count = || {
        polls.set(polls.get() + 1);
        false
    };
    merge_hdr_ex(&refs(&frames), 1, &[None; 3], &HdrParams::default(), &count, &mut Vec::new()).unwrap();
    assert!(polls.get() >= 8, "polled {} times", polls.get());
    for stop_at in [1usize, 4, 7] {
        let polls = Cell::new(0usize);
        let stop = || {
            polls.set(polls.get() + 1);
            polls.get() >= stop_at
        };
        let r = merge_hdr_ex(&refs(&frames), 1, &[None; 3], &HdrParams::default(), &stop, &mut Vec::new());
        assert_eq!(r.unwrap_err(), HdrError::Cancelled, "stop at poll {stop_at}");
    }
}

// ---- round 2 (lead) ---------------------------------------------------------------

/// Landscape: dark ground (left 40 %) under a bright sky that the +2 frame clips and the loader
/// neutralises; the measured exposure must not be pulled by the grey sky blocks.
fn landscape_frame(g: f32, seed: u32) -> ImageBuf {
    let wb = [2.0, 1.0, 1.5];
    let mut img = ImageBuf::new(W, H);
    for y in 0..H as i64 {
        for x in 0..W as i64 {
            let s = if (x as u32) < W * 2 / 5 { 0.08 + 0.04 * (y as f32 / H as f32) } else { 1.2 + 0.6 * (x as f32 / W as f32) };
            let mut sensor = [0.0f32; 3];
            for c in 0..3 {
                let n = 0.5 * (hash(x, y, seed * 5 + c as u32) + hash(x, y, seed * 5 + 3 + c as u32));
                sensor[c] = (g * s * COLOUR[c] / wb[c] * (1.0 + 0.01 * n)).clamp(0.0, 1.0);
            }
            let o = loader_pixel(sensor, wb);
            img.set_pixel(x as u32, y as u32, [o[0], o[1], o[2], 1.0]);
        }
    }
    img
}

#[test]
fn neutralised_sky_does_not_bias_the_measured_exposure() {
    let frames: Vec<ImageBuf> = [(0.25, 1), (1.0, 2), (4.0, 3)].iter().map(|&(g, s)| landscape_frame(g, s)).collect();
    for meta in [[None; 3], [Some(-2.0), Some(0.0), Some(2.0)]] {
        let out = merge_hdr(&refs(&frames), 1, &meta, &HdrParams::default()).unwrap();
        // Ground at the reference exposure (gain 1).
        let (mut a, mut b) = (0.0f64, 0.0f64);
        for y in 0..H {
            for x in 0..W * 2 / 5 {
                let s = 0.08 + 0.04 * (y as f32 / H as f32);
                a += out.pixel(x, y)[1] as f64;
                b += (s * COLOUR[1]) as f64;
            }
        }
        let e = (a / b - 1.0).abs();
        assert!(e < 0.03, "metadata {:?}: ground off by {e}", meta[0]);
    }
}

#[test]
fn a_measured_ratio_far_from_the_metadata_gives_way_with_a_note() {
    let frames = bracket([(0, 0); 3]);
    let mut notes = Vec::new();
    // Metadata that disagrees by a full stop with what the pixels say: the metadata wins, with a note.
    let wrong = [Some(-3.0), Some(0.0), Some(2.0)];
    let _ = merge_hdr_ex(&refs(&frames), 1, &wrong, &HdrParams::default(), &|| false, &mut notes).unwrap();
    assert!(notes.iter().any(|n| n.contains("image 1") && n.contains("metadata is used")), "{notes:?}");
}

#[test]
fn motion_in_the_dark_frame_is_left_to_deghosting() {
    // A mid-grey block moves in the darkest frame only; with the reference in the middle the result
    // at the block must follow the reference, not a mix.
    let block = (30, 60, 50, 80);
    let mut frames = bracket([(0, 0); 3]);
    frames[0] = render(&Shot { block: Some(block), ..Shot::new(-2.0, 1) });
    let inside = |x: i64, y: i64| (block.0 + 2..block.2 - 2).contains(&x) && (block.1 + 2..block.3 - 2).contains(&y);
    let on = HdrParams { deghost: 0.5, ..Default::default() };
    let clean = merge_hdr(&refs(&frames), 1, &[None; 3], &on).unwrap();
    assert!(region_error(&clean, 0, inside) < 0.06, "{}", region_error(&clean, 0, inside));
}

#[test]
fn a_frame_whose_exposure_cannot_be_told_is_left_out() {
    let mut frames = bracket([(0, 0); 3]);
    // Fully clipped frame, no metadata.
    let mut white = ImageBuf::new(W, H);
    for y in 0..H {
        for x in 0..W {
            white.set_pixel(x, y, [1.0, 1.0, 1.0, 1.0]);
        }
    }
    frames.push(white);
    let mut notes = Vec::new();
    let out = merge_hdr_ex(&refs(&frames), 1, &[None; 4], &HdrParams::default(), &|| false, &mut notes).unwrap();
    assert!(notes.iter().any(|n| n.contains("image 4") && n.contains("left out")), "{notes:?}");
    check_all_regions(&out, 0.03);
}

/// Fable's landscape probe (blue sky with white clouds over dark ground, loader neutralisation):
/// the measured exposure must match the metadata within 3 %.
#[test]
fn blue_sky_and_clouds_neutralised_in_two_frames_keep_the_measured_exposure() {
    let (w, h) = (300usize, 200usize);
    let wb = [2.0f32, 1.0, 1.5];
    let horizon = 120usize;
    let scene = |x: usize, y: usize| -> [f32; 3] {
        if y < horizon {
            if ((x as f32 * 0.08).sin() * (y as f32 * 0.14).cos()) > 0.8 {
                [0.9, 0.9, 0.9]
            } else {
                let s = 0.42 + 0.12 * y as f32 / horizon as f32;
                [0.25 * s, 0.45 * s, s]
            }
        } else {
            let t = 0.01 + 0.10 * (0.5 + 0.5 * ((x as f32 * 0.84).sin() * (y as f32 * 0.68).cos())) * (0.3 + 0.7 * x as f32 / w as f32);
            [t * 0.9, t, t * 0.6]
        }
    };
    let frames: Vec<ImageBuf> = [(0.25f32, 1u32), (1.0, 2), (4.0, 3)]
        .iter()
        .map(|&(g, seed)| {
            let mut img = ImageBuf::new(w as u32, h as u32);
            for y in 0..h {
                for x in 0..w {
                    let sc = scene(x, y);
                    let mut sensor = [0.0f32; 3];
                    for c in 0..3 {
                        let n = 0.5 * (hash(x as i64, y as i64, seed * 5 + c as u32) + hash(x as i64, y as i64, seed * 5 + 3 + c as u32));
                        sensor[c] = (g * sc[c] / wb[c] * (1.0 + 0.01 * n)).clamp(0.0, 1.0);
                    }
                    let o = loader_pixel(sensor, wb);
                    img.set_pixel(x as u32, y as u32, [o[0], o[1], o[2], 1.0]);
                }
            }
            img
        })
        .collect();
    for meta in [[None; 3], [Some(-2.0), Some(0.0), Some(2.0)]] {
        let out = merge_hdr(&refs(&frames), 1, &meta, &HdrParams::default()).unwrap();
        let (mut a, mut b) = (0.0f64, 0.0f64);
        for y in horizon + 10..h - 10 {
            for x in 10..w - 10 {
                a += out.pixel(x as u32, y as u32)[1] as f64;
                b += scene(x, y)[1] as f64;
            }
        }
        let e = (a / b - 1.0).abs();
        assert!(e < 0.03, "metadata {:?}: ground off by {e}", meta[0]);
    }
}

