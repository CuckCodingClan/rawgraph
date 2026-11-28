#![allow(clippy::chunks_exact_to_as_chunks)] // runtime-sized pixel slices; as_chunks would not read better here

//! Neighbourhood passes (local tone, filter, sharpen) and chains against the CPU reference: every
//! result must be within one code value of the CPU after the display transform. Without a GPU
//! adapter every test prints "skipped: no GPU adapter" and passes. Small images only.

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use rawgraph_core::ops;
use rawgraph_core::ImageBuf;
use rawgraph_gpu::{
    apply_chain_to_cpu, display_rgba8, run_chain, ChainScratch, FilterOp, GpuContext, GpuImage, GpuNode, GpuToneMap, Loc,
    LocalToneOp, PointOp, SharpenOp,
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

/// One test at a time on the shared device (see `gpu.rs`).
fn exclusive() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

macro_rules! gpu {
    () => {
        match shared() {
            Some(c) => (c, exclusive()),
            None => {
                println!("skipped: no GPU adapter");
                return;
            }
        }
    };
}

/// Detail (fine checker and noise-like texture), gradients, saturated colours, a bright HDR patch
/// and a dark region: what the neighbourhood operations react to.
fn photo_like(w: u32, h: u32) -> ImageBuf {
    let mut img = ImageBuf::new(w, h);
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / (w - 1) as f32, y as f32 / (h - 1) as f32);
            // Large smooth scene: a bright sky on top, dark ground below, a vertical edge in the middle.
            let sky = 0.2 + 3.0 * (1.0 - fy).powi(2);
            let ground = 0.01 + 0.08 * fx;
            let base = if fy < 0.45 { sky } else { ground };
            let edge = if fx > 0.55 && fy > 0.3 && fy < 0.8 { 4.0 } else { 1.0 };
            let texture = 1.0 + 0.25 * ((x as f32 * 0.9).sin() * (y as f32 * 1.3).cos()) + 0.1 * (rnd() - 0.5);
            let checker = if (x / 3 + y / 3) % 2 == 0 { 1.0 } else { 0.8 };
            let v = base * edge * texture * checker;
            let px = match (x * 5 + y * 3) % 61 {
                0 => [2.0, 0.02, 0.01],
                1 => [0.01, 1.5, 0.05],
                2 => [0.02, 0.05, 3.0],
                _ => [v, v * (0.5 + 0.5 * fx), v * (0.3 + 0.7 * fy)],
            };
            img.set_pixel(x, y, [px[0], px[1], px[2], 1.0]);
        }
    }
    img
}

fn max_code_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len());
    a.iter().zip(b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0)
}

/// Largest difference in 8-bit codes after the display transform, over the tone maps and a few exposures.
fn display_diff(ctx: &GpuContext, gpu: &ImageBuf, cpu: &ImageBuf) -> u8 {
    let up = GpuImage::upload(ctx, gpu).unwrap();
    let mut worst = 0;
    for (gt, ct) in [(GpuToneMap::Clip, ToneMap::Clip), (GpuToneMap::SoftHighlights, ToneMap::SoftHighlights), (GpuToneMap::AcesFit, ToneMap::AcesFit)] {
        for ev in [-1.5f32, 0.0, 1.0] {
            let g = display_rgba8(ctx, &up, ev, gt).unwrap();
            let c = DisplayTransform { exposure_ev: ev, tone_map: ct }.apply_with_rgba8(cpu).1;
            worst = worst.max(max_code_diff(&g, &c));
        }
    }
    worst
}

fn check(ctx: &GpuContext, name: &str, img: &ImageBuf, nodes: &[GpuNode], cpu: &ImageBuf) {
    let base = GpuImage::upload(ctx, img).unwrap();
    let got = apply_chain_to_cpu(ctx, &base, nodes).unwrap();
    let d = display_diff(ctx, &got, cpu);
    let raw = got.data().iter().zip(cpu.data()).map(|(a, b)| (a - b).abs() / b.abs().max(1.0)).fold(0.0f32, f32::max);
    println!("{name}: max code diff {d}, max relative float diff {raw:.2e}");
    assert!(d <= 1, "{name}: {d} code values off the CPU");
    // The float result too, not only its 8-bit picture (measured: below 2e-6 everywhere).
    assert!(raw <= 1e-4, "{name}: relative float difference {raw:.2e}");
}

