#![allow(clippy::chunks_exact_to_as_chunks)] // runtime-sized pixel slices; as_chunks would not read better here

//! Wave 8: whites/blacks, vibrance and the colour curves (Sat vs Sat, Hue vs Hue, Hue vs Sat) against
//! the CPU reference of `rawgraph_fx`. Without a GPU adapter every test prints "skipped: no GPU adapter"
//! and passes. Run with `-- --nocapture` to see the measured maxima.

use std::sync::OnceLock;

use rawgraph_core::color::AP1_LUMA;
use rawgraph_core::ImageBuf;
use rawgraph_gpu::{apply_point_ops, display_rgba8, render_into, DisplayTarget, GpuContext, GpuImage, GpuToneMap, PointOp};
use rawgraph_scopes::{DisplayTransform, ToneMap};

fn shared() -> Option<&'static GpuContext> {
    static CTX: OnceLock<Option<GpuContext>> = OnceLock::new();
    CTX.get_or_init(|| GpuContext::new().ok()).as_ref()
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

/// Log ramp, negative values, HDR checker, saturated primaries, exact and almost neutral rows, a hue sweep
/// at several chroma levels and brightnesses, and a row of single special values (needs `w >= 64`, `h >= 12`).
fn image(w: u32, h: u32) -> ImageBuf {
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
    for x in 0..w {
        let f = x as f32 / (w - 1) as f32;
        let grey = (-8.0 + 16.0 * f).exp2();
        img.set_pixel(x, 0, [grey, grey, grey, 1.0]);
        img.set_pixel(x, 1, [grey * (1.0 + 1e-4), grey, grey * (1.0 - 1e-4), 1.0]);
        let ang = f * std::f32::consts::TAU;
        let (cr, cg, cb) = (ang.cos(), (ang - 2.0944).cos(), (ang - 4.1888).cos());
        for (row, (chroma, level)) in [(2u32, (0.6f32, 0.18f32)), (3, (0.15, 0.18)), (4, (0.9, 3.0)), (5, (0.5, 0.01))] {
            img.set_pixel(x, row, [level * (1.0 + chroma * cr), level * (1.0 + chroma * cg), level * (1.0 + chroma * cb), 1.0]);
        }
        img.set_pixel(x, 6, [-0.1 * f, 0.4, 0.2 * (1.0 - f), 1.0]);
        img.set_pixel(x, 7, [-0.5, -0.5, -0.5, 1.0]);
        // Weakly to strongly saturated sweep at constant hue, dark to very bright.
        let lvl = (-6.0 + 18.0 * f).exp2();
        img.set_pixel(x, 8, [lvl, lvl * 0.5, lvl * 0.25, 1.0]);
        img.set_pixel(x, 9, [lvl, lvl * (1.0 - f), lvl * (1.0 - f), 1.0]);
    }
    // Row 10: single values that matter: 0, tiny, 0.18, 1, 16, 4096, negative, primaries, greys.
    let specials: [[f32; 3]; 20] = [
        [0.0, 0.0, 0.0],
        [1e-30, 2e-30, 1e-30],
        [1e-6, 3e-6, 2e-6],
        [0.18, 0.18, 0.18],
        [1.0, 1.0, 1.0],
        [16.0, 16.0, 16.0],
        [4096.0, 4096.0, 4096.0],
        [4096.0, 1.0, 0.18],
        [-1.0, -2.0, -3.0],
        [-0.5, 0.5, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [16.0, 0.0, 0.0],
        [0.0, 0.0, 16.0],
        [0.0, 1.0, 1.0],
        [1.0, 1.0, 0.0],
        [3e30, 1e30, 2e30],
        [0.5, -0.0001, 2.0],
        [0.0, 0.05, 0.0],
    ];
    for (i, px) in specials.iter().enumerate() {
        img.set_pixel(i as u32, 10, [px[0], px[1], px[2], 1.0]);
    }
    img
}

/// Same image plus NaN / infinity pixels in row 11.
fn image_with_specials(w: u32, h: u32) -> ImageBuf {
    let mut img = image(w, h);
    let specials: [[f32; 3]; 12] = [
        [f32::NAN, 0.5, 0.5],
        [0.5, f32::NAN, 0.5],
        [0.5, 0.5, f32::NAN],
        [f32::NAN; 3],
        [f32::INFINITY, 0.2, 0.2],
        [0.2, f32::NEG_INFINITY, 0.2],
        [f32::INFINITY; 3],
        [f32::NEG_INFINITY, f32::INFINITY, 0.0],
        [-f32::NAN, -0.5, 0.0],
        [0.0, f32::INFINITY, -1.0],
        [1e36, 1e36, 0.0],
        [-1e36, 1.0, f32::NAN],
    ];
    for (i, px) in specials.iter().enumerate() {
        img.set_pixel(i as u32, 11, [px[0], px[1], px[2], 1.0]);
    }
    img
}

/// Per-pixel error relative to max(|R|, |G|, |B|, 1) of the finite CPU channels; NaN must be NaN on both
/// sides and infinities must be equal.
fn rel_errors(a: &ImageBuf, b: &ImageBuf) -> Vec<f32> {
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
            assert_eq!(p[3].to_bits(), q[3].to_bits(), "alpha changed");
            worst
        })
        .collect()
}

