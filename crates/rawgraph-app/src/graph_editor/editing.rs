//! Editing operations of the graph editor beyond single nodes: copy, cut,
//! paste, duplicate, select all, recipes and the dialogs around them.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect};
use rawgraph_core::{NodeId, Subgraph};

use super::{GraphEditor, Xform};
use crate::graph_clip::{self, Anchor};
use crate::recipes::{self, RecipeInfo};
use crate::selection::NodeSelection;
use crate::session::Session;
use crate::status::Status;

const RECIPE_LIST_TTL: Duration = Duration::from_millis(800);
/// Hover text of the copy and paste commands.
pub const PER_IMAGE_NOTE: &str = "Per-image values (Individual mode) are not copied; only the shared values are.";

/// An editing command from the menu or a key.
#[derive(Clone, Debug, PartialEq)]
pub enum EditOp {
    Copy,
    Cut,
    /// Paste from the clipboard; at the pointer when it is over the editor, else offset.
    Paste,
    /// Paste at a graph position (background menu).
    PasteAt([f32; 2]),
    Duplicate,
    SelectAll,
    /// Opens the "Save selection as recipe" prompt.
    SaveRecipe,
    InsertRecipe(String, Option<[f32; 2]>),
    ManageRecipes,
    ImportRecipeFile,
}

/// Rubber band in progress.
pub(super) struct Band {
    pub start: Pos2,
    /// Nodes that stay selected (Shift/Ctrl held when the band started).
    pub base: Vec<NodeId>,
}

pub(super) struct RecipePrompt {
    pub name: String,
    pub sub: Subgraph,
    pub error: Option<String>,
    pub focus: bool,
}

#[derive(Default)]
pub(super) struct ManageUi {
    pub rename: Option<(String, String)>,
    pub confirm_delete: Option<String>,
    pub message: Option<(String, bool)>,
}

enum DialogPurpose {
    Import,
    Export(String),
}

pub(super) struct RecipeDialog {
    purpose: DialogPurpose,
    rx: Receiver<Option<PathBuf>>,
}

#[derive(Default)]
pub(super) struct EditState {
    pub band: Option<Band>,
    pub ops: Vec<EditOp>,
    pub paste_count: u32,
    pub prompt: Option<RecipePrompt>,
    pub manage: ManageUi,
    pub dialog: Option<RecipeDialog>,
    pub list: Option<(Instant, Vec<RecipeInfo>)>,
}

/// The recipe list, read again when the cached one is older than a moment.
pub(super) fn cached_list(slot: &mut Option<(Instant, Vec<RecipeInfo>)>, dir: Option<&std::path::Path>) -> Vec<RecipeInfo> {
    let Some(dir) = dir else { return Vec::new() };
    let stale = slot.as_ref().is_none_or(|(t, _)| t.elapsed() > RECIPE_LIST_TTL);
    if stale {
        *slot = Some((Instant::now(), recipes::list(dir)));
    }
    slot.as_ref().map(|(_, l)| l.clone()).unwrap_or_default()
}

fn pluralize(n: usize) -> String {
    format!("{n} node{}", if n == 1 { "" } else { "s" })
}

impl GraphEditor {
    /// Queues an editing command (menu items); it runs in the next `show`.
    pub fn request(&mut self, op: EditOp) {
        self.edit.ops.push(op);
    }

    /// Where the recipes live (`None` disables the recipe commands).
    pub fn set_recipes_dir(&mut self, dir: Option<PathBuf>) {
        self.recipes_dir = dir;
        self.edit.list = None;
    }

    /// The text last put on the clipboard by this editor.
    pub fn last_copy(&self) -> Option<&str> {
        self.last_copy.as_deref()
    }

    /// The Tab search box is open (Esc belongs to it).
    pub fn search_open(&self) -> bool {
        self.search.is_some()
    }

