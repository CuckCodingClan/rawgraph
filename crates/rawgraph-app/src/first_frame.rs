//! `--bench-first-frame`: samples, statistics, table and CSV of the "selection to first visible
//! frame" measurement. The driver that selects images and takes the times lives in
//! `app/first_frame.rs`; everything here is plain data and easy to test.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Whether RawGraph's own caches were bypassed for the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Pass {
    Uncached,
    Cached,
}

/// One selection of one image.
#[derive(Clone, Debug)]
pub struct Sample {
    pub run: u32,
    pub pass: Pass,
    pub index: usize,
    pub file: String,
    /// Upper-case extension ("ARW", "CR3", "DNG", ...).
    pub format: String,
    pub camera: String,
    pub file_bytes: u64,
    /// Decoded proxy at the time the final frame arrived.
    pub proxy: Option<(u32, u32)>,
    /// "GPU" or "CPU".
    pub path: &'static str,
    /// Selection request until the first picture of any kind was shown.
    pub first_ms: f32,
    /// What that first picture was: "edited thumbnail", "proxy cache", "thumbnail" (filmstrip),
    /// "thumbnail cache", "embedded preview" or "final frame".
    pub first_source: &'static str,
    /// Selection request until the final graph render was shown.
    pub final_ms: f32,
    /// Worker-side time of the final frame: graph evaluation and display transform (ms).
    pub eval_ms: f32,
    pub display_ms: f32,
    /// No final frame arrived (timeout or render error); excluded from the statistics.
    pub failed: bool,
}

/// Label of the statistics group a sample belongs to. Run 1 of the uncached pass is the first
/// look at the folder; later runs find the files in the operating system's file cache.
pub fn pass_label(sample: &Sample) -> &'static str {
    match (sample.pass, sample.run) {
        (Pass::Cached, _) => "cached",
        (Pass::Uncached, 1) => "uncached (first pass)",
        (Pass::Uncached, _) => "uncached (repeat)",
    }
}

/// Nearest-rank percentile of an ascending list; `p` in 0..=100. NaN for an empty list.
pub fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let rank = ((p / 100.0 * sorted.len() as f32).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// Median (mean of the two middle values for an even count). NaN for an empty list.
pub fn median(sorted: &[f32]) -> f32 {
    match sorted.len() {
        0 => f32::NAN,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    }
}

fn sorted(mut v: Vec<f32>) -> Vec<f32> {
    v.sort_by(f32::total_cmp);
    v
}

/// Statistics of one group of samples (a pass label, optionally one camera format).
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub label: &'static str,
    /// Camera format, or "all".
    pub format: String,
    pub n: usize,
    pub failed: usize,
    pub file_mb_median: f32,
    /// Most common proxy size ("2560x1707"), or "-".
    pub proxy: String,
    /// "GPU", "CPU" or "GPU+CPU".
    pub path: String,
    pub first_median: f32,
    pub first_p95: f32,
    pub final_median: f32,
    pub final_p95: f32,
}

fn summarize_group(label: &'static str, format: &str, group: &[&Sample]) -> Summary {
    let ok: Vec<&Sample> = group.iter().copied().filter(|s| !s.failed).collect();
    let first = sorted(ok.iter().map(|s| s.first_ms).collect());
    let last = sorted(ok.iter().map(|s| s.final_ms).collect());
    let sizes = sorted(group.iter().map(|s| s.file_bytes as f32 / (1024.0 * 1024.0)).collect());
    let mut proxies: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    for p in group.iter().filter_map(|s| s.proxy) {
        *proxies.entry(p).or_default() += 1;
    }
    let proxy = proxies.into_iter().max_by_key(|(_, n)| *n).map_or_else(|| "-".to_owned(), |((w, h), _)| format!("{w}x{h}"));
    let mut paths: Vec<&str> = group.iter().map(|s| s.path).collect();
    paths.sort_unstable();
    paths.dedup();
    Summary {
        label,
        format: format.to_owned(),
        n: ok.len(),
        failed: group.len() - ok.len(),
        file_mb_median: median(&sizes),
        proxy,
        path: if paths.is_empty() { "-".to_owned() } else { paths.join("+") },
        first_median: median(&first),
        first_p95: percentile(&first, 95.0),
        final_median: median(&last),
        final_p95: percentile(&last, 95.0),
    }
}

