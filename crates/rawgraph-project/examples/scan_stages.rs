//! Synthetic scan benchmark: `scan_stages <folder-of-tiny-pngs> [legacy|staged|rescan]`.
//!
//! This isolates I/O and bookkeeping costs, not RAW decode time: the metadata
//! callback stands in for `read_capture_info` with one open and read of the file.
use std::path::Path;
use std::time::Instant;

use rawgraph_project::staged::{HashQueue, Listing};
use rawgraph_project::{content_hash, Project};

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let folder = args.next().expect("usage: scan_stages <folder-of-tiny-pngs> [legacy|staged|rescan]");
    let mode = args.next().map_or_else(|| "staged".to_owned(), |m| m.to_string_lossy().into_owned());
    let folder = Path::new(&folder);
    let supported = |path: &Path| path.extension().is_some_and(|ext| ext == "png");
    let capture = |path: &Path| {
        let _ = std::fs::read(path);
        None
    };
    match mode.as_str() {
        "legacy" => {
            let mut legacy = Project::new();
            let start = Instant::now();
            let mut first = None;
            let report = legacy.scan_folder_with(folder, true, &supported, &capture, &mut |p| {
                if p.done == 0 {
                    println!("legacy total known (listing done) {}: {:.1} ms", p.total, ms(start));
                } else if p.done == 1 {
                    first = Some(ms(start));
                }
                true
            });
            println!("legacy first entry: {:.1} ms", first.unwrap_or_default());
            println!("legacy all {} entries (8 workers, hash+metadata): {:.1} ms", report.added.len(), ms(start));
        }
        "staged" | "rescan" => {
            let mut project = Project::new();
            if mode == "rescan" {
                // Fully scanned catalog, then rescan the unchanged folder.
                project.scan_folder(folder, true, &supported, &capture);
            }
            let start = Instant::now();
            let mut scan = project.list_catalog(folder, true, &supported, &mut |event| {
                if let Listing::Count(n) = event {
                    println!("{mode} count {n}: {:.1} ms", ms(start));
                }
                true
            });
            let n = scan.entries.len();
            println!("{mode} list {n} entries: {:.1} ms (hash jobs {}, metadata jobs {})", ms(start), scan.hashes.len(), scan.metadata.len());
            if scan.hashes.is_empty() {
                return;
            }
            let start = Instant::now();
            let mut queue = HashQueue::new(&scan.hashes);
            let jobs = scan.hashes.len();
            while let Some(i) = queue.take() {
                let hash = content_hash(&scan.hashes[i].path);
                assert!(scan.finish_hash(&mut project, i, hash).is_some());
            }
            println!("{mode} hash (1 worker): {:.1} us/file", start.elapsed().as_secs_f64() * 1e6 / jobs as f64);
            let start = Instant::now();
            let meta = scan.metadata.len();
            for id in std::mem::take(&mut scan.metadata) {
                let capture = capture(&project.image(id).unwrap().path);
                project.finish_capture(id, capture);
            }
            println!("{mode} metadata I/O (1 worker): {:.1} us/file", start.elapsed().as_secs_f64() * 1e6 / meta.max(1) as f64);
        }
        other => panic!("unknown mode {other}"),
    }
}
