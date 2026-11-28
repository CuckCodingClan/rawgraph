//! Verification gallery: before / after PNGs of every effect on real samples.
//!
//! `cargo run --release -p rawgraph-fx --example fx_gallery -- [portrait.dng] [colour.dng]`
//! Samples: `RAWGRAPH_SAMPLES`, output: `RAWGRAPH_OUT\fx` (see README; samples never leave this machine).

#![allow(clippy::chunks_exact_to_as_chunks)]

use std::path::{Path, PathBuf};

use rawgraph_core::color::{acescg_to_rec709, srgb_oetf, to_f32};
use rawgraph_core::{
    ops, BlendMode, BrushStroke, CdlParams, FitMode, ImageBuf, MaskBuf, ResampleFilter, VignetteRadial,
    WarpRectilinear,
};
use rawgraph_fx::*;

mod common;

fn out_fx() -> PathBuf {
    common::out_dir().join("fx")
}

fn to_srgb8(img: &ImageBuf) -> Vec<u8> {
    let m = to_f32(&acescg_to_rec709());
    let mut out = Vec::with_capacity(img.width() as usize * img.height() as usize * 3);
    for px in img.data().chunks_exact(4) {
        for row in &m {
            let v = row[0] * px[0] + row[1] * px[1] + row[2] * px[2];
            out.push((srgb_oetf(v.clamp(0.0, 1.0)) * 255.0 + 0.5) as u8);
        }
    }
    out
}

fn save(name: &str, img: &ImageBuf) {
    let p = out_fx().join(format!("{name}.png"));
    rawgraph_io::save_srgb8(&p, img.width(), img.height(), &to_srgb8(img), 90, true).expect("save");
}

fn save_mask(name: &str, m: &MaskBuf) {
    let rgb: Vec<u8> = m.data.iter().flat_map(|v| [(v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8; 3]).collect();
    let p = out_fx().join(format!("{name}.png"));
    rawgraph_io::save_srgb8(&p, m.width, m.height, &rgb, 90, true).expect("save");
}

/// Left half before, right half after, side by side in one file (same size as the inputs).
fn pair(name: &str, before: &ImageBuf, after: &ImageBuf) {
    let (w, h) = (before.width(), before.height());
    let (aw, ah) = (after.width(), after.height());
    let (tw, th) = (w + aw + 8, h.max(ah));
    let mut out = ImageBuf::new(tw, th);
    for y in 0..th {
        for x in 0..tw {
            out.set_pixel(x, y, [0.02, 0.02, 0.02, 1.0]);
        }
    }
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(x, y, before.pixel(x, y));
        }
    }
    for y in 0..ah {
        for x in 0..aw {
            out.set_pixel(w + 8 + x, y, after.pixel(x, y));
        }
    }
    save(name, &out);
}


fn stroke(points: &[[f32; 2]], radius: f32, hardness: f32, flow: f32, erase: bool) -> BrushStroke {
    BrushStroke { points: points.to_vec(), radius, hardness, flow, erase }
}

/// Soft S-shaped stroke, hard stroke crossing it, erase stroke through both, dabs.
fn demo_strokes() -> Vec<BrushStroke> {
    let s_curve: Vec<[f32; 2]> = (0..=60)
        .map(|i| {
            let u = i as f32 / 60.0;
            [0.12 + 0.76 * u, 0.5 + 0.28 * (u * std::f32::consts::TAU).sin() * (1.0 - 0.3 * u)]
        })
        .collect();
    vec![
        stroke(&s_curve, 0.05, 0.1, 1.0, false),
        stroke(&[[0.5, 0.08], [0.45, 0.4], [0.55, 0.7], [0.5, 0.95]], 0.025, 1.0, 1.0, false),
        stroke(&[[0.05, 0.62], [0.3, 0.45], [0.7, 0.55], [0.95, 0.4]], 0.02, 0.5, 1.0, true),
        stroke(&[[0.15, 0.15]], 0.06, 0.0, 1.0, false),
        stroke(&[[0.3, 0.15]], 0.06, 0.6, 0.5, false),
        stroke(&[[0.45, 0.15]], 0.06, 1.0, 1.0, false),
        stroke(&[[0.8, 0.88]], 0.07, 0.3, 0.6, false),
        stroke(&[[0.8, 0.88]], 0.035, 0.3, 1.0, true),
    ]
}

