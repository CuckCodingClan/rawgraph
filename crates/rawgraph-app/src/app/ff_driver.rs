//! `--bench-first-frame`: selects the images of a folder one after another like a user and
//! measures, per image, the time from the selection request until (a) the first picture of any
//! kind is on screen and (b) the final graph render is on screen.
//!
//! The selection is made at the start of an `update`, just before the selection placeholder is
//! chosen. A picture is noticed at the end of the `update` that put it into a texture and is
//! stamped at the start of the following `update`, i.e. after that frame was painted and
//! presented (vsync included). So a time is the selection request until the start of the frame
//! after the one that showed the picture; the offset to the true presentation is below one frame
//! interval. The folder is only read; everything RawGraph writes (thumbnail cache, recovery
//! copies) goes to `--data-dir`.

use super::*;
use crate::first_frame::{self as report, Pass, Sample};

/// Idle time after a final frame before the next selection (background work of the previous
/// image should not be charged to the next one).
const SETTLE: Duration = Duration::from_millis(300);
/// Quiet time after the folder is scanned and its first picture rendered, before measuring.
const START_QUIET: Duration = Duration::from_millis(1500);
/// The folder must be scanned and shown within this time, else the run fails.
const START_TIMEOUT: Duration = Duration::from_secs(180);
/// A selection without a final frame after this long counts as failed.
const STEP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for a busy renderer before going on anyway.
const PENDING_CAP: Duration = Duration::from_secs(30);
/// How long to wait for running decodes / thumbnail jobs before an uncached selection.
const IDLE_CAP: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    /// RawGraph's caches for the image are emptied before every selection.
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
    /// Seen in a texture at the end of an update; stamped at the start of the next one.
    first_seen: Option<&'static str>,
    first: Option<(f32, &'static str)>,
    final_seen: bool,
    final_ms: Option<f32>,
}

struct FinalFrame {
    image: ImageId,
    path: &'static str,
    eval_ms: f32,
    display_ms: f32,
}

pub(super) struct Driver {
    plan: Vec<Item>,
    plan_pos: usize,
    index: usize,
    started: bool,
    ready_since: Option<Instant>,
    pending_since: Option<Instant>,
    idle_since: Option<Instant>,
    sel: Option<Selection>,
    settle_until: Option<Instant>,
    final_frame: Option<FinalFrame>,
    failure: Option<String>,
    samples: Vec<Sample>,
    folder: PathBuf,
    /// Taken before the scan starts.
    before: report::Snapshot,
    startup_error: Option<String>,
    out: Option<PathBuf>,
    created: Instant,
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
        let folder = cli.bench_first_frame.clone().unwrap_or_default();
        let startup_error = std::fs::read_dir(&folder).err().map(|e| format!("cannot read {}: {e}", folder.display()));
        let before = report::folder_snapshot(&folder);
        for dir in &before.unreadable {
            eprintln!("BENCH warning: cannot read {}; its files are not measured", dir.display());
        }
        Self {
            plan,
            plan_pos: 0,
            index: 0,
            started: false,
            ready_since: None,
            pending_since: None,
            idle_since: None,
            sel: None,
            settle_until: None,
            final_frame: None,
            failure: None,
            samples: Vec::new(),
            folder,
            before,
            startup_error,
            out: cli.bench_out.clone(),
            created: Instant::now(),
            run_started: Instant::now(),
        }
    }

    /// The current pass empties RawGraph's caches: no prefetching of neighbours.
    pub(super) fn cold(&self) -> bool {
        matches!(self.plan.get(self.plan_pos), Some(Item::Uncached(_)))
    }

    /// The renderer delivered a frame (called from `apply_frame`).
    pub(super) fn on_frame(&mut self, image: ImageId, path: &'static str, eval_ms: f32, display_ms: f32) {
        if self.final_frame.is_none() && self.sel.as_ref().is_some_and(|s| s.image == image) {
            self.final_frame = Some(FinalFrame { image, path, eval_ms, display_ms });
        }
    }

    /// The render of the selected image failed or it is not part of the viewed collection.
    pub(super) fn on_failed(&mut self, message: String) {
        if self.sel.is_some() && self.failure.is_none() {
            self.failure = Some(message);
        }
    }
}

