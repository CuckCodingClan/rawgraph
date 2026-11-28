//! Tests of `export_node` and `settings_from_spec`: every field of an Export node arrives in the
//! written file, and Source folders, Merge name collisions and Layer pairing work end to end.
#![allow(clippy::needless_range_loop)]

use std::fs;
use std::path::{Path, PathBuf};

use image::{ImageDecoder, ImageReader};
use rawgraph_core::{
    export_spec, ExportCollision, ExportFileFormat, ExportSize, ExportText, Graph, ImageId, Mode, NodeData, NodeId,
    NodeKind, OutputSpace,
};
use rawgraph_engine::{CancelToken, ImageProvider};
use rawgraph_scopes::DisplayTransform;

use super::tests::{files_in, item, load, read_exr, write_grey, TempDir, DECODES};
use super::*;

const A: ImageId = ImageId(0);

/// A catalog of flat-colour PNG files in `src/` with a Source -> Export graph; the output goes to `out/`.
struct Rig {
    t: TempDir,
    g: Graph,
    src: NodeId,
    exp: NodeId,
    catalog: Vec<ExportItem>,
}

fn write_rgb(path: &Path, w: u32, h: u32, rgb: [u8; 3]) {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_default();
    }
    let data: Vec<u8> = (0..w * h).flat_map(|_| rgb).collect();
    rawgraph_io::save_srgb8(path, w, h, &data, 95, true).expect("write test source");
}

impl Rig {
    fn new(files: &[(&str, u32, u32, [u8; 3])]) -> Rig {
        let t = TempDir::new();
        let catalog: Vec<ExportItem> = files
            .iter()
            .enumerate()
            .map(|(i, (name, w, h, rgb))| {
                let p = t.path(&format!("src/{name}"));
                write_rgb(&p, *w, *h, *rgb);
                item(i as u64 + 1, p)
            })
            .collect();
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let exp = g.add_node(NodeKind::Export, [0.0; 2]);
        g.connect(src, exp, 0).expect("connect");
        let mut r = Rig { t, g, src, exp, catalog };
        let out = r.t.path("out").to_string_lossy().into_owned();
        r.text(|x| x.out_dir = out);
        r
    }

    fn one(w: u32, h: u32, rgb: [u8; 3]) -> Rig {
        Rig::new(&[("a.png", w, h, rgb)])
    }

    fn p(&mut self, name: &str, v: f32) {
        assert!(self.g.node_mut(self.exp).expect("node").set_param(name, A, v), "param {name}");
    }

    fn text(&mut self, f: impl FnOnce(&mut ExportText)) {
        let node = self.g.node_mut(self.exp).expect("node");
        let mut tx = match node.data(A) {
            Some(NodeData::Export(t)) => t.clone(),
            _ => ExportText::default(),
        };
        f(&mut tx);
        assert!(node.set_data(A, Some(NodeData::Export(tx))));
    }

    fn out(&self) -> PathBuf {
        self.t.path("out")
    }

    fn ids(&self) -> Vec<ImageId> {
        self.catalog.iter().map(|i| i.id).collect()
    }

    fn run(&self) -> ExportReport {
        self.run_with(&self.ids(), DisplayTransform::default())
    }

    fn run_with(&self, selection: &[ImageId], display: DisplayTransform) -> ExportReport {
        export_node(&self.g, self.exp, &self.catalog, selection, display, &CancelToken::new(), &mut |_| {}).expect("export_node")
    }

    fn first_path(&self) -> PathBuf {
        written_of(&self.run().items[0].1)
    }
}

fn written_of(s: &ItemStatus) -> PathBuf {
    match s {
        ItemStatus::Written { path, .. } => path.clone(),
        other => panic!("expected Written, got {other:?}"),
    }
}

fn icc_of(path: &Path) -> Option<Vec<u8>> {
    if matches!(path.extension().and_then(|e| e.to_str()), Some("tif")) {
        let mut t = image::codecs::tiff::TiffDecoder::new(std::io::BufReader::new(fs::File::open(path).ok()?)).ok()?;
        return t.icc_profile().ok().flatten();
    }
    let mut d = ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
    d.icc_profile().ok().flatten()
}

fn exif_of(path: &Path) -> Option<exif::Exif> {
    exif::Reader::new().read_from_container(&mut std::io::BufReader::new(fs::File::open(path).ok()?)).ok()
}

fn has(ex: &exif::Exif, tag: exif::Tag) -> bool {
    ex.get_field(tag, exif::In::PRIMARY).is_some()
}

fn text_of(ex: &exif::Exif, tag: exif::Tag) -> Option<String> {
    match &ex.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(v) => v.first().map(|s| String::from_utf8_lossy(s).trim_matches('\0').to_string()),
        _ => None,
    }
}

fn rgb8_of(path: &Path) -> (u32, u32, Vec<u8>) {
    let img = image::open(path).expect("read back").to_rgb8();
    (img.width(), img.height(), img.into_raw())
}

// ------------------------------------------------------------------------------ spec mapping

#[test]
fn settings_from_spec_maps_every_field() {
    let mut r = Rig::one(8, 8, [10, 20, 30]);
    r.p("format", 6.0); // AVIF
    r.p("quality", 55.0);
    r.p("size", 2.0); // percent
    r.p("percent", 25.0);
    r.p("color_space", 1.0); // Display P3
    r.p("profile", 0.0);
    r.p("collision", 2.0); // rename
    r.p("meta_gps", 1.0);
    r.p("meta_software", 0.0);
    r.text(|x| {
        x.name_template = "{name}_x".into();
        x.artist = "Ann".into();
        x.copyright = "(c) Ann".into();
        x.description = "desc".into();
    });
    let node = r.g.node(r.exp).expect("node");
    let s = settings_from_spec(&export_spec(node, A), DisplayTransform::default());
    assert_eq!(s.format, ExportFormat::Avif { quality: 55 });
    assert_eq!((s.long_edge, s.percent), (None, Some(25.0)));
    assert_eq!((s.space, s.embed_profile, s.on_conflict), (OutputSpace::DisplayP3, false, OnConflict::Rename));
    assert!(s.meta.gps && !s.meta.software && s.meta.camera && s.meta.exposure && s.meta.date && s.meta.author);
    assert_eq!(s.name_template, "{name}_x");
    assert_eq!(s.out_dir, r.out());
    assert_eq!((s.artist.as_deref(), s.copyright.as_deref(), s.description.as_deref()), (Some("Ann"), Some("(c) Ann"), Some("desc")));
    assert!(s.copy_metadata);

    // Long edge, other formats, empty overrides.
    r.p("format", 4.0);
    r.p("size", 1.0);
    r.p("long_edge", 640.0);
    r.text(|x| {
        x.artist.clear();
        x.copyright.clear();
        x.description.clear();
    });
    let node = r.g.node(r.exp).expect("node");
    let s = settings_from_spec(&export_spec(node, A), DisplayTransform::default());
    assert_eq!(s.format, ExportFormat::Tiff16);
    assert_eq!((s.long_edge, s.percent), (Some(640), None));
    assert_eq!((s.artist, s.copyright, s.description), (None, None, None));

    // Everything switched off means no metadata block at all.
    for g in ["meta_camera", "meta_exposure", "meta_date", "meta_gps", "meta_author", "meta_software", "meta_keywords"] {
        r.p(g, 0.0);
    }
    let node = r.g.node(r.exp).expect("node");
    assert!(!settings_from_spec(&export_spec(node, A), DisplayTransform::default()).copy_metadata);
}

