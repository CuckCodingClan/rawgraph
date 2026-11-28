//! Wave 8 real run: Source -> Develop -> Export through `export_node`.
//!
//! wave8 <out dir> <file> [<file> ...]
//!
//! Writes every file as JPEG (long edge 2048) in sRGB and in Display P3, and as AVIF (sRGB), and
//! prints file, format, space, size and time.

use std::path::{Path, PathBuf};
use std::time::Instant;

use rawgraph_batch::{export_node, ExportItem, ItemStatus};
use rawgraph_core::{ExportText, Graph, ImageId, NodeData, NodeKind};
use rawgraph_engine::CancelToken;
use rawgraph_scopes::{DisplayTransform, ToneMap};

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().ok_or("usage: wave8 <out dir> <file> ...")?);
    let catalog: Vec<ExportItem> =
        args.enumerate().map(|(i, p)| ExportItem { id: ImageId(i as u64 + 1), source: PathBuf::from(p), keywords: Vec::new() }).collect();
    if catalog.is_empty() {
        return Err("no input files".into());
    }
    let selection: Vec<ImageId> = catalog.iter().map(|i| i.id).collect();
    let display = DisplayTransform { exposure_ev: 0.0, tone_map: ToneMap::AcesFit };

    // (label, format index, colour space index, name suffix)
    for (label, format, space, suffix) in [("JPEG", 0.0, 0.0, "_srgb"), ("JPEG", 0.0, 1.0, "_p3"), ("AVIF", 6.0, 0.0, "_avif")] {
        let mut g = Graph::new();
        let s = g.add_node(NodeKind::Source, [0.0; 2]);
        let d = g.add_node(NodeKind::Develop, [0.0; 2]);
        let e = g.add_node(NodeKind::Export, [0.0; 2]);
        g.connect(s, d, 0).map_err(|e| e.to_string())?;
        g.connect(d, e, 0).map_err(|e| e.to_string())?;
        let n = g.node_mut(e).ok_or("export node missing")?;
        let text = ExportText {
            out_dir: out.to_string_lossy().into_owned(),
            name_template: format!("{{name}}{suffix}"),
            ..Default::default()
        };
        n.set_data(ImageId(0), Some(NodeData::Export(text)));
        for (k, v) in [("format", format), ("size", 1.0), ("long_edge", 2048.0), ("color_space", space), ("collision", 1.0), ("quality", 90.0)] {
            if !n.set_param(k, ImageId(0), v) {
                return Err(format!("cannot set {k}"));
            }
        }
        let start = Instant::now();
        let mut last = Instant::now();
        let report = export_node(&g, e, &catalog, &selection, display, &CancelToken::new(), &mut |p| {
            let now = Instant::now();
            if let Some((_, st)) = &p.last {
                let name = p.current.file_name().unwrap_or_default().to_string_lossy().into_owned();
                let ms = now.duration_since(last).as_millis();
                match st {
                    ItemStatus::Written { path, width, height } => {
                        let bytes = std::fs::metadata(Path::new(path)).map(|m| m.len()).unwrap_or(0);
                        println!(
                            "{name} | {label} | {} | {width}x{height} | {} KB | {ms} ms",
                            if space == 0.0 { "sRGB" } else { "Display P3" },
                            bytes / 1024
                        );
                    }
                    other => println!("{name} | {label} | {other:?} | {ms} ms"),
                }
            }
            last = now;
        })
        .map_err(|e| e.to_string())?;
        println!("-- {label} {suffix}: written {} failed {} in {:.1} s", report.written(), report.failed(), start.elapsed().as_secs_f64());
    }
    Ok(())
}
