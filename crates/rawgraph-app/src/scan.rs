//! Background folder scan: a worker thread runs `Project::scan_folder_with`
//! on a clone of the project and reports progress and finished entries.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use eframe::egui;
use rawgraph_project::{ImageEntry, Project, ScanReport};

pub enum ScanMsg {
    Progress { done: usize, total: usize, current: PathBuf, entry: Option<ImageEntry> },
    Done { project: Box<Project>, report: ScanReport },
}

pub struct ScanJob {
    pub rx: Receiver<ScanMsg>,
    cancel: Arc<AtomicBool>,
    pub done: usize,
    pub total: usize,
    pub current: String,
}

impl ScanJob {
    /// Starts scanning `folder` (recursively) into a clone of `project`.
    pub fn start(ctx: egui::Context, project: Project, folder: PathBuf, recursive: bool) -> Self {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (c2, f2) = (cancel.clone(), folder.clone());
        let spawned = std::thread::Builder::new().name("rawgraph-scan".into()).spawn(move || {
            let mut project = project;
            let tx_progress = tx.clone();
            let ctx_p = ctx.clone();
            let report = project.scan_folder_with(
                &f2,
                recursive,
                &rawgraph_io::is_supported,
                &|p| rawgraph_io::read_capture_info(p).ok(),
                &mut |p| {
                    let _ = tx_progress.send(ScanMsg::Progress { done: p.done, total: p.total, current: p.current.clone(), entry: p.entry.clone() });
                    ctx_p.request_repaint();
                    !c2.load(Ordering::Relaxed)
                },
            );
            let _ = tx.send(ScanMsg::Done { project: Box::new(project), report });
            ctx.request_repaint();
        });
        if spawned.is_err() {
            // The channel closes; the app reports the scan as failed.
        }
        Self { rx, cancel, done: 0, total: 0, current: String::new() }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            self.done as f32 / self.total as f32
        }
    }
}

/// One-line summary of a finished scan.
pub fn summarize(r: &ScanReport) -> String {
    format!(
        "Scan {}: {} added, {} unchanged, {} relinked, {} updated, {} failed",
        if r.cancelled { "cancelled" } else { "finished" },
        r.added.len(),
        r.unchanged,
        r.relinked.len(),
        r.updated.len(),
        r.failed.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn write_pngs(dir: &std::path::Path, n: usize) {
        for i in 0..n {
            image::RgbaImage::from_pixel(16, 16, image::Rgba([(i % 80) as u8 * 3, 20, 40, 255])).save(dir.join(format!("p{i:03}.png"))).unwrap();
        }
    }

    fn drain(job: &ScanJob) -> (Vec<ImageEntry>, Option<(Project, ScanReport)>) {
        let started = Instant::now();
        let mut entries = Vec::new();
        while started.elapsed() < Duration::from_secs(30) {
            match job.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(ScanMsg::Progress { entry, .. }) => entries.extend(entry),
                Ok(ScanMsg::Done { project, report }) => return (entries, Some((*project, report))),
                Err(_) => {}
            }
        }
        (entries, None)
    }

    #[test]
    fn entries_arrive_progressively_and_the_project_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        write_pngs(dir.path(), 12);
        let job = ScanJob::start(egui::Context::default(), Project::new(), dir.path().to_path_buf(), true);
        let (entries, done) = drain(&job);
        let (project, report) = done.expect("scan finished");
        assert_eq!(entries.len(), 12, "every finished file is announced");
        assert_eq!(project.images().len(), 12);
        assert_eq!(report.added.len(), 12);
        assert!(summarize(&report).contains("12 added"));
        // Entries arrive in path order, before Done.
        let names: Vec<String> = entries.iter().map(|e| e.file_name()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
    }

    #[test]
    fn cancel_stops_the_scan_with_a_consistent_project() {
        let dir = tempfile::tempdir().unwrap();
        write_pngs(dir.path(), 200);
        let job = ScanJob::start(egui::Context::default(), Project::new(), dir.path().to_path_buf(), true);
        job.cancel(); // cancelled before the first progress call returns
        let (_, done) = drain(&job);
        let (project, report) = done.expect("worker still reports");
        assert!(report.cancelled);
        assert!(project.images().len() < 200, "a cancelled scan does not finish the folder");
        assert_eq!(project.images().len(), report.added.len());
    }
}
