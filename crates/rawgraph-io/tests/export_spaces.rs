//! Output colour spaces, GPS / description metadata, metadata selection and AVIF files, written to
//! temporary files and read back.

use std::path::{Path, PathBuf};

use image::{ImageDecoder, ImageReader};
use rawgraph_core::color::OutputSpace;
use rawgraph_core::MetaSelection;
use rawgraph_io::*;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("rawgraph-io-spaces-{}-{name}", std::process::id()));
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
        ..Default::default()
    }
}

fn icc_of(path: &Path) -> Option<Vec<u8>> {
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

fn text(ex: &exif::Exif, tag: exif::Tag) -> Option<String> {
    match &ex.get_field(tag, exif::In::PRIMARY)?.value {
        exif::Value::Ascii(v) => v.first().map(|s| String::from_utf8_lossy(s).to_string()),
        _ => None,
    }
}

fn uint(ex: &exif::Exif, tag: exif::Tag) -> Option<u32> {
    ex.get_field(tag, exif::In::PRIMARY)?.value.get_uint(0)
}

fn opts(metadata: Option<ExportMetadata>, space: OutputSpace) -> SaveOptions {
    SaveOptions { jpeg_quality: 95, overwrite: true, embed_profile: true, space, metadata }
}

const SPACES: [OutputSpace; 4] = [OutputSpace::Srgb, OutputSpace::DisplayP3, OutputSpace::AdobeRgb, OutputSpace::Rec2020];

#[test]
fn every_writer_embeds_the_profile_of_its_space() {
    let d = Dir::new("profiles");
    let (px8, px16) = (rgb8(24, 12), rgb16(24, 12));
    for space in SPACES {
        let want = icc_profile(space);
        for ext in ["jpg", "png", "tif"] {
            let p = d.file(&format!("{space:?}8.{ext}"));
            save_srgb8_with(&p, 24, 12, &px8, &opts(Some(meta()), space)).unwrap_or_else(|e| panic!("{space:?} {ext}: {e}"));
            assert_eq!(icc_of(&p).as_deref(), Some(want), "{space:?} {ext}: profile");
            let ex = exif_of(&p).expect("exif");
            let want_cs = if space == OutputSpace::Srgb { 1 } else { 0xFFFF };
            assert_eq!(uint(&ex, exif::Tag::ColorSpace), Some(want_cs), "{space:?} {ext}: EXIF ColorSpace");
        }
        for ext in ["png", "tif"] {
            let p = d.file(&format!("{space:?}16.{ext}"));
            save_srgb16_with(&p, 24, 12, &px16, &opts(None, space)).unwrap_or_else(|e| panic!("{space:?} {ext}: {e}"));
            assert_eq!(icc_of(&p).as_deref(), Some(want), "{space:?} 16-bit {ext}: profile");
            assert_eq!(image::open(&p).expect("open").to_rgb16().as_raw(), &px16, "{space:?} {ext}: pixels untouched");
        }
        // The switch still turns the profile off.
        let p = d.file(&format!("{space:?}-off.png"));
        save_srgb8_with(&p, 24, 12, &px8, &SaveOptions { embed_profile: false, ..opts(None, space) }).expect("write");
        assert!(icc_of(&p).is_none());
    }
    for (i, a) in SPACES.iter().enumerate() {
        for b in &SPACES[i + 1..] {
            assert_ne!(icc_profile(*a), icc_profile(*b));
        }
    }
    assert_eq!(srgb_icc_profile(), icc_profile(OutputSpace::Srgb));
}

fn with_gps(gps: GpsInfo, description: Option<&str>) -> ExportMetadata {
    ExportMetadata { gps: Some(gps), description: description.map(String::from), ..meta() }
}

fn gps_cases() -> Vec<GpsInfo> {
    vec![
        GpsInfo { latitude: 47.070_713_3, longitude: 15.439_504_2, altitude_m: Some(353.4) }, // N, E, above sea level
        GpsInfo { latitude: -33.868_820_1, longitude: 151.209_296_4, altitude_m: Some(58.0) }, // S, E
        GpsInfo { latitude: 40.712_776, longitude: -74.005_974, altitude_m: Some(-12.5) },     // N, W, below sea level
        GpsInfo { latitude: -54.801_9, longitude: -68.303_0, altitude_m: None },               // S, W, no altitude
        GpsInfo { latitude: 90.0, longitude: 180.0, altitude_m: Some(0.0) },
        GpsInfo { latitude: -90.0, longitude: -180.0, altitude_m: Some(-0.001) },
    ]
}

fn assert_gps(got: Option<GpsInfo>, want: GpsInfo, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: no GPS read back"));
    assert!((got.latitude - want.latitude).abs() < 1e-7, "{what}: lat {} vs {}", got.latitude, want.latitude);
    assert!((got.longitude - want.longitude).abs() < 1e-7, "{what}: lon {} vs {}", got.longitude, want.longitude);
    match (got.altitude_m, want.altitude_m) {
        (None, None) => {}
        (Some(a), Some(b)) => assert!((a - b).abs() < 6e-4, "{what}: alt {a} vs {b}"),
        other => panic!("{what}: altitude {other:?}"),
    }
}

#[test]
fn gps_and_description_roundtrip_in_jpeg_png_and_tiff() {
    let d = Dir::new("gps");
    let px = rgb8(16, 16);
    let description = "Café am Hauptplatz – Größe 日本 ©";
    for (i, gps) in gps_cases().into_iter().enumerate() {
        for ext in ["jpg", "png", "tif"] {
            let p = d.file(&format!("g{i}.{ext}"));
            save_srgb8_with(&p, 16, 16, &px, &opts(Some(with_gps(gps, Some(description))), OutputSpace::Srgb)).unwrap_or_else(|e| panic!("{ext}: {e}"));
            let back = read_export_metadata(&p);
            assert_gps(back.gps, gps, &format!("{ext} case {i}"));
            assert_eq!(back.description.as_deref(), Some(description), "{ext}: description");
            // The other fields are intact.
            let strip = |m: ExportMetadata| ExportMetadata { gps: None, description: None, software: None, ..m };
            assert_eq!(strip(back), strip(meta()), "{ext}: other fields");
            // Raw EXIF level: references and altitude reference.
            let ex = exif_of(&p).expect("exif");
            let field = |t| ex.get_field(t, exif::In::PRIMARY).map(|f| f.display_value().to_string());
            assert_eq!(field(exif::Tag::GPSLatitudeRef).as_deref(), Some(if gps.latitude < 0.0 { "S" } else { "N" }), "{ext} case {i}");
            assert_eq!(field(exif::Tag::GPSLongitudeRef).as_deref(), Some(if gps.longitude < 0.0 { "W" } else { "E" }), "{ext} case {i}");
            match gps.altitude_m {
                Some(a) => assert_eq!(uint(&ex, exif::Tag::GPSAltitudeRef), Some(u32::from(a < 0.0)), "{ext} case {i}: altitude ref"),
                None => assert!(field(exif::Tag::GPSAltitude).is_none(), "{ext} case {i}"),
            }
        }
    }
    // The 16-bit paths carry GPS as well.
    let gps = gps_cases()[2];
    for ext in ["tif", "png"] {
        let p = d.file(&format!("g16.{ext}"));
        save_srgb16_with(&p, 16, 16, &rgb16(16, 16), &opts(Some(with_gps(gps, None)), OutputSpace::DisplayP3)).expect("write");
        assert_gps(read_export_metadata(&p).gps, gps, &format!("{ext} 16-bit"));
    }
}

#[test]
fn description_is_stored_as_utf8_in_imagedescription() {
    let d = Dir::new("desc");
    let p = d.file("d.jpg");
    let md = ExportMetadata { description: Some("Größe".into()), ..Default::default() };
    save_srgb8_with(&p, 8, 8, &rgb8(8, 8), &opts(Some(md), OutputSpace::Srgb)).expect("write");
    let ex = exif_of(&p).expect("exif");
    let f = ex.get_field(exif::Tag::ImageDescription, exif::In::PRIMARY).expect("tag 270");
    let exif::Value::Ascii(parts) = &f.value else { panic!("not ASCII-typed") };
    assert_eq!(parts[0], "Größe".as_bytes(), "UTF-8 bytes in the ASCII-typed entry");
    // No description, no GPS: neither tag exists.
    let q = d.file("n.jpg");
    save_srgb8_with(&q, 8, 8, &rgb8(8, 8), &opts(Some(ExportMetadata::default()), OutputSpace::Srgb)).expect("write");
    let ex = exif_of(&q).expect("exif");
    assert!(ex.get_field(exif::Tag::ImageDescription, exif::In::PRIMARY).is_none());
    assert!(ex.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY).is_none());
    // Invalid positions are not written.
    for bad in [f64::NAN, 91.0, -91.0] {
        let md = ExportMetadata { gps: Some(GpsInfo { latitude: bad, longitude: 10.0, altitude_m: None }), ..Default::default() };
        let q = d.file("bad.jpg");
        save_srgb8_with(&q, 8, 8, &rgb8(8, 8), &opts(Some(md), OutputSpace::Srgb)).expect("write");
        assert!(read_export_metadata(&q).gps.is_none());
    }
}

