//! Schema 2: migration, uuids, revisions, history, conflicts, source folders.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rawgraph_core::{CaptureInfo, ImageId, NodeKind};

use super::*;
use crate::history::{HistoryEntry, HistoryLog, MAX_HISTORY_ENTRIES, MAX_SNAPSHOT_BYTES};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rawgraph-project-v2-{}-{n}", std::process::id()));
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
    /// A copy of the version-1 fixture inside the temporary directory.
    fn v1_copy(&self, name: &str) -> PathBuf {
        let dest = self.path(name);
        fs::copy(fixture(), &dest).unwrap();
        dest
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join("project_v1.rawgraph")
}

fn supported(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("raw"))
}

fn no_capture(_: &Path) -> Option<CaptureInfo> {
    None
}

fn schema_version_of(path: &Path) -> String {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap()
}

fn project_with_files(t: &Tmp, names: &[&str]) -> Project {
    for n in names {
        t.write(&format!("photos/{n}"), &format!("content of {n}"));
    }
    let mut p = Project::new();
    let r = p.scan_folder(&t.path("photos"), false, &supported, &no_capture);
    assert_eq!(r.added.len(), names.len());
    p
}

// ---- migration ----

#[test]
fn migration_chain_is_complete() {
    let steps = crate::store::migration_steps();
    assert_eq!(steps, (1..SCHEMA_VERSION).collect::<Vec<u32>>());
}

#[test]
fn fixture_v1_loads_intact_and_the_file_is_not_touched() {
    let t = Tmp::new();
    let file = t.v1_copy("old.rawgraph");
    let before = fs::read(&file).unwrap();
    assert_eq!(schema_version_of(&file), "1");

    let p = Project::load(&file).unwrap();
    assert_eq!(fs::read(&file).unwrap(), before, "loading must not modify the file");
    assert!(fs::read_dir(&t.0).unwrap().count() == 1, "loading must not leave other files behind");

    assert_eq!(p.images().len(), 3);
    let a = p.image(ImageId(1)).unwrap();
    assert_eq!(a.path, PathBuf::from("D:/photos/shoot/IMG_0001.RAW"));
    assert_eq!((a.rating, a.flag, a.color_label), (5, Flag::Pick, Some(ColorLabel::Green)));
    assert_eq!(a.capture.iso, Some(100.0));
    assert_eq!(a.capture.make.as_deref(), Some("Maker"));
    assert_eq!(a.content_hash.len(), 64);
    assert!(a.modified_unix_ms.is_some());
    let b = p.image(ImageId(2)).unwrap();
    assert_eq!((b.rating, b.flag, b.color_label), (2, Flag::Reject, Some(ColorLabel::Purple)));
    let c = p.image(ImageId(3)).unwrap();
    assert!(c.missing);
    assert_eq!((c.rating, c.flag, c.color_label), (0, Flag::None, None));

    let exp = p.graph.nodes().find(|n| n.kind == NodeKind::Exposure).expect("exposure node");
    assert_eq!(exp.param("ev", ImageId(1)), 1.5);
    assert_eq!(exp.param("ev", ImageId(2)), -0.75);
    assert!(p.graph.nodes().any(|n| n.kind == NodeKind::Source));
    assert_eq!(p.graph.nodes().count(), 3);
    assert_eq!(p.graph.edges().len(), 1);

    // New in schema 2.
    assert!(crate::ids::is_uuid(p.uuid()));
    let uuids: HashSet<&str> = p.images().iter().map(|e| e.uuid.as_str()).collect();
    assert_eq!(uuids.len(), 3);
    assert!(uuids.iter().all(|u| crate::ids::is_uuid(u)));
    assert_eq!(p.revision(), 0);
    assert!(p.history().is_empty());
    assert!(p.folders().is_empty());
    assert_eq!(p.graph_at(0), None);

    // Ids keep counting after the migration.
    let mut p = p;
    t.write("new/n.raw", "n");
    let r = p.scan_folder(&t.path("new"), false, &supported, &no_capture);
    assert_eq!(r.added, vec![ImageId(4)]);

    // The repository fixture itself loads read-only as well.
    let direct = Project::load(&fixture()).unwrap();
    assert_eq!(direct.images().len(), 3);
}

