//! Tests of the Colorize look: colours of the traces, the switch, and the guarantee that
//! "off" reproduces the pixels of the version before Colorize existed.

use crate::prepare::{prepare, PrepareSettings, PreparedScopes};
use crate::render::{
    colored_trace_to_rgba, glow_filter, glow_filter_src, histogram_to_rgba, layers_to_rgba_glow,
    trace_colour_of_mean, vectorscope_tint, vectorscope_to_rgba, Layer, HISTOGRAM_FILL_COLOUR,
    HISTOGRAM_FILL_PLAIN,
};
use crate::style::{ScopeStyle, WaveformMode, LUMA_TINT};
use crate::{ParadeMode, ScopePanel};
use rawgraph_scopes::{analyze, patterns, vectorscope_targets, DisplayImage, WaveformColors};

fn fnv(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn photo(width: u32, height: u32) -> DisplayImage {
    let mut rgb = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let (u, v) = (x as f32 / width as f32, y as f32 / height as f32);
            let n = ((x.wrapping_mul(2_654_435_761) ^ y.wrapping_mul(40_503)) & 0xFF) as f32 / 255.0 * 0.03;
            rgb.extend([
                (0.2 + 0.6 * u + n).clamp(0.0, 1.0),
                (0.3 + 0.4 * v + n).clamp(0.0, 1.0),
                (0.8 - 0.5 * u * v + n).clamp(0.0, 1.0),
            ]);
        }
    }
    DisplayImage { width, height, rgb }
}

fn run(img: &DisplayImage, st: &PrepareSettings) -> PreparedScopes {
    prepare(&analyze(img, &st.request()), st)
}

fn mono(st: PrepareSettings) -> PrepareSettings {
    PrepareSettings { colourise: false, ..st }
}

/// Hashes of the prepared pixels of the last version without Colorize (recorded before the
/// change with the then default `colourise: false`): the Colorize-off look must stay
/// byte-identical. `(picture, case, hashes of waveform pixels / edges, parade, vectorscope,
/// histogram counts)`.
const MONO_HASHES: &[(&str, &str, &str)] = &[
    ("bars", "default", "wf=5439c152dc685330/5791726c2c435025 pa=ecde75a7a159d6bd vs=9fdc164735d946d6 hi=9495f3aa9ce118a5"),
    ("bars", "noglow", "wf=54617415de902325/5791726c2c435025 pa=d0a37a510db16b25 vs=9fdc164735d946d6 hi=9495f3aa9ce118a5"),
    ("bars", "overlay", "wf=f35610236f88f7ff/0e1f529b67e06e25 pa=94917c6e47178066 vs=9fdc164735d946d6 hi=9495f3aa9ce118a5"),
    ("bars", "red", "wf=b0ea52a9ea4b73d1/e4815cbcb3af5f25 pa=ecde75a7a159d6bd vs=9fdc164735d946d6 hi=9495f3aa9ce118a5"),
    ("bars", "yrgb", "wf=5439c152dc685330/5791726c2c435025 pa=644e452754f3482d vs=9fdc164735d946d6 hi=9495f3aa9ce118a5"),
    ("bars", "ycbcr", "wf=731aead5ec357844/5791726c2c435025 pa=5b14d9596a591425 vs=9fdc164735d946d6 hi=9495f3aa9ce118a5"),
    ("bars", "step3", "wf=166e9297187ee114/d2fafae4291b97a7 pa=a972c57fe5358182 vs=3c4931db2c641866 hi=79574a3cd6c4eb69"),
    ("photo", "default", "wf=32d4b679f8eb17a4/7da144b97d054b25 pa=100b6d89289c1fa2 vs=4489eb484f07ea55 hi=dffb56dec19b44b2"),
    ("photo", "noglow", "wf=90a5fbcaae33c9df/7da144b97d054b25 pa=3529c80719b2b3a0 vs=4489eb484f07ea55 hi=dffb56dec19b44b2"),
    ("photo", "overlay", "wf=e27ca4bd515776a4/7da144b97d054b25 pa=aa80350c68123ed1 vs=4489eb484f07ea55 hi=dffb56dec19b44b2"),
    ("photo", "red", "wf=492fc7d9cdb422b1/7da144b97d054b25 pa=100b6d89289c1fa2 vs=4489eb484f07ea55 hi=dffb56dec19b44b2"),
    ("photo", "yrgb", "wf=32d4b679f8eb17a4/7da144b97d054b25 pa=e679724ea7d4a6c6 vs=4489eb484f07ea55 hi=dffb56dec19b44b2"),
    ("photo", "ycbcr", "wf=8774f16477c8b94e/7da144b97d054b25 pa=86ba5b47a719a207 vs=4489eb484f07ea55 hi=dffb56dec19b44b2"),
    ("photo", "step3", "wf=841db1c74b6e5646/7da144b97d054b25 pa=caa48ff430265273 vs=e12b875eef4a62d5 hi=c9ab212f33331137"),
];

