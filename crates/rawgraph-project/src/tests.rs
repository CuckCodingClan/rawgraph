use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rawgraph_core::{CaptureInfo, ImageId, Mode, NodeKind};

use super::*;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Temporary directory removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rawgraph-project-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.path(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn supported(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("raw"))
}

fn no_capture(_: &Path) -> Option<CaptureInfo> {
    None
}

/// Reads "iso=<n>" from the first line of the fake file.
fn fake_capture(p: &Path) -> Option<CaptureInfo> {
    let text = fs::read_to_string(p).ok()?;
    let iso: f32 = text.lines().next()?.strip_prefix("iso=")?.parse().ok()?;
    Some(CaptureInfo { iso: Some(iso), ..Default::default() })
}

fn scan(project: &mut Project, dir: &Path, recursive: bool) -> ScanReport {
    project.scan_folder(dir, recursive, &supported, &fake_capture)
}

fn names(project: &Project, ids: &[ImageId]) -> Vec<String> {
    ids.iter().map(|id| project.image(*id).unwrap().file_name()).collect()
}

#[test]
fn new_project_has_connected_source_and_output() {
    let p = Project::new();
    let kinds: Vec<NodeKind> = p.graph.nodes().map(|n| n.kind).collect();
    assert!(kinds.contains(&NodeKind::Source) && kinds.contains(&NodeKind::Output));
    assert_eq!(p.graph.nodes().count(), 2);
    assert_eq!(p.graph.edges().len(), 1);
    assert!(p.graph.validate().is_empty());
    assert!(p.images().is_empty());
}

#[test]
fn scan_adds_supported_files_in_natural_order_and_is_idempotent() {
    let t = Tmp::new();
    t.write("img10.raw", "ten");
    t.write("IMG2.RAW", "two");
    t.write("img1.raw", "one");
    t.write("notes.txt", "ignored");
    let mut p = Project::new();
    let r = scan(&mut p, &t.0, false);
    assert_eq!(r.added, vec![ImageId(1), ImageId(2), ImageId(3)]);
    assert_eq!(names(&p, &r.added), ["img1.raw", "IMG2.RAW", "img10.raw"]);
    assert!(r.failed.is_empty() && r.relinked.is_empty() && r.unchanged == 0);
    assert!(p.images().iter().all(|e| e.path.is_absolute()));

    let again = scan(&mut p, &t.0, false);
    assert!(again.added.is_empty() && again.relinked.is_empty());
    assert_eq!(again.unchanged, 3);
    assert_eq!(p.images().len(), 3);
}

#[test]
fn renamed_file_is_relinked_with_its_settings() {
    let t = Tmp::new();
    t.write("a.raw", "content a");
    t.write("b.raw", "content b");
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    let id = p.images()[0].id;
    {
        let e = p.image_mut(id).unwrap();
        e.rating = 4;
        e.flag = Flag::Pick;
        e.color_label = Some(ColorLabel::Green);
    }
    fs::rename(t.path("a.raw"), t.path("renamed.raw")).unwrap();
    let r = scan(&mut p, &t.0, false);
    assert_eq!(r.relinked, vec![id]);
    assert!(r.added.is_empty());
    assert_eq!(p.images().len(), 2);
    let e = p.image(id).unwrap();
    assert_eq!(e.file_name(), "renamed.raw");
    assert_eq!((e.rating, e.flag, e.color_label), (4, Flag::Pick, Some(ColorLabel::Green)));
    assert!(!e.missing);
}

#[test]
fn copy_of_a_present_file_is_a_new_image() {
    let t = Tmp::new();
    let a = t.write("a.raw", "same bytes");
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    fs::copy(&a, t.path("copy.raw")).unwrap();
    let r = scan(&mut p, &t.0, false);
    assert_eq!(r.added, vec![ImageId(2)]);
    assert!(r.relinked.is_empty());
    assert_eq!(p.images().len(), 2);
}

#[test]
fn deleted_file_is_missing_and_hidden_by_filter() {
    let t = Tmp::new();
    t.write("a.raw", "a");
    t.write("b.raw", "bb");
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    assert_eq!(p.refresh_missing(), 0);
    fs::remove_file(t.path("a.raw")).unwrap();
    assert_eq!(p.refresh_missing(), 1);
    assert!(p.images()[0].missing && !p.images()[1].missing);
    assert_eq!(p.filter(&Filter::default()), vec![ImageId(2)]);
    let all = Filter { include_missing: true, ..Default::default() };
    assert_eq!(p.filter(&all), vec![ImageId(1), ImageId(2)]);
}

