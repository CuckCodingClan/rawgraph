//! The eframe application: menus, layouts, shortcuts and the glue between
//! the session (project), the graph editor, the renderer, the scan worker and
//! the secondary windows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers, RichText, ViewportId};
use rawgraph_core::{Graph, ImageId, Mode, NodeId, NodeKind};
use rawgraph_project::{ColorLabel, Flag, Project, ScanReport};
use rawgraph_scope_views::ScopePanel;
use rawgraph_scope_views::PrepareSettings;
use serde::{Deserialize, Serialize};

use crate::autosave::{OwnFiles, Scheduler, WriteJob};
use crate::capture_refresh::CaptureRefresh;
use crate::cli::Cli;
use crate::collection::CollectionWorker;
use crate::compare::{Compare, SharedView};
use crate::datadir::DataDirs;
use crate::dialogs::{RecoveryDialog, RelinkUi};
use crate::graph_editor::EditOp;
use crate::missing_check::MissingCheck;
use crate::recovery_guard::{Orphans, SessionLock};
use crate::thumbs::{ThumbLoader, CACHE_BUDGET};
use crate::xmp_import::{XmpDialog, XmpJob, XmpOffer};
use crate::export::{self, DialogAction, ExportJob, ExportMsg, ExportState, FinishedExport, ReportAction};
use crate::filmstrip::Filmstrip;
use crate::graph_editor::GraphEditor;
use crate::history::History;
use crate::inspector;
use crate::node_ui;
use crate::prefetch::Prefetcher;
use crate::provider::{FileProvider, DEFAULT_CACHE_BYTES, DEFAULT_PREVIEW, PREVIEW_SIZES};
use crate::gpu_path::{RenderPath, RendererMode};
use crate::gpu_real::GpuSetup;
use crate::render::{Frame, GpuConfig, GpuHints, Renderer, Reply};
use crate::scan::{summarize, ScanJob, ScanMsg};
use crate::session::{Session, ViewerTarget, ViewSettings};
use crate::shot::{report, save_png};
use crate::status::Status;
use crate::testimages::display_name;
use crate::tools::Tools;
use crate::viewer::{self, Viewer};
use crate::windows::{self, AuxState, AuxWindow, MonitorInfo};
use crate::workspace::{MainLayout, Preset, WindowRole, WorkspaceStore};

mod ext;
mod ff_driver;
mod nodes;
mod rename;
mod revs;
mod workspaces;
#[cfg(test)]
mod workspaces_tests;

/// Process exit code requested by the app (screenshot failures).
pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

const SETTINGS_KEY: &str = "rawgraph.settings.v2";
/// Workspaces live under their own key (JSON, read leniently) so that one unreadable entry can
/// never reset the other settings.
const WORKSPACES_KEY: &str = "rawgraph.workspaces.v1";
const GRAPH_EXT: &str = ".rawgraph.json";
const PROJECT_EXT: &str = ".rawgraph";
const MIN_FRAMES_BEFORE_SHOT: u64 = 30;
const SHOT_TIMEOUT: Duration = Duration::from_secs(150);
/// Frames to wait before the first render request, so the panels (and with
/// them the scope settings) have their real size.
const SETTLE_FRAMES: u64 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workspace {
    #[default]
    Single,
    Dual,
}

/// Where the scopes live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopesPos {
    #[default]
    BottomRight,
    RightColumn,
    Window,
    Hidden,
}

impl ScopesPos {
    pub const ALL: [ScopesPos; 4] = [ScopesPos::BottomRight, ScopesPos::RightColumn, ScopesPos::Window, ScopesPos::Hidden];

    pub fn label(self) -> &'static str {
        match self {
            ScopesPos::BottomRight => "Bottom right",
            ScopesPos::RightColumn => "Right column",
            ScopesPos::Window => "Separate window",
            ScopesPos::Hidden => "Hidden",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    scopes_pos: ScopesPos,
    workspace: Workspace,
    viewer: AuxState,
    clean: AuxState,
    scopes: AuxState,
    preview_size: u32,
    renderer: RendererMode,
    click_zoom: f32,
    wheel_navigates: bool,
    editable_thumb_gib: u32,
    /// Always write the whole project file instead of updating it in place.
    always_rewrite: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            scopes_pos: ScopesPos::BottomRight,
            workspace: Workspace::Single,
            viewer: AuxState::default(),
            clean: AuxState::default(),
            scopes: AuxState::default(),
            preview_size: DEFAULT_PREVIEW,
            renderer: RendererMode::Auto,
            click_zoom: 2.0,
            wheel_navigates: false,
            editable_thumb_gib: crate::live_thumbs::DEFAULT_GIB,
            always_rewrite: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    NewProject,
    OpenProject,
    OpenFolder,
    Save,
    SaveAs,
    History,
    ImportGraph,
    ExportGraph,
    Export,
    RunExportNodes,
    OpenDemo,
    Quit,
    Undo,
    Redo,
    DeleteNode,
    ViewNode,
    FrameAll,
    Fit,
    Actual,
    SetWorkspace(Workspace),
    Preset(Preset),
    ToggleClean,
    ToggleScopes,
    SetScopesPos(ScopesPos),
    ResetLayout,
    Shortcuts,
    ScanReport,
    ToggleMaskOverlay,
    Copy,
    Cut,
    Paste,
    Duplicate,
    SelectAll,
    SaveRecipe,
    ManageRecipes,
    GroupSelection,
    Ungroup,
    OpenGroup,
    UpOneLevel,
    SaveMacro,
    ImportXmp,
    Compare,
    ToggleMissingOnly,
    ClearThumbCache,
    RenameFiles,
    UndoRename,
}

/// Actions that replace the document and therefore ask about unsaved changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    NewProject,
    OpenProject,
    OpenDemo,
    Quit,
}

enum DialogKind {
    ExportFolder,
    OpenProject,
    OpenFolder,
    SaveAs(Option<Pending>),
    ImportGraph,
    ExportGraph,
    /// Pick the new place of a missing image's file.
    LocateFile(ImageId),
    /// Pick the new place of a moved folder.
    RelinkNewFolder,
    /// Pick the folder a Source node shows.
    SourceFolder(NodeId),
    /// Pick the output folder of an Export node.
    NodeExportFolder(NodeId),
    /// Pick the `.cube` file of a LUT node.
    LutFile(NodeId),
    /// Pick the caption sheet (.xlsx / .csv) of the rename dialog.
    CaptionSheet,
}

struct Dialog {
    kind: DialogKind,
    rx: Receiver<Option<PathBuf>>,
}

fn sc(mods: Modifiers, key: Key) -> KeyboardShortcut {
    KeyboardShortcut::new(mods, key)
}

fn shortcut_of(cmd: Command) -> Option<KeyboardShortcut> {
    let c = Modifiers::COMMAND;
    let cs = Modifiers::COMMAND | Modifiers::SHIFT;
    Some(match cmd {
        Command::NewProject => sc(c, Key::N),
        Command::OpenProject => sc(c, Key::O),
        Command::OpenFolder => sc(cs, Key::O),
        Command::Save => sc(c, Key::S),
        Command::Export => sc(c, Key::E),
        Command::SaveAs => sc(cs, Key::S),
        Command::Quit => sc(c, Key::Q),
        Command::Undo => sc(c, Key::Z),
        Command::Redo => sc(c, Key::Y),
        Command::DeleteNode => sc(Modifiers::NONE, Key::Delete),
        Command::ViewNode => sc(Modifiers::NONE, Key::V),
        Command::FrameAll => sc(Modifiers::NONE, Key::F),
        Command::Fit => sc(c, Key::Num0),
        Command::Actual => sc(c, Key::Num1),
        Command::SetWorkspace(Workspace::Dual) => sc(cs, Key::D),
        Command::Preset(p) => workspaces::preset_shortcut(p),
        Command::ToggleClean => sc(cs, Key::C),
        Command::ToggleScopes => sc(cs, Key::K),
        Command::ToggleMaskOverlay => sc(Modifiers::NONE, Key::M),
        Command::Copy => sc(c, Key::C),
        Command::Cut => sc(c, Key::X),
        Command::Paste => sc(c, Key::V),
        Command::Duplicate => sc(c, Key::D),
        Command::SelectAll => sc(c, Key::A),
        Command::Compare => sc(Modifiers::NONE, Key::C),
        _ => return None,
    })
}

/// Commands bound to a key, most specific modifier combination first.
const SHORTCUT_ORDER: [Command; 23] = [
    // Ctrl+Alt+1..5 first: Ctrl+1 (100 %) would also match them, extra Alt is ignored by egui.
    Command::Preset(Preset::Single),
    Command::Preset(Preset::Dual),
    Command::Preset(Preset::DualScopes),
    Command::Preset(Preset::DualScopesClean),
    Command::Preset(Preset::CleanOnly),
    Command::SaveAs,
    Command::OpenFolder,
    Command::SetWorkspace(Workspace::Dual),
    Command::ToggleClean,
    Command::ToggleScopes,
    Command::NewProject,
    Command::OpenProject,
    Command::Save,
    Command::Export,
    Command::Quit,
    Command::Undo,
    Command::Redo,
    Command::Fit,
    Command::Actual,
    Command::ViewNode,
    Command::FrameAll,
    Command::ScanReport,
    Command::ToggleMaskOverlay,
];

/// Everything that decides whether a new render is needed. Node positions are
/// zeroed so that moving a node does not trigger an evaluation.
#[derive(Clone, PartialEq)]
struct RequestKey {
    graph: Graph,
    node: NodeId,
    /// Output port shown (the crop tool shows the picture entering the node).
    port: Option<usize>,
    image: ImageId,
    view: ViewSettings,
    preview: u32,
    /// Settings of the visible scopes; a change triggers exactly one new request.
    scopes: Option<PrepareSettings>,
    /// Mask shown as a red overlay in the viewer.
    overlay: Option<NodeId>,
    /// Changes when the metadata or the files behind the image change.
    revision: u64,
    /// A brush overlay's consumer and the crops between it and the picture shown.
    overlay_frame: Option<crate::frames::OverlayFrame>,
    /// Curve node whose input gets a histogram.
    histogram: Option<NodeId>,
    /// The renderer setting (a change must produce a new frame on the other path).
    mode: RendererMode,
    /// The compare view needs the CPU picture of the selected image.
    compare: bool,
    timing: bool,
    /// Moves when a LUT file was reloaded or changed on disk (`rawgraph_core::lut::generation`).
    lut_generation: u64,
}

impl RequestKey {
    #[cfg(test)]
    fn new(session: &Session, node: NodeId, scopes: Option<PrepareSettings>, overlay: Option<NodeId>) -> Self {
        Self::for_target(session, node, None, scopes, overlay)
    }

    #[cfg(test)]
    fn for_target(session: &Session, node: NodeId, port: Option<usize>, scopes: Option<PrepareSettings>, overlay: Option<NodeId>) -> Self {
        Self::for_image(session, session.current_image(), node, port, scopes, overlay)
    }

    /// The request for a given image (the compare view renders the reference image this way).
    fn for_image(session: &Session, image: ImageId, node: NodeId, port: Option<usize>, scopes: Option<PrepareSettings>, overlay: Option<NodeId>) -> Self {
        use rawgraph_engine::ImageProvider;
        let mut graph = session.project.graph.clone();
        let ids: Vec<NodeId> = graph.nodes().map(|n| n.id).collect();
        for id in ids {
            if let Some(n) = graph.node_mut(id) {
                n.pos = [0.0, 0.0];
            }
        }
        Self { graph, node, port, image, view: session.view, preview: session.provider.preview_size(), scopes, overlay, revision: session.provider.revision(image), overlay_frame: None, histogram: None, mode: RendererMode::Auto, compare: false, timing: false, lut_generation: rawgraph_core::lut::generation() }
    }
}

struct FrameInfo {
    width: u32,
    height: u32,
    eval_ms: f32,
    display_ms: f32,
    hits: u64,
    misses: u64,
    path: RenderPath,
    scope_step: usize,
    /// Findings of nodes behind the viewed picture (a Panorama's stitching warnings).
    warnings: Vec<String>,
}

struct Shot {
    path: Option<PathBuf>,
    /// `--dump-frame`: where the viewer picture goes, and how far that is.
    dump: Option<PathBuf>,
    dump_sent: bool,
    dump_done: bool,
    requested: bool,
    rx: Option<Receiver<Arc<egui::ColorImage>>>,
    /// Frame counter value when the latest preview reply arrived.
    reply_at: Option<u64>,
    started: Instant,
}

/// Automated timing run (`--bench`).
struct Bench {
    phase: BenchPhase,
    idx: usize,
    started: Instant,
    uncached: Vec<f32>,
    cached: Vec<f32>,
    prefetched: Vec<f32>,
    exposure: Vec<(f32, f32, f32)>,
    /// Develop white-balance changes: (latency, worker time, scopes).
    develop: Vec<(f32, f32, f32)>,
    /// Latency of Tone Curve, Curve point and Hue / Saturation changes, and of steps to a neighbour
    /// image that had been given time to be preloaded.
    tone: Vec<f32>,
    /// Develop exposure / contrast (tone part, GPU tail) changes: latency to the frame.
    dev_exposure: Vec<f32>,
    dev_contrast: Vec<f32>,
    curve: Vec<f32>,
    hue: Vec<f32>,
    /// Develop Lights (local tone), Filter radius and Sharpen amount changes: latency to the frame.
    lights: Vec<f32>,
    filter: Vec<f32>,
    sharpen: Vec<f32>,
    steps: Vec<f32>,
    step_pending: bool,
    steps_started: bool,
    scopes: Vec<f32>,
    scopes_worker: Vec<f32>,
    event: Option<(ImageId, f32, f32)>,
    wait_until: Option<Instant>,
    /// Worker time of the scopes of the frames made while a value is dragged (exposure, Develop).
    drag_scopes: Vec<f32>,
    /// Path of the newest bench frame ("GPU" / "CPU").
    path_label: String,
    done_deadline: Option<Instant>,
    /// The graph before the Lights / Filter / Sharpen phases, put back before the Steps phase so
    /// that it measures the same graph as before those phases existed.
    graph_snapshot: Option<Graph>,
}

/// How long `--bench` waits for the first picture before it gives up.
const BENCH_WAIT_LIMIT: Duration = Duration::from_secs(60);

