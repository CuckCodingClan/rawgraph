//! Rules of the GPU preview path that need no GPU: the renderer setting, the base-image cache
//! policy, the fallback state machine and the scope refinement schedule. Everything here is
//! plain data and can be tested without a window or an adapter.

use std::time::{Duration, Instant};

use rawgraph_core::ImageId;
use serde::{Deserialize, Serialize};

/// Which renderer the interactive preview may use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RendererMode {
    /// GPU when possible, the CPU otherwise (default).
    #[default]
    Auto,
    /// Always the CPU reference path.
    Cpu,
    /// GPU only; a frame that cannot be produced on the GPU is an error (tests, `--renderer gpu`).
    Gpu,
}

impl RendererMode {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "cpu" => Some(Self::Cpu),
            "gpu" => Some(Self::Gpu),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Automatic (GPU when possible)",
            Self::Cpu => "CPU only",
            Self::Gpu => "GPU only",
        }
    }
}

/// A failure injected for tests (`--gpu-fail`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuFailure {
    /// Every upload fails with an out-of-memory error.
    Oom,
    /// The device counts as lost.
    Lost,
    /// Every upload fails with some other GPU error.
    Other,
}

impl GpuFailure {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "oom" => Some(Self::Oom),
            "lost" => Some(Self::Lost),
            "other" => Some(Self::Other),
            _ => None,
        }
    }
}

/// Which path produced a frame, and why the CPU was used when it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderPath {
    Gpu,
    /// `fallback`: the GPU was wanted but failed or was unavailable (as opposed to a view that
    /// is on the CPU by design, such as a mask or the crop tool).
    Cpu { reason: String, fallback: bool },
}

/// Why the CPU makes a frame by design. The status bar derives its short text from these.
pub const REASON_CROP_TOOL: &str = "the crop tool shows the picture entering the node, which needs CPU pixels";
pub const REASON_MASK_OVERLAY: &str = "the mask overlay is composited on the CPU";
pub const REASON_CPU_ONLY: &str = "the renderer is set to CPU only";
pub const REASON_COMPARE: &str = "the compare view shows the selected image from CPU pixels";

/// Short form of a CPU reason for the status bar ("mask overlay", "crop tool", ...).
pub fn short_reason(reason: &str) -> String {
    let table = [
        ("crop tool", "crop tool"),
        ("mask overlay", "mask overlay"),
        ("compare view", "compare view"),
        ("set to CPU only", "CPU only"),
        ("no wgpu render state", "no GPU"),
        ("switched off", "GPU switched off"),
        ("out of memory", "GPU out of memory"),
        ("device lost", "GPU device lost"),
        ("larger than this GPU", "picture too large for the GPU"),
        ("GPU error", "GPU error"),
        ("always uses the CPU", "this view"),
    ];
    for (needle, short) in table {
        if reason.contains(needle) {
            return short.to_owned();
        }
    }
    let first = reason.split(['(', ';']).next().unwrap_or(reason).trim();
    let mut out: String = first.chars().take(32).collect();
    if first.chars().count() > 32 {
        out.push('\u{2026}');
    }
    out
}

impl RenderPath {
    /// Text of the status bar: "GPU", "CPU", or "CPU \u{2014} mask overlay" (the reason is only
    /// spelled out while the renderer is on Automatic; with a fixed renderer the CPU is no surprise).
    pub fn status_label(&self, mode: RendererMode) -> String {
        match self {
            RenderPath::Gpu => "GPU".to_owned(),
            RenderPath::Cpu { reason, .. } if mode == RendererMode::Auto => format!("CPU \u{2014} {}", short_reason(reason)),
            RenderPath::Cpu { .. } => "CPU".to_owned(),
        }
    }

    /// The CPU by design.
    pub fn cpu(reason: impl Into<String>) -> Self {
        Self::Cpu { reason: reason.into(), fallback: false }
    }

    /// The CPU because the GPU path could not be used.
    pub fn fallback(reason: impl Into<String>) -> Self {
        Self::Cpu { reason: reason.into(), fallback: true }
    }

