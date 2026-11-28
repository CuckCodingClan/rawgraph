//! Tests of saving with revisions, the conflict questions, the History window's logic and the
//! small defects of wave 8b-2. No window: temp folders and the pure logic behind the dialogs.

use std::path::{Path, PathBuf};

use rawgraph_core::{Graph, ImageId, Mode, NodeId, NodeKind};
use rawgraph_project::{Flag, Project};

use crate::history::History;
use crate::revisions::{self, Baseline, Row};
use crate::save_flow::{Outcome, SaveFlow, Stage};
use crate::session::tests::{png_project, provider};
use crate::session::Session;

#[derive(Clone, Debug, PartialEq)]
enum Act {
    Quit,
    Open,
}

fn session_with_images(dir: &Path, n: usize) -> Session {
    Session::new(png_project(dir, n), provider())
}

fn add(s: &mut Session, kind: NodeKind) -> NodeId {
    let n = s.project.graph.nodes().count() as f32;
    let id = s.project.graph.add_node(kind, [n * 240.0, 300.0]);
    s.touch();
    id
}

fn last_summary(s: &Session) -> String {
    s.project.history().last().map(|e| e.summary.clone()).unwrap_or_default()
}

fn commit(s: &mut Session, path: &Path) -> u64 {
    s.commit_project(path, false).unwrap()
}

// ------------------------------------------------------------------ A1: revisions and summaries

#[test]
fn every_save_is_a_revision_with_a_summary_of_what_changed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("p.rawgraph");
    let mut s = session_with_images(dir.path(), 3);
    let ids: Vec<ImageId> = s.visible().to_vec();

    assert_eq!(commit(&mut s, &file), 1);
    assert_eq!(last_summary(&s), "Saved", "nothing changed since the start");
    assert!(!s.dirty);

    let a = add(&mut s, NodeKind::Exposure);
    add(&mut s, NodeKind::Contrast);
    assert_eq!(commit(&mut s, &file), 2);
    assert_eq!(last_summary(&s), "2 nodes added");

    assert!(s.project.graph.node_mut(a).unwrap().set_param("ev", ImageId(0), 0.4));
    s.touch();
    assert_eq!(commit(&mut s, &file), 3);
    assert_eq!(last_summary(&s), "1 parameter change");

    s.set_rating(ids[0], 4);
    s.set_rating(ids[1], 2);
    s.set_flag(ids[2], Flag::Pick);
    assert_eq!(commit(&mut s, &file), 4);
    assert_eq!(last_summary(&s), "2 ratings, 1 flag");

    s.project.graph.remove_node(a);
    s.touch();
    assert_eq!(commit(&mut s, &file), 5);
    assert!(last_summary(&s).starts_with("1 node removed"), "{}", last_summary(&s));

    assert_eq!(commit(&mut s, &file), 6);
    assert_eq!(last_summary(&s), "Saved");

    let on_disk = Project::load(&file).unwrap();
    assert_eq!(on_disk.revision(), 6);
    let revs: Vec<u64> = on_disk.history().iter().map(|e| e.revision).collect();
    assert_eq!(revs, [1, 2, 3, 4, 5, 6]);
}

#[test]
fn added_images_and_connection_changes_are_in_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::new(Project::new(), provider());
    let base = Baseline::of(&s.project);
    let finished = png_project(dir.path(), 4);
    s.finish_scan(finished, true);
    let text = base.summarize(&s.project);
    assert!(text.contains("4 images added"), "{text}");
    let base = Baseline::of(&s.project);
    let out = s.output_node().unwrap();
    s.project.graph.disconnect(out, 0);
    assert_eq!(base.summarize(&s.project), "1 connection change");
}

// ------------------------------------------------------------------ A2: conflict

/// Two copies of the same saved project: (path, A, B).
fn two_sessions(dir: &Path) -> (PathBuf, Session, Session) {
    let file = dir.join("shared.rawgraph");
    let mut a = session_with_images(dir, 2);
    commit(&mut a, &file);
    let mut b = Session::new(Project::new(), provider());
    b.load_project(&file).unwrap();
    (file, a, b)
}

