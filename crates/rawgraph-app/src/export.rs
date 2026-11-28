//! Export dialog state, the mapping to `rawgraph_batch::ExportSettings`, the
//! background export job and the report. No UI code except `show_dialog` /
//! `show_report`, so the rules are unit-testable.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use rawgraph_batch::{ExportError, ExportFormat, ExportItem, ExportReport, ExportSettings, ItemStatus, OnConflict, SheetBinding};
use rawgraph_core::{Graph, ImageId, NodeId, NodeKind, PortType};
use rawgraph_engine::CancelToken;
use rawgraph_scopes::DisplayTransform;

use crate::session::{Session, ViewSettings};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Selected,
    Filtered,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    Jpeg,
    Png,
    Tiff,
    Exr,
    Png16,
    Tiff16,
    Webp,
    /// Needs the operating system's HEVC encoder (see [`heic_status`]).
    Heic,
}

struct HeicState {
    result: Option<Result<(), String>>,
    checked: Option<std::time::Instant>,
    probing: bool,
}

static HEIC: std::sync::Mutex<HeicState> = std::sync::Mutex::new(HeicState { result: None, checked: None, probing: false });

fn start_heic_probe(s: &mut HeicState) {
    if s.probing {
        return;
    }
    s.probing = true;
    let spawned = std::thread::Builder::new().name("rawgraph-heic-probe".into()).spawn(|| {
        let result = rawgraph_io::heic_available();
        if let Ok(mut s) = HEIC.lock() {
            s.result = Some(result);
            s.checked = Some(std::time::Instant::now());
            s.probing = false;
        }
    });
    if spawned.is_err() {
        s.probing = false;
    }
}

/// Whether the system HEIC encoder works: `None` while the first check (a trial encode on a worker
/// thread, so the first look never stalls the window) is still running. A success stays; a failure
/// is looked at again in the background once it is 30 seconds old (the extensions can be installed
/// while RawGraph runs) and whenever [`heic_recheck`] is called.
pub fn heic_status() -> Option<Result<(), String>> {
    let mut s = HEIC.lock().ok()?;
    let stale = matches!(s.result, Some(Err(_))) && s.checked.is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(30));
    if s.result.is_none() || stale {
        start_heic_probe(&mut s);
    }
    s.result.clone()
}

/// "Check again": probes the encoder now.
pub fn heic_recheck() {
    if let Ok(mut s) = HEIC.lock() {
        start_heic_probe(&mut s);
    }
}

impl Fmt {
    pub const ALL: [Fmt; 8] = [Fmt::Jpeg, Fmt::Png, Fmt::Png16, Fmt::Tiff, Fmt::Tiff16, Fmt::Exr, Fmt::Webp, Fmt::Heic];

    pub fn label(self) -> &'static str {
        match self {
            Fmt::Jpeg => "JPEG",
            Fmt::Webp => "WebP",
            Fmt::Heic => "HEIC",
            Fmt::Png => "PNG",
            Fmt::Tiff => "TIFF",
            Fmt::Exr => "EXR (linear)",
            Fmt::Png16 => "PNG 16-bit",
            Fmt::Tiff16 => "TIFF 16-bit",
        }
    }

    /// Whether the sRGB profile and the capture metadata options apply (EXR stays linear and plain).
    pub fn has_profile_options(self) -> bool {
        self != Fmt::Exr
    }
}

/// Everything the export dialog edits. Kept for the whole session (not saved
/// in the graph or the project).
#[derive(Clone, Debug, PartialEq)]
pub struct ExportState {
    pub scope: Scope,
    pub format: Fmt,
    pub quality: u8,
    pub webp_lossless: bool,
    pub full_size: bool,
    pub long_edge: u32,
    /// Empty until the user picks one: the default is never a source folder.
    pub out_dir: String,
    pub template: String,
    pub on_conflict: OnConflict,
    pub apply_tone_map: bool,
    /// "Embed sRGB profile" (default on).
    pub embed_profile: bool,
    /// "Copy capture metadata" (default on).
    pub copy_metadata: bool,
    /// The loaded caption sheet (for `{sheet_name}` / `{caption}` in the name template); set by the app.
    pub sheet: Option<Arc<SheetBinding>>,
}

impl Default for ExportState {
    fn default() -> Self {
        Self {
            scope: Scope::Selected,
            format: Fmt::Jpeg,
            quality: 92,
            webp_lossless: false,
            full_size: true,
            long_edge: 2048,
            out_dir: String::new(),
            template: "{name}".to_owned(),
            on_conflict: OnConflict::Rename,
            apply_tone_map: true,
            embed_profile: true,
            copy_metadata: true,
            sheet: None,
        }
    }
}

/// Quick Export requires an explicit viewed image node (legacy Output is supported).
pub fn export_node(session: &Session) -> Option<NodeId> {
    let g = &session.project.graph;
    session.target_node().and_then(|id| g.node(id)).filter(|n| n.kind == NodeKind::Output || n.kind.output() == Some(PortType::Images)).map(|n| n.id)
}

pub fn node_label(graph: &Graph, node: NodeId) -> String {
    graph.node(node).map_or_else(|| "(none)".to_owned(), |n| n.kind.label().to_owned())
}

