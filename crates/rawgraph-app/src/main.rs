//! RawGraph desktop application (wave 1: interactive loop on built-in test images).

mod app;
mod autosave;
mod brush_logic;
mod capture_refresh;
mod cli;
mod clock;
mod collection;
mod compare;
mod crop_logic;
mod curve_editor;
mod curve_logic;
mod datadir;
mod develop;
mod dialogs;
mod export;
mod filmstrip;
mod filter;
mod frames;
mod gpu_path;
mod gpu_real;
mod gpu_session;
mod graph_clip;
mod graph_editor;
mod groups;
mod histogram;
mod history;
mod inspector;
mod missing_check;
mod node_ui;
mod prefetch;
mod provider;
mod ranges_logic;
mod recipes;
mod recovery_guard;
mod relink;
mod render;
mod revisions;
mod save_flow;
mod scan;
mod selection;
mod session;
mod shot;
mod status;
mod testimages;
mod textfit;
mod thumbs;
mod tool_panels;
mod tools;
mod viewer;
#[cfg(test)]
mod wave8_tests;
#[cfg(test)]
mod wave8b_tests;
mod windows;
mod xmp_import;

use std::sync::atomic::Ordering;

fn main() {
    let cli = match cli::parse_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rawgraph-app: {e}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    if cli.help {
        println!("{}", cli::USAGE);
        return;
    }

    let (w, h) = cli.size.unwrap_or((1600.0, 1000.0));
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("RawGraph")
        .with_inner_size([w, h])
        .with_min_inner_size([900.0, 600.0]);
    if cli.screenshot.is_some() {
        // Reproducible screenshots: fixed size, not restored from a previous run.
        viewport = viewport.with_resizable(true);
    }
    // Same device as egui would create, with the limits the GPU preview needs (never above
    // what the adapter offers, so the request cannot fail on them).
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut wgpu_options.wgpu_setup {
        create.device_descriptor = std::sync::Arc::new(gpu_real::device_descriptor);
    }
    let options = eframe::NativeOptions {
        viewport,
        wgpu_options,
        persist_window: cli.screenshot.is_none() && cli.run_export_node.is_none(),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    let cli_for_app = cli.clone();
    let result = eframe::run_native("RawGraph", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, cli_for_app)))));
    if let Err(e) = result {
        eprintln!("rawgraph-app: {e}");
        std::process::exit(1);
    }
    let code = app::EXIT_CODE.load(Ordering::SeqCst);
    if code != 0 {
        std::process::exit(code);
    }
}