fn bytes(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

#[test]
fn the_second_save_of_two_sessions_raises_the_conflict_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut a, mut b) = two_sessions(dir.path());
    add(&mut a, NodeKind::Contrast);
    assert_eq!(commit(&mut a, &file), 2);
    let after_a = bytes(&file);

    add(&mut b, NodeKind::Exposure);
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    let out = flow.save(&mut b, &file, None);
    assert_eq!(out, Outcome::Asked);
    assert!(matches!(&flow.stage, Stage::Conflict { disk_revision: 2, loaded_revision: 1, .. }), "{:?}", flow.stage);
    assert_eq!(bytes(&file), after_a, "nothing is written before a choice");
    assert!(b.dirty, "the project is still changed");
}

#[test]
fn cancel_keeps_the_project_changed_and_the_file_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut a, mut b) = two_sessions(dir.path());
    add(&mut a, NodeKind::Contrast);
    commit(&mut a, &file);
    let theirs = bytes(&file);
    add(&mut b, NodeKind::Exposure);
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    assert_eq!(flow.save(&mut b, &file, None), Outcome::Asked);
    flow.cancel();
    assert!(!flow.is_open());
    assert!(b.dirty);
    assert_eq!(bytes(&file), theirs);
}

#[test]
fn save_as_copy_keeps_both_files_intact() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut a, mut b) = two_sessions(dir.path());
    add(&mut a, NodeKind::Contrast);
    commit(&mut a, &file);
    let theirs = bytes(&file);
    let id = b.visible()[0];
    b.set_rating(id, 5);
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    assert_eq!(flow.save(&mut b, &file, Some(Act::Open)), Outcome::Asked);
    let (name, then) = flow.choose_copy("shared", "2026-11-15").unwrap();
    assert_eq!(name, "shared (conflict 2026-11-15).rawgraph");
    assert_eq!(then, Some(Act::Open));
    assert!(!flow.is_open());
    assert_eq!(bytes(&file), theirs, "choosing the copy writes nothing");
    let copy = dir.path().join(&name);
    assert!(!copy.exists());

    // The Save as dialog then saves to the new name.
    match flow.save(&mut b, &copy, then) {
        Outcome::Saved { then, target, .. } => {
            assert_eq!(then, Some(Act::Open));
            assert_eq!(target, copy);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(bytes(&file), theirs, "their file is untouched");
    assert_eq!(b.project_path.as_deref(), Some(copy.as_path()));
    assert!(!b.dirty);
    let loaded = Project::load(&copy).unwrap();
    assert_eq!(loaded.image(id).unwrap().rating, 5);
    let original = Project::load(&file).unwrap();
    assert_eq!(original.image(id).unwrap().rating, 0, "their file has no rating");
}

#[test]
fn overwrite_needs_a_second_confirmation_and_names_what_is_lost() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut a, mut b) = two_sessions(dir.path());
    add(&mut a, NodeKind::Contrast);
    commit(&mut a, &file);
    let theirs = bytes(&file);
    add(&mut b, NodeKind::Exposure);
    let mine = b.project.graph.clone();
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    assert_eq!(flow.save(&mut b, &file, None), Outcome::Asked);
    flow.choose_overwrite();
    match &flow.stage {
        Stage::ConfirmOverwrite { lost, .. } => {
            assert!(lost.starts_with("1 revision by "), "{lost}");
            assert!(lost.contains(" on "), "{lost}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(bytes(&file), theirs, "still nothing written");
    match flow.confirm(&mut b) {
        Outcome::Saved { revision, .. } => assert_eq!(revision, 3, "a revision number is never reused"),
        other => panic!("{other:?}"),
    }
    assert!(!b.dirty);
    assert_eq!(Project::load(&file).unwrap().graph, mine);
}

#[test]
fn quitting_with_unsaved_changes_and_a_conflict_does_not_close() {
    let dir = tempfile::tempdir().unwrap();
    let (file, mut a, mut b) = two_sessions(dir.path());
    add(&mut a, NodeKind::Contrast);
    commit(&mut a, &file);
    add(&mut b, NodeKind::Exposure);
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    // "Save" in the unsaved-changes question: the quit waits inside the flow.
    let out = flow.save(&mut b, &file, Some(Act::Quit));
    assert_eq!(out, Outcome::Asked, "no `then` comes out: the application stays open");
    assert!(b.dirty);
    flow.choose_overwrite();
    match flow.confirm(&mut b) {
        Outcome::Saved { then, .. } => assert_eq!(then, Some(Act::Quit), "only now may the quit go on"),
        other => panic!("{other:?}"),
    }
    // A failed save (no folder) also does not release the action.
    add(&mut b, NodeKind::Exposure);
    let out = flow.save(&mut b, &dir.path().join("missing-folder").join("x.rawgraph"), Some(Act::Quit));
    assert!(matches!(out, Outcome::Failed(ref m) if !m.is_empty()), "{out:?}");
    assert!(b.dirty);
}

#[test]
fn a_save_during_a_scan_goes_through_and_saves_what_was_found() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session_with_images(dir.path(), 1);
    s.begin_scan();
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    let out = flow.save(&mut s, &dir.path().join("x.rawgraph"), None);
    assert!(matches!(out, Outcome::Saved { revision: 1, .. }), "{out:?}");
    assert!(dir.path().join("x.rawgraph").exists());
    assert!(s.scanning, "the scan goes on");
}

// ------------------------------------------------------------------ A3: another project's file

#[test]
fn save_as_over_another_project_asks_before_replacing_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("other.rawgraph");
    let mut other = session_with_images(dir.path(), 1);
    commit(&mut other, &file);
    let theirs = bytes(&file);
    let other_uuid = other.project.uuid().to_owned();

    let mut mine = Session::new(Project::new(), provider());
    add(&mut mine, NodeKind::Contrast);
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    assert_eq!(flow.save(&mut mine, &file, None), Outcome::Asked);
    assert!(matches!(&flow.stage, Stage::ConfirmReplace { target, .. } if target == &file), "{:?}", flow.stage);
    assert_eq!(bytes(&file), theirs);
    // Cancel: nothing happens.
    flow.cancel();
    assert_eq!(bytes(&file), theirs);
    assert!(mine.dirty);
    // Yes: the file is replaced.
    assert_eq!(flow.save(&mut mine, &file, None), Outcome::Asked);
    assert!(matches!(flow.confirm(&mut mine), Outcome::Saved { .. }));
    let now = Project::load(&file).unwrap();
    assert_eq!(now.uuid(), mine.project.uuid());
    assert_ne!(now.uuid(), other_uuid);
}

