//! Export writers (8/16-bit, ICC profile, EXIF), metadata reading and XMP packets, on synthetic files.

use std::path::{Path, PathBuf};

use image::{ImageDecoder, ImageReader};
use rawgraph_core::color::OutputSpace;
use rawgraph_io::*;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("rawgraph-io-fmt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("temp dir");
        Dir(p)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rgb8(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[(x * 255 / (w - 1).max(1)) as u8, (y * 255 / (h - 1).max(1)) as u8, ((x + y) * 3 % 256) as u8]);
        }
    }
    v
}

fn rgb16(w: u32, h: u32) -> Vec<u16> {
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[(x * 65535 / (w - 1).max(1)) as u16, (y * 65535 / (h - 1).max(1)) as u16, ((x * 977 + y * 1303) % 65536) as u16]);
        }
    }
    v
}

fn meta() -> ExportMetadata {
    ExportMetadata {
        make: Some("SONY".into()),
        model: Some("ILCE-7M4".into()),
        lens: Some("FE 35mm F1.4 GM".into()),
        iso: Some(640),
        exposure_time: Some((1, 250)),
        f_number: Some((28, 10)),
        focal_length: Some((35, 1)),
        datetime_original: Some("2026:09:26 14:03:09".into()),
        artist: Some("A. Photographer".into()),
        copyright: Some("(c) Test".into()),
        software: None,
        gps: None,
        description: None,
        keywords: Vec::new(),
    }
}

fn icc_of(path: &Path) -> Option<Vec<u8>> {
    // `image` drops a TIFF's profile once limits are applied; read it through the TIFF decoder itself.
    if matches!(path.extension().and_then(|e| e.to_str()), Some("tif" | "tiff")) {
        let mut t = image::codecs::tiff::TiffDecoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?)).ok()?;
        return t.icc_profile().ok().flatten();
    }
    let mut d = ImageReader::open(path).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
    d.icc_profile().ok().flatten()
}

fn exif_of(path: &Path) -> Option<exif::Exif> {
    let f = std::fs::File::open(path).ok()?;
    exif::Reader::new().read_from_container(&mut std::io::BufReader::new(f)).ok()
}

/// True if the file carries the EXIF block we write (a TIFF always has some IFD0 fields, so look
/// for the Software tag that every written block contains).
fn has_exif(path: &Path) -> bool {
    exif_of(path).is_some_and(|ex| ex.get_field(exif::Tag::Software, exif::In::PRIMARY).is_some())
}

fn text(ex: &exif::Exif, tag: exif::Tag) -> Option<String> {
    match &ex.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(v) => v.first().map(|s| String::from_utf8_lossy(s).to_string()),
        _ => None,
    }
}

fn uint(ex: &exif::Exif, tag: exif::Tag) -> Option<u32> {
    ex.get_field(tag, exif::In::PRIMARY)?.value.get_uint(0)
}

fn ratio(ex: &exif::Exif, tag: exif::Tag) -> Option<(u32, u32)> {
    match &ex.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Rational(v) => v.first().map(|r| (r.num, r.denom)),
        _ => None,
    }
}

fn assert_exif_matches(path: &Path, want: &ExportMetadata) {
    let ex = exif_of(path).unwrap_or_else(|| panic!("{}: EXIF not readable", path.display()));
    assert_eq!(text(&ex, exif::Tag::Make), want.make);
    assert_eq!(text(&ex, exif::Tag::Model), want.model);
    assert_eq!(text(&ex, exif::Tag::LensModel), want.lens);
    assert_eq!(uint(&ex, exif::Tag::PhotographicSensitivity), want.iso);
    assert_eq!(ratio(&ex, exif::Tag::ExposureTime), want.exposure_time);
    assert_eq!(ratio(&ex, exif::Tag::FNumber), want.f_number);
    assert_eq!(ratio(&ex, exif::Tag::FocalLength), want.focal_length);
    assert_eq!(text(&ex, exif::Tag::DateTimeOriginal), want.datetime_original);
    assert_eq!(text(&ex, exif::Tag::Artist), want.artist);
    assert_eq!(text(&ex, exif::Tag::Copyright), want.copyright);
    assert_eq!(text(&ex, exif::Tag::Software).as_deref(), Some(want.software.as_deref().unwrap_or("RawGraph")));
    assert_eq!(uint(&ex, exif::Tag::Orientation), Some(1));
}

