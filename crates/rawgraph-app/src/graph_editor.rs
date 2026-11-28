//! Node graph editor painted directly on `rawgraph_core::Graph`.
//!
//! The graph is the single source of truth: node positions live in
//! `Node::pos`, selection and the viewed node in the [`Session`]. The editor
//! never keeps a second model of the graph.

use std::collections::HashMap;

use eframe::egui::{
    self, epaint::CubicBezierShape, Align2, Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2,
};
use rawgraph_core::{Graph, ImageId, Mode, Node, NodeId, NodeKind, PortType};

use crate::groups::{all_rows, box_row_count, limit_rows, parameter_row, MAX_BOX_ROWS};
use crate::selection::{band_hits, NodeSelection};
use crate::session::Session;
use crate::status::Status;

mod editing;
pub use editing::{EditOp, PER_IMAGE_NOTE};
use editing::{cached_list, Band, EditState};

// Geometry in graph units (one unit is one point at zoom 1).
pub const NODE_W: f32 = 196.0;
const TITLE_H: f32 = 26.0;
const ROW_H: f32 = 21.0;
const PARAM_H: f32 = 16.0;
const PAD: f32 = 8.0;
const PORT_R: f32 = 5.5;
const TAB_SIZE: f32 = 16.0;
const TAB_INSET: f32 = 6.0;
const TITLE_GAP: f32 = 4.0;
const TAB_INPUT: Color32 = Color32::from_rgb(58, 120, 190);
const TAB_OUTPUT: Color32 = Color32::from_rgb(180, 70, 74);

pub const COLOR_IMAGES: Color32 = Color32::from_rgb(92, 165, 240);
const COLOR_SELECTED: Color32 = Color32::from_rgb(255, 210, 90);
const COLOR_WARN: Color32 = Color32::from_rgb(255, 176, 40);

pub fn port_color(ty: PortType) -> Color32 {
    match ty {
        PortType::Images => COLOR_IMAGES,
    }
}

/// Title colour of a node box, by category (Source and Output stand out).
fn title_color(kind: NodeKind) -> Color32 {
    match kind {
        NodeKind::Source => return Color32::from_rgb(52, 104, 70),
        NodeKind::Output => return Color32::from_rgb(130, 56, 56),
        NodeKind::Export => return Color32::from_rgb(120, 62, 96),
        NodeKind::Layer => return Color32::from_rgb(36, 112, 126),
        _ => {}
    }
    category_color(kind.category())
}

pub fn category_color(category: &str) -> Color32 {
    match category {
        "Develop" => Color32::from_rgb(146, 88, 40),
        "Tone" => Color32::from_rgb(52, 82, 130),
        "Colour" => Color32::from_rgb(120, 64, 118),
        "Detail" => Color32::from_rgb(98, 66, 128),
        "Geometry" => Color32::from_rgb(56, 108, 92),
        "Effects" => Color32::from_rgb(100, 84, 56),
        "Collection" => Color32::from_rgb(40, 110, 104),
        "Generators" => Color32::from_rgb(128, 90, 34),
        _ => Color32::from_rgb(70, 76, 90),
    }
}

/// Categories in the order of the add-node menu and the Tab search.
pub const CATEGORY_ORDER: [&str; 9] = ["Input/Output", "Develop", "Tone", "Colour", "Detail", "Geometry", "Effects", "Collection", "Generators"];

/// Groups of the add-node menu and the Tab search: every node kind once, by
/// `NodeKind::category()`, categories in `CATEGORY_ORDER`, kinds in their
/// declaration order. Categories without kinds are left out.
pub fn node_groups() -> Vec<(&'static str, Vec<NodeKind>)> {
    let mut groups: Vec<(&'static str, Vec<NodeKind>)> = CATEGORY_ORDER.iter().map(|c| (*c, Vec::new())).collect();
    for kind in NodeKind::ALL {
        if kind == NodeKind::Output { continue; }
        let cat = kind.category();
        match groups.iter_mut().find(|(c, _)| *c == cat) {
            Some((_, kinds)) => kinds.push(kind),
            // A category this build does not know about still gets a menu.
            None => groups.push((cat, vec![kind])),
        }
    }
    groups.retain(|(_, kinds)| !kinds.is_empty());
    groups
}

/// Group name of a node kind.
pub fn group_of(kind: NodeKind) -> &'static str {
    kind.category()
}

const COUNT_H: f32 = 15.0;

/// Number of parameter summary rows in the node box (at most four, plus a
/// "... +n" row when more parameters exist).
fn summary_row_count(kind: NodeKind) -> usize {
    if kind == NodeKind::MetadataFilter || kind == NodeKind::Export {
        2
    } else if kind == NodeKind::Layer {
        1
    } else if kind == NodeKind::ColorWheels {
        crate::groups::WHEELS.len()
    } else {
        box_row_count(kind)
    }
}

fn compact(v: f32) -> String {
    if (v - v.round()).abs() < 1e-3 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

/// Rows shown in the node box for `image`: (label, value). Choices appear by
/// their label, toggles as on/off.
pub fn summary_rows(node: &rawgraph_core::Node, image: ImageId) -> Vec<(String, String)> {
    if node.kind == NodeKind::MetadataFilter {
        let measure = node.choice("measure", image);
        if measure == rawgraph_core::Measure::Keyword as usize {
            let text = match node.data(image) {
                Some(rawgraph_core::NodeData::Keyword(t)) if !t.is_empty() => format!("\"{t}\""),
                _ => "any keyword".to_owned(),
            };
            let op = ["contains", "does not contain"].get(node.choice("keyword_op", image)).copied().unwrap_or("contains");
            return vec![("Keyword".to_owned(), format!("{op} {text}")), (String::new(), if node.toggle("invert", image) { "inverted".to_owned() } else { String::new() })];
        }
        let name = ["ISO", "Shutter", "Aperture", "EV100", "Flash"].get(measure).copied().unwrap_or("Value");
        let (lo, hi) = (node.param("min", image), node.param("max", image));
        let lo_t = if lo <= 0.0 { "0".to_owned() } else { compact(lo) };
        let hi_t = if hi >= 1_000_000.0 { "max".to_owned() } else { compact(hi) };
        let mut note = Vec::new();
        if node.toggle("invert", image) {
            note.push("inverted");
        }
        note.push(if node.choice("missing", image) == 1 { "missing: include" } else { "missing: exclude" });
        return vec![(name.to_owned(), format!("{lo_t} ... {hi_t}")), (String::new(), note.join(", "))];
    }
    if node.kind == NodeKind::ColorWheels {
        return crate::groups::wheels_rows(node, image).into_iter().filter(|r| r.changed).map(|r| (r.label, r.value)).collect();
    }
    // Exposed parameters always occupy the first summary rows, in stable port
    // order, regardless of the current image or which slider values changed.
    let mut rows: Vec<_> = node.exposed_inputs().iter().flatten()
        .map(|input| match node.kind.param_spec(&input.parameter) {
            Some(spec) => parameter_row(node, image, spec),
            // Vector inputs such as Filter direction have no slider: show the port's own label.
            None => crate::groups::Row { key: None, label: Graph::parameter_input_label(node.kind, &input.parameter).unwrap_or(&input.parameter).to_owned(), value: "image".to_owned(), changed: true },
        })
        .collect();
    let max = MAX_BOX_ROWS.saturating_sub(rows.len());
    let other = all_rows(node, image).into_iter()
        .filter(|row| row.changed && !rows.iter().any(|exposed| exposed.key == row.key))
        .collect();
    rows.extend(limit_rows(other, max));
    rows.into_iter().map(|r| (r.label, r.value)).collect()
}

/// The rows of a node box: like [`summary_rows`], but a Source shows its folder, a Layer a compact
/// summary of its layers and an Export node its format and folder (these need more than the
/// node's own values).
pub fn box_rows(graph: &Graph, node: &rawgraph_core::Node, image: ImageId) -> Vec<(String, String)> {
    match node.kind {
        NodeKind::Source => vec![("Folder".to_owned(), crate::export::elide_middle(&crate::node_ui::source_label(node), 20))],
        NodeKind::Layer => vec![(String::new(), crate::node_ui::layer_summary(graph, node))],
        NodeKind::Export => crate::node_ui::export_box_rows(node, image).into_iter().map(|(l, v)| (l, crate::export::elide_middle(&v, 20))).collect(),
        _ => summary_rows(node, image),
    }
}

/// Size of a node in graph units.
pub fn node_size(kind: NodeKind) -> Vec2 {
    let rows = port_rows(kind) as f32;
    let params = summary_row_count(kind) as f32;
    let param_block = if params > 0.0 { params * PARAM_H + 4.0 } else { 0.0 };
    let count = COUNT_H;
    Vec2::new(NODE_W, TITLE_H + 6.0 + rows * ROW_H + param_block + count + PAD)
}

/// Port rows of the box: the larger of the input and output counts (at least one).
pub fn port_rows(kind: NodeKind) -> usize {
    kind.inputs().len().max(kind.outputs().len()).max(1)
}

fn node_port_rows(node: &rawgraph_core::Node) -> usize {
    node.kind.inputs().len().max(node.kind.outputs().len() + node.exposed_outputs().len()).max(1)
}

#[cfg(test)]
fn record_port_control(response: &egui::Response, id: NodeId, name: &'static str) {
    response.ctx.data_mut(|data| data.insert_temp(egui::Id::unique(("port_control", id, name)), response.rect));
}

fn input_menu(ui: &mut egui::Ui, node: &rawgraph_core::Node, id: NodeId, actions: &mut Vec<Action>) {
    for &parameter in Graph::supported_parameter_inputs(node.kind) {
        if node.exposed_inputs().iter().flatten().any(|p| p.parameter == parameter) { continue; }
        let label = Graph::parameter_input_label(node.kind, parameter).unwrap();
        let hint = if parameter == "direction" { "Use linear RGB as an encoded Motion Blur direction" }
            else { "Scale this slider per pixel by luminance: black = no effect, white = the slider value" };
        let response = ui.button(label).on_hover_text(hint);
        #[cfg(test)] record_port_control(&response, id, "input_item");
        if response.clicked() { actions.push(Action::ExposeInput(id, parameter)); ui.close(); }
    }
}

fn output_menu(ui: &mut egui::Ui, node: &rawgraph_core::Node, id: NodeId, actions: &mut Vec<Action>) {
    if let Some(source) = node.kind.outputs().iter().position(|p| p.ty == PortType::Images) {
        for label in ["Luma", "R", "G", "B", "A"] {
            if node.exposed_outputs().iter().flatten().any(|p| p.label == label) { continue; }
            let response = ui.button(label);
            #[cfg(test)] if label == "Luma" { record_port_control(&response, id, "output_item"); }
            if response.clicked() { actions.push(Action::ExposeOutput(id, source, label)); ui.close(); }
        }
    }
}

fn has_input_tab(kind: NodeKind) -> bool {
    !Graph::supported_parameter_inputs(kind).is_empty()
}

fn has_output_tab(kind: NodeKind) -> bool {
    kind.outputs().iter().any(|p| p.ty == PortType::Images)
}

fn exposed_port_menus(ui: &mut egui::Ui, node: &rawgraph_core::Node, id: NodeId, actions: &mut Vec<Action>) {
    if has_input_tab(node.kind) {
        ui.menu_button("Add Input", |ui| input_menu(ui, node, id, actions));
    }
    if has_output_tab(node.kind) {
        ui.menu_button("Add Output", |ui| output_menu(ui, node, id, actions));
    }
}

fn actual_node_size(node: &Node) -> Vec2 {
    let extra_port_rows = node_port_rows(node) - port_rows(node.kind);
    let extra_param_rows = node.exposed_inputs().iter().flatten().count().saturating_sub(MAX_BOX_ROWS);
    node_size(node.kind) + Vec2::new(0.0, extra_port_rows as f32 * ROW_H + extra_param_rows as f32 * PARAM_H)
}

fn parameter_row_y(node: &Node, row: usize) -> f32 {
    TITLE_H + 6.0 + node_port_rows(node) as f32 * ROW_H + 4.0 + (row as f32 + 0.5) * PARAM_H
}

fn builtin_input_port(pos: [f32; 2], port: usize) -> Pos2 {
    Pos2::new(pos[0], pos[1] + TITLE_H + 6.0 + port as f32 * ROW_H + ROW_H / 2.0)
}

/// Centre of an input in graph units, including parameter inputs on summary rows.
fn input_port(node: &Node, port: usize) -> Pos2 {
    if let Some(i) = port.checked_sub(node.kind.inputs().len()) {
        // Removed inputs leave holes in their port indices, not in the layout.
        let row = node.exposed_inputs()[..i].iter().flatten().count();
        Pos2::new(node.pos[0], node.pos[1] + parameter_row_y(node, row))
    } else {
        builtin_input_port(node.pos, port)
    }
}

/// Centre of output port `port`, in graph units.
pub fn output_port(pos: [f32; 2], port: usize) -> Pos2 {
    Pos2::new(pos[0] + NODE_W, pos[1] + TITLE_H + 6.0 + port as f32 * ROW_H + ROW_H / 2.0)
}

struct TitleBar {
    rect: Rect,
    input: Option<Rect>,
    output: Option<Rect>,
    mode: Option<Rect>,
    count: Option<Rect>,
    warning: Option<Rect>,
    text: Rect,
}

impl TitleBar {
    fn new(node: &Node, node_rect: Rect, zoom: f32, has_warning: bool) -> Self {
        let rect = Rect::from_min_size(node_rect.min, Vec2::new(node_rect.width(), TITLE_H * zoom));
        let cy = rect.center().y;
        let side = TAB_SIZE * zoom;
        let gap = TITLE_GAP * zoom;
        let left = rect.left() + TAB_INSET * zoom;
        let mut right = rect.right() - TAB_INSET * zoom;
        let input = has_input_tab(node.kind)
            .then(|| Rect::from_min_size(Pos2::new(left, cy - side / 2.0), Vec2::splat(side)));
        let mut reserve = |width: f32| {
            let badge = Rect::from_min_size(Pos2::new(right - width, cy - side / 2.0), Vec2::new(width, side));
            right = badge.left() - gap;
            badge
        };
        let output = has_output_tab(node.kind).then(|| reserve(side));
        let show_mode = zoom >= 0.5 && !node.kind.params().is_empty();
        let mode = show_mode.then(|| reserve(side));
        let count = (show_mode && node.mode == Mode::Individual).then(|| {
            let digits = node.individual_count().to_string().len() as f32;
            reserve((10.0 + 6.5 * digits) * zoom)
        });
        let warning = (zoom >= 0.5 && has_warning).then(|| reserve(side));
        let text = Rect::from_min_max(
            Pos2::new(input.map_or(left, |tab| tab.right() + gap), rect.top()),
            Pos2::new(right, rect.bottom()),
        );
        Self { rect, input, output, mode, count, warning, text }
    }
}

/// Graph <-> screen mapping: `screen = origin + pan + graph * zoom`.
#[derive(Clone, Copy, Debug)]
pub struct Xform {
    pub origin: Pos2,
    pub pan: Vec2,
    pub zoom: f32,
}

impl Xform {
    pub fn to_screen(self, p: Pos2) -> Pos2 {
        self.origin + self.pan + p.to_vec2() * self.zoom
    }

    pub fn to_graph(self, p: Pos2) -> Pos2 {
        ((p - self.origin - self.pan) / self.zoom).to_pos2()
    }

    /// Zooms to `new_zoom` keeping the graph point under `cursor` fixed.
    pub fn zoom_at(&mut self, cursor: Pos2, new_zoom: f32) {
        let g = self.to_graph(cursor);
        self.zoom = new_zoom;
        self.pan = cursor - self.origin - g.to_vec2() * new_zoom;
    }
}

#[derive(Clone, Copy)]
enum ConnDrag {
    /// Dragging from an output port towards an input.
    FromOutput(NodeId, usize, Option<(NodeId, usize)>),
    /// Dragging from an unconnected input towards an output.
    FromInput { node: NodeId, port: usize },
}

struct Search {
    graph_pos: [f32; 2],
    screen_pos: Pos2,
    query: String,
    selected: usize,

    connection: Option<ConnDrag>,
}

pub struct GraphEditor {
    pan: Vec2,
    zoom: f32,
    frame_all_pending: bool,
    /// Frames to wait before fitting, so panel sizes have settled.
    frame_all_delay: u8,
    /// The view was framed automatically and the user has not moved it since: it
    /// is framed again when the editor's size changes (window or panel resized).
    auto_framed: bool,
    framed_size: Vec2,
    conn: Option<ConnDrag>,
    search: Option<Search>,
    menu_graph_pos: [f32; 2],
    /// True while the pointer is over the editor (the app uses it for Tab).
    pub hovered: bool,
    /// Set by the app when Tab was pressed over the editor.
    pub open_search: bool,
    /// Copy and paste go through the system clipboard (off in screenshot runs and tests).
    pub use_system_clipboard: bool,
    last_copy: Option<String>,
    recipes_dir: Option<std::path::PathBuf>,
    manage_open: bool,
    edit: EditState,
    /// Debug > Per-node CPU timing: the last profiled preview, drawn under each node.
    pub timings: Option<NodeTimings>,
}

/// Per-node CPU timing of the last profiled preview, for the on-node debug labels.
#[derive(Clone, Debug, Default)]
pub struct NodeTimings {
    pub rows: HashMap<NodeId, rawgraph_engine::NodeTiming>,
    pub total: std::time::Duration,
    /// The pointwise tail ran batched on the GPU, so those nodes have no CPU time.
    pub gpu_tail: bool,
}

impl NodeTimings {
    pub fn new(rows: &[rawgraph_engine::NodeTiming], gpu_tail: bool) -> Self {
        Self { total: rows.iter().map(|r| r.cpu).sum(), rows: rows.iter().map(|r| (r.node, r.clone())).collect(), gpu_tail }
    }

    /// Label, colour and hover text for one node, or None if it has no timing.
    fn label(&self, id: NodeId) -> Option<(String, Color32, String)> {
        let r = self.rows.get(&id)?;
        let ms = r.cpu.as_secs_f64() * 1000.0;
        let share = if self.total.is_zero() { 0.0 } else { r.cpu.as_secs_f64() / self.total.as_secs_f64() };
        let text = if r.computed == 0 && r.cache_hits > 0 { "cached".to_owned() } else if ms < 10.0 { format!("{ms:.1} ms") } else { format!("{ms:.0} ms") };
        let color = if r.computed == 0 { Color32::from_gray(120) }
            else if share >= 0.5 { Color32::from_rgb(235, 110, 95) }
            else if share >= 0.2 { Color32::from_rgb(230, 180, 90) }
            else { Color32::from_gray(185) };
        let mut tip = format!("{ms:.3} ms exclusive CPU time ({:.0} % of this preview), {} computed, {} cache hits", share * 100.0, r.computed, r.cache_hits);
        if self.gpu_tail { tip.push_str("\nThe pointwise tail ran batched on the GPU; those nodes have no CPU time here."); }
        Some((text, color, tip))
    }
}

/// How a node takes part in the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sel {
    No,
    Member,
    /// The node selected last; the inspector shows it.
    Primary,
}

