//! Writes a synthetic test picture for CPU / GPU comparisons: `gentest <out.png> [long edge]`
//! (default 2560 px, 3:2). It has smooth gradients (sky, ground), a hard edge, fine texture and
//! noise, a checker, and saturated colour bars, so every neighbourhood operation has something to do.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(out) = args.first() else {
        eprintln!("usage: gentest <out.png> [long edge]");
        std::process::exit(2);
    };
    let long: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(2560).max(16);
    let (w, h) = (long, long * 2 / 3);
    let mut img = image::RgbImage::new(w, h);
    let mut seed = 0x9E37_79B9u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
    };
    let scale = w as f32 / 2560.0;
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            // Sky: a bright vertical gradient; ground: dark, slowly varying.
            let mut v = if fy < 0.5 { 0.95 - 0.55 * fy } else { 0.10 + 0.12 * fx };
            // A hard-edged bright rectangle and a dark one.
            if (0.58..0.8).contains(&fx) && (0.25..0.72).contains(&fy) {
                v = 0.98;
            }
            if (0.1..0.3).contains(&fx) && (0.55..0.9).contains(&fy) {
                v = 0.03;
            }
            // Fine texture and sensor-like noise.
            let tex = 0.5 + 0.5 * ((x as f32 / scale * 0.55).sin() * (y as f32 / scale * 0.8).cos());
            v *= 0.85 + 0.15 * tex;
            v += 0.03 * noise();
            let mut px = [v * (0.75 + 0.25 * fx), v * (0.85 + 0.15 * fy), v * (1.05 - 0.2 * fx)];
            // Checker.
            let cell = (24.0 * scale).max(1.0) as u32;
            if (0.35..0.55).contains(&fx) && (0.6..0.85).contains(&fy) && (x / cell + y / cell).is_multiple_of(2) {
                px = [px[0] * 0.55, px[1] * 0.55, px[2] * 0.55];
            }
            // Saturated colour bars along the bottom.
            if fy > 0.92 {
                let bars: [[f32; 3]; 6] = [[0.95, 0.05, 0.05], [0.95, 0.85, 0.05], [0.05, 0.85, 0.1], [0.05, 0.8, 0.9], [0.1, 0.1, 0.95], [0.9, 0.1, 0.8]];
                px = bars[((fx * 6.0) as usize).min(5)];
            }
            img.put_pixel(x, y, image::Rgb(px.map(|c| (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)));
        }
    }
    if let Err(e) = img.save(out) {
        eprintln!("{out}: {e}");
        std::process::exit(1);
    }
    println!("wrote {out} ({w}x{h})");
}