/// One summary per pass label (all formats), followed by one per pass label and camera format
/// when the folder holds more than one format.
pub fn summarize(samples: &[Sample]) -> Vec<Summary> {
    let mut labels: Vec<&'static str> = Vec::new();
    for s in samples {
        let l = pass_label(s);
        if !labels.contains(&l) {
            labels.push(l);
        }
    }
    let order = |l: &&str| match *l {
        "uncached (first pass)" => 0,
        "uncached (repeat)" => 1,
        _ => 2,
    };
    labels.sort_by_key(order);
    let mut out = Vec::new();
    for label in labels {
        let group: Vec<&Sample> = samples.iter().filter(|s| pass_label(s) == label).collect();
        out.push(summarize_group(label, "all", &group));
        let mut formats: Vec<&str> = group.iter().map(|s| s.format.as_str()).collect();
        formats.sort_unstable();
        formats.dedup();
        if formats.len() > 1 {
            for f in formats {
                let sub: Vec<&Sample> = group.iter().copied().filter(|s| s.format == f).collect();
                out.push(summarize_group(label, f, &sub));
            }
        }
    }
    out
}

fn ms(v: f32) -> String {
    if v.is_nan() { "n/a".to_owned() } else { format!("{v:.0}") }
}

/// Fixed-width table of the summaries (all times in ms).
pub fn table(summaries: &[Summary]) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<22} {:<8} {:>3} {:>4} {:>8} {:>10} {:>8} | {:>9} {:>8} | {:>9} {:>8}",
        "views", "format", "n", "fail", "file MB", "proxy", "path", "1st med", "1st p95", "final med", "fin p95"
    );
    for s in summaries {
        let _ = writeln!(
            out,
            "{:<22} {:<8} {:>3} {:>4} {:>8.1} {:>10} {:>8} | {:>9} {:>8} | {:>9} {:>8}",
            s.label, s.format, s.n, s.failed, s.file_mb_median, s.proxy, s.path, ms(s.first_median), ms(s.first_p95), ms(s.final_median), ms(s.final_p95)
        );
    }
    out
}

fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) { format!("\"{}\"", v.replace('"', "\"\"")) } else { v.to_owned() }
}

pub const SAMPLE_HEADER: &str = "run,pass,views,index,file,format,camera,file_bytes,proxy_w,proxy_h,path,first_picture_ms,first_picture_source,final_frame_ms,eval_ms,display_ms,failed";

/// One row per selection.
pub fn samples_csv(samples: &[Sample]) -> String {
    let mut out = format!("{SAMPLE_HEADER}\n");
    for s in samples {
        let (pw, ph) = s.proxy.map_or((String::new(), String::new()), |(w, h)| (w.to_string(), h.to_string()));
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{},{},{:.1},{},{:.1},{:.1},{:.1},{}",
            s.run,
            if s.pass == Pass::Uncached { "uncached" } else { "cached" },
            pass_label(s),
            s.index,
            csv_field(&s.file),
            csv_field(&s.format),
            csv_field(&s.camera),
            s.file_bytes,
            pw,
            ph,
            s.path,
            s.first_ms,
            s.first_source,
            s.final_ms,
            s.eval_ms,
            s.display_ms,
            s.failed
        );
    }
    out
}

pub const SUMMARY_HEADER: &str = "views,format,n,failed,file_mb_median,proxy,path,first_median_ms,first_p95_ms,final_median_ms,final_p95_ms";

pub fn summary_csv(summaries: &[Summary]) -> String {
    let mut out = format!("{SUMMARY_HEADER}\n");
    for s in summaries {
        let _ = writeln!(
            out,
            "{},{},{},{},{:.2},{},{},{:.1},{:.1},{:.1},{:.1}",
            csv_field(s.label), csv_field(&s.format), s.n, s.failed, s.file_mb_median, s.proxy, s.path, s.first_median, s.first_p95, s.final_median, s.final_p95
        );
    }
    out
}

/// `<stem>-summary.csv` next to the per-image CSV.
pub fn summary_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().map_or_else(|| "bench".into(), |s| s.to_string_lossy().into_owned());
    out.with_file_name(format!("{stem}-summary.csv"))
}

/// Files (size, modification time), directories (modification time, which changes when an entry
/// is added or removed) and unreadable directories of a folder tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub files: Vec<(PathBuf, u64, Option<SystemTime>)>,
    pub dirs: Vec<(PathBuf, Option<SystemTime>)>,
    pub unreadable: Vec<PathBuf>,
}