impl Default for GraphEditor {
    fn default() -> Self {
        Self {
            pan: Vec2::ZERO,
            zoom: 1.0,
            frame_all_pending: true,
            frame_all_delay: 2,
            auto_framed: false,
            framed_size: Vec2::ZERO,
            conn: None,
            search: None,
            menu_graph_pos: [0.0, 0.0],
            hovered: false,
            open_search: false,
            use_system_clipboard: true,
            last_copy: None,
            recipes_dir: None,
            manage_open: false,
            edit: EditState::default(),
            timings: None,
        }
    }
}

/// Graph-space bounding rectangle of all nodes.
pub fn bounds(graph: &Graph) -> Option<Rect> {
    let mut rect: Option<Rect> = None;
    for n in graph.nodes() {
        let r = Rect::from_min_size(Pos2::new(n.pos[0], n.pos[1]), actual_node_size(n));
        rect = Some(rect.map_or(r, |acc| acc.union(r)));
    }
    rect
}

/// Smallest zoom the automatic framing uses: the smallest node text (10.5
/// graph units) then still measures about 9 points on screen.
pub const MIN_READABLE_ZOOM: f32 = 0.86;
const MAX_FIT_ZOOM: f32 = 1.4;
const FRAME_MARGIN: f32 = 36.0;

/// Pan and zoom that show `content` in `viewport` (pan is relative to the
/// viewport's top-left corner). The zoom fits everything when that stays
/// readable; otherwise it stops at [`MIN_READABLE_ZOOM`] and the pan keeps
/// `focus` (the selected node, or the left end of the graph) in view, so the
/// user scrolls instead of squinting.
pub fn fit_view(content: Rect, viewport: Rect, focus: Option<Rect>) -> (Vec2, f32) {
    let avail = (viewport.size() - Vec2::splat(2.0 * FRAME_MARGIN)).max(Vec2::splat(40.0));
    let fit = (avail.x / content.width().max(1.0)).min(avail.y / content.height().max(1.0));
    let zoom = fit.clamp(MIN_READABLE_ZOOM, MAX_FIT_ZOOM);
    let vp = viewport.size();
    let axis = |vp: f32, c_min: f32, c_max: f32, f: Option<(f32, f32)>| -> f32 {
        let size = (c_max - c_min) * zoom;
        if size + 2.0 * FRAME_MARGIN <= vp {
            return vp * 0.5 - (c_min + c_max) * 0.5 * zoom;
        }
        // Content wider than the viewport: leftmost pan shows the content's
        // start, rightmost its end; the focus decides in between.
        let start = FRAME_MARGIN - c_min * zoom;
        let end = vp - FRAME_MARGIN - c_max * zoom;
        match f {
            Some((f_min, f_max)) => (vp * 0.5 - (f_min + f_max) * 0.5 * zoom).clamp(end, start),
            None => start,
        }
    };
    let focus = focus.unwrap_or(Rect::from_min_size(content.min, Vec2::new(NODE_W, 1.0)));
    let pan = Vec2::new(
        axis(vp.x, content.min.x, content.max.x, Some((focus.min.x, focus.max.x))),
        axis(vp.y, content.min.y, content.max.y, Some((focus.min.y, focus.max.y))),
    );
    (pan, zoom)
}

// egui-winit's viewport inner rect is in screen points, using the same
// pixels_per_point (native DPI * egui zoom) as the canvas.
#[cfg(any(windows, test))]
fn screen_to_canvas(screen_px: [i32; 2], pixels_per_point: f32, inner: Rect, canvas: Rect) -> Option<Pos2> {
    if !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
        return None;
    }
    let screen = Pos2::new(screen_px[0] as f32, screen_px[1] as f32) / pixels_per_point;
    if !inner.contains(screen) {
        return None;
    }
    let local = (screen - inner.min).to_pos2();
    canvas.contains(local).then_some(local)
}

#[cfg(windows)]
fn native_drop_pos(ctx: &egui::Context, canvas: Rect) -> Option<Pos2> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    let mut point = POINT { x: 0, y: 0 };
    // SAFETY: point is writable, and GetCursorPos only fills it synchronously.
    if unsafe { GetCursorPos(&mut point) } == 0 {
        return None;
    }
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    screen_to_canvas([point.x, point.y], ctx.pixels_per_point(), inner, canvas)
}

#[cfg(not(windows))]
fn native_drop_pos(ctx: &egui::Context, canvas: Rect) -> Option<Pos2> {
    ctx.input(|i| i.pointer.hover_pos()).filter(|p| canvas.contains(*p))
}

fn add_dropped_folders(session: &mut Session, pos: Pos2, paths: &[std::path::PathBuf]) {
    for (i, path) in paths.iter().enumerate() {
        let id = session.project.graph.add_node(NodeKind::Source, [pos.x, pos.y + i as f32 * 160.0]);
        crate::node_ui::set_source_folder(&mut session.project.graph, id, Some(path.to_string_lossy().into_owned()));
        session.set_node_selection(NodeSelection::single(id));
    }
    session.dirty = true;
}

