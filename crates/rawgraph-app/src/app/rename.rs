//! The Library > "Rename files..." feature of [`App`]: opening the window, making the preview,
//! loading the caption sheet, carrying the rename out and undoing it. Methods of [`App`] kept apart so
//! `app.rs` stays readable; the rules live in `rename_ops`, the drawing in `rename_ui`.

use super::*;

use rawgraph_rename::{build, check_blocks, PlanOptions};

use crate::rename_ops::{self, Activity};
use crate::rename_ui::{self, RenameAction, RenameInfo, RenameScope, RenameUi};
use crate::thumbs::Wanted;

impl App {
    /// What else is working on the image files right now.
    pub(super) fn rename_activity(&self) -> Activity {
        Activity {
            scanning: self.scan.is_some(),
            exporting: self.export_job.is_some(),
            other: if self.xmp_job.is_some() {
                Some("The XMP import")
            } else if self.capture_job.is_some() {
                Some("The metadata refresh")
            } else if self.missing_job.is_some() {
                Some("The missing-files check")
            } else {
                None
            },
        }
    }

    fn rename_log_path(&self) -> PathBuf {
        rename_ops::log_path(&self.session, &self.dirs)
    }

    pub(super) fn open_rename(&mut self) {
        if self.rename.is_some() {
            return;
        }
        if let Some(why) = rename_ops::refusal(&self.session, &self.rename_activity()) {
            self.status.error(why);
            return;
        }
        let mut st = RenameUi::new(RenameScope::Listed, self.session.visible().to_vec());
        st.log = rename_ops::log_state(&self.rename_log_path());
        st.sheet_status = self.session.caption_sheet.as_ref().map(|b| (false, rawgraph_rename::sheet::describe(&b.sheet)));
        if let Some(b) = &self.session.caption_sheet {
            st.rules.pattern = b.pattern.spec();
            if let Some(p) = &b.sheet.source {
                st.sheet.path = p.to_string_lossy().into_owned();
            }
        }
        self.rename = Some(st);
    }

    /// "Undo last rename" from the Library menu.
    pub(super) fn undo_rename_command(&mut self) {
        let log = self.rename_log_path();
        let activity = self.rename_activity();
        match rename_ops::undo(&mut self.session, &log, &activity) {
            Ok(o) => {
                self.after_relink(&[]);
                self.status.info(format!("Undone: {} file(s) have their old names again", o.files));
            }
            Err(e) => self.status.error(format!("Undo failed: {e}")),
        }
        if let Some(st) = &mut self.rename {
            st.log = rename_ops::log_state(&log);
            st.invalidate();
        }
    }

    fn load_caption_sheet(&mut self) {
        let Some(st) = &mut self.rename else { return };
        match rename_ops::load_sheet(&st.sheet, &st.rules.pattern) {
            Ok(b) => {
                st.sheet_status = Some((false, rawgraph_rename::sheet::describe(&b.sheet)));
                if let Some(path) = &b.sheet.source {
                    st.sheet.path = path.to_string_lossy().into_owned();
                }
                let ambiguous = b.sheet.ambiguous_numbers();
                if !ambiguous.is_empty() {
                    let shown: Vec<String> = ambiguous.iter().take(8).map(u64::to_string).collect();
                    st.sheet_status = Some((true, format!("{} - numbers with several different rows: {}", rawgraph_rename::sheet::describe(&b.sheet), shown.join(", "))));
                }
                self.session.caption_sheet = Some(rename_ops::shared(b));
            }
            Err(e) => st.sheet_status = Some((true, e)),
        }
        self.rename_sheet_gen += 1;
    }

    /// Makes the plan again when an input changed.
    fn refresh_rename_plan(&mut self) {
        let Some(st) = &mut self.rename else { return };
        if !st.needs_plan(self.rename_sheet_gen) {
            return;
        }
        let ids: Vec<ImageId> = match st.scope {
            RenameScope::Listed => self.session.visible().to_vec(),
            RenameScope::Selected => self.session.selected.into_iter().collect(),
        };
        st.ids = ids;
        let started = Instant::now();
        let sources = rename_ops::sources(&self.session, &st.ids);
        let sheet = self.session.caption_sheet.as_ref().map(|b| &b.sheet);
        match st.rules.rules().and_then(|r| build(&sources, &r, sheet, &PlanOptions { pairs: st.rules.pairs })) {
            Ok(plan) => {
                st.blocks = sheet.map(|s| check_blocks(&plan, s));
                st.plan = Some(plan);
                st.plan_error = None;
            }
            Err(e) => {
                st.plan = None;
                st.blocks = None;
                st.plan_error = Some(e);
            }
        }
        st.plan_ms = Some(started.elapsed().as_millis());
        st.mark_planned(self.rename_sheet_gen);
    }

