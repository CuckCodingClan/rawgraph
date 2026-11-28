//! Tests that need no sample files: everything is generated into the temp directory.

use std::path::{Path, PathBuf};

use image::{ImageBuffer, Rgb, Rgba};
use rawgraph_core::color::{rec709_to_acescg, srgb_eotf};
use rawgraph_core::ImageBuf;
use rawgraph_io::*;

/// Temp directory that removes itself.
struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("rawgraph-io-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn grey_image(w: u32, h: u32, v: u8) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    ImageBuffer::from_pixel(w, h, Rgb([v, v, v]))
}

#[test]
fn png_grey_118_is_neutral_018() {
    let t = Tmp::new("png-grey");
    let p = t.file("grey.png");
    grey_image(8, 6, 118).save(&p).unwrap();
    let l = load(&p, None).unwrap();
    assert_eq!(l.kind, SourceKind::Rendered);
    assert_eq!(l.full_size, (8, 6));
    for px in l.image.data().as_chunks::<4>().0.iter() {
        assert!((px[0] - px[1]).abs() < 1e-3 && (px[1] - px[2]).abs() < 1e-3, "not neutral: {px:?}");
        assert!((px[0] - 0.18).abs() < 0.005, "expected ~0.18, got {}", px[0]);
        assert_eq!(px[3], 1.0);
    }
}

#[test]
fn tiff_grey_118_is_neutral_018() {
    let t = Tmp::new("tiff-grey");
    let p = t.file("grey.tif");
    grey_image(5, 5, 118).save(&p).unwrap();
    let l = load(&p, None).unwrap();
    let px = l.image.pixel(2, 2);
    assert!((px[0] - px[1]).abs() < 1e-3 && (px[1] - px[2]).abs() < 1e-3);
    assert!((px[0] - 0.18).abs() < 0.005);
}

#[test]
fn tiff_16bit_keeps_precision() {
    let t = Tmp::new("tiff16");
    let p = t.file("deep.tif");
    let mut img: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::new(2, 1);
    img.put_pixel(0, 0, Rgb([30000, 30000, 30000]));
    img.put_pixel(1, 0, Rgb([30001, 30001, 30001]));
    img.save(&p).unwrap();
    let l = load(&p, None).unwrap();
    let (a, b) = (l.image.pixel(0, 0)[0], l.image.pixel(1, 0)[0]);
    assert!(b > a, "one 16-bit code value step must survive: {a} vs {b}");
    assert!((a - srgb_eotf(30000.0 / 65535.0)).abs() < 1e-5);
    assert!((b - srgb_eotf(30001.0 / 65535.0)).abs() < 1e-5);
}

#[test]
fn srgb_red_maps_to_acescg_red_column() {
    let t = Tmp::new("red");
    let p = t.file("red.png");
    ImageBuffer::from_pixel(3, 3, Rgb([255u8, 0, 0])).save(&p).unwrap();
    let l = load(&p, None).unwrap();
    let m = rec709_to_acescg();
    let px = l.image.pixel(1, 1);
    for k in 0..3 {
        assert!((px[k] as f64 - m[k][0]).abs() < 1e-5, "channel {k}: {} vs {}", px[k], m[k][0]);
    }
}

#[test]
fn png_alpha_is_preserved_and_opaque_otherwise() {
    let t = Tmp::new("alpha");
    let p = t.file("a.png");
    ImageBuffer::from_pixel(2, 2, Rgba([118u8, 118, 118, 128])).save(&p).unwrap();
    let l = load(&p, None).unwrap();
    assert!((l.image.pixel(0, 0)[3] - 128.0 / 255.0).abs() < 1e-6);
    let p2 = t.file("b.png");
    grey_image(2, 2, 10).save(&p2).unwrap();
    assert_eq!(load(&p2, None).unwrap().image.pixel(0, 0)[3], 1.0);
}

#[test]
fn max_dim_gives_proxy_and_keeps_full_size() {
    let t = Tmp::new("proxy");
    let p = t.file("big.png");
    grey_image(640, 480, 100).save(&p).unwrap();
    let l = load(&p, Some(100)).unwrap();
    assert_eq!(l.full_size, (640, 480));
    assert!(l.image.width().max(l.image.height()) <= 100);
    assert_eq!((l.image.width(), l.image.height()), (91, 68)); // factor ceil(640/100) = 7
    let l = load(&p, Some(4000)).unwrap();
    assert_eq!((l.image.width(), l.image.height()), (640, 480));
}

