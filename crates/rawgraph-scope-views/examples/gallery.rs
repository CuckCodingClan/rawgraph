//! Visual check of the scope views.
//!
//! `cargo run -p rawgraph-scope-views --example gallery -- --pattern photo`
//! `... -- --pattern bars75 --screenshot out.png`   (renders ~30 frames, saves PNG, exits)

use eframe::egui;
use rawgraph_scope_views::{HistogramMode, ParadeMode, ScopePanel, WaveformMode};
use rawgraph_scopes::{patterns, DisplayImage};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const PATTERNS: [(&str, &str); 7] = [
    ("bars75", "Colour bars 75 %"),
    ("bars100", "Colour bars 100 %"),
    ("ramp", "Horizontal ramp"),
    ("grey", "Flat mid-grey"),
    ("photo", "Synthetic photo"),
    ("noisy", "Noisy photo (grain + chroma noise)"),
    ("clipping", "Clipped highlights + crushed blacks"),
];

fn hash(x: u32, y: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    (h & 0xFFFF) as f32 / 65535.0
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Smooth sky / ground gradients with skin-tone-like patches and a little grain.
fn photo(width: u32, height: u32) -> DisplayImage {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    let skins = [
        (0.30f32, 0.58f32, 0.17f32, [0.86f32, 0.62, 0.50]),
        (0.62, 0.52, 0.14, [0.72, 0.50, 0.38]),
        (0.82, 0.66, 0.09, [0.93, 0.74, 0.62]),
    ];
    for y in 0..height {
        for x in 0..width {
            let (u, v) = (x as f32 / width as f32, y as f32 / height as f32);
            let sky_t = smooth(0.0, 0.55, v);
            let sky = [
                0.18 + 0.62 * sky_t,
                0.38 + 0.38 * sky_t,
                0.80 - 0.08 * sky_t,
            ];
            let ground_t = smooth(0.5, 1.0, v);
            let ground = [
                0.20 + 0.15 * ground_t + 0.10 * u,
                0.38 - 0.12 * ground_t,
                0.14 + 0.05 * u,
            ];
            let g = smooth(0.50, 0.60, v);
            let mut c = [0.0f32; 3];
            for k in 0..3 {
                c[k] = sky[k] * (1.0 - g) + ground[k] * g;
            }
            for (cx, cy, r, skin) in skins {
                let (dx, dy) = ((u - cx) * 1.6, v - cy);
                let d = (dx * dx + dy * dy).sqrt() / r;
                let m = 1.0 - smooth(0.75, 1.0, d);
                let shade = 1.0 - 0.25 * (dx * 3.0 + dy * 2.0).clamp(-1.0, 1.0).max(0.0);
                for k in 0..3 {
                    c[k] = c[k] * (1.0 - m) + skin[k] * shade * m;
                }
            }
            let n = (hash(x, y) - 0.5) * 0.03;
            rgb.extend(c.map(|v| (v + n).clamp(0.0, 1.0)));
        }
    }
    DisplayImage { width, height, rgb }
}

/// The synthetic photo with strong, per-channel noise (sensor grain and chroma speckle).
fn noisy(width: u32, height: u32) -> DisplayImage {
    let mut img = photo(width, height);
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 3) as usize;
            let luma_n = (hash(x, y) - 0.5) * 0.10;
            for k in 0..3 {
                let n = luma_n + (hash(x.wrapping_add(k * 7919), y.wrapping_add(k * 104_729)) - 0.5) * 0.12;
                img.rgb[i + k as usize] = (img.rgb[i + k as usize] + n).clamp(0.0, 1.0);
            }
        }
    }
    img
}

