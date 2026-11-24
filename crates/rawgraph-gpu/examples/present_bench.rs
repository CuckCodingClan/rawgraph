#![allow(clippy::chunks_exact_to_as_chunks)] // runtime-sized pixel slices; as_chunks would not read better here

//! Benchmark of the wave-6 application path (2560x1707 only, min / median of 10 runs):
//! `cargo run --release -p rawgraph-gpu --example present_bench`
//!
//! Refuses to run with less than 700 MiB of free video memory (checked through `nvidia-smi`; when the
//! tool is missing the check is skipped by the caller, not here).

use std::process::Command;
use std::time::Instant;

use rawgraph_core::ImageBuf;
use rawgraph_gpu::{display_samples, render_into, DisplayTarget, GpuContext, GpuImage, GpuToneMap, PointOp};
use rawgraph_scopes::{analyze, DisplayTransform, ScopeRequest, ToneMap};

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

fn stats(mut f: impl FnMut()) -> (f64, f64) {
    let mut ms: Vec<f64> = (0..RUNS)
        .map(|_| {
            let t = Instant::now();
            f();
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

fn main() {
    match free_vram_mib() {
        Some(free) if free >= 700 => println!("free video memory: {free} MiB"),
        Some(free) => {
            println!("skipped: only {free} MiB of video memory free (need 700)");
            return;
        }
        None => {
            println!("skipped: free video memory could not be determined");
            return;
        }
    }
    let ctx = match GpuContext::new() {
        Ok(c) => c,
        Err(e) => {
            println!("skipped: no GPU adapter ({e})");
            return;
        }
    };
    println!("GPU: {}", ctx.adapter_summary());
    let img = synthetic();
    let base = GpuImage::upload(&ctx, &img).expect("upload");
    let scratch = GpuImage::new_empty(&ctx, W, H).expect("scratch");
    let target = DisplayTarget::new(&ctx, W, H).expect("target");
    let ops = vec![
        PointOp::Gain(1.6),
        PointOp::Matrix([[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]),
        PointOp::Saturation(1.3),
        PointOp::Contrast { amount: 0.45, pivot: 0.18 },
        PointOp::HighlightsShadows { highlights: -0.5, shadows: 0.35 },
    ];
    println!("{W}x{H}, min / median of {RUNS} runs in ms\n");

    let mut ev = 0.0f32;
    // Warm-up builds the pipelines and the sample targets.
    render_into(&ctx, &base, &scratch, &target, &ops, 0.0, GpuToneMap::AcesFit).expect("render");
    ctx.wait_idle();
    let (mn, md) = stats(|| {
        ev += 0.01;
        render_into(&ctx, &base, &scratch, &target, &ops, ev, GpuToneMap::AcesFit).expect("render");
        ctx.wait_idle();
    });
    println!("GPU render_into, 5 ops + display, submit + wait: {mn:8.2} / {md:8.2}");

    for step in [1u32, 2, 4] {
        display_samples(&ctx, &base, 0.0, GpuToneMap::AcesFit, step).expect("samples");
        let (mn, md) = stats(|| {
            ev += 0.01;
            let d = display_samples(&ctx, &base, ev, GpuToneMap::AcesFit, step).expect("samples");
            std::hint::black_box(d);
        });
        println!("GPU display_samples step {step}:                   {mn:8.2} / {md:8.2}");
    }

    let t = DisplayTransform { exposure_ev: 0.0, tone_map: ToneMap::AcesFit };
    let req = |step: usize| ScopeRequest {
        waveform: Some((512, 256)),
        vectorscope: Some(256),
        histogram: Some(256),
        clip_stats: true,
        step,
        ..Default::default()
    };
    for step in [1usize, 2, 4] {
        let (mn, md) = stats(|| {
            let (disp, rgba) = t.apply_with_rgba8(&img);
            let s = analyze(&disp, &req(step));
            std::hint::black_box((rgba, s));
        });
        println!("CPU apply_with_rgba8 + analyze step {step}:        {mn:8.2} / {md:8.2}");
    }
    // The GPU scopes path = display_samples + analyze at step 1 on the small image.
    for step in [2u32, 4] {
        let (mn, md) = stats(|| {
            ev += 0.01;
            let d = display_samples(&ctx, &base, ev, GpuToneMap::AcesFit, step).expect("samples");
            std::hint::black_box(analyze(&d, &req(1)));
        });
        println!("GPU display_samples + analyze, step {step}:         {mn:8.2} / {md:8.2}");
    }
}
