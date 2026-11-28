//! LUT op against the CPU reference `rawgraph_fx::lut_apply`. Without a GPU adapter every test prints
//! "skipped: no GPU adapter" and passes.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rawgraph_core::lut::parse_cube;
use rawgraph_core::{ImageBuf, Lut, LutSpace};
use rawgraph_gpu::{apply_point_ops, display_rgba8, GpuContext, GpuImage, GpuToneMap, PointOp};
use rawgraph_scopes::{DisplayTransform, ToneMap};

fn shared() -> Option<&'static GpuContext> {
    static CTX: OnceLock<Option<GpuContext>> = OnceLock::new();
    CTX.get_or_init(|| GpuContext::new().ok()).as_ref()
}

fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn image(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / (w - 1) as f32, y as f32 / (h - 1) as f32);
            let px = match (x * 5 + y * 3) % 17 {
                0 => [4.0, 0.5, 0.01],
                1 => [-0.2, 0.3, 0.9],
                2 => [0.0, 0.0, 0.0],
                _ => [(-8.0 + 11.0 * fx).exp2(), 0.02 + 0.8 * fy, 0.7 * (1.0 - fx) * fy + 0.01],
            };
            img.set_pixel(x, y, [px[0], px[1], px[2], 0.3 + 0.7 * fx]);
        }
    }
    img
}

/// A cube with cross-channel mixing and a curve, so every tetrahedron is exercised.
fn cube(n: usize, domain: Option<(f32, f32)>) -> String {
    let mut s = String::new();
    if let Some((lo, hi)) = domain {
        s.push_str(&format!("DOMAIN_MIN {lo} {lo} {lo}\nDOMAIN_MAX {hi} {hi} {hi}\n"));
    }
    s.push_str(&format!("LUT_3D_SIZE {n}\n"));
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let d = (n - 1) as f32;
                let (r, g, b) = (r as f32 / d, g as f32 / d, b as f32 / d);
                s.push_str(&format!("{} {} {}\n", (r * 0.7 + g * 0.3).powf(0.8), g * (1.0 - 0.4 * b) + 0.1 * r * r, 0.2 + 0.6 * b * b + 0.2 * (r - g).abs()));
            }
        }
    }
    s
}

fn both() -> Lut {
    let mut text = String::from("LUT_1D_SIZE 16\nLUT_3D_SIZE 5\n");
    for i in 0..16 {
        let t = i as f32 / 15.0;
        text.push_str(&format!("{} {} {}\n", t.sqrt(), t * t, t));
    }
    let c = cube(5, None);
    text.push_str(c.split_once('\n').unwrap().1);
    parse_cube(&text).unwrap()
}

fn cpu(img: &ImageBuf, lut: &Lut, space: LutSpace, strength: f32) -> ImageBuf {
    let mut out = img.clone();
    rawgraph_fx::lut_apply(&mut out, lut, space, strength);
    out
}

#[test]
fn lut_op_matches_the_cpu_in_float_and_after_the_display_transform() {
    let Some(ctx) = shared() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let _gpu = exclusive();
    let img = image(67, 41);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let luts: Vec<(&str, Arc<Lut>)> = vec![
        ("3D 9", Arc::new(parse_cube(&cube(9, None)).unwrap())),
        ("3D 17 domain", Arc::new(parse_cube(&cube(17, Some((-0.5, 2.0)))).unwrap())),
        ("1D+3D", Arc::new(both())),
    ];
    let mut worst_float = 0.0f32;
    let mut worst_code = 0u8;
    for (name, lut) in &luts {
        for space in [LutSpace::Linear, LutSpace::Srgb709, LutSpace::AcesCct] {
            for strength in [1.0, 0.6, 0.5, 0.01] {
                let op = PointOp::Lut { lut: Arc::clone(lut), space, strength };
                let gpu = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap();
                let out = gpu.download(ctx).unwrap();
                let want = cpu(&img, lut, space, strength);
                let mut err = 0.0f32;
                for (a, b) in out.data().iter().zip(want.data()) {
                    err = err.max((a - b).abs() / b.abs().max(1.0));
                }
                worst_float = worst_float.max(err);
                assert!(err <= 2e-4, "{name} {space:?} {strength}: float error {err}");
                let g = display_rgba8(ctx, &gpu, 0.0, GpuToneMap::AcesFit).unwrap();
                let c = DisplayTransform { exposure_ev: 0.0, tone_map: ToneMap::AcesFit }.apply_with_rgba8(&want).1;
                let d = g.iter().zip(&c).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
                worst_code = worst_code.max(d);
                assert!(d <= 1, "{name} {space:?} {strength}: {d} code values");
            }
        }
    }
    println!("LUT GPU vs CPU: worst relative float error {worst_float:.3e}, worst display difference {worst_code} code value(s)");
}