impl App {
    /// Empties what RawGraph itself keeps for `image` on the way to a first picture: decoded
    /// proxies, prefetch, the renderer's evaluator and GPU base images, the image's thumbnail
    /// (texture, entry in the disk cache) and its edited gallery picture. The rest of the gallery
    /// keeps its thumbnails. The operating system's file cache cannot be flushed.
    fn ff_drop_caches(&mut self, image: ImageId) {
        self.session.provider.clear_cache();
        self.prefetch.forget();
        self.renderer.drop_caches();
        self.filmstrip.thumbs.invalidate(&[image]);
        self.filmstrip.live.forget(image);
        if let (Some(cache), Some(entry)) = (&self.thumb_cache, self.session.entry(image)) {
            cache.remove(&entry.content_hash, None);
        }
    }

    /// Start of an `update`: stamps pictures shown by the previous frame, then makes the next
    /// selection.
    pub(super) fn first_frame_drive(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.first_frame.take() else { return };
        ctx.request_repaint();
        self.first_frame_advance(&mut d, ctx);
        if !self.allow_close {
            self.first_frame = Some(d);
        }
    }

    fn first_frame_complete(&mut self, d: &mut Driver) {
        let Some(sel) = d.sel.take() else { return };
        let failed = d.failure.is_some() || sel.final_ms.is_none();
        let final_ms = sel.final_ms.unwrap_or_else(|| sel.at.elapsed().as_secs_f32() * 1000.0);
        let (first_ms, first_source) = sel.first.map_or((final_ms, "final frame"), |(ms, src)| (ms.min(final_ms), src));
        if let Some(why) = &d.failure {
            eprintln!("BENCH image {} failed: {why}", sel.index);
        } else if failed {
            eprintln!("BENCH image {} failed: no final frame within {} s", sel.index, STEP_TIMEOUT.as_secs());
        }
        if !matches!(sel.item, Item::Warm) {
            let entry = self.session.entry(sel.image);
            let file = entry.map(|e| e.path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())).unwrap_or_default();
            let format = entry.and_then(|e| e.path.extension()).map(|x| x.to_string_lossy().to_uppercase()).unwrap_or_default();
            let ff = d.final_frame.as_ref().filter(|f| f.image == sel.image);
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
                path: ff.map_or("-", |f| f.path),
                first_ms,
                first_source,
                final_ms,
                eval_ms: ff.map_or(0.0, |f| f.eval_ms),
                display_ms: ff.map_or(0.0, |f| f.display_ms),
                failed,
            });
        }
        d.failure = None;
        d.final_frame = None;
        d.index += 1;
        d.settle_until = Some(Instant::now() + SETTLE);
    }

    fn first_frame_advance(&mut self, d: &mut Driver, ctx: &egui::Context) {
        if let Some(sel) = d.sel.as_mut() {
            let elapsed = |t: Instant| t.elapsed().as_secs_f32() * 1000.0;
            if sel.first.is_none() {
                if let Some(src) = sel.first_seen {
                    sel.first = Some((elapsed(sel.at), src));
                }
            }
            if sel.final_ms.is_none() && sel.final_seen {
                sel.final_ms = Some(elapsed(sel.at));
            }
            if (sel.final_ms.is_some() && sel.first.is_some()) || d.failure.is_some() || sel.at.elapsed() > STEP_TIMEOUT {
                self.first_frame_complete(d);
            }
            return;
        }
        let n = self.session.visible().len();
        if !d.started {
            if let Some(e) = d.startup_error.clone() {
                self.first_frame_abort(&e, 2, ctx);
                return;
            }
            if self.cli_applied && self.scan.is_none() && n == 0 {
                let e = format!("no images found in {}", d.folder.display());
                self.first_frame_abort(&e, 2, ctx);
                return;
            }
            if d.created.elapsed() > START_TIMEOUT {
                let e = format!("the folder was not scanned and shown within {} s", START_TIMEOUT.as_secs());
                self.first_frame_abort(&e, 2, ctx);
                return;
            }
            // Nothing is rendered without a node to view (a new project's graph has no Output node):
            // view the Output node, or else the last node of the graph, as `--bench` does.
            if self.cli_applied && self.session.selected_node.is_none() {
                let g = &self.session.project.graph;
                let pick = g.nodes().find(|n| n.kind == NodeKind::Output).or_else(|| g.nodes().last()).map(|n| n.id);
                if pick.is_some() {
                    self.session.selected_node = pick;
                }
            }
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
            eprintln!(
                "BENCH first-frame: {} images in {}, {} pass(es), renderer {}, preview size {}",
                n,
                d.folder.display(),
                d.plan.len(),
                self.renderer_mode.label(),
                self.session.provider.preview_size()
            );
        }
        if d.settle_until.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if self.renderer.pending() {
            // Never wait forever for a renderer that does not answer.
            if d.pending_since.get_or_insert_with(Instant::now).elapsed() < PENDING_CAP {
                return;
            }
        }
        d.pending_since = None;
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
            // Nothing may still be decoding or writing this image's thumbnail when the caches go.
            let busy = !self.session.provider.decode_idle() || self.filmstrip.thumbs.busy_for(image);
            if busy && d.idle_since.get_or_insert_with(Instant::now).elapsed() < IDLE_CAP {
                return;
            }
            d.idle_since = None;
            self.ff_drop_caches(image);
        }
        if image == self.session.current_image() {
            // Selecting the shown image again would change nothing: start from an empty viewer.
            self.viewer = Viewer::with_preferences(self.viewer.click_zoom, self.viewer.wheel_navigates);
            self.last_key = None;
        }
        d.final_frame = None;
        d.failure = None;
        d.sel = Some(Selection { image, at: Instant::now(), item, index: d.index, first_seen: None, first: None, final_seen: false, final_ms: None });
        self.session.select_index(d.index);
    }

    /// End of an `update`: notices which pictures this frame put on screen.
    pub(super) fn first_frame_observe(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.first_frame.take() else { return };
        if let Some(sel) = d.sel.as_mut() {
            let has_texture = self.viewer.preview_image == Some(sel.image) && self.viewer.texture_id().is_some();
            let final_here = d.final_frame.as_ref().is_some_and(|f| f.image == sel.image);
            if sel.first_seen.is_none() && (has_texture || final_here) {
                sel.first_seen = Some(if self.viewer.awaiting_final() && !final_here { self.placeholder_src } else { "final frame" });
            }
            if final_here {
                sel.final_seen = true;
            }
            if sel.first_seen.is_some() || sel.final_seen {
                ctx.request_repaint();
            }
        }
        self.first_frame = Some(d);
    }

    fn first_frame_abort(&mut self, message: &str, code: i32, ctx: &egui::Context) {
        eprintln!("BENCH failed: {message}");
        EXIT_CODE.store(code, Ordering::SeqCst);
        self.allow_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn first_frame_finish(&mut self, d: &mut Driver, ctx: &egui::Context) {
        let summaries = report::summarize(&d.samples);
        eprintln!("BENCH first-frame results (ms; selection request until the frame that shows the picture was presented):");
        for line in report::table(&summaries).lines() {
            eprintln!("BENCH {line}");
        }
        eprintln!(
            "BENCH first picture = first thumbnail / embedded preview / cached proxy / final frame of any kind on screen; final = graph render on screen. \
             RawGraph's own caches for the selected image (proxy, prefetch, evaluator, GPU images, its thumbnail in memory and on disk, its edited gallery picture) were emptied before every uncached selection \
             and neighbours were not prefetched; the operating system's file cache could not be emptied, so only the first uncached pass over a folder is cold for the files."
        );
        let mut code = 0;
        if let Some(out) = &d.out {
            let mut write = |path: &std::path::Path, text: String| match std::fs::write(path, text) {
                Ok(()) => eprintln!("BENCH wrote {}", path.display()),
                Err(e) => {
                    eprintln!("BENCH cannot write {}: {e}", path.display());
                    code = 5;
                }
            };
            write(out, report::samples_csv(&d.samples));
            write(&report::summary_path(out), report::summary_csv(&summaries));
        }
        let after = report::folder_snapshot(&d.folder);
        let unchanged = after == d.before;
        eprintln!("BENCH folder {} {} (the run only reads it)", d.folder.display(), if unchanged { "unchanged" } else { "CHANGED" });
        for dir in &after.unreadable {
            eprintln!("BENCH folder part not readable: {}", dir.display());
        }
        let failed = d.samples.iter().filter(|s| s.failed).count();
        if failed > 0 {
            eprintln!("BENCH {failed} selection(s) failed (listed in the CSV, left out of the statistics)");
            code = code.max(4);
        }
        if !unchanged {
            code = 3;
        }
        if code != 0 {
            EXIT_CODE.store(code, Ordering::SeqCst);
        }
        eprintln!("BENCH total run {:.1} s", d.run_started.elapsed().as_secs_f32());
        self.allow_close = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}
