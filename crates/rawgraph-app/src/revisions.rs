//! Revisions of a project file in the window: the generated summary of a save,
//! the rows and the comparison of the history window, restoring an older graph,
//! reading the file format before opening, and what a conflict would lose.
//!
//! Pure logic, no UI code.

use std::collections::HashMap;
use std::path::Path;

use rawgraph_core::{Graph, ImageId, Mode};
use rawgraph_project::{ColorLabel, Flag, HistoryEntry, Project, ProjectError};

use crate::history::History;
use crate::session::Session;

// ------------------------------------------------------------------ summary of a save

type Cull = (u8, Flag, Option<ColorLabel>);

/// The state of the document as of the last commit or load: what a new
/// revision's summary is compared against.
#[derive(Clone, Debug)]
pub struct Baseline {
    graph: Graph,
    images: HashMap<ImageId, Cull>,
}

impl Baseline {
    pub fn of(project: &Project) -> Self {
        Self { graph: project.graph.clone(), images: project.images().iter().map(|e| (e.id, (e.rating, e.flag, e.color_label))).collect() }
    }

    /// A short factual text of what changed since the baseline; "Saved" when nothing can be derived.
    pub fn summarize(&self, project: &Project) -> String {
        let ids: Vec<ImageId> = project.images().iter().map(|e| e.id).collect();
        let d = diff_graphs(&self.graph, &project.graph, &ids);
        let mut parts: Vec<String> = Vec::new();
        let mut add = |n: usize, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        add(d.added.len(), "node added", "nodes added");
        add(d.removed.len(), "node removed", "nodes removed");
        add(d.param_changes, "parameter change", "parameter changes");
        add(d.edge_changes, "connection change", "connection changes");
        let now: HashMap<ImageId, Cull> = project.images().iter().map(|e| (e.id, (e.rating, e.flag, e.color_label))).collect();
        add(now.keys().filter(|k| !self.images.contains_key(k)).count(), "image added", "images added");
        add(self.images.keys().filter(|k| !now.contains_key(k)).count(), "image removed", "images removed");
        let both = || now.iter().filter_map(|(k, v)| self.images.get(k).map(|old| (old, v)));
        add(both().filter(|(o, n)| o.0 != n.0).count(), "rating", "ratings");
        add(both().filter(|(o, n)| o.1 != n.1).count(), "flag", "flags");
        add(both().filter(|(o, n)| o.2 != n.2).count(), "label", "labels");
        if parts.is_empty() {
            "Saved".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

// ------------------------------------------------------------------ comparing graphs

/// What differs between two graphs (positions of nodes are ignored).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphDiff {
    /// Labels of nodes only in the second graph.
    pub added: Vec<String>,
    /// Labels of nodes only in the first graph.
    pub removed: Vec<String>,
    /// Nodes in both whose parameters or data differ: label and number of differences.
    pub changed: Vec<(String, usize)>,
    pub param_changes: usize,
    pub edge_changes: usize,
}

impl GraphDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty() && self.edge_changes == 0
    }
}

fn node_changes(a: &rawgraph_core::Node, b: &rawgraph_core::Node, images: &[ImageId]) -> usize {
    if a.kind != b.kind {
        return 1;
    }
    let sample: Vec<ImageId> = if images.is_empty() { vec![ImageId(0)] } else { images.to_vec() };
    let per_image = a.mode == Mode::Individual || b.mode == Mode::Individual;
    let used = if per_image { &sample[..] } else { &sample[..1] };
    let mut n = usize::from(a.mode != b.mode);
    for spec in a.kind.params() {
        n += used.iter().filter(|img| a.param(spec.name, **img).to_bits() != b.param(spec.name, **img).to_bits()).count();
    }
    n += used.iter().filter(|img| a.data(**img) != b.data(**img)).count();
    if n == 0 {
        let mut moved = a.clone();
        moved.pos = b.pos;
        if moved != *b {
            n = 1;
        }
    }
    n
}

