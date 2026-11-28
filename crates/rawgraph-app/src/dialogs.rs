//! Small windows of the library and safety-net features: crash recovery, XMP
//! import, relinking and clearing the thumbnail cache. They only draw and
//! report what the user chose; the app carries it out.

use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText};
use rawgraph_core::ImageId;
use rawgraph_project::RecoveryInfo;

use crate::clock::local_stamp;
use crate::relink::FolderPreview;
use crate::xmp_import::XmpDialog;

fn warn(text: &str) -> RichText {
    RichText::new(text).color(Color32::from_rgb(255, 190, 120))
}

// ---------------------------------------------------------------- recovery

pub struct RecoveryDialog {
    pub items: Vec<RecoveryInfo>,
    /// The file whose Discard is waiting for the second click.
    pub confirm: Option<PathBuf>,
}

#[derive(Debug, PartialEq)]
pub enum RecoveryAction {
    None,
    Recover(PathBuf),
    Discard(PathBuf),
    Later,
}

/// "job" for `D:\x\job.rawgraph`, "Unsaved project" without a path.
pub fn recovery_title(info: &RecoveryInfo) -> String {
    info.project_path.as_ref().and_then(|p| p.file_stem()).map_or_else(|| "Unsaved project".to_owned(), |s| s.to_string_lossy().into_owned())
}

pub fn recovery_window(ctx: &egui::Context, d: &mut RecoveryDialog) -> RecoveryAction {
    let mut action = RecoveryAction::None;
    egui::Window::new("Recover unsaved work").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.set_min_width(520.0);
        ui.label("RawGraph did not close properly last time. These projects had unsaved changes that were kept automatically.");
        ui.label(RichText::new("Recovering opens a copy as an unsaved project; the project file on disk is not touched until you save.").weak());
        ui.add_space(6.0);
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for info in &d.items {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(recovery_title(info)).strong());
                            let place = info.project_path.as_ref().map_or_else(|| "never saved".to_owned(), |p| p.display().to_string());
                            ui.label(RichText::new(format!("{}   |   {} image{}   |   {place}", local_stamp(info.saved_unix_ms), info.image_count, if info.image_count == 1 { "" } else { "s" })).weak().small());
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if d.confirm.as_ref() == Some(&info.file) {
                                if ui.button("Cancel").clicked() {
                                    d.confirm = None;
                                }
                                if ui.button(warn("Discard for good")).clicked() {
                                    action = RecoveryAction::Discard(info.file.clone());
                                }
                                ui.label(warn("Delete this copy?"));
                            } else {
                                if ui.button("Discard").clicked() {
                                    d.confirm = Some(info.file.clone());
                                }
                                if ui.button("Recover").clicked() {
                                    action = RecoveryAction::Recover(info.file.clone());
                                }
                            }
                        });
                    });
                });
            }
        });
        ui.add_space(4.0);
        if ui.button("Later").on_hover_text("Ask again at the next start").clicked() {
            action = RecoveryAction::Later;
        }
    });
    action
}

// --------------------------------------------------------------------- XMP

#[derive(Debug, PartialEq, Eq)]
pub enum XmpAction {
    None,
    Import,
    Cancel,
    Close,
}

