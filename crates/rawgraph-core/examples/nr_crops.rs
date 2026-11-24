//! Visual check of the noise reduction on a real high-ISO file.
//!
//! `cargo run --release -p rawgraph-core --example nr_crops -- overview` writes a
//! downscaled overview (with a coordinate grid every 1000 px) and
//! `... -- crops [x1 y1 x2 y2 ...]` writes 700x700 100 % crops at the default radius for
//! several luma/chroma settings (defaults: face/eyelashes, flat wall, flower crown, lace). Output stays on this machine.

#![allow(clippy::chunks_exact_to_as_chunks)]

use std::path::{Path, PathBuf};

use rawgraph_core::color::{acescg_to_rec709, srgb_oetf, to_f32};
use rawgraph_core::ops::{apply_gain, apply_matrix, noise_reduction};
use rawgraph_core::ImageBuf;

mod common;

/// Default source file inside the sample folder; `--file <path>` overrides it.
const SOURCE_NAME: &str = "_M5_9155.dng";
const SETTINGS: [(f32, f32); 4] = [(0.0, 0.0), (0.25, 0.5), (0.5, 0.7), (1.0, 1.0)];
const DEFAULT_RADIUS_PERMILLE: f32 = 0.3;
const CROP: u32 = 700;
/// Default crop origins; overridable on the command line.
const DEFAULT_ORIGINS: [(u32, u32); 4] = [(2900, 600), (6100, 300), (2700, 0), (1900, 1950)];

fn to_srgb8(img: &ImageBuf) -> Vec<u8> {
    let mut out = img.clone();
    apply_matrix(&mut out, to_f32(&acescg_to_rec709()));
    out.data()
        .chunks_exact(4)
        .flat_map(|p| [p[0], p[1], p[2]])
        .map(|v| (srgb_oetf(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8)
        .collect()
}

fn save(path: &Path, img: &ImageBuf) {
    let rgb = to_srgb8(img);
    rawgraph_io::save_srgb8(path, img.width(), img.height(), &rgb, 92, true).expect("write png");
    println!("wrote {}", path.display());
}

fn crop(img: &ImageBuf, x0: u32, y0: u32, w: u32, h: u32) -> ImageBuf {
    let mut out = ImageBuf::new(w, h);
    for y in 0..h {
        let src = ((y0 + y) as usize * img.width() as usize + x0 as usize) * 4;
        let dst = y as usize * w as usize * 4;
        out.data_mut()[dst..dst + w as usize * 4].copy_from_slice(&img.data()[src..src + w as usize * 4]);
    }
    out
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let source = match args.iter().position(|a| a == "--file") {
        Some(i) if i + 1 < args.len() => {
            let p = PathBuf::from(args.remove(i + 1));
            args.remove(i);
            p
        }
        _ => common::samples_dir().join(SOURCE_NAME),
    };
    let mode = args.first().map(String::as_str).unwrap_or("crops");
    let t = std::time::Instant::now();
    let loaded = rawgraph_io::load(source.as_path(), None).expect("load dng");
    let mut img = loaded.image;
    println!("loaded {}x{} in {:?}, ISO {:?}", img.width(), img.height(), t.elapsed(), loaded.capture.iso);
    apply_gain(&mut img, 2.0); // +1 EV
    let dir = common::out_dir().join("nr");
    std::fs::create_dir_all(&dir).expect("create output directory");

    if mode == "overview" {
        let mut small = img.downscale_box(1400);
        let scale = small.width() as f32 / img.width() as f32;
        // Tick marks every 500 full-resolution pixels, labelled by position in the file name only.
        for gx in (0..img.width()).step_by(500) {
            let x = (gx as f32 * scale) as u32;
            for y in 0..small.height() {
                small.set_pixel(x.min(small.width() - 1), y, [1.0, 0.0, 0.0, 1.0]);
            }
        }
        for gy in (0..img.height()).step_by(500) {
            let y = (gy as f32 * scale) as u32;
            for x in 0..small.width() {
                small.set_pixel(x, y.min(small.height() - 1), [0.0, 1.0, 0.0, 1.0]);
            }
        }
        save(&dir.join("overview_grid500.png"), &small);
        return;
    }

    let nums: Vec<u32> = args.iter().skip(1).filter_map(|a| a.parse().ok()).collect();
    let origins: Vec<(u32, u32)> =
        if nums.len() >= 2 { nums.chunks_exact(2).map(|c| (c[0], c[1])).collect() } else { DEFAULT_ORIGINS.to_vec() };
    let radius_px = DEFAULT_RADIUS_PERMILLE / 1000.0 * img.width().max(img.height()) as f32;
    println!("radius {radius_px:.2} px");
    for (luma, chroma) in SETTINGS {
        let t = std::time::Instant::now();
        for (i, (ox, oy)) in origins.iter().enumerate() {
            // Denoise the crop with a margin so the filter sees real neighbours at its border.
            let margin = 48;
            let (x0, y0) = (ox.saturating_sub(margin), oy.saturating_sub(margin));
            let (w, h) = ((CROP + 2 * margin).min(img.width() - x0), (CROP + 2 * margin).min(img.height() - y0));
            let region = crop(&img, x0, y0, w, h);
            let out = noise_reduction(&region, luma, chroma, radius_px);
            let c = crop(&out, ox - x0, oy - y0, CROP, CROP);
            let name = format!("crop{}_l{:03}_c{:03}.png", i + 1, (luma * 100.0) as u32, (chroma * 100.0) as u32);
            save(&dir.join(name), &c);
        }
        println!("luma {luma} chroma {chroma}: {:?}", t.elapsed());
    }
}