impl ExportState {
    /// Items of the chosen scope, in list order.
    pub fn items(&self, session: &Session) -> Vec<ExportItem> {
        let ids: Vec<ImageId> = match self.scope {
            Scope::Selected => session.selected.into_iter().collect(),
            Scope::Filtered => session.visible().to_vec(),
            Scope::All => session.project.images().iter().map(|e| e.id).collect(),
        };

        let mut ids = ids;
        if self.scope != Scope::Selected {
            ids.extend(Self::generated_items(session));
        }
        ids.into_iter().map(|id| ExportItem { id, source: session.entry(id).map(|e| e.path.clone()).unwrap_or_default(), keywords: session.entry(id).map(|e| e.keywords.clone()).unwrap_or_default() }).collect()
    }

    fn generated_items(session: &Session) -> Vec<ImageId> {
        let Some(node) = export_node(session) else { return Vec::new(); };
        let ids: Vec<_> = session.project.images().iter().map(|entry| entry.id).collect();
        let capture = |id| session.entry(id).map(crate::session::capture_with_path);
        let catalog = rawgraph_core::Catalog { images: &ids, capture: &capture };
        session.project.graph.members(node, 0, &catalog).into_iter()
            .filter(|id| session.entry(*id).is_none()).collect()
    }

    /// Image counts for the three scopes: (selected, filtered, all).
    pub fn scope_counts(session: &Session) -> (usize, usize, usize) {
        let generated = Self::generated_items(session).len();
        (usize::from(session.selected.is_some()), session.visible().len() + generated, session.total_images() + generated)
    }

    /// The display transform used for the export. The viewing exposure is a
    /// viewing aid and is never exported; the tone map only if asked for.
    pub fn display(&self, view: &ViewSettings) -> DisplayTransform {
        let t = view.transform();
        DisplayTransform { exposure_ev: 0.0, tone_map: if self.apply_tone_map { t.tone_map } else { rawgraph_scopes::ToneMap::Clip } }
    }

    // `..Default::default()` keeps this building when the batch crate adds an option.
    #[allow(clippy::needless_update)]
    pub fn settings(&self, view: &ViewSettings) -> Result<ExportSettings, String> {
        if self.out_dir.trim().is_empty() {
            return Err("Choose an output folder".to_owned());
        }
        if self.format == Fmt::Heic {
            if let Some(Err(why)) = heic_status() {
                return Err(why);
            }
        }
        Ok(ExportSettings {
            out_dir: PathBuf::from(self.out_dir.trim()),
            format: match self.format {
                Fmt::Jpeg => ExportFormat::Jpeg { quality: self.quality.clamp(1, 100) },
                Fmt::Webp => ExportFormat::Webp { quality: self.quality.clamp(1, 100), lossless: self.webp_lossless },
                Fmt::Heic => ExportFormat::Heic { quality: self.quality.clamp(1, 100) },
                Fmt::Png => ExportFormat::Png,
                Fmt::Tiff => ExportFormat::Tiff,
                Fmt::Exr => ExportFormat::Exr,
                Fmt::Png16 => ExportFormat::Png16,
                Fmt::Tiff16 => ExportFormat::Tiff16,
            },
            name_template: self.template.clone(),
            long_edge: (!self.full_size).then_some(self.long_edge.max(1)),
            on_conflict: self.on_conflict,
            display: self.display(view),
            generator_view_tone: view.tone.tone_map(),
            embed_profile: self.embed_profile,
            copy_metadata: self.copy_metadata,
            sheet: self.sheet.clone(),
            ..Default::default()
        })
    }

    /// File name of the first resulting file, or the reason it cannot be planned.
    pub fn preview(&self, view: &ViewSettings, items: &[ExportItem]) -> Result<String, String> {
        let settings = self.settings(view)?;
        let first = items.first().ok_or_else(|| "There is nothing to export in this scope".to_owned())?;
        rawgraph_batch::planned_path(first, 1, &settings)
            .map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .map_err(|e| e.to_string())
    }

    /// Warns when the output folder is the folder of a source file.
    pub fn writes_next_to_sources(&self, items: &[ExportItem]) -> bool {
        let out = PathBuf::from(self.out_dir.trim());
        !self.out_dir.trim().is_empty() && items.iter().any(|i| i.source.parent() == Some(out.as_path()))
    }
}

/// Count of report entries by outcome: (written, skipped, excluded, failed, cancelled).
pub fn counts(report: &ExportReport) -> (usize, usize, usize, usize, usize) {
    (report.written(), report.skipped(), report.excluded(), report.failed(), report.cancelled())
}

pub fn summarize(report: &ExportReport) -> String {
    let (w, s, x, f, c) = counts(report);
    let mut text = format!("Export finished: {w} written, {s} skipped, {x} excluded, {f} failed, {c} cancelled");
    if let Some((_, first)) = report.warnings.first() {
        text.push_str(&format!("; {} warning(s), e.g. {first}", report.warnings.len()));
    }
    text
}

pub enum ExportMsg {
    Progress { done: usize, total: usize },
    Done(Result<ExportReport, ExportError>),
}

/// What an export through an Export node needs besides the images to write: every image of the
/// project (Source folders, Merge name rules and Layer pairing are resolved against them) and the
/// display transform.
#[derive(Clone)]
pub struct NodeRun {
    pub catalog: Vec<ExportItem>,
    pub display: DisplayTransform,
    pub members: Vec<ImageId>,
    pub culling: std::collections::HashMap<ImageId, (u8, rawgraph_project::Flag, Option<rawgraph_project::ColorLabel>)>,
    /// The loaded caption sheet, for the name tokens of the node's template.
    pub sheet: Option<Arc<SheetBinding>>,
}