impl GraphEditor {
    pub fn request_frame_all(&mut self) {
        self.frame_all_pending = true;
        self.frame_all_delay = 2;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, session: &mut Session, status: &mut Status) {
        let rect = ui.available_rect_before_wrap();
        let bg = ui.allocate_rect(rect, Sense::click_and_drag());
        bg.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Group, true, "Node graph editor"));
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(26, 28, 32));

        if self.auto_framed && !self.frame_all_pending && (rect.size() - self.framed_size).abs().max_elem() > 2.0 {
            self.request_frame_all();
        }
        if self.frame_all_pending && self.frame_all_delay > 0 {
            self.frame_all_delay -= 1;
            ui.ctx().request_repaint();
        } else if self.frame_all_pending && rect.width() > 120.0 && rect.height() > 80.0 {
            if let Some(b) = bounds(&session.project.graph) {
                let focus = session
                    .selected_node
                    .and_then(|id| session.project.graph.node(id))
                    .map(|n| Rect::from_min_size(Pos2::new(n.pos[0], n.pos[1]), actual_node_size(n)));
                let (pan, zoom) = fit_view(b, Rect::from_min_size(Pos2::ZERO, rect.size()), focus);
                self.pan = pan;
                self.zoom = zoom;
                self.auto_framed = true;
                self.framed_size = rect.size();
            }
            self.frame_all_pending = false;
        }

        let ctx = ui.ctx().clone();
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        self.hovered = pointer.is_some_and(|p| rect.contains(p));
        // Windows OLE drops do not carry a cursor position through winit. Query
        // the cursor only for an actual drop; never import outside the canvas.
        let has_drop = ctx.input(|i| !i.raw.dropped_files.is_empty());
        if let Some(pos) = has_drop.then(|| native_drop_pos(&ctx, rect)).flatten() {
            let paths = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path()).filter(|p| p.is_dir()).map(|p| p.to_owned()).collect::<Vec<_>>());
            if !paths.is_empty() {
                if session.demo {
                    status.error("Create or open a project before dropping folders into the graph");
                } else {
                    let xf = Xform { origin: rect.min, pan: self.pan, zoom: self.zoom };
                    let graph_pos = xf.to_graph(pos);
                    add_dropped_folders(session, graph_pos, &paths);
                    status.info(format!("Added {} Source node(s)", paths.len()));
                }
            }
        }
        let typing = ctx.egui_wants_keyboard_input();
        let space = ctx.input(|i| i.key_down(Key::Space)) && !typing;

        // Navigation: middle-drag or Space+drag pans, scroll pans, Ctrl+scroll zooms.
        if self.hovered {
            let (scroll, zoom_delta, middle, primary, delta) = ctx.input(|i| {
                (
                    i.smooth_scroll_delta,
                    i.zoom_delta(),
                    i.pointer.middle_down(),
                    i.pointer.primary_down(),
                    i.pointer.delta(),
                )
            });
            if middle || (space && primary) {
                self.pan += delta;
                self.auto_framed = false;
            }
            if scroll != Vec2::ZERO {
                self.pan += scroll;
                self.auto_framed = false;
            }
            if (zoom_delta - 1.0).abs() > 1e-4 {
                self.auto_framed = false;
                if let Some(c) = pointer {
                    let mut x = Xform { origin: rect.min, pan: self.pan, zoom: self.zoom };
                    x.zoom_at(c, (self.zoom * zoom_delta).clamp(0.2, 3.0));
                    self.pan = x.pan;
                    self.zoom = x.zoom;
                }
            }
            if self.open_search {
                self.open_search = false;
                if let Some(c) = pointer {
                    let x = Xform { origin: rect.min, pan: self.pan, zoom: self.zoom };
                    let g = x.to_graph(c);
                    self.search = Some(Search { graph_pos: [g.x, g.y], screen_pos: c, query: String::new(), selected: 0, connection: None });
                }
            }
        }
        self.open_search = false;
        let xf = Xform { origin: rect.min, pan: self.pan, zoom: self.zoom };
        session.reconcile_selection();
        // Buttons and the canvas may take egui keyboard focus too; that is not
        // text editing. Only accept clipboard events for this graph or no focus.
        let focused = ctx.memory(|m| m.focused());
        let graph_focus = focused.is_none() || focused == Some(bg.id) || session.project.graph.nodes().any(|n| {
            focused == Some(ui.scope_id().with(("node", n.id.0)))
                || (0..n.kind.inputs().len() + n.exposed_inputs().len()).any(|i| focused == Some(ui.scope_id().with(("in", n.id.0, i))))
                || (0..n.kind.outputs().len() + n.exposed_outputs().len()).any(|i| focused == Some(ui.scope_id().with(("out", n.id.0, i))))
        });
        if graph_focus && self.search.is_none() && !session.node_sel.is_empty()
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Delete) || i.consume_key(Modifiers::NONE, Key::Backspace))
        {
            if let Err(reason) = session.delete_selected() { status.error(reason); }
        }
        self.run_edit_input(&ctx, session, status, rect, &xf, pointer, graph_focus);
        let mods = ctx.input(|i| i.modifiers);
        let additive = mods.shift || mods.command;

        self.paint_grid(&painter, rect, &xf);

        // Rubber band: dragging on empty space selects the nodes it touches.
        if bg.drag_started_by(egui::PointerButton::Primary) && !space && self.conn.is_none() && self.search.is_none() {
            if let Some(origin) = ctx.input(|i| i.pointer.press_origin()) {
                self.edit.band = Some(Band { start: origin, base: if additive { session.node_sel.ids().to_vec() } else { Vec::new() } });
            }
        }
        let mut band_rect = None;
        if let Some(band) = &self.edit.band {
            let now = pointer.or_else(|| ctx.input(|i| i.pointer.latest_pos()));
            if let Some(now) = now {
                let r = Rect::from_two_pos(band.start, now);
                let boxes = session.project.graph.nodes().map(|n| (n.id, Rect::from_min_size(xf.to_screen(Pos2::new(n.pos[0], n.pos[1])), actual_node_size(n) * self.zoom)));
                let hits = band_hits(boxes, r);
                let mut sel = NodeSelection::default();
                sel.replace(band.base.iter().copied().chain(hits));
                if !sel.ids().is_empty() && sel != session.node_sel {
                    session.set_node_selection(sel);
                }
                band_rect = Some(r);
            }
            if ctx.input(|i| i.pointer.primary_down()) {
                ctx.request_repaint();
            } else {
                self.edit.band = None;
            }
        }

        let image = session.current_image();
        let issues: HashMap<NodeId, String> = {
            let mut m: HashMap<NodeId, String> = HashMap::new();
            for issue in session.project.graph.validate() {
                m.entry(issue.node).and_modify(|t| {
                    t.push('\n');
                    t.push_str(&issue.message);
                }).or_insert(issue.message);
            }
            m
        };

        // Draw order: unselected nodes first, the selected one on top.
        let mut order: Vec<NodeId> = session.project.graph.nodes().map(|n| n.id).collect();
        order.sort_by_key(|id| (session.node_sel.contains(*id), Some(*id) == session.selected_node));

        // Wires sit behind the nodes.
        for e in session.project.graph.edges() {
            let (Some(from), Some(to)) = (session.project.graph.node(e.from), session.project.graph.node(e.to)) else { continue };
            let color = from.output_type(e.from_port).map_or(Color32::GRAY, port_color);
            draw_wire(&painter, xf.to_screen(output_port(from.pos, e.from_port)), xf.to_screen(input_port(to, e.to_port)), color, self.zoom);
        }

        // Port screen positions for the drop test, filled while drawing.
        let mut inputs: Vec<(NodeId, usize, Pos2, PortType)> = Vec::new();
        let mut outputs: Vec<(NodeId, usize, Pos2, PortType)> = Vec::new();
        let mut actions = Vec::new();

        for id in order {
            let Some(node) = session.project.graph.node(id).cloned() else { continue };
            let kind = node.kind;
            let pos = node.pos;
            let size = actual_node_size(&node) * self.zoom;
            let node_rect = Rect::from_min_size(xf.to_screen(Pos2::new(pos[0], pos[1])), size);
            if !node_rect.expand(40.0).intersects(rect) {
                // Still register ports for completeness of drops? Off-screen: skip.
                continue;
            }
            let selected = if session.selected_node == Some(id) {
                Sel::Primary
            } else if session.node_sel.contains(id) {
                Sel::Member
            } else {
                Sel::No
            };
            let viewed = session.target_node() == Some(id);
            let sense = if space { Sense::hover() } else { Sense::click_and_drag() };
            let resp = ui.interact(node_rect.intersect(rect), ui.scope_id().with(("node", id.0)), sense);
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Node {}", kind.label())));

            let issue = issues.get(&id).map(String::as_str);
            let title = TitleBar::new(&node, node_rect, self.zoom, issue.is_some());
            self.paint_node(&painter, session, id, node_rect, &title, image);
            self.badges(ui, &painter, &node, &title, issue);

            // Register tabs after the node so even a drag that starts on a tab
            // belongs to the tab, not to the node underneath it.
            for (input, tab) in [(true, title.input), (false, title.output)] {
                let Some(tab) = tab else { continue };
                let label = if input { "Add input port" } else { "Add output port" };
                let response = ui.interact(tab.intersect(rect), ui.scope_id().with(("port_tab", id.0, input)),
                    if space { Sense::hover() } else { Sense::click_and_drag() });
                response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, !space, label));
                let response = response.on_hover_text(label);
                let fill = if input { TAB_INPUT } else { TAB_OUTPUT };
                let fill = if response.hovered() { fill.gamma_multiply(1.15) } else { fill };
                painter.rect_filled(tab, (4.0 * self.zoom).round() as u8, fill);
                painter.text(tab.center(), Align2::CENTER_CENTER, "+", FontId::proportional(13.0 * self.zoom),
                    Color32::from_white_alpha(217));
                #[cfg(test)] record_port_control(&response, id, if input { "input" } else { "output" });
                if !space {
                    egui::Popup::menu(&response).show(|ui| {
                        if input { input_menu(ui, &node, id, &mut actions); } else { output_menu(ui, &node, id, &mut actions); }
                    });
                }
            }
            // Keep the selection outline above the tabs, even at minimum zoom.
            self.paint_outline(&painter, node_rect, selected, viewed);
            self.paint_timing(ui, &painter, id, node_rect);

            // Ports.
            if self.zoom > 0.0 {
                for i in 0..kind.inputs().len() + node.exposed_inputs().len() {
                    let Some(ty) = node.input_type(i) else { continue };
                    let builtin = kind.inputs().get(i);
                    let label = builtin.map(|s| s.label).unwrap_or_else(|| Graph::parameter_input_label(kind, &node.exposed_inputs()[i - kind.inputs().len()].as_ref().unwrap().parameter).unwrap_or("Input"));
                    let c = xf.to_screen(input_port(&node, i));
                    let connected = session.project.graph.input_of(id, i).is_some();
                    inputs.push((id, i, c, ty));
                    let hover_rect = Rect::from_center_size(c, Vec2::splat((PORT_R * 2.6 * self.zoom).max(14.0)));
                    let pr = ui.interact(hover_rect, ui.scope_id().with(("in", id.0, i)), if space { Sense::hover() } else { Sense::click_and_drag() });
                    #[cfg(test)] if builtin.is_none() { record_port_control(&pr, id, "parameter_port"); }
                    pr.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{} input {}", kind.label(), label))
                    });
                    let pr = pr.on_hover_text(format!(
                        "{} input ({:?}{})",
                        label,
                        ty,
                        if builtin.is_some_and(|s| s.required) { ", required" } else { ", optional" }
                    ));
                    if builtin.is_none() { pr.context_menu(|ui| { if ui.button("Remove port").clicked() { actions.push(Action::RemoveInput(id, i)); ui.close(); } }); }
                    if pr.drag_started() {
                        if let Some((src, src_port)) = session.project.graph.input_edge(id, i) {
                            self.conn = Some(ConnDrag::FromOutput(src, src_port, Some((id, i))));
                        } else {
                            self.conn = Some(ConnDrag::FromInput { node: id, port: i });
                        }
                    }
                    draw_port(&painter, c, ty, connected, pr.hovered() || pr.dragged(), self.zoom);
                }
                for port in 0..kind.outputs().len() + node.exposed_outputs().len() {
                    let Some(ty) = node.output_type(port) else { continue };
                    let c = xf.to_screen(output_port(pos, port));
                    outputs.push((id, port, c, ty));
                    let hover_rect = Rect::from_center_size(c, Vec2::splat((PORT_R * 2.6 * self.zoom).max(14.0)));
                    let pr = ui.interact(hover_rect, ui.scope_id().with(("out", id.0, port)), if space { Sense::hover() } else { Sense::click_and_drag() });
                    let label = session.project.graph.output_label(id, port, image);
                    pr.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("{} output {label}", kind.label())));
                    let tip = match session.collection.splits.get(&id).filter(|_| kind == NodeKind::MetadataSplit) {
                        Some(s) => format!("{label}: {} image(s) leave here. Drag to an input to connect.", s.per_port.get(port).copied().unwrap_or(0)),
                        None => format!("Output ({ty:?}): drag to an input to connect"),
                    };
                    let pr = pr.on_hover_text(tip);
                    if port >= kind.outputs().len() { pr.context_menu(|ui| { if ui.button("Remove port").clicked() { actions.push(Action::RemoveOutput(id, port)); ui.close(); } }); }
                    if pr.drag_started() {
                        self.conn = Some(ConnDrag::FromOutput(id, port, None));
                    }
                    let connected = session.project.graph.edges().iter().any(|e| e.from == id && e.from_port == port);
                    draw_port(&painter, c, ty, connected, pr.hovered() || pr.dragged(), self.zoom);
                }
            }

            // Node interaction (after ports so ports take priority where they overlap).
            if !space && self.conn.is_none() {
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    // Dragging a selected node moves the whole selection; an unselected one is selected first.
                    let mut sel = session.node_sel.clone();
                    if sel.contains(id) || additive {
                        sel.add(id);
                    } else {
                        sel.set_single(id);
                    }
                    session.set_node_selection(sel);
                } else if resp.clicked() {
                    let mut sel = session.node_sel.clone();
                    if additive {
                        sel.toggle(id);
                    } else {
                        sel.set_single(id);
                    }
                    session.set_node_selection(sel);
                } else if resp.secondary_clicked() && !session.node_sel.contains(id) {
                    session.set_node_selection(NodeSelection::single(id));
                }
                if resp.dragged_by(egui::PointerButton::Primary) {
                    let d = resp.drag_delta() / self.zoom;
                    actions.push(Action::Move(id, d));
                }
            }
            resp.context_menu(|ui| {
                exposed_port_menus(ui, &node, id, &mut actions);
                ui.menu_button("Remove exposed port", |ui| {
                    for (i, port) in node.exposed_inputs().iter().enumerate() {
                        if let Some(port) = port { if ui.button(format!("Input {}", port.parameter)).clicked() { actions.push(Action::RemoveInput(id, kind.inputs().len() + i)); ui.close(); } }
                    }
                    for (i, port) in node.exposed_outputs().iter().enumerate() {
                        if let Some(port) = port { if ui.button(format!("Output {}", port.label)).clicked() { actions.push(Action::RemoveOutput(id, kind.outputs().len() + i)); ui.close(); } }
                    }
                });
                ui.separator();
                if ui.button("Copy  (Ctrl+C)").on_hover_text(PER_IMAGE_NOTE).clicked() {
                    actions.push(Action::Edit(EditOp::Copy));
                    ui.close();
                }
                if ui.button("Cut  (Ctrl+X)").on_hover_text(PER_IMAGE_NOTE).clicked() {
                    actions.push(Action::Edit(EditOp::Cut));
                    ui.close();
                }
                if ui.button("Duplicate  (Ctrl+D)").on_hover_text(PER_IMAGE_NOTE).clicked() {
                    actions.push(Action::Edit(EditOp::Duplicate));
                    ui.close();
                }
                if ui.button("Save selection as recipe...").on_hover_text(PER_IMAGE_NOTE).clicked() {
                    actions.push(Action::Edit(EditOp::SaveRecipe));
                    ui.close();
                }
                ui.separator();
                if ui.button("View this node  (V)").clicked() {
                    actions.push(Action::View(id));
                    ui.close();
                }
                if ui.button("Delete node  (Del)").clicked() {
                    actions.push(Action::Delete(id));
                    ui.close();
                }
            });
        }

        // Context menu on the background: add nodes.
        if bg.hovered() && ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Secondary)) {
            if let Some(p) = pointer {
                let g = xf.to_graph(p);
                self.menu_graph_pos = [g.x, g.y];
            }
        }
        let menu_pos = self.menu_graph_pos;
        let recipes_dir = self.recipes_dir.clone();
        let mut list_slot = self.edit.list.take();
        bg.context_menu(|ui| {
            if ui.button("Paste here  (Ctrl+V)").on_hover_text(PER_IMAGE_NOTE).clicked() {
                actions.push(Action::Edit(EditOp::PasteAt(menu_pos)));
                ui.close();
            }
            if ui.button("Select all  (Ctrl+A)").clicked() {
                actions.push(Action::Edit(EditOp::SelectAll));
                ui.close();
            }
            ui.separator();
            ui.label(egui::RichText::new("Add node").weak());
            for (group, kinds) in node_groups() {
                ui.menu_button(group, |ui| {
                    for kind in kinds {
                        if ui.button(kind.label()).clicked() {
                            actions.push(Action::Add(kind, menu_pos));
                            ui.close();
                        }
                    }
                });
            }
            ui.separator();
            ui.menu_button("Recipes", |ui| {
                let has_dir = recipes_dir.is_some();
                let list = cached_list(&mut list_slot, recipes_dir.as_deref());
                if !has_dir {
                    ui.label(egui::RichText::new("Recipes are not available").weak());
                } else if list.is_empty() {
                    ui.label(egui::RichText::new("No recipes yet").weak());
                }
                for r in &list {
                    let label = if r.usable { r.stem.clone() } else { format!("{} (unusable)", r.stem) };
                    let hover = if r.usable { format!("Inserts {} node(s), not connected", r.nodes) } else { r.note.clone() };
                    if ui.add_enabled(r.usable, egui::Button::new(label)).on_hover_text(hover).clicked() {
                        actions.push(Action::Edit(EditOp::InsertRecipe(r.stem.clone(), Some(menu_pos))));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.add_enabled(has_dir, egui::Button::new("Manage recipes...")).clicked() {
                    actions.push(Action::Edit(EditOp::ManageRecipes));
                    ui.close();
                }
                if ui.add_enabled(has_dir, egui::Button::new("Import recipe file...")).clicked() {
                    actions.push(Action::Edit(EditOp::ImportRecipeFile));
                    ui.close();
                }
            });
        });
        self.edit.list = list_slot;
        if bg.double_clicked() && !space && self.conn.is_none() && self.search.is_none() {
            if let Some(p) = pointer {
                if let Some(edge) = nearest_wire(&session.project.graph, &xf, p) {
                    session.project.graph.disconnect_edge(edge);
                }
            }
        }
        if let Some(r) = band_rect {
            painter.rect_filled(r, 2.0, Color32::from_rgba_unmultiplied(90, 140, 220, 36));
            painter.rect_stroke(r, 2.0, Stroke::new(1.0, Color32::from_rgb(120, 170, 240)), StrokeKind::Inside);
        }

        // In-progress wire.
        if let Some(conn) = &self.conn {
            if let Some(p) = pointer.or_else(|| ctx.input(|i| i.pointer.latest_pos())) {
                match conn {
                    ConnDrag::FromOutput(src, src_port, _) => {
                        if let Some(n) = session.project.graph.node(*src) {
                            let ty = n.output_type(*src_port).unwrap_or(PortType::Images);
                            draw_wire(&painter, xf.to_screen(output_port(n.pos, *src_port)), p, port_color(ty), self.zoom);
                            let target = nearest_input(&inputs, p, self.zoom, Some(ty));
                            if let Some((_, _, c, _)) = target {
                                painter.circle_stroke(c, 9.0 * self.zoom.max(0.8), Stroke::new(2.0, Color32::from_rgb(120, 230, 140)));
                            }
                        }
                    }
                    ConnDrag::FromInput { node, port } => {
                        if let Some(n) = session.project.graph.node(*node) {
                            let ty = n.input_type(*port).unwrap_or(PortType::Images);
                            draw_wire(&painter, p, xf.to_screen(input_port(n, *port)), port_color(ty), self.zoom);
                            let target = nearest_output(&outputs, p, self.zoom, Some(ty));
                            if let Some((_, _, c, _)) = target {
                                painter.circle_stroke(c, 9.0 * self.zoom.max(0.8), Stroke::new(2.0, Color32::from_rgb(120, 230, 140)));
                            }
                        }
                    }
                }
            }
            // Release: connect or drop.
            if !ctx.input(|i| i.pointer.any_down()) {
                let release = pointer.or_else(|| ctx.input(|i| i.pointer.latest_pos()));
                let conn = self.conn.take();
                if let (Some(conn), Some(p)) = (conn, release) {
                    let over_port = nearest_input(&inputs, p, self.zoom, None).is_some() || nearest_output(&outputs, p, self.zoom, None).is_some();
                    let over_node = session.project.graph.nodes().any(|n| Rect::from_min_size(xf.to_screen(Pos2::new(n.pos[0], n.pos[1])), actual_node_size(n) * self.zoom).contains(p));
                    if rect.contains(p) && !over_port && !over_node {
                        let g = xf.to_graph(p);
                        self.search = Some(Search { graph_pos: [g.x, g.y], screen_pos: p, query: String::new(), selected: 0, connection: Some(conn) });
                    } else { match conn {
                        ConnDrag::FromOutput(src, from_port, old) => {
                            if let Some((to, port, _, _)) = nearest_input(&inputs, p, self.zoom, None) {
                                actions.push(Action::Connect { from: src, from_port, to, port, old });
                            }
                        }
                        ConnDrag::FromInput { node, port } => {
                            if let Some((from, from_port, _, _)) = nearest_output(&outputs, p, self.zoom, None) {
                                actions.push(Action::Connect { from, from_port, to: node, port, old: None });
                            }
                        }
                    }}
                }
            }
            ctx.request_repaint();
        }

        self.apply(actions, session, status);
        self.search_popup(&ctx, session, status);
        self.recipe_windows(&ctx, session, status, rect, &xf);

        // Empty-graph hint.
        if session.project.graph.nodes().next().is_none() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Right-click or press Tab to add a node",
                FontId::proportional(15.0),
                Color32::from_gray(130),
            );
        }
    }

    fn paint_grid(&self, painter: &egui::Painter, rect: Rect, xf: &Xform) {
        let step = 32.0 * self.zoom;
        if step < 8.0 {
            return;
        }
        let color = Color32::from_rgb(38, 41, 47);
        let origin = xf.to_screen(Pos2::ZERO);
        let mut x = rect.min.x + (origin.x - rect.min.x).rem_euclid(step);
        while x < rect.max.x {
            painter.line_segment([Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)], Stroke::new(1.0, color));
            x += step;
        }
        let mut y = rect.min.y + (origin.y - rect.min.y).rem_euclid(step);
        while y < rect.max.y {
            painter.line_segment([Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)], Stroke::new(1.0, color));
            y += step;
        }
    }

    fn paint_outline(&self, painter: &egui::Painter, node_rect: Rect, selected: Sel, viewed: bool) {
        let outline = if selected == Sel::Primary {
            Stroke::new(2.5, COLOR_SELECTED)
        } else if selected == Sel::Member {
            Stroke::new(2.0, Color32::from_rgb(214, 168, 64))
        } else if viewed {
            Stroke::new(1.5, Color32::from_rgb(150, 205, 255))
        } else {
            Stroke::new(1.0, Color32::from_rgb(84, 89, 99))
        };
        painter.rect_stroke(node_rect, self.node_radius(), outline, StrokeKind::Inside);
    }

    fn node_radius(&self) -> u8 {
        (7.0 * self.zoom).round().clamp(2.0, 20.0) as u8
    }

    fn paint_node(
        &self,
        painter: &egui::Painter,
        session: &Session,
        id: NodeId,
        node_rect: Rect,
        title: &TitleBar,
        image: ImageId,
    ) {
        let Some(node) = session.project.graph.node(id) else { return };
        let z = self.zoom;
        let r = self.node_radius();
        painter.rect_filled(node_rect.translate(Vec2::new(2.0, 3.0) * z), r, Color32::from_black_alpha(70));
        painter.rect_filled(node_rect, r, Color32::from_rgb(44, 47, 54));
        painter.rect_filled(title.rect, CornerRadius { nw: r, ne: r, sw: 0, se: 0 }, title_color(node.kind));

        if z < 0.35 {
            return;
        }
        let color = Color32::from_gray(240);
        let mut job = egui::text::LayoutJob::simple_singleline(node.kind.label().to_owned(), FontId::proportional(13.0 * z), color);
        job.wrap.max_width = title.text.width();
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = painter.layout_job(job);
        let text_rect = Align2::CENTER_CENTER.anchor_size(title.text.center(), galley.size());
        painter.with_clip_rect(title.text).galley(text_rect.min, galley, color);
        if z < 0.5 {
            return;
        }
        let label_font = FontId::proportional(11.5 * z);
        let label_color = Color32::from_gray(176);
        for (i, spec) in node.kind.inputs().iter().enumerate() {
            let p = node_rect.min + (builtin_input_port([0.0, 0.0], i).to_vec2()) * z;
            painter.text(p + Vec2::new(11.0 * z, 0.0), Align2::LEFT_CENTER, spec.label, label_font.clone(), label_color);
        }
        for (i, output) in node.exposed_outputs().iter().enumerate() {
            if let Some(output) = output {
                let p = node_rect.min + output_port([0.0, 0.0], node.kind.outputs().len() + i).to_vec2() * z;
                painter.text(p - Vec2::new(11.0 * z, 0.0), Align2::RIGHT_CENTER, &output.label, label_font.clone(), label_color);
            }
        }
        let is_split = node.kind == NodeKind::MetadataSplit;
        let defined = match node.data(image) {
            Some(rawgraph_core::NodeData::Ranges(r)) => r.len(),
            _ => 0,
        };
        for (port, spec) in node.kind.outputs().iter().enumerate() {
            let p = node_rect.min + output_port([0.0, 0.0], port).to_vec2() * z;
            if !is_split {
                painter.text(p - Vec2::new(11.0 * z, 0.0), Align2::RIGHT_CENTER, spec.label, label_font.clone(), label_color);
                continue;
            }
            // Split by Metadata: range name and how many images leave through the port.
            let unused = port < rawgraph_core::graph::SPLIT_RANGES && port >= defined;
            let name = session.project.graph.output_label(id, port, image);
            let name = if unused { format!("{name} (unused)") } else { name };
            let (name_color, count_color) = if unused { (Color32::from_gray(98), Color32::from_gray(80)) } else { (Color32::from_gray(214), Color32::from_rgb(130, 190, 175)) };
            let count = session.collection.splits.get(&id).and_then(|s| s.per_port.get(port)).map_or_else(|| "...".to_owned(), |n| crate::collection::with_thousands(*n));
            painter.text(p - Vec2::new(11.0 * z, 0.0), Align2::RIGHT_CENTER, if unused { String::new() } else { count }, FontId::monospace(11.0 * z), count_color);
            painter.text(p - Vec2::new(54.0 * z, 0.0), Align2::RIGHT_CENTER, name, label_font.clone(), name_color);
        }
        // Parameter summary for the selected image.
        let own = node.mode == Mode::Individual && node.has_individual(image);
        let color = if own { Color32::from_rgb(255, 196, 120) } else { Color32::from_gray(150) };
        for (i, (label, value)) in box_rows(&session.project.graph, node, image).into_iter().enumerate() {
            let y = parameter_row_y(node, i);
            #[cfg(test)] if i == 0 && node.exposed_inputs().iter().any(Option::is_some) {
                painter.ctx().data_mut(|d| d.insert_temp(egui::Id::unique(("parameter_row", id)), node_rect.top() + y * z));
            }
            painter.text(node_rect.min + Vec2::new(10.0, y) * z, Align2::LEFT_CENTER, label, FontId::proportional(11.0 * z), Color32::from_gray(130));
            painter.text(node_rect.min + Vec2::new(NODE_W - 10.0, y) * z, Align2::RIGHT_CENTER, value, FontId::monospace(11.0 * z), color);
        }
        {
            let text = match session.count_for(node) {
                Some(n) => format!("{} img", crate::collection::with_thousands(n)),
                None => "... img".to_owned(),
            };
            let cy = node_rect.max.y - (PAD * 0.5 + COUNT_H * 0.5) * z;
            painter.text(Pos2::new(node_rect.max.x - 10.0 * z, cy), Align2::RIGHT_CENTER, text, FontId::proportional(10.5 * z), Color32::from_rgb(130, 190, 175));
        }
    }

    /// Debug timing pill just below the node: time spent in this node for the last preview.
    fn paint_timing(&self, ui: &mut egui::Ui, painter: &egui::Painter, id: NodeId, node_rect: Rect) {
        let Some(timings) = &self.timings else { return };
        let z = self.zoom;
        if z < 0.35 { return; }
        let Some((text, color, tip)) = timings.label(id) else { return };
        let font = FontId::monospace(10.5 * z);
        let galley = painter.layout_no_wrap(text, font, color);
        let size = galley.size() + Vec2::new(10.0, 4.0) * z;
        let pill = Rect::from_min_size(Pos2::new(node_rect.right() - size.x, node_rect.bottom() + 4.0 * z), size);
        painter.rect_filled(pill, (4.0 * z).round() as u8, Color32::from_rgb(28, 30, 35));
        painter.galley(pill.center() - galley.size() / 2.0, galley, color);
        ui.interact(pill, ui.scope_id().with(("timing", id.0)), Sense::hover()).on_hover_text(tip);
        #[cfg(test)] painter.ctx().data_mut(|d| d.insert_temp(egui::Id::unique(("timing_pill", id.0)), pill));
    }

    fn badges(&self, ui: &mut egui::Ui, painter: &egui::Painter, node: &Node, title: &TitleBar, issue: Option<&str>) {
        let id = node.id;
        let z = self.zoom;
        if let (Some(rect), Some(msg)) = (title.warning, issue) {
            let c = rect.center();
            let tri = vec![
                Pos2::new(c.x, c.y - 7.5 * z),
                Pos2::new(c.x + 8.0 * z, c.y + 6.5 * z),
                Pos2::new(c.x - 8.0 * z, c.y + 6.5 * z),
            ];
            painter.add(egui::Shape::convex_polygon(tri, COLOR_WARN, Stroke::new(1.0, Color32::from_rgb(90, 50, 0))));
            painter.text(Pos2::new(c.x, c.y + 1.5 * z), Align2::CENTER_CENTER, "!", FontId::proportional(11.0 * z), Color32::BLACK);
            ui.interact(rect, ui.scope_id().with(("warn", id.0)), Sense::hover()).on_hover_text(msg);
        }
        if let Some(rect) = title.count {
            let n = node.individual_count();
            painter.rect_filled(rect, (4.0 * z).round() as u8, Color32::from_rgb(30, 32, 38));
            painter.text(rect.center(), Align2::CENTER_CENTER, n.to_string(), FontId::monospace(11.0 * z), Color32::from_rgb(255, 205, 140));
            ui.interact(rect, ui.scope_id().with(("count", id.0)), Sense::hover())
                .on_hover_text(format!("{n} image(s) have their own values on this node"));
        }
        let Some(rect) = title.mode else { return };
        let (letter, fill, tip) = match node.mode {
            Mode::All => ("A", Color32::from_rgb(70, 76, 90), "Mode All: the same values for every image"),
            Mode::Individual => ("I", Color32::from_rgb(214, 128, 40), "Mode Individual: values per image"),
        };
        painter.rect_filled(rect, (4.0 * z).round() as u8, fill);
        painter.text(rect.center(), Align2::CENTER_CENTER, letter, FontId::proportional(11.5 * z), Color32::WHITE);
        let _badge = ui.interact(rect, ui.scope_id().with(("mode", id.0)), Sense::hover()).on_hover_text(tip);
        #[cfg(test)] record_port_control(&_badge, id, "mode");
    }

    fn apply(&mut self, actions: Vec<Action>, session: &mut Session, status: &mut Status) {
        for a in actions {
            match a {
                Action::ExposeInput(id, parameter) => {
                    if let Err(e) = session.project.graph.expose_parameter(id, parameter, PortType::Images) { status.error(format!("Cannot expose input: {e}")); }
                }
                Action::ExposeOutput(id, source, label) => {
                    if let Err(e) = session.project.graph.expose_output(id, source, label) { status.error(format!("Cannot expose output: {e}")); }
                }
                Action::RemoveInput(id, port) => { session.project.graph.remove_exposed_input(id, port); }
                Action::RemoveOutput(id, port) => { session.project.graph.remove_exposed_output(id, port); }
                Action::Move(id, d) => {
                    // A selected node drags the whole selection along.
                    let ids: Vec<NodeId> = if session.node_sel.contains(id) && session.node_sel.len() > 1 { session.node_sel.ids().to_vec() } else { vec![id] };
                    for id in ids {
                        if let Some(n) = session.project.graph.node_mut(id) {
                            n.pos[0] += d.x;
                            n.pos[1] += d.y;
                        }
                    }
                }
                Action::Connect { from, from_port, to, port, old } => {
                    let mut graph = session.project.graph.clone();
                    if let Some((node, input)) = old { graph.disconnect_edge(rawgraph_core::graph::Edge { from, from_port, to: node, to_port: input }); }
                    match graph.connect_ports(from, from_port, to, port) {
                        Ok(()) => session.project.graph = graph,
                        Err(e) => status.error(format!("Cannot connect: {e}")),
                    }
                },
                Action::Add(kind, pos) => {
                    let id = session.project.graph.add_node(kind, pos);
                    if kind == NodeKind::Source { crate::node_ui::set_source_folder(&mut session.project.graph, id, None); }
                    session.set_node_selection(NodeSelection::single(id));
                }
                Action::View(id) => { session.viewed_node = None; session.set_node_selection(NodeSelection::single(id)); },
                Action::Delete(id) => {
                    let result = if session.node_sel.contains(id) && session.node_sel.len() > 1 { session.delete_selected().map(|_| ()) } else { session.delete_node(id) };
                    if let Err(why) = result {
                        status.error(why);
                    }
                }
                Action::Edit(op) => self.edit.ops.push(op),
            }
        }
    }

    fn search_popup(&mut self, ctx: &egui::Context, session: &mut Session, status: &mut Status) {
        let Some(search) = &mut self.search else { return };
        let mut close = false;
        let mut add: Option<NodeKind> = None;
        let q = search.query.to_lowercase();
        let matches: Vec<NodeKind> = node_groups().into_iter().flat_map(|(_, kinds)| kinds).filter(|k| (k.label().to_lowercase().contains(&q) || group_of(*k).to_lowercase().contains(&q)) && search.connection.is_none_or(|c| compatible_port(&session.project.graph, c, *k).is_some())).collect();
        search.selected = search.selected.min(matches.len().saturating_sub(1));

        let (down, up, enter, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if down && !matches.is_empty() {
            search.selected = (search.selected + 1) % matches.len();
        }
        if up && !matches.is_empty() {
            search.selected = (search.selected + matches.len() - 1) % matches.len();
        }
        if enter {
            add = matches.get(search.selected).copied();
            close = true;
        }
        if esc {
            close = true;
        }

        let area = egui::Area::new(egui::Id::unique("node_search"))
            .order(egui::Order::Foreground)
            .fixed_pos(search.screen_pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(230.0);
                    let input_id = egui::Id::unique("rawgraph_node_search_text");
                    // Search is the active keyboard target for the lifetime of
                    // the popup, even when canvas focus is reassigned.
                    ui.memory_mut(|memory| memory.request_focus(input_id));
                    ui.add(egui::TextEdit::singleline(&mut search.query).id(input_id).hint_text("Add node...").desired_width(f32::INFINITY));
                    ui.add_space(4.0);
                    if matches.is_empty() {
                        ui.label(egui::RichText::new("No matching node").weak());
                    }
                    for (i, kind) in matches.iter().enumerate() {
                        if ui.selectable_label(i == search.selected, format!("{}    ({})", kind.label(), group_of(*kind))).clicked() {
                            add = Some(*kind);
                            close = true;
                        }
                    }
                });
            });
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed())
            && ctx.input(|i| i.pointer.interact_pos()).is_some_and(|p| !area.response.rect.contains(p));
        if clicked_outside {
            close = true;
        }
        if let Some(kind) = add {
            let pos = search.graph_pos;
            let mut graph = session.project.graph.clone();
            let port = search.connection.and_then(|c| compatible_port(&graph, c, kind));
            let id = graph.add_node(kind, pos);
            if kind == NodeKind::Source { crate::node_ui::set_source_folder(&mut graph, id, None); }
            let result = match (search.connection, port) {
                (Some(ConnDrag::FromOutput(src, src_port, old)), Some(port)) => {
                    if let Some((node, input)) = old { graph.disconnect_edge(rawgraph_core::graph::Edge { from: src, from_port: src_port, to: node, to_port: input }); }
                    graph.connect_ports(src, src_port, id, port)
                }
                (Some(ConnDrag::FromInput { node, port }), Some(from_port)) => graph.connect_ports(id, from_port, node, port),
                (None, _) => Ok(()),
                _ => { self.search = None; return; }
            };
            match result {
                Ok(()) => { session.project.graph = graph; session.set_node_selection(NodeSelection::single(id)); status.info(format!("Added {}", kind.label())); }
                Err(e) => status.error(format!("Cannot connect: {e}")),
            }
        }
        if close {
            self.search = None;
        }
    }
}