fn hash_line(p: &PreparedScopes) -> String {
    let mut parts = Vec::new();
    if let Some(w) = &p.waveform {
        parts.push(format!("wf={:016x}/{:016x}", fnv(&w.rgba), fnv(&w.edges)));
    }
    if let Some(t) = p.parade_traces() {
        let all: Vec<u8> = t.iter().flat_map(|t| t.rgba.iter().copied()).collect();
        parts.push(format!("pa={:016x}", fnv(&all)));
    }
    if let Some(v) = &p.vectorscope {
        parts.push(format!("vs={:016x}", fnv(&v.rgba)));
    }
    if let Some(h) = &p.histogram {
        let all: Vec<u8> =
            [&h.r, &h.g, &h.b, &h.luma].iter().flat_map(|v| v.iter().flat_map(|c| c.to_le_bytes())).collect();
        parts.push(format!("hi={:016x}", fnv(&all)));
    }
    parts.join(" ")
}

#[test]
fn colorize_off_reproduces_the_previous_pixels() {
    let imgs = [("bars", patterns::color_bars(256, 64, 0.75)), ("photo", photo(512, 128))];
    let cases: Vec<(&str, PrepareSettings)> = vec![
        ("default", PrepareSettings::default()),
        ("noglow", PrepareSettings { glow: false, ..Default::default() }),
        ("overlay", PrepareSettings { waveform_mode: WaveformMode::Rgb, ..Default::default() }),
        ("red", PrepareSettings { waveform_mode: WaveformMode::Red, ..Default::default() }),
        ("yrgb", PrepareSettings { parade_mode: ParadeMode::Yrgb, ..Default::default() }),
        ("ycbcr", PrepareSettings { parade_mode: ParadeMode::YCbCr, lowpass: true, ..Default::default() }),
        ("step3", PrepareSettings { step: 3, gain: 2.0, ..Default::default() }),
    ];
    let mut n = 0;
    for (iname, img) in &imgs {
        for (cname, st) in &cases {
            let got = hash_line(&run(img, &mono(st.clone())));
            let (_, _, want) = MONO_HASHES.iter().find(|(i, c, _)| i == iname && c == cname).unwrap();
            assert_eq!(&got, want, "{iname} / {cname}");
            n += 1;
        }
    }
    assert_eq!(n, MONO_HASHES.len());
}

#[test]
fn colorize_is_on_by_default_everywhere() {
    assert!(ScopeStyle::default().colourise);
    assert!(PrepareSettings::default().colourise);
    assert!(ScopePanel::new().prepare_settings().colourise);
    // The switch is part of the data settings: flipping it asks for new data.
    let ctx = egui::Context::default();
    let mut p = ScopePanel::new();
    p.set_image(&ctx, &patterns::color_bars(64, 8, 0.75));
    assert!(!p.needs_refresh());
    p.style.colourise = false;
    assert!(p.needs_refresh());
    // The luma waveform asks `analyze` for colours only when Colorize is on.
    assert!(PrepareSettings::default().request().colorize);
    assert!(!mono(PrepareSettings::default()).request().colorize);
    assert!(!PrepareSettings { waveform_mode: WaveformMode::Red, ..Default::default() }.request().colorize);
}