/// A running export. The graph is cloned, so the app stays fully usable.
pub struct ExportJob {
    pub rx: Receiver<ExportMsg>,
    cancel: CancelToken,
    pub done: usize,
    pub total: usize,
    pub items: Vec<ExportItem>,
    pub settings: ExportSettings,
    pub node: NodeId,
    pub started: std::time::Instant,
    /// Set for an export through an Export node (the settings then come from the node).
    pub node_run: Option<NodeRun>,
    pub quick_catalog: Vec<ExportItem>,
}

impl ExportJob {
    pub fn start(ctx: egui::Context, graph: Graph, node: NodeId, items: Vec<ExportItem>, settings: ExportSettings, catalog: Vec<ExportItem>) -> Self {
        let (tx, rx) = channel();
        let cancel = CancelToken::new();
        let (c2, i2, s2, catalog2) = (cancel.clone(), items.clone(), settings.clone(), catalog.clone());
        let total = items.len();
        let spawned = std::thread::Builder::new().name("rawgraph-export".into()).spawn(move || {
            let tx_p = tx.clone();
            let ctx_p = ctx.clone();
            let result = rawgraph_batch::export_catalog(&graph, node, &catalog2, &i2, &s2, &c2, &mut |p| {
                let _ = tx_p.send(ExportMsg::Progress { done: p.done, total: p.total });
                ctx_p.request_repaint();
            });
            let _ = tx.send(ExportMsg::Done(result));
            ctx.request_repaint();
        });
        drop(spawned);
        Self { rx, cancel, done: 0, total, items, settings, node, started: std::time::Instant::now(), node_run: None, quick_catalog: catalog }
    }

    /// Runs the Export node `node`: writes `items` (a subset of the run's catalog) with the node's
    /// own settings. `settings` is only what the report shows (folder), taken from the node.
    pub fn start_node(ctx: egui::Context, graph: Graph, node: NodeId, items: Vec<ExportItem>, run: NodeRun) -> Self {
        let (tx, rx) = channel();
        let cancel = CancelToken::new();
        let (c2, run2) = (cancel.clone(), run.clone());
        let selection: Vec<ImageId> = items.iter().map(|i| i.id).collect();
        let settings = node_settings(&graph, node, items.first().map_or(ImageId(0), |i| i.id), run.display);
        let total = items.len();
        let spawned = std::thread::Builder::new().name("rawgraph-export".into()).spawn(move || {
            let tx_p = tx.clone();
            let ctx_p = ctx.clone();
            let is_xmp = graph.node(node).is_some_and(|n| selection.iter().any(|id| rawgraph_core::export_spec(n, *id).format == rawgraph_core::ExportFileFormat::Xmp));
            let result = if is_xmp {
                Ok(crate::xmp_export::run(&graph, node, &run2, &selection, &c2, &mut |done, total| {
                    let _ = tx_p.send(ExportMsg::Progress { done, total });
                    ctx_p.request_repaint();
                }))
            } else { rawgraph_batch::export_node_sheet(&graph, node, &run2.catalog, &selection, run2.display, run2.sheet.clone(), &c2, &mut |p| {
                let _ = tx_p.send(ExportMsg::Progress { done: p.done, total: p.total });
                ctx_p.request_repaint();
            }) };
            let _ = tx.send(ExportMsg::Done(result));
            ctx.request_repaint();
        });
        drop(spawned);
        Self { rx, cancel, done: 0, total, items, settings, node, started: std::time::Instant::now(), node_run: Some(run), quick_catalog: Vec::new() }
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// The settings an Export node gives `image` (for the report: folder and format).
pub fn node_settings(graph: &Graph, node: NodeId, image: ImageId, display: DisplayTransform) -> ExportSettings {
    match graph.node(node) {
        Some(n) => rawgraph_batch::settings_from_spec(&rawgraph_core::export_spec(n, image), display),
        None => ExportSettings::default(),
    }
}

/// A finished export, kept for the report window.
pub struct FinishedExport {
    pub report: ExportReport,
    pub items: Vec<ExportItem>,
    pub settings: ExportSettings,
    pub node: NodeId,
    pub node_run: Option<NodeRun>,
    pub quick_catalog: Vec<ExportItem>,
}

/// The display transform of an export through an Export node: the same one the export dialog
/// uses (view tone map applied, the viewing exposure never exported).
pub fn node_display(view: &ViewSettings) -> DisplayTransform {
    ExportState::default().display(view)
}

/// One Export node as the "Run Export nodes" list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportNodeEntry {
    pub node: NodeId,
    pub label: String,
    pub run: bool,
    /// Why it cannot run, if it cannot.
    pub problem: Option<String>,
}

/// Every Export node of the graph with a short description, for the "Run Export nodes" dialog.
pub fn export_nodes(graph: &Graph, image: ImageId) -> Vec<ExportNodeEntry> {
    let mut out = Vec::new();
    for (i, n) in graph.nodes().filter(|n| n.kind == NodeKind::Export).enumerate() {
        let spec = rawgraph_core::export_spec(n, image);
        let dir = if spec.text.out_dir.trim().is_empty() { "no folder".to_owned() } else { crate::node_ui::folder_name(&spec.text.out_dir) };
        let problem = if spec.text.out_dir.trim().is_empty() {
            Some("no output folder set".to_owned())
        } else if graph.input_of(n.id, 0).is_none() {
            Some("no input is connected".to_owned())
        } else {
            crate::node_ui::export_conflict(spec.format, spec.space).map(str::to_owned)
        };
        out.push(ExportNodeEntry { node: n.id, label: format!("Export {} \u{2014} {}, {}", i + 1, spec.format.label(), dir), run: problem.is_none(), problem });
    }
    out
}

/// Every project image as an export item, in project order (the catalog of a node export).
pub fn catalog_items(session: &Session) -> Vec<ExportItem> {
    if session.demo {
        return Vec::new();
    }
    session.project.images().iter().map(|e| ExportItem { id: e.id, source: e.path.clone(), keywords: e.keywords.clone() }).collect()
}

/// The images a node export writes for `scope`: the node's collection, limited to the selected
/// image, to the filmstrip's filtered list, or all of it. Order: filmstrip order for the filtered
/// list, collection order otherwise.
pub fn node_selection(session: &mut Session, node: NodeId, scope: Scope) -> Vec<ImageId> {
    let members = node_members(session, node);
    match scope {
        Scope::Selected => session.selected.filter(|id| members.contains(id)).into_iter().collect(),
        Scope::Filtered => {
            let set: std::collections::HashSet<ImageId> = members.iter().copied().collect();
            let mut ids: Vec<_> = session.visible().iter().copied().filter(|id| set.contains(id)).collect();
            ids.extend(members.iter().copied().filter(|id| session.entry(*id).is_none()));
            ids
        }
        Scope::All => members,
    }
}

/// The collection that reaches an Export node (its input), in the order the graph defines.
pub fn node_members(session: &mut Session, node: NodeId) -> Vec<ImageId> {
    let (ids, caps) = session.id_captures();
    let capture = |i: ImageId| caps.get(&i).cloned();
    let catalog = rawgraph_core::Catalog { images: &ids, capture: &capture };
    session.project.graph.members(node, 0, &catalog)
}

/// Items and run of a node export for `scope`.
pub fn node_plan(session: &mut Session, node: NodeId, scope: Scope) -> (Vec<ExportItem>, NodeRun) {
    let catalog = catalog_items(session);
    let selection = node_selection(session, node, scope);
    let by_id: std::collections::HashMap<ImageId, &ExportItem> = catalog.iter().map(|i| (i.id, i)).collect();
    let items: Vec<ExportItem> = selection.iter().map(|id| by_id.get(id).map(|i| (*i).clone()).unwrap_or_else(|| ExportItem { id: *id, source: Default::default(), keywords: Vec::new() })).collect();
    let display = node_display(&session.view);
    let members = node_members(session, node);
    let culling = session.project.images().iter().map(|e| (e.id, (e.rating, e.flag, e.color_label))).collect();
    (items, NodeRun { catalog, display, members, culling, sheet: session.caption_sheet.clone() })
}

/// Whether `out_dir` is the folder of any source photo of `catalog`: an export never writes there.
pub fn writes_into_sources(catalog: &[ExportItem], out_dir: &str) -> bool {
    let out = crate::session::folder_key(out_dir);
    !out.is_empty() && catalog.iter().any(|i| i.source.parent().is_some_and(|p| crate::session::folder_key(&p.to_string_lossy()) == out))
}

/// The reason an export through `node` cannot start, if there is one: no output folder, or an output
/// folder that holds source photos.
pub fn node_blocker(session: &Session, node: NodeId) -> Option<String> {
    let n = session.project.graph.node(node)?;
    let spec = rawgraph_core::export_spec(n, session.current_image());
    if spec.text.out_dir.trim().is_empty() {
        return Some("The Export node has no output folder".to_owned());
    }
    writes_into_sources(&catalog_items(session), spec.text.out_dir.trim()).then(|| "The output folder holds source photos; an export never writes there. Choose another folder.".to_owned())
}

/// State of the export dialog in node mode.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeExportState {
    pub node: NodeId,
    pub scope: Scope,
    /// Images per scope when the dialog was opened: (selected, filtered, all).
    pub counts: (usize, usize, usize),
}

