//! In-place commits: one transaction in the rollback journal, only changed rows,
//! appended history, atomic conflict check, crash safety.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use rawgraph_core::{BrushStroke, CaptureInfo, ImageId, NodeData, NodeKind};

use super::*;
use crate::{store, writemode};

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
            keywords: Vec::new(),
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
    store::write_file(&p, &store::Versioning { revision, log: &log }, &full, &[], true, None).unwrap();
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

/// A copy of `file` taken while a transaction on it is open and has spilled changed pages
/// into the file: the state an interrupted save leaves behind (changed file plus hot journal).
fn crashed_copy(t: &Tmp, file: &Path, name: &str) -> PathBuf {
    let conn = rusqlite::Connection::open(file).unwrap();
    conn.execute_batch("PRAGMA cache_size = 2; BEGIN IMMEDIATE; UPDATE images SET rating = 5, path = path || '-changed-by-the-interrupted-commit';").unwrap();
    let crashed = t.path(&format!("{name}.rawgraph"));
    let journal = t.path(&format!("{name}.rawgraph-journal"));
    let copied = fs::copy(file, &crashed).is_ok() && fs::copy(t.path("p.rawgraph-journal"), &journal).is_ok();
    drop(conn); // rolls the original back
    // No silent pass: if the platform cannot copy an open database, this test fails and says so.
    assert!(copied && journal.exists(), "cannot copy an open database and its journal on this platform; this test needs that");
    crashed
}

#[test]
fn a_leftover_journal_is_never_replayed_by_a_read_and_recovery_is_explicit() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(400);
    p.commit(&file, "base").unwrap();
    let crashed = crashed_copy(&t, &file, "crashed");
    let changed_bytes = fs::read(&crashed).unwrap();
    let journal_bytes = fs::read(t.path("crashed.rawgraph-journal")).unwrap();

    // Loading refuses with a clear message and touches nothing.
    let err = Project::load(&crashed).unwrap_err().to_string();
    assert!(err.contains("unfinished save") && err.contains("crashed.rawgraph-journal"), "{err}");
    assert_eq!(fs::read(&crashed).unwrap(), changed_bytes);
    assert_eq!(fs::read(t.path("crashed.rawgraph-journal")).unwrap(), journal_bytes);

    // The same on a file that is read-only (a share, a locked-down folder).
    let mut perms = fs::metadata(&crashed).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&crashed, perms).unwrap();
    let err = Project::load(&crashed).unwrap_err().to_string();
    assert!(err.contains("unfinished save"), "{err}");
    let mut perms = fs::metadata(&crashed).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    fs::set_permissions(&crashed, perms).unwrap();

    // Explicit recovery restores the last complete state.
    assert!(Project::recover_unfinished_save(&crashed).unwrap());
    assert!(!t.path("crashed.rawgraph-journal").exists());
    assert_eq!(Project::load(&crashed).unwrap(), p);
    assert!(!Project::recover_unfinished_save(&crashed).unwrap(), "nothing left to recover");
    integrity_ok(&crashed);
}

#[test]
fn a_full_write_over_a_file_with_a_hot_journal_never_replays_the_journal_but_moves_it_aside() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(100);
    p.commit(&file, "base").unwrap();
    let crashed = crashed_copy(&t, &file, "crashed");
    // Another project replaces the file as a whole (commit_overwrite: full write).
    let mut other = project_with_images(7);
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    other.commit_overwrite(&crashed, "replace").unwrap();
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
    assert!(!t.path("crashed.rawgraph-journal").exists(), "no journal may stay next to the new file");
    assert_eq!(stale_journals(&t).len(), 1, "the old journal is kept aside, not replayed and not deleted");
    assert_eq!(Project::load(&crashed).unwrap(), other);
    integrity_ok(&crashed);
}

#[test]
fn reading_while_another_writer_commits_always_sees_one_consistent_state() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(300);
    p.commit(&file, "base").unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (file, stop) = (file.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut commits = 0;
            while !stop.load(Ordering::SeqCst) && commits < 150 {
                p.merge_found(&synthetic_entries(1000 + commits, 1));
                p.image_mut(ImageId(1 + commits % 300)).unwrap().rating = (commits % 6) as u8;
                match p.commit(&file, "writer") {
                    Ok(_) => commits += 1,
                    Err(ProjectError::Io(m)) if m.contains("in use") => {}
                    Err(e) => panic!("{e}"),
                }
            }
            commits
        })
    };
    let mut last = 0;
    let mut loads = 0;
    while loads < 60 {
        match Project::load(&file) {
            Ok(q) => {
                assert!(q.revision() >= last, "revisions never go back");
                assert_eq!(q.history().last().map(|e| e.revision), Some(q.revision()), "history and revision belong together");
                last = q.revision();
                loads += 1;
            }
            Err(ProjectError::Io(m)) if m.contains("in use") => {}
            Err(e) => panic!("a healthy file must load: {e}"),
        }
    }
    stop.store(true, Ordering::SeqCst);
    assert!(writer.join().unwrap() > 0);
}

