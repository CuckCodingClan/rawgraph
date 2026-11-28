//! Lazy thumbnail loading for the filmstrip: a few background threads decode
//! only what is visible (or nearby); textures live in a bounded cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use rawgraph_core::ImageId;
use rawgraph_project::ThumbCache;

use crate::provider::{FileProvider, DEMO_PREFIX};

pub const THUMB_SIZE: u32 = 256;
pub const MAX_TEXTURES: usize = 500;
const WORKERS: usize = 3;
const UPLOADS_PER_FRAME: usize = 6;
/// JPEG quality of the thumbnails kept on disk.
pub const CACHE_QUALITY: u8 = 85;
/// Size budget of the thumbnail cache on disk.
pub const CACHE_BUDGET: u64 = 1 << 30;

/// One thumbnail the filmstrip wants: the file and the entry's content hash (the disk-cache key).
#[derive(Clone, Debug, PartialEq)]
pub struct Wanted {
    pub id: ImageId,
    pub path: PathBuf,
    pub hash: String,
}

/// What happened to the disk cache for one thumbnail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheEvent {
    /// Served from the disk cache.
    Hit,
    /// Decoded and stored.
    Miss,
    /// The cached entry was unusable; decoded and stored again.
    Replaced,
    /// No cache, or the entry has no usable hash: decoded only.
    Skipped,
}

#[derive(Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub replaced: AtomicU64,
}

impl CacheStats {
    fn note(&self, e: CacheEvent) {
        match e {
            CacheEvent::Hit => self.hits.fetch_add(1, Ordering::Relaxed),
            CacheEvent::Miss => self.misses.fetch_add(1, Ordering::Relaxed),
            CacheEvent::Replaced => self.replaced.fetch_add(1, Ordering::Relaxed),
            CacheEvent::Skipped => 0,
        };
    }

    pub fn snapshot(&self) -> (u64, u64, u64) {
        (self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed), self.replaced.load(Ordering::Relaxed))
    }
}

type Rgba = (u32, u32, Vec<u8>);

/// JPEG bytes (quality [`CACHE_QUALITY`]) of an RGBA thumbnail; alpha is dropped.
pub fn encode_jpeg(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if w == 0 || h == 0 || rgba.len() != w as usize * h as usize * 4 {
        return None;
    }
    let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = Vec::new();
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, CACHE_QUALITY);
    enc.encode(&rgb, w, h, image::ExtendedColorType::Rgb8).ok()?;
    Some(out)
}

/// Decodes cached JPEG bytes; `None` for anything that is not a sane picture.
pub fn decode_jpeg(bytes: &[u8]) -> Option<Rgba> {
    let decoded = std::panic::catch_unwind(|| image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).ok()).ok().flatten()?;
    let rgba = decoded.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    (w > 0 && h > 0).then(|| (w, h, rgba.into_raw()))
}

/// The disk cache in front of a decoder: looks up `(hash, size)`, decodes on a
/// miss and stores the result. A corrupt entry is ignored and replaced.
pub fn fetch_thumb(cache: Option<&ThumbCache>, hash: &str, size: u32, decode: impl FnOnce() -> Result<Rgba, String>) -> (Result<Rgba, String>, CacheEvent) {
    let Some(cache) = cache.filter(|_| !hash.is_empty()) else { return (decode(), CacheEvent::Skipped) };
    let cached = cache.get(hash, size);
    if let Some(t) = cached.as_deref().and_then(decode_jpeg) {
        return (Ok(t), CacheEvent::Hit);
    }
    let event = if cached.is_some() { CacheEvent::Replaced } else { CacheEvent::Miss };
    let result = decode();
    if let Ok((w, h, rgba)) = &result {
        if let Some(bytes) = encode_jpeg(*w, *h, rgba) {
            // A full disk or a locked file only means the next start decodes again.
            let _ = cache.put(hash, size, &bytes);
        }
    }
    (result, event)
}

struct Shared {
    wanted: Mutex<VecDeque<(u64, Wanted)>>,
    wake: Condvar,
    shutdown: AtomicBool,
}

struct Done {
    ticket: u64,
    id: ImageId,
    result: Result<(u32, u32, Vec<u8>), String>,
}