    /// Saves the selected nodes as a recipe (command line).
    pub fn save_selection_as_recipe(&mut self, session: &Session, name: &str) -> Result<PathBuf, String> {
        let sub = session.project.graph.extract_subgraph(session.node_sel.ids());
        self.save_recipe_now(name, &sub)
    }

    pub fn open_manage(&mut self) {
        self.manage_open = true;
    }

    fn clipboard_write(&mut self, text: &str) {
        self.last_copy = Some(text.to_owned());
        self.edit.paste_count = 0;
        if self.use_system_clipboard {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(text.to_owned());
            }
        }
    }

    fn clipboard_read(&self, from_event: Option<&str>) -> Option<String> {
        if !self.use_system_clipboard {
            return from_event.map(str::to_owned).or_else(|| self.last_copy.clone());
        }
        arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()).or_else(|| from_event.map(str::to_owned))
    }

    pub fn copy_selection(&mut self, session: &Session, status: &mut Status) -> bool {
        match graph_clip::copy_text(&session.project.graph, session.node_sel.ids()) {
            Some(text) => {
                self.clipboard_write(&text);
                status.info(format!("Copied {}", pluralize(session.node_sel.len())));
                true
            }
            None => {
                status.info("Select nodes first");
                false
            }
        }
    }

    pub fn cut_selection(&mut self, session: &mut Session, status: &mut Status) {
        if !self.copy_selection(session, status) {
            return;
        }
        let n = session.node_sel.len();
        match session.delete_selected() {
            Ok(_) => status.info(format!("Cut {}", pluralize(n))),
            Err(why) => status.error(why),
        }
    }

    /// Pastes clipboard text. `at` is the graph position under the pointer, if the pointer is over the editor.
    pub fn paste_text(&mut self, text: Option<&str>, at: Option<[f32; 2]>, session: &mut Session, status: &mut Status) -> bool {
        let Some(text) = text else {
            status.error("The clipboard is empty");
            return false;
        };
        let sub = match graph_clip::parse_clip(text) {
            Ok(s) => s,
            Err(why) => {
                status.error(why);
                return false;
            }
        };
        let anchor = match at {
            Some(p) => Anchor::At(p),
            None => {
                let a = Anchor::Offset(self.edit.paste_count);
                self.edit.paste_count += 1;
                a
            }
        };
        let ids = graph_clip::insert_at(&mut session.project.graph, &sub, anchor);
        let n = ids.len();
        let mut sel = NodeSelection::default();
        sel.replace(ids);
        session.set_node_selection(sel);
        status.info(format!("Pasted {}", pluralize(n)));
        true
    }

    pub fn duplicate_selection(&mut self, session: &mut Session, status: &mut Status) {
        let Some(text) = graph_clip::copy_text(&session.project.graph, session.node_sel.ids()) else {
            status.info("Select nodes first");
            return;
        };
        let n = session.node_sel.len();
        if self.paste_text(Some(&text), None, session, status) {
            // Duplicating again moves on from the new copy, so the count starts over.
            self.edit.paste_count = 0;
            status.info(format!("Duplicated {}", pluralize(n)));
        }
    }

    pub(super) fn recipe_list(&mut self) -> Vec<RecipeInfo> {
        let dir = self.recipes_dir.clone();
        cached_list(&mut self.edit.list, dir.as_deref())
    }

    fn refresh_recipes(&mut self) {
        self.edit.list = None;
    }

    pub fn insert_recipe(&mut self, stem: &str, at: Option<[f32; 2]>, session: &mut Session, status: &mut Status) -> bool {
        let Some(dir) = self.recipes_dir.clone() else { return false };
        match recipes::load(&dir, stem) {
            Ok(sub) => {
                let anchor = at.map_or_else(
                    || {
                        let a = Anchor::Offset(self.edit.paste_count);
                        self.edit.paste_count += 1;
                        a
                    },
                    Anchor::At,
                );
                let ids = graph_clip::insert_at(&mut session.project.graph, &sub, anchor);
                let n = ids.len();
                let mut sel = NodeSelection::default();
                sel.replace(ids);
                session.set_node_selection(sel);
                status.info(format!("Inserted recipe \"{stem}\" ({})", pluralize(n)));
                true
            }
            Err(why) => {
                status.error(why);
                false
            }
        }
    }

    fn save_recipe_now(&mut self, name: &str, sub: &Subgraph) -> Result<PathBuf, String> {
        let dir = self.recipes_dir.clone().ok_or_else(|| "Recipes are not available".to_owned())?;
        let path = recipes::save(&dir, name, sub)?;
        self.refresh_recipes();
        Ok(path)
    }

    /// Runs queued commands and the keyboard / clipboard events of this frame.
    pub(super) fn run_edit_input(&mut self, ctx: &egui::Context, session: &mut Session, status: &mut Status, rect: Rect, xf: &Xform, pointer: Option<Pos2>, graph_focus: bool) {
        let at_pointer = pointer.filter(|p| rect.contains(*p)).map(|p| {
            let g = xf.to_graph(p);
            [g.x, g.y]
        });
        let mut ops = std::mem::take(&mut self.edit.ops);
        if graph_focus && !ctx.text_edit_focused() && self.search.is_none() {
            let events = ctx.input(|i| i.events.clone());
            for ev in events {
                match ev {
                    egui::Event::Copy => ops.push(EditOp::Copy),
                    egui::Event::Cut => ops.push(EditOp::Cut),
                    egui::Event::Paste(text) => {
                        self.paste_text(Some(&text), at_pointer, session, status);
                    }
                    _ => {}
                }
            }
            let c = egui::Modifiers::COMMAND;
            if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(c, egui::Key::A))) {
                ops.push(EditOp::SelectAll);
            }
            if ctx.input_mut(|i| i.consume_shortcut(&egui::KeyboardShortcut::new(c, egui::Key::D))) {
                ops.push(EditOp::Duplicate);
            }
        }
        for op in ops {
            match op {
                EditOp::Copy => {
                    self.copy_selection(session, status);
                }
                EditOp::Cut => self.cut_selection(session, status),
                EditOp::Paste => {
                    let text = self.clipboard_read(None);
                    self.paste_text(text.as_deref(), at_pointer, session, status);
                }
                EditOp::PasteAt(p) => {
                    let text = self.clipboard_read(None);
                    self.paste_text(text.as_deref(), Some(p), session, status);
                }
                EditOp::Duplicate => self.duplicate_selection(session, status),
                EditOp::SelectAll => {
                    let mut sel = session.node_sel.clone();
                    sel.select_all(&session.project.graph);
                    // The previously primary node stays primary.
                    if let Some(p) = session.selected_node {
                        sel.add(p);
                    }
                    session.set_node_selection(sel);
                }
                EditOp::SaveRecipe => {
                    let sub = session.project.graph.extract_subgraph(session.node_sel.ids());
                    if sub.is_empty() {
                        status.info("Select the nodes for the recipe first");
                    } else if self.recipes_dir.is_none() {
                        status.error("Recipes are not available");
                    } else {
                        self.edit.prompt = Some(RecipePrompt { name: String::new(), sub, error: None, focus: true });
                    }
                }
                EditOp::InsertRecipe(stem, at) => {
                    let at = at.or_else(|| Some({
                        let g = xf.to_graph(rect.center());
                        [g.x, g.y]
                    }));
                    self.insert_recipe(&stem, at, session, status);
                }
                EditOp::ManageRecipes => self.manage_open = true,
                EditOp::ImportRecipeFile => self.pick_recipe_file(DialogPurpose::Import),
            }
        }
    }

    fn pick_recipe_file(&mut self, purpose: DialogPurpose) {
        if self.edit.dialog.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let export_name = match &purpose {
            DialogPurpose::Export(stem) => Some(format!("{stem}{}", recipes::RECIPE_EXT)),
            DialogPurpose::Import => None,
        };
        let spawned = std::thread::Builder::new().name("rawgraph-recipe-dialog".into()).spawn(move || {
            let d = rfd::FileDialog::new().add_filter("RawGraph recipe", &["json"]);
            let picked = match export_name {
                Some(name) => d.set_title("Export recipe").set_file_name(name).save_file(),
                None => d.set_title("Import recipe").pick_file(),
            };
            let _ = tx.send(picked);
        });
        if spawned.is_ok() {
            self.edit.dialog = Some(RecipeDialog { purpose, rx });
        }
    }

    fn poll_recipe_dialog(&mut self, status: &mut Status) {
        let Some(d) = &self.edit.dialog else { return };
        let picked = match d.rx.try_recv() {
            Ok(p) => p,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => None,
        };
        let Some(d) = self.edit.dialog.take() else { return };
        let (Some(path), Some(dir)) = (picked, self.recipes_dir.clone()) else { return };
        match d.purpose {
            DialogPurpose::Import => match recipes::import(&dir, &path) {
                Ok(p) => status.info(format!("Imported recipe {}", p.display())),
                Err(why) => status.error(why),
            },
            DialogPurpose::Export(stem) => {
                let mut dest = path;
                if !dest.file_name().is_some_and(|n| n.to_string_lossy().ends_with(recipes::RECIPE_EXT)) {
                    let name = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let stem_only = name.strip_suffix(".json").unwrap_or(&name).to_owned();
                    dest.set_file_name(format!("{stem_only}{}", recipes::RECIPE_EXT));
                }
                match recipes::export(&dir, &stem, &dest) {
                    Ok(()) => status.info(format!("Exported recipe to {}", dest.display())),
                    Err(why) => status.error(why),
                }
            }
        }
        self.refresh_recipes();
    }

    /// The name prompt and the manage window.
    pub(super) fn recipe_windows(&mut self, ctx: &egui::Context, session: &mut Session, status: &mut Status, rect: Rect, xf: &Xform) {
        self.poll_recipe_dialog(status);
        self.save_prompt(ctx, status);
        if !self.manage_open {
            return;
        }
        let list = self.recipe_list();
        let mut open = true;
        let mut insert: Option<String> = None;
        let mut export: Option<String> = None;
        let mut import = false;
        let mut do_delete: Option<String> = None;
        let mut do_rename: Option<(String, String)> = None;
        let dir_text = self.recipes_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
        egui::Window::new("Manage recipes").open(&mut open).resizable(true).default_width(520.0).constrain_to(rect).default_pos(rect.center() - egui::vec2(260.0, 120.0)).show(ctx, |ui| {
            if self.recipes_dir.is_none() {
                ui.label("Recipes are not available.");
                return;
            }
            ui.label(egui::RichText::new(&dir_text).weak().small());
            if let Some((m, err)) = &self.edit.manage.message {
                ui.label(egui::RichText::new(m).color(if *err { egui::Color32::from_rgb(255, 150, 120) } else { egui::Color32::from_gray(200) }));
            }
            ui.add_space(4.0);
            if list.is_empty() {
                ui.label(egui::RichText::new("No recipes yet. Select nodes, right-click one and choose \"Save selection as recipe...\".").weak());
            }
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for r in &list {
                    ui.horizontal(|ui| {
                        let renaming = self.edit.manage.rename.as_ref().is_some_and(|(s, _)| *s == r.stem);
                        let confirming = self.edit.manage.confirm_delete.as_deref() == Some(r.stem.as_str());
                        if renaming {
                            if let Some((_, text)) = &mut self.edit.manage.rename {
                                ui.add(egui::TextEdit::singleline(text).desired_width(180.0));
                            }
                            if ui.button("OK").clicked() {
                                do_rename = self.edit.manage.rename.take();
                            }
                            if ui.button("Cancel").clicked() {
                                self.edit.manage.rename = None;
                            }
                        } else if confirming {
                            ui.label(egui::RichText::new(format!("Delete \"{}\"? This cannot be undone.", r.stem)).color(egui::Color32::from_rgb(255, 190, 120)));
                            if ui.button("Delete").clicked() {
                                do_delete = self.edit.manage.confirm_delete.take();
                            }
                            if ui.button("Keep").clicked() {
                                self.edit.manage.confirm_delete = None;
                            }
                        } else {
                            ui.label(egui::RichText::new(&r.stem).strong());
                            if r.usable {
                                ui.label(egui::RichText::new(pluralize(r.nodes)).weak());
                            } else {
                                ui.label(egui::RichText::new(format!("unusable: {}", r.note)).color(egui::Color32::from_rgb(255, 150, 120)));
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Delete").clicked() {
                                    self.edit.manage.confirm_delete = Some(r.stem.clone());
                                }
                                if ui.add_enabled(r.usable, egui::Button::new("Export...")).clicked() {
                                    export = Some(r.stem.clone());
                                }
                                if ui.button("Rename").clicked() {
                                    self.edit.manage.rename = Some((r.stem.clone(), r.stem.clone()));
                                }
                                if ui.add_enabled(r.usable, egui::Button::new("Insert")).on_hover_text("Adds the nodes in the middle of the editor, not connected").clicked() {
                                    insert = Some(r.stem.clone());
                                }
                            });
                        }
                    });
                }
            });
            ui.separator();
            if ui.button("Import recipe file...").clicked() {
                import = true;
            }
        });
        if !open {
            self.manage_open = false;
            self.edit.manage = ManageUi::default();
        }
        let Some(dir) = self.recipes_dir.clone() else { return };
        if let Some((from, to)) = do_rename {
            self.edit.manage.message = Some(match recipes::rename(&dir, &from, &to) {
                Ok(p) => (format!("Renamed to {}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()), false),
                Err(why) => (why, true),
            });
            self.refresh_recipes();
        }
        if let Some(stem) = do_delete {
            self.edit.manage.message = Some(match recipes::delete(&dir, &stem) {
                Ok(()) => (format!("Deleted \"{stem}\""), false),
                Err(why) => (why, true),
            });
            self.refresh_recipes();
        }
        if let Some(stem) = insert {
            let g = xf.to_graph(rect.center());
            if self.insert_recipe(&stem, Some([g.x, g.y]), session, status) {
                self.edit.manage.message = Some((format!("Inserted \"{stem}\""), false));
            }
        }
        if let Some(stem) = export {
            self.pick_recipe_file(DialogPurpose::Export(stem));
        }
        if import {
            self.pick_recipe_file(DialogPurpose::Import);
        }
    }

    fn save_prompt(&mut self, ctx: &egui::Context, status: &mut Status) {
        let Some(prompt) = &mut self.edit.prompt else { return };
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Save selection as recipe").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
            ui.label(format!("{} will be saved. Per-image values are not part of a recipe.", pluralize(prompt.sub.node_count())));
            let edit = ui.add(egui::TextEdit::singleline(&mut prompt.name).hint_text("Recipe name").desired_width(280.0));
            if prompt.focus {
                edit.request_focus();
                prompt.focus = false;
            }
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                save = true;
            }
            if let Some(e) = &prompt.error {
                ui.label(egui::RichText::new(e).color(egui::Color32::from_rgb(255, 150, 120)));
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
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.edit.prompt = None;
            return;
        }
        if save {
            let (name, sub) = match &self.edit.prompt {
                Some(p) => (p.name.clone(), p.sub.clone()),
                None => return,
            };
            match self.save_recipe_now(&name, &sub) {
                Ok(p) => {
                    status.info(format!("Saved recipe {}", p.display()));
                    self.edit.prompt = None;
                }
                Err(why) => {
                    if let Some(p) = &mut self.edit.prompt {
                        p.error = Some(why);
                    }
                }
            }
        }
    }
}
