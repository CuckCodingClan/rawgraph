//! Library and safety-net features of the application: XMP import, relinking
//! missing files, the compare view, autosave and crash recovery. Methods of
//! [`App`] kept apart so `app.rs` stays readable.

use super::*;

use crate::autosave::{Tick, WriteJob};
use crate::clock::local_stamp;
use crate::compare::{self, Half};
use crate::dialogs::{self, RecoveryAction, RecoveryDialog, RelinkAction, RelinkUi, XmpAction};
use crate::filmstrip::FilmAction;
use crate::missing_check::MissingCheck;
use crate::recovery_guard::{process_alive, remove_locks, scan_orphans, Orphans};
use crate::relink::{self, RelinkError};
use crate::xmp_import::{self, JobResult, XmpDialog, XmpFound, XmpJob, XmpOffer};

impl App {
    // ------------------------------------------------------------ missing files

    pub(super) fn start_missing_check(&mut self, ctx: &egui::Context) {
        let items = self.session.id_paths();
        self.missing_job = (!items.is_empty()).then(|| MissingCheck::start(ctx.clone(), items));
    }

    pub(super) fn poll_missing(&mut self) {
        let Some(job) = &self.missing_job else { return };
        if let Some(updates) = job.poll() {
            self.missing_job = None;
            let n = self.session.apply_missing(&updates);
            if n > 0 {
                self.last_key = None;
            }
        }
    }

    // ------------------------------------------------------------------ recovery

    /// Spawns the start-up check for crashed sessions (never blocks the UI).
    pub(super) fn spawn_startup_scan(ctx: &egui::Context, dir: PathBuf, own_lock: String) -> Receiver<Orphans> {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new().name("rawgraph-recovery-scan".into()).spawn(move || {
            let found = scan_orphans(&dir, &own_lock, crate::clock::now_unix_ms(), &process_alive);
            let _ = tx.send(found);
            ctx.request_repaint();
        });
        if spawned.is_err() {
            // The channel closes; no recovery offer this time.
        }
        rx
    }

    pub(super) fn poll_startup_recovery(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.startup_scan else { return };
        let found = match rx.try_recv() {
            Ok(f) => f,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Orphans::default(),
        };
        self.startup_scan = None;
        remove_locks(&found.stale_locks);
        if found.recoveries.is_empty() || self.cli.simulate_crash {
            return;
        }
        if self.cli.recover_latest {
            let latest = found.recoveries[0].file.clone();
            self.recover(&latest, ctx);
            return;
        }
        self.recovery_seen_at = Some(self.frames);
        self.recovery_dialog = Some(RecoveryDialog { items: found.recoveries, confirm: None });
    }

    /// Loads a recovery copy as an unsaved project. The original project file is not touched.
    pub(super) fn recover(&mut self, file: &std::path::Path, ctx: &egui::Context) {
        match rawgraph_project::load_recovery(file) {
            Ok((project, info)) => {
                self.cancel_scan();
                let keyed_by_path = info.project_path.is_some();
                self.session.replace_project(project, info.project_path.clone());
                self.session.touch();
                self.after_document_change();
                self.start_capture_refresh(ctx);
                self.start_missing_check(ctx);
                // A copy keyed by the project path is replaced by the next autosave; an unsaved one is
                // removed once a newer copy exists (or the project is saved).
                self.pending_old_recovery = (!keyed_by_path).then(|| file.to_path_buf());
                let name = dialogs::recovery_title(&info);
                self.status.info(format!("Recovered \"{name}\" ({} images, copy of {}); unsaved", info.image_count, local_stamp(info.saved_unix_ms)));
            }
            Err(e) => self.status.error(format!("Cannot recover {}: {e}", file.display())),
        }
    }

    pub(super) fn recovery_window(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.recovery_dialog else { return };
        match dialogs::recovery_window(ctx, d) {
            RecoveryAction::None => {}
            RecoveryAction::Later => self.recovery_dialog = None,
            RecoveryAction::Recover(file) => {
                self.recovery_dialog = None;
                self.recover(&file, ctx);
            }
            RecoveryAction::Discard(file) => {
                match rawgraph_project::discard_recovery(&file) {
                    Ok(()) => self.status.info("Discarded the recovery copy"),
                    Err(e) => self.status.error(format!("Cannot discard it: {e}")),
                }
                if let Some(d) = &mut self.recovery_dialog {
                    d.items.retain(|i| i.file != file);
                    d.confirm = None;
                    if d.items.is_empty() {
                        self.recovery_dialog = None;
                    }
                }
            }
        }
    }

