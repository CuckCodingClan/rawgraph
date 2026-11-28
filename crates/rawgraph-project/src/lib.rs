//! RawGraph project model.
//!
//! A project is folder-based and database-backed. Photos stay where they are
//! and are never modified, renamed or deleted by this crate; the project only
//! remembers them. Every image gets a stable [`ImageId`] that survives renames
//! and moves (see [`Project::scan_folder`]), which is what keeps per-image node
//! settings attached to the right photo. Images and the node graph are stored
//! together in one SQLite file.
//!
//! The crate does not depend on any image decoder: deciding which files are
//! supported and reading their metadata is injected by the caller as closures.
//!
//! # Identity
//!
//! `content_hash` is a *partial* BLAKE3 hash over the file size, the first
//! 64 KiB and the last 64 KiB (see [`content_hash`]). It is deliberately partial
//! so scanning stays fast on network storage; it is used to recognise a moved
//! or renamed file, not to prove two files are byte-identical.

mod history;
mod ids;
mod keywords;
mod maintain;
mod recovery;
mod store;
pub mod staged;
mod thumbs;
mod util;
mod writemode;
mod xmp;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_extra;
#[cfg(test)]
mod tests_project;
#[cfg(test)]
mod tests_inplace;
#[cfg(test)]
mod tests_keywords;

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use rawgraph_core::{CaptureInfo, Graph, ImageId, NodeKind};
use serde::{Deserialize, Serialize};

pub use history::HistoryEntry;
pub use keywords::{add_to_list as add_keywords_to_list, count_keywords, remove_from_list as remove_keyword_from_list, KeywordCount, KeywordMatch};
pub use recovery::{discard_recovery, list_recoveries, load_recovery, RecoveryInfo};
pub use thumbs::ThumbCache;
pub use util::{content_hash, natural_cmp};
pub use writemode::{always_rewrite, set_always_rewrite, write_mode, WriteChoice, WriteMode};
pub use xmp::{parse_xmp_culling, xmp_sidecar_path, XmpCulling};

