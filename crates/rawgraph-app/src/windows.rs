//! Secondary OS windows (egui viewports): clean feed, scopes placeholder and
//! the detached viewer of the dual workspace. Monitor selection is plain data
//! so the "monitor vanished" fallback can be unit-tested.

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
    /// Top-left and size in logical points of this monitor.
    pub fn logical(&self) -> (Pos2, Vec2) {
        let s = self.scale.max(0.5) as f32;
        (
            Pos2::new(self.pos[0] as f32 / s, self.pos[1] as f32 / s),
            Vec2::new(self.size[0] as f32 / s, self.size[1] as f32 / s),
        )
    }
}

/// Picks the monitor a window should use. The remembered monitor wins if it
/// still exists and is not in `avoid`; otherwise the first free monitor
/// (primary first); if every monitor is taken, the primary one (the caller
/// then cascades the window).
pub fn choose_monitor(monitors: &[MonitorInfo], remembered: Option<&str>, avoid: &[usize]) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }
    if let Some(i) = remembered.and_then(|id| monitors.iter().position(|m| m.id == id)) {
        if !avoid.contains(&i) {
            return Some(i);
        }
    }
    let primary = monitors.iter().position(|m| m.primary).unwrap_or(0);
    let order = std::iter::once(primary).chain((0..monitors.len()).filter(|i| *i != primary));
    for i in order {
        if !avoid.contains(&i) {
            return Some(i);
        }
    }
    Some(primary)
}

/// The monitor after `current`, wrapping around.
#[cfg(test)]
pub fn next_monitor(monitors: &[MonitorInfo], current: Option<usize>) -> Option<usize> {
    if monitors.is_empty() {
        return None;
    }
    Some(current.map_or(0, |c| (c + 1) % monitors.len()))
}