/// A grid id that no other test (or image size) shares.
fn unique_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1000);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn cpu_high(img: &ImageBuf, sigma: f32, box_kernel: bool) -> ImageBuf {
    let low = ops::filter_low(img, sigma, box_kernel);
    let mut out = low;
    for (o, orig) in out.data_mut().chunks_exact_mut(4).zip(img.data().chunks_exact(4)) {
        for c in 0..3 {
            o[c] = 0.5 + orig[c] - o[c];
        }
        o[3] = orig[3];
    }
    out
}

fn local_tone_node(id: u64, img: &ImageBuf, v: [f32; 4]) -> GpuNode {
    let grid = Arc::new(ops::local_tone_grid(img).unwrap());
    GpuNode::LocalTone(LocalToneOp { id, grid, highlights: v[0], shadows: v[1], whites: v[2], blacks: v[3] })
}

#[test]
fn local_tone_matches_the_cpu() {
    let (ctx, _g) = gpu!();
    // 520 px wide: the grid is area-resized to 512 and sampled back bilinearly.
    for (w, h) in [(160u32, 100u32), (520, 150)] {
        let img = photo_like(w, h);
        for (i, v) in [[-0.6, 0.5, 0.2, -0.2], [0.0, 0.0, 0.0, 0.0], [-1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, -1.0], [1.0, -1.0, -0.5, 0.7]].into_iter().enumerate() {
            let mut cpu = img.clone();
            ops::develop_local_tone(&mut cpu, v[0], v[1], v[2], v[3]);
            let _ = i;
            check(ctx, &format!("local tone {w}x{h} {v:?}"), &img, &[local_tone_node(unique_id(), &img, v)], &cpu);
        }
    }
}

#[test]
fn filter_matches_the_cpu() {
    let (ctx, _g) = gpu!();
    let small = photo_like(150, 90);
    // Exact full-resolution path (radius < 16).
    for (sigma, box_kernel, high) in [(3.0f32, false, false), (3.4, true, false), (7.0, false, true), (1.0, true, true), (15.0, false, false)] {
        let cpu = if high { cpu_high(&small, sigma, box_kernel) } else { ops::filter_low(&small, sigma, box_kernel) };
        check(ctx, &format!("filter sigma {sigma} box {box_kernel} high {high}"), &small, &[GpuNode::Filter(FilterOp { sigma, box_kernel, high })], &cpu);
    }
    // Radius 0 is the identity for the low pass and 0.5 for the high pass.
    check(ctx, "filter radius 0 low", &small, &[GpuNode::Filter(FilterOp { sigma: 0.3, box_kernel: false, high: false })], &small);
    check(ctx, "filter radius 0 high", &small, &[GpuNode::Filter(FilterOp { sigma: 0.0, box_kernel: false, high: true })], &cpu_high(&small, 0.0, false));
    // Low-resolution path (radius >= 16): scale 2 (16..31), 4 (32..63).
    let big = photo_like(300, 200);
    for (sigma, box_kernel, high) in [(16.0f32, false, false), (20.0, false, true), (24.4, true, false), (40.0, false, false), (45.0, false, true)] {
        let cpu = if high { cpu_high(&big, sigma, box_kernel) } else { ops::filter_low(&big, sigma, box_kernel) };
        check(ctx, &format!("filter sigma {sigma} box {box_kernel} high {high}"), &big, &[GpuNode::Filter(FilterOp { sigma, box_kernel, high })], &cpu);
    }
}