#[test]
fn first_commit_over_a_v1_file_keeps_a_backup_and_writes_schema_2() {
    let t = Tmp::new();
    let file = t.v1_copy("old.rawgraph");
    let original = fs::read(&file).unwrap();
    let mut p = Project::load(&file).unwrap();
    let image_uuids: Vec<String> = p.images().iter().map(|e| e.uuid.clone()).collect();
    let project_uuid = p.uuid().to_string();

    assert_eq!(p.commit(&file, "migrate").unwrap(), 1);
    let bak = t.path("old.rawgraph.v1.bak");
    assert_eq!(fs::read(&bak).unwrap(), original, "the backup is the untouched old file");
    assert_eq!(schema_version_of(&file), "2");
    assert!(!t.path("old.rawgraph.saving").exists());

    let back = Project::load(&file).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.uuid(), project_uuid);
    assert_eq!(back.images().iter().map(|e| e.uuid.clone()).collect::<Vec<_>>(), image_uuids);
    assert_eq!(back.revision(), 1);
    assert_eq!(back.history().len(), 1);
    assert_eq!(back.image(ImageId(1)).unwrap().rating, 5);

    // A file that is already schema 2 gets no further backup.
    let mut back = back;
    back.commit(&file, "again").unwrap();
    let baks: Vec<_> = fs::read_dir(&t.0).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".bak")).collect();
    assert_eq!(baks.len(), 1);
}

#[test]
fn backups_never_overwrite_an_existing_copy() {
    let t = Tmp::new();
    let file = t.v1_copy("old.rawgraph");
    let original = fs::read(&file).unwrap();
    t.write("old.rawgraph.v1.bak", "precious");

    let p = Project::load(&file).unwrap();
    p.save(&file).unwrap(); // a plain save over a v1 file also keeps a copy
    assert_eq!(fs::read_to_string(t.path("old.rawgraph.v1.bak")).unwrap(), "precious");
    assert_eq!(fs::read(t.path("old.rawgraph.v1 (2).bak")).unwrap(), original);

    // Put a v1 file back and write again: third name.
    fs::write(&file, &original).unwrap();
    let mut q = Project::load(&file).unwrap();
    q.commit(&file, "x").unwrap();
    assert_eq!(fs::read(t.path("old.rawgraph.v1 (3).bak")).unwrap(), original);
    assert_eq!(fs::read_to_string(t.path("old.rawgraph.v1.bak")).unwrap(), "precious");
}

#[test]
fn commit_over_a_v1_file_needs_a_project_that_came_from_it() {
    let t = Tmp::new();
    let file = t.v1_copy("old.rawgraph");
    let before = fs::read(&file).unwrap();
    let mut stranger = Project::new();
    match stranger.commit(&file, "x") {
        Err(ProjectError::Mismatch(m)) => assert!(m.contains("old project file")),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(fs::read(&file).unwrap(), before);
    assert_eq!(stranger.commit_overwrite(&file, "x").unwrap(), 1);
    assert!(t.path("old.rawgraph.v1.bak").exists());
}

#[test]
fn newer_file_version_is_too_new_and_never_replaced_by_commit() {
    let t = Tmp::new();
    let file = t.path("future.rawgraph");
    let mut p = Project::new();
    p.save(&file).unwrap();
    {
        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.execute("UPDATE meta SET value = '7' WHERE key = 'schema_version'", []).unwrap();
    }
    match Project::load(&file) {
        Err(ProjectError::TooNew { file_version: 7, supported }) => assert_eq!(supported, SCHEMA_VERSION),
        other => panic!("unexpected {other:?}"),
    }
    let text = Project::load(&file).unwrap_err().to_string();
    assert!(text.contains('7') && text.contains(&SCHEMA_VERSION.to_string()));
    let before = fs::read(&file).unwrap();
    assert!(matches!(p.commit(&file, "x"), Err(ProjectError::TooNew { file_version: 7, .. })));
    assert_eq!(fs::read(&file).unwrap(), before);
    // The explicit overwrite goes ahead, but keeps the newer file as a backup.
    p.commit_overwrite(&file, "x").unwrap();
    assert_eq!(fs::read(t.path("future.rawgraph.v7.bak")).unwrap(), before);
}

// ---- uuids ----

#[test]
fn uuids_are_unique_and_well_formed() {
    let mut seen = HashSet::new();
    for _ in 0..10_000 {
        let u = crate::ids::new_uuid();
        assert!(crate::ids::is_uuid(&u), "{u}");
        assert!(seen.insert(u));
    }
    let mut from_threads = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..4).map(|_| s.spawn(|| (0..500).map(|_| crate::ids::new_uuid()).collect::<Vec<_>>())).collect();
        for h in handles {
            from_threads.extend(h.join().unwrap());
        }
    });
    assert!(from_threads.iter().all(|u| seen.insert(u.clone())));
    assert!(!crate::ids::is_uuid("ABCDEFABCDEFABCDEFABCDEFABCDEFAB"));
    assert!(!crate::ids::is_uuid("abc"));
}

