//! Metadata refresh and manual relinking.

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use rawgraph_core::{CaptureInfo, ImageId};

use crate::{modified_ms, util, Project, ProjectError};

fn abs(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Components of `path` below `prefix` (compared like path identity), if any.
fn strip_prefix_ci(path: &Path, prefix: &Path) -> Option<PathBuf> {
    let key = |c: &Component<'_>| util::path_key(Path::new(c.as_os_str()));
    let mut pc = path.components();
    for p in prefix.components() {
        if key(&pc.next()?) != key(&p) {
            return None;
        }
    }
    Some(pc.as_path().to_path_buf())
}

impl Project {
    /// Re-reads capture metadata for the given entries on up to
    /// [`Project::scan_threads`] threads. Missing entries and unknown ids are
    /// skipped; `None` from the reader leaves an entry unchanged. Returns how
    /// many entries changed.
    pub fn refresh_capture(
        &mut self,
        ids: &[ImageId],
        read_capture: &(dyn Fn(&Path) -> Option<CaptureInfo> + Sync),
    ) -> usize {
        let mut seen = std::collections::HashSet::new();
        let jobs: Vec<(ImageId, PathBuf)> = ids
            .iter()
            .filter(|id| seen.insert(**id))
            .filter_map(|id| self.image(*id))
            .filter(|e| !e.missing)
            .map(|e| (e.id, e.path.clone()))
            .collect();
        if jobs.is_empty() {
            return 0;
        }
        let workers = self.scan_threads.clamp(1, 32).min(jobs.len());
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(ImageId, CaptureInfo)>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    let j = next.fetch_add(1, Ordering::Relaxed);
                    let Some((id, path)) = jobs.get(j) else { break };
                    if let Some(info) = read_capture(path) {
                        if let Ok(mut r) = results.lock() {
                            r.push((*id, info));
                        }
                    }
                });
            }
        });
        let results = results.into_inner().unwrap_or_default();
        let mut changed = 0;
        for (id, info) in results {
            if let Some(entry) = self.image_mut(id) {
                if entry.capture != info {
                    entry.capture = info;
                    changed += 1;
                }
            }
        }
        changed
    }

    fn path_used_by_other(&self, id: ImageId, path: &Path) -> bool {
        let key = util::path_key(path);
        self.images().iter().any(|e| e.id != id && util::path_key(&e.path) == key)
    }

    /// Points an entry at another file. Without `force` the new file's size
    /// and content hash must equal the entry's ([`ProjectError::Mismatch`]
    /// otherwise); with `force` the entry takes the new file's size, hash and
    /// modification time. A path used by another entry is refused. Id, rating,
    /// flag, label and capture are kept and `missing` is cleared.
    pub fn relink(&mut self, id: ImageId, new_path: &Path, force: bool) -> Result<(), ProjectError> {
        let Some(entry) = self.image(id) else {
            return Err(ProjectError::Mismatch(format!("no image with id {}", id.0)));
        };
        let (old_size, old_hash) = (entry.file_size, entry.content_hash.clone());
        let new_path = abs(new_path);
        let meta = std::fs::metadata(&new_path)
            .map_err(|e| ProjectError::Io(format!("{}: {e}", new_path.display())))?;
        if !meta.is_file() {
            return Err(ProjectError::Io(format!("{} is not a file", new_path.display())));
        }
        if self.path_used_by_other(id, &new_path) {
            return Err(ProjectError::Mismatch(format!(
                "{} is already used by another image of the project",
                new_path.display()
            )));
        }
        let (hash, size) = util::content_hash(&new_path)
            .map_err(|e| ProjectError::Io(format!("{}: {e}", new_path.display())))?;
        // An entry whose scan was interrupted before hashing has only its size to compare.
        if !force && (size != old_size || (!old_hash.is_empty() && hash != old_hash)) {
            return Err(ProjectError::Mismatch(format!(
                "{} differs from the original file (size {size} bytes instead of {old_size}, or different content)",
                new_path.display()
            )));
        }
        let mtime = modified_ms(&meta);
        let Some(entry) = self.image_mut(id) else {
            return Err(ProjectError::Mismatch(format!("no image with id {}", id.0)));
        };
        entry.path = new_path;
        entry.file_size = size;
        entry.content_hash = hash;
        entry.modified_unix_ms = mtime;
        entry.missing = false;
        Ok(())
    }

    /// Rewrites the paths of all entries under `old_prefix` to `new_prefix`
    /// (for a moved job folder), only for entries whose new path exists and is
    /// not used by another entry. Nothing is verified; `missing` is cleared
    /// for the rewritten entries. Returns how many entries were rewritten.
    pub fn relink_folder(&mut self, old_prefix: &Path, new_prefix: &Path) -> usize {
        let old_prefix = abs(old_prefix);
        let new_prefix = abs(new_prefix);
        let mut used: std::collections::HashSet<String> =
            self.images.iter().map(|e| util::path_key(&e.path)).collect();
        let mut count = 0;
        for entry in &mut self.images {
            let Some(rel) = strip_prefix_ci(&entry.path, &old_prefix) else { continue };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let new_path = new_prefix.join(rel);
            if !new_path.is_file() {
                continue;
            }
            let new_key = util::path_key(&new_path);
            if new_key != util::path_key(&entry.path) && used.contains(&new_key) {
                continue;
            }
            used.remove(&util::path_key(&entry.path));
            used.insert(new_key);
            entry.path = new_path;
            entry.missing = false;
            count += 1;
        }
        count
    }
}
