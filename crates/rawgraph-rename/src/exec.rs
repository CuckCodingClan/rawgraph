//! Carrying out a rename safely.
//!
//! Every run goes through the same steps, for a rename and for its undo:
//! 1. Check again that every old file is there with the size the plan saw, that no new name is taken
//!    by a file outside the run and that nothing leaves its folder.
//! 2. Write the journal (state `Started`) before the first file is touched.
//! 3. Phase 1: every file gets a unique temporary name in its own folder. Phase 2: every temporary
//!    name becomes the final name. Chains (a->b, b->c) and swaps (a<->b) work because no final name
//!    is written before all old names are out of the way.
//! 4. Verify: every new name exists with the old size, every old name is gone, no temporary name is left.
//! 5. A failure in phase 1, phase 2 or the verification puts every file back (best effort; whatever
//!    could not be put back is reported by name) and the journal says so.
//!
//! Files never change folder and never leave the volume: a rename inside one folder only.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::log::{new_run_id, now_ms, LogEntry, RenameLog, RunKind, RunRecord, RunState};
use crate::names::key;
use crate::plan::{Move, Plan};

/// The file system operations of a run (replaceable in tests to inject a failure).
pub trait Fs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn size(&self, path: &Path) -> io::Result<u64>;
    /// Whether anything (file, folder, link) is at `path`.
    fn exists(&self, path: &Path) -> bool;
    /// The exact file names in `dir`.
    fn list(&self, dir: &Path) -> io::Result<HashSet<OsString>>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealFs;

impl Fs for RealFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn size(&self, path: &Path) -> io::Result<u64> {
        let m = std::fs::metadata(path)?;
        if m.is_file() {
            Ok(m.len())
        } else {
            Err(io::Error::other("not a file"))
        }
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    fn list(&self, dir: &Path) -> io::Result<HashSet<OsString>> {
        Ok(std::fs::read_dir(dir)?.flatten().map(|e| e.file_name()).collect())
    }
}

