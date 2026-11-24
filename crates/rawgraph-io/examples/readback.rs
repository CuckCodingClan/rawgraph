//! Prints what an exported file carries: colour type, ICC profile size, EXIF fields.
//!
//! readback <file>...   (artist / copyright are reported as present, not printed)

use image::{ImageDecoder, ImageReader};

fn main() {
    for arg in std::env::args().skip(1) {
        let path = std::path::Path::new(&arg);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let icc = if matches!(path.extension().and_then(|e| e.to_str()), Some("tif" | "tiff")) {
            std::fs::File::open(path)
                .ok()
                .and_then(|f| image::codecs::tiff::TiffDecoder::new(std::io::BufReader::new(f)).ok())
                .and_then(|mut t| t.icc_profile().ok().flatten())
        } else {
            ImageReader::open(path).ok().and_then(|r| r.with_guessed_format().ok()).and_then(|r| r.into_decoder().ok()).and_then(|mut d| d.icc_profile().ok().flatten())
        };
        let color = image::open(path).map(|i| format!("{:?} {}x{}", i.color(), i.width(), i.height())).unwrap_or_else(|e| format!("unreadable: {e}"));
        println!("{name}: {color}");
        println!(
            "  ICC: {} (matches srgb_icc_profile(): {})",
            icc.as_ref().map_or("none".to_string(), |p| format!("{} bytes", p.len())),
            icc.as_deref() == Some(rawgraph_io::srgb_icc_profile())
        );
        let m = rawgraph_io::read_export_metadata(path);
        println!(
            "  EXIF via read_export_metadata: make {:?} model {:?} lens {:?} iso {:?} exposure {:?} f {:?} focal {:?} date {:?} artist {} copyright {}",
            m.make, m.model, m.lens, m.iso, m.exposure_time, m.f_number, m.focal_length, m.datetime_original, m.artist.is_some(), m.copyright.is_some()
        );
        if let Ok(f) = std::fs::File::open(path) {
            if let Ok(ex) = exif::Reader::new().read_from_container(&mut std::io::BufReader::new(f)) {
                for tag in [exif::Tag::Orientation, exif::Tag::Software, exif::Tag::ColorSpace, exif::Tag::ExifVersion] {
                    if let Some(fld) = ex.get_field(tag, exif::In::PRIMARY) {
                        println!("  {}: {}", tag, fld.display_value());
                    }
                }
            }
        }
    }
}