#[test]
fn source_kind_is_case_insensitive() {
    assert_eq!(source_kind(Path::new("a.JPG")), Some(SourceKind::Rendered));
    assert_eq!(source_kind(Path::new("a.Tiff")), Some(SourceKind::Rendered));
    assert_eq!(source_kind(Path::new("dir.x/b.ARW")), Some(SourceKind::Raw));
    assert_eq!(source_kind(Path::new("c.dNg")), Some(SourceKind::Raw));
    assert_eq!(source_kind(Path::new("d.txt")), None);
    assert_eq!(source_kind(Path::new("noext")), None);
    assert!(is_supported(Path::new("x.CR3")));
    assert!(!is_supported(Path::new("x.mp4")));
    for e in RAW_EXTENSIONS.iter().chain(RENDERED_EXTENSIONS) {
        assert_eq!(*e, e.to_ascii_lowercase());
        assert!(!e.starts_with('.'));
    }
}

#[test]
fn unknown_extension_is_unsupported() {
    let t = Tmp::new("unsupported");
    let p = t.file("a.txt");
    std::fs::write(&p, b"hello").unwrap();
    assert!(matches!(load(&p, None), Err(IoError::Unsupported(_))));
    assert!(matches!(read_capture_info(&p), Err(IoError::Unsupported(_))));
}

#[test]
fn missing_file_is_not_found() {
    let t = Tmp::new("missing");
    for name in ["nope.jpg", "nope.dng", "nope.PNG", "nope.arw"] {
        let p = t.file(name);
        assert!(matches!(load(&p, None), Err(IoError::NotFound(_))), "{name}");
        assert!(matches!(read_capture_info(&p), Err(IoError::NotFound(_))), "{name}");
        assert!(matches!(load_thumbnail(&p, 64), Err(IoError::NotFound(_))), "{name}");
    }
}

#[test]
fn garbage_is_a_decode_error_not_a_panic() {
    let t = Tmp::new("garbage");
    let junk: Vec<u8> = (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
    for name in ["junk.jpg", "junk.dng", "junk.png", "junk.tif", "junk.arw", "junk.cr3", "junk.nef"] {
        let p = t.file(name);
        std::fs::write(&p, &junk).unwrap();
        assert!(matches!(load(&p, None), Err(IoError::Decode(_))), "load {name}");
        assert!(matches!(load(&p, Some(64)), Err(IoError::Decode(_))), "load proxy {name}");
        assert!(matches!(read_capture_info(&p), Err(IoError::Decode(_))), "info {name}");
        assert!(matches!(load_thumbnail(&p, 64), Err(IoError::Decode(_))), "thumb {name}");
    }
    // Empty and truncated files, too.
    let p = t.file("empty.dng");
    std::fs::write(&p, b"").unwrap();
    assert!(matches!(load(&p, None), Err(IoError::Decode(_))));
    let p = t.file("trunc.jpg");
    std::fs::write(&p, [0xFF, 0xD8, 0xFF, 0xE0, 0, 16]).unwrap();
    assert!(matches!(load(&p, None), Err(IoError::Decode(_))));
}

#[test]
fn thumbnail_of_rendered_file() {
    let t = Tmp::new("thumb");
    let p = t.file("t.png");
    grey_image(400, 200, 118).save(&p).unwrap();
    let (w, h, px) = load_thumbnail(&p, 100).unwrap();
    assert!(w.max(h) <= 100 && w == 100 && h == 50);
    assert_eq!(px.len(), (w * h * 4) as usize);
    assert!(px.as_chunks::<4>().0.iter().all(|c| c[3] == 255 && (c[0] as i32 - 118).abs() <= 1));
}

fn rgb_gradient(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[(x * 7 % 256) as u8, (y * 11 % 256) as u8, ((x + y) * 3 % 256) as u8]);
        }
    }
    v
}

