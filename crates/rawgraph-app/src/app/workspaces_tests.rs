//! Tests of the workspace glue: applying workspaces, display loss and return, persistence,
//! shortcuts. The pure rules (matching, state machine, store) are tested in `crate::workspace`.

use super::*;

use eframe::Storage as _;
use super::workspaces::startup_workspace;
use crate::workspace::{DisplayRef, DisplayTarget, WindowSpec};

fn headless_app(dir: &std::path::Path) -> App {
    let ctx = egui::Context::default();
    App::new(&eframe::CreationContext::_new_kittest(ctx), Cli { demo: true, data_dir: Some(dir.to_owned()), ..Default::default() })
}

fn mon(name: &str, x: i32, primary: bool) -> MonitorInfo {
    MonitorInfo { id: format!("{name}|2560x1440@{x},0"), pos: [x, 0], size: [2560, 1440], scale: 1.0, primary, device: None }
}

#[derive(Default)]
struct Mem(std::collections::HashMap<String, String>);

impl eframe::Storage for Mem {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn open_roles(app: &App) -> Vec<WindowRole> {
    WindowRole::ALL.into_iter().filter(|r| app.aux(*r).is_open()).collect()
}

#[test]
fn presets_open_and_close_the_right_windows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    assert!(open_roles(&app).is_empty() && app.workspace == Workspace::Single);
    app.apply_key("dual-scopes-clean");
    assert_eq!(open_roles(&app), vec![WindowRole::Viewer, WindowRole::Scopes, WindowRole::CleanFeed]);
    assert_eq!((app.workspace, app.scopes_pos), (Workspace::Dual, ScopesPos::Window));
    assert!(app.ws.reset_panels, "panels take the new sizes");
    app.apply_key("clean-feed");
    assert_eq!(open_roles(&app), vec![WindowRole::CleanFeed]);
    assert_eq!((app.workspace, app.scopes_pos), (Workspace::Single, ScopesPos::BottomRight), "the docked scopes come back");
    app.apply_key("Dual");
    assert_eq!(open_roles(&app), vec![WindowRole::Viewer]);
    app.apply_key("no-such-workspace");
    assert_eq!(open_roles(&app), vec![WindowRole::Viewer], "an unknown name changes nothing");
    assert!(app.status.current().is_some_and(|(_, error)| error));
    // Closing a window by hand removes it from the workspace again.
    app.viewer_win.close();
    app.sync_closed_windows();
    assert_eq!((app.workspace, app.ws.intent.has(WindowRole::Viewer)), (Workspace::Single, false));
    app.shutdown();
}

#[test]
fn the_scopes_window_follows_the_primary_viewer() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    let develop = app.session.node_by_index(1).unwrap();
    let generator = app.session.project.graph.add_node(NodeKind::Generator, [0.0; 2]);
    app.session.selected_node = Some(generator);
    app.session.viewed_node = Some(develop);
    app.frames = SETTLE_FRAMES;
    app.apply_key("dual-scopes");
    assert_eq!(app.scopes_pos, ScopesPos::Window, "the docked scopes are hidden for the window");
    let until = Instant::now() + Duration::from_secs(5);
    while App::gallery_members(app.gallery_result.as_ref(), app.primary_target()).is_none() {
        app.refresh_gallery();
        assert!(Instant::now() < until, "gallery worker did not finish");
        std::thread::sleep(Duration::from_millis(2));
    }
    app.submit_if_needed();
    let key = app.last_key.as_ref().unwrap();
    assert_eq!(key.node, develop, "the viewer's node, not the inspector selection");
    assert!(key.scopes.is_some(), "the scopes window asks for scope data");
    // There is one scope panel: leaving the window docks the same panel again.
    let pinned = app.primary_target();
    app.set_scopes_pos(ScopesPos::RightColumn);
    assert!(!app.scopes_win.is_open() && app.scopes_pos == ScopesPos::RightColumn);
    assert_eq!(app.primary_target(), pinned);
    app.submit_if_needed();
    assert!(app.last_key.as_ref().unwrap().scopes.is_some());
    app.shutdown();
}