/// The luminance sum cancels (|Y| < 2 % of the sum of its terms): the saturation of such a pixel
/// (`C / (|Y| + C)`) is ill-conditioned, rounding differences of the GPU are amplified there.
fn ill_conditioned(px: &[f32]) -> bool {
    let y = AP1_LUMA[0] * px[0] + AP1_LUMA[1] * px[1] + AP1_LUMA[2] * px[2];
    let terms = AP1_LUMA[0] * px[0].abs() + AP1_LUMA[1] * px[1].abs() + AP1_LUMA[2] * px[2].abs();
    y.abs() < 0.02 * terms
}

fn cpu_apply(img: &ImageBuf, ops: &[PointOp]) -> ImageBuf {
    let mut out = img.clone();
    for op in ops {
        match op {
            PointOp::WhitesBlacks { whites, blacks } => rawgraph_fx::whites_blacks(&mut out, *whites, *blacks),
            PointOp::Vibrance(a) => rawgraph_fx::vibrance(&mut out, *a),
            PointOp::PointCurve { points, channel } => {
                if let Some(ch) = rawgraph_fx::CurveChannel::from_index(usize::from(*channel)) {
                    rawgraph_fx::point_curve(&mut out, points, ch);
                }
            }
            PointOp::Saturation(s) => rawgraph_core::ops::saturation(&mut out, *s),
            PointOp::Contrast { amount, pivot } => rawgraph_core::ops::contrast(&mut out, *amount, *pivot),
            PointOp::Gain(g) => rawgraph_core::ops::apply_gain(&mut out, *g),
            _ => panic!("op not used by these tests: {op:?}"),
        }
    }
    out
}

fn bump() -> Vec<[f32; 2]> {
    vec![[0.0, 0.5], [0.25, 0.8], [0.5, 0.5], [0.75, 0.2], [1.0, 0.5]]
}

fn cases() -> Vec<(&'static str, PointOp)> {
    let wb = |whites, blacks| PointOp::WhitesBlacks { whites, blacks };
    let cc = |points: Vec<[f32; 2]>, channel| PointOp::PointCurve { points, channel };
    vec![
        ("whites +0.7", wb(0.7, 0.0)),
        ("whites -1", wb(-1.0, 0.0)),
        ("blacks +0.6", wb(0.0, 0.6)),
        ("blacks -0.8", wb(0.0, -0.8)),
        ("whites/blacks mixed", wb(0.5, -0.3)),
        ("whites/blacks +1/+1", wb(1.0, 1.0)),
        ("whites/blacks out of range", wb(5.0, -7.0)),
        ("whites NaN, blacks 0.4", wb(f32::NAN, 0.4)),
        ("whites inf, blacks -0.4", wb(f32::INFINITY, -0.4)),
        ("vibrance +0.8", PointOp::Vibrance(0.8)),
        ("vibrance -0.5", PointOp::Vibrance(-0.5)),
        ("vibrance -1", PointOp::Vibrance(-1.0)),
        ("vibrance +1", PointOp::Vibrance(1.0)),
        ("vibrance out of range", PointOp::Vibrance(4.0)),
        ("sat vs sat boost low", cc(vec![[0.0, 0.5], [0.3, 0.9], [1.0, 0.5]], 4)),
        ("sat vs sat cut high", cc(vec![[0.0, 0.5], [0.6, 0.5], [1.0, 0.1]], 4)),
        ("sat vs sat steep", cc(vec![[0.0, 0.5], [0.4, 0.5], [0.45, 1.0], [1.0, 1.0]], 4)),
        ("sat vs sat identity line", cc(vec![[0.0, 0.0], [1.0, 1.0]], 4)),
        ("hue vs hue bump", cc(bump(), 5)),
        ("hue vs hue constant +", cc(vec![[0.0, 0.9], [1.0, 0.9]], 5)),
        ("hue vs hue constant -", cc(vec![[0.0, 0.1], [1.0, 0.1]], 5)),
        ("hue vs hue steep", cc(vec![[0.0, 0.5], [0.3, 0.5], [0.34, 1.0], [0.7, 1.0], [0.72, 0.0], [1.0, 0.0]], 5)),
        ("hue vs hue unsorted/dup", cc(vec![[1.0, 0.6], [0.0, 0.4], [0.5, 0.9], [0.5, 0.2]], 5)),
        ("hue vs sat bump", cc(bump(), 6)),
        ("hue vs sat grey out", cc(vec![[0.0, 0.0], [1.0, 0.0]], 6)),
        ("hue vs sat boost blue", cc(vec![[0.0, 0.5], [0.55, 0.5], [0.67, 1.0], [0.8, 0.5], [1.0, 0.5]], 6)),
        ("hue vs sat non-finite point", cc(vec![[0.0, 0.5], [f32::NAN, 0.2], [0.5, 0.8], [1.0, 0.5]], 6)),
    ]
}

