use crate::patterns::*;
use crate::*;
use rawgraph_core::color::srgb_oetf;
use rawgraph_core::ImageBuf;

fn neutral(w: u32, h: u32, v: f32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for px in img.data_mut().as_chunks_mut::<4>().0 {
        px[0] = v;
        px[1] = v;
        px[2] = v;
    }
    img
}

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

#[test]
fn display_mid_grey_and_white() {
    let d = clip(0.0).apply(&neutral(2, 2, 0.18));
    assert!(
        d.rgb.iter().all(|v| (v - 0.4614).abs() < 1e-3),
        "{:?}",
        &d.rgb[..3]
    );
    let d = clip(0.0).apply(&neutral(2, 2, 1.0));
    assert!(
        d.rgb.iter().all(|v| (v - 1.0).abs() < 1e-3),
        "{:?}",
        &d.rgb[..3]
    );
    let up = clip(1.0).apply(&neutral(1, 1, 0.18));
    let expect = srgb_oetf(0.36);
    assert!(up.rgb.iter().all(|v| (v - expect).abs() < 1e-3));
}

#[test]
fn display_highlights() {
    let img = neutral(1, 1, 4.0);
    let c = clip(0.0).apply(&img);
    assert!(c.rgb.iter().all(|&v| v > 1.0));
    assert!(c.to_rgba8().iter().all(|&v| v == 255));
    let s = soft().apply(&img);
    assert!(s.rgb.iter().all(|&v| v < 1.0 && v > 0.0), "{:?}", s.rgb);
    let w = soft().apply(&neutral(1, 1, 8.0));
    assert!(w.rgb.iter().all(|v| (v - 1.0).abs() < 1e-4), "{:?}", w.rgb);
    let mut prev = -1.0;
    for i in 0..=800 {
        let v = soft().apply(&neutral(1, 1, i as f32 / 100.0)).rgb[1];
        assert!(v >= prev, "not monotonic at {i}");
        prev = v;
    }
}

#[test]
fn rgba8_layout_and_clamp() {
    let img = DisplayImage {
        width: 2,
        height: 1,
        rgb: vec![-0.5, 0.5, 2.0, 0.0, 1.0, f32::NAN],
    };
    let o = img.to_rgba8();
    assert_eq!(o.len(), 8);
    assert_eq!(o, vec![0, 128, 255, 255, 0, 255, 0, 255]);
}

#[test]
fn ramp_waveform_is_diagonal() {
    let img = horizontal_ramp(256, 8);
    let wf = waveform(&img, 256, 256, WaveformChannel::Luma);
    for c in 0..256 {
        let col: Vec<u32> = (0..256).map(|r| wf.counts[r * 256 + c]).collect();
        let rows: Vec<usize> = (0..256).filter(|&r| col[r] != 0).collect();
        assert_eq!(rows.len(), 1, "column {c}");
        assert_eq!(col[rows[0]], 8);
        let want = 255 - c as i64;
        assert!(
            (rows[0] as i64 - want).abs() <= 1,
            "column {c} row {}",
            rows[0]
        );
    }
}

#[test]
fn flat_grey_histogram_single_bin() {
    let img = flat(10, 7, [0.5, 0.5, 0.5]);
    let h = histogram(&img, 256);
    // floor(0.5 * 256) = 128 for every channel; luma weights sum to ~1 so luma
    // of a neutral 0.5 sits in bin 127 or 128 (checked as a single bin below).
    for ch in [&h.r, &h.g, &h.b] {
        assert_eq!(ch[128], 70);
        assert_eq!(ch.iter().sum::<u32>(), 70);
    }
    assert_eq!(h.luma.iter().sum::<u32>(), 70);
    assert_eq!(h.luma.iter().filter(|&&c| c != 0).count(), 1);
    assert_eq!(h.luma[127] + h.luma[128], 70);
}

fn nasty() -> DisplayImage {
    DisplayImage {
        width: 3,
        height: 2,
        rgb: vec![
            -0.5,
            2.0,
            f32::NAN,
            0.5,
            0.5,
            0.5,
            f32::INFINITY,
            f32::NEG_INFINITY,
            1.0,
            0.0,
            0.0,
            0.0,
            1.0,
            1.0,
            1.0,
            f32::NAN,
            f32::NAN,
            f32::NAN,
        ],
    }
}

#[test]
fn counts_sum_to_pixels() {
    for img in [nasty(), color_bars(37, 5, 0.75), horizontal_ramp(1, 3)] {
        let n = img.width * img.height;
        let h = histogram(&img, 64);
        for ch in [&h.r, &h.g, &h.b, &h.luma] {
            assert_eq!(ch.iter().sum::<u32>(), n);
        }
        for ch in [
            WaveformChannel::Luma,
            WaveformChannel::Red,
            WaveformChannel::Green,
            WaveformChannel::Blue,
        ] {
            assert_eq!(waveform(&img, 16, 32, ch).counts.iter().sum::<u32>(), n);
        }
        for w in parade(&img, 16, 32) {
            assert_eq!(w.counts.iter().sum::<u32>(), n);
        }
        assert_eq!(vectorscope(&img, 64).counts.iter().sum::<u32>(), n);
    }
}