/// Records the tree below `root` (sorted), to prove that the run did not write there.
pub fn folder_snapshot(root: &Path) -> Snapshot {
    let mut out = Snapshot::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        out.dirs.push((dir.clone(), std::fs::metadata(&dir).and_then(|m| m.modified()).ok()));
        let Ok(read) = std::fs::read_dir(&dir) else {
            out.unreadable.push(dir);
            continue;
        };
        for entry in read {
            let Ok(entry) = entry else {
                out.unreadable.push(dir.clone());
                continue;
            };
            let path = entry.path();
            match entry.metadata() {
                Ok(m) if m.is_dir() => stack.push(path),
                Ok(m) => out.files.push((path, m.len(), m.modified().ok())),
                Err(_) => out.unreadable.push(path),
            }
        }
    }
    out.files.sort_by(|a, b| a.0.cmp(&b.0));
    out.dirs.sort_by(|a, b| a.0.cmp(&b.0));
    out.unreadable.sort();
    out
}

fn inside(inner: &Path, outer: &Path) -> bool {
    // Canonical form of the nearest existing ancestor plus the missing rest (a data folder that
    // does not exist yet must compare in the same form as the photo folder).
    let canon = |p: &Path| {
        let mut rest = Vec::new();
        let mut base = p.to_path_buf();
        loop {
            if let Ok(c) = std::fs::canonicalize(&base) {
                return rest.iter().rev().fold(c, |acc, part: &std::ffi::OsString| acc.join(part));
            }
            match (base.file_name().map(std::ffi::OsString::from), base.parent().map(Path::to_path_buf)) {
                (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                    rest.push(name);
                    base = parent;
                }
                _ => return p.to_path_buf(),
            }
        }
    };
    canon(inner).starts_with(canon(outer))
}

fn has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut r| r.next().is_some())
}