#[test]
fn windows_fall_back_to_the_primary_display_and_return_without_losing_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    let (a, b) = (mon("DELL", 0, true), mon("LG", 2560, false));
    app.monitors = vec![a.clone(), b.clone()];
    app.cached_main_monitor = Some(0);
    app.apply_key("dual-scopes");
    // Display 2 exists, display 3 does not.
    assert_eq!(app.viewer_win.state.monitor.as_deref(), Some(b.id.as_str()));
    assert!(app.viewer_win.state.fullscreen && !app.viewer_win.fallback);
    assert_eq!(app.scopes_win.state.monitor.as_deref(), Some(a.id.as_str()));
    assert!(app.scopes_win.fallback && !app.scopes_win.state.fullscreen);
    assert_eq!(app.display_note().as_deref(), Some("Scopes on the primary display (display missing)"));
    let before = (app.workspace, app.scopes_pos, app.session.viewed_node);

    // The LG is unplugged.
    app.monitors = vec![a.clone()];
    app.reconcile_windows();
    assert!(app.viewer_win.fallback && !app.viewer_win.state.fullscreen, "windowed on the primary display, not covering the editor");
    assert_eq!(app.viewer_win.state.monitor.as_deref(), Some(a.id.as_str()));
    assert!(app.viewer_win.is_open() && app.scopes_win.is_open(), "nothing was closed");
    assert_ne!(app.viewer_win.target_for_test(), app.scopes_win.target_for_test(), "the two fallback windows are cascaded");
    assert!(app.status.current().is_some_and(|(t, _)| t.contains("moved to the primary display")));
    assert_eq!(app.ws.intent.target(WindowRole::Viewer), Some(&DisplayTarget::Slot(2)), "the intended display is remembered");
    assert_eq!((app.workspace, app.scopes_pos, app.session.viewed_node), before, "the rest of the state is untouched");

    // It is plugged in again, and a third display appears.
    let c = mon("BenQ", 5120, false);
    app.monitors = vec![a.clone(), b.clone(), c.clone()];
    app.reconcile_windows();
    assert!(!app.viewer_win.fallback && app.viewer_win.state.fullscreen);
    assert_eq!(app.viewer_win.state.monitor.as_deref(), Some(b.id.as_str()));
    assert_eq!(app.scopes_win.state.monitor.as_deref(), Some(c.id.as_str()));
    assert!(!app.scopes_win.fallback && app.display_note().is_none());
    app.shutdown();
}

#[test]
fn choosing_a_display_is_remembered_for_the_next_opening() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    let (a, b, c) = (mon("DELL", 0, true), mon("LG", 2560, false), mon("BenQ", 5120, false));
    app.monitors = vec![a, b, c.clone()];
    app.cached_main_monitor = Some(0);
    let target = DisplayTarget::Display(DisplayRef::of(&c));
    app.set_role_display(WindowRole::CleanFeed, Some(target));
    assert!(!app.clean_win.is_open());
    app.run(Command::ToggleClean, &egui::Context::default());
    assert_eq!(app.clean_win.state.monitor.as_deref(), Some(c.id.as_str()));
    app.set_role_display(WindowRole::CleanFeed, None);
    assert_eq!(app.ws.intent.target(WindowRole::CleanFeed), Some(&DisplayTarget::Slot(2)), "automatic = first free display after the main one");
    assert_ne!(app.clean_win.state.monitor.as_deref(), Some(c.id.as_str()));
    app.shutdown();
}