#[test]
fn uuids_survive_save_load_relink_save_as_and_recovery() {
    let t = Tmp::new();
    let mut p = project_with_files(&t, &["a.raw", "b.raw"]);
    assert!(crate::ids::is_uuid(p.uuid()));
    let uuids: Vec<String> = p.images().iter().map(|e| e.uuid.clone()).collect();
    assert_ne!(uuids[0], uuids[1]);
    let project_uuid = p.uuid().to_string();

    // Relink by scan (moved file) and by hand.
    fs::rename(t.path("photos/a.raw"), t.path("photos/moved.raw")).unwrap();
    let r = p.scan_folder(&t.path("photos"), false, &supported, &no_capture);
    assert_eq!(r.relinked, vec![ImageId(1)]);
    assert_eq!(p.image(ImageId(1)).unwrap().uuid, uuids[0]);
    fs::copy(t.path("photos/b.raw"), t.path("elsewhere.raw")).unwrap();
    fs::remove_file(t.path("photos/b.raw")).unwrap();
    p.relink(ImageId(2), &t.path("elsewhere.raw"), false).unwrap();
    assert_eq!(p.image(ImageId(2)).unwrap().uuid, uuids[1]);

    let f1 = t.path("one.rawgraph");
    let f2 = t.path("two.rawgraph");
    p.save(&f1).unwrap();
    p.save(&f2).unwrap(); // "save as" keeps the project uuid
    for f in [&f1, &f2] {
        let q = Project::load(f).unwrap();
        assert_eq!(q.uuid(), project_uuid);
        assert_eq!(q.images().iter().map(|e| e.uuid.clone()).collect::<Vec<_>>(), uuids);
    }
    p.commit(&f1, "c").unwrap();
    assert_eq!(Project::load(&f1).unwrap().uuid(), project_uuid);

    let rec = p.save_recovery(&t.path("rec"), Some(&f1), "k").unwrap();
    let (q, _) = load_recovery(&rec).unwrap();
    assert_eq!(q, p);
    assert_eq!(q.uuid(), project_uuid);
    assert_eq!(q.revision(), 1);
    assert_eq!(q.history(), p.history());

    // Removing and adding images never reuses an image uuid.
    p.remove_images(&[ImageId(2)]);
    t.write("photos/new.raw", "new");
    p.scan_folder(&t.path("photos"), false, &supported, &no_capture);
    let all: HashSet<&str> = p.images().iter().map(|e| e.uuid.as_str()).collect();
    assert_eq!(all.len(), p.images().len());
    assert!(!all.contains(uuids[1].as_str()));
}

// ---- revisions and history ----

#[test]
fn commit_increments_the_revision_and_records_the_entry() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    assert_eq!(p.revision(), 0);
    assert!(p.history().is_empty());
    let before = ids_now();
    assert_eq!(p.commit(&file, "first").unwrap(), 1);
    p.graph.add_node(NodeKind::Exposure, [10.0, 10.0]);
    assert_eq!(p.commit(&file, "second").unwrap(), 2);
    let after = ids_now();

    assert_eq!(p.revision(), 2);
    let h = p.history();
    assert_eq!(h.len(), 2);
    assert_eq!((h[0].revision, h[0].summary.as_str()), (1, "first"));
    assert_eq!((h[1].revision, h[1].summary.as_str()), (2, "second"));
    for e in h {
        assert!(e.has_graph);
        assert!(!e.author.is_empty() && !e.device.is_empty());
        assert!(e.time_unix_ms >= before && e.time_unix_ms <= after);
    }
    assert!(h[0].time_unix_ms <= h[1].time_unix_ms);

    let back = Project::load(&file).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.revision(), 2);
    assert_eq!(back.history(), p.history());
}

fn ids_now() -> i64 {
    crate::ids::now_ms()
}

