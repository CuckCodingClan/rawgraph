//! Prints a table of what `rawgraph-io` makes of every supported file in a folder and writes an
//! sRGB PNG preview (long edge <= 1600) per file to `<samples>\_preview\<name>.png`.
//!
//! Usage: `cargo run --release -p rawgraph-io --example inspect -- <folder>`

use std::path::{Path, PathBuf};
use std::time::Instant;

use rawgraph_core::color;
use rawgraph_io::{is_supported, load, load_thumbnail, read_capture_info, save_srgb8, source_kind, SourceKind};

fn fmt_opt(v: Option<f32>, prec: usize) -> String {
    v.map_or("-".to_string(), |v| format!("{v:.prec$}"))
}

fn shutter(v: Option<f32>) -> String {
    match v {
        Some(t) if t > 0.0 && t < 1.0 => format!("1/{:.0}", 1.0 / t),
        Some(t) => format!("{t:.1}s"),
        None => "-".into(),
    }
}

fn main() {
    let Some(dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: inspect <folder>");
        std::process::exit(2);
    };
    let preview_dir = dir.join("_preview");
    if let Err(e) = std::fs::create_dir_all(&preview_dir) {
        eprintln!("cannot create {}: {e}", preview_dir.display());
        std::process::exit(1);
    }
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file() && is_supported(p)).collect(),
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            std::process::exit(1);
        }
    };
    files.sort();

    let m = color::to_f32(&color::acescg_to_rec709());
    for path in files {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        println!("== {name}");
        let t = Instant::now();
        let info = read_capture_info(&path);
        let t_info = t.elapsed();
        let t = Instant::now();
        let full = load(&path, None);
        let t_full = t.elapsed();
        let t = Instant::now();
        let proxy = load(&path, Some(2560));
        let t_proxy = t.elapsed();

        let loaded = match full {
            Ok(l) => l,
            Err(e) => {
                println!("   FAILED full decode: {e}");
                continue;
            }
        };
        let kind = match source_kind(&path) {
            Some(SourceKind::Raw) => "RAW",
            Some(SourceKind::Rendered) => "rendered",
            None => "?",
        };
        let c = &loaded.capture;
        let img = &loaded.image;
        let n = img.data().as_chunks::<4>().0.len() as f64;
        let mut sum = [0.0f64; 3];
        let mut max = [f32::MIN; 3];
        let mut over = 0u64;
        let mut nan = 0u64;
        for px in img.data().as_chunks::<4>().0.iter() {
            let mut o = false;
            for k in 0..3 {
                if !px[k].is_finite() {
                    nan += 1;
                    continue;
                }
                sum[k] += px[k] as f64;
                max[k] = max[k].max(px[k]);
                o |= px[k] > 1.0;
            }
            over += o as u64;
        }
        println!(
            "   kind {kind} | size {}x{} (image {}x{}) | {} {} | lens {}",
            loaded.full_size.0,
            loaded.full_size.1,
            img.width(),
            img.height(),
            c.make.as_deref().unwrap_or("-"),
            c.model.as_deref().unwrap_or("-"),
            c.lens.as_deref().unwrap_or("-"),
        );
        println!(
            "   ISO {} | shutter {} | f/{} | flash {} | EV100 {}",
            fmt_opt(c.iso, 0),
            shutter(c.shutter_s),
            fmt_opt(c.aperture, 1),
            c.flash_fired.map_or("-".to_string(), |f| f.to_string()),
            fmt_opt(c.ev100(), 2),
        );
        println!(
            "   mean ACEScg [{:.4} {:.4} {:.4}] | max [{:.3} {:.3} {:.3}] | >1.0: {:.3}% | non-finite {}",
            sum[0] / n,
            sum[1] / n,
            sum[2] / n,
            max[0],
            max[1],
            max[2],
            over as f64 / n * 100.0,
            nan
        );
        println!(
            "   raw {} | as-shot {} K | tint {} | baseline exposure {} EV",
            c.raw.map_or("-".to_string(), |r| r.to_string()),
            fmt_opt(c.as_shot_kelvin, 0),
            fmt_opt(c.as_shot_tint, 1),
            fmt_opt(c.baseline_exposure, 2),
        );
        let info_equal = info.as_ref().map(|i| i == c).unwrap_or(false);
        println!(
            "   time: full {:.0} ms | max_dim 2560 {:.0} ms ({}) | read_capture_info {:.1} ms (equals load: {})",
            t_full.as_secs_f64() * 1000.0,
            t_proxy.as_secs_f64() * 1000.0,
            proxy.as_ref().map_or("failed".to_string(), |p| format!("{}x{}", p.image.width(), p.image.height())),
            t_info.as_secs_f64() * 1000.0,
            info_equal
        );

        let t = Instant::now();
        match load_thumbnail(&path, 512) {
            Ok((w, h, _)) => println!("   thumbnail(512) {}x{} in {:.0} ms", w, h, t.elapsed().as_secs_f64() * 1000.0),
            Err(e) => println!("   thumbnail FAILED: {e}"),
        }

        // Preview: proxy to <= 1600, ACEScg -> Rec.709 -> clamp -> sRGB OETF.
        match load(&path, Some(1600)) {
            Ok(p) => {
                let (w, h) = (p.image.width(), p.image.height());
                let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
                for px in p.image.data().as_chunks::<4>().0.iter() {
                    for row in &m {
                        let lin = row[0] * px[0] + row[1] * px[1] + row[2] * px[2];
                        rgb.push((color::srgb_oetf(lin).clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
                    }
                }
                let stem = Path::new(&name).file_name().and_then(|n| n.to_str()).unwrap_or("x");
                let out = preview_dir.join(format!("{stem}.png"));
                match save_srgb8(&out, w, h, &rgb, 90, true) {
                    Ok(()) => println!("   preview {}x{} -> {}", w, h, out.display()),
                    Err(e) => println!("   preview FAILED: {e}"),
                }
            }
            Err(e) => println!("   preview decode FAILED: {e}"),
        }
    }
}