fn opts(metadata: Option<ExportMetadata>, profile: bool) -> SaveOptions {
    SaveOptions { jpeg_quality: 95, overwrite: true, embed_profile: profile, space: OutputSpace::Srgb, metadata }
}

#[test]
fn defaults() {
    let o = SaveOptions::default();
    assert_eq!((o.jpeg_quality, o.overwrite, o.embed_profile, o.space, o.metadata), (92, false, true, OutputSpace::Srgb, None));
}

#[test]
fn profile_and_metadata_roundtrip_in_all_8bit_formats() {
    let d = Dir::new("rt8");
    let px = rgb8(33, 17);
    for ext in ["jpg", "jpeg", "png", "tif", "tiff"] {
        let p = d.file(&format!("a.{ext}"));
        save_srgb8_with(&p, 33, 17, &px, &opts(Some(meta()), true)).unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert_eq!(icc_of(&p).as_deref(), Some(srgb_icc_profile()), "{ext}: profile");
        assert_exif_matches(&p, &meta());
        // Pixels are still readable (and exact for the lossless formats).
        let img = image::open(&p).unwrap_or_else(|e| panic!("{ext}: {e}")).to_rgb8();
        assert_eq!((img.width(), img.height()), (33, 17));
        if ext != "jpg" && ext != "jpeg" {
            assert_eq!(img.as_raw(), &px, "{ext}: pixels");
        }
    }
}

#[test]
fn flags_switch_profile_and_metadata_off() {
    let d = Dir::new("flags");
    let px = rgb8(16, 16);
    for ext in ["jpg", "png", "tif"] {
        let p = d.file(&format!("n.{ext}"));
        save_srgb8_with(&p, 16, 16, &px, &opts(None, false)).expect("write");
        assert!(icc_of(&p).is_none(), "{ext}: no profile expected");
        assert!(!has_exif(&p), "{ext}: no EXIF expected");
        // Profile only / metadata only.
        let p = d.file(&format!("p.{ext}"));
        save_srgb8_with(&p, 16, 16, &px, &opts(None, true)).expect("write");
        assert!(icc_of(&p).is_some() && !has_exif(&p), "{ext}: profile only");
        let p = d.file(&format!("m.{ext}"));
        save_srgb8_with(&p, 16, 16, &px, &opts(Some(meta()), false)).expect("write");
        assert!(icc_of(&p).is_none() && has_exif(&p), "{ext}: metadata only");
    }
}

#[test]
fn software_defaults_to_rawgraph_and_empty_metadata_is_valid() {
    let d = Dir::new("sw");
    let px = rgb8(8, 8);
    let p = d.file("e.jpg");
    save_srgb8_with(&p, 8, 8, &px, &opts(Some(ExportMetadata::default()), true)).expect("write");
    let ex = exif_of(&p).expect("exif");
    assert_eq!(text(&ex, exif::Tag::Software).as_deref(), Some("RawGraph"));
    assert_eq!(uint(&ex, exif::Tag::Orientation), Some(1));
    assert_eq!(text(&ex, exif::Tag::Make), None);
    let custom = ExportMetadata { software: Some("Other 1.0".into()), ..meta() };
    let p = d.file("c.tif");
    save_srgb8_with(&p, 8, 8, &px, &opts(Some(custom.clone()), true)).expect("write");
    assert_exif_matches(&p, &custom);
}

#[test]
fn sixteen_bit_png_and_tiff_keep_all_16_bits() {
    let d = Dir::new("rt16");
    let px = rgb16(37, 21);
    for ext in ["png", "tif", "tiff"] {
        let p = d.file(&format!("s.{ext}"));
        save_srgb16_with(&p, 37, 21, &px, &opts(Some(meta()), true)).unwrap_or_else(|e| panic!("{ext}: {e}"));
        let img = image::open(&p).unwrap_or_else(|e| panic!("{ext}: {e}"));
        assert_eq!(img.color(), image::ColorType::Rgb16, "{ext}");
        assert_eq!(img.to_rgb16().as_raw(), &px, "{ext}: 16-bit pixels");
        assert_eq!(icc_of(&p).as_deref(), Some(srgb_icc_profile()), "{ext}: profile");
        assert_exif_matches(&p, &meta());
    }
    let p = d.file("none.png");
    save_srgb16_with(&p, 37, 21, &px, &opts(None, false)).expect("write");
    assert!(icc_of(&p).is_none() && !has_exif(&p));
}

