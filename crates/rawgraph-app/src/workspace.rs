//! Workspaces as plain data: the main window layout, the secondary windows with
//! their target displays, the built-in presets, the user's saved workspaces, the
//! matching of remembered displays against the connected ones and the state
//! machine that moves windows when a display disappears or returns.
//!
//! Nothing in here touches egui or winit; the application feeds in the monitor
//! list (`MonitorInfo`) and carries out the resulting actions.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::app::ScopesPos;
use crate::windows::MonitorInfo;

/// What a secondary window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum WindowRole {
    /// The detached viewer of the dual workspace.
    Viewer,
    /// The full scope panel.
    Scopes,
    /// The image-only output.
    CleanFeed,
}

impl WindowRole {
    pub const ALL: [WindowRole; 3] = [WindowRole::Viewer, WindowRole::Scopes, WindowRole::CleanFeed];

    pub fn label(self) -> &'static str {
        match self {
            WindowRole::Viewer => "Viewer",
            WindowRole::Scopes => "Scopes",
            WindowRole::CleanFeed => "Clean feed",
        }
    }
}

/// A display as remembered in a saved workspace: name, resolution and position as the window
/// system reports them, plus a device identity where the platform has one (on Windows the
/// monitor's device path, which survives the renumbering of the GDI names).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayRef {
    pub name: String,
    pub size: [u32; 2],
    pub pos: [i32; 2],
    #[serde(default)]
    pub device: Option<String>,
}

/// Two device identities that are both known must be equal; an unknown one does not decide.
fn device_compatible(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

impl DisplayRef {
    pub fn of(m: &MonitorInfo) -> Self {
        Self { name: m.name().to_owned(), size: m.size, pos: m.pos, device: m.device.clone() }
    }

    /// Name, resolution and position equal, and no known device identity contradicts it.
    pub fn is_exactly(&self, m: &MonitorInfo) -> bool {
        self.name == m.name() && self.size == m.size && self.pos == m.pos && device_compatible(&self.device, &m.device)
    }

    /// Both identities are known and equal: the same physical monitor, wherever it sits now.
    pub fn is_same_device(&self, m: &MonitorInfo) -> bool {
        self.device.is_some() && self.device == m.device
    }

    pub fn label(&self) -> String {
        format!("{} ({} x {} at {}, {})", self.name, self.size[0], self.size[1], self.pos[0], self.pos[1])
    }
}

/// Where a window wants to be.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayTarget {
    /// The n-th display counted from the main window's display (1 = the main display,
    /// 2 = the first other display from the left, ...). Presets use slots, so they work
    /// on any setup.
    Slot(u8),
    /// A specific display, matched by `plan`.
    Display(DisplayRef),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSpec {
    pub role: WindowRole,
    pub target: DisplayTarget,
}

/// Panel sizes and visibility of the main window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MainLayout {
    pub show_filmstrip: bool,
    pub show_inspector: bool,
    /// The node graph strip below the viewer (single workspace only).
    pub show_graph: bool,
    /// Width of the filmstrip in points.
    pub filmstrip_w: f32,
    /// Width of the inspector column in points.
    pub inspector_w: f32,
    /// Height of the graph strip as a fraction of the free height (single workspace).
    pub graph_frac: f32,
    /// Docked scopes: fraction of the graph strip width (bottom right) or of the
    /// inspector column height (right column).
    pub scopes_frac: f32,
    /// Where the scopes are docked while no scopes window exists.
    pub scopes_pos: ScopesPos,
}

impl Default for MainLayout {
    fn default() -> Self {
        Self {
            show_filmstrip: true,
            show_inspector: true,
            show_graph: true,
            filmstrip_w: 232.0,
            inspector_w: 400.0,
            graph_frac: 0.36,
            scopes_frac: 0.4,
            scopes_pos: ScopesPos::BottomRight,
        }
    }
}

impl MainLayout {
    /// Layout of the dual workspaces (the viewer is a window, so the graph fills the main window).
    pub fn dual() -> Self {
        Self { inspector_w: 420.0, scopes_frac: 0.28, ..Self::default() }
    }

    /// Clamps every number into a usable range (settings may come from an older or edited file).
    pub fn sanitized(mut self) -> Self {
        let d = Self::default();
        let fix = |v: f32, lo: f32, hi: f32, dv: f32| if v.is_finite() { v.clamp(lo, hi) } else { dv };
        self.filmstrip_w = fix(self.filmstrip_w, 170.0, 420.0, d.filmstrip_w);
        self.inspector_w = fix(self.inspector_w, 300.0, 900.0, d.inspector_w);
        self.graph_frac = fix(self.graph_frac, 0.1, 0.8, d.graph_frac);
        self.scopes_frac = fix(self.scopes_frac, 0.1, 0.75, d.scopes_frac);
        if self.scopes_pos == ScopesPos::Window {
            self.scopes_pos = ScopesPos::BottomRight;
        }
        self
    }
}

/// A complete workspace: the main window and the secondary windows.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceSpec {
    pub layout: MainLayout,
    pub windows: Vec<WindowSpec>,
}

impl Default for WorkspaceSpec {
    fn default() -> Self {
        Preset::Single.spec()
    }
}

impl WorkspaceSpec {
    pub fn window(&self, role: WindowRole) -> Option<&WindowSpec> {
        self.windows.iter().find(|w| w.role == role)
    }

    pub fn has(&self, role: WindowRole) -> bool {
        self.window(role).is_some()
    }

    /// The same windows (role and target), ignoring panel sizes.
    pub fn same_windows(&self, other: &WorkspaceSpec) -> bool {
        let key = |s: &WorkspaceSpec| {
            let mut v: Vec<(WindowRole, DisplayTarget)> = s.windows.iter().map(|w| (w.role, w.target.clone())).collect();
            v.sort_by_key(|(r, _)| *r);
            v
        };
        key(self) == key(other)
    }

    fn with(layout: MainLayout, windows: &[(WindowRole, u8)]) -> Self {
        Self { layout, windows: windows.iter().map(|(role, n)| WindowSpec { role: *role, target: DisplayTarget::Slot(*n) }).collect() }
    }
}

/// The built-in workspaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Preset {
    Single,
    Dual,
    DualScopes,
    DualScopesClean,
    CleanOnly,
}

impl Preset {
    pub const ALL: [Preset; 5] = [Preset::Single, Preset::Dual, Preset::DualScopes, Preset::DualScopesClean, Preset::CleanOnly];

    /// Name used by `--workspace` and as the stored key.
    pub fn slug(self) -> &'static str {
        match self {
            Preset::Single => "single",
            Preset::Dual => "dual",
            Preset::DualScopes => "dual-scopes",
            Preset::DualScopesClean => "dual-scopes-clean",
            Preset::CleanOnly => "clean-feed",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::Single => "Single",
            Preset::Dual => "Dual",
            Preset::DualScopes => "Dual + Scopes",
            Preset::DualScopesClean => "Dual + Scopes + Clean Feed",
            Preset::CleanOnly => "Clean Feed only",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Preset::Single => "One window: viewer, graph, inspector and docked scopes.",
            Preset::Dual => "Viewer fullscreen on display 2; graph and inspector stay on display 1.",
            Preset::DualScopes => "Dual, plus the scopes in their own window on display 3.",
            Preset::DualScopesClean => "Dual, scopes on display 3 and the clean feed on display 4.",
            Preset::CleanOnly => "Single, plus the clean feed fullscreen on display 2.",
        }
    }

    /// Ctrl+Alt+n selects this preset.
    pub fn shortcut_digit(self) -> u8 {
        Preset::ALL.iter().position(|p| *p == self).map_or(0, |i| i as u8 + 1)
    }

    pub fn from_key(key: &str) -> Option<Preset> {
        let k = key.trim();
        Preset::ALL.into_iter().find(|p| p.slug().eq_ignore_ascii_case(k) || p.name().eq_ignore_ascii_case(k))
    }

    pub fn spec(self) -> WorkspaceSpec {
        use WindowRole::{CleanFeed, Scopes, Viewer};
        match self {
            Preset::Single => WorkspaceSpec::with(MainLayout::default(), &[]),
            Preset::Dual => WorkspaceSpec::with(MainLayout::dual(), &[(Viewer, 2)]),
            Preset::DualScopes => WorkspaceSpec::with(MainLayout::dual(), &[(Viewer, 2), (Scopes, 3)]),
            Preset::DualScopesClean => WorkspaceSpec::with(MainLayout::dual(), &[(Viewer, 2), (Scopes, 3), (CleanFeed, 4)]),
            Preset::CleanOnly => WorkspaceSpec::with(MainLayout::default(), &[(CleanFeed, 2)]),
        }
    }
}

