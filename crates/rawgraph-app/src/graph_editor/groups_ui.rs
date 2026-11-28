//! Groups and macros in the graph editor: which level is open, the breadcrumb bar, the port
//! stubs of an open group, and the commands (group, ungroup, open, up, save as macro).

use std::collections::HashMap;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use rawgraph_core::{Graph, NodeId, NodeKind, PortType};

use super::{draw_port, GraphEditor, NODE_W, PAD, PORT_R, ROW_H, TITLE_H};
use crate::macros;
use crate::selection::NodeSelection;
use crate::session::Session;
use crate::status::Status;

/// Width of the "Inputs" / "Outputs" stubs of an open group.
pub const STUB_W: f32 = 150.0;
const STUB_GAP: f32 = 90.0;
pub const BREADCRUMB_H: f32 = 26.0;
pub const GROUP_COLOR: Color32 = Color32::from_rgb(88, 92, 132);

pub struct MacroPrompt {
    pub group: NodeId,
    pub name: String,
    pub description: String,
    pub error: Option<String>,
    pub focus: bool,
}

#[derive(Default)]
pub struct GroupState {
    /// The groups that are open, outermost first. Empty: the top level of the graph.
    pub path: Vec<NodeId>,
    /// Pan and zoom each level had when it was left.
    views: HashMap<Option<NodeId>, (Vec2, f32)>,
    last_level: Option<Option<NodeId>>,
    pub(super) macro_prompt: Option<MacroPrompt>,
    pub(super) rename: Option<(NodeId, String, bool)>,
    /// Macro list cache of the add-node menu.
    pub(super) macro_list: Option<(std::time::Instant, Vec<macros::MacroInfo>)>,
}

impl GroupState {
    /// Closes every level that no longer exists (a group was deleted, undone or ungrouped).
    pub fn reconcile(&mut self, graph: &Graph) {
        let mut keep = 0;
        let mut parent: Option<NodeId> = None;
        for id in &self.path {
            let is_group = graph.node(*id).is_some_and(|n| n.kind == NodeKind::Group);
            if !is_group || graph.parent_of(*id) != parent {
                break;
            }
            parent = Some(*id);
            keep += 1;
        }
        self.path.truncate(keep);
    }

    pub fn level(&self) -> Option<NodeId> {
        self.path.last().copied()
    }
}

/// The macros of `dir`, read again when the cached list is older than a moment.
pub fn cached_macro_list(slot: &mut Option<(std::time::Instant, Vec<macros::MacroInfo>)>, dir: &std::path::Path) -> Vec<macros::MacroInfo> {
    let stale = slot.as_ref().is_none_or(|(t, _)| t.elapsed() > std::time::Duration::from_millis(800));
    if stale {
        *slot = Some((std::time::Instant::now(), macros::list(dir)));
    }
    slot.as_ref().map(|(_, l)| l.clone()).unwrap_or_default()
}

/// Where the stubs of an open group sit, in graph units.
#[derive(Clone, Copy, Debug)]
pub struct Stubs {
    pub inp: [f32; 2],
    pub out: [f32; 2],
    pub in_rows: usize,
    pub out_rows: usize,
}

impl Stubs {
    pub fn size(rows: usize) -> Vec2 {
        Vec2::new(STUB_W, TITLE_H + 6.0 + rows.max(1) as f32 * ROW_H + PAD)
    }
    pub fn in_rect(&self) -> Rect {
        Rect::from_min_size(Pos2::new(self.inp[0], self.inp[1]), Self::size(self.in_rows))
    }
    pub fn out_rect(&self) -> Rect {
        Rect::from_min_size(Pos2::new(self.out[0], self.out[1]), Self::size(self.out_rows))
    }
    /// A port of the left stub: it feeds the inside, so it is a source (output side).
    pub fn in_port(&self, k: usize) -> Pos2 {
        Pos2::new(self.inp[0] + STUB_W, self.inp[1] + TITLE_H + 6.0 + k as f32 * ROW_H + ROW_H / 2.0)
    }
    /// A port of the right stub: it receives what leaves the inside.
    pub fn out_port(&self, k: usize) -> Pos2 {
        Pos2::new(self.out[0], self.out[1] + TITLE_H + 6.0 + k as f32 * ROW_H + ROW_H / 2.0)
    }
}

/// Stub positions around the content of the open group.
pub fn stubs_for(graph: &Graph, group: NodeId) -> Stubs {
    let def = graph.node(group).and_then(|n| n.group());
    let content = super::bounds_in(graph, Some(group)).unwrap_or(Rect::from_min_size(Pos2::ZERO, Vec2::new(NODE_W, 60.0)));
    Stubs {
        inp: [content.min.x - STUB_W - STUB_GAP, content.min.y],
        out: [content.max.x + STUB_GAP, content.min.y],
        in_rows: def.map_or(1, |d| d.inputs.len()),
        out_rows: def.map_or(1, |d| d.outputs.len()),
    }
}

