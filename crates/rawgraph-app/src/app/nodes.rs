//! Glue of the wave-8 nodes: what the inspector asks for (folder dialogs, the node-mode export),
//! scanning the folder a Source was pointed at, running Export nodes one after another and the
//! `--run-export-node` command line option. Methods of [`App`] kept apart so `app.rs` stays readable.

use super::*;

use crate::export::{self, NodeExportState, RunAction, RunNodesState, Scope};
use crate::node_ui;
use crate::session::UiRequest;

impl App {
    // ------------------------------------------------------------- inspector requests

    /// Takes the requests the inspector made during the frame.
    pub(super) fn handle_ui_requests(&mut self, ctx: &egui::Context) {
        let requests = std::mem::take(&mut self.session.ui_requests);
        for r in requests {
            match r {
                UiRequest::PickSourceFolder(node) => self.open_dialog(DialogKind::SourceFolder(node)),
                UiRequest::PickExportFolder(node) => self.open_dialog(DialogKind::NodeExportFolder(node)),
                UiRequest::OpenNodeExport(node) => {
                    if self.session.demo {
                        self.status.error("The demo images cannot be exported. Open a folder first.");
                    } else {
                        self.node_export = Some(NodeExportState::open(&mut self.session, node));
                    }
                }
            }
        }
        self.session.export_busy = self.export_job.is_some();
        let _ = ctx;
    }

    // ----------------------------------------------------------------- source scans

    /// A Source that was pointed at a folder (or got "Include subfolders" switched on) scans it
    /// into the project when no scanned folder covers it yet. Never scans on load: only when the
    /// folder state of a Source changes.
    pub(super) fn sync_source_scans(&mut self) {
        let now = node_ui::source_states(&self.session.project.graph);
        if now != self.source_seen {
            let session = &self.session;
            for item in node_ui::scans_needed(&self.source_seen, &now, &|f, sub| session.folder_covered(f, sub)) {
                if !self.scan_queue.contains(&item) {
                    self.scan_queue.push(item);
                }
            }
            self.source_seen = now;
        }
    }

    /// Starts the next queued scan once none is running.
    pub(super) fn start_queued_scan(&mut self, ctx: &egui::Context) {
        if self.scan.is_some() || self.scan_queue.is_empty() || self.session.demo {
            return;
        }
        let (folder, recursive) = self.scan_queue.remove(0);
        self.start_scan_with(ctx, folder, recursive);
    }

    /// Forgets what the Source nodes looked like (a new document starts from its own state).
    pub(super) fn reset_source_baseline(&mut self) {
        self.source_seen = node_ui::source_states(&self.session.project.graph);
        self.scan_queue.clear();
    }

    // ------------------------------------------------------------------- node export

    /// Starts an export through the Export node `node` for `scope`.
    pub(super) fn start_node_export(&mut self, ctx: &egui::Context, node: NodeId, scope: Scope) -> bool {
        if self.export_job.is_some() {
            self.status.error("An export is already running");
            return false;
        }
        if let Some(why) = export::node_blocker(&self.session, node) {
            self.status.error(why);
            return false;
        }
        let (items, run) = export::node_plan(&mut self.session, node, scope);
        if items.is_empty() {
            self.status.error("There is nothing to export: the node's collection has no image in this scope");
            return false;
        }
        let dir = export::node_settings(&self.session.project.graph, node, items[0].id, run.display).out_dir;
        self.status.info(format!("Exporting {} image(s) through the Export node to {}", items.len(), dir.display()));
        self.export_job = Some(export::ExportJob::start_node(ctx.clone(), self.session.project.graph.clone(), node, items, run));
        self.finished_export = None;
        self.session.export_busy = true;
        true
    }

