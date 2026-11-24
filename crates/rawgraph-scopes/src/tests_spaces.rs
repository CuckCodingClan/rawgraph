//! Tests of the output colour spaces of the display transform and of the waveform colours.

use crate::*;
use rawgraph_core::color::{
    acescg_to_output, invert, mul_vec, rec709_to_output, srgb_eotf, OutputSpace, Transfer,
};
use rawgraph_core::ImageBuf;

fn clip(ev: f32) -> DisplayTransform {
    DisplayTransform {
        exposure_ev: ev,
        tone_map: ToneMap::Clip,
    }
}

fn soft() -> DisplayTransform {
    DisplayTransform {
        exposure_ev: 0.0,
        tone_map: ToneMap::SoftHighlights,
    }
}

const TONES: [ToneMap; 3] = [ToneMap::Clip, ToneMap::SoftHighlights, ToneMap::AcesFit];

/// Deterministic pseudo random pixels including extreme values.
fn random_image(w: u32, h: u32, seed: u64) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    let mut state = seed | 1;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let special = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1.0e30,
        -1.0e30,
        3.0e38,
        -3.0e38,
        0.0,
        -0.0,
        1.0e-30,
        1.0e-8,
        -0.5,
        8.0,
    ];
    for (i, px) in img.data_mut().as_chunks_mut::<4>().0.iter_mut().enumerate() {
        for v in px.iter_mut().take(3) {
            let r = next();
            *v = if r < 0.15 {
                special[(next() * special.len() as f64) as usize % special.len()]
            } else if r < 0.5 {
                (next() * 1.2) as f32
            } else {
                (next() * 16.0 - 2.0) as f32
            };
        }
        px[3] = if i % 2 == 0 { 1.0 } else { 0.5 };
    }
    img
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|x| x.to_bits()).collect()
}

fn pixel(rgb: [f32; 3]) -> ImageBuf {
    let mut img = ImageBuf::new(1, 1);
    let px = &mut img.data_mut()[..4];
    px[..3].copy_from_slice(&rgb);
    px[3] = 1.0;
    img
}

/// ACEScg of the linear colour `lin` of `space`.
fn acescg_of(space: OutputSpace, lin: [f64; 3]) -> [f32; 3] {
    let v = mul_vec(&invert(&acescg_to_output(space)), lin);
    [v[0] as f32, v[1] as f32, v[2] as f32]
}

#[test]
fn srgb_space_is_bit_identical_to_apply() {
    let img = random_image(61, 37, 0x9E37_79B9_7F4A_7C15);
    for tone in TONES {
        for ev in [-2.0f32, 0.0, 0.7, 3.0] {
            let t = DisplayTransform {
                exposure_ev: ev,
                tone_map: tone,
            };
            let a = t.apply(&img);
            let b = t.apply_in_space(&img, OutputSpace::Srgb);
            assert_eq!(bits(&a.rgb), bits(&b.rgb), "{tone:?} ev {ev}");
            let (a, a8) = t.apply_with_rgba8(&img);
            let (b, b8) = t.apply_with_rgba8_in_space(&img, OutputSpace::Srgb);
            assert_eq!(bits(&a.rgb), bits(&b.rgb), "{tone:?} ev {ev} (rgba8)");
            assert_eq!(a8, b8);
        }
    }
}

#[test]
fn rgba8_in_space_matches_apply_then_to_rgba8() {
    let img = random_image(40, 23, 7);
    for space in OutputSpace::ALL {
        for tone in TONES {
            let t = DisplayTransform {
                exposure_ev: 0.3,
                tone_map: tone,
            };
            let a = t.apply_in_space(&img, space);
            let (b, b8) = t.apply_with_rgba8_in_space(&img, space);
            assert_eq!(bits(&a.rgb), bits(&b.rgb), "{space:?} {tone:?}");
            assert_eq!(a.to_rgba8(), b8, "{space:?} {tone:?}");
        }
    }
}

