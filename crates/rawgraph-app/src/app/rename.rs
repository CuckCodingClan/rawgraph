//! The Library > "Rename files..." feature of [`App`]: opening the window, making the preview,
//! loading the caption sheet, carrying the rename out on its own thread (progress, cancel) and undoing
//! it. Methods of [`App`] kept apart so `app.rs` stays readable; the rules live in `rename_ops`, the
//! drawing in `rename_ui`.

use super::*;

use rawgraph_rename::{build, check_blocks, PlanOptions};

use crate::rename_ops::{self, Activity, RenameJob};
use crate::rename_ui::{self, RenameAction, RenameInfo, RenameScope, RenameUi};
use crate::thumbs::Wanted;

impl App {
    /// What else is working on the image files right now.
    pub(super) fn rename_activity(&self) -> Activity {
        Activity {
            scanning: self.scan.is_some(),
            exporting: self.export_job.is_some(),
            other: if self.rename_job.is_some() {
                Some("Another rename")
            } else if self.xmp_job.is_some() {
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

    /// Reads the journal state for the window (undo possible, interrupted runs, why it is unusable).
    fn refresh_rename_log_state(&mut self) {
        let log = self.rename_log_path();
        let problem = rawgraph_rename::RenameLog::load(&log).err();
        if let Some(st) = &mut self.rename {
            st.log = rename_ops::log_state(&log);
            st.log_error = problem;
        }
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
        st.sheet_status = self.session.caption_sheet.as_ref().map(|b| (false, rawgraph_rename::sheet::describe(&b.sheet)));
        if let Some(b) = &self.session.caption_sheet {
            st.rules.pattern = b.pattern.spec();
            if let Some(p) = &b.sheet.source {
                st.sheet.path = p.to_string_lossy().into_owned();
            }
        }
        self.rename = Some(st);
        self.refresh_rename_log_state();
    }

    /// "Undo last rename" from the Library menu and the window: runs on its own thread like the rename.
    pub(super) fn undo_rename_command(&mut self, ctx: &egui::Context) {
        let log = self.rename_log_path();
        if let Some(why) = rename_ops::refusal(&self.session, &self.rename_activity()).or_else(|| rename_ops::log_refusal(&self.session, &log, true)) {
            self.status.error(format!("Undo refused: {why}"));
            if let Some(st) = &mut self.rename {
                st.message = Some((true, why));
            }
            return;
        }
        let repaint = ctx.clone();
        self.rename_job = Some(RenameJob::start(None, log, "undo".to_owned(), move || repaint.request_repaint()));
        self.status.info("Undoing the last rename...");
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

    /// Starts the rename of the shown plan on its own thread.
    fn run_rename(&mut self, ctx: &egui::Context) {
        let log = self.rename_log_path();
        if let Some(why) = rename_ops::refusal(&self.session, &self.rename_activity()) {
            self.status.error(why);
            return;
        }
        let Some(plan) = self.rename.as_ref().and_then(|s| s.plan.clone()) else { return };
        let label = format!("{} file(s) in {}", plan.change_count(), self.session.project_name());
        let repaint = ctx.clone();
        self.rename_job = Some(RenameJob::start(Some(plan), log, label, move || repaint.request_repaint()));
        if let Some(st) = &mut self.rename {
            st.message = None;
            st.confirm = false;
        }
    }

    /// Collects the result of a running rename: the project follows (and is saved), or the error is shown.
    pub(super) fn poll_rename_job(&mut self, ctx: &egui::Context) {
        let Some(job) = &self.rename_job else { return };
        let result = match job.rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(rawgraph_rename::RunError::Blocked("the rename thread stopped unexpectedly".into())),
        };
        let Some(job) = self.rename_job.take() else { return };
        let outcome = rename_ops::finish_job(&mut self.session, &job, result);
        let mut message = match &outcome {
            Ok(o) if job.undo => (false, format!("Undone: {} file(s) have their old names again.", o.files)),
            Ok(o) => (false, format!("Renamed {} file(s) on disk; {} image(s) of the project follow. \"Undo last rename\" reverses it.", o.files, o.images)),
            Err(e) => (true, format!("{} failed: {e}", if job.undo { "Undo" } else { "Rename" })),
        };
        if let Ok(o) = &outcome {
            self.after_relink(&[]);
            if let Some(w) = &o.warning {
                message = (true, format!("{} WARNING: {w}", message.1));
            }
            if o.images > 0 {
                if self.session.project_path.is_some() {
                    // Keep the project file in step with the disk at once.
                    self.session.next_summary = Some(format!("{} {} files", if job.undo { "Undid the rename of" } else { "Renamed" }, o.files));
                    self.save_project(None, None, ctx);
                } else {
                    message.1.push_str(" The project has no file yet: save it, or the new names are only in this session (the rename log keeps them).");
                }
            }
        }
        if message.0 {
            self.status.error(message.1.clone());
        } else {
            self.status.info(message.1.clone());
        }
        if let Some(st) = &mut self.rename {
            st.message = Some(message);
            st.invalidate();
        }
        self.refresh_rename_log_state();
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
        let progress = self.rename_job.as_ref().map(|j| j.progress());
        let info = RenameInfo { busy: busy.as_deref(), sheet_loaded: self.session.caption_sheet.is_some(), selected_available: self.session.selected.is_some(), listed: self.session.visible().len(), project_name: &project_name, progress };
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
            RenameAction::Run => self.run_rename(ctx),
            RenameAction::Undo => self.undo_rename_command(ctx),
            RenameAction::Cancel => {
                if let Some(j) = &self.rename_job {
                    j.cancel();
                }
            }
            RenameAction::Recover => {
                let log = self.rename_log_path();
                let activity = self.rename_activity();
                match rename_ops::recover(&mut self.session, &log, &activity) {
                    Ok(m) => {
                        self.status.info(format!("Recovery: {m}"));
                        self.after_relink(&[]);
                        if self.session.project_path.is_some() && self.session.dirty {
                            self.session.next_summary = Some("Rename recovered after an interruption".to_owned());
                            self.save_project(None, None, ctx);
                        }
                        if let Some(st) = &mut self.rename {
                            st.message = Some((false, format!("Recovery: {m}")));
                        }
                    }
                    Err(e) => {
                        self.status.error(format!("Recovery failed: {e}"));
                        if let Some(st) = &mut self.rename {
                            st.message = Some((true, format!("Recovery failed: {e}")));
                        }
                    }
                }
                self.refresh_rename_log_state();
                if let Some(st) = &mut self.rename {
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

    /// Waits for the rename thread and applies its result (what the frame loop does).
    fn settle(app: &mut App) {
        let ctx = egui::Context::default();
        let until = Instant::now() + Duration::from_secs(20);
        while app.rename_job.is_some() {
            app.poll_rename_job(&ctx);
            assert!(Instant::now() < until, "the rename did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
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
        // A second rename cannot start while the first one runs.
        app.run_rename(&egui::Context::default());
        assert!(app.rename_job.is_some());
        assert!(app.rename_activity().other.is_some(), "other jobs see the running rename");
        settle(&mut app);
        assert_eq!(names(&job), vec!["Job_001.png", "Job_002.png", "Job_003.png"]);
        assert!(app.session.dirty, "the project is marked changed");
        assert_eq!(app.session.entry(ids[0]).unwrap().file_name(), "Job_001.png");
        let msg = app.rename.as_ref().unwrap().message.clone().unwrap();
        assert!(!msg.0 && msg.1.contains("Renamed 3"), "{msg:?}");
        assert_eq!(app.rename.as_ref().unwrap().log, (true, 0), "the undo is offered");
        frame(&mut app);

        app.undo_rename_command(&egui::Context::default());
        settle(&mut app);
        assert_eq!(names(&job), vec!["a.png", "b.png", "c.png"]);
        assert_eq!(app.session.entry(ids[2]).unwrap().file_name(), "c.png");
        assert_eq!(app.rename.as_ref().unwrap().log, (false, 0));
        let log = tmp.path().join("data").join("rename").join("unsaved-project.rename-log.json");
        assert!(log.is_file(), "an unsaved project logs in the application data folder");
        app.shutdown();
    }
}
