//! Exports a folder of photos through Source -> Exposure -> Output.
//!
//! export <source folder> <out dir> [--format jpg|png|tif|png16|tif16|exr|avif] [--long-edge N] [--ev X] [--quality Q] [--iso-split T] [--iso-only MIN:MAX] [--develop] [--no-profile] [--no-metadata]
//!
//! By default the sRGB profile is embedded and the source's capture metadata is copied into the
//! file; `--no-profile` / `--no-metadata` switch that off.
//!
//! `--develop` inserts a Develop node (default parameters) right after Source and prints, per item,
//! where its baseline exposure came from and the gain applied.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rawgraph_batch::{export, ExportFormat, ExportItem, ExportSettings, ItemStatus, OnConflict};
use rawgraph_core::{CaptureInfo, Graph, ImageId, NodeId, NodeKind};
use rawgraph_engine::CancelToken;
use rawgraph_scopes::DisplayTransform;

fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.to_lowercase(), b.to_lowercase());
    let (mut ia, mut ib) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s.trim_start_matches('0').to_string()
                };
                let (na, nb) = (take(&mut ia), take(&mut ib));
                let ord = na.len().cmp(&nb.len()).then(na.cmp(&nb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ia.next();
                ib.next();
            }
        }
    }
}

fn collect(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && rawgraph_io::is_supported(p))
        .collect();
    files.sort_by(|a, b| {
        natural_cmp(&a.file_name().unwrap_or_default().to_string_lossy(), &b.file_name().unwrap_or_default().to_string_lossy())
    });
    Ok(files)
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let (Some(src), Some(out)) = (args.next(), args.next()) else {
        return Err("usage: export <source folder> <out dir> [--format jpg|png|tif|png16|tif16|exr|avif] [--long-edge N] [--ev X] [--quality Q] [--iso-split T] [--iso-only MIN:MAX] [--develop] [--no-profile] [--no-metadata]".into());
    };
    let (mut format, mut long_edge, mut ev, mut quality) = ("jpg".to_string(), None, 0.0f32, 92u8);
    let (mut iso_split, mut iso_only): (Option<f32>, Option<(f32, f32)>) = (None, None);
    let mut develop = false;
    let (mut embed_profile, mut copy_metadata) = (true, true);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--develop" => {
                develop = true;
                continue;
            }
            "--no-profile" => {
                embed_profile = false;
                continue;
            }
            "--no-metadata" => {
                copy_metadata = false;
                continue;
            }
            _ => {}
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--format" => format = value,
            "--long-edge" => long_edge = Some(value.parse::<u32>().map_err(|e| format!("--long-edge: {e}"))?),
            "--ev" => ev = value.parse().map_err(|e| format!("--ev: {e}"))?,
            "--quality" => quality = value.parse().map_err(|e| format!("--quality: {e}"))?,
            "--iso-split" => iso_split = Some(value.parse().map_err(|e| format!("--iso-split: {e}"))?),
            "--iso-only" => {
                let (a, b) = value.split_once(':').ok_or("--iso-only needs MIN:MAX")?;
                iso_only = Some((
                    a.parse().map_err(|e| format!("--iso-only min: {e}"))?,
                    b.parse().map_err(|e| format!("--iso-only max: {e}"))?,
                ));
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    let format = match format.as_str() {
        "jpg" | "jpeg" => ExportFormat::Jpeg { quality },
        "png" => ExportFormat::Png,
        "tif" | "tiff" => ExportFormat::Tiff,
        "png16" => ExportFormat::Png16,
        "tif16" | "tiff16" => ExportFormat::Tiff16,
        "exr" => ExportFormat::Exr,
        "avif" => ExportFormat::Avif { quality },
        other => return Err(format!("unknown format {other}")),
    };

    let files = collect(Path::new(&src))?;
    let items: Vec<ExportItem> =
        files.into_iter().enumerate().map(|(i, source)| ExportItem { id: ImageId(i as u64 + 1), source }).collect();

    let mut graph = Graph::new();
    let develop_node = develop.then(|| graph.add_node(NodeKind::Develop, [100.0, 0.0]));
    let mut merge: Option<NodeId> = None;
    let o;
    if let Some(t) = iso_split {
        // Source -> [Filter ISO <= t -> Exposure] / [Filter ISO > t -> NR -> Exposure] -> Merge -> Output
        let src = graph.add_node(NodeKind::Source, [0.0, 0.0]);
        let fa = graph.add_node(NodeKind::MetadataFilter, [200.0, 0.0]);
        let ea = graph.add_node(NodeKind::Exposure, [400.0, 0.0]);
        let fb = graph.add_node(NodeKind::MetadataFilter, [200.0, 200.0]);
        let nr = graph.add_node(NodeKind::NoiseReduction, [400.0, 200.0]);
        let eb = graph.add_node(NodeKind::Exposure, [600.0, 200.0]);
        let m = graph.add_node(NodeKind::Merge, [800.0, 100.0]);
        o = graph.add_node(NodeKind::Output, [1000.0, 100.0]);
        let head = match develop_node {
            Some(d) => {
                graph.connect(src, d, 0).map_err(|e| e.to_string())?;
                d
            }
            None => src,
        };
        for (a, b) in [(head, fa), (fa, ea), (head, fb), (fb, nr), (nr, eb), (m, o)] {
            graph.connect(a, b, 0).map_err(|e| e.to_string())?;
        }
        graph.connect(ea, m, 0).map_err(|e| e.to_string())?;
        graph.connect(eb, m, 1).map_err(|e| e.to_string())?;
        let id0 = ImageId(0);
        let mut set = |n: NodeId, name: &str, v: f32| {
            graph.node_mut(n).map(|n| n.set_param(name, id0, v)).unwrap_or(false)
        };
        let ok = set(fa, "min", 0.0)
            & set(fa, "max", t)
            & set(fb, "min", 0.0)
            & set(fb, "max", t)
            & set(fb, "invert", 1.0)
            & set(nr, "luma", 0.5)
            & set(nr, "chroma", 0.7)
            & set(ea, "ev", ev)
            & set(eb, "ev", ev);
        if !ok {
            return Err("could not set graph parameters".into());
        }
        merge = Some(m);
    } else {
        let s = graph.add_node(NodeKind::Source, [0.0, 0.0]);
        let mut prev = s;
        if let Some(d) = develop_node {
            graph.connect(prev, d, 0).map_err(|e| e.to_string())?;
            prev = d;
        }
        if let Some((lo, hi)) = iso_only {
            let f = graph.add_node(NodeKind::MetadataFilter, [200.0, 0.0]);
            graph.connect(prev, f, 0).map_err(|e| e.to_string())?;
            let node = graph.node_mut(f).ok_or("filter node missing")?;
            node.set_param("min", ImageId(0), lo);
            node.set_param("max", ImageId(0), hi);
            prev = f;
        }
        let e = graph.add_node(NodeKind::Exposure, [400.0, 0.0]);
        o = graph.add_node(NodeKind::Output, [600.0, 0.0]);
        graph.connect(prev, e, 0).map_err(|e| e.to_string())?;
        graph.connect(e, o, 0).map_err(|e| e.to_string())?;
        graph.node_mut(e).ok_or("exposure node missing")?.set_param("ev", ImageId(0), ev);
    }

    // Metadata for the printout only; the export reads it on its own.
    let caps: HashMap<u64, Option<CaptureInfo>> =
        items.iter().map(|i| (i.id.0, rawgraph_io::read_capture_info(&i.source).ok())).collect();

    let settings = ExportSettings {
        out_dir: PathBuf::from(&out),
        format,
        name_template: "{name}".into(),
        long_edge,
        on_conflict: OnConflict::Overwrite,
        display: DisplayTransform::default(),
        embed_profile,
        copy_metadata,
        ..Default::default()
    };

    println!(
        "{} files, format {:?}, long edge {:?}, ev {ev}, profile {embed_profile}, metadata {copy_metadata}",
        items.len(),
        format,
        long_edge
    );
    let start = Instant::now();
    let mut last = Instant::now();
    let report = export(&graph, o, &items, &settings, &CancelToken::new(), &mut |p| {
        let now = Instant::now();
        if let Some((_, status)) = &p.last {
            let ms = now.duration_since(last).as_millis();
            let name = p.current.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let text = match status {
                ItemStatus::Written { width, height, .. } => format!("written {width}x{height}"),
                ItemStatus::SkippedExisting(_) => "skipped".into(),
                ItemStatus::Failed(m) => format!("FAILED {m}"),
                ItemStatus::Cancelled => "cancelled".into(),
                ItemStatus::Excluded => "excluded".into(),
            };
            let cap = p.last.as_ref().and_then(|(id, _)| caps.get(&id.0)).cloned().flatten();
            let iso = cap.as_ref().and_then(|c| c.iso).map_or("?".to_string(), |v| format!("{v:.0}"));
            let branch = match merge {
                Some(m) => {
                    let port = p.last.as_ref().and_then(|(id, _)| graph.merge_source(m, *id, &|_| cap.clone()));
                    match port {
                        Some(0) => " branch A (low ISO)".to_string(),
                        Some(1) => " branch B (high ISO + noise reduction)".to_string(),
                        Some(n) => format!(" branch #{n}"),
                        None => " no branch".to_string(),
                    }
                }
                None => String::new(),
            };
            let dev = match develop_node.and_then(|d| graph.node(d)) {
                Some(node) => {
                    let id = p.last.as_ref().map_or(ImageId(0), |(id, _)| *id);
                    let s = rawgraph_core::develop_settings(node, id, cap.as_ref());
                    format!(" develop: baseline {:?} {:+.2} EV + brightness {:+.2} EV = {:+.2} EV, wb {:.0} K / {:+.0}", s.baseline_source, s.baseline_ev, s.brightness_ev, s.gain_ev, s.effective.0, s.effective.1)
                }
                None => String::new(),
            };
            let written = match status {
                ItemStatus::Written { path, .. } if copy_metadata => {
                    let m = rawgraph_io::read_export_metadata(&p.current);
                    let fields = [
                        ("make", m.make.is_some()),
                        ("model", m.model.is_some()),
                        ("lens", m.lens.is_some()),
                        ("iso", m.iso.is_some()),
                        ("exposure", m.exposure_time.is_some()),
                        ("f-number", m.f_number.is_some()),
                        ("focal", m.focal_length.is_some()),
                        ("date", m.datetime_original.is_some()),
                        ("artist", m.artist.is_some()),
                        ("copyright", m.copyright.is_some()),
                    ];
                    let names: Vec<&str> = fields.iter().filter(|f| f.1).map(|f| f.0).collect();
                    format!(" [{} bytes; EXIF: {}]", std::fs::metadata(path).map(|m| m.len()).unwrap_or(0), names.join(","))
                }
                ItemStatus::Written { path, .. } => format!(" [{} bytes]", std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)),
                _ => String::new(),
            };
            println!("[{}/{}] {name}: ISO {iso}{branch}{dev} -> {text}{written} ({ms} ms)", p.done, p.total);
        }
        last = now;
    })
    .map_err(|e| e.to_string())?;
    let secs = start.elapsed().as_secs_f64();
    println!(
        "written {}, excluded {}, skipped {}, failed {}, cancelled {} in {secs:.1} s = {:.1} images/min",
        report.written(),
        report.excluded(),
        report.skipped(),
        report.failed(),
        report.cancelled(),
        report.written() as f64 * 60.0 / secs.max(1e-9)
    );
    Ok(())
}