// ------------------------------------------------------------------ A4 / A5: opening

/// Copy of a saved project whose format number reads 3 (a newer program wrote it).
fn newer_file(dir: &Path) -> PathBuf {
    let src = dir.join("src.rawgraph");
    let mut s = session_with_images(dir, 1);
    commit(&mut s, &src);
    let mut data = bytes(&src);
    let key = b"schema_version";
    let at = data.windows(key.len()).position(|w| w == key).unwrap() + key.len();
    assert_eq!(data[at], b'2');
    data[at] = b'3';
    let out = dir.join("newer.rawgraph");
    std::fs::write(&out, data).unwrap();
    out
}

#[test]
fn a_newer_format_is_refused_with_a_clear_message_and_the_project_stays() {
    let dir = tempfile::tempdir().unwrap();
    let newer = newer_file(dir.path());
    let mut s = session_with_images(dir.path(), 2);
    let before = s.project.clone();
    s.set_rating(s.visible()[0], 3);
    let err = s.load_project(&newer).unwrap_err();
    assert_eq!(err.to_string(), "This project was saved by a newer version of RawGraph (format 3, this version reads up to 2)");
    assert_eq!(s.project.images().len(), before.images().len(), "the current project stays open");
    assert!(s.dirty, "with its unsaved change");
    assert!(s.project_path.is_none());
    // Saving onto such a file does not damage it either.
    let theirs = bytes(&newer);
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    let out = flow.save(&mut s, &newer, None);
    assert!(matches!(&out, Outcome::Failed(m) if m.contains("newer version of RawGraph")), "{out:?}");
    assert_eq!(bytes(&newer), theirs);
}

