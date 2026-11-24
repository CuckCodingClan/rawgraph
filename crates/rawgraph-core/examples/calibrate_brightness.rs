#![allow(clippy::chunks_exact_to_as_chunks)]
//! Calibrates the default of the Develop node's `brightness` parameter.
//!
//! For every RAW file in a folder (default: the local samples folder) it
//! compares the embedded preview (the camera's or Adobe's own rendering) with
//! our decode after baseline gain and the ACES-fit display transform, and
//! solves for the extra EV at which the median / mean display luminance match.
//!
//! Run: cargo run --release -p rawgraph-core --example calibrate_brightness [folder]

use rawgraph_core::color::{srgb_eotf, REC709_LUMA};
use rawgraph_core::ImageBuf;
use rawgraph_io::SourceKind;
use rawgraph_scopes::{DisplayTransform, ToneMap};

fn stats(mut y: Vec<f32>) -> (f32, f32) {
    let mean = y.iter().map(|v| f64::from(*v)).sum::<f64>() as f32 / y.len().max(1) as f32;
    y.sort_by(|a, b| a.total_cmp(b));
    (y[y.len() / 2], mean)
}

fn luma_linear(rgb: impl Iterator<Item = [f32; 3]>) -> Vec<f32> {
    rgb.map(|p| {
        let l = [srgb_eotf(p[0]), srgb_eotf(p[1]), srgb_eotf(p[2])];
        REC709_LUMA[0] * l[0] + REC709_LUMA[1] * l[1] + REC709_LUMA[2] * l[2]
    })
    .collect()
}

fn ours(img: &ImageBuf, ev: f32) -> (f32, f32) {
    let mut scaled = img.clone();
    let g = 2f32.powf(ev);
    for px in scaled.data_mut().chunks_exact_mut(4) {
        for v in &mut px[..3] {
            *v *= g;
        }
    }
    let d = DisplayTransform { exposure_ev: 0.0, tone_map: ToneMap::AcesFit }.apply(&scaled);
    stats(luma_linear(d.rgb.chunks_exact(3).map(|c| [c[0], c[1], c[2]])))
}

/// Smallest-error x in -2..4 with f(x) == target (f monotonic increasing).
fn solve(target: f32, f: impl Fn(f32) -> f32) -> f32 {
    let (mut lo, mut hi) = (-2.0f32, 4.0f32);
    for _ in 0..22 {
        let mid = 0.5 * (lo + hi);
        if f(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

fn quantile(sorted: &[f32], q: f32) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let pos = q * (sorted.len() - 1) as f32;
    let (i, frac) = (pos.floor() as usize, pos.fract());
    let j = (i + 1).min(sorted.len() - 1);
    sorted[i] * (1.0 - frac) + sorted[j] * frac
}

mod common;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| common::samples_dir().to_string_lossy().into_owned());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("samples folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && rawgraph_io::source_kind(p) == Some(SourceKind::Raw))
        .collect();
    files.sort();
    println!("{:<16} {:<22} {:>6} {:>9} {:>8} {:>8} {:>8} {:>8}", "file", "camera", "ISO", "baseline", "x(med)", "x(mean)", "refMed", "refMean");
    let (mut xm, mut xa) = (Vec::new(), Vec::new());
    for path in files {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let (Ok((w, h, rgba)), Ok(loaded)) = (rawgraph_io::load_thumbnail(&path, 512), rawgraph_io::load(&path, Some(512))) else {
            println!("{name:<16} (could not be loaded)");
            continue;
        };
        let _ = (w, h);
        let reference = stats(luma_linear(rgba.chunks_exact(4).map(|c| {
            [f32::from(c[0]) / 255.0, f32::from(c[1]) / 255.0, f32::from(c[2]) / 255.0]
        })));
        let baseline = loaded.capture.baseline_exposure.unwrap_or(0.35);
        let x_med = solve(reference.0, |x| ours(&loaded.image, baseline + x).0);
        let x_mean = solve(reference.1, |x| ours(&loaded.image, baseline + x).1);
        let cam = loaded.capture.model.clone().unwrap_or_default();
        let iso = loaded.capture.iso.map_or("-".to_owned(), |v| format!("{v:.0}"));
        println!("{name:<16} {cam:<22} {iso:>6} {baseline:>9.2} {x_med:>8.2} {x_mean:>8.2} {:>8.3} {:>8.3}", reference.0, reference.1);
        xm.push(x_med);
        xa.push(x_mean);
    }
    for (label, v) in [("median-matching", &mut xm), ("mean-matching", &mut xa)] {
        v.sort_by(|a, b| a.total_cmp(b));
        println!(
            "x {label}: n={} q1={:.2} median={:.2} q3={:.2} IQR={:.2} min={:.2} max={:.2}",
            v.len(),
            quantile(v, 0.25),
            quantile(v, 0.5),
            quantile(v, 0.75),
            quantile(v, 0.75) - quantile(v, 0.25),
            v.first().copied().unwrap_or(f32::NAN),
            v.last().copied().unwrap_or(f32::NAN)
        );
    }
}
