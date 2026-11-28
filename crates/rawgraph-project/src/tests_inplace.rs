//! In-place commits: one transaction in the rollback journal, only changed rows,
//! appended history, atomic conflict check, crash safety.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use rawgraph_core::{BrushStroke, CaptureInfo, ImageId, NodeData, NodeKind};

use super::*;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rawgraph-project-inplace-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn synthetic_entries(from: u64, count: u64) -> Vec<ImageEntry> {
    (from..from + count)
        .map(|n| ImageEntry {
            id: ImageId(n),
            uuid: crate::ids::new_uuid(),
            path: PathBuf::from(format!("/photos/shoot/IMG_{n:05}.RAW")),
            content_hash: format!("{n:064x}"),
            file_size: 25_000_000 + n,
            modified_unix_ms: Some(1_700_000_000_000 + n as i64),
            capture: CaptureInfo { iso: Some(100.0 + (n % 7) as f32 * 100.0), make: Some("Maker".into()), model: Some("Model X".into()), ..Default::default() },
            rating: (n % 6) as u8,
            flag: Flag::None,
            color_label: None,
            missing: false,
        })
        .collect()
}

fn project_with_images(count: u64) -> Project {
    let mut p = Project::new();
    assert_eq!(p.merge_found(&synthetic_entries(1, count)), count as usize);
    p
}

fn page_count(path: &Path) -> usize {
    fs::metadata(path).unwrap().len() as usize / 4096
}

/// Number of 4 KiB pages that differ between two versions of a file (pages that exist in one only count).
fn changed_pages(before: &[u8], after: &[u8]) -> usize {
    let pages = before.len().max(after.len()).div_ceil(4096);
    (0..pages)
        .filter(|i| {
            let range = |b: &[u8]| b.get(i * 4096..((i + 1) * 4096).min(b.len())).map(<[u8]>::to_vec);
            range(before) != range(after)
        })
        .count()
}

fn rows(path: &Path, sql: &str) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn integrity_ok(path: &Path) {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let verdict: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(verdict, "ok");
}

#[test]
fn in_place_commits_equal_a_full_rewrite_through_many_kinds_of_edits() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(60);
    p.commit(&file, "first").unwrap();
    let exposure = p.graph.add_node(NodeKind::Exposure, [0.0, 0.0]);

    let check = |p: &Project, what: &str| {
        let loaded = Project::load(&file).unwrap();
        assert_eq!(&loaded, p, "{what}: the file holds the project");
        // The same project written as a whole new file reads back identically.
        let whole = t.path("whole.rawgraph");
        let _ = fs::remove_file(&whole);
        p.save(&whole).unwrap();
        assert_eq!(Project::load(&whole).unwrap(), loaded, "{what}: in place equals full write");
        integrity_ok(&file);
        assert!(!t.path("p.rawgraph-journal").exists(), "{what}: no journal left");
        assert!(!t.path("p.rawgraph.saving").exists());
    };

    // Parameter change.
    p.graph.node_mut(exposure).unwrap().set_param("ev", ImageId(1), 0.5);
    assert_eq!(p.commit(&file, "param").unwrap(), 2);
    check(&p, "param");
    // Rating, flag and label of single images.
    p.image_mut(ImageId(3)).unwrap().rating = 5;
    p.image_mut(ImageId(4)).unwrap().flag = Flag::Reject;
    p.image_mut(ImageId(5)).unwrap().color_label = Some(ColorLabel::Blue);
    assert_eq!(p.commit(&file, "culling").unwrap(), 3);
    check(&p, "culling");
    // Removing images in the middle shifts the positions of all later rows.
    assert_eq!(p.remove_images(&[ImageId(2), ImageId(30), ImageId(31)]), 3);
    assert_eq!(p.commit(&file, "removed").unwrap(), 4);
    check(&p, "removed");
    // New images and a new folder.
    p.merge_found(&synthetic_entries(100, 5));
    p.note_scanned_folder(&t.0, true);
    assert_eq!(p.folders().len(), 1);
    assert_eq!(p.commit(&file, "added").unwrap(), 5);
    check(&p, "added");
    // A different folder list and a changed image path (relink-like edit).
    p.image_mut(ImageId(10)).unwrap().path = PathBuf::from("/elsewhere/IMG_10.RAW");
    p.image_mut(ImageId(10)).unwrap().missing = true;
    assert_eq!(p.commit(&file, "moved").unwrap(), 6);
    check(&p, "moved");
    // A commit without any change still creates a revision.
    assert_eq!(p.commit(&file, "nothing").unwrap(), 7);
    check(&p, "nothing");
    // save() (no revision) updates in place as well.
    p.image_mut(ImageId(6)).unwrap().rating = 1;
    p.save(&file).unwrap();
    assert_eq!(p.revision(), 7);
    check(&p, "save");
}