    pub fn short(&self) -> &'static str {
        if self.is_gpu() { "GPU" } else { "CPU" }
    }

    pub fn is_gpu(&self) -> bool {
        matches!(self, Self::Gpu)
    }

}

/// Everything that makes a GPU base image different from another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseKey {
    pub image: ImageId,
    pub fingerprint: u64,
    pub preview: u32,
    /// Changes when the files or metadata behind the image change.
    pub revision: u64,
}

/// Least-recently-used list with a fixed capacity; the newest entry is last.
pub struct Lru<K, V> {
    entries: Vec<(K, V)>,
    cap: usize,
}

impl<K: PartialEq + Copy, V> Lru<K, V> {
    pub fn new(cap: usize) -> Self {
        Self { entries: Vec::new(), cap: cap.max(1) }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// The value for `key`, marked as most recently used.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let at = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(at);
        self.entries.push(entry);
        self.entries.last_mut().map(|(_, v)| v)
    }

    /// Every entry, mutable, without changing the order.
    pub fn values_mut(&mut self) -> impl Iterator<Item = (&K, &mut V)> {
        self.entries.iter_mut().map(|(k, v)| (&*k, v))
    }

    /// Without changing the order.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Removes the least recently used entries until one more fits and returns them (so the
    /// caller can free their GPU memory BEFORE allocating the new entry).
    pub fn make_room(&mut self) -> Vec<V> {
        let mut out = Vec::new();
        while self.entries.len() >= self.cap {
            out.push(self.entries.remove(0).1);
        }
        out
    }

    /// Adds an entry as the newest; call [`Lru::make_room`] first (a stale duplicate is replaced).
    pub fn insert(&mut self, key: K, value: V) -> Vec<V> {
        let mut out = Vec::new();
        if let Some(at) = self.entries.iter().position(|(k, _)| *k == key) {
            out.push(self.entries.remove(at).1);
        }
        out.extend(self.make_room());
        self.entries.push((key, value));
        out
    }

    pub fn drain(&mut self) -> Vec<V> {
        self.entries.drain(..).map(|(_, v)| v).collect()
    }

    /// The keys, oldest first.
    pub fn keys_oldest_first(&self) -> Vec<K> {
        self.entries.iter().map(|(k, _)| *k).collect()
    }

    /// Number of entries.
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Removes the least recently used entry for which `evictable` is true.
    pub fn evict_oldest_where(&mut self, mut evictable: impl FnMut(&K) -> bool) -> Option<V> {
        let at = self.entries.iter().position(|(k, _)| evictable(k))?;
        Some(self.entries.remove(at).1)
    }

    /// Drops every entry for which `keep` is false and returns them.
    pub fn remove_where(&mut self, mut drop_it: impl FnMut(&K) -> bool) -> Vec<V> {
        let mut gone = Vec::new();
        let mut kept = Vec::new();
        for (k, v) in self.entries.drain(..) {
            if drop_it(&k) { gone.push(v) } else { kept.push((k, v)) }
        }
        self.entries = kept;
        gone
    }

    #[cfg(test)]
    pub fn keys(&self) -> Vec<K> {
        self.entries.iter().map(|(k, _)| *k).collect()
    }
}

/// Consecutive out-of-memory failures after which the GPU stays off for the session.
pub const MAX_OOM_FAILURES: u32 = 3;

/// What the fallback rules say about the next GPU attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attempt {
    Try,
    /// Use the CPU for this frame; the text is the reason shown in the status bar tooltip.
    Skip(String),
}

/// The fallback state machine of the GPU path.
#[derive(Debug)]
pub struct Fallback {
    mode: RendererMode,
    /// The GPU is off for the rest of the session (reason).
    permanent: Option<String>,
    consecutive_oom: u32,
    /// (image, preview size) of the last out-of-memory failure: no new attempt before it changes.
    blocked: Option<(ImageId, u32)>,
    blocked_reason: String,
    logged_other: bool,
}