impl GraphEditor {
    /// Opens `id` (a group of the level shown).
    pub fn open_group(&mut self, graph: &Graph, id: NodeId) -> bool {
        if graph.node(id).is_none_or(|n| n.kind != NodeKind::Group) || graph.parent_of(id) != self.groups.level() {
            return false;
        }
        self.groups.path.push(id);
        true
    }

    /// Goes one level up; the group just left becomes the selection.
    pub fn close_group(&mut self, session: &mut Session) -> bool {
        let Some(left) = self.groups.path.pop() else { return false };
        if session.project.graph.node(left).is_some() {
            session.set_node_selection(NodeSelection::single(left));
        }
        true
    }

    /// Goes up to the level with `depth` open groups (0: the top level).
    pub fn go_to_level(&mut self, session: &mut Session, depth: usize) {
        while self.groups.path.len() > depth {
            self.close_group(session);
        }
    }

    /// Called once per frame: keeps pan/zoom per level and drops selected nodes the new level does not show.
    pub(super) fn enter_level_if_changed(&mut self, session: &mut Session) {
        let level = self.groups.level();
        if self.groups.last_level == Some(level) {
            return;
        }
        if let Some(prev) = self.groups.last_level {
            self.groups.views.insert(prev, (self.pan, self.zoom));
        }
        match self.groups.views.get(&level).copied() {
            Some((pan, zoom)) if self.groups.last_level.is_some() => {
                self.pan = pan;
                self.zoom = zoom;
                self.auto_framed = false;
            }
            _ => self.request_frame_all(),
        }
        self.groups.last_level = Some(level);
        let visible = session.project.graph.children(level);
        let kept: Vec<NodeId> = session.node_sel.ids().iter().copied().filter(|id| visible.contains(id)).collect();
        if kept.len() != session.node_sel.len() {
            let mut sel = NodeSelection::default();
            sel.replace(kept);
            session.set_node_selection(sel);
        }
        session.viewed_node = session.viewed_node.filter(|id| session.project.graph.node(*id).is_some());
    }