#[test]
fn workspaces_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage = Mem::default();
    let mut app = headless_app(dir.path());
    app.apply_key("dual-scopes-clean");
    app.ws.layout.show_inspector = false;
    app.ws.layout.filmstrip_w = 300.0;
    let spec = app.current_spec();
    app.ws.store.save_as("Wall", spec.clone()).unwrap();
    app.ws.store.set_default(Some("Wall")).unwrap();
    app.apply_key("single");
    eframe::App::save(&mut app, &mut storage);
    app.shutdown();

    let settings: Settings = eframe::get_value(&storage, SETTINGS_KEY).unwrap();
    let store = WorkspaceStore::from_json(&storage.get_string(WORKSPACES_KEY).unwrap());
    assert_eq!(store.custom.len(), 1);
    assert_eq!(store.session.as_ref().map(|s| s.windows.len()), Some(0), "the last session was Single");
    // The default beats the last session.
    let (start, note) = startup_workspace(&settings, &store, &Cli::default());
    assert_eq!((start, note), (spec.clone(), None));
    // --workspace beats the default; clean feed and scopes options add to it.
    let cli = Cli { workspace: Some("dual".into()), clean_feed: true, ..Cli::default() };
    let (start, _) = startup_workspace(&settings, &store, &cli);
    assert_eq!(start.windows.iter().map(|w| w.role).collect::<Vec<_>>(), vec![WindowRole::Viewer, WindowRole::CleanFeed]);
    let cli = Cli { workspace: Some("wall".into()), scopes: Some(ScopesPos::Hidden), ..Cli::default() };
    let (start, _) = startup_workspace(&settings, &store, &cli);
    assert!(!start.has(WindowRole::Scopes) && start.layout.scopes_pos == ScopesPos::Hidden && start.has(WindowRole::Viewer));
    // An unknown name is reported and the normal choice applies.
    let cli = Cli { workspace: Some("nope".into()), ..Cli::default() };
    let (start, note) = startup_workspace(&settings, &store, &cli);
    assert_eq!(start, spec);
    assert!(note.unwrap().contains("nope"));
    // Without a default the last session comes back.
    let mut no_default = store;
    no_default.set_default(None).unwrap();
    let (start, _) = startup_workspace(&settings, &no_default, &Cli::default());
    assert!(start.windows.is_empty());
    assert!(startup_workspace(&Settings::default(), &WorkspaceStore::default(), &Cli::default()).0.windows.is_empty());
    // Settings from before workspaces existed.
    let old = Settings { workspace: Workspace::Dual, scopes_pos: ScopesPos::Window, clean: AuxState { open: true, ..AuxState::default() }, ..Settings::default() };
    let (start, _) = startup_workspace(&old, &WorkspaceStore::default(), &Cli::default());
    assert_eq!(start.windows.iter().map(|w| w.role).collect::<Vec<_>>(), vec![WindowRole::Viewer, WindowRole::Scopes, WindowRole::CleanFeed]);
    let mut old_text = Mem::default();
    old_text.0.insert(SETTINGS_KEY.to_owned(), "(workspace: Dual)".to_owned());
    let loaded: Settings = eframe::get_value(&old_text, SETTINGS_KEY).unwrap();
    assert_eq!(startup_workspace(&loaded, &WorkspaceStore::default(), &Cli::default()).0.windows.len(), 1);
}

#[test]
fn damaged_workspace_text_never_touches_the_other_settings() {
    let dir = tempfile::tempdir().unwrap();
    let mut storage = Mem::default();
    let mut app = headless_app(dir.path());
    app.filmstrip.live.set_budget(23);
    app.ws.store.save_as("Keep", Preset::Dual.spec()).unwrap();
    eframe::App::save(&mut app, &mut storage);
    app.shutdown();
    // A newer build wrote a window role this one does not know, and part of the text is garbage.
    storage.0.insert(WORKSPACES_KEY.to_owned(), r#"{"custom":[{"name":"Keep","spec":{"windows":[]}},{"name":"New","spec":{"windows":[{"role":"Waveform3D","target":{"Slot":2}}]}}],"session":42}"#.to_owned());
    let settings: Settings = eframe::get_value(&storage, SETTINGS_KEY).unwrap();
    assert_eq!(settings.editable_thumb_gib, 23, "the other settings are intact");
    let store = WorkspaceStore::from_json(&storage.get_string(WORKSPACES_KEY).unwrap());
    assert_eq!(store.custom.len(), 2, "the readable entries stay");
    assert!(store.session.is_none());
    storage.0.insert(WORKSPACES_KEY.to_owned(), "\u{0}{{{ broken".to_owned());
    assert_eq!(WorkspaceStore::from_json(&storage.get_string(WORKSPACES_KEY).unwrap()), WorkspaceStore::default());
}

#[test]
fn an_unplugged_primary_display_and_a_missing_primary_flag_pick_a_sensible_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    // Wayland style: no display is flagged primary; the editor sits on the second one.
    let (a, b) = (mon("DELL", 0, false), mon("LG", 2560, false));
    app.monitors = vec![a, b.clone()];
    app.cached_main_monitor = Some(1);
    app.apply_key("dual-scopes");
    // Display 2 is the DELL (the only other one); display 3 is missing: fallback next to the editor, on the LG.
    assert_eq!(app.scopes_win.state.monitor.as_deref(), Some(b.id.as_str()), "without a primary display the fallback is the display of the editor");
    assert!(app.scopes_win.fallback);
    app.shutdown();
}

#[test]
fn a_hand_opened_window_goes_to_a_free_display() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    let (a, b, c) = (mon("DELL", 0, true), mon("LG", 2560, false), mon("BenQ", 5120, false));
    app.monitors = vec![a, b.clone(), c.clone()];
    app.cached_main_monitor = Some(0);
    app.set_role_display(WindowRole::Viewer, Some(DisplayTarget::Display(DisplayRef::of(&b))));
    app.run(Command::SetWorkspace(Workspace::Dual), &egui::Context::default());
    assert_eq!(app.viewer_win.state.monitor.as_deref(), Some(b.id.as_str()));
    app.run(Command::ToggleClean, &egui::Context::default());
    assert_eq!(app.clean_win.state.monitor.as_deref(), Some(c.id.as_str()), "the feed must not share the viewer's display");
    assert!(!app.clean_win.fallback);
    app.shutdown();
}

