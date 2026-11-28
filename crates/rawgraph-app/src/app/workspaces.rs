//! Workspaces in the application: applying a workspace (main layout plus secondary
//! windows), keeping the windows on their displays while displays come and go, the
//! View -> Workspace menu and the dialogs to save and manage workspaces. The decisions
//! (matching, fallback, restore) are plain data in `crate::workspace`.

use super::*;

use crate::windows::Placing;
use crate::workspace::{self, Action, Change, DisplayRef, DisplayTarget, DisplayWatch, Intent, MainLayout, NamedWorkspace, Preset, WindowRole, WorkspaceSpec, WorkspaceStore};

/// Interval of the display poll while a workspace with secondary windows is active.
pub(super) const MONITOR_POLL: Duration = Duration::from_secs(2);

/// Workspace state of the application.
pub(super) struct Workspaces {
    pub store: WorkspaceStore,
    /// The windows that are open and where they want to be (never changed by a fallback).
    pub intent: Intent,
    /// Panel sizes and visibility of the main window, as last measured.
    pub layout: MainLayout,
    /// Notices changes of the display list, the primary display and the display of the main window.
    pub watch: DisplayWatch,
    /// Panels forget their remembered sizes in the next frame (a workspace was applied).
    pub reset_panels: bool,
    pub pending: Vec<WsAction>,
    pub dialog: Option<WsDialog>,
}

/// Requests from the menu, handled once per frame outside the menu closures.
pub(super) enum WsAction {
    Apply(String),
    /// `None` = automatic (the window's usual display slot).
    Display(WindowRole, Option<DisplayTarget>),
    OpenSave,
    OpenManage,
}

pub(super) enum WsDialog {
    Save { name: String, error: Option<String> },
    Manage { rename: Option<(String, String)>, error: Option<String> },
}

impl Workspaces {
    pub fn new(mut store: WorkspaceStore, spec: &WorkspaceSpec) -> Self {
        store.sanitize();
        Self { store, intent: Intent::from_spec(spec), layout: spec.layout.clone().sanitized(), watch: DisplayWatch::default(), reset_panels: false, pending: Vec::new(), dialog: None }
    }
}

/// Adds a window with its usual display unless the spec has it already.
fn add_window(spec: &mut WorkspaceSpec, role: WindowRole) {
    if !spec.has(role) {
        let target = Intent::from_spec(spec).default_target(role);
        spec.windows.push(workspace::WindowSpec { role, target });
    }
}

/// Chooses the workspace to start with: `--workspace`, else the default workspace, else the
/// one of the last session, else what older settings describe. Returns the spec and a note
/// when the requested name does not exist.
pub(super) fn startup_workspace(settings: &Settings, store: &WorkspaceStore, cli: &Cli) -> (WorkspaceSpec, Option<String>) {
    let mut note = None;
    let requested = cli.workspace.as_deref().and_then(|name| {
        let found = store.resolve(name);
        if found.is_none() {
            let known: Vec<&str> = Preset::ALL.iter().map(|p| p.slug()).chain(store.custom.iter().map(|w| w.name.as_str())).collect();
            note = Some(format!("Unknown workspace \"{name}\" (known: {})", known.join(", ")));
        }
        found
    });
    let mut spec = requested
        .or_else(|| store.default.as_deref().and_then(|d| store.resolve(d)))
        .or_else(|| store.session.clone())
        .unwrap_or_else(|| legacy_spec(settings));
    spec.layout = spec.layout.clone().sanitized();
    if cli.clean_feed {
        add_window(&mut spec, WindowRole::CleanFeed);
    }
    if let Some(p) = cli.scopes {
        if p == ScopesPos::Window {
            add_window(&mut spec, WindowRole::Scopes);
        } else {
            spec.windows.retain(|w| w.role != WindowRole::Scopes);
            spec.layout.scopes_pos = p;
        }
    }
    (spec, note)
}

