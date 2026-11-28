//! `--bench-first-frame`: selects the images of a folder one after another like a user and
//! measures, per image, the time from the selection request until (a) the first picture of any
//! kind is on screen and (b) the final graph render is on screen.
//!
//! The selection is made at the start of an `update` (just before the selection placeholder is
//! chosen), the pictures are noticed at the end of the same or a later `update`, so each time
//! is accurate to one frame. The folder is only read; everything RawGraph writes (thumbnail
//! cache, recovery copies) goes to `--data-dir`.

use super::*;
use crate::first_frame::{self as report, Pass, Sample};

/// Idle time after a final frame before the next selection (background work of the previous
/// image should not be charged to the next one).
const SETTLE: Duration = Duration::from_millis(300);
/// Quiet time after the folder is scanned and its first picture rendered, before measuring.
const START_QUIET: Duration = Duration::from_millis(1500);
/// A selection without a final frame after this long counts as failed.
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    /// RawGraph's caches are emptied before every selection.
    Uncached(u32),
    /// Unmeasured pass that fills the caches for the cached pass.
    Warm,
    Cached(u32),
}

struct Selection {
    image: ImageId,
    at: Instant,
    item: Item,
    index: usize,
    first: Option<(f32, &'static str)>,
}

pub(super) struct Driver {
    plan: Vec<Item>,
    plan_pos: usize,
    index: usize,
    started: bool,
    ready_since: Option<Instant>,
    sel: Option<Selection>,
    settle_until: Option<Instant>,
    final_seen: Option<(ImageId, Instant, &'static str)>,
    samples: Vec<Sample>,
    folder: PathBuf,
    folder_before: Vec<(PathBuf, u64, Option<std::time::SystemTime>)>,
    out: Option<PathBuf>,
    run_started: Instant,
}

impl Driver {
    pub(super) fn new(cli: &Cli) -> Self {
        let runs = cli.runs.unwrap_or(1).max(1);
        let mut plan = Vec::new();
        for run in 1..=runs {
            plan.push(Item::Uncached(run));
            if !cli.uncached {
                plan.push(Item::Warm);
                plan.push(Item::Cached(run));
            }
        }
        Self {
            plan,
            plan_pos: 0,
            index: 0,
            started: false,
            ready_since: None,
            sel: None,
            settle_until: None,
            final_seen: None,
            samples: Vec::new(),
            folder: cli.bench_first_frame.clone().unwrap_or_default(),
            folder_before: Vec::new(),
            out: cli.bench_out.clone(),
            run_started: Instant::now(),
        }
    }

    /// The renderer delivered a frame (called from `apply_frame`).
    pub(super) fn on_frame(&mut self, image: ImageId, path: &'static str) {
        if self.final_seen.is_none() && self.sel.as_ref().is_some_and(|s| s.image == image) {
            self.final_seen = Some((image, Instant::now(), path));
        }
    }
}

impl App {
    /// Empties every cache RawGraph itself keeps for an image: decoded proxies, prefetch, the
    /// renderer's evaluator and GPU base images, thumbnail textures and the thumbnail caches on
    /// disk (all below `--data-dir`). The operating system's file cache cannot be flushed.
    fn ff_drop_caches(&mut self) {
        self.session.provider.clear_cache();
        self.prefetch.forget();
        self.renderer.drop_caches();
        self.filmstrip.thumbs.clear();
        self.filmstrip.live.reset();
        self.filmstrip.live.clear_disk();
        if let Some(cache) = &self.thumb_cache {
            cache.clear();
        }
    }

    /// Start of an `update`: waits for the folder to be ready, then makes the next selection.
    pub(super) fn first_frame_drive(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.first_frame.take() else { return };
        ctx.request_repaint();
        self.first_frame_advance(&mut d, ctx);
        if self.first_frame.is_none() && !self.allow_close {
            self.first_frame = Some(d);
        }
    }

    fn first_frame_advance(&mut self, d: &mut Driver, ctx: &egui::Context) {
        if d.sel.is_some() {
            return;
        }
        let n = self.session.visible().len();
        if !d.started {
            let ready = self.cli_applied && self.scan.is_none() && n > 0 && self.last_key.is_some() && !self.renderer.pending();
            if !ready {
                d.ready_since = None;
                return;
            }
            if d.ready_since.get_or_insert_with(Instant::now).elapsed() < START_QUIET {
                return;
            }
            d.started = true;
            d.run_started = Instant::now();
            d.folder_before = report::folder_snapshot(&d.folder);
            eprintln!(
                "BENCH first-frame: {} images in {}, {} pass(es), renderer {}, preview size {}",
                n,
                d.folder.display(),
                d.plan.len(),
                self.renderer_mode.label(),
                self.session.provider.preview_size()
            );
        }
        if d.settle_until.is_some_and(|t| Instant::now() < t) || self.renderer.pending() {
            return;
        }
        if d.index >= n {
            d.index = 0;
            d.plan_pos += 1;
        }
        let Some(&item) = d.plan.get(d.plan_pos) else {
            self.first_frame_finish(d, ctx);
            return;
        };
        let Some(&image) = self.session.visible().get(d.index) else { return };
        if matches!(item, Item::Uncached(_)) {
            self.ff_drop_caches();
        }
        if image == self.session.current_image() {
            // Selecting the shown image again would change nothing: start from an empty viewer.
            self.viewer = Viewer::with_preferences(self.viewer.click_zoom, self.viewer.wheel_navigates);
            self.last_key = None;
        }
        d.final_seen = None;
        d.sel = Some(Selection { image, at: Instant::now(), item, index: d.index, first: None });
        self.session.select_index(d.index);
    }