#[test]
fn display_p3_red_survives_in_p3_and_clips_in_srgb() {
    let img = pixel(acescg_of(OutputSpace::DisplayP3, [1.0, 0.0, 0.0]));
    let t = clip(0.0);
    let p3 = t.apply_in_space(&img, OutputSpace::DisplayP3).rgb;
    assert!(
        (p3[0] - 1.0).abs() < 1e-4 && p3[1].abs() < 1e-4 && p3[2].abs() < 1e-4,
        "{p3:?}"
    );
    let (_, px8) = t.apply_with_rgba8_in_space(&img, OutputSpace::DisplayP3);
    assert_eq!(&px8[..4], &[255, 0, 0, 255]);
    // In sRGB the same colour lies outside the gamut: red above 1, green negative.
    let srgb = t.apply_in_space(&img, OutputSpace::Srgb).rgb;
    assert!(srgb[0] > 1.01, "{srgb:?}");
    assert!(srgb[1] < -0.01, "{srgb:?}");
    assert_eq!(
        &t.apply_with_rgba8_in_space(&img, OutputSpace::Srgb).1[..4],
        &[255, 0, 0, 255]
    );
    // Rec. 2020 holds (almost all of) P3: red stays below 1 and green positive; the linear blue
    // is -0.001, which the gamma-2.4 encoding of a tiny negative magnifies to about -0.05.
    let r2020 = t.apply_in_space(&img, OutputSpace::Rec2020).rgb;
    assert!(
        r2020[0] < 0.95 && r2020[1] > 0.2 && r2020[2] > -0.08,
        "{r2020:?}"
    );
}

#[test]
fn neutral_greys_match_in_every_space_up_to_the_curve() {
    for v in [0.0f32, 0.001, 0.05, 0.18, 0.5, 1.0] {
        let img = pixel([v; 3]);
        for space in OutputSpace::ALL {
            let d = clip(0.0).apply_in_space(&img, space).rgb;
            // The matrix rows of a shared white sum to 1, so a neutral keeps its linear value.
            let want = space.encode(v);
            for (c, x) in d.iter().enumerate() {
                assert!(
                    (x - want).abs() < 2e-4 + 1e-3 * want,
                    "{space:?} v {v} ch {c}: {x} vs {want}"
                );
            }
            let s = soft().apply_in_space(&img, space).rgb;
            assert!(
                (s[0] - s[1]).abs() < 1e-4 && (s[1] - s[2]).abs() < 1e-4,
                "{space:?} {s:?}"
            );
        }
    }
    let img = pixel([0.18; 3]);
    let g = |s| clip(0.0).apply_in_space(&img, s).rgb[1];
    assert!((g(OutputSpace::DisplayP3) - g(OutputSpace::Srgb)).abs() < 2e-4);
    assert!((g(OutputSpace::AdobeRgb) - 0.18f32.powf(256.0 / 563.0)).abs() < 5e-4);
    assert!((g(OutputSpace::Rec2020) - 0.18f32.powf(1.0 / 2.4)).abs() < 5e-4);
}

#[test]
fn adobe_uses_gamma_563_256_and_rec2020_gamma_2_4() {
    assert_eq!(
        OutputSpace::AdobeRgb.transfer(),
        Transfer::Gamma(563.0 / 256.0)
    );
    assert_eq!(OutputSpace::Rec2020.transfer(), Transfer::Gamma(2.4));
    let img = pixel([0.5; 3]);
    let a = clip(0.0).apply_in_space(&img, OutputSpace::AdobeRgb).rgb[0];
    assert!((a - 0.5f32.powf(256.0 / 563.0)).abs() < 3e-4, "{a}");
    let r = clip(0.0).apply_in_space(&img, OutputSpace::Rec2020).rgb[0];
    assert!((r - 0.5f32.powf(1.0 / 2.4)).abs() < 3e-4, "{r}");
    // A pure power law has no linear toe: a dark value is encoded exactly.
    let dark = pixel([0.001; 3]);
    let a = clip(0.0).apply_in_space(&dark, OutputSpace::AdobeRgb).rgb[0];
    assert!(
        (a - 0.001f32.powf(256.0 / 563.0)).abs() < 5e-4,
        "no toe: {a}"
    );
}

