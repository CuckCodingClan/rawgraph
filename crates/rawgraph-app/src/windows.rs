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
}

impl MonitorInfo {
    /// The OS name of the monitor (the part of `id` before the first `|`).
    pub fn name(&self) -> &str {
        self.id.split('|').next().unwrap_or(&self.id)
    }

    /// Top-left and size in logical points of this monitor.
    pub fn logical(&self) -> (Pos2, Vec2) {
        let s = self.scale.max(0.5) as f32;
        (
            Pos2::new(self.pos[0] as f32 / s, self.pos[1] as f32 / s),
            Vec2::new(self.size[0] as f32 / s, self.size[1] as f32 / s),
        )
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

#[derive(Default)]
pub struct AuxWindow {
    pub state: AuxState,
    /// Placement applied to the next created viewport.
    initial: Option<(Pos2, Vec2)>,
    /// True once the OS window exists (placement commands only make sense then).
    created: bool,
    /// The wanted display is missing; the window is a normal window on the primary display.
    pub fallback: bool,
    /// Move the existing OS window after the next `show` (after its mode changed).
    pending_apply: bool,
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
    pub fn initial_for_test(&self) -> Option<(Pos2, Vec2)> {
        self.initial
    }

    /// Marks the window open. Its place comes from `place`, called before it is first shown.
    pub fn open(&mut self) {
        self.state.open = true;
        self.created = false;
        self.pending_apply = false;
    }

    pub fn close(&mut self) {
        self.state.open = false;
        self.created = false;
        self.pending_apply = false;
    }

    /// Index of the monitor this window was placed on.
    pub fn monitor_index(&self, monitors: &[MonitorInfo]) -> Option<usize> {
        if !self.state.open {
            return None;
        }
        self.state.monitor.as_deref().and_then(|id| monitors.iter().position(|m| m.id == id))
    }

    /// Puts the window on a display. A window that is already shown moves there with the next frame
    /// without being recreated, so its contents and state stay as they are.
    pub fn place(&mut self, monitors: &[MonitorInfo], placing: Placing) {
        let Some(m) = monitors.get(placing.monitor) else { return };
        self.fallback = placing.fallback;
        self.state.fullscreen = placing.fullscreen && !placing.fallback;
        self.state.monitor = Some(m.id.clone());
        let (pos, size) = m.logical();
        let inner = size * placing.size_fraction;
        let mut pos = pos + (size - inner) / 2.0;
        let mut inner = inner;
        if placing.fallback && placing.cascade > 0 {
            pos += Vec2::splat(56.0 * placing.cascade as f32);
            inner = (inner * 0.85).max(Vec2::new(320.0, 200.0));
        }
        self.initial = Some((pos, inner));
        self.pending_apply = self.state.open && self.created;
    }

    fn apply_placement(&mut self, ctx: &egui::Context, id: ViewportId, monitors: &[MonitorInfo]) {
        if self.state.fullscreen {
            // SetMonitor selects the monitor explicitly for borderless fullscreen;
            // moving a fullscreen window then using Fullscreen(true) is racy.
            if let Some(index) = self.monitor_index(monitors) {
                ctx.send_viewport_cmd_to(id, egui::ViewportCommand::SetMonitor(index));
            }
        } else if let Some((pos, size)) = self.initial {
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::OuterPosition(pos));
            ctx.send_viewport_cmd_to(id, egui::ViewportCommand::InnerSize(size));
        }
    }

    pub fn toggle_fullscreen(&mut self, ctx: &egui::Context, id: ViewportId) {
        self.state.fullscreen = !self.state.fullscreen;
        ctx.send_viewport_cmd_to(id, egui::ViewportCommand::Fullscreen(self.state.fullscreen));
    }

    fn builder(&self, title: &str, decorations: bool, monitors: &[MonitorInfo]) -> ViewportBuilder {
        let mut b = ViewportBuilder::default()
            .with_title(title)
            .with_decorations(decorations && !self.state.fullscreen)
            .with_min_inner_size([320.0, 200.0]);
        if self.state.fullscreen {
            // Geometry changes become viewport commands on monitor switches and
            // can resize/reposition an already-fullscreen window. Let winit use
            // the selected monitor's physical bounds instead.
            b = b.with_fullscreen(true);
            // egui-winit resolves this at creation to Borderless(Some(monitor));
            // Windows winit applies it after CreateWindowEx, not during WM_CREATE.
            if let Some(index) = self.monitor_index(monitors) { b = b.with_monitor(index); }
        } else {
            b = b.with_inner_size([960.0, 600.0]);
            if let Some((pos, size)) = self.initial {
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
        if self.pending_apply && self.created {
            // After the mode switch of this frame (fullscreen on/off) so the new position wins.
            self.pending_apply = false;
            self.apply_placement(ctx, id, monitors);
        }
        self.created = true;
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

/// Lists the monitors known to the OS window of the main viewport.
pub fn enumerate_monitors(frame: &eframe::Frame) -> Vec<MonitorInfo> {
    let Some(window) = frame.winit_window() else { return Vec::new() };
    let primary = window.primary_monitor();
    window
        .available_monitors()
        .map(|m| {
            let pos = m.position();
            let size = m.size();
            let name = m.name().unwrap_or_else(|| "monitor".to_owned());
            MonitorInfo {
                id: format!("{name}|{}x{}@{},{}", size.width, size.height, pos.x, pos.y),
                pos: [pos.x, pos.y],
                size: [size.width, size.height],
                scale: m.scale_factor(),
                primary: primary.as_ref() == Some(&m),
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

    fn mon(id: &str, x: i32, primary: bool) -> MonitorInfo {
        MonitorInfo { id: id.to_owned(), pos: [x, 0], size: [2560, 1440], scale: 1.25, primary }
    }

    fn on(monitor: usize, fullscreen: bool, size_fraction: f32) -> Placing {
        Placing { monitor, fullscreen, size_fraction, fallback: false, cascade: 0 }
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
        let mut w = AuxWindow::from_state(AuxState { open: false, fullscreen: true, monitor: Some("a".into()) });
        w.open();
        w.place(&ms, on(0, true, 1.0));
        let mut builder = w.builder("feed", false, &ms);
        w.place(&ms, on(1, true, 1.0));
        let (commands, recreate) = builder.patch(w.builder("feed", false, &ms));
        assert!(!recreate);
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::SetMonitor(1))));
        assert!(!commands.iter().any(|c| matches!(c, egui::ViewportCommand::OuterPosition(_) | egui::ViewportCommand::InnerSize(_))));
    }

    #[test]
    fn placement_is_inside_the_monitor() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::default();
        w.open();
        w.place(&ms, on(1, false, 0.9));
        let (pos, size) = w.initial.unwrap();
        let (mpos, msize) = ms[1].logical();
        assert!(pos.x >= mpos.x && pos.x + size.x <= mpos.x + msize.x + 0.01);
        assert!(pos.y >= mpos.y && pos.y + size.y <= mpos.y + msize.y + 0.01);
        assert_eq!(w.state.monitor.as_deref(), Some("b"));
        assert!(!w.state.fullscreen && !w.fallback);
    }

    #[test]
    fn fallback_windows_are_movable_and_cascaded_on_the_primary_display() {
        let ms = [mon("a", 0, true)];
        let mut viewer = AuxWindow::from_state(AuxState { open: false, fullscreen: true, monitor: None });
        viewer.open();
        viewer.place(&ms, Placing { monitor: 0, fullscreen: true, size_fraction: 0.9, fallback: true, cascade: 1 });
        assert!(viewer.fallback);
        assert!(!viewer.state.fullscreen, "a fullscreen viewer must not cover the editor on the only display");
        let mut clean = AuxWindow::default();
        clean.open();
        clean.place(&ms, Placing { monitor: 0, fullscreen: true, size_fraction: 0.6, fallback: true, cascade: 2 });
        let (p1, _) = viewer.initial.unwrap();
        let (p2, _) = clean.initial.unwrap();
        assert!((p1 - p2).length() > 50.0, "cascaded: {p1:?} vs {p2:?}");
        let b = viewer.builder("viewer", true, &ms);
        assert_eq!(b.fullscreen, None);
        assert_eq!(b.decorations, Some(true));
        assert!(b.position.is_some());
    }

    #[test]
    fn a_shown_window_moves_without_being_recreated() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::default();
        w.open();
        w.place(&ms, on(1, true, 1.0));
        w.created = true; // as if shown once
        w.place(&ms, Placing { monitor: 0, fullscreen: true, size_fraction: 0.9, fallback: true, cascade: 1 });
        assert!(w.pending_apply && w.is_open(), "the move is applied after the next show");
        w.close();
        assert!(!w.pending_apply);
    }
}
