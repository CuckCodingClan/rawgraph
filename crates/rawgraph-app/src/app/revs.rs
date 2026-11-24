//! Revisions in the window: the outcome of a save, the questions of a conflict, the History
//! window and the project line of the status bar. The decisions themselves are in
//! `save_flow.rs` and `revisions.rs`; this only draws them and carries out their results.

use super::*;

use crate::revisions;
use crate::save_flow::{Outcome, Stage};

/// State of the History window.
#[derive(Default)]
pub(super) struct HistoryUi {
    pub open: bool,
    pub selected: Option<u64>,
    /// (revision, graph changes seen, lines): the comparison, made again only when one of them changes.
    cache: Option<(u64, u64, Vec<String>)>,
}

impl App {
    /// Carries out what a save attempt led to.
    pub(super) fn after_save(&mut self, outcome: Outcome<Pending>, ctx: &egui::Context) {
        match outcome {
            Outcome::Saved { revision, target, then } => {
                self.discard_own_recovery();
                self.status.info(format!("Saved {} (revision {revision})", target.display()));
                if let Some(p) = then {
                    self.execute(p, ctx);
                }
            }
            Outcome::Asked => {}
            Outcome::Failed(text) => {
                if !text.is_empty() {
                    self.status.error(text);
                }
            }
        }
    }

    /// `--save-project`: saves as a new revision; with `simulate`, somebody else saves first.
    pub(super) fn cli_save_project(&mut self, path: &std::path::Path, simulate: bool) {
        if simulate {
            match Project::load(path) {
                Ok(mut other) => {
                    other.graph.add_node(NodeKind::Contrast, [900.0, 420.0]);
                    match other.commit(path, "1 node added") {
                        Ok(r) => eprintln!("--simulate-conflict: another copy saved revision {r} of {}", path.display()),
                        Err(e) => eprintln!("--simulate-conflict: {e}"),
                    }
                }
                Err(e) => eprintln!("--simulate-conflict: cannot read {}: {e}", path.display()),
            }
        }
        match self.save_flow.save(&mut self.session, path, None) {
            Outcome::Saved { revision, .. } => {
                self.discard_own_recovery();
                eprintln!("project saved: {} (revision {revision})", path.display());
            }
            Outcome::Asked => {
                let what = match &self.save_flow.stage {
                    Stage::Conflict { disk_revision, loaded_revision, .. } => format!("conflict: revision {disk_revision} on disk, this copy is based on revision {loaded_revision}"),
                    _ => "the target belongs to another project".to_owned(),
                };
                eprintln!("--save-project: nothing was written ({what}); the window asks how to go on");
            }
            Outcome::Failed(e) => {
                eprintln!("--save-project: {e}");
                EXIT_CODE.store(1, Ordering::SeqCst);
            }
        }
    }

    /// Project name, revision and "unsaved" at the left end of the status bar.
    pub(super) fn project_status(&self, ui: &mut egui::Ui) {
        if self.session.demo {
            return;
        }
        ui.label(RichText::new(self.session.project_name()).strong());
        let rev = self.session.project.revision();
        ui.label(RichText::new(format!("rev {rev}")).weak()).on_hover_text(if rev == 0 { "Not saved as a revision yet" } else { "The revision this copy is based on" });
        if self.session.dirty {
            ui.label(RichText::new("unsaved").color(egui::Color32::from_rgb(255, 190, 90)));
        }
        ui.separator();
    }

