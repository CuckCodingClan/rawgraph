//! The undo log: a JSON file next to the project file (or in the app data folder when the project
//! is not saved). Every run is written to it before the first file is touched.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    /// Written before the first rename; still this when the program stopped in phase 1.
    Started,
    /// Every file has its temporary name; written before the first final name. Still this when the program
    /// stopped in phase 2 or before the final save.
    Phase1Done,
    Done,
    /// A finished rename that was undone later.
    Undone,
    /// The run failed (or was refused) and every file is under its old name.
    RolledBack,
    /// The run failed and is being put back; still this when putting it back failed or the program
    /// stopped meanwhile. Files may be at old, temporary or new names: recovery looks at each one.
    RollingBack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunKind {
    Rename,
    Undo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry {
    pub old: PathBuf,
    pub new: PathBuf,
    pub size: u64,
    /// The size must be unchanged (false for sidecars, which programs rewrite).
    #[serde(default = "yes")]
    pub exact_size: bool,
    /// Modification time (ns since 1970) when the run started, for pictures: renames keep it, so it tells
    /// two files of the same size apart (a swap). `None` for sidecars and in older logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stamp: Option<u64>,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub started_unix_ms: i64,
    pub label: String,
    pub kind: RunKind,
    pub state: RunState,
    pub entries: Vec<LogEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameLog {
    pub version: u32,
    pub runs: Vec<RunRecord>,
}

impl Default for RenameLog {
    fn default() -> Self {
        RenameLog { version: 1, runs: Vec::new() }
    }
}

impl RenameLog {
    /// A missing file is an empty log; an unreadable or broken one is an error (never silently replaced).
    pub fn load(path: &Path) -> Result<RenameLog, String> {
        // `exists` would say "no" for a folder that cannot be read, and a new run would then start a new
        // log over the old one.
        match path.try_exists() {
            Ok(false) => return Ok(RenameLog::default()),
            Ok(true) => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::from_str(&text).map_err(|e| format!("{} is not a valid rename log: {e}", path.display()))
    }

    /// Writes a temporary file beside the log and renames it over the log.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
            f.write_all(text.as_bytes()).map_err(|e| format!("{}: {e}", tmp.display()))?;
            f.sync_all().map_err(|e| format!("{}: {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Make the rename itself durable (a folder cannot be opened for syncing on Windows).
        #[cfg(unix)]
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }

    /// The finished rename that "Undo last rename" would reverse.
    pub fn last_undoable(&self) -> Option<&RunRecord> {
        self.runs.iter().rev().find(|r| r.kind == RunKind::Rename && r.state == RunState::Done)
    }

    /// Runs whose journal says they were interrupted.
    pub fn interrupted(&self) -> usize {
        self.runs.iter().filter(|r| matches!(r.state, RunState::Started | RunState::Phase1Done | RunState::RollingBack)).count()
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A short id, unique within a process and practically unique across runs.
pub fn new_run_id() -> String {
    format!("{:x}{:x}{:02x}", now_ms(), std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed) & 0xff)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_errors() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("sub").join("log.json");
        assert_eq!(RenameLog::load(&p).unwrap(), RenameLog::default(), "missing = empty");
        let mut l = RenameLog::default();
        l.runs.push(RunRecord { id: "a".into(), started_unix_ms: 5, label: "x".into(), kind: RunKind::Rename, state: RunState::Done, entries: vec![LogEntry { old: "a.jpg".into(), new: "b.jpg".into(), size: 3, exact_size: true, stamp: None }] });
        l.save(&p).unwrap();
        assert_eq!(RenameLog::load(&p).unwrap(), l);
        assert_eq!(l.last_undoable().map(|r| r.id.as_str()), Some("a"));
        assert!(!p.with_extension("json.tmp").exists(), "no temporary file is left");
        std::fs::write(&p, "{ broken").unwrap();
        assert!(RenameLog::load(&p).is_err(), "a broken log is never silently replaced");
    }

    #[test]
    fn run_ids_differ() {
        assert_ne!(new_run_id(), new_run_id());
    }
}
