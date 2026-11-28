//! Secondary OS windows (egui viewports): the clean feed, the scopes window and the
//! detached viewer of the dual workspace, plus the monitor list they are placed on.
//! Which display a window wants and how it moves when displays change is plain data
//! in `workspace.rs`; this file turns a decision into viewport commands.

use eframe::egui::{self, Key, Pos2, Vec2, ViewportBuilder, ViewportId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    /// Stable-ish identity: name, size and position. A changed layout gives a
    /// new id, which makes remembered placements fall back to the primary one.
    pub id: String,
    /// Top-left in physical pixels (virtual desktop).
    pub pos: [i32; 2],
    /// Physical pixels.
    pub size: [u32; 2],
    pub scale: f64,
    pub primary: bool,
    /// A stable identity of the physical monitor where the platform provides one (Windows: the
    /// monitor device path). `None` elsewhere.
    pub device: Option<String>,
}

impl MonitorInfo {
    /// The OS name of the monitor (the part of `id` before the first `|`).
    pub fn name(&self) -> &str {
        self.id.split('|').next().unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuxState {
    pub open: bool,
    pub fullscreen: bool,
    /// Monitor id this window was last placed on.
    pub monitor: Option<String>,
}

/// Where and how a window is put on a display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placing {
    /// Index into the monitor list.
    pub monitor: usize,
    /// Cover the whole display (borderless) instead of a movable window.
    pub fullscreen: bool,
    /// Window size relative to the display when not fullscreen.
    pub size_fraction: f32,
    /// The window's own display is missing: it is shown as a normal, movable window on the
    /// primary display, shifted by `cascade` steps so it does not hide the main window.
    pub fallback: bool,
    pub cascade: u32,
}

/// A window rectangle in physical pixels of the virtual desktop. Kept physical so it can be sent
/// with the scale the window has at that moment (see `AuxWindow::commands`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PhysRect {
    pub pos: [f32; 2],
    pub size: [f32; 2],
}

impl PhysRect {
    /// Position and size in points for a window whose pixels per point is `ppp`.
    pub fn in_points(&self, ppp: f32) -> (Pos2, Vec2) {
        let ppp = ppp.max(0.1);
        (Pos2::new(self.pos[0] / ppp, self.pos[1] / ppp), Vec2::new(self.size[0] / ppp, self.size[1] / ppp))
    }
}

/// Frames between a change of the window mode (fullscreen on/off) and the move that follows it:
/// winit restores the old placement asynchronously when a window leaves fullscreen.
const SETTLE_FRAMES: u8 = 2;

#[derive(Default)]
pub struct AuxWindow {
    pub state: AuxState,
    /// Where the window belongs, in physical pixels.
    target: Option<PhysRect>,
    /// Scale factor of the monitor the target lies on (only for the first, approximate placement).
    target_scale: f64,
    /// True once the OS window exists (placement commands only make sense then).
    created: bool,
    /// The wanted display is missing; the window is a normal window on the primary display.
    pub fallback: bool,
    /// Frames left until the window is moved to `target` (0 = nothing to do).
    settle: u8,
    /// The position was sent; the size follows one frame later, with the scale of the display the
    /// window then sits on (a move across displays of different scale changes it).
    resize_pending: bool,
    /// Outer rectangle of the OS window in points, as last reported.
    pub last_rect: Option<egui::Rect>,
}

pub struct AuxOutput {
    pub close: bool,
}

impl AuxWindow {
    pub fn from_state(state: AuxState) -> Self {
        Self { state, ..Self::default() }
    }

    pub fn is_open(&self) -> bool {
        self.state.open
    }

    #[cfg(test)]
    pub fn target_for_test(&self) -> Option<PhysRect> {
        self.target
    }

    /// Marks the window open. Its place comes from `place`, called before it is first shown.
    pub fn open(&mut self) {
        self.state.open = true;
        self.created = false;
        self.settle = 0;
        self.resize_pending = false;
    }

    pub fn close(&mut self) {
        self.state.open = false;
        self.created = false;
        self.settle = 0;
        self.resize_pending = false;
    }

    /// Index of the monitor this window was placed on.
    pub fn monitor_index(&self, monitors: &[MonitorInfo]) -> Option<usize> {
        if !self.state.open {
            return None;
        }
        self.state.monitor.as_deref().and_then(|id| monitors.iter().position(|m| m.id == id))
    }