/// Frames the UI has drawn and whether the benchmark got past its Wait phase (read by the watchdog,
/// which also fires when no frame is drawn at all, e.g. for a minimized or locked window).
static BENCH_FRAMES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static BENCH_STARTED: AtomicBool = AtomicBool::new(false);
/// What the Wait phase last saw: bit 0 CLI options applied, 1 scan finished, 2 no render pending, 3 images listed, 4 a render was requested.
static BENCH_WAIT_BITS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Bumped whenever the benchmark makes progress (a phase or image step, or the folder scan is
/// still running); the watchdog fires when it stands still for [`BENCH_WAIT_LIMIT`].
static BENCH_PROGRESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The current phase (`BenchPhase as u8`), for the watchdog's message.
static BENCH_PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// `--bench` gives up with a clear message and exit code 2 when it makes no progress for
/// [`BENCH_WAIT_LIMIT`] (no first picture, or a phase that never gets its frame).
fn bench_watchdog() {
    let _ = std::thread::Builder::new().name("bench-watchdog".into()).spawn(|| {
        let mut seen = u64::MAX;
        loop {
            std::thread::sleep(BENCH_WAIT_LIMIT);
            let now = BENCH_PROGRESS.load(Ordering::Relaxed);
            if now != seen {
                seen = now;
                continue;
            }
            let waiting = if BENCH_STARTED.load(Ordering::Relaxed) {
                "the next frame of this phase".to_owned()
            } else {
                let bits = BENCH_WAIT_BITS.load(Ordering::Relaxed);
                let names = ["CLI options applied", "folder scan finished", "render queue idle", "images in the list", "first render request"];
                let missing: Vec<&str> = names.iter().enumerate().filter(|(i, _)| bits & (1 << i) == 0).map(|(_, n)| *n).collect();
                if missing.is_empty() { "nothing".to_owned() } else { missing.join(", ") }
            };
            eprintln!(
                "BENCH gave up: no progress for {} s in phase {} ({} frames drawn; waiting for: {waiting}). Use a folder with images, \
--select-image 0 and a window that is visible and not minimized (a hidden window gets no frames).",
                BENCH_WAIT_LIMIT.as_secs(),
                BENCH_PHASE.load(Ordering::Relaxed),
                BENCH_FRAMES.load(Ordering::Relaxed),
            );
            std::process::exit(2);
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BenchPhase {
    Wait,
    Uncached,
    Cached,
    PrefetchSetup,
    PrefetchWait,
    PrefetchStep,
    Exposure,
    Develop,
    DevelopExposure,
    DevelopContrast,
    ToneCurve,
    CurveDrag,
    HueSat,
    DevelopLights,
    FilterRadius,
    SharpenAmount,
    Steps,
    Done,
}

pub struct App {
    session: Session,
    history: History,
    status: Status,
    renderer: Renderer,
    /// View > Renderer (or `--renderer`).
    renderer_mode: RendererMode,
    timing_debug: bool,
    node_timings: Option<Vec<rawgraph_engine::NodeTiming>>,
    timing_gpu_tail: bool,
    /// Pixels of the last CPU frame, kept only for `--dump-frame`.
    last_cpu_pixels: Option<(u32, u32, Vec<u8>)>,
    /// `--bench`: when the exact scopes of the resting GPU picture arrived after the last frame.
    bench_refined: Vec<f32>,
    last_frame_at: Option<Instant>,
    viewer: Viewer,
    filmstrip: Filmstrip,
    quick_preview: crate::quick_preview::QuickPreview,
    quick_generation: u64,
    /// The selection placeholder is the gallery's edited thumbnail (not an unedited preview).
    placeholder_edited: bool,
    /// What the selection placeholder shows (for `--bench-first-frame`).
    placeholder_src: &'static str,
    /// `--bench-first-frame`: the measurement driver.
    first_frame: Option<ff_driver::Driver>,
    editor: GraphEditor,
    scope_panel: ScopePanel,
    /// UI-thread time of the last scope upload (`set_prepared`) in ms.
    scope_ms: f32,
    /// Worker time of the last scope analysis in ms.
    scope_worker_ms: f32,
    prefetch: Prefetcher,
    workspace: Workspace,
    viewer_win: AuxWindow,
    clean_win: AuxWindow,
    scopes_win: AuxWindow,
    scopes_pos: ScopesPos,
    /// Workspace presets, saved workspaces, intended window displays and panel sizes.
    ws: workspaces::Workspaces,
    monitors: Vec<MonitorInfo>,
    monitors_at: Option<Instant>,
    cached_main_monitor: Option<usize>,
    startup_done: bool,
    cli: Cli,
    cli_applied: bool,
    last_key: Option<RequestKey>,
    frame_info: Option<FrameInfo>,
    last_error: Option<String>,
    show_shortcuts: bool,
    layout_epoch: u32,
    late_disconnect_done: bool,
    frames: u64,
    shot: Option<Shot>,
    scan: Option<ScanJob>,
    scan_report: Option<ScanReport>,
    show_report: bool,
    dialog: Option<Dialog>,
    prompt: Option<Pending>,
    allow_close: bool,
    title: String,
    submitted_at: Option<Instant>,
    bench: Option<Bench>,
    gallery_collection: CollectionWorker,
    gallery_key: Option<(Graph, Option<NodeId>, u64, usize)>,
    gallery_result: Option<crate::collection::CollectionResult>,
    collection: CollectionWorker,
    collection_key: Option<(Graph, Option<rawgraph_core::NodeId>, u64)>,
    /// Frame counter value when the last export finished (screenshots wait for the report to fade in).
    export_done_frame: Option<u64>,
    export_ui: ExportState,
    show_export: bool,
    export_job: Option<ExportJob>,
    finished_export: Option<FinishedExport>,
    /// View > Show mask overlay (not part of the project, never exported).
    show_mask_overlay: bool,
    /// Re-reading capture metadata of an old project in the background.
    capture_job: Option<CaptureRefresh>,
    /// The last reply was an error or an excluded image: the scopes show older data.
    scopes_stale: bool,
    /// Number of render requests sent (screenshot diagnostics).
    submits: u64,
    /// Rectangle between the menu bar and the status bar, for movable windows.
    window_area: egui::Rect,
    /// Pointer state of the brush and crop tools.
    tools: Tools,
    /// Timing run of the paint and curve tools (`--bench-tools`).
    tool_bench: Option<ToolBench>,
    // ---- library and safety net
    dirs: DataDirs,
    thumb_cache: Option<Arc<rawgraph_project::ThumbCache>>,
    session_key: String,
    lock: Option<SessionLock>,
    startup_scan: Option<Receiver<Orphans>>,
    recovery_dialog: Option<RecoveryDialog>,
    /// A project file with an unfinished save beside it: waiting for the user's decision.
    unfinished: Option<ext::UnfinishedState>,
    autosave: Scheduler,
    autosave_job: Option<WriteJob>,
    own_recovery: OwnFiles,
    last_recovery_ms: Option<i64>,
    pending_old_recovery: Option<PathBuf>,
    crash_done: bool,
    compare: Compare,
    cmp_view: SharedView,
    cmp_panes: [ext::PaneRender; 2],
    compare_started: Option<Instant>,
    compare_reported: bool,
    xmp_dialog: Option<XmpDialog>,
    /// Library > Rename files: the open window (None = closed).
    rename: Option<crate::rename_ui::RenameUi>,
    /// Thumbnails of the block check (its own loader, so the filmstrip's requests are left alone).
    rename_thumbs: Option<ThumbLoader>,
    /// Bumped whenever the caption sheet is loaded or removed (the preview is made again).
    rename_sheet_gen: u64,
    /// A batch rename (or its undo) running on its own thread.
    rename_job: Option<crate::rename_ops::RenameJob>,
    xmp_job: Option<XmpJob>,
    xmp_probe: Option<XmpJob>,
    xmp_offer: Option<XmpOffer>,
    relink: RelinkUi,
    missing_job: Option<MissingCheck>,
    clear_cache_confirm: bool,
    started_at: Instant,
    scan_done_at: Option<Instant>,
    fill_reported: bool,
    xmp_probe_pending: bool,
    /// Frame at which the recovery dialog first appeared (screenshots wait for its fade-in).
    recovery_seen_at: Option<u64>,
    /// Folder and sub-folder switch of every Source node as of the last frame.
    source_seen: Vec<(NodeId, String, bool)>,
    /// Folders to scan into the project because a Source was pointed at them: (folder, recursive).
    scan_queue: Vec<(PathBuf, bool)>,
    /// The export dialog in node mode, when open.
    node_export: Option<export::NodeExportState>,
    /// "Run Export nodes...", when open.
    run_nodes: Option<export::RunNodesState>,
    /// Export nodes still to run after the current export: (node, scope).
    export_queue: std::collections::VecDeque<(NodeId, export::Scope)>,
    /// `--run-export-node` has started its export.
    cli_run_started: bool,
    /// `--run-export-node` has printed its report.
    cli_run_reported: bool,
    /// An open question about a save (conflict, foreign project).
    save_flow: crate::save_flow::SaveFlow<Pending>,
    /// The History window.
    history_ui: revs::HistoryUi,
    /// File name offered by the next "Save as" dialog (after a conflict).
    save_as_name: Option<String>,
}

/// `--bench-tools`: synthetic paint and curve drags through the real render path.
struct ToolBench {
    phase: ToolBenchPhase,
    step: usize,
    /// When the graph was last changed by the synthetic pointer.
    moved_at: Option<Instant>,
    paint: Vec<f32>,
    curve: Vec<f32>,
    /// Crop / Rotate with a 4 degree rotation: (change -> frame ms, worker evaluation ms).
    transform: Vec<(f32, f32)>,
    brush: Option<NodeId>,
    curve_node: Option<NodeId>,
    transform_node: Option<NodeId>,
    base: Vec<rawgraph_core::BrushStroke>,
    wait_frames: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ToolBenchPhase {
    Setup,
    Paint,
    Curve,
    Transform,
    Done,
}

const TOOL_BENCH_STEPS: usize = 30;

const VIEWER_ID: &str = "rawgraph-viewer-window";
const CLEAN_ID: &str = "rawgraph-clean-feed";
const SCOPES_ID: &str = "rawgraph-scopes-window";

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cli: Cli) -> Self {
        setup_style(&cc.egui_ctx);
        if cli.embed_windows {
            // Lets screenshots show the secondary windows (eframe cannot capture OS viewports).
            cc.egui_ctx.set_embed_viewports(true);
        }
        if let Some(scale) = cli.scale {
            cc.egui_ctx.set_pixels_per_point(scale);
        }
        let screenshot_mode = cli.capture() || cli.benching() || cli.run_export_node.is_some();
        let settings: Settings = if screenshot_mode {
            Settings::default()
        } else {
            cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY)).unwrap_or_default()
        };

        let provider = Arc::new(FileProvider::new(DEFAULT_CACHE_BYTES));
        provider.set_preview_size(cli.preview_size.unwrap_or(settings.preview_size));
        let mut session = if cli.demo {
            Session::demo(provider.clone())
        } else {
            Session::new(crate::session::fresh_project(), provider.clone())
        };
        let mut status = Status::default();
        let mut startup_unfinished: Option<PathBuf> = None;
        if let Some(path) = &cli.project {
            crate::splash::stage(crate::splash::Stage::OpeningProject);
            match session.load_project(path) {
                Ok(note) => {
                    if let Some(n) = note {
                        status.info(n);
                    }
                    status.migration("Project", session.project.migration_notes().to_vec());
                }
                Err(crate::revisions::OpenError::Unfinished(_)) => {
                    // The dialog opens with the first frame.
                    startup_unfinished = Some(path.clone());
                    eprintln!("--project: {} has an unfinished save beside it", path.display());
                }
                Err(e) => {
                    status.error(format!("Cannot open {}: {e}", path.display()));
                    eprintln!("--project: {e}");
                }
            }
        }

        crate::splash::stage(crate::splash::Stage::PreparingEditor);
        let history = History::new(&session.project.graph);
        let renderer_mode = cli.renderer.unwrap_or(settings.renderer);
        let renderer = Renderer::with_gpu(
            cc.egui_ctx.clone(),
            provider.clone(),
            GpuConfig {
                setup: GpuSetup { render_state: cc.wgpu_render_state.clone(), inject: cli.gpu_fail },
                mode: renderer_mode,
                ready: (!cli.no_gpu_preload).then(|| {
                    let p = provider.clone();
                    Arc::new(move |id| p.is_cached(id)) as crate::render::ReadyFn
                }),
            },
        );
        rawgraph_project::set_always_rewrite(settings.always_rewrite);
        let dirs = DataDirs::from_option(cli.data_dir.as_deref());
        let thumb_cache = rawgraph_project::ThumbCache::new(&dirs.thumbs(), CACHE_BUDGET).map_err(|e| eprintln!("thumbnail cache: {e}")).ok().map(Arc::new);
        let mut live = crate::live_thumbs::LiveThumbs::new(cc.egui_ctx.clone(), provider.clone(), dirs.thumbs().join("linear"), settings.editable_thumb_gib);
        live.allow_gpu(cc.wgpu_render_state.is_some());
        let filmstrip = Filmstrip::with_thumbs(ThumbLoader::with_cache(cc.egui_ctx.clone(), provider.clone(), thumb_cache.clone()), live);
        let session_key = format!("{}-{}", std::process::id(), crate::clock::now_unix_ms());
        let lock = SessionLock::create(&dirs.recovery(), &session_key).map_err(|e| eprintln!("recovery lock: {e}")).ok();
        let startup_scan = Some(Self::spawn_startup_scan(&cc.egui_ctx, dirs.recovery(), lock.as_ref().and_then(SessionLock::file_name).unwrap_or_default()));
        let mut editor = GraphEditor::default();
        editor.set_recipes_dir(Some(dirs.recipes()));
        // Screenshot and bench runs never touch the system clipboard.
        editor.use_system_clipboard = !screenshot_mode;
        let prefetch = Prefetcher::new(provider);

        let store = if screenshot_mode {
            WorkspaceStore::default()
        } else {
            cc.storage.and_then(|s| s.get_string(WORKSPACES_KEY)).map(|text| WorkspaceStore::from_json(&text)).unwrap_or_default()
        };
        let (start_spec, start_note) = workspaces::startup_workspace(&settings, &store, &cli);
        if let Some(note) = start_note {
            status.error(note.clone());
            eprintln!("--workspace: {note}");
        }
        let workspace = if start_spec.has(WindowRole::Viewer) { Workspace::Dual } else { Workspace::Single };
        let scopes_pos = if start_spec.has(WindowRole::Scopes) { ScopesPos::Window } else { start_spec.layout.scopes_pos };
        let mut viewer_state = settings.viewer;
        viewer_state.open = start_spec.has(WindowRole::Viewer);
        viewer_state.fullscreen = true;
        let mut scopes_state = settings.scopes;
        scopes_state.open = start_spec.has(WindowRole::Scopes);
        let mut clean_state = settings.clean;
        // A restored clean feed is always fullscreen.
        clean_state.fullscreen = true;
        clean_state.open = start_spec.has(WindowRole::CleanFeed);
        let ws = workspaces::Workspaces::new(store, &start_spec);
        let cli_open_export = cli.open_export_dialog;
        let cli_mask_overlay = cli.mask_overlay;
        let (cli_export_format, cli_no_profile, cli_no_metadata) = (cli.export_format.unwrap_or(export::Fmt::Jpeg), cli.export_no_profile, cli.export_no_metadata);
        let cli_export_folder = cli.export_folder.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let shot = cli.capture().then(|| Shot { path: cli.screenshot.clone(), dump: cli.dump_frame.clone(), dump_sent: false, dump_done: false, requested: false, rx: None, reply_at: None, started: Instant::now() });
        let cli_first_frame = cli.bench_first_frame.is_some().then(|| ff_driver::Driver::new(&cli));
        if cli.bench {
            bench_watchdog();
        }
        let bench = cli.bench.then(|| Bench {
            phase: BenchPhase::Wait,
            idx: 0,
            started: Instant::now(),
            uncached: Vec::new(),
            cached: Vec::new(),
            prefetched: Vec::new(),
            exposure: Vec::new(),
            develop: Vec::new(),
            tone: Vec::new(),
            dev_exposure: Vec::new(),
            dev_contrast: Vec::new(),
            curve: Vec::new(),
            hue: Vec::new(),
            lights: Vec::new(),
            filter: Vec::new(),
            sharpen: Vec::new(),
            steps: Vec::new(),
            step_pending: false,
            steps_started: false,
            scopes: Vec::new(),
            scopes_worker: Vec::new(),
            event: None,
            wait_until: None,
            drag_scopes: Vec::new(),
            path_label: String::new(),
            done_deadline: None,
            graph_snapshot: None,
        });

        let app_autosave_secs = cli.autosave_interval.unwrap_or(60);
        let mut app = Self {
            session,
            history,
            status,
            renderer,
            renderer_mode,
            timing_debug: false,
            node_timings: None,
            timing_gpu_tail: false,
            last_cpu_pixels: None,
            bench_refined: Vec::new(),
            last_frame_at: None,
            viewer: Viewer::with_preferences(settings.click_zoom, settings.wheel_navigates),
            filmstrip,
            quick_preview: crate::quick_preview::QuickPreview::new(cc.egui_ctx.clone(), thumb_cache.clone()),
            quick_generation: 0,
            placeholder_edited: false,
            placeholder_src: "",
            first_frame: cli_first_frame,
            editor,
            scope_panel: ScopePanel::new(),
            scope_ms: 0.0,
            scope_worker_ms: 0.0,
            prefetch,
            workspace,
            viewer_win: AuxWindow::from_state(viewer_state),
            clean_win: AuxWindow::from_state(clean_state),
            scopes_win: AuxWindow::from_state(scopes_state),
            scopes_pos,
            ws,
            monitors: Vec::new(),
            monitors_at: None,
            cached_main_monitor: None,
            startup_done: false,
            cli,
            cli_applied: false,
            last_key: None,
            frame_info: None,
            last_error: None,
            show_shortcuts: false,
            layout_epoch: 0,
            late_disconnect_done: false,
            frames: 0,
            shot,
            scan: None,
            scan_report: None,
            show_report: false,
            dialog: None,
            prompt: None,
            allow_close: false,
            title: String::new(),
            submitted_at: None,
            bench,
            gallery_collection: CollectionWorker::new(cc.egui_ctx.clone()),
            gallery_key: None,
            gallery_result: None,
            collection: CollectionWorker::new(cc.egui_ctx.clone()),
            collection_key: None,
            export_done_frame: None,
            export_ui: ExportState { out_dir: cli_export_folder, format: cli_export_format, embed_profile: !cli_no_profile, copy_metadata: !cli_no_metadata, ..ExportState::default() },
            show_export: cli_open_export,
            export_job: None,
            finished_export: None,
            show_mask_overlay: cli_mask_overlay,
            capture_job: None,
            scopes_stale: false,
            submits: 0,
            window_area: egui::Rect::NOTHING,
            tools: Tools::default(),
            tool_bench: None,
            dirs,
            thumb_cache,
            session_key,
            lock,
            startup_scan,
            recovery_dialog: None,
            unfinished: None,
            autosave: Scheduler::with_interval(Duration::from_secs(app_autosave_secs)),
            autosave_job: None,
            own_recovery: OwnFiles::default(),
            last_recovery_ms: None,
            pending_old_recovery: None,
            crash_done: false,
            compare: Compare::default(),
            cmp_view: SharedView::default(),
            cmp_panes: [ext::PaneRender::default(), ext::PaneRender::default()],
            compare_started: None,
            compare_reported: false,
            xmp_dialog: None,
            rename: None,
            rename_thumbs: None,
            rename_sheet_gen: 0,
            rename_job: None,
            xmp_job: None,
            xmp_probe: None,
            xmp_offer: None,
            relink: RelinkUi::None,
            missing_job: None,
            clear_cache_confirm: false,
            started_at: Instant::now(),
            scan_done_at: None,
            fill_reported: false,
            xmp_probe_pending: false,
            recovery_seen_at: None,
            source_seen: Vec::new(),
            scan_queue: Vec::new(),
            node_export: None,
            run_nodes: None,
            export_queue: std::collections::VecDeque::new(),
            cli_run_started: false,
            cli_run_reported: false,
            save_flow: crate::save_flow::SaveFlow::default(),
            history_ui: revs::HistoryUi::default(),
            save_as_name: None,
        };
        app.source_seen = node_ui::source_states(&app.session.project.graph);
        app.tool_bench = app.cli.bench_tools.then(|| ToolBench {
            phase: ToolBenchPhase::Setup,
            step: 0,
            moved_at: None,
            paint: Vec::new(),
            curve: Vec::new(),
            transform: Vec::new(),
            brush: None,
            curve_node: None,
            transform_node: None,
            base: Vec::new(),
            wait_frames: 0,
        });
        app.start_capture_refresh(&cc.egui_ctx);
        app.start_missing_check(&cc.egui_ctx);
        if let Some(path) = startup_unfinished {
            app.ask_unfinished(path.clone(), ext::Retry::Open(path));
        }
        if let Some(folder) = app.cli.open_folder.clone() {
            app.start_scan(&cc.egui_ctx, folder);
        }
        app
    }

    // ---------------------------------------------------------------- commands

    fn run(&mut self, cmd: Command, ctx: &egui::Context) {
        match cmd {
            Command::NewProject => self.guard(Pending::NewProject, ctx),
            Command::OpenProject => self.guard(Pending::OpenProject, ctx),
            Command::OpenDemo => self.guard(Pending::OpenDemo, ctx),
            Command::OpenFolder => self.open_dialog(DialogKind::OpenFolder),
            Command::Save => self.save_project(None, None, ctx),
            Command::History => self.history_ui.open = !self.history_ui.open,
            Command::Export => self.show_export = !self.show_export,
            Command::RunExportNodes => self.open_run_nodes(),
            Command::SaveAs => self.open_dialog(DialogKind::SaveAs(None)),
            Command::ImportGraph => self.open_dialog(DialogKind::ImportGraph),
            Command::ExportGraph => self.open_dialog(DialogKind::ExportGraph),
            Command::Quit => self.guard(Pending::Quit, ctx),
            Command::Undo => {
                if let Some(g) = self.history.undo() {
                    self.session.project.graph = g;
                    self.session.dirty = true;
                    self.session.sanitize();
                }
            }
            Command::Redo => {
                if let Some(g) = self.history.redo() {
                    self.session.project.graph = g;
                    self.session.dirty = true;
                    self.session.sanitize();
                }
            }
            Command::DeleteNode => {
                if !self.session.node_sel.is_empty() {
                    if let Err(why) = self.session.delete_selected() {
                        self.status.error(why);
                    }
                }
            }
            Command::Copy => self.editor.request(EditOp::Copy),
            Command::Cut => self.editor.request(EditOp::Cut),
            Command::Paste => self.editor.request(EditOp::Paste),
            Command::Duplicate => self.editor.request(EditOp::Duplicate),
            Command::SelectAll => self.editor.request(EditOp::SelectAll),
            Command::SaveRecipe => self.editor.request(EditOp::SaveRecipe),
            Command::ManageRecipes => self.editor.request(EditOp::ManageRecipes),
            Command::GroupSelection => self.editor.request(EditOp::GroupSelection),
            Command::Ungroup => self.editor.request(EditOp::Ungroup),
            Command::OpenGroup => self.editor.request(EditOp::OpenSelected),
            Command::UpOneLevel => self.editor.request(EditOp::CloseGroup),
            Command::SaveMacro => self.editor.request(EditOp::SaveMacro),
            Command::ImportXmp => self.open_xmp_dialog(),
            Command::RenameFiles => self.open_rename(),
            Command::UndoRename => self.undo_rename_command(ctx),
            Command::Compare => self.toggle_compare(),
            Command::ToggleMissingOnly => {
                self.session.filter.missing_only = !self.session.filter.missing_only;
                self.session.refresh_visible();
            }
            Command::ClearThumbCache => self.clear_cache_confirm = true,
            Command::ViewNode => {
                if let Some(id) = self.session.selected_node {
                    self.session.viewed_node = None;
                    self.session.selected_node = Some(id);
                }
            }
            Command::FrameAll => self.editor.request_frame_all(),
            Command::Fit => { if self.compare.active() { self.cmp_view = SharedView::default(); } else { self.viewer.fit(); } },
            Command::Actual => { if self.compare.active() { self.cmp_view.mode = crate::viewer::ZoomMode::Custom(1.0 / ctx.pixels_per_point().max(0.1)); self.cmp_view.pan = egui::Vec2::ZERO; } else { self.viewer.actual_size(ctx.pixels_per_point()); } },
            Command::SetWorkspace(w) => self.set_workspace(w),
            Command::Preset(p) => self.apply_key(p.slug()),
            Command::ToggleClean => self.toggle_role(WindowRole::CleanFeed),
            Command::ToggleScopes => {
                let next = if self.scopes_pos == ScopesPos::Window { self.ws.layout.scopes_pos } else { ScopesPos::Window };
                self.set_scopes_pos(next);
            }
            Command::SetScopesPos(p) => self.set_scopes_pos(p),
            Command::ResetLayout => {
                let defaults = if self.workspace == Workspace::Dual { MainLayout::dual() } else { MainLayout::default() };
                self.ws.layout = MainLayout { scopes_pos: self.ws.layout.scopes_pos, ..defaults };
                self.ws.reset_panels = true;
                self.editor.request_frame_all();
                self.viewer.fit();
                self.status.info("Layout reset");
            }
            Command::Shortcuts => self.show_shortcuts = !self.show_shortcuts,
            Command::ScanReport => self.show_report = !self.show_report,
            Command::ToggleMaskOverlay => self.show_mask_overlay = !self.show_mask_overlay,
        }
    }

    /// Asks about unsaved changes before an action that replaces the document.
    fn guard(&mut self, p: Pending, ctx: &egui::Context) {
        if self.session.dirty && self.shot.is_none() && self.bench.is_none() && self.first_frame.is_none() {
            self.prompt = Some(p);
        } else {
            self.execute(p, ctx);
        }
    }

    fn execute(&mut self, p: Pending, ctx: &egui::Context) {
        // The rename thread relinks and saves the open project when it ends: the document must stay.
        if self.rename_job.is_some() {
            if matches!(p, Pending::Quit) {
                self.cancel_rename_for_close();
            } else {
                self.status.error("A rename is running: wait until it has finished (or cancel it in the Rename window)");
            }
            return;
        }
        match p {
            Pending::NewProject => {
                self.cancel_scan();
                self.discard_own_recovery();
                self.session.new_project();
                self.after_document_change();
                self.status.clear_notes();
                self.status.info("New project");
            }
            Pending::OpenProject => self.open_dialog(DialogKind::OpenProject),
            Pending::OpenDemo => {
                self.cancel_scan();
                self.discard_own_recovery();
                self.session.enter_demo();
                self.after_document_change();
                self.status.info("Demo images");
            }
            Pending::Quit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    /// Resets everything derived from the previous document.
    fn after_document_change(&mut self) {
        self.history.reset(&self.session.project.graph);
        self.filmstrip.thumbs.clear();
        self.filmstrip.live.reset();
        self.editor.request_frame_all();
        self.last_key = None;
        self.frame_info = None;
        self.last_error = None;
        self.scope_panel.clear();
        self.viewer = Viewer::with_preferences(self.viewer.click_zoom, self.viewer.wheel_navigates);
        self.scan_report = None;
        self.capture_job = None;
        self.scopes_stale = false;
        self.prefetch.forget();
        self.xmp_offer = None;
        self.xmp_probe = None;
        self.compare.forget();
        self.cmp_panes = [ext::PaneRender::default(), ext::PaneRender::default()];
        self.gallery_key = None;
        self.gallery_result = None;
        self.relink = RelinkUi::None;
        self.node_export = None;
        self.run_nodes = None;
        self.export_queue.clear();
        self.reset_source_baseline();
    }

    /// Re-reads the capture metadata of entries saved before the as-shot
    /// fields existed; the UI never waits for it.
    fn start_capture_refresh(&mut self, ctx: &egui::Context) {
        if self.scan.is_some() {
            return;
        }
        let targets = self.session.capture_refresh_targets();
        self.capture_job = (!targets.is_empty()).then(|| CaptureRefresh::start(ctx.clone(), targets));
    }

    fn poll_capture_refresh(&mut self) {
        let Some(job) = &mut self.capture_job else { return };
        let (items, finished) = job.poll();
        job.done += items.len();
        let changed = self.session.apply_capture_updates(items);
        if changed > 0 {
            self.last_key = None;
        }
        if finished {
            let n = job.done;
            self.capture_job = None;
            if n > 0 {
                self.status.info(format!("Updated the capture metadata of {n} image(s); the project has unsaved changes"));
            }
        }
    }

    fn open_dialog(&mut self, kind: DialogKind) {
        if self.dialog.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let (title, start) = (
            match &kind {
                DialogKind::ExportFolder => "Choose the export folder",
                DialogKind::OpenProject => "Open project",
                DialogKind::OpenFolder => "Open folder",
                DialogKind::SaveAs(_) => "Save project",
                DialogKind::ImportGraph => "Import graph",
                DialogKind::ExportGraph => "Export graph",
                DialogKind::LocateFile(_) => "Locate the file",
                DialogKind::RelinkNewFolder => "New place of the folder",
                DialogKind::SourceFolder(_) => "Choose the folder of the Source",
                DialogKind::NodeExportFolder(_) => "Choose the output folder of the Export node",
                DialogKind::LutFile(_) => "Choose a LUT file",
                DialogKind::CaptionSheet => "Choose the caption sheet",
            },
            self.session.project_path.as_ref().and_then(|p| p.parent()).map(std::path::Path::to_path_buf),
        );
        let which = match &kind {
            DialogKind::OpenProject => 0,
            DialogKind::OpenFolder => 1,
            DialogKind::SaveAs(_) => 2,
            DialogKind::ImportGraph => 3,
            DialogKind::ExportGraph => 4,
            DialogKind::ExportFolder => 5,
            DialogKind::LocateFile(_) => 6,
            DialogKind::RelinkNewFolder => 7,
            DialogKind::SourceFolder(_) | DialogKind::NodeExportFolder(_) => 8,
            DialogKind::LutFile(_) => 9,
            DialogKind::CaptionSheet => 10,
        };
        let name = self.save_as_name.take().unwrap_or_else(|| format!("{}{PROJECT_EXT}", self.session.project_name().replace("Untitled project", "untitled")));
        let _ = std::thread::Builder::new().name("rawgraph-dialog".into()).spawn(move || {
            let mut d = rfd::FileDialog::new().set_title(title);
            if let Some(s) = start {
                d = d.set_directory(s);
            }
            let picked = match which {
                0 => d.add_filter("RawGraph project (*.rawgraph)", &["rawgraph"]).pick_file(),
                1 | 5 | 7 | 8 => d.pick_folder(),
                6 => d.pick_file(),
                9 => d.add_filter("Cube LUT (*.cube)", &["cube", "CUBE", "Cube"]).pick_file(),
                10 => d.add_filter("Caption sheet (*.xlsx, *.csv)", &["xlsx", "xlsm", "xls", "ods", "csv", "tsv"]).pick_file(),
                2 => d.set_file_name(name).add_filter("RawGraph project (*.rawgraph)", &["rawgraph"]).save_file(),
                3 => d.add_filter("RawGraph graph (*.rawgraph.json)", &["json"]).pick_file(),
                _ => d.set_file_name(format!("graph{GRAPH_EXT}")).add_filter("RawGraph graph (*.rawgraph.json)", &["json"]).save_file(),
            };
            let _ = tx.send(picked);
        });
        self.dialog = Some(Dialog { kind, rx });
    }

    fn poll_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = &self.dialog else { return };
        let result = match d.rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        let Some(d) = self.dialog.take() else { return };
        let Some(path) = result else { return };
        match d.kind {
            // The current project stays open if that fails.
            DialogKind::OpenProject => self.open_project_path(&path, ctx),
            DialogKind::OpenFolder => self.start_scan(ctx, path),
            DialogKind::ExportFolder => self.export_ui.out_dir = path.to_string_lossy().into_owned(),
            DialogKind::LocateFile(id) => self.locate_file(id, path),
            DialogKind::CaptionSheet => self.caption_sheet_picked(path),
            DialogKind::SourceFolder(node) => {
                let folder = path.to_string_lossy().into_owned();
                if !node_ui::set_source_folder(&mut self.session.project.graph, node, Some(folder)) {
                    self.status.error("The Source node no longer exists");
                }
            }
            DialogKind::LutFile(node) => {
                let image = self.session.current_image();
                if let Err(e) = node_ui::set_lut_file(&mut self.session.project.graph, node, image, Some(path.to_string_lossy().into_owned())) {
                    self.status.error(e);
                }
            }
            DialogKind::NodeExportFolder(node) => {
                let image = self.session.current_image();
                if let Err(e) = node_ui::set_export_text(&mut self.session.project.graph, node, image, node_ui::ExportField::OutDir, &path.to_string_lossy()) {
                    self.status.error(e);
                }
            }
            DialogKind::RelinkNewFolder => {
                if let RelinkUi::Folder { new, .. } = &mut self.relink {
                    *new = path.to_string_lossy().into_owned();
                    self.check_relink_folder();
                }
            }
            DialogKind::SaveAs(then) => self.save_project(Some(ensure_extension(path, PROJECT_EXT, Some(".json"))), then, ctx),
            DialogKind::ImportGraph => match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| Graph::from_json_repaired(&t).map_err(|e| e.to_string())) {
                Ok((g, notes)) => {
                    self.session.set_graph(g);
                    self.status.clear_notes();
                    self.editor.request_frame_all();
                    self.status.info(format!("Imported graph from {}", path.display()));
                    self.status.migration("Import", notes);
                }
                Err(e) => self.status.error(format!("Cannot import {}: {e}", path.display())),
            },
            DialogKind::ExportGraph => {
                let path = ensure_extension(path, GRAPH_EXT, Some(".json"));
                match std::fs::write(&path, self.session.project.graph.to_json()) {
                    Ok(()) => self.status.info(format!("Exported graph to {}", path.display())),
                    Err(e) => self.status.error(format!("Cannot export {}: {e}", path.display())),
                }
            }
        }
    }

    /// Saves to `path` or the project's own path as a new revision; opens "Save as" if there is
    /// none. `then` is carried out only once the save has really been written (a conflict or
    /// a failure keeps the project open and changed).
    fn save_project(&mut self, path: Option<PathBuf>, then: Option<Pending>, ctx: &egui::Context) {
        if self.session.demo {
            self.status.error("The demo images are not a project; open a folder first");
            return;
        }
        let Some(target) = path.or_else(|| self.session.project_path.clone()) else {
            self.open_dialog(DialogKind::SaveAs(then));
            return;
        };
        let outcome = self.save_flow.save(&mut self.session, &target, then);
        self.after_save(outcome, ctx);
    }

    // ----------------------------------------------------------------- export

    /// Starts an export of the dialog's scope through the batch pipeline.
    fn start_export(&mut self, ctx: &egui::Context) {
        if self.export_job.is_some() {
            self.status.error("An export is already running");
            return;
        }
        if self.rename_job.is_some() {
            self.status.error("A rename is running; wait until it has finished");
            return;
        }
        let Some(node) = export::export_node(&self.session) else {
            self.status.error("Select or pin an image node to export");
            return;
        };
        self.export_ui.sheet = self.session.caption_sheet.clone();
        let items = self.export_ui.items(&self.session);
        let settings = match self.export_ui.settings(&self.session.view) {
            Ok(s) => s,
            Err(e) => {
                self.status.error(e);
                return;
            }
        };
        self.spawn_export(ctx, node, items, settings, export::catalog_items(&self.session));
    }

    fn spawn_export(&mut self, ctx: &egui::Context, node: rawgraph_core::NodeId, items: Vec<rawgraph_batch::ExportItem>, settings: rawgraph_batch::ExportSettings, catalog: Vec<rawgraph_batch::ExportItem>) {
        self.status.info(format!("Exporting {} image(s) to {}", items.len(), settings.out_dir.display()));
        self.export_job = Some(ExportJob::start(ctx.clone(), self.session.project.graph.clone(), node, items, settings, catalog));
        self.finished_export = None;
    }

    fn poll_export(&mut self) {
        let Some(job) = &mut self.export_job else { return };
        let mut done = None;
        while let Ok(msg) = job.rx.try_recv() {
            match msg {
                ExportMsg::Progress { done, total } => {
                    job.done = done;
                    job.total = total;
                }
                ExportMsg::Done(r) => done = Some(r),
            }
        }
        if let Some(result) = done {
            let Some(job) = self.export_job.take() else { return };
            self.export_done_frame = Some(self.frames);
            match result {
                Ok(report) => {
                    let text = export::summarize(&report);
                    let secs = job.started.elapsed().as_secs_f32();
                    // `--run-export-node` prints the report itself (once, on standard output).
                    let printed_elsewhere = !report_on_stderr(&self.cli);
                    if !printed_elsewhere {
                        eprintln!("{text}");
                    }
                    eprintln!("export time: {secs:.1} s for {} image(s), {:.1} images/min", report.items.len(), report.items.len() as f32 * 60.0 / secs.max(0.001));
                    for (id, st) in &report.items {
                        if let rawgraph_batch::ItemStatus::Failed(m) = st {
                            if !printed_elsewhere {
                                eprintln!("  failed {}: {m}", id.0);
                            }
                        }
                    }
                    if report.failed() > 0 {
                        self.status.error(text);
                    } else {
                        self.status.info(text);
                    }
                    self.finished_export = Some(FinishedExport { report, items: job.items, settings: job.settings, node: job.node, node_run: job.node_run, quick_catalog: job.quick_catalog });
                }
                Err(e) => {
                    eprintln!("export failed: {e}");
                    self.status.error(format!("Export failed: {e}"));
                }
            }
        }
    }

    fn export_windows(&mut self, ctx: &egui::Context) {
        if self.show_export {
            self.export_ui.sheet = self.session.caption_sheet.clone();
            let items = self.export_ui.items(&self.session);
            let counts = ExportState::scope_counts(&self.session);
            let node_text = export::export_node(&self.session).map_or_else(|| "(no image node selected)".to_owned(), |n| export::node_label(&self.session.project.graph, n));
            match export::show_dialog(ctx, self.window_area, &mut self.export_ui, &self.session.view, &export::DialogInfo { counts, node_text: &node_text, items: &items, busy: self.export_job.is_some(), demo: self.session.demo }) {
                DialogAction::PickFolder => self.open_dialog(DialogKind::ExportFolder),
                DialogAction::Start => {
                    self.start_export(ctx);
                    self.show_export = false;
                }
                DialogAction::Close => self.show_export = false,
                DialogAction::None => {}
            }
        }
        let mut retry = None;
        let mut close = false;
        if let Some(fin) = &self.finished_export {
            match export::show_report(ctx, self.window_area, fin) {
                ReportAction::Retry => retry = Some((fin.node, fin.report.retry_items(&fin.items), fin.settings.clone(), fin.node_run.clone(), fin.quick_catalog.clone())),
                ReportAction::OpenFolder => export::open_folder(&fin.settings.out_dir),
                ReportAction::Close => close = true,
                ReportAction::None => {}
            }
        }
        if close {
            self.finished_export = None;
        }
        if let Some((node, items, settings, node_run, catalog)) = retry {
            match node_run {
                Some(run) => {
                    self.status.info(format!("Exporting {} image(s) again through the Export node", items.len()));
                    self.export_job = Some(ExportJob::start_node(ctx.clone(), self.session.project.graph.clone(), node, items, run));
                    self.finished_export = None;
                }
                None => self.spawn_export(ctx, node, items, settings, catalog),
            }
        }
    }

    // --------------------------------------------------------------- scanning

    fn start_scan(&mut self, ctx: &egui::Context, folder: PathBuf) {
        if self.scan.is_some() {
            self.status.error("A scan is already running");
            return;
        }
        if self.rename_job.is_some() {
            self.status.error("A rename is running; wait until it has finished");
            return;
        }
        if self.session.demo {
            self.session.new_project();
            self.after_document_change();
        }
        let recursive = !self.cli.no_recursive;
        bind_open_folder(&mut self.session, &folder, recursive);
        // This explicit scan handles the change; do not enqueue it a second time.
        self.source_seen = node_ui::source_states(&self.session.project.graph);
        self.start_scan_with(ctx, folder, recursive);
    }

    fn start_scan_with(&mut self, ctx: &egui::Context, folder: PathBuf, recursive: bool) {
        if self.scan.is_some() {
            self.status.error("A scan is already running");
            return;
        }
        if self.session.demo {
            self.session.new_project();
            self.after_document_change();
        }
        self.xmp_offer = None;
        self.xmp_probe = None;
        self.capture_job = None;
        self.scan_done_at = None;
        self.session.begin_scan();
        let absolute = std::path::absolute(&folder).unwrap_or_else(|_| folder.clone());
        self.session.scan_count = Some((crate::session::folder_key(&absolute.to_string_lossy()), recursive, 0));
        self.session.scan_folder = Some((absolute.clone(), recursive));
        self.scan = Some(ScanJob::start(ctx.clone(), self.session.project.clone(), folder.clone(), recursive));
        self.status.info(format!("Scanning {}", folder.display()));
    }

    fn cancel_scan(&mut self) {
        if let Some(job) = self.scan.take() {
            job.cancel();
            self.session.cancel_scan_state();
        }
    }

    fn poll_scan(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.scan else { return };
        let mut finished = None;
        let mut disconnected = false;
        for index in 0..512 {
            if index == 511 { ctx.request_repaint(); }
            match job.rx.try_recv() {
                Ok(ScanMsg::Listing(event)) => {
                    use rawgraph_project::staged::Listing;
                    let count = match event {
                        Listing::Counting(n) => { job.counting = n; n }
                        Listing::Count(n) => { job.count = Some(n); n }
                    };
                    if let Some((_, _, current)) = &mut self.session.scan_count { *current = count; }
                }
                Ok(ScanMsg::Catalog(entries)) => {
                    job.listed = true;
                    let stale: Vec<_> = entries.iter().filter(|e| e.content_hash.is_empty() && self.session.project.image(e.id).is_some()).map(|e| e.id).collect();
                    if !stale.is_empty() {
                        self.filmstrip.thumbs.invalidate(&stale);
                        self.session.provider.clear_cache();
                    }
                    self.session.scan_catalog(entries);
                    ctx.request_repaint();
                    break; // Paint the full placeholder list before applying file results.
                }
                Ok(ScanMsg::Hashed(entries)) => {
                    let moved: Vec<_> = entries.iter().filter(|entry| self.session.entry(entry.id).is_some_and(|e| e.path != entry.path)).map(|entry| entry.id).collect();
                    if !moved.is_empty() { self.filmstrip.thumbs.invalidate(&moved); }
                    self.session.scan_entries(entries);
                }
                Ok(ScanMsg::Metadata { done, total, entries }) => {
                    job.metadata = Some((done, total));
                    self.session.scan_metadata(entries);
                }
                Ok(ScanMsg::Done { project, report }) => {
                    finished = Some((*project, report));
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if let Some((project, report)) = finished {
            // The session takes over the commit state, graph and edits itself and decides whether anything is unsaved.
            self.session.finish_scan(project, false);
            let text = summarize(&report);
            if report.failed.is_empty() {
                self.status.info(text);
            } else {
                self.status.error(text);
                self.show_report = true;
            }
            let added_any = !report.added.is_empty();
            self.scan_done_at = Some(Instant::now());
            self.scan_report = Some(report);
            self.scan = None;
            if added_any {
                self.xmp_probe_pending = true;
            }
        } else if disconnected {
            self.session.cancel_scan_state();
            self.scan = None;
            self.status.error("The scan stopped unexpectedly");
        }
    }

    // ------------------------------------------------------------------- frame

    fn startup(&mut self) {
        self.startup_done = true;
        if self.cli.capture() || self.cli.benching() {
            let list: Vec<String> = self.monitors.iter().map(|m| format!("{} (scale {:.2}{})", m.id, m.scale, if m.primary { ", primary" } else { "" })).collect();
            eprintln!("monitors: {}", list.join("; "));
        }
    }

    /// Applies the `--select-node`, `--rate`, ... options once the scan is done.
    fn apply_cli(&mut self) {
        self.cli_applied = true;
        let cli = self.cli.clone();
        apply_cli_state(&mut self.session, &cli);
        self.history.reset(&self.session.project.graph);
        self.editor.request_frame_all();
        if let Some(path) = &cli.save_project {
            self.cli_save_project(path, cli.simulate_conflict);
        }
        if cli.open_history || cli.select_revision.is_some() {
            self.history_ui.open = true;
            self.history_ui.selected = cli.select_revision;
        }
    }

    /// Runs --export through the same path as the dialog.
    fn cli_export(&mut self, ctx: &egui::Context) {
        let Some(dir) = self.cli.export.clone() else { return };
        let mut st = ExportState { out_dir: dir.to_string_lossy().into_owned(), ..Default::default() };
        st.format = self.cli.export_format.unwrap_or(export::Fmt::Jpeg);
        if let Some(n) = self.cli.export_long_edge {
            st.full_size = false;
            st.long_edge = n;
        }
        st.scope = self.cli.export_scope.unwrap_or(export::Scope::Selected);
        st.embed_profile = !self.cli.export_no_profile;
        st.copy_metadata = !self.cli.export_no_metadata;
        self.export_ui = st;
        self.start_export(ctx);
    }

    fn poll_renderer(&mut self, ctx: &egui::Context) {
        let reply = self.renderer.poll();
        if self.last_key.is_none() {
            self.renderer.take_refined();
            self.renderer.take_histogram();
            return;
        }
        let matches_selection = !self.viewer.awaiting_final() || self.last_key.as_ref().map(|k| k.image) == self.viewer.preview_image;
        if let Some(r) = self.renderer.take_refined().filter(|_| matches_selection && !self.compare.active()) {
            // The scopes of the resting GPU picture, now read at the full resolution.
            self.scope_panel.set_prepared(ctx, r.prepared);
            self.scope_worker_ms = r.scopes_ms;
            if let Some(f) = &mut self.frame_info {
                f.scope_step = r.step;
            }
            if let Some(t) = self.last_frame_at {
                self.bench_refined.push(t.elapsed().as_secs_f32() * 1000.0);
            }
        }
        if let Some(h) = self.renderer.take_histogram() {
            self.session.curve_hist = Some(crate::session::CurveHist { node: h.node, image: h.image, bins: h.bins });
        }
        if let Some(reply) = reply {
            if !matches_selection { return; }
            if let Some(s) = &mut self.shot {
                s.reply_at = Some(self.frames);
            }
            match reply {
                Reply::Frame(f) => {
                    if !self.compare.active() { self.scopes_stale = false; }
                    self.apply_frame(ctx, &f);
                }
                Reply::Refined(_) | Reply::Dumped { .. } | Reply::Histogram(_) => {}
                Reply::Error { message, .. } => {
                    if self.renderer_mode == RendererMode::Gpu && self.cli.capture() {
                        eprintln!("rawgraph-app: {message}");
                        EXIT_CODE.store(1, Ordering::SeqCst);
                        self.allow_close = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    if let Some(ff) = &mut self.first_frame {
                        ff.on_failed(format!("render error: {message}"));
                    }
                    self.last_error = Some(message.clone());
                    self.viewer.set_error(message);
                    if !self.compare.active() { self.scopes_stale = true; }
                }
                Reply::Excluded { message, .. } => {
                    if let Some(ff) = &mut self.first_frame {
                        ff.on_failed(format!("not shown: {message}"));
                    }
                    self.last_error = None;
                    self.viewer.set_excluded(message);
                    if !self.compare.active() { self.scopes_stale = true; }
                }
            }
        }
    }

    fn apply_frame(&mut self, ctx: &egui::Context, f: &Frame) {
        let latency = self.submitted_at.map_or(0.0, |t| t.elapsed().as_secs_f32() * 1000.0);
        self.node_timings = f.node_timings.clone();
        self.timing_gpu_tail = f.gpu_tail;
        self.viewer.set_frame(ctx, f);
        self.last_error = None;
        self.last_frame_at = Some(Instant::now());
        if let Some(ff) = &mut self.first_frame {
            ff.on_frame(f.image, f.path.short(), f.eval_ms, f.display_ms);
        }
        if let Some(note) = &f.notice {
            self.status.info(note.clone());
        }
        // Only a frame dump needs the CPU pixels; they are not kept otherwise.
        self.last_cpu_pixels = (self.cli.dump_frame.is_some() && f.gpu.is_none()).then(|| (f.width, f.height, f.rgba8.clone()));
        self.frame_info = Some(FrameInfo { width: f.width, height: f.height, eval_ms: f.eval_ms, display_ms: f.display_ms, hits: f.hits, misses: f.misses, path: f.path.clone(), scope_step: f.scope_step, warnings: f.warnings.clone() });
        if let Some(prepared) = f.prepared.clone().filter(|_| !self.compare.active()) {
            let t = Instant::now();
            self.scope_panel.set_prepared(ctx, prepared);
            self.scope_ms = t.elapsed().as_secs_f32() * 1000.0;
            self.scope_worker_ms = f.scopes_ms;
        }
        if let Some(tb) = &mut self.tool_bench {
            if let Some(t) = tb.moved_at.take() {
                let ms = t.elapsed().as_secs_f32() * 1000.0;
                match tb.phase {
                    ToolBenchPhase::Paint => tb.paint.push(ms),
                    ToolBenchPhase::Curve => tb.curve.push(ms),
                    ToolBenchPhase::Transform => tb.transform.push((ms, f.eval_ms)),
                    _ => {}
                }
            }
        }
        if let Some(b) = &mut self.bench {
            b.event = Some((f.image, latency, f.eval_ms + f.display_ms));
            b.path_label = f.path.short().to_owned();
            b.scopes.push(self.scope_ms);
            b.scopes_worker.push(self.scope_worker_ms);
        }
        if !self.late_disconnect_done && !self.cli.late_disconnect.is_empty() {
            self.late_disconnect_done = true;
            for &(node, port) in &self.cli.late_disconnect {
                if let Some(id) = self.session.node_by_index(node) {
                    self.session.project.graph.disconnect(id, port);
                }
            }
            if let Some(s) = &mut self.shot {
                s.reply_at = None;
            }
        }
        // Step to a neighbour instantly: decode them in the background now.
        let cold_bench = self.bench.as_ref().is_some_and(|b| matches!(b.phase, BenchPhase::Wait | BenchPhase::Uncached))
            || self.first_frame.as_ref().is_some_and(ff_driver::Driver::cold);
        if f.image == self.session.current_image() && !cold_bench {
            self.prefetch.request(self.session.neighbours());
        }
    }

    /// The graph with node positions zeroed: moving a node changes nothing that is computed.
    fn neutral_graph(&self) -> Graph {
        let mut g = self.session.project.graph.clone();
        let ids: Vec<rawgraph_core::NodeId> = g.nodes().map(|n| n.id).collect();
        for id in ids {
            if let Some(n) = g.node_mut(id) {
                n.pos = [0.0, 0.0];
            }
        }
        g
    }

    /// Re-computes the collection info when the graph, the viewed node or the
    /// image set or a capture metadata batch changed.
    fn refresh_collection(&mut self) {
        if let Some(r) = self.collection.poll() {
            self.session.collection = r;
        }
        let g = self.neutral_graph();
        let key = (g, self.session.target_node(), self.session.images_gen);
        if self.collection_key.as_ref() != Some(&key) {
            let (ids, caps) = self.session.id_captures();
            self.collection.request(key.0.clone(), ids, caps, key.1);
            self.collection_key = Some(key);
        }
    }

    fn refresh_gallery(&mut self) {
        if let Some(result) = self.gallery_collection.poll() {
            if !self.gallery_collection.pending() && self.gallery_key.as_ref().is_some_and(|key| key.1 == result.view_node) {
                self.gallery_result = Some(result);
            }
        }
        let node = self.primary_target().node;
        let key = (self.neutral_graph(), node, self.session.images_gen, self.session.scan_found());
        if self.gallery_key.as_ref() != Some(&key) {
            if !self.session.scanning || self.gallery_key.as_ref().is_none_or(|old| old.0 != key.0 || old.1 != key.1) {
                self.gallery_result = None;
            }
            if node.is_some() {
                let (ids, caps) = self.session.id_captures();
                self.gallery_collection.request(key.0.clone(), ids, caps, node);
            }
            self.gallery_key = Some(key);
        }
    }

    fn gallery_members(result: Option<&crate::collection::CollectionResult>, target: ViewerTarget) -> Option<&std::collections::HashSet<ImageId>> {
        result.filter(|r| r.view_node.is_some() && r.view_node == target.node).map(|r| &r.view_members)
    }

    fn single_viewer_request(&self) -> Option<(ImageId, NodeId, Option<usize>)> {
        let target = if self.compare.active() { self.session.viewer_target(None, self.session.viewed_node) } else { self.primary_target() };
        let (node, port) = self.session.render_viewer_target(target)?;
        // A pinned image stays; otherwise a node whose collection does not contain the selected
        // photo (e.g. a standalone Generator) shows its own image.
        let image = if target.image_pinned { target.image? } else { self.session.image_for_target(node, port).or(target.image)? };
        Some((image, node, port))
    }

    fn update_selection_preview(&mut self, ctx: &egui::Context) {
        let target = if self.compare.active() {
            let target = self.primary_target();
            target.image.zip(target.node)
        } else {
            self.single_viewer_request().map(|(image, node, _)| (image, node))
        };
        let viewer = if self.compare.active() { &mut self.cmp_panes[1].viewer } else { &mut self.viewer };
        let Some((image, node)) = target else {
            if viewer.preview_image.is_some() { *viewer = Viewer::with_preferences(viewer.click_zoom, viewer.wheel_navigates); }
            return;
        };
        // The gallery already renders the Primary viewer's node for every photo, so its
        // thumbnail shows the current edits; prefer it over an unedited proxy or camera JPEG.
        let edited = if self.compare.active() { None } else { self.filmstrip.live.current(image) };
        if viewer.preview_image != Some(image) {
            self.quick_generation = self.quick_generation.wrapping_add(1);
            viewer.select(image, self.quick_generation);
            self.prefetch.forget();
            self.scopes_stale = true;
            self.placeholder_edited = false;
            self.placeholder_src = "";
            let provider = &self.session.provider;
            if let Some(tex) = edited.clone() {
                viewer.set_placeholder(self.quick_generation, tex.clone(), edited_geometry(&self.session, node, image, &tex));
                self.placeholder_edited = true;
                self.placeholder_src = "edited thumbnail";
            } else if self.session.project.graph.has_upstream_kind(node, rawgraph_core::NodeKind::Panorama) {
                // Behind a Panorama the picture is not the frame of this image (other size and aspect): the
                // proxy, the camera thumbnail and the embedded preview would show the wrong picture.
                self.placeholder_src = "none (panorama)";
            } else if let Some(proxy) = provider.cached_proxy(image) {
                let size = (proxy.width(), proxy.height());
                let tex = crate::quick_preview::proxy_texture(ctx, &proxy, self.session.view.transform());
                viewer.set_placeholder(self.quick_generation, tex, crate::quick_preview::geometry(&self.session, node, image, size));
                self.placeholder_src = "proxy cache";
            } else if let Some(tex) = self.filmstrip.thumbs.texture(image) {
                let [w, h] = tex.size();
                let size = crate::quick_preview::estimate_proxy_size(provider, image, (w as u32, h as u32));
                viewer.set_placeholder(self.quick_generation, tex, crate::quick_preview::geometry(&self.session, node, image, size));
                self.placeholder_src = "thumbnail";
            } else if let Some(path) = provider.path_of(image) {
                let hash = self.session.entry(image).map_or_else(String::new, |e| e.content_hash.clone());
                self.quick_preview.request(crate::quick_preview::Job { generation: self.quick_generation, path, hash });
                self.placeholder_src = "quick preview pending";
            }
            ctx.request_repaint();
        }
        if !self.placeholder_edited && viewer.awaiting_final() {
            // The edited gallery thumbnail arrived after the selection: upgrade the placeholder.
            if let Some(tex) = edited {
                viewer.set_placeholder(self.quick_generation, tex.clone(), edited_geometry(&self.session, node, image, &tex));
                self.placeholder_edited = true;
                self.placeholder_src = "edited thumbnail";
            }
        }
        for (generation, pixels, source) in self.quick_preview.poll() {
            if generation != self.quick_generation || !viewer.awaiting_final() || self.placeholder_edited { continue; }
            if self.session.project.graph.has_upstream_kind(node, rawgraph_core::NodeKind::Panorama) { continue; }
            let [w, h] = pixels.size;
            let size = crate::quick_preview::estimate_proxy_size(&self.session.provider, image, (w as u32, h as u32));
            let tex = ctx.load_texture("embedded selection preview", pixels, egui::TextureOptions::LINEAR);
            viewer.set_placeholder(generation, tex, crate::quick_preview::geometry(&self.session, node, image, size));
            self.placeholder_src = source;
        }
    }

    fn submit_if_needed(&mut self) {
        let Some((image, node, port)) = self.single_viewer_request() else {
            if self.last_key.take().is_some() {
                self.viewer = Viewer::with_preferences(self.viewer.click_zoom, self.viewer.wheel_navigates);
                self.frame_info = None;
                self.last_error = None;
                if !self.compare.active() {
                    self.scope_panel.clear();
                    self.scopes_stale = false;
                }
            }
            return;
        };
        // The panels need a few frames to settle; asking before that would use
        // default scope settings and cost a second render.
        if self.frames < SETTLE_FRAMES || (!self.compare.active() && Self::gallery_members(self.gallery_result.as_ref(), self.primary_target()).is_none()) {
            return;
        }
        let scopes = (!self.compare.active() && self.scopes_pos != ScopesPos::Hidden).then(|| self.scope_panel.prepare_settings());
        let overlay = crate::groups::overlay_request(self.show_mask_overlay, &self.session.project.graph, self.session.selected_node);
        // A drawing is drawn in the frame of the node that uses it; one that cannot be shown right
        // on this picture is not drawn at all.
        let (overlay, overlay_frame) = if port.is_some() { (overlay, None) } else { crate::frames::overlay_for_render(&self.session.project.graph, overlay, node) };
        let histogram = self
            .session
            .selected_node
            .and_then(|id| self.session.project.graph.node(id))
            .filter(|n| n.kind == NodeKind::PointCurve && self.session.project.graph.input_edge(n.id, 0).is_some())
            .map(|n| n.id)
            .filter(|_| port.is_none());
        let mut key = RequestKey::for_image(&self.session, image, node, port, scopes, overlay);
        key.overlay_frame = overlay_frame.clone();
        key.histogram = histogram;
        key.mode = self.renderer_mode;
        key.compare = self.compare.active();
        key.timing = self.timing_debug;
        if self.last_key.as_ref() != Some(&key) {
            if self.last_key.as_ref().is_some_and(|k| k.preview != key.preview) {
                self.prefetch.forget();
            }
            let cpu_reason = if self.renderer_mode == RendererMode::Cpu {
                Some(crate::gpu_path::REASON_CPU_ONLY.to_owned())
            } else if key.compare {
                Some(crate::gpu_path::REASON_COMPARE.to_owned())
            } else {
                None
            };
            let hints = GpuHints { timing: key.timing, cpu_reason, revision: key.revision, preview: key.preview, neighbours: self.session.neighbours(), histogram, overlay_frame };
            // A notice belongs to the previous request, not the frame we are
            // waiting for. Keep the last successful texture visible.
            self.viewer.excluded = None;
            self.viewer.error = None;
            self.viewer.rendering();
            self.renderer.submit_with(self.session.project.graph.clone(), node, port, key.image, self.session.view, key.scopes.clone(), key.overlay, hints);
            self.submits += 1;
            self.submitted_at = Some(Instant::now());
            self.last_key = Some(key);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) -> Vec<Command> {
        let typing = ctx.egui_wants_keyboard_input();
        let mut cmds = Vec::new();
        // Alternative redo binding; must be checked before Ctrl+Z.
        if ctx.input_mut(|i| i.consume_shortcut(&sc(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z))) {
            cmds.push(Command::Redo);
        }
        for cmd in SHORTCUT_ORDER {
            let Some(s) = shortcut_of(cmd) else { continue };
            if s.modifiers == Modifiers::NONE && typing {
                continue;
            }
            if ctx.input_mut(|i| i.consume_shortcut(&s)) {
                cmds.push(cmd);
            }
        }
        cmds
    }

    /// 0-5 rating, P/X/U flags, 6-9 colour labels, K for the keyword field (when no text field has focus).
    fn handle_culling(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.session.demo {
            return;
        }
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        // K: type a keyword (the add field of the keyword panel takes the focus).
        if pressed(Key::K) && !self.session.selected_images().is_empty() {
            self.filmstrip.keyword_panel.focus_add = true;
        }
        let Some(id) = self.session.selected else { return };
        let ratings = [(Key::Num0, 0u8), (Key::Num1, 1), (Key::Num2, 2), (Key::Num3, 3), (Key::Num4, 4), (Key::Num5, 5)];
        for (k, r) in ratings {
            if pressed(k) {
                self.session.set_rating(id, r);
            }
        }
        if pressed(Key::P) {
            self.session.set_flag(id, Flag::Pick);
        }
        if pressed(Key::X) {
            self.session.set_flag(id, Flag::Reject);
        }
        if pressed(Key::U) {
            self.session.set_flag(id, Flag::None);
        }
        let labels = [(Key::Num6, ColorLabel::Red), (Key::Num7, ColorLabel::Yellow), (Key::Num8, ColorLabel::Green), (Key::Num9, ColorLabel::Blue)];
        for (k, l) in labels {
            if pressed(k) {
                self.session.toggle_label(id, l);
            }
        }
    }

    fn shot_ready(&self, shot: &Shot) -> bool {
        let expects_frame = self.single_viewer_request().is_some();
        self.frames >= MIN_FRAMES_BEFORE_SHOT
            && self.cli_applied
            && self.scan.is_none()
            && self.dialog.is_none()
            && self.export_job.is_none()
            && self.scan_queue.is_empty()
            && self.export_queue.is_empty()
            && self.capture_job.is_none()
            && self.xmp_job.is_none()
            && self.xmp_probe.is_none()
            && !self.xmp_probe_pending
            && self.missing_job.is_none()
            && self.startup_scan.is_none()
            && self.recovery_seen_at.is_none_or(|f| self.frames >= f + 25)
            && self.autosave_job.is_none()
            && (!self.compare.active() || self.cmp_ready())
            && self.export_done_frame.is_none_or(|f| self.frames >= f + 25)
            && !self.collection.pending()
            && (!expects_frame || shot.reply_at.is_some_and(|at| self.frames >= at + 4))
            && !self.renderer.pending()
            && self.filmstrip.all_loaded()
            && (self.late_disconnect_done || self.cli.late_disconnect.is_empty())
            && self.last_key.is_some() == expects_frame
            && self.curve_hist_ready()
            && self.scopes_ready()
    }

    /// The scopes are computed after the picture; a screenshot waits until their data was uploaded.
    fn scopes_ready(&self) -> bool {
        let wanted = self.single_viewer_request().is_some() && !self.compare.active() && self.scopes_pos != ScopesPos::Hidden;
        // No scopes come after an error or an excluded image (stale), and a failed GPU readback
        // keeps the old ones: wait a few seconds after the last frame at most.
        let waited_long = self.last_frame_at.is_some_and(|t| t.elapsed() > Duration::from_secs(5));
        !wanted || self.scopes_stale || waited_long || !self.scope_panel.needs_refresh()
    }

    /// A selected Curve node gets its histogram a moment after the frame: screenshots wait for it.
    fn curve_hist_ready(&self) -> bool {
        let graph = &self.session.project.graph;
        let Some(node) = self.session.selected_node.and_then(|id| graph.node(id)).filter(|n| n.kind == NodeKind::PointCurve && graph.input_edge(n.id, 0).is_some()) else { return true };
        self.session.curve_hist.as_ref().is_some_and(|h| h.node == node.id && h.image == self.session.current_image())
    }

    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let Some(mut shot) = self.shot.take() else { return };
        ctx.request_repaint();
        let close = self.capture_step(ctx, &mut shot);
        if close {
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.shot = Some(shot);
    }

    /// One step of the capture (frame dump, then screenshot). True when the run is over.
    fn capture_step(&mut self, ctx: &egui::Context, shot: &mut Shot) -> bool {
        if !shot.requested {
            if self.shot_ready(shot) {
                if self.dump_step(shot) {
                    return true;
                }
                if shot.dump.is_some() && !shot.dump_done {
                    return false;
                }
                let Some(_) = &shot.path else { return true };
                let windows = [(&self.viewer_win, "viewer"), (&self.clean_win, "clean feed"), (&self.scopes_win, "scopes")];
                for (win, name) in windows {
                    if let (true, Some(r)) = (win.is_open(), win.last_rect) {
                        eprintln!("{name} window outer rect (points): {:.0},{:.0} {:.0}x{:.0}", r.min.x, r.min.y, r.width(), r.height());
                    }
                }
                eprintln!("render requests sent: {}", self.submits);
                if let Some(f) = &self.frame_info {
                    eprintln!("picture on screen: {} ({}x{})", match &f.path {
                        RenderPath::Gpu => "GPU".to_owned(),
                        RenderPath::Cpu { reason, .. } => format!("CPU - {reason}"),
                    }, f.width, f.height);
                }
                let (tx, rx) = channel();
                ctx.request_screenshot(move |img| {
                    let _ = tx.send(img);
                });
                shot.rx = Some(rx);
                shot.requested = true;
            } else if shot.started.elapsed() > SHOT_TIMEOUT {
                eprintln!("capture: timed out waiting for the scan, the preview frame or the thumbnails");
                EXIT_CODE.store(1, Ordering::SeqCst);
                return true;
            }
            return false;
        }
        if let (Some(img), Some(path)) = (shot.rx.as_ref().and_then(|rx| rx.try_recv().ok()), &shot.path) {
            report(save_png(path, &img), path);
            return true;
        }
        false
    }

    /// Writes the viewer picture for `--dump-frame`: the GPU picture is read back by the worker,
    /// a CPU frame is written from its pixels. True when the run has to end (failure).
    fn dump_step(&mut self, shot: &mut Shot) -> bool {
        let Some(path) = shot.dump.clone() else { return false };
        if shot.dump_done {
            return false;
        }
        let on_gpu = self.frame_info.as_ref().is_some_and(|f| f.path.is_gpu());
        if !shot.dump_sent {
            shot.dump_sent = true;
            if on_gpu {
                self.renderer.dump_gpu_frame(path);
            } else {
                if self.renderer_mode == RendererMode::Gpu {
                    let why = match self.frame_info.as_ref().map(|f| &f.path) {
                        Some(RenderPath::Cpu { reason, .. }) => reason.clone(),
                        _ => "no picture".to_owned(),
                    };
                    eprintln!("--dump-frame: --renderer gpu was requested but the picture on screen is a CPU frame ({why})");
                    EXIT_CODE.store(1, Ordering::SeqCst);
                    return true;
                }
                let result = match &self.last_cpu_pixels {
                    Some((w, h, px)) => crate::render::write_png(&path, *w, *h, px),
                    None => Err("there is no picture to write".to_owned()),
                };
                if let Err(e) = &result {
                    eprintln!("--dump-frame failed: {e}");
                    EXIT_CODE.store(1, Ordering::SeqCst);
                    return true;
                }
                eprintln!("frame dumped (CPU frame): {}", path.display());
                shot.dump_done = true;
            }
            return false;
        }
        if let Some((path, result)) = self.renderer.take_dumped() {
            match result {
                Ok(()) => {
                    eprintln!("frame dumped (GPU frame): {}", path.display());
                    shot.dump_done = true;
                }
                Err(e) => {
                    eprintln!("--dump-frame failed: {e}");
                    EXIT_CODE.store(1, Ordering::SeqCst);
                    return true;
                }
            }
        }
        false
    }

    /// Drives the `--bench` run: steps through the images and prints timings.
    fn bench_step(&mut self, ctx: &egui::Context) {
        let Some(mut b) = self.bench.take() else { return };
        ctx.request_repaint();
        // A minimized (or occluded) window gets no frames, so the run would never advance.
        if ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        }
        let phase_before = b.phase;
        let idx_before = b.idx;
        let n = self.session.visible().len();
        if self.scan.is_some() {
            // A long folder scan is progress too.
            BENCH_PROGRESS.fetch_add(1, Ordering::Relaxed);
        }
        let event = b.event.take();
        let matches = |ev: Option<(ImageId, f32, f32)>, id: ImageId| ev.filter(|e| e.0 == id);
        match b.phase {
            BenchPhase::Wait => {
                let bits = [self.cli_applied, self.scan.is_none(), !self.renderer.pending(), n > 0, self.last_key.is_some()];
                BENCH_WAIT_BITS.store(bits.iter().enumerate().fold(0u8, |a, (i, b)| a | (u8::from(*b) << i)), Ordering::Relaxed);
                // Nothing is rendered without a node to view (this is why a plain `--bench --open-folder`
                // run never started): view the Output node, or else the last node of the graph.
                if self.cli_applied && self.session.selected_node.is_none() {
                    let g = &self.session.project.graph;
                    let pick = g.nodes().find(|n| n.kind == NodeKind::Output).or_else(|| g.nodes().last()).map(|n| n.id);
                    if pick.is_some() {
                        self.session.selected_node = pick;
                    }
                }
                if self.cli_applied && self.scan.is_none() && !self.renderer.pending() && n > 0 && self.last_key.is_some() {
                    self.session.select_index(stride_index(0, n));
                    b.idx = 0;
                    b.phase = BenchPhase::Uncached;
                    BENCH_STARTED.store(true, Ordering::Relaxed);
                    self.session.provider.clear_cache();
                    self.prefetch.forget();
                    // Render even if the first image is already selected.
                    self.last_key = None;
                    b.event = None;
                } else {
                    // The watchdog gives up when nothing moves for BENCH_WAIT_LIMIT.
                    b.event = event;
                }
            }
            BenchPhase::Uncached | BenchPhase::Cached => {
                let id = self.session.current_image();
                if let Some(e) = matches(event, id) {
                    let cached = b.phase == BenchPhase::Cached;
                    if cached { &mut b.cached } else { &mut b.uncached }.push(e.1);
                    // Give the prefetcher no time to help the uncached pass.
                    b.idx += 1;
                    if b.idx >= n {
                        b.idx = 0;
                        b.phase = if cached { BenchPhase::PrefetchSetup } else { BenchPhase::Cached };
                        if b.phase == BenchPhase::PrefetchSetup {
                            self.session.provider.clear_cache();
                            self.prefetch.forget();
                            self.last_key = None;
                        }
                    }
                    if b.phase == BenchPhase::Uncached {
                        // Jump around so neighbours prefetched earlier do not help.
                        self.session.select_index(stride_index(b.idx, n));
                    } else if b.phase == BenchPhase::Cached {
                        self.session.select_index(b.idx);
                    } else {
                        self.session.select_index(n / 2);
                    }
                } else {
                    b.event = event;
                }
            }
            BenchPhase::PrefetchSetup => {
                // Wait for the middle image's frame, then give the prefetcher time.
                let id = self.session.current_image();
                if matches(event, id).is_some() {
                    b.phase = BenchPhase::PrefetchWait;
                    b.wait_until = Some(Instant::now() + Duration::from_secs(8));
                }
            }
            BenchPhase::PrefetchWait => {
                let next = self.session.neighbours().first().copied();
                let ready = next.is_some_and(|i| self.session.provider.is_cached(i));
                if ready || b.wait_until.is_some_and(|t| Instant::now() > t) {
                    self.session.step_image(1);
                    b.phase = BenchPhase::PrefetchStep;
                }
            }
            BenchPhase::PrefetchStep => {
                let id = self.session.current_image();
                if let Some(e) = matches(event, id) {
                    b.prefetched.push(e.1);
                    // Edit the Exposure node of the graph (or add one) for the editing phase.
                    let g = &mut self.session.project.graph;
                    let existing = g.nodes().find(|n| n.kind == NodeKind::Exposure).map(|n| n.id);
                    if let Some(exp) = existing {
                        self.session.selected_node = Some(exp);
                    } else {
                        let src = g.nodes().find(|n| n.kind == NodeKind::Source).map(|n| n.id);
                        let out = g.nodes().find(|n| n.kind == NodeKind::Output).map(|n| n.id);
                        if let (Some(src), Some(out)) = (src, out) {
                            let exp = g.add_node(NodeKind::Exposure, [150.0, 100.0]);
                            let _ = g.connect(src, exp, 0);
                            let _ = g.connect(exp, out, 0);
                            self.session.selected_node = Some(exp);
                        }
                    }
                    b.idx = 0;
                    b.phase = BenchPhase::Exposure;
                    self.bench_set_exposure(0);
                }
            }
            BenchPhase::Exposure => {
                let id = self.session.current_image();
                if let Some(e) = matches(event, id) {
                    b.exposure.push((e.1, e.2, self.scope_ms));
                    b.drag_scopes.push(self.scope_worker_ms);
                    b.idx += 1;
                    if b.idx >= 10 {
                        // Next: white-balance changes on the Develop node.
                        self.bench_enter(&mut b, BenchPhase::Develop);
                    } else {
                        self.bench_set_exposure(b.idx);
                    }
                } else {
                    b.event = event;
                }
            }
            BenchPhase::Develop => {
                let id = self.session.current_image();
                if let Some(e) = matches(event, id) {
                    b.develop.push((e.1, e.2, self.scope_ms));
                    b.drag_scopes.push(self.scope_worker_ms);
                    b.idx += 1;
                    if b.idx >= 10 {
                        self.bench_enter(&mut b, BenchPhase::DevelopExposure);
                    } else {
                        self.bench_set_white_balance(b.idx);
                    }
                } else {
                    b.event = event;
                }
            }
            BenchPhase::DevelopExposure
            | BenchPhase::DevelopContrast
            | BenchPhase::ToneCurve
            | BenchPhase::CurveDrag
            | BenchPhase::HueSat
            | BenchPhase::DevelopLights
            | BenchPhase::FilterRadius
            | BenchPhase::SharpenAmount => {
                let id = self.session.current_image();
                if let Some(e) = matches(event, id) {
                    match b.phase {
                        BenchPhase::DevelopExposure => b.dev_exposure.push(e.1),
                        BenchPhase::DevelopContrast => b.dev_contrast.push(e.1),
                        BenchPhase::ToneCurve => b.tone.push(e.1),
                        BenchPhase::CurveDrag => b.curve.push(e.1),
                        BenchPhase::DevelopLights => b.lights.push(e.1),
                        BenchPhase::FilterRadius => b.filter.push(e.1),
                        BenchPhase::SharpenAmount => b.sharpen.push(e.1),
                        _ => b.hue.push(e.1),
                    }
                    b.drag_scopes.push(self.scope_worker_ms);
                    b.idx += 1;
                    if b.idx >= 10 {
                        let next = match b.phase {
                            BenchPhase::DevelopExposure => BenchPhase::DevelopContrast,
                            BenchPhase::DevelopContrast => BenchPhase::ToneCurve,
                            BenchPhase::ToneCurve => BenchPhase::CurveDrag,
                            BenchPhase::CurveDrag => BenchPhase::HueSat,
                            BenchPhase::HueSat => BenchPhase::DevelopLights,
                            BenchPhase::DevelopLights => BenchPhase::FilterRadius,
                            BenchPhase::FilterRadius => BenchPhase::SharpenAmount,
                            _ => BenchPhase::Steps,
                        };
                        self.bench_enter(&mut b, next);
                    } else {
                        let step = b.idx;
                        self.bench_change(b.phase, step);
                    }
                } else {
                    b.event = event;
                }
            }
            BenchPhase::Steps => {
                let id = self.session.current_image();
                if b.step_pending {
                    // The step was made: wait for its frame, then give the worker time to preload.
                    if let Some(e) = matches(event, id) {
                        b.steps.push(e.1);
                        b.step_pending = false;
                        b.idx += 1;
                        b.wait_until = Some(Instant::now() + Duration::from_millis(900));
                    } else {
                        b.event = event;
                    }
                } else {
                    if !b.steps_started {
                        // Start at the first image so that there are enough neighbours to step through.
                        b.steps_started = true;
                        self.session.select_index(0);
                        b.wait_until = Some(Instant::now() + Duration::from_millis(1500));
                        self.bench = Some(b);
                        return;
                    }
                    let has_next = self.session.selected_index().is_some_and(|i| i + 1 < n);
                    let next = has_next.then(|| self.session.neighbours().first().copied()).flatten();
                    let ready = next.is_some_and(|i| self.session.provider.is_cached(i));
                    let settled = b.wait_until.is_none_or(|t| Instant::now() >= t);
                    if b.idx >= 8 || !has_next {
                        b.phase = BenchPhase::Done;
                    } else if settled && ready && !self.renderer.pending() {
                        self.session.step_image(1);
                        b.step_pending = true;
                    }
                }
            }
            BenchPhase::Done => {
                // The exact scopes of a resting GPU picture arrive a moment after the last frame.
                let gpu = self.frame_info.as_ref().is_some_and(|f| f.path.is_gpu());
                if b.done_deadline.is_none() {
                    b.done_deadline = Some(Instant::now() + Duration::from_millis(2000));
                    self.bench_refined.clear();
                }
                if gpu && self.bench_refined.is_empty() && b.done_deadline.is_some_and(|d| Instant::now() < d) {
                    self.bench = Some(b);
                    return;
                }
                self.bench_counts();
                print_bench(&b, &self.session, self.renderer_mode, &self.bench_refined);
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return; // the run is over; do not print twice
            }
        }
        if b.phase != phase_before || b.idx != idx_before {
            BENCH_PROGRESS.fetch_add(1, Ordering::Relaxed);
            BENCH_PHASE.store(b.phase as u8, Ordering::Relaxed);
        }
        if b.phase != phase_before {
            eprintln!("BENCH phase {:?} -> {:?} after {:.1} s", phase_before, b.phase, b.started.elapsed().as_secs_f32());
        }
        self.bench = Some(b);
    }

    /// Drives `--bench-tools`: 30 existing strokes, then synthetic paint and curve drags.
    fn tool_bench_step(&mut self, ctx: &egui::Context) {
        let Some(mut tb) = self.tool_bench.take() else { return };
        ctx.request_repaint();
        let idle = self.cli_applied && self.scan.is_none() && !self.renderer.pending() && self.last_key.is_some() && tb.moved_at.is_none() && !self.collection.pending();
        match tb.phase {
            ToolBenchPhase::Setup if idle => {
                let image = self.session.current_image();
                let g = &mut self.session.project.graph;
                let exp = g.nodes().find(|n| n.kind == NodeKind::Exposure).map(|n| n.id);
                let sat = g.nodes().find(|n| n.kind == NodeKind::Saturation).map(|n| n.id);
                let out = g.nodes().find(|n| n.kind == NodeKind::Output).map(|n| n.id);
                if let (Some(exp), Some(sat), Some(out)) = (exp, sat, out) {
                    let brush = g.add_node(NodeKind::Draw, [0.0, 300.0]);
                    if let Some(input) = g.input_of(exp, 0) { let _ = g.connect(input, brush, 0); }
                    if let Ok(port) = g.expose_parameter(exp, "ev", rawgraph_core::PortType::Images) { let _ = g.connect(brush, exp, port); }
                    if let Some(n) = g.node_mut(exp) {
                        n.set_param("ev", image, 1.5);
                    }
                    let curve = g.add_node(NodeKind::PointCurve, [900.0, 300.0]);
                    let tf = g.add_node(NodeKind::Transform, [1100.0, 300.0]);
                    let _ = g.connect(sat, curve, 0);
                    let _ = g.connect(curve, tf, 0);
                    let _ = g.connect(tf, out, 0);
                    tb.transform_node = Some(tf);
                    let settings = crate::brush_logic::BrushSettings { size_pct: 6.0, ..Default::default() };
                    let strokes: Vec<rawgraph_core::BrushStroke> = (0..30)
                        .map(|s| {
                            let mut stroke = settings.new_stroke(false, [0.1 + 0.025 * s as f32, 0.2]);
                            for p in 1..24 {
                                let t = p as f32 / 23.0;
                                stroke.points.push([0.1 + 0.025 * s as f32 + 0.08 * (t * 6.0).sin(), 0.2 + 0.6 * t]);
                            }
                            stroke
                        })
                        .collect();
                    tb.base = strokes.clone();
                    crate::tools::set_strokes(&mut self.session, brush, image, strokes);
                    self.session.selected_node = Some(brush);
                    tb.brush = Some(brush);
                    tb.curve_node = Some(curve);
                    tb.phase = ToolBenchPhase::Paint;
                    tb.wait_frames = 0;
                }
            }
            ToolBenchPhase::Paint | ToolBenchPhase::Curve | ToolBenchPhase::Transform if idle => {
                // Let the previous render settle for a few frames between steps.
                tb.wait_frames += 1;
                if tb.wait_frames >= 4 {
                    tb.wait_frames = 0;
                    let image = self.session.current_image();
                    let step = tb.step;
                    if tb.phase == ToolBenchPhase::Paint {
                        if step >= TOOL_BENCH_STEPS {
                            tb.phase = ToolBenchPhase::Curve;
                            tb.step = 0;
                            self.session.selected_node = tb.curve_node;
                        } else if let Some(brush) = tb.brush {
                            // The stroke in progress grows by one point per synthetic pointer move.
                            let settings = crate::brush_logic::BrushSettings { size_pct: 6.0, ..Default::default() };
                            let mut stroke = settings.new_stroke(false, [0.5, 0.5]);
                            for p in 1..=(step + 1) {
                                stroke.points.push([0.5 + 0.004 * p as f32, 0.5 + 0.1 * (p as f32 * 0.3).sin()]);
                            }
                            crate::tools::set_strokes(&mut self.session, brush, image, crate::brush_logic::with_stroke(&tb.base, &stroke));
                            tb.moved_at = Some(Instant::now());
                            tb.step += 1;
                        }
                    } else if tb.phase == ToolBenchPhase::Transform {
                        if step >= TOOL_BENCH_STEPS {
                            tb.phase = ToolBenchPhase::Done;
                        } else if let Some(n) = tb.transform_node.and_then(|id| self.session.project.graph.node_mut(id)) {
                            // A new angle every step, so nothing is served from the cache.
                            n.set_param("angle", image, 4.0 + 0.02 * step as f32);
                            tb.moved_at = Some(Instant::now());
                            tb.step += 1;
                        }
                    } else if step >= TOOL_BENCH_STEPS {
                        tb.phase = ToolBenchPhase::Transform;
                        tb.step = 0;
                    } else if let Some(curve) = tb.curve_node {
                        let mut pts = vec![[0.0, 0.0], [0.25, 0.18], [0.5, 0.5], [0.75, 0.82], [1.0, 1.0]];
                        if step > 0 {
                            crate::curve_logic::move_point(&mut pts, 1, [0.25, 0.18 - 0.004 * step as f32]);
                            crate::curve_logic::move_point(&mut pts, 3, [0.75, 0.82 + 0.004 * step as f32]);
                        }
                        if let Some(n) = self.session.project.graph.node_mut(curve) {
                            n.set_data(image, Some(rawgraph_core::NodeData::Curve(pts)));
                        }
                        tb.moved_at = Some(Instant::now());
                        tb.step += 1;
                    }
                }
            }
            ToolBenchPhase::Done => {
                let stats = |v: &[f32]| -> String {
                    if v.is_empty() {
                        return "n/a".to_owned();
                    }
                    let mut s = v.to_vec();
                    s.sort_by(f32::total_cmp);
                    format!("n={} min {:.1} median {:.1} max {:.1} ms", s.len(), s[0], s[s.len() / 2], s[s.len() - 1])
                };
                eprintln!("BENCH preview {} px, drawing with 30 existing strokes + {} points in progress", self.session.provider.preview_size(), TOOL_BENCH_STEPS);
                eprintln!("BENCH paint: pointer move -> updated frame: {}", stats(&tb.paint));
                eprintln!("BENCH curve: drag step -> updated frame:    {}", stats(&tb.curve));
                let (lat, eval): (Vec<f32>, Vec<f32>) = tb.transform.iter().copied().unzip();
                eprintln!("BENCH crop/rotate 4 deg: change -> frame:    {}", stats(&lat));
                eprintln!("BENCH crop/rotate 4 deg: worker evaluation:  {}", stats(&eval));
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            _ => {}
        }
        self.tool_bench = Some(tb);
    }

    /// Times the collection counts of every node: for the project's images and graph, and for a
    /// synthetic catalog of 4,000 images with a 30-node graph (best of five runs each).
    fn bench_counts(&mut self) {
        use rawgraph_core::CaptureInfo;
        let best = |graph: &Graph, ids: &[ImageId], caps: &crate::collection::Captures| -> f32 {
            (0..5).map(|_| crate::collection::compute(graph, ids, caps, None).millis).fold(f32::INFINITY, f32::min)
        };
        let (ids, caps) = self.session.id_captures();
        let graph = self.session.project.graph.clone();
        eprintln!("BENCH node counts for {} images and {} nodes: {:.2} ms (best of 5)", ids.len(), graph.nodes().count(), best(&graph, &ids, &caps));
        let synthetic_ids: Vec<ImageId> = (1..=4000u64).map(ImageId).collect();
        let synthetic_caps: crate::collection::Captures = Arc::new(synthetic_ids.iter().map(|id| (*id, CaptureInfo { path: Some(format!("D:/photos/{}/img{:04}.dng", id.0 % 4, id.0)), iso: Some(100.0 + (id.0 % 6400) as f32), ..Default::default() })).collect());
        let mut big = Graph::new();
        let src = big.add_node(NodeKind::Source, [0.0, 0.0]);
        let mut prev = src;
        for i in 0..28 {
            let kind = if i % 7 == 3 { NodeKind::MetadataFilter } else { NodeKind::Exposure };
            let n = big.add_node(kind, [0.0, 0.0]);
            if big.connect(prev, n, 0).is_ok() {
                prev = n;
            }
        }
        let out = big.add_node(NodeKind::Output, [0.0, 0.0]);
        let _ = big.connect(prev, out, 0);
        eprintln!("BENCH node counts for {} images and {} nodes: {:.2} ms (best of 5)", synthetic_ids.len(), big.nodes().count(), best(&big, &synthetic_ids, &synthetic_caps));
    }

    /// Starts `phase` when the graph has a node of the kind it changes, else goes on to the next one.
    fn bench_enter(&mut self, b: &mut Bench, mut phase: BenchPhase) {
        loop {
            if matches!(phase, BenchPhase::DevelopLights | BenchPhase::FilterRadius | BenchPhase::SharpenAmount) && b.graph_snapshot.is_none() {
                b.graph_snapshot = Some(self.session.project.graph.clone());
            }
            if phase == BenchPhase::Steps {
                if let Some(graph) = b.graph_snapshot.take() {
                    self.session.project.graph = graph;
                    // The node added for Lights/Filter/Sharpen is gone: view one that exists.
                    let g = &self.session.project.graph;
                    if !self.session.selected_node.is_some_and(|id| g.node(id).is_some()) {
                        self.session.selected_node = g.nodes().find(|n| n.kind == NodeKind::Output).or_else(|| g.nodes().last()).map(|n| n.id);
                    }
                }
            }
            let kind = match phase {
                BenchPhase::Develop | BenchPhase::DevelopExposure | BenchPhase::DevelopContrast | BenchPhase::DevelopLights => Some(NodeKind::Develop),
                BenchPhase::ToneCurve => Some(NodeKind::ToneCurve),
                BenchPhase::CurveDrag => Some(NodeKind::PointCurve),
                BenchPhase::HueSat => Some(NodeKind::HueSaturation),
                BenchPhase::FilterRadius => Some(NodeKind::Blur),
                BenchPhase::SharpenAmount => Some(NodeKind::Sharpen),
                _ => None,
            };
            // The Filter and Sharpen phases bring their own node when the graph has none.
            if matches!(phase, BenchPhase::FilterRadius | BenchPhase::SharpenAmount) {
                if let Some(kind) = kind {
                    self.bench_ensure_node(kind);
                }
            }
            let Some(kind) = kind else {
                b.phase = phase;
                b.idx = 0;
                b.wait_until = None;
                return;
            };
            let found = self.session.project.graph.nodes().find(|n| n.kind == kind).map(|n| n.id);
            if let Some(id) = found {
                self.session.selected_node = Some(id);
                b.phase = phase;
                b.idx = 0;
                if phase == BenchPhase::Develop {
                    self.bench_set_white_balance(0);
                } else {
                    self.bench_change(phase, 0);
                }
                return;
            }
            phase = match phase {
                BenchPhase::Develop => BenchPhase::DevelopExposure,
                BenchPhase::DevelopExposure => BenchPhase::DevelopContrast,
                BenchPhase::DevelopContrast => BenchPhase::ToneCurve,
                BenchPhase::ToneCurve => BenchPhase::CurveDrag,
                BenchPhase::CurveDrag => BenchPhase::HueSat,
                BenchPhase::HueSat => BenchPhase::DevelopLights,
                BenchPhase::DevelopLights => BenchPhase::FilterRadius,
                BenchPhase::FilterRadius => BenchPhase::SharpenAmount,
                _ => BenchPhase::Steps,
            };
        }
    }

    /// The first node of `kind`, or a new one between the Output node and what feeds it.
    fn bench_ensure_node(&mut self, kind: NodeKind) -> Option<NodeId> {
        let g = &mut self.session.project.graph;
        if let Some(n) = g.nodes().find(|n| n.kind == kind) {
            return Some(n.id);
        }
        let node_pos = [200.0, 140.0];
        let output = g.nodes().find(|n| n.kind == NodeKind::Output).map(|n| n.id);
        match output {
            Some(out) => {
                let (up, up_port) = g.input_edge(out, 0)?;
                let node = g.add_node(kind, node_pos);
                g.connect_ports(up, up_port, node, 0).ok()?;
                g.connect(node, out, 0).ok()?;
                Some(node)
            }
            // The default graph has no Output node: continue after its last node.
            None => {
                let last = g.nodes().last().map(|n| n.id)?;
                let node = g.add_node(kind, node_pos);
                g.connect(last, node, 0).ok()?;
                Some(node)
            }
        }
    }

    /// One slider / point move of the Tone Curve, Curve or Hue / Saturation phase of the benchmark.
    fn bench_change(&mut self, phase: BenchPhase, step: usize) {
        let image = self.session.current_image();
        let Some(id) = self.session.selected_node else { return };
        let Some(n) = self.session.project.graph.node_mut(id) else { return };
        let t = step as f32 / 9.0;
        match phase {
            BenchPhase::DevelopExposure => {
                n.set_param("exposure", image, -0.5 + 1.0 * t);
            }
            BenchPhase::DevelopContrast => {
                n.set_param("contrast", image, -0.3 + 0.6 * t);
            }
            BenchPhase::ToneCurve => {
                n.set_param("shadows", image, -0.4 + 0.8 * t);
                n.set_param("highlights", image, 0.4 - 0.8 * t);
            }
            BenchPhase::CurveDrag => {
                let pts = vec![[0.0, 0.0], [0.25, 0.2 + 0.1 * t], [0.6, 0.55 + 0.3 * t], [1.0, 1.0]];
                n.set_data(image, Some(rawgraph_core::NodeData::Curve(pts)));
            }
            BenchPhase::HueSat => {
                n.set_param("red_s", image, -0.5 + t);
                n.set_param("blue_h", image, 0.3 - 0.6 * t);
            }
            BenchPhase::DevelopLights => {
                // The (non-Legacy) local tone: Lights and Shadows, the part a GPU tail now runs.
                n.set_param("highlights", image, -0.8 + 1.6 * t);
                n.set_param("shadows", image, 0.2 + 0.4 * t);
            }
            BenchPhase::FilterRadius => {
                // 4 to 24 per mille of the long edge: 10 to 61 px at 2560 px.
                n.set_param("radius", image, 4.0 + 20.0 * t);
            }
            BenchPhase::SharpenAmount => {
                n.set_param("radius", image, 1.0);
                n.set_param("amount", image, 0.2 + 0.8 * t);
            }
            _ => {}
        }
    }

    /// Custom white balance, a different temperature each step.
    fn bench_set_white_balance(&mut self, step: usize) {
        let image = self.session.current_image();
        if let Some(id) = self.session.selected_node {
            if let Some(n) = self.session.project.graph.node_mut(id) {
                n.set_param("wb", image, 1.0);
                n.set_param("temperature", image, 3000.0 + 700.0 * step as f32);
            }
        }
    }

    fn bench_set_exposure(&mut self, step: usize) {
        let image = self.session.current_image();
        if let Some(id) = self.session.selected_node {
            if let Some(n) = self.session.project.graph.node_mut(id) {
                n.set_param("ev", image, 0.4 + 0.3 * step as f32);
            }
        }
    }

    // ---------------------------------------------------------------------- UI

    fn menu_bar(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let ctx = ui.ctx().clone();
        let can_undo = self.history.can_undo();
        let can_redo = self.history.can_redo();
        let has_sel = self.session.selected_node.is_some();
        let clean_open = self.clean_win.is_open();
        let scopes_pos = self.scopes_pos;
        let preview = self.session.provider.preview_size();
        let has_report = self.scan_report.is_some();
        let show_overlay = self.show_mask_overlay;
        let mut new_preview = None;
        let mut thumb_budget = self.filmstrip.live.budget();
        let renderer_mode = self.renderer_mode;
        let mut new_renderer = None;
        let item_tip = |ui: &mut egui::Ui, cmds: &mut Vec<Command>, label: &str, cmd: Command, enabled: bool, tip: &str| {
            let mut button = egui::Button::new(label);
            if let Some(s) = shortcut_of(cmd) {
                button = button.shortcut_text(ctx.format_shortcut(&s));
            }
            let mut r = ui.add_enabled(enabled, button);
            if !tip.is_empty() {
                r = r.on_hover_text(tip).on_disabled_hover_text(tip);
            }
            if r.clicked() {
                cmds.push(cmd);
                ui.close();
            }
        };
        let item = |ui: &mut egui::Ui, cmds: &mut Vec<Command>, label: &str, cmd: Command, enabled: bool| item_tip(ui, cmds, label, cmd, enabled, "");
        let compare_on = self.compare.active();
        let missing_only = self.session.filter.missing_only;
        let has_nodes = !self.session.node_sel.is_empty();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                item(ui, cmds, "New project", Command::NewProject, true);
                item(ui, cmds, "Open project...", Command::OpenProject, true);
                item(ui, cmds, "Open folder...", Command::OpenFolder, true);
                item(ui, cmds, "Import ratings and keywords from XMP...", Command::ImportXmp, true);
                ui.separator();
                item(ui, cmds, "Save", Command::Save, true);
                item(ui, cmds, "Save as...", Command::SaveAs, true);
                item_tip(ui, cmds, "History...", Command::History, true, "The revisions saved in this project file; an older graph can be restored");
                ui.separator();
                item(ui, cmds, "Export...", Command::Export, true);
                item_tip(ui, cmds, "Run Export nodes...", Command::RunExportNodes, true, "Runs the Export nodes of the graph one after another");
                ui.menu_button("Graph", |ui| {
                    item(ui, cmds, "Import...", Command::ImportGraph, true);
                    item(ui, cmds, "Export...", Command::ExportGraph, true);
                });
                ui.separator();
                item(ui, cmds, "Quit", Command::Quit, true);
            });
            ui.menu_button("Edit", |ui| {
                item(ui, cmds, "Undo", Command::Undo, can_undo);
                item(ui, cmds, "Redo", Command::Redo, can_redo);
                ui.separator();
                let note = crate::graph_editor::PER_IMAGE_NOTE;
                item_tip(ui, cmds, "Copy", Command::Copy, has_nodes, note);
                item_tip(ui, cmds, "Cut", Command::Cut, has_nodes, note);
                item_tip(ui, cmds, "Paste", Command::Paste, true, note);
                item_tip(ui, cmds, "Duplicate", Command::Duplicate, has_nodes, note);
                item(ui, cmds, "Select all nodes", Command::SelectAll, true);
                ui.separator();
                item_tip(ui, cmds, "Save selection as recipe...", Command::SaveRecipe, has_nodes, note);
                item(ui, cmds, "Manage recipes...", Command::ManageRecipes, true);
                ui.separator();
                item_tip(ui, cmds, "Group selection  (Ctrl+G)", Command::GroupSelection, has_nodes, "Collapses the selected nodes into one Group node; nothing is evaluated differently");
                item(ui, cmds, "Ungroup  (Ctrl+Shift+G)", Command::Ungroup, has_nodes);
                item(ui, cmds, "Open group  (Enter)", Command::OpenGroup, has_nodes);
                item(ui, cmds, "Up one level  (Esc)", Command::UpOneLevel, true);
                item_tip(ui, cmds, "Save group as macro...", Command::SaveMacro, has_nodes, crate::macros::COPY_NOTE);
                ui.separator();
                item(ui, cmds, "Delete selected nodes", Command::DeleteNode, has_sel);
                item(ui, cmds, "View selected node", Command::ViewNode, has_sel);
                item(ui, cmds, "Frame all nodes", Command::FrameAll, true);
            });
            ui.menu_button("Library", |ui| {
                item_tip(ui, cmds, "Rename files...", Command::RenameFiles, true, "Renames the listed or the selected files on disk (RAW originals and .xmp sidecars included), with a full preview and an undo log");
                let undo_ok = crate::rename_ops::log_state(&crate::rename_ops::log_path(&self.session, &self.dirs)).0;
                item_tip(ui, cmds, "Undo last rename", Command::UndoRename, undo_ok, "Puts the files of the last rename back under their old names");
            });
            ui.menu_button("Debug", |ui| {
                if ui.checkbox(&mut self.timing_debug, "Per-node CPU timing").changed() {
                    self.node_timings = None;
                    self.timing_gpu_tail = false;
                }
            });
            ui.menu_button("View", |ui| {
                ui.menu_button("Workspace", |ui| self.workspace_menu(ui));
                ui.separator();
                ui.label(RichText::new("Viewer click zoom (relative to fit)").weak());
                ui.radio_value(&mut self.viewer.click_zoom, 2.0, "2× fit");
                ui.radio_value(&mut self.viewer.click_zoom, 3.0, "3× fit");
                ui.label(RichText::new("Viewer wheel / swipe").weak());
                ui.radio_value(&mut self.viewer.wheel_navigates, false, "Zoom");
                ui.radio_value(&mut self.viewer.wheel_navigates, true, "Navigate images");
                ui.separator();
                let mut rewrite = rawgraph_project::always_rewrite();
                if ui.checkbox(&mut rewrite, "Always rewrite the whole project file").on_hover_text("Saves by writing a complete new file instead of updating the project file in place. Used automatically on network drives and in sync folders (Nextcloud, Dropbox, OneDrive).").changed() {
                    rawgraph_project::set_always_rewrite(rewrite);
                }
                ui.separator();
                ui.label(RichText::new("Preview size (long edge)").weak());
                for n in PREVIEW_SIZES {
                    if ui.radio(preview == n, format!("{n} px")).clicked() {
                        new_preview = Some(n);
                        ui.close();
                    }
                }
                ui.separator();
                ui.menu_button("Editable thumbnail cache", |ui| {
                    ui.add(egui::DragValue::new(&mut thumb_budget).range(0..=128).suffix(" GiB"));
                    ui.label("0 keeps editable bases in memory only.");
                });
                ui.label(RichText::new("Renderer").weak());
                let tip = "Automatic runs the last pointwise nodes and the display transform on the GPU and falls back to the CPU when that is not possible.\nExport, collection counts and the compare view's reference image always use the CPU.";
                for m in [RendererMode::Auto, RendererMode::Cpu] {
                    if ui.radio(renderer_mode == m, m.label()).on_hover_text(tip).clicked() {
                        new_renderer = Some(m);
                        ui.close();
                    }
                }
                ui.separator();
                let clean_label = if clean_open { "Close clean feed" } else { "Clean feed" };
                item(ui, cmds, clean_label, Command::ToggleClean, true);
                self.display_menu(ui, WindowRole::CleanFeed);
                ui.menu_button("Scopes position", |ui| {
                    for p in ScopesPos::ALL {
                        if ui.radio(scopes_pos == p, p.label()).clicked() {
                            cmds.push(Command::SetScopesPos(p));
                            ui.close();
                        }
                    }
                });
                ui.separator();
                let mut overlay = show_overlay;
                let mut label = egui::Button::new("Show mask overlay").selected(overlay);
                label = label.shortcut_text(ctx.format_shortcut(&sc(Modifiers::NONE, Key::M)));
                if ui.add(label).on_hover_text("Tints the viewer red where the mask of the selected node is above 0").clicked() {
                    overlay = !overlay;
                    cmds.push(Command::ToggleMaskOverlay);
                    ui.close();
                }
                let _ = overlay;
                ui.separator();
                let mut compare_button = egui::Button::new("Compare").selected(compare_on);
                compare_button = compare_button.shortcut_text(ctx.format_shortcut(&sc(Modifiers::NONE, Key::C)));
                if ui.add(compare_button).on_hover_text("Reference image on the left, the selected image on the right; Esc leaves").clicked() {
                    cmds.push(Command::Compare);
                    ui.close();
                }
                let mut missing_button = egui::Button::new("Show missing files only").selected(missing_only);
                missing_button = missing_button.min_size(egui::vec2(0.0, 0.0));
                if ui.add(missing_button).clicked() {
                    cmds.push(Command::ToggleMissingOnly);
                    ui.close();
                }
                ui.separator();
                item(ui, cmds, "Fit", Command::Fit, true);
                item(ui, cmds, "100 %", Command::Actual, true);
                item(ui, cmds, "Last scan report", Command::ScanReport, has_report);
                ui.separator();
                item(ui, cmds, "Reset layout", Command::ResetLayout, true);
                ui.separator();
                item(ui, cmds, "Clear thumbnail cache...", Command::ClearThumbCache, true);
            });
            ui.menu_button("Help", |ui| {
                item(ui, cmds, "Shortcuts", Command::Shortcuts, true);
                item(ui, cmds, "Open demo images", Command::OpenDemo, true);
            });
        });
        if thumb_budget != self.filmstrip.live.budget() { self.filmstrip.live.set_budget(thumb_budget); }
        if let Some(n) = new_preview {
            self.session.provider.set_preview_size(n);
        }
        if let Some(m) = new_renderer {
            self.renderer_mode = m;
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let ppp = ui.ctx().pixels_per_point();
        ui.horizontal_centered(|ui| {
            self.project_status(ui);
            if let Some(e) = self.session.current_entry() {
                ui.label(RichText::new(display_name(e)).strong());
            }
            match &self.frame_info {
                Some(f) => {
                    ui.label(format!("{} x {}", f.width, f.height));
                    ui.separator();
                    let stage = self.session.target_node().and_then(|n| node_ui::cpu_stage(&self.session.project.graph, n));
                    let (text, tip) = match &f.path {
                        RenderPath::Gpu => match stage {
                            Some(s) => (format!("GPU \u{2014} {s} on CPU"), format!("The display transform and the last pointwise nodes ran on the GPU; the base picture comes from the CPU because {s} is not pointwise. It is cached, so changes after it stay fast.")),
                            None => ("GPU".to_owned(), "This picture was made on the GPU: the base image stays cached and only the last pointwise nodes plus the display transform run on every change.".to_owned()),
                        },
                        RenderPath::Cpu { reason, .. } => (f.path.status_label(self.renderer_mode), format!("This picture was made on the CPU: {reason}.")),
                    };
                    let color = match &f.path {
                        RenderPath::Gpu => egui::Color32::from_rgb(120, 210, 150),
                        RenderPath::Cpu { fallback: true, .. } => egui::Color32::from_rgb(255, 190, 90),
                        RenderPath::Cpu { .. } => egui::Color32::from_gray(200),
                    };
                    ui.label(RichText::new(text).strong().color(color)).on_hover_text(tip);
                    ui.separator();
                    ui.label(format!("eval {:.1} ms (+{:.1} display)", f.eval_ms, f.display_ms));
                    ui.separator();
                    ui.label(format!("cache {} hits / {} misses", f.hits, f.misses));
                    ui.separator();
                    ui.label(format!("scopes {:.1} ms UI, {:.0} ms worker", self.scope_ms, self.scope_worker_ms));
                }
                None => {
                    ui.label("no preview yet");
                }
            }
            if self.viewer.texture_id().is_some() {
                ui.separator();
                ui.label(format!("zoom {:.0} %", self.viewer.zoom_percent(ppp)));
            }
            if let Some(note) = self.display_note() {
                ui.separator();
                ui.label(RichText::new("display missing").color(egui::Color32::from_rgb(255, 190, 90))).on_hover_text(format!("{note}. The windows return to their displays when these are connected again."));
            }
            ui.separator();
            let last = self.last_recovery_ms.map_or_else(|| "No recovery copy has been written yet.".to_owned(), |ms| format!("Last recovery copy: {}", crate::clock::local_stamp(ms)));
            ui.label(RichText::new("autosave").weak().small()).on_hover_text(format!("{last}\nWhile there are unsaved changes a copy is kept every minute so a crash loses little."));
            let mut cancel_xmp = false;
            if let Some(job) = &self.xmp_job {
                ui.separator();
                let frac = if job.total == 0 { 0.0 } else { job.progress() as f32 / job.total as f32 };
                ui.add(egui::ProgressBar::new(frac).desired_width(170.0).text(format!("Reading XMP {}/{}", job.progress(), job.total)));
                cancel_xmp = ui.button("Cancel").clicked();
            }
            if cancel_xmp {
                if let Some(job) = &self.xmp_job {
                    job.cancel();
                }
            }
            if self.renderer.pending() {
                ui.separator();
                ui.label(RichText::new("rendering...").color(egui::Color32::from_rgb(130, 190, 255)));
            }
            let mut cancel = false;
            if let Some(job) = &self.scan {
                ui.separator();
                let n = crate::collection::with_thousands(job.count.unwrap_or(job.counting));
                let detail = match (job.count, job.metadata) {
                    (None, _) => "counting…".to_owned(),
                    (_, Some((done, total))) => format!("metadata {done}/{total}"),
                    (_, None) => "preparing thumbnails…".to_owned(),
                };
                ui.label(format!("{n} images")).on_hover_text(detail);
                cancel = ui.button("Cancel").clicked();
            }
            if cancel {
                if let Some(job) = &self.scan {
                    job.cancel();
                }
            }
            let mut cancel_export = false;
            if let Some(job) = &self.export_job {
                ui.separator();
                let frac = if job.total == 0 { 0.0 } else { job.done as f32 / job.total as f32 };
                ui.add(egui::ProgressBar::new(frac).desired_width(170.0).text(format!("Exporting {}/{}", job.done, job.total)));
                cancel_export = ui.button("Cancel export").clicked();
            }
            if cancel_export {
                if let Some(job) = &self.export_job {
                    job.cancel();
                }
            }
            // Right side: transient notice, else the first problem.
            let mut dismiss_notes = false;
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((text, is_error)) = self.status.current() {
                    let color = if is_error { egui::Color32::from_rgb(255, 150, 120) } else { egui::Color32::from_gray(210) };
                    ui.label(RichText::new(text).color(color));
                } else if let Some(err) = &self.last_error {
                    ui.label(RichText::new(err).color(egui::Color32::from_rgb(255, 150, 120)));
                } else if let Some((text, all)) = self.frame_info.as_ref().and_then(|f| crate::render::warning_line(&f.warnings)) {
                    ui.label(RichText::new(text).color(egui::Color32::from_rgb(255, 190, 90))).on_hover_text(all);
                } else if let Some(first) = self.status.notes().first() {
                    let more = self.status.notes().len() - 1;
                    let text = if more > 0 { format!("Migrated graph: {first} (+{more} more)") } else { format!("Migrated graph: {first}") };
                    let all = self.status.notes().join("
");
                    let r = ui.add(egui::Label::new(RichText::new(text).color(egui::Color32::from_rgb(255, 190, 90))).sense(egui::Sense::click()));
                    dismiss_notes = r.on_hover_text(format!("{all}

Click to dismiss")).clicked();
                } else if let Some(issue) = self.session.project.graph.validate().first() {
                    ui.label(RichText::new(format!("Warning: {}", issue.message)).color(egui::Color32::from_rgb(255, 190, 90)));
                }
            });
            if dismiss_notes {
                self.status.clear_notes();
            }
        });
    }

    /// The scopes widget with a small margin (all positions share it).
    fn scopes_ui(&mut self, ui: &mut egui::Ui) {
        let stale = self.scopes_stale;
        let area = ui.available_rect_before_wrap();
        let step = self.frame_info.as_ref().filter(|f| !self.compare.active() && f.path.is_gpu()).map(|f| f.scope_step);
        egui::Frame::NONE.inner_margin(6.0).show(ui, |ui| {
            if let Some(step) = step {
                scope_step_label(ui, step);
            }
            self.scope_panel.show(ui)
        });
        if stale {
            paint_stale_scopes(ui, area);
        }
    }

    /// Node graph, with the scopes beside it when they are at the bottom right.
    fn graph_area(&mut self, ui: &mut egui::Ui, epoch: u32) {
        if self.scopes_pos == ScopesPos::BottomRight {
            let w = ui.available_width();
            // Dual workspace: the viewer has its own window, so the scopes may never take more
            // than a third of the graph strip (they would squeeze the node graph).
            let max = if self.workspace == Workspace::Dual { w / 3.0 } else { w * 0.75 };
            let default = w * self.ws.layout.scopes_frac;
            self.reset_panel(ui, "scopes-br", epoch);
            let panel = egui::Panel::right(("scopes-br", epoch))
                .resizable(true)
                .default_size(default.max(220.0))
                .size_range(180.0..=max.max(240.0))
                .show(ui, |ui| self.scopes_ui(ui));
            if w > 1.0 {
                self.ws.layout.scopes_frac = (panel.response.rect.width() / w).clamp(0.1, 0.75);
            }
        }
        self.editor.timings = self.timing_debug.then(|| {
            crate::graph_editor::NodeTimings::new(self.node_timings.as_deref().unwrap_or_default(), self.timing_gpu_tail)
        });
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.editor.show(ui, &mut self.session, &mut self.status));
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts {
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard shortcuts").open(&mut open).resizable(false).show(ctx, |ui| {
            let rows = [
                ("Shift/Ctrl+click, drag on empty space", "Add to the node selection, rubber band"),
                ("Ctrl+A", "Select all nodes"),
                ("Ctrl+C / X / V", "Copy / cut / paste nodes (as text)"),
                ("Ctrl+D", "Duplicate the selected nodes"),
                ("C", "Compare view (Esc or C leaves)"),
                ("Ctrl+N / O / S", "New / open project, save"),
                ("Ctrl+Shift+O / S", "Open folder / save as"),
                ("Ctrl+Z", "Undo (graph edits)"),
                ("Ctrl+Y, Ctrl+Shift+Z", "Redo"),
                ("Left / Right", "Previous / next image in the list"),
                ("0-5", "Rating"),
                ("P / X / U", "Pick / reject / unflag"),
                ("6 7 8 9", "Colour label red / yellow / green / blue"),
                ("Tab (over graph)", "Search and add a node"),
                ("Right-click (graph)", "Add node menu"),
                ("Delete, Backspace", "Delete the selected nodes"),
                ("V", "View the selected node"),
                ("M", "Mask overlay for the selected node"),
                ("Double-click node", "View this node"),
                ("F", "Frame all nodes"),
                ("Middle-drag, Space+drag", "Pan the graph"),
                ("Scroll / Ctrl+scroll", "Pan / zoom the graph"),
                ("Drag from a port", "Connect; drag off an input to disconnect"),
                ("Double-click slider", "Reset to default"),
                ("Viewer: click, wheel, drag", "Toggle fit zoom, zoom/navigate, pan"),
                ("Viewer: double-click", "Toggle fit / 100 %"),
                ("Ctrl+0 / Ctrl+1", "Fit / 100 %"),
                ("Ctrl+Alt+1 ... 5", "Workspace presets: Single, Dual, Dual + Scopes, + Clean Feed, Clean Feed only"),
                ("Ctrl+Shift+D", "Dual workspace"),
                ("Ctrl+Shift+C", "Clean feed window"),
                ("Ctrl+Shift+K", "Scopes window"),
                ("Right-click image", "Set as reference, locate a missing file"),
                ("Right-click graph", "Paste here, Recipes"),
                ("File > Import ratings and keywords from XMP", "Stars, rejects, labels and keywords from XMP"),
                ("View > Clear thumbnail cache", "Deletes the thumbnails kept on disk"),
            ];
            egui::Grid::new("shortcut_grid").num_columns(2).spacing([20.0, 4.0]).show(ui, |ui| {
                for (k, d) in rows {
                    ui.label(RichText::new(k).monospace());
                    ui.label(d);
                    ui.end_row();
                }
            });
        });
        self.show_shortcuts = open;
    }

    fn report_window(&mut self, ctx: &egui::Context) {
        if !self.show_report {
            return;
        }
        let Some(r) = &self.scan_report else { return };
        let mut open = true;
        let area = self.window_area;
        egui::Window::new("Scan report").open(&mut open).resizable(true).default_width(520.0).constrain_to(area).default_pos(area.center() - egui::vec2(260.0, 120.0)).show(ctx, |ui| {
            ui.label(summarize(r));
            if !r.failed.is_empty() {
                ui.separator();
                ui.label(RichText::new("Failed files").strong());
                egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    for (path, why) in &r.failed {
                        ui.label(format!("{}: {why}", path.display()));
                    }
                });
            }
        });
        self.show_report = open;
    }

    /// Save / Discard / Cancel dialog for unsaved changes.
    fn prompt_window(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.prompt else { return };
        let mut choice = None;
        egui::Window::new("Unsaved changes").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(format!("\"{}\" has unsaved changes.", self.session.project_name()));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    choice = Some(0);
                }
                if ui.button("Discard").clicked() {
                    choice = Some(1);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(2);
                }
            });
        });
        match choice {
            Some(0) => {
                self.prompt = None;
                // Carried out by `after_save` once the revision is written; a conflict keeps the
                // action (and the application) waiting for the user's decision.
                self.save_project(None, Some(pending), ctx);
            }
            Some(1) => {
                self.prompt = None;
                self.execute(pending, ctx);
            }
            Some(_) => self.prompt = None,
            None => {}
        }
    }

    fn viewer_area(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        ui.horizontal(|ui| {
            if ui.selectable_label(self.compare.active(), "Split").on_hover_text("Toggle split viewer (C)").clicked() { cmds.push(Command::Compare); }
            if ui.button("Fit").clicked() { cmds.push(Command::Fit); }
            if ui.button("100%").clicked() { cmds.push(Command::Actual); }
            ui.separator();
            let pinned = self.session.viewed_node.is_some();
            if crate::icons::button(ui, if pinned { crate::icons::Icon::Unpin } else { crate::icons::Icon::Pin }, if pinned { "Unpin viewer" } else { "Pin viewer" }, pinned || self.session.target_node().is_some()).clicked() {
                self.session.viewed_node = if pinned { None } else { self.session.target_node() };
            }
            ui.label(if pinned { "Pinned" } else { "Following selection" });
            ui.separator();
            let label = ui.add(egui::Label::new("Display exposure (EV)").sense(egui::Sense::click())).on_hover_text("Preview and scopes only. Double-click the slider or label to reset.");
            crate::fine_slider::show(ui, &mut self.session.view.exposure_ev, -6.0..=6.0, 0.0, false, &label, 100.0, 2);
        });
        if self.compare.active() {
            self.compare_area(ui);
            return;
        }
        if self.primary_target().image.is_none() && self.session.total_images() == 0 && self.scan.is_none() {
            ui.centered_and_justified(|ui| { ui.label("Choose a folder in a Source node to preview its images"); });
            return;
        }
        if self.primary_target().node.is_none() {
            ui.centered_and_justified(|ui| { ui.label("Select a node to view it"); });
            return;
        }
        if self.primary_target().image.is_none() { return; }
        if let Some(message) = show_viewer(&mut self.viewer, &mut self.tools, &mut self.session, ui) {
            self.status.error(message);
        }
    }

    fn layout_single(&mut self, ui: &mut egui::Ui, cmds: &mut Vec<Command>) {
        let epoch = self.layout_epoch;
        let total = ui.available_height();
        let layout = self.ws.layout.clone();
        // Without the node graph the strip below the viewer only holds docked scopes (bottom right).
        if layout.show_graph || self.scopes_pos == ScopesPos::BottomRight {
            self.reset_panel(ui, "graph", epoch);
            let graph = egui::Panel::bottom(("graph", epoch))
                .resizable(true)
                .default_size((total * layout.graph_frac).max(200.0))
                .size_range(140.0..=f32::INFINITY)
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    if layout.show_graph {
                        self.graph_area(ui, epoch);
                    } else {
                        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.scopes_ui(ui));
                    }
                });
            if total > 1.0 {
                self.ws.layout.graph_frac = (graph.response.rect.height() / total).clamp(0.1, 0.8);
            }
        }
        self.filmstrip_panel(ui, epoch);
        self.right_column(ui, epoch, 400.0);
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.viewer_area(ui, cmds));
    }

    fn layout_dual(&mut self, ui: &mut egui::Ui, _cmds: &mut Vec<Command>) {
        let epoch = self.layout_epoch;
        self.filmstrip_panel(ui, epoch);
        self.right_column(ui, epoch + 1000, 420.0);
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.graph_area(ui, epoch + 2000));
    }

    /// The inspector (always scrollable), plus the scopes in "Right column" mode.
    fn right_column(&mut self, ui: &mut egui::Ui, epoch: u32, _width: f32) {
        let height = ui.available_height();
        let layout = self.ws.layout.clone();
        let scopes_column = self.scopes_pos == ScopesPos::RightColumn;
        if !layout.show_inspector && !scopes_column {
            return;
        }
        self.reset_panel(ui, "right", epoch);
        let panel = egui::Panel::right(("right", epoch)).resizable(true).default_size(layout.inspector_w).size_range(300.0..=900.0).show(ui, |ui| {
            if scopes_column && layout.show_inspector {
                self.reset_panel(ui, "scopes-col", epoch);
                let scopes = egui::Panel::bottom(("scopes-col", epoch)).resizable(true).default_size((height * layout.scopes_frac).max(220.0)).size_range(120.0..=(height * 0.8).max(200.0)).show(ui, |ui| self.scopes_ui(ui));
                if height > 1.0 {
                    self.ws.layout.scopes_frac = (scopes.response.rect.height() / height).clamp(0.1, 0.75);
                }
            } else if scopes_column {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.scopes_ui(ui));
            }
            if layout.show_inspector {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| inspector::show(ui, &mut self.session));
            }
        });
        self.ws.layout.inspector_w = panel.response.rect.width().clamp(300.0, 900.0);
    }

    fn filmstrip_panel(&mut self, ui: &mut egui::Ui, epoch: u32) {
        if !self.ws.layout.show_filmstrip {
            return;
        }
        self.reset_panel(ui, "filmstrip", epoch);
        let panel = egui::Panel::left(("filmstrip", epoch)).resizable(true).default_size(self.ws.layout.filmstrip_w).size_range(170.0..=420.0).show(ui, |ui| {
            let target = self.primary_target();
            egui::Panel::bottom(("filmstrip-info", epoch)).resizable(false).show_separator_line(true).show(ui, |ui| {
                ui.add_space(4.0);
                Filmstrip::capture_info(ui, &self.session, target.image);
                ui.add_space(4.0);
                ui.separator();
                self.filmstrip.keywords_ui(ui, &mut self.session);
                ui.add_space(4.0);
            });
            // A Source is just folder membership. Don't hold its newly listed
            // placeholders behind an asynchronous graph collection query.
            let immediate = self.scan.as_ref().filter(|job| job.listed)
                .and(target.node)
                .and_then(|id| self.session.project.graph.node(id))
                .filter(|node| node.kind == NodeKind::Source)
                .and_then(|node| node_ui::source_folder(node).map(|folder| (folder, node_ui::source_subfolders(node))))
                .map(|(folder, recursive)| self.session.visible().iter().copied().filter(|&id| {
                    self.session.entry(id).is_some_and(|e| rawgraph_core::path_in_folder(&e.path.to_string_lossy(), &folder, recursive))
                }).collect::<std::collections::HashSet<_>>());
            let members = immediate.as_ref().or_else(|| Self::gallery_members(self.gallery_result.as_ref(), target));
            let groups = self.gallery_result.as_ref().filter(|r| r.view_node.is_some() && r.view_node == target.node).map(|r| r.group_sizes());
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| self.filmstrip.show(ui, &mut self.session, target, members, groups));
            if let Some(job) = self.scan.as_ref().filter(|j| j.listed && (members.is_some() || target.node.is_none())) {
                job.prioritize(self.filmstrip.scan_wanted(), self.filmstrip.scan_thumbs_ready());
            }
        });
        self.ws.layout.filmstrip_w = panel.response.rect.width().clamp(170.0, 420.0);
    }

    fn primary_frame(&self) -> (Option<egui::TextureId>, [u32; 2]) {
        if self.compare.active() {
            self.primary_split_frame()
        } else {
            (self.single_viewer_request().and_then(|_| self.viewer.texture_id()), self.viewer.frame_size)
        }
    }

    fn secondary_windows(&mut self, ctx: &egui::Context) {
        let monitors = self.monitors.clone();
        let viewer_id = ViewportId::from_hash_of(VIEWER_ID);
        let clean_id = ViewportId::from_hash_of(CLEAN_ID);
        let scopes_id = ViewportId::from_hash_of(SCOPES_ID);

        let mut viewer_win = std::mem::take(&mut self.viewer_win);
        let out = viewer_win.show(ctx, viewer_id, "RawGraph - Viewer", false, &monitors, Some(true), |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                if self.compare.active() {
                    self.compare_area(ui);
                } else if let Some(message) = self.single_viewer_request().and_then(|_| show_viewer(&mut self.viewer, &mut self.tools, &mut self.session, ui)) {
                    self.status.error(message);
                }
            });
        });
        self.viewer_win = viewer_win;
        let _ = out;

        let (texture, size) = self.primary_frame();
        let uv = if self.compare.active() { self.cmp_panes[1].viewer.preview_uv } else { self.viewer.preview_uv };
        let editor_focused = ctx.input(|i| {
            i.focused || [viewer_id, clean_id, scopes_id].into_iter().any(|id| {
                i.raw.viewports.get(&id).and_then(|v| v.focused) == Some(true)
            })
        });
        let focus_key = egui::Id::unique("clean_feed_editor_was_focused");
        let editor_refocused = ctx.data_mut(|data| {
            let was_focused = data.get_temp::<bool>(focus_key).unwrap_or(false);
            data.insert_temp(focus_key, editor_focused);
            editor_focused && !was_focused
        });
        let clean_out = self.clean_win.show(ctx, clean_id, "RawGraph - Clean feed", false, &monitors, Some(editor_focused), |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                viewer::paint_clean(ui, texture, size, uv);
            });
        });
        // Not while the feed is a movable window on the primary display (its display is missing):
        // raising it would pop it over the editor again and again.
        if self.clean_win.state.open && !clean_out.close && editor_refocused && !self.clean_win.fallback {
            // Raise without taking keyboard focus from the editor. Immediately
            // restore normal stacking so the feed won't cover other apps later.
            ctx.send_viewport_cmd_to(clean_id, egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
            ctx.send_viewport_cmd_to(clean_id, egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
        }

        let panel = &mut self.scope_panel;
        let stale = self.scopes_stale;
        let step = self.frame_info.as_ref().filter(|f| !self.compare.active() && f.path.is_gpu()).map(|f| f.scope_step);
        self.scopes_win.show(ctx, scopes_id, "RawGraph - Scopes", true, &monitors, None, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let area = ui.available_rect_before_wrap();
                if let Some(step) = step {
                    scope_step_label(ui, step);
                }
                panel.show(ui);
                if stale {
                    paint_stale_scopes(ui, area);
                }
            });
        });
        // Closing a window (Escape, F11, close button) removes it from the workspace; the
        // viewer returns to the single workspace and the scopes to their docked place.
        self.sync_closed_windows();
    }
}