    /// End of an `update`: notices the first picture and the final frame of the selection.
    pub(super) fn first_frame_observe(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.first_frame.take() else { return };
        if let Some(sel) = d.sel.as_mut() {
            if sel.first.is_none() && self.viewer.preview_image == Some(sel.image) && self.viewer.texture_id().is_some() {
                let src = if self.viewer.awaiting_final() { self.placeholder_src } else { "final frame" };
                sel.first = Some((sel.at.elapsed().as_secs_f32() * 1000.0, src));
            }
            let done = d.final_seen.filter(|f| f.0 == sel.image);
            let timed_out = done.is_none() && sel.at.elapsed() > STEP_TIMEOUT;
            if done.is_some() || timed_out {
                let final_ms = done.map_or_else(|| sel.at.elapsed().as_secs_f32() * 1000.0, |f| f.1.duration_since(sel.at).as_secs_f32() * 1000.0);
                let (first_ms, first_source) = sel.first.map_or((final_ms, "final frame"), |(ms, src)| (ms.min(final_ms), src));
                let path = done.map_or("-", |f| f.2);
                let measured = !matches!(sel.item, Item::Warm);
                if measured {
                    let entry = self.session.entry(sel.image);
                    let file = entry.map(|e| e.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())).unwrap_or_default();
                    let format = entry.and_then(|e| e.path.extension()).map(|x| x.to_string_lossy().to_uppercase()).unwrap_or_default();
                    d.samples.push(Sample {
                        run: match sel.item {
                            Item::Uncached(r) | Item::Cached(r) => r,
                            Item::Warm => 0,
                        },
                        pass: if matches!(sel.item, Item::Uncached(_)) { Pass::Uncached } else { Pass::Cached },
                        index: sel.index,
                        file,
                        format,
                        camera: entry.and_then(|e| e.capture.model.clone()).unwrap_or_default(),
                        file_bytes: entry.map_or(0, |e| e.file_size),
                        proxy: self.session.provider.cached_proxy(sel.image).map(|p| (p.width(), p.height())),
                        path,
                        first_ms,
                        first_source,
                        final_ms,
                        failed: timed_out,
                    });
                }
                d.sel = None;
                d.index += 1;
                d.settle_until = Some(Instant::now() + SETTLE);
                ctx.request_repaint();
            }
        }
        self.first_frame = Some(d);
    }

    fn first_frame_finish(&mut self, d: &mut Driver, ctx: &egui::Context) {
        let summaries = report::summarize(&d.samples);
        eprintln!("BENCH first-frame results (ms; time from the selection request, accurate to one frame):");
        for line in report::table(&summaries).lines() {
            eprintln!("BENCH {line}");
        }
        eprintln!(
            "BENCH first picture = first thumbnail / embedded preview / cached proxy / final frame of any kind on screen; final = graph render on screen. \
             RawGraph's own caches (thumbnails in memory and on disk, proxies, GPU images, evaluator) were emptied before every uncached selection; \
             the operating system's file cache could not be, so only the first uncached pass over a folder is cold for the files."
        );
        if let Some(out) = &d.out {
            let write = |path: &std::path::Path, text: String| match std::fs::write(path, text) {
                Ok(()) => eprintln!("BENCH wrote {}", path.display()),
                Err(e) => eprintln!("BENCH cannot write {}: {e}", path.display()),
            };
            write(out, report::samples_csv(&d.samples));
            write(&report::summary_path(out), report::summary_csv(&summaries));
        }
        let unchanged = report::folder_snapshot(&d.folder) == d.folder_before;
        eprintln!("BENCH folder {} {} (the run only reads it)", d.folder.display(), if unchanged { "unchanged" } else { "CHANGED" });
        if !unchanged {
            EXIT_CODE.store(3, Ordering::SeqCst);
        }
        eprintln!("BENCH total run {:.1} s", d.run_started.elapsed().as_secs_f32());
        self.allow_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}