#[test]
fn only_the_changed_rows_and_the_new_history_entry_are_written() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(3000);
    p.commit(&file, "first").unwrap();
    p.commit(&file, "second").unwrap();
    let total_pages = page_count(&file);
    assert!(total_pages > 200, "the file is big enough to see a difference ({total_pages} pages)");
    let before = fs::read(&file).unwrap();

    p.image_mut(ImageId(1500)).unwrap().rating = 3;
    p.commit(&file, "one rating").unwrap();
    let after = fs::read(&file).unwrap();
    let changed = changed_pages(&before, &after);
    eprintln!("one rating of 3000 images: {changed} of {total_pages} pages differ");
    assert!(changed * 20 < total_pages, "only a few pages change: {changed} of {total_pages}");
    assert_eq!(Project::load(&file).unwrap(), p);
}

#[test]
fn history_grows_by_one_row_and_pruning_deletes_the_oldest_rows() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    for i in 1..=105u64 {
        assert_eq!(p.commit(&file, &format!("c{i}")).unwrap(), i);
    }
    assert_eq!(rows(&file, "SELECT COUNT(*) FROM history"), 100);
    assert_eq!(rows(&file, "SELECT MIN(revision) FROM history"), 6);
    assert_eq!(rows(&file, "SELECT MAX(revision) FROM history"), 105);
    assert_eq!(Project::load(&file).unwrap(), p);
    integrity_ok(&file);
}

#[test]
fn a_snapshot_dropped_for_size_is_cleared_in_the_file() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    p.commit(&file, "a").unwrap();
    p.commit(&file, "b").unwrap();
    assert_eq!(rows(&file, "SELECT COUNT(graph_json) FROM history"), 2);
    p.history.snapshots[0] = None;
    p.history.entries[0].has_graph = false;
    p.save(&file).unwrap();
    assert_eq!(rows(&file, "SELECT COUNT(graph_json) FROM history"), 1);
    let loaded = Project::load(&file).unwrap();
    assert!(!loaded.history()[0].has_graph && loaded.history()[1].has_graph);
    assert_eq!(loaded, p);
}

