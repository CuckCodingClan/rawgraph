//! Change history: one entry per commit, with a snapshot of the graph JSON.

use std::sync::Arc;

/// Newest entries kept in a project file.
pub(crate) const MAX_HISTORY_ENTRIES: usize = 100;
/// Total size of the stored graph snapshots (JSON bytes).
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

/// One committed revision of a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Revision this entry created (1 for the first commit).
    pub revision: u64,
    /// Commit time, milliseconds since the Unix epoch.
    pub time_unix_ms: i64,
    /// OS user name of the committing user.
    pub author: String,
    /// Computer name of the committing machine.
    pub device: String,
    pub summary: String,
    /// The graph of this revision is still stored (see [`crate::Project::graph_at`]).
    pub has_graph: bool,
}

/// Entries (oldest first) and their graph snapshots, index for index.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HistoryLog {
    pub(crate) entries: Vec<HistoryEntry>,
    pub(crate) snapshots: Vec<Option<Arc<str>>>,
}

impl HistoryLog {
    /// Builds a log from stored parts; `has_graph` is made to agree with the snapshots.
    pub(crate) fn from_parts(mut entries: Vec<HistoryEntry>, snapshots: Vec<Option<Arc<str>>>) -> Self {
        for (e, s) in entries.iter_mut().zip(&snapshots) {
            e.has_graph = s.is_some();
        }
        Self { entries, snapshots }
    }

    pub(crate) fn snapshot_bytes(&self) -> usize {
        self.snapshots.iter().flatten().map(|s| s.len()).sum()
    }

    /// Appends an entry and enforces the limits: more than `max_entries`
    /// entries drops the oldest entries; more than `max_bytes` of snapshots
    /// drops the oldest snapshots (their entries stay with `has_graph = false`).
    pub(crate) fn push(&mut self, mut entry: HistoryEntry, graph_json: String, max_entries: usize, max_bytes: usize) {
        entry.has_graph = true;
        self.entries.push(entry);
        self.snapshots.push(Some(Arc::from(graph_json)));
        self.trim(max_entries, max_bytes);
    }

    pub(crate) fn trim(&mut self, max_entries: usize, max_bytes: usize) {
        if self.entries.len() > max_entries {
            let drop = self.entries.len() - max_entries;
            self.entries.drain(..drop);
            self.snapshots.drain(..drop);
        }
        let mut total = self.snapshot_bytes();
        for (e, s) in self.entries.iter_mut().zip(self.snapshots.iter_mut()) {
            if total <= max_bytes {
                break;
            }
            if let Some(snap) = s.take() {
                total -= snap.len();
                e.has_graph = false;
            }
        }
    }

    pub(crate) fn snapshot(&self, revision: u64) -> Option<&str> {
        let i = self.entries.iter().position(|e| e.revision == revision)?;
        self.snapshots[i].as_deref()
    }
}