impl Fallback {
    /// `available` is the reason the GPU cannot be used at all (no render state), if any.
    pub fn new(mode: RendererMode, unavailable: Option<String>) -> Self {
        Self { mode, permanent: unavailable, consecutive_oom: 0, blocked: None, blocked_reason: String::new(), logged_other: false }
    }

    pub fn permanent_reason(&self) -> Option<&str> {
        self.permanent.as_deref()
    }

    #[cfg(test)]
    pub fn consecutive_oom(&self) -> u32 {
        self.consecutive_oom
    }

    pub fn strict(&self) -> bool {
        self.mode == RendererMode::Gpu
    }

    /// Whether a GPU attempt may be made for this base.
    pub fn decide(&self, key: &BaseKey) -> Attempt {
        if let Some(why) = &self.permanent {
            return Attempt::Skip(why.clone());
        }
        if self.strict() {
            return Attempt::Try;
        }
        if self.blocked == Some((key.image, key.preview)) {
            return Attempt::Skip(self.blocked_reason.clone());
        }
        Attempt::Try
    }

    /// An allocation failed. Returns the one-time status note when this made the GPU permanent.
    pub fn on_out_of_memory(&mut self, key: &BaseKey, message: &str) -> Option<String> {
        self.consecutive_oom += 1;
        self.blocked = Some((key.image, key.preview));
        self.blocked_reason = format!("GPU out of memory ({message}); this image is shown on the CPU");
        if self.consecutive_oom >= MAX_OOM_FAILURES && !self.strict() {
            let why = format!("the GPU ran out of memory {MAX_OOM_FAILURES} times in a row");
            self.permanent = Some(format!("GPU switched off for this session: {why}"));
            return Some(format!("GPU preview switched off for this session ({why}); using the CPU"));
        }
        None
    }

    /// The device is gone. Returns the one-time status note.
    pub fn on_device_lost(&mut self) -> Option<String> {
        if self.permanent.is_some() {
            return None;
        }
        self.permanent = Some("GPU device lost; the CPU is used for the rest of the session".to_owned());
        Some("The GPU device was lost; using the CPU for the rest of the session".to_owned())
    }

    /// Any other GPU error: the CPU shows this frame. True the first time (log it once).
    pub fn on_other_error(&mut self) -> bool {
        let first = !self.logged_other;
        self.logged_other = true;
        first
    }

    /// A GPU frame was produced.
    pub fn on_success(&mut self) {
        self.consecutive_oom = 0;
        self.blocked = None;
    }
}

/// How long no new request may arrive before the resting scopes are computed exactly.
pub const SCOPE_IDLE: Duration = Duration::from_millis(150);

/// Decides at which pixel step the scopes of a GPU frame are read: coarse while requests keep
/// arriving (dragging), exact once nothing has come for [`SCOPE_IDLE`]. The clock is passed in.
#[derive(Debug, Default)]
pub struct ScopeScheduler {
    last_request: Option<Instant>,
    refine_at: Option<Instant>,
}

impl ScopeScheduler {
    /// A request arrived: the pixel step for this frame (`base` is the step the panel asked for).
    pub fn on_request(&mut self, now: Instant, base: usize) -> usize {
        let base = base.max(1);
        let dragging = self.last_request.is_some_and(|t| now.saturating_duration_since(t) < SCOPE_IDLE);
        self.last_request = Some(now);
        self.refine_at = Some(now + SCOPE_IDLE);
        if dragging {
            base.max(2)
        } else {
            base
        }
    }

    /// A request arrived but needs no scope reading (CPU frame, scopes hidden).
    pub fn on_request_without_scopes(&mut self, now: Instant) {
        self.last_request = Some(now);
        self.refine_at = None;
    }

    /// When the exact reading is due, if one is pending.
    pub fn refine_due(&self) -> Option<Instant> {
        self.refine_at
    }

    /// True once (and clears it) when the exact reading is due at `now`.
    pub fn take_refine(&mut self, now: Instant) -> bool {
        match self.refine_at {
            Some(t) if now >= t => {
                self.refine_at = None;
                true
            }
            _ => false,
        }
    }
}

