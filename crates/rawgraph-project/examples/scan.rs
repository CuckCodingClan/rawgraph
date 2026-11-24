//! Read-only scan timing: `cargo run --release --example scan -- <folder> <project-file> [threads]`.
use std::path::Path;
use std::time::Instant;

use rawgraph_project::Project;

fn supported(p: &Path) -> bool {
    const EXT: [&str; 11] = ["arw", "dng", "cr3", "cr2", "nef", "raf", "jpg", "jpeg", "tif", "tiff", "png"];
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXT.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(folder), Some(out)) = (args.next(), args.next()) else {
        eprintln!("usage: scan <folder> <project-file> [threads]");
        return;
    };
    let threads: usize = args.next().and_then(|t| t.parse().ok()).unwrap_or(8);
    let mut project = Project::new();
    project.set_scan_threads(threads);
    println!("threads: {}", project.scan_threads());
    for run in ["first scan", "second scan"] {
        let t = Instant::now();
        let r = project.scan_folder(Path::new(&folder), true, &supported, &|_| None);
        println!(
            "{run}: {:.2?} added={} unchanged={} relinked={} updated={} failed={}",
            t.elapsed(),
            r.added.len(),
            r.unchanged,
            r.relinked.len(),
            r.updated.len(),
            r.failed.len()
        );
    }
    println!("images: {}", project.images().len());
    match project.save(Path::new(&out)) {
        Ok(()) => println!("saved {out} ({} bytes)", std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0)),
        Err(e) => eprintln!("save failed: {e}"),
    }
}