/// The workspace described by settings written before workspaces existed.
fn legacy_spec(settings: &Settings) -> WorkspaceSpec {
    let mut spec = if settings.workspace == Workspace::Dual { Preset::Dual.spec() } else { Preset::Single.spec() };
    spec.layout.scopes_pos = if settings.scopes_pos == ScopesPos::Window { ScopesPos::BottomRight } else { settings.scopes_pos };
    if settings.scopes_pos == ScopesPos::Window {
        add_window(&mut spec, WindowRole::Scopes);
    }
    if settings.clean.open {
        add_window(&mut spec, WindowRole::CleanFeed);
    }
    spec
}

impl App {
    pub(super) fn aux(&self, role: WindowRole) -> &AuxWindow {
        match role {
            WindowRole::Viewer => &self.viewer_win,
            WindowRole::Scopes => &self.scopes_win,
            WindowRole::CleanFeed => &self.clean_win,
        }
    }

    pub(super) fn aux_mut(&mut self, role: WindowRole) -> &mut AuxWindow {
        match role {
            WindowRole::Viewer => &mut self.viewer_win,
            WindowRole::Scopes => &mut self.scopes_win,
            WindowRole::CleanFeed => &mut self.clean_win,
        }
    }

    /// What the workspace is right now: panel sizes plus the open windows and their intended displays.
    pub(super) fn current_spec(&self) -> WorkspaceSpec {
        let mut layout = self.ws.layout.clone();
        if self.scopes_pos != ScopesPos::Window {
            layout.scopes_pos = self.scopes_pos;
        }
        let windows = WindowRole::ALL.into_iter().filter(|r| self.aux(*r).is_open()).filter_map(|r| self.ws.intent.target(r).map(|t| workspace::WindowSpec { role: r, target: t.clone() })).collect();
        WorkspaceSpec { layout: layout.sanitized(), windows }
    }

    /// Takes over a workspace's layout and window list without placing anything (windows are
    /// placed by `reconcile_windows` once the monitor list is known).
    pub(super) fn apply_spec_state(&mut self, spec: &WorkspaceSpec) {
        for role in WindowRole::ALL {
            let wanted = spec.has(role);
            let win = self.aux_mut(role);
            if wanted && !win.is_open() {
                win.open();
            } else if !wanted && win.is_open() {
                win.close();
            }
        }
        // Windows that stay open on the same target are not placed again.
        let mut intent = Intent::from_spec(spec);
        intent.adopt_placement(&self.ws.intent);
        self.ws.intent = intent;
        self.ws.layout = spec.layout.clone().sanitized();
        self.workspace = if spec.has(WindowRole::Viewer) { Workspace::Dual } else { Workspace::Single };
        self.scopes_pos = if spec.has(WindowRole::Scopes) { ScopesPos::Window } else { self.ws.layout.scopes_pos };
        // The panels take the sizes of the workspace (their remembered sizes are dropped).
        self.ws.reset_panels = true;
        self.editor.request_frame_all();
    }

    pub(super) fn apply_spec(&mut self, spec: &WorkspaceSpec) {
        self.apply_spec_state(spec);
        self.reconcile_windows();
    }

    /// Applies a preset or saved workspace by key (slug or name).
    pub(super) fn apply_key(&mut self, key: &str) {
        match self.ws.store.resolve(key) {
            Some(spec) => {
                self.apply_spec(&spec);
                let name = Preset::from_key(key).map_or_else(|| key.trim().to_owned(), |p| p.name().to_owned());
                self.status.info(format!("Workspace: {name}"));
            }
            None => self.status.error(format!("There is no workspace \"{key}\"")),
        }
    }

    /// The display of the main window; unknown (some compositors) falls back to the primary display.
    fn main_monitor(&self) -> Option<usize> {
        self.cached_main_monitor.or_else(|| self.monitors.iter().position(|m| m.primary))
    }