/// A workspace saved by the user.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedWorkspace {
    pub name: String,
    pub spec: WorkspaceSpec,
}

/// Longest accepted name of a saved workspace, in characters.
pub const MAX_NAME: usize = 40;

/// Everything about workspaces that lives in the application settings.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceStore {
    pub custom: Vec<NamedWorkspace>,
    /// Key (preset slug or saved name) applied at start when `--workspace` is not given.
    pub default: Option<String>,
    /// The workspace of the last session, including manual window changes. Used at
    /// start when there is no default.
    pub session: Option<WorkspaceSpec>,
    /// The display the user chose for a window while it was closed (or last chose), used
    /// the next time that window opens by hand.
    pub preferred: Vec<WindowSpec>,
}

impl WorkspaceStore {
    /// Looks a workspace up by preset slug/name or saved name (case-insensitive).
    pub fn resolve(&self, key: &str) -> Option<WorkspaceSpec> {
        if let Some(p) = Preset::from_key(key) {
            return Some(p.spec());
        }
        let k = key.trim();
        self.custom.iter().find(|w| w.name.eq_ignore_ascii_case(k)).map(|w| {
            let mut spec = w.spec.clone();
            spec.layout = spec.layout.sanitized();
            spec
        })
    }

    /// A name that can be used for a saved workspace: trimmed, 1..=MAX_NAME characters
    /// and not the name of a preset.
    pub fn check_name(name: &str) -> Result<String, String> {
        let n = name.trim();
        if n.is_empty() {
            return Err("Enter a name for the workspace".to_owned());
        }
        if n.chars().count() > MAX_NAME {
            return Err(format!("The name is longer than {MAX_NAME} characters"));
        }
        if n.chars().any(char::is_control) {
            return Err("The name contains control characters".to_owned());
        }
        if Preset::from_key(n).is_some() {
            return Err(format!("\"{n}\" is the name of a built-in workspace"));
        }
        Ok(n.to_owned())
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.custom.iter().position(|w| w.name.eq_ignore_ascii_case(name.trim()))
    }

    /// Saves (or replaces) a workspace; returns the stored name.
    pub fn save_as(&mut self, name: &str, spec: WorkspaceSpec) -> Result<String, String> {
        let name = Self::check_name(name)?;
        match self.index_of(&name) {
            Some(i) => self.custom[i] = NamedWorkspace { name: name.clone(), spec },
            None => self.custom.push(NamedWorkspace { name: name.clone(), spec }),
        }
        Ok(name)
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<String, String> {
        let new = Self::check_name(new)?;
        let i = self.index_of(old).ok_or_else(|| format!("There is no workspace \"{old}\""))?;
        if let Some(j) = self.index_of(&new) {
            if j != i {
                return Err(format!("A workspace named \"{new}\" exists already"));
            }
        }
        let was_default = self.default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(&self.custom[i].name));
        self.custom[i].name = new.clone();
        if was_default {
            self.default = Some(new.clone());
        }
        Ok(new)
    }

    pub fn delete(&mut self, name: &str) -> bool {
        let Some(i) = self.index_of(name) else { return false };
        let removed = self.custom.remove(i);
        if self.default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(&removed.name)) {
            self.default = None;
        }
        true
    }

    /// Sets (or clears) the workspace applied at start. The key must exist.
    pub fn set_default(&mut self, key: Option<&str>) -> Result<(), String> {
        match key {
            None => self.default = None,
            Some(k) => {
                if self.resolve(k).is_none() {
                    return Err(format!("There is no workspace \"{k}\""));
                }
                self.default = Some(Preset::from_key(k).map_or_else(|| k.trim().to_owned(), |p| p.slug().to_owned()));
            }
        }
        Ok(())
    }

    pub fn is_default(&self, key: &str) -> bool {
        self.default.as_deref().is_some_and(|d| d.eq_ignore_ascii_case(key.trim()))
    }

    /// Drops saved entries that cannot be used (empty or clashing names, duplicates).
    pub fn sanitize(&mut self) {
        let mut seen: Vec<String> = Vec::new();
        self.custom.retain(|w| {
            let ok = Self::check_name(&w.name).is_ok() && !seen.iter().any(|s| s.eq_ignore_ascii_case(w.name.trim()));
            if ok {
                seen.push(w.name.trim().to_owned());
            }
            ok
        });
        for w in &mut self.custom {
            w.name = w.name.trim().to_owned();
            w.spec.layout = w.spec.layout.clone().sanitized();
        }
        if let Some(s) = &mut self.session {
            s.layout = s.layout.clone().sanitized();
        }
        if let Some(d) = self.default.clone() {
            if self.resolve(&d).is_none() {
                self.default = None;
            }
        }
        retain_usable_windows(&mut self.preferred);
        for w in &mut self.custom {
            retain_usable_windows(&mut w.spec.windows);
        }
        if let Some(s) = &mut self.session {
            retain_usable_windows(&mut s.windows);
        }
    }

    /// The settings text. Workspaces have their own storage key so that one unreadable entry
    /// can never reset the other settings.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Reads settings text leniently: an entry that cannot be read (a window role or target from
    /// a newer build, a damaged value) is dropped and the rest is kept. Never fails.
    pub fn from_json(text: &str) -> Self {
        let value: serde_json::Value = serde_json::from_str(text).unwrap_or(serde_json::Value::Null);
        let mut store = WorkspaceStore::default();
        if let Some(list) = value.get("custom").and_then(|v| v.as_array()) {
            store.custom = list
                .iter()
                .filter_map(|e| Some(NamedWorkspace { name: e.get("name")?.as_str()?.to_owned(), spec: spec_from_value(e.get("spec")?) }))
                .collect();
        }
        store.default = value.get("default").and_then(|v| v.as_str()).map(str::to_owned);
        store.session = value.get("session").filter(|v| v.is_object()).map(spec_from_value);
        if let Some(list) = value.get("preferred").and_then(|v| v.as_array()) {
            store.preferred = windows_from_values(list);
        }
        store.sanitize();
        store
    }

    pub fn preferred_target(&self, role: WindowRole) -> Option<&DisplayTarget> {
        self.preferred.iter().find(|w| w.role == role).map(|w| &w.target)
    }

    /// Remembers (or with `None` forgets) the display chosen for a window.
    pub fn prefer(&mut self, role: WindowRole, target: Option<DisplayTarget>) {
        self.preferred.retain(|w| w.role != role);
        if let Some(target) = target {
            self.preferred.push(WindowSpec { role, target });
        }
    }
}

fn windows_from_values(list: &[serde_json::Value]) -> Vec<WindowSpec> {
    list.iter().filter_map(|w| serde_json::from_value::<WindowSpec>(w.clone()).ok()).collect()
}