impl NodeExportState {
    pub fn open(session: &mut Session, node: NodeId) -> Self {
        let counts = (
            node_selection(session, node, Scope::Selected).len(),
            node_selection(session, node, Scope::Filtered).len(),
            node_selection(session, node, Scope::All).len(),
        );
        // The most useful scope first: the selected image when it is in the collection.
        let scope = if counts.0 > 0 { Scope::Selected } else { Scope::Filtered };
        Self { node, scope, counts }
    }

    pub fn count(&self) -> usize {
        match self.scope {
            Scope::Selected => self.counts.0,
            Scope::Filtered => self.counts.1,
            Scope::All => self.counts.2,
        }
    }
}

/// The node-mode export dialog: the node's settings read-only, the scope, Export / Close.
pub fn show_node_dialog(ctx: &egui::Context, area: egui::Rect, st: &mut NodeExportState, lines: &[(String, String)], busy: bool, blocker: Option<&str>) -> DialogAction {
    let mut action = DialogAction::None;
    let mut open = true;
    let window = egui::Window::new("Export through the Export node").open(&mut open).collapsible(false).resizable(false).default_width(480.0);
    let window = if area.is_positive() { window.constrain_to(area).default_pos(area.center() - egui::vec2(240.0, 200.0)) } else { window };
    window.show(ctx, |ui| {
        ui.label(RichText::new("The settings come from the node (change them in its inspector):").weak());
        egui::Grid::new("node_export_grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            for (k, v) in lines {
                ui.label(k.as_str());
                ui.label(v.as_str());
                ui.end_row();
            }
        });
        ui.separator();
        ui.label("Images of the node's collection");
        ui.radio_value(&mut st.scope, Scope::Selected, format!("Selected image ({})", st.counts.0));
        ui.radio_value(&mut st.scope, Scope::Filtered, format!("Filtered list ({})", st.counts.1));
        ui.radio_value(&mut st.scope, Scope::All, format!("All images ({})", st.counts.2));
        if let Some(b) = blocker {
            ui.add_space(4.0);
            ui.label(RichText::new(b).color(Color32::from_rgb(255, 150, 120)));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let n = st.count();
            let ok = n > 0 && !busy && blocker.is_none();
            if ui.add_enabled(ok, egui::Button::new(format!("Export {n} image{}", if n == 1 { "" } else { "s" }))).clicked() {
                action = DialogAction::Start;
            }
            if ui.button("Close").clicked() {
                action = DialogAction::Close;
            }
            if busy {
                ui.label(RichText::new("An export is running").weak());
            }
        });
    });
    if !open {
        action = DialogAction::Close;
    }
    action
}