#[test]
fn recursive_and_non_recursive_scans() {
    let t = Tmp::new();
    t.write("top.raw", "t");
    t.write("sub/inner.raw", "i");
    t.write("sub/deeper/deep.raw", "d");
    let mut flat = Project::new();
    assert_eq!(scan(&mut flat, &t.0, false).added.len(), 1);
    let mut deep = Project::new();
    let r = scan(&mut deep, &t.0, true);
    assert_eq!(r.added.len(), 3);
    assert_eq!(names(&deep, &r.added), ["deep.raw", "inner.raw", "top.raw"]);
}

#[test]
fn missing_metadata_still_adds_and_bad_folder_is_one_failure() {
    let t = Tmp::new();
    t.write("a.raw", "a");
    let mut p = Project::new();
    let r = p.scan_folder(&t.0, false, &supported, &no_capture);
    assert_eq!(r.added.len(), 1);
    assert_eq!(p.images()[0].capture, CaptureInfo::default());

    let r = p.scan_folder(&t.path("does-not-exist"), true, &supported, &no_capture);
    assert_eq!(r.failed.len(), 1);
    assert!(r.added.is_empty());
    assert_eq!(p.images().len(), 1);
}

#[test]
fn filters_by_iso_rating_flag_flash_and_text() {
    let mut p = Project::new();
    let t = Tmp::new();
    t.write("a_low.raw", "iso=100");
    t.write("b_mid.raw", "iso=800");
    t.write("c_high.raw", "iso=3200");
    t.write("d_none.raw", "no metadata");
    scan(&mut p, &t.0, false);
    assert_eq!(p.images().len(), 4);
    let (a, b, c, d) = (ImageId(1), ImageId(2), ImageId(3), ImageId(4));
    p.image_mut(b).unwrap().rating = 3;
    p.image_mut(c).unwrap().rating = 5;
    p.image_mut(c).unwrap().flag = Flag::Pick;
    p.image_mut(a).unwrap().capture.flash_fired = Some(true);
    p.image_mut(b).unwrap().capture.flash_fired = Some(false);

    let f = |f: Filter| p.filter(&f);
    assert_eq!(f(Filter::default()), vec![a, b, c, d]);
    assert_eq!(f(Filter { iso_min: Some(200.0), iso_max: Some(3200.0), ..Default::default() }), vec![b, c]);
    assert_eq!(f(Filter { iso_max: Some(800.0), ..Default::default() }), vec![a, b]);
    assert_eq!(f(Filter { iso_min: Some(0.0), ..Default::default() }), vec![a, b, c]);
    assert_eq!(f(Filter { min_rating: Some(3), ..Default::default() }), vec![b, c]);
    assert_eq!(f(Filter { flag: Some(Flag::Pick), ..Default::default() }), vec![c]);
    assert_eq!(f(Filter { flash_fired: Some(true), ..Default::default() }), vec![a]);
    assert_eq!(f(Filter { flash_fired: Some(false), ..Default::default() }), vec![b]);
    assert_eq!(f(Filter { text: Some("MID".into()), ..Default::default() }), vec![b]);
    assert_eq!(
        f(Filter { iso_min: Some(100.0), min_rating: Some(4), text: Some(".RAW".into()), ..Default::default() }),
        vec![c]
    );
}