fn spec_from_value(v: &serde_json::Value) -> WorkspaceSpec {
    WorkspaceSpec {
        layout: v.get("layout").and_then(|l| serde_json::from_value::<MainLayout>(l.clone()).ok()).unwrap_or_default(),
        windows: v.get("windows").and_then(|w| w.as_array()).map(|a| windows_from_values(a)).unwrap_or_default(),
    }
}

/// Keeps the first window of each role and drops slots that cannot exist (0 and 1: display 1 is
/// the main window's own display).
fn retain_usable_windows(windows: &mut Vec<WindowSpec>) {
    let mut seen = Vec::new();
    windows.retain(|w| {
        if matches!(w.target, DisplayTarget::Slot(n) if n < 2) || seen.contains(&w.role) {
            return false;
        }
        seen.push(w.role);
        true
    });
}

// ----------------------------------------------------------------- display matching

/// How a window found its display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    /// A display slot (presets).
    Slot,
    /// Name, resolution and position all equal.
    Exact,
    /// The same physical monitor (device identity), at another position or resolution.
    SameDevice,
    /// Same name (the position or resolution changed).
    SameName,
    /// Same resolution only.
    SameResolution,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    On { monitor: usize, how: MatchKind },
    /// The wanted display is not connected: the caller puts the window on the primary display.
    Fallback,
}

impl Placement {
    pub fn monitor(self) -> Option<usize> {
        match self {
            Placement::On { monitor, .. } => Some(monitor),
            Placement::Fallback => None,
        }
    }
}

/// The monitors other than the main window's, ordered left to right (then top to bottom).
fn other_monitors(monitors: &[MonitorInfo], main: Option<usize>) -> Vec<usize> {
    let mut v: Vec<usize> = (0..monitors.len()).filter(|i| Some(*i) != main).collect();
    v.sort_by_key(|i| (monitors[*i].pos[0], monitors[*i].pos[1], *i));
    v
}

/// The monitor of `slot` when nothing else is in use: 2 = first other display, 3 = second, ...
/// Slot 1 is the main display, which is never used for a secondary window by slot.
#[cfg(test)]
pub fn slot_monitor(monitors: &[MonitorInfo], main: Option<usize>, slot: u8) -> Option<usize> {
    let n = usize::from(slot).checked_sub(2)?;
    other_monitors(monitors, main).get(n).copied()
}

fn distance2(a: [i32; 2], b: [i32; 2]) -> i64 {
    let dx = i64::from(a[0]) - i64::from(b[0]);
    let dy = i64::from(a[1]) - i64::from(b[1]);
    dx * dx + dy * dy
}

/// Finds the monitor for a remembered display. Order: exact (name, size, position), same
/// physical device, same name (same size preferred, then nearest position), same resolution
/// (nearest position), otherwise none. Monitors in `taken` are skipped by the last two.
pub fn match_display(monitors: &[MonitorInfo], want: &DisplayRef, taken: &[usize]) -> Option<(usize, MatchKind)> {
    if let Some(i) = monitors.iter().position(|m| want.is_exactly(m)) {
        return Some((i, MatchKind::Exact));
    }
    if let Some(i) = monitors.iter().position(|m| want.is_same_device(m)) {
        return Some((i, MatchKind::SameDevice));
    }
    let free = |i: &usize| !taken.contains(i);
    let by_name = (0..monitors.len())
        .filter(free)
        .filter(|i| monitors[*i].name() == want.name)
        .min_by_key(|i| (monitors[*i].size != want.size, distance2(monitors[*i].pos, want.pos), *i));
    if let Some(i) = by_name {
        return Some((i, MatchKind::SameName));
    }
    (0..monitors.len())
        .filter(free)
        .filter(|i| monitors[*i].size == want.size)
        .min_by_key(|i| (distance2(monitors[*i].pos, want.pos), *i))
        .map(|i| (i, MatchKind::SameResolution))
}

/// Decides the display of every window. `main` is the monitor of the main window.
///
/// Remembered displays resolve first, in four passes - exact, same device, same name, same
/// resolution - so a window with a better match is never robbed by a looser match of another
/// window, and two identical monitors are told apart by device or position. Slots follow: slot
/// n starts at the n-th display other than the main one and moves on to the next free display
/// when that one is used by another window, so a window opened by hand never lands on a display
/// that is already in use. A looser match or a slot never takes the main display.
pub fn plan(windows: &[WindowSpec], monitors: &[MonitorInfo], main: Option<usize>) -> Vec<(WindowRole, Placement)> {
    let mut out: Vec<(WindowRole, Placement)> = windows.iter().map(|w| (w.role, Placement::Fallback)).collect();
    let mut taken: Vec<usize> = main.into_iter().collect();
    // Exact and same-device matches are deliberate (they may sit on the main display).
    for pass in [MatchKind::Exact, MatchKind::SameDevice] {
        for (i, w) in windows.iter().enumerate() {
            let DisplayTarget::Display(want) = &w.target else { continue };
            if out[i].1 != Placement::Fallback {
                continue;
            }
            let found = if pass == MatchKind::Exact {
                monitors.iter().position(|m| want.is_exactly(m))
            } else {
                monitors.iter().position(|m| want.is_same_device(m))
            };
            if let Some(m) = found {
                out[i].1 = Placement::On { monitor: m, how: pass };
                taken.push(m);
            }
        }
    }
    for pass in [MatchKind::SameName, MatchKind::SameResolution] {
        for (i, w) in windows.iter().enumerate() {
            let DisplayTarget::Display(want) = &w.target else { continue };
            if out[i].1 != Placement::Fallback {
                continue;
            }
            if let Some((m, how)) = match_display(monitors, want, &taken) {
                if how == pass {
                    out[i].1 = Placement::On { monitor: m, how };
                    taken.push(m);
                }
            }
        }
    }
    let others = other_monitors(monitors, main);
    let mut slots: Vec<(u8, usize)> = windows
        .iter()
        .enumerate()
        .filter_map(|(i, w)| if let DisplayTarget::Slot(n) = w.target { Some((n, i)) } else { None })
        .collect();
    slots.sort_unstable();
    for (n, i) in slots {
        let Some(start) = usize::from(n).checked_sub(2) else { continue };
        if let Some(m) = others.iter().skip(start).copied().find(|m| !taken.contains(m)) {
            out[i].1 = Placement::On { monitor: m, how: MatchKind::Slot };
            taken.push(m);
        }
    }
    out
}

// ------------------------------------------------------------- disconnect / reconnect

/// What happened to a window when the monitor list was reconciled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// First placement on a display.
    Placed,
    /// First placement, but the wanted display is not connected: it sits on the primary display.
    Missing,
    /// Its display vanished.
    Lost,
    /// Its display is back.
    Restored,
    /// It moved to another display (rearranged or replaced).
    Moved,
    /// Its own display changed position or resolution; nothing to tell, only to remember.
    Refreshed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Action {
    pub role: WindowRole,
    pub placement: Placement,
    pub change: Change,
}

/// The windows an application wants (`windows`, the intended layout - never changed by
/// a fallback) and where each really sits (`placed`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Intent {
    pub windows: Vec<WindowSpec>,
    placed: Vec<(WindowRole, Option<DisplayRef>)>,
}

impl Intent {
    pub fn from_spec(spec: &WorkspaceSpec) -> Self {
        Self { windows: spec.windows.clone(), placed: Vec::new() }
    }

    pub fn has(&self, role: WindowRole) -> bool {
        self.windows.iter().any(|w| w.role == role)
    }