#[test]
fn graph_at_returns_the_graph_of_each_revision() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    let g0 = p.graph.clone();
    p.commit(&file, "base").unwrap();
    let exp = p.graph.add_node(NodeKind::Exposure, [100.0, 0.0]);
    p.graph.node_mut(exp).unwrap().set_param("ev", ImageId(1), 2.0);
    let g1 = p.graph.clone();
    p.commit(&file, "exposure").unwrap();
    p.graph.add_node(NodeKind::Exposure, [200.0, 0.0]);
    p.commit(&file, "another").unwrap();

    assert_eq!(p.graph_at(1), Some(g0));
    assert_eq!(p.graph_at(2), Some(g1));
    assert_eq!(p.graph_at(3), Some(p.graph.clone()));
    assert_eq!(p.graph_at(0), None);
    assert_eq!(p.graph_at(4), None);

    let back = Project::load(&file).unwrap();
    assert_eq!(back.graph_at(2).unwrap().nodes().count(), 3);
    assert_eq!(back.graph_at(1).unwrap().nodes().count(), 2);
}

#[test]
fn history_keeps_at_most_100_entries() {
    assert_eq!(MAX_HISTORY_ENTRIES, 100);
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    for i in 1..=105u64 {
        assert_eq!(p.commit(&file, &format!("change {i}")).unwrap(), i);
    }
    assert_eq!(p.history().len(), 100);
    assert_eq!(p.history().first().unwrap().revision, 6);
    assert_eq!(p.history().last().unwrap().revision, 105);
    assert!(p.graph_at(5).is_none());
    assert!(p.graph_at(6).is_some());
    let back = Project::load(&file).unwrap();
    assert_eq!(back.history().len(), 100);
    assert_eq!(back.history().first().unwrap().summary, "change 6");
    assert_eq!(back.revision(), 105);
}

fn entry(revision: u64) -> HistoryEntry {
    HistoryEntry {
        revision,
        time_unix_ms: 1_000 + revision as i64,
        author: "a".into(),
        device: "d".into(),
        summary: format!("r{revision}"),
        has_graph: true,
    }
}

#[test]
fn history_drops_old_snapshots_before_old_entries() {
    assert_eq!(MAX_SNAPSHOT_BYTES, 32 * 1024 * 1024);
    // Limit of 25 bytes, 10 bytes per snapshot: the two newest snapshots stay.
    let mut log = HistoryLog::default();
    for r in 1..=5u64 {
        log.push(entry(r), "0123456789".to_string(), 100, 25);
    }
    assert_eq!(log.entries.len(), 5, "entries stay while the count limit holds");
    assert_eq!(log.entries.iter().map(|e| e.has_graph).collect::<Vec<_>>(), [false, false, false, true, true]);
    assert_eq!(log.snapshot_bytes(), 20);
    assert_eq!(log.snapshot(1), None);
    assert_eq!(log.snapshot(5), Some("0123456789"));
    // The count limit drops the oldest entries (with whatever they hold).
    log.push(entry(6), "0123456789".to_string(), 4, 25);
    assert_eq!(log.entries.iter().map(|e| e.revision).collect::<Vec<_>>(), [3, 4, 5, 6]);
    assert_eq!(log.entries.iter().map(|e| e.has_graph).collect::<Vec<_>>(), [false, false, true, true]);
    assert!(log.snapshot_bytes() <= 25);
    // One snapshot alone above the limit cannot be kept.
    let mut big = HistoryLog::default();
    big.push(entry(1), "x".repeat(30), 100, 25);
    assert_eq!(big.entries.len(), 1);
    assert!(!big.entries[0].has_graph);
}

#[test]
fn entries_without_snapshots_roundtrip() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    let mut log = HistoryLog::default();
    for r in 1..=3u64 {
        log.push(entry(r), p.graph.to_json(), 100, 10);
    }
    assert!(log.entries.iter().all(|e| !e.has_graph));
    p.history = log;
    p.revision = 3;
    p.save(&file).unwrap();
    let back = Project::load(&file).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.history().len(), 3);
    assert!(back.history().iter().all(|e| !e.has_graph));
    assert_eq!(back.graph_at(3), None);
}

#[test]
fn save_does_not_create_a_revision() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    p.save(&file).unwrap();
    let q = Project::load(&file).unwrap();
    assert_eq!((q.revision(), q.history().len()), (0, 0));

    p.commit(&file, "one").unwrap();
    p.graph.add_node(NodeKind::Exposure, [0.0, 0.0]);
    p.save(&file).unwrap();
    let q = Project::load(&file).unwrap();
    assert_eq!(q.revision(), 1);
    assert_eq!(q.history().len(), 1);
    assert_eq!(q.graph.nodes().count(), 3, "save writes the current state");
    assert_eq!(q.graph_at(1).unwrap().nodes().count(), 2, "the snapshot keeps the committed state");
    assert_eq!(p.revision(), 1);
}