#[test]
fn defaults_match_the_contract() {
    let s = ExportSettings::default();
    assert_eq!(s.space, OutputSpace::Srgb);
    assert!(s.meta.camera && s.meta.exposure && s.meta.date && s.meta.author && s.meta.software && !s.meta.gps);
    assert_eq!((s.artist, s.copyright, s.description, s.percent), (None, None, None, None));

    let r = Rig::one(8, 8, [10, 20, 30]);
    let node = r.g.node(r.exp).expect("node");
    let spec = export_spec(node, A);
    assert_eq!(spec.format, ExportFileFormat::Jpeg);
    assert_eq!((spec.size, spec.collision), (ExportSize::Original, ExportCollision::Skip));
    assert!(spec.embed_profile);
}

// ------------------------------------------------------------------------------ formats, quality, size

#[test]
fn every_format_is_written_with_its_signature() {
    for (idx, ext, sig) in [
        (0.0, "jpg", &[0xFFu8, 0xD8][..]),
        (1.0, "png", &[0x89, b'P', b'N', b'G'][..]),
        (2.0, "png", &[0x89, b'P', b'N', b'G'][..]),
        (5.0, "exr", &[0x76, 0x2F, 0x31, 0x01][..]),
    ] {
        let mut r = Rig::one(12, 8, [200, 100, 50]);
        r.p("format", idx);
        let p = r.first_path();
        assert_eq!(p, r.out().join(format!("a.{ext}")));
        assert!(fs::read(&p).expect("read").starts_with(sig), "format {idx}");
    }
    for idx in [3.0, 4.0] {
        let mut r = Rig::one(12, 8, [200, 100, 50]);
        r.p("format", idx);
        let p = r.first_path();
        assert_eq!(p, r.out().join("a.tif"));
        let b = fs::read(&p).expect("read");
        assert!(b.starts_with(&[b'I', b'I', 42, 0]) || b.starts_with(&[b'M', b'M', 0, 42]), "TIFF {idx}");
    }
    let mut r = Rig::one(16, 16, [200, 100, 50]);
    r.p("format", 6.0);
    let p = r.first_path();
    assert_eq!(p, r.out().join("a.avif"));
    let b = fs::read(&p).expect("read");
    assert_eq!(&b[4..8], b"ftyp", "AVIF is an ISO base media file");
    assert!(b[8..32].windows(4).any(|w| w == b"avif"), "AVIF brand");
}

fn noisy(path: &Path, w: u32, h: u32) {
    let mut s = 12345u32;
    let data: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 24) as u8
        })
        .collect();
    rawgraph_io::save_srgb8(path, w, h, &data, 95, true).expect("write noise");
}

#[test]
fn quality_changes_the_file_size() {
    for (fmt, w) in [(0.0, 96), (6.0, 48)] {
        let mut r = Rig::one(8, 8, [1, 2, 3]);
        noisy(&r.catalog[0].source, w, w);
        r.p("format", fmt);
        r.p("collision", 1.0);
        r.p("quality", 15.0);
        let small = fs::metadata(r.first_path()).expect("meta").len();
        r.p("quality", 95.0);
        let big = fs::metadata(r.first_path()).expect("meta").len();
        assert!(big > small, "format {fmt}: quality 95 gives {big} bytes, 15 gives {small}");
    }
}