#[test]
fn save_srgb8_roundtrips_all_formats() {
    let t = Tmp::new("save8");
    let (w, h) = (37, 23);
    let rgb = rgb_gradient(w, h);
    for name in ["o.png", "o.tif", "o.tiff", "o.jpg", "o.JPEG"] {
        let p = t.file(name);
        save_srgb8(&p, w, h, &rgb, 92, false).unwrap();
        assert_eq!(image::image_dimensions(&p).unwrap(), (w, h), "{name}");
        let back = image::open(&p).unwrap().to_rgb8();
        if !name.to_ascii_lowercase().ends_with("jpg") && !name.to_ascii_lowercase().ends_with("jpeg") {
            assert_eq!(back.as_raw(), &rgb, "{name} must be lossless");
        }
    }
    // No temp files left behind.
    let left: Vec<_> = std::fs::read_dir(&t.0).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".rgtmp")).collect();
    assert!(left.is_empty());
}

#[test]
fn save_refuses_to_replace_unless_overwrite() {
    let t = Tmp::new("exists");
    let rgb = rgb_gradient(4, 4);
    let p = t.file("x.png");
    save_srgb8(&p, 4, 4, &rgb, 90, false).unwrap();
    let before = std::fs::read(&p).unwrap();
    assert!(matches!(save_srgb8(&p, 4, 4, &rgb, 90, false), Err(IoError::Exists(_))));
    assert_eq!(std::fs::read(&p).unwrap(), before, "existing file must stay untouched");
    let other = vec![255u8; 4 * 4 * 3];
    save_srgb8(&p, 4, 4, &other, 90, true).unwrap();
    assert_ne!(std::fs::read(&p).unwrap(), before);

    let e = t.file("x.exr");
    let img = ImageBuf::new(2, 2);
    save_exr(&e, &img, false).unwrap();
    assert!(matches!(save_exr(&e, &img, false), Err(IoError::Exists(_))));
    save_exr(&e, &img, true).unwrap();
}

#[test]
fn save_srgb8_rejects_bad_arguments() {
    let t = Tmp::new("badargs");
    let p = t.file("x.png");
    assert!(matches!(save_srgb8(&p, 4, 4, &[0u8; 10], 90, false), Err(IoError::Io(_))));
    assert!(matches!(save_srgb8(&p, 0, 4, &[], 90, false), Err(IoError::Io(_))));
    assert!(matches!(save_srgb8(&t.file("x.bmp"), 1, 1, &[0u8; 3], 90, false), Err(IoError::Unsupported(_))));
    assert!(!p.exists());
}

#[test]
fn save_exr_roundtrips_floats_exactly() {
    let t = Tmp::new("exr");
    let p = t.file("f.exr");
    let (w, h) = (9u32, 5u32);
    let mut img = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let f = (y * w + x) as f32;
            img.set_pixel(x, y, [f * 0.37 - 3.0, f / 7.0 + 1.0e-3, 1000.0 * f.sin(), 0.25 + f / 100.0]);
        }
    }
    save_exr(&p, &img, false).unwrap();
    let back = image::open(&p).unwrap().to_rgba32f();
    assert_eq!((back.width(), back.height()), (w, h));
    assert_eq!(back.as_raw().as_slice(), img.data(), "values (including >1 and <0) must be bit-identical");
}

#[test]
fn save_exr_marks_acescg_primaries() {
    let t = Tmp::new("exr-meta");
    let p = t.file("m.exr");
    save_exr(&p, &ImageBuf::new(2, 2), false).unwrap();
    let meta = exr::meta::MetaData::read_from_file(&p, false).unwrap();
    let c = meta.headers[0].shared_attributes.chromaticities.expect("chromaticities attribute");
    assert!((c.red.0 - 0.713).abs() < 1e-4 && (c.blue.1 - 0.044).abs() < 1e-4);
}

/// Minimal little-endian TIFF/EXIF block: Make, Orientation, ExposureTime 1/250, f/2.8, ISO 400, Flash fired,
/// focal length 24 mm (36 mm in 35-mm equivalent).
fn exif_block(orientation: u16) -> Vec<u8> {
    fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
        let mut e = Vec::new();
        e.extend_from_slice(&tag.to_le_bytes());
        e.extend_from_slice(&typ.to_le_bytes());
        e.extend_from_slice(&count.to_le_bytes());
        e.extend_from_slice(&value.to_le_bytes());
        e
    }
    let mut t = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
    // IFD0 at 8: 3 entries -> 2 + 36 + 4 = 42 bytes -> ends at 50. Make string at 50 (6 bytes), Exif IFD at 56.
    t.extend_from_slice(&3u16.to_le_bytes());
    t.extend(entry(0x010F, 2, 5, 50));
    t.extend(entry(0x0112, 3, 1, orientation as u32));
    t.extend(entry(0x8769, 4, 1, 56));
    t.extend_from_slice(&0u32.to_le_bytes());
    t.extend_from_slice(b"Acme\0\0");
    // Exif IFD at 56: 6 entries -> 2 + 72 + 4 = 78 -> ends at 134; rationals at 134, 142 and 150.
    t.extend_from_slice(&6u16.to_le_bytes());
    t.extend(entry(0x829A, 5, 1, 134));
    t.extend(entry(0x829D, 5, 1, 142));
    t.extend(entry(0x8827, 3, 1, 400));
    t.extend(entry(0x9209, 3, 1, 1));
    t.extend(entry(0x920A, 5, 1, 150));
    t.extend(entry(0xA405, 3, 1, 36));
    t.extend_from_slice(&0u32.to_le_bytes());
    for v in [1u32, 250, 28, 10, 240, 10] {
        t.extend_from_slice(&v.to_le_bytes());
    }
    t
}