#[test]
fn huge_and_infinite_values_display_white_in_every_mode() {
    let modes = [ToneMap::Clip, ToneMap::SoftHighlights, ToneMap::AcesFit];
    for tone_map in modes {
        let t = DisplayTransform {
            exposure_ev: 0.0,
            tone_map,
        };
        for v in [1e6f32, 1e12, 1e30, f32::MAX, f32::INFINITY] {
            let out = t.apply_with_rgba8(&neutral(1, 1, v)).1;
            assert_eq!(&out[..3], &[255, 255, 255], "{tone_map:?} {v}");
        }
        for v in [-1.0f32, -1e30, f32::NEG_INFINITY, f32::NAN] {
            let out = t.apply_with_rgba8(&neutral(1, 1, v)).1;
            assert_eq!(&out[..3], &[0, 0, 0], "{tone_map:?} {v}");
        }
        // Two infinite channels must not turn into NaN (inf - inf) in the matrix product.
        let mut img = ImageBuf::new(1, 1);
        img.set_pixel(0, 0, [f32::INFINITY, f32::INFINITY, 0.0, 1.0]);
        let out = t.apply_with_rgba8(&img).1;
        assert!(out[0] == 255 && out[1] == 255, "{tone_map:?} {out:?}");
    }
}

#[test]
fn nan_is_treated_as_zero() {
    let img = DisplayImage {
        width: 1,
        height: 1,
        rgb: vec![f32::NAN; 3],
    };
    let h = histogram(&img, 8);
    assert_eq!(h.r[0], 1);
    let a = DisplayTransform::default().apply(&neutral(1, 1, f32::NAN));
    assert!(a.rgb.iter().all(|&v| v == 0.0));
}

#[test]
fn cbcr_values() {
    for v in [0.0, 0.3, 1.0] {
        let (cb, cr) = cbcr([v; 3]);
        assert!(cb.abs() < 1e-6 && cr.abs() < 1e-6);
    }
    let (cb, cr) = cbcr([1.0, 0.0, 0.0]);
    assert!(
        (cb + 0.1146).abs() < 1e-3 && (cr - 0.5).abs() < 1e-3,
        "{cb} {cr}"
    );
    let (cb, cr) = cbcr([0.0, 0.0, 1.0]);
    assert!(
        (cb - 0.5).abs() < 1e-3 && (cr + 0.0459).abs() < 1e-3,
        "{cb} {cr}"
    );
    let rgbs = [
        [1.0, 0.0, 0.0],
        [1.0, 0.0, 1.0],
        [0.0, 0.0, 1.0],
        [0.0, 1.0, 1.0],
        [0.0, 1.0, 0.0],
        [1.0, 1.0, 0.0],
    ];
    for ((_, cb, cr), rgb) in vectorscope_targets().iter().zip(rgbs) {
        let (a, b) = cbcr(rgb);
        assert_eq!((a, b), (*cb, *cr));
    }
}

#[test]
fn bars_hit_exactly_the_75_percent_targets() {
    let size = 256;
    let img = color_bars(640, 64, 0.75);
    let vs = vectorscope(&img, size);
    let per_bar = 640 / 8 * 64;
    let mut expect = std::collections::HashMap::new();
    for (_, cb, cr) in vectorscope_targets() {
        let b = vectorscope_bin(cb * 0.75, cr * 0.75, size);
        *expect.entry(b).or_insert(0u32) += per_bar;
    }
    *expect.entry(vectorscope_bin(0.0, 0.0, size)).or_insert(0) += 2 * per_bar;
    assert_eq!(expect.len(), 7, "targets must land in distinct bins");
    for (i, &c) in vs.counts.iter().enumerate() {
        let want = expect.get(&(i % size, i / size)).copied().unwrap_or(0);
        assert_eq!(c, want, "bin ({}, {})", i % size, i / size);
    }
}

#[test]
fn red_parade() {
    let img = flat(20, 10, [1.0, 0.0, 0.0]);
    let [r, g, b] = parade(&img, 4, 16);
    assert_eq!(r.counts[..4].iter().sum::<u32>(), 200);
    assert_eq!(r.counts.iter().sum::<u32>(), 200);
    for w in [g, b] {
        assert_eq!(w.counts[12 * 4..].iter().sum::<u32>(), 200);
        assert_eq!(w.counts.iter().sum::<u32>(), 200);
    }
}