/// State of "Run Export nodes...".
#[derive(Clone, Debug, PartialEq)]
pub struct RunNodesState {
    pub entries: Vec<ExportNodeEntry>,
    pub scope: Scope,
}

pub enum RunAction {
    None,
    Run,
    Close,
}

/// The "Run Export nodes" dialog: every Export node with a checkbox, and the scope for all of them.
pub fn show_run_nodes_dialog(ctx: &egui::Context, area: egui::Rect, st: &mut RunNodesState, busy: bool) -> RunAction {
    let mut action = RunAction::None;
    let mut open = true;
    let window = egui::Window::new("Run Export nodes").open(&mut open).collapsible(false).resizable(false).default_width(480.0);
    let window = if area.is_positive() { window.constrain_to(area).default_pos(area.center() - egui::vec2(240.0, 160.0)) } else { window };
    window.show(ctx, |ui| {
        if st.entries.is_empty() {
            ui.label("The graph has no Export node. Add one from the node menu and connect it.");
        }
        for e in &mut st.entries {
            ui.horizontal(|ui| {
                ui.add_enabled(e.problem.is_none(), egui::Checkbox::new(&mut e.run, e.label.as_str()));
                if let Some(p) = &e.problem {
                    ui.label(RichText::new(p.as_str()).color(Color32::from_rgb(255, 190, 90)));
                }
            });
        }
        ui.separator();
        ui.label("Images");
        ui.radio_value(&mut st.scope, Scope::Selected, "Selected image");
        ui.radio_value(&mut st.scope, Scope::Filtered, "Filtered list");
        ui.radio_value(&mut st.scope, Scope::All, "All images of each node's collection");
        ui.label(RichText::new("The chosen nodes run one after another.").weak());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let n = st.entries.iter().filter(|e| e.run && e.problem.is_none()).count();
            if ui.add_enabled(n > 0 && !busy, egui::Button::new(format!("Run {n} node{}", if n == 1 { "" } else { "s" }))).clicked() {
                action = RunAction::Run;
            }
            if ui.button("Close").clicked() {
                action = RunAction::Close;
            }
        });
    });
    if !open {
        action = RunAction::Close;
    }
    action
}

pub enum DialogAction {
    None,
    PickFolder,
    Start,
    Close,
}

/// What the dialog needs to know besides its own state.
pub struct DialogInfo<'a> {
    pub counts: (usize, usize, usize),
    pub node_text: &'a str,
    pub items: &'a [ExportItem],
    pub busy: bool,
    pub demo: bool,
}