/// How long the worker must have been idle (no request since the last GPU frame) before a
/// neighbour image is uploaded in the background.
pub const PRELOAD_IDLE: Duration = Duration::from_millis(80);

/// When to upload the neighbours of the shown image to the GPU. The clock is passed in.
///
/// The policy: after a GPU frame has been shown, the neighbours are worked off one by one once
/// [`PRELOAD_IDLE`] has passed; any new request abandons what is left.
#[derive(Debug, Default)]
pub struct Preloader {
    queue: Vec<ImageId>,
    armed_at: Option<Instant>,
    /// How often a neighbour was put back because its proxy was not decoded yet.
    deferrals: u32,
}

/// A neighbour whose proxy is not ready is looked at again this often at most (in total).
pub const MAX_DEFERRALS: u32 = 40;

impl Preloader {
    /// A request arrived: whatever was left to preload is abandoned.
    pub fn on_request(&mut self) {
        self.queue.clear();
        self.armed_at = None;
        self.deferrals = 0;
    }

    /// A GPU frame was shown at `now`; `neighbours` (most useful first) are to be preloaded.
    pub fn on_gpu_frame(&mut self, now: Instant, neighbours: &[ImageId]) {
        self.queue = neighbours.to_vec();
        self.armed_at = (!neighbours.is_empty()).then_some(now);
        self.deferrals = 0;
    }

    /// `id` could not be uploaded yet (its proxy is still being decoded): it goes to the back of
    /// the queue and is tried again after another idle period, [`MAX_DEFERRALS`] times at most.
    pub fn defer(&mut self, now: Instant, id: ImageId) {
        if self.deferrals < MAX_DEFERRALS {
            self.deferrals += 1;
            self.queue.push(id);
            self.armed_at = Some(now);
        }
    }

    pub fn pending(&self) -> bool {
        !self.queue.is_empty()
    }

    /// When the next neighbour may be uploaded, if any is waiting.
    pub fn due(&self) -> Option<Instant> {
        self.armed_at.filter(|_| self.pending()).map(|t| t + PRELOAD_IDLE)
    }

