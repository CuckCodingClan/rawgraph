//! Autosave for crash recovery: when to write a recovery copy (a pure
//! scheduler driven by an injected clock) and the background write itself.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use rawgraph_project::Project;

/// A recovery copy is written at most this often while there are unsaved changes.
pub const INTERVAL: Duration = Duration::from_secs(60);
/// After the window lost focus, a copy is written this long after the last edit.
pub const FOCUS_DELAY: Duration = Duration::from_secs(2);

/// What the scheduler needs to know each frame.
#[derive(Clone, Copy, Debug)]
pub struct Tick {
    pub now: Instant,
    /// The project has unsaved changes.
    pub dirty: bool,
    /// Changes whenever the document changes (never repeats while dirty).
    pub token: u64,
    pub focused: bool,
}

#[derive(Debug)]
pub struct Scheduler {
    interval: Duration,
    dirty_since: Option<Instant>,
    seen_token: Option<u64>,
    last_edit: Option<Instant>,
    /// Token of the newest successfully written copy.
    written_token: Option<u64>,
    last_write: Option<Instant>,
    focus_lost_at: Option<Instant>,
    in_flight: Option<u64>,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::with_interval(INTERVAL)
    }
}

impl Scheduler {
    /// A scheduler that writes every `interval` instead of every minute (tests and `--autosave-interval`).
    pub fn with_interval(interval: Duration) -> Self {
        Self { interval, dirty_since: None, seen_token: None, last_edit: None, written_token: None, last_write: None, focus_lost_at: None, in_flight: None }
    }

    /// True when a recovery copy should be written now.
    pub fn due(&mut self, t: Tick) -> bool {
        if !t.dirty {
            // Saved or unchanged: forget everything, nothing to protect.
            *self = Self { in_flight: self.in_flight, ..Self::with_interval(self.interval) };
            return false;
        }
        if self.dirty_since.is_none() {
            self.dirty_since = Some(t.now);
        }
        if self.seen_token != Some(t.token) {
            self.seen_token = Some(t.token);
            self.last_edit = Some(t.now);
        }
        if t.focused {
            self.focus_lost_at = None;
        } else if self.focus_lost_at.is_none() {
            self.focus_lost_at = Some(t.now);
        }
        if self.in_flight.is_some() || self.written_token == Some(t.token) {
            return false;
        }
        let base = self.last_write.or(self.dirty_since).unwrap_or(t.now);
        if t.now.saturating_duration_since(base) >= self.interval {
            return true;
        }
        if let (Some(lost), Some(edit)) = (self.focus_lost_at, self.last_edit) {
            let due_at = lost.max(edit + FOCUS_DELAY);
            if t.now >= due_at {
                return true;
            }
        }
        false
    }

    /// A write was started for the document state `token`.
    pub fn started(&mut self, token: u64) {
        self.in_flight = Some(token);
    }

    /// The write finished. A failure is retried after the normal interval.
    pub fn finished(&mut self, now: Instant, ok: bool) {
        if let (Some(token), true) = (self.in_flight.take(), ok) {
            self.written_token = Some(token);
        }
        self.last_write = Some(now);
    }

    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }
}

/// Result of one background write.
pub struct WriteResult {
    pub file: Result<PathBuf, String>,
    pub finished_at_unix_ms: i64,
}

/// One recovery write on its own thread (never on the UI thread).
pub struct WriteJob {
    rx: Receiver<WriteResult>,
}

impl WriteJob {
    pub fn start(project: Project, dir: PathBuf, project_path: Option<PathBuf>, session_key: String, ctx: eframe::egui::Context) -> Self {
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new().name("rawgraph-autosave".into()).spawn(move || {
            let file = project.save_recovery(&dir, project_path.as_deref(), &session_key).map_err(|e| e.to_string());
            let _ = tx.send(WriteResult { file, finished_at_unix_ms: crate::clock::now_unix_ms() });
            ctx.request_repaint();
        });
        if spawned.is_err() {
            // The receiver sees a closed channel and reports it as a failed write.
        }
        Self { rx }
    }

    /// `Some` once the thread is done; a thread that died gives an error result.
    pub fn poll(&self) -> Option<WriteResult> {
        match self.rx.try_recv() {
            Ok(r) => Some(r),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(WriteResult { file: Err("the autosave thread stopped".to_owned()), finished_at_unix_ms: crate::clock::now_unix_ms() }),
        }
    }
}

/// The recovery files this session wrote (so a clean save or quit can remove exactly those).
#[derive(Debug, Default)]
pub struct OwnFiles {
    files: BTreeSet<PathBuf>,
}

impl OwnFiles {
    pub fn add(&mut self, f: PathBuf) {
        self.files.insert(f);
    }

    pub fn list(&self) -> Vec<PathBuf> {
        self.files.iter().cloned().collect()
    }

    #[cfg(test)]
    pub fn contains(&self, f: &std::path::Path) -> bool {
        self.files.contains(f)
    }