    fn run_rename(&mut self) {
        let log = self.rename_log_path();
        let activity = self.rename_activity();
        let Some(plan) = self.rename.as_ref().and_then(|s| s.plan.clone()) else { return };
        let result = rename_ops::run(&mut self.session, &plan, &log, &activity);
        match &result {
            Ok(o) => {
                self.after_relink(&[]);
                let mut text = format!("Renamed {} file(s) on disk; {} image(s) of the project follow. The rename can be undone.", o.files, o.images);
                if let Some(w) = &o.warning {
                    text.push_str(&format!(" Warning: {w}"));
                }
                self.status.info(text.clone());
            }
            Err(e) => self.status.error(format!("Rename failed: {e}")),
        }
        if let Some(st) = &mut self.rename {
            st.message = Some(match result {
                Ok(o) => (false, format!("Renamed {} file(s). Undo with \"Undo last rename\".", o.files)),
                Err(e) => (true, e),
            });
            st.log = rename_ops::log_state(&log);
            st.invalidate();
        }
    }

    pub(super) fn rename_windows(&mut self, ctx: &egui::Context) {
        if self.rename.is_none() {
            self.rename_thumbs = None;
            return;
        }
        self.refresh_rename_plan();
        let busy = rename_ops::refusal(&self.session, &self.rename_activity());
        let area = self.window_area;
        let project_name = self.session.project_name();
        let info = RenameInfo { busy: busy.as_deref(), sheet_loaded: self.session.caption_sheet.is_some(), selected_available: self.session.selected.is_some(), listed: self.session.visible().len(), project_name: &project_name };
        let action = {
            let Some(st) = &mut self.rename else { return };
            rename_ui::rename_window(ctx, area, st, &info)
        };
        match action {
            RenameAction::None => {}
            RenameAction::Close => {
                self.rename = None;
                self.rename_thumbs = None;
                return;
            }
            RenameAction::Rebuild => {
                if let Some(st) = &mut self.rename {
                    st.invalidate();
                }
            }
            RenameAction::BrowseSheet => self.open_dialog(DialogKind::CaptionSheet),
            RenameAction::LoadSheet => self.load_caption_sheet(),
            RenameAction::ClearSheet => {
                self.session.caption_sheet = None;
                if let Some(st) = &mut self.rename {
                    st.sheet_status = None;
                    st.blocks_open = false;
                }
                self.rename_sheet_gen += 1;
            }
            RenameAction::Run => self.run_rename(),
            RenameAction::Undo => self.undo_rename_command(),
            RenameAction::Recover => {
                let log = self.rename_log_path();
                match rename_ops::recover(&mut self.session, &log) {
                    Ok(m) => {
                        self.status.info(m);
                        self.after_relink(&[]);
                    }
                    Err(e) => self.status.error(format!("Recovery failed: {e}")),
                }
                if let Some(st) = &mut self.rename {
                    st.log = rename_ops::log_state(&log);
                    st.invalidate();
                }
            }
        }
        self.rename_blocks_window(ctx);
    }

    fn rename_blocks_window(&mut self, ctx: &egui::Context) {
        let area = self.window_area;
        let Some(st) = &mut self.rename else { return };
        if !st.blocks_open {
            self.rename_thumbs = None;
            return;
        }
        let loader = self.rename_thumbs.get_or_insert_with(|| ThumbLoader::with_cache(ctx.clone(), self.session.provider.clone(), self.thumb_cache.clone()));
        loader.poll(ctx);
        let session = &self.session;
        rename_ui::blocks_window(ctx, area, st, &mut |id| loader.texture(id));
        if let (Some(plan), Some(report)) = (&st.plan, &st.blocks) {
            let wanted: Vec<Wanted> = rename_ui::wanted_ids(plan, report, st.visible_blocks.clone())
                .into_iter()
                .filter_map(|id| session.entry(id).map(|e| Wanted { id, path: e.path.clone(), hash: e.content_hash.clone() }))
                .collect();
            loader.set_wanted(&wanted);
        }
    }

