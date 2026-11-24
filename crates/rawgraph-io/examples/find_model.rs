//! Lists files of a folder whose camera model contains a text (read-only; metadata only).
//! Usage: `find_model <folder> <model text> [max files] [max hits]`

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: find_model <folder> <model text> [max files] [max hits]");
        std::process::exit(2);
    }
    let dir = PathBuf::from(&args[1]);
    let needle = args[2].to_ascii_lowercase();
    let max_files: usize = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(800);
    let max_hits: usize = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(2);
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && rawgraph_io::is_supported(p)).collect(),
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    files.sort();
    let (mut checked, mut hits) = (0, 0);
    let mut models = std::collections::BTreeMap::<String, usize>::new();
    for p in files.iter().take(max_files) {
        checked += 1;
        if let Ok(c) = rawgraph_io::read_capture_info(p) {
            let model = c.model.clone().unwrap_or_default();
            *models.entry(model.clone()).or_default() += 1;
            if model.to_ascii_lowercase().contains(&needle) {
                println!("HIT {}", p.display());
                hits += 1;
                if hits >= max_hits {
                    break;
                }
            }
        }
    }
    println!("checked {checked} of {} files, hits {hits}, models seen {models:?}", files.len());
}