#[test]
fn select_applies_every_switch() {
    let all = MetaSelection { camera: true, exposure: true, date: true, gps: true, author: true, software: true };
    let none = MetaSelection { camera: false, exposure: false, date: false, gps: false, author: false, software: false };
    let full = ExportMetadata { software: Some("App 1".into()), ..with_gps(gps_cases()[0], Some("desc")) };
    assert_eq!(full.select(&all), full, "all on keeps everything");
    assert_eq!(
        full.select(&none),
        ExportMetadata { software: Some(String::new()), description: Some("desc".into()), ..Default::default() },
        "all off keeps only the description"
    );

    let only = |f: &dyn Fn(&mut MetaSelection)| {
        let mut s = none;
        f(&mut s);
        full.select(&s)
    };
    let camera = only(&|s| s.camera = true);
    assert_eq!((&camera.make, &camera.model, &camera.lens), (&full.make, &full.model, &full.lens));
    assert!(camera.iso.is_none() && camera.datetime_original.is_none() && camera.gps.is_none() && camera.artist.is_none());
    let exposure = only(&|s| s.exposure = true);
    assert_eq!((exposure.iso, exposure.exposure_time, exposure.f_number, exposure.focal_length), (full.iso, full.exposure_time, full.f_number, full.focal_length));
    assert!(exposure.make.is_none() && exposure.copyright.is_none());
    let date = only(&|s| s.date = true);
    assert_eq!(date.datetime_original, full.datetime_original);
    assert!(date.iso.is_none() && date.make.is_none());
    let gps = only(&|s| s.gps = true);
    assert_eq!(gps.gps, full.gps);
    assert!(gps.datetime_original.is_none());
    let author = only(&|s| s.author = true);
    assert_eq!((&author.artist, &author.copyright), (&full.artist, &full.copyright));
    assert!(author.gps.is_none() && author.make.is_none());
    let software = only(&|s| s.software = true);
    assert_eq!(software.software.as_deref(), Some("App 1"));
    assert!(software.artist.is_none());

    // A switched-off Software tag really leaves the tag out; a default one still writes RawGraph.
    let d = Dir::new("select");
    let p = d.file("s.jpg");
    save_srgb8_with(&p, 8, 8, &rgb8(8, 8), &opts(Some(full.select(&none)), OutputSpace::Srgb)).expect("write");
    let ex = exif_of(&p).expect("exif");
    assert!(ex.get_field(exif::Tag::Software, exif::In::PRIMARY).is_none());
    assert!(ex.get_field(exif::Tag::Make, exif::In::PRIMARY).is_none());
    assert!(ex.get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY).is_none());
    assert_eq!(text(&ex, exif::Tag::ImageDescription).as_deref(), Some("desc"));
    let no_software = ExportMetadata { software: None, ..full.select(&all) };
    save_srgb8_with(&p, 8, 8, &rgb8(8, 8), &opts(Some(no_software), OutputSpace::Srgb)).expect("write");
    assert_eq!(text(&exif_of(&p).expect("exif"), exif::Tag::Software).as_deref(), Some("RawGraph"));
}