#[test]
fn sharpen_matches_the_cpu() {
    let (ctx, _g) = gpu!();
    let img = photo_like(300, 200);
    // radius < 4: true Gaussian; 4..16: box blur at full size; >= 16: low-resolution blur.
    for (amount, radius, threshold) in [(0.8f32, 1.5f32, 0.0f32), (1.0, 3.9, 0.2), (-0.5, 5.0, 0.0), (1.0, 9.0, 0.3), (1.0, 20.0, 0.0), (0.6, 33.0, 0.1)] {
        let cpu = rawgraph_fx::sharpen(&img, amount, radius, threshold, None);
        check(ctx, &format!("sharpen {amount} r {radius} t {threshold}"), &img, &[GpuNode::Sharpen(SharpenOp { amount, radius_px: radius, threshold })], &cpu);
    }
    // No-ops stay no-ops.
    check(ctx, "sharpen amount 0", &img, &[GpuNode::Sharpen(SharpenOp { amount: 0.0, radius_px: 3.0, threshold: 0.0 })], &img);
    check(ctx, "sharpen radius 0", &img, &[GpuNode::Sharpen(SharpenOp { amount: 1.0, radius_px: 0.0, threshold: 0.0 })], &img);
}

#[test]
fn chains_mixing_pointwise_and_neighbourhood_nodes_match_the_cpu() {
    let (ctx, _g) = gpu!();
    let img = photo_like(260, 180);
    // gain -> local tone (grid of the gained image) -> saturation -> filter high -> sharpen -> gain.
    let mut cpu = img.clone();
    ops::apply_gain(&mut cpu, 1.4);
    let grid_node = local_tone_node(7, &cpu, [-0.5, 0.4, 0.1, -0.1]);
    ops::develop_local_tone(&mut cpu, -0.5, 0.4, 0.1, -0.1);
    ops::saturation(&mut cpu, 1.3);
    let cpu_before_high = cpu.clone();
    cpu = cpu_high(&cpu_before_high, 5.0, false);
    cpu = rawgraph_fx::sharpen(&cpu, 0.7, 2.5, 0.1, None);
    ops::apply_gain(&mut cpu, 0.8);
    let nodes = vec![
        GpuNode::Point(vec![PointOp::Gain(1.4)]),
        grid_node,
        GpuNode::Point(vec![PointOp::Saturation(1.3)]),
        GpuNode::Filter(FilterOp { sigma: 5.0, box_kernel: false, high: true }),
        GpuNode::Sharpen(SharpenOp { amount: 0.7, radius_px: 2.5, threshold: 0.1 }),
        GpuNode::Point(vec![PointOp::Gain(0.8)]),
    ];
    check(ctx, "mixed chain", &img, &nodes, &cpu);

    // Low pass on a low-resolution radius between pointwise nodes, then sharpen.
    let mut cpu2 = img.clone();
    ops::apply_gain(&mut cpu2, 0.7);
    cpu2 = ops::filter_low(&cpu2, 18.0, false);
    cpu2 = rawgraph_fx::sharpen(&cpu2, 1.0, 6.0, 0.0, None);
    let nodes2 = vec![
        GpuNode::Point(vec![PointOp::Gain(0.7)]),
        GpuNode::Filter(FilterOp { sigma: 18.0, box_kernel: false, high: false }),
        GpuNode::Sharpen(SharpenOp { amount: 1.0, radius_px: 6.0, threshold: 0.0 }),
    ];
    check(ctx, "low-res filter chain", &img, &nodes2, &cpu2);
}