#[test]
fn the_conflict_check_and_the_write_are_one_atomic_step() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut first = project_with_images(200);
    first.commit(&file, "base").unwrap();
    for round in 0..3 {
        let n = 6;
        let copies: Vec<Project> = (0..n).map(|_| Project::load(&file).unwrap()).collect();
        let base = copies[0].revision();
        let barrier = Arc::new(Barrier::new(n));
        let handles: Vec<_> = copies
            .into_iter()
            .enumerate()
            .map(|(i, mut copy)| {
                let (barrier, file) = (barrier.clone(), file.clone());
                std::thread::spawn(move || {
                    copy.image_mut(ImageId(i as u64 + 1)).unwrap().rating = 5;
                    barrier.wait();
                    copy.commit(&file, &format!("writer {i}")).map_err(|e| matches!(e, ProjectError::Conflict { .. }))
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let wins = results.iter().filter(|r| r.is_ok()).count();
        let conflicts = results.iter().filter(|r| matches!(r, Err(true))).count();
        assert_eq!((wins, conflicts), (1, n - 1), "round {round}: exactly one writer passes, the rest see the conflict: {results:?}");
        assert_eq!(rows(&file, "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'revision'"), base as i64 + 1);
        integrity_ok(&file);
    }
}

#[test]
fn a_conflict_writes_nothing() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut a = project_with_images(10);
    a.commit(&file, "base").unwrap();
    let mut b = Project::load(&file).unwrap();
    a.image_mut(ImageId(1)).unwrap().rating = 5;
    a.commit(&file, "a").unwrap();
    let bytes = fs::read(&file).unwrap();
    b.image_mut(ImageId(2)).unwrap().rating = 4;
    assert!(matches!(b.commit(&file, "b"), Err(ProjectError::Conflict { disk_revision: 2, loaded_revision: 1 })));
    assert_eq!(fs::read(&file).unwrap(), bytes, "the file is untouched");
    assert_eq!(b.revision(), 1);
    // The explicit overwrite goes in place too and never reuses the number.
    assert_eq!(b.commit_overwrite(&file, "b").unwrap(), 3);
    assert_eq!(Project::load(&file).unwrap(), b);
}

#[test]
fn a_hot_journal_of_an_interrupted_commit_is_rolled_back_when_the_file_is_read() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(400);
    p.commit(&file, "base").unwrap();

    // A transaction that is still open (its cache is tiny, so it has spilled
    // changed pages into the file and holds the original pages in the journal).
    let conn = rusqlite::Connection::open(&file).unwrap();
    conn.execute_batch("PRAGMA cache_size = 2; BEGIN IMMEDIATE; UPDATE images SET rating = 5, path = path || '-changed-by-the-interrupted-commit';").unwrap();
    let crashed = t.path("crashed.rawgraph");
    let copied = fs::copy(&file, &crashed).is_ok()
        && fs::copy(t.path("p.rawgraph-journal"), t.path("crashed.rawgraph-journal")).is_ok();
    drop(conn); // rolls the original back
    if !copied {
        eprintln!("the platform does not allow copying an open database; hot-journal case skipped");
        return;
    }
    assert!(t.path("crashed.rawgraph-journal").exists());
    // Reading the copy rolls back the interrupted transaction: the old state is intact.
    let loaded = Project::load(&crashed).unwrap();
    assert_eq!(loaded, p);
    assert!(!t.path("crashed.rawgraph-journal").exists(), "the journal was consumed");
    integrity_ok(&crashed);
}

#[test]
fn another_projects_file_and_old_schemas_are_still_written_as_a_whole() {
    let t = Tmp::new();
    let theirs = t.path("theirs.rawgraph");
    let mut other = project_with_images(5);
    other.commit(&theirs, "theirs").unwrap();
    let mut mine = project_with_images(7);
    assert!(matches!(mine.commit(&theirs, "mine"), Err(ProjectError::Mismatch(_))));
    assert_eq!(mine.commit_overwrite(&theirs, "mine").unwrap(), 1, "another project's revision does not count");
    let loaded = Project::load(&theirs).unwrap();
    assert_eq!(loaded.uuid(), mine.uuid());
    assert_eq!(loaded.images().len(), 7);
    // Save as: a new file is a complete write (no in-place update is attempted).
    let copy = t.path("copy.rawgraph");
    mine.save(&copy).unwrap();
    assert_eq!(Project::load(&copy).unwrap(), mine);
}

#[test]
fn inherit_versioning_keeps_the_commit_state_of_the_original() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut original = project_with_images(10);
    let scan_copy = original.clone();
    original.commit(&file, "saved while the scan ran").unwrap();
    // The scan returns its copy: it carries the old revision until it inherits.
    let mut finished = scan_copy;
    finished.merge_found(&synthetic_entries(11, 5));
    assert_eq!(finished.revision(), 0);
    finished.inherit_versioning(&original);
    assert_eq!((finished.revision(), finished.history().len()), (1, 1));
    // The next save is not a conflict and keeps the history.
    assert_eq!(finished.commit(&file, "after the scan").unwrap(), 2);
    let loaded = Project::load(&file).unwrap();
    assert_eq!((loaded.images().len(), loaded.history().len()), (15, 2));
}