    /// Places every window whose display assignment changed (see `Intent::reconcile`).
    pub(super) fn reconcile_windows(&mut self) {
        let main = self.main_monitor();
        let actions = self.ws.intent.reconcile(&self.monitors, main);
        if actions.is_empty() {
            return;
        }
        // Where a window goes when its display is missing: the primary display; without one
        // (Wayland reports none) the display of the main window; else the first one.
        let primary = self.monitors.iter().position(|m| m.primary).or(main).unwrap_or(0);
        let monitors = std::mem::take(&mut self.monitors);
        let mut notes = Vec::new();
        for a in &actions {
            if !self.aux(a.role).is_open() {
                continue;
            }
            if a.change == Change::Refreshed {
                // The own display changed its place or resolution: a fullscreen window follows it,
                // a normal window stays where it is.
                if let Some(index) = a.placement.monitor() {
                    if self.aux(a.role).state.fullscreen {
                        let placing = self.placing_for(a, primary);
                        self.aux_mut(a.role).place(&monitors, placing);
                    } else {
                        self.aux_mut(a.role).retarget_monitor(&monitors, index);
                    }
                }
                continue;
            }
            let placing = self.placing_for(a, primary);
            self.aux_mut(a.role).place(&monitors, placing);
            notes.extend(workspace::describe(a));
        }
        self.monitors = monitors;
        if !notes.is_empty() {
            self.status.info(notes.join("; "));
        }
    }

    fn placing_for(&self, a: &Action, primary: usize) -> Placing {
        let (fullscreen, own, spare) = match a.role {
            WindowRole::Viewer => (true, 0.9, 0.8),
            WindowRole::CleanFeed => (true, 0.6, 0.6),
            WindowRole::Scopes => (false, 0.9, 0.5),
        };
        match a.placement.monitor() {
            Some(monitor) => Placing { monitor, fullscreen, size_fraction: own, fallback: false, cascade: 0 },
            None => {
                // Windows on the primary display step diagonally, in the order Viewer, Scopes, Clean feed.
                let before = WindowRole::ALL.iter().take_while(|r| **r != a.role).filter(|r| self.ws.intent.in_fallback(**r) && self.aux(**r).is_open()).count();
                Placing { monitor: primary, fullscreen, size_fraction: spare, fallback: true, cascade: before as u32 + 1 }
            }
        }
    }

    /// Looks at the displays: right away on the first frame, then every 2 seconds while a workspace has
    /// secondary windows (otherwise every second, only when the interface is updated anyway). The
    /// windows are placed again when the list of displays or the primary display changes, or when
    /// the main window has settled on another display.
    pub(super) fn poll_displays(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        let has_windows = !self.ws.intent.windows.is_empty();
        let interval = if has_windows { MONITOR_POLL } else { Duration::from_secs(1) };
        let due = self.monitors_at.is_none_or(|t| t.elapsed() >= interval);
        if due {
            self.monitors_at = Some(Instant::now());
            self.monitors = windows::enumerate_monitors(frame);
            let main = windows::current_monitor_index(frame, &self.monitors);
            if self.ws.watch.observe(&self.monitors, main, Instant::now()) {
                self.cached_main_monitor = self.ws.watch.main_index(&self.monitors);
                self.reconcile_windows();
            }
        }
        if has_windows {
            // egui sleeps without input; wake up to look at the displays again.
            ctx.request_repaint_after(MONITOR_POLL);
        }
    }

    /// Drops the remembered size of a panel when a workspace was just applied, so that it takes
    /// the size of that workspace. The panel keeps its id, so switching workspaces does not leave
    /// a new set of panel states behind in egui's memory every time.
    pub(super) fn reset_panel(&self, ui: &egui::Ui, name: &'static str, epoch: u32) {
        if self.ws.reset_panels {
            let id = ui.scope_id().with((name, epoch));
            ui.ctx().data_mut(|d| d.remove::<egui::containers::panel::PanelState>(id));
        }
    }