pub struct ThumbLoader {
    shared: Arc<Shared>,
    rx: Receiver<Done>,
    textures: HashMap<ImageId, (TextureHandle, u64)>,
    failed: HashSet<ImageId>,
    in_flight: HashMap<ImageId, u64>,
    ticket: u64,
    tick: u64,
    last_wanted: Vec<ImageId>,
    pub stats: Arc<CacheStats>,
    /// When the first thumbnail was asked for (time to fill the filmstrip is measured from here).
    pub first_request: Option<std::time::Instant>,
}

impl ThumbLoader {
    pub fn with_cache(ctx: egui::Context, provider: Arc<FileProvider>, cache: Option<Arc<ThumbCache>>) -> Self {
        let stats = Arc::new(CacheStats::default());
        let shared = Arc::new(Shared { wanted: Mutex::new(VecDeque::new()), wake: Condvar::new(), shutdown: AtomicBool::new(false) });
        let (tx, rx) = channel();
        for n in 0..WORKERS {
            let (shared, tx, ctx, provider, cache, stats) = (shared.clone(), tx.clone(), ctx.clone(), provider.clone(), cache.clone(), stats.clone());
            let _ = std::thread::Builder::new().name(format!("rawgraph-thumb-{n}")).spawn(move || loop {
                let job = {
                    let Ok(mut q) = shared.wanted.lock() else { return };
                    loop {
                        if shared.shutdown.load(Ordering::Relaxed) {
                            return;
                        }
                        if let Some(j) = q.pop_front() {
                            break j;
                        }
                        q = match shared.wake.wait(q) {
                            Ok(q) => q,
                            Err(_) => return,
                        };
                    }
                };
                let (ticket, Wanted { id, path, hash }) = job;
                let result = if path.to_string_lossy().starts_with(DEMO_PREFIX) {
                    provider.demo_thumbnail(id, THUMB_SIZE)
                } else {
                    let (r, event) = fetch_thumb(cache.as_deref(), &hash, THUMB_SIZE, || rawgraph_io::load_thumbnail(&path, THUMB_SIZE).map_err(|e| e.to_string()));
                    stats.note(event);
                    r
                };
                if tx.send(Done { ticket, id, result }).is_err() {
                    return;
                }
                ctx.request_repaint();
            });
        }
        Self { shared, rx, textures: HashMap::new(), failed: HashSet::new(), in_flight: HashMap::new(), ticket: 0, tick: 0, last_wanted: Vec::new(), stats, first_request: None }
    }

    /// Tells the loader which images are needed now (most important first).
    /// Replaces any earlier request that has not started yet.
    pub fn set_wanted(&mut self, items: &[Wanted]) {
        let ids: Vec<ImageId> = items.iter().map(|w| w.id).collect();
        if ids == self.last_wanted {
            return;
        }
        self.last_wanted = ids;
        if self.first_request.is_none() && !items.is_empty() {
            self.first_request = Some(std::time::Instant::now());
        }
        if let Ok(mut q) = self.shared.wanted.lock() {
            // Requests that never started are forgotten, then the new list is queued.
            for (ticket, w) in q.iter() {
                if self.in_flight.get(&w.id) == Some(ticket) { self.in_flight.remove(&w.id); }
            }
            let mut queue: VecDeque<_> = items
                .iter()
                .filter(|w| !self.textures.contains_key(&w.id) && !self.failed.contains(&w.id) && !self.in_flight.contains_key(&w.id))
                .cloned()
                .collect();
            self.ticket += 1;
            for w in &queue { self.in_flight.insert(w.id, self.ticket); }
            *q = queue.drain(..).map(|w| (self.ticket, w)).collect();
        }
        self.shared.wake.notify_all();
    }

    pub fn texture(&mut self, id: ImageId) -> Option<TextureHandle> {
        self.tick += 1;
        let tick = self.tick;
        self.textures.get_mut(&id).map(|(t, used)| {
            *used = tick;
            t.clone()
        })
    }

    /// A thumbnail job for `id` is queued or running.
    pub fn busy_for(&self, id: ImageId) -> bool {
        self.in_flight.contains_key(&id)
    }

    pub fn has_failed(&self, id: ImageId) -> bool {
        self.failed.contains(&id)
    }