    /// Takes over where the windows of `old` sit, for windows with an unchanged target (a new
    /// workspace that keeps a window where it is must not move it).
    pub fn adopt_placement(&mut self, old: &Intent) {
        for w in &self.windows {
            if old.target(w.role) == Some(&w.target) {
                if let Some(p) = old.placed.iter().find(|(r, _)| *r == w.role) {
                    self.placed.push(p.clone());
                }
            }
        }
    }

    pub fn target(&self, role: WindowRole) -> Option<&DisplayTarget> {
        self.windows.iter().find(|w| w.role == role).map(|w| &w.target)
    }

    /// Adds a window (or changes the target of an existing one) and forgets where it sat, so the
    /// next `reconcile` places it.
    pub fn set(&mut self, role: WindowRole, target: DisplayTarget) {
        match self.windows.iter_mut().find(|w| w.role == role) {
            Some(w) => w.target = target,
            None => self.windows.push(WindowSpec { role, target }),
        }
        self.placed.retain(|(r, _)| *r != role);
    }

    pub fn remove(&mut self, role: WindowRole) {
        self.windows.retain(|w| w.role != role);
        self.placed.retain(|(r, _)| *r != role);
    }

    /// The first slot from 2 up that no other window asks for.
    pub fn free_slot(&self, except: WindowRole) -> u8 {
        (2u8..=8)
            .find(|n| !self.windows.iter().any(|w| w.role != except && w.target == DisplayTarget::Slot(*n)))
            .unwrap_or(2)
    }

    /// The default target of a window opened by hand: the first display slot no other window uses.
    pub fn default_target(&self, role: WindowRole) -> DisplayTarget {
        DisplayTarget::Slot(self.free_slot(role))
    }

    /// True while a window sits on the primary display because its display is missing.
    pub fn in_fallback(&self, role: WindowRole) -> bool {
        self.placed.iter().any(|(r, d)| *r == role && d.is_none())
    }

    #[cfg(test)]
    pub fn any_fallback(&self) -> bool {
        self.placed.iter().any(|(_, d)| d.is_none())
    }

    /// The display a window sits on, if it is on one of its own.
    #[cfg(test)]
    pub fn placed_on(&self, role: WindowRole) -> Option<&DisplayRef> {
        self.placed.iter().find(|(r, _)| *r == role).and_then(|(_, d)| d.as_ref())
    }

    /// Compares the plan for the current monitors with where the windows sit and returns
    /// what must move. An empty monitor list means "unknown" (enumeration unavailable) and
    /// changes nothing.
    pub fn reconcile(&mut self, monitors: &[MonitorInfo], main: Option<usize>) -> Vec<Action> {
        if monitors.is_empty() {
            return Vec::new();
        }
        let mut actions = Vec::new();
        for (role, placement) in plan(&self.windows, monitors, main) {
            let now = placement.monitor().map(|m| DisplayRef::of(&monitors[m]));
            let before = self.placed.iter().find(|(r, _)| *r == role).map(|(_, d)| d.clone());
            let change = match (before, &now) {
                (None, Some(_)) => Change::Placed,
                (None, None) => Change::Missing,
                (Some(old), new) if old == *new => continue,
                (Some(Some(_)), None) => Change::Lost,
                (Some(None), Some(_)) => Change::Restored,
                (Some(Some(old)), Some(new)) => {
                    if same_display(&old, new, monitors) {
                        Change::Refreshed
                    } else {
                        Change::Moved
                    }
                }
                (Some(None), None) => continue,
            };
            self.placed.retain(|(r, _)| *r != role);
            self.placed.push((role, now));
            actions.push(Action { role, placement, change });
        }
        actions
    }
}

/// True when `new` is the monitor `old` described, whose position or resolution has changed:
/// the same device identity, or - without identities - the same name when only one connected
/// monitor carries it.
fn same_display(old: &DisplayRef, new: &DisplayRef, monitors: &[MonitorInfo]) -> bool {
    if old.device.is_some() && new.device.is_some() {
        return old.device == new.device;
    }
    old.name == new.name && monitors.iter().filter(|m| m.name() == new.name).count() == 1
}

/// Text for the status bar.
pub fn describe(action: &Action) -> Option<String> {
    let name = action.role.label();
    match action.change {
        Change::Placed | Change::Refreshed => None,
        Change::Missing => Some(format!("{name}: its display is not connected, shown on the primary display for now")),
        Change::Lost => Some(format!("{name}: its display was disconnected, moved to the primary display")),
        Change::Restored => Some(format!("{name}: its display is back, moved there again")),
        Change::Moved => Some(format!("{name}: moved to another display")),
    }
}

/// How long the main window must stay on a new display before the windows are re-planned
/// (dragging across displays must not make the secondary windows jump around).
pub const MAIN_MOVE_DEBOUNCE: Duration = Duration::from_millis(1000);

/// Decides when the monitor situation has changed enough to reconcile the windows again: the
/// list of displays, the primary display or (debounced) the display of the main window.
#[derive(Debug, Default)]
pub struct DisplayWatch {
    seen: bool,
    ids: Vec<String>,
    primary: Option<usize>,
    main: Option<String>,
    pending: Option<(String, Instant)>,
}

impl DisplayWatch {
    /// `main` is the index of the display the main window sits on now, if known. Returns true when
    /// the windows must be reconciled. An empty list (enumeration failed) never triggers.
    pub fn observe(&mut self, monitors: &[MonitorInfo], main: Option<usize>, now: Instant) -> bool {
        if monitors.is_empty() {
            return false;
        }
        let ids: Vec<String> = monitors.iter().map(|m| m.id.clone()).collect();
        let primary = monitors.iter().position(|m| m.primary);
        if !self.seen || ids != self.ids || primary != self.primary {
            self.seen = true;
            self.main = main.map(|i| monitors[i].id.clone()).or_else(|| self.main.take().filter(|m| ids.contains(m)));
            self.ids = ids;
            self.primary = primary;
            self.pending = None;
            return true;
        }
        // Unknown (some compositors report no display for a window before it was shown): keep the last one.
        let Some(main_id) = main.map(|i| monitors[i].id.clone()) else { return false };
        if self.main.as_ref() == Some(&main_id) {
            self.pending = None;
            return false;
        }
        match &self.pending {
            Some((id, since)) if *id == main_id => {
                if now.duration_since(*since) >= MAIN_MOVE_DEBOUNCE {
                    self.main = Some(main_id);
                    self.pending = None;
                    true
                } else {
                    false
                }
            }
            _ => {
                self.pending = Some((main_id, now));
                false
            }
        }
    }

