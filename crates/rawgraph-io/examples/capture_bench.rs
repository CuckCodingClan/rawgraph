//! Times `read_capture_info` (min of N runs) for every supported file in a folder.
//!
//! capture_bench <folder> [runs]

use std::path::Path;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args.next() else {
        eprintln!("usage: capture_bench <folder> [runs]");
        return;
    };
    let runs: usize = args.next().and_then(|v| v.parse().ok()).unwrap_or(5);
    let mut files: Vec<_> = std::fs::read_dir(Path::new(&dir))
        .map(|d| d.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && rawgraph_io::is_supported(p)).collect())
        .unwrap_or_default();
    files.sort();
    for p in files {
        let mut best = f64::MAX;
        let mut first = 0.0;
        let mut info = None;
        for i in 0..runs {
            let t = Instant::now();
            let r = rawgraph_io::read_capture_info(&p);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if i == 0 {
                first = ms;
            }
            best = best.min(ms);
            info = r.ok();
        }
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match info {
            Some(c) => println!(
                "{name:<28} first {first:7.1} ms  min {best:7.1} ms  K {:?} tint {:?} base {:?} iso {:?}",
                c.as_shot_kelvin.map(|v| v.round()),
                c.as_shot_tint.map(|v| (v * 10.0).round() / 10.0),
                c.baseline_exposure,
                c.iso
            ),
            None => println!("{name:<28} error"),
        }
    }
}