    /// Uploads finished thumbnails (a few per frame) and trims the cache.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let mut uploaded = 0;
        while uploaded < UPLOADS_PER_FRAME {
            let Ok(done) = self.rx.try_recv() else { break };
            uploaded += 1;
            if self.in_flight.get(&done.id) != Some(&done.ticket) { continue; }
            self.in_flight.remove(&done.id);
            match done.result {
                Ok((w, h, rgba)) if rgba.len() == w as usize * h as usize * 4 => {
                    let img = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    self.tick += 1;
                    self.textures.insert(done.id, (ctx.load_texture(format!("thumb-{}", done.id.0), img, TextureOptions::LINEAR), self.tick));
                }
                _ => {
                    self.failed.insert(done.id);
                }
            }
        }
        if uploaded == UPLOADS_PER_FRAME {
            ctx.request_repaint();
        }
        if self.textures.len() > MAX_TEXTURES {
            let keep: HashSet<ImageId> = self.last_wanted.iter().copied().collect();
            let mut order: Vec<(ImageId, u64)> = self.textures.iter().filter(|(id, _)| !keep.contains(id)).map(|(id, (_, t))| (*id, *t)).collect();
            order.sort_by_key(|(_, t)| *t);
            let excess = self.textures.len() - MAX_TEXTURES;
            for (id, _) in order.into_iter().take(excess) {
                self.textures.remove(&id);
            }
        }
    }

    /// True when everything requested last has a texture or has failed.
    pub fn wanted_loaded(&self) -> bool {
        self.last_wanted.iter().all(|id| self.textures.contains_key(id) || self.failed.contains(id))
    }

    /// Forgets the given images (their file changed, e.g. after a relink); they load again when wanted.
    pub fn invalidate(&mut self, ids: &[ImageId]) {
        for id in ids {
            self.textures.remove(id);
            self.failed.remove(id);
            self.in_flight.remove(id);
        }
        self.last_wanted.clear();
    }

    /// Forget everything (new project): thumbnails are keyed by id only.
    pub fn clear(&mut self) {
        self.textures.clear();
        self.failed.clear();
        self.in_flight.clear();
        self.last_wanted.clear();
        if let Ok(mut q) = self.shared.wanted.lock() {
            q.clear();
        }
    }

}