    /// Index of the settled main display in `monitors`.
    pub fn main_index(&self, monitors: &[MonitorInfo]) -> Option<usize> {
        let id = self.main.as_ref()?;
        monitors.iter().position(|m| &m.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, x: i32, w: u32, h: u32, primary: bool) -> MonitorInfo {
        MonitorInfo { id: format!("{name}|{w}x{h}@{x},0"), pos: [x, 0], size: [w, h], scale: 1.0, primary, device: None }
    }

    fn two() -> Vec<MonitorInfo> {
        vec![mon("DELL", 0, 2560, 1440, true), mon("LG", 2560, 3840, 2160, false)]
    }

    fn want(m: &MonitorInfo) -> DisplayTarget {
        DisplayTarget::Display(DisplayRef::of(m))
    }

    fn spec(role: WindowRole, target: DisplayTarget) -> WindowSpec {
        WindowSpec { role, target }
    }

    #[test]
    fn exact_match_wins_over_name_and_resolution() {
        let ms = vec![mon("LG", 0, 1920, 1080, true), mon("LG", 1920, 1920, 1080, false), mon("DELL", 3840, 1920, 1080, false)];
        let r = DisplayRef { name: "LG".into(), size: [1920, 1080], pos: [1920, 0], device: None };
        assert_eq!(match_display(&ms, &r, &[]), Some((1, MatchKind::Exact)));
        // An exact match is honoured even when the monitor is listed as taken.
        assert_eq!(match_display(&ms, &r, &[1]), Some((1, MatchKind::Exact)));
    }

    #[test]
    fn same_name_prefers_same_size_then_nearest_position() {
        let ms = vec![mon("LG", 0, 1920, 1080, true), mon("LG", 5000, 2560, 1440, false), mon("LG", 2000, 2560, 1440, false)];
        let r = DisplayRef { name: "LG".into(), size: [2560, 1440], pos: [4500, 0], device: None };
        assert_eq!(match_display(&ms, &r, &[]), Some((1, MatchKind::SameName)), "same size, nearest to the old position");
        assert_eq!(match_display(&ms, &r, &[1]), Some((2, MatchKind::SameName)), "the next free one");
        let r2 = DisplayRef { name: "LG".into(), size: [3840, 2160], pos: [0, 0], device: None };
        assert_eq!(match_display(&ms, &r2, &[0]), Some((2, MatchKind::SameName)), "no same-size LG: nearest free LG");
    }

    #[test]
    fn same_resolution_when_the_name_is_unknown() {
        let ms = vec![mon("A", 0, 1920, 1080, true), mon("B", 1920, 3840, 2160, false)];
        let r = DisplayRef { name: "Gone".into(), size: [3840, 2160], pos: [9999, 0], device: None };
        assert_eq!(match_display(&ms, &r, &[]), Some((1, MatchKind::SameResolution)));
        let none = DisplayRef { name: "Gone".into(), size: [800, 600], pos: [0, 0], device: None };
        assert_eq!(match_display(&ms, &none, &[]), None, "nothing similar: the caller falls back");
        assert_eq!(match_display(&[], &none, &[]), None);
    }

    #[test]
    fn two_identical_monitors_are_told_apart_by_position() {
        let ms = vec![mon("LG", 0, 1920, 1080, true), mon("LG", 1920, 1920, 1080, false), mon("LG", 3840, 1920, 1080, false)];
        let wins = vec![spec(WindowRole::Scopes, want(&ms[2])), spec(WindowRole::CleanFeed, want(&ms[1]))];
        let p = plan(&wins, &ms, Some(0));
        assert_eq!(p[0].1, Placement::On { monitor: 2, how: MatchKind::Exact });
        assert_eq!(p[1].1, Placement::On { monitor: 1, how: MatchKind::Exact });
    }

    #[test]
    fn identical_monitors_rearranged_get_distinct_displays() {
        // Saved at x=1920 and x=3840; now the same two monitors sit at x=-1920 and x=1920.
        let saved_a = DisplayRef { name: "LG".into(), size: [1920, 1080], pos: [1920, 0], device: None };
        let saved_b = DisplayRef { name: "LG".into(), size: [1920, 1080], pos: [3840, 0], device: None };
        let ms = vec![mon("LG", 0, 1920, 1080, true), mon("LG", -1920, 1920, 1080, false), mon("LG", 1920, 1920, 1080, false)];
        let wins = vec![
            spec(WindowRole::Scopes, DisplayTarget::Display(saved_a)),
            spec(WindowRole::CleanFeed, DisplayTarget::Display(saved_b)),
        ];
        let p = plan(&wins, &ms, Some(0));
        // Scopes matches monitor 2 exactly; the clean feed takes the remaining LG by name.
        assert_eq!(p[0].1, Placement::On { monitor: 2, how: MatchKind::Exact });
        assert_eq!(p[1].1, Placement::On { monitor: 1, how: MatchKind::SameName });
    }

    #[test]
    fn a_loose_match_never_takes_the_main_display_or_a_used_one() {
        let ms = vec![mon("LG", 0, 1920, 1080, true), mon("LG", 1920, 1920, 1080, false)];
        let moved = DisplayRef { name: "LG".into(), size: [1920, 1080], pos: [5000, 0], device: None };
        let wins = vec![spec(WindowRole::Scopes, DisplayTarget::Display(moved.clone())), spec(WindowRole::CleanFeed, DisplayTarget::Display(moved))];
        let p = plan(&wins, &ms, Some(0));
        assert_eq!(p[0].1, Placement::On { monitor: 1, how: MatchKind::SameName });
        assert_eq!(p[1].1, Placement::Fallback, "the only free display is used already; main is not taken");
    }

    #[test]
    fn exact_match_is_not_robbed_by_a_looser_one() {
        let ms = vec![mon("A", 0, 1920, 1080, true), mon("LG", 1920, 1920, 1080, false)];
        let loose = DisplayRef { name: "LG".into(), size: [1920, 1080], pos: [9000, 0], device: None };
        // The loose window comes first in the list, but the exact one claims the display.
        let wins = vec![spec(WindowRole::Scopes, DisplayTarget::Display(loose)), spec(WindowRole::CleanFeed, want(&ms[1]))];
        let p = plan(&wins, &ms, Some(0));
        assert_eq!(p[0].1, Placement::Fallback);
        assert_eq!(p[1].1, Placement::On { monitor: 1, how: MatchKind::Exact });
    }

    #[test]
    fn slots_count_from_the_main_display_left_to_right() {
        // Listed in arbitrary order; main is monitor 2 in the middle.
        let ms = vec![mon("R", 3000, 1920, 1080, false), mon("L", -1920, 1920, 1080, false), mon("M", 0, 3000, 1080, true)];
        assert_eq!(slot_monitor(&ms, Some(2), 2), Some(1), "left display is display 2");
        assert_eq!(slot_monitor(&ms, Some(2), 3), Some(0));
        assert_eq!(slot_monitor(&ms, Some(2), 4), None);
        assert_eq!(slot_monitor(&ms, Some(2), 1), None, "slot 1 is the main display");
        assert_eq!(slot_monitor(&ms, Some(2), 0), None);
        assert_eq!(slot_monitor(&ms, None, 2), Some(1), "unknown main: lowest position first");
    }

    #[test]
    fn missing_slot_falls_back() {
        let ms = vec![mon("A", 0, 1920, 1080, true)];
        let wins = Preset::DualScopesClean.spec().windows;
        assert!(plan(&wins, &ms, Some(0)).iter().all(|(_, p)| *p == Placement::Fallback));
        let three = vec![mon("A", 0, 1920, 1080, true), mon("B", 1920, 1920, 1080, false), mon("C", 3840, 1920, 1080, false)];
        let p = plan(&wins, &three, Some(0));
        assert_eq!(p[0].1.monitor(), Some(1));
        assert_eq!(p[1].1.monitor(), Some(2));
        assert_eq!(p[2].1, Placement::Fallback, "no display 4");
    }

    #[test]
    fn preset_definitions() {
        use WindowRole::{CleanFeed, Scopes, Viewer};
        let roles = |p: Preset| p.spec().windows.iter().map(|w| w.role).collect::<Vec<_>>();
        assert_eq!(roles(Preset::Single), vec![]);
        assert_eq!(roles(Preset::Dual), vec![Viewer]);
        assert_eq!(roles(Preset::DualScopes), vec![Viewer, Scopes]);
        assert_eq!(roles(Preset::DualScopesClean), vec![Viewer, Scopes, CleanFeed]);
        assert_eq!(roles(Preset::CleanOnly), vec![CleanFeed]);
        let s = Preset::DualScopesClean.spec();
        assert_eq!(s.window(Viewer).unwrap().target, DisplayTarget::Slot(2));
        assert_eq!(s.window(Scopes).unwrap().target, DisplayTarget::Slot(3));
        assert_eq!(s.window(CleanFeed).unwrap().target, DisplayTarget::Slot(4));
        assert_eq!(Preset::CleanOnly.spec().window(CleanFeed).unwrap().target, DisplayTarget::Slot(2));
        // Unique slugs and shortcut digits 1..=5.
        let digits: Vec<u8> = Preset::ALL.iter().map(|p| p.shortcut_digit()).collect();
        assert_eq!(digits, vec![1, 2, 3, 4, 5]);
        for (i, a) in Preset::ALL.iter().enumerate() {
            for b in &Preset::ALL[i + 1..] {
                assert_ne!(a.slug(), b.slug());
            }
            assert_eq!(Preset::from_key(a.slug()), Some(*a));
            assert_eq!(Preset::from_key(&a.name().to_uppercase()), Some(*a));
        }
        assert_eq!(Preset::from_key("triple"), None);
    }

    // ---- state machine

    fn run(intent: &mut Intent, ms: &[MonitorInfo]) -> Vec<(WindowRole, Change)> {
        intent.reconcile(ms, Some(0)).into_iter().map(|a| (a.role, a.change)).collect()
    }

    #[test]
    fn display_vanishes_and_returns() {
        let full = two();
        let mut intent = Intent::from_spec(&Preset::Dual.spec());
        assert_eq!(run(&mut intent, &full), vec![(WindowRole::Viewer, Change::Placed)]);
        assert_eq!(run(&mut intent, &full), vec![], "nothing changes while the displays stay the same");
        // Unplug the second display.
        let one = vec![full[0].clone()];
        assert_eq!(run(&mut intent, &one), vec![(WindowRole::Viewer, Change::Lost)]);
        assert!(intent.in_fallback(WindowRole::Viewer));
        assert_eq!(intent.target(WindowRole::Viewer), Some(&DisplayTarget::Slot(2)), "the intended layout is kept");
        assert_eq!(run(&mut intent, &one), vec![], "no repeated moves");
        // Plug it in again.
        assert_eq!(run(&mut intent, &full), vec![(WindowRole::Viewer, Change::Restored)]);
        assert!(!intent.in_fallback(WindowRole::Viewer));
        assert_eq!(intent.placed_on(WindowRole::Viewer).map(|d| d.name.as_str()), Some("LG"));
    }

    #[test]
    fn missing_at_start_is_restored_later() {
        let mut intent = Intent::from_spec(&Preset::DualScopes.spec());
        let two_displays = two();
        assert_eq!(run(&mut intent, &two_displays), vec![(WindowRole::Viewer, Change::Placed), (WindowRole::Scopes, Change::Missing)]);
        assert!(intent.in_fallback(WindowRole::Scopes) && intent.any_fallback());
        let three = vec![two_displays[0].clone(), two_displays[1].clone(), mon("BenQ", 6400, 1920, 1080, false)];
        assert_eq!(run(&mut intent, &three), vec![(WindowRole::Scopes, Change::Restored)]);
        assert!(!intent.any_fallback());
    }

    #[test]
    fn rearranged_displays_move_windows_and_unknown_list_changes_nothing() {
        let full = two();
        let mut intent = Intent::from_spec(&WorkspaceSpec { layout: MainLayout::default(), windows: vec![spec(WindowRole::CleanFeed, want(&full[1]))] });
        assert_eq!(run(&mut intent, &full), vec![(WindowRole::CleanFeed, Change::Placed)]);
        assert_eq!(run(&mut intent, &[]), vec![], "an empty list means the enumeration failed");
        assert!(!intent.in_fallback(WindowRole::CleanFeed));
        // The LG moves to the left of the main display.
        let moved = vec![mon("DELL", 0, 2560, 1440, true), mon("LG", -3840, 3840, 2160, false)];
        // The only LG is the same monitor at another place: remembered, not reported as a move.
        assert_eq!(run(&mut intent, &moved), vec![(WindowRole::CleanFeed, Change::Refreshed)]);
        assert_eq!(run(&mut intent, &moved), vec![], "settled");
        // The intent still names the original display, so a return to the old layout is exact again.
        assert_eq!(run(&mut intent, &full), vec![(WindowRole::CleanFeed, Change::Refreshed)]);
        assert_eq!(intent.placed_on(WindowRole::CleanFeed), Some(&DisplayRef::of(&full[1])));
        assert!(describe(&Action { role: WindowRole::CleanFeed, placement: Placement::Fallback, change: Change::Refreshed }).is_none());
    }

    #[test]
    fn a_resolution_change_of_the_own_display_is_not_a_move() {
        let full = two();
        let mut intent = Intent::from_spec(&WorkspaceSpec { layout: MainLayout::default(), windows: vec![spec(WindowRole::Scopes, want(&full[1]))] });
        run(&mut intent, &full);
        let mut changed = full.clone();
        changed[1] = mon("LG", 2560, 2560, 1440, false);
        assert_eq!(run(&mut intent, &changed), vec![(WindowRole::Scopes, Change::Refreshed)]);
        // Two monitors with that name: without a device identity it cannot be told, so it counts as a move.
        let mut twins = changed.clone();
        twins.push(mon("LG", 5120, 2560, 1440, false));
        let mut intent = Intent::from_spec(&WorkspaceSpec { layout: MainLayout::default(), windows: vec![spec(WindowRole::Scopes, want(&full[1]))] });
        run(&mut intent, &full);
        assert_eq!(run(&mut intent, &twins), vec![(WindowRole::Scopes, Change::Moved)]);
    }

    fn dev(name: &str, x: i32, device: &str) -> MonitorInfo {
        MonitorInfo { device: Some(device.to_owned()), ..mon(name, x, 1920, 1080, false) }
    }

    #[test]
    fn the_device_identity_tells_identical_monitors_apart_even_when_positions_swap() {
        // Two identical monitors; the user swapped their places in the display settings.
        let a_old = dev("LG", 1920, "UID-A");
        let b_old = dev("LG", 3840, "UID-B");
        let saved_b = DisplayRef::of(&b_old);
        let swapped = vec![mon("MAIN", 0, 1920, 1080, true), dev("LG", 1920, "UID-B"), dev("LG", 3840, "UID-A")];
        // Name, size and position alone would pick monitor 2 (the one now at 3840); the device says monitor 1.
        let wins = vec![spec(WindowRole::Scopes, DisplayTarget::Display(saved_b.clone()))];
        assert_eq!(plan(&wins, &swapped, Some(0))[0].1, Placement::On { monitor: 1, how: MatchKind::SameDevice });
        assert!(!saved_b.is_exactly(&swapped[2]), "same name, size and position but another device");
        assert_eq!(match_display(&swapped, &saved_b, &[]), Some((1, MatchKind::SameDevice)));
        // Unknown identities (other platforms) do not contradict a position match.
        assert!(saved_b.is_exactly(&mon("LG", 3840, 1920, 1080, false)));
        let _ = a_old;
        // The device wins over a position match that belongs to another device.
        let mut intent = Intent::from_spec(&WorkspaceSpec { layout: MainLayout::default(), windows: wins });
        let before = vec![mon("MAIN", 0, 1920, 1080, true), dev("LG", 1920, "UID-A"), dev("LG", 3840, "UID-B")];
        assert_eq!(run(&mut intent, &before), vec![(WindowRole::Scopes, Change::Placed)]);
        assert_eq!(run(&mut intent, &swapped), vec![(WindowRole::Scopes, Change::Refreshed)], "same device, new place: quietly remembered");
    }

    #[test]
    fn a_window_opened_by_hand_never_lands_on_a_display_that_is_in_use() {
        let ms = vec![mon("A", 0, 1920, 1080, true), mon("B", 1920, 1920, 1080, false), mon("C", 3840, 1920, 1080, false)];
        // Viewer on the concrete display B; a clean feed opened by hand asks for the first free slot (2 = B).
        let mut intent = Intent::default();
        intent.set(WindowRole::Viewer, want(&ms[1]));
        let target = intent.default_target(WindowRole::CleanFeed);
        assert_eq!(target, DisplayTarget::Slot(2));
        intent.set(WindowRole::CleanFeed, target);
        let p = plan(&intent.windows, &ms, Some(0));
        assert_eq!(p[0].1, Placement::On { monitor: 1, how: MatchKind::Exact });
        assert_eq!(p[1].1, Placement::On { monitor: 2, how: MatchKind::Slot }, "B is taken, so C");
        // With only B connected there is no free display: the primary display (fallback).
        let two = &ms[..2];
        assert_eq!(plan(&intent.windows, two, Some(0))[1].1, Placement::Fallback);
        // Slot windows keep their absolute numbers when nothing is in the way.
        let wins = Preset::DualScopes.spec().windows;
        assert_eq!(plan(&wins, &ms, Some(0)).iter().map(|(_, p)| p.monitor()).collect::<Vec<_>>(), vec![Some(1), Some(2)]);
        // A stale remembered display resolves before the slots, which then move on.
        let stale = DisplayRef { name: "B".into(), size: [1920, 1080], pos: [9999, 0], device: None };
        let wins = vec![spec(WindowRole::Viewer, DisplayTarget::Display(stale)), spec(WindowRole::Scopes, DisplayTarget::Slot(2))];
        let p = plan(&wins, &ms, Some(0));
        assert_eq!((p[0].1.monitor(), p[1].1.monitor()), (Some(1), Some(2)));
    }

    #[test]
    fn the_watch_triggers_on_list_primary_and_settled_main_changes() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut w = DisplayWatch::default();
        let two = two();
        assert!(!w.observe(&[], Some(0), at(0)), "an empty list never triggers");
        assert!(w.observe(&two, Some(0), at(0)), "first sight");
        assert!(!w.observe(&two, Some(0), at(2000)));
        assert_eq!(w.main_index(&two), Some(0));
        // The editor is dragged to the second display: only after it stayed there.
        assert!(!w.observe(&two, Some(1), at(2100)), "first sighting starts the debounce");
        assert!(!w.observe(&two, Some(1), at(2600)), "too early");
        assert!(w.observe(&two, Some(1), at(3200)), "settled");
        assert_eq!(w.main_index(&two), Some(1));
        assert!(!w.observe(&two, Some(1), at(5000)));
        // A drag across and back inside the debounce time changes nothing.
        assert!(!w.observe(&two, Some(0), at(6000)));
        assert!(!w.observe(&two, Some(1), at(6300)));
        assert!(!w.observe(&two, Some(1), at(9000)));
        // An unknown main display (Wayland before the first enter event) keeps the last one.
        assert!(!w.observe(&two, None, at(10_000)));
        assert_eq!(w.main_index(&two), Some(1));
        // The primary display changes while a window is in fallback.
        let mut other_primary = two.clone();
        other_primary[0].primary = false;
        other_primary[1].primary = true;
        assert!(w.observe(&other_primary, Some(1), at(11_000)));
        // The list changes.
        let one = vec![two[0].clone()];
        assert!(w.observe(&one, Some(0), at(12_000)));
        assert_eq!(w.main_index(&one), Some(0));
        // Unknown main on the very first look: remembered as unknown, then settles.
        let mut w = DisplayWatch::default();
        assert!(w.observe(&two, None, at(0)));
        assert_eq!(w.main_index(&two), None);
        assert!(!w.observe(&two, Some(1), at(100)));
        assert!(w.observe(&two, Some(1), at(1200)));
        assert_eq!(w.main_index(&two), Some(1));
    }

