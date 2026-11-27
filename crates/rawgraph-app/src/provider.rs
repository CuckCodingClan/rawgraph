//! File-backed image provider: ImageId -> path -> decoded proxy, with a
//! byte-bounded LRU cache. Decoding happens on the calling thread, which is
//! always the evaluation worker or the prefetch thread, never the UI thread.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use rawgraph_core::{CaptureInfo, ImageBuf, ImageId, LensProfile};
use rawgraph_engine::ImageProvider;


pub const DEFAULT_CACHE_BYTES: usize = 4 << 30;
pub const PREVIEW_SIZES: [u32; 4] = [1280, 1920, 2560, 3840];
pub const DEFAULT_PREVIEW: u32 = 2560;
/// Paths of the built-in test images look like `demo:1`.
pub const DEMO_PREFIX: &str = "demo:";

/// LRU cache bounded by bytes. An entry that is still referenced elsewhere
/// (`Arc::strong_count > 1`, e.g. by the evaluator) is never evicted, and
/// neither is the entry that was just inserted.
pub struct LruBytes<K> {
    budget: usize,
    used: usize,
    tick: u64,
    map: HashMap<K, Slot>,
}

struct Slot {
    value: Arc<ImageBuf>,
    bytes: usize,
    used_at: u64,
}

pub fn image_bytes(img: &ImageBuf) -> usize {
    std::mem::size_of_val(img.data())
}

impl<K: std::hash::Hash + Eq + Clone> LruBytes<K> {
    pub fn new(budget: usize) -> Self {
        Self { budget, used: 0, tick: 0, map: HashMap::new() }
    }

