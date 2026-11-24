//! Tests on real photos. They only run where the local sample folder exists, so the suite stays
//! green on other machines. The samples are private client photos: they are never copied,
//! uploaded or committed, and these tests print only aggregate numbers.

use std::path::PathBuf;

use rawgraph_io::*;

/// `RAWGRAPH_SAMPLES`, else `%LOCALAPPDATA%\rawgraph\samples` (elsewhere `$HOME/.local/share/rawgraph/samples`).
fn samples_dir() -> PathBuf {
    let var = |n: &str| std::env::var_os(n).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("RAWGRAPH_SAMPLES").unwrap_or_else(|| {
        let base = var("LOCALAPPDATA")
            .map(|d| d.join("rawgraph"))
            .or_else(|| var("HOME").map(|h| h.join(".local").join("share").join("rawgraph")))
            .unwrap_or_else(|| PathBuf::from("rawgraph"));
        base.join("samples")
    })
}

fn sample_files() -> Option<Vec<PathBuf>> {
    let dir = samples_dir();
    if !dir.is_dir() {
        println!("skipped: no sample folder (set RAWGRAPH_SAMPLES)");
        return None;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && is_supported(p))
        .collect();
    files.sort();
    if files.is_empty() {
        println!("skipped: no sample folder (set RAWGRAPH_SAMPLES)");
        return None;
    }
    Some(files)
}

fn check_plausible_capture(c: &rawgraph_core::CaptureInfo, kind: SourceKind, name: &str) {
    if kind == SourceKind::Rendered {
        return; // exported JPEGs may have had their EXIF stripped
    }
    assert!(c.make.is_some() && c.model.is_some(), "{name}: make/model missing");
    let iso = c.iso.unwrap_or_else(|| panic!("{name}: ISO missing"));
    assert!((50.0..=409_600.0).contains(&iso), "{name}: implausible ISO {iso}");
    let t = c.shutter_s.unwrap_or_else(|| panic!("{name}: shutter missing"));
    assert!((1.0 / 32000.0..=120.0).contains(&t), "{name}: implausible shutter {t}");
    let n = c.aperture.unwrap_or_else(|| panic!("{name}: aperture missing"));
    assert!((0.95..=64.0).contains(&n), "{name}: implausible aperture {n}");
    let ev = c.ev100().unwrap_or_else(|| panic!("{name}: EV100 missing"));
    assert!((-6.0..=22.0).contains(&ev), "{name}: implausible EV100 {ev}");
}

#[test]
fn every_sample_decodes_with_plausible_pixels_and_metadata() {
    let Some(files) = sample_files() else { return };
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let l = load(&path, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (w, h) = (l.image.width(), l.image.height());
        assert_eq!((w, h), l.full_size, "{name}: full-res image must match full_size");
        assert!(w.min(h) >= 1500 && w.max(h) >= 2500, "{name}: implausible size {w}x{h}");
        assert_eq!(Some(l.kind), source_kind(&path));
        check_plausible_capture(&l.capture, l.kind, &name);

        let mut sum = 0.0f64;
        let mut max = 0.0f32;
        let mut n = 0u64;
        for px in l.image.data().as_chunks::<4>().0.iter() {
            assert!(px.iter().all(|v| v.is_finite()), "{name}: NaN or infinity in pixels");
            assert_eq!(px[3], 1.0, "{name}: alpha must be opaque");
            sum += ((px[0] + px[1] + px[2]) / 3.0) as f64;
            max = max.max(px[0]).max(px[1]).max(px[2]);
            n += 1;
        }
        let mean = sum / n as f64;
        assert!(max > 0.1, "{name}: image is (nearly) black, max {max}");
        assert!((0.003..0.8).contains(&mean), "{name}: implausible mean {mean}");

        match l.kind {
            SourceKind::Raw => {
                assert_eq!(l.capture.raw, Some(true), "{name}");
                let k = l.capture.as_shot_kelvin.unwrap_or_else(|| panic!("{name}: as-shot kelvin missing"));
                assert!((2000.0..=12000.0).contains(&k), "{name}: implausible as-shot kelvin {k}");
                assert!(l.capture.as_shot_tint.is_some_and(f32::is_finite), "{name}: tint missing");
            }
            SourceKind::Rendered => {
                assert_eq!(l.capture.raw, Some(false), "{name}");
                assert_eq!((l.capture.as_shot_kelvin, l.capture.baseline_exposure), (None, None), "{name}");
            }
        }

        // Metadata path agrees with the full decode and is much cheaper.
        let info = read_capture_info(&path).unwrap();
        assert_eq!(info, l.capture, "{name}: read_capture_info != load().capture");

        // Proxy: right size, same orientation, similar brightness.
        let p = load(&path, Some(1024)).unwrap();
        assert_eq!(p.full_size, l.full_size, "{name}");
        assert!(p.image.width().max(p.image.height()) <= 1024, "{name}");
        assert_eq!(p.image.width() > p.image.height(), w > h, "{name}: proxy orientation differs");
        let pm: f64 = p.image.data().as_chunks::<4>().0.iter().map(|c| ((c[0] + c[1] + c[2]) / 3.0) as f64).sum::<f64>()
            / (p.image.width() as f64 * p.image.height() as f64);
        assert!((pm - mean).abs() < 0.02 + mean * 0.1, "{name}: proxy mean {pm} vs full mean {mean}");

        // Thumbnail: upright, same aspect class as the decoded image.
        let (tw, th, rgba) = load_thumbnail(&path, 320).unwrap();
        assert!(tw.max(th) <= 320 && tw > 0 && th > 0, "{name}");
        assert_eq!(rgba.len(), (tw * th * 4) as usize);
        assert_eq!(tw > th, w > h, "{name}: thumbnail is not upright ({tw}x{th} vs {w}x{h})");
        let ar_img = w as f32 / h as f32;
        let ar_th = tw as f32 / th as f32;
        assert!((ar_img / ar_th - 1.0).abs() < 0.12, "{name}: thumbnail aspect {ar_th} vs image {ar_img}");
        println!("ok: {name} {w}x{h} mean {mean:.4} max {max:.2}");
    }
}

