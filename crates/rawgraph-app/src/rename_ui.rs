//! The "Rename files" window: scope, caption sheet, naming rules, the full preview table with its
//! conflicts, the block-boundary check and the buttons. It only draws and reports what the user chose;
//! `app/rename.rs` builds the plan and carries it out.

use std::ops::Range;

use eframe::egui::{self, Color32, RichText};
use rawgraph_core::ImageId;
use rawgraph_rename::{BlockReport, CaseChange, EntryKind, ExtCase, Plan};

use crate::rename_ops::{RuleFields, SheetFields};

const RED: Color32 = Color32::from_rgb(255, 120, 100);
const AMBER: Color32 = Color32::from_rgb(255, 190, 120);
const GREEN: Color32 = Color32::from_rgb(130, 220, 150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameScope {
    /// Every image the filmstrip lists (the current filter).
    Listed,
    /// Only the selected image.
    Selected,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RenameAction {
    None,
    Close,
    BrowseSheet,
    LoadSheet,
    ClearSheet,
    /// Run the rename (after the confirmation click).
    Run,
    Undo,
    Recover,
    /// Make a new plan (the scope changed).
    Rebuild,
}

/// All state of the window. Kept only while it is open.
pub struct RenameUi {
    pub scope: RenameScope,
    pub ids: Vec<ImageId>,
    pub rules: RuleFields,
    pub sheet: SheetFields,
    /// Result of the last "Load sheet": text and whether it is an error.
    pub sheet_status: Option<(bool, String)>,
    pub plan: Option<Plan>,
    pub plan_error: Option<String>,
    pub only_problems: bool,
    /// The rename button was clicked once; the second click runs it.
    pub confirm: bool,
    pub message: Option<(bool, String)>,
    pub blocks: Option<BlockReport>,
    pub blocks_open: bool,
    /// Block rows currently on screen (the app asks for their thumbnails).
    pub visible_blocks: Range<usize>,
    /// (an undo is possible, interrupted runs in the journal).
    pub log: (bool, usize),
    /// The inputs the current plan was made from.
    snapshot: Option<(RuleFields, RenameScope, u64)>,
    pub plan_ms: Option<u128>,
}

impl RenameUi {
    pub fn new(scope: RenameScope, ids: Vec<ImageId>) -> Self {
        RenameUi {
            scope,
            ids,
            rules: RuleFields::default(),
            sheet: SheetFields::default(),
            sheet_status: None,
            plan: None,
            plan_error: None,
            only_problems: false,
            confirm: false,
            message: None,
            blocks: None,
            blocks_open: false,
            visible_blocks: 0..0,
            log: (false, 0),
            snapshot: None,
            plan_ms: None,
        }
    }

    /// Whether the inputs differ from those of the current plan (then a new plan is made).
    pub fn needs_plan(&self, sheet_generation: u64) -> bool {
        self.snapshot.as_ref() != Some(&(self.rules.clone(), self.scope, sheet_generation))
    }

    pub fn mark_planned(&mut self, sheet_generation: u64) {
        self.snapshot = Some((self.rules.clone(), self.scope, sheet_generation));
        self.confirm = false;
    }

    /// Forces a new plan on the next frame.
    pub fn invalidate(&mut self) {
        self.snapshot = None;
        self.confirm = false;
    }
}

fn case_label(c: CaseChange) -> &'static str {
    match c {
        CaseChange::None => "Keep case",
        CaseChange::Lower => "lower case",
        CaseChange::Upper => "UPPER CASE",
        CaseChange::Title => "Title Case",
    }
}

fn ext_label(c: ExtCase) -> &'static str {
    match c {
        ExtCase::Keep => "Keep extension case",
        ExtCase::Lower => "Extension .jpg",
        ExtCase::Upper => "Extension .JPG",
    }
}

