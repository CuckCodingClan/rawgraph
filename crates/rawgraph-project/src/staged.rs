//! Directory-only catalog discovery, followed by independently scheduled file work.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use rawgraph_core::{CaptureInfo, ImageId};

use crate::{ids, modified_ms, util, Flag, ImageEntry, Project, ScanReport};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Listing {
    Counting(usize),
    Count(usize),
}

pub struct CatalogScan {
    pub entries: Vec<ImageEntry>,
    pub report: ScanReport,
    pub hashes: Vec<HashJob>,
    pub metadata: Vec<ImageId>,
    candidates: HashMap<(u64, String), Vec<ImageId>>,
}

pub struct HashJob {
    pub path: PathBuf,
    pub id: Option<ImageId>,
    size: u64,
    mtime: Option<i64>,
    previous: Option<(u64, String, CaptureInfo)>,
}

/// Job indices in visible-first order. Reprioritizing never queues a job twice.
pub struct HashQueue {
    remaining: HashSet<usize>,
    natural: VecDeque<usize>,
    priority: VecDeque<usize>,
    by_id: HashMap<ImageId, usize>,
}

impl HashQueue {
    pub fn new(jobs: &[HashJob]) -> Self {
        Self {
            remaining: (0..jobs.len()).collect(),
            natural: (0..jobs.len()).collect(),
            priority: VecDeque::new(),
            by_id: jobs.iter().enumerate().filter_map(|(i, job)| Some((job.id?, i))).collect(),
        }
    }

    pub fn prioritize(&mut self, wanted: &[ImageId]) {
        self.priority = wanted.iter().filter_map(|id| self.by_id.get(id).copied()).collect();
    }

    pub fn take(&mut self) -> Option<usize> {
        while let Some(i) = self.priority.pop_front().or_else(|| self.natural.pop_front()) {
            if self.remaining.remove(&i) {
                return Some(i);
            }
        }
        None
    }
}

impl Project {
    /// Publishes the count before sorting and assembling the catalog. No file
    /// contents are opened here. Only new paths that may be relinks have their
    /// identity deferred; no provisional ID can escape into the graph.
    pub fn list_catalog(
        &mut self,
        folder: &Path,
        recursive: bool,
        is_supported: &(dyn Fn(&Path) -> bool + Sync),
        notify: &mut dyn FnMut(Listing) -> bool,
    ) -> CatalogScan {
        let mut scan = CatalogScan {
            entries: Vec::new(), report: ScanReport::default(), hashes: Vec::new(),
            metadata: Vec::new(), candidates: HashMap::new(),
        };
        let folder = match std::path::absolute(folder) {
            Ok(path) => path,
            Err(e) => {
                scan.report.failed.push((folder.to_owned(), e.to_string()));
                return scan;
            }
        };
        let mut cancelled = false;
        let mut files = crate::collect_files_with(&folder, recursive, is_supported, &mut scan.report.failed, &mut |n| {
            cancelled = !notify(Listing::Counting(n));
            !cancelled
        });
        if cancelled || !notify(Listing::Count(files.len())) {
            scan.report.cancelled = true;
            return scan;
        }
        if folder.is_dir() {
            self.note_folder(&folder, recursive);
        }
        util::sort_under(&folder, &mut files);
        let keys: HashSet<_> = files.iter().map(|(path, _)| util::path_key(path)).collect();
        let by_path: HashMap<_, _> = self.images.iter().map(|e| (util::path_key(&e.path), e.id)).collect();
        let new_sizes: HashSet<_> = files.iter().filter(|(p, _)| !by_path.contains_key(&util::path_key(p))).map(|(_, m)| m.len()).collect();
        let mut candidate_sizes = HashSet::new();
        for e in &self.images {
            if !e.content_hash.is_empty() && new_sizes.contains(&e.file_size) && !keys.contains(&util::path_key(&e.path))
                && std::fs::metadata(&e.path).is_err_and(|err| err.kind() == std::io::ErrorKind::NotFound)
            {
                candidate_sizes.insert(e.file_size);
                scan.candidates.entry((e.file_size, e.content_hash.clone())).or_default().push(e.id);
            }
        }
        for ids in scan.candidates.values_mut() { ids.reverse(); }
        for (path, meta) in files {
            let size = meta.len();
            let mtime = modified_ms(&meta);
            let key = util::path_key(&path);
            if let Some(&id) = by_path.get(&key) {
                let e = self.image_mut(id).expect("listed catalog id");
                let same_stat = size == e.file_size && mtime.is_some() && mtime == e.modified_unix_ms;
                e.missing = false;
                if same_stat && !e.content_hash.is_empty() {
                    if e.capture.scan_pending {
                        scan.metadata.push(id);
                    } else {
                        scan.report.unchanged += 1;
                    }
                } else {
                    let previous = (!e.content_hash.is_empty()).then(|| (e.file_size, std::mem::take(&mut e.content_hash), e.capture.clone()));
                    e.file_size = size;
                    e.modified_unix_ms = mtime;
                    e.capture = pending_capture();
                    scan.hashes.push(HashJob { path, id: Some(id), size, mtime, previous });
                }
                scan.entries.push(e.clone());
            } else if candidate_sizes.contains(&size) {
                scan.hashes.push(HashJob { path, id: None, size, mtime, previous: None });
            } else {
                let e = self.add_pending(path.clone(), size, mtime);
                scan.report.added.push(e.id);
                scan.hashes.push(HashJob { path, id: Some(e.id), size, mtime, previous: None });
                scan.entries.push(e);
            }
        }
        scan
    }