/// (max error over well-conditioned pixels, max error over ill-conditioned pixels)
fn split_max(errs: &[f32], src: &ImageBuf) -> (f32, f32) {
    let (mut good, mut bad) = (0.0f32, 0.0f32);
    for (e, px) in errs.iter().zip(src.data().chunks_exact(4)) {
        if ill_conditioned(px) {
            bad = bad.max(*e);
        } else {
            good = good.max(*e);
        }
    }
    (good, bad)
}

fn is_colour_curve(op: &PointOp) -> bool {
    matches!(op, PointOp::PointCurve { channel, .. } if *channel >= 4)
}

#[test]
fn each_wave8_op_matches_the_cpu_in_float() {
    let ctx = gpu!();
    let img = image_with_specials(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let mut worst: [(&str, f32); 3] = [("whites/blacks", 0.0), ("vibrance", 0.0), ("colour curves", 0.0)];
    for (name, op) in cases() {
        let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
        let cpu = cpu_apply(&img, std::slice::from_ref(&op));
        let errs = rel_errors(&out, &cpu);
        let (good, ill) = if is_colour_curve(&op) { split_max(&errs, &img) } else { (errs.iter().copied().fold(0.0, f32::max), 0.0) };
        println!("{name}: max relative error {good:.3e} (ill-conditioned pixels {ill:.3e})");
        assert!(good <= 2e-3, "{name}: {good}");
        assert!(ill <= 2e-2, "{name}: ill-conditioned {ill}");
        let slot = match op {
            PointOp::WhitesBlacks { .. } => 0,
            PointOp::Vibrance(_) => 1,
            _ => 2,
        };
        worst[slot].1 = worst[slot].1.max(good);
        // NaN pixels are NaN on both sides (checked in rel_errors); NaN does not spread to other pixels.
        let nan_gpu = out.data().chunks_exact(4).filter(|p| p[..3].iter().any(|v| v.is_nan())).count();
        let nan_cpu = cpu.data().chunks_exact(4).filter(|p| p[..3].iter().any(|v| v.is_nan())).count();
        assert_eq!(nan_gpu, nan_cpu, "{name}: number of NaN pixels differs");
    }
    for (n, e) in worst {
        println!("WORST float error {n}: {e:.3e}");
    }
}

#[test]
fn each_wave8_op_matches_the_cpu_after_the_display_transform() {
    let ctx = gpu!();
    let img = image(257, 131);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let pairs = [(GpuToneMap::Clip, ToneMap::Clip), (GpuToneMap::SoftHighlights, ToneMap::SoftHighlights), (GpuToneMap::AcesFit, ToneMap::AcesFit)];
    let mut worst = [0u8; 3];
    let mut worst_over1 = 0usize;
    for (name, op) in cases() {
        let gpu_out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap();
        let cpu_img = cpu_apply(&img, std::slice::from_ref(&op));
        let slot = match op {
            PointOp::WhitesBlacks { .. } => 0,
            PointOp::Vibrance(_) => 1,
            _ => 2,
        };
        for (gt, ct) in pairs {
            let gpu_bytes = display_rgba8(ctx, &gpu_out, 0.0, gt).unwrap();
            let cpu_bytes = DisplayTransform { exposure_ev: 0.0, tone_map: ct }.apply_with_rgba8(&cpu_img).1;
            let diffs: Vec<u8> = gpu_bytes.iter().zip(&cpu_bytes).map(|(a, b)| a.abs_diff(*b)).collect();
            let max_d = diffs.iter().copied().max().unwrap();
            let over1 = diffs.chunks_exact(4).filter(|p| p.iter().any(|d| *d > 1)).count();
            println!("{name} + display {gt:?}: max diff {max_d}, pixels over 1 code: {over1}");
            assert!(max_d <= 1, "{name} {gt:?}: max diff {max_d}");
            worst[slot] = worst[slot].max(max_d);
            worst_over1 = worst_over1.max(over1);
        }
    }
    println!("WORST display difference (code values): whites/blacks {}, vibrance {}, colour curves {}", worst[0], worst[1], worst[2]);
}

#[test]
fn neutral_wave8_parameters_are_the_bit_exact_identity() {
    let ctx = gpu!();
    let img = image_with_specials(129, 65);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let neutral = [
        PointOp::WhitesBlacks { whites: 0.0, blacks: 0.0 },
        PointOp::WhitesBlacks { whites: f32::NAN, blacks: f32::INFINITY },
        PointOp::WhitesBlacks { whites: -0.0, blacks: f32::NEG_INFINITY },
        PointOp::Vibrance(0.0),
        PointOp::Vibrance(f32::NAN),
        PointOp::Vibrance(f32::INFINITY),
        PointOp::PointCurve { points: vec![[0.0, 0.5], [1.0, 0.5]], channel: 4 },
        PointOp::PointCurve { points: vec![[0.0, 0.5], [1.0, 0.5]], channel: 5 },
        PointOp::PointCurve { points: vec![[0.0, 0.5], [0.4, 0.5], [1.0, 0.5]], channel: 6 },
        PointOp::PointCurve { points: vec![[0.3, 0.9]], channel: 5 },
        PointOp::PointCurve { points: vec![], channel: 4 },
        PointOp::PointCurve { points: vec![[0.4, 0.1], [0.4, 0.9]], channel: 6 },
        PointOp::PointCurve { points: vec![[f32::NAN, 0.1], [0.4, 0.9]], channel: 6 },
        PointOp::PointCurve { points: vec![[0.0, 0.1], [1.0, 0.9]], channel: 7 },
    ];
    for op in &neutral {
        let out = apply_point_ops(ctx, &src, std::slice::from_ref(op)).unwrap().download(ctx).unwrap();
        let same = |a: &ImageBuf, b: &ImageBuf| a.data().iter().zip(b.data()).all(|(x, y)| x.to_bits() == y.to_bits());
        assert!(same(&out, &img), "{op:?} must be the bit-exact identity");
        assert!(same(&cpu_apply(&img, std::slice::from_ref(op)), &img), "CPU reference of {op:?} must agree");
    }
    let out = apply_point_ops(ctx, &src, &neutral).unwrap().download(ctx).unwrap();
    assert!(out.data().iter().zip(img.data()).all(|(x, y)| x.to_bits() == y.to_bits()));
    println!("neutral wave 8 parameters: bit-exact identity ({} cases)", neutral.len());
}

fn chain(tone: f32) -> Vec<PointOp> {
    vec![
        PointOp::Gain(1.2),
        PointOp::WhitesBlacks { whites: 0.4 * tone, blacks: -0.3 },
        PointOp::PointCurve { points: bump(), channel: 5 },
        PointOp::Vibrance(0.5 * tone),
        PointOp::PointCurve { points: vec![[0.0, 0.5], [0.3, 0.8], [1.0, 0.5]], channel: 4 },
        PointOp::PointCurve { points: vec![[0.0, 0.5], [0.6, 0.9], [1.0, 0.5]], channel: 6 },
        PointOp::Saturation(1.1),
    ]
}

#[test]
fn chains_with_the_new_ops_cache_pipelines_and_tables() {
    let Ok(ctx) = GpuContext::new() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let img = image(129, 65);
    let src = GpuImage::upload(&ctx, &img).unwrap();
    let a = chain(1.0);
    let mut b = chain(-0.7);
    b[2] = PointOp::PointCurve { points: vec![[0.0, 0.2], [0.5, 0.5], [1.0, 0.8]], channel: 5 };
    b[4] = PointOp::PointCurve { points: vec![[0.0, 0.5], [0.3, 0.2], [1.0, 0.5]], channel: 4 };

    let ra = apply_point_ops(&ctx, &src, &a).unwrap().download(&ctx).unwrap();
    assert_eq!(ctx.pipelines_created(), 1);
    let rb = apply_point_ops(&ctx, &src, &b).unwrap().download(&ctx).unwrap();
    let ra2 = apply_point_ops(&ctx, &src, &a).unwrap().download(&ctx).unwrap();
    assert_eq!(ctx.pipelines_created(), 1, "same kinds, other parameters and curves: no new pipeline");
    assert_ne!(ra, rb);
    assert_eq!(ra, ra2, "switching the curves back and forth must restore the exact result");
    for (label, out, ops) in [("a", &ra, &a), ("b", &rb, &b)] {
        let cpu = cpu_apply(&img, ops);
        let errs = rel_errors(out, &cpu);
        let (good, ill) = split_max(&errs, &img);
        println!("seven-op chain {label}: max relative error {good:.3e} (ill-conditioned {ill:.3e})");
        assert!(good <= 2e-3, "{label}: {good}");
        // Neutral parameters inside a chain do not change anything.
        let mut with_neutral = ops.clone();
        with_neutral.push(PointOp::Vibrance(0.0));
        with_neutral.push(PointOp::WhitesBlacks { whites: 0.0, blacks: 0.0 });
        let on = apply_point_ops(&ctx, &src, &with_neutral).unwrap().download(&ctx).unwrap();
        assert_eq!(&on, out, "neutral ops at the end of the chain must not change the result");
    }

    // Slider drag through render_into: after the first call nothing new at all (tables included).
    let scratch = GpuImage::new_empty(&ctx, 129, 65).unwrap();
    let target = DisplayTarget::new(&ctx, 129, 65).unwrap();
    render_into(&ctx, &src, &scratch, &target, &a, 0.0, GpuToneMap::Clip).unwrap();
    ctx.wait_idle();
    let (p0, a0) = (ctx.pipelines_created(), ctx.allocations_created());
    for i in 0..6 {
        let ops = if i % 2 == 0 { chain(0.1 * i as f32) } else { b.clone() };
        render_into(&ctx, &src, &scratch, &target, &ops, 0.0, GpuToneMap::AcesFit).unwrap();
    }
    ctx.wait_idle();
    assert_eq!(ctx.pipelines_created(), p0, "no new pipeline while dragging");
    assert_eq!(ctx.allocations_created(), a0, "no new texture or buffer while dragging");
}

#[test]
fn render_into_with_the_new_ops_matches_the_cpu_display() {
    let ctx = gpu!();
    let img = image(257, 131);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let scratch = GpuImage::new_empty(ctx, 257, 131).unwrap();
    let target = DisplayTarget::new(ctx, 257, 131).unwrap();
    let ops = chain(1.0);
    let cpu_img = cpu_apply(&img, &ops);
    for (gt, ct) in [(GpuToneMap::Clip, ToneMap::Clip), (GpuToneMap::SoftHighlights, ToneMap::SoftHighlights), (GpuToneMap::AcesFit, ToneMap::AcesFit)] {
        render_into(ctx, &base, &scratch, &target, &ops, 0.0, gt).unwrap();
        let got = target.read_rgba8(ctx).unwrap();
        let two_step = display_rgba8(ctx, &apply_point_ops(ctx, &base, &ops).unwrap(), 0.0, gt).unwrap();
        assert_eq!(got, two_step, "{gt:?}");
        let cpu = DisplayTransform { exposure_ev: 0.0, tone_map: ct }.apply_with_rgba8(&cpu_img).1;
        let diffs: Vec<u8> = got.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).collect();
        let max_d = diffs.iter().copied().max().unwrap();
        let over1 = diffs.chunks_exact(4).filter(|p| p.iter().any(|d| *d > 1)).count();
        println!("render_into seven-op chain {gt:?}: max diff {max_d}, pixels over 1: {over1} of {}", diffs.len() / 4);
        assert!(max_d <= 2);
        assert!(over1 as f64 <= 0.001 * (diffs.len() / 4) as f64);
    }
}