#[test]
fn lut_matches_exact_srgb() {
    let lut = crate::display::lut_for_tests();
    let mut worst = 0.0f32;
    let n = 2_000_000;
    for i in 0..=n {
        let x = i as f32 / n as f32;
        worst = worst.max((crate::display::srgb_encode_fast(lut, x) - srgb_oetf(x)).abs());
    }
    assert!(worst <= 1e-4, "worst LUT error {worst}");
    for x in [-0.5f32, -0.001, 1.0, 1.5, 4.0, 100.0] {
        assert_eq!(crate::display::srgb_encode_fast(lut, x), srgb_oetf(x));
    }
}

fn gradient(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for (i, px) in img.data_mut().as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (i % w as usize) as f32 / w as f32;
        let y = (i / w as usize) as f32 / h as f32;
        px[0] = x * 3.0 - 0.2;
        px[1] = y * y;
        px[2] = (x + y) * 0.5;
    }
    img
}

#[test]
fn fast_apply_is_close_to_exact_and_fused_is_identical() {
    let img = gradient(97, 53);
    for tr in [clip(0.0), clip(1.5), soft()] {
        let fast = tr.apply(&img);
        let exact = tr.apply_exact(&img);
        for (a, b) in fast.rgb.iter().zip(&exact.rgb) {
            assert!((a - b).abs() <= 1e-4, "{a} {b}");
        }
        let (d, rgba) = tr.apply_with_rgba8(&img);
        assert_eq!(d, fast);
        assert_eq!(rgba, fast.to_rgba8());
    }
}

fn assert_analyze_matches(img: &DisplayImage) {
    let req = ScopeRequest {
        waveform: Some((50, 40)),
        parade: Some((30, 20)),
        vectorscope: Some(64),
        histogram: Some(32),
        step: 1,
        ..Default::default()
    };
    let s = analyze(img, &req);
    assert_eq!(s.samples, (img.width * img.height) as u64);
    assert_eq!(
        s.waveform.unwrap(),
        waveform(img, 50, 40, WaveformChannel::Luma)
    );
    assert_eq!(s.parade.unwrap(), parade(img, 30, 20));
    assert_eq!(s.vectorscope.unwrap(), vectorscope(img, 64));
    assert_eq!(s.histogram.unwrap(), histogram(img, 32));
}

#[test]
fn analyze_step_one_equals_single_scopes() {
    assert_analyze_matches(&nasty());
    assert_analyze_matches(&color_bars(640, 64, 0.75));
    assert_analyze_matches(&horizontal_ramp(256, 8));
    assert_analyze_matches(&flat(10, 7, [0.5, 0.2, 0.9]));
    assert_analyze_matches(&soft().apply(&gradient(301, 211)));
}

#[test]
fn analyze_step_samples_and_partial_requests() {
    let img = soft().apply(&gradient(301, 211));
    for step in [2usize, 3, 7] {
        let req = ScopeRequest {
            waveform: Some((64, 32)),
            parade: Some((64, 32)),
            vectorscope: Some(32),
            histogram: Some(16),
            step,
            ..Default::default()
        };
        let s = analyze(&img, &req);
        let expect = (301usize.div_ceil(step) * 211usize.div_ceil(step)) as u64;
        assert_eq!(s.samples, expect);
        let sum = |c: &[u32]| c.iter().map(|&v| v as u64).sum::<u64>();
        assert_eq!(sum(&s.waveform.unwrap().counts), expect);
        for w in s.parade.unwrap() {
            assert_eq!(sum(&w.counts), expect);
        }
        assert_eq!(sum(&s.vectorscope.unwrap().counts), expect);
        let h = s.histogram.unwrap();
        for ch in [&h.r, &h.g, &h.b, &h.luma] {
            assert_eq!(sum(ch), expect);
        }
    }
    let none = analyze(
        &img,
        &ScopeRequest {
            waveform: None,
            parade: None,
            vectorscope: Some(8),
            histogram: None,
            step: 1,
            ..Default::default()
        },
    );
    assert!(none.waveform.is_none() && none.parade.is_none() && none.histogram.is_none());
    assert!(none.vectorscope.is_some());
}

fn aces() -> DisplayTransform {
    DisplayTransform {
        exposure_ev: 0.0,
        tone_map: ToneMap::AcesFit,
    }
}

fn aces_neutral(v: f32) -> [f32; 3] {
    let d = aces().apply(&neutral(1, 1, v));
    [d.rgb[0], d.rgb[1], d.rgb[2]]
}

#[test]
fn aces_fit_black_and_neutral() {
    for c in aces_neutral(0.0) {
        assert!(c.abs() <= 1e-3, "{c}");
    }
    for v in [0.01f32, 0.18, 1.0, 4.0, 16.0] {
        let o = aces_neutral(v);
        assert!(
            (o[0] - o[1]).abs() < 1e-3 && (o[1] - o[2]).abs() < 1e-3,
            "{v}: {o:?}"
        );
    }
}