/// `running` is `(done, total)` while the files are being read.
pub fn xmp_window(ctx: &egui::Context, area: egui::Rect, d: &mut XmpDialog, running: Option<(usize, usize)>) -> XmpAction {
    let mut action = XmpAction::None;
    let mut open = true;
    egui::Window::new("Import ratings and keywords from XMP").open(&mut open).collapsible(false).resizable(false).constrain_to(area).default_pos(area.center() - egui::vec2(220.0, 150.0)).show(ctx, |ui| {
        ui.set_min_width(440.0);
        ui.label("Reads star ratings, reject marks, colour labels and keywords. The image files and their XMP sidecars are only read, never changed.");
        ui.add_space(4.0);
        ui.add_enabled_ui(running.is_none(), |ui| {
            ui.checkbox(&mut d.opts.sidecars, "Sidecar files (.xmp)");
            ui.checkbox(&mut d.opts.embedded, "Embedded in the files");
            ui.checkbox(&mut d.opts.only_unrated, "Only images I have not rated here");
            ui.checkbox(&mut d.opts.merge_keywords, "Merge keywords").on_hover_text("Adds the keywords of dc:subject and lr:hierarchicalSubject to the images; keywords already there stay");
        });
        ui.label(RichText::new("When an image has both, the sidecar wins for the rating; keywords of both are merged.").weak().small());
        ui.add_space(6.0);
        match running {
            Some((done, total)) => {
                ui.add(egui::ProgressBar::new(if total == 0 { 0.0 } else { done as f32 / total as f32 }).desired_width(380.0).text(format!("Reading {done}/{total}")));
                if ui.button("Cancel").on_hover_text("Stops reading; nothing is changed").clicked() {
                    action = XmpAction::Cancel;
                }
            }
            None => {
                ui.horizontal(|ui| {
                    let any = d.opts.sidecars || d.opts.embedded;
                    if ui.add_enabled(any, egui::Button::new("Import")).clicked() {
                        action = XmpAction::Import;
                    }
                    if ui.button("Close").clicked() {
                        action = XmpAction::Close;
                    }
                });
            }
        }
        if let Some(m) = &d.message {
            ui.add_space(4.0);
            ui.label(warn(m));
        }
        if let Some(s) = &d.summary {
            ui.separator();
            egui::Grid::new("xmp_summary").num_columns(2).spacing([16.0, 3.0]).show(ui, |ui| {
                let rows = [("Changed", s.changed), ("Unchanged", s.unchanged), ("Without XMP", s.without_xmp), ("Errors", s.errors)];
                for (name, n) in rows {
                    ui.label(name);
                    let text = RichText::new(n.to_string()).monospace();
                    ui.label(if name == "Errors" && n > 0 { text.color(Color32::from_rgb(255, 150, 120)) } else { text });
                    ui.end_row();
                }
            });
            let mut notes = Vec::new();
            if s.skipped_rated > 0 {
                notes.push(format!("{} already rated here (skipped)", s.skipped_rated));
            }
            notes.push(format!("{} from sidecars, {} embedded", s.from_sidecar, s.from_embedded));
            if let Some(ms) = d.elapsed_ms {
                notes.push(format!("{ms} ms"));
            }
            ui.label(RichText::new(notes.join("   |   ")).weak().small());
        }
    });
    if !open && running.is_none() {
        action = XmpAction::Close;
    }
    action
}

// ----------------------------------------------------------------- relink

pub enum RelinkUi {
    None,
    /// The chosen file differs from the original.
    Mismatch { id: ImageId, path: PathBuf, message: String, confirm: bool },
    Folder { old: String, new: String, preview: Option<FolderPreview>, error: Option<String> },
}

#[derive(Debug, PartialEq)]
pub enum RelinkAction {
    None,
    UseAnyway(ImageId, PathBuf),
    Browse,
    Check,
    Apply,
    Close,
}