#[test]
fn the_whole_file_rewrite_mode_gives_the_same_files_and_never_touches_the_in_place_path() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(50);
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    p.commit(&file, "one").unwrap();
    // The in-place hook would fail the commit; it is not reached in this mode.
    store::FAIL_BEFORE_COMMIT.with(|f| f.set(true));
    p.image_mut(ImageId(3)).unwrap().rating = 5;
    assert_eq!(p.commit(&file, "two").unwrap(), 2);
    assert!(store::FAIL_BEFORE_COMMIT.with(|f| f.replace(false)), "the in-place path was not used");
    p.save(&file).unwrap();
    assert_eq!(Project::load(&file).unwrap(), p);
    assert!(!t.path("p.rawgraph-journal").exists() && !t.path("p.rawgraph.saving").exists());
    // A stale copy still gets the conflict.
    let mut stale = Project::load(&file).unwrap();
    p.commit(&file, "three").unwrap();
    assert!(matches!(stale.commit(&file, "x"), Err(ProjectError::Conflict { .. })));
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
}

#[test]
fn a_constraint_fallback_to_the_full_write_keeps_the_conflict_check() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut a = project_with_images(10);
    a.commit(&file, "base").unwrap();
    let mut b = Project::load(&file).unwrap();
    // Swapping two image uuids cannot be done row by row (the uuids are unique): full write.
    let (u1, u2) = (a.image(ImageId(1)).unwrap().uuid.clone(), a.image(ImageId(2)).unwrap().uuid.clone());
    a.image_mut(ImageId(1)).unwrap().uuid = u2.clone();
    a.image_mut(ImageId(2)).unwrap().uuid = u1.clone();
    assert_eq!(a.commit(&file, "swap").unwrap(), 2);
    let loaded = Project::load(&file).unwrap();
    assert_eq!((loaded.image(ImageId(1)).unwrap().uuid.as_str(), loaded.image(ImageId(2)).unwrap().uuid.as_str()), (u2.as_str(), u1.as_str()));
    assert_eq!(loaded, a);
    // The same swap from a copy that is behind: conflict, nothing written.
    let bytes = fs::read(&file).unwrap();
    b.image_mut(ImageId(1)).unwrap().uuid = u2;
    b.image_mut(ImageId(2)).unwrap().uuid = u1;
    assert!(matches!(b.commit(&file, "swap too"), Err(ProjectError::Conflict { .. })));
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(b.revision(), 1);
}

#[test]
fn a_locked_file_gives_a_clear_error_and_nothing_changes() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(20);
    p.commit(&file, "base").unwrap();
    let bytes = fs::read(&file).unwrap();
    let blocker = rusqlite::Connection::open(&file).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    p.image_mut(ImageId(1)).unwrap().rating = 5;
    let before = p.clone();
    let started = std::time::Instant::now();
    let err = p.commit(&file, "blocked").unwrap_err();
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "waits at most a few seconds");
    assert!(matches!(&err, ProjectError::Io(m) if m == "The project file is in use by another program or computer; try again."), "{err}");
    assert_eq!(p, before, "the project is unchanged");
    assert!(!t.path("p.rawgraph.saving").exists(), "no rewrite was attempted");
    blocker.execute_batch("ROLLBACK").unwrap();
    drop(blocker);
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert_eq!(p.commit(&file, "free again").unwrap(), 2);
}

#[test]
fn an_in_place_update_leaves_the_same_meta_rows_as_a_full_write() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(5);
    p.commit(&file, "base").unwrap();
    rusqlite::Connection::open(&file).unwrap().execute("INSERT INTO meta (key, value) VALUES ('recovery_marker', 'x')", []).unwrap();
    p.commit(&file, "next").unwrap();
    assert_eq!(rows(&file, "SELECT COUNT(*) FROM meta WHERE key LIKE 'recovery%'"), 0);
    assert_eq!(rows(&file, "SELECT COUNT(*) FROM meta"), 5);
}