#[test]
fn aces_fit_monotonic_and_bounded() {
    let mut prev = -1.0;
    for i in 0..=6400 {
        let o = aces_neutral(i as f32 / 100.0);
        assert!(o[1] >= prev - 1e-6, "not monotonic at {i}");
        prev = o[1];
    }
    for v in [
        1.0e6f32,
        -3.0,
        -1.0e6,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ] {
        for c in aces_neutral(v) {
            assert!((0.0..=1.0).contains(&c), "{v}: {c}");
        }
    }
}

#[test]
fn aces_fit_reference_values() {
    let g = aces_neutral(0.18)[1];
    println!(
        "AcesFit encoded: 0.18 -> {g}, 1.0 -> {}, 4.0 -> {}, 16.0 -> {}",
        aces_neutral(1.0)[1],
        aces_neutral(4.0)[1],
        aces_neutral(16.0)[1]
    );
    assert!((0.35..=0.50).contains(&g), "{g}");
    assert!(aces_neutral(16.0)[1] > 0.97);
}

#[test]
fn aces_fit_fused_matches_and_exposure_acts_before_tone_map() {
    let img = gradient(97, 53);
    for ev in [0.0f32, -2.0, 1.5] {
        let tr = DisplayTransform {
            exposure_ev: ev,
            tone_map: ToneMap::AcesFit,
        };
        let (d, rgba) = tr.apply_with_rgba8(&img);
        assert_eq!(d, tr.apply(&img));
        assert_eq!(rgba, d.to_rgba8());
    }
    // +1 EV on 0.18 equals the tone-mapped 0.36 (exposure precedes the curve).
    let up = DisplayTransform {
        exposure_ev: 1.0,
        tone_map: ToneMap::AcesFit,
    }
    .apply(&neutral(1, 1, 0.18));
    let direct = aces_neutral(0.36);
    for c in 0..3 {
        assert!((up.rgb[c] - direct[c]).abs() < 1e-4);
    }
}

#[test]
#[ignore]
fn perf_smoke() {
    use std::time::Instant;
    let (w, h) = (3840u32, 2160u32);
    let mut img = ImageBuf::new(w, h);
    for (i, px) in img.data_mut().as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = (i % w as usize) as f32 / w as f32;
        let y = (i / w as usize) as f32 / h as f32;
        px[0] = x * 2.0;
        px[1] = y;
        px[2] = (x + y) * 0.5;
    }
    let t = |name: &str, f: &mut dyn FnMut()| -> f64 {
        f(); // warm up
        let mut v: Vec<f64> = (0..15)
            .map(|_| {
                let s = Instant::now();
                f();
                s.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("{name:<28} min {:>7.2} ms   median {:>7.2} ms", v[0], v[7]);
        v[0]
    };
    let tr = soft();
    let d = tr.apply(&img);
    let mut sum = 0.0;
    println!("--- old path (powf) ---");
    sum += t("apply (exact powf)", &mut || {
        std::hint::black_box(tr.apply_exact(&img));
    });
    sum += t("to_rgba8", &mut || {
        std::hint::black_box(d.to_rgba8());
    });
    let mut sep = 0.0;
    sep += t("waveform 512x256", &mut || {
        std::hint::black_box(waveform(&d, 512, 256, WaveformChannel::Luma));
    });
    sep += t("parade 3x256x256", &mut || {
        std::hint::black_box(parade(&d, 256, 256));
    });
    sep += t("vectorscope 256", &mut || {
        std::hint::black_box(vectorscope(&d, 256));
    });
    sep += t("histogram 256", &mut || {
        std::hint::black_box(histogram(&d, 256));
    });
    println!("old total (sum of minima)    {:.2} ms", sum + sep);
    println!("--- new path ---");
    let aces_tr = DisplayTransform {
        exposure_ev: 0.0,
        tone_map: ToneMap::AcesFit,
    };
    let clip_tr = clip(0.0);
    t("apply_with_rgba8 Clip", &mut || {
        std::hint::black_box(clip_tr.apply_with_rgba8(&img));
    });
    t("apply_with_rgba8 AcesFit", &mut || {
        std::hint::black_box(aces_tr.apply_with_rgba8(&img));
    });
    let a = t("apply (LUT)", &mut || {
        std::hint::black_box(tr.apply(&img));
    });
    let b = t("apply_with_rgba8", &mut || {
        std::hint::black_box(tr.apply_with_rgba8(&img));
    });
    let req = |step| ScopeRequest {
        waveform: Some((512, 256)),
        parade: Some((256, 256)),
        vectorscope: Some(256),
        histogram: Some(256),
        step,
        ..Default::default()
    };
    let s1 = t("analyze step 1", &mut || {
        std::hint::black_box(analyze(&d, &req(1)));
    });
    let s2 = t("analyze step 2", &mut || {
        std::hint::black_box(analyze(&d, &req(2)));
    });
    println!(
        "(a) apply_with_rgba8 + analyze1 = {:.2} ms (apply alone {:.2})",
        b + s1,
        a
    );
    println!("(b) apply_with_rgba8 + analyze2 = {:.2} ms", b + s2);
}

// ------------------------------------------------------------------ wave 5

fn hash01(x: u32, y: u32, salt: u32) -> f32 {
    let mut h = x
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add(y.wrapping_mul(0x85EB_CA6B))
        .wrapping_add(salt.wrapping_mul(0xC2B2_AE35));
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h & 0xFFFF) as f32 / 65535.0
}