    /// The sheet file the user picked in the file dialog.
    pub(super) fn caption_sheet_picked(&mut self, path: PathBuf) {
        if let Some(st) = &mut self.rename {
            st.sheet.path = path.to_string_lossy().into_owned();
            self.load_caption_sheet();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headless(dir: &std::path::Path) -> App {
        let ctx = egui::Context::default();
        App::new(&eframe::CreationContext::_new_kittest(ctx), Cli { demo: true, data_dir: Some(dir.to_owned()), ..Default::default() })
    }

    fn frame(app: &mut App) {
        let ctx = egui::Context::default();
        app.window_area = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0));
        let input = egui::RawInput { screen_rect: Some(app.window_area), ..Default::default() };
        ctx.run_ui(input, |ui| app.rename_windows(&ui.ctx().clone())).drop_without_applying_deltas();
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// The whole flow through the application: open the window, the preview follows the inputs, run, undo.
    #[test]
    fn open_preview_rename_and_undo_through_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let job = tmp.path().join("job");
        std::fs::create_dir_all(&job).unwrap();
        for (i, n) in ["a.png", "b.png", "c.png"].iter().enumerate() {
            image::RgbaImage::from_pixel(8, 8, image::Rgba([10 + i as u8 * 50, 60, 90, 255])).save(job.join(n)).unwrap();
        }
        let mut app = headless(&tmp.path().join("data"));
        app.session.new_project();
        app.session.project.scan_folder(&job, false, &rawgraph_io::is_supported, &|_| None);
        app.session.sync_paths();
        app.session.refresh_visible();
        let ids = app.session.visible().to_vec();
        assert_eq!(ids.len(), 3);

        // A running export or scan keeps the window closed.
        app.session.export_busy = true;
        app.open_rename();
        assert!(app.rename.is_none(), "refused while an export runs");
        app.session.export_busy = false;
        app.session.scanning = true;
        app.open_rename();
        assert!(app.rename.is_none(), "refused while a scan runs");
        app.session.scanning = false;

        app.open_rename();
        assert!(app.rename.is_some());
        frame(&mut app);
        assert_eq!(app.rename.as_ref().unwrap().plan.as_ref().unwrap().change_count(), 0, "the default template changes nothing");
        {
            let st = app.rename.as_mut().unwrap();
            st.rules.template = "Job_{counter}".into();
        }
        frame(&mut app);
        let st = app.rename.as_ref().unwrap();
        let plan = st.plan.as_ref().unwrap();
        assert!(!plan.blocked());
        assert_eq!(plan.entries.iter().map(|e| e.new_name()).collect::<Vec<_>>(), vec!["Job_001.png", "Job_002.png", "Job_003.png"]);
        assert_eq!(names(&job), vec!["a.png", "b.png", "c.png"], "the preview touches nothing");

        // A conflict blocks the run in the window: a template that gives every file the same name.
        app.rename.as_mut().unwrap().rules.template = "same".into();
        frame(&mut app);
        assert!(app.rename.as_ref().unwrap().plan.as_ref().unwrap().blocked());
        app.rename.as_mut().unwrap().rules.template = "Job_{counter}".into();
        frame(&mut app);

        app.session.dirty = false;
        app.run_rename();
        assert_eq!(names(&job), vec!["Job_001.png", "Job_002.png", "Job_003.png"]);
        assert!(app.session.dirty, "the project is marked changed");
        assert_eq!(app.session.entry(ids[0]).unwrap().file_name(), "Job_001.png");
        let msg = app.rename.as_ref().unwrap().message.clone().unwrap();
        assert!(!msg.0 && msg.1.contains("Renamed 3"), "{msg:?}");
        assert_eq!(app.rename.as_ref().unwrap().log, (true, 0), "the undo is offered");
        frame(&mut app);

        app.undo_rename_command();
        assert_eq!(names(&job), vec!["a.png", "b.png", "c.png"]);
        assert_eq!(app.session.entry(ids[2]).unwrap().file_name(), "c.png");
        assert_eq!(app.rename.as_ref().unwrap().log, (false, 0));
        let log = tmp.path().join("data").join("rename").join("unsaved-project.rename-log.json");
        assert!(log.is_file(), "an unsaved project logs in the application data folder");
        app.shutdown();
    }
}