/// File extension of project files (without the dot).
pub const PROJECT_EXTENSION: &str = "rawgraph";
/// Version of the SQLite layout written by [`Project::save`] and
/// [`Project::commit`]. Files with an older version are migrated when loaded
/// (in memory; the file on disk is only replaced by the next write, after a
/// `.v<N>.bak` copy of it), files with a newer one give [`ProjectError::TooNew`].
pub const SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Flag {
    None,
    Pick,
    Reject,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorLabel {
    Red,
    Yellow,
    Green,
    Blue,
    Purple,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImageEntry {
    /// Stable for the life of the project, never reused. Do not change it.
    pub id: ImageId,
    /// Globally unique id (32 lowercase hex characters), created when the
    /// image is first added and never changed afterwards, not even by relinking.
    /// For collaboration and for references that must outlive this project file;
    /// graphs keep using the numeric `id`.
    pub uuid: String,
    /// Absolute path of the source file.
    pub path: PathBuf,
    /// Partial content hash (hex), or empty until hashing succeeds.
    pub content_hash: String,
    pub file_size: u64,
    /// File modification time, milliseconds since the Unix epoch, if known.
    pub modified_unix_ms: Option<i64>,
    pub capture: CaptureInfo,
    /// 0..=5
    pub rating: u8,
    pub flag: Flag,
    pub color_label: Option<ColorLabel>,
    /// Keywords (stichwörter): normalised (see `rawgraph_core::keywords`), de-duplicated
    /// case-insensitively and sorted; a hierarchical keyword is `Parent|Child`. Stored in the
    /// `keywords` table (schema 3).
    pub keywords: Vec<String>,
    /// The file was not found at `path` at the last check.
    pub missing: bool,
}

impl ImageEntry {
    /// File name without directories (lossy for non-UTF-8 names).
    pub fn file_name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// A folder that was scanned into the project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFolder {
    /// Absolute folder path.
    pub path: PathBuf,
    /// The last scan of this folder included its subfolders.
    pub recursive: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScanReport {
    pub added: Vec<ImageId>,
    pub unchanged: usize,
    pub relinked: Vec<ImageId>,
    /// Files replaced at the same path with different content.
    pub updated: Vec<ImageId>,
    /// The progress callback stopped the scan.
    pub cancelled: bool,
    pub failed: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanProgress {
    /// Number of files finished so far, `0..=total`.
    pub done: usize,
    pub total: usize,
    /// The file that just finished (the first file when `done == 0`).
    pub current: PathBuf,
    /// Clone of the entry just finished and committed to the project (added,
    /// relinked, updated or unchanged). `None` for the initial `done == 0`
    /// call and for files that failed. Entries arrive in path order, so new
    /// images arrive in project order.
    pub entry: Option<ImageEntry>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    pub iso_min: Option<f32>,
    /// Inclusive range; images without ISO never match when a bound is set.
    pub iso_max: Option<f32>,
    pub min_rating: Option<u8>,
    pub flag: Option<Flag>,
    /// Images with unknown flash state never match when set.
    pub flash_fired: Option<bool>,
    /// Case-insensitive substring of the file name.
    pub text: Option<String>,
    /// Keywords to look for (an image carries a keyword when it has it or one below it). Empty = no
    /// keyword filter. Combined by [`Filter::keyword_match`].
    pub keywords: Vec<String>,
    pub keyword_match: KeywordMatch,
    /// Only images without any keyword (wins over `keywords`).
    pub no_keywords: bool,
    pub include_missing: bool,
}

impl Filter {
    pub fn matches(&self, e: &ImageEntry) -> bool {
        if e.missing && !self.include_missing {
            return false;
        }
        if self.iso_min.is_some() || self.iso_max.is_some() {
            let Some(iso) = e.capture.iso else { return false };
            if self.iso_min.is_some_and(|min| iso < min) || self.iso_max.is_some_and(|max| iso > max) {
                return false;
            }
        }
        if self.min_rating.is_some_and(|r| e.rating < r) {
            return false;
        }
        if self.flag.is_some_and(|f| e.flag != f) {
            return false;
        }
        if let Some(want) = self.flash_fired {
            if e.capture.flash_fired != Some(want) {
                return false;
            }
        }
        if let Some(n) = &self.text {
            if !e.file_name().to_lowercase().contains(&n.to_lowercase()) {
                return false;
            }
        }
        keywords::matches_filter(e, &self.keywords, self.keyword_match, self.no_keywords)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    FileName,
    ModifiedDate,
    CaptureDate,
    Iso,
    Ev100,
    Rating,
}

#[derive(Debug)]
pub enum ProjectError {
    Io(String),
    Database(String),
    /// No longer produced: loading a newer file gives [`ProjectError::TooNew`].
    UnsupportedVersion(u32),
    Corrupt(String),
    /// A file offered for relinking does not match the entry, or a commit
    /// target belongs to another project.
    Mismatch(String),
    /// The file on disk has a newer revision than the one this project was
    /// loaded with or last committed (someone else committed in between).
    /// Nothing was written.
    Conflict { disk_revision: u64, loaded_revision: u64 },
    /// The file was written by a newer version of the program.
    TooNew { file_version: u32, supported: u32 },
    /// A save was interrupted (crash, power loss) and left its rollback journal next to the
    /// project file; the file is not read until the save is rolled back, see
    /// [`Project::recover_unfinished_save`]. Carries the project file.
    UnfinishedSave(PathBuf),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectError::Io(e) => write!(f, "file error: {e}"),
            ProjectError::Database(e) => write!(f, "project database error: {e}"),
            ProjectError::UnsupportedVersion(v) => {
                write!(f, "project format version {v} is newer than this build supports ({SCHEMA_VERSION})")
            }
            ProjectError::Corrupt(e) => write!(f, "project file is damaged or not a RawGraph project: {e}"),
            ProjectError::Mismatch(e) => write!(f, "file does not match: {e}"),
            ProjectError::Conflict { disk_revision, loaded_revision } => write!(
                f,
                "the project file was changed by someone else (revision {disk_revision} on disk, this copy is based on revision {loaded_revision})"
            ),
            ProjectError::TooNew { file_version, supported } => write!(
                f,
                "project file format version {file_version} is newer than this build supports (version {supported})"
            ),
            ProjectError::UnfinishedSave(file) => {
                let journal = file.file_name().map(|n| format!("{}-journal", n.to_string_lossy())).unwrap_or_default();
                write!(
                    f,
                    "the project file has an unfinished save next to it ({journal}): open it from the computer that saved it, or recover the unfinished save (only if no other computer is saving this project right now)"
                )
            }
        }
    }
}

impl std::error::Error for ProjectError {}

pub(crate) const DEFAULT_SCAN_THREADS: usize = 8;
const MAX_SCAN_THREADS: usize = 32;

/// What a scan has to do for one file.
enum Plan {
    /// Same path, size and mtime: nothing to read.
    Unchanged(usize),
    /// Same path but size or mtime differ: entry index, old size, old hash.
    Existing(usize, u64, String),
    New,
}

struct Hashed {
    hash: String,
    size: u64,
    /// Present when the file is new or its content changed.
    capture: Option<CaptureInfo>,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub graph: Graph,
    /// Stable id of the project (32 hex chars), created once and kept by
    /// every save, "save as" and recovery copy.
    uuid: String,
    /// In project order; ids are strictly ascending because entries are only appended.
    images: Vec<ImageEntry>,
    next_image_id: u64,
    /// Revision this copy is based on: loaded with, or last committed.
    revision: u64,
    history: history::HistoryLog,
    folders: Vec<SourceFolder>,
    /// Schema version of the file this project was migrated from; not persisted
    /// and not part of equality.
    pub(crate) migrated_from: Option<u32>,
    /// What the graph migration of an older file could not carry over exactly (graph format 1 and 2:
    /// removed mask nodes and ports); empty for a current file. Not persisted and not part of
    /// equality: the next save writes the migrated graph in the current format.
    pub(crate) migration_notes: Vec<String>,
    /// Not persisted and not part of equality.
    pub(crate) scan_threads: usize,
    /// How the last commit really wrote its file; not persisted, not part of equality.
    pub(crate) last_write: Option<WriteChoice>,
}

impl PartialEq for Project {
    fn eq(&self, other: &Self) -> bool {
        self.graph == other.graph
            && self.uuid == other.uuid
            && self.images == other.images
            && self.next_image_id == other.next_image_id
            && self.revision == other.revision
            && self.history == other.history
            && self.folders == other.folders
    }
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

impl Project {
    /// Empty project whose graph already contains a Source node connected to an Output node.
    pub fn new() -> Self {
        let mut graph = Graph::new();
        let source = graph.add_node(NodeKind::Source, [0.0, 0.0]);
        let output = graph.add_node(NodeKind::Output, [300.0, 0.0]);
        // Source -> Output is always a valid connection, so the result carries no information.
        let _ = graph.connect(source, output, 0);
        Self {
            graph,
            uuid: ids::new_uuid(),
            images: Vec::new(),
            next_image_id: 1,
            revision: 0,
            history: history::HistoryLog::default(),
            folders: Vec::new(),
            migrated_from: None,
            migration_notes: Vec::new(),
            scan_threads: DEFAULT_SCAN_THREADS,
            last_write: None,
        }
    }

    /// Stable id of the project: 32 lowercase hex characters, created when the
    /// project is created, kept by saves, "save as" and recovery copies. A
    /// version-1 file has none; loading it generates one, and the file keeps
    /// no id until it is written again.
    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    /// Revision this copy is based on: 0 = never committed, otherwise the
    /// revision it was loaded with or last committed.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Notes of the graph migration done while loading this project (empty when the graph needed none).
    pub fn migration_notes(&self) -> &[String] {
        &self.migration_notes
    }

    /// Commit history, oldest first (at most 100 entries).
    pub fn history(&self) -> &[HistoryEntry] {
        &self.history.entries
    }

    /// The graph as it was at `revision`, if that entry and its snapshot are still stored.
    pub fn graph_at(&self, revision: u64) -> Option<Graph> {
        self.graph_at_with_notes(revision).map(|(g, _)| g)
    }

    /// Like [`Project::graph_at`], with the notes of the graph migration when the snapshot is of an older format.
    pub fn graph_at_with_notes(&self, revision: u64) -> Option<(Graph, Vec<String>)> {
        Graph::from_json_repaired(self.history.snapshot(revision)?).ok()
    }

    /// Folders scanned into the project, in the order they were first scanned.
    pub fn folders(&self) -> &[SourceFolder] {
        &self.folders
    }

    /// Makes this copy of the project continue where `from` is in its commit
    /// history: revision, history and the "migrated from" note are taken over.
    ///
    /// A background scan works on a copy of the project and hands it back when
    /// it is done. If the original was saved (committed) meanwhile, the copy
    /// still carries the old revision and history; swapping it in unchanged
    /// would make the next save conflict with the file the user saved
    /// themselves. Call this before swapping.
    pub fn inherit_versioning(&mut self, from: &Project) {
        self.revision = from.revision;
        self.history = from.history.clone();
        self.migrated_from = from.migrated_from;
    }

    /// Adds the images a running scan has found so far (`found`, any order) and
    /// replaces the entries it has already changed. Entries for ids that are
    /// older than the newest entry and unknown here are ignored (the image was
    /// removed meanwhile). Keeps ids strictly ascending and the id counter ahead
    /// of every id. Returns how many entries were added or replaced.
    pub fn merge_found(&mut self, found: &[ImageEntry]) -> usize {
        let mut sorted: Vec<&ImageEntry> = found.iter().collect();
        sorted.sort_by_key(|e| e.id);
        let mut merged = 0;
        for entry in sorted {
            if let Some(i) = self.index_of(entry.id) {
                let old = &self.images[i];
                // A rescan empties the hash (and marks the capture pending) of files that changed on
                // disk while it reads them. Such a half-finished entry never replaces a complete one:
                // the file keeps the old, good values, and because their size and time no longer match
                // the file, the next scan reads it again.
                let downgrade = !old.content_hash.is_empty()
                    && (entry.content_hash.is_empty() || (entry.capture.scan_pending && !old.capture.scan_pending));
                if !downgrade && *old != *entry {
                    self.images[i] = entry.clone();
                    merged += 1;
                }
            } else if self.images.last().is_none_or(|last| last.id < entry.id) {
                self.next_image_id = self.next_image_id.max(entry.id.0 + 1);
                self.images.push(entry.clone());
                merged += 1;
            }
        }
        merged
    }

    /// Records `folder` as scanned (what a scan does first), for a project that
    /// is saved while the scan is still running.
    pub fn note_scanned_folder(&mut self, folder: &Path, recursive: bool) {
        if let Ok(absolute) = std::path::absolute(folder) {
            if absolute.is_dir() {
                self.note_folder(&absolute, recursive);
            }
        }
    }

    fn note_folder(&mut self, folder: &Path, recursive: bool) {
        let normal: PathBuf = folder.components().collect();
        let key = util::path_key(&normal);
        match self.folders.iter_mut().find(|f| util::path_key(&f.path) == key) {
            Some(f) => f.recursive = recursive,
            None => self.folders.push(SourceFolder { path: normal, recursive }),
        }
    }

    pub fn images(&self) -> &[ImageEntry] {
        &self.images
    }

    fn index_of(&self, id: ImageId) -> Option<usize> {
        match self.images.binary_search_by_key(&id.0, |e| e.id.0) {
            Ok(i) => Some(i),
            // Fallback in case a caller changed an id through `image_mut`.
            Err(_) => self.images.iter().position(|e| e.id == id),
        }
    }

    pub fn image(&self, id: ImageId) -> Option<&ImageEntry> {
        self.index_of(id).map(|i| &self.images[i])
    }

    /// For rating / flag / label edits. Do not change `id` through this.
    pub fn image_mut(&mut self, id: ImageId) -> Option<&mut ImageEntry> {
        let i = self.index_of(id)?;
        Some(&mut self.images[i])
    }

    fn alloc_id(&mut self) -> ImageId {
        let id = ImageId(self.next_image_id);
        self.next_image_id += 1;
        id
    }

    /// Adds every supported file in `folder`. `is_supported` decides by path;
    /// `read_capture` returns metadata or `None` if it cannot be read (the
    /// image is still added, with default [`CaptureInfo`]).
    ///
    /// Files are processed in natural, case-insensitive path order. For each:
    /// 1. an entry with the same path exists: unchanged (a changed size or
    ///    hash is updated, the id is kept);
    /// 2. otherwise an entry with the same size and hash whose file is gone
    ///    from disk is relinked to the new path (id, rating, flag, label and
    ///    capture are kept);
    /// 3. otherwise a new entry with a fresh id is added (a copy of a file that
    ///    is still present is a new image).
    ///
    /// Unreadable files and folders are reported in `failed`; the scan goes on.
    /// Nothing on disk is ever written.
    pub fn scan_folder(
        &mut self,
        folder: &Path,
        recursive: bool,
        is_supported: &(dyn Fn(&Path) -> bool + Sync),
        read_capture: &(dyn Fn(&Path) -> Option<CaptureInfo> + Sync),
    ) -> ScanReport {
        self.scan_folder_with(folder, recursive, is_supported, read_capture, &mut |_| true)
    }

    /// Like [`Project::scan_folder`] with progress reporting and cancellation.
    ///
    /// `progress` runs on the calling thread: once before any work with
    /// `done == 0`, then after every finished file with `done` = number of
    /// files finished so far (`1..=total`; `current` is the file that just
    /// finished, or the first file for `done == 0`). Return `false` to stop.
    ///
    /// An entry with the same path whose size and modification time are
    /// unchanged is `unchanged` without opening the file. If either differs the
    /// file is re-hashed: same hash -> just size/mtime are updated
    /// (`unchanged`); different hash -> the file was replaced, so hash, size,
    /// mtime and capture are refreshed and the id is listed in `updated`
    /// (id, rating, flag and label are kept).
    ///
    /// Hashing and metadata reading run on up to [`Project::set_scan_threads`]
    /// worker threads (so both closures must be `Sync`), but results are
    /// applied strictly in natural path order: ids and report vectors do not
    /// depend on which file finishes first. On cancellation the files finished
    /// so far form a prefix of that order and are kept completely; work that
    /// workers had already done for later files is discarded, so a later scan
    /// simply picks those files up. Nothing is ever half-added.
    pub fn scan_folder_with(
        &mut self,
        folder: &Path,
        recursive: bool,
        is_supported: &(dyn Fn(&Path) -> bool + Sync),
        read_capture: &(dyn Fn(&Path) -> Option<CaptureInfo> + Sync),
        progress: &mut dyn FnMut(&ScanProgress) -> bool,
    ) -> ScanReport {
        let mut report = ScanReport::default();
        let folder = match std::path::absolute(folder) {
            Ok(p) => p,
            Err(e) => {
                report.failed.push((folder.to_path_buf(), e.to_string()));
                return report;
            }
        };

        if folder.is_dir() {
            self.note_folder(&folder, recursive);
        }
        let mut files = collect_files_with(&folder, recursive, is_supported, &mut report.failed, &mut |_| true);
        util::sort_under(&folder, &mut files);
        let total = files.len();
        if total == 0 {
            return report;
        }

        let mut by_path: HashMap<String, usize> =
            self.images.iter().enumerate().map(|(i, e)| (util::path_key(&e.path), i)).collect();
        // Candidates for relinking, keyed by (size, hash).
        let mut by_content: HashMap<(u64, String), Vec<usize>> = HashMap::new();
        for (i, e) in self.images.iter().enumerate() {
            if !e.content_hash.is_empty() { by_content.entry((e.file_size, e.content_hash.clone())).or_default().push(i); }
        }

        // Decide up front which files need to be opened.
        let mtimes: Vec<Option<i64>> = files.iter().map(|(_, m)| modified_ms(m)).collect();
        let plans: Vec<Plan> = files
            .iter()
            .zip(&mtimes)
            .map(|((path, meta), mtime)| match by_path.get(&util::path_key(path)) {
                Some(&i) => {
                    let e = &self.images[i];
                    if !e.content_hash.is_empty() && !e.capture.scan_pending && e.file_size == meta.len() && mtime.is_some() && e.modified_unix_ms == *mtime {
                        Plan::Unchanged(i)
                    } else {
                        Plan::Existing(i, e.file_size, if e.capture.scan_pending { String::new() } else { e.content_hash.clone() })
                    }
                }
                None => Plan::New,
            })
            .collect();
        let jobs: Vec<usize> = (0..total).filter(|&k| !matches!(plans[k], Plan::Unchanged(_))).collect();
        let workers = self.scan_threads.clamp(1, MAX_SCAN_THREADS).min(jobs.len());

        let cancel = AtomicBool::new(false);
        let next_job = AtomicUsize::new(0);
        let (tx, rx) = mpsc::channel::<(usize, Result<Hashed, String>)>();
        let (files_ref, plans_ref, jobs_ref) = (&files, &plans, &jobs);
        let (cancel_ref, next_ref) = (&cancel, &next_job);

        std::thread::scope(|scope| {
            for _ in 0..workers {
                let tx = tx.clone();
                scope.spawn(move || loop {
                    if cancel_ref.load(Ordering::Relaxed) {
                        break;
                    }
                    let j = next_ref.fetch_add(1, Ordering::Relaxed);
                    let Some(&k) = jobs_ref.get(j) else { break };
                    let path = &files_ref[k].0;
                    let result = util::content_hash(path).map_err(|e| e.to_string()).map(|(hash, size)| {
                        let changed = match &plans_ref[k] {
                            Plan::Existing(_, old_size, old_hash) => *old_size != size || *old_hash != hash,
                            _ => true,
                        };
                        let capture = changed.then(|| read_capture(path).unwrap_or_default());
                        Hashed { hash, size, capture }
                    });
                    if tx.send((k, result)).is_err() {
                        break;
                    }
                });
            }
            drop(tx);

            if !progress(&ScanProgress { done: 0, total, current: files[0].0.clone(), entry: None }) {
                report.cancelled = true;
                cancel.store(true, Ordering::Relaxed);
                return;
            }
            let mut pending: HashMap<usize, Result<Hashed, String>> = HashMap::new();
            for k in 0..total {
                let outcome = if matches!(plans[k], Plan::Unchanged(_)) {
                    None
                } else {
                    loop {
                        if let Some(r) = pending.remove(&k) {
                            break Some(r);
                        }
                        match rx.recv() {
                            Ok((j, r)) => {
                                pending.insert(j, r);
                            }
                            Err(_) => break Some(Err("scan worker stopped unexpectedly".to_string())),
                        }
                    }
                };
                let path = &files[k].0;
                let mtime = mtimes[k];
                let committed: Option<usize> = match (&plans[k], outcome) {
                    (Plan::Unchanged(i), _) => {
                        self.images[*i].missing = false;
                        report.unchanged += 1;
                        Some(*i)
                    }
                    (_, Some(Err(e))) => {
                        report.failed.push((path.clone(), e));
                        None
                    }
                    (Plan::Existing(i, _, _), Some(Ok(h))) => {
                        let entry = &mut self.images[*i];
                        entry.missing = false;
                        entry.modified_unix_ms = mtime;
                        match h.capture {
                            None => report.unchanged += 1,
                            Some(capture) => {
                                entry.content_hash = h.hash.clone();
                                entry.file_size = h.size;
                                entry.capture = capture;
                                by_content.entry((h.size, h.hash)).or_default().push(*i);
                                report.updated.push(entry.id);
                            }
                        }
                        Some(*i)
                    }
                    (Plan::New, Some(Ok(h))) => {
                        let moved = by_content.get(&(h.size, h.hash.clone())).and_then(|cands| {
                            cands.iter().copied().find(|&i| {
                                let e = &self.images[i];
                                e.file_size == h.size && e.content_hash == h.hash && !e.path.is_file()
                            })
                        });
                        if let Some(i) = moved {
                            let entry = &mut self.images[i];
                            by_path.remove(&util::path_key(&entry.path));
                            entry.path = path.clone();
                            entry.missing = false;
                            entry.modified_unix_ms = mtime;
                            by_path.insert(util::path_key(path), i);
                            report.relinked.push(entry.id);
                            Some(i)
                        } else {
                            let id = self.alloc_id();
                            let i = self.images.len();
                            by_path.insert(util::path_key(path), i);
                            by_content.entry((h.size, h.hash.clone())).or_default().push(i);
                            self.images.push(ImageEntry {
                                id,
                                uuid: ids::new_uuid(),
                                path: path.clone(),
                                content_hash: h.hash,
                                file_size: h.size,
                                modified_unix_ms: mtime,
                                capture: h.capture.unwrap_or_default(),
                                rating: 0,
                                flag: Flag::None,
                                color_label: None,
                                keywords: Vec::new(),
                                missing: false,
                            });
                            report.added.push(id);
                            Some(i)
                        }
                    }
                    // Unreachable combinations (a job always yields an outcome).
                    (_, None) => None,
                };
                let entry = committed.map(|i| self.images[i].clone());
                if !progress(&ScanProgress { done: k + 1, total, current: path.clone(), entry }) && k + 1 < total {
                    report.cancelled = true;
                    break;
                }
            }
            cancel.store(true, Ordering::Relaxed);
        });
        report
    }

    /// Number of worker threads used by scans (default 8, clamped to 1..=32).
    pub fn set_scan_threads(&mut self, n: usize) {
        self.scan_threads = n.clamp(1, MAX_SCAN_THREADS);
    }

    pub fn scan_threads(&self) -> usize {
        self.scan_threads
    }

    /// Removes entries from the project only (files are never touched).
    /// Returns how many were removed. Ids are not reused afterwards.
    pub fn remove_images(&mut self, ids: &[ImageId]) -> usize {
        let set: std::collections::HashSet<ImageId> = ids.iter().copied().collect();
        let before = self.images.len();
        self.images.retain(|e| !set.contains(&e.id));
        before - self.images.len()
    }

    /// Re-checks every entry's file; updates `missing`; returns how many are missing.
    pub fn refresh_missing(&mut self) -> usize {
        let mut count = 0;
        for entry in &mut self.images {
            entry.missing = !std::fs::metadata(&entry.path).is_ok_and(|m| m.is_file());
            if entry.missing {
                count += 1;
            }
        }
        count
    }

    /// Ids of the images matching `filter`, in project order.
    pub fn filter(&self, filter: &Filter) -> Vec<ImageId> {
        self.images.iter().filter(|e| filter.matches(e))
            .map(|e| e.id)
            .collect()
    }

    /// Orders `ids` by `key`. Unknown ids are dropped. `FileName` is natural
    /// and case-insensitive; for `Iso` and `Ev100` images without a value come
    /// last in both directions; ties keep (ascending) file-name order.
    pub fn sorted(&self, ids: &[ImageId], key: SortKey, descending: bool) -> Vec<ImageId> {
        Self::sort_entries(ids.iter().filter_map(|id| self.image(*id)), key, descending)
    }

    pub fn sort_entries<'a>(entries: impl IntoIterator<Item = &'a ImageEntry>, key: SortKey, descending: bool) -> Vec<ImageId> {
        use std::cmp::Ordering;
        struct Item {
            id: ImageId,
            name: String,
            value: Option<f32>,
            rating: u8,
            date: Option<i64>,
        }
        let mut items: Vec<Item> = entries.into_iter()
            .map(|e| Item {
                id: e.id,
                name: e.file_name(),
                value: match key {
                    SortKey::Iso => e.capture.iso,
                    SortKey::Ev100 => e.capture.ev100(),
                    _ => None,
                },
                rating: e.rating,
                date: match key {
                    SortKey::CaptureDate => e.capture.captured_unix_ms.or(e.modified_unix_ms),
                    SortKey::ModifiedDate => e.modified_unix_ms,
                    _ => None,
                },
            })
            .collect();
        let by_name = |a: &Item, b: &Item| natural_cmp(&a.name, &b.name).then(a.id.cmp(&b.id));
        let directed = |ord: Ordering| if descending { ord.reverse() } else { ord };
        items.sort_by(|a, b| match key {
            SortKey::FileName => directed(by_name(a, b)),
            SortKey::ModifiedDate | SortKey::CaptureDate => match (a.date, b.date) {
                (Some(x), Some(y)) => directed(x.cmp(&y)).then_with(|| by_name(a, b)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => by_name(a, b),
            },
            SortKey::Rating => directed(a.rating.cmp(&b.rating)).then_with(|| by_name(a, b)),
            SortKey::Iso | SortKey::Ev100 => match (a.value, b.value) {
                (Some(x), Some(y)) => directed(x.total_cmp(&y)).then_with(|| by_name(a, b)),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => by_name(a, b),
            },
        });
        items.into_iter().map(|i| i.id).collect()
    }

    /// Writes the current state (images, graph, folders, revision and history,
    /// schema 2) into one SQLite file, replacing its previous content, without
    /// a new revision and without any conflict check (see [`Project::commit`]).
    /// An existing file of this project in the current schema is updated in
    /// place in one transaction (only what differs is written); any other
    /// target gets a complete new file that is written next to it and swapped
    /// in, so a failure leaves an existing project file untouched. An existing
    /// file of another schema version is first copied to `<name>.v<N>.bak`.
    pub fn save(&self, path: &Path) -> Result<(), ProjectError> {
        store::save(self, path)
    }

    /// Rolls an interrupted save back (see the module notes in `docs/project-format.md`):
    /// loading never replays a leftover `<name>-journal`, it fails with a message that
    /// names it. This opens the file read-write once so SQLite restores the last
    /// complete state. Call it only when the user asks for it, on the computer that
    /// saved the file, while no program is saving it. Returns whether a journal was found.
    pub fn recover_unfinished_save(path: &Path) -> Result<bool, ProjectError> {
        store::recover_unfinished_save(path)
    }

    /// Like [`Project::load`], but an interrupted save next to the file is dealt with when
    /// only this computer writes the file (write mode "in place": a local fixed drive outside
    /// sync folders): the save is rolled back first and `true` is returned. For network drives
    /// and sync folders the error `UnfinishedSave` is returned as `load` does, so the caller can
    /// ask the user (the file may be in use by another computer).
    pub fn load_recovering(path: &Path) -> Result<(Project, bool), ProjectError> {
        match store::load(path) {
            Err(ProjectError::UnfinishedSave(_)) if writemode::write_mode(path).mode == WriteMode::InPlace => {
                store::recover_unfinished_save(path)?;
                Ok((store::load(path)?, true))
            }
            other => other.map(|p| (p, false)),
        }
    }

    /// Loads a project file. Schema 1 files are migrated in memory (the file
    /// is not modified; the first write over it keeps a `.v1.bak` copy).
    pub fn load(path: &Path) -> Result<Project, ProjectError> {
        store::load(path)
    }

    /// Saves like [`Project::save`], but as a new revision with a history
    /// entry, and only if nobody else committed in between: a file of this
    /// project with a higher revision than [`Project::revision`] gives
    /// [`ProjectError::Conflict`], a file of another project gives
    /// [`ProjectError::Mismatch`]; nothing is written then. A missing target
    /// is simply created. An existing file of this project in the current
    /// schema is updated in place, in one transaction (rollback journal): the
    /// conflict check runs inside that transaction under the write lock, so
    /// check and write are one atomic step. Any other existing target is
    /// replaced by a complete new file that is swapped in atomically. If the
    /// write fails the file keeps its old content and the project stays
    /// unchanged. Returns the new revision.
    pub fn commit(&mut self, path: &Path, summary: &str) -> Result<u64, ProjectError> {
        self.commit_impl(path, summary, false)
    }

    /// Like [`Project::commit`] without the conflict and project checks: the
    /// target is replaced even if it holds a newer revision or another project.
    pub fn commit_overwrite(&mut self, path: &Path, summary: &str) -> Result<u64, ProjectError> {
        self.commit_impl(path, summary, true)
    }

    /// The history with the entry of `revision` appended (limits applied).
    fn log_with_commit(&self, revision: u64, summary: &str) -> history::HistoryLog {
        let mut log = self.history.clone();
        log.push(
            HistoryEntry {
                revision,
                time_unix_ms: ids::now_ms(),
                author: ids::author_name(),
                device: ids::device_name(),
                summary: summary.to_string(),
                has_graph: true,
            },
            self.graph.to_json(),
            history::MAX_HISTORY_ENTRIES,
            history::MAX_SNAPSHOT_BYTES,
        );
        log
    }

    /// How the last [`Project::commit`] really wrote the file (not how it would have liked to):
    /// in place, or the whole file with the reason (new file, older format, another project's
    /// file, a change that cannot be made row by row, or the place does not allow in place).
    pub fn last_write(&self) -> Option<&WriteChoice> {
        self.last_write.as_ref()
    }

    /// The check made right before a full write swaps the file in: the file may have been
    /// committed to since the first look. (Full writes are still not atomic between computers:
    /// a commit by somebody else between this look and the rename is lost, the last writer wins.)
    fn recheck_before_swap(&self, path: &Path, overwrite: bool) -> Result<(), ProjectError> {
        if overwrite || !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
            return Ok(());
        }
        let d = store::read_identity(path)?;
        if d.version > SCHEMA_VERSION {
            return Err(ProjectError::TooNew { file_version: d.version, supported: SCHEMA_VERSION });
        }
        match d.uuid {
            Some(u) if u == self.uuid && d.revision > self.revision => {
                Err(ProjectError::Conflict { disk_revision: d.revision, loaded_revision: self.revision })
            }
            Some(u) if u != self.uuid => Err(ProjectError::Mismatch(format!(
                "{} belongs to another project (use commit_overwrite to replace it)",
                path.display()
            ))),
            _ => Ok(()),
        }
    }

    fn commit_impl(&mut self, path: &Path, summary: &str, overwrite: bool) -> Result<u64, ProjectError> {
        let mut base = self.revision;
        let meta = std::fs::metadata(path);
        let choice = writemode::write_mode(path);
        let mut why_whole = if meta.is_err() { "new file".to_owned() } else { choice.reason.clone() };
        if meta.as_ref().is_ok_and(|m| m.is_file()) && choice.mode == WriteMode::InPlace {
            // Fast path: the file is a current-schema file of this project. It is
            // updated in place in one transaction, and the conflict check runs
            // inside that transaction (atomic with the write).
            let own = self.revision;
            let outcome = store::update_in_place(self, path, &mut |disk| {
                if !overwrite && disk.revision > own {
                    return Err(ProjectError::Conflict { disk_revision: disk.revision, loaded_revision: own });
                }
                let base = if overwrite { own.max(disk.revision) } else { own };
                let revision = base.checked_add(1).ok_or_else(|| ProjectError::Database("revision overflow".into()))?;
                Ok((revision, self.log_with_commit(revision, summary)))
            })?;
            match outcome {
                store::InPlace::Written { revision, log } => {
                    self.revision = revision;
                    self.history = log;
                    self.migrated_from = None;
                    self.last_write = Some(choice);
                    return Ok(revision);
                }
                store::InPlace::NotApplicable(reason) => why_whole = reason.to_owned(),
            }
        }
        match meta {
            Ok(m) if m.is_file() => {
                let disk = store::read_identity(path);
                if overwrite {
                    if let Ok(d) = disk {
                        if d.uuid.as_deref() == Some(self.uuid.as_str()) {
                            base = base.max(d.revision);
                        }
                    }
                } else {
                    let d = disk?;
                    if d.version > SCHEMA_VERSION {
                        return Err(ProjectError::TooNew { file_version: d.version, supported: SCHEMA_VERSION });
                    }
                    match d.uuid {
                        Some(u) if u == self.uuid => {
                            if d.revision > self.revision {
                                return Err(ProjectError::Conflict {
                                    disk_revision: d.revision,
                                    loaded_revision: self.revision,
                                });
                            }
                        }
                        Some(_) => {
                            return Err(ProjectError::Mismatch(format!(
                                "{} belongs to another project (use commit_overwrite to replace it)",
                                path.display()
                            )));
                        }
                        // A schema 1 file has no project id: accept it only for a
                        // project that was migrated from such a file.
                        None if self.migrated_from.is_some() => {}
                        None => {
                            return Err(ProjectError::Mismatch(format!(
                                "{} is an old project file that this project was not loaded from (use commit_overwrite to replace it)",
                                path.display()
                            )));
                        }
                    }
                }
            }
            Ok(_) => return Err(ProjectError::Io(format!("{} is not a file", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ProjectError::Io(format!("{}: {e}", path.display()))),
        }

        let revision = base.checked_add(1).ok_or_else(|| ProjectError::Database("revision overflow".into()))?;
        let log = self.log_with_commit(revision, summary);
        let guard = || self.recheck_before_swap(path, overwrite);
        store::write_file(self, &store::Versioning { revision, log: &log }, path, &[], true, Some(&guard))?;
        self.revision = revision;
        self.history = log;
        self.migrated_from = None;
        self.last_write = Some(WriteChoice::new(WriteMode::FullRewrite, why_whole));
        Ok(revision)
    }
}

fn modified_ms(meta: &std::fs::Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).ok(),
        Err(e) => i64::try_from(e.duration().as_millis()).ok().map(|v| -v),
    }
}

/// Supported files below `folder`, unsorted. Symlinked directories are not followed.
fn collect_files_with(
    folder: &Path,
    recursive: bool,
    is_supported: &(dyn Fn(&Path) -> bool + Sync),
    failed: &mut Vec<(PathBuf, String)>,
    counting: &mut dyn FnMut(usize) -> bool,
) -> Vec<(PathBuf, std::fs::Metadata)> {
    let mut files = Vec::new();
    let mut stack = vec![folder.to_path_buf()];
    let mut checked = 0;
    if !counting(0) { return files; }
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                failed.push((dir, e.to_string()));
                continue;
            }
        };
        for entry in entries {
            checked += 1;
            if checked % 256 == 0 && !counting(files.len()) { return files; }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    failed.push((dir.clone(), e.to_string()));
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(e) => {
                    failed.push((path, e.to_string()));
                    continue;
                }
            };
            if file_type.is_dir() {
                if recursive { stack.push(path); }
                continue;
            }
            // On Windows the directory listing already carries size and mtime,
            // so `entry.metadata()` costs no extra round trip to the NAS.
            let meta = if file_type.is_symlink() { std::fs::metadata(&path) } else { entry.metadata() };
            let meta = match meta {
                Ok(m) => m,
                Err(e) => {
                    failed.push((path, e.to_string()));
                    continue;
                }
            };
            if meta.is_file() && is_supported(&path) {
                files.push((path, meta));
            }
        }
    }
    files
}
