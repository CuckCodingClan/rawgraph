//! Importing ratings and colour labels from XMP: sidecar files (`.xmp` next to
//! the image) and packets embedded in the image files themselves. The files are
//! only read. Reading runs on a worker thread with progress and Cancel; the
//! results are applied to the project on the UI thread afterwards.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use rayon::prelude::*;
use rawgraph_core::ImageId;
use rawgraph_project::{parse_xmp_culling, xmp_sidecar_path, ColorLabel, Flag, ImageEntry, Project, XmpCulling};

/// Sidecars bigger than this are not XMP.
const MAX_SIDECAR_BYTES: u64 = 4 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XmpOptions {
    pub sidecars: bool,
    pub embedded: bool,
    /// Skip images that already carry a rating, flag or label here.
    pub only_unrated: bool,
}

impl Default for XmpOptions {
    fn default() -> Self {
        Self { sidecars: true, embedded: true, only_unrated: true }
    }
}

/// What one source (sidecar or embedded packet) gave for one image.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceRead {
    /// There is no such source for the image.
    Absent,
    /// The source was read; it may carry nothing useful.
    Read(XmpCulling),
    /// The source exists but cannot be read or parsed.
    Broken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Sidecar,
    Embedded,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    Use(XmpCulling, Origin),
    /// No usable rating or label anywhere.
    NoXmp,
    /// A source existed but was broken and nothing else helped.
    Error,
}

fn useful(c: &XmpCulling) -> bool {
    c.rating.is_some() || c.label.is_some()
}

/// The decision table. The sidecar wins whenever it carries something; the
/// embedded packet is the fallback; a broken source only counts as an error
/// when nothing else gave data.
pub fn decide(opts: &XmpOptions, sidecar: &SourceRead, embedded: &SourceRead) -> Decision {
    let mut broken = false;
    if opts.sidecars {
        match sidecar {
            SourceRead::Read(c) if useful(c) => return Decision::Use(c.clone(), Origin::Sidecar),
            SourceRead::Broken => broken = true,
            _ => {}
        }
    }
    if opts.embedded {
        match embedded {
            SourceRead::Read(c) if useful(c) => return Decision::Use(c.clone(), Origin::Embedded),
            SourceRead::Broken => broken = true,
            _ => {}
        }
    }
    if broken {
        Decision::Error
    } else {
        Decision::NoXmp
    }
}

/// An image to look at, with the state that decides whether it is skipped.
#[derive(Clone, Debug)]
pub struct Target {
    pub id: ImageId,
    pub path: PathBuf,
}

/// Images that are looked at: not missing, and (with `only_unrated`) still without rating, flag and label.
/// Returns `(targets, skipped because already rated)`.
pub fn pick_targets<'a>(entries: impl IntoIterator<Item = &'a ImageEntry>, opts: &XmpOptions) -> (Vec<Target>, usize) {
    let mut skipped = 0;
    let mut out = Vec::new();
    for e in entries {
        if e.missing {
            continue;
        }
        if opts.only_unrated && (e.rating != 0 || e.flag != Flag::None || e.color_label.is_some()) {
            skipped += 1;
            continue;
        }
        out.push(Target { id: e.id, path: e.path.clone() });
    }
    (out, skipped)
}

fn read_sidecar(path: &Path) -> SourceRead {
    let Some(sidecar) = xmp_sidecar_path(path) else { return SourceRead::Absent };
    let ok_size = std::fs::metadata(&sidecar).is_ok_and(|m| m.len() <= MAX_SIDECAR_BYTES);
    if !ok_size {
        return SourceRead::Broken;
    }
    match std::fs::read(&sidecar) {
        Ok(bytes) => match parse_xmp_culling(&String::from_utf8_lossy(&bytes)) {
            Ok(c) => SourceRead::Read(c),
            Err(_) => SourceRead::Broken,
        },
        Err(_) => SourceRead::Broken,
    }
}

fn read_embedded(path: &Path) -> SourceRead {
    match rawgraph_io::read_xmp_packet(path) {
        None => SourceRead::Absent,
        Some(packet) => match parse_xmp_culling(&packet) {
            Ok(c) => SourceRead::Read(c),
            Err(_) => SourceRead::Broken,
        },
    }
}