    fn update_lock(&self) {
        if let Some(l) = &self.lock {
            l.set_files(&self.own_recovery.list());
        }
    }

    /// Removes this session's recovery files (after a save, or when the document is replaced).
    pub(super) fn discard_own_recovery(&mut self) {
        self.own_recovery.discard_all();
        if let Some(old) = self.pending_old_recovery.take() {
            let _ = rawgraph_project::discard_recovery(&old);
        }
        self.update_lock();
    }

    /// The step of the scheduler: starts a background write when one is due, collects finished ones.
    pub(super) fn autosave_step(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if let Some(job) = &self.autosave_job {
            if let Some(result) = job.poll() {
                self.autosave_job = None;
                match result.file {
                    Ok(file) => {
                        self.last_recovery_ms = Some(result.finished_at_unix_ms);
                        self.own_recovery.add(file.clone());
                        if !self.session.dirty {
                            // Saved while the copy was being written: it is not needed.
                            self.own_recovery.discard_all();
                        } else if let Some(old) = self.pending_old_recovery.take() {
                            if old != file {
                                let _ = rawgraph_project::discard_recovery(&old);
                            }
                        }
                        self.update_lock();
                        self.autosave.finished(now, true);
                    }
                    Err(e) => {
                        self.status.error(format!("Autosave failed: {e}"));
                        self.autosave.finished(now, false);
                    }
                }
            }
        }
        let token = (self.history.changes() << 32) ^ self.session.edit_gen;
        let tick = Tick { now, dirty: self.session.dirty && !self.session.demo && !self.session.scanning, token, focused: ctx.input(|i| i.viewport().focused.unwrap_or(true)) };
        if self.autosave.due(tick) && self.autosave_job.is_none() {
            self.autosave.started(token);
            self.autosave_job = Some(WriteJob::start(self.session.project.clone(), self.dirs.recovery(), self.session.project_path.clone(), self.session_key.clone(), ctx.clone()));
        }
        if self.autosave.busy() || self.session.dirty {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
    }

    /// `--simulate-crash`: writes the recovery copy, then ends the process without any cleanup.
    pub(super) fn crash_step(&mut self) {
        if !self.cli.simulate_crash || self.crash_done || !self.cli_applied || self.scan.is_some() || self.capture_job.is_some() || self.startup_scan.is_some() || self.xmp_job.is_some() {
            return;
        }
        self.crash_done = true;
        let started = Instant::now();
        match self.session.project.save_recovery(&self.dirs.recovery(), self.session.project_path.as_deref(), &self.session_key) {
            Ok(file) => {
                self.own_recovery.add(file.clone());
                self.update_lock();
                eprintln!("simulated crash: recovery copy written to {} in {:.1} ms ({} images)", file.display(), started.elapsed().as_secs_f32() * 1000.0, self.session.project.images().len());
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("simulated crash: cannot write the recovery copy: {e}");
                std::process::exit(1);
            }
        }
    }

    /// Clean shutdown: wait for a running write, remove this session's recovery files and its lock.
    pub(super) fn shutdown(&mut self) {
        if let Some(job) = self.autosave_job.take() {
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(5) {
                if let Some(r) = job.poll() {
                    if let Ok(f) = r.file {
                        self.own_recovery.add(f);
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        self.discard_own_recovery();
        if let Some(l) = &self.lock {
            l.release();
        }
    }

    // ------------------------------------------------------------------- compare

    pub(super) fn toggle_compare(&mut self) {
        let visible = self.session.visible().to_vec();
        let was = self.compare.active();
        if !self.compare.toggle(self.session.selected, &visible) && !was {
            self.status.info("Compare needs a second image");
            return;
        }
        if self.compare.active() {
            self.compare_started = Some(Instant::now());
            self.cmp_key = None;
            self.compare_reported = false;
        }
    }

    /// C toggles, Esc leaves; neither while a text field or a popup has the keyboard.
    pub(super) fn handle_compare_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.prompt.is_some() {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::C)) {
            self.toggle_compare();
        }
        if self.compare.active() && !self.editor.search_open() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.compare.leave();
        }
    }

    pub(super) fn compare_step(&mut self, ctx: &egui::Context) {
        self.compare.note_selection(self.session.selected);
        if !self.compare.active() {
            return;
        }
        let Some(reference) = self.compare.reference() else { return };
        if self.cmp_renderer.is_none() {
            let provider: Arc<dyn rawgraph_engine::ImageProvider> = self.session.provider.clone();
            self.cmp_renderer = Some(Renderer::new(ctx.clone(), provider));
        }
        if let Some(reply) = self.cmp_renderer.as_mut().and_then(|r| r.poll()) {
            match reply {
                Reply::Frame(f) => {
                    self.cmp_viewer.set_frame(ctx, &f);
                    self.cmp_image = Some(f.image);
                    self.cmp_error = None;
                }
                Reply::Error { message, .. } | Reply::Excluded { message, .. } => {
                    self.cmp_error = Some(message);
                    self.cmp_image = Some(reference);
                }
                Reply::Refined(_) | Reply::Dumped { .. } | Reply::Histogram(_) => {}
            }
        }
        if self.session.selected.is_none() || self.frames < SETTLE_FRAMES {
            return;
        }
        let Some((node, port)) = self.session.render_target() else { return };
        let key = RequestKey::for_image(&self.session, reference, node, port, None, None);
        if self.cmp_key.as_ref() != Some(&key) {
            if let Some(r) = &mut self.cmp_renderer {
                r.submit(self.session.project.graph.clone(), node, port, reference, self.session.view, None, None);
            }
            self.cmp_key = Some(key);
            self.cmp_image = self.cmp_image.filter(|i| *i == reference);
        }
        if !self.compare_reported && self.cmp_ready() {
            if let Some(t) = self.compare_started {
                eprintln!("COMPARE both halves shown {:.0} ms after switching it on", t.elapsed().as_secs_f32() * 1000.0);
            }
            self.compare_reported = true;
        }
    }

    /// Both halves hold the frame of their image.
    pub(super) fn cmp_ready(&self) -> bool {
        let right_ready = self.viewer.texture_id().is_some() && !self.renderer.pending() && self.last_key.is_some();
        let left_ready = self.compare.reference().is_some_and(|r| self.cmp_image == Some(r)) && self.cmp_renderer.as_ref().is_some_and(|r| !r.pending());
        right_ready && left_ready
    }

    pub(super) fn compare_area(&mut self, ui: &mut egui::Ui) {
        let name = |e: &Option<rawgraph_project::ImageEntry>| e.as_ref().map(display_name).unwrap_or_default();
        let ref_entry = self.compare.reference().and_then(|r| self.session.entry(r)).cloned();
        let sel_entry = self.session.current_entry().cloned();
        let left_stale = self.cmp_image != self.compare.reference() || self.cmp_renderer.as_ref().is_some_and(|r| r.pending());
        let left = Half {
            texture: self.cmp_viewer.texture_id(),
            size: self.cmp_viewer.frame_size,
            label: name(&ref_entry),
            entry: ref_entry.as_ref(),
            note: self.cmp_error.clone().or_else(|| left_stale.then(String::new)),
            tag: "REFERENCE",
        };
        let right_note = self.last_error.clone().or_else(|| self.renderer.pending().then(String::new));
        let right = Half { texture: self.viewer.texture_id(), size: self.viewer.frame_size, label: name(&sel_entry), entry: sel_entry.as_ref(), note: right_note, tag: "SELECTED" };
        let scale = compare::show(ui, &mut self.cmp_view, &left, &right);
        self.viewer.last_scale = scale;
    }

    // ----------------------------------------------------------------------- XMP

    pub(super) fn open_xmp_dialog(&mut self) {
        if self.xmp_dialog.is_none() {
            self.xmp_dialog = Some(XmpDialog::new());
        }
    }

    pub(super) fn start_xmp_import(&mut self, ctx: &egui::Context) {
        if self.session.demo || self.scan.is_some() || self.xmp_job.is_some() {
            if let Some(d) = &mut self.xmp_dialog {
                d.message = Some(if self.session.demo { "The demo images have no files.".to_owned() } else { "Wait until the scan has finished.".to_owned() });
            }
            return;
        }
        let Some(d) = &mut self.xmp_dialog else { return };
        d.summary = None;
        d.message = None;
        let opts = d.opts;
        let (targets, skipped) = xmp_import::pick_targets(self.session.project.images(), &opts);
        self.xmp_job = Some(XmpJob::start(ctx.clone(), targets, skipped, opts));
    }

    /// Applies what a finished read found and fills the dialog's summary.
    fn apply_xmp(&mut self, found: &XmpFound, elapsed_ms: u128) {
        let (summary, changed) = xmp_import::apply(&mut self.session.project, found);
        if !changed.is_empty() {
            self.session.touch();
            self.session.refresh_visible();
        }
        eprintln!(
            "XMP import: {} checked, {} changed, {} unchanged, {} without XMP, {} errors in {elapsed_ms} ms",
            found.checked, summary.changed, summary.unchanged, summary.without_xmp, summary.errors
        );
        self.status.info(format!("XMP: {} changed, {} unchanged, {} without XMP, {} errors", summary.changed, summary.unchanged, summary.without_xmp, summary.errors));
        if let Some(d) = &mut self.xmp_dialog {
            d.summary = Some(summary);
            d.elapsed_ms = Some(elapsed_ms);
        }
    }

    pub(super) fn poll_xmp(&mut self) {
        if let Some(job) = &self.xmp_job {
            if let Some(result) = job.poll() {
                let elapsed = job.started.elapsed().as_millis();
                self.xmp_job = None;
                match result {
                    Ok(JobResult::Done(found)) => self.apply_xmp(&found, elapsed),
                    Ok(JobResult::Cancelled) => {
                        self.status.info("XMP import cancelled; nothing was changed");
                        if let Some(d) = &mut self.xmp_dialog {
                            d.message = Some("Cancelled. Nothing was changed.".to_owned());
                        }
                    }
                    Err(()) => self.status.error("The XMP reader stopped unexpectedly"),
                }
            }
        }
        if let Some(job) = &self.xmp_probe {
            if let Some(result) = job.poll() {
                self.xmp_probe = None;
                if let Ok(JobResult::Done(found)) = result {
                    let count = found.rated_count();
                    if count > 0 {
                        self.xmp_offer = Some(XmpOffer { count, found });
                    }
                }
            }
        }
        self.filmstrip.xmp_banner = self.xmp_offer.as_ref().map(|o| o.count);
    }

    /// After a scan: look (quietly, in the background) whether the new folder carries XMP ratings.
    pub(super) fn start_xmp_probe(&mut self, ctx: &egui::Context) {
        if self.cli.import_xmp || self.session.demo {
            return;
        }
        let opts = xmp_import::XmpOptions::default();
        let (targets, skipped) = xmp_import::pick_targets(self.session.project.images(), &opts);
        if targets.is_empty() {
            return;
        }
        self.xmp_offer = None;
        self.xmp_probe = Some(XmpJob::start(ctx.clone(), targets, skipped, opts));
    }

    /// "Import" in the banner: applies what the background look found, minus images rated meanwhile.
    fn import_offer(&mut self) {
        let Some(mut offer) = self.xmp_offer.take() else { return };
        let session = &self.session;
        offer.found.items.retain(|(id, _, _)| session.entry(*id).is_some_and(|e| e.rating == 0 && e.flag == rawgraph_project::Flag::None && e.color_label.is_none()));
        self.xmp_dialog = Some(XmpDialog::new());
        self.apply_xmp(&offer.found, 0);
    }

    pub(super) fn xmp_window(&mut self, ctx: &egui::Context) {
        let area = self.window_area;
        let running = self.xmp_job.as_ref().map(|j| (j.progress(), j.total));
        let Some(d) = &mut self.xmp_dialog else { return };
        match dialogs::xmp_window(ctx, area, d, running) {
            XmpAction::None => {}
            XmpAction::Import => self.start_xmp_import(ctx),
            XmpAction::Cancel => {
                if let Some(j) = &self.xmp_job {
                    j.cancel();
                }
            }
            XmpAction::Close => {
                if self.xmp_job.is_none() {
                    self.xmp_dialog = None;
                }
            }
        }
    }

    // ------------------------------------------------------------------ filmstrip

    pub(super) fn handle_film_actions(&mut self, ctx: &egui::Context) {
        let actions: Vec<FilmAction> = std::mem::take(&mut self.filmstrip.actions);
        for a in actions {
            match a {
                FilmAction::SetReference(id) => {
                    self.compare.set_reference(id);
                    let name = self.session.entry(id).map(display_name).unwrap_or_default();
                    self.status.info(format!("Reference for compare: {name}"));
                    if self.compare.active() {
                        self.cmp_key = None;
                    }
                }
                FilmAction::Locate(id) => self.open_dialog(DialogKind::LocateFile(id)),
                FilmAction::RelinkFolder(id) => self.open_relink_folder(id),
                FilmAction::XmpImport => self.import_offer(),
                FilmAction::XmpDismiss => self.xmp_offer = None,
            }
        }
        let _ = ctx;
    }

    // -------------------------------------------------------------------- relink

    pub(super) fn open_relink_folder(&mut self, id: rawgraph_core::ImageId) {
        let old = self.session.entry(id).and_then(|e| e.path.parent().map(|p| p.display().to_string())).unwrap_or_default();
        self.relink = RelinkUi::Folder { old, new: String::new(), preview: None, error: None };
    }

    fn after_relink(&mut self, ids: &[rawgraph_core::ImageId]) {
        if ids.is_empty() {
            self.filmstrip.thumbs.clear();
        } else {
            self.filmstrip.thumbs.invalidate(ids);
        }
        self.last_key = None;
        self.prefetch.forget();
        self.collection_key = None;
    }

    /// The user picked a replacement file for a missing image.
    pub(super) fn locate_file(&mut self, id: rawgraph_core::ImageId, path: PathBuf) {
        match relink::locate(&mut self.session, id, &path, false) {
            Ok(()) => {
                self.after_relink(&[id]);
                self.status.info(format!("Relinked to {}", path.display()));
            }
            Err(RelinkError::Mismatch(message)) => self.relink = RelinkUi::Mismatch { id, path, message, confirm: false },
            Err(RelinkError::Other(m)) => self.status.error(m),
        }
    }

    pub(super) fn relink_windows(&mut self, ctx: &egui::Context) {
        let area = self.window_area;
        match dialogs::relink_window(ctx, area, &mut self.relink) {
            RelinkAction::None => {}
            RelinkAction::Close => self.relink = RelinkUi::None,
            RelinkAction::UseAnyway(id, path) => {
                self.relink = RelinkUi::None;
                match relink::locate(&mut self.session, id, &path, true) {
                    Ok(()) => {
                        self.after_relink(&[id]);
                        self.status.info(format!("Relinked to {} although it differs from the original", path.display()));
                    }
                    Err(e) => self.status.error(e.message().to_owned()),
                }
            }
            RelinkAction::Browse => self.open_dialog(DialogKind::RelinkNewFolder),
            RelinkAction::Check => self.check_relink_folder(),
            RelinkAction::Apply => {
                let RelinkUi::Folder { old, new, .. } = &self.relink else { return };
                let (old, new) = (PathBuf::from(old.trim()), PathBuf::from(new.trim()));
                match relink::relink_folder(&mut self.session, &old, &new) {
                    Ok(changed) => {
                        let n = changed.len();
                        self.after_relink(&[]);
                        self.relink = RelinkUi::None;
                        self.status.info(format!("Relinked {n} file(s) to {}", new.display()));
                    }
                    Err(e) => {
                        if let RelinkUi::Folder { error, .. } = &mut self.relink {
                            *error = Some(e.message().to_owned());
                        }
                    }
                }
            }
        }
    }

    pub(super) fn check_relink_folder(&mut self) {
        let RelinkUi::Folder { old, new, preview, error } = &mut self.relink else { return };
        let (o, n) = (PathBuf::from(old.trim()), PathBuf::from(new.trim()));
        if !n.is_dir() {
            *preview = None;
            *error = Some(format!("{} is not a folder", n.display()));
            return;
        }
        *error = None;
        *preview = Some(relink::folder_preview(&self.session.project, &o, &n));
    }

    // ------------------------------------------------------------- thumbnail cache

    pub(super) fn clear_cache_window(&mut self, ctx: &egui::Context) {
        if !self.clear_cache_confirm {
            return;
        }
        let size = self.thumb_cache.as_ref().map_or(0, |c| c.total_bytes());
        let text = if size >= 1 << 20 { format!("{:.1} MB", size as f64 / 1_048_576.0) } else { format!("{} KB", size >> 10) };
        match dialogs::clear_cache_window(ctx, &text) {
            None => {}
            Some(false) => self.clear_cache_confirm = false,
            Some(true) => {
                self.clear_cache_confirm = false;
                // `ThumbCache` has no "remove all"; a zero budget evicts every entry.
                match rawgraph_project::ThumbCache::new(&self.dirs.thumbs(), 0) {
                    Ok(empty) => {
                        let freed = empty.evict();
                        self.status.info(format!("Thumbnail cache cleared ({:.1} MB freed)", freed as f64 / 1_048_576.0));
                    }
                    Err(e) => self.status.error(format!("Cannot clear the thumbnail cache: {e}")),
                }
            }
        }
    }

    // ---------------------------------------------------------------- start-up options

    /// The command-line actions of this wave that need the app (selection copy, recipes, dialogs, compare).
    pub(super) fn cli_extras(&mut self, ctx: &egui::Context) {
        let cli = self.cli.clone();
        if cli.copy_selection {
            let (session, status) = (&self.session, &mut self.status);
            self.editor.copy_selection(session, status);
        }
        if cli.paste_selection {
            let text = self.editor.last_copy().map(str::to_owned);
            // Below the originals, so a screenshot shows both groups.
            let below = crate::graph_editor::bounds(&self.session.project.graph).map(|b| {
                let x = self.session.node_sel.ids().iter().filter_map(|id| self.session.project.graph.node(*id)).map(|n| n.pos[0]).fold(f32::INFINITY, f32::min);
                [if x.is_finite() { x } else { b.min.x }, b.max.y + 36.0]
            });
            self.editor.paste_text(text.as_deref(), below, &mut self.session, &mut self.status);
        }
        for name in &cli.save_recipe {
            match self.editor.save_selection_as_recipe(&self.session, name) {
                Ok(p) => eprintln!("recipe saved: {}", p.display()),
                Err(e) => eprintln!("--save-recipe {name}: {e}"),
            }
        }
        if cli.open_recipes {
            self.editor.open_manage();
        }
        if cli.open_xmp_dialog || cli.import_xmp {
            self.open_xmp_dialog();
        }
        if cli.import_xmp {
            self.start_xmp_import(ctx);
        }
        if let Some(i) = cli.reference {
            if let Some(id) = self.session.visible().get(i).copied() {
                self.compare.set_reference(id);
            } else {
                eprintln!("--reference: there is no image {i}");
            }
        }
        if cli.compare {
            self.toggle_compare();
        }
        if let Some(i) = cli.relink_dialog {
            match self.session.visible().get(i).copied() {
                Some(id) => {
                    self.open_relink_folder(id);
                    if let (Some(new), RelinkUi::Folder { new: slot, .. }) = (&cli.relink_new, &mut self.relink) {
                        *slot = new.display().to_string();
                        self.check_relink_folder();
                    }
                }
                None => eprintln!("--relink-dialog: there is no image {i}"),
            }
        }
    }

    // ----------------------------------------------------------------- measurements

    /// Prints how long the filmstrip took to fill (screenshot and bench runs).
    pub(super) fn report_fill_time(&mut self) {
        if self.fill_reported || !self.cli.capture() && !self.cli.bench {
            return;
        }
        let Some(first) = self.filmstrip.thumbs.first_request else { return };
        if self.scan.is_some() || self.session.visible().is_empty() || !self.filmstrip.all_loaded() {
            return;
        }
        self.fill_reported = true;
        let (hits, misses, replaced) = self.filmstrip.thumbs.stats.snapshot();
        let since_scan = self.scan_done_at.map_or(f32::NAN, |t| t.elapsed().as_secs_f32() * 1000.0);
        eprintln!(
            "FILMSTRIP filled {:.0} ms after the first thumbnail was asked for, {:.0} ms after start ({} thumbnails: {hits} from the disk cache, {misses} decoded, {replaced} replaced); {since_scan:.0} ms since the scan finished",
            first.elapsed().as_secs_f32() * 1000.0,
            self.started_at.elapsed().as_secs_f32() * 1000.0,
            hits + misses + replaced
        );
    }
}