    /// Puts the window on a display. A window that is already shown moves there a few frames later
    /// without being recreated, so its contents and state stay as they are.
    pub fn place(&mut self, monitors: &[MonitorInfo], placing: Placing) {
        let Some(m) = monitors.get(placing.monitor) else { return };
        self.fallback = placing.fallback;
        self.state.fullscreen = placing.fullscreen && !placing.fallback;
        self.state.monitor = Some(m.id.clone());
        let scale = m.scale.max(0.5) as f32;
        let mon_pos = [m.pos[0] as f32, m.pos[1] as f32];
        let mon_size = [m.size[0] as f32, m.size[1] as f32];
        let mut size = [mon_size[0] * placing.size_fraction, mon_size[1] * placing.size_fraction];
        let mut pos = [mon_pos[0] + (mon_size[0] - size[0]) / 2.0, mon_pos[1] + (mon_size[1] - size[1]) / 2.0];
        if placing.fallback && placing.cascade > 0 {
            let step = 56.0 * scale * placing.cascade as f32;
            pos = [pos[0] + step, pos[1] + step];
            size = [(size[0] * 0.85).max(320.0 * scale), (size[1] * 0.85).max(200.0 * scale)];
        }
        self.target = Some(PhysRect { pos, size });
        self.target_scale = f64::from(scale);
        // Leaving fullscreen needs a few frames before the move; entering it (SetMonitor) does not,
        // so a returning output does not flash on the wrong display.
        self.settle = match (self.state.open && self.created, self.state.fullscreen) {
            (false, _) => 0,
            (true, true) => 1,
            (true, false) => SETTLE_FRAMES,
        };
        self.resize_pending = false;
    }

    /// Just rewrites which monitor the window is remembered on (the display changed its place or
    /// resolution; the window stays where it is).
    pub fn retarget_monitor(&mut self, monitors: &[MonitorInfo], index: usize) {
        if let Some(m) = monitors.get(index) {
            self.state.monitor = Some(m.id.clone());
        }
    }

    /// The commands that bring an existing OS window to its place. `ppp` is the pixels per point of
    /// that window now (the scale of its current display times the zoom): egui-winit multiplies the
    /// points by it, so dividing the physical target by it lands on the right pixel on any display.
    pub fn commands(&self, ppp: f32, monitors: &[MonitorInfo]) -> Vec<egui::ViewportCommand> {
        if self.state.fullscreen {
            // SetMonitor selects the monitor explicitly for borderless fullscreen;
            // moving a fullscreen window then using Fullscreen(true) is racy.
            return self.monitor_index(monitors).map(egui::ViewportCommand::SetMonitor).into_iter().collect();
        }
        match self.target {
            Some(rect) => {
                let (pos, size) = rect.in_points(ppp);
                vec![egui::ViewportCommand::OuterPosition(pos), egui::ViewportCommand::InnerSize(size)]
            }
            None => Vec::new(),
        }
    }

