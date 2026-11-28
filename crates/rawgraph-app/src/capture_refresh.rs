//! Background re-read of capture metadata for project entries that were saved
//! before the as-shot fields existed (`CaptureInfo::raw` is `None`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

use eframe::egui;
use rawgraph_core::{CaptureInfo, ImageId};

/// Reader threads (metadata reads are small; more threads help on a NAS).
const THREADS: usize = 4;

/// What a failed read reports: "looked, found nothing". Applying it only sets `focal_read`.
pub fn failure_marker() -> CaptureInfo {
    CaptureInfo { focal_read: true, ..Default::default() }
}

/// True for [`failure_marker`].
pub fn is_failure_marker(info: &CaptureInfo) -> bool {
    *info == failure_marker()
}

pub struct CaptureRefresh {
    rx: Receiver<(ImageId, CaptureInfo)>,
    pub done: usize,
}

impl CaptureRefresh {
    /// Starts reading the metadata of `targets`. Files that do not exist are
    /// skipped silently. Returns immediately.
    pub fn start(ctx: egui::Context, targets: Vec<(ImageId, PathBuf)>) -> Self {
        let (tx, rx) = channel();
        let total = targets.len();
        let targets = Arc::new(targets);
        let next = Arc::new(AtomicUsize::new(0));
        for n in 0..THREADS.min(total.max(1)) {
            let (tx, targets, next, ctx) = (tx.clone(), targets.clone(), next.clone(), ctx.clone());
            let _ = std::thread::Builder::new().name(format!("rawgraph-capture-{n}")).spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((id, path)) = targets.get(i) else { break };
                if !path.is_file() {
                    continue;
                }
                // A file that cannot be read is remembered (the marker), so it is not read again at every start.
                let info = rawgraph_io::read_capture_info(path).unwrap_or_else(|_| failure_marker());
                if tx.send((*id, info)).is_err() {
                    break;
                }
                if i % 32 == 0 {
                    ctx.request_repaint();
                }
            });
        }
        drop(tx);
        Self { rx, done: 0 }
    }

    /// Results that arrived since the last call, and whether the job is over.
    pub fn poll(&mut self) -> (Vec<(ImageId, CaptureInfo)>, bool) {
        let mut out = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(item) => out.push(item),
                Err(TryRecvError::Empty) => return (out, false),
                Err(TryRecvError::Disconnected) => return (out, true),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn reads_existing_files_and_skips_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for i in 0..6u64 {
            let p = dir.path().join(format!("a{i}.png"));
            image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255])).save(&p).unwrap();
            targets.push((ImageId(i + 1), p));
        }
        targets.push((ImageId(99), dir.path().join("missing.png")));
        let mut job = CaptureRefresh::start(egui::Context::default(), targets);
        let started = Instant::now();
        let mut got = Vec::new();
        loop {
            let (mut items, finished) = job.poll();
            got.append(&mut items);
            if finished || started.elapsed() > Duration::from_secs(20) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.len(), 6);
        assert!(got.iter().all(|(id, _)| id.0 != 99));
        assert!(got.iter().all(|(_, c)| c.raw == Some(false)), "PNG files are rendered sources");
    }
}