#[test]
fn sorting_is_natural_and_puts_unknown_values_last() {
    let mut p = Project::new();
    let t = Tmp::new();
    t.write("img10.raw", "iso=400");
    t.write("img2.raw", "iso=100");
    t.write("Img1.raw", "no iso");
    t.write("img3.raw", "iso=100");
    scan(&mut p, &t.0, false);
    let ids: Vec<ImageId> = p.images().iter().map(|e| e.id).collect();

    let asc = p.sorted(&ids, SortKey::FileName, false);
    assert_eq!(names(&p, &asc), ["Img1.raw", "img2.raw", "img3.raw", "img10.raw"]);
    let desc = p.sorted(&ids, SortKey::FileName, true);
    assert_eq!(names(&p, &desc), ["img10.raw", "img3.raw", "img2.raw", "Img1.raw"]);

    let iso_asc = p.sorted(&ids, SortKey::Iso, false);
    assert_eq!(names(&p, &iso_asc), ["img2.raw", "img3.raw", "img10.raw", "Img1.raw"]);
    let iso_desc = p.sorted(&ids, SortKey::Iso, true);
    assert_eq!(names(&p, &iso_desc), ["img10.raw", "img2.raw", "img3.raw", "Img1.raw"]);

    // Ev100: give three images full exposure data, leave one without.
    for (name, shutter) in [("img10.raw", 1.0 / 60.0), ("img2.raw", 1.0 / 250.0), ("img3.raw", 1.0 / 60.0)] {
        let id = p.images().iter().find(|e| e.file_name() == name).unwrap().id;
        let cap = &mut p.image_mut(id).unwrap().capture;
        cap.shutter_s = Some(shutter);
        cap.aperture = Some(4.0);
    }
    let ev_asc = p.sorted(&ids, SortKey::Ev100, false);
    // img10 (ISO 400) is darkest, img2/img3 share ISO 100 but differ in shutter.
    assert_eq!(names(&p, &ev_asc), ["img10.raw", "img3.raw", "img2.raw", "Img1.raw"]);
    let ev_desc = p.sorted(&ids, SortKey::Ev100, true);
    assert_eq!(names(&p, &ev_desc), ["img2.raw", "img3.raw", "img10.raw", "Img1.raw"]);

    p.image_mut(ImageId(1)).unwrap().rating = 2;
    let by_rating = p.sorted(&ids, SortKey::Rating, true);
    assert_eq!(by_rating[0], ImageId(1));
    // Unknown ids are ignored.
    assert_eq!(p.sorted(&[ImageId(999), ImageId(1)], SortKey::FileName, false), vec![ImageId(1)]);
}

#[test]
fn save_load_roundtrip_keeps_everything_and_ids_keep_counting() {
    let t = Tmp::new();
    let photos = t.path("photos");
    fs::create_dir_all(&photos).unwrap();
    for (n, iso) in [("a.raw", 100), ("b.raw", 200), ("c.raw", 400)] {
        fs::write(photos.join(n), format!("iso={iso}\npayload {n}")).unwrap();
    }
    let mut p = Project::new();
    scan(&mut p, &photos, false);
    p.image_mut(ImageId(1)).unwrap().rating = 5;
    p.image_mut(ImageId(1)).unwrap().flag = Flag::Reject;
    p.image_mut(ImageId(2)).unwrap().color_label = Some(ColorLabel::Purple);
    p.image_mut(ImageId(2)).unwrap().capture = CaptureInfo {
        make: Some("Maker".into()),
        model: Some("Model \u{00e9}".into()),
        lens: None,
        iso: Some(200.0),
        shutter_s: Some(1.0 / 125.0),
        aperture: Some(2.8),
        flash_fired: Some(false),
        ..Default::default()
    };
    p.image_mut(ImageId(3)).unwrap().missing = true;

    let exp = p.graph.add_node(NodeKind::Exposure, [100.0, 50.0]);
    let node = p.graph.node_mut(exp).unwrap();
    node.mode = Mode::Individual;
    node.set_param("ev", ImageId(1), 1.5);
    node.set_param("ev", ImageId(2), -0.75);

    let file = t.path(&format!("test.{PROJECT_EXTENSION}"));
    p.save(&file).unwrap();
    let back = Project::load(&file).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.graph.node(exp).unwrap().param("ev", ImageId(2)), -0.75);

    // A new file gets a fresh id, even after a reload.
    let mut back = back;
    fs::write(photos.join("d.raw"), "iso=50\nnew").unwrap();
    let r = scan(&mut back, &photos, false);
    assert_eq!(r.added, vec![ImageId(4)]);

    back.save(&file).unwrap();
    let again = Project::load(&file).unwrap();
    assert_eq!(again, back);
}

#[test]
fn loading_bad_files_fails_cleanly_and_saving_twice_works() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let p = Project::new();
    p.save(&file).unwrap();
    p.save(&file).unwrap();
    assert_eq!(Project::load(&file).unwrap(), p);
    assert!(!t.path("p.rawgraph.saving").exists(), "temporary file must be cleaned up");

    // Newer schema version.
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute("UPDATE meta SET value = '999' WHERE key = 'schema_version'", []).unwrap();
    }
    assert!(matches!(Project::load(&file), Err(ProjectError::TooNew { file_version: 999, supported: SCHEMA_VERSION })));

    // A text file, an empty file and a missing file.
    let text = t.write("notes.rawgraph", "this is not a database, just some text\n".repeat(20).as_str());
    assert!(matches!(Project::load(&text), Err(ProjectError::Corrupt(_) | ProjectError::Database(_))));
    let empty = t.write("empty.rawgraph", "");
    assert!(Project::load(&empty).is_err());
    assert!(matches!(Project::load(&t.path("nope.rawgraph")), Err(ProjectError::Io(_))));

    // A valid SQLite file that is not a RawGraph project.
    let other = t.path("other.rawgraph");
    {
        let conn = rusqlite::Connection::open(&other).unwrap();
        conn.execute("CREATE TABLE something (x)", []).unwrap();
    }
    assert!(Project::load(&other).is_err());
}

