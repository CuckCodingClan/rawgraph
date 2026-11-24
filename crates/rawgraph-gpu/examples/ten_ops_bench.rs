#![allow(clippy::chunks_exact_to_as_chunks)] // runtime-sized pixel slices; as_chunks would not read better here

//! Benchmark of a chain of all ten point-op kinds at 2560x1707 (min / median of 10 runs, release):
//! `cargo run --release -p rawgraph-gpu --example ten_ops_bench`
//!
//! GPU: `render_into` (op pass + display) until the queue is idle. CPU: core + fx functions, then
//! `DisplayTransform::apply_with_rgba8`. Refuses to run with less than 700 MiB of free video memory
//! (checked through `nvidia-smi`).

use std::process::Command;
use std::time::Instant;

use rawgraph_core::{ops, ImageBuf};
use rawgraph_gpu::{render_into, DisplayTarget, GpuContext, GpuImage, GpuToneMap, PointOp};
use rawgraph_scopes::{DisplayTransform, ToneMap};

const RUNS: usize = 10;
const W: u32 = 2560;
const H: u32 = 1707;

fn free_vram_mib() -> Option<u64> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used,memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.lines().next()?.split(',').map(|v| v.trim().parse::<u64>());
    let (used, total) = (it.next()?.ok()?, it.next()?.ok()?);
    Some(total.saturating_sub(used))
}

fn stats(mut f: impl FnMut(usize)) -> (f64, f64) {
    let mut ms: Vec<f64> = (0..RUNS)
        .map(|i| {
            let t = Instant::now();
            f(i);
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    ms.sort_by(|a, b| a.total_cmp(b));
    (ms[0], (ms[RUNS / 2 - 1] + ms[RUNS / 2]) / 2.0)
}

fn synthetic() -> ImageBuf {
    let mut img = ImageBuf::new(W, H);
    for (y, row) in img.data_mut().chunks_exact_mut(W as usize * 4).enumerate() {
        let fy = y as f32 / H as f32;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let fx = x as f32 / W as f32;
            px[0] = (-10.0 + 14.0 * fx).exp2();
            px[1] = fy * 3.0 - 0.3;
            px[2] = if (x / 64 + y / 64) % 2 == 0 { 0.02 } else { 3.0 };
            px[3] = 1.0;
        }
    }
    img
}

fn chain(shift: f32) -> Vec<PointOp> {
    vec![
        PointOp::Gain(1.3),
        PointOp::Matrix([[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]),
        PointOp::Cdl { slope: [1.1, 0.95, 1.05], offset: [0.01, 0.0, -0.01], power: [0.95, 1.05, 1.0] },
        PointOp::ToneCurve { shadows: 0.3 + shift, darks: -0.2, lights: 0.25, highlights: -0.4 },
        PointOp::HueSaturation {
            bands: [[0.1, 0.2, 0.0], [0.0, 0.3, 0.1], [-0.2, 0.1, 0.0], [0.0, -0.2, 0.0], [0.2, 0.2, -0.1], [0.0, 0.1, 0.0]],
        },
        PointOp::PointCurve { points: vec![[0.0, 0.0], [0.25, 0.15 + shift * 0.1], [0.6, 0.72], [1.0, 1.0]], channel: 0 },
        PointOp::Saturation(1.15),
        PointOp::Contrast { amount: 0.3, pivot: 0.18 },
        PointOp::HighlightsShadows { highlights: -0.4, shadows: 0.3 },
        PointOp::Vignette { amount: -0.5, midpoint: 0.55, feather: 0.6 },
    ]
}

fn cpu_chain(img: &ImageBuf) -> Vec<u8> {
    let mut out = img.clone();
    ops::apply_gain(&mut out, 1.3);
    ops::apply_matrix(&mut out, [[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]);
    // The CDL has no CPU function in the workspace yet: the same per-channel formula inline.
    let (s, o, p) = ([1.1f32, 0.95, 1.05], [0.01f32, 0.0, -0.01], [0.95f32, 1.05, 1.0]);
    for px in out.data_mut().chunks_exact_mut(4) {
        for c in 0..3 {
            let v = px[c] * s[c] + o[c];
            px[c] = if v > 0.0 { v.powf(p[c]) } else { v };
        }
    }
    rawgraph_fx::tone_curve(&mut out, 0.3, -0.2, 0.25, -0.4);
    rawgraph_fx::hue_saturation(
        &mut out,
        &[[0.1, 0.2, 0.0], [0.0, 0.3, 0.1], [-0.2, 0.1, 0.0], [0.0, -0.2, 0.0], [0.2, 0.2, -0.1], [0.0, 0.1, 0.0]],
    );
    rawgraph_fx::point_curve(&mut out, &[[0.0, 0.0], [0.25, 0.15], [0.6, 0.72], [1.0, 1.0]], rawgraph_fx::CurveChannel::Luminance);
    ops::saturation(&mut out, 1.15);
    ops::contrast(&mut out, 0.3, 0.18);
    ops::highlights_shadows(&mut out, -0.4, 0.3);
    rawgraph_fx::vignette(&mut out, -0.5, 0.55, 0.6);
    DisplayTransform { exposure_ev: 0.0, tone_map: ToneMap::SoftHighlights }.apply_with_rgba8(&out).1
}

fn main() {
    match free_vram_mib() {
        Some(free) if free >= 700 => println!("free video memory: {free} MiB"),
        Some(free) => {
            println!("only {free} MiB of video memory free (< 700): benchmark skipped");
            return;
        }
        None => {
            println!("nvidia-smi not available: benchmark skipped");
            return;
        }
    }
    let ctx = match GpuContext::new() {
        Ok(c) => c,
        Err(e) => {
            println!("no GPU: {e}");
            return;
        }
    };
    println!("{}", ctx.adapter_summary());
    let img = synthetic();
    let base = GpuImage::upload(&ctx, &img).expect("upload");
    let scratch = GpuImage::new_empty(&ctx, W, H).expect("scratch");
    let target = DisplayTarget::new(&ctx, W, H).expect("target");
    let fixed = chain(0.0);
    // Warm-up: pipeline compilation and first table upload are not part of the numbers.
    render_into(&ctx, &base, &scratch, &target, &fixed, 0.0, GpuToneMap::SoftHighlights).expect("render");
    ctx.wait_idle();

    let (mn, med) = stats(|_| {
        render_into(&ctx, &base, &scratch, &target, &fixed, 0.0, GpuToneMap::SoftHighlights).expect("render");
        ctx.wait_idle();
    });
    println!("GPU render_into, ten op kinds, parameters unchanged: min {mn:.2} ms, median {med:.2} ms");

    // A slider drag: the tone curve and the point curve change every run (both tables are rewritten).
    let (mn, med) = stats(|i| {
        let ops = chain(0.01 * (i + 1) as f32);
        render_into(&ctx, &base, &scratch, &target, &ops, 0.0, GpuToneMap::SoftHighlights).expect("render");
        ctx.wait_idle();
    });
    println!("GPU render_into, ten op kinds, both curves change every run: min {mn:.2} ms, median {med:.2} ms");

    let (mn, med) = stats(|_| {
        std::hint::black_box(cpu_chain(&img));
    });
    println!("CPU core + fx chain + apply_with_rgba8: min {mn:.2} ms, median {med:.2} ms");
}