/// Differences from `old` to `new`; `images` are the images whose individual values are compared.
pub fn diff_graphs(old: &Graph, new: &Graph, images: &[ImageId]) -> GraphDiff {
    let mut d = GraphDiff::default();
    for n in new.nodes() {
        match old.node(n.id) {
            None => d.added.push(n.kind.label().to_owned()),
            Some(o) => {
                let c = node_changes(o, n, images);
                if c > 0 {
                    d.changed.push((n.kind.label().to_owned(), c));
                    d.param_changes += c;
                }
            }
        }
    }
    for o in old.nodes() {
        if new.node(o.id).is_none() {
            d.removed.push(o.kind.label().to_owned());
        }
    }
    d.edge_changes = old.edges().iter().filter(|e| !new.edges().contains(e)).count() + new.edges().iter().filter(|e| !old.edges().contains(e)).count();
    d
}

/// Most lines the comparison shows.
pub const MAX_COMPARE_LINES: usize = 10;

fn list(prefix: &str, labels: &[String]) -> String {
    let shown: Vec<&str> = labels.iter().take(6).map(String::as_str).collect();
    let more = labels.len().saturating_sub(shown.len());
    let tail = if more > 0 { format!(" and {more} more") } else { String::new() };
    format!("{prefix} ({}): {}{tail}", labels.len(), shown.join(", "))
}

/// The comparison of the current graph with the graph of a revision, as lines
/// (at most [`MAX_COMPARE_LINES`]) describing what "Restore this graph" would do.
pub fn compare_lines(current: &Graph, revision: &Graph, images: &[ImageId]) -> Vec<String> {
    let d = diff_graphs(current, revision, images);
    if d.is_empty() {
        return vec!["This graph is the same as the current one.".to_owned()];
    }
    let mut lines = Vec::new();
    if !d.added.is_empty() {
        lines.push(list("Nodes that come back", &d.added));
    }
    if !d.removed.is_empty() {
        lines.push(list("Nodes that go away", &d.removed));
    }
    if d.edge_changes > 0 {
        lines.push(format!("Connections that differ: {}", d.edge_changes));
    }
    if !d.changed.is_empty() {
        lines.push(format!("Parameters that differ: {} in {} node(s)", d.param_changes, d.changed.len()));
        let room = MAX_COMPARE_LINES.saturating_sub(lines.len());
        let take = if d.changed.len() > room { room.saturating_sub(1) } else { d.changed.len() };
        for (label, n) in d.changed.iter().take(take) {
            lines.push(format!("   {label}: {n} value(s)"));
        }
        if take < d.changed.len() {
            lines.push(format!("   and {} more node(s)", d.changed.len() - take));
        }
    }
    lines.truncate(MAX_COMPARE_LINES);
    lines
}

// ------------------------------------------------------------------ the history window

/// One line of the history window.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub revision: u64,
    pub when: String,
    pub author: String,
    pub device: String,
    pub summary: String,
    pub has_graph: bool,
}

impl Row {
    fn of(e: &HistoryEntry) -> Self {
        Self { revision: e.revision, when: crate::clock::local_stamp(e.time_unix_ms), author: e.author.clone(), device: e.device.clone(), summary: e.summary.clone(), has_graph: e.has_graph }
    }

    /// The summary, marked when the graph snapshot was dropped.
    pub fn summary_text(&self) -> String {
        if self.has_graph {
            self.summary.clone()
        } else {
            format!("{} (graph not kept)", self.summary)
        }
    }
}

/// The revisions of the project, newest first.
pub fn rows(project: &Project) -> Vec<Row> {
    project.history().iter().rev().map(Row::of).collect()
}

/// Replaces the current graph with the graph of `revision` as one undoable step. Ratings,
/// flags, labels and the image list stay as they are. `Ok(false)`: it was the same graph.
pub fn restore_graph(session: &mut Session, history: &mut History, revision: u64) -> Result<bool, String> {
    let graph = session.project.graph_at(revision).ok_or_else(|| format!("The graph of revision {revision} was not kept"))?;
    // Edits that are still waiting for their own undo step must not end up inside the restore step.
    history.commit_if_changed(&session.project.graph, false);
    if graph == session.project.graph {
        return Ok(false);
    }
    session.set_graph(graph);
    history.commit_if_changed(&session.project.graph, false);
    session.sanitize();
    Ok(true)
}

// ------------------------------------------------------------------ opening and conflicts