/// Gradients plus per-channel noise, including values outside 0..1.
fn busy(w: u32, h: u32) -> DisplayImage {
    let mut rgb = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
            rgb.extend([
                u * 1.2 - 0.1 + (hash01(x, y, 1) - 0.5) * 0.1,
                v + (hash01(x, y, 2) - 0.5) * 0.1,
                0.5 * (u + v) + (hash01(x, y, 3) - 0.5) * 0.2,
            ]);
        }
    }
    DisplayImage {
        width: w,
        height: h,
        rgb,
    }
}

fn noisy_flat(w: u32, h: u32, amp: f32) -> DisplayImage {
    let mut rgb = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let n = (hash01(x, y, 9) - 0.5) * amp;
            rgb.extend([0.5 + n; 3]);
        }
    }
    DisplayImage {
        width: w,
        height: h,
        rgb,
    }
}

#[test]
fn yrgb_parade_equals_single_channel_waveforms() {
    let img = busy(97, 41);
    let req = ScopeRequest {
        parade: Some((24, 64)),
        parade_mode: ParadeMode::Yrgb,
        ..Default::default()
    };
    let s = analyze(&img, &req);
    assert!(s.parade.is_none());
    let planes = s.parade_planes.as_ref().unwrap();
    assert_eq!(planes.len(), 4);
    let want = [
        WaveformChannel::Luma,
        WaveformChannel::Red,
        WaveformChannel::Green,
        WaveformChannel::Blue,
    ];
    for (p, ch) in planes.iter().zip(want) {
        assert_eq!(*p, waveform(&img, 24, 64, ch), "{ch:?}");
    }
    assert_eq!(s.parade_columns().unwrap().len(), 4);
}

#[test]
fn ycbcr_parade_y_is_luma_and_chroma_follow_cbcr() {
    let img = busy(97, 41);
    let req = ScopeRequest {
        parade: Some((24, 64)),
        parade_mode: ParadeMode::YCbCr,
        ..Default::default()
    };
    let s = analyze(&img, &req);
    let planes = s.parade_planes.as_ref().unwrap();
    assert_eq!(planes.len(), 3);
    assert_eq!(planes[0], waveform(&img, 24, 64, WaveformChannel::Luma));
    // Independent computation of the Cb and Cr planes.
    let mut cb = vec![0u32; 24 * 64];
    let mut cr = vec![0u32; 24 * 64];
    for y in 0..41usize {
        for x in 0..97usize {
            let i = (y * 97 + x) * 3;
            let (a, b) = cbcr([img.rgb[i], img.rgb[i + 1], img.rgb[i + 2]]);
            let col = x * 24 / 97;
            let row = |v: f32| 63 - ((v + 0.5) * 63.0).round().clamp(0.0, 63.0) as usize;
            cb[row(a) * 24 + col] += 1;
            cr[row(b) * 24 + col] += 1;
        }
    }
    assert_eq!(planes[1].counts, cb);
    assert_eq!(planes[2].counts, cr);
}

/// The only row with counts in column `c`, panicking when there is not exactly one.
fn single_row(w: &Waveform, c: usize) -> usize {
    let rows: Vec<usize> = (0..w.levels)
        .filter(|&r| w.counts[r * w.columns + c] != 0)
        .collect();
    assert_eq!(rows.len(), 1, "column {c}: {rows:?}");
    rows[0]
}