/// What the app tells the window about the rest of the program.
pub struct RenameInfo<'a> {
    /// Why nothing may run now (scan, export), if so.
    pub busy: Option<&'a str>,
    pub sheet_loaded: bool,
    pub selected_available: bool,
    pub listed: usize,
    pub project_name: &'a str,
}

/// Draws the window.
pub fn rename_window(ctx: &egui::Context, area: egui::Rect, st: &mut RenameUi, info: &RenameInfo) -> RenameAction {
    let mut action = RenameAction::None;
    let mut open = true;
    egui::Window::new("Rename files").open(&mut open).collapsible(false).resizable(true).constrain_to(area).default_pos(area.min + egui::vec2(60.0, 30.0)).default_size([920.0, 640.0]).show(ctx, |ui| {
        ui.label(RichText::new("Renames the files on disk, including RAW originals and their .xmp sidecars. Nothing happens before you confirm; the full list below is checked for conflicts first.").weak());
        ui.add_space(4.0);
        scope_row(ui, st, info, &mut action);
        ui.separator();
        sheet_section(ui, st, info, &mut action);
        ui.separator();
        naming_section(ui, st, info);
        ui.separator();
        buttons_row(ui, st, info, &mut action);
        if let Some((error, m)) = &st.message {
            ui.label(RichText::new(m.as_str()).color(if *error { RED } else { GREEN }));
        }
        ui.separator();
        preview_table(ui, st);
    });
    if !open {
        action = RenameAction::Close;
    }
    action
}

fn scope_row(ui: &mut egui::Ui, st: &mut RenameUi, info: &RenameInfo, action: &mut RenameAction) {
    ui.horizontal(|ui| {
        ui.label("Files:");
        let before = st.scope;
        ui.radio_value(&mut st.scope, RenameScope::Listed, format!("All {} listed in the filmstrip", info.listed));
        let only = ui.add_enabled(info.selected_available, egui::RadioButton::new(st.scope == RenameScope::Selected, "Only the selected image"));
        if only.clicked() {
            st.scope = RenameScope::Selected;
        }
        if st.scope != before {
            *action = RenameAction::Rebuild;
        }
        ui.label(RichText::new(format!("Project: {}", info.project_name)).weak());
    });
}

fn sheet_section(ui: &mut egui::Ui, st: &mut RenameUi, info: &RenameInfo, action: &mut RenameAction) {
    egui::CollapsingHeader::new(if info.sheet_loaded { "Caption sheet (loaded)" } else { "Caption sheet" }).default_open(info.sheet_loaded).show(ui, |ui| {
        ui.label(RichText::new("An .xlsx or .csv file: column A = photo number, B = target name (076_Anna Berger), C = caption. The photo number is read from each file name.").weak().small());
        ui.horizontal(|ui| {
            ui.label("File");
            ui.add(egui::TextEdit::singleline(&mut st.sheet.path).desired_width(420.0));
            if ui.button("Browse...").clicked() {
                *action = RenameAction::BrowseSheet;
            }
        });
        ui.horizontal(|ui| {
            ui.label("Columns: number");
            ui.add(egui::TextEdit::singleline(&mut st.sheet.col_number).desired_width(28.0));
            ui.label("name");
            ui.add(egui::TextEdit::singleline(&mut st.sheet.col_name).desired_width(28.0));
            ui.label("caption");
            ui.add(egui::TextEdit::singleline(&mut st.sheet.col_caption).desired_width(28.0));
            ui.label("sheet tab");
            ui.add(egui::TextEdit::singleline(&mut st.sheet.tab).desired_width(90.0).hint_text("first"));
        });
        ui.horizontal(|ui| {
            ui.label("Number in the file name")
                .on_hover_text("last = the last group of digits (Job-0076 = 76), first = the first group, or a regular expression whose first group is the number, e.g. -(\\d{4})_");
            ui.add(egui::TextEdit::singleline(&mut st.rules.pattern).desired_width(160.0));
            if ui.add_enabled(!st.sheet.path.trim().is_empty(), egui::Button::new("Load sheet")).clicked() {
                *action = RenameAction::LoadSheet;
            }
            if info.sheet_loaded && ui.button("Remove sheet").clicked() {
                *action = RenameAction::ClearSheet;
            }
            if info.sheet_loaded && ui.button("Use {sheet_name}").on_hover_text("Name the files after column B of the sheet").clicked() {
                st.rules.template = "{sheet_name}".to_owned();
            }
        });
        if let Some((error, m)) = &st.sheet_status {
            ui.label(RichText::new(m.as_str()).color(if *error { RED } else { GREEN }));
        }
    });
}