#[test]
fn sizes_long_edge_percent_and_original() {
    let mut r = Rig::one(40, 30, [90, 90, 90]);
    r.p("format", 1.0);
    r.p("collision", 1.0);
    let dims = |r: &Rig| {
        let st = r.run().items[0].1.clone();
        match st {
            ItemStatus::Written { width, height, path } => {
                assert_eq!(image::image_dimensions(&path).expect("dims"), (width, height));
                (width, height)
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(dims(&r), (40, 30));
    r.p("size", 1.0);
    r.p("long_edge", 20.0);
    assert_eq!(dims(&r), (20, 15));
    r.p("long_edge", 400.0);
    assert_eq!(dims(&r), (40, 30), "never upscales");
    r.p("size", 2.0);
    r.p("percent", 50.0);
    assert_eq!(dims(&r), (20, 15));
    r.p("percent", 100.0);
    assert_eq!(dims(&r), (40, 30));
    r.p("percent", 1.0);
    assert_eq!(dims(&r), (1, 1));

    // `long_edge` wins over `percent` when both are set in the settings.
    let t = TempDir::new();
    let src = t.path("src/a.png");
    write_rgb(&src, 40, 30, [90, 90, 90]);
    let mut g = Graph::new();
    let s = g.add_node(NodeKind::Source, [0.0; 2]);
    let o = g.add_node(NodeKind::Output, [0.0; 2]);
    g.connect(s, o, 0).expect("connect");
    let st = ExportSettings {
        out_dir: t.path("out"),
        format: ExportFormat::Png,
        long_edge: Some(10),
        percent: Some(50.0),
        ..Default::default()
    };
    let rep = export(&g, o, &[item(1, src)], &st, &CancelToken::new(), &mut |_| {}).expect("export");
    assert!(matches!(rep.items[0].1, ItemStatus::Written { width: 10, height: 8, .. }), "{:?}", rep.items[0].1);
}

// ------------------------------------------------------------------------------ colour

/// Linear Rec.709 -> linear output space, from the standard D65 matrices (independent of the crates).
fn m709_to(space: usize) -> [[f64; 3]; 3] {
    match space {
        1 => [[0.8224621, 0.1775380, 0.0], [0.0331941, 0.9668058, 0.0], [0.0170827, 0.0723974, 0.9105199]],
        2 => [[0.7151627, 0.2848373, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0411619, 0.9588381]],
        3 => [[0.6274039, 0.3292830, 0.0433131], [0.0690973, 0.9195404, 0.0113623], [0.0163914, 0.0880133, 0.8955953]],
        _ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
    }
}

fn srgb_dec(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn enc(space: usize, v: f64) -> f64 {
    let v = v.clamp(0.0, 1.0);
    match space {
        2 => v.powf(256.0 / 563.0),
        3 => v.powf(1.0 / 2.4),
        _ => {
            if v <= 0.0031308 {
                v * 12.92
            } else {
                1.055 * v.powf(1.0 / 2.4) - 0.055
            }
        }
    }
}

#[test]
fn each_colour_space_encodes_and_embeds_its_profile() {
    let src_rgb = [200u8, 40, 30]; // saturated red, inside every gamut
    let lin: Vec<f64> = src_rgb.iter().map(|v| srgb_dec(*v as f64 / 255.0)).collect();
    let spaces = [OutputSpace::Srgb, OutputSpace::DisplayP3, OutputSpace::AdobeRgb, OutputSpace::Rec2020];
    let mut seen = Vec::new();
    for (i, space) in spaces.into_iter().enumerate() {
        for (fmt, ext) in [(1.0, "png"), (0.0, "jpg")] {
            let mut r = Rig::one(8, 8, src_rgb);
            r.p("format", fmt);
            r.p("quality", 100.0);
            r.p("color_space", i as f32);
            let p = r.first_path();
            assert_eq!(p.extension().and_then(|e| e.to_str()), Some(ext));
            assert_eq!(icc_of(&p).as_deref(), Some(rawgraph_io::icc_profile(space)), "{space:?} {ext}: profile");
            let (_, _, px) = rgb8_of(&p);
            let m = m709_to(i);
            for c in 0..3 {
                let l = m[c][0] * lin[0] + m[c][1] * lin[1] + m[c][2] * lin[2];
                let expected = enc(i, l) * 255.0;
                let tol = if ext == "jpg" { 4.0 } else { 2.0 };
                assert!((px[c] as f64 - expected).abs() <= tol, "{space:?} {ext} channel {c}: {} vs {expected:.1}", px[c]);
            }
            if ext == "png" {
                seen.push(px[..3].to_vec());
            }
        }
    }
    for i in 0..seen.len() {
        for j in 0..i {
            assert_ne!(seen[i], seen[j], "spaces {j} and {i} give the same encoded colour");
        }
    }

    // 16-bit follows the same space.
    let mut r = Rig::one(8, 8, src_rgb);
    r.p("format", 2.0);
    r.p("color_space", 1.0);
    let p = r.first_path();
    let img = image::open(&p).expect("16-bit").to_rgb16();
    let m = m709_to(1);
    for c in 0..3 {
        let l = m[c][0] * lin[0] + m[c][1] * lin[1] + m[c][2] * lin[2];
        let expected = enc(1, l) * 65535.0;
        assert!((img.as_raw()[c] as f64 - expected).abs() <= 400.0, "16-bit channel {c}: {} vs {expected:.0}", img.as_raw()[c]);
    }
    assert_eq!(icc_of(&p).as_deref(), Some(rawgraph_io::icc_profile(OutputSpace::DisplayP3)));
}

#[test]
fn profile_switch_and_exr_ignore() {
    for (fmt, ext) in [(0.0, "jpg"), (1.0, "png"), (2.0, "png"), (3.0, "tif"), (4.0, "tif")] {
        let mut r = Rig::one(8, 8, [10, 200, 90]);
        r.p("format", fmt);
        r.p("color_space", 2.0);
        r.p("collision", 1.0);
        let on = r.first_path();
        assert_eq!(icc_of(&on).as_deref(), Some(rawgraph_io::icc_profile(OutputSpace::AdobeRgb)), "{ext}");
        r.p("profile", 0.0);
        let off = r.first_path();
        assert!(icc_of(&off).is_none(), "{ext}: profile off");
    }
    // EXR is linear ACEScg whatever the space: same pixels for sRGB and Rec. 2020.
    let mut a = Rig::one(8, 8, [10, 200, 90]);
    a.p("format", 5.0);
    let mut b = Rig::one(8, 8, [10, 200, 90]);
    b.p("format", 5.0);
    b.p("color_space", 3.0);
    assert_eq!(read_exr(&a.first_path()).2, read_exr(&b.first_path()).2);
}

// ------------------------------------------------------------------------------ collisions, template

#[test]
fn collision_modes_and_name_template() {
    let mut r = Rig::new(&[("a.png", 8, 8, [50, 60, 70]), ("b.png", 8, 8, [50, 60, 70])]);
    r.p("format", 1.0);
    r.text(|x| x.name_template = "{name}_{index:03}".into());
    let rep = r.run();
    assert_eq!(files_in(&r.out()), vec!["a_001.png", "b_002.png"]);
    assert_eq!(rep.written(), 2);

    // Skip leaves an existing file alone.
    let existing = r.out().join("a_001.png");
    fs::write(&existing, b"precious").expect("write");
    let rep = r.run();
    assert_eq!(rep.items[0].1, ItemStatus::SkippedExisting(existing.clone()));
    assert_eq!(fs::read(&existing).expect("read"), b"precious");

    // Rename adds a number.
    r.p("collision", 2.0);
    let rep = r.run();
    assert_eq!(written_of(&rep.items[0].1), r.out().join("a_001 (2).png"));
    assert_eq!(fs::read(&existing).expect("read"), b"precious");

    // Overwrite replaces it.
    r.p("collision", 1.0);
    let rep = r.run();
    assert_eq!(written_of(&rep.items[0].1), existing);
    assert_ne!(fs::read(&existing).expect("read"), b"precious");
}

#[test]
fn sources_are_never_overwritten_by_an_export_node() {
    let mut r = Rig::new(&[("a.png", 8, 8, [50, 60, 70]), ("b.png", 8, 8, [50, 60, 70])]);
    let dir = r.t.path("src").to_string_lossy().into_owned();
    r.text(|x| x.out_dir = dir);
    r.p("format", 1.0);
    let before: Vec<Vec<u8>> = r.catalog.iter().map(|i| fs::read(&i.source).expect("read")).collect();
    for collision in [0.0, 1.0, 2.0] {
        r.p("collision", collision);
        let rep = r.run();
        assert_eq!(rep.failed() + rep.skipped(), 2, "{rep:?}");
        for (i, it) in r.catalog.iter().enumerate() {
            assert_eq!(fs::read(&it.source).expect("read"), before[i]);
        }
        assert_eq!(files_in(&r.t.path("src")), vec!["a.png", "b.png"]);
    }
}

// ------------------------------------------------------------------------------ metadata

fn full_meta() -> rawgraph_io::ExportMetadata {
    rawgraph_io::ExportMetadata {
        make: Some("SONY".into()),
        model: Some("ILCE-7M4".into()),
        lens: Some("FE 35mm F1.4 GM".into()),
        iso: Some(640),
        exposure_time: Some((1, 250)),
        f_number: Some((28, 10)),
        focal_length: Some((35, 1)),
        datetime_original: Some("2026:09:26 14:03:09".into()),
        artist: Some("Source Artist".into()),
        copyright: Some("(c) Source".into()),
        software: None,
        gps: Some(rawgraph_io::GpsInfo { latitude: 47.07, longitude: 15.44, altitude_m: Some(353.0) }),
        description: None,
        keywords: Vec::new(),
    }
}

fn meta_rig() -> Rig {
    let r = Rig::one(8, 8, [90, 90, 90]);
    let rgb = vec![90u8; 16 * 12 * 3];
    let o = rawgraph_io::SaveOptions { jpeg_quality: 95, overwrite: true, embed_profile: false, metadata: Some(full_meta()), ..Default::default() };
    let p = r.t.path("src/photo.jpg");
    rawgraph_io::save_srgb8_with(&p, 16, 12, &rgb, &o).expect("source with metadata");
    let mut r = r;
    r.catalog = vec![item(1, p)];
    r
}

const GROUPS: [(&str, &[exif::Tag]); 6] = [
    ("meta_camera", &[exif::Tag::Make, exif::Tag::Model, exif::Tag::LensModel]),
    (
        "meta_exposure",
        &[exif::Tag::PhotographicSensitivity, exif::Tag::ExposureTime, exif::Tag::FNumber, exif::Tag::FocalLength],
    ),
    ("meta_date", &[exif::Tag::DateTimeOriginal]),
    ("meta_gps", &[exif::Tag::GPSLatitude, exif::Tag::GPSLongitude]),
    ("meta_author", &[exif::Tag::Artist, exif::Tag::Copyright]),
    ("meta_software", &[exif::Tag::Software]),
];

#[test]
fn each_metadata_switch_controls_its_group_only() {
    for (only, _) in GROUPS {
        let mut r = meta_rig();
        for (g, _) in GROUPS {
            r.p(g, if g == only { 1.0 } else { 0.0 });
        }
        let ex = exif_of(&r.first_path()).unwrap_or_else(|| panic!("{only}: no EXIF block"));
        for (g, gtags) in GROUPS {
            for t in gtags {
                assert_eq!(has(&ex, *t), g == only, "only {only}: tag {t:?} of {g}");
            }
        }
    }
}

#[test]
fn default_metadata_has_no_gps_and_gps_can_be_enabled() {
    let mut r = meta_rig();
    r.p("collision", 1.0);
    let ex = exif_of(&r.first_path()).expect("exif");
    assert!(!has(&ex, exif::Tag::GPSLatitude) && !has(&ex, exif::Tag::GPSLongitude), "GPS is off by default");
    assert_eq!(text_of(&ex, exif::Tag::Make).as_deref(), Some("SONY"));
    assert_eq!(text_of(&ex, exif::Tag::Artist).as_deref(), Some("Source Artist"));
    assert_eq!(text_of(&ex, exif::Tag::Software).as_deref(), Some("RawGraph"));
    assert!(has(&ex, exif::Tag::PhotographicSensitivity) && has(&ex, exif::Tag::DateTimeOriginal));

    r.p("meta_gps", 1.0);
    let ex = exif_of(&r.first_path()).expect("exif");
    assert!(has(&ex, exif::Tag::GPSLatitude) && has(&ex, exif::Tag::GPSLongitude), "GPS on");

    // All switches off and no description: no metadata block at all.
    for (g, _) in GROUPS {
        r.p(g, 0.0);
    }
    r.p("meta_keywords", 0.0);
    assert!(exif_of(&r.first_path()).is_none());
}

#[test]
fn overrides_and_description() {
    let mut r = meta_rig();
    r.p("collision", 1.0);
    r.text(|x| {
        x.artist = "New Artist".into();
        x.copyright = "(c) New".into();
        x.description = "A description".into();
    });
    let ex = exif_of(&r.first_path()).expect("exif");
    assert_eq!(text_of(&ex, exif::Tag::Artist).as_deref(), Some("New Artist"));
    assert_eq!(text_of(&ex, exif::Tag::Copyright).as_deref(), Some("(c) New"));
    assert_eq!(text_of(&ex, exif::Tag::ImageDescription).as_deref(), Some("A description"));

    // Author group off: the artist/copyright overrides are dropped, the description stays.
    r.p("meta_author", 0.0);
    let ex = exif_of(&r.first_path()).expect("exif");
    assert!(!has(&ex, exif::Tag::Artist) && !has(&ex, exif::Tag::Copyright));
    assert_eq!(text_of(&ex, exif::Tag::ImageDescription).as_deref(), Some("A description"));

    // Only a description: a block with just that.
    for (g, _) in GROUPS {
        r.p(g, 0.0);
    }
    let ex = exif_of(&r.first_path()).expect("exif");
    assert_eq!(text_of(&ex, exif::Tag::ImageDescription).as_deref(), Some("A description"));
    assert!(!has(&ex, exif::Tag::Make) && !has(&ex, exif::Tag::Artist) && !has(&ex, exif::Tag::Software));

    // Empty overrides keep the source's values.
    let mut r = meta_rig();
    let ex = exif_of(&r.first_path()).expect("exif");
    assert_eq!(text_of(&ex, exif::Tag::Copyright).as_deref(), Some("(c) Source"));
    r.p("collision", 1.0);
}

// ------------------------------------------------------------------------------ node checks

#[test]
fn empty_folder_and_wrong_node_are_errors_before_anything_is_written() {
    let mut r = Rig::one(8, 8, [10, 10, 10]);
    r.text(|x| x.out_dir.clear());
    let e = export_node(&r.g, r.exp, &r.catalog, &r.ids(), DisplayTransform::default(), &CancelToken::new(), &mut |_| panic!("no progress"));
    assert!(matches!(e, Err(ExportError::InvalidSettings(m)) if m.contains("folder")));
    assert!(!r.out().exists());

    let r = Rig::one(8, 8, [10, 10, 10]);
    let mut g = r.g.clone();
    let out = g.add_node(NodeKind::Output, [0.0; 2]);
    g.connect(r.src, out, 0).expect("connect");
    for n in [out, r.src, NodeId(999)] {
        let e = export_node(&g, n, &r.catalog, &r.ids(), DisplayTransform::default(), &CancelToken::new(), &mut |_| {});
        assert!(matches!(e, Err(ExportError::InvalidSettings(_))), "{n:?}");
    }
    assert!(!r.out().exists());

    // Bad template.
    let mut r = Rig::one(8, 8, [10, 10, 10]);
    r.text(|x| x.name_template = "a/{name}".into());
    assert!(export_node(&r.g, r.exp, &r.catalog, &r.ids(), DisplayTransform::default(), &CancelToken::new(), &mut |_| {}).is_err());
    assert!(!r.out().exists());
}

#[test]
fn excluded_and_unknown_images() {
    let mut r = Rig::new(&[("a.png", 8, 8, [10, 10, 10]), ("b.png", 8, 8, [20, 20, 20])]);
    // Only the images in src/a (none here) ... use a folder that holds only a.png.
    let dir = r.t.path("only");
    fs::create_dir_all(&dir).expect("dir");
    fs::copy(&r.catalog[0].source, dir.join("a.png")).expect("copy");
    r.catalog = vec![item(1, dir.join("a.png")), r.catalog[1].clone()];
    assert!(r.g.node_mut(r.src).expect("src").set_data(A, Some(NodeData::Folder(dir.to_string_lossy().into_owned()))));
    r.p("format", 1.0);
    let rep = r.run_with(&[ImageId(1), ImageId(2), ImageId(77)], DisplayTransform::default());
    assert!(matches!(rep.items[0].1, ItemStatus::Written { .. }));
    assert_eq!(rep.items[1].1, ItemStatus::Excluded);
    assert!(matches!(&rep.items[2].1, ItemStatus::Failed(m) if m.contains("77")), "{:?}", rep.items[2]);
    assert_eq!(files_in(&r.out()), vec!["a.png"]);
    let sel = [item(1, dir.join("a.png")), item(2, r.catalog[1].source.clone()), item(77, PathBuf::from("none.png"))];
    assert_eq!(rep.retry_items(&sel), vec![sel[2].clone()], "excluded images are not retried");
}

#[test]
fn individual_settings_go_to_different_folders() {
    let mut r = Rig::new(&[("a.png", 8, 8, [10, 10, 10]), ("b.png", 8, 8, [20, 20, 20])]);
    let (d1, d2) = (r.t.path("one"), r.t.path("two"));
    {
        let n = r.g.node_mut(r.exp).expect("node");
        n.mode = Mode::Individual;
        let t = |d: &Path, name: &str| NodeData::Export(ExportText { out_dir: d.to_string_lossy().into_owned(), name_template: name.into(), ..Default::default() });
        assert!(n.set_data(ImageId(1), Some(t(&d1, "{name}_first"))));
        assert!(n.set_data(ImageId(2), Some(t(&d2, "{name}_second"))));
        assert!(n.set_param("format", ImageId(1), 1.0));
        assert!(n.set_param("format", ImageId(2), 0.0));
        assert!(n.set_param("quality", ImageId(2), 70.0));
    }
    let rep = r.run();
    assert_eq!(rep.written(), 2, "{rep:?}");
    assert_eq!(written_of(&rep.items[0].1), d1.join("a_first.png"));
    assert_eq!(written_of(&rep.items[1].1), d2.join("b_second.jpg"));
    assert!(!r.out().exists(), "the base folder is not used when every image has its own");
}

#[test]
fn cancel_mid_way_and_retry() {
    let mut r = Rig::new(&[
        ("a.png", 8, 8, [10, 10, 10]),
        ("b.png", 8, 8, [20, 20, 20]),
        ("c.png", 8, 8, [30, 30, 30]),
        ("d.png", 8, 8, [40, 40, 40]),
    ]);
    r.p("format", 1.0);
    let cancel = CancelToken::new();
    let rep = export_node(&r.g, r.exp, &r.catalog, &r.ids(), DisplayTransform::default(), &cancel, &mut |p| {
        if p.done == 2 {
            cancel.cancel();
        }
    })
    .expect("export");
    assert_eq!((rep.written(), rep.cancelled()), (2, 2));
    assert_eq!(files_in(&r.out()), vec!["a.png", "b.png"]);
    let retry = rep.retry_items(&r.catalog);
    assert_eq!(retry, r.catalog[2..].to_vec());
    let ids: Vec<ImageId> = retry.iter().map(|i| i.id).collect();
    let rep2 = r.run_with(&ids, DisplayTransform::default());
    assert_eq!(rep2.written(), 2);
    assert_eq!(files_in(&r.out()), vec!["a.png", "b.png", "c.png", "d.png"]);

    // A file that cannot be decoded fails alone; after repair the retry succeeds.
    let mut r = Rig::new(&[("good.png", 8, 8, [10, 10, 10]), ("bad.png", 8, 8, [20, 20, 20])]);
    r.p("format", 1.0);
    fs::write(&r.catalog[1].source, b"not a png").expect("write");
    let rep = r.run();
    assert_eq!((rep.written(), rep.failed()), (1, 1));
    let retry = rep.retry_items(&r.catalog);
    assert_eq!(retry, vec![r.catalog[1].clone()]);
    write_rgb(&r.catalog[1].source, 8, 8, [20, 20, 20]);
    let rep = r.run_with(&[retry[0].id], DisplayTransform::default());
    assert_eq!(rep.written(), 1);
}

// ------------------------------------------------------------------------------ Source, Merge, Layer

struct Folders {
    t: TempDir,
    catalog: Vec<ExportItem>,
}

/// `groups`: (folder, [(file name, colour)]). Ids are 1.. in the order given.
type Files<'a> = &'a [(&'a str, [u8; 3])];

fn folders(groups: &[(&str, Files)]) -> Folders {
    let t = TempDir::new();
    let mut catalog = Vec::new();
    for (dir, files) in groups {
        for (name, rgb) in *files {
            let p = t.path(&format!("{dir}/{name}"));
            write_rgb(&p, 6, 4, *rgb);
            catalog.push(item(catalog.len() as u64 + 1, p));
        }
    }
    Folders { t, catalog }
}

fn source_for(g: &mut Graph, dir: &Path) -> NodeId {
    let s = g.add_node(NodeKind::Source, [0.0; 2]);
    assert!(g.node_mut(s).expect("node").set_data(A, Some(NodeData::Folder(dir.to_string_lossy().into_owned()))));
    s
}

fn export_node_to(g: &mut Graph, out: &Path, format: f32) -> NodeId {
    let e = g.add_node(NodeKind::Export, [0.0; 2]);
    let n = g.node_mut(e).expect("node");
    assert!(n.set_data(A, Some(NodeData::Export(ExportText { out_dir: out.to_string_lossy().into_owned(), ..Default::default() }))));
    assert!(n.set_param("format", A, format));
    e
}

#[test]
fn standalone_generator_normals_exports_without_source() {
    let t = TempDir::new();
    let mut g = Graph::new();
    let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
    let id = ImageId(u64::MAX - generator.0);
    assert!(g.node_mut(generator).unwrap().set_param("width", id, 7.0));
    assert!(g.node_mut(generator).unwrap().set_param("height", id, 5.0));
    let normals = g.add_node(NodeKind::NormalMap, [0.0; 2]);
    assert!(g.node_mut(generator).unwrap().set_param("output", id, 1.0));
    let e = export_node_to(&mut g, &t.path("out"), 1.0);
    g.connect(generator, normals, 0).unwrap();
    g.connect(normals, e, 0).unwrap();
    let g = Graph::from_json(&g.to_json()).unwrap();
    let report = export_node(&g, e, &[], &[id], DisplayTransform::default(), &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(report.written(), 1, "{report:?}");
    let image = image::open(written_of(&report.items[0].1)).unwrap();
    assert_eq!((image.width(), image.height()), (7, 5));
}

#[test]
fn standalone_generator_exports_without_source() {
    let t = TempDir::new();
    let mut g = Graph::new();
    let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
    let id = ImageId(u64::MAX - generator.0);
    assert!(g.node_mut(generator).unwrap().set_param("width", id, 7.0));
    assert!(g.node_mut(generator).unwrap().set_param("height", id, 5.0));
    let e = export_node_to(&mut g, &t.path("out"), 1.0);
    g.connect(generator, e, 0).unwrap();
    let report = export_node(&g, e, &[], &[id], DisplayTransform::default(), &CancelToken::new(), &mut |_| {}).unwrap();
    let image = image::open(written_of(&report.items[0].1)).unwrap();
    assert_eq!((image.width(), image.height()), (7, 5));
}

fn run_node(g: &Graph, e: NodeId, f: &Folders) -> ExportReport {
    let ids: Vec<ImageId> = f.catalog.iter().map(|i| i.id).collect();
    export_node(g, e, &f.catalog, &ids, DisplayTransform::default(), &CancelToken::new(), &mut |_| {}).expect("export_node")
}

#[test]
fn source_folder_limits_the_export() {
    let f = folders(&[("a", &[("1.png", [10, 10, 10]), ("2.png", [20, 20, 20])]), ("b", &[("3.png", [30, 30, 30])])]);
    let mut g = Graph::new();
    let s = source_for(&mut g, &f.t.path("b"));
    let e = export_node_to(&mut g, &f.t.path("out"), 1.0);
    g.connect(s, e, 0).expect("connect");
    let rep = run_node(&g, e, &f);
    assert_eq!((rep.written(), rep.excluded()), (1, 2));
    assert_eq!(files_in(&f.t.path("out")), vec!["3.png"]);
}

#[test]
fn merge_first_input_wins_on_same_named_files() {
    let f = folders(&[
        ("a", &[("x.png", [10, 10, 10]), ("y.png", [20, 20, 20])]),
        ("b", &[("x.png", [200, 200, 200]), ("z.png", [30, 30, 30])]),
    ]);
    for (collision, expected_written, x_source) in [(1.0, 3usize, "a"), (2.0, 3, "b")] {
        let mut g = Graph::new();
        let (sa, sb) = (source_for(&mut g, &f.t.path("a")), source_for(&mut g, &f.t.path("b")));
        let m = g.add_node(NodeKind::Merge, [0.0; 2]);
        let e = export_node_to(&mut g, &f.t.path(&format!("out{collision}")), 1.0);
        g.connect(sa, m, 0).expect("connect");
        g.connect(sb, m, 1).expect("connect");
        g.connect(m, e, 0).expect("connect");
        assert!(g.node_mut(m).expect("merge").set_param("collision", A, collision));
        let rep = run_node(&g, e, &f);
        assert_eq!((rep.written(), rep.excluded()), (expected_written, 4 - expected_written), "{rep:?}");
        let out = f.t.path(&format!("out{collision}"));
        assert_eq!(files_in(&out), vec!["x.png", "y.png", "z.png"]);
        let x = load(&out.join("x.png"));
        let expected = load(&f.t.path(&format!("{x_source}/x.png")));
        assert_eq!(x, expected, "x.png comes from folder {x_source}");
    }

    // Keep both: two images would be written to x.png. Skip skips the later one, Rename numbers it,
    // Overwrite is refused before any work.
    for (collision, name) in [(0.0, "skip"), (1.0, "overwrite"), (2.0, "rename")] {
        let mut g = Graph::new();
        let (sa, sb) = (source_for(&mut g, &f.t.path("a")), source_for(&mut g, &f.t.path("b")));
        let m = g.add_node(NodeKind::Merge, [0.0; 2]);
        let e = export_node_to(&mut g, &f.t.path(&format!("out-both-{name}")), 1.0);
        g.connect(sa, m, 0).expect("connect");
        g.connect(sb, m, 1).expect("connect");
        g.connect(m, e, 0).expect("connect");
        assert!(g.node_mut(e).expect("export").set_param("collision", A, collision));
        let ids: Vec<ImageId> = f.catalog.iter().map(|i| i.id).collect();
        let res = export_node(&g, e, &f.catalog, &ids, DisplayTransform::default(), &CancelToken::new(), &mut |_| {});
        let out = f.t.path(&format!("out-both-{name}"));
        match name {
            "overwrite" => {
                assert!(matches!(res, Err(ExportError::InvalidSettings(m)) if m.contains("both be written")));
                assert!(!out.exists());
            }
            "skip" => {
                let rep = res.expect("export");
                assert_eq!((rep.written(), rep.skipped()), (3, 1), "{rep:?}");
                assert_eq!(files_in(&out), vec!["x.png", "y.png", "z.png"]);
            }
            _ => {
                let rep = res.expect("export");
                assert_eq!((rep.written(), rep.skipped(), rep.failed()), (4, 0, 0), "{rep:?}");
                assert_eq!(files_in(&out), vec!["x (2).png", "x.png", "y.png", "z.png"]);
                assert_eq!(load(&out.join("x.png")), load(&f.t.path("a/x.png")));
                assert_eq!(load(&out.join("x (2).png")), load(&f.t.path("b/x.png")));
            }
        }
    }
}

fn rgb_of_file(path: &Path) -> Vec<u8> {
    rgb8_of(path).2
}

fn layer_export(blend: f32, opacity: f32) -> (Folders, Vec<PathBuf>, usize) {
    let f = folders(&[
        (
            "a",
            &[("a1.png", [200, 100, 50]), ("a2.png", [60, 160, 220]), ("a3.png", [120, 120, 40]), ("a4.png", [30, 90, 250])],
        ),
        ("b", &[("b1.png", [180, 220, 100]), ("b2.png", [90, 40, 200])]),
    ]);
    let mut g = Graph::new();
    let (sa, sb) = (source_for(&mut g, &f.t.path("a")), source_for(&mut g, &f.t.path("b")));
    let l = g.add_node(NodeKind::Layer, [0.0; 2]);
    let e = export_node_to(&mut g, &f.t.path("out"), 1.0);
    g.connect(sa, l, 0).expect("connect");
    g.connect(sb, l, 1).expect("connect");
    g.connect(l, e, 0).expect("connect");
    {
        let n = g.node_mut(l).expect("layer");
        assert!(n.set_param("l2_blend", A, blend));
        assert!(n.set_param("l2_opacity", A, opacity));
    }
    DECODES.with(|c| c.set(0));
    let rep = run_node(&g, e, &f);
    let decodes = DECODES.with(|c| c.get());
    assert_eq!((rep.written(), rep.excluded(), rep.failed()), (4, 2, 0), "{rep:?}");
    let paths = rep.items.iter().filter_map(|(_, s)| if let ItemStatus::Written { path, .. } = s { Some(path.clone()) } else { None }).collect();
    (f, paths, decodes)
}

/// Linear value of an 8-bit sRGB code.
fn lin(v: u8) -> f64 {
    srgb_dec(v as f64 / 255.0)
}

fn flat_rgb(path: &Path) -> [u8; 3] {
    let px = rgb_of_file(path);
    [px[0], px[1], px[2]]
}

#[test]
fn layer_multiply_pairs_the_images_like_match() {
    let (f, paths, decodes) = layer_export(3.0, 1.0); // Multiply, opacity 1
    assert_eq!(paths.len(), 4);
    let names: Vec<String> = paths.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["a1.png", "a2.png", "a3.png", "a4.png"]);
    // Pairs (1,1) (2,2) (3,1) (4,2): the bottom layer is folder a, layer 2 is folder b.
    // Multiply happens on the ACEScg working pixels, so the expectation is built from those.
    let pairs = [("a1.png", "b1.png"), ("a2.png", "b2.png"), ("a3.png", "b1.png"), ("a4.png", "b2.png")];
    for (path, (a, b)) in paths.iter().zip(pairs) {
        let (la, lb) = (load(&f.t.path(&format!("a/{a}"))), load(&f.t.path(&format!("b/{b}"))));
        let mut comp = la.clone();
        for (d, l) in comp.data_mut().chunks_mut(4).zip(lb.data().chunks(4)) {
            for c in 0..3 {
                d[c] *= l[c];
            }
        }
        let expected = DisplayTransform::default().apply(&comp).to_rgba8();
        let got = rgb8_of(path).2;
        for c in 0..3 {
            assert!((got[c] as i32 - expected[c] as i32).abs() <= 1, "{a} x {b} channel {c}: {} vs {}", got[c], expected[c]);
        }
    }
    // One decoded image per layer per item, released after each item: 4 items x 2 layers.
    assert_eq!(decodes, 8, "decodes");
}

#[test]
fn layer_normal_half_opacity_mixes_in_linear_light() {
    let (f, paths, _) = layer_export(0.0, 0.5);
    let pairs = [("a1.png", "b1.png"), ("a2.png", "b2.png"), ("a3.png", "b1.png"), ("a4.png", "b2.png")];
    for (path, (a, b)) in paths.iter().zip(pairs) {
        let (ca, cb) = (flat_rgb(&f.t.path(&format!("a/{a}"))), flat_rgb(&f.t.path(&format!("b/{b}"))));
        let got = flat_rgb(path);
        for c in 0..3 {
            let (la, lb) = (lin(ca[c]), lin(cb[c]));
            let expected = enc(0, la + 0.5 * (lb - la)) * 255.0;
            assert!((got[c] as f64 - expected).abs() <= 1.5, "{a} + 50% {b} channel {c}: {} vs {expected:.1}", got[c]);
        }
    }
}

// ------------------------------------------------------------------------------ provider wiring

#[test]
fn provider_serves_catalog_paths_sizes_and_lens_profiles() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let t = TempDir::new();
    let (p1, p2) = (t.path("x/one.png"), t.path("x/two.png"));
    write_rgb(&p1, 10, 6, [1, 2, 3]);
    write_rgb(&p2, 4, 8, [1, 2, 3]);
    let catalog = vec![item(5, p1.clone()), item(3, p2.clone())];
    let lens_calls = AtomicUsize::new(0);
    let profile = rawgraph_core::LensProfile {
        warp: None,
        vignette: Some(rawgraph_core::VignetteRadial { k: [-0.5, 0.0, 0.0, 0.0, 0.0], center: [0.5, 0.5] }),
    };
    let read_lens = |p: &Path| {
        lens_calls.fetch_add(1, Ordering::Relaxed);
        p.ends_with("one.png").then(|| profile.clone())
    };
    let read_capture = |_: &Path| Some(rawgraph_core::CaptureInfo { iso: Some(400.0), ..Default::default() });
    let provider = CatalogProvider::new(&catalog, &read_capture, &read_lens);

    assert_eq!(provider.images(), vec![ImageId(5), ImageId(3)], "catalog order, not id order");
    let cap = provider.capture(ImageId(5)).expect("capture");
    assert_eq!(cap.path.as_deref(), Some(p1.to_string_lossy().as_ref()));
    assert_eq!(cap.iso, Some(400.0));
    assert!(provider.capture(ImageId(99)).is_none());

    assert_eq!(provider.full_size(ImageId(5)), Some((10, 6)));
    assert_eq!(provider.full_size(ImageId(3)), Some((4, 8)));
    assert_eq!(provider.full_size(ImageId(99)), None);
    assert_eq!(provider.loaded_images(), 2);
    provider.release_pixels();
    assert_eq!(provider.loaded_images(), 0);
    assert!(provider.image(ImageId(99)).is_err());

    assert_eq!(provider.lens_profile(ImageId(5)), Some(profile.clone()));
    assert_eq!(provider.lens_profile(ImageId(5)), Some(profile.clone()));
    assert_eq!(provider.lens_profile(ImageId(3)), None);
    assert_eq!(provider.lens_profile(ImageId(99)), None);
    assert_eq!(lens_calls.load(Ordering::Relaxed), 2, "each file is asked once");
}

#[test]
fn lens_correction_auto_uses_the_profile_of_the_file() {
    let mut r = Rig::one(16, 16, [180, 180, 180]);
    let lens = r.g.add_node(NodeKind::LensManual, [0.0; 2]);
    r.g.disconnect(r.exp, 0);
    r.g.connect(r.src, lens, 0).expect("connect");
    r.g.connect(lens, r.exp, 0).expect("connect");
    r.p("format", 1.0);
    r.p("collision", 1.0);
    let profile = rawgraph_core::LensProfile {
        warp: None,
        vignette: Some(rawgraph_core::VignetteRadial { k: [-0.6, 0.0, 0.0, 0.0, 0.0], center: [0.5, 0.5] }),
    };
    let reader = |_: &Path| Some(profile.clone());
    let run = |r: &Rig| {
        let rep = export_node_with(
            &r.g,
            r.exp,
            &r.catalog,
            &r.ids(),
            DisplayTransform::default(),
            &CancelToken::new(),
            &mut |_| {},
            &|_| None,
            &reader,
        )
        .expect("export");
        rgb8_of(&written_of(&rep.items[0].1)).2
    };
    let corner = |px: &[u8]| px[0] as i32;
    let manual = run(&r);
    assert_eq!(corner(&manual[..3]), 180, "Manual mode ignores the profile");

    assert!(r.g.node_mut(lens).expect("lens").set_param("mode", A, 1.0)); // Auto
    let auto = run(&r);
    let corner_auto = corner(&auto[..3]);
    let centre = auto[(8 * 16 + 8) * 3] as i32;
    assert!(corner_auto < 150, "the corner is darkened by the vignette profile: {corner_auto}");
    assert!(centre > corner_auto + 20, "the centre is darkened less than the corner: {centre} vs {corner_auto}");
}

#[test]
fn write_grey_helper_is_used() {
    // Keeps the shared helper exercised from this module as well.
    let t = TempDir::new();
    write_grey(&t.path("g.png"), 4, 4, 100);
    assert!(t.path("g.png").is_file());
}

#[test]
fn data_map_png_stores_unmanaged_rgb_values() {
    let mut rig = Rig::one(1, 1, [128, 128, 255]);
    rig.p("format", 8.0);
    let spec = export_spec(rig.g.node(rig.exp).unwrap(), A);
    assert_eq!(spec.format, ExportFileFormat::Png16Data);
    assert_eq!(settings_from_spec(&spec, DisplayTransform::default()).format, ExportFormat::Png16Data);
    let image = ImageBuf::from_rgba(2, 1, vec![0.5, 0.0, 1.0, 0.2, -1.0, 0.25, 2.0, 1.0]).unwrap();
    let path = rig.t.path("map.png");
    let settings = ExportSettings { format: ExportFormat::Png16Data, embed_profile: true, ..Default::default() };
    data_map_output(&image).write(&path, &settings, true, &None).unwrap();
    let decoded = image::open(&path).unwrap().to_rgb16();
    assert_eq!(decoded.into_raw(), vec![32768, 0, 65535, 0, 16384, 65535]);
    assert!(icc_of(&path).is_none());
    let bytes = fs::read(&path).unwrap();
    let mut pos = 8;
    while pos + 12 <= bytes.len() {
        let n = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = &bytes[pos + 4..pos + 8];
        assert!(!matches!(kind, b"iCCP" | b"sRGB" | b"gAMA" | b"cHRM"));
        pos += n + 12;
    }
    let source = rawgraph_io::load(&rig.catalog[0].source, None).unwrap().image;
    let expected: Vec<u16> = source.data()[..3].iter().map(|v| (v.clamp(0.0, 1.0) * 65535.0).round() as u16).collect();
    let report = rig.run();
    assert_eq!(report.written(), 1);
    let actual = written_of(&report.items[0].1);
    assert_eq!(image::open(&actual).unwrap().to_rgb16().into_raw(), expected);
    assert!(icc_of(&actual).is_none());
    assert!(exif_of(&actual).is_none());
}

#[test]
fn webp_node_maps_mode_and_writes_webp() {
    let mut r = Rig::one(32, 24, [70, 120, 180]);
    r.p("format", 9.0);
    r.p("webp_lossless", 1.0);
    let spec = export_spec(r.g.node(r.exp).unwrap(), A);
    assert_eq!(spec.format, ExportFileFormat::Webp);
    assert_eq!(settings_from_spec(&spec, DisplayTransform::default()).format, ExportFormat::Webp { quality: 92, lossless: true });
    let path = r.first_path();
    assert_eq!(path.extension().unwrap(), "webp");
    let bytes = fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
}

#[test]
fn heic_node_maps_format_and_writes_heic_or_reports_the_missing_encoder() {
    let mut r = Rig::one(32, 24, [70, 120, 180]);
    r.p("format", 10.0);
    r.p("color_space", 1.0);
    let spec = export_spec(r.g.node(r.exp).unwrap(), A);
    assert_eq!(spec.format, ExportFileFormat::Heic);
    assert_eq!(spec.format.label(), "HEIC");
    assert_eq!(settings_from_spec(&spec, DisplayTransform::default()).format, ExportFormat::Heic { quality: 92 });
    match rawgraph_io::heic_available() {
        Ok(()) => {
            let path = r.first_path();
            assert_eq!(path.extension().unwrap(), "heic");
            let bytes = fs::read(path).unwrap();
            assert_eq!(&bytes[4..8], b"ftyp");
            let img = rawgraph_io::decode_heic8(&bytes).unwrap();
            assert_eq!((img.width, img.height), (32, 24));
            assert_eq!(rawgraph_io::embedded_icc(&bytes).as_deref(), Some(rawgraph_io::icc_profile(spec.space)), "output-space profile embedded");
        }
        Err(why) => {
            // Nothing is written; the whole batch is refused with the reason.
            let err = export_node(&r.g, r.exp, &r.catalog, &r.ids(), DisplayTransform::default(), &CancelToken::new(), &mut |_| {}).unwrap_err();
            assert!(err.to_string().contains(&why), "{err}");
            assert!(!r.out().exists() || fs::read_dir(r.out()).unwrap().next().is_none());
        }
    }
}

#[test]
fn quick_export_selected_uses_full_catalog_for_layer_pairing() {
    let f = folders(&[("a", &[("a1.png", [200, 100, 50]), ("a2.png", [60, 160, 220])]),
                      ("b", &[("b1.png", [180, 220, 100]), ("b2.png", [90, 40, 200])])]);
    let mut g = Graph::new();
    let (a, b) = (source_for(&mut g, &f.t.path("a")), source_for(&mut g, &f.t.path("b")));
    let layer = g.add_node(NodeKind::Layer, [0.0; 2]);
    g.connect(a, layer, 0).unwrap();
    g.connect(b, layer, 1).unwrap();
    let mut settings = ExportSettings { out_dir: f.t.path("all"), format: ExportFormat::Png, ..Default::default() };
    let all = export_catalog(&g, layer, &f.catalog, &f.catalog, &settings, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(all.written(), 2);
    settings.out_dir = f.t.path("selected");
    let selected = export_catalog(&g, layer, &f.catalog, &f.catalog[1..2], &settings, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(selected.written(), 1);
    assert_eq!(rgb_of_file(&f.t.path("all/a2.png")), rgb_of_file(&f.t.path("selected/a2.png")));
}

#[test]
fn quick_export_never_overwrites_unselected_original() {
    let f = folders(&[("a", &[("one.png", [200, 100, 50]), ("two.png", [60, 160, 220])])]);
    let mut g = Graph::new();
    let source = source_for(&mut g, &f.t.path("a"));
    let before = fs::read(&f.catalog[1].source).unwrap();
    let settings = ExportSettings { out_dir: f.t.path("a"), format: ExportFormat::Png,
        name_template: "two".into(), on_conflict: OnConflict::Overwrite, ..Default::default() };
    let report = export_catalog(&g, source, &f.catalog, &f.catalog[..1], &settings, &CancelToken::new(), &mut |_| {}).unwrap();
    assert_eq!(report.failed(), 1);
    assert_eq!(fs::read(&f.catalog[1].source).unwrap(), before);
}

#[test]
fn an_unreadable_lut_fails_only_its_own_images() {
    let mut r = Rig::new(&[("a.png", 8, 8, [10, 20, 30]), ("b.png", 8, 8, [40, 50, 60])]);
    let good = r.t.path("good.cube");
    let bad = r.t.path("bad.cube");
    let mut cube = String::from("LUT_3D_SIZE 2\n");
    for b in 0..2 {
        for g in 0..2 {
            for rr in 0..2 {
                cube.push_str(&format!("{rr} {g} {b}\n"));
            }
        }
    }
    fs::write(&good, cube).unwrap();
    fs::write(&bad, "LUT_3D_SIZE 2\n0 0 0\nbroken\n").unwrap();
    let lut = r.g.add_node(NodeKind::Lut, [0.0; 2]);
    r.g.connect(r.src, lut, 0).expect("connect");
    r.g.connect(lut, r.exp, 0).expect("connect");
    let ids = r.ids();
    let n = r.g.node_mut(lut).expect("lut");
    assert!(n.set_param("input_space", A, 3.0));
    assert!(n.set_data(A, Some(NodeData::LutFile(good.to_string_lossy().into_owned()))));
    n.mode = Mode::Individual;
    assert!(n.set_data(ids[0], Some(NodeData::LutFile(bad.to_string_lossy().into_owned()))));
    // The viewer passes the broken file through; validate() says so, but it is no reason to stop a run.
    let _ = rawgraph_core::lut::load_cached(&bad); // validate() only reads what is known
    assert!(r.g.validate().iter().any(|i| i.per_item && i.message.contains("bad.cube")));
    let rep = r.run();
    assert_eq!((rep.written(), rep.failed()), (1, 1), "{:?}", rep.items);
    match &rep.items[0].1 {
        ItemStatus::Failed(m) => assert!(m.contains("bad.cube") && m.contains("line 3"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(rep.items[1].1, ItemStatus::Written { .. }));
}

// ------------------------------------------------------------------------------ keywords

fn tag(r: &mut Rig, id: usize, keywords: &[&str]) {
    r.catalog[id].keywords = keywords.iter().map(|k| (*k).to_owned()).collect();
}

#[test]
fn keywords_reach_the_exported_files_and_follow_the_switch() {
    for (format, ext) in [(0.0, "jpg"), (1.0, "png"), (3.0, "tif"), (9.0, "webp")] {
        let mut r = Rig::one(16, 16, [90, 90, 90]);
        tag(&mut r, 0, &["Reise|Österreich|Graz", "Food"]);
        r.p("format", format);
        r.p("collision", 1.0);
        let path = r.first_path();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some(ext));
        let packet = rawgraph_io::read_xmp_packet(&path).unwrap_or_else(|| panic!("{ext}: no packet"));
        assert!(packet.contains("<rdf:li>Graz</rdf:li>") && packet.contains("<rdf:li>Reise|Österreich|Graz</rdf:li>"), "{ext}: {packet}");
        // Switch off: the file carries no keywords.
        r.p("meta_keywords", 0.0);
        assert_eq!(rawgraph_io::read_xmp_packet(&r.first_path()), None, "{ext}");
    }
    // Other formats and untagged images: no packet.
    let mut r = Rig::new(&[("a.png", 8, 8, [10, 10, 10]), ("b.png", 8, 8, [10, 10, 10])]);
    tag(&mut r, 0, &["x"]);
    let report = r.run();
    assert_eq!(rawgraph_io::read_xmp_packet(&written_of(&report.items[1].1)), None);
    assert!(rawgraph_io::read_xmp_packet(&written_of(&report.items[0].1)).is_some_and(|p| p.contains("<rdf:li>x</rdf:li>")));
}

#[test]
fn the_keywords_token_names_the_files() {
    let mut r = Rig::new(&[("a.png", 8, 8, [10, 10, 10]), ("b.png", 8, 8, [10, 10, 10]), ("c.png", 8, 8, [10, 10, 10])]);
    tag(&mut r, 0, &["Travel|Italy|Rome", "a/b"]);
    tag(&mut r, 1, &["Ärger"]);
    r.text(|x| x.name_template = "{name}_{keywords}".into());
    let report = r.run();
    let names: Vec<String> = report.items.iter().map(|(_, s)| written_of(s).file_name().and_then(|n| n.to_str()).unwrap_or("").to_owned()).collect();
    assert_eq!(names, ["a_Rome-a_b.jpg", "b_Ärger.jpg", "c_untagged.jpg"], "last level of each keyword, unsafe characters replaced, `untagged` without keywords");
}

#[test]
fn the_keyword_measure_of_a_filter_sees_the_items_keywords() {
    let mut r = Rig::new(&[("a.png", 8, 8, [10, 10, 10]), ("b.png", 8, 8, [10, 10, 10]), ("c.png", 8, 8, [10, 10, 10])]);
    tag(&mut r, 0, &["Wedding|Church"]);
    tag(&mut r, 2, &["wedding"]);
    let f = r.g.add_node(NodeKind::MetadataFilter, [0.0; 2]);
    r.g.connect(r.src, f, 0).expect("connect");
    r.g.disconnect(r.exp, 0);
    r.g.connect(f, r.exp, 0).expect("connect");
    let node = r.g.node_mut(f).expect("filter");
    node.set_param("measure", A, 5.0);
    assert!(node.set_data(A, Some(NodeData::Keyword("WEDDING".into()))));
    let report = r.run();
    let status: Vec<bool> = report.items.iter().map(|(_, s)| matches!(s, ItemStatus::Written { .. })).collect();
    assert_eq!(status, [true, false, true]);
    let node = r.g.node_mut(f).expect("filter");
    node.set_param("keyword_op", A, 1.0);
    r.p("collision", 1.0);
    let status: Vec<bool> = r.run().items.iter().map(|(_, s)| matches!(s, ItemStatus::Written { .. })).collect();
    assert_eq!(status, [false, true, false], "does not contain");
}
