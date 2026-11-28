//! Metadata-only Adobe sidecars. Never open a source for writing or render pixels.
use std::io::Write;
use std::path::Path;
use rawgraph_batch::{ExportReport, ItemStatus};
use rawgraph_core::{ExportFileFormat, Graph, ImageId, NodeId};
use rawgraph_engine::CancelToken;
use rawgraph_project::{ColorLabel, Flag};
use crate::export::NodeRun;

fn proprietary_raw(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(),
        "3fr" | "ari" | "arw" | "bay" | "cr2" | "cr3" | "crw" | "cap" | "dcr" | "dcs" | "drf" | "eip" | "erf" | "fff" | "iiq" | "k25" | "kdc" | "mef" | "mos" | "mrw" | "nef" | "nrw" | "orf" | "pef" | "ptx" | "pxn" | "raf" | "raw" | "rwl" | "rw2" | "rwz" | "sr2" | "srf" | "srw" | "x3f"))
}

fn packet(rating: u8, flag: Flag, label: Option<ColorLabel>) -> String {
    let rating = if flag == Flag::Reject { -1 } else { i32::from(rating.min(5)) };
    let label = match label { Some(ColorLabel::Red) => "Red", Some(ColorLabel::Yellow) => "Yellow", Some(ColorLabel::Green) => "Green", Some(ColorLabel::Blue) => "Blue", Some(ColorLabel::Purple) => "Purple", None => "" };
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\" xmp:Rating=\"{rating}\" xmp:Label=\"{label}\"/></rdf:RDF></x:xmpmeta>\n")
}

pub fn run(graph: &Graph, node: NodeId, run: &NodeRun, selection: &[ImageId], cancel: &CancelToken, progress: &mut dyn FnMut(usize, usize)) -> ExportReport {
    let mut report = ExportReport::default();
    progress(0, selection.len());
    for (i, id) in selection.iter().copied().enumerate() {
        let status = if cancel.is_cancelled() { ItemStatus::Cancelled }
        else if !run.members.contains(&id) { ItemStatus::Excluded }
        else if let (Some(n), Some(item), Some(&(rating, flag, label))) = (graph.node(node), run.catalog.iter().find(|e| e.id == id), run.culling.get(&id)) {
            let spec = rawgraph_core::export_spec(n, id);
            if spec.format != ExportFileFormat::Xmp {
                ItemStatus::Failed("Mixed XMP and rendered formats in one run are not supported; run separately".into())
            } else { write_sidecar(&item.source, Path::new(spec.text.out_dir.trim()), &run.catalog, rating, flag, label) }
        } else { ItemStatus::Failed("Image or metadata missing from project".into()) };
        report.items.push((id, status));
        progress(i + 1, selection.len());
    }
    report
}

fn write_sidecar(source: &Path, dir: &Path, catalog: &[rawgraph_batch::ExportItem], rating: u8, flag: Flag, label: Option<ColorLabel>) -> ItemStatus {
    fn write(source: &Path, dir: &Path, catalog: &[rawgraph_batch::ExportItem], rating: u8, flag: Flag, label: Option<ColorLabel>) -> Result<ItemStatus, String> {
        if !proprietary_raw(source) { return Err("XMP export supports proprietary RAW files only (not DNG or rendered images)".into()); }
        if !source.is_file() { return Err("Source RAW is missing or not a file".into()); }
        if dir.as_os_str().is_empty() { return Err("Choose an output folder".into()); }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let canonical = dir.canonicalize().map_err(|e| e.to_string())?;
        if catalog.iter().filter_map(|i| i.source.parent()).any(|p| p.canonicalize().ok().as_ref() == Some(&canonical)) {
            return Err("Output folder contains source photos; choose another folder".into());
        }
        let name = source.file_stem().ok_or("Source has no filename")?;
        let target = canonical.join(source.file_name().ok_or("Source has no filename")?).with_extension("xmp");
        // Check all extension cases, including dangling symlinks; creation is exclusive as well.
        for ent in std::fs::read_dir(&canonical).map_err(|e| e.to_string())? {
            let p = ent.map_err(|e| e.to_string())?.path();
            if p.file_stem() == Some(name) && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xmp")) {
                return Ok(ItemStatus::SkippedExisting(p));
            }
        }
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&target) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(ItemStatus::SkippedExisting(target)),
            Err(e) => return Err(e.to_string()),
        };
        if let Err(e) = file.write_all(packet(rating, flag, label).as_bytes()) {
            drop(file);
            let _ = std::fs::remove_file(&target);
            return Err(e.to_string());
        }
        Ok(ItemStatus::Written { path: target, width: 0, height: 0 })
    }
    write(source, dir, catalog, rating, flag, label).unwrap_or_else(ItemStatus::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn culling_packet_and_raw_restrictions() {
        let parsed = rawgraph_project::parse_xmp_culling(&packet(4, Flag::None, Some(ColorLabel::Blue))).unwrap();
        assert_eq!(parsed.rating, Some(4));
        assert_eq!(parsed.label.as_deref(), Some("Blue"));
        assert_eq!(rawgraph_project::parse_xmp_culling(&packet(3, Flag::Reject, None)).unwrap().rating, Some(-1));
        assert!(proprietary_raw(Path::new("a.CR3")));
        assert!(!proprietary_raw(Path::new("a.dng")));
        assert!(!proprietary_raw(Path::new("a.jpg")));
    }
    #[test]
    fn never_overwrite_or_write_into_source_dir() {
        let t = tempfile::tempdir().unwrap();
        let source = t.path().join("a.nef");
        std::fs::write(&source, b"original").unwrap();
        let catalog = vec![rawgraph_batch::ExportItem { id: ImageId(1), source: source.clone() }];
        assert!(matches!(write_sidecar(&source, t.path(), &catalog, 3, Flag::None, None), ItemStatus::Failed(_)));
        let out = t.path().join("out");
        assert!(matches!(write_sidecar(&source, &out, &catalog, 3, Flag::None, None), ItemStatus::Written { .. }));
        assert!(matches!(write_sidecar(&source, &out, &catalog, 5, Flag::None, None), ItemStatus::SkippedExisting(_)));
        assert_eq!(std::fs::read(source).unwrap(), b"original");
    }
}