enum Action {
    ExposeInput(NodeId, &'static str),

    ExposeOutput(NodeId, usize, &'static str),
    RemoveInput(NodeId, usize),
    RemoveOutput(NodeId, usize),
    Move(NodeId, Vec2),
    Connect { from: NodeId, from_port: usize, to: NodeId, port: usize, old: Option<(NodeId, usize)> },
    Add(NodeKind, [f32; 2]),
    View(NodeId),
    Delete(NodeId),
    /// An editing command that runs at the start of the next frame.
    Edit(EditOp),
}

fn compatible_port(graph: &Graph, conn: ConnDrag, kind: NodeKind) -> Option<usize> {
    match conn {
        ConnDrag::FromOutput(node, port, _) => {
            let ty = graph.node(node)?.output_type(port)?;
            kind.inputs().iter().position(|p| p.ty == ty)
        }
        ConnDrag::FromInput { node, port } => {
            let ty = graph.node(node)?.input_type(port)?;
            kind.outputs().iter().position(|p| p.ty == ty)
        }
    }
}

fn hit_radius(zoom: f32) -> f32 {
    (PORT_R * 2.4 * zoom).max(14.0)
}

fn nearest_input(
    inputs: &[(NodeId, usize, Pos2, PortType)],
    p: Pos2,
    zoom: f32,
    ty: Option<PortType>,
) -> Option<(NodeId, usize, Pos2, PortType)> {
    let r = hit_radius(zoom);
    inputs
        .iter()
        .filter(|(_, _, c, t)| c.distance(p) <= r && ty.is_none_or(|want| want == *t))
        .min_by(|a, b| a.2.distance(p).total_cmp(&b.2.distance(p)))
        .copied()
}

fn nearest_output(outputs: &[(NodeId, usize, Pos2, PortType)], p: Pos2, zoom: f32, ty: Option<PortType>) -> Option<(NodeId, usize, Pos2, PortType)> {
    let r = hit_radius(zoom);
    outputs
        .iter()
        .filter(|(_, _, c, t)| c.distance(p) <= r && ty.is_none_or(|want| want == *t))
        .min_by(|a, b| a.2.distance(p).total_cmp(&b.2.distance(p)))
        .copied()
}

fn wire_points(from: Pos2, to: Pos2, zoom: f32) -> [Pos2; 4] {
    let dx = ((to.x - from.x).abs() * 0.5).max(50.0 * zoom);
    [from, from + Vec2::new(dx, 0.0), to - Vec2::new(dx, 0.0), to]
}

fn nearest_wire(graph: &Graph, xf: &Xform, p: Pos2) -> Option<rawgraph_core::graph::Edge> {
    let mut best = 8.0_f32;
    let mut hit = None;
    for edge in graph.edges() {
        let (Some(from), Some(to)) = (graph.node(edge.from), graph.node(edge.to)) else { continue };
        let pts = wire_points(xf.to_screen(output_port(from.pos, edge.from_port)), xf.to_screen(input_port(to, edge.to_port)), xf.zoom);
        let mut prev = pts[0];
        for i in 1..=64 {
            let t = i as f32 / 64.0;
            let u = 1.0 - t;
            let q = (pts[0].to_vec2() * (u*u*u) + pts[1].to_vec2() * (3.0*u*u*t) + pts[2].to_vec2() * (3.0*u*t*t) + pts[3].to_vec2() * (t*t*t)).to_pos2();
            let d = q - prev;
            let fraction = if d.length_sq() > 0.0 { ((p - prev).dot(d) / d.length_sq()).clamp(0.0, 1.0) } else { 0.0 };
            let distance = p.distance(prev + d * fraction);
            if distance < best { best = distance; hit = Some(*edge); }
            prev = q;
        }
    }
    hit
}

fn draw_wire(painter: &egui::Painter, from: Pos2, to: Pos2, color: Color32, zoom: f32) {
    let pts = wire_points(from, to, zoom);
    painter.add(CubicBezierShape::from_points_stroke(pts, false, Color32::TRANSPARENT, Stroke::new((2.2 * zoom).max(1.2), color)));
}

fn draw_port(painter: &egui::Painter, c: Pos2, ty: PortType, connected: bool, hot: bool, zoom: f32) {
    let r = PORT_R * zoom;
    let color = port_color(ty);
    let fill = if connected || hot { color } else { Color32::from_rgb(30, 32, 38) };
    let stroke = Stroke::new((1.6 * zoom).max(1.0), if hot { Color32::WHITE } else { color });
    painter.circle_filled(c, r, fill);
    painter.circle_stroke(c, r, stroke);
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::demo_graph;

    #[test]
    fn windows_drop_coordinates_use_screen_dpi_and_canvas_bounds() {
        let inner = Rect::from_min_size(Pos2::new(-800.0, 100.0), Vec2::new(800.0, 600.0));
        let canvas = Rect::from_min_max(Pos2::new(200.0, 100.0), Pos2::new(700.0, 500.0));
        assert_eq!(screen_to_canvas([-900, 450], 1.5, inner, canvas), Some(Pos2::new(200.0, 200.0)));
        assert_eq!(screen_to_canvas([-1050, 450], 1.5, inner, canvas), None); // sidebar
        assert_eq!(screen_to_canvas([100, 450], 1.5, inner, canvas), None); // other window
        assert_eq!(screen_to_canvas([-900, 450], 0.0, inner, canvas), None);
        let xf = Xform { origin: canvas.min, pan: Vec2::new(40.0, -20.0), zoom: 2.0 };
        assert_eq!(xf.to_graph(Pos2::new(200.0, 200.0)), Pos2::new(-20.0, 60.0));
    }

    #[test]
    fn dropped_folders_create_sources_and_schedule_scans() {
        let mut session = crate::session::tests::demo_session();
        session.new_project();
        let before = crate::node_ui::source_states(&session.project.graph);
        let paths = vec![std::path::PathBuf::from("/test/a"), std::path::PathBuf::from("/test/b")];
        add_dropped_folders(&mut session, Pos2::new(40.0, 60.0), &paths);
        let after = crate::node_ui::source_states(&session.project.graph);
        let scans = crate::node_ui::scans_needed(&before, &after, &|_, _| false);
        assert_eq!(scans.len(), 2);
        assert!(paths.iter().all(|p| scans.iter().any(|(folder, _)| folder == p)));
        let created: Vec<_> = session.project.graph.nodes().filter(|n| crate::node_ui::source_folder(n).is_some_and(|f| f == "/test/a" || f == "/test/b")).collect();
        assert_eq!(created.len(), 2);
        assert_eq!(created[0].pos, [40.0, 60.0]);
        assert_eq!(created[1].pos, [40.0, 220.0]);
        assert!(session.dirty);
    }

    #[test]
    fn transform_roundtrip_and_zoom_anchor() {
        let mut x = Xform { origin: Pos2::new(100.0, 50.0), pan: Vec2::new(20.0, -10.0), zoom: 1.0 };
        let p = Pos2::new(37.0, 91.0);
        assert!((x.to_graph(x.to_screen(p)) - p).length() < 1e-4);
        let cursor = Pos2::new(400.0, 300.0);
        let before = x.to_graph(cursor);
        x.zoom_at(cursor, 2.5);
        assert!((x.to_graph(cursor) - before).length() < 1e-3);
        assert_eq!(x.zoom, 2.5);
    }

    #[test]
    fn exposed_ports_layout_hit_testing_and_removal() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [300.0, 0.0]);
        let input = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        let output = g.expose_output(src, 0, "Luma").unwrap();
        g.connect_ports(src, output, exp, input).unwrap();
        let n = g.node(exp).unwrap();
        assert!(input_port(n, input).y < n.pos[1] + actual_node_size(n).y);
        let p = input_port(n, input);
        assert_eq!(nearest_input(&[(exp, input, p, PortType::Images)], p, 1.0, Some(PortType::Images)).unwrap().1, input);
        assert_eq!(compatible_port(&g, ConnDrag::FromOutput(src, output, None), NodeKind::Exposure), Some(0));
        assert!(g.remove_exposed_input(exp, input));
        assert!(g.edges().is_empty());
        let next = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        assert!(next > input);
        assert!(g.node(exp).unwrap().input_type(input).is_none());
    }

