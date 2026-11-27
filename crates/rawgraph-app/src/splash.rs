//! A small native Windows splash can paint while eframe initializes the GPU.
use crate::cli::Cli;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    #[cfg_attr(not(windows), allow(dead_code))]
    InitializingGpu,
    OpeningProject,
    PreparingEditor,
}

impl Stage {
    #[cfg_attr(not(windows), allow(dead_code))]
    fn text(self) -> &'static str {
        match self {
            Self::InitializingGpu => "Initializing GPU...",
            Self::OpeningProject => "Opening project...",
            Self::PreparingEditor => "Preparing editor...",
        }
    }
}

fn enabled(cli: &Cli) -> bool {
    !cli.help && !cli.capture() && !cli.bench && !cli.bench_tools
        && cli.run_export_node.is_none() && !cli.simulate_crash && cli.export.is_none()
}

#[cfg(windows)]
mod native {
    use super::Stage;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    static WINDOW: AtomicIsize = AtomicIsize::new(0);
    const UPDATE: u32 = WM_APP + 1;
    const CLOSE: u32 = WM_APP + 2;
    fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }
    fn title(stage: Stage) -> Vec<u16> {
        wide(&format!("\n\nRawGraph\n\n{}\n\n{}", crate::display_version(), stage.text()))
    }
    pub fn start() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || unsafe {
            let width = 460;
            let height = 240;
            let hwnd = CreateWindowExW(0, wide("STATIC").as_ptr(), title(Stage::InitializingGpu).as_ptr(),
                WS_POPUP | WS_VISIBLE | 1, (GetSystemMetrics(SM_CXSCREEN)-width)/2,
                (GetSystemMetrics(SM_CYSCREEN)-height)/2, width, height,
                std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
            let _ = tx.send(hwnd as isize);
            if hwnd.is_null() { return; }
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                if msg.message == CLOSE { break; }
                if msg.message == UPDATE {
                    let stage = if msg.wParam == 1 { Stage::OpeningProject } else { Stage::PreparingEditor };
                    SetWindowTextW(hwnd, title(stage).as_ptr());
                } else {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            DestroyWindow(hwnd);
        });
        if let Ok(hwnd) = rx.recv() { WINDOW.store(hwnd, Ordering::SeqCst); }
    }
    pub fn stage(stage: Stage) {
        let hwnd = WINDOW.load(Ordering::SeqCst);
        if hwnd != 0 {
            let value = if stage == Stage::OpeningProject { 1 } else { 2 };
            unsafe { PostMessageW(hwnd as _, UPDATE, value, 0); }
        }
    }
    pub fn finish() {
        let hwnd = WINDOW.swap(0, Ordering::SeqCst);
        if hwnd != 0 { unsafe { PostMessageW(hwnd as _, CLOSE, 0, 0); } }
    }
}

pub struct Splash;
impl Drop for Splash { fn drop(&mut self) { finish(); } }
pub fn start(cli: &Cli) -> Splash {
    if enabled(cli) {
        #[cfg(windows)]
        native::start();
    }
    Splash
}
pub fn stage(stage: Stage) {
    #[cfg(windows)]
    native::stage(stage);
    #[cfg(not(windows))]
    let _ = stage;
}
pub fn finish() {
    #[cfg(windows)]
    native::finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modes_skip_splash() {
        let mut cli = Cli::default();
        assert!(enabled(&cli));
        cli.bench = true; assert!(!enabled(&cli)); cli.bench = false;
        cli.bench_tools = true; assert!(!enabled(&cli)); cli.bench_tools = false;
        cli.screenshot = Some("test.png".into()); assert!(!enabled(&cli)); cli.screenshot = None;
        cli.dump_frame = Some("test.png".into()); assert!(!enabled(&cli)); cli.dump_frame = None;
        cli.run_export_node = Some(0); assert!(!enabled(&cli)); cli.run_export_node = None;
        cli.simulate_crash = true; assert!(!enabled(&cli));
    }
    #[test]
    fn stages_have_truthful_labels() {
        assert_eq!(Stage::InitializingGpu.text(), "Initializing GPU...");
        assert_eq!(Stage::OpeningProject.text(), "Opening project...");
        assert_eq!(Stage::PreparingEditor.text(), "Preparing editor...");
    }
}