fn save_rgb(name: &str, w: u32, h: u32, rgb: &[u8]) {
    let p = out_fx().join(format!("{name}.png"));
    rawgraph_io::save_srgb8(&p, w, h, rgb, 90, true).expect("save");
}

/// Plots a curve (perceptual axis, grid, diagonal, control points) into a PNG.
fn plot_curve(name: &str, pts: &[[f32; 2]]) {
    const N: u32 = 360;
    let mut rgb = vec![24u8; (N * N * 3) as usize];
    let mut put = |x: i32, y: i32, c: [u8; 3]| {
        if x >= 0 && y >= 0 && (x as u32) < N && (y as u32) < N {
            let i = ((y as u32 * N + x as u32) * 3) as usize;
            rgb[i..i + 3].copy_from_slice(&c);
        }
    };
    let m = 20i32;
    let span = (N as i32 - 2 * m) as f32;
    let px = |t: f32| m + (t * span) as i32;
    let py = |t: f32| N as i32 - m - (t * span) as i32;
    for k in 0..=4 {
        let t = k as f32 / 4.0;
        for i in 0..=span as i32 {
            put(px(t), m + i, [60, 60, 60]);
            put(m + i, py(t), [60, 60, 60]);
        }
    }
    for i in 0..=span as i32 {
        let t = i as f32 / span;
        put(px(t), py(t), [110, 110, 110]);
    }
    for i in 0..=(span as i32 * 4) {
        let t = i as f32 / (span * 4.0);
        let (x, y) = (px(t), py(curve_eval(pts, t)));
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            put(x + dx, y + dy, [255, 210, 80]);
        }
    }
    for p in pts {
        for dy in -3..=3 {
            for dx in -3..=3 {
                put(px(p[0]) + dx, py(p[1]) + dy, [255, 255, 255]);
            }
        }
    }
    save_rgb(name, N, N, &rgb);
}

fn bands(f: impl Fn(&mut [[f32; 3]; 6])) -> [[f32; 3]; 6] {
    let mut b = [[0.0; 3]; 6];
    f(&mut b);
    b
}