/// A dusk scene with a blown-out sky/sun (R and G clip, B less), a bright window that
/// clips every channel, and crushed shadows at the bottom (below zero).
fn clipping(width: u32, height: u32) -> DisplayImage {
    let mut rgb = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            let (u, v) = (x as f32 / width as f32, y as f32 / height as f32);
            // Sky: bright orange-warm gradient exposed too high.
            let sky = 1.0 - smooth(0.0, 0.55, v);
            let mut c = [0.40 + 0.80 * sky, 0.34 + 0.72 * sky, 0.30 + 0.38 * sky];
            // Sun: a small disc where everything is white.
            let (dx, dy) = ((u - 0.72) * 1.7, v - 0.22);
            let sun = 1.0 - smooth(0.03, 0.06, (dx * dx + dy * dy).sqrt());
            for v in &mut c {
                *v = *v * (1.0 - sun) + 1.25 * sun;
            }
            // Ground: mid-dark, falling below zero towards the bottom edge.
            let ground = smooth(0.52, 0.6, v);
            let t = smooth(0.55, 1.0, v);
            let g = [0.34 - 0.62 * t, 0.30 - 0.56 * t, 0.24 - 0.42 * t];
            for k in 0..3 {
                c[k] = c[k] * (1.0 - ground) + g[k] * ground;
            }
            // A bright window in the ground.
            if (0.12..0.20).contains(&u) && (0.62..0.72).contains(&v) {
                c = [1.1, 1.05, 0.95];
            }
            // A mildly textured wall on the left that stays inside the range.
            let n = (hash(x, y) - 0.5) * 0.02;
            rgb.extend(c.map(|t| t + n));
        }
    }
    DisplayImage { width, height, rgb }
}

fn make_pattern(name: &str) -> DisplayImage {
    match name {
        "noisy" => noisy(1024, 576),
        "clipping" => clipping(1024, 576),
        "bars75" => patterns::color_bars(1024, 576, 0.75),
        "bars100" => patterns::color_bars(1024, 576, 1.0),
        "ramp" => patterns::horizontal_ramp(1024, 576),
        "grey" => patterns::flat(1024, 576, [0.5, 0.5, 0.5]),
        _ => photo(1024, 576),
    }
}

fn to_color_image(img: &DisplayImage) -> egui::ColorImage {
    let px: Vec<egui::Color32> = img
        .rgb
        .as_chunks::<3>()
        .0
        .iter()
        .map(|c| {
            let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            egui::Color32::from_rgb(b(c[0]), b(c[1]), b(c[2]))
        })
        .collect();
    egui::ColorImage::new([img.width as usize, img.height as usize], px)
}

struct Gallery {
    pattern: String,
    image: DisplayImage,
    preview: Option<egui::TextureHandle>,
    panel: ScopePanel,
    dirty: bool,
    panel_size: Option<egui::Vec2>,
    frame: u32,
    screenshot: Option<PathBuf>,
    shot_requested: bool,
    shot_done: Arc<Mutex<bool>>,
}

impl Gallery {
    fn select(&mut self, name: &str) {
        self.pattern = name.to_owned();
        self.image = make_pattern(name);
        self.preview = None;
        self.dirty = true;
    }
}

impl eframe::App for Gallery {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frame += 1;
        if self.preview.is_none() {
            self.preview =
                Some(ctx.load_texture("preview", to_color_image(&self.image), egui::TextureOptions::LINEAR));
        }
        if self.dirty || self.panel.needs_refresh() {
            self.panel.set_image(&ctx, &self.image);
            self.dirty = false;
        }