pub fn relink_window(ctx: &egui::Context, area: egui::Rect, state: &mut RelinkUi) -> RelinkAction {
    let mut action = RelinkAction::None;
    match state {
        RelinkUi::None => {}
        RelinkUi::Mismatch { id, path, message, confirm } => {
            egui::Window::new("This is not the same file").collapsible(false).resizable(false).constrain_to(area).default_pos(area.center() - egui::vec2(260.0, 100.0)).show(ctx, |ui| {
                ui.set_max_width(520.0);
                ui.label(message.as_str());
                ui.label(RichText::new(path.display().to_string()).weak().small());
                ui.add_space(6.0);
                if *confirm {
                    ui.label(warn("Use it anyway? The image keeps its rating and edits but now stands for this file."));
                    ui.horizontal(|ui| {
                        if ui.button(warn("Yes, use this file")).clicked() {
                            action = RelinkAction::UseAnyway(*id, path.clone());
                        }
                        if ui.button("No").clicked() {
                            action = RelinkAction::Close;
                        }
                    });
                } else {
                    ui.horizontal(|ui| {
                        if ui.button("Use anyway").clicked() {
                            *confirm = true;
                        }
                        if ui.button("Cancel").clicked() {
                            action = RelinkAction::Close;
                        }
                    });
                }
            });
        }
        RelinkUi::Folder { old, new, preview, error } => {
            egui::Window::new("Relink folder").collapsible(false).resizable(false).constrain_to(area).default_pos(area.center() - egui::vec2(280.0, 140.0)).show(ctx, |ui| {
                ui.set_min_width(640.0);
                ui.label("The folder moved. Point the images below the old folder at the same files in the new place. Nothing on disk changes.");
                ui.add_space(4.0);
                egui::Grid::new("relink_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Old folder");
                    ui.add(egui::TextEdit::singleline(old).desired_width(500.0));
                    ui.end_row();
                    ui.label("New location");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(new).desired_width(410.0));
                        if ui.button("Browse...").clicked() {
                            action = RelinkAction::Browse;
                        }
                    });
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!new.trim().is_empty() && !old.trim().is_empty(), egui::Button::new("Check")).clicked() {
                        action = RelinkAction::Check;
                    }
                    if let Some(p) = preview {
                        let text = format!("{} of {} missing files found there", p.missing_found, p.missing_total);
                        ui.label(RichText::new(text).strong().color(if p.missing_found > 0 { Color32::from_rgb(130, 220, 150) } else { Color32::from_rgb(255, 150, 120) }));
                    }
                });
                if let Some(p) = preview {
                    if p.also_moved > 0 {
                        ui.label(warn(&format!("{} more image(s) below the old folder whose file is not missing would be re-pointed too.", p.also_moved)));
                    }
                }
                if let Some(e) = error {
                    ui.label(RichText::new(e.as_str()).color(Color32::from_rgb(255, 150, 120)));
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let ready = preview.as_ref().is_some_and(|p| p.missing_found > 0 || p.also_moved > 0);
                    if ui.add_enabled(ready, egui::Button::new("Relink")).on_hover_text("Run Check first").clicked() {
                        action = RelinkAction::Apply;
                    }
                    if ui.button("Cancel").clicked() {
                        action = RelinkAction::Close;
                    }
                });
            });
        }
    }
    action
}

// ------------------------------------------------------------ thumbnail cache

/// `Some(true)` = clear, `Some(false)` = cancel.
pub fn clear_cache_window(ctx: &egui::Context, size_text: &str) -> Option<bool> {
    let mut answer = None;
    egui::Window::new("Clear thumbnail cache").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.label(format!("Delete the thumbnails kept on disk ({size_text})?"));
        ui.label(RichText::new("They are made again when the filmstrip needs them. Your photos are not touched.").weak());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Clear").clicked() {
                answer = Some(true);
            }
            if ui.button("Cancel").clicked() {
                answer = Some(false);
            }
        });
    });
    answer
}

#[derive(Debug, PartialEq)]
pub enum UnfinishedAction {
    None,
    Recover,
    Cancel,
}

/// A project file has the journal of an interrupted save beside it and sits on a network drive or in a
/// sync folder, where another computer may be saving it right now. Plain words, two buttons.
pub fn unfinished_window(ctx: &egui::Context, file: &Path, error: Option<&str>) -> UnfinishedAction {
    let mut action = UnfinishedAction::None;
    egui::Window::new("Unfinished save").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
        ui.set_min_width(520.0);
        ui.label(RichText::new(file.display().to_string()).strong());
        ui.add_space(4.0);
        ui.label("A save of this project was interrupted (the program or the computer stopped while it was writing), or another computer is saving it right now. The project cannot be read until that is cleared up.");
        ui.label("\"Recover unfinished save\" rolls the file back to its last complete state. Your last save may be lost; nothing else is.");
        ui.label(warn("Do this only if no other computer is saving this project right now."));
        if let Some(e) = error {
            ui.add_space(4.0);
            ui.label(warn(e));
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Recover unfinished save").clicked() {
                action = UnfinishedAction::Recover;
            }
            if ui.button("Cancel").clicked() {
                action = UnfinishedAction::Cancel;
            }
        });
    });
    action
}