fn naming_section(ui: &mut egui::Ui, st: &mut RenameUi, info: &RenameInfo) {
    let _ = info;
    egui::Grid::new("rename_rules").num_columns(2).spacing([10.0, 5.0]).show(ui, |ui| {
        ui.label("Name template");
        ui.add(egui::TextEdit::singleline(&mut st.rules.template).desired_width(420.0)).on_hover_text(
            "{stem} original name, {counter} / {counter:4}, {date} {time} {datetime} (capture time), {camera}, {no} / {no:03} photo number, {sheet_name} {caption} (caption sheet)",
        );
        ui.end_row();
        ui.label("Counter");
        ui.horizontal(|ui| {
            ui.label("start");
            ui.add(egui::DragValue::new(&mut st.rules.counter_start).range(0..=999_999_999));
            ui.label("step");
            ui.add(egui::DragValue::new(&mut st.rules.counter_step).range(-1000..=1000));
            ui.label("digits");
            ui.add(egui::DragValue::new(&mut st.rules.counter_pad).range(0..=12));
        });
        ui.end_row();
        ui.label("Find / replace");
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut st.rules.find).desired_width(170.0).hint_text("find"));
            ui.label("\u{2192}");
            ui.add(egui::TextEdit::singleline(&mut st.rules.replace).desired_width(170.0).hint_text("replace with"));
            ui.checkbox(&mut st.rules.regex, "regex").on_hover_text("The find text is a regular expression; ${1} in the replacement is its first group");
            ui.checkbox(&mut st.rules.case_sensitive, "match case");
        });
        ui.end_row();
        ui.label("Case");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("rename-case").selected_text(case_label(st.rules.case)).show_ui(ui, |ui| {
                for c in [CaseChange::None, CaseChange::Lower, CaseChange::Upper, CaseChange::Title] {
                    ui.selectable_value(&mut st.rules.case, c, case_label(c));
                }
            });
            egui::ComboBox::from_id_salt("rename-ext").selected_text(ext_label(st.rules.ext_case)).show_ui(ui, |ui| {
                for c in [ExtCase::Keep, ExtCase::Lower, ExtCase::Upper] {
                    ui.selectable_value(&mut st.rules.ext_case, c, ext_label(c));
                }
            });
            ui.checkbox(&mut st.rules.pairs, "Rename same-name files (RAW + JPEG) together");
        });
        ui.end_row();
    });
}

