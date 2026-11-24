//! On-disk thumbnail cache keyed by content hash and size. The bytes are opaque.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::ProjectError;

const EXT: &str = "thumb";
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Least-recently-used file cache with a byte budget. Safe to share between
/// threads. Last use is tracked through the files' modification times.
#[derive(Debug)]
pub struct ThumbCache {
    dir: PathBuf,
    max_bytes: u64,
    /// Running estimate of the stored bytes, used to trigger eviction.
    approx_bytes: AtomicU64,
}

fn io_err(path: &Path, e: std::io::Error) -> ProjectError {
    ProjectError::Io(format!("{}: {e}", path.display()))
}

impl ThumbCache {
    pub fn new(dir: &Path, max_bytes: u64) -> Result<Self, ProjectError> {
        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        let cache = Self { dir: dir.to_path_buf(), max_bytes, approx_bytes: AtomicU64::new(0) };
        cache.total_bytes();
        Ok(cache)
    }

    /// File name from the hash (hex only) and size; never from other text.
    fn file_for(&self, content_hash: &str, size: u32) -> Option<PathBuf> {
        let ok = !content_hash.is_empty()
            && content_hash.len() <= 128
            && content_hash.bytes().all(|b| b.is_ascii_hexdigit());
        ok.then(|| self.dir.join(format!("{}_{size}.{EXT}", content_hash.to_ascii_lowercase())))
    }

    fn entries(&self) -> Vec<(PathBuf, u64, SystemTime)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        rd.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == EXT))
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                m.is_file().then(|| (e.path(), m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
            })
            .collect()
    }

    /// Cached bytes, if present; also refreshes the last-used time.
    pub fn get(&self, content_hash: &str, size: u32) -> Option<Vec<u8>> {
        let path = self.file_for(content_hash, size)?;
        let bytes = std::fs::read(&path).ok()?;
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(bytes)
    }

    /// Stores bytes atomically. Evicts when the budget is exceeded.
    pub fn put(&self, content_hash: &str, size: u32, bytes: &[u8]) -> Result<(), ProjectError> {
        let path = self
            .file_for(content_hash, size)
            .ok_or_else(|| ProjectError::Corrupt(format!("invalid content hash \"{content_hash}\"")))?;
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!("tmp-{}-{n}.part", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|e| io_err(&tmp, e))?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            // Another thread may hold the target open; the content for a key is
            // the same, so an existing file is as good as ours.
            if !path.is_file() {
                return Err(io_err(&path, e));
            }
        }
        let total = self.approx_bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
        if total > self.max_bytes {
            self.evict();
        }
        Ok(())
    }

    /// Removes the least recently used files until the cache fits its budget.
    /// Returns the bytes freed.
    pub fn evict(&self) -> u64 {
        let mut entries = self.entries();
        let mut total: u64 = entries.iter().map(|e| e.1).sum();
        let mut freed = 0;
        if total > self.max_bytes {
            entries.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
            for (path, len, _) in entries {
                if total <= self.max_bytes {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    total -= len;
                    freed += len;
                }
            }
        }
        self.approx_bytes.store(total, Ordering::Relaxed);
        freed
    }

    pub fn total_bytes(&self) -> u64 {
        let total: u64 = self.entries().iter().map(|e| e.1).sum();
        self.approx_bytes.store(total, Ordering::Relaxed);
        total
    }
}