/// Pixels `(x, y, [r, g, b, a])` of a prepared trace that carry light.
fn lit(w: usize, rgba: &[u8]) -> Vec<(usize, usize, [u8; 4])> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter(|(_, p)| p[3] > 0)
        .map(|(i, p)| (i % w, i / w, [p[0], p[1], p[2], p[3]]))
        .collect()
}

#[test]
fn waveform_trace_takes_the_colour_of_the_picture() {
    // Left half red at a low luma, right half green at a higher luma.
    let (w, h) = (512usize, 8usize);
    let mut rgb = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            rgb.extend(if x < w / 2 { [0.55f32, 0.05, 0.05] } else { [0.15, 0.75, 0.2] });
        }
    }
    let img = DisplayImage { width: w as u32, height: h as u32, rgb };
    let st = PrepareSettings {
        glow: false,
        parade: false,
        vectorscope: false,
        histogram: false,
        clipping: false,
        ..Default::default()
    };
    let p = run(&img, &st);
    let t = p.waveform.as_ref().unwrap();
    let px = lit(t.width, &t.rgba);
    assert_eq!(px.len(), 512, "one bin per column");
    let (mut red_rows, mut green_rows) = (Vec::new(), Vec::new());
    for (x, y, c) in &px {
        if *x < 256 {
            assert!(c[0] > 200 && c[1] < 60 && c[2] < 60, "left trace is red: {c:?}");
            red_rows.push(*y);
        } else {
            assert!(c[1] > 200 && c[0] < 90 && c[2] < 90, "right trace is green: {c:?}");
            green_rows.push(*y);
        }
    }
    // Green content has the higher luma, so its trace sits higher (smaller y).
    let mean = |v: &[usize]| v.iter().sum::<usize>() as f32 / v.len() as f32;
    assert!(mean(&green_rows) < mean(&red_rows) - 20.0);
    // Colorize off: the same positions in the single phosphor colour.
    let m = run(&img, &mono(st));
    let mt = m.waveform.as_ref().unwrap();
    let mpx = lit(mt.width, &mt.rgba);
    assert_eq!(mpx.len(), px.len());
    for ((x, y, c), (mx, my, mc)) in px.iter().zip(&mpx) {
        assert_eq!((x, y), (mx, my));
        assert_eq!(c[3], mc[3], "brightness (alpha) is the same: it follows the count");
        assert_eq!(mc[1], mc[3], "mono trace uses the phosphor tint");
    }
}

#[test]
fn waveform_brightness_follows_counts_not_colour() {
    // Dark saturated colour and a bright one produce the same brightness at equal counts.
    let mut counts = vec![0u32; 4 * 4];
    counts[5] = 10;
    counts[10] = 10;
    let mut colors = WaveformColors { columns: 4, levels: 4, rgb_sum: vec![0u32; 4 * 4 * 3] };
    // Bin index is row-major (row * columns + column); the sums are stored per column.
    let at = |bin: usize| ((bin % 4) * 4 + bin / 4) * 3;
    colors.rgb_sum[at(5)..at(5) + 3].copy_from_slice(&[10 * 40, 0, 0]); // dark red, mean (0.157, 0, 0)
    colors.rgb_sum[at(10)..at(10) + 3].copy_from_slice(&[10 * 255, 10 * 255, 10 * 255]); // white
    let px = colored_trace_to_rgba(&counts, &colors, 4, 4, 1.0, 10, false);
    assert_eq!(&px[5 * 4..5 * 4 + 4], &[255, 0, 0, 255], "dark red reads as full red");
    assert_eq!(&px[10 * 4..10 * 4 + 4], &[255, 255, 255, 255]);
    assert_eq!(&px[0..4], &[0, 0, 0, 0]);
    let mono = layers_to_rgba_glow(&[Layer { counts: &counts, tint: LUMA_TINT }], 4, 4, 1.0, 10, false);
    for i in 0..16 {
        assert_eq!(px[i * 4 + 3], mono[i * 4 + 3], "alpha {i}");
    }
}