    pub fn toggle_fullscreen(&mut self, ctx: &egui::Context, id: ViewportId) {
        self.state.fullscreen = !self.state.fullscreen;
        ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Fullscreen(self.state.fullscreen));
    }

    fn builder(&self, title: &str, decorations: bool, monitors: &[MonitorInfo]) -> ViewportBuilder {
        // The mode is always stated: egui only sends `Fullscreen(false)` when the builder says
        // `Some(false)` after `Some(true)`; leaving it out would keep a window borderless forever.
        let mut b = ViewportBuilder::default()
            .with_title(title)
            .with_decorations(decorations && !self.state.fullscreen)
            .with_fullscreen(self.state.fullscreen)
            .with_min_inner_size([320.0, 200.0]);
        if self.state.fullscreen {
            // Geometry changes become viewport commands on monitor switches and
            // can resize/reposition an already-fullscreen window. Let winit use
            // the selected monitor's physical bounds instead.
            // egui-winit resolves this at creation to Borderless(Some(monitor));
            // Windows winit applies it after CreateWindowEx, not during WM_CREATE.
            if let Some(index) = self.monitor_index(monitors) { b = b.with_monitor(index); }
        } else if !self.created {
            // Only for the creation, and only approximately (the new window's scale is not known
            // yet); `commands` corrects it right after, with the real scale. An existing window is
            // never moved by its builder: that would use the wrong scale on a mixed-DPI setup.
            b = b.with_inner_size([960.0, 600.0]);
            if let Some(rect) = self.target {
                let (pos, size) = rect.in_points(self.target_scale.max(0.5) as f32);
                b = b.with_position(pos).with_inner_size(size);
            }
        }
        b
    }

    /// Shows the window (immediate viewport) if it is open. `content` paints
    /// into the window's root `Ui`.
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        id: ViewportId,
        title: &str,
        decorations: bool,
        monitors: &[MonitorInfo],
        output_visible: Option<bool>,
        mut content: impl FnMut(&mut egui::Ui),
    ) -> AuxOutput {
        let mut out = AuxOutput { close: false };
        if !self.state.open {
            return out;
        }
        // A window on its own display covers it; one that lost its display is a normal window.
        if output_visible.is_some() { self.state.fullscreen = !self.fallback; }
        let decorations = decorations || self.fallback;
        let mut builder = self.builder(title, decorations, monitors);
        if let Some(visible) = output_visible {
            builder = builder.with_taskbar(false).with_active(false).with_visible(visible);
        }
        let mut fullscreen_toggle = false;
        let mut last_rect = self.last_rect;
        ctx.show_viewport_immediate(id, builder, |ui, _class| {
            content(ui);
            let (close_requested, esc, f11, rect) = ui.input(|i| {
                (i.viewport().close_requested(), i.key_pressed(Key::Escape), i.key_pressed(Key::F11), i.viewport().outer_rect)
            });
            last_rect = rect;
            if close_requested || esc {
                out.close = true;
            }
            if f11 {
                // A chrome-free clean feed must not turn into a windowed output.
                if decorations && output_visible.is_none() {
                    fullscreen_toggle = true;
                } else {
                    out.close = true;
                }
            }
        });
        if !self.created && !self.state.fullscreen && self.target.is_some() {
            // First frame of a new window: correct the approximate creation placement.
            self.settle = SETTLE_FRAMES;
        }
        self.created = true;
        if self.settle == 0 && self.resize_pending {
            // Second step of a move: the window now sits on its target display, so its own
            // pixels per point are the right ones for the size.
            self.resize_pending = false;
            if let (false, Some(rect)) = (self.state.fullscreen, self.target) {
                let ppp = ctx.input_for(id, |i| i.pixels_per_point());
                ctx.send_viewport_cmd_to(id, egui::ViewportCommand::InnerSize(rect.in_points(ppp).1));
            }
        }
        if self.settle > 0 {
            self.settle -= 1;
            if self.settle == 0 {
                let ppp = ctx.input_for(id, |i| i.pixels_per_point());
                if self.state.fullscreen {
                    for command in self.commands(ppp, monitors) {
                        ctx.send_viewport_cmd_to(id, command);
                    }
                } else if let Some(rect) = self.target {
                    ctx.send_viewport_cmd_to(id, egui::ViewportCommand::OuterPosition(rect.in_points(ppp).0));
                    self.resize_pending = true;
                    ctx.request_repaint();
                }
            } else {
                ctx.request_repaint();
            }
        }
        self.last_rect = last_rect;
        if fullscreen_toggle {
            self.toggle_fullscreen(ctx, id);
        }
        if out.close {
            self.close();
        }
        out
    }
}

/// What the platform knows about the physical monitor behind a winit monitor.
#[derive(Clone)]
struct DeviceIdentity {
    /// The OS name winit reports for the monitor (Windows: the GDI name `\\.\DISPLAYn`).
    os_name: String,
    /// The monitor's own name (EDID), if known.
    friendly: String,
    /// A stable identity of the connection (Windows: the monitor device path).
    path: String,
}

