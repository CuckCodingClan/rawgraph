//! Background prefetch of neighbouring images so stepping is instant.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

use rawgraph_core::ImageId;
use rawgraph_engine::ImageProvider;

use crate::provider::FileProvider;

pub struct Prefetcher {
    tx: Sender<Vec<ImageId>>,
    generation: Arc<AtomicU64>,
    last: Vec<ImageId>,
}

impl Prefetcher {
    pub fn new(provider: Arc<FileProvider>) -> Self {
        let (tx, rx) = channel::<Vec<ImageId>>();
        let generation = Arc::new(AtomicU64::new(0));
        let g = generation.clone();
        let _ = std::thread::Builder::new().name("rawgraph-prefetch".into()).spawn(move || {
            while let Ok(mut ids) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    ids = newer;
                }
                let mine = g.load(Ordering::SeqCst);
                for id in ids {
                    // A newer request replaces this one between decodes.
                    if g.load(Ordering::SeqCst) != mine {
                        break;
                    }
                    let _ = provider.image(id);
                }
            }
        });
        Self { tx, generation, last: Vec::new() }
    }

    /// Asks for `ids` (most useful first) to be decoded into the cache.
    pub fn request(&mut self, ids: Vec<ImageId>) {
        if ids == self.last {
            return;
        }
        self.last = ids.clone();
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.tx.send(ids);
    }

    /// Forces the next `request` to go through (after a preview-size change).
    pub fn forget(&mut self) {
        self.last.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn prefetch_fills_the_cache() {
        let provider = Arc::new(FileProvider::new(1 << 30));
        provider.set_paths([(ImageId(1), "demo:1".into()), (ImageId(2), "demo:2".into())]);
        provider.set_preview_size(1280);
        let mut p = Prefetcher::new(provider.clone());
        p.request(vec![ImageId(2), ImageId(1)]);
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(30) && !(provider.is_cached(ImageId(1)) && provider.is_cached(ImageId(2))) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(provider.is_cached(ImageId(1)) && provider.is_cached(ImageId(2)));
    }
}