/// Index of the monitor with this id, falling back to the primary one when it
/// was disconnected.
pub fn resolve_monitor(monitors: &[MonitorInfo], id: Option<&str>) -> Option<usize> {
    id.and_then(|id| monitors.iter().position(|m| m.id == id))
        .or_else(|| monitors.iter().position(|m| m.primary))
        .or(if monitors.is_empty() { None } else { Some(0) })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuxState {
    pub open: bool,
    pub fullscreen: bool,
    /// Monitor id this window was last placed on.
    pub monitor: Option<String>,
}

#[derive(Default)]
pub struct AuxWindow {
    pub state: AuxState,
    /// Placement applied to the next created viewport.
    initial: Option<(Pos2, Vec2)>,
    /// True once the OS window exists (placement commands only make sense then).
    created: bool,
    /// Outer rectangle of the OS window in points, as last reported.
    pub last_rect: Option<egui::Rect>,
}

pub struct AuxOutput {
    pub close: bool,
}

impl AuxWindow {
    pub fn from_state(state: AuxState) -> Self {
        Self { state, initial: None, created: false, last_rect: None }
    }

    pub fn is_open(&self) -> bool {
        self.state.open
    }

    /// Opens the window on a monitor not in `avoid`. If every monitor is taken
    /// it shares one and is cascaded by `cascade` steps so windows never sit
    /// exactly on top of each other.
    pub fn open(&mut self, monitors: &[MonitorInfo], avoid: &[usize], size_fraction: f32, cascade: u32) {
        self.state.open = true;
        self.created = false;
        let idx = if self.state.fullscreen {
            resolve_monitor(monitors, self.state.monitor.as_deref())
        } else {
            choose_monitor(monitors, self.state.monitor.as_deref(), avoid)
        };
        let shared = idx.is_some_and(|i| avoid.contains(&i));
        self.place_on(idx, monitors, size_fraction);
        if shared && !self.state.fullscreen {
            if let Some((pos, size)) = &mut self.initial {
                let step = 56.0 * cascade.max(1) as f32;
                *pos += Vec2::splat(step);
                *size = (*size * 0.85).max(Vec2::new(320.0, 200.0));
            }
        }
    }

    /// Index of the monitor this window was placed on.
    pub fn monitor_index(&self, monitors: &[MonitorInfo]) -> Option<usize> {
        if !self.state.open {
            return None;
        }
        self.state.monitor.as_deref().and_then(|id| monitors.iter().position(|m| m.id == id))
    }

    pub fn close(&mut self) {
        self.state.open = false;
        self.created = false;
    }

    fn place_on(&mut self, idx: Option<usize>, monitors: &[MonitorInfo], size_fraction: f32) {
        self.initial = idx.and_then(|i| monitors.get(i)).map(|m| {
            self.state.monitor = Some(m.id.clone());
            let (pos, size) = m.logical();
            let inner = size * size_fraction;
            (pos + (size - inner) / 2.0, inner)
        });
    }

    /// Choose a display even while closed; a running fullscreen feed moves immediately.
    pub fn select_monitor(&mut self, ctx: &egui::Context, id: ViewportId, monitors: &[MonitorInfo], index: usize) {
        if index >= monitors.len() { return; }
        self.place_on(Some(index), monitors, 1.0);
        if self.state.open && self.created { self.apply_placement(ctx, id, monitors); }
    }

    /// Re-places the window on the primary monitor if its monitor was unplugged.
    pub fn ensure_monitor_exists(&mut self, ctx: &egui::Context, id: ViewportId, monitors: &[MonitorInfo], size_fraction: f32) {
        if !self.state.open || monitors.is_empty() {
            return;
        }
        let exists = self.state.monitor.as_deref().is_some_and(|cur| monitors.iter().any(|m| m.id == cur));
        if !exists && self.created {
            let idx = resolve_monitor(monitors, None);
            self.place_on(idx, monitors, size_fraction);
            self.apply_placement(ctx, id, monitors);
        }
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
        if output_visible.is_some() { self.state.fullscreen = true; }
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

    #[test]
    fn remembered_monitor_is_used_when_present() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        assert_eq!(choose_monitor(&ms, Some("b"), &[]), Some(1));
        assert_eq!(choose_monitor(&ms, Some("b"), &[1]), Some(0), "a taken monitor is not reused");
    }

    #[test]
    fn vanished_monitor_falls_back_to_the_primary_one() {
        let ms = [mon("a", 0, true)];
        assert_eq!(choose_monitor(&ms, Some("gone"), &[]), Some(0));
        assert_eq!(resolve_monitor(&ms, Some("gone")), Some(0));
        let ms = [mon("x", 0, false), mon("a", 2560, true)];
        assert_eq!(resolve_monitor(&ms, Some("gone")), Some(1), "primary, not the first monitor");
    }

    #[test]
    fn secondary_window_prefers_another_monitor_but_survives_with_one() {
        let two = [mon("a", 0, true), mon("b", 2560, false)];
        assert_eq!(choose_monitor(&two, None, &[0]), Some(1));
        let one = [mon("a", 0, true)];
        assert_eq!(choose_monitor(&one, None, &[0]), Some(0));
        assert_eq!(choose_monitor(&[], None, &[]), None);
        assert_eq!(choose_monitor(&two, None, &[0, 1]), Some(0), "all taken: primary");
    }

    #[test]
    fn next_monitor_wraps() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        assert_eq!(next_monitor(&ms, Some(0)), Some(1));
        assert_eq!(next_monitor(&ms, Some(1)), Some(0));
        assert_eq!(next_monitor(&ms, None), Some(0));
    }

    #[test]
    fn fullscreen_uses_explicit_selected_display_even_if_occupied() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::from_state(AuxState { open: false, fullscreen: true, monitor: Some("b".into()) });
        w.open(&ms, &[1], 0.6, 1);
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
        w.open(&ms, &[], 1.0, 0);
        let mut builder = w.builder("feed", false, &ms);
        w.place_on(Some(1), &ms, 1.0);
        let (commands, recreate) = builder.patch(w.builder("feed", false, &ms));
        assert!(!recreate);
        assert!(commands.iter().any(|c| matches!(c, egui::ViewportCommand::SetMonitor(1))));
        assert!(!commands.iter().any(|c| matches!(c, egui::ViewportCommand::OuterPosition(_) | egui::ViewportCommand::InnerSize(_))));
    }

    #[test]
    fn placement_is_inside_the_monitor() {
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut w = AuxWindow::default();
        w.open(&ms, &[0], 0.9, 1);
        let (pos, size) = w.initial.unwrap();
        let (mpos, msize) = ms[1].logical();
        assert!(pos.x >= mpos.x && pos.x + size.x <= mpos.x + msize.x + 0.01);
        assert!(pos.y >= mpos.y && pos.y + size.y <= mpos.y + msize.y + 0.01);
        assert_eq!(w.state.monitor.as_deref(), Some("b"));
    }

    #[test]
    fn two_windows_never_sit_on_top_of_each_other() {
        let ms = [mon("a", 0, true)];
        let mut viewer = AuxWindow::default();
        viewer.open(&ms, &[0], 0.9, 1);
        let mut clean = AuxWindow::default();
        clean.open(&ms, &[0, 0], 0.6, 2);
        let (p1, _) = viewer.initial.unwrap();
        let (p2, _) = clean.initial.unwrap();
        assert!((p1 - p2).length() > 50.0, "cascaded: {p1:?} vs {p2:?}");
        // With a second monitor they separate completely.
        let ms = [mon("a", 0, true), mon("b", 2560, false)];
        let mut v = AuxWindow::default();
        v.open(&ms, &[0], 0.9, 1);
        let mut c = AuxWindow::default();
        c.open(&ms, &[0, v.monitor_index(&ms).unwrap()], 0.6, 1);
        assert_eq!(v.state.monitor, Some("b".to_owned()));
        assert_eq!(c.state.monitor, Some("a".to_owned()), "falls back to the primary monitor, cascaded");
    }
}
