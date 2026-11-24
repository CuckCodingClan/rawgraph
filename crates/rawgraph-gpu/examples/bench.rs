#![allow(clippy::chunks_exact_to_as_chunks)] // runtime-sized pixel slices; as_chunks would not read better here

//! GPU vs CPU benchmark. Run with:
//! `cargo run --release -p rawgraph-gpu --example bench`

use std::time::Instant;

use rawgraph_core::ops;
use rawgraph_core::ImageBuf;
use rawgraph_gpu::{
    apply_point_ops, apply_point_ops_into, display_into, display_rgba8, display_texture, GpuContext, GpuImage,
    GpuToneMap, PointOp,
};
use rawgraph_scopes::{DisplayTransform, ToneMap};

const RUNS: usize = 10;

fn chain() -> Vec<PointOp> {
    vec![
        PointOp::Gain(1.6),
        PointOp::Matrix([[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]),
        PointOp::Saturation(1.3),
        PointOp::Contrast { amount: 0.45, pivot: 0.18 },
        PointOp::HighlightsShadows { highlights: -0.5, shadows: 0.35 },
    ]
}

fn cpu_chain(img: &mut ImageBuf) {
    ops::apply_gain(img, 1.6);
    ops::apply_matrix(img, [[1.05, -0.03, -0.02], [0.01, 0.98, 0.01], [-0.02, 0.04, 0.98]]);
    ops::saturation(img, 1.3);
    ops::contrast(img, 0.45, 0.18);
    ops::highlights_shadows(img, -0.5, 0.35);
}

/// (min, median) in milliseconds over `RUNS` runs; `f` returns the elapsed time itself so that
/// setup can be excluded.
fn stats(mut f: impl FnMut() -> f64) -> (f64, f64) {
    let mut ms: Vec<f64> = (0..RUNS).map(|_| f()).collect();
    ms.sort_by(|a, b| a.total_cmp(b));
    (ms[0], (ms[RUNS / 2 - 1] + ms[RUNS / 2]) / 2.0)
}

fn timed(f: impl FnOnce()) -> f64 {
    let t = Instant::now();
    f();
    t.elapsed().as_secs_f64() * 1000.0
}

fn synthetic(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    for (y, row) in img.data_mut().chunks_exact_mut(w as usize * 4).enumerate() {
        let fy = y as f32 / h as f32;
        for (x, px) in row.chunks_exact_mut(4).enumerate() {
            let fx = x as f32 / w as f32;
            px[0] = (-10.0 + 14.0 * fx).exp2();
            px[1] = fy * 3.0 - 0.3;
            px[2] = if (x / 64 + y / 64) % 2 == 0 { 0.02 } else { 3.0 };
        }
    }
    img
}

fn main() {
    let ctx = match GpuContext::new() {
        Ok(c) => c,
        Err(e) => {
            println!("skipped: no GPU adapter ({e})");
            return;
        }
    };
    println!("GPU: {}", ctx.adapter_summary());
    println!("runs per measurement: {RUNS}; values are min / median in ms\n");
    let ops_list = chain();
    let mut rows: Vec<(String, Vec<String>)> = Vec::new();
    let labels = [
        "GPU upload",
        "GPU apply_point_ops (5 ops)",
        "GPU display_texture (no readback)",
        "GPU slider path: ops + display_texture",
        "GPU display_rgba8 (with readback)",
        "GPU download (Rgba32Float)",
        "GPU ops, pipelined (per pass of 40)",
        "GPU display_texture, pipelined (per pass of 40)",
        "GPU slider path, pipelined (per pass of 40)",
        "CPU 5-op chain",
        "CPU display (apply_with_rgba8)",
        "CPU chain + display",
    ];
    for l in labels {
        rows.push((l.to_string(), Vec::new()));
    }
    // `bench small` skips 8192x5464 (it needs about 1.6 GB of free VRAM and many minutes when
    // the GPU is shared).
    let all = [(2560u32, 1707u32), (3840, 2160), (8192, 5464)];
    let sizes = if std::env::args().nth(1).as_deref() == Some("small") { &all[..2] } else { &all[..] };
    for &(w, h) in sizes {
        let img = synthetic(w, h);
        let mut cells: Vec<(f64, f64)> = Vec::new();

        // Warm-up: compiles the shaders and touches the driver paths; its cost is reported once.
        // Also the allocation test: the GPU may be shared with video encodes.
        let t = Instant::now();
        let warm_up = || -> Result<(), rawgraph_gpu::GpuError> {
            let src = GpuImage::upload(&ctx, &img)?;
            let warm = apply_point_ops(&ctx, &src, &ops_list)?;
            display_texture(&ctx, &warm, 0.0, GpuToneMap::AcesFit)?;
            ctx.wait_idle();
            Ok(())
        };
        let gpu_ok = match warm_up() {
            Ok(()) => {
                println!("{w}x{h}: first call including shader compilation: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
                true
            }
            Err(e) => {
                println!("{w}x{h}: GPU measurements skipped: {e}");
                false
            }
        };

        if gpu_ok {
            cells.push(stats(|| {
                let ms = timed(|| {
                    let g = GpuImage::upload(&ctx, &img).unwrap();
                    ctx.wait_idle();
                    drop(g);
                });
                ctx.wait_idle();
                ms
            }));
            let src = GpuImage::upload(&ctx, &img).unwrap();
            ctx.wait_idle();
            cells.push(stats(|| {
                timed(|| {
                    let out = apply_point_ops(&ctx, &src, &ops_list).unwrap();
                    ctx.wait_idle();
                    drop(out);
                })
            }));
            cells.push(stats(|| {
                timed(|| {
                    let t = display_texture(&ctx, &src, 0.5, GpuToneMap::AcesFit).unwrap();
                    ctx.wait_idle();
                    drop(t);
                })
            }));
            cells.push(stats(|| {
                timed(|| {
                    let out = apply_point_ops(&ctx, &src, &ops_list).unwrap();
                    let t = display_texture(&ctx, &out, 0.5, GpuToneMap::AcesFit).unwrap();
                    ctx.wait_idle();
                    drop((out, t));
                })
            }));
            cells.push(stats(|| {
                timed(|| {
                    let bytes = display_rgba8(&ctx, &src, 0.5, GpuToneMap::AcesFit).unwrap();
                    std::hint::black_box(bytes);
                })
            }));
            cells.push(stats(|| {
                timed(|| {
                    let back = src.download(&ctx).unwrap();
                    std::hint::black_box(back);
                })
            }));
            // The 8K case already fills the VRAM the other processes left free (the extra 700 MB
            // target pushes WDDM into paging and stalls for minutes), so it is skipped there.
            if w * h > 16_000_000 {
                cells.extend([(f64::NAN, f64::NAN); 3]);
            } else {
                // N back-to-back passes into preallocated targets, one wait: amortises the scheduling
                // latency when the GPU is shared with other processes (a transcription job here) and
                // is the shape of a real slider drag (no allocation per frame).
                const N: usize = 40;
                let dst = GpuImage::upload(&ctx, &ImageBuf::new(w, h)).unwrap();
                let shown = display_texture(&ctx, &src, 0.0, GpuToneMap::Clip).unwrap();
                ctx.wait_idle();
                cells.push(stats(|| {
                    timed(|| {
                        for _ in 0..N {
                            apply_point_ops_into(&ctx, &src, &dst, &ops_list).unwrap();
                        }
                        ctx.wait_idle();
                    }) / N as f64
                }));
                cells.push(stats(|| {
                    timed(|| {
                        for _ in 0..N {
                            display_into(&ctx, &src, &shown, 0.5, GpuToneMap::AcesFit).unwrap();
                        }
                        ctx.wait_idle();
                    }) / N as f64
                }));
                cells.push(stats(|| {
                    timed(|| {
                        for _ in 0..N {
                            apply_point_ops_into(&ctx, &src, &dst, &ops_list).unwrap();
                            display_into(&ctx, &dst, &shown, 0.5, GpuToneMap::AcesFit).unwrap();
                        }
                        ctx.wait_idle();
                    }) / N as f64
                }));
            }
        } else {
            cells.extend([(f64::NAN, f64::NAN); 9]);
        }
        let t = DisplayTransform { exposure_ev: 0.5, tone_map: ToneMap::AcesFit };
        let mut cpu_ops = Vec::new();
        let mut cpu_disp = Vec::new();
        for _ in 0..RUNS {
            let mut work = img.clone();
            cpu_ops.push(timed(|| cpu_chain(&mut work)));
            cpu_disp.push(timed(|| {
                std::hint::black_box(t.apply_with_rgba8(&work));
            }));
        }
        let summarize = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.total_cmp(b));
            (v[0], (v[RUNS / 2 - 1] + v[RUNS / 2]) / 2.0)
        };
        let both: Vec<f64> = cpu_ops.iter().zip(&cpu_disp).map(|(a, b)| a + b).collect();
        cells.push(summarize(cpu_ops));
        cells.push(summarize(cpu_disp));
        cells.push(summarize(both));

        for (row, (min, med)) in rows.iter_mut().zip(cells) {
            row.1.push(if min.is_nan() { format!("{:^19}", "n/a (VRAM)") } else { format!("{min:8.2} / {med:8.2}") });
        }
    }
    println!();
    println!("{:<42} | {:^19} | {:^19} | {:^19}", "min / median ms", "2560x1707", "3840x2160", "8192x5464");
    println!("{}", "-".repeat(42 + 3 * 22));
    for (label, cells) in rows {
        let cell = |i: usize| cells.get(i).cloned().unwrap_or_else(|| format!("{:^19}", "not run"));
        println!("{label:<42} | {} | {} | {}", cell(0), cell(1), cell(2));
    }
}