#[test]
fn failed_save_keeps_the_existing_file() {
    let t = Tmp::new();
    let file = t.path("keep.rawgraph");
    let p = Project::new();
    p.save(&file).unwrap();
    let before = fs::read(&file).unwrap();
    // Force a failure by making the target a directory.
    let dir_target = t.path("dir.rawgraph");
    fs::create_dir_all(&dir_target).unwrap();
    assert!(p.save(&dir_target).is_err());
    assert!(dir_target.is_dir());
    assert!(!t.path("dir.rawgraph.saving").exists());
    assert_eq!(fs::read(&file).unwrap(), before, "a failed save elsewhere must not touch other files");
}

#[test]
fn hash_covers_head_tail_and_size() {
    let t = Tmp::new();
    let mib = 1024 * 1024;
    let mut data = vec![0u8; 3 * mib];
    let a = t.path("a.bin");
    fs::write(&a, &data).unwrap();
    let (h0, s0) = content_hash(&a).unwrap();
    assert_eq!(s0, (3 * mib) as u64);
    assert_eq!(h0.len(), 64);

    // Change inside the first 64 KiB.
    data[10] = 1;
    fs::write(&a, &data).unwrap();
    let (h1, _) = content_hash(&a).unwrap();
    assert_ne!(h0, h1);

    // Change inside the last 64 KiB.
    data[10] = 0;
    data[3 * mib - 5] = 9;
    fs::write(&a, &data).unwrap();
    let (h2, _) = content_hash(&a).unwrap();
    assert_ne!(h0, h2);

    // Only the size differs (zeros in the head and tail ranges are identical).
    let b = t.path("b.bin");
    fs::write(&b, vec![0u8; 3 * mib + 1]).unwrap();
    let (h3, s3) = content_hash(&b).unwrap();
    assert_eq!(s3, (3 * mib + 1) as u64);
    assert_ne!(h0, h3);

    // Same content, same hash; tiny and empty files work.
    let c = t.path("c.bin");
    fs::write(&c, vec![0u8; 3 * mib]).unwrap();
    assert_eq!(content_hash(&c).unwrap().0, h0);
    let tiny = t.write("tiny.bin", "x");
    assert!(content_hash(&tiny).is_ok());
    let empty = t.write("empty.bin", "");
    assert_eq!(content_hash(&empty).unwrap().1, 0);
}

#[test]
fn natural_order_helpers() {
    use std::cmp::Ordering::*;
    assert_eq!(natural_cmp("img2", "img10"), Less);
    assert_eq!(natural_cmp("IMG2", "img10"), Less);
    assert_eq!(natural_cmp("a", "A"), Greater, "ties fall back to the plain order");
    assert_eq!(natural_cmp("img007", "img7"), Less);
    assert_eq!(natural_cmp("x", "x"), Equal);
}

fn set_mtime(p: &Path, offset_s: u64) {
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(offset_s)).unwrap();
}

/// Number of hash calls so far for files below `t`.
fn hash_calls(t: &Tmp) -> usize {
    crate::util::HASHED.lock().unwrap().iter().filter(|p| p.starts_with(&t.0)).count()
}

#[test]
fn rescan_of_unchanged_files_does_not_read_them() {
    let t = Tmp::new();
    for n in ["a.raw", "b.raw", "c.raw"] {
        t.write(n, &format!("iso=100\n{n}"));
    }
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    assert_eq!(hash_calls(&t), 3);
    assert!(p.images().iter().all(|e| e.modified_unix_ms.is_some()));
    let r = scan(&mut p, &t.0, false);
    assert_eq!(r.unchanged, 3);
    assert_eq!(hash_calls(&t), 3, "unchanged files must not be opened");
}