#[test]
fn a_half_finished_entry_never_replaces_a_complete_one() {
    let mut p = project_with_images(3);
    let good = p.image(ImageId(2)).unwrap().clone();
    // A rescan saw the file change: hash emptied, capture pending.
    let mut pending = good.clone();
    pending.content_hash.clear();
    pending.file_size += 1;
    pending.capture = CaptureInfo { scan_pending: true, ..Default::default() };
    assert_eq!(p.merge_found(&[pending]), 0);
    assert_eq!(p.image(ImageId(2)).unwrap(), &good);
    // A new hash with a pending capture does not drop the good capture either.
    let mut rehashed = good.clone();
    rehashed.content_hash = "f".repeat(64);
    rehashed.capture = CaptureInfo { scan_pending: true, ..Default::default() };
    assert_eq!(p.merge_found(&[rehashed]), 0);
    assert_eq!(p.image(ImageId(2)).unwrap(), &good);
    // A complete entry replaces it normally.
    let mut done = good.clone();
    done.content_hash = "e".repeat(64);
    assert_eq!(p.merge_found(&[done.clone()]), 1);
    assert_eq!(p.image(ImageId(2)).unwrap(), &done);
}

fn stale_journals(t: &Tmp) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(&t.0)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains("-journal.stale-")))
        .collect();
    found.sort();
    found
}

#[test]
fn an_orphan_journal_next_to_a_new_file_is_moved_aside_never_deleted_or_replayed() {
    let t = Tmp::new();
    let file = t.path("new.rawgraph");
    // Left over from an older file of the same name that is long gone.
    let junk: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8 | 1).collect();
    fs::write(t.path("new.rawgraph-journal"), &junk).unwrap();
    let mut p = project_with_images(20);
    p.commit(&file, "first").unwrap();
    assert!(!t.path("new.rawgraph-journal").exists(), "no journal next to the new file");
    let stale = stale_journals(&t);
    assert_eq!(stale.len(), 1, "{stale:?}");
    let name = stale[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("new.rawgraph-journal.stale-") && name.len() == "new.rawgraph-journal.stale-20260101-000000".len(), "{name}");
    assert_eq!(fs::read(&stale[0]).unwrap(), junk, "kept as it was");
    // The next commit (in place) and a load do not see any foreign journal.
    p.image_mut(ImageId(2)).unwrap().rating = 5;
    p.commit(&file, "second").unwrap();
    assert_eq!(Project::load(&file).unwrap(), p);
    integrity_ok(&file);
    assert_eq!(stale_journals(&t).len(), 1);
}

#[cfg(windows)]
#[test]
fn a_failed_swap_puts_the_journal_back_and_leaves_the_old_file_byte_identical() {
    use std::os::windows::fs::OpenOptionsExt;
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut theirs = project_with_images(10);
    theirs.commit(&file, "theirs").unwrap();
    let junk = vec![7u8; 1024];
    fs::write(t.path("p.rawgraph-journal"), &junk).unwrap();
    let before = fs::read(&file).unwrap();
    // Somebody holds the target open without allowing it to be replaced (share read+write, not delete).
    let holder = fs::OpenOptions::new().read(true).share_mode(3).open(&file).unwrap();
    let mut mine = project_with_images(5);
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    let err = mine.commit_overwrite(&file, "replace").unwrap_err();
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
    assert!(matches!(err, ProjectError::Io(_)), "{err}");
    drop(holder);
    assert_eq!(fs::read(&file).unwrap(), before, "the old file is byte-identical");
    assert_eq!(fs::read(t.path("p.rawgraph-journal")).unwrap(), junk, "the journal is back where it was");
    assert!(stale_journals(&t).is_empty() && !t.path("p.rawgraph.saving").exists());
    assert_eq!(mine.revision(), 0, "the project is unchanged");
}

#[test]
fn a_journal_with_a_zeroed_header_is_stale_and_blocks_neither_recovery_nor_a_full_write() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(30);
    p.commit(&file, "base").unwrap();
    let bytes = fs::read(&file).unwrap();
    // A save was killed before the journal header was synced: the file is complete, the journal is zeros.
    fs::write(t.path("p.rawgraph-journal"), vec![0u8; 512]).unwrap();
    assert!(Project::recover_unfinished_save(&file).unwrap());
    assert!(!t.path("p.rawgraph-journal").exists());
    assert_eq!(fs::read(&file).unwrap(), bytes, "nothing was rolled back or changed");
    assert!(stale_journals(&t).len() <= 1);
    assert_eq!(Project::load(&file).unwrap(), p);

    // The same for a full write over it.
    fs::write(t.path("p.rawgraph-journal"), vec![0u8; 512]).unwrap();
    let mut other = project_with_images(4);
    other.commit_overwrite(&file, "replace").unwrap();
    assert!(!t.path("p.rawgraph-journal").exists());
    assert_eq!(Project::load(&file).unwrap(), other);
    assert!(!stale_journals(&t).is_empty());
}

