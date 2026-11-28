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

// ---------------------------------------------------------------- independent decoder (ffmpeg)

fn ffmpeg_decode(bytes: &[u8], name: &str) -> Option<(u32, u32, Vec<u8>)> {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("rawgraph-heic-ff-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("in.heic");
    std::fs::write(&src, bytes).ok()?;
    let probe = Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0:s=,"]).arg(&src).output().ok()?;
    let dims = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    let (w, h) = dims.trim_end_matches(',').split_once(',').and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))?;
    let out = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&src).args(["-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "-"]).output().ok()?;
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.success() && out.stdout.len() == (w * h * 3) as usize).then_some((w, h, out.stdout))
}

/// Size of the tile grid (stream group) that ffprobe reports. ffmpeg itself does not stitch grids.
fn ffprobe_grid(bytes: &[u8]) -> Option<(u32, u32)> {
    let dir = std::env::temp_dir().join(format!("rawgraph-heic-grid-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("in.heic");
    std::fs::write(&src, bytes).ok()?;
    let out = std::process::Command::new("ffprobe").args(["-v", "error", "-show_stream_groups"]).arg(&src).output().ok()?;
    let _ = std::fs::remove_dir_all(&dir);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let (mut in_group, mut in_stream, mut w, mut h) = (false, false, None, None);
    for line in text.lines() {
        match line.trim() {
            "[STREAM_GROUP]" => in_group = true,
            "[/STREAM_GROUP]" => in_group = false,
            "[STREAM]" => in_stream = true,
            "[/STREAM]" => in_stream = false,
            l if in_group && !in_stream => {
                if let Some(v) = l.strip_prefix("width=") {
                    w = w.or(v.parse().ok());
                } else if let Some(v) = l.strip_prefix("height=") {
                    h = h.or(v.parse().ok());
                }
            }
            _ => {}
        }
    }
    Some((w?, h?))
}

fn chart(w: u32, h: u32) -> Vec<u8> {
    let cols: [[u8; 3]; 8] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 255], [0, 0, 0], [128, 128, 128], [255, 255, 0], [200, 120, 40]];
    let mut v = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let c = cols[(x * 8 / w) as usize];
            let s = [255u32, 192, 128, 64][(y * 4 / h) as usize];
            v.extend(c.map(|k| (u32::from(k) * s / 255) as u8));
        }
    }
    v
}

/// Needs ffmpeg and ffprobe on the PATH and the system HEIC encoder:
/// `cargo test -p rawgraph-io --test heic -- --ignored --nocapture`
#[test]
#[ignore = "needs ffmpeg on the PATH"]
fn heic_colours_match_an_independent_decoder() {
    if heic_available().is_err() || std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skipped: no HEIC encoder or no ffmpeg");
        return;
    }
    let (w, h) = (256u32, 128u32);
    let rgb = chart(w, h);
    let mut worst_all = 0u8;
    for space in [OutputSpace::Srgb, OutputSpace::DisplayP3] {
        let bytes = encode_heic8(w, h, &rgb, &SaveOptions { jpeg_quality: 100, space, ..Default::default() }).unwrap();
        if let Ok(dir) = std::env::var("HEIC_KEEP") {
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("chart-{space:?}.heic")), &bytes);
        }
        let (fw, fh, ff) = ffmpeg_decode(&bytes, "chart").expect("ffmpeg decodes the file");
        assert_eq!((fw, fh), (w, h));
        let mut worst = 0u8;
        for by in 0..4 {
            for bx in 0..8 {
                let i = (((by * 32 + 16) * w + bx * 32 + 16) * 3) as usize;
                let d = (0..3).map(|k| ff[i + k].abs_diff(rgb[i + k])).max().unwrap();
                worst = worst.max(d);
            }
        }
        eprintln!("{space:?}: worst patch difference to the source {worst} code values (ffmpeg)");
        assert!(worst <= 2, "{space:?}: {worst}");
        worst_all = worst_all.max(worst);
    }
    assert!(worst_all <= 2);
}

/// Tile-grid sizes with a side of 512*n + 1 px are written by WIC but cannot be read back by Windows:
/// the export refuses them with a clear message instead of writing an unreadable file.
#[test]
fn heic_sizes_windows_cannot_read_back_are_refused_clearly() {
    if heic_available().is_err() {
        return;
    }
    for (w, h) in [(4097u32, 600u32), (3000, 1025)] {
        let rgb = vec![90u8; (w * h * 3) as usize];
        match encode_heic8(w, h, &rgb, &SaveOptions::default()) {
            Err(e) => assert!(e.to_string().contains("cannot read back"), "{w}x{h}: {e}"),
            Ok(bytes) => assert!(decode_heic8(&bytes).is_ok(), "{w}x{h}: written but unreadable"),
        }
    }
}