    pub(super) fn open_role(&mut self, role: WindowRole) {
        if self.aux(role).is_open() && self.ws.intent.has(role) {
            return;
        }
        let target = self.ws.store.preferred_target(role).cloned().unwrap_or_else(|| self.ws.intent.default_target(role));
        self.ws.intent.set(role, target);
        self.aux_mut(role).open();
        match role {
            WindowRole::Viewer => self.workspace = Workspace::Dual,
            WindowRole::Scopes => self.scopes_pos = ScopesPos::Window,
            WindowRole::CleanFeed => {}
        }
        self.reconcile_windows();
    }

    pub(super) fn close_role(&mut self, role: WindowRole) {
        self.ws.intent.remove(role);
        self.aux_mut(role).close();
        match role {
            WindowRole::Viewer => self.workspace = Workspace::Single,
            WindowRole::Scopes => {
                if self.scopes_pos == ScopesPos::Window {
                    self.scopes_pos = self.ws.layout.scopes_pos;
                }
            }
            WindowRole::CleanFeed => {}
        }
    }

    pub(super) fn toggle_role(&mut self, role: WindowRole) {
        if self.aux(role).is_open() {
            self.close_role(role);
        } else {
            self.open_role(role);
        }
    }

    /// Closing a window with Escape/F11 or its close button removes it from the workspace.
    pub(super) fn sync_closed_windows(&mut self) {
        for role in WindowRole::ALL {
            if self.ws.intent.has(role) && !self.aux(role).is_open() {
                self.close_role(role);
            }
        }
    }

    pub(super) fn set_workspace(&mut self, w: Workspace) {
        match w {
            Workspace::Dual => self.open_role(WindowRole::Viewer),
            Workspace::Single => self.close_role(WindowRole::Viewer),
        }
    }

    pub(super) fn set_scopes_pos(&mut self, p: ScopesPos) {
        if p == ScopesPos::Window {
            self.open_role(WindowRole::Scopes);
        } else {
            self.ws.layout.scopes_pos = p;
            self.close_role(WindowRole::Scopes);
            self.scopes_pos = p;
        }
    }

    /// Chooses the display of a window (remembered for its next opening, applied now if it is open).
    pub(super) fn set_role_display(&mut self, role: WindowRole, target: Option<DisplayTarget>) {
        self.ws.store.prefer(role, target.clone());
        if self.aux(role).is_open() {
            let t = target.unwrap_or_else(|| {
                let mut other = self.ws.intent.clone();
                other.remove(role);
                other.default_target(role)
            });
            self.ws.intent.set(role, t);
            self.reconcile_windows();
        }
    }

    pub(super) fn process_ws_actions(&mut self) {
        for action in std::mem::take(&mut self.ws.pending) {
            match action {
                WsAction::Apply(key) => self.apply_key(&key),
                WsAction::Display(role, target) => self.set_role_display(role, target),
                WsAction::OpenSave => self.ws.dialog = Some(WsDialog::Save { name: String::new(), error: None }),
                WsAction::OpenManage => self.ws.dialog = Some(WsDialog::Manage { rename: None, error: None }),
            }
        }
    }

    /// Windows that sit on the primary display because their display is not connected.
    pub(super) fn display_note(&self) -> Option<String> {
        let names: Vec<&str> = WindowRole::ALL.into_iter().filter(|r| self.aux(*r).is_open() && self.ws.intent.in_fallback(*r)).map(WindowRole::label).collect();
        (!names.is_empty()).then(|| format!("{} on the primary display (display missing)", names.join(", ")))
    }

    // ----------------------------------------------------------------- menu

