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

/// What the pane's collection membership was last computed for.
type MembershipKey = (rawgraph_core::Graph, u64, usize, ImageId, NodeId);

#[derive(Default)]
pub(super) struct PaneRender {
    renderer: Option<Renderer>,
    pub(super) viewer: Viewer,
    key: Option<RequestKey>,
    scopes: Option<rawgraph_scope_views::PrepareSettings>,
    image: Option<ImageId>,
    error: Option<String>,
    compatible: Option<(ImageId, NodeId)>,
    resolved: Option<NodeId>,
    membership_key: Option<MembershipKey>,
}

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
            Err(rawgraph_project::ProjectError::UnfinishedSave(f)) => self.ask_unfinished(f, Retry::Recovery(file.to_path_buf())),
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
        let tick = Tick { now, dirty: self.session.dirty && !self.session.demo, token, focused: ctx.input(|i| i.viewport().focused.unwrap_or(true)) };
        if self.autosave.due(tick) && self.autosave_job.is_none() {
            self.autosave.started(token);
            self.autosave_job = Some(WriteJob::start(self.session.saveable_project(), self.dirs.recovery(), self.session.project_path.clone(), self.session_key.clone(), ctx.clone()));
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
        self.compare.toggle();
        if self.compare.active() {
            self.scope_panel.clear();
            self.scopes_stale = false;
            self.cmp_panes[1].key = None; // Restore primary scopes even if the pins did not change.
            self.compare_started = Some(Instant::now());
            self.compare_reported = false;
        }
    }

    pub(super) fn handle_compare_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.prompt.is_some() { return; }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::C)) { self.toggle_compare(); }
        if self.compare.active() && !self.editor.search_open() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) { self.compare.leave(); }
    }

    fn pane_target(&self, index: usize) -> ViewerTarget {
        let pane = &self.compare.panes[index];
        let mut target = self.session.viewer_target(pane.image, pane.node);
        let render = &self.cmp_panes[index];
        if let (Some((_, _, _, image, node)), Some(resolved)) = (&render.membership_key, render.resolved) {
            if pane.node.is_none() && target.image == Some(*image) && target.node == Some(*node) {
                target.node = self.session.viewer_target(None, Some(resolved)).node;
            }
        }
        target
    }

    pub(super) fn primary_target(&self) -> ViewerTarget {
        if self.compare.active() { self.pane_target(1) } else { self.session.viewer_target(None, self.session.viewed_node) }
    }

    pub(super) fn pane_key(&self, index: usize) -> Option<RequestKey> {
        let target = if index == 1 { self.primary_target() } else { self.pane_target(index) };
        let (image, node) = (target.image?, target.node?);
        Some(RequestKey::for_image(&self.session, image, node, None, None, None))
    }

    /// An image-only pin can keep its last compatible stage when the selected
    /// node does not accept that image. Resolve before building any consumer's
    /// request so the right pane and gallery agree on the fallback.
    pub(super) fn resolve_compare_pins(&mut self) {
        if !self.compare.active() { return; }
        for index in 0..2 {
            let pins = self.compare.panes[index];
            let target = self.session.viewer_target(pins.image, pins.node);
            let pane = &mut self.cmp_panes[index];
            let Some((image, node)) = pins.image.zip(target.node).filter(|_| pins.node.is_none()) else {
                pane.membership_key = None;
                pane.resolved = None;
                // Remember the current compatible stage when a node pin is removed.
                pane.compatible = pins.image.zip(pins.node);
                continue;
            };
            let key = (self.session.project.graph.clone(), self.session.images_gen, self.session.scan_found(), image, node);
            if pane.membership_key.as_ref() == Some(&key) { continue; }
            pane.membership_key = Some(key);
            let (ids, captures) = self.session.id_captures();
            let graph = &self.session.project.graph;
            let accepts = |node| crate::collection::view_members(graph, &ids, &captures, node).contains(&image);
            pane.resolved = None;
            if accepts(node) {
                pane.compatible = Some((image, node));
            } else if let Some((_, old_node)) = pane.compatible.filter(|(i, n)| *i == image && accepts(*n)) {
                pane.resolved = Some(old_node);
            }
        }
    }

    pub(super) fn compare_step(&mut self, ctx: &egui::Context) {
        if !self.compare.active() { return; }
        self.resolve_compare_pins();
        for index in 0..2 {
            let target = self.primary_target();
            if index == 1 && target.image.is_some() && !target.image_pinned && Self::gallery_members(self.gallery_result.as_ref(), target).is_none() { continue; }
            let scopes = (index == 1 && self.scopes_pos != ScopesPos::Hidden).then(|| self.scope_panel.prepare_settings());
            let key = self.pane_key(index);
            let pane = &mut self.cmp_panes[index];
            let key = match key {
                Some(key) => key,
                None => {
                    if index == 1 && pane.key.is_some() {
                        self.scope_panel.clear();
                        self.scopes_stale = false;
                    }
                    pane.key = None;
                    pane.viewer.preview_image = None;
                    pane.image = None;
                    pane.error = None;
                    continue;
                }
            };
            if self.frames < SETTLE_FRAMES { continue; }
            if pane.renderer.is_none() {
                let provider: Arc<dyn rawgraph_engine::ImageProvider> = self.session.provider.clone();
                pane.renderer = Some(Renderer::new(ctx.clone(), provider));
            }
            if pane.key.as_ref() != Some(&key) || pane.scopes != scopes {
                pane.renderer.as_mut().unwrap().submit(self.session.project.graph.clone(), key.node, key.port, key.image, self.session.view, scopes.clone(), None);
                pane.viewer.rendering();
                pane.scopes = scopes;
                pane.key = Some(key.clone());
                pane.image = None;
                pane.error = None;
            }
            if let Some(reply) = pane.renderer.as_mut().and_then(|r| r.poll()) {
                match reply {
                    Reply::Frame(f) => {
                        pane.viewer.set_frame(ctx, &f);
                        pane.image = Some(f.image);
                        pane.error = None;
                        if index == 1 {
                            self.scopes_stale = false;
                            if let Some(prepared) = f.prepared.clone() {
                                self.scope_panel.set_prepared(ctx, prepared);
                                self.scope_worker_ms = f.scopes_ms;
                                self.scopes_stale = false;
                            }
                        }
                    }
                    Reply::Error { message, .. } | Reply::Excluded { message, .. } => {
                        pane.viewer.set_error(message.clone());
                        pane.error = Some(message);
                        pane.image = Some(key.image);
                        if index == 1 { self.scopes_stale = true; }
                    }
                    Reply::Refined(_) | Reply::Dumped { .. } | Reply::Histogram(_) => {}
                }
            }
            if let Some(refined) = pane.renderer.as_mut().and_then(|r| r.take_refined()) {
                if index == 1 {
                    self.scope_panel.set_prepared(ctx, refined.prepared);
                    self.scope_worker_ms = refined.scopes_ms;
                    self.scopes_stale = false;
                }
            }
        }
        if !self.compare_reported && self.cmp_ready() {
            if let Some(t) = self.compare_started { eprintln!("COMPARE both halves shown {:.0} ms after switching it on", t.elapsed().as_secs_f32() * 1000.0); }
            self.compare_reported = true;
        }
    }

    pub(super) fn cmp_ready(&self) -> bool {
        (0..2).all(|i| {
            let pane = &self.cmp_panes[i];
            match self.pane_key(i) {
                None => true,
                Some(key) => pane.key.as_ref() == Some(&key) && pane.image == Some(key.image) && pane.renderer.as_ref().is_some_and(|r| !r.pending()),
            }
        })
    }

    pub(super) fn primary_split_frame(&self) -> (Option<egui::TextureId>, [u32; 2]) {
        let pane = &self.cmp_panes[1];
        let target = self.primary_target().image;
        let has_target = target.is_some() && (target == pane.viewer.preview_image || target == pane.image);
        (if has_target && pane.error.is_none() { pane.viewer.texture_id() } else { None }, pane.viewer.frame_size)
    }

    pub(super) fn compare_area(&mut self, ui: &mut egui::Ui) {
        ui.columns(2, |columns| {
            for (i, ui) in columns.iter_mut().enumerate() {
                let target = self.pane_target(i);
                let pane = &mut self.compare.panes[i];
                ui.horizontal(|ui| {
                    ui.label(if i == 0 { "Reference" } else { "Primary · Clean Feed" });
                    if ui.selectable_label(pane.image.is_some(), "Pin image").on_hover_text("Pin or follow the selected image").clicked() { pane.toggle_image(target.image); }
                    if ui.selectable_label(pane.node.is_some(), "Pin node").on_hover_text("Pin or follow the selected node").clicked() { pane.toggle_node(target.node); }
                });
            }
        });
        self.resolve_compare_pins();
        let session = &self.session;
        let ctx = ui.ctx().clone();
        let panes = &self.cmp_panes;
        let make_half = |i: usize| {
            let pane = &panes[i];
            let ViewerTarget { image, node, .. } = self.pane_target(i);
            let entry = image.and_then(|id| session.entry(id));
            let image_label = entry.map(display_name).unwrap_or_else(|| image.map(|id| format!("Image #{}", id.0)).unwrap_or_else(|| "No image".into()));
            let node_label = node.map(|id| session.project.graph.node(id).map(|n| format!("{} #{}", n.kind.label(), id.0)).unwrap_or_else(|| format!("Missing node #{}", id.0))).unwrap_or_else(|| "No node".into());
            let desired = self.pane_key(i);
            let valid = desired.as_ref().is_some_and(|key| pane.key.as_ref() == Some(key) && pane.image == Some(key.image) && pane.renderer.as_ref().is_some_and(|r| !r.pending()));
            let texture = if (i == 0 || desired.as_ref().map(|k| k.image) == pane.viewer.preview_image) && desired.is_some() && (!valid || pane.error.is_none()) { pane.viewer.texture_id() } else { None };
            let note = if desired.is_none() { Some(if node.is_none() { "No node selected" } else { "No image selected" }.into()) } else if valid { pane.error.clone() } else if i == 0 { Some("Rendering…".into()) } else { None };
            Half { texture, size: pane.viewer.frame_size, uv: pane.viewer.preview_uv, badge: (i == 1 && desired.is_some()).then(|| pane.viewer.badge(&ctx, Instant::now())).flatten(), label: format!("{} — {}{}", image_label, node_label, if pane.resolved == node && node.is_some() { " (last compatible; selected node excludes image)" } else { "" }), entry, note, tag: if i == 0 { "LEFT" } else { "RIGHT" } }
        };
        let left = make_half(0);
        let right = make_half(1);
        let scale = compare::show(ui, &mut self.cmp_view, &mut self.viewer, &left, &right);
        self.viewer.last_scale = scale;
        let step = std::mem::take(&mut self.viewer.navigation_step);
        if step != 0 {
            self.session.step_viewer_target(self.primary_target(), step);
        }
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
        self.filmstrip.live.reset();
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
                self.filmstrip.live.clear_disk();
                let freed = self.thumb_cache.as_ref().map_or(0, |cache| cache.clear());
                self.status.info(format!("Thumbnail caches cleared ({:.1} MB of camera previews freed)", freed as f64 / 1_048_576.0));
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

// ----------------------------------------------------------------------- unfinished saves

/// What to do again once an interrupted save has been rolled back.
pub(super) enum Retry {
    Open(PathBuf),
    Save { target: PathBuf, then: Option<Pending> },
    Recovery(PathBuf),
}

/// The open question: the project file, what to retry, and the error of a failed recovery.
pub(super) struct UnfinishedState {
    file: PathBuf,
    retry: Retry,
    error: Option<String>,
}

impl App {
    /// A load or save found the journal of an interrupted save beside `file` (a network drive or
    /// sync folder, where another computer may be writing): ask before it is rolled back.
    pub(super) fn ask_unfinished(&mut self, file: PathBuf, retry: Retry) {
        self.status.error(format!("{} has an unfinished save beside it", file.display()));
        self.unfinished = Some(UnfinishedState { file, retry, error: None });
    }

    pub(super) fn unfinished_window(&mut self, ctx: &egui::Context) {
        let Some(state) = &mut self.unfinished else { return };
        match dialogs::unfinished_window(ctx, &state.file, state.error.as_deref()) {
            dialogs::UnfinishedAction::None => {}
            dialogs::UnfinishedAction::Cancel => self.unfinished = None,
            dialogs::UnfinishedAction::Recover => match rawgraph_project::Project::recover_unfinished_save(&state.file) {
                Err(e) => state.error = Some(e.to_string()),
                Ok(_) => {
                    let Some(state) = self.unfinished.take() else { return };
                    self.status.info(format!("Recovered an interrupted save of {}", state.file.display()));
                    match state.retry {
                        Retry::Open(path) => self.open_project_path(&path, ctx),
                        Retry::Save { target, then } => self.save_project(Some(target), then, ctx),
                        Retry::Recovery(file) => self.recover(&file, ctx),
                    }
                }
            },
        }
    }

    /// Opens a project file in the window: the current project stays open if that fails. Asks when an
    /// interrupted save beside the file needs the user's word.
    pub(super) fn open_project_path(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        match crate::revisions::open_project_ex(path) {
            Ok(opened) => {
                self.cancel_scan();
                self.discard_own_recovery();
                self.session.replace_project(opened.project, Some(path.to_path_buf()));
                self.after_document_change();
                self.start_capture_refresh(ctx);
                self.start_missing_check(ctx);
                self.status.clear_notes();
                match opened.note {
                    Some(n) if opened.recovered => self.status.info(format!("{n}; opened {}", path.display())),
                    Some(n) => self.status.info(n),
                    None => self.status.info(format!("Opened {}", path.display())),
                }
                self.status.migration("Project", self.session.project.migration_notes().to_vec());
            }
            Err(crate::revisions::OpenError::Unfinished(file)) => self.ask_unfinished(file, Retry::Open(path.to_path_buf())),
            Err(e) => self.status.error(format!("Cannot open {}: {e}", path.display())),
        }
    }
}