fn fixture_v1() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("rawgraph-project").join("tests").join("fixtures").join("project_v1.rawgraph")
}

#[test]
fn a_format_1_project_opens_with_a_note_and_the_first_save_keeps_a_backup() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("old.rawgraph");
    std::fs::copy(fixture_v1(), &file).unwrap();
    let original = bytes(&file);
    assert_eq!(revisions::file_format_version(&file), Some(1));

    let (_, note) = revisions::open_project(&file).unwrap();
    assert_eq!(note, Some("Project converted from format 1 \u{2014} a backup copy is kept next to the file on the next save"));
    assert_eq!(bytes(&file), original, "opening changes nothing");

    let mut s = Session::new(Project::new(), provider());
    assert_eq!(s.load_project(&file).unwrap(), Some(revisions::CONVERTED_NOTE));
    assert_eq!(commit(&mut s, &file), 1);
    let baks: Vec<String> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".v1.bak")).collect();
    assert_eq!(baks, ["old.rawgraph.v1.bak"]);
    assert_eq!(bytes(&dir.path().join(&baks[0])), original, "the backup is the old file");
    // Now it is format 2: no note.
    assert_eq!(revisions::file_format_version(&file), Some(2));
    assert_eq!(revisions::open_project(&file).unwrap().1, None);
}

#[test]
fn the_fixture_is_not_modified_by_these_tests() {
    // Only copies are opened; reading the fixture itself must not change its format.
    assert_eq!(revisions::file_format_version(&fixture_v1()), Some(1));
}

// ------------------------------------------------------------------ B1 / B2: history window

#[test]
fn history_rows_are_newest_first_with_their_texts() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("h.rawgraph");
    let mut s = session_with_images(dir.path(), 2);
    commit(&mut s, &file);
    add(&mut s, NodeKind::Contrast);
    commit(&mut s, &file);
    s.set_rating(s.visible()[0], 1);
    commit(&mut s, &file);
    let rows = revisions::rows(&s.project);
    assert_eq!(rows.iter().map(|r| r.revision).collect::<Vec<_>>(), [3, 2, 1]);
    assert_eq!(rows[0].summary, "1 rating");
    assert_eq!(rows[1].summary, "1 node added");
    assert!(rows.iter().all(|r| r.has_graph && !r.author.is_empty() && !r.device.is_empty()));
    assert!(rows[0].when.len() == 19 && rows[0].when.as_bytes()[4] == b'-', "{}", rows[0].when);
    assert_eq!(rows[0].summary_text(), "1 rating");
    let dropped = Row { has_graph: false, ..rows[2].clone() };
    assert_eq!(dropped.summary_text(), "Saved (graph not kept)");
}

#[test]
fn the_comparison_names_nodes_and_counts_parameter_changes() {
    let mut old = Graph::new();
    let src = old.add_node(NodeKind::Source, [0.0, 0.0]);
    let exp = old.add_node(NodeKind::Exposure, [200.0, 0.0]);
    let out = old.add_node(NodeKind::Output, [400.0, 0.0]);
    old.connect(src, exp, 0).unwrap();
    old.connect(exp, out, 0).unwrap();
    let mut cur = old.clone();
    assert_eq!(revisions::compare_lines(&cur, &old, &[]), ["This graph is the same as the current one."]);
    // Moving a node is not a difference.
    cur.node_mut(exp).unwrap().pos = [250.0, 40.0];
    assert_eq!(revisions::compare_lines(&cur, &old, &[]).len(), 1);
    cur.node_mut(exp).unwrap().set_param("ev", ImageId(0), 1.5);
    cur.add_node(NodeKind::Contrast, [0.0, 0.0]);
    cur.remove_node(src);
    let lines = revisions::compare_lines(&cur, &old, &[ImageId(1)]);
    let text = lines.join("\n");
    assert!(text.contains("Nodes that come back (1): Source"), "{text}");
    assert!(text.contains("Nodes that go away (1): Contrast"), "{text}");
    assert!(text.contains("Parameters that differ: 1 in 1 node(s)"), "{text}");
    assert!(text.contains("Exposure: 1 value(s)"), "{text}");
}