#[test]
fn the_pool_keeps_only_what_the_frame_needs() {
    let (ctx, _g) = gpu!();
    let img = photo_like(120, 80);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let mut sc = ChainScratch::new();
    let point = [GpuNode::Point(vec![PointOp::Gain(2.0)])];
    assert_eq!(run_chain(ctx, &base, &mut sc, &[]).unwrap(), Loc::Base);
    assert_eq!(sc.image_count(), 0);
    assert_eq!(run_chain(ctx, &base, &mut sc, &point).unwrap(), Loc::Scratch(0));
    assert_eq!(sc.image_count(), 1);
    // A high pass on the base ping-pongs between two textures and needs no third one.
    let high = [GpuNode::Filter(FilterOp { sigma: 3.0, box_kernel: false, high: true })];
    run_chain(ctx, &base, &mut sc, &high).unwrap();
    assert_eq!(sc.image_count(), 2);
    assert!(!sc.has_small());
    // A high pass behind another node needs the original, the ping-pong pair: three.
    let after_point = [point[0].clone(), high[0].clone()];
    run_chain(ctx, &base, &mut sc, &after_point).unwrap();
    assert_eq!(sc.image_count(), 3);
    // Large radius: the low-resolution pair appears and is freed with the next frame that does not need it.
    run_chain(ctx, &base, &mut sc, &[GpuNode::Filter(FilterOp { sigma: 20.0, box_kernel: false, high: false })]).unwrap();
    assert!(sc.has_small());
    run_chain(ctx, &base, &mut sc, &point).unwrap();
    assert!(!sc.has_small());
    assert_eq!(sc.image_count(), 1);
    // Same chain again: nothing is allocated (local tone grid included).
    let lt = [local_tone_node(42, &img, [0.3, 0.2, 0.0, 0.0]), high[0].clone()];
    run_chain(ctx, &base, &mut sc, &lt).unwrap();
    let before = ctx.allocations_created();
    let pipelines = ctx.pipelines_created();
    let lt2 = [local_tone_node(42, &img, [0.5, -0.2, 0.1, 0.0]), high[0].clone()];
    run_chain(ctx, &base, &mut sc, &lt2).unwrap();
    assert_eq!(ctx.allocations_created(), before, "a slider drag allocates nothing");
    assert_eq!(ctx.pipelines_created(), pipelines, "and compiles nothing");
}

#[test]
fn a_changed_grid_id_uploads_the_new_grid() {
    let (ctx, _g) = gpu!();
    let img = photo_like(120, 80);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let dark = {
        let mut d = img.clone();
        ops::apply_gain(&mut d, 0.1);
        d
    };
    let v = [-0.8, 0.8, 0.0, 0.0];
    let a = apply_chain_to_cpu(ctx, &base, &[local_tone_node(900, &img, v)]).unwrap();
    // Same id: the cached grid wins even if the data differs (ids identify the data).
    let same = apply_chain_to_cpu(ctx, &base, &[local_tone_node(900, &dark, v)]).unwrap();
    assert_eq!(a, same);
    let other = apply_chain_to_cpu(ctx, &base, &[local_tone_node(901, &dark, v)]).unwrap();
    assert_ne!(a, other);
    let mut cpu = img.clone();
    let grid = ops::local_tone_grid(&dark).unwrap();
    assert_eq!((grid.width, grid.height), (120, 80));
    ops::develop_local_tone(&mut cpu, v[0], v[1], v[2], v[3]);
    assert!(display_diff(ctx, &a, &cpu) <= 1);
}

/// Negative and out-of-gamut pixels in the picture (scene-referred data can have them).
fn with_negatives(mut img: ImageBuf) -> ImageBuf {
    let (w, h) = (img.width(), img.height());
    for y in 0..h {
        for x in 0..w {
            if (x * 7 + y * 3) % 17 == 0 {
                img.set_pixel(x, y, [-0.2, 0.05, 0.4, 1.0]);
            }
        }
    }
    img
}

#[test]
fn the_low_resolution_filter_path_handles_awkward_sizes() {
    let (ctx, _g) = gpu!();
    // Odd sizes (partial border blocks), a scale of 8 and 16, images narrower than the scale, one pixel wide or high.
    for (w, h, sigma) in [(301u32, 203u32, 20.0f32), (257, 131, 33.0), (300, 200, 70.0), (310, 190, 140.0), (3, 200, 70.0), (5, 120, 40.0), (200, 3, 70.0), (1, 64, 20.0), (64, 1, 20.0), (1, 1, 20.0), (2, 2, 16.0)] {
        let img = with_negatives(photo_like(w.max(2), h.max(2)).clone());
        let img = if w == 1 || h == 1 { crop_to(&img, w, h) } else { img };
        for (box_kernel, high) in [(false, false), (true, true), (false, true)] {
            let cpu = if high { cpu_high(&img, sigma, box_kernel) } else { ops::filter_low(&img, sigma, box_kernel) };
            check(ctx, &format!("{w}x{h} sigma {sigma} box {box_kernel} high {high}"), &img, &[GpuNode::Filter(FilterOp { sigma, box_kernel, high })], &cpu);
        }
    }
}