#[test]
fn sixteen_bit_jpeg_is_unsupported_and_arguments_are_checked() {
    let d = Dir::new("bad16");
    let px = rgb16(4, 4);
    assert!(matches!(save_srgb16_with(&d.file("x.jpg"), 4, 4, &px, &SaveOptions::default()), Err(IoError::Unsupported(_))));
    assert!(matches!(save_srgb16_with(&d.file("x.bmp"), 4, 4, &px, &SaveOptions::default()), Err(IoError::Unsupported(_))));
    assert!(matches!(save_srgb16_with(&d.file("x.png"), 4, 5, &px, &SaveOptions::default()), Err(IoError::Io(_))));
    assert!(matches!(save_srgb16_with(&d.file("x.png"), 0, 4, &[], &SaveOptions::default()), Err(IoError::Io(_))));
    assert!(!d.file("x.png").exists() && !d.file("x.jpg").exists());
}

#[test]
fn writes_are_atomic_and_keep_the_overwrite_rule() {
    let d = Dir::new("atomic");
    let px = rgb8(8, 8);
    let p = d.file("a.tif");
    let keep = SaveOptions { overwrite: false, ..opts(Some(meta()), true) };
    save_srgb8_with(&p, 8, 8, &px, &keep).expect("first");
    let before = std::fs::read(&p).expect("read");
    assert!(matches!(save_srgb8_with(&p, 8, 8, &px, &keep), Err(IoError::Exists(_))));
    assert!(matches!(save_srgb16_with(&p, 8, 8, &rgb16(8, 8), &keep), Err(IoError::Exists(_))));
    assert_eq!(std::fs::read(&p).expect("read"), before, "existing file must stay untouched");
    save_srgb8_with(&p, 8, 8, &px, &opts(Some(meta()), true)).expect("overwrite");
    let names: Vec<_> = std::fs::read_dir(&d.0).expect("dir").filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert_eq!(names.len(), 1, "no temporary files left: {names:?}");
}

#[test]
fn save_srgb8_without_options_writes_no_profile_and_no_metadata() {
    let d = Dir::new("plain");
    let p = d.file("a.png");
    save_srgb8(&p, 8, 8, &rgb8(8, 8), 90, false).expect("write");
    assert!(icc_of(&p).is_none() && !has_exif(&p));
}

#[test]
fn read_export_metadata_roundtrips_and_never_fails() {
    let d = Dir::new("readmeta");
    for ext in ["jpg", "png", "tif"] {
        let p = d.file(&format!("m.{ext}"));
        save_srgb8_with(&p, 8, 8, &rgb8(8, 8), &opts(Some(meta()), false)).expect("write");
        let got = read_export_metadata(&p);
        assert_eq!(got, ExportMetadata { software: None, ..meta() }, "{ext}");
        // Without EXIF: an empty struct, not a failure.
        let q = d.file(&format!("plain.{ext}"));
        save_srgb8_with(&q, 8, 8, &rgb8(8, 8), &opts(None, false)).expect("write");
        assert_eq!(read_export_metadata(&q), ExportMetadata::default(), "{ext}");
    }
    assert_eq!(read_export_metadata(&d.file("missing.jpg")), ExportMetadata::default());
    let junk = d.file("junk.arw");
    std::fs::write(&junk, b"not a raw file at all").expect("write");
    assert_eq!(read_export_metadata(&junk), ExportMetadata::default());
}

// ------------------------------------------------------------------ XMP

const PACKET: &str = "<?xpacket begin='\u{feff}' id='W5M0MpCehiHzreSzNTczkc9d'?><x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='4'/></rdf:RDF></x:xmpmeta><?xpacket end='w'?>";

fn jpeg_with_xmp(plain: &[u8]) -> Vec<u8> {
    let mut seg = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    seg.extend_from_slice(PACKET.as_bytes());
    let mut out = plain[..2].to_vec();
    // An unrelated APP2 segment first, then the XMP APP1.
    out.extend_from_slice(&[0xFF, 0xE2, 0x00, 0x06, 1, 2, 3, 4]);
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&seg);
    out.extend_from_slice(&plain[2..]);
    out
}