fn jpeg_with_exif(w: u32, h: u32, orientation: u16) -> Vec<u8> {
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90)
        .encode(&rgb_gradient(w, h), w, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    assert_eq!(&jpg[..2], &[0xFF, 0xD8]);
    let tiff = exif_block(orientation);
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend(tiff);
    let mut out = jpg[..2].to_vec();
    out.extend(seg);
    out.extend_from_slice(&jpg[2..]);
    out
}

#[test]
fn jpeg_exif_metadata_and_orientation() {
    let t = Tmp::new("exif");
    let p = t.file("o6.jpg");
    std::fs::write(&p, jpeg_with_exif(8, 4, 6)).unwrap();

    let info = read_capture_info(&p).unwrap();
    assert_eq!(info.make.as_deref(), Some("Acme"));
    assert_eq!(info.model, None);
    assert_eq!(info.lens, None);
    assert_eq!(info.iso, Some(400.0));
    assert_eq!(info.shutter_s, Some(1.0 / 250.0));
    assert!((info.aperture.unwrap() - 2.8).abs() < 1e-6);
    assert_eq!(info.flash_fired, Some(true));
    assert_eq!((info.focal_mm, info.focal_35mm), (Some(24.0), Some(36.0)));
    assert!(info.ev100().is_some());

    // Orientation 6 (rotate 90 CW) turns the 8x4 landscape into a 4x8 portrait.
    let l = load(&p, None).unwrap();
    assert_eq!(l.full_size, (4, 8));
    assert_eq!((l.image.width(), l.image.height()), (4, 8));
    assert_eq!(l.capture, info);
    let (w, h, _) = load_thumbnail(&p, 64).unwrap();
    assert_eq!((w, h), (4, 8));
}

#[test]
fn jpeg_without_exif_has_empty_capture_info() {
    let t = Tmp::new("noexif");
    let p = t.file("plain.jpg");
    save_srgb8(&p, 16, 16, &rgb_gradient(16, 16), 90, false).unwrap();
    // Only the source-type flag is known (and that the focal length was looked for); every other field is missing.
    let expect = rawgraph_core::CaptureInfo { raw: Some(false), focal_read: true, ..Default::default() };
    assert_eq!(read_capture_info(&p).unwrap(), expect);
}

#[test]
fn rendered_files_report_raw_false_and_no_white_balance() {
    let t = Tmp::new("rendered-flag");
    let png = t.file("g.png");
    grey_image(8, 6, 118).save(&png).unwrap();
    let jpg = t.file("o.jpg");
    std::fs::write(&jpg, jpeg_with_exif(8, 4, 1)).unwrap();
    for p in [&png, &jpg] {
        let info = read_capture_info(p).unwrap();
        assert_eq!(info.raw, Some(false));
        assert_eq!((info.as_shot_kelvin, info.as_shot_tint, info.baseline_exposure), (None, None, None));
        assert_eq!(load(p, None).unwrap().capture, info);
    }
}

// ---------------------------------------------------------------- thumbnails and input profiles

/// sRGB codes of a full decode: ACEScg -> Rec.709 -> sRGB OETF, RGBA8.
fn full_decode_srgb8(path: &Path) -> (u32, u32, Vec<u8>) {
    let l = load(path, None).unwrap();
    let m = rawgraph_core::color::to_f32(&rawgraph_core::color::acescg_to_rec709());
    let rgba = l.image.data().as_chunks::<4>().0.iter().flat_map(|p| {
        let c = |k: usize| (rawgraph_core::color::srgb_oetf((m[k][0] * p[0] + m[k][1] * p[1] + m[k][2] * p[2]).clamp(0.0, 1.0)) * 255.0 + 0.5) as u8;
        [c(0), c(1), c(2), 255]
    }).collect();
    (l.image.width(), l.image.height(), rgba)
}