fn run(tag: &str, path: &Path) {
    let loaded = rawgraph_io::load(path, Some(1600)).expect("load");
    let mut base = loaded.image;
    ops::apply_gain(&mut base, 2.0); // +1 EV
    let (w, h) = (base.width(), base.height());
    println!("{tag}: {} -> {w}x{h}", path.display());
    save(&format!("{tag}_00_base"), &base);

    let mut a = base.clone();
    tone_curve(&mut a, 0.8, -0.6, 0.7, -0.9);
    pair(&format!("{tag}_01_tone_curve"), &base, &a);

    let mut a = base.clone();
    black_white(&mut a, [0.5, 0.3, 0.2]);
    pair(&format!("{tag}_02_black_white"), &base, &a);

    // Hue +1 on yellow/green, saturation up on blue/cyan, down on red (skin), lightness on magenta.
    let mut a = base.clone();
    hue_saturation(&mut a, &bands(|b| {
        b[0] = [0.0, -0.8, 0.0];
        b[1] = [-0.7, 0.0, 0.3];
        b[2] = [0.8, 0.5, 0.0];
        b[3] = [0.0, 0.8, 0.0];
        b[4] = [0.0, 1.0, -0.6];
        b[5] = [0.5, 0.0, 0.5];
    }));
    pair(&format!("{tag}_03_hue_saturation"), &base, &a);

    let mut a = base.clone();
    hue_saturation(&mut a, &bands(|b| b.iter_mut().for_each(|x| x[1] = 1.0)));
    pair(&format!("{tag}_03b_sat_all_plus1"), &base, &a);

    let mut a = base.clone();
    vignette(&mut a, -0.8, 0.45, 0.5);
    pair(&format!("{tag}_04_vignette_dark"), &base, &a);
    let mut a = base.clone();
    vignette(&mut a, 0.6, 0.45, 0.5);
    pair(&format!("{tag}_04b_vignette_bright"), &base, &a);

    pair(&format!("{tag}_08_sharpen"), &base, &sharpen(&base, 1.5, 2.0, 0.0, None));
    let soft = ops::blur(&base, 2.0);
    pair(&format!("{tag}_08b_sharpen_on_soft"), &soft, &sharpen(&soft, 2.0, 2.0, 0.02, None));

    let p = TransformParams { angle_deg: 8.0, left: 0.05, right: 0.95, ..Default::default() };
    pair(&format!("{tag}_09_transform_rot8_crop"), &base, &transform(&base, &p));
    let p = TransformParams { quarter_turns: 1, flip_h: true, ..Default::default() };
    pair(&format!("{tag}_09b_transform_turn_flip"), &base, &transform(&base, &p));

    // Synthetic grid shows distortion best.
    let mut grid = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let on = x % 100 < 3 || y % 100 < 3;
            let v = if on { 1.0 } else { 0.1 };
            grid.set_pixel(x, y, [v, v, v, 1.0]);
        }
    }
    pair(&format!("{tag}_15_lens_grid_d+0.25"), &grid, &lens_manual(&grid, 0.25, 0.0, 0.0, 0.0));
    pair(&format!("{tag}_15b_lens_grid_d-0.2"), &grid, &lens_manual(&grid, -0.2, 0.0, 0.0, 0.0));
    pair(&format!("{tag}_16_lens_distortion_ca"), &base, &lens_manual(&base, -0.15, 0.7, 6.0, -6.0));

    // Painted brush strokes.
    let strokes = demo_strokes();
    let brush = stroke_mask(w, h, &strokes, false);
    save_mask(&format!("{tag}_20_brush_mask"), &brush);
    save_mask(&format!("{tag}_20b_brush_mask_inverted"), &stroke_mask(w, h, &strokes, true));
    save_mask(&format!("{tag}_20c_brush_mask_1600"), &stroke_mask(1600, 1067, &strokes, false));
    let mut a = base.clone();
    let exposure: Vec<_> = brush.data.iter().map(|&v| v * 1.5).collect();
    ops::exposure_mapped(&mut a, &exposure);
    pair(&format!("{tag}_21_brush_gain_plus1_5ev"), &base, &a);

    // Point curves.
    let s_pts = [[0.0, 0.0], [0.25, 0.12], [0.5, 0.5], [0.75, 0.88], [1.0, 1.0]];
    let lift_pts = [[0.0, 0.12], [0.12, 0.38], [0.35, 0.55], [1.0, 1.0]];
    plot_curve("curve_plot_s", &s_pts);
    plot_curve("curve_plot_lift", &lift_pts);
    let mut a = base.clone();
    point_curve(&mut a, &s_pts, CurveChannel::Luminance);
    pair(&format!("{tag}_22_curve_s_luminance"), &base, &a);
    let mut a = base.clone();
    point_curve(&mut a, &lift_pts, CurveChannel::Luminance);
    pair(&format!("{tag}_23_curve_shadow_lift"), &base, &a);
    let mut a = base.clone();
    point_curve(&mut a, &[[0.0, 0.0], [0.5, 0.42], [1.0, 1.0]], CurveChannel::Blue);
    point_curve(&mut a, &[[0.0, 0.0], [0.5, 0.56], [1.0, 1.0]], CurveChannel::Red);
    pair(&format!("{tag}_24_curve_red_blue_warm"), &base, &a);
}

/// Crop of `img` (pixel box), for 100 % views.
fn crop(img: &ImageBuf, x0: u32, y0: u32, w: u32, h: u32) -> ImageBuf {
    let mut out = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            out.set_pixel(x, y, img.pixel(x0 + x, y0 + y));
        }
    }
    out
}

