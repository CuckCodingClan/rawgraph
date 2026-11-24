//! Prints the lens data (DNG OpcodeList3) found in every file of a folder, read only.
//!
//! Usage: `cargo run --release -p rawgraph-io --example lens_scan -- <folder>`

use std::path::PathBuf;

use rawgraph_io::{read_lens_profile, read_export_metadata};

fn main() {
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: lens_scan <folder>");
        std::process::exit(2);
    };
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file()).collect(),
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    files.sort();
    for f in files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let md = read_export_metadata(&f);
        let camera = format!("{} {}", md.make.unwrap_or_default(), md.model.unwrap_or_default());
        let lens = md.lens.unwrap_or_default();
        let r = |v: f64| format!("{v:.5}");
        match read_lens_profile(&f) {
            None => println!("{name}: [{camera}] [{lens}] no lens profile"),
            Some(p) => {
                println!("{name}: [{camera}] [{lens}]");
                match &p.warp {
                    None => println!("    warp: none"),
                    Some(w) => {
                        println!("    warp: {} plane(s), centre ({}, {})", w.planes.len(), r(w.center[0]), r(w.center[1]));
                        for (i, pl) in w.planes.iter().enumerate() {
                            println!("      plane {i}: kr = [{}, {}, {}, {}]  kt = [{}, {}]", r(pl[0]), r(pl[1]), r(pl[2]), r(pl[3]), r(pl[4]), r(pl[5]));
                        }
                    }
                }
                match &p.vignette {
                    None => println!("    vignette: none"),
                    Some(v) => println!(
                        "    vignette: k = [{}, {}, {}, {}, {}], centre ({}, {})",
                        r(v.k[0]),
                        r(v.k[1]),
                        r(v.k[2]),
                        r(v.k[3]),
                        r(v.k[4]),
                        r(v.center[0]),
                        r(v.center[1])
                    ),
                }
            }
        }
    }
}