/// A failure after files were touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFailure {
    pub message: String,
    /// Every file is back under its old name.
    pub rolled_back: bool,
    /// Files that could not be put back, each with where it is now.
    pub rollback_errors: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// Refused before anything was touched (conflicts, bad move).
    Blocked(String),
    /// A file is not what the plan saw (changed, gone). Nothing was touched.
    Stale(String),
    /// Something failed while files were being renamed.
    Failed(RunFailure),
    /// The undo log could not be written; nothing was touched.
    Log(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Blocked(m) | RunError::Stale(m) => f.write_str(m),
            RunError::Log(m) => write!(f, "the undo log could not be written, nothing was renamed: {m}"),
            RunError::Failed(r) => {
                write!(f, "{}", r.message)?;
                if r.rolled_back {
                    write!(f, " Everything was put back under its old name.")?;
                } else {
                    write!(f, " NOT everything could be put back: {}", r.rollback_errors.join("; "))?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RunError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunReport {
    pub run_id: String,
    pub moved: Vec<Move>,
    /// The rename was done and verified but the undo log could not be saved afterwards.
    pub log_warning: Option<String>,
}

fn parent_of(p: &Path) -> PathBuf {
    p.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn temp_path(old: &Path, run_id: &str, index: usize) -> PathBuf {
    parent_of(old).join(format!(".rgrename-{run_id}-{index}.tmp"))
}

fn name_of(p: &Path) -> OsString {
    p.file_name().map(OsString::from).unwrap_or_default()
}

/// Checks before the first file is touched.
fn preflight(moves: &[Move], fs: &dyn Fs) -> Result<(), RunError> {
    let old_keys: HashSet<String> = moves.iter().map(|m| key(&m.old)).collect();
    let mut new_keys: HashSet<String> = HashSet::new();
    for m in moves {
        if m.old == m.new {
            return Err(RunError::Blocked(format!("{} is listed with an unchanged name", m.old.display())));
        }
        if parent_of(&m.old) != parent_of(&m.new) || m.new.file_name().is_none() {
            return Err(RunError::Blocked(format!("{} would leave its folder; renaming never moves files between folders", m.old.display())));
        }
        if !new_keys.insert(key(&m.new)) {
            return Err(RunError::Blocked(format!("two files would both be named {}", m.new.display())));
        }
        match fs.size(&m.old) {
            Ok(s) if s == m.size => {}
            Ok(s) => return Err(RunError::Stale(format!("{} changed since the preview (size {s} instead of {} bytes); make a new preview", m.old.display(), m.size))),
            Err(e) => return Err(RunError::Stale(format!("{} cannot be read any more ({e}); make a new preview", m.old.display()))),
        }
        if fs.exists(&m.new) && !old_keys.contains(&key(&m.new)) {
            return Err(RunError::Blocked(format!("{} exists already and is not part of this rename", m.new.display())));
        }
    }
    Ok(())
}

fn fail(message: String, moves: &[Move], temps: &[PathBuf], p2_done: usize, p1_done: usize, fs: &dyn Fs) -> RunError {
    let errors = rollback(moves, temps, p2_done, p1_done, fs);
    RunError::Failed(RunFailure { message, rolled_back: errors.is_empty(), rollback_errors: errors })
}

/// Undoes phase 2 for the first `p2_done` moves and phase 1 for the first `p1_done`. Returns what failed.
fn rollback(moves: &[Move], temps: &[PathBuf], p2_done: usize, p1_done: usize, fs: &dyn Fs) -> Vec<String> {
    let mut errors = Vec::new();
    let mut stuck: HashSet<usize> = HashSet::new();
    for i in (0..p2_done).rev() {
        if let Err(e) = fs.rename(&moves[i].new, &temps[i]) {
            stuck.insert(i);
            errors.push(format!("{} is now named {} (taking it back failed: {e})", moves[i].old.display(), moves[i].new.display()));
        }
    }
    for i in (0..p1_done).rev() {
        if stuck.contains(&i) {
            continue;
        }
        if fs.exists(&moves[i].old) {
            errors.push(format!("{} is now named {} (the old name is occupied)", moves[i].old.display(), temps[i].display()));
            continue;
        }
        if let Err(e) = fs.rename(&temps[i], &moves[i].old) {
            errors.push(format!("{} is now named {} (taking it back failed: {e})", moves[i].old.display(), temps[i].display()));
        }
    }
    errors
}

/// The two-phase rename with verification. `Ok` only when everything is renamed and verified.
pub fn run_moves(moves: &[Move], run_id: &str, fs: &dyn Fs) -> Result<(), RunError> {
    if moves.is_empty() {
        return Ok(());
    }
    preflight(moves, fs)?;
    let temps: Vec<PathBuf> = moves.iter().enumerate().map(|(i, m)| temp_path(&m.old, run_id, i)).collect();
    if let Some(t) = temps.iter().find(|t| fs.exists(t)) {
        return Err(RunError::Blocked(format!("the temporary name {} is already in use", t.display())));
    }
    // Phase 1: out of the way.
    for i in 0..moves.len() {
        if let Err(e) = fs.rename(&moves[i].old, &temps[i]) {
            return Err(fail(format!("Renaming {} failed in phase 1: {e}.", moves[i].old.display()), moves, &temps, 0, i, fs));
        }
    }
    // Phase 2: into place.
    for i in 0..moves.len() {
        if fs.exists(&moves[i].new) {
            return Err(fail(format!("{} appeared during the rename.", moves[i].new.display()), moves, &temps, i, moves.len(), fs));
        }
        if let Err(e) = fs.rename(&temps[i], &moves[i].new) {
            return Err(fail(format!("Renaming to {} failed in phase 2: {e}.", moves[i].new.display()), moves, &temps, i, moves.len(), fs));
        }
    }
    // Verification.
    let problems = verify(moves, &temps, fs);
    if !problems.is_empty() {
        let shown: Vec<&str> = problems.iter().take(5).map(String::as_str).collect();
        return Err(fail(format!("Verification failed ({} problem(s): {}).", problems.len(), shown.join("; ")), moves, &temps, moves.len(), moves.len(), fs));
    }
    Ok(())
}

fn verify(moves: &[Move], temps: &[PathBuf], fs: &dyn Fs) -> Vec<String> {
    let mut problems = Vec::new();
    let mut listings: HashMap<PathBuf, Option<HashSet<OsString>>> = HashMap::new();
    let new_names: HashSet<OsString> = moves.iter().map(|m| name_of(&m.new)).collect();
    for (i, m) in moves.iter().enumerate() {
        let dir = parent_of(&m.new);
        let listing = listings.entry(dir.clone()).or_insert_with(|| fs.list(&dir).ok());
        let Some(names) = listing else {
            problems.push(format!("the folder {} cannot be listed", dir.display()));
            continue;
        };
        if !names.contains(&name_of(&m.new)) {
            problems.push(format!("{} does not exist", m.new.display()));
        }
        // An old name may be in use again as somebody else's new name (a chain or a swap).
        if names.contains(&name_of(&m.old)) && !new_names.contains(&name_of(&m.old)) {
            problems.push(format!("{} still exists", m.old.display()));
        }
        if fs.exists(&temps[i]) {
            problems.push(format!("the temporary file {} is left over", temps[i].display()));
        }
        match fs.size(&m.new) {
            Ok(s) if s == m.size => {}
            Ok(s) => problems.push(format!("{} has {s} bytes instead of {}", m.new.display(), m.size)),
            Err(e) => problems.push(format!("{} cannot be read: {e}", m.new.display())),
        }
    }
    problems
}

fn finish_log(log_path: Option<&Path>, log: &mut RenameLog, idx: usize, state: RunState) -> Option<String> {
    let path = log_path?;
    log.runs[idx].state = state;
    log.save(path).err()
}

fn run_logged(moves: &[Move], fs: &dyn Fs, log_path: Option<&Path>, label: &str, kind: RunKind) -> Result<RunReport, RunError> {
    let run_id = new_run_id();
    let mut log = RenameLog::default();
    let mut idx = 0;
    if let Some(path) = log_path {
        log = RenameLog::load(path).map_err(RunError::Log)?;
        log.runs.push(RunRecord {
            id: run_id.clone(),
            started_unix_ms: now_ms(),
            label: label.to_owned(),
            kind,
            state: RunState::Started,
            entries: moves.iter().map(|m| LogEntry { old: m.old.clone(), new: m.new.clone(), size: m.size }).collect(),
        });
        idx = log.runs.len() - 1;
        log.save(path).map_err(RunError::Log)?;
    }
    match run_moves(moves, &run_id, fs) {
        Ok(()) => {
            let warn = finish_log(log_path, &mut log, idx, RunState::Done);
            Ok(RunReport { run_id, moved: moves.to_vec(), log_warning: warn.map(|e| format!("the rename is done but the undo log could not be saved: {e}")) })
        }
        Err(e) => {
            // Anything but a failed rollback leaves every file under its old name.
            let state = match &e {
                RunError::Failed(f) if !f.rolled_back => RunState::Started,
                _ => RunState::RolledBack,
            };
            let _ = finish_log(log_path, &mut log, idx, state);
            Err(e)
        }
    }
}

/// Renames every changed file of a plan without conflicts. The undo log (when given) is written first.
pub fn execute(plan: &Plan, fs: &dyn Fs, log_path: Option<&Path>, label: &str) -> Result<RunReport, RunError> {
    if plan.blocked() {
        return Err(RunError::Blocked(format!("the preview has {} file(s) with conflicts; nothing was renamed", plan.problem_count())));
    }
    let moves = plan.moves();
    if moves.is_empty() {
        return Err(RunError::Blocked("no file would get a new name".into()));
    }
    run_logged(&moves, fs, log_path, label, RunKind::Rename)
}

/// Reverses the last finished rename of the log the same way it was done, with the same checks.
pub fn undo_last(log_path: &Path, fs: &dyn Fs) -> Result<RunReport, RunError> {
    let log = RenameLog::load(log_path).map_err(RunError::Log)?;
    let Some(pos) = log.runs.iter().rposition(|r| r.kind == RunKind::Rename && r.state == RunState::Done) else {
        return Err(RunError::Blocked("there is no finished rename to undo".into()));
    };
    let moves: Vec<Move> = log.runs[pos].entries.iter().map(|e| Move { old: e.new.clone(), new: e.old.clone(), size: e.size }).collect();
    let label = format!("undo of {}", log.runs[pos].label);
    let mut report = run_logged(&moves, fs, Some(log_path), &label, RunKind::Undo)?;
    // The undo wrote its own record; mark the original one on a fresh copy of the log.
    match RenameLog::load(log_path) {
        Ok(mut fresh) => {
            if let Some(r) = fresh.runs.iter_mut().find(|r| r.id == log.runs[pos].id) {
                r.state = RunState::Undone;
            }
            if let Err(e) = fresh.save(log_path) {
                report.log_warning = Some(format!("the undo is done but the log could not be saved: {e}"));
            }
        }
        Err(e) => report.log_warning = Some(format!("the undo is done but the log could not be read: {e}")),
    }
    Ok(report)
}

/// Finishes the clean-up of a run the program did not get to finish (power loss, crash): every file of a
/// `Started` run is put back under its old name. Returns one line per file that was moved.
pub fn recover_interrupted(log_path: &Path, fs: &dyn Fs) -> Result<Vec<String>, RunError> {
    let mut log = RenameLog::load(log_path).map_err(RunError::Log)?;
    let mut lines = Vec::new();
    for run in log.runs.iter_mut().filter(|r| r.state == RunState::Started) {
        let mut stuck = false;
        for (i, e) in run.entries.iter().enumerate() {
            let temp = temp_path(&e.old, &run.id, i);
            let from = if fs.exists(&temp) {
                Some(temp)
            } else if !fs.exists(&e.old) && fs.exists(&e.new) {
                Some(e.new.clone())
            } else {
                None
            };
            if let Some(from) = from {
                if fs.exists(&e.old) {
                    lines.push(format!("{} could not be put back: {} is occupied", from.display(), e.old.display()));
                    stuck = true;
                    continue;
                }
                match fs.rename(&from, &e.old) {
                    Ok(()) => lines.push(format!("{} -> {}", from.display(), e.old.display())),
                    Err(err) => {
                        lines.push(format!("{} could not be put back: {err}", from.display()));
                        stuck = true;
                    }
                }
            }
        }
        if !stuck {
            run.state = RunState::RolledBack;
        }
    }
    log.save(log_path).map_err(RunError::Log)?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::tests::{src, touch};
    use crate::plan::{build, PlanOptions};
    use crate::rules::RenameRules;
    use std::cell::Cell;

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    fn mv(old: &Path, new: &Path) -> Move {
        Move { old: old.to_path_buf(), new: new.to_path_buf(), size: std::fs::metadata(old).unwrap().len() }
    }

    /// Fails the n-th rename call (counting from 1) and passes everything else through.
    struct FailAt {
        n: usize,
        calls: Cell<usize>,
        /// Also fail the calls of the rollback (so something stays stuck).
        stay_broken: bool,
    }

    impl Fs for FailAt {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            let c = self.calls.get() + 1;
            self.calls.set(c);
            if c == self.n || (self.stay_broken && c > self.n) {
                return Err(io::Error::other("injected failure"));
            }
            RealFs.rename(from, to)
        }
        fn size(&self, p: &Path) -> io::Result<u64> {
            RealFs.size(p)
        }
        fn exists(&self, p: &Path) -> bool {
            RealFs.exists(p)
        }
        fn list(&self, d: &Path) -> io::Result<HashSet<OsString>> {
            RealFs.list(d)
        }
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn a_chain_and_a_swap_resolve_through_temp_names() {
        let t = tempfile::tempdir().unwrap();
        let (a, b, c) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"), touch(t.path(), "c.jpg", "CCC"));
        // chain a->b, b->c, c->d
        let d = t.path().join("d.jpg");
        run_moves(&[mv(&a, &b), mv(&b, &c), mv(&c, &d)], "t1", &RealFs).unwrap();
        assert_eq!((read(&b), read(&c), read(&d)), ("A".into(), "BB".into(), "CCC".into()));
        assert_eq!(listing(t.path()), vec!["b.jpg", "c.jpg", "d.jpg"], "no temp file is left");
        // swap b<->c
        run_moves(&[mv(&b, &c), mv(&c, &b)], "t2", &RealFs).unwrap();
        assert_eq!((read(&b), read(&c)), ("BB".into(), "A".into()));
        // case-only change
        let up = t.path().join("B.JPG");
        run_moves(&[mv(&b, &up)], "t3", &RealFs).unwrap();
        assert!(listing(t.path()).contains(&"B.JPG".to_string()));
        assert!(!listing(t.path()).contains(&"b.jpg".to_string()));
    }

    #[test]
    fn plan_chain_executes_end_to_end() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "1.jpg", "one");
        let b = touch(t.path(), "2.jpg", "two");
        // Counter start 2: 1->2, 2->3 (a chain through an existing name of the set).
        let r = RenameRules { template: "{counter}".into(), counter: crate::rules::Counter { start: 2, step: 1, pad: 1 }, ..Default::default() };
        let plan = build(&[src(&a, 1), src(&b, 2)], &r, None, &PlanOptions::default()).unwrap();
        assert!(!plan.blocked());
        let rep = execute(&plan, &RealFs, None, "chain").unwrap();
        assert_eq!(rep.moved.len(), 2);
        assert_eq!((read(&t.path().join("2.jpg")), read(&t.path().join("3.jpg"))), ("one".into(), "two".into()));
    }

    #[test]
    fn sidecars_and_pairs_are_renamed_with_their_file() {
        let t = tempfile::tempdir().unwrap();
        let raw = touch(t.path(), "IMG_1.CR2", "raw");
        touch(t.path(), "IMG_1.xmp", "x");
        touch(t.path(), "IMG_1.jpg", "jpeg");
        let r = RenameRules { template: "Job_001".into(), ..Default::default() };
        let plan = build(&[src(&raw, 1)], &r, None, &PlanOptions { pairs: true }).unwrap();
        execute(&plan, &RealFs, None, "x").unwrap();
        assert_eq!(listing(t.path()), vec!["Job_001.CR2", "Job_001.jpg", "Job_001.xmp"]);
        assert_eq!(read(&t.path().join("Job_001.xmp")), "x");
    }

    #[test]
    fn a_blocked_plan_does_not_touch_anything() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "A");
        let b = touch(t.path(), "b.jpg", "B");
        touch(t.path(), "same.jpg", "S");
        let plan = build(&[src(&a, 1), src(&b, 2)], &RenameRules { template: "same".into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
        let before = listing(t.path());
        let err = execute(&plan, &RealFs, None, "x").unwrap_err();
        assert!(matches!(err, RunError::Blocked(_)), "{err}");
        assert_eq!(listing(t.path()), before);
    }

    #[test]
    fn nothing_leaves_its_folder_or_overwrites() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("sub")).unwrap();
        let a = touch(t.path(), "a.jpg", "A");
        let other = touch(t.path(), "other.jpg", "O");
        let err = run_moves(&[mv(&a, &t.path().join("sub").join("a.jpg"))], "x", &RealFs).unwrap_err();
        assert!(matches!(err, RunError::Blocked(m) if m.contains("leave its folder")));
        let err = run_moves(&[mv(&a, &other)], "x", &RealFs).unwrap_err();
        assert!(matches!(err, RunError::Blocked(m) if m.contains("exists already")));
        assert_eq!((read(&a), read(&other)), ("A".into(), "O".into()));
    }

    #[test]
    fn a_file_that_changed_since_the_preview_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "A");
        let m = mv(&a, &t.path().join("b.jpg"));
        std::fs::write(&a, "longer now").unwrap();
        assert!(matches!(run_moves(std::slice::from_ref(&m), "x", &RealFs), Err(RunError::Stale(_))));
        std::fs::remove_file(&a).unwrap();
        assert!(matches!(run_moves(&[m], "x", &RealFs), Err(RunError::Stale(_))));
    }

    #[test]
    fn a_failure_in_phase_two_rolls_everything_back() {
        let t = tempfile::tempdir().unwrap();
        let (a, b, c) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"), touch(t.path(), "c.jpg", "CCC"));
        let moves = [mv(&a, &t.path().join("x.jpg")), mv(&b, &t.path().join("y.jpg")), mv(&c, &t.path().join("z.jpg"))];
        // calls 1-3 = phase 1, 4 = first phase-2 rename, 5 = second (fails)
        let fs = FailAt { n: 5, calls: Cell::new(0), stay_broken: false };
        let err = run_moves(&moves, "inj", &fs).unwrap_err();
        match &err {
            RunError::Failed(f) => assert!(f.rolled_back && f.rollback_errors.is_empty() && f.message.contains("phase 2"), "{err}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg", "c.jpg"]);
        assert_eq!((read(&a), read(&b), read(&c)), ("A".into(), "BB".into(), "CCC".into()));
    }

    #[test]
    fn a_failure_in_phase_one_rolls_back_too() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let moves = [mv(&a, &t.path().join("x.jpg")), mv(&b, &t.path().join("y.jpg"))];
        let err = run_moves(&moves, "inj", &FailAt { n: 2, calls: Cell::new(0), stay_broken: false }).unwrap_err();
        assert!(matches!(&err, RunError::Failed(f) if f.rolled_back && f.message.contains("phase 1")), "{err}");
        assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg"]);
    }

    #[test]
    fn a_failed_rollback_names_every_file_that_is_stuck() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let moves = [mv(&a, &t.path().join("x.jpg")), mv(&b, &t.path().join("y.jpg"))];
        // phase 1 = calls 1,2; phase 2 call 3 fails and everything after fails too (broken disk).
        let err = run_moves(&moves, "inj", &FailAt { n: 3, calls: Cell::new(0), stay_broken: true }).unwrap_err();
        match &err {
            RunError::Failed(f) => {
                assert!(!f.rolled_back);
                assert_eq!(f.rollback_errors.len(), 2, "{:?}", f.rollback_errors);
                assert!(f.rollback_errors.iter().all(|e| e.contains(".rgrename-inj-")), "the temporary names are given: {:?}", f.rollback_errors);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn verification_failure_rolls_back() {
        // A file system that lies about the size after the rename.
        struct Liar(RealFs);
        impl Fs for Liar {
            fn rename(&self, f: &Path, t: &Path) -> io::Result<()> {
                self.0.rename(f, t)
            }
            fn size(&self, p: &Path) -> io::Result<u64> {
                let real = self.0.size(p)?;
                Ok(if p.file_name().is_some_and(|n| n == "b.jpg") { real + 1 } else { real })
            }
            fn exists(&self, p: &Path) -> bool {
                self.0.exists(p)
            }
            fn list(&self, d: &Path) -> io::Result<HashSet<OsString>> {
                self.0.list(d)
            }
        }
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "A");
        let m = Move { old: a.clone(), new: t.path().join("b.jpg"), size: 1 };
        let err = run_moves(&[m], "v", &Liar(RealFs)).unwrap_err();
        assert!(matches!(&err, RunError::Failed(f) if f.message.contains("Verification failed") && f.rolled_back), "{err}");
        assert_eq!(listing(t.path()), vec!["a.jpg"]);
    }

    #[test]
    fn undo_reverses_the_last_run_with_verification() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("job.rename-log.json");
        let dir = t.path().join("shots");
        std::fs::create_dir(&dir).unwrap();
        let a = touch(&dir, "IMG_1.jpg", "one");
        let b = touch(&dir, "IMG_2.jpg", "twoo");
        touch(&dir, "IMG_1.xmp", "x");
        let r = RenameRules { template: "{counter}".into(), counter: crate::rules::Counter { start: 2, step: -1, pad: 1 }, ..Default::default() };
        // IMG_1 -> 2, IMG_2 -> 1 (a swap of names with the sidecar)
        let plan = build(&[src(&a, 1), src(&b, 2)], &r, None, &PlanOptions::default()).unwrap();
        assert!(!plan.blocked());
        execute(&plan, &RealFs, Some(&log), "first").unwrap();
        assert_eq!(listing(&dir), vec!["1.jpg", "2.jpg", "2.xmp"]);
        assert_eq!(read(&dir.join("2.jpg")), "one");
        // the log has one finished run
        let l = RenameLog::load(&log).unwrap();
        assert_eq!((l.runs.len(), l.runs[0].state), (1, RunState::Done));
        let rep = undo_last(&log, &RealFs).unwrap();
        assert_eq!(rep.moved.len(), 3);
        assert_eq!(listing(&dir), vec!["IMG_1.jpg", "IMG_1.xmp", "IMG_2.jpg"]);
        assert_eq!((read(&a), read(&b)), ("one".into(), "twoo".into()));
        // nothing left to undo
        assert!(matches!(undo_last(&log, &RealFs), Err(RunError::Blocked(_))));
        let l = RenameLog::load(&log).unwrap();
        assert!(l.runs.iter().any(|r| r.state == RunState::Undone));
    }

    #[test]
    fn undo_refuses_when_files_changed_or_names_are_taken() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let a = touch(t.path(), "a.jpg", "one");
        let plan = build(&[src(&a, 1)], &RenameRules { template: "b".into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
        execute(&plan, &RealFs, Some(&log), "x").unwrap();
        // the old name is taken by someone else meanwhile
        touch(t.path(), "a.jpg", "intruder");
        let err = undo_last(&log, &RealFs).unwrap_err();
        assert!(matches!(&err, RunError::Blocked(m) if m.contains("exists already")), "{err}");
        assert_eq!(read(&t.path().join("a.jpg")), "intruder");
        std::fs::remove_file(t.path().join("a.jpg")).unwrap();
        // the renamed file was edited
        std::fs::write(t.path().join("b.jpg"), "edited").unwrap();
        assert!(matches!(undo_last(&log, &RealFs), Err(RunError::Stale(_))));
        std::fs::write(t.path().join("b.jpg"), "one").unwrap();
        undo_last(&log, &RealFs).unwrap();
        assert_eq!(read(&a), "one");
    }

    #[test]
    fn undo_goes_back_one_run_at_a_time() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let a = touch(t.path(), "a.jpg", "1");
        for (from, to) in [("a.jpg", "b.jpg"), ("b.jpg", "c.jpg")] {
            let plan = build(&[src(&t.path().join(from), 1)], &RenameRules { template: to.trim_end_matches(".jpg").into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
            execute(&plan, &RealFs, Some(&log), "step").unwrap();
        }
        assert_eq!(listing(t.path()), vec!["c.jpg", "l.json"]);
        undo_last(&log, &RealFs).unwrap();
        assert!(t.path().join("b.jpg").exists());
        undo_last(&log, &RealFs).unwrap();
        assert!(a.exists());
    }

    #[test]
    fn a_log_that_cannot_be_written_stops_before_any_file_is_touched() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "1");
        // The log path is a folder: loading/saving fails.
        let bad = t.path().join("dir.json");
        std::fs::create_dir(&bad).unwrap();
        let plan = build(&[src(&a, 1)], &RenameRules { template: "b".into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
        assert!(matches!(execute(&plan, &RealFs, Some(&bad), "x"), Err(RunError::Log(_))));
        assert!(a.exists());
    }

    #[test]
    fn an_interrupted_run_can_be_recovered_from_the_journal() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let moves = [mv(&a, &t.path().join("x.jpg")), mv(&b, &t.path().join("y.jpg"))];
        // Simulate a crash after phase 1 and one rename of phase 2: journal says Started.
        let mut l = RenameLog::default();
        l.runs.push(RunRecord {
            id: "crash".into(),
            started_unix_ms: 0,
            label: "x".into(),
            kind: RunKind::Rename,
            state: RunState::Started,
            entries: moves.iter().map(|m| LogEntry { old: m.old.clone(), new: m.new.clone(), size: m.size }).collect(),
        });
        l.save(&log).unwrap();
        std::fs::rename(&a, temp_path(&a, "crash", 0)).unwrap();
        std::fs::rename(&b, temp_path(&b, "crash", 1)).unwrap();
        std::fs::rename(temp_path(&a, "crash", 0), t.path().join("x.jpg")).unwrap();
        let lines = recover_interrupted(&log, &RealFs).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg", "l.json"]);
        assert_eq!((read(&a), read(&b)), ("A".into(), "BB".into()));
        assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::RolledBack);
    }
}