fn buttons_row(ui: &mut egui::Ui, st: &mut RenameUi, info: &RenameInfo, action: &mut RenameAction) {
    let blocked = st.plan.as_ref().is_none_or(|p| p.blocked());
    let changes = st.plan.as_ref().map_or(0, |p| p.change_count());
    ui.horizontal(|ui| {
        if let Some(p) = &st.plan {
            let main = p.entries.iter().filter(|e| e.kind == EntryKind::Main).count();
            let extra = p.entries.len() - main;
            let mut text = format!("{main} file(s)");
            if extra > 0 {
                text.push_str(&format!(" + {extra} sidecar/pair file(s)"));
            }
            text.push_str(&format!(", {changes} will be renamed"));
            ui.label(text);
            if p.blocked() {
                ui.label(RichText::new(format!("{} with conflicts - fix them to continue", p.problem_count())).color(RED).strong());
            } else if changes == 0 {
                ui.label(RichText::new("no name changes").weak());
            } else {
                ui.label(RichText::new("no conflicts").color(GREEN));
            }
        }
        if let Some(e) = &st.plan_error {
            ui.label(RichText::new(e.as_str()).color(RED));
        }
    });
    ui.horizontal(|ui| {
        let can_run = !blocked && changes > 0 && info.busy.is_none();
        if st.confirm {
            ui.label(RichText::new(format!("Rename {changes} file(s) on disk now? Undo is kept in the rename log.")).color(AMBER));
            if ui.add_enabled(can_run, egui::Button::new(RichText::new("Yes, rename").strong())).clicked() {
                *action = RenameAction::Run;
            }
            if ui.button("Cancel").clicked() {
                st.confirm = false;
            }
        } else {
            let b = ui.add_enabled(can_run, egui::Button::new(format!("Rename {changes} file(s)...")));
            if b.clicked() {
                st.confirm = true;
            }
            if let Some(why) = info.busy {
                b.on_disabled_hover_text(why);
            }
        }
        ui.separator();
        if ui.add_enabled(st.log.0 && info.busy.is_none(), egui::Button::new("Undo last rename")).on_hover_text("Puts the files of the last rename back under their old names (same safe method, with checks)").clicked() {
            *action = RenameAction::Undo;
        }
        if st.log.1 > 0 && ui.button(RichText::new(format!("Recover {} interrupted rename(s)", st.log.1)).color(AMBER)).on_hover_text("The program stopped in the middle of a rename; this puts those files back").clicked() {
            *action = RenameAction::Recover;
        }
        ui.separator();
        if ui.add_enabled(info.sheet_loaded && st.plan.is_some(), egui::Button::new("Block check...")).on_hover_text("First and last photo of every block of the sheet, with thumbnails, and gaps / duplicates in the numbers").clicked() {
            st.blocks_open = true;
        }
        if let Some(why) = info.busy {
            ui.label(RichText::new(why).color(AMBER));
        }
    });
}

fn preview_table(ui: &mut egui::Ui, st: &mut RenameUi) {
    let Some(plan) = &st.plan else {
        ui.label(RichText::new("The preview appears here.").weak());
        return;
    };
    ui.horizontal(|ui| {
        ui.checkbox(&mut st.only_problems, "Only files with conflicts");
        ui.label(RichText::new("Red rows block the rename. Grey rows keep their name.").weak().small());
    });
    let rows: Vec<usize> = (0..plan.entries.len()).filter(|&i| !st.only_problems || !plan.entries[i].problems.is_empty()).collect();
    let rh = ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y + 2.0;
    egui::ScrollArea::both().auto_shrink([false, false]).max_height(ui.available_height().max(160.0)).show_rows(ui, rh, rows.len(), |ui, range| {
        for &i in &rows[range] {
            let e = &plan.entries[i];
            let bad = !e.problems.is_empty();
            let color = if bad { RED } else if e.changed() { ui.visuals().text_color() } else { ui.visuals().weak_text_color() };
            ui.horizontal(|ui| {
                let tag = match e.kind {
                    EntryKind::Main => "",
                    EntryKind::Sidecar => "sidecar",
                    EntryKind::Pair => "pair",
                };
                ui.add_sized([52.0, rh - 2.0], egui::Label::new(RichText::new(tag).small().weak()));
                ui.add_sized([300.0, rh - 2.0], egui::Label::new(RichText::new(e.old_name()).color(color).monospace()).truncate());
                ui.label(RichText::new("\u{2192}").color(color));
                let new = if e.changed() || bad { e.new_name() } else { "(unchanged)".to_owned() };
                ui.add_sized([300.0, rh - 2.0], egui::Label::new(RichText::new(new).color(color).monospace()).truncate());
                if bad {
                    let text = e.problems.iter().map(|p| p.message()).collect::<Vec<_>>().join("; ");
                    ui.label(RichText::new(text).color(RED).small());
                }
            });
        }
    });
}