/// Reads what the options ask for and decides.
pub fn read_target(path: &Path, opts: &XmpOptions) -> Decision {
    let sidecar = if opts.sidecars { read_sidecar(path) } else { SourceRead::Absent };
    // The embedded packet is not even opened when the sidecar already decides.
    if opts.sidecars && matches!(&sidecar, SourceRead::Read(c) if useful(c)) {
        return decide(opts, &sidecar, &SourceRead::Absent);
    }
    let embedded = if opts.embedded { read_embedded(path) } else { SourceRead::Absent };
    decide(opts, &sidecar, &embedded)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct XmpSummary {
    pub changed: usize,
    /// Had XMP data that changes nothing here, plus the images skipped because they are rated already.
    pub unchanged: usize,
    pub skipped_rated: usize,
    pub without_xmp: usize,
    pub errors: usize,
    pub from_sidecar: usize,
    pub from_embedded: usize,
}

/// What the worker found.
#[derive(Debug, Default)]
pub struct XmpFound {
    pub items: Vec<(ImageId, XmpCulling, Origin)>,
    pub without_xmp: usize,
    pub errors: usize,
    pub skipped_rated: usize,
    pub checked: usize,
}

impl XmpFound {
    /// Images whose XMP carries a star rating (for the "n images have ratings" banner).
    pub fn rated_count(&self) -> usize {
        self.items.iter().filter(|(_, c, _)| matches!(c.rating, Some(1..=5))).count()
    }
}

/// Applies the found data to the project; returns the summary and the ids that changed.
pub fn apply(project: &mut Project, found: &XmpFound) -> (XmpSummary, Vec<ImageId>) {
    let mut s = XmpSummary { without_xmp: found.without_xmp, errors: found.errors, skipped_rated: found.skipped_rated, unchanged: found.skipped_rated, ..Default::default() };
    let mut changed = Vec::new();
    for (id, culling, origin) in &found.items {
        if project.apply_xmp_culling(*id, culling) {
            s.changed += 1;
            changed.push(*id);
        } else {
            s.unchanged += 1;
        }
        match origin {
            Origin::Sidecar => s.from_sidecar += 1,
            Origin::Embedded => s.from_embedded += 1,
        }
    }
    (s, changed)
}

/// Whether the entry would change if `c` were applied (used by tests and the banner).
#[allow(dead_code)]
pub fn would_change(e: &ImageEntry, c: &XmpCulling) -> bool {
    let rating = match c.rating {
        Some(r @ 1..=5) => e.rating != r as u8,
        Some(-1) => e.flag != Flag::Reject,
        _ => false,
    };
    let label = c.label.as_deref().and_then(label_of).is_some_and(|l| e.color_label != Some(l));
    rating || label
}

fn label_of(text: &str) -> Option<ColorLabel> {
    match text.trim().to_lowercase().as_str() {
        "red" | "rot" => Some(ColorLabel::Red),
        "yellow" | "gelb" => Some(ColorLabel::Yellow),
        "green" | "grün" | "gruen" => Some(ColorLabel::Green),
        "blue" | "blau" => Some(ColorLabel::Blue),
        "purple" | "lila" => Some(ColorLabel::Purple),
        _ => None,
    }
}

/// Number of files an XMP import reads at the same time. Reading is waiting for
/// the disk or the network share, not computing, so the number is fixed instead
/// of following the CPU count, and the reads run on their own pool: the global
/// rayon pool belongs to rendering and must not be held up by a slow share.
pub const XMP_READ_THREADS: usize = 8;

/// Reads every target with at most `threads` reads at a time. The result is in
/// target order whatever finishes first. After `cancel` is set no new file is
/// started (those entries are `None`). `done` counts finished files for progress.
fn read_all(
    targets: &[Target],
    threads: usize,
    cancel: &AtomicBool,
    done: &AtomicUsize,
    read: &(dyn Fn(&Path) -> Decision + Sync),
) -> Vec<Option<(ImageId, Decision)>> {
    let one = |t: &Target| {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let decision = read(&t.path);
        done.fetch_add(1, Ordering::Relaxed);
        Some((t.id, decision))
    };
    match rayon::ThreadPoolBuilder::new().num_threads(threads.max(1)).thread_name(|i| format!("rawgraph-xmp-read-{i}")).build() {
        Ok(pool) => pool.install(|| targets.par_iter().map(one).collect()),
        // No threads available: read one after the other.
        Err(_) => targets.iter().map(one).collect(),
    }
}

pub enum JobResult {
    Done(XmpFound),
    Cancelled,
}

/// A running read of XMP data on worker threads.
pub struct XmpJob {
    rx: Receiver<JobResult>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    pub total: usize,
    pub started: Instant,
}

impl XmpJob {
    pub fn start(ctx: egui::Context, targets: Vec<Target>, skipped_rated: usize, opts: XmpOptions) -> Self {
        Self::start_with_cancel(ctx, targets, skipped_rated, opts, Arc::new(AtomicBool::new(false)))
    }

    /// Like [`XmpJob::start`] with the cancel flag supplied by the caller.
    pub fn start_with_cancel(ctx: egui::Context, targets: Vec<Target>, skipped_rated: usize, opts: XmpOptions, cancel: Arc<AtomicBool>) -> Self {
        let (tx, rx) = channel();
        let done = Arc::new(AtomicUsize::new(0));
        let total = targets.len();
        let (c2, d2) = (cancel.clone(), done.clone());
        let spawned = std::thread::Builder::new().name("rawgraph-xmp".into()).spawn(move || {
            let results = read_all(&targets, XMP_READ_THREADS, &c2, &d2, &|path| read_target(path, &opts));
            let reply = if c2.load(Ordering::Relaxed) {
                JobResult::Cancelled
            } else {
                let mut found = XmpFound { skipped_rated, checked: results.len(), ..Default::default() };
                for (id, d) in results.into_iter().flatten() {
                    match d {
                        Decision::Use(c, origin) => found.items.push((id, c, origin)),
                        Decision::NoXmp => found.without_xmp += 1,
                        Decision::Error => found.errors += 1,
                    }
                }
                JobResult::Done(found)
            };
            let _ = tx.send(reply);
            ctx.request_repaint();
        });
        if spawned.is_err() {
            // The channel closes and the app reports the failure.
        }
        Self { rx, cancel, done, total, started: Instant::now() }
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn progress(&self) -> usize {
        self.done.load(Ordering::Relaxed).min(self.total)
    }

    /// `Some(Err(()))` when the worker died without a result.
    pub fn poll(&self) -> Option<Result<JobResult, ()>> {
        match self.rx.try_recv() {
            Ok(r) => Some(Ok(r)),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(())),
        }
    }
}

/// State of the "Import ratings from XMP" dialog.
pub struct XmpDialog {
    pub opts: XmpOptions,
    pub summary: Option<XmpSummary>,
    pub elapsed_ms: Option<u128>,
    pub message: Option<String>,
}

impl XmpDialog {
    pub fn new() -> Self {
        Self { opts: XmpOptions::default(), summary: None, elapsed_ms: None, message: None }
    }
}

/// Banner in the filmstrip header: images in this project have ratings in XMP.
pub struct XmpOffer {
    pub count: usize,
    pub found: XmpFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rating(r: i8) -> XmpCulling {
        XmpCulling { rating: Some(r), label: None }
    }

    const BOTH: XmpOptions = XmpOptions { sidecars: true, embedded: true, only_unrated: true };

    #[test]
    fn the_sidecar_wins_over_the_embedded_packet() {
        let d = decide(&BOTH, &SourceRead::Read(rating(2)), &SourceRead::Read(rating(5)));
        assert_eq!(d, Decision::Use(rating(2), Origin::Sidecar));
        // Without a sidecar the embedded packet decides.
        assert_eq!(decide(&BOTH, &SourceRead::Absent, &SourceRead::Read(rating(5))), Decision::Use(rating(5), Origin::Embedded));
        // A sidecar that says nothing does not hide the embedded rating.
        let empty = SourceRead::Read(XmpCulling::default());
        assert_eq!(decide(&BOTH, &empty, &SourceRead::Read(rating(4))), Decision::Use(rating(4), Origin::Embedded));
        // Sidecar only / embedded only.
        let only_sidecar = XmpOptions { embedded: false, ..BOTH };
        assert_eq!(decide(&only_sidecar, &SourceRead::Absent, &SourceRead::Read(rating(5))), Decision::NoXmp);
        let only_embedded = XmpOptions { sidecars: false, ..BOTH };
        assert_eq!(decide(&only_embedded, &SourceRead::Read(rating(1)), &SourceRead::Read(rating(5))), Decision::Use(rating(5), Origin::Embedded));
    }

    #[test]
    fn broken_sources_are_errors_only_when_nothing_else_helps() {
        assert_eq!(decide(&BOTH, &SourceRead::Broken, &SourceRead::Read(rating(3))), Decision::Use(rating(3), Origin::Embedded));
        assert_eq!(decide(&BOTH, &SourceRead::Broken, &SourceRead::Absent), Decision::Error);
        assert_eq!(decide(&BOTH, &SourceRead::Absent, &SourceRead::Broken), Decision::Error);
        assert_eq!(decide(&BOTH, &SourceRead::Absent, &SourceRead::Absent), Decision::NoXmp);
        // A source that is switched off is never an error.
        let only_embedded = XmpOptions { sidecars: false, ..BOTH };
        assert_eq!(decide(&only_embedded, &SourceRead::Broken, &SourceRead::Absent), Decision::NoXmp);
    }

    fn entry(id: u64, name: &str, rating: u8) -> ImageEntry {
        ImageEntry {
            id: ImageId(id),
            path: PathBuf::from(name),
            content_hash: String::new(),
            file_size: 0,
            modified_unix_ms: None,
            capture: Default::default(),
            rating,
            flag: Flag::None,
            color_label: None,
            missing: false,
            uuid: format!("{id:032x}"),
        }
    }

    #[test]
    fn only_unrated_skips_rated_and_missing_images() {
        let mut gone = entry(3, "c.png", 0);
        gone.missing = true;
        let mut flagged = entry(4, "d.png", 0);
        flagged.flag = Flag::Pick;
        let all = [entry(1, "a.png", 0), entry(2, "b.png", 3), gone, flagged];
        let (t, skipped) = pick_targets(&all, &XmpOptions::default());
        assert_eq!((t.iter().map(|t| t.id.0).collect::<Vec<_>>(), skipped), (vec![1], 2));
        let (t, skipped) = pick_targets(&all, &XmpOptions { only_unrated: false, ..BOTH });
        assert_eq!((t.iter().map(|t| t.id.0).collect::<Vec<_>>(), skipped), (vec![1, 2, 4], 0));
    }

    const SIDECAR_XML: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="RATING" xmp:Label="Red"/></rdf:RDF></x:xmpmeta>"#;

    fn png(path: &Path, packet: Option<&str>) {
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([40, 80, 120, 255]));
        img.save(path).unwrap();
        if let Some(p) = packet {
            // Embed the packet as an uncompressed iTXt chunk before IEND.
            let mut bytes = std::fs::read(path).unwrap();
            let iend = bytes.len() - 12;
            let mut data = b"XML:com.adobe.xmp\0\0\0\0\0".to_vec();
            data.extend_from_slice(p.as_bytes());
            let mut chunk = Vec::new();
            chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = b"iTXt".to_vec();
            body.extend_from_slice(&data);
            chunk.extend_from_slice(&body);
            chunk.extend_from_slice(&crc32(&body).to_be_bytes());
            bytes.splice(iend..iend, chunk);
            std::fs::write(path, bytes).unwrap();
        }
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    fn run(job: XmpJob) -> XmpFound {
        let t = Instant::now();
        while t.elapsed().as_secs() < 30 {
            match job.poll() {
                Some(Ok(JobResult::Done(f))) => return f,
                Some(_) => panic!("job did not finish normally"),
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        panic!("timeout");
    }

    #[test]
    fn real_files_sidecar_and_embedded_with_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        // a: sidecar 2 + embedded 5 -> 2.  b: embedded 4 only.  c: no XMP.  d: broken sidecar, no packet.  e: sidecar Rating -1.
        png(&p("a.png"), Some(&SIDECAR_XML.replace("RATING", "5")));
        std::fs::write(p("a.xmp"), SIDECAR_XML.replace("RATING", "2")).unwrap();
        png(&p("b.png"), Some(&SIDECAR_XML.replace("RATING", "4")));
        png(&p("c.png"), None);
        png(&p("d.png"), None);
        std::fs::write(p("d.xmp"), "<<< not xml").unwrap();
        png(&p("e.png"), None);
        std::fs::write(p("e.xmp"), SIDECAR_XML.replace("RATING", "-1")).unwrap();
        let mut project = Project::new();
        project.scan_folder(dir.path(), false, &rawgraph_io::is_supported, &|_| None);
        let (targets, skipped) = pick_targets(project.images(), &XmpOptions::default());
        assert_eq!((targets.len(), skipped), (5, 0));
        let found = run(XmpJob::start(egui::Context::default(), targets, skipped, XmpOptions::default()));
        assert_eq!((found.items.len(), found.without_xmp, found.errors), (3, 1, 1));
        let (summary, changed) = apply(&mut project, &found);
        assert_eq!((summary.changed, summary.unchanged, summary.without_xmp, summary.errors), (3, 0, 1, 1));
        assert_eq!((summary.from_sidecar, summary.from_embedded), (2, 1));
        assert_eq!(changed.len(), 3);
        let by = |n: &str| project.images().iter().find(|e| e.file_name() == n).unwrap();
        assert_eq!(by("a.png").rating, 2, "the sidecar wins");
        assert_eq!(by("a.png").color_label, Some(ColorLabel::Red));
        assert_eq!(by("b.png").rating, 4, "embedded when there is no sidecar");
        assert_eq!(by("e.png").flag, Flag::Reject);
        assert_eq!(by("c.png").rating, 0);
        // The image files and sidecars are untouched by the import (read only).
        assert!(std::fs::read_to_string(p("a.xmp")).unwrap().contains("Rating=\"2\""));
        // Second run: "only unrated" now skips the three imported images.
        let (targets, skipped) = pick_targets(project.images(), &XmpOptions::default());
        assert_eq!((targets.len(), skipped), (2, 3));
        // Switching it off re-reads everything; nothing changes any more.
        let all = XmpOptions { only_unrated: false, ..BOTH };
        let (targets, skipped) = pick_targets(project.images(), &all);
        let found = run(XmpJob::start(egui::Context::default(), targets, skipped, all));
        let (summary, _) = apply(&mut project, &found);
        assert_eq!((summary.changed, summary.unchanged), (0, 3));
        assert_eq!(found.rated_count(), 2, "a and b carry star ratings; e is a reject");
    }

    #[test]
    fn cancel_gives_no_result_to_apply() {
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for i in 0..200 {
            let path = dir.path().join(format!("{i}.png"));
            png(&path, Some(&SIDECAR_XML.replace("RATING", "3")));
            targets.push(Target { id: ImageId(i + 1), path });
        }
        let flag = Arc::new(AtomicBool::new(true));
        let job = XmpJob::start_with_cancel(egui::Context::default(), targets, 0, BOTH, flag);
        let t = Instant::now();
        loop {
            match job.poll() {
                Some(Ok(JobResult::Cancelled)) => break,
                Some(Ok(JobResult::Done(_))) => panic!("cancelled before it started: must not report Done"),
                Some(Err(())) => panic!("worker died"),
                None if t.elapsed().as_secs() > 30 => panic!("timeout"),
                None => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
        assert_eq!(job.progress(), 0, "nothing was read after the cancel");
    }

    #[test]
    fn would_change_mirrors_the_apply_rules() {
        let e = entry(1, "a.png", 3);
        assert!(would_change(&e, &rating(4)) && !would_change(&e, &rating(3)) && !would_change(&e, &rating(0)));
        assert!(would_change(&e, &rating(-1)));
        assert!(would_change(&e, &XmpCulling { rating: None, label: Some("Rot".into()) }));
        assert!(!would_change(&e, &XmpCulling { rating: None, label: Some("Turquoise".into()) }));
    }

    fn fake_targets(n: u64) -> Vec<Target> {
        (1..=n).map(|i| Target { id: ImageId(i), path: PathBuf::from(format!("/share/IMG_{i}.CR3")) }).collect()
    }

    /// A reader that takes a while (like a network share) and watches how many reads overlap.
    fn slow_reader<'a>(live: &'a AtomicUsize, peak: &'a AtomicUsize) -> impl Fn(&Path) -> Decision + Sync + 'a {
        move |path: &Path| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            // Later files finish first, so a wrong order would show.
            let n: u64 = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.trim_start_matches("IMG_").parse().ok()).unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_millis(40u64.saturating_sub(n % 40)));
            live.fetch_sub(1, Ordering::SeqCst);
            match n % 3 {
                0 => Decision::NoXmp,
                1 => Decision::Use(rating((n % 5) as i8 + 1), Origin::Sidecar),
                _ => Decision::Error,
            }
        }
    }

    #[test]
    fn xmp_files_are_read_in_parallel_but_never_more_than_the_pool_size_and_in_order() {
        let targets = fake_targets(64);
        let (live, peak, done) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
        let reader = slow_reader(&live, &peak);
        let started = Instant::now();
        let parallel = read_all(&targets, XMP_READ_THREADS, &AtomicBool::new(false), &done, &reader);
        let parallel_time = started.elapsed();
        let peak = peak.load(Ordering::SeqCst);
        assert!((2..=XMP_READ_THREADS).contains(&peak), "reads overlap but never exceed the pool size: {peak}");
        assert_eq!(done.load(Ordering::SeqCst), 64, "progress counts every file");
        // Identical to reading one after the other, in target order.
        let serial = read_all(&targets, 1, &AtomicBool::new(false), &AtomicUsize::new(0), &reader);
        assert_eq!(parallel, serial);
        assert_eq!(parallel.iter().flatten().map(|(id, _)| id.0).collect::<Vec<_>>(), (1..=64).collect::<Vec<_>>());
        eprintln!("64 slow reads: {parallel_time:?} with {XMP_READ_THREADS} threads");
    }

    #[test]
    fn a_cancelled_xmp_read_starts_no_new_file() {
        let targets = fake_targets(50);
        let (live, peak, done) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0));
        let cancel = AtomicBool::new(false);
        let reader = slow_reader(&live, &peak);
        let results = read_all(&targets, 4, &cancel, &done, &|p: &Path| {
            cancel.store(true, Ordering::SeqCst);
            reader(p)
        });
        assert!(results.iter().filter(|r| r.is_some()).count() <= 4 + 1, "only the reads that were already running finish");
        assert_eq!(results.len(), 50);
    }

    #[test]
    fn errors_stay_per_file_in_a_parallel_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut targets = Vec::new();
        for i in 0..40u64 {
            let path = dir.path().join(format!("{i}.png"));
            match i % 4 {
                0 => png(&path, Some(&SIDECAR_XML.replace("RATING", "3"))),
                1 => {
                    png(&path, None);
                    std::fs::write(path.with_extension("xmp"), "<<< broken").unwrap();
                }
                _ => png(&path, None),
            }
            targets.push(Target { id: ImageId(i + 1), path });
        }
        let found = run(XmpJob::start(egui::Context::default(), targets.clone(), 0, BOTH));
        assert_eq!((found.items.len(), found.errors, found.without_xmp), (10, 10, 20));
        let ids: Vec<u64> = found.items.iter().map(|(id, _, _)| id.0).collect();
        assert_eq!(ids, (0..40u64).filter(|i| i % 4 == 0).map(|i| i + 1).collect::<Vec<_>>(), "found items stay in image order");
    }
}