#[test]
fn the_comparison_is_never_longer_than_ten_lines() {
    let mut old = Graph::new();
    let ids: Vec<NodeId> = (0..25).map(|i| old.add_node(NodeKind::Exposure, [i as f32 * 10.0, 0.0])).collect();
    let mut cur = old.clone();
    for id in &ids {
        cur.node_mut(*id).unwrap().set_param("ev", ImageId(0), 2.0);
    }
    cur.add_node(NodeKind::Contrast, [0.0, 0.0]);
    let lines = revisions::compare_lines(&cur, &old, &[]);
    assert!(lines.len() <= revisions::MAX_COMPARE_LINES, "{lines:?}");
    assert!(lines.last().unwrap().contains("more node(s)"), "{lines:?}");
}

/// A session at revision 2 with a history of the graph: (session, history stack, file, graph of revision 1).
fn restore_setup(dir: &Path) -> (Session, History, PathBuf, Graph) {
    let file = dir.join("r.rawgraph");
    let mut s = session_with_images(dir, 3);
    let mut hist = History::new(&s.project.graph);
    add(&mut s, NodeKind::Exposure);
    hist.commit_if_changed(&s.project.graph, false);
    commit(&mut s, &file);
    let rev1 = s.project.graph.clone();
    add(&mut s, NodeKind::Contrast);
    add(&mut s, NodeKind::Blur);
    hist.commit_if_changed(&s.project.graph, false);
    commit(&mut s, &file);
    (s, hist, file, rev1)
}

#[test]
fn restoring_is_one_undo_step_and_leaves_ratings_flags_and_images_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut hist, _file, rev1) = restore_setup(dir.path());
    let ids: Vec<ImageId> = s.visible().to_vec();
    s.set_rating(ids[0], 5);
    s.set_flag(ids[1], Flag::Reject);
    let before = s.project.graph.clone();
    let images_before = s.project.images().to_vec();
    let steps = hist.undo_len();
    s.dirty = false;

    assert_eq!(revisions::restore_graph(&mut s, &mut hist, 1), Ok(true));
    assert_eq!(s.project.graph, rev1);
    assert!(s.dirty, "the project is changed");
    assert_eq!(hist.undo_len(), steps + 1, "exactly one step");
    assert_eq!(s.project.images(), &images_before[..], "ratings, flags and the image list are as they were");
    // A second call with the same target changes nothing.
    assert_eq!(revisions::restore_graph(&mut s, &mut hist, 1), Ok(false));
    assert_eq!(hist.undo_len(), steps + 1);
    // Undo brings the previous graph back in one go.
    assert_eq!(hist.undo(), Some(before));
}

#[test]
fn restoring_a_revision_that_was_not_kept_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut hist, _file, _) = restore_setup(dir.path());
    let g = s.project.graph.clone();
    let err = revisions::restore_graph(&mut s, &mut hist, 99).unwrap_err();
    assert!(err.contains("revision 99"), "{err}");
    assert_eq!(s.project.graph, g);
}

#[test]
fn a_restored_graph_with_values_for_images_that_are_gone_still_renders() {
    use rawgraph_engine::{CancelToken, Evaluator};
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("m.rawgraph");
    let mut s = session_with_images(dir.path(), 2);
    let mut hist = History::new(&s.project.graph);
    // Revision 1: an Exposure node (per image) with a value for image 999, which does not exist.
    let exp = add(&mut s, NodeKind::Exposure);
    {
        let node = s.project.graph.node_mut(exp).unwrap();
        node.mode = Mode::Individual;
        assert!(node.set_param("ev", ImageId(999), 1.0));
    }
    // It sits between Source and Output like a real graph.
    let out = s.output_node().unwrap();
    s.project.graph.disconnect(out, 0);
    let src = s.project.graph.nodes().find(|n| n.kind == NodeKind::Source).unwrap().id;
    s.project.graph.connect(src, exp, 0).unwrap();
    s.project.graph.connect(exp, out, 0).unwrap();
    s.touch();
    hist.commit_if_changed(&s.project.graph, false);
    commit(&mut s, &file);
    // Later the graph changed.
    s.project.graph.remove_node(exp);
    s.touch();
    hist.commit_if_changed(&s.project.graph, false);
    s.selected_node = Some(exp);

    assert_eq!(revisions::restore_graph(&mut s, &mut hist, 1), Ok(true));
    assert_eq!(s.project.graph.node(exp).unwrap().param("ev", ImageId(999)), 1.0);
    s.sanitize();
    let image = s.current_image();
    let frame = Evaluator::new().eval_image(&s.project.graph, out, image, &*s.provider, &CancelToken::new());
    assert!(frame.is_ok(), "{:?}", frame.err());
    assert!(s.project.graph.validate().is_empty());
}

