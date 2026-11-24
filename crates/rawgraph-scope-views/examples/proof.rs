//! Proof pictures of the Colorize look: writes the PREPARED scope pixel buffers (the ones the
//! widgets upload as textures; the histogram is rasterised by `render::histogram_to_rgba`, the
//! software twin of the widget's mesh drawing) for one input picture, once with Colorize on and
//! once off (`-mono`).
//!
//! `cargo run --release -p rawgraph-scope-views --example proof [picture.png] [out-dir]`
//!
//! Without a picture the first PNG of `%LOCALAPPDATA%\rawgraph\samples\_preview` is read (read
//! only, never copied), else a generated colour-bar + gradient pattern is used.
//! Without an out-dir the pictures go to `%LOCALAPPDATA%\rawgraph\shots`.

use rawgraph_scope_views::render::histogram_to_rgba;
use rawgraph_scope_views::{
    prepare, PrepareSettings, PreparedScopes, PreparedTrace, WaveformMode, HISTOGRAM_BINS,
};
use rawgraph_scopes::{analyze, patterns, DisplayImage};
use std::path::{Path, PathBuf};

fn local_app_data() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rawgraph")
}

/// A generated picture with saturated colours: colour bars over a hue sweep and a grey ramp.
fn generated(width: u32, height: u32) -> DisplayImage {
    let bars = patterns::color_bars(width, height / 3, 0.9);
    let mut rgb = bars.rgb;
    for y in height / 3..height {
        for x in 0..width {
            let u = x as f32 / width as f32;
            let v = (y - height / 3) as f32 / (height - height / 3) as f32;
            if v < 0.5 {
                // Hue sweep at three quarters brightness.
                let h = u * 6.0;
                let (i, f) = (h.floor() as i32, h - h.floor());
                let (p, q, t) = (0.0, 1.0 - f, f);
                let c = match i % 6 {
                    0 => [1.0, t, p],
                    1 => [q, 1.0, p],
                    2 => [p, 1.0, t],
                    3 => [p, q, 1.0],
                    4 => [t, p, 1.0],
                    _ => [1.0, p, q],
                };
                rgb.extend(c.map(|c| c * 0.75));
            } else {
                rgb.extend([u; 3]);
            }
        }
    }
    DisplayImage { width, height, rgb }
}

fn load(path: &Path) -> Option<DisplayImage> {
    let img = image::open(path).ok()?.to_rgb8();
    let (w, h) = img.dimensions();
    let rgb = img.as_raw().iter().map(|&v| f32::from(v) / 255.0).collect();
    Some(DisplayImage { width: w, height: h, rgb })
}

fn first_sample() -> Option<PathBuf> {
    let dir = local_app_data().join("samples").join("_preview");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")))
        .collect();
    files.sort();
    files.into_iter().next()
}

/// Premultiplied RGBA on black -> RGB8 (the colour channels already are the colour on black).
fn rgb8(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect()
}

fn save(dir: &Path, name: &str, w: usize, h: usize, rgba: &[u8]) {
    let path = dir.join(name);
    match image::save_buffer(&path, &rgb8(rgba), w as u32, h as u32, image::ColorType::Rgb8) {
        Ok(()) => println!("wrote {} ({w} x {h})", path.display()),
        Err(e) => eprintln!("cannot write {}: {e}", path.display()),
    }
}

/// Traces side by side with a 2 px grey gap, as the parade shows its columns.
fn hstack(traces: &[PreparedTrace]) -> (usize, usize, Vec<u8>) {
    let h = traces.iter().map(|t| t.height).max().unwrap_or(0);
    let gap = 2;
    let w: usize = traces.iter().map(|t| t.width).sum::<usize>() + gap * traces.len().saturating_sub(1);
    let mut out = vec![0u8; w * h * 4];
    let mut x0 = 0;
    for (k, t) in traces.iter().enumerate() {
        for y in 0..t.height {
            let dst = (y * w + x0) * 4;
            out[dst..dst + t.width * 4].copy_from_slice(&t.rgba[y * t.width * 4..(y + 1) * t.width * 4]);
        }
        x0 += t.width;
        if k + 1 < traces.len() {
            for y in 0..h {
                for g in 0..gap {
                    let i = (y * w + x0 + g) * 4;
                    out[i..i + 4].copy_from_slice(&[70, 70, 76, 76]);
                }
            }
            x0 += gap;
        }
    }
    (w, h, out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let input = args.first().map(PathBuf::from).or_else(first_sample);
    let loaded = input.as_deref().and_then(|p| load(p).map(|i| (p.to_path_buf(), i)));
    let (label, img) = match loaded {
        Some((p, i)) => (format!("picture {} ({} x {})", p.display(), i.width, i.height), i),
        None => ("generated colour bars + hue sweep + grey ramp".to_owned(), generated(1536, 960)),
    };
    println!("input: {label}");
    let out_dir = args.get(1).map(PathBuf::from).unwrap_or_else(|| local_app_data().join("shots"));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("cannot create {}: {e}", out_dir.display());
        return;
    }

    for colourise in [true, false] {
        let suffix = if colourise { "" } else { "-mono" };
        let base = PrepareSettings {
            waveform: false,
            parade: false,
            vectorscope: false,
            histogram: false,
            clipping: false,
            colourise,
            ..Default::default()
        };
        let run = |s: PrepareSettings| -> PreparedScopes { prepare(&analyze(&img, &s.request()), &s) };

        let p = run(PrepareSettings { waveform: true, ..base.clone() });
        if let Some(t) = &p.waveform {
            save(&out_dir, &format!("8-scope-waveform{suffix}.png"), t.width, t.height, &t.rgba);
        }
        let p = run(PrepareSettings { waveform: true, waveform_mode: WaveformMode::Rgb, ..base.clone() });
        if let Some(t) = &p.waveform {
            save(&out_dir, &format!("8-scope-rgb-overlay{suffix}.png"), t.width, t.height, &t.rgba);
        }
        let p = run(PrepareSettings { parade: true, ..base.clone() });
        if let Some(traces) = p.parade_traces() {
            let (w, h, px) = hstack(traces);
            save(&out_dir, &format!("8-scope-parade{suffix}.png"), w, h, &px);
        }
        let p = run(PrepareSettings { vectorscope: true, ..base.clone() });
        if let Some(t) = &p.vectorscope {
            save(&out_dir, &format!("8-scope-vectorscope{suffix}.png"), t.width, t.height, &t.rgba);
        }
        let p = run(PrepareSettings { histogram: true, ..base.clone() });
        if let Some(h) = &p.histogram {
            let (w, hh) = (HISTOGRAM_BINS * 2, 256);
            let px = histogram_to_rgba(&h.r, &h.g, &h.b, &h.luma, w, hh, colourise, false, 1.0);
            save(&out_dir, &format!("8-scope-histogram{suffix}.png"), w, hh, &px);
        }
    }
}