/// Says how many pixels the scopes of a GPU picture were read from.
fn scope_step_label(ui: &mut egui::Ui, step: usize) {
    let text = match step {
        0 | 1 => "scopes: all pixels".to_owned(),
        2 => "scopes: every 2nd pixel".to_owned(),
        n => format!("scopes: every {n}th pixel"),
    };
    let tip = "While a slider is dragged the scopes of the GPU picture are read from every 2nd pixel to stay fast; a moment after you stop they are read from all pixels.";
    ui.label(RichText::new(text).weak().small()).on_hover_text(tip);
}

/// Dims the scopes and says that they belong to the last image that was shown.
fn paint_stale_scopes(ui: &egui::Ui, area: egui::Rect) {
    let painter = ui.painter_at(area);
    painter.rect_filled(area, 0.0, egui::Color32::from_black_alpha(140));
    let galley = painter.layout(
        "Scopes of the last image shown".to_owned(),
        egui::FontId::proportional(13.0),
        egui::Color32::from_gray(225),
        (area.width() - 24.0).max(60.0),
    );
    let r = egui::Rect::from_center_size(area.center(), galley.size()).expand2(egui::vec2(12.0, 7.0));
    painter.rect_filled(r, 5.0, egui::Color32::from_rgba_unmultiplied(16, 18, 22, 225));
    painter.galley(r.center() - galley.size() / 2.0, galley, egui::Color32::WHITE);
}


impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        crate::splash::finish();
        let ctx = ui.ctx().clone();
        self.frames += 1;
        BENCH_FRAMES.store(self.frames, Ordering::Relaxed);
        if self.frames == 20 && self.shot.is_some() {
            eprintln!("window content rect (points): {:?}, pixels per point {}", ctx.content_rect(), ctx.pixels_per_point());
        }
        self.poll_displays(frame, &ctx);
        if !self.startup_done {
            self.startup();
            if self.cli.bench {
                // A benchmark started from a script may get a minimized window, which gets no frames.
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            }
            if let Some((w, h)) = self.cli.size {
                // The builder size alone is not always honoured; ask again.
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.rename_job.is_some() {
            // Never leave a rename half done: stop it (it is put back), then the window can close.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.cancel_rename_for_close();
        } else if ctx.input(|i| i.viewport().close_requested()) && !self.allow_close && self.session.dirty && self.shot.is_none() && self.bench.is_none() && self.first_frame.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.prompt = Some(Pending::Quit);
        }
        self.poll_renderer(&ctx);
        self.poll_scan(&ctx);
        self.poll_capture_refresh();
        self.poll_export();
        self.advance_export_queue(&ctx);
        self.poll_dialog(&ctx);
        self.poll_startup_recovery(&ctx);
        self.poll_xmp();
        self.poll_missing();
        if self.xmp_probe_pending && self.scan.is_none() {
            self.xmp_probe_pending = false;
            self.start_xmp_probe(&ctx);
        }
        if !self.cli_applied && self.scan.is_none() && self.missing_job.is_none() {
            self.apply_cli();
            if self.cli.actual_size {
                self.viewer.actual_size(ctx.pixels_per_point());
            }
            self.cli_extras(&ctx);
            self.cli_export(&ctx);
            self.cli_open_node_dialogs();
        }
        self.sync_source_scans();
        self.start_queued_scan(&ctx);
        self.cli_run_export_node(&ctx);

        self.session.reconcile_image_selection();
        let mut cmds = self.handle_shortcuts(&ctx);
        if self.prompt.is_none() && !self.save_flow.is_open() {
            // A K request of the last frame for a keyword panel that was not drawn is dropped.
            self.filmstrip.end_frame();
            self.handle_culling(&ctx);
            self.handle_compare_keys(&ctx);
        }

        let menu_bottom = egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, &mut cmds)).response.rect.bottom();
        let status_top = egui::Panel::bottom("status").show_separator_line(true).show(ui, |ui| self.status_bar(ui)).response.rect.top();
        let content = ctx.content_rect();
        self.window_area = egui::Rect::from_min_max(egui::pos2(content.left(), menu_bottom + 2.0), egui::pos2(content.right(), (status_top - 2.0).max(menu_bottom + 40.0)));
        self.resolve_compare_pins();
        self.refresh_gallery();
        self.update_selection_preview(&ctx);
        if self.frames == 2 && self.cli.capture() {
            // With --scale the very first frame is laid out in a window of unknown size (thousands of
            // points), and the panels keep the sizes they got then. Capture runs do not keep panel
            // sizes anyway, so lay them out again once the window has its requested size.
            self.ws.reset_panels = true;
        }
        match self.workspace {
            Workspace::Single => self.layout_single(ui, &mut cmds),
            Workspace::Dual => self.layout_dual(ui, &mut cmds),
        }
        self.ws.reset_panels = false;
        self.handle_ui_requests(&ctx);
        self.update_selection_preview(&ctx);
        self.shortcuts_window(&ctx);
        self.report_window(&ctx);
        self.prompt_window(&ctx);
        self.save_flow_window(&ctx);
        self.history_window(&ctx);
        self.export_windows(&ctx);
        self.node_export_windows(&ctx);
        self.secondary_windows(&ctx);
        self.workspace_windows(&ctx);
        self.recovery_window(&ctx);
        self.unfinished_window(&ctx);
        self.xmp_window(&ctx);
        self.relink_windows(&ctx);
        self.poll_rename_job(&ctx);
        self.rename_windows(&ctx);
        self.clear_cache_window(&ctx);
        self.handle_film_actions(&ctx);

        for cmd in cmds {
            self.run(cmd, &ctx);
        }
        self.process_ws_actions();

        // One undo step per finished gesture, then re-render when needed.
        let gesture = ctx.input(|i| i.pointer.any_down()) || ctx.dragged_id().is_some();
        if self.history.commit_if_changed(&self.session.project.graph, gesture) {
            self.session.dirty = true;
        }
        self.session.sanitize();
        self.autosave_step(&ctx);
        self.refresh_collection();
        self.tool_bench_step(&ctx);
        self.first_frame_drive(&ctx);
        self.update_selection_preview(&ctx);
        self.compare_step(&ctx);
        self.submit_if_needed();
        let target = self.primary_target();
        self.filmstrip.request_live(&ctx, &mut self.session, target, self.renderer_mode != RendererMode::Cpu && !self.cli.capture());
        self.filmstrip.poll(&ctx);
        self.crash_step();
        self.report_fill_time();
        self.screenshot_step(&ctx);
        self.bench_step(&ctx);
        self.first_frame_observe(&ctx);
        self.cli_run_export_finish(&ctx);

        let title = self.session.window_title();
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
        if self.status.active() || self.session.scanning {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        // A LUT file edited elsewhere is noticed by the status poll (every couple of seconds); keep
        // frames coming while the graph uses one, so the picture follows without an input event.
        if self.session.project.graph.nodes().any(|n| n.kind == NodeKind::Lut && rawgraph_core::lut_file(n, self.session.current_image()).is_some()) {
            ctx.request_repaint_after(rawgraph_core::lut::refresh_interval().max(Duration::from_millis(500)));
        }
        if !self.renderer.is_alive() {
            self.last_error = Some("The render thread stopped unexpectedly".to_owned());
        }
    }

    fn on_exit(&mut self) {
        // A clean quit: the recovery copies of this session are not needed any more.
        rawgraph_engine::depth::shutdown_depth_workers();
        self.shutdown();
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        // Text pasted into any field comes from the system clipboard (egui keeps its own copy otherwise).
        if self.editor.use_system_clipboard && raw_input.events.iter().any(|e| matches!(e, egui::Event::Paste(_))) {
            if let Some(text) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
                for e in &mut raw_input.events {
                    if let egui::Event::Paste(t) = e {
                        t.clone_from(&text);
                    }
                }
            }
        }
        if self.editor.hovered {
            let open = &mut self.editor.open_search;
            raw_input.events.retain(|e| match e {
                egui::Event::Key { key: Key::Tab, pressed: true, modifiers, .. } if modifiers.is_none() => {
                    *open = true;
                    false
                }
                _ => true,
            });
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.cli.capture() || self.cli.benching() || self.cli.run_export_node.is_some() {
            return;
        }
        let mut workspaces = self.ws.store.clone();
        workspaces.session = Some(self.current_spec());
        storage.set_string(WORKSPACES_KEY, workspaces.to_json());
        let settings = Settings {
            workspace: self.workspace,
            viewer: self.viewer_win.state.clone(),
            clean: self.clean_win.state.clone(),
            scopes: self.scopes_win.state.clone(),
            scopes_pos: self.scopes_pos,
            preview_size: self.session.provider.preview_size(),
            click_zoom: self.viewer.click_zoom,
            wheel_navigates: self.viewer.wheel_navigates,
            editable_thumb_gib: self.filmstrip.live.budget(),
            always_rewrite: rawgraph_project::always_rewrite(),
            // GPU-only is a test setting from the command line, never saved.
            renderer: if self.renderer_mode == RendererMode::Gpu { RendererMode::Auto } else { self.renderer_mode },
        };
        eframe::set_value(storage, SETTINGS_KEY, &settings);
    }

    fn persist_egui_memory(&self) -> bool {
        !self.cli.capture() && !self.cli.benching() && self.cli.run_export_node.is_none()
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        egui::Color32::from_rgb(20, 22, 26).to_normalized_gamma_f32()
    }
}