// ---------------------------------------------------------------- block check

/// What the block window needs about the images: the thumbnail of an id.
pub type TextureOf<'a> = dyn FnMut(ImageId) -> Option<egui::TextureHandle> + 'a;

/// Draws the block check. `names` gives (old name, new name) of an entry index of the plan.
pub fn blocks_window(ctx: &egui::Context, area: egui::Rect, st: &mut RenameUi, tex: &mut TextureOf) {
    let mut open = true;
    let (Some(plan), Some(report)) = (st.plan.as_ref(), st.blocks.as_ref()) else {
        st.blocks_open = false;
        return;
    };
    let mut visible = 0..0;
    egui::Window::new("Block check").open(&mut open).collapsible(false).resizable(true).constrain_to(area).default_pos(area.min + egui::vec2(120.0, 60.0)).default_size([760.0, 560.0]).show(ctx, |ui| {
        ui.label(RichText::new("Photos of one person or team share a caption, so a shift by one number inside a block is invisible in the names. Check the first and last photo of each block against the picture.").weak().small());
        let list = |ui: &mut egui::Ui, title: &str, items: Vec<String>| {
            if !items.is_empty() {
                let shown: Vec<String> = items.iter().take(24).cloned().collect();
                let more = if items.len() > 24 { format!(" ... and {} more", items.len() - 24) } else { String::new() };
                ui.label(RichText::new(format!("{title}: {}{more}", shown.join(", "))).color(AMBER));
            }
        };
        list(ui, "Numbers of the sheet with no file", report.missing_numbers.iter().map(u64::to_string).collect());
        list(ui, "Numbers carried by several different files", report.duplicate_numbers.iter().map(|(n, v)| format!("{n} ({} files)", v.len())).collect());
        list(ui, "Files whose number is not in the sheet", report.unknown_numbers.iter().map(|(n, _)| n.to_string()).collect());
        list(ui, "Files without a number in the name", report.no_number.iter().map(|&i| plan.entries[i].old_name()).collect());
        if !report.has_warnings() {
            ui.label(RichText::new("No gaps, no duplicates: every number of the folder has exactly one row.").color(GREEN));
        }
        ui.separator();
        let rh = 92.0;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, rh, report.blocks.len(), |ui, range| {
            visible = range.clone();
            for b in &report.blocks[range] {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        ui.label(RichText::new(if b.caption.is_empty() { "(no caption)" } else { b.caption.as_str() }).strong());
                        let range = if b.first_no == b.last_no { format!("No. {}", b.first_no) } else { format!("No. {} - {}", b.first_no, b.last_no) };
                        ui.label(range);
                        let text = format!("{} of {} in the folder", b.in_folder, b.expected());
                        ui.label(RichText::new(text).small().color(if b.in_folder as u64 == b.expected() { GREEN } else { AMBER }));
                    });
                    for idx in [b.first, b.last].into_iter().flatten().take(if b.first == b.last { 1 } else { 2 }) {
                        let e = &plan.entries[idx];
                        let (rect, _) = ui.allocate_exact_size(egui::vec2(104.0, rh - 8.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, Color32::from_gray(30));
                        if let Some(t) = tex(ImageId(e.item)) {
                            let s = t.size_vec2();
                            let k = (rect.width() / s.x).min(rect.height() / s.y);
                            ui.painter().image(t.id(), egui::Rect::from_center_size(rect.center(), s * k), egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)), Color32::WHITE);
                        }
                        ui.vertical(|ui| {
                            ui.set_width(150.0);
                            ui.label(RichText::new(format!("No. {}", e.number.map_or_else(|| "?".to_owned(), |n| n.to_string()))).small().weak());
                            ui.add(egui::Label::new(RichText::new(e.old_name()).small().monospace()).truncate());
                            ui.add(egui::Label::new(RichText::new(format!("\u{2192} {}", e.new_name())).small().monospace().color(if e.problems.is_empty() { GREEN } else { RED })).truncate());
                        });
                    }
                });
                ui.separator();
            }
        });
    });
    st.visible_blocks = visible;
    if !open {
        st.blocks_open = false;
    }
}