/// Windows: the GDI names `\\.\DISPLAYn` that winit reports are renumbered when displays are
/// docked, undocked or the machine restarts, and say nothing about the monitor. The display
/// configuration API maps each of them to the monitor's friendly name and its device path.
#[cfg(windows)]
fn device_identities() -> Vec<DeviceIdentity> {
    use windows_sys::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    fn text(buf: &[u16]) -> String {
        let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    }
    use std::sync::Mutex;
    // The last complete answer: a failed query (it races with hot-plugging) keeps the identities
    // instead of renaming every monitor for one poll.
    static LAST: Mutex<Vec<DeviceIdentity>> = Mutex::new(Vec::new());
    const ERROR_INSUFFICIENT_BUFFER: i32 = 122;
    let mut out = Vec::new();
    let mut complete = false;
    // SAFETY: plain calls of the display configuration API with buffers sized by the API itself;
    // the structures are zero-initialised C structs whose header (type, size, adapter, id) is
    // filled in as the API requires.
    unsafe {
        let mut paths = Vec::new();
        // The configuration can change between sizing and query (hot-plug): retry a few times.
        for _ in 0..5 {
            let (mut path_count, mut mode_count) = (0u32, 0u32);
            if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count) != 0 {
                break;
            }
            let mut found = vec![std::mem::zeroed::<DISPLAYCONFIG_PATH_INFO>(); path_count as usize];
            let mut modes = vec![std::mem::zeroed::<DISPLAYCONFIG_MODE_INFO>(); mode_count as usize];
            let result = QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut path_count, found.as_mut_ptr(), &mut mode_count, modes.as_mut_ptr(), std::ptr::null_mut());
            if result == 0 {
                found.truncate(path_count as usize);
                paths = found;
                complete = true;
                break;
            }
            if result as i32 != ERROR_INSUFFICIENT_BUFFER {
                break;
            }
        }
        for p in &paths {
            let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = std::mem::zeroed();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = p.sourceInfo.adapterId;
            source.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                complete = false;
                continue;
            }
            let mut target: DISPLAYCONFIG_TARGET_DEVICE_NAME = std::mem::zeroed();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = p.targetInfo.adapterId;
            target.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                complete = false;
                continue;
            }
            out.push(DeviceIdentity { os_name: text(&source.viewGdiDeviceName), friendly: text(&target.monitorFriendlyDeviceName), path: text(&target.monitorDevicePath) });
        }
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if complete {
        last.clone_from(&out);
        return out;
    }
    // Incomplete answer: fill the gaps from the last complete one (same GDI name).
    for old in last.iter() {
        if !out.iter().any(|d| d.os_name.eq_ignore_ascii_case(&old.os_name)) {
            out.push(old.clone());
        }
    }
    out
}

#[cfg(not(windows))]
fn device_identities() -> Vec<DeviceIdentity> {
    Vec::new()
}

/// Lists the monitors known to the OS window of the main viewport.
pub fn enumerate_monitors(frame: &eframe::Frame) -> Vec<MonitorInfo> {
    let Some(window) = frame.winit_window() else { return Vec::new() };
    let primary = window.primary_monitor();
    let identities = device_identities();
    window
        .available_monitors()
        .map(|m| {
            let pos = m.position();
            let size = m.size();
            let os_name = m.name().unwrap_or_else(|| "monitor".to_owned());
            let identity = identities.iter().find(|d| d.os_name.eq_ignore_ascii_case(&os_name));
            let name = identity.map(|d| d.friendly.trim()).filter(|f| !f.is_empty()).map_or(os_name, str::to_owned).replace('|', "/");
            MonitorInfo {
                id: format!("{name}|{}x{}@{},{}", size.width, size.height, pos.x, pos.y),
                pos: [pos.x, pos.y],
                size: [size.width, size.height],
                scale: m.scale_factor(),
                primary: primary.as_ref() == Some(&m),
                device: identity.map(|d| d.path.clone()).filter(|p| !p.is_empty()),
            }
        })
        .collect()
}