/// Wave 7: local contrast and colour balance on a Develop-like base (+2 EV).
/// `crop_box` = (x, y, w, h) of a 100 % detail crop that is saved next to the full pairs.
fn run_w7(tag: &str, path: &Path, crop_box: (u32, u32, u32, u32)) {
    let loaded = rawgraph_io::load(path, Some(1600)).expect("load");
    let mut base = loaded.image;
    ops::apply_gain(&mut base, 4.0); // +2 EV, the Develop-equivalent exposure
    println!("{tag} (wave 7): {}x{}", base.width(), base.height());
    let (cx, cy, cw, ch) = crop_box;
    let cases: [(&str, f32, f32); 4] =
        [("clarity_p0.8", 0.8, 0.0), ("clarity_m0.6", -0.6, 0.0), ("texture_p0.8", 0.0, 0.8), ("texture_m0.6", 0.0, -0.6)];
    for (name, c, t) in cases {
        let t0 = std::time::Instant::now();
        let out = local_contrast(&base, c, t);
        println!("  {name}: {:.0} ms", t0.elapsed().as_secs_f64() * 1000.0);
        pair(&format!("{tag}_w7_{name}"), &base, &out);
        pair(&format!("{tag}_w7_{name}_crop"), &crop(&base, cx, cy, cw, ch), &crop(&out, cx, cy, cw, ch));
    }
    // CDL: gain_r +0.25 / gain_b -0.3 stops; lift_b 0.15 (offset 0.18 * 0.15); gamma +0.5 (power 2^-0.5).
    let settings: [(&str, CdlParams); 3] = [
        ("warm_highlights", CdlParams { slope: [0.25f32.exp2(), 1.0, (-0.3f32).exp2()], ..CdlParams::IDENTITY }),
        ("cool_shadows", CdlParams { offset: [0.0, 0.0, 0.18 * 0.15], ..CdlParams::IDENTITY }),
        ("gamma_p0.5", CdlParams { power: [(-0.5f32).exp2(); 3], ..CdlParams::IDENTITY }),
    ];
    for (name, p) in settings {
        let mut a = base.clone();
        color_wheels(&mut a, &p);
        pair(&format!("{tag}_w7_wheels_{name}"), &base, &a);
    }
}

// ------------------------------------------------------------------ wave 8

const TILE_W: u32 = 460;
const TILE_H: u32 = 307;

fn save_shot(name: &str, img: &ImageBuf) {
    let p = common::out_dir().join(name);
    rawgraph_io::save_srgb8(&p, img.width(), img.height(), &to_srgb8(img), 90, true).expect("save");
    println!("wrote {} ({}x{})", p.display(), img.width(), img.height());
}

/// Equal-sized tiles in a grid with a dark gap.
fn sheet(tiles: &[ImageBuf], cols: usize, gap: u32) -> ImageBuf {
    let (tw, th) = (tiles[0].width(), tiles[0].height());
    let rows = tiles.len().div_ceil(cols) as u32;
    let (sw, sh) = (cols as u32 * tw + (cols as u32 + 1) * gap, rows * th + (rows + 1) * gap);
    let mut out = ImageBuf::new(sw, sh);
    for px in out.data_mut().chunks_exact_mut(4) {
        px.copy_from_slice(&[0.004, 0.004, 0.004, 1.0]);
    }
    for (i, t) in tiles.iter().enumerate() {
        let (x0, y0) = (gap + (i % cols) as u32 * (tw + gap), gap + (i / cols) as u32 * (th + gap));
        for y in 0..th {
            for x in 0..tw {
                out.set_pixel(x0 + x, y0 + y, t.pixel(x, y));
            }
        }
    }
    out
}

fn tile_of(img: &ImageBuf) -> ImageBuf {
    place_layer(img, TILE_W, TILE_H, FitMode::Fill, ResampleFilter::Bicubic, 1.0)
}

fn checker(w: u32, h: u32) -> ImageBuf {
    let mut c = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let v = if (x / 16 + y / 16) % 2 == 0 { 0.03 } else { 0.06 };
            c.set_pixel(x, y, [v, v, v, 1.0]);
        }
    }
    c
}

fn grid_image(w: u32, h: u32) -> ImageBuf {
    let mut g = ImageBuf::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let on = (x + 19) % 38 < 2 || (y + 19) % 38 < 2;
            let v = if on { 0.9 } else { 0.04 };
            g.set_pixel(x, y, [v, v, v, 1.0]);
        }
    }
    g
}

fn load_photo(name: &str, gain: f32) -> ImageBuf {
    let mut img = rawgraph_io::load(&common::samples_dir().join(name), Some(1600)).expect("load").image;
    ops::apply_gain(&mut img, gain);
    img
}