/// Ids of the first / last images of the block rows on screen.
pub fn wanted_ids(plan: &Plan, report: &BlockReport, rows: Range<usize>) -> Vec<ImageId> {
    let end = rows.end.min(report.blocks.len());
    let start = rows.start.min(end);
    report.blocks[start..end].iter().flat_map(|b| [b.first, b.last]).flatten().map(|i| ImageId(plan.entries[i].item)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_is_rebuilt_only_when_an_input_changed() {
        let mut st = RenameUi::new(RenameScope::Listed, vec![ImageId(1)]);
        assert!(st.needs_plan(0));
        st.mark_planned(0);
        assert!(!st.needs_plan(0));
        st.rules.template = "x".into();
        assert!(st.needs_plan(0));
        st.mark_planned(0);
        st.scope = RenameScope::Selected;
        assert!(st.needs_plan(0));
        st.mark_planned(0);
        assert!(st.needs_plan(1), "a new sheet makes a new plan");
        st.mark_planned(1);
        st.confirm = true;
        st.invalidate();
        assert!(st.needs_plan(1) && !st.confirm, "a changed plan cancels the confirmation");
    }

    /// Draws the window and the block check headless: no panic, the actions stay quiet, rows are laid out.
    #[test]
    fn the_windows_draw_with_a_plan_and_a_block_report() {
        use rawgraph_rename::{build, check_blocks, CaptionSheet, PlanOptions, RenameRules, SheetColumns, SourceFile};
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for n in [75u64, 76, 77, 80] {
            let p = dir.path().join(format!("Job-{n:04}.jpg"));
            std::fs::write(&p, "x").unwrap();
            files.push(SourceFile { key: n, path: p, capture_unix_ms: None, camera: None });
        }
        let cells: Vec<Vec<String>> = (75u64..=79).map(|n| vec![n.to_string(), format!("{n:03}_Team"), "Team".to_owned()]).collect();
        let sheet = CaptionSheet::from_cells(&cells, &SheetColumns::default()).unwrap();
        let rules = RenameRules { template: "{sheet_name}".into(), ..Default::default() };
        let plan = build(&files, &rules, Some(&sheet), &PlanOptions::default()).unwrap();
        let mut st = RenameUi::new(RenameScope::Listed, vec![ImageId(75)]);
        st.blocks = Some(check_blocks(&plan, &sheet));
        st.plan = Some(plan);
        st.blocks_open = true;
        st.message = Some((true, "x".into()));
        assert!(st.plan.as_ref().unwrap().blocked(), "photo 80 has no sheet row");
        let ctx = egui::Context::default();
        let area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        let info = RenameInfo { busy: None, sheet_loaded: true, selected_available: true, listed: 4, project_name: "p" };
        let mut seen = Vec::new();
        for frame in 0..3 {
            let input = egui::RawInput { screen_rect: Some(area), ..Default::default() };
            ctx.run_ui(input, |ui| {
                let ctx = ui.ctx().clone();
                seen.push(rename_window(&ctx, area, &mut st, &info));
                blocks_window(&ctx, area, &mut st, &mut |_| None);
            })
            .drop_without_applying_deltas();
            if frame == 0 {
                st.confirm = true;
                st.only_problems = true;
            }
        }
        assert!(seen.iter().all(|a| *a == RenameAction::None), "{seen:?}");
        let r = st.blocks.as_ref().unwrap();
        assert_eq!(r.missing_numbers, vec![78, 79], "numbers 78 and 79 of the sheet have no file");
        assert!(st.visible_blocks.end >= 1, "block rows were laid out");
        assert_eq!(wanted_ids(st.plan.as_ref().unwrap(), r, 0..1).len(), 2, "first and last image of the first block");
    }
}