/// Appends `ext` unless the name already ends with it; a trailing `strip`
/// (e.g. `.json` for `.rawgraph.json`) is replaced.
fn ensure_extension(path: PathBuf, ext: &str, strip: Option<&str>) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "untitled".to_owned());
    if name.to_lowercase().ends_with(ext) {
        return path;
    }
    let mut stem = name;
    if let Some(s) = strip.and_then(|s| stem.strip_suffix(s)) {
        stem = s.to_owned();
    }
    path.with_file_name(format!("{stem}{ext}"))
}

/// Bind an explicit open-folder action to a Source, never to the catalog alone.
/// A selected Source is intentional; otherwise use an empty Source or add one.
fn bind_open_folder(session: &mut Session, folder: &std::path::Path, recursive: bool) -> NodeId {
    let graph = &mut session.project.graph;
    let selected = session.selected_node.filter(|id| graph.node(*id).is_some_and(|n| n.kind == NodeKind::Source));
    let empty = graph.nodes().find(|n| n.kind == NodeKind::Source && matches!(n.data(ImageId(0)), Some(rawgraph_core::NodeData::Folder(f)) if f.is_empty())).map(|n| n.id);
    let id = selected.or(empty).unwrap_or_else(|| graph.add_node(NodeKind::Source, [0.0, 300.0]));
    node_ui::set_source_folder(graph, id, Some(folder.to_string_lossy().into_owned()));
    let node = graph.node_mut(id).unwrap();
    let mode = node.mode;
    node.mode = rawgraph_core::Mode::All;
    node.set_param("subfolders", ImageId(0), if recursive { 1.0 } else { 0.0 });
    node.mode = mode;
    session.dirty = true;
    id
}