#[test]
fn merge_found_keeps_ids_ascending_and_the_counter_ahead() {
    let mut p = project_with_images(3);
    let mut found = synthetic_entries(2, 6); // ids 2..=7: two known (other uuids), four new
    found[0].rating = 5;
    found.reverse();
    assert_eq!(p.merge_found(&found), 6, "two replaced (ids 2 and 3), four added");
    assert_eq!(p.images().iter().map(|e| e.id.0).collect::<Vec<_>>(), (1..=7).collect::<Vec<_>>());
    assert_eq!(p.image(ImageId(2)).unwrap().rating, 5);
    assert_eq!(p.next_image_id, 8);
    // An unknown id below the newest one is a removed image: it is not brought back.
    p.remove_images(&[ImageId(4)]);
    assert_eq!(p.merge_found(&synthetic_entries(4, 1)), 0);
    assert!(p.image(ImageId(4)).is_none());
}

/// Timing and written volume of one save after a parameter change, in place
/// against a full rewrite (the former behaviour), 4,000 images and 100 history
/// entries of a ~300 KB graph. Run with
/// `cargo test -p rawgraph-project --release bench_save -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench_save_4000_images_100_history_entries() {
    let t = Tmp::new();
    let file = t.path("big.rawgraph");
    let mut p = project_with_images(4000);
    let exposure = p.graph.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let draw = p.graph.add_node(NodeKind::Draw, [100.0, 0.0]);
    let strokes: Vec<BrushStroke> = (0..65)
        .map(|s| BrushStroke {
            points: (0..50).map(|k| [((s * 7 + k) % 100) as f32 / 100.0 + 0.00123, k as f32 / 50.0 + 0.00456]).collect(),
            radius: 0.01,
            hardness: 0.5,
            flow: 1.0,
            erase: false,
        })
        .collect();
    assert!(p.graph.node_mut(draw).unwrap().set_data(ImageId(1), Some(NodeData::Strokes(strokes))));
    let graph_bytes = p.graph.to_json().len();
    let started = std::time::Instant::now();
    for i in 0..100 {
        p.graph.node_mut(exposure).unwrap().set_param("ev", ImageId(1), i as f32 * 0.01);
        p.commit(&file, "edit").unwrap();
    }
    eprintln!("graph {} KB; building the file with 100 in-place commits took {:?}", graph_bytes / 1024, started.elapsed());
    assert_eq!(p.history().len(), 100);
    let size = fs::metadata(&file).unwrap().len();

    // After: one more parameter change, saved in place.
    let before = fs::read(&file).unwrap();
    p.graph.node_mut(exposure).unwrap().set_param("ev", ImageId(1), 0.777);
    let started = std::time::Instant::now();
    p.commit(&file, "one more edit").unwrap();
    let in_place = started.elapsed();
    let after = fs::read(&file).unwrap();
    let pages = changed_pages(&before, &after);
    let grown_pages = (after.len().saturating_sub(before.len())) / 4096;

    // Before: the former commit wrote a complete new file with all 100 snapshots.
    let revision = p.revision() + 1;
    let log = p.log_with_commit(revision, "one more edit");
    let full = t.path("full.rawgraph");
    let started = std::time::Instant::now();
    store::write_file(&p, &store::Versioning { revision, log: &log }, &full, &[], true).unwrap();
    let rewrite = started.elapsed();
    let full_size = fs::metadata(&full).unwrap().len();

    eprintln!("file {} MB, {} history entries", size / (1 << 20), p.history().len());
    eprintln!("BEFORE (full rewrite): {rewrite:?}, {} KB written", full_size / 1024);
    eprintln!(
        "AFTER  (in place):     {in_place:?}, {} KB of pages changed in the file ({} KB of it new pages), plus ~{} KB rollback journal",
        pages * 4,
        grown_pages * 4,
        pages.saturating_sub(grown_pages) * 4
    );
    assert!(in_place < rewrite);
}