    /// The next neighbour to upload, once the worker has been idle long enough at `now`.
    pub fn take_next(&mut self, now: Instant) -> Option<ImageId> {
        match self.due() {
            Some(t) if now >= t => Some(self.queue.remove(0)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_reasons_of_every_cpu_frame() {
        assert_eq!(short_reason(REASON_MASK_OVERLAY), "mask overlay");
        assert_eq!(short_reason(REASON_CROP_TOOL), "crop tool");
        assert_eq!(short_reason(REASON_COMPARE), "compare view");
        assert_eq!(short_reason(REASON_CPU_ONLY), "CPU only");
        assert_eq!(short_reason("eframe has no wgpu render state, so there is no GPU to use"), "no GPU");
        assert_eq!(short_reason("GPU out of memory (x); this image is shown on the CPU"), "GPU out of memory");
        assert_eq!(short_reason("GPU switched off for this session: the GPU ran out of memory 3 times in a row"), "GPU switched off");
        assert_eq!(short_reason("GPU device lost; the CPU is used for the rest of the session"), "GPU device lost");
        assert_eq!(short_reason("the picture is larger than this GPU allows (x)"), "picture too large for the GPU");
        assert_eq!(short_reason("GPU error: boom"), "GPU error");
        let long = short_reason("something quite unexpected and rather long happened here (details)");
        assert!(long.chars().count() <= 33 && !long.contains('('), "{long}");
    }

    #[test]
    fn the_status_label_spells_the_reason_out_only_on_automatic() {
        let overlay = RenderPath::cpu(REASON_MASK_OVERLAY);
        assert_eq!(overlay.status_label(RendererMode::Auto), "CPU \u{2014} mask overlay");
        assert_eq!(overlay.status_label(RendererMode::Cpu), "CPU");
        assert_eq!(RenderPath::Gpu.status_label(RendererMode::Auto), "GPU");
        assert_eq!(RenderPath::fallback("GPU out of memory (x)").status_label(RendererMode::Auto), "CPU \u{2014} GPU out of memory");
        assert_eq!(RenderPath::cpu(REASON_CROP_TOOL).status_label(RendererMode::Auto), "CPU \u{2014} crop tool");
        assert_eq!(RenderPath::cpu(REASON_COMPARE).status_label(RendererMode::Auto), "CPU \u{2014} compare view");
    }

    #[test]
    fn the_preloader_waits_for_idle_and_a_request_abandons_it() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut p = Preloader::default();
        assert!(p.due().is_none() && p.take_next(ms(1000)).is_none(), "nothing armed");
        p.on_gpu_frame(ms(0), &[ImageId(3), ImageId(1)]);
        assert_eq!(p.due(), Some(ms(80)));
        assert!(p.take_next(ms(79)).is_none(), "not idle long enough");
        assert_eq!(p.take_next(ms(80)), Some(ImageId(3)));
        assert_eq!(p.take_next(ms(81)), Some(ImageId(1)), "one after the other, most useful first");
        assert!(p.take_next(ms(500)).is_none() && !p.pending());
        // A request in between abandons the rest.
        p.on_gpu_frame(ms(1000), &[ImageId(3), ImageId(1)]);
        assert_eq!(p.take_next(ms(1090)), Some(ImageId(3)));
        p.on_request();
        assert!(p.take_next(ms(5000)).is_none() && p.due().is_none());
        // No neighbours, nothing to do.
        p.on_gpu_frame(ms(2000), &[]);
        assert!(p.due().is_none());
        // A neighbour that was not decoded yet is tried again later, but not for ever.
        p.on_gpu_frame(ms(3000), &[ImageId(3)]);
        assert_eq!(p.take_next(ms(3080)), Some(ImageId(3)));
        p.defer(ms(3080), ImageId(3));
        assert!(p.take_next(ms(3100)).is_none(), "another idle period first");
        assert_eq!(p.take_next(ms(3160)), Some(ImageId(3)));
        for i in 0..MAX_DEFERRALS + 5 {
            p.defer(ms(4000 + u64::from(i)), ImageId(3));
        }
        let mut tries = 0;
        while p.take_next(ms(100_000)).is_some() {
            tries += 1;
        }
        assert_eq!(tries, MAX_DEFERRALS - 1, "the 40 deferrals are the limit (one was used above)");
    }

    fn key(image: u64, fp: u64) -> BaseKey {
        BaseKey { image: ImageId(image), fingerprint: fp, preview: 2560, revision: 0 }
    }

    #[test]
    fn renderer_setting_parses() {
        assert_eq!(RendererMode::parse("auto"), Some(RendererMode::Auto));
        assert_eq!(RendererMode::parse(" CPU "), Some(RendererMode::Cpu));
        assert_eq!(RendererMode::parse("gpu"), Some(RendererMode::Gpu));
        assert_eq!(RendererMode::parse("vulkan"), None);
        assert_eq!(RendererMode::default(), RendererMode::Auto);
        assert_eq!(GpuFailure::parse("oom"), Some(GpuFailure::Oom));
        assert_eq!(GpuFailure::parse("nope"), None);
    }

    #[test]
    fn lru_keeps_three_and_drops_the_oldest() {
        let mut lru: Lru<u32, &str> = Lru::new(3);
        for (k, v) in [(1, "a"), (2, "b"), (3, "c")] {
            assert!(lru.make_room().is_empty());
            assert!(lru.insert(k, v).is_empty());
        }
        // Touching 1 makes 2 the oldest.
        assert!(lru.get_mut(&1).is_some());
        let freed = lru.make_room();
        assert_eq!(freed, vec!["b"]);
        lru.insert(4, "d");
        assert_eq!(lru.keys(), vec![3, 1, 4]);
        assert_eq!(lru.len(), 3);
        assert!(!lru.contains(&2));
    }

    #[test]
    fn oom_blocks_the_same_image_and_three_failures_are_permanent() {
        let mut fb = Fallback::new(RendererMode::Auto, None);
        assert_eq!(fb.decide(&key(1, 10)), Attempt::Try);
        assert!(fb.on_out_of_memory(&key(1, 10), "x").is_none());
        // Same image: no second attempt, even when only the fingerprint changed.
        assert!(matches!(fb.decide(&key(1, 11)), Attempt::Skip(_)));
        // Another image: one new attempt.
        assert_eq!(fb.decide(&key(2, 10)), Attempt::Try);
        assert!(fb.on_out_of_memory(&key(2, 10), "x").is_none());
        let note = fb.on_out_of_memory(&key(3, 10), "x");
        assert!(note.is_some_and(|n| n.contains("switched off")));
        assert!(matches!(fb.decide(&key(4, 10)), Attempt::Skip(why) if why.contains("switched off")));
    }

    #[test]
    fn a_success_resets_the_failure_count() {
        let mut fb = Fallback::new(RendererMode::Auto, None);
        fb.on_out_of_memory(&key(1, 1), "x");
        fb.on_out_of_memory(&key(2, 1), "x");
        fb.on_success();
        assert_eq!(fb.consecutive_oom(), 0);
        assert!(fb.on_out_of_memory(&key(3, 1), "x").is_none());
        assert_eq!(fb.decide(&key(4, 1)), Attempt::Try);
    }

    #[test]
    fn device_loss_and_missing_render_state_are_permanent() {
        let mut fb = Fallback::new(RendererMode::Auto, None);
        assert!(fb.on_device_lost().is_some());
        assert!(fb.on_device_lost().is_none(), "the note is given once");
        assert!(matches!(fb.decide(&key(1, 1)), Attempt::Skip(_)));
        let none = Fallback::new(RendererMode::Auto, Some("no wgpu render state".into()));
        assert!(matches!(none.decide(&key(1, 1)), Attempt::Skip(r) if r.contains("no wgpu")));
    }

    #[test]
    fn other_errors_are_logged_once_and_never_permanent() {
        let mut fb = Fallback::new(RendererMode::Auto, None);
        assert!(fb.on_other_error());
        assert!(!fb.on_other_error());
        assert_eq!(fb.decide(&key(1, 1)), Attempt::Try);
    }

    #[test]
    fn strict_mode_never_skips_after_failures() {
        let mut fb = Fallback::new(RendererMode::Gpu, None);
        for i in 0..5 {
            assert!(fb.on_out_of_memory(&key(i, 1), "x").is_none());
            assert_eq!(fb.decide(&key(i, 1)), Attempt::Try);
        }
    }

    #[test]
    fn scopes_are_coarse_while_dragging_and_exact_at_rest() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut s = ScopeScheduler::default();
        // The first request of a gesture is read exactly.
        assert_eq!(s.on_request(ms(0), 1), 1);
        assert_eq!(s.refine_due(), Some(ms(150)));
        // Requests every 16 ms: every 2nd pixel, a refinement is scheduled 150 ms after the last.
        assert_eq!(s.on_request(ms(16), 1), 2);
        assert_eq!(s.on_request(ms(32), 1), 2);
        assert_eq!(s.refine_due(), Some(ms(32 + 150)));
        assert!(!s.take_refine(ms(100)));
        assert!(s.take_refine(ms(182)));
        assert!(!s.take_refine(ms(300)), "only once");
        // A late request starts a new gesture.
        assert_eq!(s.on_request(ms(900), 1), 1);
        // A panel step of 4 stays 4 while dragging.
        assert_eq!(s.on_request(ms(910), 4), 4);
    }

    #[test]
    fn a_new_request_postpones_the_refinement() {
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut s = ScopeScheduler::default();
        s.on_request(ms(0), 1);
        s.on_request(ms(10), 1);
        assert_eq!(s.refine_due(), Some(ms(160)));
        s.on_request(ms(100), 1);
        assert_eq!(s.refine_due(), Some(ms(250)));
        s.on_request_without_scopes(ms(120));
        assert!(s.refine_due().is_none());
    }
}