/// True when the finished export's report goes to the application's own log; a
/// `--run-export-node` run prints it once, on standard output, itself.
pub fn report_on_stderr(cli: &Cli) -> bool {
    cli.run_export_node.is_none()
}

/// Applies the `--select-node`, `--rate`, `--set`, ... options.
pub fn apply_cli_state(session: &mut Session, cli: &Cli) {
    session.inspector_scroll = cli.inspector_scroll;
    if cli.demo_graph && !session.demo {
        session.project.graph = crate::session::demo_graph_neutral();
        session.dirty = true;
    }
    for (kind, pos) in &cli.add_node {
        let n = session.project.graph.nodes().count();
        let pos = pos.unwrap_or([(n % 7) as f32 * 240.0, 200.0 + (n / 7) as f32 * 160.0]);
        let id = session.project.graph.add_node(*kind, pos);
        if *kind == NodeKind::Source { node_ui::set_source_folder(&mut session.project.graph, id, None); }
    }
    for &(node, x, y) in &cli.move_node {
        if let Some(n) = session.node_by_index(node).and_then(|id| session.project.graph.node_mut(id)) {
            n.pos = [x, y];
        } else {
            eprintln!("--move-node: there is no node {node}");
        }
    }
    for &(from, to, port) in &cli.connect {
        match (session.node_by_index(from), session.node_by_index(to)) {
            (Some(f), Some(t)) => {
                if let Err(e) = session.project.graph.connect(f, t, port) {
                    eprintln!("--connect {from}:{to}:{port}: {e}");
                }
            }
            _ => eprintln!("--connect {from}:{to}:{port}: no such node"),
        }
    }
    for &(from, from_port, to, port) in &cli.connect_ports {
        match (session.node_by_index(from), session.node_by_index(to)) {
            (Some(f), Some(t)) => {
                if let Err(e) = session.project.graph.connect_ports(f, from_port, t, port) {
                    eprintln!("--connect {from}.{from_port}:{to}:{port}: {e}");
                }
            }
            _ => eprintln!("--connect {from}.{from_port}:{to}:{port}: no such node"),
        }
    }
    for &(image, rating) in &cli.rate {
        if let Some(id) = session.visible().get(image).copied() {
            session.set_rating(id, rating);
        } else {
            eprintln!("--rate: there is no image {image}");
        }
    }
    for (image, keywords) in &cli.keyword {
        if let Some(id) = session.visible().get(*image).copied() {
            session.edit_keywords(&[id], |list| rawgraph_project::add_keywords_to_list(list, keywords));
        } else {
            eprintln!("--keyword: there is no image {image}");
        }
    }
    if !cli.filter_keyword.is_empty() {
        session.filter.keywords = cli.filter_keyword.clone();
        session.refresh_visible();
    }
    for &(image, flag) in &cli.flag {
        if let Some(id) = session.visible().get(image).copied() {
            session.set_flag(id, flag);
        }
    }
    for &(image, label) in &cli.label {
        if let Some(id) = session.visible().get(image).copied() {
            let current = session.entry(id).and_then(|e| e.color_label);
            match (label, current) {
                (Some(l), c) if c != Some(l) => {
                    session.toggle_label(id, l);
                }
                (None, Some(c)) => {
                    session.toggle_label(id, c);
                }
                _ => {}
            }
        }
    }
    if cli.missing_only {
        session.filter.missing_only = true;
        session.refresh_visible();
    }
    if let Some(n) = cli.min_rating {
        session.filter.min_rating = n;
        session.refresh_visible();
    }
    if let Some((lo, hi)) = cli.filter_iso {
        session.filter.iso_min = lo.map(|v| v.to_string()).unwrap_or_default();
        session.filter.iso_max = hi.map(|v| v.to_string()).unwrap_or_default();
        session.refresh_visible();
    }
    if let Some(i) = cli.select_image {
        session.select_index(i);
    }
    if let Some((a, b)) = cli.select_range {
        let ids = session.visible().to_vec();
        if let Some(first) = ids.get(a).copied() {
            session.selected = Some(first);
            if let Some(last) = ids.get(b).copied() {
                session.click_image(last, false, true, &ids);
            }
        }
    }
    for &(node, port) in &cli.disconnect {
        if let Some(id) = session.node_by_index(node) {
            session.project.graph.disconnect(id, port);
        }
    }
    for &n in &cli.individual {
        if let Some(id) = session.node_by_index(n) {
            session.set_mode(id, Mode::Individual);
        }
    }
    let image = session.current_image();
    for (node, ranges) in &cli.split_ranges {
        match session.node_by_index(*node) {
            Some(id) => {
                if let Err(e) = crate::tool_panels::apply_ranges(session, id, image, ranges) {
                    eprintln!("--split-ranges {node}: {e}");
                }
            }
            None => eprintln!("--split-ranges: there is no node {node}"),
        }
    }
    for (node, points) in &cli.curve {
        let id = session.node_by_index(*node).filter(|id| session.project.graph.node(*id).is_some_and(|n| n.kind == NodeKind::PointCurve));
        match id.and_then(|id| session.project.graph.node_mut(id)) {
            Some(n) => {
                if !n.set_data(image, Some(rawgraph_core::NodeData::Curve(points.clone()))) {
                    eprintln!("--curve: node {node} refused the curve");
                }
            }
            None => eprintln!("--curve: node {node} is not a Curve node"),
        }
    }
    for (node, channel, points) in &cli.curves {
        let id = session.node_by_index(*node).filter(|id| session.project.graph.node(*id).is_some_and(|n| n.kind == NodeKind::PointCurve));
        let Some(id) = id else {
            eprintln!("--curve: node {node} is not a Curves node");
            continue;
        };
        let mut points = points.clone();
        if *channel == 5 {
            crate::curve_logic::wrap_from_first(&mut points);
        }
        if let Err(e) = crate::curve_editor::store_channel(session, id, image, *channel, points) {
            eprintln!("--curve: node {node} refused the curve: {e}");
        }
        if let Some(n) = session.project.graph.node_mut(id) {
            n.set_param("channel", image, f32::from(*channel));
        }
    }
    for (node, folder) in &cli.set_folder {
        let ok = session.node_by_index(*node).is_some_and(|id| node_ui::set_source_folder(&mut session.project.graph, id, Some(folder.to_string_lossy().into_owned())));
        if !ok {
            eprintln!("--set-folder: node {node} is not a Source node");
        }
    }
    for (node, field, value) in &cli.set_text {
        let result = match (session.node_by_index(*node), node_ui::ExportField::parse(field)) {
            (Some(id), Some(f)) => node_ui::set_export_text(&mut session.project.graph, id, image, f, value),
            _ => Err(format!("node {node} or the field \"{field}\" does not exist")),
        };
        if let Err(e) = result {
            eprintln!("--set-text: {e}");
        }
    }
    for (node, points, look) in &cli.paint {
        let Some(id) = session.node_by_index(*node).filter(|id| session.project.graph.node(*id).is_some_and(|n| n.kind == NodeKind::Draw)) else {
            eprintln!("--paint: node {node} is not a Draw");
            continue;
        };
        let image = session.draw_image(id, image);
        let mut settings = session.brush.clone();
        if let Some([size, hardness, flow]) = look {
            settings.size_pct = *size;
            settings.hardness = *hardness;
            settings.flow = *flow;
        }
        let mut stroke = settings.new_stroke(false, points[0]);
        stroke.points.extend_from_slice(&points[1..]);
        let mut strokes = session.project.graph.node(id).map(|n| crate::tools::strokes_of(n, image)).unwrap_or_default();
        strokes.push(stroke);
        if !crate::tools::set_strokes(session, id, image, strokes) {
            eprintln!("--paint: node {node} refused the stroke");
        }
    }
    for (node, param, value) in &cli.set {
        if let Some(n) = session.node_by_index(*node).and_then(|id| session.project.graph.node_mut(id)) {
            if !n.set_param(param, image, *value) {
                eprintln!("--set: node {node} ({}) has no parameter \"{param}\"", n.kind.label());
            }
        } else {
            eprintln!("--set: there is no node {node}");
        }
    }
    for &(node, target) in &cli.set_ev {
        let capture_ev = session.current_entry().and_then(|e| e.capture.ev100());
        let (Some(id), Some(ev)) = (session.node_by_index(node), capture_ev) else {
            eprintln!("--set-ev: node {node} or the capture EV is missing");
            continue;
        };
        let up = crate::develop::upstream_delta(&session.project.graph, id, image);
        let delta = crate::develop::delta_for_absolute(ev, up, target);
        if let Some(n) = session.project.graph.node_mut(id) {
            n.set_param("delta", image, delta);
        }
    }
    if let Some(n) = cli.select_node {
        session.selected_node = session.node_by_index(n);
    }
    if let Some(n) = cli.view_node {
        session.viewed_node = session.node_by_index(n);
    }
    if !cli.select_nodes.is_empty() {
        let ids: Vec<NodeId> = cli.select_nodes.iter().filter_map(|n| session.node_by_index(*n)).collect();
        if ids.len() != cli.select_nodes.len() {
            eprintln!("--select-nodes: some of the nodes do not exist");
        }
        let mut sel = crate::selection::NodeSelection::default();
        sel.replace(ids);
        session.set_node_selection(sel);
    }
    if let Some(name) = &cli.group_nodes {
        let ids: Vec<NodeId> = session.node_sel.ids().to_vec();
        match session.project.graph.group_nodes(&ids, name) {
            Ok(group) => session.set_node_selection(crate::selection::NodeSelection::single(group)),
            Err(e) => eprintln!("--group-nodes: {e}"),
        }
    }
    for (n, param, label) in &cli.publish {
        let Some(node) = session.node_by_index(*n) else {
            eprintln!("--publish: there is no node {n}");
            continue;
        };
        let Some(group) = session.project.graph.parent_of(node) else {
            eprintln!("--publish: node {n} is not inside a group");
            continue;
        };
        if let Err(e) = session.project.graph.publish_param(group, node, param, label.as_deref()) {
            eprintln!("--publish: {e}");
        }
    }
    // Deletions come last, so every index above still means the original numbering.
    let doomed: Vec<NodeId> = cli.delete_node.iter().filter_map(|n| session.node_by_index(*n)).collect();
    for id in doomed {
        if let Err(why) = session.delete_node(id) {
            eprintln!("--delete-node: {why}");
        }
    }
}