    /// Removes this session's recovery files; returns how many were deleted.
    pub fn discard_all(&mut self) -> usize {
        let mut n = 0;
        for f in std::mem::take(&mut self.files) {
            if rawgraph_project::discard_recovery(&f).is_ok() {
                n += 1;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Clock {
        t0: Instant,
        secs: f64,
    }

    impl Clock {
        fn new() -> Self {
            Self { t0: Instant::now(), secs: 0.0 }
        }
        fn at(&mut self, secs: f64) -> Instant {
            self.secs = secs;
            self.t0 + Duration::from_secs_f64(secs)
        }
    }

    fn tick(now: Instant, dirty: bool, token: u64, focused: bool) -> Tick {
        Tick { now, dirty, token, focused }
    }

    #[test]
    fn a_clean_project_never_writes() {
        let mut c = Clock::new();
        let mut s = Scheduler::default();
        for sec in [0.0, 30.0, 61.0, 500.0] {
            assert!(!s.due(tick(c.at(sec), false, 0, false)));
        }
    }

    #[test]
    fn the_sixty_second_rule_with_an_injected_clock() {
        let mut c = Clock::new();
        let mut s = Scheduler::default();
        assert!(!s.due(tick(c.at(0.0), true, 1, true)), "first dirty frame starts the clock");
        assert!(!s.due(tick(c.at(59.0), true, 1, true)));
        assert!(s.due(tick(c.at(60.0), true, 1, true)), "60 s after the first unsaved change");
        s.started(1);
        assert!(!s.due(tick(c.at(61.0), true, 2, true)), "never overlapping: a write is running");
        assert!(!s.due(tick(c.at(200.0), true, 2, true)), "still running");
        s.finished(c.at(62.0), true);
        assert!(!s.due(tick(c.at(63.0), true, 2, true)), "the next copy waits for its 60 s");
        assert!(s.due(tick(c.at(122.0), true, 2, true)), "62 s + 60 s");
        s.started(2);
        s.finished(c.at(123.0), true);
        assert!(!s.due(tick(c.at(500.0), true, 2, true)), "nothing new since the last copy: no write");
        assert!(s.due(tick(c.at(501.0), true, 3, true)), "a new change after a long pause is due at once (the last copy is old)");
    }

    #[test]
    fn leaving_the_window_writes_two_seconds_after_the_last_edit() {
        let mut c = Clock::new();
        let mut s = Scheduler::default();
        assert!(!s.due(tick(c.at(10.0), true, 1, true)));
        assert!(!s.due(tick(c.at(11.0), true, 2, true)), "edit at 11 s");
        assert!(!s.due(tick(c.at(11.5), true, 2, false)), "focus lost at 11.5 s: 2 s after the edit is 13 s");
        assert!(!s.due(tick(c.at(12.9), true, 2, false)));
        assert!(s.due(tick(c.at(13.0), true, 2, false)));
        s.started(2);
        s.finished(c.at(13.1), true);
        assert!(!s.due(tick(c.at(20.0), true, 2, false)), "nothing new: no second copy");
        // Edit again while unfocused: copy again 2 s later.
        assert!(!s.due(tick(c.at(30.0), true, 3, false)));
        assert!(s.due(tick(c.at(32.0), true, 3, false)));
        // Back in focus resets the rule.
        s.started(3);
        s.finished(c.at(32.1), true);
        assert!(!s.due(tick(c.at(33.0), true, 4, true)));
        assert!(!s.due(tick(c.at(40.0), true, 4, true)), "focused: only the 60 s rule");
    }

    #[test]
    fn a_failed_write_is_retried_after_the_interval_and_saving_resets_everything() {
        let mut c = Clock::new();
        let mut s = Scheduler::default();
        s.due(tick(c.at(0.0), true, 1, true));
        assert!(s.due(tick(c.at(60.0), true, 1, true)));
        s.started(1);
        s.finished(c.at(60.5), false);
        assert!(!s.due(tick(c.at(61.0), true, 1, true)));
        assert!(s.due(tick(c.at(121.0), true, 1, true)), "retry");
        // The user saves: clean again, and a later change starts from scratch.
        assert!(!s.due(tick(c.at(130.0), false, 1, true)));
        assert!(!s.due(tick(c.at(140.0), true, 5, true)));
        assert!(!s.due(tick(c.at(199.0), true, 5, true)));
        assert!(s.due(tick(c.at(200.0), true, 5, true)));
    }

    #[test]
    fn a_background_write_produces_a_recovery_file_that_loads_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = Project::new();
        p.graph.add_node(rawgraph_core::NodeKind::Exposure, [10.0, 10.0]);
        let rec = dir.path().join("recovery");
        let job = WriteJob::start(p.clone(), rec.clone(), None, "key-1".into(), eframe::egui::Context::default());
        let t = Instant::now();
        let result = loop {
            if let Some(r) = job.poll() {
                break r;
            }
            assert!(t.elapsed().as_secs() < 30, "timeout");
            std::thread::sleep(Duration::from_millis(5));
        };
        let file = result.file.unwrap();
        assert!(file.starts_with(&rec));
        let (loaded, info) = rawgraph_project::load_recovery(&file).unwrap();
        assert_eq!(loaded.graph, p.graph);
        assert_eq!(info.project_path, None);
        // The own-files list removes exactly what this session wrote.
        let other = Project::new().save_recovery(&rec, Some(&dir.path().join("x.rawgraph")), "k2").unwrap();
        let mut own = OwnFiles::default();
        own.add(file.clone());
        assert!(own.contains(&file));
        assert_eq!(own.discard_all(), 1);
        assert!(!file.exists() && other.exists(), "another session's file stays");
    }
}