    /// The node-mode export dialog and the "Run Export nodes" dialog.
    pub(super) fn node_export_windows(&mut self, ctx: &egui::Context) {
        if let Some(mut st) = self.node_export.take() {
            let image = self.session.current_image();
            let lines = self.session.project.graph.node(st.node).map(|n| node_ui::export_lines(&rawgraph_core::export_spec(n, image))).unwrap_or_default();
            let busy = self.export_job.is_some();
            let area = self.window_area;
            let blocker = export::node_blocker(&self.session, st.node);
            match export::show_node_dialog(ctx, area, &mut st, &lines, busy, blocker.as_deref()) {
                DialogAction::Start => {
                    let (node, scope) = (st.node, st.scope);
                    self.start_node_export(ctx, node, scope);
                }
                DialogAction::Close => {}
                _ => self.node_export = Some(st),
            }
        }
        if let Some(mut st) = self.run_nodes.take() {
            let busy = self.export_job.is_some();
            match export::show_run_nodes_dialog(ctx, self.window_area, &mut st, busy) {
                RunAction::Run => {
                    self.export_queue = st.entries.iter().filter(|e| e.run && e.problem.is_none()).map(|e| (e.node, st.scope)).collect();
                    self.advance_export_queue(ctx);
                }
                RunAction::Close => {}
                RunAction::None => self.run_nodes = Some(st),
            }
        }
    }

    /// Opens "Run Export nodes...".
    pub(super) fn open_run_nodes(&mut self) {
        if self.session.demo {
            self.status.error("The demo images cannot be exported. Open a folder first.");
            return;
        }
        let entries = export::export_nodes(&self.session.project.graph, self.session.current_image());
        self.run_nodes = Some(RunNodesState { entries, scope: Scope::All });
    }

    /// Starts the next Export node of a "Run Export nodes" batch when no export is running.
    pub(super) fn advance_export_queue(&mut self, ctx: &egui::Context) {
        while self.export_job.is_none() {
            let Some((node, scope)) = self.export_queue.pop_front() else { return };
            if self.start_node_export(ctx, node, scope) {
                return;
            }
        }
    }

    // --------------------------------------------------------------------------- CLI

    /// `--run-export-node <n>`: runs that Export node for the filtered images once the start-up
    /// actions (including folder scans) are done.
    pub(super) fn cli_run_export_node(&mut self, ctx: &egui::Context) {
        let Some(index) = self.cli.run_export_node else { return };
        if !self.cli_applied || self.cli_run_started || self.scan.is_some() || !self.scan_queue.is_empty() || self.missing_job.is_some() || self.capture_job.is_some() {
            return;
        }
        self.cli_run_started = true;
        let Some(node) = self.session.node_by_index(index).filter(|id| self.session.project.graph.node(*id).is_some_and(|n| n.kind == NodeKind::Export)) else {
            eprintln!("--run-export-node: node {index} is not an Export node");
            EXIT_CODE.store(1, Ordering::SeqCst);
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        };
        if !self.start_node_export(ctx, node, Scope::Filtered) {
            eprintln!("--run-export-node: nothing was exported");
            EXIT_CODE.store(1, Ordering::SeqCst);
            self.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// `--open-node-export` and `--open-run-nodes`: show the dialogs at start.
    pub(super) fn cli_open_node_dialogs(&mut self) {
        if let Some(index) = self.cli.open_node_export {
            match self.session.node_by_index(index) {
                Some(node) => self.node_export = Some(NodeExportState::open(&mut self.session, node)),
                None => eprintln!("--open-node-export: there is no node {index}"),
            }
        }
        if self.cli.open_run_nodes {
            self.open_run_nodes();
        }
    }

    /// Prints the report of a `--run-export-node` run and ends the program.
    pub(super) fn cli_run_export_finish(&mut self, ctx: &egui::Context) {
        if !self.cli_run_started || self.cli_run_reported || self.cli.run_export_node.is_none() || self.export_job.is_some() || !self.export_queue.is_empty() {
            return;
        }
        let Some(fin) = &self.finished_export else {
            // The job ended with an error (already printed): nothing to report.
            if self.status.current().is_some_and(|(_, e)| e) {
                EXIT_CODE.store(1, Ordering::SeqCst);
                self.allow_close = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        };
        self.cli_run_reported = true;
        println!("{}", export::summarize(&fin.report));
        for (id, st) in &fin.report.items {
            match st {
                rawgraph_batch::ItemStatus::Written { path, width, height } => {
                    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    println!("written {} ({width}x{height}, {bytes} bytes) image {}", path.display(), id.0);
                }
                other => println!("image {}: {other:?}", id.0),
            }
        }
        if fin.report.failed() > 0 || fin.report.written() == 0 {
            EXIT_CODE.store(1, Ordering::SeqCst);
        }
        self.allow_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}