/// The format (schema) version stored in a project file, read from its `meta` table without
/// opening a database; `None` when it cannot be found.
pub fn file_format_version(path: &Path) -> Option<u32> {
    let bytes = std::fs::read(path).ok()?;
    let key = b"schema_version";
    let mut from = 0usize;
    while from + key.len() < bytes.len() {
        let at = bytes[from..].windows(key.len()).position(|w| w == key)? + from + key.len();
        let digits: String = bytes[at..].iter().take(4).take_while(|b| b.is_ascii_digit()).map(|b| char::from(*b)).collect();
        if let Ok(v) = digits.parse::<u32>() {
            return Some(v);
        }
        from = at;
    }
    None
}

/// Shown when a project of format 1 was opened.
pub const CONVERTED_NOTE: &str = "Project converted from format 1 \u{2014} a backup copy is kept next to the file on the next save";

/// The message for a project written by a newer RawGraph.
pub fn too_new_text(file_version: u32, supported: u32) -> String {
    format!("This project was saved by a newer version of RawGraph (format {file_version}, this version reads up to {supported})")
}

/// Shown when an interrupted save next to the project file was rolled back while opening it.
pub const RECOVERED_NOTE: &str = "Recovered an interrupted save";

/// Why a project file could not be opened.
#[derive(Debug, PartialEq)]
pub enum OpenError {
    /// An interrupted save left its journal next to the file and the place is shared (network drive,
    /// sync folder): the user has to say whether it may be rolled back. Carries the project file.
    Unfinished(std::path::PathBuf),
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Unfinished(file) => write!(f, "{}", ProjectError::UnfinishedSave(file.clone())),
            OpenError::Other(text) => write!(f, "{text}"),
        }
    }
}

/// A project that was opened: the note is set when the file was of format 1 (converted in memory) or
/// when an interrupted save was rolled back first.
pub struct Opened {
    pub project: Project,
    pub note: Option<&'static str>,
    pub recovered: bool,
}

/// Loads a project file. An interrupted save next to a file on a local drive that only this computer
/// writes is rolled back automatically; on a network drive or in a sync folder `OpenError::Unfinished`
/// is returned so the application can ask.
pub fn open_project_ex(path: &Path) -> Result<Opened, OpenError> {
    let version = file_format_version(path);
    match Project::load_recovering(path) {
        Ok((project, recovered)) => {
            let note = if recovered { Some(RECOVERED_NOTE) } else { (version == Some(1)).then_some(CONVERTED_NOTE) };
            Ok(Opened { project, note, recovered })
        }
        Err(ProjectError::UnfinishedSave(file)) => Err(OpenError::Unfinished(file)),
        Err(ProjectError::TooNew { file_version, supported }) => Err(OpenError::Other(too_new_text(file_version, supported))),
        Err(e) => Err(OpenError::Other(e.to_string())),
    }
}

/// Like [`open_project_ex`] with the error as text.
#[cfg(test)]
pub fn open_project(path: &Path) -> Result<(Project, Option<&'static str>), String> {
    open_project_ex(path).map(|o| (o.project, o.note)).map_err(|e| e.to_string())
}

/// What overwriting the file at `target` throws away: the revisions of the others
/// after `loaded_revision`, read from the file when that works.
pub fn lost_changes_text(target: &Path, loaded_revision: u64, disk_revision: u64) -> String {
    if let Ok(disk) = Project::load(target) {
        let theirs: Vec<&HistoryEntry> = disk.history().iter().filter(|e| e.revision > loaded_revision).collect();
        if !theirs.is_empty() {
            let mut authors: Vec<&str> = Vec::new();
            let mut devices: Vec<&str> = Vec::new();
            for e in &theirs {
                if !authors.contains(&e.author.as_str()) {
                    authors.push(&e.author);
                }
                if !devices.contains(&e.device.as_str()) {
                    devices.push(&e.device);
                }
            }
            let n = theirs.len();
            return format!("{n} revision{} by {} on {}", if n == 1 { "" } else { "s" }, authors.join(", "), devices.join(", "));
        }
    }
    let first = loaded_revision.saturating_add(1);
    if first >= disk_revision {
        format!("revision {disk_revision}")
    } else {
        format!("revisions {first} to {disk_revision}")
    }
}

/// Suggested file name for a copy saved after a conflict: "<name> (conflict <date>).rawgraph".
pub fn conflict_copy_name(project_name: &str, date: &str) -> String {
    format!("{project_name} (conflict {date}).rawgraph")
}