#[test]
fn avif_files_have_the_signature_exif_and_follow_the_overwrite_rule() {
    let d = Dir::new("avif");
    let px = rgb8(48, 32);
    let md = with_gps(gps_cases()[1], Some("Straße"));
    let p = d.file("a.avif");
    let o = |space| SaveOptions { jpeg_quality: 60, overwrite: false, ..opts(Some(md.clone()), space) };
    save_avif8(&p, 48, 32, &px, &o(OutputSpace::DisplayP3)).expect("write");
    let bytes = std::fs::read(&p).expect("read");
    assert_eq!(&bytes[4..8], b"ftyp");
    assert_eq!(&bytes[8..12], b"avif");
    // The Exif item holds a TIFF stream the EXIF reader accepts, with GPS and description.
    let at = bytes.windows(4).position(|w| w == b"II*\0").expect("TIFF stream in the Exif item");
    let (fields, _) = exif::parse_exif(&bytes[at..]).expect("valid exif");
    assert!(fields.iter().any(|f| f.tag == exif::Tag::GPSLatitude));
    assert!(fields.iter().any(|f| f.tag == exif::Tag::ImageDescription));
    // Existing file: refused and untouched.
    assert!(matches!(save_avif8(&p, 48, 32, &px, &o(OutputSpace::Srgb)), Err(IoError::Exists(_))));
    assert_eq!(std::fs::read(&p).expect("read"), bytes);
    save_avif8(&p, 48, 32, &px, &SaveOptions { overwrite: true, ..o(OutputSpace::Rec2020) }).expect("overwrite");
    // Adobe RGB cannot be signalled: clear error, nothing written; argument errors as for the others.
    let q = d.file("adobe.avif");
    let err = save_avif8(&q, 48, 32, &px, &o(OutputSpace::AdobeRgb)).expect_err("Adobe RGB");
    assert!(err.to_string().contains("Adobe RGB"), "{err}");
    assert!(!q.exists());
    assert!(matches!(save_avif8(&q, 48, 33, &px, &o(OutputSpace::Srgb)), Err(IoError::Io(_))));
    assert!(matches!(save_avif8(&q, 0, 0, &[], &o(OutputSpace::Srgb)), Err(IoError::Io(_))));
    let names: Vec<_> = std::fs::read_dir(&d.0).expect("dir").filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert_eq!(names.len(), 1, "no temporary files left: {names:?}");
}