#[test]
fn aces_fit_stays_inside_the_rec709_gamut_in_every_space() {
    let img = random_image(50, 40, 99);
    let t = DisplayTransform {
        exposure_ev: 0.0,
        tone_map: ToneMap::AcesFit,
    };
    let want = t.apply_in_space(&img, OutputSpace::Srgb);
    for space in OutputSpace::ALL {
        let d = t.apply_in_space(&img, space);
        assert!(
            d.rgb.iter().all(|v| (0.0..=1.0).contains(v)),
            "{space:?}: out of 0..1"
        );
        // Decode and map back to Rec.709: the result is the sRGB result, a colour inside the
        // Rec.709 gamut, whatever the space.
        let back = invert(&rec709_to_output(space));
        let lin = |v: f32| match space.transfer() {
            Transfer::Srgb => f64::from(srgb_eotf(v)),
            Transfer::Gamma(g) => f64::from(v).powf(g),
        };
        for (px, w) in d.rgb.chunks(3).zip(want.rgb.chunks(3)) {
            let r709 = mul_vec(&back, [lin(px[0]), lin(px[1]), lin(px[2])]);
            for c in 0..3 {
                let expect = f64::from(srgb_eotf(w[c]));
                assert!(
                    (r709[c] - expect).abs() < 2e-3,
                    "{space:?}: {r709:?} vs {w:?}"
                );
            }
        }
    }
}

#[test]
fn extreme_values_follow_the_srgb_rules_in_every_space() {
    let neutral_cases = [
        (1.0e30f32, true),
        (f32::INFINITY, true),
        (3.0e38, true),
        (-1.0e30, false),
        (f32::NEG_INFINITY, false),
        (f32::NAN, false),
        (-0.5, false),
    ];
    for space in OutputSpace::ALL {
        for tone in TONES {
            for (v, white) in neutral_cases {
                let t = DisplayTransform {
                    exposure_ev: 0.0,
                    tone_map: tone,
                };
                let (d, px8) = t.apply_with_rgba8_in_space(&pixel([v; 3]), space);
                let want = if white { 255 } else { 0 };
                assert_eq!(&px8[..3], &[want; 3], "{space:?} {tone:?} {v}: {:?}", d.rgb);
                assert!(
                    d.rgb.iter().all(|x| !x.is_nan()),
                    "{space:?} {tone:?} {v}: NaN in {:?}",
                    d.rgb
                );
            }
        }
    }
    // Random extremes never produce NaN, in any space.
    let img = random_image(70, 30, 5);
    for space in OutputSpace::ALL {
        for tone in TONES {
            let d = DisplayTransform {
                exposure_ev: 1.0,
                tone_map: tone,
            }
            .apply_in_space(&img, space);
            assert!(d.rgb.iter().all(|x| !x.is_nan()), "{space:?} {tone:?}");
        }
    }
}

#[test]
fn empty_images_work_in_every_space() {
    for space in OutputSpace::ALL {
        let d = clip(0.0).apply_in_space(&ImageBuf::new(0, 0), space);
        assert!(d.rgb.is_empty());
        let (d, p) = clip(0.0).apply_with_rgba8_in_space(&ImageBuf::new(0, 3), space);
        assert!(d.rgb.is_empty() && p.is_empty());
    }
}

// ------------------------------------------------------------- waveform colours

/// Left half pure red, right half pure green (display values).
fn red_green(w: u32, h: u32) -> DisplayImage {
    let mut rgb = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            rgb.extend(if x < w / 2 {
                [0.8f32, 0.1, 0.1]
            } else {
                [0.1, 0.7, 0.2]
            });
        }
    }
    DisplayImage {
        width: w,
        height: h,
        rgb,
    }
}

