//! 100 % crops for judging `sharpen`: 2x2 grids (top-left amount 0, top-right 0.5, bottom-left
//! 1.0, bottom-right 2.0) per crop and threshold, written to ...\shots\fx\sharpen\.
//!
//! `cargo run --release -p rawgraph-fx --example sharpen_crops -- [file] [radius_px]`
#![allow(clippy::chunks_exact_to_as_chunks)]

use rawgraph_core::color::{acescg_to_rec709, srgb_oetf, to_f32};
use rawgraph_core::{ops, ImageBuf};
use rawgraph_fx::sharpen;

mod common;

/// Default file inside the sample folder; the first command-line argument overrides it.
const FILE_NAME: &str = "_M4_1209.ARW";
const SIZE: u32 = 700;
const PAD: u32 = 48;

fn crop(img: &ImageBuf, x0: u32, y0: u32, w: u32, h: u32) -> ImageBuf {
    let mut out = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(x, y, img.pixel(x0 + x, y0 + y));
        }
    }
    out
}

fn to_srgb8(img: &ImageBuf) -> Vec<u8> {
    let m = to_f32(&acescg_to_rec709());
    let mut out = Vec::new();
    for px in img.data().chunks_exact(4) {
        for row in &m {
            let v = row[0] * px[0] + row[1] * px[1] + row[2] * px[2];
            out.push((srgb_oetf(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8);
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().map_or_else(|| common::samples_dir().join(FILE_NAME), std::path::PathBuf::from);
    let out = common::out_dir().join("fx").join("sharpen");
    let radius: f32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1.0);
    std::fs::create_dir_all(&out).unwrap();
    let mut img = rawgraph_io::load(path.as_path(), None).expect("load").image;
    ops::apply_gain(&mut img, 2.0);
    let (w, h) = (img.width(), img.height());
    println!("full image {w}x{h}, radius {radius}");

    // (name, centre as fraction of the image)
    let spots = [("eyes_hair", 0.415, 0.32), ("skin", 0.565, 0.56), ("speaker_edge", 0.315, 0.15)];
    for (name, fx, fy) in spots {
        let cx = (fx * w as f32) as u32;
        let cy = (fy * h as f32) as u32;
        let big = SIZE + 2 * PAD;
        let x0 = cx.saturating_sub(big / 2).min(w - big);
        let y0 = cy.saturating_sub(big / 2).min(h - big);
        let region = crop(&img, x0, y0, big, big);
        for thr in [0.0f32, 0.2] {
            let mut grid = ImageBuf::new(2 * SIZE, 2 * SIZE);
            for (i, amount) in [0.0f32, 0.5, 1.0, 2.0].into_iter().enumerate() {
                let s = sharpen(&region, amount, radius, thr, None);
                let (ox, oy) = ((i as u32 % 2) * SIZE, (i as u32 / 2) * SIZE);
                for y in 0..SIZE {
                    for x in 0..SIZE {
                        grid.set_pixel(ox + x, oy + y, s.pixel(PAD + x, PAD + y));
                    }
                }
            }
            let p = out.join(format!("{name}_r{radius}_thr{thr}.png"));
            rawgraph_io::save_srgb8(&p, grid.width(), grid.height(), &to_srgb8(&grid), 90, true).unwrap();
        }
    }
}