/// Refuses settings that could write into the photo folder or disturb real user data:
/// a missing or empty folder, a data folder inside the photo folder or one that already holds
/// recovery copies or recipes, and an output file inside the photo folder.
pub fn validate(folder: &Path, data_dir: Option<&Path>, out: Option<&Path>) -> Result<(), String> {
    if !folder.is_dir() {
        return Err(format!("--bench-first-frame: {} is not a folder", folder.display()));
    }
    if let Some(data) = data_dir {
        if inside(data, folder) {
            return Err(format!("--data-dir {} lies inside the folder that is measured; the run only reads that folder", data.display()));
        }
        for sub in ["recovery", "recipes"] {
            if has_entries(&data.join(sub)) {
                return Err(format!("--data-dir {} already holds {sub} data; use a fresh folder for the benchmark (it empties the caches there)", data.display()));
            }
        }
    }
    if let Some(out) = out {
        let parent = out.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        if inside(parent, folder) {
            return Err(format!("--bench-out {} lies inside the folder that is measured; the run only reads that folder", out.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(run: u32, pass: Pass, format: &str, first: f32, last: f32) -> Sample {
        Sample {
            run,
            pass,
            index: 0,
            file: "a,b.arw".into(),
            format: format.into(),
            camera: "Cam \"X\"".into(),
            file_bytes: 50 << 20,
            proxy: Some((2560, 1707)),
            path: "GPU",
            first_ms: first,
            first_source: "thumbnail",
            final_ms: last,
            eval_ms: 40.0,
            display_ms: 2.5,
            failed: false,
        }
    }

    #[test]
    fn percentile_and_median() {
        let v: Vec<f32> = (1..=20).map(|i| i as f32).collect();
        assert_eq!(percentile(&v, 95.0), 19.0);
        assert_eq!(percentile(&v, 100.0), 20.0);
        assert_eq!(percentile(&v, 0.0), 1.0);
        assert_eq!(median(&v), 10.5);
        assert_eq!(median(&[3.0]), 3.0);
        assert!(median(&[]).is_nan() && percentile(&[], 95.0).is_nan());
        assert_eq!(percentile(&[5.0, 9.0], 95.0), 9.0);
    }

    #[test]
    fn groups_split_first_pass_repeat_and_cached_and_formats() {
        let mut samples = vec![
            sample(1, Pass::Uncached, "ARW", 100.0, 900.0),
            sample(1, Pass::Uncached, "CR3", 200.0, 1100.0),
            sample(2, Pass::Uncached, "ARW", 50.0, 600.0),
            sample(1, Pass::Cached, "ARW", 10.0, 80.0),
        ];
        let mut bad = sample(1, Pass::Cached, "ARW", 1.0, 99999.0);
        bad.failed = true;
        samples.push(bad);
        let s = summarize(&samples);
        let labels: Vec<(&str, &str, usize)> = s.iter().map(|x| (x.label, x.format.as_str(), x.n)).collect();
        assert_eq!(
            labels,
            vec![
                ("uncached (first pass)", "all", 2),
                ("uncached (first pass)", "ARW", 1),
                ("uncached (first pass)", "CR3", 1),
                ("uncached (repeat)", "all", 1),
                ("cached", "all", 1),
            ]
        );
        assert_eq!(s[0].first_median, 150.0);
        assert_eq!(s[0].final_p95, 1100.0);
        assert_eq!(s[4].failed, 1);
        assert_eq!(s[4].final_median, 80.0);
        assert_eq!(s[0].proxy, "2560x1707");
        assert_eq!(s[0].path, "GPU");
        let t = table(&s);
        assert!(t.contains("uncached (first pass)") && t.lines().count() == 6, "{t}");
    }

    #[test]
    fn csv_escapes_and_has_stable_columns() {
        let csv = samples_csv(&[sample(1, Pass::Uncached, "ARW", 12.34, 567.8)]);
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some(SAMPLE_HEADER));
        let row = lines.next().unwrap();
        assert!(row.starts_with("1,uncached,uncached (first pass),0,\"a,b.arw\",ARW,\"Cam \"\"X\"\"\",52428800,2560,1707,GPU,12.3,thumbnail,567.8,40.0,2.5,false"), "{row}");
        assert_eq!(SAMPLE_HEADER.split(',').count(), row.matches(',').count(), "one comma inside the quoted file name");
        let sum = summary_csv(&summarize(&[sample(1, Pass::Cached, "ARW", 1.0, 2.0)]));
        assert_eq!(sum.lines().next(), Some(SUMMARY_HEADER));
        assert!(sum.lines().nth(1).unwrap().starts_with("cached,all,1,0,50.00,2560x1707,GPU,1.0,1.0,2.0,2.0"));
        assert_eq!(summary_path(Path::new("C:/x/run.csv")), PathBuf::from("C:/x/run-summary.csv"));
    }

    #[test]
    fn snapshot_sees_changes_but_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.raw"), b"abc").unwrap();
        std::fs::write(dir.path().join("sub").join("b.raw"), b"de").unwrap();
        let before = folder_snapshot(dir.path());
        assert_eq!(before.files.len(), 2);
        assert_eq!(before.dirs.len(), 2);
        assert!(before.unreadable.is_empty());
        assert_eq!(folder_snapshot(dir.path()), before);
        std::fs::write(dir.path().join("a.raw"), b"abcd").unwrap();
        assert_ne!(folder_snapshot(dir.path()), before);
        assert_eq!(folder_snapshot(&dir.path().join("missing")).unreadable.len(), 1);
    }

    #[test]
    fn validation_refuses_unsafe_locations() {
        let root = tempfile::tempdir().unwrap();
        let photos = root.path().join("photos");
        std::fs::create_dir_all(photos.join("sub")).unwrap();
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let out = root.path().join("out.csv");
        assert!(validate(&photos, Some(&data), Some(&out)).is_ok());
        assert!(validate(&photos, None, None).is_ok());
        assert!(validate(&root.path().join("nope"), None, None).is_err());
        assert!(validate(&photos, Some(&photos.join("sub").join("data")), None).unwrap_err().contains("inside"));
        assert!(validate(&photos, Some(&data), Some(&photos.join("out.csv"))).unwrap_err().contains("--bench-out"));
        assert!(validate(&photos, Some(&data), Some(Path::new("out.csv"))).is_ok());
        std::fs::create_dir_all(data.join("recovery")).unwrap();
        assert!(validate(&photos, Some(&data), None).is_ok(), "an empty recovery folder is fine");
        std::fs::write(data.join("recovery").join("x.rgproject"), b"1").unwrap();
        assert!(validate(&photos, Some(&data), None).unwrap_err().contains("recovery"));
        std::fs::remove_file(data.join("recovery").join("x.rgproject")).unwrap();
        std::fs::create_dir_all(data.join("recipes")).unwrap();
        std::fs::write(data.join("recipes").join("r.json"), b"1").unwrap();
        assert!(validate(&photos, Some(&data), None).unwrap_err().contains("recipes"));
    }
}