#[test]
fn trace_colour_normalises_and_handles_empty_bins() {
    assert_eq!(trace_colour_of_mean(None), [1.0; 3]);
    assert_eq!(trace_colour_of_mean(Some([0.0; 3])), [1.0; 3]);
    assert_eq!(trace_colour_of_mean(Some([0.5; 3])), [1.0; 3]);
    let c = trace_colour_of_mean(Some([0.4, 0.2, 0.1]));
    assert_eq!(c, [1.0, 0.5, 0.25]);
}

#[test]
fn glow_texels_take_the_colour_of_their_source() {
    // One red bin in the middle of a 9 x 9 grid.
    let mut counts = vec![0u32; 81];
    counts[4 * 9 + 4] = 50;
    let mut colors = WaveformColors { columns: 9, levels: 9, rgb_sum: vec![0u32; 81 * 3] };
    let o = (4 * 9 + 4) * 3; // column 4, row 4
    colors.rgb_sum[o..o + 3].copy_from_slice(&[50 * 255, 50 * 40, 50 * 40]);
    let plain = colored_trace_to_rgba(&counts, &colors, 9, 9, 1.0, 50, false);
    let glow = colored_trace_to_rgba(&counts, &colors, 9, 9, 1.0, 50, true);
    let at = |p: &[u8], x: usize, y: usize| {
        let i = (y * 9 + x) * 4;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    };
    assert_eq!(at(&glow, 4, 4), at(&plain, 4, 4));
    let lifted = at(&glow, 4, 3);
    assert!(lifted[3] > 0 && lifted[3] < 255, "{lifted:?}");
    assert!(lifted[0] > lifted[1] && lifted[0] > lifted[2], "glow keeps the red hue: {lifted:?}");
    assert_eq!(at(&glow, 2, 4), [0, 0, 0, 0], "no smear two columns away");
    // The glow of the coloured trace is the glow of the mono trace.
    let b: Vec<f32> = counts.iter().map(|&c| if c > 0 { 1.0 } else { 0.0 }).collect();
    let (g, src) = glow_filter_src(&b, 9, 9);
    assert_eq!(g, glow_filter(&b, 9, 9));
    assert_eq!(src[3 * 9 + 4] as usize, 4 * 9 + 4);
    assert_eq!(src[0], 0);
}

#[test]
fn rgb_overlay_is_additive_and_neutral_for_equal_channels() {
    // A grey ramp has R = G = B everywhere: the overlay trace has to be neutral.
    let img = patterns::horizontal_ramp(512, 8);
    let on = PrepareSettings {
        waveform_mode: WaveformMode::Rgb,
        parade: false,
        vectorscope: false,
        histogram: false,
        ..Default::default()
    };
    for glow in [false, true] {
        let p = run(&img, &PrepareSettings { glow, ..on.clone() });
        let t = p.waveform.as_ref().unwrap();
        let px = lit(t.width, &t.rgba);
        assert!(px.len() > 400);
        for (x, y, c) in &px {
            assert!(c[0] == c[1] && c[1] == c[2], "grey ramp trace is neutral at {x},{y}: {c:?}");
        }
    }
    // Off: the previous tinted channels (not exactly neutral).
    let p = run(&img, &mono(on.clone()));
    let t = p.waveform.as_ref().unwrap();
    assert!(lit(t.width, &t.rgba).iter().any(|(_, _, c)| c[0] != c[2]));
    // A pure red picture traces red only; yellow adds green to it.
    let flat_rgb = |c: [f32; 3]| patterns::flat(64, 8, c);
    let red = run(&flat_rgb([0.8, 0.0, 0.0]), &on);
    let t = red.waveform.as_ref().unwrap();
    // (G and B sit at level 0, the bottom rows, and light those up in their colours.)
    let above_floor = |p: &&(usize, usize, [u8; 4])| p.1 < t.height - 6;
    let red_rows: Vec<_> = lit(t.width, &t.rgba).into_iter().filter(|p| above_floor(&p)).collect();
    assert!(!red_rows.is_empty());
    assert!(red_rows.iter().all(|(_, _, c)| c[0] > 0 && c[1] == 0 && c[2] == 0));
    let yel = run(&flat_rgb([0.8, 0.8, 0.0]), &PrepareSettings { glow: false, ..on.clone() });
    let t = yel.waveform.as_ref().unwrap();
    let px = lit(t.width, &t.rgba);
    let px: Vec<_> = px.into_iter().filter(|p| p.1 < t.height - 6).collect();
    assert!(!px.is_empty());
    assert!(px.iter().all(|(_, _, c)| c[0] == c[1] && c[2] == 0), "R and G overlap to yellow");
    // Different levels stay separate: red at 0.8, green at 0.3 gives a red row and a green row.
    let split = DisplayImage {
        width: 64,
        height: 4,
        rgb: (0..64 * 4).flat_map(|_| [0.8f32, 0.3, 0.0]).collect(),
    };
    let p = run(&split, &PrepareSettings { glow: false, ..on });
    let t = p.waveform.as_ref().unwrap();
    let px = lit(t.width, &t.rgba);
    assert!(px.iter().any(|(_, _, c)| c[0] > 0 && c[1] == 0));
    assert!(px.iter().any(|(_, _, c)| c[1] > 0 && c[0] == 0));
}