impl Drop for ThumbLoader {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Relaxed);
        self.shared.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn gradient(w: u32, h: u32) -> Rgba {
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 128, 255]);
            }
        }
        (w, h, px)
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn jpeg_round_trip_stays_close_and_garbage_is_refused() {
        let (w, h, px) = gradient(64, 48);
        let bytes = encode_jpeg(w, h, &px).unwrap();
        assert!(bytes.len() < px.len() / 4, "smaller than raw pixels");
        let (w2, h2, back) = decode_jpeg(&bytes).unwrap();
        assert_eq!((w2, h2), (64, 48));
        let worst = px.chunks(4).zip(back.chunks(4)).map(|(a, b)| (0..3).map(|c| (i16::from(a[c]) - i16::from(b[c])).abs()).max().unwrap()).max().unwrap();
        assert!(worst <= 24, "quality 85 keeps a smooth gradient close: {worst}");
        assert!(decode_jpeg(&bytes[..10]).is_none());
        for bad in [&b""[..], b"not a jpeg", &[0xff, 0xd8, 0xff, 0x00, 0x01]] {
            assert!(decode_jpeg(bad).is_none());
        }
        assert!(encode_jpeg(0, 0, &[]).is_none() && encode_jpeg(2, 2, &[0; 3]).is_none());
    }

    #[test]
    fn first_load_misses_second_hits_without_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::new(dir.path(), CACHE_BUDGET).unwrap();
        let calls = Cell::new(0);
        let decode = || {
            calls.set(calls.get() + 1);
            Ok(gradient(40, 30))
        };
        let (r, e) = fetch_thumb(Some(&cache), HASH, 256, decode);
        assert_eq!((e, r.unwrap().0, calls.get()), (CacheEvent::Miss, 40, 1));
        assert!(cache.total_bytes() > 0, "stored on disk");
        let (r, e) = fetch_thumb(Some(&cache), HASH, 256, decode);
        assert_eq!((e, r.unwrap().1, calls.get()), (CacheEvent::Hit, 30, 1), "no decode on a hit");
        // A new cache object over the same folder (the next start) still hits.
        let again = ThumbCache::new(dir.path(), CACHE_BUDGET).unwrap();
        assert_eq!(fetch_thumb(Some(&again), HASH, 256, decode).1, CacheEvent::Hit);
    }

    #[test]
    fn the_key_includes_the_size_and_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::new(dir.path(), CACHE_BUDGET).unwrap();
        let decode = || Ok(gradient(16, 16));
        assert_eq!(fetch_thumb(Some(&cache), HASH, 256, decode).1, CacheEvent::Miss);
        assert_eq!(fetch_thumb(Some(&cache), HASH, 128, decode).1, CacheEvent::Miss, "another size is another entry");
        assert_eq!(fetch_thumb(Some(&cache), "ffff", 256, decode).1, CacheEvent::Miss, "another content is another entry");
        assert_eq!(fetch_thumb(Some(&cache), HASH, 256, decode).1, CacheEvent::Hit);
        assert_eq!(fetch_thumb(Some(&cache), HASH, 128, decode).1, CacheEvent::Hit);
    }

    #[test]
    fn a_corrupt_entry_is_ignored_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::new(dir.path(), CACHE_BUDGET).unwrap();
        cache.put(HASH, 256, b"this is not a jpeg at all").unwrap();
        let (r, e) = fetch_thumb(Some(&cache), HASH, 256, || Ok(gradient(20, 10)));
        assert_eq!((e, r.unwrap().0), (CacheEvent::Replaced, 20), "decoded from the file instead");
        let (r, e) = fetch_thumb(Some(&cache), HASH, 256, || panic!("must be served from the cache now"));
        assert_eq!((e, r.unwrap().0), (CacheEvent::Hit, 20), "the replacement is good");
    }

    #[test]
    fn no_cache_no_hash_and_failed_decodes_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::new(dir.path(), CACHE_BUDGET).unwrap();
        assert_eq!(fetch_thumb(None, HASH, 256, || Ok(gradient(8, 8))).1, CacheEvent::Skipped);
        assert_eq!(fetch_thumb(Some(&cache), "", 256, || Ok(gradient(8, 8))).1, CacheEvent::Skipped);
        assert_eq!(fetch_thumb(Some(&cache), "not-hex!", 256, || Ok(gradient(8, 8))).1, CacheEvent::Miss, "an odd hash is just never stored");
        let (r, _) = fetch_thumb(Some(&cache), HASH, 256, || Err("broken file".to_owned()));
        assert_eq!(r.unwrap_err(), "broken file");
        assert_eq!(cache.total_bytes(), 0, "failures are not cached");
    }

    #[test]
    fn clearing_with_a_zero_budget_removes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ThumbCache::new(dir.path(), CACHE_BUDGET).unwrap();
        for h in ["aa", "bb", "cc"] {
            let _ = fetch_thumb(Some(&cache), h, 256, || Ok(gradient(32, 32)));
        }
        assert!(cache.total_bytes() > 0);
        let before = cache.total_bytes();
        assert_eq!(cache.clear(), before);
        assert_eq!(cache.total_bytes(), 0);
        // Clearing keeps the budget: later thumbnails are cached again.
        let _ = fetch_thumb(Some(&cache), "dd", 256, || Ok(gradient(32, 32)));
        assert!(cache.total_bytes() > 0);
        // Opening with a zero budget also empties the cache.
        ThumbCache::new(dir.path(), 0).unwrap();
        assert_eq!(cache.total_bytes(), 0);
    }

    #[test]
    fn the_loader_fills_from_files_and_then_from_the_disk_cache() {
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("img");
        std::fs::create_dir_all(&images).unwrap();
        let mut project = rawgraph_project::Project::new();
        for i in 0..4u8 {
            image::RgbaImage::from_pixel(64, 48, image::Rgba([30 + i * 40, 90, 150, 255])).save(images.join(format!("p{i}.png"))).unwrap();
        }
        project.scan_folder(&images, false, &rawgraph_io::is_supported, &|_| None);
        let wanted: Vec<Wanted> = project.images().iter().map(|e| Wanted { id: e.id, path: e.path.clone(), hash: e.content_hash.clone() }).collect();
        let cache = Arc::new(ThumbCache::new(&dir.path().join("cache"), CACHE_BUDGET).unwrap());
        let fill = |cache: Arc<ThumbCache>| {
            let ctx = egui::Context::default();
            let mut l = ThumbLoader::with_cache(ctx.clone(), Arc::new(FileProvider::new(1 << 20)), Some(cache));
            l.set_wanted(&wanted);
            let t = std::time::Instant::now();
            while !l.wanted_loaded() && t.elapsed().as_secs() < 30 {
                l.poll(&ctx);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(l.wanted_loaded() && wanted.iter().all(|w| l.texture(w.id).is_some()));
            l.stats.snapshot()
        };
        assert_eq!(fill(cache.clone()), (0, 4, 0), "cold: every thumbnail decoded and stored");
        assert_eq!(fill(cache), (4, 0, 0), "warm: every thumbnail from disk");
    }
}