#[test]
fn fast_proxy_matches_reference_proxy_on_samples() {
    use rawgraph_io::raw::{load_with_mode, Mode};
    let Some(files) = sample_files() else { return };
    for path in files.into_iter().filter(|p| source_kind(p) == Some(SourceKind::Raw)) {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        for max_dim in [1280u32, 2560] {
            let o = LoadOptions { max_dim: Some(max_dim), ..Default::default() };
            let (a, _) = load_with_mode(&path, &o, Mode::Auto).unwrap();
            let (b, _) = load_with_mode(&path, &o, Mode::FullDemosaic).unwrap();
            assert_eq!((a.image.width(), a.image.height()), (b.image.width(), b.image.height()), "{name} {max_dim}");
            assert_eq!(a.full_size, b.full_size, "{name}");
            let (mut sum, mut mean) = (0.0f64, 0.0f64);
            for (x, y) in a.image.data().as_chunks::<4>().0.iter().zip(b.image.data().as_chunks::<4>().0) {
                for c in 0..3 {
                    sum += (x[c] - y[c]).abs() as f64;
                    mean += y[c] as f64;
                }
            }
            let n = (a.image.width() * a.image.height() * 3) as f64;
            let (d, m) = (sum / n, mean / n);
            assert!(d < 0.005 + 0.05 * m, "{name} {max_dim}: mean |diff| {d} vs mean {m}");
        }
    }
}

#[test]
fn highlight_neutralisation_off_changes_only_bright_pixels_on_samples() {
    let Some(files) = sample_files() else { return };
    let mut any_change = false;
    for path in files.into_iter().filter(|p| source_kind(p) == Some(SourceKind::Raw)) {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let on = load_with(&path, &LoadOptions { max_dim: Some(3600), neutralize_clipped_highlights: true }).unwrap().image;
        let off = load_with(&path, &LoadOptions { max_dim: Some(3600), neutralize_clipped_highlights: false }).unwrap().image;
        assert_eq!((on.width(), on.height()), (off.width(), off.height()));
        for (a, b) in on.data().as_chunks::<4>().0.iter().zip(off.data().as_chunks::<4>().0) {
            let differs = (0..3).any(|c| (a[c] - b[c]).abs() > 1e-6);
            if differs {
                any_change = true;
                // Only pixels with a (nearly) clipped channel may change: those are bright.
                assert!(b[0].max(b[1]).max(b[2]) > 0.1, "{name}: a dark pixel changed: {b:?} -> {a:?}");
            }
        }
    }
    assert!(any_change, "no sample contains clipped highlights?");
}

#[test]
fn export_metadata_and_xmp_of_samples() {
    let Some(files) = sample_files() else { return };
    let (mut with_xmp, mut with_rating) = (0, 0);
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let m = read_export_metadata(path);
        if source_kind(path) == Some(SourceKind::Raw) {
            assert!(m.make.is_some() && m.model.is_some(), "{name}: make/model missing");
            assert!(m.iso.is_some() && m.exposure_time.is_some() && m.f_number.is_some(), "{name}: exposure data missing");
            assert!(m.focal_length.is_some(), "{name}: focal length missing");
            let dt = m.datetime_original.as_deref().unwrap_or("");
            assert!(dt.len() == 19 && dt.as_bytes()[4] == b':', "{name}: bad date {dt:?}");
        }
        let xmp = read_xmp_packet(path);
        let rating = xmp.as_deref().is_some_and(|x| x.contains("xmp:Rating"));
        println!("{name}: xmp packet {} bytes, xmp:Rating {rating}", xmp.as_ref().map_or(0, |x| x.len()));
        with_xmp += xmp.is_some() as usize;
        with_rating += rating as usize;
    }
    println!("{} of {} samples carry an XMP packet, {with_rating} with xmp:Rating", with_xmp, files.len());
}