        egui::Panel::left("preview").exact_size(360.0).show(ui, |ui| {
            ui.heading("Pattern");
            let mut chosen = None;
            for (id, label) in PATTERNS {
                if ui.selectable_label(self.pattern == id, label).clicked() {
                    chosen = Some(id);
                }
            }
            if let Some(id) = chosen {
                self.select(id);
            }
            ui.separator();
            if let Some(t) = &self.preview {
                let w = ui.available_width();
                let h = w * self.image.height as f32 / self.image.width as f32;
                ui.add(egui::Image::new((t.id(), egui::vec2(w, h))));
            }
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(size) = self.panel_size {
                // The panel gets exactly this area; a red frame just outside proves
                // that nothing is drawn beyond it.
                let rect = egui::Rect::from_min_size(ui.cursor().min + egui::vec2(3.0, 3.0), size);
                ui.painter().rect_stroke(
                    rect.expand(1.5),
                    0.0,
                    egui::Stroke::new(1.0, egui::Color32::from_rgb(200, 60, 60)),
                    egui::StrokeKind::Outside,
                );
                ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| self.panel.show(ui));
            } else {
                self.panel.show(ui);
            }
        });

        if self.screenshot.is_some() {
            ctx.request_repaint();
            if self.frame == 30 && !self.shot_requested {
                self.shot_requested = true;
                let path = self.screenshot.clone().expect("checked");
                let done = self.shot_done.clone();
                ctx.request_screenshot(move |img| {
                    let (w, h) = (img.width() as u32, img.height() as u32);
                    let bytes: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_array()).collect();
                    if let Some(dir) = path.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    match image::save_buffer(&path, &bytes, w, h, image::ColorType::Rgba8) {
                        Ok(()) => eprintln!("saved {} ({w}x{h})", path.display()),
                        Err(e) => eprintln!("screenshot failed: {e}"),
                    }
                    if let Ok(mut d) = done.lock() {
                        *d = true;
                    }
                });
            }
            if self.shot_done.lock().map(|d| *d).unwrap_or(false) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn parse_pair(s: &str, sep: char) -> Option<(f32, f32)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn main() -> eframe::Result {
    let mut pattern = "photo".to_owned();
    let mut screenshot = None;
    let mut panel_size = None;
    let mut hover = None;
    let mut opts: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pattern" => pattern = args.next().unwrap_or(pattern),
            "--screenshot" => screenshot = args.next().map(PathBuf::from),
            "--panel-size" => panel_size = args.next().and_then(|s| parse_pair(&s, 'x')),
            "--hover" => hover = args.next().and_then(|s| parse_pair(&s, ',')),
            "--option" => {
                if let Some(o) = args.next() {
                    opts.push(o);
                }
            }
            other => eprintln!("ignoring argument {other}"),
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1600.0, 1000.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "RawGraph scope views gallery",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let image = make_pattern(&pattern);
            Ok(Box::new(Gallery {
                pattern,
                image,
                preview: None,
                panel: {
                    let mut p = ScopePanel::new();
                    p.simulated_pointer = hover.map(|(x, y)| egui::vec2(x, y));
                    for o in &opts {
                        match o.as_str() {
                            "targets100" => p.style.targets_100 = true,
                            "percent" => p.style.scale = rawgraph_scope_views::ScaleKind::Percent,
                            "rgb" => p.style.waveform_rgb = true,
                            "zoom" => p.style.zoom = true,
                            "yrgb" => p.style.parade_mode = ParadeMode::Yrgb,
                            "ycbcr" => p.style.parade_mode = ParadeMode::YCbCr,
                            "hist-stacked" => p.style.histogram_mode = HistogramMode::Stacked,
                            "hist-luma" => p.style.histogram_mode = HistogramMode::Luma,
                            "lowpass" => p.style.lowpass = true,
                            "wave-r" => p.style.waveform_mode = WaveformMode::Red,
                            "wave-g" => p.style.waveform_mode = WaveformMode::Green,
                            "wave-b" => p.style.waveform_mode = WaveformMode::Blue,
                            "wave-rgb" => p.style.waveform_mode = WaveformMode::Rgb,
                            "no-markers" => p.style.clip_markers = false,
                            "focus-parade" => p.focus = rawgraph_scope_views::ScopeKind::Parade,
                            "layout-parade" => p.layout = rawgraph_scope_views::PanelLayout::Single(rawgraph_scope_views::ScopeKind::Parade),
                            "layout-hist" => p.layout = rawgraph_scope_views::PanelLayout::Single(rawgraph_scope_views::ScopeKind::Histogram),
                            "layout-wave" => p.layout = rawgraph_scope_views::PanelLayout::Single(rawgraph_scope_views::ScopeKind::Waveform),
                            _ => {}
                        }
                    }
                    p
                },
                panel_size: panel_size.map(|(w, h)| egui::vec2(w, h)),
                dirty: true,
                frame: 0,
                screenshot,
                shot_requested: false,
                shot_done: Arc::new(Mutex::new(false)),
            }))
        }),
    )
}