/// Index of the monitor the main window currently sits on.
pub fn current_monitor_index(frame: &eframe::Frame, monitors: &[MonitorInfo]) -> Option<usize> {
    let window = frame.winit_window()?;
    let m = window.current_monitor()?;
    let pos = m.position();
    monitors.iter().position(|info| info.pos == [pos.x, pos.y] && info.size == [m.size().width, m.size().height])
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::ViewportCommand as Cmd;

    fn mon(id: &str, x: i32, primary: bool) -> MonitorInfo {
        MonitorInfo { id: id.to_owned(), pos: [x, 0], size: [2560, 1440], scale: 1.25, primary, device: None }
    }

    fn on(monitor: usize, fullscreen: bool, size_fraction: f32) -> Placing {
        Placing { monitor, fullscreen, size_fraction, fallback: false, cascade: 0 }
    }

    fn fallback(monitor: usize, cascade: u32) -> Placing {
        Placing { monitor, fullscreen: true, size_fraction: 0.8, fallback: true, cascade }
    }

    /// A window as the application keeps it once it has been shown.
    fn shown(ms: &[MonitorInfo], placing: Placing) -> AuxWindow {
        let mut w = AuxWindow::default();
        w.open();
        w.place(ms, placing);
        w.created = true;
        w
    }

    #[test]
    fn monitor_name_is_the_part_before_the_separator() {
        assert_eq!(mon("DELL U2720Q|2560x1440@0,0", 0, true).name(), "DELL U2720Q");
        assert_eq!(mon("plain", 0, true).name(), "plain");
    }

    #[test]
    fn fullscreen_uses_explicit_selected_display_even_if_occupied() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::from_state(AuxState { open: false, fullscreen: true, monitor: Some("b".into()) });
        w.open();
        w.place(&ms, on(1, true, 0.6));
        assert_eq!(w.monitor_index(&ms), Some(1));
        let builder = w.builder("feed", false, &ms);
        assert_eq!(builder.fullscreen, Some(true));
        assert_eq!(builder.monitor, Some(1));
        assert_eq!(builder.decorations, Some(false));
        assert_eq!(builder.position, None);
        assert_eq!(builder.inner_size, None);
    }

    #[test]
    fn fullscreen_monitor_change_does_not_issue_window_geometry_commands() {
        let mut second = mon("b", -1920, false);
        second.size = [1920, 1080];
        second.scale = 1.0;
        let ms = [mon("a", 0, true), second];
        let mut w = shown(&ms, on(0, true, 1.0));
        let mut builder = w.builder("feed", false, &ms);
        w.place(&ms, on(1, true, 1.0));
        let (commands, recreate) = builder.patch(w.builder("feed", false, &ms));
        assert!(!recreate);
        assert!(commands.iter().any(|c| matches!(c, Cmd::SetMonitor(1))));
        assert!(!commands.iter().any(|c| matches!(c, Cmd::OuterPosition(_) | Cmd::InnerSize(_))));
    }

    #[test]
    fn leaving_and_re_entering_fullscreen_sends_the_commands_in_the_right_order() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = shown(&ms, on(1, true, 1.0));
        let mut builder = w.builder("viewer", false, &ms);
        assert_eq!((builder.fullscreen, builder.monitor), (Some(true), Some(1)));

        // The display vanishes: fullscreen -> a movable window on the primary display.
        w.place(&ms, fallback(0, 1));
        assert!(w.fallback && !w.state.fullscreen);
        let (now, recreate) = builder.patch(w.builder("viewer", true, &ms));
        assert!(!recreate);
        assert!(now.iter().any(|c| matches!(c, Cmd::Fullscreen(false))), "the window must leave fullscreen explicitly: {now:?}");
        assert!(now.iter().any(|c| matches!(c, Cmd::Decorations(true))));
        assert!(!now.iter().any(|c| matches!(c, Cmd::OuterPosition(_) | Cmd::InnerSize(_))), "geometry is sent later, after winit restored the old placement");
        assert_eq!(w.settle, SETTLE_FRAMES);
        let later = w.commands(1.25, &ms);
        assert!(matches!(later.as_slice(), [Cmd::OuterPosition(_), Cmd::InnerSize(_)]), "{later:?}");

        // The display returns: windowed -> fullscreen on it.
        w.place(&ms, on(1, true, 1.0));
        assert!(!w.fallback && w.state.fullscreen);
        let (back, _) = builder.patch(w.builder("viewer", false, &ms));
        assert!(back.iter().any(|c| matches!(c, Cmd::Fullscreen(true))), "{back:?}");
        // The builder alone would put it on the display it is on now; the explicit move follows in
        // the same frame (entering fullscreen needs no settling), so it never flashes elsewhere.
        assert_eq!(w.settle, 1);
        assert_eq!(w.commands(1.25, &ms), vec![Cmd::SetMonitor(1)]);
    }

    #[test]
    fn an_existing_window_is_never_moved_by_its_builder() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::default();
        w.open();
        w.place(&ms, on(1, false, 0.9));
        let created = w.builder("scopes", true, &ms);
        assert!(created.position.is_some() && created.inner_size.is_some(), "the first placement is part of the creation");
        w.created = true;
        w.place(&ms, on(0, false, 0.9));
        let existing = w.builder("scopes", true, &ms);
        assert!(existing.position.is_none() && existing.inner_size.is_none());
    }

    #[test]
    fn placement_is_inside_the_monitor_in_physical_pixels() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::default();
        w.open();
        w.place(&ms, on(1, false, 0.9));
        let r = w.target_for_test().unwrap();
        assert!(r.pos[0] >= 2560.0 && r.pos[0] + r.size[0] <= 2560.0 + 2560.0 + 0.01);
        assert!(r.pos[1] >= 0.0 && r.pos[1] + r.size[1] <= 1440.0 + 0.01);
        assert_eq!(w.state.monitor.as_deref(), Some("b"));
        assert!(!w.state.fullscreen && !w.fallback);
    }

    #[test]
    fn mixed_scale_factors_and_zoom_land_on_the_right_pixel() {
        // Primary at 150 %, the third display at 100 % to the right of it.
        let primary = MonitorInfo { id: "p".into(), pos: [0, 0], size: [3840, 2160], scale: 1.5, primary: true, device: None };
        let third = MonitorInfo { id: "t".into(), pos: [6400, 0], size: [1920, 1080], scale: 1.0, primary: false, device: None };
        let left = MonitorInfo { id: "l".into(), pos: [-2560, 0], size: [2560, 1440], scale: 1.25, primary: false, device: None };
        let ms = [primary, third, left];
        for (target, expect_x) in [(1usize, 6400.0 + 96.0), (2, -2560.0 + 128.0)] {
            let w = shown(&ms, on(target, false, 0.9));
            let rect = w.target_for_test().unwrap();
            assert_eq!(rect.pos[0], expect_x, "physical position on display {target}");
            // The window sits on the 150 % display now, with egui zoom 1.0 and 1.25.
            for ppp in [1.5_f32, 1.5 * 1.25, 1.0, 1.25] {
                let cmds = w.commands(ppp, &ms);
                let (Cmd::OuterPosition(pos), Cmd::InnerSize(size)) = (&cmds[0], &cmds[1]) else { panic!("{cmds:?}") };
                // egui-winit multiplies the points by the window's pixels per point again.
                assert!((pos.x * ppp - rect.pos[0]).abs() < 0.01 && (pos.y * ppp - rect.pos[1]).abs() < 0.01, "ppp {ppp}: {pos:?}");
                assert!((size.x * ppp - rect.size[0]).abs() < 0.01 && (size.y * ppp - rect.size[1]).abs() < 0.01, "ppp {ppp}: {size:?}");
            }
        }
        // The old way (target display's own scale) would have sent x = 6400 * 1.5 = 9600 for the third display.
        let w = shown(&ms, on(1, false, 0.9));
        let (pos, _) = w.target_for_test().unwrap().in_points(1.5);
        assert!((pos.x * 1.5 - 6496.0).abs() < 0.01 && pos.x * 1.5 < 6400.0 + 1920.0);
    }

    #[test]
    fn fallback_windows_are_movable_and_cascaded_on_the_primary_display() {
        let ms = [mon("a", 0, true)];
        let mut viewer = AuxWindow::from_state(AuxState { open: false, fullscreen: true, monitor: None });
        viewer.open();
        viewer.place(&ms, fallback(0, 1));
        assert!(viewer.fallback);
        assert!(!viewer.state.fullscreen, "a fullscreen viewer must not cover the editor on the only display");
        let mut clean = AuxWindow::default();
        clean.open();
        clean.place(&ms, fallback(0, 2));
        let (p1, p2) = (viewer.target_for_test().unwrap().pos, clean.target_for_test().unwrap().pos);
        assert!((p1[0] - p2[0]).abs() > 50.0, "cascaded: {p1:?} vs {p2:?}");
        let b = viewer.builder("viewer", true, &ms);
        assert_eq!(b.fullscreen, Some(false));
        assert_eq!(b.decorations, Some(true));
        assert!(b.position.is_some());
    }

    #[test]
    fn a_shown_window_moves_without_being_recreated() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = shown(&ms, on(1, true, 1.0));
        w.place(&ms, fallback(0, 1));
        assert!(w.settle > 0 && w.is_open(), "the move is applied a few frames later");
        w.close();
        assert_eq!(w.settle, 0);
    }
}