    #[test]
    fn two_windows_on_identical_monitors_survive_an_unplug() {
        let ms = vec![mon("LG", 0, 1920, 1080, true), mon("LG", 1920, 1920, 1080, false), mon("LG", 3840, 1920, 1080, false)];
        let wins = vec![spec(WindowRole::Scopes, want(&ms[1])), spec(WindowRole::CleanFeed, want(&ms[2]))];
        let mut intent = Intent { windows: wins, ..Intent::default() };
        assert_eq!(run(&mut intent, &ms).len(), 2);
        // The monitor at x=3840 goes: the clean feed has nothing similar and falls back; scopes stay.
        let less = ms[..2].to_vec();
        assert_eq!(run(&mut intent, &less), vec![(WindowRole::CleanFeed, Change::Lost)]);
        assert_eq!(intent.placed_on(WindowRole::Scopes), Some(&DisplayRef::of(&ms[1])));
        assert_eq!(run(&mut intent, &ms), vec![(WindowRole::CleanFeed, Change::Restored)]);
    }

    #[test]
    fn closing_and_reopening_a_window_places_it_again() {
        let full = two();
        let mut intent = Intent::from_spec(&Preset::Dual.spec());
        run(&mut intent, &full);
        intent.remove(WindowRole::Viewer);
        assert!(!intent.has(WindowRole::Viewer));
        assert_eq!(run(&mut intent, &full), vec![]);
        let t = intent.default_target(WindowRole::Viewer);
        assert_eq!(t, DisplayTarget::Slot(2));
        intent.set(WindowRole::Viewer, t);
        assert_eq!(run(&mut intent, &full), vec![(WindowRole::Viewer, Change::Placed)]);
    }