#[test]
fn ycbcr_parade_of_75_percent_bars_hits_known_chroma() {
    // Known Rec.709 chroma of the 100 % bars (Cb, Cr): white, yellow, cyan, green,
    // magenta, red, blue, black. 75 % bars scale them by 0.75.
    let known: [(f32, f32); 8] = [
        (0.0, 0.0),
        (-0.5, 0.0459),
        (0.1146, -0.5),
        (-0.3854, -0.4541),
        (0.3854, 0.4541),
        (-0.1146, 0.5),
        (0.5, -0.0459),
        (0.0, 0.0),
    ];
    let levels = 255usize;
    let img = color_bars(640, 16, 0.75);
    let s = analyze(
        &img,
        &ScopeRequest {
            parade: Some((8, levels)),
            parade_mode: ParadeMode::YCbCr,
            ..Default::default()
        },
    );
    let planes = s.parade_planes.unwrap();
    let row_of =
        |chroma: f32| levels - 1 - ((chroma * 0.75 + 0.5) * (levels - 1) as f32).round() as usize;
    for (bar, (cb, cr)) in known.into_iter().enumerate() {
        let (got_cb, got_cr) = (single_row(&planes[1], bar), single_row(&planes[2], bar));
        assert!(
            (got_cb as i64 - row_of(cb) as i64).abs() <= 1,
            "bar {bar} Cb row {got_cb} want {}",
            row_of(cb)
        );
        assert!(
            (got_cr as i64 - row_of(cr) as i64).abs() <= 1,
            "bar {bar} Cr row {got_cr} want {}",
            row_of(cr)
        );
    }
    // Neutrals sit exactly on the centre row (level 0.5 -> row 127 of 255).
    for bar in [0, 7] {
        assert_eq!(single_row(&planes[1], bar), 127);
        assert_eq!(single_row(&planes[2], bar), 127);
    }
    // Y of the white bar is 0.75, of black 0.
    assert_eq!(
        single_row(&planes[0], 0),
        254 - (0.75f32 * 254.0).round() as usize
    );
    assert_eq!(single_row(&planes[0], 7), 254);
}

#[test]
fn single_channel_waveform_and_overlay_requests() {
    let img = busy(60, 30);
    for ch in [
        WaveformChannel::Red,
        WaveformChannel::Green,
        WaveformChannel::Blue,
        WaveformChannel::Luma,
    ] {
        let s = analyze(
            &img,
            &ScopeRequest {
                waveform: Some((20, 32)),
                waveform_channel: ch,
                ..Default::default()
            },
        );
        assert_eq!(s.waveform.unwrap(), waveform(&img, 20, 32, ch));
    }
    let s = analyze(
        &img,
        &ScopeRequest {
            parade: Some((8, 16)),
            parade_mode: ParadeMode::YCbCr,
            overlay: Some((20, 32)),
            ..Default::default()
        },
    );
    assert_eq!(s.overlay.unwrap(), parade(&img, 20, 32));
    assert!(s.parade.is_none() && s.parade_planes.is_some());
}

#[test]
fn new_options_leave_the_old_results_untouched() {
    for img in [busy(120, 50), nasty(), color_bars(64, 8, 1.0)] {
        let base = ScopeRequest {
            waveform: Some((40, 32)),
            parade: Some((24, 32)),
            vectorscope: Some(48),
            histogram: Some(64),
            ..Default::default()
        };
        let a = analyze(&img, &base);
        let b = analyze(
            &img,
            &ScopeRequest {
                clip_stats: true,
                ..base
            },
        );
        assert_eq!(a.clipping, None);
        assert!(b.clipping.is_some());
        assert_eq!(
            (
                &a.waveform,
                &a.parade,
                &a.vectorscope,
                &a.histogram,
                a.samples
            ),
            (
                &b.waveform,
                &b.parade,
                &b.vectorscope,
                &b.histogram,
                b.samples
            )
        );
        assert!(a.parade_planes.is_none() && a.overlay.is_none());
        // The RGB mode is the default mode.
        let c = analyze(
            &img,
            &ScopeRequest {
                parade_mode: ParadeMode::Rgb,
                ..base
            },
        );
        assert_eq!(a, c);
    }
}

#[test]
fn counts_sum_to_samples_in_every_mode() {
    for img in [busy(101, 37), nasty(), horizontal_ramp(1, 3)] {
        for step in [1usize, 2, 5] {
            for mode in [ParadeMode::Rgb, ParadeMode::Yrgb, ParadeMode::YCbCr] {
                for lowpass in [false, true] {
                    let s = analyze(
                        &img,
                        &ScopeRequest {
                            waveform: Some((16, 32)),
                            parade: Some((8, 16)),
                            overlay: Some((12, 20)),
                            vectorscope: Some(32),
                            histogram: Some(32),
                            step,
                            waveform_channel: WaveformChannel::Green,
                            parade_mode: mode,
                            lowpass,
                            clip_stats: true,
                            colorize: true,
                        },
                    );
                    let sum = |w: &Waveform| w.counts.iter().sum::<u32>() as u64;
                    let n = s.samples;
                    assert_eq!(sum(s.waveform.as_ref().unwrap()), n);
                    for p in s.parade_columns().unwrap() {
                        assert_eq!(sum(p), n, "{mode:?} step {step} lowpass {lowpass}");
                    }
                    assert_eq!(s.parade_columns().unwrap().len(), mode.planes());
                    for p in s.overlay.as_ref().unwrap() {
                        assert_eq!(sum(p), n);
                    }
                    assert_eq!(
                        s.vectorscope.as_ref().unwrap().counts.iter().sum::<u32>() as u64,
                        n
                    );
                    assert_eq!(s.clipping.unwrap().samples, n);
                }
            }
        }
    }
    // Empty images do not panic in any mode.
    let empty = DisplayImage {
        width: 0,
        height: 0,
        rgb: vec![],
    };
    let s = analyze(
        &empty,
        &ScopeRequest {
            waveform: Some((4, 4)),
            parade: Some((4, 4)),
            parade_mode: ParadeMode::Yrgb,
            lowpass: true,
            clip_stats: true,
            ..Default::default()
        },
    );
    assert_eq!(s.samples, 0);
    assert_eq!(s.clipping, Some(ClipStats::default()));
}