fn crop_to(img: &ImageBuf, w: u32, h: u32) -> ImageBuf {
    let mut out = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(x, y, img.pixel(x, y));
        }
    }
    out
}

#[test]
fn small_and_negative_inputs_match_for_the_other_operations() {
    let (ctx, _g) = gpu!();
    for (w, h) in [(1u32, 40u32), (40, 1), (7, 5), (301, 203)] {
        let img = crop_to(&with_negatives(photo_like(301.max(w), 203.max(h))), w, h);
        let cpu = rawgraph_fx::sharpen(&img, 0.9, 6.0, 0.1, None);
        check(ctx, &format!("sharpen {w}x{h}"), &img, &[GpuNode::Sharpen(SharpenOp { amount: 0.9, radius_px: 6.0, threshold: 0.1 })], &cpu);
        let cpu = rawgraph_fx::sharpen(&img, 0.9, 2.0, 0.0, None);
        check(ctx, &format!("sharpen small radius {w}x{h}"), &img, &[GpuNode::Sharpen(SharpenOp { amount: 0.9, radius_px: 2.0, threshold: 0.0 })], &cpu);
        let mut cpu = img.clone();
        ops::develop_local_tone(&mut cpu, -0.5, 0.6, 0.2, -0.3);
        check(ctx, &format!("local tone {w}x{h}"), &img, &[local_tone_node(unique_id(), &img, [-0.5, 0.6, 0.2, -0.3])], &cpu);
    }
}

#[test]
fn a_failed_frame_frees_the_pool_and_a_released_pool_comes_back() {
    let (ctx, _g) = gpu!();
    let img = photo_like(64, 48);
    let base = GpuImage::upload(ctx, &img).unwrap();
    let mut sc = ChainScratch::new();
    // A grid whose data does not match its size fails after the first node has taken a texture.
    let bad = GpuNode::LocalTone(LocalToneOp {
        id: unique_id(),
        grid: Arc::new(ops::LocalToneGrid { width: 4, height: 4, coeff: vec![0.0; 3] }),
        highlights: 0.1,
        shadows: 0.0,
        whites: 0.0,
        blacks: 0.0,
    });
    let nodes = [GpuNode::Point(vec![PointOp::Gain(2.0)]), GpuNode::Filter(FilterOp { sigma: 20.0, box_kernel: false, high: true }), bad];
    assert!(run_chain(ctx, &base, &mut sc, &nodes).is_err());
    assert_eq!(sc.image_count(), 0, "an error leaves nothing allocated");
    assert!(!sc.has_small());
    run_chain(ctx, &base, &mut sc, &nodes[..2]).unwrap();
    assert!(sc.image_count() > 0);
    sc.release();
    assert_eq!((sc.image_count(), sc.has_small()), (0, false));
    // And the pool works again after a release.
    assert_eq!(run_chain(ctx, &base, &mut sc, &nodes[..1]).unwrap(), Loc::Scratch(0));
}

#[test]
fn warm_up_compiles_the_neighbourhood_pipelines_once() {
    let _g = exclusive();
    let Ok(ctx) = GpuContext::new() else {
        println!("skipped: no GPU adapter");
        return;
    };
    let before = ctx.pipelines_created();
    rawgraph_gpu::warm_up_neighbourhood(&ctx);
    assert_eq!(ctx.pipelines_created() - before, 6);
    let img = photo_like(300, 200);
    let base = GpuImage::upload(&ctx, &img).unwrap();
    let nodes = [
        local_tone_node(unique_id(), &img, [0.3, 0.2, 0.1, 0.0]),
        GpuNode::Filter(FilterOp { sigma: 20.0, box_kernel: false, high: true }),
        GpuNode::Sharpen(SharpenOp { amount: 0.5, radius_px: 6.0, threshold: 0.0 }),
    ];
    let after = ctx.pipelines_created();
    apply_chain_to_cpu(&ctx, &base, &nodes).unwrap();
    assert_eq!(ctx.pipelines_created(), after, "the first drag and the first 16 px crossing compile nothing");
    rawgraph_gpu::warm_up_neighbourhood(&ctx);
    assert_eq!(ctx.pipelines_created(), after, "a second warm-up is free");
}