    #[test]
    fn switching_workspace_keeps_windows_that_stay() {
        let full = two();
        let mut old = Intent::from_spec(&Preset::DualScopes.spec());
        run(&mut old, &full);
        let mut new = Intent::from_spec(&Preset::Dual.spec());
        new.adopt_placement(&old);
        assert_eq!(run(&mut new, &full), vec![], "the viewer stays where it is");
        let mut other = Intent::from_spec(&Preset::DualScopesClean.spec());
        other.adopt_placement(&old);
        // The scopes window already sits on the primary display (no display 3) and stays; only the new clean feed is placed.
        assert_eq!(run(&mut other, &full), vec![(WindowRole::CleanFeed, Change::Missing)]);
    }

    #[test]
    fn default_targets_avoid_used_slots() {
        let mut intent = Intent::default();
        intent.set(WindowRole::CleanFeed, DisplayTarget::Slot(2));
        assert_eq!(intent.default_target(WindowRole::Viewer), DisplayTarget::Slot(3), "2 is taken, 3 is free");
        intent.set(WindowRole::Viewer, DisplayTarget::Slot(3));
        assert_eq!(intent.default_target(WindowRole::Scopes), DisplayTarget::Slot(4));
        assert_eq!(intent.default_target(WindowRole::CleanFeed), DisplayTarget::Slot(2), "its own slot does not count");
    }

    #[test]
    fn describe_names_the_window() {
        let a = Action { role: WindowRole::Scopes, placement: Placement::Fallback, change: Change::Lost };
        assert!(describe(&a).unwrap().starts_with("Scopes:"));
        assert!(describe(&Action { change: Change::Placed, ..a }).is_none());
    }

    // ---- store