fn tagged_codes(w: u32, h: u32) -> Vec<u8> {
    // Saturated and neutral colours, so a missing conversion is large.
    (0..w * h).flat_map(|i| {
        let (x, y) = (i % w, i / w);
        [(40 + x * 3 % 200) as u8, (30 + y * 5 % 200) as u8, (200 - (x + y) % 150) as u8]
    }).collect()
}

#[test]
fn thumbnails_of_tagged_files_match_the_full_decode() {
    use rawgraph_core::color::OutputSpace;
    let t = Tmp::new("thumb-icc");
    let (w, h) = (64u32, 48u32);
    let codes = tagged_codes(w, h);
    let mut checked = 0;
    for (space, name) in [(OutputSpace::AdobeRgb, "adobe"), (OutputSpace::DisplayP3, "p3"), (OutputSpace::Rec2020, "rec2020")] {
        for ext in ["jpg", "png", "tif"] {
            let p = t.file(&format!("{name}.{ext}"));
            let options = SaveOptions { space, embed_profile: true, jpeg_quality: 98, ..Default::default() };
            save_srgb8_with(&p, w, h, &codes, &options).unwrap();
            let (tw, th, thumb) = load_thumbnail(&p, 256).unwrap();
            let (fw, fh, full) = full_decode_srgb8(&p);
            assert_eq!((tw, th), (fw, fh));
            assert_eq!(thumb.len(), full.len());
            let worst = thumb.iter().zip(&full).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            assert!(worst <= 2, "{name}.{ext}: largest difference {worst} code values");
            // The conversion is not a no-op: the tagged codes differ clearly from the sRGB result.
            let moved = thumb.as_chunks::<4>().0.iter().zip(codes.as_chunks::<3>().0).map(|(a, b)| (0..3).map(|k| a[k].abs_diff(b[k])).max().unwrap()).max().unwrap();
            assert!(moved >= 8, "{name}.{ext}: thumbnail equals the tagged codes ({moved})");
            checked += 1;
        }
    }
    assert_eq!(checked, 9);
}

#[test]
fn thumbnails_of_srgb_and_untagged_files_are_untouched() {
    use rawgraph_core::color::OutputSpace;
    let t = Tmp::new("thumb-srgb");
    let (w, h) = (32u32, 32u32);
    let codes = tagged_codes(w, h);
    let tagged = t.file("srgb.png");
    save_srgb8_with(&tagged, w, h, &codes, &SaveOptions { space: OutputSpace::Srgb, embed_profile: true, ..Default::default() }).unwrap();
    let plain = t.file("plain.png");
    save_srgb8_with(&plain, w, h, &codes, &SaveOptions { embed_profile: false, ..Default::default() }).unwrap();
    for p in [&tagged, &plain] {
        let (_, _, px) = load_thumbnail(p, 256).unwrap();
        for (a, b) in px.as_chunks::<4>().0.iter().zip(codes.as_chunks::<3>().0) {
            assert_eq!(&a[..3], b);
        }
    }
}

#[test]
#[ignore = "diagnostic timing, run explicitly with --ignored --nocapture"]
fn thumbnail_profile_conversion_cost() {
    use rawgraph_core::color::OutputSpace;
    let t = Tmp::new("thumb-cost");
    let (w, h) = (1800u32, 1200u32);
    let codes = tagged_codes(w, h);
    for (space, name) in [(OutputSpace::Srgb, "srgb"), (OutputSpace::AdobeRgb, "adobe")] {
        let p = t.file(&format!("{name}.jpg"));
        save_srgb8_with(&p, w, h, &codes, &SaveOptions { space, jpeg_quality: 90, ..Default::default() }).unwrap();
        let mut best = f64::MAX;
        for _ in 0..8 {
            let s = std::time::Instant::now();
            let _ = load_thumbnail(&p, 256).unwrap();
            best = best.min(s.elapsed().as_secs_f64() * 1000.0);
        }
        println!("{name}: {best:.2} ms per 256 px thumbnail from a {w}x{h} JPEG");
    }
}
