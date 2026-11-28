//! Decode timings in milliseconds, with the OS file cache left untouched.
use rawgraph_io::{LoadOptions, SourceKind, raw::{Mode, load_embedded_preview, load_with_mode}};
use std::{path::PathBuf, time::Instant};

fn main() -> std::io::Result<()> {
    let Some(folder) = std::env::args_os().nth(1) else {
        eprintln!("usage: first_draw <RAW folder>");
        std::process::exit(2);
    };
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && rawgraph_io::source_kind(p) == Some(SourceKind::Raw)).collect();
    files.sort();
    if files.is_empty() { eprintln!("No RAW files found"); }
    for file in files {
        println!("{}", file.display());
        let t = Instant::now();
        match load_with_mode(&file, &LoadOptions { max_dim: Some(2560), ..Default::default() }, Mode::Auto) {
            Ok((l, stages)) => {
                println!("  proxy {}×{}: {:.3} ms total", l.image.width(), l.image.height(), t.elapsed().as_secs_f64() * 1000.0);
                for (stage, ms) in stages.0 { println!("    {stage}: {ms:.3} ms"); }
            }
            Err(e) => eprintln!("  proxy failed: {e}"),
        }
        let t = Instant::now();
        let result = load_embedded_preview(&file, 1280);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok(Some((w, h, _))) => println!("  embedded (file already opened for proxy) {w}×{h}: {ms:.3} ms"),
            Ok(None) => println!("  no embedded preview: {ms:.3} ms"),
            Err(e) => eprintln!("  embedded failed after {ms:.3} ms: {e}"),
        }
    }
    Ok(())
}