/// Sizes that the Windows encoder rejects (tile remainders) work through the padded canvas and
/// decode to the real size in WIC and in ffmpeg.
#[test]
fn heic_odd_sizes_encode_and_report_their_real_size() {
    if heic_available().is_err() {
        return;
    }
    let sizes: [(u32, u32); 10] = [(4104, 600), (4098, 700), (4610, 600), (600, 4106), (4200, 3080), (6152, 4104), (8196, 800), (16392, 600), (8200, 8200), (1000, 666)];
    let ff = std::process::Command::new("ffmpeg").arg("-version").output().is_ok();
    for (w, h) in sizes {
        let rgb = vec![90u8; (w * h * 3) as usize];
        let bytes = encode_heic8(w, h, &rgb, &SaveOptions::default()).unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
        if let Ok(dir) = std::env::var("HEIC_KEEP") {
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("size-{w}x{h}.heic")), &bytes);
        }
        if u64::from(w) * u64::from(h) < 30_000_000 {
            let img = decode_heic8(&bytes).unwrap_or_else(|e| panic!("{w}x{h}: {e}"));
            assert_eq!((img.width, img.height), (w, h), "WIC reports the real size");
            assert!(img.rgb.iter().all(|v| v.abs_diff(90) <= 3), "{w}x{h}: flat grey stays flat");
            if ff && w.max(h) > 4096 {
                assert_eq!(ffprobe_grid(&bytes), Some((w, h)), "{w}x{h}: ffprobe reports the real size of the tile grid");
            } else if ff {
                let (fw, fh, px) = ffmpeg_decode(&bytes, "size").unwrap_or_else(|| panic!("{w}x{h}: ffmpeg cannot decode"));
                assert_eq!((fw, fh), (w, h), "ffmpeg reports the real size");
                assert!(px.iter().all(|v| v.abs_diff(90) <= 3));
            }
        }
    }
}

#[test]
fn tiny_pictures_are_refused_before_the_system_encoder_sees_them() {
    for (w, h) in [(1u32, 100u32), (1, 99), (100, 1), (7, 64), (15, 15), (1, 128)] {
        let rgb = vec![50u8; (w * h * 3) as usize];
        match encode_heic8(w, h, &rgb, &SaveOptions::default()) {
            Err(IoError::Unsupported(m)) => assert!(m.contains("at least 16"), "{m}"),
            other => panic!("{w}x{h}: {other:?}"),
        }
    }
    if heic_available().is_ok() {
        let rgb = vec![50u8; 16 * 16 * 3];
        assert!(encode_heic8(16, 16, &rgb, &SaveOptions::default()).is_ok());
    }
}

#[cfg(windows)]
#[test]
fn heic_encodes_from_sta_threads_and_in_parallel() {
    if heic_available().is_err() {
        return;
    }
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    let rgb = chart(64, 64);
    let sta = std::thread::spawn({
        let rgb = rgb.clone();
        move || {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap() };
            let r = encode_heic8(64, 64, &rgb, &SaveOptions::default()).map(|b| b.len());
            unsafe { CoUninitialize() };
            r
        }
    });
    let workers: Vec<_> = (0..6).map(|i| std::thread::spawn({ let rgb = rgb.clone(); move || (0..3).all(|_| encode_heic8(64, 64, &rgb, &SaveOptions { jpeg_quality: 50 + i, ..Default::default() }).is_ok()) })).collect();
    assert!(sta.join().unwrap().unwrap() > 100);
    assert!(workers.into_iter().all(|h| h.join().unwrap()));
}

#[test]
fn heic_exif_values_and_text_read_back() {
    if heic_available().is_err() {
        return;
    }
    let rgb = chart(64, 64);
    let md = ExportMetadata {
        make: Some("SONY".into()), model: Some("ILCE-7M4".into()), artist: Some("J\u{f6}rg M\u{fc}ller".into()), copyright: Some("\u{a9} 2026".into()),
        lens: Some("FE 24-70mm".into()), datetime_original: Some("2026:11:28 10:00:00".into()), iso: Some(400), exposure_time: Some((1, 250)),
        f_number: Some((28, 10)), focal_length: Some((35, 1)), ..Default::default()
    };
    let done = encode_heic8_detailed(64, 64, &rgb, &SaveOptions { metadata: Some(md), ..Default::default() }).unwrap();
    eprintln!("EXIF stored: {} of {} entries", done.exif_written, done.exif_requested);
    assert_eq!(done.exif_written, done.exif_requested, "every requested EXIF entry is accepted");
    let img = decode_heic8(&done.bytes).unwrap();
    assert_eq!(img.make.as_deref(), Some("SONY"));
    // The EXIF payload is in the file: find it by scanning for its values.
    let has = |needle: &[u8]| done.bytes.windows(needle.len()).any(|w| w == needle);
    assert!(has(b"ILCE-7M4") && has(b"FE 24-70mm") && has(b"2026:11:28 10:00:00"));
    assert!(has("J\u{f6}rg M\u{fc}ller".as_bytes()), "non-ASCII text is stored as UTF-8");
}
