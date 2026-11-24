//! Checks in the background which files of a project are gone (stat only), so
//! a project opened later shows MISSING markers without the UI waiting on a
//! network drive.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use eframe::egui;
use rawgraph_core::ImageId;

pub struct MissingCheck {
    rx: Receiver<Vec<(ImageId, bool)>>,
}

impl MissingCheck {
    pub fn start(ctx: egui::Context, items: Vec<(ImageId, PathBuf)>) -> Self {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new().name("rawgraph-missing".into()).spawn(move || {
            let out: Vec<(ImageId, bool)> = items.into_iter().map(|(id, p)| (id, !std::fs::metadata(&p).is_ok_and(|m| m.is_file()))).collect();
            let _ = tx.send(out);
            ctx.request_repaint();
        });
        if spawned.is_err() {
            // A closed channel just means no markers.
        }
        Self { rx }
    }

    /// `Some` once finished (an empty list when the thread died).
    pub fn poll(&self) -> Option<Vec<(ImageId, bool)>> {
        match self.rx.try_recv() {
            Ok(v) => Some(v),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_which_files_exist() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().join("a.png");
        std::fs::write(&here, "x").unwrap();
        let job = MissingCheck::start(egui::Context::default(), vec![(ImageId(1), here), (ImageId(2), dir.path().join("gone.png")), (ImageId(3), dir.path().to_path_buf())]);
        let t = std::time::Instant::now();
        let got = loop {
            if let Some(v) = job.poll() {
                break v;
            }
            assert!(t.elapsed().as_secs() < 10);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(got, vec![(ImageId(1), false), (ImageId(2), true), (ImageId(3), true)], "a folder is not an image file");
    }
}