// ------------------------------------------------------------------ C2 / CLI

#[test]
fn the_new_cli_flags_parse() {
    let cli = crate::cli::parse_args("--open-history --select-revision 2 --simulate-conflict --save-project a.rawgraph".split(' ').map(str::to_owned)).unwrap();
    assert!(cli.open_history && cli.simulate_conflict);
    assert_eq!(cli.select_revision, Some(2));
    assert!(crate::cli::parse_args(["--select-revision".to_owned(), "x".to_owned()].into_iter()).is_err());
}

#[test]
fn the_export_report_goes_to_standard_error_only_when_nobody_else_prints_it() {
    let plain = crate::cli::Cli::default();
    assert!(crate::app::report_on_stderr(&plain));
    let run = crate::cli::Cli { run_export_node: Some(3), ..Default::default() };
    assert!(!crate::app::report_on_stderr(&run), "--run-export-node prints it once itself");
}

// ------------------------------------------------------------------ C3: the end of a long path

#[test]
fn a_long_output_folder_shows_its_end_in_the_field() {
    use eframe::egui;
    let folder = "D:/Photos/2026/Smith_Jones_Hochzeit/Lieferung_final/export_web_2400px";
    let ctx = egui::Context::default();
    let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(500.0, 200.0))), ..Default::default() };
    let mut texts = Vec::new();
    for _ in 0..2 {
        let out = ctx.run_ui(input.clone(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let key = ui.make_persistent_id("c3-field");
                crate::node_ui::text_field(ui, key, folder, 170.0, "not set");
            });
        });
        texts.clear();
        fn walk(s: &egui::Shape, into: &mut Vec<String>) {
            match s {
                egui::Shape::Text(t) => into.push(t.galley.text().to_owned()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, into)),
                _ => {}
            }
        }
        for c in &out.shapes {
            walk(&c.shape, &mut texts);
        }
        out.drop_without_applying_deltas();
    }
    let shown = texts.iter().find(|t| t.contains("export_web")).cloned().unwrap_or_default();
    assert!(shown.starts_with('\u{2026}') && shown.ends_with("export_web_2400px"), "{texts:?}");
    assert!(!shown.contains("Photos"), "{shown}");
}


// ------------------------------------------------------------------ unfinished saves (journal of an interrupted save)

/// Serialises the tests that switch "always rewrite the whole file" (a process-wide setting) and
/// switches it back at the end.
static MODE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Whole;
impl Whole {
    fn on() -> (std::sync::MutexGuard<'static, ()>, Whole) {
        let guard = MODE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        rawgraph_project::set_always_rewrite(true);
        (guard, Whole)
    }
}
impl Drop for Whole {
    fn drop(&mut self) {
        rawgraph_project::set_always_rewrite(false);
    }
}

/// A saved project with the journal of an interrupted save beside it.
fn project_with_journal(dir: &Path) -> (PathBuf, Project) {
    let mut s = session_with_images(dir, 3);
    let file = dir.join("p.rawgraph");
    commit(&mut s, &file);
    std::fs::write(dir.join("p.rawgraph-journal"), vec![0xA5u8; 2048]).unwrap();
    (file, s.project.clone())
}

