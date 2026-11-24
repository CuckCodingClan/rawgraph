//! Timing of the three steps (`cargo run --release -p rawgraph-scope-views --example bench`).

use rawgraph_scope_views::{prepare, ParadeMode, PrepareSettings, ScopePanel};
use rawgraph_scopes::{analyze, DisplayImage};
use std::time::Instant;

fn photo(width: u32, height: u32) -> DisplayImage {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let (u, v) = (x as f32 / width as f32, y as f32 / height as f32);
            let n = ((x.wrapping_mul(2654435761) ^ y.wrapping_mul(40503)) & 0xFF) as f32 / 255.0 * 0.03;
            rgb.extend([
                (0.2 + 0.6 * u + n).clamp(0.0, 1.0),
                (0.3 + 0.4 * v + n).clamp(0.0, 1.0),
                (0.8 - 0.5 * u * v + n).clamp(0.0, 1.0),
            ]);
        }
    }
    DisplayImage { width, height, rgb }
}

fn stats(label: &str, mut t: Vec<f64>) {
    t.sort_by(f64::total_cmp);
    println!("{label:<14} min {:6.2} ms   median {:6.2} ms", t[0], t[t.len() / 2]);
}

fn run(label: &str, panel: &mut ScopePanel, ctx: &egui::Context, img: &DisplayImage, settings: PrepareSettings) {
    let (mut a, mut p, mut s) = (vec![], vec![], vec![]);
    for _ in 0..10 {
        let t = Instant::now();
        let set = analyze(img, &settings.request());
        a.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        let prepared = prepare(&set, &settings);
        p.push(t.elapsed().as_secs_f64() * 1e3);
        let t = Instant::now();
        panel.set_prepared(ctx, prepared);
        s.push(t.elapsed().as_secs_f64() * 1e3);
    }
    println!("== {label}");
    stats("analyze", a);
    stats("prepare", p);
    stats("set_prepared", s);
}

fn main() {
    let img = photo(2560, 1707);
    let ctx = egui::Context::default();
    let mut panel = ScopePanel::new();
    let settings = panel.prepare_settings();
    // Colorize off (the previous look and cost) next to on (the default).
    run("Colorize OFF (previous cost)", &mut panel, &ctx, &img, PrepareSettings { colourise: false, ..settings.clone() });
    run("Colorize ON (default)", &mut panel, &ctx, &img, settings.clone());
    run("Colorize OFF (again)", &mut panel, &ctx, &img, PrepareSettings { colourise: false, ..settings.clone() });
    run("Colorize ON (again)", &mut panel, &ctx, &img, settings.clone());
    let wf_only = PrepareSettings {
        parade: false,
        vectorscope: false,
        histogram: false,
        clipping: false,
        ..settings.clone()
    };
    run("waveform only, OFF", &mut panel, &ctx, &img, PrepareSettings { colourise: false, ..wf_only.clone() });
    run("waveform only, ON", &mut panel, &ctx, &img, wf_only.clone());
    let all = PrepareSettings {
        parade_mode: ParadeMode::Yrgb,
        lowpass: true,
        clipping: true,
        ..settings.clone()
    };
    run("YRGB + clipping + low-pass", &mut panel, &ctx, &img, all);
    let clip_only = PrepareSettings { clipping: false, ..settings };
    run("default, clipping off (old cost)", &mut panel, &ctx, &img, clip_only);
}