    /// The questions of a conflicting save.
    pub(super) fn save_flow_window(&mut self, ctx: &egui::Context) {
        #[derive(Clone, Copy)]
        enum Choice {
            Copy,
            Overwrite,
            Confirm,
            Cancel,
        }
        let mut choice = None;
        let area = self.window_area;
        let shown = |title: &str| {
            let w = egui::Window::new(title.to_owned()).collapsible(false).resizable(false).default_width(440.0);
            if area.is_positive() {
                w.constrain_to(area).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            } else {
                w.anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            }
        };
        match &self.save_flow.stage {
            Stage::Idle => return,
            Stage::Conflict { disk_revision, loaded_revision, .. } => {
                shown("Project changed by someone else").show(ctx, |ui| {
                    ui.label(format!("This project was saved by someone else since you opened it (revision {disk_revision} on disk, you opened revision {loaded_revision})"));
                    ui.add_space(4.0);
                    ui.label(RichText::new("Nothing has been written. Save your version as a separate file, or replace theirs.").weak());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save as copy...").on_hover_text("Keeps both files; your version goes to a new file").clicked() {
                            choice = Some(Choice::Copy);
                        }
                        if ui.button("Overwrite their changes").clicked() {
                            choice = Some(Choice::Overwrite);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(Choice::Cancel);
                        }
                    });
                });
            }
            Stage::ConfirmOverwrite { lost, .. } => {
                shown("Overwrite their changes?").show(ctx, |ui| {
                    ui.label(format!("This replaces {lost} with your version. It cannot be undone."));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Overwrite").clicked() {
                            choice = Some(Choice::Confirm);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(Choice::Cancel);
                        }
                    });
                });
            }
            Stage::ConfirmReplace { target, .. } => {
                let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| target.display().to_string());
                shown("Replace the other project?").show(ctx, |ui| {
                    ui.label(format!("Replace the other project {name}?"));
                    ui.add_space(4.0);
                    ui.label(RichText::new("That file belongs to a different project. Saving here replaces it, and its content is lost.").weak());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Replace").clicked() {
                            choice = Some(Choice::Confirm);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(Choice::Cancel);
                        }
                    });
                });
            }
        }
        match choice {
            Some(Choice::Copy) => {
                let date = crate::clock::date_label(crate::clock::now_unix_ms(), crate::clock::local_offset_secs());
                let name = self.session.project_name();
                if let Some((file, then)) = self.save_flow.choose_copy(&name, &date) {
                    self.save_as_name = Some(file);
                    self.open_dialog(DialogKind::SaveAs(then));
                }
            }
            Some(Choice::Overwrite) => self.save_flow.choose_overwrite(),
            Some(Choice::Confirm) => {
                let outcome = self.save_flow.confirm(&mut self.session);
                self.after_save(outcome, ctx);
            }
            Some(Choice::Cancel) => {
                self.save_flow.cancel();
                self.status.info("Not saved; the project still has unsaved changes");
            }
            None => {}
        }
    }

    /// File > History...
    pub(super) fn history_window(&mut self, ctx: &egui::Context) {
        if !self.history_ui.open {
            return;
        }
        let rows = revisions::rows(&self.session.project);
        let selected_row = self.history_ui.selected.and_then(|r| rows.iter().find(|row| row.revision == r)).cloned();
        if selected_row.is_none() {
            self.history_ui.selected = None;
        }
        // The comparison is read from the project only when the selection or the graph changed.
        let changes = self.history.changes();
        let wanted = selected_row.as_ref().filter(|r| r.has_graph).map(|r| r.revision);
        if let Some(rev) = wanted {
            let stale = self.history_ui.cache.as_ref().is_none_or(|(r, c, _)| *r != rev || *c != changes);
            if stale {
                let ids: Vec<ImageId> = self.session.project.images().iter().map(|e| e.id).collect();
                let lines = match self.session.project.graph_at(rev) {
                    Some(g) => revisions::compare_lines(&self.session.project.graph, &g, &ids),
                    None => vec!["The graph of this revision could not be read.".to_owned()],
                };
                self.history_ui.cache = Some((rev, changes, lines));
            }
        }
        let lines: Vec<String> = match (wanted, &self.history_ui.cache) {
            (Some(rev), Some((r, _, l))) if *r == rev => l.clone(),
            _ => Vec::new(),
        };
        let mut open = true;
        let mut clicked: Option<u64> = None;
        let mut restore = false;
        let area = self.window_area;
        let window = egui::Window::new("History").open(&mut open).default_width(800.0).collapsible(false);
        let window = if area.is_positive() { window.constrain_to(area).default_pos(area.center() - egui::vec2(400.0, 230.0)) } else { window };
        window.show(ctx, |ui| {
            if rows.is_empty() {
                ui.label("No revisions yet. Every Save adds one to the project file.");
                return;
            }
            egui::ScrollArea::vertical().max_height(230.0).auto_shrink([false, true]).show(ui, |ui| {
                egui::Grid::new("history_grid").num_columns(5).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
                    for h in ["Revision", "Date", "Author", "Device", "Summary"] {
                        ui.label(RichText::new(h).weak());
                    }
                    ui.end_row();
                    for row in &rows {
                        let sel = self.history_ui.selected == Some(row.revision);
                        if ui.selectable_label(sel, row.revision.to_string()).clicked() {
                            clicked = Some(row.revision);
                        }
                        ui.label(&row.when);
                        ui.label(&row.author);
                        ui.label(&row.device);
                        if ui.selectable_label(sel, row.summary_text()).clicked() {
                            clicked = Some(row.revision);
                        }
                        ui.end_row();
                    }
                });
            });
            ui.separator();
            match &selected_row {
                None => {
                    ui.label(RichText::new("Select a revision to compare it with the current graph.").weak());
                }
                Some(row) => {
                    ui.label(RichText::new(format!("Revision {} compared with the current graph", row.revision)).strong());
                    if row.has_graph {
                        for l in &lines {
                            ui.label(l);
                        }
                    } else {
                        ui.label("The graph of this revision was not kept, so it cannot be restored.");
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.add_enabled(row.has_graph, egui::Button::new("Restore this graph")).on_hover_text("Replaces the current graph; Undo brings the previous one back. Ratings, flags, labels and the image list are not touched.").clicked() {
                            restore = true;
                        }
                    });
                }
            }
        });
        if let Some(r) = clicked {
            self.history_ui.selected = Some(r);
        }
        if restore {
            if let Some(row) = &selected_row {
                match revisions::restore_graph(&mut self.session, &mut self.history, row.revision) {
                    Ok(true) => {
                        self.editor.request_frame_all();
                        self.status.info(format!("Restored the graph of revision {}; Undo brings the previous graph back", row.revision));
                    }
                    Ok(false) => self.status.info("That graph is the same as the current one"),
                    Err(e) => self.status.error(e),
                }
            }
        }
        if !open {
            self.history_ui.open = false;
        }
    }
}