/// The export dialog.
pub fn show_dialog(ctx: &egui::Context, area: egui::Rect, st: &mut ExportState, view: &ViewSettings, info: &DialogInfo) -> DialogAction {
    let DialogInfo { counts, node_text, items, busy, demo } = *info;
    let mut action = DialogAction::None;
    let mut open = true;
    let window = egui::Window::new("Export").open(&mut open).collapsible(false).resizable(false).default_width(480.0);
    let window = if area.is_positive() { window.constrain_to(area).default_pos(area.center() - egui::vec2(240.0, 200.0)) } else { window };
    window.show(ctx, |ui| {
        if demo {
            ui.label(RichText::new("The demo images cannot be exported. Open a folder first.").color(Color32::from_rgb(255, 190, 90)));
            return;
        }
        ui.label(format!("Exports the output of: {node_text}"));
        ui.add_space(4.0);
        egui::Grid::new("export_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Images");
            ui.vertical(|ui| {
                ui.radio_value(&mut st.scope, Scope::Selected, format!("Selected image ({})", counts.0));
                ui.radio_value(&mut st.scope, Scope::Filtered, format!("Filtered list ({})", counts.1));
                ui.radio_value(&mut st.scope, Scope::All, format!("All images ({})", counts.2));
            });
            ui.end_row();

            ui.label("Format");
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    for f in Fmt::ALL {
                        if f == Fmt::Heic {
                            let status = heic_status();
                            let why = match &status {
                                Some(Err(why)) => Some(why.clone()),
                                None => Some("Checking whether this computer can write HEIC\u{2026}".to_owned()),
                                Some(Ok(())) => None,
                            };
                            let shown = st.format == Fmt::Heic;
                            ui.add_enabled_ui(why.is_none() || shown, |ui| {
                                let r = ui.selectable_value(&mut st.format, f, f.label());
                                if let Some(why) = &why {
                                    r.on_hover_text(why.as_str()).on_disabled_hover_text(why.as_str());
                                }
                            });
                            if status.is_none() {
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                            }
                        } else {
                            ui.selectable_value(&mut st.format, f, f.label());
                        }
                    }
                });
                if st.format == Fmt::Webp { ui.checkbox(&mut st.webp_lossless, "Lossless WebP"); }
                if let (Fmt::Heic, Some(Err(why))) = (st.format, heic_status()) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(why).color(Color32::from_rgb(255, 150, 120)));
                        if ui.small_button("Check again").clicked() {
                            heic_recheck();
                        }
                    });
                }
                if matches!(st.format, Fmt::Jpeg | Fmt::Heic) || (st.format == Fmt::Webp && !st.webp_lossless) {
                    ui.add(egui::Slider::new(&mut st.quality, 1..=100).text("quality"));
                }
                if matches!(st.format, Fmt::Png16 | Fmt::Tiff16) {
                    ui.label(RichText::new("16 bits per channel, sRGB.").weak());
                }
            });
            ui.end_row();

            ui.label("Size");
            ui.horizontal(|ui| {
                ui.radio_value(&mut st.full_size, true, "Full");
                ui.radio_value(&mut st.full_size, false, "Long edge");
                ui.add_enabled(!st.full_size, egui::DragValue::new(&mut st.long_edge).range(16..=20000).suffix(" px"));
            });
            ui.end_row();

            ui.label("Output folder");
            ui.horizontal(|ui| {
                folder_field(ui, &mut st.out_dir, 280.0);
                if ui.button("Browse...").clicked() {
                    action = DialogAction::PickFolder;
                }
            });
            ui.end_row();

            ui.label("File name");
            ui.add(egui::TextEdit::singleline(&mut st.template).desired_width(280.0)).on_hover_text(crate::node_ui::NAME_TEMPLATE_HELP);
            ui.end_row();

            ui.label("Existing files");
            ui.horizontal(|ui| {
                ui.radio_value(&mut st.on_conflict, OnConflict::Skip, "Skip");
                ui.radio_value(&mut st.on_conflict, OnConflict::Rename, "Rename");
                ui.radio_value(&mut st.on_conflict, OnConflict::Overwrite, "Overwrite");
            });
            ui.end_row();

            ui.label("Files");
            ui.vertical(|ui| {
                ui.add_enabled_ui(st.format.has_profile_options(), |ui| {
                    ui.checkbox(&mut st.embed_profile, "Embed sRGB profile");
                    ui.checkbox(&mut st.copy_metadata, "Copy capture metadata (camera, lens, exposure, date, copyright)");
                });
                if !st.format.has_profile_options() {
                    ui.label(RichText::new("EXR files are linear and carry no profile or capture metadata.").weak());
                }
            });
            ui.end_row();

            ui.label("Look");
            ui.vertical(|ui| {
                ui.checkbox(&mut st.apply_tone_map, "Apply view tone map");
                ui.label(RichText::new("The display exposure slider is a viewing aid and is never exported. EXR is always linear.").weak());
            });
            ui.end_row();
        });
        ui.add_space(6.0);
        match st.preview(view, items) {
            Ok(name) => {
                ui.label(format!("First file: {name}"));
            }
            Err(e) => {
                ui.label(RichText::new(e).color(Color32::from_rgb(255, 150, 120)));
            }
        }
        if st.writes_next_to_sources(items) {
            ui.label(RichText::new("Note: the output folder is also a source folder.").color(Color32::from_rgb(255, 190, 90)));
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let ok = st.settings(view).is_ok() && !items.is_empty() && !busy && st.preview(view, items).is_ok();
            if ui.add_enabled(ok, egui::Button::new(format!("Export {} image{}", items.len(), if items.len() == 1 { "" } else { "s" }))).clicked() {
                action = DialogAction::Start;
            }
            if ui.button("Close").clicked() {
                action = DialogAction::Close;
            }
            if busy {
                ui.label(RichText::new("An export is running").weak());
            }
        });
    });
    if !open {
        action = DialogAction::Close;
    }
    action
}

/// Shortens `text` to at most `max_chars` characters by replacing the middle
/// with an ellipsis, so the start (drive, first folders) and the end (the
/// folder itself) stay visible. Never splits a character.
pub fn elide_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars || max_chars < 5 {
        return text.to_owned();
    }
    let keep = max_chars - 1;
    // The end of a path is the more telling part: give it the larger share.
    let tail = keep.div_ceil(2) + keep / 8;
    let head = keep - tail;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}\u{2026}{end}")
}

/// Single-line path field: while it has no focus a long path is shown elided
/// in the middle, with the full path in a tooltip; focusing it shows the full
/// text for editing.
fn folder_field(ui: &mut egui::Ui, text: &mut String, width: f32) {
    let id = ui.make_persistent_id("export-out-dir");
    let focused = ui.memory(|m| m.has_focus(id));
    let font = egui::TextStyle::Body.resolve(ui.style());
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, 'n')).max(4.0);
    let fits = ((width - 12.0) / char_w).floor().max(8.0) as usize;
    if !focused && text.chars().count() > fits {
        let mut shown = elide_middle(text, fits);
        let r = ui.add(egui::TextEdit::singleline(&mut shown).id(id).desired_width(width).interactive(true));
        // Editing starts from the full path: the first click only focuses.
        r.on_hover_text(text.as_str());
    } else {
        let r = ui.add(egui::TextEdit::singleline(text).id(id).hint_text("choose a folder").desired_width(width));
        if !text.is_empty() {
            r.on_hover_text(text.as_str());
        }
    }
}

pub enum ReportAction {
    None,
    Retry,
    OpenFolder,
    Close,
}