#[test]
fn replaced_file_is_updated_and_keeps_its_settings() {
    let t = Tmp::new();
    let a = t.write("a.raw", "iso=100\nfirst version");
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    let id = ImageId(1);
    p.image_mut(id).unwrap().rating = 3;
    p.image_mut(id).unwrap().flag = Flag::Pick;
    let old_hash = p.image(id).unwrap().content_hash.clone();
    assert_eq!(p.image(id).unwrap().capture.iso, Some(100.0));

    fs::write(&a, "iso=640\nsecond version").unwrap();
    set_mtime(&a, 20);
    let r = scan(&mut p, &t.0, false);
    assert_eq!(r.updated, vec![id]);
    assert!(r.added.is_empty() && r.relinked.is_empty());
    assert_eq!(r.unchanged, 0);
    let e = p.image(id).unwrap();
    assert_ne!(e.content_hash, old_hash);
    assert_eq!(e.capture.iso, Some(640.0));
    assert_eq!((e.rating, e.flag), (3, Flag::Pick));
    assert_eq!(p.images().len(), 1);
}

#[test]
fn touching_mtime_without_content_change_is_unchanged() {
    let t = Tmp::new();
    let a = t.write("a.raw", "iso=100\nsame");
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    let before = p.image(ImageId(1)).unwrap().modified_unix_ms;
    set_mtime(&a, 30);
    let r = scan(&mut p, &t.0, false);
    assert_eq!(hash_calls(&t), 2, "a changed mtime triggers one re-hash");
    assert!(r.updated.is_empty());
    assert_eq!(r.unchanged, 1);
    assert_ne!(p.image(ImageId(1)).unwrap().modified_unix_ms, before);
    scan(&mut p, &t.0, false);
    assert_eq!(hash_calls(&t), 2);
}

#[test]
fn progress_is_reported_once_per_file_and_can_cancel() {
    let t = Tmp::new();
    for n in ["a.raw", "b.raw", "c.raw", "d.raw"] {
        t.write(n, n);
    }
    let mut p = Project::new();
    let mut seen: Vec<(usize, usize, String)> = Vec::new();
    let r = p.scan_folder_with(&t.0, false, &supported, &no_capture, &mut |sp| {
        seen.push((sp.done, sp.total, sp.current.file_name().unwrap().to_string_lossy().into_owned()));
        true
    });
    assert!(!r.cancelled);
    assert_eq!(
        seen,
        vec![
            (0, 4, "a.raw".to_string()),
            (1, 4, "a.raw".to_string()),
            (2, 4, "b.raw".to_string()),
            (3, 4, "c.raw".to_string()),
            (4, 4, "d.raw".to_string())
        ]
    );

    let mut q = Project::new();
    let r = q.scan_folder_with(&t.0, false, &supported, &no_capture, &mut |sp| sp.done < 2);
    assert!(r.cancelled);
    assert_eq!(r.added.len(), 2);
    assert_eq!(q.images().len(), 2);
    let rest = scan(&mut q, &t.0, false);
    assert_eq!((rest.unchanged, rest.added.len()), (2, 2));
    assert!(!rest.cancelled);
}

#[test]
fn removed_images_never_free_their_ids_and_mtime_roundtrips() {
    let t = Tmp::new();
    t.write("a.raw", "a");
    t.write("b.raw", "bb");
    let mut p = Project::new();
    scan(&mut p, &t.0, false);
    let mtimes: Vec<Option<i64>> = p.images().iter().map(|e| e.modified_unix_ms).collect();
    assert!(mtimes.iter().all(Option::is_some));
    assert_eq!(p.remove_images(&[ImageId(2), ImageId(99)]), 1);
    assert!(t.path("b.raw").exists(), "files are never touched");
    let file = t.path("p.rawgraph");
    p.save(&file).unwrap();
    let mut back = Project::load(&file).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.images()[0].modified_unix_ms, mtimes[0]);
    let r = scan(&mut back, &t.0, false);
    assert_eq!(r.added, vec![ImageId(3)], "id 2 must not be reused");
    assert_eq!(r.unchanged, 1);
}

