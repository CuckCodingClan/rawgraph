#![allow(clippy::chunks_exact_to_as_chunks)] // runtime-sized pixel slices; as_chunks would not read better here

//! GPU vs CPU reference tests. Without a GPU adapter every test prints "skipped: no GPU adapter"
//! and passes. Run with `-- --nocapture` to see the measured maxima.

use std::sync::OnceLock;

use rawgraph_core::ops;
use rawgraph_core::ImageBuf;
use rawgraph_gpu::{
    apply_point_ops, display_rgba8, display_texture, GpuContext, GpuError, GpuImage, GpuToneMap, PointOp,
};
use rawgraph_scopes::{DisplayTransform, ToneMap};

fn shared() -> Option<&'static GpuContext> {
    static CTX: OnceLock<Option<GpuContext>> = OnceLock::new();
    CTX.get_or_init(|| match GpuContext::new() {
        Ok(c) => {
            println!("adapter: {}", c.adapter_summary());
            Some(c)
        }
        Err(e) => {
            println!("no GPU: {e}");
            None
        }
    })
    .as_ref()
}

macro_rules! gpu {
    () => {
        match shared() {
            Some(c) => c,
            None => {
                println!("skipped: no GPU adapter");
                return;
            }
        }
    };
}

/// Wide value range: log ramp 2^-12..2^12 in R, -1.5..4.5 in G, saturated/HDR checker in B,
/// varying alpha.
fn test_image(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / (w - 1) as f32, y as f32 / (h - 1) as f32);
            let r = (-12.0 + 24.0 * fx).exp2();
            let g = (fy * 6.0) - 1.5;
            let b = match (x / 8 + y / 8) % 4 {
                0 => 0.0,
                1 => 0.05,
                2 => 1.0,
                _ => 16.0,
            };
            // A few saturated primaries / negative out-of-gamut pixels.
            let px = match (x * 7 + y * 13) % 41 {
                0 => [1.0, 0.0, 0.0],
                1 => [0.0, 12.0, 0.0],
                2 => [-0.3, 0.2, 0.9],
                3 => [100.0, 0.02, 0.001],
                _ => [r, g, b],
            };
            img.set_pixel(x, y, [px[0], px[1], px[2], 0.25 + 0.75 * fx]);
        }
    }
    img
}

/// Per-pixel error of `a` against reference `b`, relative to the pixel magnitude
/// max(|R|, |G|, |B|, 1) of the reference (alpha: relative to max(|alpha|, 1)).
fn rel_errors(a: &ImageBuf, b: &ImageBuf) -> Vec<f32> {
    assert_eq!((a.width(), a.height()), (b.width(), b.height()));
    a.data()
        .chunks_exact(4)
        .zip(b.data().chunks_exact(4))
        .map(|(p, q)| {
            let mag = q[0].abs().max(q[1].abs()).max(q[2].abs()).max(1.0);
            let mut worst = 0.0f32;
            for k in 0..4 {
                assert!(p[k].is_finite() == q[k].is_finite(), "finiteness differs: gpu {}, cpu {}", p[k], q[k]);
                if p[k].is_finite() {
                    let scale = if k == 3 { q[3].abs().max(1.0) } else { mag };
                    worst = worst.max((p[k] - q[k]).abs() / scale);
                }
            }
            worst
        })
        .collect()
}

/// True where the luminance sum cancels (|Y| < 2 % of the sum of its terms). Contrast and
/// highlights/shadows divide by or exponentiate Y, so rounding differences of the GPU (fused
/// multiply-add, which WGSL cannot forbid) are amplified there by more than 1/50.
fn ill_conditioned(px: &[f32]) -> bool {
    let w = rawgraph_core::color::AP1_LUMA;
    let y = w[0] * px[0] + w[1] * px[1] + w[2] * px[2];
    let terms = w[0] * px[0].abs() + w[1] * px[1].abs() + w[2] * px[2].abs();
    y.abs() < 0.02 * terms
}

/// Runs the CPU chain and flags every pixel that is ill-conditioned at an op that uses luminance.
fn cpu_chain(img: &ImageBuf, ops_list: &[PointOp]) -> (ImageBuf, Vec<bool>) {
    let mut out = img.clone();
    let mut flags = vec![false; (img.width() * img.height()) as usize];
    for op in ops_list {
        if matches!(op, PointOp::Contrast { .. } | PointOp::HighlightsShadows { .. }) {
            for (f, px) in flags.iter_mut().zip(out.data().chunks_exact(4)) {
                *f |= ill_conditioned(px);
            }
        }
        out = cpu_apply(&out, std::slice::from_ref(op));
    }
    (out, flags)
}

/// (max error over well-conditioned pixels, max error over ill-conditioned pixels)
fn split_max(errs: &[f32], flags: &[bool]) -> (f32, f32) {
    let (mut good, mut bad) = (0.0f32, 0.0f32);
    for (e, ill) in errs.iter().zip(flags) {
        if *ill {
            bad = bad.max(*e);
        } else {
            good = good.max(*e);
        }
    }
    (good, bad)
}

#[test]
fn adapter_is_reported() {
    let ctx = gpu!();
    let s = ctx.adapter_summary();
    println!("adapter_summary: {s}");
    assert!(s.contains("max texture"));
}

#[test]
fn upload_download_is_bit_exact() {
    let ctx = gpu!();
    // Odd widths exercise the 256-byte row padding of the readback.
    for (w, h) in [(37u32, 19u32), (16, 16), (257, 131), (1, 1)] {
        let mut img = ImageBuf::new(w, h);
        let specials = [1e-6f32, 16.0, 1e6, -1e-6, -3.5, -1e6, 0.0, 0.5, 1.0e-30, 3.0e38];
        for (i, v) in img.data_mut().iter_mut().enumerate() {
            *v = specials[i % specials.len()] * (1.0 + (i / specials.len()) as f32 * 1e-3);
        }
        img.set_pixel(0, 0, [1e-6, 16.0, 1e6, 0.5]);
        let gpu = GpuImage::upload(ctx, &img).unwrap();
        assert_eq!(gpu.size(), (w, h));
        let back = gpu.download(ctx).unwrap();
        let identical = img.data().iter().zip(back.data()).all(|(a, b)| a.to_bits() == b.to_bits());
        assert!(identical, "{w}x{h} differs");
    }
}

fn single_op_cases() -> Vec<(&'static str, PointOp)> {
    vec![
        ("gain 2.5", PointOp::Gain(2.5)),
        ("gain -0.75", PointOp::Gain(-0.75)),
        (
            "matrix",
            PointOp::Matrix([[0.9, 0.2, -0.1], [-0.15, 1.1, 0.05], [0.02, -0.3, 1.2]]),
        ),
        ("saturation 1.7", PointOp::Saturation(1.7)),
        ("saturation 0", PointOp::Saturation(0.0)),
        ("contrast +0.7", PointOp::Contrast { amount: 0.7, pivot: 0.18 }),
        ("contrast -1.2", PointOp::Contrast { amount: -1.2, pivot: 0.18 }),
        ("contrast 0 (identity)", PointOp::Contrast { amount: 0.0, pivot: 0.18 }),
        ("hs +0.6/-0.4", PointOp::HighlightsShadows { highlights: 0.6, shadows: -0.4 }),
        ("hs -1/+1", PointOp::HighlightsShadows { highlights: -1.0, shadows: 1.0 }),
        ("hs 0/0 (identity)", PointOp::HighlightsShadows { highlights: 0.0, shadows: 0.0 }),
    ]
}

fn cpu_apply(img: &ImageBuf, ops_list: &[PointOp]) -> ImageBuf {
    let mut out = img.clone();
    for op in ops_list {
        match op {
            PointOp::Gain(g) => ops::apply_gain(&mut out, *g),
            PointOp::Matrix(m) => ops::apply_matrix(&mut out, *m),
            PointOp::Saturation(s) => ops::saturation(&mut out, *s),
            PointOp::Contrast { amount, pivot } => ops::contrast(&mut out, *amount, *pivot),
            PointOp::HighlightsShadows { highlights, shadows } => {
                ops::highlights_shadows(&mut out, *highlights, *shadows)
            }
            PointOp::Cdl { slope, offset, power } => rawgraph_fx::color_wheels(
                &mut out,
                &rawgraph_core::CdlParams { slope: *slope, offset: *offset, power: *power },
            ),
            PointOp::ToneCurve { shadows, darks, lights, highlights } => {
                rawgraph_fx::tone_curve(&mut out, *shadows, *darks, *lights, *highlights)
            }
            PointOp::PointCurve { points, channel } => {
                if let Some(ch) = rawgraph_fx::CurveChannel::from_index(usize::from(*channel)) {
                    rawgraph_fx::point_curve(&mut out, points, ch)
                }
            }
            PointOp::HueSaturation { bands } => rawgraph_fx::hue_saturation(&mut out, bands),
            PointOp::Vignette { amount, midpoint, feather } => {
                rawgraph_fx::vignette(&mut out, *amount, *midpoint, *feather)
            }
            PointOp::WhitesBlacks { whites, blacks } => rawgraph_fx::whites_blacks(&mut out, *whites, *blacks),
            PointOp::Vibrance(a) => rawgraph_fx::vibrance(&mut out, *a),
        }
    }
    out
}