pub fn show_report(ctx: &egui::Context, area: egui::Rect, fin: &FinishedExport) -> ReportAction {
    let mut action = ReportAction::None;
    let mut open = true;
    let window = egui::Window::new("Export report").open(&mut open).default_width(560.0).movable(true);
    // Between the menu bar and the status bar, never over the menu.
    let window = if area.is_positive() { window.constrain_to(area).default_pos(area.center() - egui::vec2(280.0, 160.0)) } else { window };
    window.show(ctx, |ui| {
        let (w, s, x, f, c) = counts(&fin.report);
        egui::Grid::new("export_report_grid").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
            ui.label("Written");
            ui.label(w.to_string());
            ui.end_row();
            ui.label("Skipped (existing)");
            ui.label(s.to_string());
            ui.end_row();
            ui.label("Excluded (not in the collection)");
            ui.label(x.to_string());
            ui.end_row();
            ui.label("Failed");
            ui.label(f.to_string());
            ui.end_row();
            ui.label("Cancelled");
            ui.label(c.to_string());
            ui.end_row();
        });
        let folder = fin.settings.out_dir.display().to_string();
        ui.label(RichText::new(format!("Folder: {}", elide_middle(&folder, 70))).weak()).on_hover_text(folder.as_str());
        let failed: Vec<(ImageId, String)> = fin
            .report
            .items
            .iter()
            .filter_map(|(id, st)| match st {
                ItemStatus::Failed(m) => Some((*id, m.clone())),
                _ => None,
            })
            .collect();
        if !failed.is_empty() {
            ui.separator();
            ui.label(RichText::new("Failed images").strong());
            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                for (id, m) in failed {
                    let name = fin.items.iter().find(|i| i.id == id).map(|i| i.source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).unwrap_or_default();
                    ui.label(format!("{name}: {m}"));
                }
            });
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let retry = fin.report.retry_items(&fin.items);
            if ui.add_enabled(!retry.is_empty(), egui::Button::new(format!("Retry failed ({})", retry.len()))).clicked() {
                action = ReportAction::Retry;
            }
            if ui.button("Open output folder").clicked() {
                action = ReportAction::OpenFolder;
            }
            if ui.button("Close").clicked() {
                action = ReportAction::Close;
            }
        });
    });
    if !open {
        action = ReportAction::Close;
    }
    action
}