    fn add_pending(&mut self, path: PathBuf, size: u64, mtime: Option<i64>) -> ImageEntry {
        let id = self.alloc_id();
        let e = ImageEntry {
            id, uuid: ids::new_uuid(), path, file_size: size, modified_unix_ms: mtime,
            content_hash: String::new(), capture: pending_capture(), rating: 0,
            flag: Flag::None, color_label: None, keywords: Vec::new(), missing: false,
        };
        self.images.push(e.clone());
        e
    }

    pub fn finish_capture(&mut self, id: ImageId, capture: Option<CaptureInfo>) -> Option<ImageEntry> {
        let e = self.image_mut(id)?;
        e.capture = capture.unwrap_or_default();
        e.capture.scan_pending = false;
        Some(e.clone())
    }
}

impl CatalogScan {
    pub fn finish_hash(&mut self, project: &mut Project, index: usize, result: std::io::Result<(String, u64)>) -> Option<ImageEntry> {
        let job = &self.hashes[index];
        let (hash, size) = match result {
            Ok((hash, size)) if size == job.size => (hash, size),
            bad => {
                self.report.failed.push((job.path.clone(), bad.err().map_or_else(|| "file size changed during scan".to_owned(), |e| e.to_string())));
                return None;
            }
        };
        if job.id.is_none() {
            if let Some(id) = self.candidates.get_mut(&(size, hash.clone())).and_then(Vec::pop) {
                let e = project.image_mut(id).expect("relink candidate");
                e.path.clone_from(&job.path);
                e.modified_unix_ms = job.mtime;
                e.missing = false;
                self.report.relinked.push(id);
                if e.capture.scan_pending {
                    self.metadata.push(id);
                }
                return Some(e.clone());
            }
        }
        let id = match job.id {
            Some(id) => id,
            None => {
                let e = project.add_pending(job.path.clone(), size, job.mtime);
                self.report.added.push(e.id);
                e.id
            }
        };
        let e = project.image_mut(id).expect("pending scan id");
        if let Some((old_size, old_hash, capture)) = &job.previous {
            if *old_size == size && *old_hash == hash {
                self.report.unchanged += 1;
                e.capture.clone_from(capture);
            } else {
                self.report.updated.push(id);
            }
        }
        e.content_hash = hash;
        if e.capture.scan_pending {
            self.metadata.push(id);
        }
        Some(e.clone())
    }
}

fn pending_capture() -> CaptureInfo {
    CaptureInfo { scan_pending: true, ..Default::default() }
}