#[test]
fn lowpass_keeps_flat_images_and_ramps_and_narrows_noise() {
    let run = |img: &DisplayImage, cols: usize, mode, lowpass| {
        analyze(
            img,
            &ScopeRequest {
                waveform: Some((cols, 256)),
                parade: Some((cols, 256)),
                parade_mode: mode,
                lowpass,
                ..Default::default()
            },
        )
    };
    // Flat: identical.
    let flat_img = flat(400, 6, [0.3, 0.5, 0.7]);
    let (fa, fb) = (
        run(&flat_img, 100, ParadeMode::Rgb, true),
        run(&flat_img, 100, ParadeMode::Rgb, false),
    );
    assert_eq!((&fa.waveform, &fa.parade), (&fb.waveform, &fb.parade));
    // Ramp: every column stays within one row of the unfiltered trace.
    let ramp = horizontal_ramp(400, 4);
    let wa = run(&ramp, 400, ParadeMode::Rgb, true).waveform.unwrap();
    let wb = run(&ramp, 400, ParadeMode::Rgb, false).waveform.unwrap();
    for c in 0..400 {
        let d = single_row(&wa, c) as i64 - single_row(&wb, c) as i64;
        assert!(d.abs() <= 1, "column {c}: {d}");
    }
    // Neutral chroma stays on the centre row.
    let a = run(&ramp, 400, ParadeMode::YCbCr, true);
    let b = run(&ramp, 400, ParadeMode::YCbCr, false);
    assert_eq!(a.parade_planes.unwrap()[1..], b.parade_planes.unwrap()[1..]);
    // Noise: the vertical spread of the trace shrinks.
    let noise = noisy_flat(400, 64, 0.3);
    let spread = |s: &ScopeSet| {
        let w = s.waveform.as_ref().unwrap();
        let per_row: Vec<f64> = (0..w.levels)
            .map(|r| {
                (0..w.columns)
                    .map(|c| f64::from(w.counts[r * w.columns + c]))
                    .sum()
            })
            .collect();
        let n: f64 = per_row.iter().sum();
        let mean = per_row
            .iter()
            .enumerate()
            .map(|(r, c)| c * r as f64)
            .sum::<f64>()
            / n;
        let var = per_row
            .iter()
            .enumerate()
            .map(|(r, c)| c * (r as f64 - mean).powi(2))
            .sum::<f64>()
            / n;
        var.sqrt()
    };
    let sa = spread(&run(&noise, 400, ParadeMode::Rgb, true));
    let sb = spread(&run(&noise, 400, ParadeMode::Rgb, false));
    assert!(sa < sb * 0.6, "spread on {sa} off {sb}");
    assert!(sa > 0.0);
    assert_eq!(lowpass_radius(400, 1), 2);
    assert_eq!(lowpass_radius(100, 1), 0);
    assert_eq!(lowpass_radius(2560, 2), 6);
}

#[test]
fn lowpass_only_touches_waveform_and_parade() {
    let img = busy(300, 20);
    let base = ScopeRequest {
        vectorscope: Some(32),
        histogram: Some(32),
        clip_stats: true,
        ..Default::default()
    };
    let a = analyze(&img, &base);
    let b = analyze(
        &img,
        &ScopeRequest {
            lowpass: true,
            ..base
        },
    );
    assert_eq!(a, b, "no waveform or parade requested: nothing to filter");
}