#[test]
fn waveform_colors_hold_the_mean_colour_of_each_bin() {
    let img = red_green(64, 8);
    let req = ScopeRequest {
        waveform: Some((64, 64)),
        colorize: true,
        ..Default::default()
    };
    let s = analyze(&img, &req);
    let w = s.waveform.as_ref().unwrap();
    let c = s.waveform_colors.as_ref().unwrap();
    assert_eq!(c.rgb_sum.len(), w.counts.len() * 3);
    assert_eq!((c.columns, c.levels), (w.columns, w.levels));
    let mut lit = 0;
    for (i, &n) in w.counts.iter().enumerate() {
        if n == 0 {
            assert_eq!(c.sum(i), Some([0, 0, 0]), "bin {i}");
            assert!(c.mean(i, n).is_none());
            continue;
        }
        lit += 1;
        let col = i % 64;
        let m = c.mean(i, n).unwrap();
        let want = if col < 32 {
            [0.8, 0.1, 0.1]
        } else {
            [0.1, 0.7, 0.2]
        };
        for k in 0..3 {
            assert!((m[k] - want[k]).abs() < 0.003, "bin {i}: {m:?} vs {want:?}");
        }
    }
    assert_eq!(lit, 64, "one bin per column");
    // Without the request flag no colour data is produced.
    let s = analyze(
        &img,
        &ScopeRequest {
            colorize: false,
            ..req
        },
    );
    assert!(s.waveform_colors.is_none());
    // The waveform counts are the same either way.
    assert_eq!(s.waveform.unwrap().counts, w.counts);
}

#[test]
fn waveform_colors_are_stable_with_steps_lowpass_and_extremes() {
    let img = random_display(77, 41);
    for step in [1usize, 2, 3] {
        for lowpass in [false, true] {
            let req = ScopeRequest {
                waveform: Some((20, 32)),
                colorize: true,
                step,
                lowpass,
                ..Default::default()
            };
            let a = analyze(&img, &req);
            let b = analyze(&img, &req);
            assert_eq!(a, b, "deterministic, step {step} lowpass {lowpass}");
            let w = a.waveform.unwrap();
            let c = a.waveform_colors.unwrap();
            // A bin's mean colour is a colour inside 0..=1.
            for (i, &n) in w.counts.iter().enumerate() {
                if let Some(m) = c.mean(i, n) {
                    assert!(m.iter().all(|v| (0.0..=1.0 + 1e-6).contains(v)), "{m:?}");
                }
            }
        }
    }
    // Empty picture.
    let empty = DisplayImage {
        width: 0,
        height: 0,
        rgb: vec![],
    };
    let s = analyze(
        &empty,
        &ScopeRequest {
            waveform: Some((4, 4)),
            colorize: true,
            ..Default::default()
        },
    );
    assert_eq!(s.waveform_colors.unwrap().rgb_sum, vec![0; 48]);
}

fn random_display(w: u32, h: u32) -> DisplayImage {
    let mut state = 12345u32;
    let rgb = (0..w * h * 3)
        .map(|i| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            match i % 17 {
                0 => f32::NAN,
                1 => 5.0,
                2 => -3.0,
                _ => (state >> 8) as f32 / (1u32 << 24) as f32,
            }
        })
        .collect();
    DisplayImage {
        width: w,
        height: h,
        rgb,
    }
}

#[test]
fn waveform_colors_line_up_with_the_counts_and_ignore_the_thread_count() {
    let img = random_display(150, 61);
    for (step, lowpass) in [(1usize, false), (2, false), (3, true), (1, true)] {
        let req = ScopeRequest {
            waveform: Some((24, 40)),
            colorize: true,
            step,
            lowpass,
            ..Default::default()
        };
        let reference = analyze(&img, &req);
        let (w, c) = (
            reference.waveform.as_ref().unwrap(),
            reference.waveform_colors.as_ref().unwrap(),
        );
        let mismatched = w
            .counts
            .iter()
            .enumerate()
            .filter(|&(i, &n)| n == 0 && c.sum(i) != Some([0, 0, 0]))
            .count();
        // Unfiltered: exact. With the low-pass filter a sample on the very edge between two
        // levels may round differently in the second pass (checked: almost never).
        assert!(
            mismatched <= if lowpass { 3 } else { 0 },
            "step {step} lowpass {lowpass}: {mismatched}"
        );
        // Colour totals: every counted pixel adds up to 3 values of at most SCALE.
        let total: u64 = c.rgb_sum.iter().map(|&v| u64::from(v)).sum();
        let counted: u64 = w.counts.iter().map(|&n| u64::from(n)).sum();
        assert!(total <= counted * 3 * u64::from(WaveformColors::SCALE));
        for threads in [1usize, 3, 7] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let again = pool.install(|| analyze(&img, &req));
            assert_eq!(
                again, reference,
                "{threads} threads, step {step} lowpass {lowpass}"
            );
        }
    }
}
