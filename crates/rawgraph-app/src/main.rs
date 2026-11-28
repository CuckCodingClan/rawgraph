//! RawGraph desktop application (wave 1: interactive loop on built-in test images).
#![cfg_attr(windows, windows_subsystem = "windows")]

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
mod live_thumbs;
mod fine_slider;
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
mod gradient_editor;
mod handles;
mod handles_logic;
mod icons;
mod missing_check;
mod node_ui;
mod prefetch;
mod quick_preview;
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
mod splash;
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

fn display_version() -> String {
    let build: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().expect("numeric Cargo patch version");
    let year: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().expect("numeric Cargo major version");
    let month: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().expect("numeric Cargo minor version");
    format!("{year:02}{month:02}.{build:03}")
}

fn main() {
    #[cfg(windows)]
    if std::env::args_os().nth(1).is_some() {
        // Do not create a console for Explorer launches. Attach before first output.
        unsafe { windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS); }
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--version")) {
        println!("RawGraph {}", display_version());
        return;
    }
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

    let _splash = splash::start(&cli);
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


mod xmp_export;
