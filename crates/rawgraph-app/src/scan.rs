//! Directory-only discovery followed by visible-first hashing and deferred metadata.
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;
use rawgraph_core::ImageId;
use rawgraph_project::staged::{HashQueue, Listing};
use rawgraph_project::{ImageEntry, Project, ScanReport};

pub enum ScanMsg {
    Listing(Listing),
    Catalog(Vec<ImageEntry>),
    Hashed(Vec<ImageEntry>),
    Metadata { done: usize, total: usize, entries: Vec<ImageEntry> },
    Done { project: Box<Project>, report: ScanReport },
}

#[derive(Default)]
struct Priority {
    wanted: Option<Vec<ImageId>>,
    queue: Option<HashQueue>,
    thumbs_ready: bool,
}

#[derive(Default)]
struct Shared {
    cancel: AtomicBool,
    priority: Mutex<Priority>,
    wake: Condvar,
}

pub struct ScanJob {
    pub rx: Receiver<ScanMsg>,
    shared: Arc<Shared>,
    pub count: Option<usize>,
    pub listed: bool,
    pub counting: usize,
    pub metadata: Option<(usize, usize)>,
}

impl ScanJob {
    pub fn start(ctx: egui::Context, mut project: Project, folder: PathBuf, recursive: bool) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("rawgraph-scan".into()).spawn(move || {
            let cancelled = || s.cancel.load(Ordering::Relaxed);
            let publish = |msg| {
                let sent = tx.send(msg).is_ok();
                ctx.request_repaint();
                sent && !cancelled()
            };
            let mut scan = project.list_catalog(&folder, recursive, &rawgraph_io::is_supported, &mut |event| publish(ScanMsg::Listing(event)));
            if !scan.report.cancelled && publish(ScanMsg::Catalog(std::mem::take(&mut scan.entries))) {
                let mut queue = HashQueue::new(&scan.hashes);
                let (mut p, _) = s.wake.wait_timeout_while(s.priority.lock().unwrap(), Duration::from_millis(150), |p| p.wanted.is_none() && !cancelled()).unwrap();
                if let Some(wanted) = &p.wanted { queue.prioritize(wanted); }
                p.queue = Some(queue);
                drop(p);
                let workers = project.scan_threads().min(scan.hashes.len());
                let paths: Vec<_> = scan.hashes.iter().map(|job| job.path.clone()).collect();
                let (result_tx, result_rx) = channel();
                std::thread::scope(|scope| {
                    for _ in 0..workers {
                        let (s, paths, tx) = (&s, &paths, result_tx.clone());
                        scope.spawn(move || {
                            while !s.cancel.load(Ordering::Relaxed) {
                                let Some(i) = s.priority.lock().unwrap().queue.as_mut().and_then(HashQueue::take) else { break };
                                if tx.send((i, rawgraph_project::content_hash(&paths[i]))).is_err() { break; }
                            }
                        });
                    }
                    drop(result_tx);
                    let mut batch = Vec::new();
                    // Publish promptly for the first on-screen items, then batch
                    // the flood of off-screen results on fast filesystems.
                    let mut early = 64;
                    for (i, result) in result_rx {
                        if cancelled() { break; }
                        if let Some(entry) = scan.finish_hash(&mut project, i, result) {
                            batch.push(entry);
                            let visible = s.priority.lock().unwrap().wanted.as_ref().is_some_and(|ids| ids.contains(&batch.last().unwrap().id));
                            if early > 0 || visible || batch.len() >= 64 {
                                early = usize::saturating_sub(early, 1);
                                if !publish(ScanMsg::Hashed(std::mem::take(&mut batch))) { break; }
                            }
                        }
                    }
                    if !batch.is_empty() { publish(ScanMsg::Hashed(batch)); }
                });
                if !scan.metadata.is_empty() {
                    let (priority, _) = s.wake.wait_timeout_while(s.priority.lock().unwrap(), Duration::from_millis(250), |p| !p.thumbs_ready && !cancelled()).unwrap();
                    drop(priority);
                }
                let total = scan.metadata.len();
                let mut batch = Vec::new();
                let mut last = Instant::now();
                for (index, id) in scan.metadata.into_iter().enumerate() {
                    // Separate lane: never occupy the thumbnail workers, and yield to them between reads.
                    let (priority, _) = s.wake.wait_timeout_while(s.priority.lock().unwrap(), Duration::from_millis(2), |p| !p.thumbs_ready && !cancelled()).unwrap();
                    drop(priority);
                    if cancelled() { break; }
                    let path = &project.image(id).expect("catalog id").path;
                    let result = rawgraph_io::read_capture_info(path);
                    if let Some(entry) = project.finish_capture(id, result.ok()) { batch.push(entry); }
                    if index + 1 == total || last.elapsed() >= Duration::from_millis(250) {
                        if !publish(ScanMsg::Metadata { done: index + 1, total, entries: std::mem::take(&mut batch) }) { break; }
                        last = Instant::now();
                    }
                }
            }
            scan.report.cancelled |= cancelled();
            let _ = publish(ScanMsg::Done { project: Box::new(project), report: scan.report });
        });
        Self { rx, shared, count: None, listed: false, counting: 0, metadata: None }
    }

    pub fn prioritize(&self, wanted: &[ImageId], thumbs_ready: bool) {
        let mut p = self.shared.priority.lock().unwrap();
        p.thumbs_ready = thumbs_ready;
        let changed = p.wanted.as_deref() != Some(wanted);
        if changed {
            if let Some(queue) = &mut p.queue { queue.prioritize(wanted); }
            p.wanted = Some(wanted.to_vec());
        }
        self.shared.wake.notify_all();
    }

    pub fn cancel(&self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

impl Drop for ScanJob {
    fn drop(&mut self) { self.cancel(); }
}

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

    #[test]
    fn scan_announces_count_and_catalog_before_visible_first_hash_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        for i in [10, 2, 1] {
            image::RgbaImage::new(16, 16).save(dir.path().join(format!("p{i}.png"))).unwrap();
        }
        let mut project = Project::new();
        project.set_scan_threads(1);
        let job = ScanJob::start(egui::Context::default(), project, dir.path().to_owned(), true);
        let next = || job.rx.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(matches!(next(), ScanMsg::Listing(Listing::Counting(0))));
        assert!(matches!(next(), ScanMsg::Listing(Listing::Count(3))));
        let ScanMsg::Catalog(entries) = next() else { panic!("catalog expected") };
        assert_eq!(entries.iter().map(ImageEntry::file_name).collect::<Vec<_>>(), ["p1.png", "p2.png", "p10.png"]);
        assert!(entries.iter().all(|e| e.content_hash.is_empty() && e.capture.scan_pending));
        job.prioritize(&[entries[2].id], true);
        for index in [2, 0, 1] {
            assert!(matches!(next(), ScanMsg::Hashed(e) if e.len() == 1 && e[0].id == entries[index].id));
        }
        assert!(matches!(next(), ScanMsg::Metadata { done: 3, total: 3, .. }));
        let ScanMsg::Done { project, report } = next() else { panic!("finished project expected") };
        assert_eq!(report.added.len(), 3);
        assert!(project.images().iter().all(|e| !e.content_hash.is_empty() && !e.capture.scan_pending));
    }

    #[test]
    fn scan_cancellation_after_catalog_keeps_pending_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.png"), b"tiny").unwrap();
        let job = ScanJob::start(egui::Context::default(), Project::new(), dir.path().to_owned(), true);
        loop {
            if matches!(job.rx.recv_timeout(Duration::from_secs(30)).unwrap(), ScanMsg::Catalog(_)) { break; }
        }
        job.cancel();
        let ScanMsg::Done { project, report } = job.rx.recv_timeout(Duration::from_secs(30)).unwrap() else { panic!("cancelled project expected") };
        assert!(report.cancelled);
        assert_eq!(project.images().len(), 1);
        assert!(project.images()[0].content_hash.is_empty());
    }
}
