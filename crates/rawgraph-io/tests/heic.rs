//! HEIC through the operating system's encoder. On machines without the HEVC extension the error
//! path is tested instead of the round trip.

use rawgraph_core::color::OutputSpace;
use rawgraph_io::*;

fn gradient(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, 120]);
        }
    }
    v
}

fn mean(rgb: &[u8]) -> [f64; 3] {
    let mut m = [0.0; 3];
    for p in rgb.as_chunks::<3>().0 {
        for k in 0..3 {
            m[k] += f64::from(p[k]);
        }
    }
    m.map(|s| s / (rgb.len() / 3) as f64)
}

#[test]
fn heic_round_trip_or_clear_error() {
    let (w, h) = (96u32, 64u32);
    let rgb = gradient(w, h);
    let dir = std::env::temp_dir().join(format!("rawgraph-heic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.heic");
    let md = ExportMetadata { make: Some("SONY".into()), model: Some("ILCE-7M4".into()), iso: Some(400), exposure_time: Some((1, 250)), ..Default::default() };
    let options = SaveOptions { jpeg_quality: 90, overwrite: false, embed_profile: true, space: OutputSpace::DisplayP3, metadata: Some(md) };
    match heic_available() {
        Ok(()) => {
            save_heic8(&path, w, h, &rgb, &options).unwrap();
            assert!(matches!(save_heic8(&path, w, h, &rgb, &options), Err(IoError::Exists(_))));
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(&bytes[4..8], b"ftyp");
            assert_eq!(embedded_icc(&bytes).as_deref(), Some(icc_profile(OutputSpace::DisplayP3)), "the Display P3 profile is stored byte for byte");
            let img = decode_heic8(&bytes).unwrap();
            assert_eq!((img.width, img.height), (w, h));
            let (a, b) = (mean(&rgb), mean(&img.rgb));
            for k in 0..3 {
                assert!((a[k] - b[k]).abs() < 4.0, "mean {k}: {a:?} vs {b:?}");
            }
            assert_eq!(img.make.as_deref(), Some("SONY"), "EXIF Make read back through WIC");
            assert!(save_heic8(&dir.join("bad.heic"), w, h, &rgb[..6], &options).is_err());
            // Quality reaches the encoder: a noisy picture is much smaller at quality 10 than at 95.
            let noisy: Vec<u8> = (0..(w * h * 3)).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
            let size = |q: u8| encode_heic8(w, h, &noisy, &SaveOptions { jpeg_quality: q, ..options.clone() }).unwrap().len();
            assert!(size(10) < size(95), "{} vs {}", size(10), size(95));
            // Without the switch no profile is stored.
            let plain = encode_heic8(w, h, &rgb, &SaveOptions { embed_profile: false, ..options.clone() }).unwrap();
            assert_eq!(embedded_icc(&plain), None);
        }
        Err(why) => {
            eprintln!("HEIC not available here: {why}");
            assert!(!why.is_empty());
            assert!(matches!(save_heic8(&path, w, h, &rgb, &options), Err(IoError::Unsupported(_))));
            assert!(!path.exists());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
