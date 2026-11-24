//! Stage breakdown, before/after timings and quality comparison of the fast proxy path.
//!
//! "old" = full PPG demosaic, then box filter (`Mode::FullDemosaic`); "new" = `Mode::Auto`.
//! Usage: `cargo run --release -p rawgraph-io --example proxy_bench -- <folder>`

use std::path::PathBuf;

use rawgraph_io::raw::{load_with_mode, Mode, Stages};
use rawgraph_io::{is_supported, source_kind, LoadOptions, SourceKind};

fn run(path: &std::path::Path, max_dim: Option<u32>, mode: Mode) -> (rawgraph_io::Loaded, Stages, f64) {
    let opts = LoadOptions { max_dim, neutralize_clipped_highlights: std::env::var("NEUTRAL").map_or(true, |v| v != "0") };
    let t = std::time::Instant::now();
    let (l, st) = load_with_mode(path, &opts, mode).expect("decode");
    (l, st, t.elapsed().as_secs_f64() * 1000.0)
}

/// Median total time over 3 runs (first run warms the file cache).
fn median(path: &std::path::Path, max_dim: Option<u32>, mode: Mode) -> f64 {
    let _ = run(path, max_dim, mode);
    let mut v: Vec<f64> = (0..3).map(|_| run(path, max_dim, mode).2).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[1]
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("folder"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_supported(p) && source_kind(p) == Some(SourceKind::Raw))
        .collect();
    files.sort();
    let detail = std::env::var("DETAIL").ok();
    println!("file | old2560 | new2560 | old1280 | new1280 | mean|d| (rel to mean) | max|d| @2560");
    for p in &files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let (o25, n25) = (median(p, Some(2560), Mode::FullDemosaic), median(p, Some(2560), Mode::Auto));
        let (o12, n12) = (median(p, Some(1280), Mode::FullDemosaic), median(p, Some(1280), Mode::Auto));
        // Quality: new proxy vs old proxy.
        let (a, _, _) = run(p, Some(2560), Mode::FullDemosaic);
        let (b, _, _) = run(p, Some(2560), Mode::Auto);
        assert_eq!((a.image.width(), a.image.height()), (b.image.width(), b.image.height()), "{name}: sizes differ");
        assert_eq!(a.full_size, b.full_size);
        let (mut sum, mut mx, mut mean, mut big) = (0.0f64, 0.0f32, 0.0f64, 0u64);
        for (x, y) in a.image.data().as_chunks::<4>().0.iter().zip(b.image.data().as_chunks::<4>().0) {
            for k in 0..3 {
                let d = (x[k] - y[k]).abs();
                sum += d as f64;
                mx = mx.max(d);
                big += (d > 0.05) as u64;
                mean += x[k] as f64;
            }
        }
        let n = (a.image.width() as f64) * (a.image.height() as f64) * 3.0;
        println!("{name} | {o25:.0} | {n25:.0} | {o12:.0} | {n12:.0} | {:.5} ({:.2}%) | {mx:.3} | >0.05: {:.4}%", sum / n, sum / mean * 100.0, big as f64 / n * 100.0);
        if detail.as_deref().is_some_and(|d| name.contains(d)) {
            for (label, mode, md) in [("old", Mode::FullDemosaic, Some(2560)), ("new", Mode::Auto, Some(2560)), ("full-res", Mode::Auto, None)] {
                let _ = run(p, md, mode);
                let (_, st, tot) = run(p, md, mode);
                println!("   [{label} {md:?}] total {tot:.0} ms");
                for (s, ms) in st.0 {
                    println!("      {s:<34} {ms:7.1} ms");
                }
            }
        }
    }
}