#[test]
fn load_recovering_rolls_an_interrupted_save_back_only_where_this_computer_alone_writes() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(100);
    p.commit(&file, "base").unwrap();
    // In-place place (a local drive): recovered, and said so.
    let crashed = crashed_copy(&t, &file, "local");
    let (loaded, recovered) = Project::load_recovering(&crashed).unwrap();
    assert!(recovered);
    assert_eq!(loaded, p);
    assert!(!t.path("local.rawgraph-journal").exists());
    assert!(!Project::load_recovering(&crashed).unwrap().1, "nothing to recover the second time");
    // A network or sync place: the error comes back, nothing is touched.
    let shared = crashed_copy(&t, &file, "shared");
    let before = (fs::read(&shared).unwrap(), fs::read(t.path("shared.rawgraph-journal")).unwrap());
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    let err = Project::load_recovering(&shared);
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
    assert!(matches!(err, Err(ProjectError::UnfinishedSave(ref f)) if f == &shared), "{err:?}");
    assert_eq!((fs::read(&shared).unwrap(), fs::read(t.path("shared.rawgraph-journal")).unwrap()), before);
    // A full-rewrite commit on that file also reports the unfinished save and writes nothing.
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    let commit = p.commit(&shared, "x");
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
    assert!(matches!(commit, Err(ProjectError::UnfinishedSave(_))), "{commit:?}");
    assert_eq!(fs::read(&shared).unwrap(), before.0);
    // After the explicit recovery everything works again.
    assert!(Project::recover_unfinished_save(&shared).unwrap());
    assert_eq!(Project::load(&shared).unwrap(), p);
}

#[test]
fn the_check_right_before_the_swap_catches_a_commit_that_came_in_late() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut a = project_with_images(10);
    a.commit(&file, "base").unwrap();
    let b = Project::load(&file).unwrap();
    assert!(b.recheck_before_swap(&file, false).is_ok());
    a.image_mut(ImageId(1)).unwrap().rating = 5;
    a.commit(&file, "a").unwrap();
    assert!(matches!(b.recheck_before_swap(&file, false), Err(ProjectError::Conflict { disk_revision: 2, loaded_revision: 1 })));
    assert!(b.recheck_before_swap(&file, true).is_ok(), "overwrite skips it");
    // The guard stops a full write: the target is untouched and no temporary file stays.
    let bytes = fs::read(&file).unwrap();
    let ver = store::Versioning { revision: 2, log: &b.history };
    let refuse = || -> Result<(), ProjectError> { Err(ProjectError::Conflict { disk_revision: 9, loaded_revision: 1 }) };
    assert!(store::write_file(&b, &ver, &file, &[], true, Some(&refuse)).is_err());
    assert_eq!(fs::read(&file).unwrap(), bytes);
    assert!(!t.path("p.rawgraph.saving").exists());
}

#[test]
fn last_write_tells_how_the_file_was_really_written() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project_with_images(10);
    assert!(p.last_write().is_none());
    p.commit(&file, "first").unwrap();
    let w = p.last_write().unwrap();
    assert_eq!((w.mode, w.reason.as_str()), (WriteMode::FullRewrite, "new file"), "the first save is a whole file");
    p.commit(&file, "second").unwrap();
    assert_eq!(p.last_write().unwrap().mode, WriteMode::InPlace);
    // Another project's file is replaced as a whole.
    let mut other = project_with_images(3);
    other.commit_overwrite(&file, "replace").unwrap();
    let w = other.last_write().unwrap();
    assert_eq!(w.mode, WriteMode::FullRewrite);
    assert!(w.reason.contains("another project"), "{w:?}");
    // A change that cannot be made row by row falls back, and says so.
    let (u1, u2) = (other.image(ImageId(1)).unwrap().uuid.clone(), other.image(ImageId(2)).unwrap().uuid.clone());
    other.image_mut(ImageId(1)).unwrap().uuid = u2;
    other.image_mut(ImageId(2)).unwrap().uuid = u1;
    other.commit(&file, "swap").unwrap();
    let w = other.last_write().unwrap();
    assert_eq!(w.mode, WriteMode::FullRewrite);
    assert!(w.reason.contains("row by row"), "{w:?}");
    // A place that does not allow in place.
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    other.commit(&file, "again").unwrap();
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
    assert_eq!(other.last_write().unwrap().mode, WriteMode::FullRewrite);
    assert_eq!(other.last_write().unwrap().reason, "test");
}