/// Shows the viewer and lets the tool of the selected node work on top of it.
pub fn show_viewer(viewer: &mut Viewer, tools: &mut Tools, session: &mut Session, ui: &mut egui::Ui) -> Option<String> {
    crate::tools::prepare_viewer(viewer, session);
    if tools.handles.claims_pointer(session) {
        // A handle under the pointer owns the primary button: no pan, no click zoom.
        viewer.tool_active = true;
    }
    let geom = viewer.show(ui);
    let result = if viewer.awaiting_final() { None } else { geom.as_ref().and_then(|geom| tools.show(ui, geom, session, viewer.frame_size)) };
    let step = std::mem::take(&mut viewer.navigation_step);
    if step != 0 { session.step_viewer_image(step); }
    result
}

/// Index visited at step `i` of a stride walk over `n` images (no two
/// consecutive visits are neighbours when n is large enough).
fn stride_index(i: usize, n: usize) -> usize {
    fn gcd(a: usize, b: usize) -> usize {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let s = [5usize, 7, 3, 11, 13].into_iter().find(|s| *s < n && gcd(*s, n) == 1).unwrap_or(1);
    (i * s) % n.max(1)
}

fn print_bench(b: &Bench, session: &Session, mode: RendererMode, refined: &[f32]) {
    let stats = |v: &[f32]| -> String {
        if v.is_empty() {
            return "n/a".to_owned();
        }
        let mut s = v.to_vec();
        s.sort_by(f32::total_cmp);
        format!("n={} min {:.2} median {:.2} max {:.2} ms", s.len(), s[0], s[s.len() / 2], s[s.len() - 1])
    };
    eprintln!("BENCH renderer setting: {}; the frames came from the {} path", mode.label(), b.path_label);
    eprintln!("BENCH select uncached image -> frame shown: {}", stats(&b.uncached));
    eprintln!("BENCH select cached image -> frame shown:   {}", stats(&b.cached));
    eprintln!("BENCH step to prefetched neighbour:         {}", stats(&b.prefetched));
    let lat: Vec<f32> = b.exposure.iter().map(|e| e.0).collect();
    let work: Vec<f32> = b.exposure.iter().map(|e| e.1).collect();
    let scopes: Vec<f32> = b.exposure.iter().map(|e| e.2).collect();
    eprintln!("BENCH exposure change -> frame shown:        {}", stats(&lat));
    eprintln!("BENCH exposure change worker eval+display:   {}", stats(&work));
    eprintln!("BENCH develop exposure change -> frame:      {}", stats(&b.dev_exposure));
    eprintln!("BENCH develop contrast change -> frame:      {}", stats(&b.dev_contrast));
    let dev_lat: Vec<f32> = b.develop.iter().map(|e| e.0).collect();
    let dev_work: Vec<f32> = b.develop.iter().map(|e| e.1).collect();
    eprintln!("BENCH develop white-balance change -> frame: {}", stats(&dev_lat));
    eprintln!("BENCH develop white-balance worker eval+display: {}", stats(&dev_work));
    eprintln!("BENCH tone-curve change -> frame shown:      {}", stats(&b.tone));
    eprintln!("BENCH curve-point drag -> frame shown:       {}", stats(&b.curve));
    eprintln!("BENCH hue/saturation change -> frame shown:  {}", stats(&b.hue));
    eprintln!("BENCH develop lights/shadows (local tone) change -> frame: {}", stats(&b.lights));
    eprintln!("BENCH filter radius change -> frame:         {}", stats(&b.filter));
    eprintln!("BENCH sharpen amount change -> frame:        {}", stats(&b.sharpen));
    eprintln!("BENCH step to a neighbour (proxy decoded, idle time to preload): {}", stats(&b.steps));
    eprintln!("BENCH scopes while dragging (worker, exposure+Develop frames): {}", stats(&b.drag_scopes));
    eprintln!("BENCH scopes at rest (last frame -> exact scopes shown; GPU only, includes the 150 ms idle wait): {}", stats(refined));
    eprintln!("BENCH scopes set_image (UI thread):          {}", stats(&scopes));
    eprintln!("BENCH scopes UI upload (set_prepared), all frames: {}", stats(&b.scopes));
    eprintln!("BENCH scopes worker analyze+prepare, all frames: {}", stats(&b.scopes_worker));
    eprintln!("BENCH collection counts for {} images: {:.1} ms", session.total_images(), session.collection.millis);
    eprintln!("BENCH proxy cache holds {} MiB", session.provider.cached_bytes() >> 20);
    eprintln!("BENCH total run {:.1} s", b.started.elapsed().as_secs_f32());
    let pid = std::process::id();
    if let Ok(out) = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &format!("$p=Get-Process -Id {pid}; '{{0:N0}} MiB working set, {{1:N0}} MiB peak' -f ($p.WorkingSet64/1MB), ($p.PeakWorkingSet64/1MB)")])
        .output()
    {
        eprintln!("BENCH process memory: {}", String::from_utf8_lossy(&out.stdout).trim());
    }
}