fn clip_image() -> DisplayImage {
    let px: [[f32; 3]; 5] = [
        [1.0, 1.0, 1.0],
        [0.0, 0.0, 0.0],
        [1.5, 0.5, -0.2],
        [f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
        [0.5, 0.5, 0.5],
    ];
    DisplayImage {
        width: 5,
        height: 2,
        rgb: px.iter().chain(px.iter()).flatten().copied().collect(),
    }
}

#[test]
fn clip_stats_exact_shares_min_max_and_odd_values() {
    let s = analyze(
        &clip_image(),
        &ScopeRequest {
            clip_stats: true,
            ..Default::default()
        },
    );
    let c = s.clipping.unwrap();
    assert_eq!(c.samples, 10);
    // R: high at white and 1.5; low at black and NaN.
    // G: high at white and +inf; low at black.
    // B: high at white; low at black, -0.2 and -inf.
    // Y: high at white and the +inf pixel; low at black only.
    assert_eq!(c.high, [4, 4, 2, 4]);
    assert_eq!(c.low, [4, 2, 6, 2]);
    assert!((c.high_share(0) - 0.4).abs() < 1e-6);
    assert!((c.low_share(2) - 0.6).abs() < 1e-6);
    assert!(c.any_clipped());
    assert_eq!(c.min[0], 0.0);
    assert_eq!(c.max[0], 1.5);
    assert_eq!(c.min[1], 0.0);
    assert_eq!(c.max[1], 1.0e6);
    assert_eq!(c.min[2], -1.0e6);
    assert_eq!(c.max[2], 1.0);
    assert_eq!(c.min[3], 0.0);
    assert!(c.max[3] > 6.0e5 && c.max[3].is_finite());
    // Nothing clipped: all zero, min/max of the data.
    let g = analyze(
        &flat(4, 4, [0.25, 0.5, 0.75]),
        &ScopeRequest {
            clip_stats: true,
            ..Default::default()
        },
    );
    let g = g.clipping.unwrap();
    assert!(!g.any_clipped());
    assert_eq!(
        (g.min[0], g.max[0], g.min[2], g.max[2]),
        (0.25, 0.25, 0.75, 0.75)
    );
}

#[test]
fn clip_stats_with_step_follow_the_sampled_pixels() {
    // Pixel (x, y) is clipped when x and y are both multiples of 2.
    let (w, h) = (8u32, 8u32);
    let mut rgb = Vec::new();
    for y in 0..h {
        for x in 0..w {
            rgb.extend(if x % 2 == 0 && y % 2 == 0 {
                [1.0; 3]
            } else {
                [0.5; 3]
            });
        }
    }
    let img = DisplayImage {
        width: w,
        height: h,
        rgb,
    };
    let stats = |step| {
        analyze(
            &img,
            &ScopeRequest {
                clip_stats: true,
                step,
                ..Default::default()
            },
        )
        .clipping
        .unwrap()
    };
    let full = stats(1);
    assert_eq!((full.samples, full.high[0]), (64, 16));
    let s2 = stats(2);
    assert_eq!((s2.samples, s2.high[0], s2.high[3]), (16, 16, 16));
    assert!((s2.high_share(0) - 1.0).abs() < 1e-6);
    // Step 3 samples x, y in {0, 3, 6}: clipped only where both are even.
    let s3 = stats(3);
    let sampled: Vec<(u32, u32)> = (0..h)
        .step_by(3)
        .flat_map(|y| (0..w).step_by(3).map(move |x| (x, y)))
        .collect();
    let want = sampled
        .iter()
        .filter(|(x, y)| x % 2 == 0 && y % 2 == 0)
        .count() as u64;
    assert_eq!((s3.samples, s3.high[1]), (sampled.len() as u64, want));
}

#[test]
fn lowpass_reports_edge_columns_of_the_unfiltered_pixels() {
    // One white spike pixel in column 100 of a grey image; the filter flattens it in the counts.
    let mut img = flat(400, 4, [0.5; 3]);
    for y in 0..4 {
        let i = (y * 400 + 100) * 3;
        img.rgb[i..i + 3].copy_from_slice(&[1.0, 1.0, 1.0]);
    }
    let req = ScopeRequest {
        waveform: Some((400, 64)),
        parade: Some((400, 64)),
        parade_mode: ParadeMode::Yrgb,
        ..Default::default()
    };
    assert!(analyze(&img, &req).unfiltered_edges.is_none());
    let s = analyze(
        &img,
        &ScopeRequest {
            lowpass: true,
            ..req
        },
    );
    let e = s.unfiltered_edges.unwrap();
    assert_eq!(e.waveform.len(), 400);
    for (c, f) in e.waveform.iter().enumerate() {
        assert_eq!(*f, if c == 100 { EDGE_HIGH } else { 0 }, "column {c}");
    }
    assert_eq!(e.parade.len(), 4);
    assert!(e.parade.iter().all(|p| p[100] == EDGE_HIGH && p[99] == 0));
    // The filtered counts no longer reach the top row there.
    let w = s.waveform.unwrap();
    assert_eq!(w.counts[100], 0);
    // Crushed blacks flag the bottom edge.
    let black = flat(400, 2, [0.0; 3]);
    let s = analyze(
        &black,
        &ScopeRequest {
            waveform: Some((10, 16)),
            lowpass: true,
            ..Default::default()
        },
    );
    assert!(s
        .unfiltered_edges
        .unwrap()
        .waveform
        .iter()
        .all(|&f| f == EDGE_LOW));
}