fn wave8() {
    std::fs::create_dir_all(common::out_dir()).expect("shots folder");
    let a_full = load_photo("_M4_1209.ARW", 4.0);
    let b_full = load_photo("_M5_6571.dng", 4.0);
    let a = tile_of(&a_full);
    let b = tile_of(&b_full);

    // --- layers: photo B over photo A in five blend modes, then the four fit modes
    let blended = |mode: BlendMode, opacity: f32| {
        let mut c = a.clone();
        let placed = place_layer(&b_full, TILE_W, TILE_H, FitMode::Fill, ResampleFilter::Bicubic, 1.0);
        blend_layer(&mut c, &placed, mode, opacity);
        c
    };
    // Portrait layer: the middle 2:3 slice of photo B, placed on a landscape checkerboard canvas.
    let (bw, bh) = (b_full.width(), b_full.height());
    let pw = ((bh as f32 * 2.0 / 3.0) as u32).min(bw);
    let mut portrait = ImageBuf::new(pw, bh);
    for y in 0..bh {
        for x in 0..pw {
            portrait.set_pixel(x, y, b_full.pixel(x + (bw - pw) / 2, y));
        }
    }
    let on_checker = |fit: FitMode, scale: f32| {
        let mut c = checker(TILE_W, TILE_H);
        let placed = place_layer(&portrait, TILE_W, TILE_H, fit, ResampleFilter::Bicubic, scale);
        blend_layer(&mut c, &placed, BlendMode::Normal, 1.0);
        c
    };
    let tiles = vec![
        a.clone(),
        b.clone(),
        blended(BlendMode::Normal, 0.5),
        blended(BlendMode::Multiply, 1.0),
        blended(BlendMode::Screen, 1.0),
        blended(BlendMode::Overlay, 1.0),
        blended(BlendMode::Difference, 1.0),
        on_checker(FitMode::Stretch, 1.0),
        on_checker(FitMode::Fit, 1.0),
        on_checker(FitMode::Fill, 1.0),
        on_checker(FitMode::Crop, 0.35),
    ];
    let big = sheet(&tiles, 4, 8);
    let lh = (big.height() as f32 * 1400.0 / big.width() as f32) as u32;
    save_shot("8-fx-layers.png", &place_layer(&big, 1400, lh, FitMode::Stretch, ResampleFilter::Bicubic, 1.0));

    // --- whites / blacks
    let wb = |w: f32, bl: f32| {
        let mut x = a.clone();
        whites_blacks(&mut x, w, bl);
        x
    };
    save_shot("8-fx-whites-blacks.png", &sheet(&[a.clone(), wb(1.0, 0.0), wb(-1.0, 0.0), wb(0.0, 1.0), wb(0.0, -1.0)], 3, 8));

    // --- vibrance on the colourful photo
    let vb = |v: f32| {
        let mut x = b.clone();
        vibrance(&mut x, v);
        x
    };
    save_shot("8-fx-vibrance.png", &sheet(&[b.clone(), vb(1.0), vb(-1.0)], 3, 8));

    // --- hue curves
    let curve = |pts: &[[f32; 2]], ch: CurveChannel| {
        let mut x = a.clone();
        point_curve(&mut x, pts, ch);
        x
    };
    // Reds (hue near 0 and near 1) 15 degrees towards orange: y = 0.5 + 15 / 120.
    let red_to_orange = [[0.0, 0.62], [0.03, 0.62], [0.08, 0.5], [0.95, 0.5], [0.98, 0.62], [1.0, 0.62]];
    let green_grey = [[0.0, 0.5], [0.22, 0.5], [0.28, 0.0], [0.4, 0.0], [0.46, 0.5], [1.0, 0.5]];
    let low_sat_boost = [[0.0, 1.0], [0.25, 0.75], [0.6, 0.5], [1.0, 0.5]];
    let tiles = vec![
        a.clone(),
        curve(&red_to_orange, CurveChannel::HueVsHue),
        curve(&green_grey, CurveChannel::HueVsSat),
        curve(&low_sat_boost, CurveChannel::SatVsSat),
    ];
    save_shot("8-fx-huecurves.png", &sheet(&tiles, 2, 8));

    // --- warp and vignette
    let grid = grid_image(TILE_W, TILE_H);
    let centre = [0.5, 0.5];
    let barrel = warp_rectilinear(&grid, &WarpRectilinear { planes: vec![[1.0, 0.18, 0.0, 0.0, 0.0, 0.0]], center: centre });
    let corrected =
        warp_rectilinear(&barrel, &WarpRectilinear { planes: vec![[1.0, -0.16, 0.0, 0.0, 0.0, 0.0]], center: centre });
    let mut falloff = a.clone();
    {
        let (w, h) = (falloff.width() as f32, falloff.height() as f32);
        let m = (w * 0.5).hypot(h * 0.5);
        for y in 0..falloff.height() {
            for x in 0..falloff.width() {
                let r2 = ((x as f32 + 0.5 - w * 0.5).powi(2) + (y as f32 + 0.5 - h * 0.5).powi(2)) / (m * m);
                let g = 1.0 / (1.0 + 0.6 * r2 + 0.5 * r2 * r2);
                let p = falloff.pixel(x, y);
                falloff.set_pixel(x, y, [p[0] * g, p[1] * g, p[2] * g, p[3]]);
            }
        }
    }
    let mut fixed = falloff.clone();
    vignette_radial(&mut fixed, &VignetteRadial { k: [0.6, 0.5, 0.0, 0.0, 0.0], center: centre });
    // Timings on 24 MP (6000 x 4000).
    {
        let big = ImageBuf::new(6000, 4000);
        let t = std::time::Instant::now();
        let o = warp_rectilinear(&big, &WarpRectilinear { planes: vec![[1.0, 0.02, 0.0, 0.0, 0.0, 0.0]], center: centre });
        println!("warp_rectilinear 24 MP, 1 plane: {:.2} s", t.elapsed().as_secs_f64());
        drop(o);
        let t = std::time::Instant::now();
        let three = vec![[1.001, 0.02, 0.0, 0.0, 0.0, 0.0], [1.0, 0.02, 0.0, 0.0, 0.0, 0.0], [0.999, 0.02, 0.0, 0.0, 0.0, 0.0]];
        let o = warp_rectilinear(&big, &WarpRectilinear { planes: three, center: centre });
        println!("warp_rectilinear 24 MP, 3 planes: {:.2} s", t.elapsed().as_secs_f64());
        drop(o);
        let t = std::time::Instant::now();
        let o = place_layer(&big, 6000, 4000, FitMode::Crop, ResampleFilter::Bicubic, 1.0003);
        println!("place_layer 24 MP bicubic (generic path): {:.2} s", t.elapsed().as_secs_f64());
        drop(o);
        let t = std::time::Instant::now();
        let o = place_layer(&big, 4500, 3000, FitMode::Fit, ResampleFilter::Bicubic, 1.0);
        println!("place_layer 24 MP -> 13.5 MP bicubic: {:.2} s", t.elapsed().as_secs_f64());
        drop(o);
        let mut c = big.clone();
        let p = place_layer(&big, 6000, 4000, FitMode::Fit, ResampleFilter::Linear, 1.0);
        let t = std::time::Instant::now();
        blend_layer(&mut c, &p, BlendMode::Overlay, 0.7);
        println!("blend_layer 24 MP: {:.2} s", t.elapsed().as_secs_f64());
    }
    save_shot("8-fx-warp.png",&sheet(&[grid, barrel, corrected, a.clone(), falloff, fixed], 3, 8));
}

fn main() {
    std::fs::create_dir_all(out_fx()).expect("output folder");
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let pick = |i: usize, default: &str| args.get(i).cloned().unwrap_or_else(|| common::samples_dir().join(default));
    if std::env::var_os("RG_W8_ONLY").is_some() {
        wave8();
        return;
    }
    // RG_W7_ONLY=1 skips the long list of earlier effects.
    if std::env::var_os("RG_W7_ONLY").is_none() {
        run("portrait", &pick(0, "_M4_1209.ARW"));
        run("colour", &pick(1, "_M5_6571.dng"));
    }
    run_w7("portrait", &pick(0, "_M4_1209.ARW"), (300, 150, 700, 450));
    run_w7("colour", &pick(1, "_M5_6571.dng"), (300, 300, 700, 450));
    wave8();
}