/// Opens a folder in the system file browser.
pub fn open_folder(path: &std::path::Path) {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(not(windows))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(path).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::{png_project, provider};
    use rawgraph_project::Flag;

    fn session(n: usize) -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let s = Session::new(png_project(dir.path(), n), provider());
        (dir, s)
    }

    #[test]
    fn empty_source_never_exports_the_catalog() {
        let (_dir, mut s) = session(2);
        let src = s.project.graph.add_node(rawgraph_core::NodeKind::Source, [0.0, 0.0]);
        let out = s.project.graph.add_node(rawgraph_core::NodeKind::Export, [0.0, 0.0]);
        s.project.graph.connect(src, out, 0).unwrap();
        assert!(crate::node_ui::set_source_folder(&mut s.project.graph, src, None));
        assert!(node_members(&mut s, out).is_empty());
        assert!(node_selection(&mut s, out, Scope::All).is_empty());
    }

    #[test]
    fn long_paths_are_elided_in_the_middle() {
        let long = r"\\nas\photos\2026\Kunde Mustermann\Hochzeit Garten und Schloss\ROH\Auswahl final";
        let e = elide_middle(long, 40);
        assert_eq!(e.chars().count(), 40, "{e}");
        assert!(e.contains('\u{2026}'));
        assert!(e.starts_with(r"\\nas\"), "the start stays: {e}");
        assert!(e.ends_with("Auswahl final"), "the last folder stays: {e}");
        assert_eq!(elide_middle("C:/out", 40), "C:/out", "short paths are untouched");
        assert_eq!(elide_middle("", 40), "");
        // Multi-byte characters are never split.
        let umlauts = "C:/Bilder/\u{00fc}\u{00e4}\u{00f6}\u{00df}\u{00fc}\u{00e4}\u{00f6}\u{00df}\u{00fc}\u{00e4}\u{00f6}\u{00df}/Ordner";
        assert!(elide_middle(umlauts, 14).chars().count() <= 14);
    }

    #[test]
    fn default_has_no_output_folder_and_asks_for_one() {
        let st = ExportState::default();
        assert!(st.out_dir.is_empty(), "never default to a source folder");
        assert_eq!(st.settings(&ViewSettings::default()).unwrap_err(), "Choose an output folder");
        assert_eq!(st.on_conflict, OnConflict::Rename);
    }

    #[test]
    fn settings_mapping() {
        let mut st = ExportState { out_dir: "C:/out".into(), format: Fmt::Jpeg, quality: 80, full_size: false, long_edge: 1600, ..Default::default() };
        let s = st.settings(&ViewSettings::default()).unwrap();
        assert_eq!(s.format, ExportFormat::Jpeg { quality: 80 });
        assert_eq!(s.long_edge, Some(1600));
        assert_eq!(s.out_dir, PathBuf::from("C:/out"));
        st.full_size = true;
        st.format = Fmt::Exr;
        let s = st.settings(&ViewSettings::default()).unwrap();
        assert_eq!((s.format, s.long_edge), (ExportFormat::Exr, None));
    }

    #[test]
    fn the_sixteen_bit_formats_and_the_file_options_map_to_the_batch_settings() {
        let view = ViewSettings::default();
        let mut st = ExportState { out_dir: "C:/out".into(), ..Default::default() };
        assert!(st.embed_profile && st.copy_metadata, "both options default to on");
        let s = st.settings(&view).unwrap();
        assert!(s.embed_profile && s.copy_metadata);
        st.format = Fmt::Png16;
        assert_eq!(st.settings(&view).unwrap().format, ExportFormat::Png16);
        st.format = Fmt::Tiff16;
        assert_eq!(st.settings(&view).unwrap().format, ExportFormat::Tiff16);
        st.embed_profile = false;
        st.copy_metadata = false;
        let s = st.settings(&view).unwrap();
        assert!(!s.embed_profile && !s.copy_metadata);
        assert!(Fmt::ALL.contains(&Fmt::Webp));
        assert_eq!(Fmt::ALL.len(), 8);
        st.format = Fmt::Heic;
        let heic = st.settings(&view);
        // The encoder probe runs on a worker thread: until it reports, the settings are accepted;
        // a refusal must carry the reason the system gives.
        match heic {
            Ok(s) => assert_eq!(s.format, ExportFormat::Heic { quality: st.quality }),
            Err(why) => assert_eq!(Err(why), rawgraph_io::heic_available()),
        }
        assert_eq!(Fmt::Heic.label(), "HEIC");
        assert_eq!((Fmt::Png16.label(), Fmt::Tiff16.label()), ("PNG 16-bit", "TIFF 16-bit"));
        assert!(Fmt::Png16.has_profile_options() && !Fmt::Exr.has_profile_options());
        // The file name keeps the format's extension.
        let items = vec![ExportItem { id: ImageId(1), source: "a.arw".into(), keywords: Vec::new() }];
        st.format = Fmt::Tiff16;
        assert_eq!(st.preview(&view, &items).unwrap(), "a.tif");
        st.format = Fmt::Png16;
        assert_eq!(st.preview(&view, &items).unwrap(), "a.png");
    }

    #[test]
    fn view_exposure_is_never_exported_and_tone_map_is_optional() {
        let view = ViewSettings { exposure_ev: 2.5, tone: crate::session::ViewTone::Soft };
        let mut st = ExportState { out_dir: "x".into(), ..Default::default() };
        let d = st.settings(&view).unwrap().display;
        assert_eq!(d.exposure_ev, 0.0, "the display exposure is a viewing aid");
        assert_eq!(d.tone_map, rawgraph_scopes::ToneMap::SoftHighlights);
        st.apply_tone_map = false;
        assert_eq!(st.settings(&view).unwrap().display.tone_map, rawgraph_scopes::ToneMap::Clip);
    }

    #[test]
    fn scope_lists_follow_selection_filter_and_project() {
        let (_d, mut s) = session(5);
        let ids = s.visible().to_vec();
        s.set_rating(ids[1], 5);
        s.set_rating(ids[3], 5);
        s.filter.min_rating = 5;
        s.refresh_visible();
        let mut st = ExportState { scope: Scope::Selected, ..ExportState::default() };
        assert_eq!(st.items(&s).iter().map(|i| i.id).collect::<Vec<_>>(), vec![ids[1]]);
        st.scope = Scope::Filtered;
        assert_eq!(st.items(&s).iter().map(|i| i.id).collect::<Vec<_>>(), vec![ids[1], ids[3]]);
        st.scope = Scope::All;
        assert_eq!(st.items(&s).len(), 5);
        assert_eq!(ExportState::scope_counts(&s), (1, 2, 5));
        let _ = Flag::None;
    }

    #[test]
    fn preview_shows_the_first_name_or_the_error() {
        let (d, s) = session(3);
        let mut st = ExportState { out_dir: "C:/out".into(), template: "{name}_web".into(), format: Fmt::Png, ..Default::default() };
        st.scope = Scope::All;
        let items = st.items(&s);
        assert_eq!(st.preview(&ViewSettings::default(), &items).unwrap(), "img_000_web.png");
        st.template = "bad/name".into();
        assert!(st.preview(&ViewSettings::default(), &items).is_err());
        st.template = String::new();
        assert!(st.preview(&ViewSettings::default(), &items).unwrap_err().contains("empty"));
        st.template = "{name}".into();
        st.out_dir = d.path().to_string_lossy().into_owned();
        assert!(st.writes_next_to_sources(&items), "warn when writing next to the sources");
        assert!(st.preview(&ViewSettings::default(), &[]).is_err());
    }

    #[test]
    fn export_node_requires_an_explicit_image_view() {
        let (_d, mut s) = session(1);
        assert_eq!(export_node(&s), None);
        let out = s.project.graph.add_node(NodeKind::Output, [0.0, 0.0]);
        let src = s.node_by_index(0).unwrap();
        s.viewed_node = Some(src);
        assert_eq!(export_node(&s), Some(src));
        s.viewed_node = Some(out);
        assert_eq!(export_node(&s), Some(out));
    }

    #[test]
    fn report_summary_and_counts() {
        let report = ExportReport {
            items: vec![
                (ImageId(1), ItemStatus::Written { path: "a.jpg".into(), width: 1, height: 1 }),
                (ImageId(2), ItemStatus::SkippedExisting("b.jpg".into())),
                (ImageId(3), ItemStatus::Failed("nope".into())),
                (ImageId(4), ItemStatus::Cancelled),
                (ImageId(5), ItemStatus::Excluded),
            ],
            ..Default::default()
        };
        assert_eq!(counts(&report), (1, 1, 1, 1, 1));
        assert_eq!(summarize(&report), "Export finished: 1 written, 1 skipped, 1 excluded, 1 failed, 1 cancelled");
        let items: Vec<ExportItem> = (1..=5).map(|i| ExportItem { id: ImageId(i), source: format!("{i}.arw").into(), keywords: Vec::new() }).collect();
        assert_eq!(report.retry_items(&items).len(), 2);
    }
}