    pub fn get(&mut self, key: &K) -> Option<Arc<ImageBuf>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|s| {
            s.used_at = tick;
            s.value.clone()
        })
    }

    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    pub fn insert(&mut self, key: K, value: Arc<ImageBuf>) {
        self.tick += 1;
        let bytes = image_bytes(&value);
        if let Some(old) = self.map.insert(key.clone(), Slot { value, bytes, used_at: self.tick }) {
            self.used -= old.bytes;
        }
        self.used += bytes;
        while self.used > self.budget {
            let victim = self
                .map
                .iter()
                .filter(|(k, s)| **k != key && Arc::strong_count(&s.value) == 1)
                .min_by_key(|(_, s)| s.used_at)
                .map(|(k, _)| k.clone());
            let Some(victim) = victim else { break };
            if let Some(s) = self.map.remove(&victim) {
                self.used -= s.bytes;
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.used = 0;
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

}

type Key = (ImageId, u32);

struct Cache {
    lru: LruBytes<Key>,
    inflight: HashSet<Key>,
}

pub struct FileProvider {
    paths: RwLock<HashMap<ImageId, PathBuf>>,
    captures: RwLock<HashMap<ImageId, CaptureInfo>>,
    /// Colour profile of every decoded rendered source (kept with the proxy).
    profiles: RwLock<HashMap<ImageId, rawgraph_io::InputProfile>>,
    /// Full-resolution (oriented) size of every decoded file, even though only a proxy is kept.
    full_sizes: RwLock<HashMap<ImageId, (u32, u32)>>,
    /// Every image in project order (the catalog the collection queries run against).
    order: RwLock<Vec<ImageId>>,
    /// Lens profile of the file of every image that has been asked about (`None`: the file has none).
    lens: RwLock<HashMap<ImageId, Option<LensProfile>>>,
    /// Images whose lens profile a background read is working on.
    lens_inflight: Mutex<HashSet<ImageId>>,
    cache: Mutex<Cache>,
    done: Condvar,
    preview: AtomicU32,
    epoch: AtomicU64,
    demo: crate::session::TestImages,
}

impl FileProvider {
    pub fn new(cache_bytes: usize) -> Self {
        Self {
            paths: RwLock::new(HashMap::new()),
            captures: RwLock::new(HashMap::new()),
            profiles: RwLock::new(HashMap::new()),
            full_sizes: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            lens: RwLock::new(HashMap::new()),
            lens_inflight: Mutex::new(HashSet::new()),
            cache: Mutex::new(Cache { lru: LruBytes::new(cache_bytes), inflight: HashSet::new() }),
            done: Condvar::new(),
            preview: AtomicU32::new(DEFAULT_PREVIEW),
            epoch: AtomicU64::new(0),
            demo: crate::session::TestImages::new(),
        }
    }

    pub fn preview_size(&self) -> u32 {
        self.preview.load(Ordering::Relaxed)
    }

    pub fn set_preview_size(&self, n: u32) {
        let n = n.clamp(256, 8192);
        if self.preview.swap(n, Ordering::Relaxed) != n {
            if let Ok(mut c) = self.cache.lock() {
                c.lru.clear();
            }
        }
    }

    /// Registers or replaces id -> path mappings. A changed path for a known id
    /// invalidates everything derived from the old file.
    pub fn set_paths(&self, items: impl IntoIterator<Item = (ImageId, PathBuf)>) {
        let Ok(mut map) = self.paths.write() else { return };
        let mut changed = false;
        let mut added = Vec::new();
        for (id, path) in items {
            if map.get(&id).is_some_and(|old| *old != path) {
                changed = true;
            }
            if map.insert(id, path).is_none() {
                added.push(id);
            }
        }
        drop(map);
        if !added.is_empty() {
            if let Ok(mut order) = self.order.write() {
                order.extend(added);
            }
        }
        if changed {
            self.forget_lens();
            self.epoch.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut c) = self.cache.lock() {
                c.lru.clear();
            }
            if let Ok(mut p) = self.profiles.write() {
                p.clear();
            }
            if let Ok(mut p) = self.full_sizes.write() {
                p.clear();
            }
        }
    }

    /// Pixel size of the file at full resolution (orientation applied), once it was decoded.
    pub fn full_size_of(&self, id: ImageId) -> Option<(u32, u32)> {
        self.full_sizes.read().ok()?.get(&id).copied()
    }

    fn forget_lens(&self) {
        if let Ok(mut l) = self.lens.write() {
            l.clear();
        }
    }

    /// The lens profile of `id` if it has been read already: `Some(None)` means the file has none,
    /// `None` that nobody has looked yet. Never touches the disk, so the UI thread may call it.
    pub fn lens_known(&self, id: ImageId) -> Option<Option<LensProfile>> {
        self.lens.read().ok()?.get(&id).cloned()
    }

    /// Reads the lens profile of `id` on a background thread (once) and repaints when it is there.
    pub fn request_lens(self: &Arc<Self>, ctx: &eframe::egui::Context, id: ImageId) {
        if self.lens_known(id).is_some() {
            return;
        }
        let Ok(mut inflight) = self.lens_inflight.lock() else { return };
        if !inflight.insert(id) {
            return;
        }
        drop(inflight);
        let (this, ctx) = (self.clone(), ctx.clone());
        let spawned = std::thread::Builder::new().name("rawgraph-lens".into()).spawn(move || {
            let _ = this.lens_blocking(id);
            if let Ok(mut inflight) = this.lens_inflight.lock() {
                inflight.remove(&id);
            }
            ctx.request_repaint();
        });
        if spawned.is_err() {
            if let Ok(mut inflight) = self.lens_inflight.lock() {
                inflight.remove(&id);
            }
        }
    }

    /// Reads (or recalls) the lens profile of `id`. Reads the file: only for worker threads.
    fn lens_blocking(&self, id: ImageId) -> Option<LensProfile> {
        if let Some(known) = self.lens_known(id) {
            return known;
        }
        let path = self.path_of(id)?;
        let profile = if path.to_string_lossy().starts_with(DEMO_PREFIX) || !path.is_file() { None } else { rawgraph_io::read_lens_profile(&path) };
        if let Ok(mut l) = self.lens.write() {
            l.insert(id, profile.clone());
        }
        profile
    }

    /// How the embedded colour profile of `id` was treated, once the file was decoded.
    pub fn profile_of(&self, id: ImageId) -> Option<rawgraph_io::InputProfile> {
        self.profiles.read().ok()?.get(&id).cloned()
    }

    /// Registers capture metadata (used by Metadata Filter nodes while evaluating).
    /// A changed capture changes what Develop and the filters compute, so the
    /// revision moves on (cached evaluations are recomputed).
    pub fn set_captures(&self, items: impl IntoIterator<Item = (ImageId, CaptureInfo)>) {
        let mut changed = false;
        if let Ok(mut map) = self.captures.write() {
            for (id, c) in items {
                if map.get(&id).is_some_and(|old| *old != c) {
                    changed = true;
                }
                map.insert(id, c);
            }
        }
        if changed {
            self.epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drops all mappings (new project).
    pub fn reset_paths(&self) {
        if let Ok(mut map) = self.paths.write() {
            map.clear();
        }
        if let Ok(mut order) = self.order.write() {
            order.clear();
        }
        self.forget_lens();
        if let Ok(mut map) = self.captures.write() {
            map.clear();
        }
        if let Ok(mut map) = self.profiles.write() {
            map.clear();
        }
        if let Ok(mut map) = self.full_sizes.write() {
            map.clear();
        }
        self.epoch.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut c) = self.cache.lock() {
            c.lru.clear();
        }
    }

    pub fn path_of(&self, id: ImageId) -> Option<PathBuf> {
        self.paths.read().ok()?.get(&id).cloned()
    }

    pub fn cached_proxy(&self, id: ImageId) -> Option<Arc<ImageBuf>> {
        self.cache.lock().ok()?.lru.get(&(id, self.preview_size()))
    }

    /// Speculative work never claims a foreground inflight slot. If a worker
    /// needs the same uncached image, it can start immediately on its own thread.
    pub fn prefetch(&self, id: ImageId) {
        self.prefetch_with(id, |path, preview| self.decode(id, path, preview));
    }

    fn prefetch_with(&self, id: ImageId, decode: impl FnOnce(&Path, u32) -> Result<Arc<ImageBuf>, String>) {
        let Some(path) = self.path_of(id) else { return };
        let preview = self.preview_size();
        let key = (id, preview);
        if self.cache.lock().is_ok_and(|c| c.lru.contains(&key) || c.inflight.contains(&key)) { return; }
        let epoch = self.epoch.load(Ordering::Relaxed);
        if let Ok(img) = decode(&path, preview) {
            if let Ok(mut c) = self.cache.lock() {
                if self.epoch.load(Ordering::Relaxed) == epoch && !c.lru.contains(&key) {
                    c.lru.insert(key, img);
                }
            }
        }
    }

    /// True if a proxy for `id` at the current preview size is in memory.
    pub fn is_cached(&self, id: ImageId) -> bool {
        let key = (id, self.preview_size());
        self.cache.lock().is_ok_and(|c| c.lru.contains(&key))
    }

    /// Drops every decoded proxy (benchmarks, memory pressure).
    pub fn clear_cache(&self) {
        if let Ok(mut c) = self.cache.lock() {
            c.lru.clear();
        }
    }

    pub fn cached_bytes(&self) -> usize {
        self.cache.lock().map_or(0, |c| c.lru.used_bytes())
    }

    /// A small RGBA8 thumbnail of a built-in demo image (`demo:N` paths).
    pub fn demo_thumbnail(&self, id: ImageId, max_dim: u32) -> Result<(u32, u32, Vec<u8>), String> {
        let img = self.demo.image(id)?;
        let small = img.downscale_box(max_dim);
        let display = rawgraph_scopes::DisplayTransform::default().apply(&small);
        Ok((display.width, display.height, display.to_rgba8()))
    }

    /// Independent thumbnail decode: never takes the viewer proxy's in-flight lock.
    pub(crate) fn thumbnail_base(&self, id: ImageId, path: &Path, size: u32) -> Result<(Arc<ImageBuf>, (u32, u32)), String> {
        if path.to_string_lossy().starts_with(DEMO_PREFIX) {
            let full = self.demo.image(id)?;
            return Ok((Arc::new(full.downscale_box(size)), (full.width(), full.height())));
        }
        rawgraph_io::load(path, Some(size)).map(|l| (Arc::new(l.image), l.full_size)).map_err(|e| e.to_string())
    }

    fn decode(&self, id: ImageId, path: &Path, preview: u32) -> Result<Arc<ImageBuf>, String> {
        if path.to_string_lossy().starts_with(DEMO_PREFIX) {
            let full = self.demo.image(id)?;
            if let Ok(mut m) = self.full_sizes.write() {
                m.insert(id, (full.width(), full.height()));
            }
            return Ok(Arc::new(full.downscale_box(preview)));
        }
        if !path.is_file() {
            return Err(format!("File not found: {}", path.display()));
        }
        rawgraph_io::load(path, Some(preview))
            .map(|l| {
                if let Ok(mut p) = self.profiles.write() {
                    p.insert(id, l.profile);
                }
                if let Ok(mut m) = self.full_sizes.write() {
                    m.insert(id, l.full_size);
                }
                Arc::new(l.image)
            })
            .map_err(|e| format!("{}: {e}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()))
    }
}

impl ImageProvider for FileProvider {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        let path = self.path_of(id).ok_or_else(|| format!("image {} is not part of the project", id.0))?;
        let preview = self.preview_size();
        let key = (id, preview);
        {
            let mut c = self.cache.lock().map_err(|_| "image cache is unavailable".to_owned())?;
            loop {
                if let Some(hit) = c.lru.get(&key) {
                    return Ok(hit);
                }
                if c.inflight.contains(&key) {
                    // Another foreground worker is decoding the same image.
                    c = self.done.wait(c).map_err(|_| "image cache is unavailable".to_owned())?;
                    continue;
                }
                c.inflight.insert(key);
                break;
            }
        }
        let result = self.decode(id, &path, preview);
        if let Ok(mut c) = self.cache.lock() {
            c.inflight.remove(&key);
            if let Ok(img) = &result {
                // Not cached if the preview size changed while decoding.
                if self.preview_size() == preview {
                    c.lru.insert(key, img.clone());
                }
            }
        }
        self.done.notify_all();
        result
    }

    fn capture(&self, id: ImageId) -> Option<CaptureInfo> {
        let mut info = self.captures.read().ok()?.get(&id).cloned()?;
        info.path = self.paths.read().ok()?.get(&id).map(|p| p.to_string_lossy().into_owned());
        Some(info)
    }

    fn images(&self) -> Vec<ImageId> {
        self.order.read().map(|o| o.clone()).unwrap_or_default()
    }

    fn full_size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.full_size_of(id)
    }

    fn lens_profile(&self, id: ImageId) -> Option<LensProfile> {
        self.lens_blocking(id)
    }

    fn revision(&self, _id: ImageId) -> u64 {
        u64::from(self.preview_size()) | (self.epoch.load(Ordering::Relaxed) << 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testimages;

    fn img(w: u32, h: u32) -> Arc<ImageBuf> {
        Arc::new(ImageBuf::new(w, h))
    }

    #[test]
    fn foreground_does_not_wait_for_speculation_of_same_image() {
        let provider = Arc::new(FileProvider::new(32 << 20));
        let id = ImageId(1);
        provider.set_paths([(id, PathBuf::from("demo:1"))]);
        let (started_tx, started) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let p = provider.clone();
        let background = std::thread::spawn(move || p.prefetch_with(id, |_, _| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(img(1, 1))
        }));
        started.recv().unwrap();
        let (done_tx, done) = std::sync::mpsc::channel();
        let p = provider.clone();
        let foreground = std::thread::spawn(move || done_tx.send(p.image(id)).unwrap());
        let result = done.recv_timeout(std::time::Duration::from_secs(5));
        release.send(()).unwrap();
        background.join().unwrap();
        foreground.join().unwrap();
        assert!(result.unwrap().unwrap().width() > 1);
        assert!(provider.cached_proxy(id).unwrap().width() > 1);
    }

    #[test]
    fn lru_evicts_oldest_by_bytes() {
        let one = image_bytes(&ImageBuf::new(10, 10));
        let mut c = LruBytes::new(one * 2 + 1);
        c.insert(1, img(10, 10));
        c.insert(2, img(10, 10));
        assert!(c.get(&1).is_some()); // 1 is now newer than 2
        c.insert(3, img(10, 10));
        assert!(c.contains(&1) && c.contains(&3) && !c.contains(&2));
        assert!(c.used_bytes() <= one * 2 + 1);
    }

    #[test]
    fn lru_never_evicts_the_entry_in_use_or_the_new_one() {
        let one = image_bytes(&ImageBuf::new(10, 10));
        let mut c = LruBytes::new(one);
        c.insert(1, img(10, 10));
        let held = c.get(&1).unwrap(); // in use by someone else
        c.insert(2, img(10, 10));
        assert!(c.contains(&1), "an entry still referenced elsewhere stays");
        assert!(c.contains(&2), "the new entry is never its own victim");
        drop(held);
        c.insert(3, img(10, 10));
        assert!(!c.contains(&1) && c.contains(&3));
    }

    #[test]
    fn provider_maps_ids_to_paths_and_reports_missing_files() {
        let p = FileProvider::new(1 << 30);
        assert!(p.image(ImageId(5)).unwrap_err().contains("not part of the project"));
        let missing = std::env::temp_dir().join("rawgraph-definitely-missing.arw");
        p.set_paths([(ImageId(5), missing.clone())]);
        assert_eq!(p.path_of(ImageId(5)), Some(missing.clone()));
        let err = p.image(ImageId(5)).unwrap_err();
        assert!(err.starts_with("File not found: "), "{err}");
        assert!(err.contains("rawgraph-definitely-missing"));
    }

    #[test]
    fn provider_decodes_caches_and_changes_revision_with_preview_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        image::RgbaImage::from_pixel(64, 48, image::Rgba([200, 100, 50, 255])).save(&path).unwrap();
        let p = FileProvider::new(1 << 30);
        p.set_paths([(ImageId(1), path)]);
        assert!(!p.is_cached(ImageId(1)));
        let a = p.image(ImageId(1)).unwrap();
        assert_eq!((a.width(), a.height()), (64, 48));
        assert!(p.is_cached(ImageId(1)));
        let r1 = p.revision(ImageId(1));
        p.set_preview_size(1280);
        assert_ne!(r1, p.revision(ImageId(1)));
        assert!(!p.is_cached(ImageId(1)), "a new preview size starts with an empty cache");
    }

    #[test]
    fn the_colour_profile_is_kept_with_the_decoded_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255])).save(&path).unwrap();
        let p = FileProvider::new(1 << 30);
        p.set_paths([(ImageId(1), path)]);
        assert_eq!(p.profile_of(ImageId(1)), None, "unknown until the file was decoded");
        p.image(ImageId(1)).unwrap();
        assert_eq!(p.profile_of(ImageId(1)), Some(rawgraph_io::InputProfile::Untagged), "a plain PNG has no profile");
        p.reset_paths();
        assert_eq!(p.profile_of(ImageId(1)), None, "forgotten with the paths");
    }

    #[test]
    fn demo_paths_resolve_to_test_images() {
        let p = FileProvider::new(1 << 30);
        p.set_paths([(ImageId(2), PathBuf::from("demo:2"))]);
        p.set_preview_size(1280);
        let img = p.image(ImageId(2)).unwrap();
        assert_eq!(img.width(), 960, "integer box reduction of 1920 for a 1280 request");
        assert_eq!(testimages::HEIGHT, 1280);
    }
}