fn setup_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles.insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Button, FontId::new(14.0, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Small, FontId::new(12.0, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional));
        style.text_styles.insert(TextStyle::Monospace, FontId::new(13.0, FontFamily::Monospace));
        style.spacing.item_spacing = egui::vec2(8.0, 5.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.spacing.slider_rail_height = 4.0;
        style.visuals.slider_trailing_fill = true;
        style.visuals.panel_fill = egui::Color32::from_rgb(33, 35, 40);
        style.visuals.window_fill = egui::Color32::from_rgb(38, 40, 46);
        style.visuals.extreme_bg_color = egui::Color32::from_rgb(22, 24, 28);
        style.visuals.selection.bg_fill = egui::Color32::from_rgb(64, 110, 190);
    });
}

/// Placement of an edited gallery thumbnail as a selection placeholder: it is already the
/// node's output (transforms and crop applied), so it maps 1:1 onto the final frame.
fn edited_geometry(session: &crate::session::Session, node: rawgraph_core::NodeId, image: rawgraph_core::ImageId, tex: &egui::TextureHandle) -> crate::quick_preview::Geometry {
    let [w, h] = tex.size();
    let source = crate::quick_preview::estimate_proxy_size(&session.provider, image, (w as u32, h as u32));
    let out = crate::quick_preview::geometry(session, node, image, source).size;
    // Final frame's long edge, the thumbnail's own aspect (it is already the node's output).
    let scale = out[0].max(out[1]).max(1) as f64 / w.max(h).max(1) as f64;
    crate::quick_preview::Geometry {
        size: [((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1)],
        uv: [egui::pos2(0.0, 0.0), egui::pos2(1.0, 0.0), egui::pos2(1.0, 1.0), egui::pos2(0.0, 1.0)],
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn open_folder_binds_a_source_without_overwriting_other_sources() {
        use super::*;
        let mut s = Session::new(crate::session::fresh_project(), provider());
        let first = bind_open_folder(&mut s, std::path::Path::new("/photos/a"), false);
        assert_eq!(node_ui::source_folder(s.project.graph.node(first).unwrap()).as_deref(), Some("/photos/a"));
        assert!(!node_ui::source_subfolders(s.project.graph.node(first).unwrap()));
        let second = bind_open_folder(&mut s, std::path::Path::new("/photos/b"), true);
        assert_ne!(first, second);
        assert_eq!(node_ui::source_folder(s.project.graph.node(first).unwrap()).as_deref(), Some("/photos/a"));
        s.selected_node = Some(first);
        assert_eq!(bind_open_folder(&mut s, std::path::Path::new("/photos/c"), true), first);
        assert_eq!(node_ui::source_folder(s.project.graph.node(second).unwrap()).as_deref(), Some("/photos/b"));
    }
    use super::*;
    use crate::session::tests::{demo_session, png_project, provider};
    use crate::session::demo_graph;

    fn headless_app(dir: &std::path::Path) -> App {
        let ctx = egui::Context::default();
        App::new(&eframe::CreationContext::_new_kittest(ctx), Cli { demo: true, data_dir: Some(dir.to_owned()), ..Default::default() })
    }

    fn load_gallery(app: &mut App) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            app.refresh_gallery();
            if App::gallery_members(app.gallery_result.as_ref(), app.primary_target()).is_some() { return; }
            assert!(Instant::now() < until, "gallery worker did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn gallery_selection_displays_cached_pixels_before_the_worker_reply() {
        use rawgraph_engine::ImageProvider;
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let ctx = egui::Context::default();
        app.session.selected_node = app.session.node_by_index(0);
        app.frames = SETTLE_FRAMES;
        load_gallery(&mut app);
        let before = app.single_viewer_request().unwrap().0;
        app.session.select_index(1);
        let after = app.single_viewer_request().unwrap().0;
        assert_ne!(before, after);
        app.session.provider.image(after).unwrap();
        ctx.run_ui(egui::RawInput::default(), |_| app.update_selection_preview(&ctx)).drop_without_applying_deltas();
        assert_eq!(app.viewer.preview_image, Some(after));
        let placeholder = app.viewer.texture_id().expect("available in the selection update");
        assert_eq!(app.viewer.badge(&ctx, Instant::now()), Some("Preview"));
        assert!(app.scopes_stale);
        app.submit_if_needed();
        let until = Instant::now() + Duration::from_secs(10);
        while app.viewer.awaiting_final() {
            ctx.run_ui(egui::RawInput::default(), |_| app.poll_renderer(&ctx)).drop_without_applying_deltas();
            assert!(Instant::now() < until, "render did not complete");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_ne!(app.viewer.texture_id().unwrap(), placeholder);
        assert_eq!(app.viewer.badge(&ctx, Instant::now()), None);
        assert!(!app.scopes_stale);
        app.shutdown();
    }

    #[test]
    fn single_viewer_pins_gallery_and_scope_request_until_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let develop = app.session.node_by_index(1).unwrap();
        let generator = app.session.project.graph.add_node(NodeKind::Generator, [0.0; 2]);
        app.session.selected_node = Some(generator);
        app.session.viewed_node = Some(develop);
        app.frames = SETTLE_FRAMES;
        app.scopes_pos = ScopesPos::RightColumn;
        load_gallery(&mut app);
        let members = App::gallery_members(app.gallery_result.as_ref(), app.primary_target()).unwrap();
        assert_eq!(app.session.gallery_images(members), app.session.visible());
        app.submit_if_needed();
        let key = app.last_key.as_ref().unwrap();
        assert_eq!(key.node, develop);
        assert!(key.scopes.is_some());
        assert_eq!(app.session.selected_node, Some(generator), "inspector selection is independent");

        app.session.viewed_node = None;
        assert!(App::gallery_members(app.gallery_result.as_ref(), app.primary_target()).is_none(), "discard stale membership");
        load_gallery(&mut app);
        let members = App::gallery_members(app.gallery_result.as_ref(), app.primary_target()).unwrap().clone();
        assert_eq!(members.len(), 1);
        app.session.step_viewer_image(1);
        app.submit_if_needed();
        let key = app.last_key.as_ref().unwrap();
        assert_eq!(key.node, generator);
        assert!(members.contains(&key.image));
        assert!(key.scopes.is_some());
        app.shutdown();
    }

    #[test]
    fn split_gallery_scopes_and_clean_feed_follow_right_pins() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let develop = app.session.node_by_index(1).unwrap();
        let generator = app.session.project.graph.add_node(NodeKind::Generator, [0.0; 2]);
        let ctx = egui::Context::default();
        app.toggle_compare();
        app.frames = SETTLE_FRAMES;
        app.scopes_pos = ScopesPos::RightColumn;
        let image = app.session.visible()[1];
        app.compare.panes[1].node = Some(develop);
        app.compare.panes[1].image = Some(image);
        app.compare.panes[0].node = Some(generator);
        app.session.selected_node = Some(generator);
        let (ids, captures) = app.session.id_captures();
        let generator_image = *crate::collection::view_members(&app.session.project.graph, &ids, &captures, generator).iter().next().unwrap();
        app.compare.panes[0].image = Some(generator_image);
        load_gallery(&mut app);
        let target = app.primary_target();
        assert_eq!((target.node, target.image), (Some(develop), Some(image)));
        let members = App::gallery_members(app.gallery_result.as_ref(), target).unwrap();
        assert!(members.contains(&image) && !members.contains(&generator_image));
        let selection = app.session.selected;
        app.session.step_viewer_target(target, 1);
        assert_eq!(app.session.selected, selection, "stepping cannot move the image pin");
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            ctx.run_ui(egui::RawInput::default(), |_| app.compare_step(&ctx)).drop_without_applying_deltas();
            // The CPU renderer publishes its picture before the asynchronous scopes.
            if app.cmp_ready() && !app.scope_panel.needs_refresh() { break; }
            assert!(Instant::now() < until, "compare frames or primary scopes did not arrive");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.pane_key(1).map(|key| (key.node, key.image)), Some((develop, image)));
        assert!(app.primary_frame().0.is_some(), "right render supplies the Clean Feed texture");
        let frame = app.primary_frame();
        app.compare.panes[0] = Default::default();
        assert_eq!(app.primary_target(), target);
        assert_eq!(app.primary_frame(), frame);
        app.compare.panes[1].node = None;
        app.resolve_compare_pins();
        assert_eq!(app.primary_target(), target, "image-only pin retains its last compatible stage");
        app.compare.panes[1].image = None;
        app.resolve_compare_pins();
        assert_eq!(app.primary_target().node, Some(generator));
        app.shutdown();
    }

    #[test]
    fn editable_thumbnail_budget_is_persisted_and_legacy_settings_default_to_eight() {
        #[derive(Default)]
        struct Storage(std::collections::HashMap<String, String>);
        impl eframe::Storage for Storage {
            fn get_string(&self, key: &str) -> Option<String> { self.0.get(key).cloned() }
            fn set_string(&mut self, key: &str, value: String) { self.0.insert(key.to_owned(), value); }
            fn remove_string(&mut self, key: &str) { self.0.remove(key); }
            fn flush(&mut self) {}
        }
        let mut storage = Storage::default();
        storage.0.insert(SETTINGS_KEY.to_owned(), "()".to_owned());
        assert_eq!(eframe::get_value::<Settings>(&storage, SETTINGS_KEY).unwrap().editable_thumb_gib, 8);
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        app.filmstrip.live.set_budget(23);
        eframe::App::save(&mut app, &mut storage);
        assert_eq!(eframe::get_value::<Settings>(&storage, SETTINGS_KEY).unwrap().editable_thumb_gib, 23);
        app.shutdown();
    }

    #[test]
    fn deleting_last_source_blanks_viewer_and_gallery_and_undo_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let ctx = egui::Context::default();
        app.session.project.graph = Graph::new();
        let source = app.session.project.graph.add_node(NodeKind::Source, [0.0; 2]);
        app.history.reset(&app.session.project.graph);
        app.session.set_node_selection(crate::selection::NodeSelection::single(source));
        app.session.viewed_node = Some(source);
        app.frames = SETTLE_FRAMES;
        load_gallery(&mut app);
        app.submit_if_needed();
        let until = Instant::now() + Duration::from_secs(10);
        while app.primary_frame().0.is_none() {
            ctx.run_ui(egui::RawInput::default(), |_| app.poll_renderer(&ctx)).drop_without_applying_deltas();
            assert!(Instant::now() < until, "source viewer frame did not arrive");
            std::thread::sleep(Duration::from_millis(2));
        }
        app.run(Command::DeleteNode, &ctx);
        assert!(app.history.commit_if_changed(&app.session.project.graph, false));
        app.refresh_gallery();
        app.submit_if_needed();
        ctx.run_ui(egui::RawInput::default(), |ui| {
            app.filmstrip_panel(ui, 0);
            app.viewer_area(ui, &mut Vec::new());
        }).drop_without_applying_deltas();
        assert!(App::gallery_members(app.gallery_result.as_ref(), app.primary_target()).is_none());
        assert!(app.primary_frame().0.is_none());
        assert!(app.last_key.is_none() && app.last_error.is_none());
        assert!(!app.status.current().is_some_and(|(_, error)| error));
        app.run(Command::Undo, &ctx);
        assert_eq!(app.session.project.graph.node(source).unwrap().kind, NodeKind::Source);
        app.shutdown();
    }

    #[test]
    fn extensions_are_enforced() {
        assert_eq!(ensure_extension(PathBuf::from("a/b"), GRAPH_EXT, Some(".json")), PathBuf::from("a/b.rawgraph.json"));
        assert_eq!(ensure_extension(PathBuf::from("a/b.json"), GRAPH_EXT, Some(".json")), PathBuf::from("a/b.rawgraph.json"));
        assert_eq!(ensure_extension(PathBuf::from("a/b.rawgraph.json"), GRAPH_EXT, Some(".json")), PathBuf::from("a/b.rawgraph.json"));
        assert_eq!(ensure_extension(PathBuf::from("a/p"), PROJECT_EXT, None), PathBuf::from("a/p.rawgraph"));
    }

    #[test]
    fn cli_state_is_applied_in_order() {
        let mut s = demo_session();
        let cli = crate::cli::parse_args(
            "--select-image 1 --select-node 4 --individual 4 --set 4:ev=2 --view-node 6 --disconnect 6:0".split_whitespace().map(str::to_owned),
        )
        .unwrap();
        apply_cli_state(&mut s, &cli);
        let exp = s.node_by_index(4).unwrap();
        let n = s.project.graph.node(exp).unwrap();
        assert_eq!(n.mode, Mode::Individual);
        assert!(n.has_individual(ImageId(2)));
        assert!(!n.has_individual(ImageId(1)));
        assert_eq!(s.selected_node, Some(exp));
        assert_eq!(s.viewed_node, s.node_by_index(6));
        assert!(s.project.graph.input_of(s.node_by_index(6).unwrap(), 0).is_none());
    }

    #[test]
    fn cli_rate_and_iso_filter_edit_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 4), provider());
        let ids = s.visible().to_vec();
        let cli = crate::cli::parse_args("--rate 2:4 --rate 9:1 --filter-iso 100:200".split_whitespace().map(str::to_owned)).unwrap();
        apply_cli_state(&mut s, &cli);
        assert!(s.dirty);
        assert_eq!(s.entry(ids[2]).unwrap().rating, 4);
        assert_eq!(s.filter.iso_min, "100");
        assert_eq!(s.filter.iso_max, "200");
        assert!(s.visible().is_empty(), "PNGs carry no ISO, so an ISO filter hides them");
    }

    #[test]
    fn a_lut_reload_or_file_change_gives_a_new_request_key() {
        let s = demo_session();
        let out = s.output_node().unwrap();
        let a = RequestKey::new(&s, out, None, None);
        assert!(a == RequestKey::new(&s, out, None, None));
        rawgraph_core::lut::forget(std::path::Path::new("D:/luts/never-loaded.cube"));
        assert!(a != RequestKey::new(&s, out, None, None), "Reload (or a changed file) renders again");
    }

    #[test]
    fn identical_state_gives_equal_request_keys_regardless_of_node_position() {
        // Other tests reload LUTs in parallel, which moves the global LUT generation: not part of this test.
        let key = |s: &Session, out, prep, overlay| RequestKey { lut_generation: 0, ..RequestKey::new(s, out, prep, overlay) };
        let mut s = demo_session();
        let out = s.output_node().unwrap();
        let a = key(&s, out, None, None);
        let id = s.node_by_index(3).unwrap();
        s.project.graph.node_mut(id).unwrap().pos = [999.0, 999.0];
        assert!(a == key(&s, out, None, None), "moving a node must not trigger a render");
        s.project.graph.node_mut(id).unwrap().set_param("temp", ImageId(1), 20.0);
        assert!(a != key(&s, out, None, None), "a parameter change must");
        let b = key(&s, out, None, None);
        s.provider.set_preview_size(1280);
        assert!(b != key(&s, out, None, None), "a new preview size must");
        let c = key(&s, out, Some(PrepareSettings::default()), None);
        let changed = PrepareSettings { gain: 2.0, ..PrepareSettings::default() };
        assert!(c == key(&s, out, Some(PrepareSettings::default()), None), "same scope settings: no new request");
        assert!(c != key(&s, out, Some(changed), None), "a scope settings change must trigger a new request");
        assert!(c != key(&s, out, None, None), "hiding the scopes must too");
        assert_eq!(demo_graph().nodes().count(), 9);
    }

    #[test]
    fn the_mask_overlay_and_new_metadata_trigger_exactly_one_new_request() {
        // Other tests reload LUTs in parallel, which moves the global LUT generation: not part of this test.
        let key = |s: &Session, out, prep, overlay| RequestKey { lut_generation: 0, ..RequestKey::new(s, out, prep, overlay) };
        let s = demo_session();
        let out = s.output_node().unwrap();
        let mask = s.project.graph.nodes().find(|n| n.kind == NodeKind::Generator).unwrap().id;
        let base = key(&s, out, None, None);
        assert!(base != key(&s, out, None, Some(mask)), "switching the overlay on asks once");
        assert!(key(&s, out, None, Some(mask)) == key(&s, out, None, Some(mask)));
        // New capture metadata (background refresh) changes the revision.
        let mut c = s.entry(ImageId(1)).unwrap().capture.clone();
        c.as_shot_kelvin = Some(4200.0);
        s.provider.set_captures([(ImageId(1), c)]);
        assert!(base != key(&s, out, None, None), "a changed capture must re-render");
    }

    #[test]
    fn cli_selects_several_nodes_and_filters_the_filmstrip() {
        let mut s = demo_session();
        let cli = crate::cli::parse_args("--select-nodes 2,3,4 --min-rating 2".split_whitespace().map(str::to_owned)).unwrap();
        apply_cli_state(&mut s, &cli);
        assert_eq!(s.node_sel.len(), 3);
        assert_eq!(s.selected_node, s.node_by_index(4), "the last one is the primary node");
        assert_eq!(s.filter.min_rating, 2);
        let cli = crate::cli::parse_args("--select-nodes 2,99".split_whitespace().map(str::to_owned)).unwrap();
        apply_cli_state(&mut s, &cli);
        assert_eq!(s.node_sel.len(), 1, "a node that does not exist is skipped");
    }

    #[test]
    fn the_new_commands_have_shortcuts_that_do_not_clash() {
        assert_eq!(shortcut_of(Command::Copy), Some(sc(Modifiers::COMMAND, Key::C)));
        assert_eq!(shortcut_of(Command::Compare), Some(sc(Modifiers::NONE, Key::C)));
        for c in [Command::Copy, Command::Cut, Command::Paste, Command::Duplicate, Command::SelectAll, Command::Compare] {
            assert!(!SHORTCUT_ORDER.contains(&c), "{c:?} is handled by the editor or its own key handler, not by the global list");
        }
        // Ctrl+Shift+D (dual workspace) is checked before the editor sees Ctrl+D.
        let pos = |c: Command| SHORTCUT_ORDER.iter().position(|x| *x == c).unwrap();
        assert!(pos(Command::SetWorkspace(Workspace::Dual)) < pos(Command::Fit));
    }

    #[test]
    fn m_toggles_the_mask_overlay() {
        assert_eq!(shortcut_of(Command::ToggleMaskOverlay), Some(sc(Modifiers::NONE, Key::M)));
        assert!(SHORTCUT_ORDER.contains(&Command::ToggleMaskOverlay));
    }

    #[test]
    fn xmp_import_updates_the_keywords_the_graph_sees_at_once() {
        use crate::session::tests::{png_project, provider};
        let dir = tempfile::tempdir().unwrap();
        let mut app = headless_app(dir.path());
        let photos = tempfile::tempdir().unwrap();
        app.session = Session::new(png_project(photos.path(), 3), provider());
        app.session.sync_paths();
        app.session.refresh_visible();
        let id = app.session.visible()[0];
        let item = (id, rawgraph_project::XmpCulling { keywords: vec!["Reise|Graz".into()], ..Default::default() }, crate::xmp_import::Origin::Sidecar);
        let found = crate::xmp_import::XmpFound { items: vec![item], ..Default::default() };
        let gen = app.session.images_gen;
        app.apply_xmp(&found, 0);
        assert!(app.session.images_gen > gen, "the collection queries are recomputed");
        assert_eq!(app.session.provider_keywords(id), vec!["Reise|Graz".to_owned()]);
    }

}