// ---- conflicts ----

#[test]
fn second_commit_from_a_stale_copy_is_a_conflict() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut seed = Project::new();
    seed.commit(&file, "seed").unwrap();

    let mut a = Project::load(&file).unwrap();
    let mut b = Project::load(&file).unwrap();
    assert_eq!((a.revision(), b.revision()), (1, 1));
    a.graph.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert_eq!(a.commit(&file, "a").unwrap(), 2);
    assert_eq!(a.commit(&file, "a again").unwrap(), 3, "the committing copy stays current");
    let disk_before = fs::read(&file).unwrap();

    b.graph.add_node(NodeKind::Exposure, [5.0, 5.0]);
    let (rev_b, hist_b) = (b.revision(), b.history().len());
    match b.commit(&file, "b") {
        Err(ProjectError::Conflict { disk_revision: 3, loaded_revision: 1 }) => {}
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(fs::read(&file).unwrap(), disk_before, "nothing is written on a conflict");
    assert_eq!((b.revision(), b.history().len()), (rev_b, hist_b));
    assert!(!t.path("p.rawgraph.saving").exists());

    // Reloading resolves it.
    let mut b2 = Project::load(&file).unwrap();
    assert_eq!(b2.commit(&file, "b2").unwrap(), 4);
}

#[test]
fn conflict_error_text_names_both_revisions() {
    let text = ProjectError::Conflict { disk_revision: 9, loaded_revision: 4 }.to_string();
    assert!(text.contains('9') && text.contains('4'));
}

#[test]
fn commit_to_a_new_path_needs_no_check() {
    let t = Tmp::new();
    let one = t.path("one.rawgraph");
    let two = t.path("two.rawgraph");
    let mut p = Project::new();
    p.commit(&one, "a").unwrap();
    p.commit(&one, "b").unwrap();
    // Another copy commits to `one` meanwhile.
    let mut other = Project::load(&one).unwrap();
    other.commit(&one, "c").unwrap();
    // "Save as" elsewhere is not affected by the stale revision.
    assert_eq!(p.commit(&two, "as").unwrap(), 3);
    let q = Project::load(&two).unwrap();
    assert_eq!((q.uuid(), q.revision()), (p.uuid(), 3));
}

#[test]
fn commit_over_another_project_needs_the_explicit_overwrite() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut first = Project::new();
    first.commit(&file, "first").unwrap();
    let before = fs::read(&file).unwrap();

    let mut second = Project::new();
    match second.commit(&file, "intruder") {
        Err(ProjectError::Mismatch(m)) => assert!(m.contains("another project"), "{m}"),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(fs::read(&file).unwrap(), before);
    assert_eq!((second.revision(), second.history().len()), (0, 0));

    assert_eq!(second.commit_overwrite(&file, "replace").unwrap(), 1);
    assert_eq!(Project::load(&file).unwrap().uuid(), second.uuid());
    // The original project now meets a foreign file.
    assert!(matches!(first.commit(&file, "x"), Err(ProjectError::Mismatch(_))));
}

#[test]
fn commit_overwrite_skips_the_conflict_check_and_never_reuses_a_revision() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut seed = Project::new();
    seed.commit(&file, "seed").unwrap();
    let mut a = Project::load(&file).unwrap();
    let mut b = Project::load(&file).unwrap();
    a.commit(&file, "a").unwrap();
    assert!(matches!(b.commit(&file, "b"), Err(ProjectError::Conflict { .. })));
    assert_eq!(b.commit_overwrite(&file, "b").unwrap(), 3);
    let disk = Project::load(&file).unwrap();
    assert_eq!(disk.revision(), 3);
    assert_eq!(disk.history().last().unwrap().summary, "b");
    // Overwriting a missing or unreadable target works as well.
    let junk = t.write("junk.rawgraph", "not a database at all\n".repeat(30).as_str());
    assert!(b.commit(&junk, "x").is_err());
    assert_eq!(b.commit_overwrite(&junk, "x").unwrap(), 4);
    assert!(Project::load(&junk).is_ok());
}

// ---- failures ----