#[test]
fn vectorscope_tint_at_the_six_targets() {
    // (name, expected high channels, expected low channels) at the 75 % targets.
    let want: [(&str, [bool; 3]); 6] = [
        ("R", [true, false, false]),
        ("Mg", [true, false, true]),
        ("B", [false, false, true]),
        ("Cy", [false, true, true]),
        ("G", [false, true, false]),
        ("Yl", [true, true, false]),
    ];
    for ((name, cb, cr), (wname, high)) in vectorscope_targets().into_iter().zip(want) {
        assert_eq!(name, wname);
        let t = vectorscope_tint(cb * 0.75, cr * 0.75);
        for c in 0..3 {
            if high[c] {
                assert!(t[c] > 0.92, "{name}: channel {c} should be high: {t:?}");
            } else {
                assert!(t[c] < 0.45, "{name}: channel {c} should be low: {t:?}");
            }
        }
    }
    // The centre is white; saturation grows with the radius; the tint stays in 0..=1.
    assert!(vectorscope_tint(0.0, 0.0).iter().all(|&c| c > 0.85));
    let sat = |r: f32| {
        let t = vectorscope_tint(0.0, r);
        t[0] - t[1]
    };
    assert!(sat(0.05) < sat(0.15) && sat(0.15) < sat(0.3));
    for &(cb, cr) in &[(0.5, 0.5), (-0.5, -0.5), (0.5, -0.5), (-0.5, 0.5), (0.0, 0.0)] {
        assert!(vectorscope_tint(cb, cr).iter().all(|c| (0.0..=1.0).contains(c)));
    }
}

#[test]
fn vectorscope_pixels_are_tinted_by_position() {
    // A flat 75 % red picture lights the red target and nothing else.
    let img = patterns::flat(32, 8, [0.75, 0.0, 0.0]);
    let st = PrepareSettings { waveform: false, parade: false, histogram: false, clipping: false, ..Default::default() };
    let t = run(&img, &st).vectorscope.unwrap();
    let px = lit(t.width, &t.rgba);
    assert!(!px.is_empty());
    for (_, _, c) in &px {
        assert!(c[0] > 60 && c[1] < c[0] / 2 && c[2] < c[0] / 2, "red target is red: {c:?}");
    }
    // Colorize off: the previous single colour.
    let m = run(&img, &mono(st)).vectorscope.unwrap();
    assert_eq!(m.rgba, vectorscope_to_rgba(&analyze(&img, &PrepareSettings::default().request()).vectorscope.unwrap().counts, 256, 1.0, false));
    assert!(lit(m.width, &m.rgba).iter().all(|(_, _, c)| c[1] >= c[0]));
}