#[test]
fn a_project_with_a_journal_on_a_local_drive_is_recovered_when_it_is_opened() {
    let _lock = MODE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let (file, saved) = project_with_journal(dir.path());
    assert!(rawgraph_project::write_mode(&file).mode == rawgraph_project::WriteMode::InPlace, "the temp folder is an ordinary local folder");
    let opened = revisions::open_project_ex(&file).unwrap();
    assert!(opened.recovered);
    assert_eq!(opened.note, Some(revisions::RECOVERED_NOTE));
    assert_eq!(opened.project, saved);
    assert!(!dir.path().join("p.rawgraph-journal").exists());
    // The session path (--project) says the same.
    std::fs::write(dir.path().join("p.rawgraph-journal"), vec![0xA5u8; 2048]).unwrap();
    let mut s = Session::new(Project::new(), provider());
    assert_eq!(s.load_project(&file).unwrap(), Some(revisions::RECOVERED_NOTE));
    assert_eq!(s.project, saved);
}

#[test]
fn a_project_with_a_journal_on_a_shared_place_asks_first_and_opens_after_the_recovery() {
    let (_lock, _whole) = Whole::on();
    let dir = tempfile::tempdir().unwrap();
    let (file, saved) = project_with_journal(dir.path());
    let err = revisions::open_project_ex(&file).err().expect("must not open");
    assert_eq!(err, revisions::OpenError::Unfinished(file.clone()));
    assert!(err.to_string().contains("unfinished save") && err.to_string().contains("no other computer is saving"), "{err}");
    assert!(dir.path().join("p.rawgraph-journal").exists(), "nothing was touched");
    let mut s = Session::new(Project::new(), provider());
    assert!(matches!(s.load_project(&file), Err(revisions::OpenError::Unfinished(_))));
    assert_eq!(s.project_path, None, "the open project stays as it was");
    // The user says "Recover unfinished save": the dialog calls this and retries.
    assert!(Project::recover_unfinished_save(&file).unwrap());
    let opened = revisions::open_project_ex(&file).unwrap();
    assert!(!opened.recovered && opened.project == saved);
}

#[test]
fn saving_onto_a_shared_file_with_a_journal_asks_and_goes_through_after_the_recovery() {
    let (_lock, _whole) = Whole::on();
    let dir = tempfile::tempdir().unwrap();
    let (file, _) = project_with_journal(dir.path());
    assert!(Project::recover_unfinished_save(&file).unwrap());
    let mut s = Session::new(Project::load(&file).unwrap(), provider());
    s.project_path = Some(file.clone());
    add(&mut s, NodeKind::Contrast);
    // A new interrupted save appears beside the file while the project is open.
    std::fs::write(dir.path().join("p.rawgraph-journal"), vec![0xA5u8; 2048]).unwrap();
    let mut flow: SaveFlow<Act> = SaveFlow::default();
    let out = flow.save(&mut s, &file, Some(Act::Quit));
    assert!(matches!(&out, Outcome::Unfinished { target, then: Some(Act::Quit) } if target == &file), "{out:?}");
    assert!(s.dirty, "nothing was saved");
    assert!(Project::recover_unfinished_save(&file).unwrap());
    match flow.save(&mut s, &file, Some(Act::Quit)) {
        Outcome::Saved { revision: 2, then, .. } => assert_eq!(then, Some(Act::Quit)),
        other => panic!("{other:?}"),
    }
    assert!(!s.dirty);
    assert_eq!(s.project.last_write().map(|w| w.mode), Some(rawgraph_project::WriteMode::FullRewrite), "what really happened is known for the status line");
}

#[test]
fn the_status_line_says_how_the_file_was_really_written() {
    let _lock = MODE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let mut s = session_with_images(dir.path(), 2);
    let file = dir.path().join("p.rawgraph");
    commit(&mut s, &file);
    let first = s.project.last_write().unwrap().describe();
    assert!(first.starts_with("whole file rewritten") && first.contains("new file"), "{first}");
    add(&mut s, NodeKind::Contrast);
    commit(&mut s, &file);
    assert!(s.project.last_write().unwrap().describe().starts_with("in place"));
}