#[test]
fn a_fallback_feed_is_not_raised_and_the_main_display_decides_after_it_settled() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    let (a, b) = (mon("DELL", 0, true), mon("LG", 2560, false));
    app.monitors = vec![a.clone(), b.clone()];
    app.cached_main_monitor = Some(0);
    app.apply_key("dual");
    assert_eq!(app.viewer_win.state.monitor.as_deref(), Some(b.id.as_str()));
    // The editor is moved onto the viewer's display; once that has settled the viewer takes the free one.
    let t0 = Instant::now();
    assert!(app.ws.watch.observe(&app.monitors, Some(0), t0));
    assert!(!app.ws.watch.observe(&app.monitors, Some(1), t0 + Duration::from_millis(100)));
    assert!(app.ws.watch.observe(&app.monitors, Some(1), t0 + Duration::from_millis(1500)));
    app.cached_main_monitor = app.ws.watch.main_index(&app.monitors);
    app.reconcile_windows();
    assert_eq!(app.viewer_win.state.monitor.as_deref(), Some(a.id.as_str()), "display 2 is now the other one");
    app.shutdown();
}

#[test]
fn preset_shortcuts_are_unique_and_win_over_ctrl_digits() {
    let digits: Vec<_> = Preset::ALL.iter().map(|p| shortcut_of(Command::Preset(*p)).unwrap()).collect();
    for (i, s) in digits.iter().enumerate() {
        assert_eq!(s.modifiers, Modifiers::COMMAND | Modifiers::ALT);
        for other in &digits[i + 1..] {
            assert_ne!(s.logical_key, other.logical_key);
        }
        // Nothing else is bound to this exact combination.
        for cmd in SHORTCUT_ORDER {
            if !matches!(cmd, Command::Preset(_)) {
                assert_ne!(shortcut_of(cmd), Some(*s), "{cmd:?}");
            }
        }
    }
    let pos = |c: Command| SHORTCUT_ORDER.iter().position(|x| *x == c).unwrap();
    for p in Preset::ALL {
        assert!(pos(Command::Preset(p)) < pos(Command::Actual), "egui ignores an extra Alt, so Ctrl+1 would match Ctrl+Alt+1 first");
    }
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    let ctx = egui::Context::default();
    let mut modifiers = Modifiers::CTRL | Modifiers::ALT;
    modifiers.command = true;
    for (key, expect) in [(Key::Num1, Command::Preset(Preset::Single)), (Key::Num4, Command::Preset(Preset::DualScopesClean))] {
        let event = egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers };
        let raw = egui::RawInput { events: vec![event], ..Default::default() };
        let mut got = Vec::new();
        ctx.run_ui(raw, |_| got = app.handle_shortcuts(&ctx)).drop_without_applying_deltas();
        assert_eq!(got, vec![expect], "Ctrl+Alt+digit selects the preset only");
    }
    // Plain Ctrl+1 is still 100 %.
    let mut plain = Modifiers::CTRL;
    plain.command = true;
    let event = egui::Event::Key { key: Key::Num1, physical_key: Some(Key::Num1), pressed: true, repeat: false, modifiers: plain };
    let raw = egui::RawInput { events: vec![event], ..Default::default() };
    let mut got = Vec::new();
    ctx.run_ui(raw, |_| got = app.handle_shortcuts(&ctx)).drop_without_applying_deltas();
    assert_eq!(got, vec![Command::Actual]);
    app.shutdown();
}

#[test]
fn panel_visibility_and_reset_layout() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = headless_app(dir.path());
    app.ws.layout.show_inspector = false;
    app.ws.layout.inspector_w = 777.0;
    let spec = app.current_spec();
    assert!(!spec.layout.show_inspector && spec.layout.inspector_w == 777.0);
    app.run(Command::ResetLayout, &egui::Context::default());
    assert_eq!(app.ws.layout.inspector_w, MainLayout::default().inspector_w);
    let mut with_scopes = Preset::Single.spec();
    with_scopes.windows.push(WindowSpec { role: WindowRole::Scopes, target: DisplayTarget::Slot(3) });
    app.apply_spec(&with_scopes);
    assert_eq!(app.scopes_pos, ScopesPos::Window);
    assert!(app.ws.layout.show_inspector, "a workspace brings its own panel visibility");
    app.shutdown();
}
