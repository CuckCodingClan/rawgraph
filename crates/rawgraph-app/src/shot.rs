//! Screenshot helpers for the `--screenshot` mode.

use std::path::Path;
use std::sync::atomic::Ordering;

use eframe::egui;

use crate::app::EXIT_CODE;

pub fn report(result: Result<(), String>, path: &Path) {
    match result {
        Ok(()) => eprintln!("screenshot saved: {}", path.display()),
        Err(e) => {
            eprintln!("screenshot failed: {e}");
            EXIT_CODE.store(1, Ordering::SeqCst);
        }
    }
}

pub fn save_png(path: &Path, img: &egui::ColorImage) -> Result<(), String> {
    let [w, h] = img.size;
    let mut bytes = Vec::with_capacity(w * h * 4);
    for p in &img.pixels {
        bytes.extend_from_slice(&p.to_srgba_unmultiplied());
    }
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
    }
    image::save_buffer(path, &bytes, w as u32, h as u32, image::ColorType::Rgba8).map_err(|e| e.to_string())
}