    #[test]
    fn store_save_rename_delete_default() {
        let mut s = WorkspaceStore::default();
        let spec = Preset::DualScopes.spec();
        assert_eq!(s.save_as("  Grading  ", spec.clone()).unwrap(), "Grading");
        assert_eq!(s.resolve("grading"), Some(spec.clone()));
        assert!(s.save_as("dual", spec.clone()).is_err(), "built-in name");
        assert!(s.save_as("Dual + Scopes", spec.clone()).is_err(), "built-in display name");
        assert!(s.save_as("   ", spec.clone()).is_err());
        assert!(s.save_as(&"x".repeat(MAX_NAME + 1), spec.clone()).is_err());
        assert!(s.save_as("a\nb", spec.clone()).is_err());
        // Saving under an existing name replaces it.
        s.save_as("GRADING", Preset::Dual.spec()).unwrap();
        assert_eq!(s.custom.len(), 1);
        assert_eq!(s.resolve("Grading"), Some(Preset::Dual.spec()));
        s.save_as("Review", spec.clone()).unwrap();
        s.set_default(Some("Review")).unwrap();
        assert!(s.is_default("review"));
        assert_eq!(s.rename("Review", "Client review").unwrap(), "Client review");
        assert!(s.is_default("Client review"), "the default follows a rename");
        assert!(s.rename("Client review", "grading").is_err(), "name taken");
        assert!(s.rename("Nope", "x").is_err());
        assert!(s.rename("Client review", "single").is_err());
        assert!(s.delete("client review"));
        assert_eq!(s.default, None, "deleting the default clears it");
        assert!(!s.delete("client review"));
        s.set_default(Some("Dual + Scopes")).unwrap();
        assert_eq!(s.default.as_deref(), Some("dual-scopes"), "presets are stored by slug");
        assert!(s.set_default(Some("missing")).is_err());
        s.set_default(None).unwrap();
        assert_eq!(s.default, None);
    }

    #[test]
    fn store_round_trip_through_json_and_old_settings() {
        let mut s = WorkspaceStore::default();
        let mut spec = Preset::DualScopesClean.spec();
        spec.windows[1].target = DisplayTarget::Display(DisplayRef { name: "LG UltraFine".into(), size: [3840, 2160], pos: [-3840, 0], device: None });
        spec.layout.show_inspector = false;
        spec.layout.scopes_pos = ScopesPos::RightColumn;
        s.save_as("Wall", spec.clone()).unwrap();
        s.set_default(Some("Wall")).unwrap();
        s.session = Some(Preset::CleanOnly.spec());
        s.prefer(WindowRole::CleanFeed, Some(DisplayTarget::Slot(3)));
        s.prefer(WindowRole::CleanFeed, Some(DisplayTarget::Slot(4)));
        assert_eq!(s.preferred_target(WindowRole::CleanFeed), Some(&DisplayTarget::Slot(4)), "one entry per role");
        let back = WorkspaceStore::from_json(&s.to_json());
        assert_eq!(back, s);
        assert_eq!(back.resolve("wall"), Some(spec));
        // Empty and partial documents (settings written before workspaces existed) still load.
        assert_eq!(WorkspaceStore::from_json("{}"), WorkspaceStore::default());
        assert_eq!(WorkspaceStore::from_json(""), WorkspaceStore::default(), "empty text");
        assert_eq!(WorkspaceStore::from_json("not json at all"), WorkspaceStore::default());
        assert_eq!(WorkspaceStore::from_json("[1,2]"), WorkspaceStore::default());
        let partial = WorkspaceStore::from_json(r#"{"custom":[{"name":"P","spec":{"windows":[{"role":"Scopes","target":{"Slot":3}}]}}]}"#);
        let p = partial.resolve("p").unwrap();
        assert_eq!(p.layout, MainLayout::default());
        assert!(p.has(WindowRole::Scopes));
    }

    #[test]
    fn one_unreadable_entry_loses_only_itself() {
        let text = r#"{
            "custom": [
                {"name": "Good", "spec": {"windows": [{"role": "Viewer", "target": {"Slot": 2}}]}},
                {"name": "FromNewerBuild", "spec": {"windows": [
                    {"role": "Waveform3D", "target": {"Slot": 3}},
                    {"role": "Scopes", "target": {"Teleport": [1, 2]}},
                    {"role": "CleanFeed", "target": {"Slot": 4}},
                    {"role": "CleanFeed", "target": {"Slot": 2}},
                    {"role": "Scopes", "target": {"Slot": 0}},
                    {"role": "Scopes", "target": {"Slot": 1}}
                ], "layout": {"scopes_pos": "SomethingNew"}}},
                {"name": 17},
                {"spec": {}},
                "garbage",
                {"name": "Display", "spec": {"windows": [{"role": "Scopes", "target": {"Display": {"name": "LG", "size": [1920, 1080], "pos": [0, 0]}}}]}}
            ],
            "default": "Good",
            "session": {"windows": [{"role": "Viewer", "target": {"Slot": 2}}, {"role": 5}]},
            "preferred": [{"role": "Scopes", "target": {"Slot": 3}}, {"nonsense": true}, {"role": "Scopes", "target": {"Slot": 4}}]
        }"#;
        let s = WorkspaceStore::from_json(text);
        let names: Vec<&str> = s.custom.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["Good", "FromNewerBuild", "Display"]);
        assert_eq!(s.default.as_deref(), Some("Good"));
        let newer = s.resolve("FromNewerBuild").unwrap();
        assert_eq!(newer.windows, vec![WindowSpec { role: WindowRole::CleanFeed, target: DisplayTarget::Slot(4) }], "unknown role/target, duplicate role and slots 0/1 dropped");
        assert_eq!(newer.layout, MainLayout::default(), "an unreadable layout falls back to the default one");
        assert_eq!(s.session.as_ref().map(|x| x.windows.len()), Some(1));
        assert_eq!(s.preferred_target(WindowRole::Scopes), Some(&DisplayTarget::Slot(3)));
        match &s.resolve("Display").unwrap().windows[0].target {
            DisplayTarget::Display(d) => assert_eq!(d.device, None, "settings written before device identities still load"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_remembered_device_round_trips() {
        let mut s = WorkspaceStore::default();
        let mut spec = Preset::Dual.spec();
        spec.windows[0].target = DisplayTarget::Display(DisplayRef { name: "DELL U2720Q".into(), size: [2560, 1440], pos: [0, 0], device: Some("\\\\?\\DISPLAY#DELA0B1#5&2&UID4352".into()) });
        s.save_as("Dev", spec.clone()).unwrap();
        assert_eq!(WorkspaceStore::from_json(&s.to_json()).resolve("dev"), Some(spec));
    }

    #[test]
    fn sanitize_drops_unusable_entries_and_clamps_layout() {
        let mut s = WorkspaceStore::default();
        let mut bad = Preset::Dual.spec();
        bad.layout.filmstrip_w = f32::NAN;
        bad.layout.inspector_w = 1e9;
        bad.layout.graph_frac = -3.0;
        s.custom = vec![
            NamedWorkspace { name: "".into(), spec: Preset::Dual.spec() },
            NamedWorkspace { name: "dual".into(), spec: Preset::Dual.spec() },
            NamedWorkspace { name: "ok".into(), spec: bad },
            NamedWorkspace { name: "OK".into(), spec: Preset::Single.spec() },
        ];
        s.default = Some("gone".into());
        s.sanitize();
        assert_eq!(s.custom.len(), 1);
        assert_eq!(s.default, None);
        let l = &s.custom[0].spec.layout;
        assert_eq!((l.filmstrip_w, l.inspector_w, l.graph_frac), (232.0, 900.0, 0.1));
        let docked = MainLayout { scopes_pos: ScopesPos::Window, ..MainLayout::default() }.sanitized();
        assert_eq!(docked.scopes_pos, ScopesPos::BottomRight, "a window is never the docked fallback");
    }

    #[test]
    fn same_windows_ignores_panel_sizes() {
        let a = Preset::DualScopes.spec();
        let mut b = a.clone();
        b.layout.filmstrip_w = 300.0;
        b.windows.reverse();
        assert!(a.same_windows(&b));
        b.windows[0].target = DisplayTarget::Slot(9);
        assert!(!a.same_windows(&b));
    }
}