    fn port_control(h: &Harness, id: NodeId, name: &'static str) -> Rect {
        h.ctx.data(|d| d.get_temp(egui::Id::unique(("port_control", id, name))).expect(name))
    }

    #[test]
    fn per_node_timing_is_drawn_under_each_node_and_hidden_when_off() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [40.0, 40.0]);
        let exp = g.add_node(NodeKind::Exposure, [300.0, 40.0]);
        g.connect(src, exp, 0).unwrap();
        let mut h = Harness::new(g);
        let pill = |h: &Harness, id: NodeId| h.ctx.data(|d| d.get_temp::<Rect>(egui::Id::unique(("timing_pill", id.0))));
        assert!(pill(&h, exp).is_none(), "no timing labels while the debug option is off");
        let row = |node, ms: u64, computed, cache_hits| rawgraph_engine::NodeTiming { node, cpu: std::time::Duration::from_millis(ms), computed, cache_hits };
        let t = NodeTimings::new(&[row(src, 0, 0, 1), row(exp, 12, 1, 0)], false);
        assert_eq!(t.label(exp).unwrap().0, "12 ms");
        assert_eq!(t.label(src).unwrap().0, "cached");
        assert_eq!(t.label(exp).unwrap().1, Color32::from_rgb(235, 110, 95), "the node with most of the time is highlighted");
        h.ed.timings = Some(t);
        h.settle();
        let node_rect = |h: &Harness, id: NodeId| {
            let n = h.s.project.graph.node(id).unwrap();
            let xf = h.xf();
            Rect::from_min_size(xf.to_screen(Pos2::new(n.pos[0], n.pos[1])), actual_node_size(n) * xf.zoom)
        };
        let (rect, p) = (node_rect(&h, exp), pill(&h, exp).expect("timing pill drawn"));
        assert!(p.top() >= rect.bottom() && (p.right() - rect.right()).abs() < 1e-3, "pill sits right under the node");
        assert!(pill(&h, src).is_some());
    }

    #[test]
    fn inset_port_tabs_open_menus_without_selecting_or_dragging_the_node() {
        let (graph, src, id, _) = loose_graph();
        let mut h = Harness::new(graph);
        h.click(h.node_title_pos(src));
        let original = h.s.project.graph.node(id).unwrap().pos;
        let xf = h.xf();
        let node_rect = Rect::from_min_size(xf.to_screen(Pos2::from(original)), node_size(NodeKind::Exposure) * xf.zoom);
        let title_rect = Rect::from_min_size(node_rect.min, Vec2::new(node_rect.width(), TITLE_H * xf.zoom));
        let input = port_control(&h, id, "input");
        let output = port_control(&h, id, "output");
        let mode = port_control(&h, id, "mode");
        for tab in [input, output] {
            assert!(title_rect.contains_rect(tab));
            assert!(node_rect.shrink(4.0 * xf.zoom).contains_rect(tab));
            assert!((tab.width() - TAB_SIZE * xf.zoom).abs() < 1e-4);
            assert!((tab.center().y - title_rect.center().y).abs() < 1e-4);
        }
        assert!((input.left() - node_rect.left() - TAB_INSET * xf.zoom).abs() < 1e-4);
        assert!((node_rect.right() - output.right() - TAB_INSET * xf.zoom).abs() < 1e-4);
        assert!(output.left() - mode.right() >= TITLE_GAP * xf.zoom - 1e-4);

        h.drag(input.center(), input.center() + Vec2::new(65.0, 20.0));
        assert_eq!(h.s.selected_node, Some(src));
        assert_eq!(h.s.project.graph.node(id).unwrap().pos, original);
        h.click(input.center());
        assert_eq!(h.s.selected_node, Some(src));
        h.click(port_control(&h, id, "input_item").center());
        let node = h.s.project.graph.node(id).unwrap();
        let port = node.kind.inputs().len();
        assert_eq!(node.exposed_inputs()[0].as_ref().unwrap().parameter, "ev");
        assert_eq!(summary_rows(node, h.s.current_image()), vec![("Exposure".into(), "0.00 EV".into())]);
        let row_y = h.ctx.data(|d| d.get_temp::<f32>(egui::Id::unique(("parameter_row", id))).unwrap());
        assert!((port_control(&h, id, "parameter_port").center().y - row_y).abs() < 1e-4);
        assert!((h.in_port(id, port).y - row_y).abs() < 1e-4);

        h.click(output.center());
        assert_eq!(h.s.selected_node, Some(src));
        h.click(port_control(&h, id, "output_item").center());
        let node = h.s.project.graph.node(id).unwrap();
        assert_eq!(node.exposed_outputs()[0].as_ref().unwrap().label, "Luma");
        assert_eq!(node.pos, original);
        let undo_output = h.hist.undo().unwrap();
        assert!(undo_output.node(id).unwrap().exposed_outputs().iter().flatten().next().is_none());
        assert_eq!(undo_output.node(id).unwrap().exposed_inputs()[0].as_ref().unwrap().parameter, "ev");
        let undo_input = h.hist.undo().unwrap();
        assert!(undo_input.node(id).unwrap().exposed_inputs().iter().flatten().next().is_none());
    }

    #[test]
    fn dragging_a_wire_to_and_from_an_inline_parameter_port() {
        let (mut graph, src, id, _) = loose_graph();
        let port = graph.expose_parameter(id, "ev", PortType::Images).unwrap();
        let mut h = Harness::new(graph);
        let image = h.s.current_image();
        h.s.project.graph.node_mut(id).unwrap().set_param("ev", image, 1.51);
        h.settle();
        assert_eq!(summary_rows(h.s.project.graph.node(id).unwrap(), image), vec![("Exposure".into(), "1.51 EV".into())]);
        let source = h.out_port(src);
        let parameter = port_control(&h, id, "parameter_port").center();
        h.drag(source, parameter);
        assert_eq!(h.s.project.graph.input_edge(id, port), Some((src, 0)));
        h.drag(parameter, h.in_port(id, 0));
        assert_eq!(h.s.project.graph.input_edge(id, port), None);
        assert_eq!(h.s.project.graph.input_edge(id, 0), Some((src, 0)));
    }

    #[test]
    fn all_exposed_develop_parameters_fit_and_removed_slots_leave_no_gap() {
        let mut graph = Graph::new();
        let id = graph.add_node(NodeKind::Develop, [0.0, 0.0]);
        let keys = Graph::supported_parameter_inputs(NodeKind::Develop);
        let ports: Vec<_> = keys.iter().map(|key| graph.expose_parameter(id, key, PortType::Images).unwrap()).collect();
        let image = ImageId(1);
        let node = graph.node_mut(id).unwrap();
        node.set_param("saturation", image, 0.25);
        let rows = summary_rows(node, image);
        assert_eq!(rows.len(), 7); // Six exposed parameters and the overflow note.
        assert!(rows[6].0.contains("+1"));
        for (i, (&port, key)) in ports.iter().zip(keys).enumerate() {
            assert_eq!(rows[i].0, node.kind.param_spec(key).unwrap().label);
            assert_eq!(input_port(node, port).y, parameter_row_y(node, i));
        }
        assert!(parameter_row_y(node, 6) < actual_node_size(node).y - COUNT_H);
        assert!(graph.remove_exposed_input(id, ports[0]));
        let node = graph.node(id).unwrap();
        assert_eq!(input_port(node, ports[1]).y, parameter_row_y(node, 0));
    }

    #[test]
    fn ports_lie_on_the_node_edges() {
        for kind in NodeKind::ALL {
            let size = node_size(kind);
            let origin = [0.0, 0.0];
            for i in 0..kind.inputs().len() {
                let p = builtin_input_port(origin, i);
                assert_eq!(p.x, 0.0);
                assert!(p.y > TITLE_H && p.y < size.y, "{kind:?} input {i} inside node");
            }
            assert_eq!(output_port(origin, 0).x, NODE_W);
            for port in 0..kind.outputs().len() {
                assert!(output_port(origin, port).y < size.y, "{kind:?} output {port} inside node");
            }
        }
    }

    #[test]
    fn fit_view_contains_a_graph_that_fits_at_a_readable_zoom() {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Exposure, [240.0, 0.0]);
        g.connect(a, b, 0).unwrap();
        let content = bounds(&g).unwrap();
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 400.0));
        let (pan, zoom) = fit_view(content, viewport, None);
        assert!(zoom >= MIN_READABLE_ZOOM);
        let tl = content.min.to_vec2() * zoom + pan;
        let br = content.max.to_vec2() * zoom + pan;
        assert!(tl.x >= 0.0 && tl.y >= 0.0, "{tl:?}");
        assert!(br.x <= viewport.width() && br.y <= viewport.height(), "{br:?} in {viewport:?}");
    }

    #[test]
    fn automatic_zoom_never_drops_below_the_readable_minimum() {
        let g = demo_graph();
        let content = bounds(&g).unwrap();
        // Wide graph, small viewports: the zoom stops at the minimum.
        for size in [Vec2::new(900.0, 300.0), Vec2::new(600.0, 250.0), Vec2::new(400.0, 200.0)] {
            let viewport = Rect::from_min_size(Pos2::ZERO, size);
            let (_, zoom) = fit_view(content, viewport, None);
            assert!(zoom >= MIN_READABLE_ZOOM - 1e-6, "{zoom} in {size:?}");
        }
        // The smallest node text is then still about 9 points.
        let smallest_text_pt = 10.5 * std::hint::black_box(MIN_READABLE_ZOOM);
        assert!(smallest_text_pt >= 8.9, "{smallest_text_pt}");
        // With a focus, the focused node stays on screen after panning.
        let viewport = Rect::from_min_size(Pos2::ZERO, Vec2::new(700.0, 300.0));
        let target = g.nodes().nth(5).unwrap();
        let focus = Rect::from_min_size(Pos2::new(target.pos[0], target.pos[1]), node_size(target.kind));
        let (pan, zoom) = fit_view(content, viewport, Some(focus));
        assert_eq!(zoom, MIN_READABLE_ZOOM);
        let left = focus.min.x * zoom + pan.x;
        let right = focus.max.x * zoom + pan.x;
        assert!(left >= 0.0 && right <= viewport.width(), "focused node at {left}..{right}");
        // Without a focus the graph starts at its left end.
        let (pan, _) = fit_view(content, viewport, None);
        assert!((content.min.x * MIN_READABLE_ZOOM + pan.x - FRAME_MARGIN).abs() < 1e-3);
        // A focus beyond the content cannot push the graph out of the viewport.
        let (pan, _) = fit_view(content, viewport, Some(Rect::from_min_size(Pos2::new(1e6, 0.0), Vec2::new(10.0, 10.0))));
        assert!(content.max.x * MIN_READABLE_ZOOM + pan.x >= viewport.width() - FRAME_MARGIN - 1e-3);
    }

    // ---- headless interaction tests: the editor is driven with synthetic input ----

    use eframe::egui::{Event, PointerButton};
    use std::sync::Arc;

    struct Harness {
        ctx: egui::Context,
        ed: GraphEditor,
        s: Session,
        st: Status,
        t: f64,
        pos: Pos2,
        mods: Modifiers,
        last_mods: Modifiers,
        other_input: Option<String>,
        hist: crate::history::History,
    }

    impl Harness {
        fn new(graph: Graph) -> Self {
            let mut h = Self {
                ctx: egui::Context::default(),
                ed: GraphEditor::default(),
                s: {
                    let mut p = rawgraph_project::Project::new();
                    p.graph = graph;
                    Session::new(p, Arc::new(crate::provider::FileProvider::new(1 << 20)))
                },
                st: Status::default(),
                t: 0.0,
                pos: Pos2::new(5.0, 5.0),
                mods: Modifiers::NONE,
                last_mods: Modifiers::NONE,
                other_input: None,
                hist: crate::history::History::new(&Graph::new()),
            };
            h.hist.reset(&h.s.project.graph);
            h.ed.use_system_clipboard = false;
            h.settle();
            h
        }

        fn frame(&mut self, mut events: Vec<Event>) {
            if self.mods != self.last_mods {
                events.insert(0, Event::ModifiersChanged(self.mods));
                self.last_mods = self.mods;
            }
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 500.0))),
                time: Some(self.t),
                events,
                ..Default::default()
            };
            self.t += 0.04;
            let (ed, s, st, other_input) = (&mut self.ed, &mut self.s, &mut self.st, &mut self.other_input);
            let out = self.ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                    if let Some(text) = other_input {
                        ui.add(egui::TextEdit::singleline(text).id(egui::Id::unique("test_other_input")));
                    }
                    ed.show(ui, s, st);
                });
            });
            out.drop_without_applying_deltas();
            let gesture = self.ctx.input(|i| i.pointer.any_down());
            self.hist.commit_if_changed(&self.s.project.graph, gesture);
        }

        fn settle(&mut self) {
            for _ in 0..6 {
                self.frame(vec![]);
            }
        }

        fn xf(&self) -> Xform {
            Xform { origin: Pos2::ZERO, pan: self.ed.pan, zoom: self.ed.zoom }
        }

        fn move_to(&mut self, p: Pos2) {
            self.pos = p;
            self.frame(vec![Event::PointerMoved(p)]);
        }

        fn button(&mut self, pressed: bool) {
            let ev = Event::PointerButton { pos: self.pos, button: PointerButton::Primary, pressed, modifiers: self.mods };
            self.frame(vec![ev]);
        }

        fn key(&mut self, key: Key) {
            let ev = Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: Modifiers::NONE };
            self.frame(vec![ev]);
        }

        fn drag(&mut self, from: Pos2, to: Pos2) {
            self.move_to(from);
            self.button(true);
            self.move_to(from + (to - from) * 0.1);
            self.move_to(from + (to - from) * 0.6);
            self.move_to(to);
            self.button(false);
            self.settle();
        }

        fn click(&mut self, p: Pos2) {
            self.move_to(p);
            self.button(true);
            self.button(false);
            self.settle();
        }

        fn node_title_pos(&self, id: NodeId) -> Pos2 {
            let n = self.s.project.graph.node(id).unwrap();
            self.xf().to_screen(Pos2::new(n.pos[0] + 60.0, n.pos[1] + TITLE_H / 2.0))
        }

        fn out_port(&self, id: NodeId) -> Pos2 {
            self.xf().to_screen(output_port(self.s.project.graph.node(id).unwrap().pos, 0))
        }

        fn out_port_n(&self, id: NodeId, port: usize) -> Pos2 {
            self.xf().to_screen(output_port(self.s.project.graph.node(id).unwrap().pos, port))
        }

        fn in_port(&self, id: NodeId, port: usize) -> Pos2 {
            self.xf().to_screen(input_port(self.s.project.graph.node(id).unwrap(), port))
        }
    }

    #[test]
    fn clicking_blank_canvas_keeps_the_viewed_node() {
        let (g, _, exp, _) = loose_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(exp));
        h.click(Pos2::new(450.0, 420.0));
        assert_eq!(h.s.selected_node, Some(exp));
        assert_eq!(h.s.target_node(), Some(exp));
    }

    fn loose_graph() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [300.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [600.0, 0.0]);
        (g, src, exp, out)
    }

    #[test]
    fn dragging_from_output_to_input_connects() {
        let (g, src, exp, _) = loose_graph();
        let mut h = Harness::new(g);
        let (from, to) = (h.out_port(src), h.in_port(exp, 0));
        h.drag(from, to);
        assert_eq!(h.s.project.graph.input_of(exp, 0), Some(src));
    }

    #[test]
    fn dropping_an_output_on_blank_canvas_adds_and_wires_a_node() {
        let (g, src, _, _) = loose_graph();
        let mut h = Harness::new(g);
        h.drag(h.out_port(src), Pos2::new(450.0, 420.0));
        assert!(h.ed.search.is_some());
        h.frame(vec![Event::Text("Exposure".to_owned())]);
        h.key(Key::Enter);
        let added = h.s.selected_node.expect("the added node is selected");
        assert_eq!(h.s.project.graph.nodes().count(), 4);
        assert_eq!(h.s.project.graph.node(added).unwrap().kind, NodeKind::Exposure);
        assert_eq!(h.s.project.graph.input_of(added, 0), Some(src));
    }

    #[test]
    fn dragging_a_wire_off_an_input_preserves_it_until_confirmed() {
        let (mut g, src, exp, _) = loose_graph();
        g.connect(src, exp, 0).unwrap();
        let mut h = Harness::new(g);
        let from = h.in_port(exp, 0);
        h.drag(from, Pos2::new(450.0, 420.0));
        assert_eq!(h.s.project.graph.input_of(exp, 0), Some(src));
        assert!(h.ed.search.is_some());
    }

    #[test]
    fn dragging_a_node_moves_it_and_selects_it() {
        let (g, _, exp, _) = loose_graph();
        let mut h = Harness::new(g);
        let before = h.s.project.graph.node(exp).unwrap().pos;
        let from = h.node_title_pos(exp);
        let zoom = h.ed.zoom;
        h.drag(from, from + Vec2::new(60.0, 40.0));
        let after = h.s.project.graph.node(exp).unwrap().pos;
        assert!((after[0] - before[0] - 60.0 / zoom).abs() < 2.0, "{before:?} -> {after:?}");
        assert!((after[1] - before[1] - 40.0 / zoom).abs() < 2.0);
        assert_eq!(h.s.selected_node, Some(exp));
    }

    #[test]
    fn backspace_deletes_the_selection_including_the_last_source() {
        let (g, src, exp, _) = loose_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(exp));
        assert_eq!(h.s.selected_node, Some(exp));
        h.key(Key::Backspace);
        assert!(h.s.project.graph.node(exp).is_none());
        h.click(h.node_title_pos(src));
        h.key(Key::Backspace);
        assert!(h.s.project.graph.node(src).is_none());
        assert!(!h.st.current().is_some_and(|(_, error)| error));
        assert!(h.hist.undo().unwrap().node(src).is_some());
    }

    #[test]
    fn single_click_views_a_node() {
        let (g, _, exp, _) = loose_graph();
        let mut h = Harness::new(g);
        let p = h.node_title_pos(exp);
        h.click(p);
        assert_eq!(h.s.target_node(), Some(exp));
        assert_eq!(h.s.viewed_node, None);
    }

    #[test]
    fn tab_search_adds_the_typed_node() {
        let (g, ..) = loose_graph();
        let mut h = Harness::new(g);
        let before = h.s.project.graph.nodes().count();
        h.move_to(Pos2::new(450.0, 300.0));
        h.ed.open_search = true; // the app sets this from the Tab key
        h.settle();
        assert!(h.ed.search.is_some(), "the search box opens");
        h.frame(vec![Event::Text("fi".to_owned())]);
        h.ctx.memory_mut(|m| m.request_focus(egui::Id::unique("test_canvas_focus")));
        h.frame(vec![Event::Text("lte".to_owned())]);
        assert_eq!(h.ed.search.as_ref().unwrap().query, "filte");
        h.key(Key::Enter);
        h.settle();
        assert_eq!(h.s.project.graph.nodes().count(), before + 1);
        let added = h.s.project.graph.nodes().last().unwrap();
        assert_eq!(added.kind, NodeKind::Blur);
        assert_eq!(h.s.selected_node, Some(added.id));
        assert!(h.ed.search.is_none());
    }

    #[test]
    fn every_node_kind_is_in_exactly_one_menu_group() {
        let groups = node_groups();
        for kind in NodeKind::ALL {
            let n = groups.iter().filter(|(_, ks)| ks.contains(&kind)).count();
            assert_eq!(n, usize::from(kind != NodeKind::Output), "{kind:?}");
        }
        assert_eq!(group_of(NodeKind::Merge), "Collection");
    }

    #[test]
    fn menu_groups_follow_the_category_order() {
        let groups = node_groups();
        let names: Vec<&str> = groups.iter().map(|(g, _)| *g).collect();
        let expected: Vec<&str> = CATEGORY_ORDER.iter().copied().filter(|c| NodeKind::ALL.iter().any(|k| k.category() == *c)).collect();
        assert_eq!(names, expected, "the order given for the menu");
        assert_eq!(names[0], "Input/Output");
        assert_eq!(names[1], "Develop");
        assert_eq!(*names.last().unwrap(), "Generators");
        for (cat, kinds) in &groups {
            assert!(kinds.iter().all(|k| k.category() == *cat));
        }
        let develop = groups.iter().find(|(g, _)| *g == "Develop").unwrap();
        assert_eq!(develop.1, vec![NodeKind::CaptureEv, NodeKind::Develop]);
        let tone: Vec<NodeKind> = groups.iter().find(|(g, _)| *g == "Tone").unwrap().1.clone();
        assert!(tone.contains(&NodeKind::Exposure) && tone.contains(&NodeKind::ToneCurve));
        let generators = &groups.iter().find(|(g, _)| *g == "Generators").unwrap().1;
        assert_eq!(generators.len(), 4);
        for kind in [NodeKind::Generator, NodeKind::Draw, NodeKind::DepthMap, NodeKind::NormalMap] {
            assert!(generators.contains(&kind));
        }
    }

    #[test]
    fn long_nodes_stay_compact() {
        // Hue / Saturation has 18 parameters but the box shows at most four plus a counter.
        let hs = node_size(NodeKind::HueSaturation);
        let one = node_size(NodeKind::Exposure);
        assert!(hs.y <= one.y + 5.0 * PARAM_H, "{hs:?} vs {one:?}");
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::HueSaturation, [0.0, 0.0]);
        assert!(summary_rows(g.node(id).unwrap(), ImageId(1)).is_empty());
        for spec in NodeKind::HueSaturation.params() {
            g.node_mut(id).unwrap().set_param(spec.name, ImageId(1), spec.max);
        }
        let rows = summary_rows(g.node(id).unwrap(), ImageId(1));
        assert_eq!(rows.len(), 5);
        assert!(rows[4].0.starts_with('\u{2026}') && rows[4].0.contains("+14"));
    }

    #[test]
    fn node_boxes_show_choices_by_label() {
        let mut g = Graph::new();
        let f = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        let first = ImageId(1);
        let n = g.node_mut(f).unwrap();
        n.set_param("min", first, 100.0);
        n.set_param("max", first, 800.0);
        let rows = summary_rows(g.node(f).unwrap(), first);
        assert_eq!(rows[0], ("ISO".to_owned(), "100 ... 800".to_owned()));
        assert_eq!(rows[1].1, "missing: exclude");
        let n = g.node_mut(f).unwrap();
        n.set_param("invert", first, 1.0);
        n.set_param("missing", first, 1.0);
        assert_eq!(summary_rows(g.node(f).unwrap(), first)[1].1, "inverted, missing: include");
        let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        assert!(summary_rows(g.node(e).unwrap(), first).is_empty());
        g.node_mut(e).unwrap().set_param("ev", first, 1.0);
        assert_eq!(summary_rows(g.node(e).unwrap(), first)[0].0, "Exposure");
    }

    #[test]
    fn merge_has_four_input_ports_in_the_box() {
        let size = node_size(NodeKind::Merge);
        for i in 0..4 {
            assert!(builtin_input_port([0.0, 0.0], i).y < size.y);
        }
        assert!(node_size(NodeKind::MetadataFilter).y > node_size(NodeKind::CaptureEv).y);
    }

    #[test]
    fn nearest_port_respects_distance() {
        let a = NodeId(1);
        let inputs = vec![(a, 0, Pos2::new(10.0, 10.0), PortType::Images), (a, 1, Pos2::new(10.0, 40.0), PortType::Images)];
        assert_eq!(nearest_input(&inputs, Pos2::new(12.0, 12.0), 1.0, None).map(|x| x.1), Some(0));
        assert!(nearest_input(&inputs, Pos2::new(300.0, 300.0), 1.0, None).is_none());
    }

    /// Source -> Split by Metadata, plus two Exposure nodes and a Merge to wire up.
    fn split_graph_loose() -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = g.add_node(NodeKind::MetadataSplit, [260.0, 0.0]);
        let a = g.add_node(NodeKind::Exposure, [560.0, 0.0]);
        let b = g.add_node(NodeKind::Exposure, [560.0, 200.0]);
        let merge = g.add_node(NodeKind::Merge, [860.0, 100.0]);
        g.connect(src, split, 0).unwrap();
        (g, src, split, a, b, merge)
    }

    #[test]
    fn a_split_box_has_seven_output_ports_in_rows_inside_the_box() {
        let size = node_size(NodeKind::MetadataSplit);
        assert!(size.y > node_size(NodeKind::Merge).y, "seven output rows are taller than four input rows: {size:?}");
        assert_eq!(port_rows(NodeKind::MetadataSplit), 7);
        assert_eq!(port_rows(NodeKind::Source), 1);
        assert_eq!(port_rows(NodeKind::Merge), 4);
        let ys: Vec<f32> = (0..7).map(|p| output_port([0.0, 0.0], p).y).collect();
        assert!(ys.windows(2).all(|w| (w[1] - w[0] - ROW_H).abs() < 1e-4), "one row per port: {ys:?}");
        assert!(ys[6] < size.y);
        // Single-output nodes look as before: the one output sits in the first row.
        assert_eq!(output_port([0.0, 0.0], 0), Pos2::new(NODE_W, TITLE_H + 6.0 + ROW_H / 2.0));
    }

    #[test]
    fn dragging_from_a_specific_output_port_connects_that_port() {
        let (g, _, split, a, b, _) = split_graph_loose();
        let mut h = Harness::new(g);
        let (from, to) = (h.out_port_n(split, 1), h.in_port(a, 0));
        h.drag(from, to);
        assert_eq!(h.s.project.graph.input_edge(a, 0), Some((split, 1)), "Range 2 feeds the first Exposure");
        let (from, to) = (h.out_port_n(split, 6), h.in_port(b, 0));
        h.drag(from, to);
        assert_eq!(h.s.project.graph.input_edge(b, 0), Some((split, 6)), "Unmatched feeds the second one");
        assert_eq!(h.s.project.graph.input_edge(a, 0), Some((split, 1)), "the first wire is untouched");
    }

    #[test]
    fn wires_start_at_the_port_they_belong_to() {
        let (mut g, _, split, a, _, _) = split_graph_loose();
        g.connect_ports(split, 3, a, 0).unwrap();
        let h = Harness::new(g);
        let e = h.s.project.graph.edges().iter().find(|e| e.to == a).copied().unwrap();
        assert_eq!(e.from_port, 3);
        let start = h.out_port_n(split, e.from_port);
        let first = h.out_port_n(split, 0);
        assert!((start.y - first.y - 3.0 * ROW_H * h.ed.zoom).abs() < 0.5, "the wire leaves from the fourth row");
    }

    #[test]
    fn dragging_a_wire_off_an_input_and_onto_another_keeps_the_source_port() {
        let (mut g, _, split, a, b, _) = split_graph_loose();
        g.connect_ports(split, 2, a, 0).unwrap();
        let mut h = Harness::new(g);
        let (from, to) = (h.in_port(a, 0), h.in_port(b, 0));
        h.drag(from, to);
        assert_eq!(h.s.project.graph.input_of(a, 0), None, "disconnected from the first input");
        assert_eq!(h.s.project.graph.input_edge(b, 0), Some((split, 2)), "reconnected from the same output port");
    }

    #[test]
    fn dragging_from_an_input_to_an_output_port_picks_that_port() {
        let (g, _, split, a, _, _) = split_graph_loose();
        let mut h = Harness::new(g);
        let (from, to) = (h.in_port(a, 0), h.out_port_n(split, 4));
        h.drag(from, to);
        assert_eq!(h.s.project.graph.input_edge(a, 0), Some((split, 4)));
    }

    #[test]
    fn port_labels_follow_the_ranges_of_the_split() {
        use rawgraph_core::{NamedRange, NodeData};
        let (mut g, _, split, ..) = split_graph_loose();
        let img = ImageId(1);
        let rows = vec![NamedRange { name: "Low".into(), min: 0.0, max: 400.0 }, NamedRange { name: "High".into(), min: 401.0, max: 6400.0 }];
        assert!(g.node_mut(split).unwrap().set_data(img, Some(NodeData::Ranges(rows))));
        assert_eq!(g.output_label(split, 0, img), "Low");
        assert_eq!(g.output_label(split, 1, img), "High");
        assert_eq!(g.output_label(split, 2, img), "Range 3", "an undefined range keeps its default label");
        assert_eq!(g.output_label(split, 6, img), "Unmatched");
        assert_eq!(g.output_label(split, 7, img), "", "no such port");
    }

    // ---- wave 5b: multi-selection, copy / paste, recipes ----

    fn row_graph() -> (Graph, NodeId, NodeId, NodeId) {
        // Three nodes in a row, well apart; no edges, so every move is a pure position change.
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Exposure, [300.0, 0.0]);
        let c = g.add_node(NodeKind::Contrast, [600.0, 0.0]);
        (g, a, b, c)
    }

    impl Harness {
        fn sel(&self) -> Vec<NodeId> {
            self.s.node_sel.ids().to_vec()
        }

        fn click_with(&mut self, p: Pos2, mods: Modifiers) {
            self.mods = mods;
            self.click(p);
            self.mods = Modifiers::NONE;
            self.settle();
        }

        fn key_with(&mut self, key: Key, mods: Modifiers) {
            let ev = Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: mods };
            self.mods = mods;
            self.frame(vec![ev]);
            self.mods = Modifiers::NONE;
            self.settle();
        }

        fn undo_steps(&self) -> usize {
            self.hist.undo_len()
        }
    }

    #[test]
    fn shift_and_ctrl_click_toggle_nodes_and_the_last_clicked_is_primary() {
        let (g, a, b, c) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(a));
        assert_eq!((h.sel(), h.s.selected_node), (vec![a], Some(a)));
        h.click_with(h.node_title_pos(b), Modifiers::SHIFT);
        h.click_with(h.node_title_pos(c), Modifiers::COMMAND);
        assert_eq!(h.sel(), vec![a, b, c]);
        assert_eq!(h.s.selected_node, Some(c), "the node clicked last is the primary one");
        h.click_with(h.node_title_pos(c), Modifiers::SHIFT);
        assert_eq!((h.sel(), h.s.selected_node), (vec![a, b], Some(b)), "toggling the primary off hands over to the previous one");
        // A plain click collapses the selection to the clicked node.
        h.click(h.node_title_pos(a));
        assert_eq!(h.sel(), vec![a]);
        // Clicking empty space preserves the selection with or without Shift.
        h.click_with(h.node_title_pos(b), Modifiers::SHIFT);
        h.click_with(Pos2::new(300.0, 420.0), Modifiers::SHIFT);
        assert_eq!(h.sel().len(), 2);
        h.click(Pos2::new(300.0, 420.0));
        assert_eq!((h.sel(), h.s.selected_node), (vec![a, b], Some(b)));
    }

    #[test]
    fn dragging_on_empty_space_draws_a_rubber_band() {
        let (g, a, b, c) = row_graph();
        let mut h = Harness::new(g);
        let (pa, pc) = (h.node_title_pos(a), h.node_title_pos(c));
        // From below-right of the third node to above-left of the first: touches all three.
        h.drag(Pos2::new(pc.x + 60.0, pc.y + 190.0), Pos2::new(pa.x - 40.0, pa.y - 10.0));
        let mut all = h.sel();
        all.sort();
        assert_eq!(all, vec![a, b, c]);
        // A small band around the middle node only.
        let pb = h.node_title_pos(b);
        h.drag(Pos2::new(pb.x - 20.0, pb.y + 190.0), Pos2::new(pb.x + 30.0, pb.y + 10.0));
        assert_eq!(h.sel(), vec![b]);
        // With Shift the band adds to what is selected.
        h.mods = Modifiers::SHIFT;
        h.drag(Pos2::new(pc.x - 10.0, pc.y + 190.0), Pos2::new(pc.x + 30.0, pc.y + 10.0));
        h.mods = Modifiers::NONE;
        assert_eq!(h.sel(), vec![b, c]);
        assert_eq!(h.s.project.graph.nodes().count(), 3, "a band never changes the graph");
    }

    #[test]
    fn ctrl_a_selects_every_node() {
        let (g, a, b, c) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.key_with(Key::A, Modifiers::COMMAND);
        let mut all = h.sel();
        all.sort();
        assert_eq!(all, vec![a, b, c]);
        assert_eq!(h.s.selected_node, Some(b), "the node that was primary stays primary");
    }

    #[test]
    fn dragging_a_selected_node_moves_the_whole_selection_as_one_undo_step() {
        let (g, a, b, c) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(a));
        h.click_with(h.node_title_pos(b), Modifiers::SHIFT);
        let before: Vec<[f32; 2]> = [a, b, c].iter().map(|id| h.s.project.graph.node(*id).unwrap().pos).collect();
        let steps = h.undo_steps();
        let from = h.node_title_pos(b);
        let zoom = h.ed.zoom;
        h.drag(from, from + Vec2::new(0.0, 80.0));
        let after: Vec<[f32; 2]> = [a, b, c].iter().map(|id| h.s.project.graph.node(*id).unwrap().pos).collect();
        assert!((after[0][1] - before[0][1] - 80.0 / zoom).abs() < 2.0 && (after[1][1] - before[1][1] - 80.0 / zoom).abs() < 2.0, "both selected nodes moved");
        assert_eq!(after[2], before[2], "the unselected one stayed");
        assert_eq!(after[0][0], before[0][0], "same shift for both");
        assert_eq!(h.undo_steps(), steps + 1, "the whole drag is one undo step");
        assert_eq!(h.sel(), vec![a, b], "the selection survives the drag");
        // Dragging an unselected node selects it alone and moves only it.
        let pc = h.node_title_pos(c);
        h.drag(pc, pc + Vec2::new(0.0, -40.0));
        assert_eq!(h.sel(), vec![c]);
        assert_eq!(h.s.project.graph.node(a).unwrap().pos, after[0]);
    }

    #[test]
    fn delete_removes_the_selection_in_one_step_and_can_empty_the_graph() {
        let (g, a, b, c) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.click_with(h.node_title_pos(c), Modifiers::SHIFT);
        let steps = h.undo_steps();
        h.key(Key::Backspace);
        assert!(h.s.project.graph.node(b).is_none() && h.s.project.graph.node(c).is_none());
        assert!(h.s.project.graph.node(a).is_some());
        assert_eq!(h.undo_steps(), steps + 1, "one undo step for both nodes");
        assert!(h.sel().is_empty());
        // Everything selected: deleting the Source leaves a valid empty graph.
        h.key_with(Key::A, Modifiers::COMMAND);
        h.key(Key::Backspace);
        assert_eq!(h.s.project.graph.nodes().count(), 0);
        assert!(h.s.project.graph.validate().is_empty());
        assert!(!h.st.current().is_some_and(|(_, error)| error));
        assert!(h.hist.undo().unwrap().node(a).is_some());
        // The earlier back-history returns the other deleted nodes.
        let g = h.hist.undo().unwrap();
        assert!(g.node(b).is_some() && g.node(c).is_some());
    }

    #[test]
    fn double_click_wire_disconnects_one_edge_and_undo_restores_it() {
        let (mut g, a, b, c) = row_graph();
        g.connect(a, b, 0).unwrap();
        g.connect(b, c, 0).unwrap();
        let mut h = Harness::new(g);
        let from = h.out_port(a);
        let to = h.xf().to_screen(input_port(h.s.project.graph.node(b).unwrap(), 0));
        let p = from + (to - from) * 0.5;
        let steps = h.undo_steps();
        h.move_to(p);
        h.button(true); h.button(false);
        assert_eq!(h.s.project.graph.input_of(b, 0), Some(a));
        h.button(true); h.button(false);
        assert_eq!(h.s.project.graph.input_of(b, 0), None);
        assert_eq!(h.s.project.graph.input_of(c, 0), Some(b));
        assert_eq!(h.undo_steps(), steps + 1);
        assert_eq!(h.hist.undo().unwrap().input_of(b, 0), Some(a));
    }

    #[test]
    fn clipboard_events_work_with_focused_node_but_not_other_widgets() {
        let (g, _, b, _) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.frame(vec![Event::Copy]);
        let text = h.ed.last_copy().expect("copy with graph focus").to_owned();
        h.frame(vec![Event::Paste(text.clone())]);
        assert_eq!(h.s.project.graph.nodes().count(), 4);
        h.ctx.memory_mut(|m| m.request_focus(egui::Id::unique("other widget")));
        h.frame(vec![Event::Paste(text)]);
        assert_eq!(h.s.project.graph.nodes().count(), 4);
    }

    #[test]
    fn delete_respects_graph_focus_and_can_be_undone() {
        let (g, _, b, _) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.other_input = Some("abc".into());
        h.frame(vec![]);
        let other = egui::Id::unique("test_other_input");
        h.ctx.memory_mut(|m| m.request_focus(other));
        h.key(Key::Delete);
        assert_eq!(h.ctx.memory(|m| m.focused()), Some(other));
        assert!(h.s.project.graph.node(b).is_some());
        h.key(Key::Backspace);
        assert_eq!(h.ctx.memory(|m| m.focused()), Some(other));
        assert!(h.s.project.graph.node(b).is_some());
        h.other_input = None;
        h.click(h.node_title_pos(b));
        h.key(Key::Delete);
        assert!(h.s.project.graph.node(b).is_none());
        assert!(h.hist.undo().unwrap().node(b).is_some());
    }

    #[test]
    fn copy_and_paste_round_trip_through_clipboard_events() {
        let (mut g, a, b, c) = row_graph();
        g.node_mut(b).unwrap().set_param("ev", ImageId(0), 1.5);
        g.connect(b, c, 0).unwrap();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.click_with(h.node_title_pos(c), Modifiers::SHIFT);
        h.frame(vec![Event::Copy]);
        let text = h.ed.last_copy().expect("the copy text").to_owned();
        assert!(text.contains("format_version"), "JSON of the sub-graph");
        assert_eq!(h.s.project.graph.nodes().count(), 3, "copy changes nothing");
        // Paste with the pointer over the editor: the group lands under the pointer.
        let steps = h.undo_steps();
        let at = Pos2::new(200.0, 300.0);
        h.move_to(at);
        h.frame(vec![Event::Paste(text.clone())]);
        h.settle();
        assert_eq!(h.s.project.graph.nodes().count(), 5);
        assert_eq!(h.undo_steps(), steps + 1, "one undo step");
        let new = h.sel();
        assert_eq!(new.len(), 2);
        assert!(!new.contains(&b) && !new.contains(&c), "the new nodes are selected, not the originals");
        let g = &h.s.project.graph;
        assert_eq!(g.input_of(new[1], 0), Some(new[0]), "the edge between the copies is kept");
        assert!((g.node(new[0]).unwrap().param("ev", ImageId(0)) - 1.5).abs() < 1e-6);
        let gp = h.xf().to_graph(at);
        let pos = g.node(new[0]).unwrap().pos;
        assert!((pos[0] - gp.x).abs() < 1.5 && (pos[1] - gp.y).abs() < 1.5, "{pos:?} vs {gp:?}");
        assert_eq!(g.node(a).unwrap().pos, [0.0, 0.0], "originals untouched");
        // Undo takes the whole paste back.
        let back = h.hist.undo().unwrap();
        assert_eq!(back.nodes().count(), 3);
    }

    #[test]
    fn paste_without_a_pointer_in_the_editor_is_offset_from_the_originals() {
        let (g, _, b, _) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.frame(vec![Event::Copy]);
        let text = h.ed.last_copy().unwrap().to_owned();
        h.frame(vec![Event::PointerGone]);
        h.frame(vec![Event::Paste(text.clone())]);
        let first = h.sel()[0];
        assert_eq!(h.s.project.graph.node(first).unwrap().pos, [300.0 + crate::graph_clip::PASTE_STEP, crate::graph_clip::PASTE_STEP]);
        h.frame(vec![Event::Paste(text)]);
        let second = h.sel()[0];
        assert_ne!(h.s.project.graph.node(second).unwrap().pos, h.s.project.graph.node(first).unwrap().pos, "a second paste does not sit on the first");
    }

    #[test]
    fn invalid_clipboard_text_changes_nothing_and_says_so() {
        let (g, ..) = row_graph();
        let mut h = Harness::new(g);
        let steps = h.undo_steps();
        for bad in ["hello world", "{\"nodes\": 5}", ""] {
            h.frame(vec![Event::Paste(bad.to_owned())]);
            assert_eq!(h.s.project.graph.nodes().count(), 3, "{bad:?}");
        }
        h.frame(vec![Event::Paste("hello world".to_owned())]);
        assert!(h.st.current().is_some_and(|(t, e)| e && t.contains("RawGraph nodes")), "{:?}", h.st.current());
        assert_eq!(h.undo_steps(), steps);
    }

    #[test]
    fn cut_removes_the_nodes_and_duplicate_adds_offset_copies() {
        let (g, a, b, c) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.key_with(Key::D, Modifiers::COMMAND);
        assert_eq!(h.s.project.graph.nodes().count(), 4, "duplicate");
        let dup = h.sel()[0];
        assert_ne!(dup, b);
        assert_eq!(h.s.project.graph.node(dup).unwrap().kind, NodeKind::Exposure);
        assert!(h.ed.last_copy().is_none(), "duplicate does not touch the clipboard");
        h.click(h.node_title_pos(c));
        h.frame(vec![Event::Cut]);
        assert!(h.s.project.graph.node(c).is_none(), "cut removes the node");
        assert!(h.ed.last_copy().is_some());
        h.frame(vec![Event::Paste(h.ed.last_copy().unwrap().to_owned())]);
        assert_eq!(h.s.project.graph.nodes().filter(|n| n.kind == NodeKind::Contrast).count(), 1, "and paste brings it back");
        let _ = a;
    }

    #[test]
    fn recipes_insert_unconnected_next_to_the_pointer() {
        let dir = tempfile::tempdir().unwrap();
        let (mut g, a, b, c) = row_graph();
        g.connect(b, c, 0).unwrap();
        let mut h = Harness::new(g);
        h.ed.set_recipes_dir(Some(dir.path().join("recipes")));
        h.click(h.node_title_pos(b));
        h.click_with(h.node_title_pos(c), Modifiers::SHIFT);
        let path = h.ed.save_selection_as_recipe(&h.s, "Look / one").unwrap();
        assert!(path.starts_with(dir.path().join("recipes")) && path.to_string_lossy().ends_with(crate::recipes::RECIPE_EXT));
        assert!(h.ed.save_selection_as_recipe(&h.s, "Look / one").is_err(), "no silent overwrite");
        let list = h.ed.recipe_list();
        assert_eq!(list.len(), 1);
        assert!(list[0].usable && list[0].nodes == 2);
        let steps = h.undo_steps();
        h.ed.request(EditOp::InsertRecipe(list[0].stem.clone(), Some([100.0, 250.0])));
        h.settle();
        assert_eq!(h.s.project.graph.nodes().count(), 5);
        assert_eq!(h.undo_steps(), steps + 1);
        let new = h.sel();
        assert_eq!(h.s.project.graph.node(new[0]).unwrap().pos, [100.0, 250.0]);
        assert_eq!(h.s.project.graph.input_of(new[0], 0), None, "not connected to anything outside the recipe");
        assert_eq!(h.s.project.graph.input_of(new[1], 0), Some(new[0]), "the recipe's own edge is there");
        let _ = a;
    }

    #[test]
    fn the_inspector_shows_the_primary_node_and_the_count() {
        let (g, _, b, c) = row_graph();
        let mut h = Harness::new(g);
        h.click(h.node_title_pos(b));
        h.click_with(h.node_title_pos(c), Modifiers::SHIFT);
        h.key_with(Key::A, Modifiers::COMMAND);
        assert_eq!(h.s.node_sel.len(), 3);
        // The inspector reads the same session state.
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 800.0))), ..Default::default() }, |ui| {
            egui::CentralPanel::default().show(ui, |ui| crate::inspector::show(ui, &mut h.s));
        });
        out.textures_delta.clear();
        let text: String = out.shapes.iter().filter_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) => Some(t.galley.text().to_owned()),
            _ => None,
        }).collect::<Vec<_>>().join("\n");
        assert!(text.contains("3 nodes selected"), "{text}");
        assert!(text.contains("Contrast"), "the primary node (clicked last, before Ctrl+A) is shown: {text}");
    }
}
