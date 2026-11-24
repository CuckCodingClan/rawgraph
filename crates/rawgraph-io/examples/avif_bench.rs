//! Times `save_avif8` on a synthetic 6000x4000 image (smooth gradients plus fine noise).
//!
//! Usage: `cargo run --release -p rawgraph-io --example avif_bench -- [out.avif] [width height]`

use std::time::Instant;

use rawgraph_core::color::OutputSpace;
use rawgraph_io::{save_avif8, SaveOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args.next().map(std::path::PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("rawgraph_avif_bench.avif"));
    let w: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(6000);
    let h: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(4000);
    let mut px = Vec::with_capacity(w as usize * h as usize * 3);
    let mut seed = 12345u32;
    for y in 0..h {
        for x in 0..w {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = ((seed >> 24) & 15) as i32 - 8;
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let v = |c: f32| (c * 255.0) as i32 + n;
            px.extend_from_slice(&[v(fx).clamp(0, 255) as u8, v(fy).clamp(0, 255) as u8, v((fx * fy).sqrt()).clamp(0, 255) as u8]);
        }
    }
    let options = SaveOptions { jpeg_quality: 80, overwrite: true, space: OutputSpace::DisplayP3, ..Default::default() };
    let t = Instant::now();
    save_avif8(&out, w, h, &px, &options).expect("avif write");
    let secs = t.elapsed().as_secs_f64();
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    println!("{w}x{h} quality 80: {secs:.1} s, {:.2} MB -> {}", size as f64 / 1e6, out.display());
    let _ = std::fs::remove_file(&out);
}