#[test]
fn each_point_op_matches_the_cpu() {
    let ctx = gpu!();
    let img = test_image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut worst = 0.0f32;
    for (name, op) in single_op_cases() {
        let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
        let (cpu, flags) = cpu_chain(&img, std::slice::from_ref(&op));
        let (err, ill) = split_max(&rel_errors(&out, &cpu), &flags);
        let n_ill = flags.iter().filter(|f| **f).count();
        println!("{name}: max relative error {err:.3e} (+ {ill:.3e} on {n_ill} ill-conditioned pixels)");
        assert!(err <= 1e-5, "{name}: {err}");
        assert!(ill <= 1e-3, "{name}: ill-conditioned pixels {ill}");
        worst = worst.max(err);
        // Alpha is untouched.
        for (a, b) in out.data().chunks_exact(4).zip(img.data().chunks_exact(4)) {
            assert_eq!(a[3].to_bits(), b[3].to_bits());
        }
    }
    println!("worst single-op error: {worst:.3e}");
}

#[test]
fn chain_of_all_five_ops_matches_the_cpu() {
    let ctx = gpu!();
    let img = test_image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let chain = vec![
        PointOp::Gain(1.6),
        PointOp::Matrix([[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]),
        PointOp::Saturation(1.3),
        PointOp::Contrast { amount: 0.45, pivot: 0.18 },
        PointOp::HighlightsShadows { highlights: -0.5, shadows: 0.35 },
    ];
    let out = apply_point_ops(ctx, &src, &chain).unwrap().download(ctx).unwrap();
    let (cpu, flags) = cpu_chain(&img, &chain);
    let (err, ill) = split_max(&rel_errors(&out, &cpu), &flags);
    let n_ill = flags.iter().filter(|f| **f).count();
    println!("five-op chain: max relative error {err:.3e} (+ {ill:.3e} on {n_ill} ill-conditioned pixels)");
    assert!(err <= 2e-5, "{err}");
    assert!(ill <= 1e-3, "ill-conditioned pixels {ill}");
    // The empty chain is a copy.
    let copy = apply_point_ops(ctx, &src, &[]).unwrap().download(ctx).unwrap();
    assert_eq!(copy, img);
}

fn tone_pairs() -> [(GpuToneMap, ToneMap); 3] {
    [
        (GpuToneMap::Clip, ToneMap::Clip),
        (GpuToneMap::SoftHighlights, ToneMap::SoftHighlights),
        (GpuToneMap::AcesFit, ToneMap::AcesFit),
    ]
}

#[test]
fn display_matches_the_cpu_within_one_code_value() {
    let ctx = gpu!();
    let img = test_image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    for (gt, ct) in tone_pairs() {
        for ev in [0.0f32, -2.0, 1.5] {
            let gpu = display_rgba8(ctx, &src, ev, gt).unwrap();
            let t = DisplayTransform { exposure_ev: ev, tone_map: ct };
            let cpu = t.apply_with_rgba8(&img).1;
            assert_eq!(cpu, t.apply(&img).to_rgba8(), "CPU paths disagree");
            assert_eq!(gpu.len(), cpu.len());
            let (mut max_diff, mut px_diff, mut ch_diff) = (0u8, 0usize, 0usize);
            for (g, c) in gpu.chunks_exact(4).zip(cpu.chunks_exact(4)) {
                let mut any = false;
                for k in 0..4 {
                    let d = g[k].abs_diff(c[k]);
                    max_diff = max_diff.max(d);
                    if d > 0 {
                        ch_diff += 1;
                        any = true;
                    }
                }
                px_diff += usize::from(any);
            }
            let n = gpu.len() / 4;
            println!(
                "{gt:?} ev {ev:+}: max diff {max_diff}, pixels differing by 1: {:.3} %, channels: {:.3} %",
                100.0 * px_diff as f64 / n as f64,
                100.0 * ch_diff as f64 / (4 * n) as f64
            );
            assert!(max_diff <= 1, "{gt:?} ev {ev}: max diff {max_diff}");
        }
    }
}

/// Extreme values per channel and mixed (8 x 8 pixels).
fn extreme_image() -> ImageBuf {
    const V: [f32; 13] = [
        0.0,
        1.0,
        16.0,
        4096.0,
        1e6,
        1e12,
        1e30,
        f32::MAX,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        -1.0,
        -1e30,
    ];
    let mut px: Vec<[f32; 3]> = Vec::new();
    for v in V {
        px.push([v, v, v]);
    }
    for v in V {
        px.push([v, 0.5, 0.5]);
    }
    for v in V {
        px.push([0.5, v, 0.5]);
    }
    for v in V {
        px.push([0.5, 0.5, v]);
    }
    px.push([f32::MAX, f32::MAX, 0.0]);
    px.push([f32::INFINITY, f32::INFINITY, 0.0]);
    px.push([f32::INFINITY, f32::NEG_INFINITY, 0.5]);
    px.push([f32::MAX, -1e30, 0.5]);
    px.push([1e30, 4096.0, 0.0]);
    px.push([f32::NAN, f32::MAX, 0.5]);
    px.push([f32::MAX, f32::NAN, f32::INFINITY]);
    px.push([1e12, 1e12, 1e-3]);
    px.push([f32::MAX, 0.0, 0.0]);
    px.push([0.0, f32::MAX, 0.0]);
    px.push([0.0, 0.0, f32::MAX]);
    px.push([f32::INFINITY, 0.0, 0.0]);
    assert!(px.len() <= 64);
    px.resize(64, [0.18, 0.18, 0.18]);
    let mut img = ImageBuf::new(8, 8);
    for (i, p) in px.iter().enumerate() {
        img.set_pixel((i % 8) as u32, (i / 8) as u32, [p[0], p[1], p[2], 1.0]);
    }
    img
}

#[test]
fn extreme_values_display_like_the_cpu() {
    let ctx = gpu!();
    let img = extreme_image();
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut worst = 0u8;
    let mut fails: Vec<String> = Vec::new();
    for (gt, ct) in tone_pairs() {
        let gpu = display_rgba8(ctx, &src, 0.0, gt).unwrap();
        let cpu = DisplayTransform { exposure_ev: 0.0, tone_map: ct }.apply_with_rgba8(&img).1;
        for (i, (g, c)) in gpu.chunks_exact(4).zip(cpu.chunks_exact(4)).enumerate() {
            let d = (0..4).map(|k| g[k].abs_diff(c[k])).max().unwrap_or(0);
            worst = worst.max(d);
            let p = img.pixel((i % 8) as u32, (i / 8) as u32);
            println!(
                "{ct:?} px {i:2} in [{:e} {:e} {:e}] cpu {:?} gpu {:?}{}",
                p[0],
                p[1],
                p[2],
                &c[..3],
                &g[..3],
                if d > 1 { "  <-- DIFF" } else { "" }
            );
            if d > 1 {
                fails.push(format!("{ct:?} pixel {i} {p:?}: cpu {c:?} gpu {g:?}"));
            }
            // Uniform pixels (first 13): huge / +Inf -> 255, negative / -Inf / NaN -> 0.
            if i < 13 {
                let v = p[0];
                let want = if v.is_nan() || v <= 0.0 {
                    Some(0u8)
                } else if v >= 1e6 {
                    Some(255)
                } else {
                    None
                };
                if let Some(w) = want {
                    if c[..3] != [w; 3] {
                        fails.push(format!("{ct:?} CPU pixel {i} {p:?} gives {c:?}, want {w}"));
                    }
                    if g[..3] != [w; 3] {
                        fails.push(format!("{ct:?} GPU pixel {i} {p:?} gives {g:?}, want {w}"));
                    }
                }
            }
        }
    }
    println!("worst extreme-value difference: {worst}");
    assert!(fails.is_empty(), "{} failures:
{}", fails.len(), fails.join("
"));
}

#[test]
fn display_texture_has_the_same_content_as_display_rgba8() {
    let ctx = gpu!();
    let img = test_image(65, 33);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let tex = display_texture(ctx, &src, 0.5, GpuToneMap::AcesFit).unwrap();
    assert_eq!(tex.format(), wgpu::TextureFormat::Rgba8Unorm);
    assert_eq!((tex.width(), tex.height()), (65, 33));
    let bytes = display_rgba8(ctx, &src, 0.5, GpuToneMap::AcesFit).unwrap();
    assert_eq!(bytes.len(), 65 * 33 * 4);
    assert!(bytes.chunks_exact(4).all(|p| p[3] == 255));
}

#[test]
fn nan_does_not_poison_neighbours() {
    let ctx = gpu!();
    let clean = test_image(64, 40);
    let mut nan = clean.clone();
    let mut zeroed = clean.clone();
    for (x, y, px) in [
        (10u32, 10u32, [f32::NAN, 0.5, 0.5, 1.0]),
        (11, 10, [f32::NAN, f32::NAN, f32::NAN, 1.0]),
        (30, 20, [0.2, f32::NAN, 1.0, 1.0]),
        (31, 20, [1.0, 1.0, f32::NAN, f32::NAN]),
    ] {
        nan.set_pixel(x, y, px);
        // The CPU display path turns NaN colour channels into 0.
        let z = px.map(|v| if v.is_nan() { 0.0 } else { v });
        zeroed.set_pixel(x, y, [z[0], z[1], z[2], 1.0]);
    }
    let gnan = GpuImage::upload(ctx, &nan).unwrap();
    let gzero = GpuImage::upload(ctx, &zeroed).unwrap();
    for (gt, ct) in tone_pairs() {
        let a = display_rgba8(ctx, &gnan, 0.0, gt).unwrap();
        let b = display_rgba8(ctx, &gzero, 0.0, gt).unwrap();
        assert_eq!(a, b, "{gt:?}: NaN input changed the output");
        let cpu = DisplayTransform { exposure_ev: 0.0, tone_map: ct }.apply_with_rgba8(&nan).1;
        let max = a.iter().zip(&cpu).map(|(g, c)| g.abs_diff(*c)).max().unwrap();
        assert!(max <= 1, "{gt:?}: differs from the CPU by {max}");
    }
}

#[test]
fn pipelines_are_cached_per_op_kind_sequence() {
    // Fresh context: the counter must not be disturbed by parallel tests.
    let Ok(ctx) = GpuContext::new() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let img = test_image(33, 17);
    let src = GpuImage::upload(&ctx, &img).unwrap();
    assert_eq!(ctx.pipelines_created(), 0);

    let a = [PointOp::Gain(1.0), PointOp::Saturation(1.2)];
    let b = [PointOp::Gain(3.0), PointOp::Saturation(0.4)];
    let ra = apply_point_ops(&ctx, &src, &a).unwrap().download(&ctx).unwrap();
    assert_eq!(ctx.pipelines_created(), 1);
    let rb = apply_point_ops(&ctx, &src, &b).unwrap().download(&ctx).unwrap();
    assert_eq!(ctx.pipelines_created(), 1, "same kinds, other parameters: no new pipeline");
    assert_ne!(ra, rb, "parameters must actually change the result");
    assert!(rel_errors(&rb, &cpu_apply(&img, &b)).iter().all(|e| *e <= 1e-5));

    // A different sequence (also a different order) needs its own pipeline, once.
    let c = [PointOp::Saturation(1.0), PointOp::Gain(2.0)];
    apply_point_ops(&ctx, &src, &c).unwrap();
    assert_eq!(ctx.pipelines_created(), 2);
    apply_point_ops(&ctx, &src, &c).unwrap();
    apply_point_ops(&ctx, &src, &a).unwrap();
    assert_eq!(ctx.pipelines_created(), 2);

    // The display shader is one pipeline for every exposure and tone map.
    display_rgba8(&ctx, &src, 0.0, GpuToneMap::Clip).unwrap();
    assert_eq!(ctx.pipelines_created(), 3);
    display_rgba8(&ctx, &src, 2.0, GpuToneMap::AcesFit).unwrap();
    display_texture(&ctx, &src, -1.0, GpuToneMap::SoftHighlights).unwrap();
    assert_eq!(ctx.pipelines_created(), 3);
}

#[test]
fn error_paths() {
    let ctx = gpu!();
    // Zero-sized image.
    let empty = ImageBuf::new(0, 0);
    assert_eq!(GpuImage::upload(ctx, &empty).err(), Some(GpuError::EmptyImage { width: 0, height: 0 }));
    let thin = ImageBuf::new(0, 5);
    assert!(matches!(GpuImage::upload(ctx, &thin), Err(GpuError::EmptyImage { .. })));

    // Larger than the maximum texture dimension: documented error, checked before any allocation.
    let max = ctx.max_texture_dimension();
    println!("max_texture_dimension_2d = {max}");
    let too_wide = ImageBuf::new(max + 1, 1);
    match GpuImage::upload(ctx, &too_wide) {
        Err(GpuError::TooLarge { width, height, max_dimension }) => {
            assert_eq!((width, height, max_dimension), (max + 1, 1, max));
            let msg = GpuError::TooLarge { width, height, max_dimension }.to_string();
            assert!(msg.contains("maximum texture dimension"), "{msg}");
        }
        other => panic!("expected TooLarge, got {:?}", other.map(|i| i.size())),
    }
    let too_tall = ImageBuf::new(1, max + 1);
    assert!(matches!(GpuImage::upload(ctx, &too_tall), Err(GpuError::TooLarge { .. })));
}

#[test]
fn into_variants_reuse_targets_and_check_sizes() {
    let ctx = gpu!();
    let img = test_image(65, 33);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let dst = GpuImage::upload(ctx, &ImageBuf::new(65, 33)).unwrap();
    let chain = [PointOp::Gain(2.0), PointOp::Saturation(0.5)];
    rawgraph_gpu::apply_point_ops_into(ctx, &src, &dst, &chain).unwrap();
    let a = dst.download(ctx).unwrap();
    let b = apply_point_ops(ctx, &src, &chain).unwrap().download(ctx).unwrap();
    assert_eq!(a, b);

    let wrong = GpuImage::upload(ctx, &ImageBuf::new(64, 33)).unwrap();
    assert!(matches!(
        rawgraph_gpu::apply_point_ops_into(ctx, &src, &wrong, &chain),
        Err(GpuError::SizeMismatch { .. })
    ));
    let tex = display_texture(ctx, &src, 0.0, GpuToneMap::Clip).unwrap();
    rawgraph_gpu::display_into(ctx, &src, &tex, 1.0, GpuToneMap::AcesFit).unwrap();
    let small = display_texture(ctx, &wrong, 0.0, GpuToneMap::Clip).unwrap();
    assert!(matches!(
        rawgraph_gpu::display_into(ctx, &src, &small, 1.0, GpuToneMap::Clip),
        Err(GpuError::SizeMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// Wave 6: DisplayTarget / render_into, display_samples, out-of-memory, limits.
// ---------------------------------------------------------------------------------------------

use rawgraph_gpu::{display_samples, render_into, DisplayTarget};
use rawgraph_scopes::{analyze, DisplayImage, ScopeRequest};

/// Well-conditioned chain (no luminance division), so the GPU/CPU difference stays far below one code.
fn gentle_chain() -> Vec<PointOp> {
    vec![
        PointOp::Gain(1.4),
        PointOp::Matrix([[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]),
        PointOp::Saturation(1.2),
    ]
}

fn max_code_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

#[test]
fn render_into_matches_display_path_and_cpu() {
    let ctx = gpu!();
    let img = test_image(257, 131);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let scratch = GpuImage::new_empty(ctx, 257, 131).unwrap();
    let target = DisplayTarget::new(ctx, 257, 131).unwrap();
    assert_eq!(target.size(), (257, 131));
    assert_eq!(target.texture().format(), wgpu::TextureFormat::Rgba8Unorm);
    let chain = gentle_chain();
    let cpu_chain_img = cpu_apply(&img, &chain);
    for ops_case in [&[][..], &chain[..]] {
        for (gt, ct) in tone_pairs() {
            for ev in [-2.0f32, 0.0, 1.5] {
                render_into(ctx, &base, &scratch, &target, ops_case, ev, gt).unwrap();
                let got = target.read_rgba8(ctx).unwrap();
                // Same shaders as the existing two-step path: bit-identical.
                let two_step = if ops_case.is_empty() {
                    display_rgba8(ctx, &base, ev, gt).unwrap()
                } else {
                    display_rgba8(ctx, &apply_point_ops(ctx, &base, ops_case).unwrap(), ev, gt).unwrap()
                };
                assert_eq!(got, two_step, "{gt:?} ev {ev} ops {}", ops_case.len());
                // And within one code value of the CPU.
                let cpu_src = if ops_case.is_empty() { &img } else { &cpu_chain_img };
                let cpu = DisplayTransform { exposure_ev: ev, tone_map: ct }.apply_with_rgba8(cpu_src).1;
                let d = max_code_diff(&got, &cpu);
                println!("render_into {gt:?} ev {ev:+} ops {}: max diff to CPU {d}", ops_case.len());
                assert!(d <= 1, "{gt:?} ev {ev} ops {}: {d}", ops_case.len());
            }
        }
    }
}

#[test]
fn render_into_errors_and_empty_ops_leave_scratch_alone() {
    let ctx = gpu!();
    let img = test_image(65, 33);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let scratch = GpuImage::upload(ctx, &ImageBuf::new(65, 33)).unwrap();
    let target = DisplayTarget::new(ctx, 65, 33).unwrap();
    let chain = gentle_chain();
    // Scratch is not needed (and untouched) without ops, even if it is the base itself.
    render_into(ctx, &base, &scratch, &target, &[], 0.0, GpuToneMap::Clip).unwrap();
    render_into(ctx, &base, &base, &target, &[], 0.0, GpuToneMap::Clip).unwrap();
    assert_eq!(scratch.download(ctx).unwrap(), ImageBuf::new(65, 33));
    // With ops the scratch must differ from the base and have its size.
    assert!(matches!(
        render_into(ctx, &base, &base, &target, &chain, 0.0, GpuToneMap::Clip),
        Err(GpuError::InvalidArgument(_))
    ));
    let small = GpuImage::new_empty(ctx, 64, 33).unwrap();
    assert!(matches!(
        render_into(ctx, &base, &small, &target, &chain, 0.0, GpuToneMap::Clip),
        Err(GpuError::SizeMismatch { .. })
    ));
    let other = DisplayTarget::new(ctx, 64, 33).unwrap();
    assert!(matches!(
        render_into(ctx, &base, &scratch, &other, &chain, 0.0, GpuToneMap::Clip),
        Err(GpuError::SizeMismatch { .. })
    ));
}

#[test]
fn render_into_creates_no_pipelines_or_allocations_after_the_first_call() {
    // Fresh context: the counters must not be disturbed by parallel tests.
    let Ok(ctx) = GpuContext::new() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let img = test_image(129, 65);
    let base = GpuImage::upload(&ctx, &img).unwrap();
    let scratch = GpuImage::new_empty(&ctx, 129, 65).unwrap();
    let target = DisplayTarget::new(&ctx, 129, 65).unwrap();
    let a = [PointOp::Gain(1.0), PointOp::Saturation(1.2), PointOp::Contrast { amount: 0.3, pivot: 0.18 }];
    let b = [PointOp::Gain(2.0), PointOp::Saturation(0.7), PointOp::Contrast { amount: -0.5, pivot: 0.18 }];
    render_into(&ctx, &base, &scratch, &target, &a, 0.0, GpuToneMap::Clip).unwrap();
    // Without ops (display reads the base directly) the display uniform size is the same one.
    render_into(&ctx, &base, &scratch, &target, &[], 1.0, GpuToneMap::SoftHighlights).unwrap();
    ctx.wait_idle();
    let (p0, a0) = (ctx.pipelines_created(), ctx.allocations_created());
    println!("after the first calls: {p0} pipelines, {a0} allocations");
    assert_eq!(p0, 2, "one op pipeline and one display pipeline");
    for i in 0..5 {
        let ops = if i % 2 == 0 { &b } else { &a };
        render_into(&ctx, &base, &scratch, &target, ops, i as f32 * 0.25, GpuToneMap::AcesFit).unwrap();
    }
    render_into(&ctx, &base, &scratch, &target, &[], 2.0, GpuToneMap::Clip).unwrap();
    ctx.wait_idle();
    assert_eq!(ctx.pipelines_created(), p0, "no new pipeline");
    assert_eq!(ctx.allocations_created(), a0, "no new texture or buffer");
}

/// Renders `view` through a copy of egui-wgpu 0.37's fragment logic (`egui.wgsl`: sample, multiply by a white
/// vertex colour, treat the texel as gamma values; for an sRGB framebuffer convert to linear at the end) into
/// a 1:1 framebuffer and returns its bytes. `srgb_framebuffer` selects `fs_main_linear_framebuffer`
/// (`Rgba8UnormSrgb` target) instead of `fs_main_gamma_framebuffer` (`Rgba8Unorm` target).
fn simulate_egui(ctx: &GpuContext, view: &wgpu::TextureView, (w, h): (u32, u32), srgb_framebuffer: bool) -> Vec<u8> {
    assert_eq!((w * 4) % 256, 0, "test sizes keep rows 256-byte aligned");
    let device = ctx.device();
    let wgsl = "
@group(0) @binding(0) var r_tex_color: texture_2d<f32>;
@group(0) @binding(1) var r_tex_sampler: sampler;
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs_main(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    var o: VOut;
    o.pos = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = vec2<f32>((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return o;
}
fn linear_from_gamma_rgb(srgb: vec3<f32>) -> vec3<f32> {
    let cutoff = srgb < vec3<f32>(0.04045);
    let lower = srgb / vec3<f32>(12.92);
    let higher = pow((srgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}
@fragment fn fs_gamma(in: VOut) -> @location(0) vec4<f32> {
    let tex_gamma = textureSample(r_tex_color, r_tex_sampler, in.uv);
    return vec4<f32>(1.0, 1.0, 1.0, 1.0) * tex_gamma;
}
@fragment fn fs_linear(in: VOut) -> @location(0) vec4<f32> {
    let tex_gamma = textureSample(r_tex_color, r_tex_sampler, in.uv);
    let c = vec4<f32>(1.0, 1.0, 1.0, 1.0) * tex_gamma;
    return vec4<f32>(linear_from_gamma_rgb(c.rgb), c.a);
}";
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("egui simulation"),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    // Same binding layout as egui's `texture_bind_group_layout` (filterable float, filtering sampler).
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    multisampled: false,
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let fb_format = if srgb_framebuffer { wgpu::TextureFormat::Rgba8UnormSrgb } else { wgpu::TextureFormat::Rgba8Unorm };
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some(if srgb_framebuffer { "fs_linear" } else { "fs_gamma" }),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: fb_format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    });
    let fb = device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: fb_format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(w * 4 * h),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let fb_view = fb.create_view(&Default::default());
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &fb_view,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: &fb, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    ctx.queue().submit(std::iter::once(enc.finish()));
    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let out = slice.get_mapped_range().unwrap().to_vec();
    staging.unmap();
    out
}

#[test]
fn egui_sampling_of_the_display_target_shows_the_cpu_code_values() {
    let ctx = gpu!();
    let (w, h) = (64u32, 32u32);
    let img = test_image(w, h);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let scratch = GpuImage::new_empty(ctx, w, h).unwrap();
    let target = DisplayTarget::new(ctx, w, h).unwrap();
    // The view egui gets is Rgba8Unorm (what egui-wgpu documents and its own textures use).
    assert_eq!(target.egui_view().texture().format(), wgpu::TextureFormat::Rgba8Unorm);
    for (gt, ct) in tone_pairs() {
        render_into(ctx, &base, &scratch, &target, &gentle_chain(), 0.5, gt).unwrap();
        let stored = target.read_rgba8(ctx).unwrap();
        let cpu = DisplayTransform { exposure_ev: 0.5, tone_map: ct }
            .apply_with_rgba8(&cpu_apply(&img, &gentle_chain()))
            .1;
        assert!(max_code_diff(&stored, &cpu) <= 1);
        // Gamma framebuffer (egui's preferred Rgba8Unorm / Bgra8Unorm): the shown value equals the stored code.
        let shown = simulate_egui(ctx, target.egui_view(), (w, h), false);
        assert_eq!(shown, stored, "{gt:?}: gamma framebuffer must show the stored codes unchanged");
        // sRGB framebuffer (fs_main_linear_framebuffer, hardware re-encodes): same code values, +-1 from rounding.
        let shown_srgb = simulate_egui(ctx, target.egui_view(), (w, h), true);
        let d = max_code_diff(&shown_srgb, &stored);
        println!("{gt:?}: sRGB framebuffer round trip max diff {d}");
        assert!(d <= 1);
        let d_cpu = max_code_diff(&shown, &cpu);
        println!("{gt:?}: shown vs CPU code values, max diff {d_cpu}");
        assert!(d_cpu <= 1);
    }

    // Negative control: the same bytes behind an Rgba8UnormSrgb view are decoded by the hardware before the shader
    // and come out much too dark, which is why the registered view must be Rgba8Unorm.
    let bytes = target.read_rgba8(ctx).unwrap();
    let tex = ctx.device().create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[wgpu::TextureFormat::Rgba8UnormSrgb],
    });
    ctx.queue().write_texture(
        wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        &bytes,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    let srgb_view = tex.create_view(&wgpu::TextureViewDescriptor {
        format: Some(wgpu::TextureFormat::Rgba8UnormSrgb),
        ..Default::default()
    });
    let wrong = simulate_egui(ctx, &srgb_view, (w, h), false);
    let max_drop = bytes.iter().zip(&wrong).map(|(c, w)| i32::from(*c) - i32::from(*w)).max().unwrap_or(0);
    println!("largest code value lost behind an sRGB view: {max_drop}");
    assert!(max_drop > 60, "an sRGB view must visibly darken mid-tones, max drop {max_drop}");
}

/// Max abs difference between the GPU samples and the CPU `DisplayTransform::apply` pixels at the same coordinates.
fn samples_vs_cpu(ctx: &GpuContext, img: &ImageBuf, src: &GpuImage, ev: f32, gt: GpuToneMap, ct: ToneMap, step: u32) -> f32 {
    let t = DisplayTransform { exposure_ev: ev, tone_map: ct };
    let cpu = t.apply(img);
    let got = display_samples(ctx, src, ev, gt, step).unwrap();
    let s = step as usize;
    let (ow, oh) = ((img.width() as usize).div_ceil(s), (img.height() as usize).div_ceil(s));
    assert_eq!((got.width as usize, got.height as usize), (ow, oh));
    assert_eq!(got.rgb.len(), ow * oh * 3);
    let mut worst = 0.0f32;
    for yi in 0..oh {
        for xi in 0..ow {
            let c = ((yi * s) * img.width() as usize + xi * s) * 3;
            for k in 0..3 {
                let g = got.rgb[(yi * ow + xi) * 3 + k];
                worst = worst.max((g - cpu.rgb[c + k]).abs());
            }
        }
    }
    worst
}

#[test]
fn display_samples_match_the_cpu_display_image() {
    let ctx = gpu!();
    // 101 x 67: not divisible by 2, 3 or 4.
    let img = test_image(101, 67);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut worst_all = 0.0f32;
    for (gt, ct) in tone_pairs() {
        for ev in [-2.0f32, 0.0, 1.5] {
            for step in [1u32, 2, 3, 4] {
                let worst = samples_vs_cpu(ctx, &img, &src, ev, gt, ct, step);
                worst_all = worst_all.max(worst);
                println!("display_samples {gt:?} ev {ev:+} step {step}: max abs difference {worst:.3e}");
                assert!(worst <= 1e-4, "{gt:?} ev {ev} step {step}: {worst}");
            }
        }
    }
    println!("display_samples: worst difference over all cases {worst_all:.3e}");
    // Values above 1.0 are preserved for Clip (and SoftHighlights), also on the decimated image.
    let clip = display_samples(ctx, &src, 1.5, GpuToneMap::Clip, 2).unwrap();
    let max = clip.rgb.iter().copied().fold(f32::MIN, f32::max);
    let min = clip.rgb.iter().copied().fold(f32::MAX, f32::min);
    println!("Clip ev +1.5 step 2: value range {min} .. {max}");
    assert!(max > 1.5, "Clip must keep values above 1.0, max {max}");
    assert!(min < 0.0, "Clip must keep negative values (mirrored OETF), min {min}");
    let soft = display_samples(ctx, &src, 1.5, GpuToneMap::SoftHighlights, 1).unwrap();
    assert!(soft.rgb.iter().copied().fold(f32::MIN, f32::max) > 1.0);
    // step 0 counts as 1.
    assert_eq!(display_samples(ctx, &src, 0.0, GpuToneMap::Clip, 0).unwrap().width, 101);
}

#[test]
fn display_samples_reuse_their_targets() {
    let Ok(ctx) = GpuContext::new() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let img = test_image(101, 67);
    let src = GpuImage::upload(&ctx, &img).unwrap();
    display_samples(&ctx, &src, 0.0, GpuToneMap::Clip, 2).unwrap();
    let (p, a) = (ctx.pipelines_created(), ctx.allocations_created());
    for ev in [0.5f32, 1.0, -1.0] {
        display_samples(&ctx, &src, ev, GpuToneMap::AcesFit, 2).unwrap();
    }
    assert_eq!((ctx.pipelines_created(), ctx.allocations_created()), (p, a), "same size: nothing new");
    display_samples(&ctx, &src, 0.0, GpuToneMap::Clip, 3).unwrap();
    assert!(ctx.allocations_created() > a, "a new output size needs new targets");
}

/// (number of differing bins, number of samples that moved from one bin to another)
fn count_differences(a: &[u32], b: &[u32]) -> (usize, u64) {
    assert_eq!(a.len(), b.len());
    let bins = a.iter().zip(b).filter(|(x, y)| x != y).count();
    let moved: u64 = a.iter().zip(b).map(|(x, y)| u64::from(x.abs_diff(*y))).sum::<u64>() / 2;
    (bins, moved)
}

#[test]
fn scopes_on_decimated_gpu_samples_agree_with_the_cpu_scopes() {
    let ctx = gpu!();
    let img = test_image(101, 67);
    let src = GpuImage::upload(ctx, &img).unwrap();
    for (gt, ct) in [(GpuToneMap::Clip, ToneMap::Clip), (GpuToneMap::AcesFit, ToneMap::AcesFit)] {
        let t = DisplayTransform { exposure_ev: 0.5, tone_map: ct };
        let cpu_display: DisplayImage = t.apply(&img);
        for step in [1usize, 2, 3] {
            let req = ScopeRequest {
                waveform: Some((50, 64)),
                parade: Some((50, 64)),
                vectorscope: Some(64),
                histogram: Some(256),
                clip_stats: true,
                step,
                ..Default::default()
            };
            let cpu = analyze(&cpu_display, &req);
            let gpu_img = display_samples(ctx, &src, 0.5, gt, step as u32).unwrap();
            let gpu = analyze(&gpu_img, &ScopeRequest { step: 1, ..req });
            assert_eq!(cpu.samples, gpu.samples, "{gt:?} step {step}: sample count");

            let (hc, hg) = (cpu.histogram.as_ref().unwrap(), gpu.histogram.as_ref().unwrap());
            let mut bins = 0;
            let mut moved = 0;
            for (a, b) in [(&hc.r, &hg.r), (&hc.g, &hg.g), (&hc.b, &hg.b), (&hc.luma, &hg.luma)] {
                let (n, m) = count_differences(a, b);
                bins += n;
                moved += m;
            }
            let (vc, vg) = (cpu.vectorscope.as_ref().unwrap(), gpu.vectorscope.as_ref().unwrap());
            let (vbins, vmoved) = count_differences(&vc.counts, &vg.counts);
            let (cc, cg) = (cpu.clipping.unwrap(), gpu.clipping.unwrap());
            println!(
                "{gt:?} step {step}: samples {}; histogram differing bins {bins} (samples moved {moved} of {}); vectorscope bins {vbins} (moved {vmoved}); clip high cpu {:?} gpu {:?}, low cpu {:?} gpu {:?}",
                cpu.samples,
                4 * cpu.samples,
                cc.high,
                cg.high,
                cc.low,
                cg.low
            );
            // The CPU encodes through a 4096-entry table (error <= 1e-4); the GPU is exact. A sample can only change
            // bin when it sits within 1e-4 of a bin edge.
            assert!(moved * 100 <= 4 * cpu.samples * 2, "histogram: {moved} samples moved");
            assert!(vmoved * 100 <= cpu.samples * 5, "vectorscope: {vmoved} samples moved");
            for k in 0..4 {
                assert!(cc.high[k].abs_diff(cg.high[k]) * 100 <= cpu.samples, "clip high {k}");
                assert!(cc.low[k].abs_diff(cg.low[k]) * 100 <= cpu.samples, "clip low {k}");
            }
            let (wc, wg) = (cpu.waveform.as_ref().unwrap(), gpu.waveform.as_ref().unwrap());
            let (n, m) = count_differences(&wc.counts, &wg.counts);
            if step == 1 {
                // Identical geometry: waveform columns map the same way.
                println!("{gt:?} step 1 waveform: differing bins {n}, moved {m}");
                assert!(m * 100 <= cpu.samples * 5);
            } else {
                // The decimated image has width ceil(w / step), so `x * columns / width` maps a sample to a slightly
                // different column than `analyze` does on the full picture; the total per waveform stays the same.
                println!("{gt:?} step {step} waveform: differing bins {n}, moved {m} (column mapping)");
                assert_eq!(wc.counts.iter().sum::<u32>(), wg.counts.iter().sum::<u32>());
            }
        }
    }
}

#[test]
fn limit_errors_leave_the_context_usable() {
    let ctx = gpu!();
    assert!(!ctx.is_device_lost());
    let max = ctx.max_texture_dimension();
    println!("limits_ok_for(max, max): {}", ctx.limits_ok_for(max, max));
    assert!(ctx.limits_ok_for(640, 480));
    assert!(!ctx.limits_ok_for(0, 10));
    assert!(!ctx.limits_ok_for(max + 1, 10));
    assert!(!ctx.limits_ok_for(10, max + 1));

    // Sizes the limits forbid are reported before anything is allocated.
    assert!(matches!(DisplayTarget::new(ctx, max + 1, 4), Err(GpuError::TooLarge { .. })));
    assert!(matches!(DisplayTarget::new(ctx, 0, 4), Err(GpuError::EmptyImage { .. })));
    assert!(matches!(GpuImage::new_empty(ctx, 4, u32::MAX), Err(GpuError::TooLarge { .. })));
    assert!(matches!(GpuImage::new_empty(ctx, 0, 0), Err(GpuError::EmptyImage { .. })));

    // Still usable afterwards, for smaller work.
    assert!(!ctx.is_device_lost());
    let img = test_image(33, 17);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let target = DisplayTarget::new(ctx, 33, 17).unwrap();
    let scratch = GpuImage::new_empty(ctx, 33, 17).unwrap();
    render_into(ctx, &src, &scratch, &target, &gentle_chain(), 0.0, GpuToneMap::Clip).unwrap();
    assert_eq!(target.read_rgba8(ctx).unwrap().len(), 33 * 17 * 4);
    assert!(display_samples(ctx, &src, 0.0, GpuToneMap::Clip, 2).is_ok());
}

// ---------------------------------------------------------------------------------------------
// Wave 7: CDL, tone curve, point curve, hue/saturation, vignette.
// ---------------------------------------------------------------------------------------------

/// `test_image` plus rows of exact neutrals, near-neutrals (tiny chroma) and a hue sweep at several
/// chroma levels and brightnesses (needs `w >= 64`, `h >= 8`).
fn wide_image(w: u32, h: u32) -> ImageBuf {
    let mut img = test_image(w, h);
    for x in 0..w {
        let f = x as f32 / (w - 1) as f32;
        let grey = (-8.0 + 16.0 * f).exp2();
        img.set_pixel(x, 0, [grey, grey, grey, 1.0]); // exact neutrals
        img.set_pixel(x, 1, [grey * (1.0 + 1e-4), grey, grey * (1.0 - 1e-4), 1.0]); // almost neutral
        let ang = f * std::f32::consts::TAU;
        let (cr, cg, cb) = (ang.cos(), (ang - 2.0944).cos(), (ang - 4.1888).cos());
        for (row, (chroma, level)) in [(2u32, (0.6f32, 0.18f32)), (3, (0.15, 0.18)), (4, (0.9, 3.0)), (5, (0.5, 0.01))] {
            img.set_pixel(x, row, [level * (1.0 + chroma * cr), level * (1.0 + chroma * cg), level * (1.0 + chroma * cb), 1.0]);
        }
        // Negative and out-of-gamut values.
        img.set_pixel(x, 6, [-0.1 * f, 0.4, 0.2 * (1.0 - f), 1.0]);
        img.set_pixel(x, 7, [-0.5, -0.5, -0.5, 1.0]);
    }
    img
}

fn s_curve() -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [0.25, 0.15], [0.6, 0.72], [1.0, 1.0]]
}

fn new_op_cases() -> Vec<(&'static str, PointOp)> {
    let bands_a = [
        [0.3, 0.2, 0.1],
        [-0.5, 0.4, -0.3],
        [0.9, -0.6, 0.5],
        [0.0, 0.7, -0.8],
        [-1.0, -0.2, 0.2],
        [0.4, 1.0, 0.9],
    ];
    let mut hue_only = [[0.0; 3]; 6];
    hue_only[1][0] = 1.0;
    let mut sat_grey = [[0.0; 3]; 6];
    for b in &mut sat_grey {
        b[1] = -1.0;
    }
    let mut light = [[0.0; 3]; 6];
    light[4][2] = 1.0;
    light[5][2] = -1.0;
    vec![
        ("cdl grade", PointOp::Cdl { slope: [1.2, 0.9, 1.1], offset: [0.02, -0.01, 0.05], power: [0.9, 1.1, 1.3] }),
        ("cdl negative offset", PointOp::Cdl { slope: [1.0, 2.0, 0.5], offset: [-0.2, -0.5, 0.0], power: [1.0, 0.6, 2.2] }),
        ("cdl power clamped low/high", PointOp::Cdl { slope: [1.0, 1.0, 0.2], offset: [0.0; 3], power: [0.0, -3.0, 50.0] }),
        ("cdl overflow capped", PointOp::Cdl { slope: [1e30, 1.0, 1.0], offset: [0.0; 3], power: [20.0, 20.0, 20.0] }),
        ("cdl non-finite params", PointOp::Cdl { slope: [f32::NAN, 1.2, 1.0], offset: [0.0, f32::INFINITY, 0.01], power: [1.0, 1.0, f32::NAN] }),
        ("cdl power only", PointOp::Cdl { slope: [1.0; 3], offset: [0.0; 3], power: [0.8, 1.0, 1.5] }),
        ("tone shadows +0.5", PointOp::ToneCurve { shadows: 0.5, darks: 0.0, lights: 0.0, highlights: 0.0 }),
        ("tone -1/+1/-1/+1 (limited)", PointOp::ToneCurve { shadows: -1.0, darks: 1.0, lights: -1.0, highlights: 1.0 }),
        ("tone mixed + out-of-range", PointOp::ToneCurve { shadows: 0.2, darks: 0.3, lights: -0.7, highlights: 5.0 }),
        ("curve luminance S", PointOp::PointCurve { points: s_curve(), channel: 0 }),
        ("curve red S", PointOp::PointCurve { points: s_curve(), channel: 1 }),
        (
            "curve green unsorted/dup",
            PointOp::PointCurve { points: vec![[1.0, 0.9], [0.0, 0.1], [0.5, 0.3], [0.5, 0.6]], channel: 2 },
        ),
        (
            "curve blue steep",
            PointOp::PointCurve { points: vec![[0.0, 0.0], [0.5, 0.1], [0.55, 0.9], [1.0, 1.0]], channel: 3 },
        ),
        ("curve luminance two points", PointOp::PointCurve { points: vec![[0.2, 0.1], [0.9, 0.95]], channel: 0 }),
        ("hue/sat mixed bands", PointOp::HueSaturation { bands: bands_a }),
        ("hue band yellow +1", PointOp::HueSaturation { bands: hue_only }),
        ("saturation -1 everywhere", PointOp::HueSaturation { bands: sat_grey }),
        ("lightness blue/magenta", PointOp::HueSaturation { bands: light }),
        ("vignette dark", PointOp::Vignette { amount: -0.6, midpoint: 0.5, feather: 0.5 }),
        ("vignette bright, wide feather", PointOp::Vignette { amount: 0.8, midpoint: 0.7, feather: 1.2 }),
        ("vignette strong, small feather", PointOp::Vignette { amount: -1.0, midpoint: 0.3, feather: 0.05 }),
    ]
}

/// Error of a GPU result against the CPU reference over all pixels: (max, number of pixels above `tol`).
fn max_err(a: &ImageBuf, b: &ImageBuf, tol: f32) -> (f32, usize) {
    let errs = rel_errors(a, b);
    (errs.iter().copied().fold(0.0, f32::max), errs.iter().filter(|e| **e > tol).count())
}

#[test]
fn each_new_op_matches_the_cpu() {
    let ctx = gpu!();
    let img = wide_image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut worst = 0.0f32;
    for (name, op) in new_op_cases() {
        let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
        let cpu = cpu_apply(&img, std::slice::from_ref(&op));
        assert_ne!(cpu, img, "{name}: the case must change the image");
        let (err, over) = max_err(&out, &cpu, 2e-3);
        println!("{name}: max relative error {err:.3e}");
        assert_eq!(over, 0, "{name}: {over} pixels above 2e-3 (max {err})");
        worst = worst.max(err);
        for (a, b) in out.data().chunks_exact(4).zip(img.data().chunks_exact(4)) {
            assert_eq!(a[3].to_bits(), b[3].to_bits(), "{name}: alpha changed");
        }
    }
    println!("worst new single-op error: {worst:.3e}");
}

#[test]
fn neutral_parameters_leave_the_image_bit_exact() {
    let ctx = gpu!();
    let img = wide_image(129, 65);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let neutral = [
        PointOp::Cdl { slope: [1.0; 3], offset: [0.0; 3], power: [1.0; 3] },
        PointOp::Cdl { slope: [f32::NAN; 3], offset: [f32::INFINITY; 3], power: [f32::NAN; 3] },
        PointOp::ToneCurve { shadows: 0.0, darks: 0.0, lights: 0.0, highlights: 0.0 },
        PointOp::ToneCurve { shadows: f32::NAN, darks: 0.0, lights: f32::INFINITY, highlights: 0.0 },
        PointOp::PointCurve { points: vec![[0.0, 0.0], [1.0, 1.0]], channel: 0 },
        PointOp::PointCurve { points: vec![[0.3, 0.7]], channel: 1 },
        PointOp::PointCurve { points: vec![], channel: 2 },
        PointOp::PointCurve { points: vec![[0.4, 0.1], [0.4, 0.9]], channel: 3 },
        PointOp::PointCurve { points: s_curve(), channel: 9 },
        PointOp::HueSaturation { bands: [[0.0; 3]; 6] },
        PointOp::HueSaturation { bands: [[f32::NAN; 3]; 6] },
        PointOp::Vignette { amount: 0.0, midpoint: 0.5, feather: 0.5 },
        PointOp::Vignette { amount: f32::NAN, midpoint: 0.5, feather: 0.5 },
    ];
    for op in &neutral {
        let out = apply_point_ops(ctx, &src, std::slice::from_ref(op)).unwrap().download(ctx).unwrap();
        assert_eq!(out, img, "{op:?} must be the bit-exact identity");
        assert_eq!(cpu_apply(&img, std::slice::from_ref(op)), img, "CPU reference of {op:?} must agree");
    }
    // All of them together are one pipeline and still the identity.
    let out = apply_point_ops(ctx, &src, &neutral).unwrap().download(ctx).unwrap();
    assert_eq!(out, img);
    println!("neutral parameters: bit-exact identity ({} cases)", neutral.len());
}

fn all_ten_chain() -> Vec<PointOp> {
    vec![
        PointOp::Gain(1.3),
        PointOp::Matrix([[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]),
        PointOp::Cdl { slope: [1.1, 0.95, 1.05], offset: [0.01, 0.0, -0.01], power: [0.95, 1.05, 1.0] },
        PointOp::ToneCurve { shadows: 0.3, darks: -0.2, lights: 0.25, highlights: -0.4 },
        PointOp::HueSaturation {
            bands: [[0.1, 0.2, 0.0], [0.0, 0.3, 0.1], [-0.2, 0.1, 0.0], [0.0, -0.2, 0.0], [0.2, 0.2, -0.1], [0.0, 0.1, 0.0]],
        },
        PointOp::PointCurve { points: s_curve(), channel: 0 },
        PointOp::Saturation(1.15),
        PointOp::Contrast { amount: 0.3, pivot: 0.18 },
        PointOp::HighlightsShadows { highlights: -0.4, shadows: 0.3 },
        PointOp::Vignette { amount: -0.5, midpoint: 0.55, feather: 0.6 },
    ]
}

#[test]
fn chain_of_all_ten_kinds_matches_the_cpu_and_the_display() {
    let ctx = gpu!();
    let img = wide_image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let chain = all_ten_chain();
    let out = apply_point_ops(ctx, &src, &chain).unwrap().download(ctx).unwrap();
    let (cpu, flags) = cpu_chain(&img, &chain);
    let errs = rel_errors(&out, &cpu);
    let (good, ill) = split_max(&errs, &flags);
    let n_ill = flags.iter().filter(|f| **f).count();
    println!("ten-op chain: max relative error {good:.3e} (+ {ill:.3e} on {n_ill} ill-conditioned pixels)");
    assert!(good <= 2e-3, "{good}");
    assert!(ill <= 2e-2, "ill-conditioned: {ill}");

    // The same chain without the two ops that divide by / exponentiate the luminance: all pixels well-conditioned.
    let chain_wc: Vec<PointOp> = chain
        .iter()
        .filter(|o| !matches!(o, PointOp::Contrast { .. } | PointOp::HighlightsShadows { .. }))
        .cloned()
        .collect();
    let out_wc = apply_point_ops(ctx, &src, &chain_wc).unwrap().download(ctx).unwrap();
    let cpu_wc = cpu_apply(&img, &chain_wc);
    let (e_wc, over) = max_err(&out_wc, &cpu_wc, 2e-3);
    println!("eight-op chain without contrast and highlights/shadows: max relative error {e_wc:.3e}");
    assert_eq!(over, 0);

    // After the display transform: at most 1 code value for >= 99.9 % of the pixels, never more than 2.
    for (gt, ct) in tone_pairs() {
        for (label, ops_case, cpu_img) in [("ten", &chain, &cpu), ("eight", &chain_wc, &cpu_wc)] {
            let gpu_out = apply_point_ops(ctx, &src, ops_case).unwrap();
            let gpu_bytes = display_rgba8(ctx, &gpu_out, 0.0, gt).unwrap();
            let cpu_bytes = DisplayTransform { exposure_ev: 0.0, tone_map: ct }.apply_with_rgba8(cpu_img).1;
            let (mut max_d, mut over1) = (0u8, 0usize);
            for (g, c) in gpu_bytes.chunks_exact(4).zip(cpu_bytes.chunks_exact(4)) {
                let d = (0..4).map(|k| g[k].abs_diff(c[k])).max().unwrap();
                max_d = max_d.max(d);
                over1 += usize::from(d > 1);
            }
            let n = gpu_bytes.len() / 4;
            let frac = 100.0 * over1 as f64 / n as f64;
            println!("{label}-op chain + display {gt:?}: max diff {max_d}, pixels over 1 code: {frac:.4} %");
            assert!(max_d <= 2, "{label} {gt:?}: max diff {max_d}");
            assert!(frac <= 0.1, "{label} {gt:?}: {frac} % of the pixels differ by more than 1");
        }
    }
}

#[test]
fn each_new_op_matches_the_cpu_after_the_display_transform() {
    let ctx = gpu!();
    let img = wide_image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut worst = 0u8;
    for (name, op) in new_op_cases() {
        let gpu_out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap();
        let cpu_img = cpu_apply(&img, std::slice::from_ref(&op));
        let gpu_bytes = display_rgba8(ctx, &gpu_out, 0.0, GpuToneMap::SoftHighlights).unwrap();
        let cpu_bytes = DisplayTransform { exposure_ev: 0.0, tone_map: ToneMap::SoftHighlights }.apply_with_rgba8(&cpu_img).1;
        let diffs: Vec<u8> = gpu_bytes.iter().zip(&cpu_bytes).map(|(a, b)| a.abs_diff(*b)).collect();
        let max_d = diffs.iter().copied().max().unwrap();
        let over1 = diffs.chunks_exact(4).filter(|p| p.iter().any(|d| *d > 1)).count();
        println!("{name} + display: max diff {max_d}, pixels over 1 code: {over1} of {}", diffs.len() / 4);
        assert!(max_d <= 2, "{name}: {max_d}");
        assert!(over1 as f64 <= 0.001 * (diffs.len() / 4) as f64, "{name}: {over1} pixels over 1 code");
        worst = worst.max(max_d);
    }
    println!("worst display difference of a single new op: {worst}");
}

/// Like `rel_errors` but infinities of equal sign count as equal (NaN must be NaN on both sides).
fn rel_errors_allow_inf(a: &ImageBuf, b: &ImageBuf) -> Vec<f32> {
    a.data()
        .chunks_exact(4)
        .zip(b.data().chunks_exact(4))
        .map(|(p, q)| {
            let mag = q[..3].iter().filter(|v| v.is_finite()).fold(1.0f32, |m, v| m.max(v.abs()));
            let mut worst = 0.0f32;
            for k in 0..3 {
                let (x, y) = (p[k], q[k]);
                if y.is_nan() || x.is_nan() {
                    assert!(x.is_nan() && y.is_nan(), "NaN differs: gpu {x}, cpu {y} in {p:?} / {q:?}");
                } else if y.is_infinite() || x.is_infinite() {
                    assert_eq!(x, y, "infinity differs: gpu {x}, cpu {y} in {p:?} / {q:?}");
                } else {
                    worst = worst.max((x - y).abs() / mag);
                }
            }
            worst
        })
        .collect()
}

#[test]
fn nan_inf_and_negative_inputs_behave_like_the_cpu() {
    let ctx = gpu!();
    let mut img = wide_image(64, 16);
    let specials: [[f32; 3]; 14] = [
        [f32::NAN, 0.5, 0.5],
        [0.5, f32::NAN, 0.5],
        [0.5, 0.5, f32::NAN],
        [f32::NAN; 3],
        [f32::INFINITY, 0.2, 0.2],
        [0.2, f32::NEG_INFINITY, 0.2],
        [f32::INFINITY; 3],
        [-1.0, -2.0, -3.0],
        [-0.5, 0.5, 0.0],
        [0.0, 0.0, 0.0],
        [-0.0, 0.0, -0.0],
        [1e-30, 2e-30, 1e-30],
        [3e30, 1e30, 2e30],
        [0.5, -0.0001, 2.0],
    ];
    for (i, px) in specials.iter().enumerate() {
        img.set_pixel(i as u32, 10, [px[0], px[1], px[2], 1.0]);
    }
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut cases = new_op_cases();
    cases.push((
        "curve luminance extreme",
        PointOp::PointCurve { points: vec![[0.0, 0.0], [0.9999, 0.5], [1.0, 1.0]], channel: 0 },
    ));
    for (name, op) in cases {
        let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
        let cpu = cpu_apply(&img, std::slice::from_ref(&op));
        let errs = rel_errors_allow_inf(&out, &cpu);
        let max = errs.iter().copied().fold(0.0, f32::max);
        println!("{name} with NaN/inf/negative pixels: max relative error {max:.3e}");
        assert!(max <= 2e-3, "{name}: {max}");
        // NaN does not spread to other pixels.
        let nan_out = out.data().chunks_exact(4).filter(|p| p[..3].iter().any(|v| v.is_nan())).count();
        let nan_cpu = cpu.data().chunks_exact(4).filter(|p| p[..3].iter().any(|v| v.is_nan())).count();
        assert_eq!(nan_out, nan_cpu, "{name}: number of NaN pixels differs");
    }
}

#[test]
fn vignette_on_non_square_and_odd_sizes() {
    let ctx = gpu!();
    for (w, h) in [(257u32, 131u32), (101, 37), (300, 64), (64, 300), (65, 65), (63, 8), (2, 2), (1, 1), (1, 9), (9, 1)] {
        let mut img = ImageBuf::new(w, h);
        for y in 0..h {
            for x in 0..w {
                img.set_pixel(x, y, [0.5 + x as f32 * 0.01, 1.0, 2.0 - y as f32 * 0.02, 0.5]);
            }
        }
        let src = GpuImage::upload(ctx, &img).unwrap();
        for (amount, mid, feather) in
            [(-0.8f32, 0.5f32, 0.5f32), (0.6, 0.2, 0.9), (-1.0, 0.0, 1.0), (-0.5, 0.9, 0.3), (-0.5, 0.4, 0.0)]
        {
            let op = PointOp::Vignette { amount, midpoint: mid, feather };
            let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
            let cpu = cpu_apply(&img, std::slice::from_ref(&op));
            let (err, over) = max_err(&out, &cpu, 2e-3);
            if feather == 0.0 {
                // A hard step can flip on single pixels sitting exactly on the edge (rounding of the distance).
                println!("{w}x{h} vignette {amount}/{mid}/{feather} (hard edge): max err {err:.3e}, {over} pixels over 2e-3");
                assert!(over <= 4 + (w * h) as usize / 100, "{w}x{h}: {over} pixels differ at the hard edge");
            } else {
                println!("{w}x{h} vignette {amount}/{mid}/{feather}: max err {err:.3e}");
                assert_eq!(over, 0, "{w}x{h} vignette {amount}/{mid}/{feather}: max {err}");
            }
        }
    }
}

#[test]
fn pipeline_cache_and_tables_with_the_new_ops() {
    // Fresh context: the counters must not be disturbed by parallel tests.
    let Ok(ctx) = GpuContext::new() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let img = wide_image(129, 65);
    let src = GpuImage::upload(&ctx, &img).unwrap();
    let chain = |curve_a: Vec<[f32; 2]>, curve_b: Vec<[f32; 2]>, tone: f32, amount: f32| {
        vec![
            PointOp::Cdl { slope: [1.1; 3], offset: [0.01; 3], power: [0.9; 3] },
            PointOp::ToneCurve { shadows: tone, darks: 0.0, lights: -tone, highlights: 0.1 },
            PointOp::PointCurve { points: curve_a, channel: 0 },
            PointOp::HueSaturation { bands: [[0.1 * tone, 0.2, 0.0]; 6] },
            PointOp::PointCurve { points: curve_b, channel: 2 },
            PointOp::Vignette { amount, midpoint: 0.5, feather: 0.5 },
        ]
    };
    let curve_1 = s_curve();
    let curve_2 = vec![[0.0, 0.0], [0.5, 0.8], [1.0, 1.0]];
    let curve_3 = vec![[0.0, 0.1], [0.4, 0.3], [1.0, 0.9]];
    let a = chain(curve_1.clone(), curve_2.clone(), 0.4, -0.5);
    let b = chain(curve_3.clone(), curve_1.clone(), -0.6, 0.7);

    let ra = apply_point_ops(&ctx, &src, &a).unwrap().download(&ctx).unwrap();
    assert_eq!(ctx.pipelines_created(), 1);
    let rb = apply_point_ops(&ctx, &src, &b).unwrap().download(&ctx).unwrap();
    let ra2 = apply_point_ops(&ctx, &src, &a).unwrap().download(&ctx).unwrap();
    assert_eq!(ctx.pipelines_created(), 1, "same kinds, other parameters and curves: no new pipeline");
    assert_ne!(ra, rb);
    assert_eq!(ra, ra2, "switching the curves back and forth must restore the exact result");
    // Two PointCurve ops with different curves in one chain, both against the CPU.
    let (e_a, over_a) = max_err(&ra, &cpu_apply(&img, &a), 2e-3);
    let (e_b, over_b) = max_err(&rb, &cpu_apply(&img, &b), 2e-3);
    println!("two curves in one chain: max relative error {e_a:.3e} / {e_b:.3e}");
    assert_eq!((over_a, over_b), (0, 0));

    // Swapping the two curves changes the result: they are applied separately.
    let swapped = chain(curve_2.clone(), curve_1.clone(), 0.4, -0.5);
    let rs = apply_point_ops(&ctx, &src, &swapped).unwrap().download(&ctx).unwrap();
    assert_ne!(rs, ra);
    assert_eq!(max_err(&rs, &cpu_apply(&img, &swapped), 2e-3).1, 0);

    // A slider drag through render_into: after the first call nothing new at all (tables included).
    let scratch = GpuImage::new_empty(&ctx, 129, 65).unwrap();
    let target = DisplayTarget::new(&ctx, 129, 65).unwrap();
    render_into(&ctx, &src, &scratch, &target, &a, 0.0, GpuToneMap::Clip).unwrap();
    ctx.wait_idle();
    let (p0, a0) = (ctx.pipelines_created(), ctx.allocations_created());
    for i in 0..6 {
        let t = 0.1 * i as f32;
        let ops = chain(if i % 2 == 0 { curve_1.clone() } else { curve_3.clone() }, curve_2.clone(), t, -0.1 * i as f32);
        render_into(&ctx, &src, &scratch, &target, &ops, 0.0, GpuToneMap::AcesFit).unwrap();
    }
    ctx.wait_idle();
    assert_eq!(ctx.pipelines_created(), p0, "no new pipeline while dragging");
    assert_eq!(ctx.allocations_created(), a0, "no new texture or buffer while dragging (tables included)");
}

#[test]
fn render_into_with_the_new_ops_equals_op_pass_plus_display() {
    let ctx = gpu!();
    let img = wide_image(257, 131);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let scratch = GpuImage::new_empty(ctx, 257, 131).unwrap();
    let target = DisplayTarget::new(ctx, 257, 131).unwrap();
    let chain = all_ten_chain();
    let cpu_img = cpu_apply(&img, &chain);
    for (gt, ct) in tone_pairs() {
        for ev in [-1.0f32, 0.0, 1.0] {
            render_into(ctx, &base, &scratch, &target, &chain, ev, gt).unwrap();
            let got = target.read_rgba8(ctx).unwrap();
            let two_step = display_rgba8(ctx, &apply_point_ops(ctx, &base, &chain).unwrap(), ev, gt).unwrap();
            assert_eq!(got, two_step, "{gt:?} ev {ev}");
            let cpu = DisplayTransform { exposure_ev: ev, tone_map: ct }.apply_with_rgba8(&cpu_img).1;
            let diffs: Vec<u8> = got.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).collect();
            let max_d = diffs.iter().copied().max().unwrap();
            let over1 = diffs.chunks_exact(4).filter(|p| p.iter().any(|d| *d > 1)).count();
            println!("render_into ten ops {gt:?} ev {ev:+}: max diff {max_d}, pixels over 1: {over1} of {}", diffs.len() / 4);
            assert!(max_d <= 2);
            assert!(over1 as f64 <= 0.001 * (diffs.len() / 4) as f64);
        }
    }
}