fn png_with_xmp(plain: &[u8]) -> Vec<u8> {
    let mut data = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
    data.extend_from_slice(PACKET.as_bytes());
    let iend = plain.len() - 12;
    let mut out = plain[..iend].to_vec();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(b"iTXt");
    out.extend_from_slice(&data);
    out.extend_from_slice(&[0, 0, 0, 0]); // CRC is not checked by the reader
    out.extend_from_slice(&plain[iend..]);
    out
}

/// Minimal TIFF with one IFD holding tag 700 (BYTE, out of line).
fn tiff_with_xmp(le: bool) -> Vec<u8> {
    let w16 = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let w32 = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let mut out = if le { b"II*\0".to_vec() } else { b"MM\0*".to_vec() };
    out.extend_from_slice(&w32(8));
    out.extend_from_slice(&w16(1));
    out.extend_from_slice(&w16(700));
    out.extend_from_slice(&w16(1));
    out.extend_from_slice(&w32(PACKET.len() as u32));
    out.extend_from_slice(&w32(8 + 2 + 12 + 4));
    out.extend_from_slice(&w32(0));
    out.extend_from_slice(PACKET.as_bytes());
    out
}

#[test]
fn xmp_packets_are_found_in_jpeg_png_and_tiff() {
    let d = Dir::new("xmp");
    let px = rgb8(8, 8);
    let (j, pn) = (d.file("p.jpg"), d.file("p.png"));
    save_srgb8_with(&j, 8, 8, &px, &opts(Some(meta()), true)).expect("jpg");
    save_srgb8_with(&pn, 8, 8, &px, &opts(Some(meta()), true)).expect("png");
    assert_eq!(read_xmp_packet(&j), None);
    assert_eq!(read_xmp_packet(&pn), None);

    let jx = d.file("x.jpg");
    std::fs::write(&jx, jpeg_with_xmp(&std::fs::read(&j).expect("read"))).expect("write");
    let got = read_xmp_packet(&jx).expect("jpeg xmp");
    assert!(got.contains("xmp:Rating='4'") && got.starts_with("<?xpacket"), "{got}");
    // The modified file still decodes (the segment is well-formed).
    assert!(image::open(&jx).is_ok());

    let px_png = d.file("x.png");
    std::fs::write(&px_png, png_with_xmp(&std::fs::read(&pn).expect("read"))).expect("write");
    assert_eq!(read_xmp_packet(&px_png).as_deref(), Some(PACKET));

    for (name, le) in [("le.tif", true), ("be.dng", false)] {
        let t = d.file(name);
        std::fs::write(&t, tiff_with_xmp(le)).expect("write");
        assert_eq!(read_xmp_packet(&t).as_deref(), Some(PACKET), "{name}");
    }
}

#[test]
fn xmp_reading_never_panics_on_damaged_files() {
    let d = Dir::new("xmp-bad");
    let base = d.file("b.jpg");
    save_srgb8_with(&base, 16, 16, &rgb8(16, 16), &opts(Some(meta()), true)).expect("jpg");
    let pn = d.file("b.png");
    save_srgb8_with(&pn, 16, 16, &rgb8(16, 16), &opts(Some(meta()), true)).expect("png");
    let samples = [
        jpeg_with_xmp(&std::fs::read(&base).expect("read")),
        png_with_xmp(&std::fs::read(&pn).expect("read")),
        tiff_with_xmp(true),
        tiff_with_xmp(false),
    ];
    let victim = d.file("v.bin");
    for (k, full) in samples.iter().enumerate() {
        let step = if full.len() > 3000 { 7 } else { 1 };
        for n in (0..full.len()).step_by(step) {
            std::fs::write(&victim, &full[..n]).expect("write");
            let _ = read_xmp_packet(&victim);
            let _ = read_export_metadata(&victim);
        }
        // Corrupt single bytes in the header area.
        for i in 0..full.len().min(64) {
            let mut v = full.clone();
            v[i] ^= 0xFF;
            std::fs::write(&victim, &v).expect("write");
            let _ = read_xmp_packet(&victim);
            let _ = read_export_metadata(&victim);
        }
        let _ = k;
    }
    assert_eq!(read_xmp_packet(&d.file("nope.jpg")), None);
}