#[test]
fn failed_commits_leave_project_and_file_unchanged() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = Project::new();
    p.commit(&file, "one").unwrap();
    let on_disk = fs::read(&file).unwrap();
    let snapshot = p.clone();

    // Target directory does not exist.
    let r = p.commit(&t.path("no-such-dir/p.rawgraph"), "x");
    assert!(r.is_err());
    assert_eq!(p, snapshot);
    assert_eq!(p.revision(), 1);

    // Target is a directory.
    fs::create_dir_all(t.path("dir.rawgraph")).unwrap();
    assert!(matches!(p.commit(&t.path("dir.rawgraph"), "x"), Err(ProjectError::Io(_))));
    assert_eq!(p, snapshot);

    // In-place update: the transaction fails after all rows were written but
    // before it is committed. The file keeps its old bytes, the project is unchanged.
    p.graph.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let changed = p.clone();
    crate::store::FAIL_BEFORE_COMMIT.with(|f| f.set(true));
    assert!(p.commit(&file, "x").is_err());
    assert_eq!(fs::read(&file).unwrap(), on_disk, "rollback restores the file exactly");
    assert!(!t.path("p.rawgraph-journal").exists(), "no journal is left behind");
    assert_eq!(p, changed, "a failed commit changes nothing, not even the history");
    assert_eq!(p.revision(), 1);
    assert_eq!(p.history().len(), 1);
    assert_eq!(p.commit(&file, "now it works").unwrap(), 2);

    // Full rewrite (another project's file is replaced): the temporary file name is
    // blocked, the write fails, the old file stays.
    let other = t.path("other.rawgraph");
    Project::new().commit(&other, "theirs").unwrap();
    let other_bytes = fs::read(&other).unwrap();
    fs::create_dir_all(t.path("other.rawgraph.saving")).unwrap();
    assert!(p.commit_overwrite(&other, "x").is_err());
    assert_eq!(fs::read(&other).unwrap(), other_bytes);
    assert_eq!(p.revision(), 2);
    fs::remove_dir_all(t.path("other.rawgraph.saving")).unwrap();
    assert_eq!(p.commit_overwrite(&other, "now it works").unwrap(), 3);
}

// ---- folders ----

#[test]
fn scanned_folders_are_recorded_and_persisted() {
    let t = Tmp::new();
    t.write("a/one.raw", "1");
    t.write("a/sub/two.raw", "2");
    t.write("b/three.raw", "3");
    let mut p = Project::new();
    assert!(p.folders().is_empty());

    p.scan_folder(&t.path("a"), true, &supported, &no_capture);
    assert_eq!(p.folders(), &[SourceFolder { path: t.path("a"), recursive: true }]);

    // Rescanning the same folder (also spelled with a trailing separator) updates the flag.
    let with_sep = t.path("a").join("");
    p.scan_folder(&with_sep, false, &supported, &no_capture);
    assert_eq!(p.folders(), &[SourceFolder { path: t.path("a"), recursive: false }]);
    p.scan_folder_with(&t.path("a"), true, &supported, &no_capture, &mut |_| true);
    assert_eq!(p.folders().len(), 1);
    assert!(p.folders()[0].recursive);

    p.scan_folder(&t.path("b"), false, &supported, &no_capture);
    assert_eq!(p.folders().len(), 2);
    assert_eq!(p.folders()[1], SourceFolder { path: t.path("b"), recursive: false });

    // A folder that does not exist is not remembered; an empty one is.
    p.scan_folder(&t.path("missing"), true, &supported, &no_capture);
    fs::create_dir_all(t.path("empty")).unwrap();
    p.scan_folder(&t.path("empty"), true, &supported, &no_capture);
    assert_eq!(p.folders().len(), 3);
    assert_eq!(p.folders()[2].path, t.path("empty"));

    let file = t.path("p.rawgraph");
    p.save(&file).unwrap();
    assert_eq!(Project::load(&file).unwrap().folders(), p.folders());
    p.commit(&file, "c").unwrap();
    assert_eq!(Project::load(&file).unwrap(), p);
}

#[cfg(windows)]
#[test]
fn folder_paths_compare_case_insensitively_on_windows() {
    let t = Tmp::new();
    t.write("Shots/x.raw", "x");
    let mut p = Project::new();
    p.scan_folder(&t.path("Shots"), false, &supported, &no_capture);
    p.scan_folder(&t.path("SHOTS"), true, &supported, &no_capture);
    assert_eq!(p.folders().len(), 1);
    assert!(p.folders()[0].recursive);
}