    pub(super) fn workspace_menu(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let current = self.current_spec();
        ui.label(RichText::new("Workspace").weak());
        for p in Preset::ALL {
            let selected = p.spec().same_windows(&current);
            let label = format!("{} ({})", p.name(), ctx.format_shortcut(&preset_shortcut(p)));
            let default = self.ws.store.is_default(p.slug());
            let mut r = ui.radio(selected, label).on_hover_text(p.description());
            if default {
                r = r.on_hover_text("Default workspace at start");
            }
            if r.clicked() {
                self.ws.pending.push(WsAction::Apply(p.slug().to_owned()));
                ui.close();
            }
        }
        let custom: Vec<NamedWorkspace> = self.ws.store.custom.clone();
        if !custom.is_empty() {
            ui.separator();
            for w in &custom {
                let label = if self.ws.store.is_default(&w.name) { format!("{} (default)", w.name) } else { w.name.clone() };
                if ui.radio(w.spec.same_windows(&current), label).clicked() {
                    self.ws.pending.push(WsAction::Apply(w.name.clone()));
                    ui.close();
                }
            }
        }
        ui.separator();
        if ui.button("Save workspace as...").clicked() {
            self.ws.pending.push(WsAction::OpenSave);
            ui.close();
        }
        if ui.button("Manage workspaces...").clicked() {
            self.ws.pending.push(WsAction::OpenManage);
            ui.close();
        }
        ui.menu_button("Panels", |ui| {
            let l = &mut self.ws.layout;
            ui.checkbox(&mut l.show_filmstrip, "Filmstrip");
            ui.checkbox(&mut l.show_inspector, "Inspector");
            ui.add_enabled(self.workspace == Workspace::Single, egui::Checkbox::new(&mut l.show_graph, "Node graph")).on_disabled_hover_text("The dual workspace always shows the node graph");
        });
        let open_roles: Vec<WindowRole> = WindowRole::ALL.into_iter().filter(|r| self.aux(*r).is_open()).collect();
        if !open_roles.is_empty() {
            ui.menu_button("Window displays", |ui| {
                for role in open_roles {
                    self.display_menu(ui, role);
                }
            });
        }
    }

    /// The displays a window can be sent to: automatic, or one of the connected displays.
    pub(super) fn display_menu(&mut self, ui: &mut egui::Ui, role: WindowRole) {
        ui.menu_button(role.label(), |ui| {
            let target = self.ws.intent.target(role).cloned().or_else(|| self.ws.store.preferred_target(role).cloned());
            let auto = matches!(target, None | Some(DisplayTarget::Slot(_)));
            let auto_label = match &target {
                Some(DisplayTarget::Slot(n)) => format!("Automatic (display {n})"),
                _ => "Automatic".to_owned(),
            };
            if ui.radio(auto, auto_label).on_hover_text("Display 1 is the display of the main window, display 2 the next one to the right (or left), and so on.").clicked() {
                self.ws.pending.push(WsAction::Display(role, None));
                ui.close();
            }
            for (index, monitor) in self.monitors.iter().enumerate() {
                let d = DisplayRef::of(monitor);
                let label = format!("{}: {}{}", index + 1, d.label(), if monitor.primary { " - primary" } else { "" });
                if ui.radio(target.as_ref() == Some(&DisplayTarget::Display(d.clone())), label).clicked() {
                    self.ws.pending.push(WsAction::Display(role, Some(DisplayTarget::Display(d))));
                    ui.close();
                }
            }
            if self.monitors.is_empty() {
                ui.label("No displays available");
            }
        });
    }

    // -------------------------------------------------------------- dialogs

    pub(super) fn workspace_windows(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.ws.dialog.take() else { return };
        self.ws.dialog = match dialog {
            WsDialog::Save { name, error } => self.save_dialog(ctx, name, error),
            WsDialog::Manage { rename, error } => self.manage_dialog(ctx, rename, error),
        };
    }