#[test]
fn histogram_overlaps_add_up_to_secondary_colours_and_white() {
    // One bin of 4: R only; R+G; R+G+B (the last columns).
    let bins = 4;
    let (r, g, b, l) = (vec![9u32, 9, 9, 0], vec![0u32, 9, 9, 0], vec![0u32, 0, 9, 0], vec![0u32; bins]);
    let px = histogram_to_rgba(&r, &g, &b, &l, 4, 8, true, false, 1.0);
    // Pixel in the lower part of each column (clear of the luma line along the bottom edge).
    let at = |x: usize| {
        let i = (4 * 4 + x) * 4;
        [px[i], px[i + 1], px[i + 2]]
    };
    let red = at(0);
    assert!(red[0] > 190 && red[1] < 60 && red[2] < 60, "{red:?}");
    let yellow = at(1);
    assert!(yellow[0] > 220 && yellow[1] > 220 && yellow[2] < 90, "R + G is yellow: {yellow:?}");
    let white = at(2);
    assert!(white.iter().all(|&v| v >= 250), "R + G + B is white: {white:?}");
    assert_eq!(at(3), [0, 0, 0]);
    // The plain look adds up to a light grey as well but keeps the old, duller fills.
    let plain = histogram_to_rgba(&r, &g, &b, &l, 4, 8, false, false, 1.0);
    let i = (4 * 4) * 4;
    assert_eq!(&plain[i..i + 3], &HISTOGRAM_FILL_PLAIN[0]);
    assert_eq!(&px[i..i + 3], &HISTOGRAM_FILL_COLOUR[0]);
    // The luminance is a thin light outline on top, not an area.
    let l2 = vec![0u32, 0, 0, 9];
    let px = histogram_to_rgba(&[0; 4], &[0; 4], &[0; 4], &l2, 8, 16, true, false, 1.0);
    let lit_px = lit(8, &px);
    assert!(!lit_px.is_empty());
    assert!(lit_px.iter().all(|(_, _, c)| c[0] == c[1] && c[1] == c[2]), "a grey outline");
    assert!(lit_px.len() < 8 * 16 / 3, "an outline, not a filled area");
    // Empty and short data never panic.
    assert!(histogram_to_rgba(&[], &[], &[], &[], 8, 8, true, true, 1.0).iter().all(|&v| v == 0));
    assert!(histogram_to_rgba(&r, &g, &b, &l, 0, 0, true, true, 1.0).is_empty());
}

#[test]
fn clipping_stats_and_modes_survive_colorize() {
    let img = patterns::color_bars(256, 16, 0.75);
    for colourise in [true, false] {
        let st = PrepareSettings { colourise, ..Default::default() };
        let p = run(&img, &st);
        let c = p.clipping.unwrap();
        assert_eq!(c.high, [0; 4]);
        assert!((c.low_share(0) - 0.5).abs() < 1e-6);
        assert!(p.mean_saturation.is_some());
        for mode in [ParadeMode::Rgb, ParadeMode::Yrgb, ParadeMode::YCbCr] {
            let st = PrepareSettings { colourise, parade_mode: mode, ..Default::default() };
            let p = run(&img, &st);
            assert_eq!(p.parade_traces().unwrap().len(), mode.planes());
            assert_eq!(p.waveform.as_ref().unwrap().rgba.len(), 512 * 256 * 4);
        }
    }
}

#[test]
fn colorize_with_lowpass_step_and_empty_pictures_does_not_panic() {
    let img = photo(300, 100);
    for (step, lowpass, mode) in [(1, true, WaveformMode::Luma), (3, true, WaveformMode::Luma), (2, false, WaveformMode::Rgb)] {
        let st = PrepareSettings { step, lowpass, waveform_mode: mode, ..Default::default() };
        let p = run(&img, &st);
        assert!(p.waveform.is_some());
    }
    let empty = DisplayImage { width: 0, height: 0, rgb: vec![] };
    let st = PrepareSettings::default();
    let _ = run(&empty, &st);
    // Data without colours (an older caller) falls back to the single colour.
    let set = {
        let mut s = analyze(&img, &st.request());
        s.waveform_colors = None;
        s
    };
    let p = prepare(&set, &st);
    let t = p.waveform.unwrap();
    assert!(lit(t.width, &t.rgba).iter().all(|(_, _, c)| c[1] >= c[0]));
}