#[test]
fn lut_op_chains_with_other_ops_and_rewrites_the_table_only_on_change() {
    let Some(ctx) = shared() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let _gpu = exclusive();
    let img = image(32, 32);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let a = Arc::new(parse_cube(&cube(9, None)).unwrap());
    let b = Arc::new(parse_cube(&cube(9, Some((0.0, 1.5)))).unwrap());
    let run = |lut: &Arc<Lut>| {
        let ops = [PointOp::Gain(1.5), PointOp::Lut { lut: Arc::clone(lut), space: LutSpace::AcesCct, strength: 1.0 }, PointOp::Saturation(0.8)];
        apply_point_ops(ctx, &src, &ops).unwrap().download(ctx).unwrap()
    };
    let first = run(&a);
    let pipelines = ctx.pipelines_created();
    let second = run(&b);
    assert_eq!(ctx.pipelines_created(), pipelines, "same table size: the shader is reused");
    assert!(first.data().iter().zip(second.data()).any(|(x, y)| (x - y).abs() > 1e-3), "the new table content reached the GPU");
    assert_eq!(first, run(&a), "going back gives the first result again");
    let mut cpu_img = img.clone();
    rawgraph_core::ops::apply_gain(&mut cpu_img, 1.5);
    rawgraph_fx::lut_apply(&mut cpu_img, &a, LutSpace::AcesCct, 1.0);
    rawgraph_core::ops::saturation(&mut cpu_img, 0.8);
    let err = first.data().iter().zip(cpu_img.data()).map(|(x, y)| (x - y).abs() / y.abs().max(1.0)).fold(0.0, f32::max);
    assert!(err <= 5e-4, "chain error {err}");
}

fn max_rel(a: &ImageBuf, b: &ImageBuf) -> f32 {
    a.data().iter().zip(b.data()).map(|(x, y)| (x - y).abs() / y.abs().max(1.0)).fold(0.0, f32::max)
}

#[test]
fn the_largest_tables_match_the_cpu() {
    let Some(ctx) = shared() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let _gpu = exclusive();
    let img = image(33, 17);
    let src = GpuImage::upload(ctx, &img).unwrap();
    // 65^3 cube (the biggest size) and a 1D table with 65536 rows, alone and as shaper + cube.
    let big3 = Arc::new(parse_cube(&cube(65, None)).unwrap());
    let mut text = String::from("LUT_1D_SIZE 65536\n");
    for i in 0..65_536 {
        let t = i as f32 / 65_535.0;
        text.push_str(&format!("{} {} {}\n", t.powf(0.8), t, t * t));
    }
    let big1 = Arc::new(parse_cube(&text).unwrap());
    let mut both_text = text.replace("LUT_1D_SIZE 65536\n", "LUT_1D_SIZE 65536\nLUT_3D_SIZE 17\n");
    both_text.push_str(cube(17, None).split_once('\n').unwrap().1);
    let big_both = Arc::new(parse_cube(&both_text).unwrap());
    for (name, lut) in [("3D 65", big3), ("1D 65536", big1), ("1D 65536 + 3D 17", big_both)] {
        for space in [LutSpace::Linear, LutSpace::Srgb709, LutSpace::AcesCct] {
            let op = PointOp::Lut { lut: Arc::clone(&lut), space, strength: 0.8 };
            let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
            let err = max_rel(&out, &cpu(&img, &lut, space, 0.8));
            println!("{name} {space:?}: {err:.3e}");
            assert!(err <= 2e-4, "{name} {space:?}: {err}");
        }
    }
}

#[test]
fn non_finite_pixels_come_out_finite_and_like_the_cpu() {
    let Some(ctx) = shared() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let _gpu = exclusive();
    let mut img = ImageBuf::new(4, 2);
    img.set_pixel(0, 0, [f32::INFINITY, 0.5, 0.2, 1.0]);
    img.set_pixel(1, 0, [f32::NAN, f32::NEG_INFINITY, 0.3, 1.0]);
    img.set_pixel(2, 0, [0.4, f32::INFINITY, f32::NAN, 0.5]);
    img.set_pixel(3, 0, [0.1, 0.2, 0.3, 1.0]);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let lut = Arc::new(parse_cube(&cube(9, None)).unwrap());
    for space in [LutSpace::Linear, LutSpace::Srgb709, LutSpace::AcesCct] {
        for strength in [1.0, 0.5] {
            let op = PointOp::Lut { lut: Arc::clone(&lut), space, strength };
            let out = apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap().download(ctx).unwrap();
            let want = cpu(&img, &lut, space, strength);
            assert!(want.data().iter().all(|v| v.is_finite()), "{space:?} {strength}: CPU {:?}", want.data());
            assert!(out.data().iter().all(|v| v.is_finite()), "{space:?} {strength}: GPU {:?}", out.data());
            let err = max_rel(&out, &want);
            assert!(err <= 1e-3, "{space:?} {strength}: {err}");
        }
    }
}

#[test]
fn lut_pipelines_are_bounded_and_the_oldest_are_rebuilt_on_demand() {
    let Some(ctx) = shared() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let _gpu = exclusive();
    let img = image(8, 8);
    let src = GpuImage::upload(ctx, &img).unwrap();
    let run = |n: usize| {
        let lut = Arc::new(parse_cube(&cube(n, None)).unwrap());
        let op = PointOp::Lut { lut, space: LutSpace::Linear, strength: 1.0 };
        apply_point_ops(ctx, &src, std::slice::from_ref(&op)).unwrap();
    };
    for n in 2..=41 {
        run(n);
    }
    let before = ctx.pipelines_created();
    run(41);
    assert_eq!(ctx.pipelines_created(), before, "a recent size is still cached");
    run(2);
    assert_eq!(ctx.pipelines_created(), before + 1, "the least recently used size was evicted and is rebuilt");
}