    fn save_dialog(&mut self, ctx: &egui::Context, mut name: String, mut error: Option<String>) -> Option<WsDialog> {
        let mut keep = true;
        let mut save = false;
        egui::Window::new("Save workspace as").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label("Saves the panel sizes, the open windows and their displays.");
            let edit = ui.add(egui::TextEdit::singleline(&mut name).hint_text("Name").desired_width(260.0));
            edit.request_focus();
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                save = true;
            }
            if let Some(e) = &error {
                ui.label(RichText::new(e).color(egui::Color32::from_rgb(255, 150, 120)));
            }
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            keep = false;
        }
        if save {
            let spec = self.current_spec();
            match self.ws.store.save_as(&name, spec) {
                Ok(saved) => {
                    self.status.info(format!("Saved workspace \"{saved}\""));
                    return None;
                }
                Err(e) => error = Some(e),
            }
        }
        keep.then_some(WsDialog::Save { name, error })
    }

    fn manage_dialog(&mut self, ctx: &egui::Context, mut rename: Option<(String, String)>, mut error: Option<String>) -> Option<WsDialog> {
        let mut keep = true;
        let mut open = true;
        let mut delete = None;
        let mut new_default: Option<Option<String>> = None;
        let mut do_rename = None;
        let custom = self.ws.store.custom.clone();
        egui::Window::new("Workspaces").open(&mut open).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            let none_default = self.ws.store.default.is_none();
            if ui.radio(none_default, "Start with the workspace of the last session").clicked() {
                new_default = Some(None);
            }
            ui.separator();
            for p in Preset::ALL {
                if ui.radio(self.ws.store.is_default(p.slug()), format!("{} (built-in)", p.name())).on_hover_text("Use at start").clicked() {
                    new_default = Some(Some(p.slug().to_owned()));
                }
            }
            if !custom.is_empty() {
                ui.separator();
            }
            for w in &custom {
                ui.horizontal(|ui| {
                    if ui.radio(self.ws.store.is_default(&w.name), "").on_hover_text("Use at start").clicked() {
                        new_default = Some(Some(w.name.clone()));
                    }
                    match &mut rename {
                        Some((old, new)) if old.eq_ignore_ascii_case(&w.name) => {
                            let edit = ui.add(egui::TextEdit::singleline(new).desired_width(180.0));
                            if (edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) || ui.button("OK").clicked() {
                                do_rename = Some((old.clone(), new.clone()));
                            }
                            if ui.button("Cancel").clicked() {
                                do_rename = Some((String::new(), String::new()));
                            }
                        }
                        _ => {
                            ui.label(&w.name);
                            let windows: Vec<&str> = w.spec.windows.iter().map(|s| s.role.label()).collect();
                            ui.label(RichText::new(if windows.is_empty() { "main window".to_owned() } else { windows.join(", ") }).weak().small());
                            if ui.small_button("Rename").clicked() {
                                rename = Some((w.name.clone(), w.name.clone()));
                            }
                            if ui.small_button("Delete").clicked() {
                                delete = Some(w.name.clone());
                            }
                        }
                    }
                });
            }
            if custom.is_empty() {
                ui.label(RichText::new("No saved workspaces yet. Use Save workspace as... in the View menu.").weak());
            }
            if let Some(e) = &error {
                ui.label(RichText::new(e).color(egui::Color32::from_rgb(255, 150, 120)));
            }
            if ui.button("Close").clicked() {
                keep = false;
            }
        });
        if let Some((old, new)) = do_rename {
            if old.is_empty() {
                rename = None;
            } else {
                match self.ws.store.rename(&old, &new) {
                    Ok(_) => {
                        rename = None;
                        error = None;
                    }
                    Err(e) => error = Some(e),
                }
            }
        }
        if let Some(name) = delete {
            self.ws.store.delete(&name);
            rename = None;
        }
        if let Some(d) = new_default {
            if let Err(e) = self.ws.store.set_default(d.as_deref()) {
                error = Some(e);
            }
        }
        (keep && open).then_some(WsDialog::Manage { rename, error })
    }
}

pub(super) fn preset_shortcut(p: Preset) -> KeyboardShortcut {
    let key = match p.shortcut_digit() {
        1 => Key::Num1,
        2 => Key::Num2,
        3 => Key::Num3,
        4 => Key::Num4,
        _ => Key::Num5,
    };
    sc(Modifiers::COMMAND | Modifiers::ALT, key)
}