    /// "Graph > Group > Inner" bar of the open group; a click goes back to that level.
    pub(super) fn breadcrumb(&mut self, ui: &mut egui::Ui, painter: &egui::Painter, rect: Rect, graph: &Graph, ops: &mut Vec<super::EditOp>) {
        if self.groups.path.is_empty() {
            return;
        }
        let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), BREADCRUMB_H));
        painter.rect_filled(bar, 0.0, Color32::from_rgba_unmultiplied(20, 22, 26, 235));
        painter.line_segment([bar.left_bottom(), bar.right_bottom()], Stroke::new(1.0, Color32::from_rgb(60, 64, 74)));
        let mut x = bar.min.x + 10.0;
        let names: Vec<String> = std::iter::once("Graph".to_owned())
            .chain(self.groups.path.iter().map(|id| graph.node(*id).and_then(|n| n.group()).map_or_else(|| "?".to_owned(), |g| g.label.clone())))
            .collect();
        let last = names.len() - 1;
        for (depth, name) in names.iter().enumerate() {
            let current = depth == last;
            let color = if current { Color32::from_gray(235) } else { Color32::from_rgb(130, 180, 240) };
            let galley = painter.layout_no_wrap(name.clone(), FontId::proportional(13.0), color);
            let seg = Rect::from_min_size(Pos2::new(x, bar.min.y + (BREADCRUMB_H - galley.size().y) / 2.0), galley.size());
            let resp = ui.interact(seg.expand2(Vec2::new(4.0, 4.0)), ui.scope_id().with(("crumb", depth)), if current { Sense::hover() } else { Sense::click() });
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, !current, format!("Go to {name}")));
            let hovered = resp.hovered() && !current;
            painter.galley(seg.min, galley, color);
            if hovered {
                painter.line_segment([seg.left_bottom(), seg.right_bottom()], Stroke::new(1.0, color));
            }
            if resp.clicked() {
                ops.push(super::EditOp::GoToLevel(depth));
            }
            x = seg.max.x + 8.0;
            if !current {
                painter.text(Pos2::new(x, bar.center().y), Align2::LEFT_CENTER, ">", FontId::proportional(13.0), Color32::from_gray(120));
                x += 16.0;
            }
        }
        painter.text(Pos2::new(bar.max.x - 10.0, bar.center().y), Align2::RIGHT_CENTER, "Esc: up one level", FontId::proportional(11.0), Color32::from_gray(120));
    }

    /// Paints the Inputs / Outputs stubs of the open group. Context menus remove ports.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn paint_stubs(&self, ui: &mut egui::Ui, painter: &egui::Painter, xf: &super::Xform, graph: &Graph, group: NodeId, edges_in: &[bool], edges_out: &[bool], remove: &mut Vec<(bool, usize)>) {
        let Some(def) = graph.node(group).and_then(|n| n.group()) else { return };
        let stubs = stubs_for(graph, group);
        let z = self.zoom;
        for (is_input, rect_g, labels, connected) in [
            (true, stubs.in_rect(), def.inputs.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(), edges_in),
            (false, stubs.out_rect(), def.outputs.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(), edges_out),
        ] {
            let r = Rect::from_min_max(xf.to_screen(rect_g.min), xf.to_screen(rect_g.max));
            let radius = self.node_radius();
            painter.rect_filled(r, radius, Color32::from_rgb(34, 36, 44));
            painter.rect_stroke(r, radius, Stroke::new(1.0, Color32::from_rgb(76, 80, 100)), StrokeKind::Inside);
            let title = Rect::from_min_size(r.min, Vec2::new(r.width(), TITLE_H * z));
            painter.rect_filled(title, egui::CornerRadius { nw: radius, ne: radius, sw: 0, se: 0 }, Color32::from_rgb(58, 62, 84));
            if z >= 0.35 {
                painter.text(title.center(), Align2::CENTER_CENTER, if is_input { "Group inputs" } else { "Group outputs" }, FontId::proportional(13.0 * z), Color32::from_gray(225));
            }
            if labels.is_empty() && z >= 0.5 {
                painter.text(Pos2::new(r.center().x, r.min.y + (TITLE_H + 6.0 + ROW_H / 2.0) * z), Align2::CENTER_CENTER, "no ports", FontId::proportional(11.0 * z), Color32::from_gray(110));
            }
            for (k, label) in labels.iter().enumerate() {
                let c = xf.to_screen(if is_input { stubs.in_port(k) } else { stubs.out_port(k) });
                let hover = Rect::from_center_size(c, Vec2::splat((PORT_R * 2.6 * z).max(14.0)));
                let resp = ui.interact(hover, ui.scope_id().with(("stub", is_input, k)), Sense::click());
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Group {} {label}", if is_input { "input" } else { "output" })));
                let resp = resp.on_hover_text(format!("{label}\nRight-click to remove this port"));
                resp.context_menu(|ui| {
                    if ui.button("Remove port").clicked() {
                        remove.push((is_input, k));
                        ui.close();
                    }
                });
                draw_port(painter, c, PortType::Images, connected.get(k).copied().unwrap_or(false), resp.hovered(), z);
                if z >= 0.5 {
                    let font = FontId::proportional(11.5 * z);
                    let color = Color32::from_gray(190);
                    let shown = crate::export::elide_middle(label, 22);
                    if is_input {
                        painter.text(c - Vec2::new(11.0 * z, 0.0), Align2::RIGHT_CENTER, shown, font, color);
                    } else {
                        painter.text(c + Vec2::new(11.0 * z, 0.0), Align2::LEFT_CENTER, shown, font, color);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------ commands

    /// Ctrl+G: the selected nodes become one group node.
    pub fn group_selection(&mut self, session: &mut Session, status: &mut Status) {
        session.reconcile_selection();
        let ids: Vec<NodeId> = session.node_sel.ids().to_vec();
        match session.project.graph.group_nodes(&ids, "Group") {
            Ok(id) => {
                session.set_node_selection(NodeSelection::single(id));
                status.info(format!("Grouped {} node{}", ids.len(), if ids.len() == 1 { "" } else { "s" }));
            }
            Err(e) => status.error(e.to_string()),
        }
    }

    /// Ctrl+Shift+G: every selected group is dissolved, its nodes stay selected.
    pub fn ungroup_selection(&mut self, session: &mut Session, status: &mut Status) {
        session.reconcile_selection();
        let ids: Vec<NodeId> = session.node_sel.ids().to_vec();
        let mut freed: Vec<NodeId> = Vec::new();
        let mut count = 0;
        for id in ids {
            if session.project.graph.node(id).is_some_and(|n| n.kind == NodeKind::Group) {
                if let Ok(members) = session.project.graph.ungroup(id) {
                    freed.extend(members);
                    count += 1;
                }
            }
        }
        if count == 0 {
            status.info("Select a group to ungroup");
            return;
        }
        let mut sel = NodeSelection::default();
        sel.replace(freed);
        session.set_node_selection(sel);
        status.info(format!("Ungrouped {count} group{}", if count == 1 { "" } else { "s" }));
    }

    /// Enter / double-click: opens the selected group.
    pub fn open_selected_group(&mut self, session: &mut Session) -> bool {
        let Some(id) = session.selected_node else { return false };
        if self.open_group(&session.project.graph, id) {
            session.set_node_selection(NodeSelection::default());
            true
        } else {
            false
        }
    }

    pub(super) fn rename_group_prompt(&mut self, session: &Session, id: NodeId) {
        if let Some(def) = session.project.graph.node(id).and_then(|n| n.group()) {
            self.groups.rename = Some((id, def.label.clone(), true));
        }
    }

    /// The small windows of group commands: rename and "save as macro".
    pub(super) fn group_windows(&mut self, ctx: &egui::Context, session: &mut Session, status: &mut Status) {
        if let Some((id, text, focus)) = &mut self.groups.rename {
            let mut done = false;
            let mut cancel = false;
            egui::Window::new("Rename group").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                let edit = ui.add(egui::TextEdit::singleline(text).desired_width(260.0));
                if *focus {
                    edit.request_focus();
                    *focus = false;
                }
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = true;
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(!text.trim().is_empty(), egui::Button::new("Rename")).clicked() {
                        done = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
            if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                cancel = true;
            }
            if done {
                let (id, text) = (*id, text.clone());
                match session.project.graph.set_group_label(id, &text) {
                    Ok(()) => self.groups.rename = None,
                    Err(e) => status.error(e.to_string()),
                }
            } else if cancel {
                self.groups.rename = None;
            }
        }
        let Some(prompt) = &mut self.groups.macro_prompt else { return };
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Save group as macro").collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label("The group is saved with everything inside it, its ports and its published parameters.");
            ui.label(egui::RichText::new(macros::COPY_NOTE).weak());
            let edit = ui.add(egui::TextEdit::singleline(&mut prompt.name).hint_text("Macro name").desired_width(320.0));
            if prompt.focus {
                edit.request_focus();
                prompt.focus = false;
            }
            ui.add(egui::TextEdit::multiline(&mut prompt.description).hint_text("Description (optional)").desired_rows(2).desired_width(320.0));
            if let Some(e) = &prompt.error {
                ui.label(egui::RichText::new(e).color(Color32::from_rgb(255, 150, 120)));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(!prompt.name.trim().is_empty(), egui::Button::new("Save")).clicked() {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
            });
        });
        if save {
            let result = self.recipes_dir.clone().ok_or_else(|| "Macros are not available".to_owned()).and_then(|dir| {
                let m = session.project.graph.extract_macro(prompt.group, &prompt.name, &prompt.description).map_err(|e| e.to_string())?;
                macros::save(&dir, &m)
            });
            match result {
                Ok(path) => {
                    status.info(format!("Saved macro {}", path.display()));
                    self.groups.macro_prompt = None;
                    self.groups.macro_list = None;
                }
                Err(e) => prompt.error = Some(e),
            }
        } else if cancel {
            self.groups.macro_prompt = None;
        }
    }

    /// Opens the "Save group as macro" prompt for the selected group.
    pub fn start_save_macro(&mut self, session: &Session, status: &mut Status) {
        let Some(id) = session.selected_node.filter(|id| session.project.graph.node(*id).is_some_and(|n| n.kind == NodeKind::Group)) else {
            status.info("Select a group to save as a macro");
            return;
        };
        if self.recipes_dir.is_none() {
            status.error("Macros are not available");
            return;
        }
        let name = session.project.graph.node(id).and_then(|n| n.group()).map(|g| g.label.clone()).unwrap_or_default();
        self.groups.macro_prompt = Some(MacroPrompt { group: id, name, description: String::new(), error: None, focus: true });
    }

    /// Inserts the macro `stem` as a group (a copy) at `at` of the level shown.
    pub fn insert_macro(&mut self, stem: &str, at: Option<[f32; 2]>, session: &mut Session, status: &mut Status) -> bool {
        let Some(dir) = self.recipes_dir.clone() else { return false };
        let (m, notes) = match macros::load_with_notes(&dir, stem) {
            Ok(x) => x,
            Err(why) => {
                status.error(why);
                return false;
            }
        };
        let pos = at.unwrap_or_else(|| {
            let n = self.edit.paste_count as f32;
            self.edit.paste_count += 1;
            [60.0 + 48.0 * n, 60.0 + 48.0 * n]
        });
        let backup = session.project.graph.clone();
        match session.project.graph.insert_macro(&m, pos) {
            Ok(id) => {
                if let Some(level) = self.groups.level() {
                    if let Err(e) = session.project.graph.adopt(level, &[id]) {
                        session.project.graph = backup;
                        status.error(format!("Macro \"{stem}\" was not inserted: {e}"));
                        return false;
                    }
                }
                session.set_node_selection(NodeSelection::single(id));
                status.info(format!("Inserted macro \"{stem}\" as a copy; changes to the macro file do not update it"));
                status.migration("Macro", notes);
                true
            }
            Err(e) => {
                status.error(e.to_string());
                false
            }
        }
    }
}