#[test]
fn thread_count_does_not_change_the_result() {
    let t = Tmp::new();
    for i in 0..40 {
        t.write(&format!("d{}/img{i}.raw", i % 3), &format!("iso={}\nbody {i}", 100 + i));
    }
    t.write("d1/dup.raw", "iso=100\nbody 0");
    let mut one = Project::new();
    one.set_scan_threads(1);
    let mut many = Project::new();
    many.set_scan_threads(8);
    let r1 = scan(&mut one, &t.0, true);
    let r8 = scan(&mut many, &t.0, true);
    assert_eq!(r1, r8);
    // Identifiers are random; everything else must be identical.
    many.uuid = one.uuid.clone();
    for (m, o) in many.images.iter_mut().zip(one.images()) {
        m.uuid = o.uuid.clone();
    }
    assert_eq!(one, many);
    assert_eq!(r1.added.len(), 41);
    let ids: Vec<u64> = one.images().iter().map(|e| e.id.0).collect();
    assert_eq!(ids, (1..=41).collect::<Vec<_>>());
    let paths: Vec<_> = one.images().iter().map(|e| e.path.clone()).collect();
    let mut sorted = paths.clone();
    sorted.sort_by(|a, b| crate::util::path_cmp(a, b));
    assert_eq!(paths, sorted);
    many.set_scan_threads(500);
    assert_eq!(many.scan_threads(), 32);
    many.set_scan_threads(0);
    assert_eq!(many.scan_threads(), 1);
}

#[test]
fn cancelling_a_parallel_scan_keeps_a_complete_prefix() {
    let t = Tmp::new();
    for i in 0..30 {
        t.write(&format!("f{i:02}.raw"), &format!("iso={}\ncontent {i}", 100 + i));
    }
    let mut p = Project::new();
    p.set_scan_threads(8);
    let r = p.scan_folder_with(&t.0, false, &supported, &fake_capture, &mut |sp| sp.done < 12);
    assert!(r.cancelled);
    assert_eq!(r.added.len(), 12);
    let ids: Vec<u64> = p.images().iter().map(|e| e.id.0).collect();
    assert_eq!(ids, (1..=12).collect::<Vec<_>>(), "kept images are the contiguous path-order prefix");
    for (i, e) in p.images().iter().enumerate() {
        assert_eq!(e.file_name(), format!("f{i:02}.raw"));
        assert_eq!(e.content_hash.len(), 64);
        assert_eq!(e.capture.iso, Some(100.0 + i as f32));
        assert!(e.modified_unix_ms.is_some());
    }
    let rest = scan(&mut p, &t.0, false);
    assert_eq!((rest.unchanged, rest.added.len()), (12, 18));
    assert_eq!(rest.added.first(), Some(&ImageId(13)));
    assert!(!rest.cancelled);
    assert_eq!(p.images().len(), 30);
}
#[test]
fn progress_carries_the_committed_entries() {
    let t = Tmp::new();
    for i in 0..10 {
        t.write(&format!("e{i:02}.raw"), &format!("iso={}\nbody {i}", 100 + i));
    }
    let mut p = Project::new();
    let mut got: Vec<ImageEntry> = Vec::new();
    let mut first_is_none = false;
    p.scan_folder_with(&t.0, false, &supported, &fake_capture, &mut |sp| {
        if sp.done == 0 {
            first_is_none = sp.entry.is_none();
        } else {
            got.extend(sp.entry.clone());
        }
        true
    });
    assert!(first_is_none);
    assert_eq!(got.as_slice(), p.images());

    // Rescan: every call carries the unchanged entry.
    let mut again: Vec<ImageEntry> = Vec::new();
    p.scan_folder_with(&t.0, false, &supported, &fake_capture, &mut |sp| {
        again.extend(sp.entry.clone());
        true
    });
    assert_eq!(again.as_slice(), p.images());

    // Cancel: everything received is present afterwards.
    let mut q = Project::new();
    let mut seen: Vec<ImageEntry> = Vec::new();
    let r = q.scan_folder_with(&t.0, false, &supported, &fake_capture, &mut |sp| {
        seen.extend(sp.entry.clone());
        sp.done < 4
    });
    assert!(r.cancelled);
    assert_eq!(seen.len(), 4);
    for e in &seen {
        assert_eq!(q.image(e.id), Some(e));
    }
}

#[test]
fn failed_file_yields_no_entry() {
    let t = Tmp::new();
    t.write("a.raw", "a");
    t.write("gone.raw", "g");
    t.write("z.raw", "z");
    // The file disappears between listing and hashing.
    let racy = |p: &Path| {
        if p.file_name().is_some_and(|n| n == "gone.raw") {
            let _ = fs::remove_file(p);
        }
        supported(p)
    };
    let mut p = Project::new();
    let mut entries: Vec<Option<String>> = Vec::new();
    let r = p.scan_folder_with(&t.0, false, &racy, &no_capture, &mut |sp| {
        if sp.done > 0 {
            entries.push(sp.entry.as_ref().map(ImageEntry::file_name));
        }
        true
    });
    assert_eq!(r.failed.len(), 1);
    assert_eq!(entries, vec![Some("a.raw".to_string()), None, Some("z.raw".to_string())]);
}
