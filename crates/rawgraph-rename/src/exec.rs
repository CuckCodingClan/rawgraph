//! Carrying out a rename safely.
//!
//! Every run goes through the same steps, for a rename and for its undo:
//! 1. Check again that every old file is there with the size the plan saw, that no new name is taken
//!    by a file outside the run and that nothing leaves its folder.
//! 2. Write the journal (state `Started`) before the first file is touched, flushed to disk.
//! 3. Phase 1: every file gets a unique temporary name in its own folder; the journal then says
//!    `Phase1Done`. Phase 2: every temporary name becomes the final name. Chains (a->b, b->c) and swaps
//!    (a<->b) work because no final name is written before all old names are out of the way.
//! 4. Verify: every new name exists with the old size, every old name is gone, no temporary name is left.
//! 5. A failure in phase 1, phase 2, the verification or a cancel puts every file back (best effort;
//!    whatever could not be put back is reported by name) and the journal says so.
//!
//! Every single rename is atomic and never replaces: `RealFs::rename` fails when something is at the
//! target, even when it appeared a moment after the check (`MoveFileExW` without REPLACE_EXISTING on
//! Windows, `renameat2(RENAME_NOREPLACE)` on Linux, a hard link plus unlink elsewhere). Files never change
//! folder and never leave the volume.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::log::{new_run_id, now_ms, LogEntry, RenameLog, RunKind, RunRecord, RunState};
use crate::names::key;
use crate::plan::{Move, Plan};

/// The file system operations of a run (replaceable in tests to inject a failure).
pub trait Fs {
    /// Renames without ever replacing: fails with `AlreadyExists` when `to` exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn size(&self, path: &Path) -> io::Result<u64>;
    /// Whether anything (file, folder, link) is at `path`.
    fn exists(&self, path: &Path) -> bool;
    /// The exact file names in `dir`.
    fn list(&self, dir: &Path) -> io::Result<HashSet<OsString>>;
    /// Modification time in ns since 1970 (kept by a rename), `None` when unknown.
    fn stamp(&self, _path: &Path) -> Option<u64> {
        None
    }
    /// When `keep` and `extra` are two names of the same file (a link-then-unlink rename that stopped
    /// half way), removes the name `extra` and returns true. Anything else: false, nothing touched.
    fn drop_duplicate_name(&self, _keep: &Path, _extra: &Path) -> io::Result<bool> {
        Ok(false)
    }
}

/// Renames with a few retries for passing errors (a virus scanner or sync client holding the file for a
/// moment); an occupied or missing name is final at once.
fn rename_retry(fs: &dyn Fs, from: &Path, to: &Path) -> io::Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        match fs.rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if matches!(e.kind(), io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound) => return Err(e),
            Err(e) => last = Some(e),
        }
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("rename failed")))
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RealFs;

#[cfg(windows)]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let wide = |p: &Path| -> Vec<u16> { p.as_os_str().encode_wide().chain(Some(0)).collect() };
    let (f, t) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated UTF-16 and live for the call. Flags 0: no replacing, no copy
    // across volumes.
    let ok = unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(f.as_ptr(), t.as_ptr(), 0) };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Hard link, then unlink the old name: never replaces. Used where no atomic no-replace rename exists.
#[cfg(not(windows))]
fn link_rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)?;
    std::fs::remove_file(from)
}

#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path contains a NUL byte"));
    let (f, t) = (c(from)?, c(to)?);
    // SAFETY: valid NUL-terminated paths for the duration of the call.
    let r = unsafe { libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE) };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) => link_rename(from, to),
        _ => Err(e),
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    link_rename(from, to)
}

impl Fs for RealFs {
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        rename_noreplace(from, to)
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

    fn stamp(&self, path: &Path) -> Option<u64> {
        let t = std::fs::metadata(path).ok()?.modified().ok()?;
        u64::try_from(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos()).ok()
    }

    #[cfg(unix)]
    fn drop_duplicate_name(&self, keep: &Path, extra: &Path) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let (a, b) = (std::fs::symlink_metadata(keep)?, std::fs::symlink_metadata(extra)?);
        if a.is_file() && b.is_file() && a.dev() == b.dev() && a.ino() == b.ino() {
            std::fs::remove_file(extra)?;
            return Ok(true);
        }
        Ok(false)
    }
}

/// Cancel flag and progress of a run, shared with the thread that shows them.
#[derive(Clone, Debug, Default)]
pub struct Control {
    /// Set to stop: the run puts every file back and ends with an error that says so.
    pub cancel: Arc<AtomicBool>,
    pub done: Arc<AtomicUsize>,
    pub total: Arc<AtomicUsize>,
}

impl Control {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn step(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
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
    /// Something failed (or the user cancelled) while files were being renamed.
    Failed(RunFailure),
    /// The undo log could not be read or written; nothing was touched.
    Log(String),
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RunError::Blocked(m) | RunError::Stale(m) => f.write_str(m),
            RunError::Log(m) => write!(f, "the undo log is unusable, nothing was renamed: {m}"),
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
    /// The rename is done and verified but the undo log could not be saved afterwards (it still says that the
    /// run was interrupted; "Recover" completes the book-keeping).
    pub log_warning: Option<String>,
}

/// What a check failure is called in messages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Preview,
    Undo,
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

/// Checks before the first file is touched. Returns the actual size of every file (the size check after the
/// rename compares against it).
fn preflight(moves: &[Move], fs: &dyn Fs, ctx: Ctx) -> Result<Vec<u64>, RunError> {
    let old_keys: HashSet<String> = moves.iter().map(|m| key(&m.old)).collect();
    let mut new_keys: HashSet<String> = HashSet::new();
    let mut sizes = Vec::with_capacity(moves.len());
    let when = if ctx == Ctx::Undo { "since the rename" } else { "since the preview" };
    let again = if ctx == Ctx::Undo { "undo cannot be done safely" } else { "make a new preview" };
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
            Ok(s) if s == m.size || !m.exact_size => sizes.push(s),
            Ok(s) => return Err(RunError::Stale(format!("{} changed {when} (size {s} instead of {} bytes); {again}", m.old.display(), m.size))),
            Err(e) => return Err(RunError::Stale(format!("{} cannot be read any more ({e}); {again}", m.old.display()))),
        }
        if fs.exists(&m.new) && !old_keys.contains(&key(&m.new)) {
            return Err(RunError::Blocked(format!("{} exists already and is not part of this rename", m.new.display())));
        }
    }
    Ok(sizes)
}

fn fail(message: String, moves: &[Move], temps: &[PathBuf], p2_done: usize, p1_done: usize, fs: &dyn Fs) -> RunError {
    let errors = rollback(moves, temps, p2_done, p1_done, fs);
    RunError::Failed(RunFailure { message, rolled_back: errors.is_empty(), rollback_errors: errors })
}

/// Undoes phase 2 for the first `p2_done` moves and phase 1 for the first `p1_done`. Returns what failed. Nothing
/// is ever replaced: an occupied name is reported instead.
fn rollback(moves: &[Move], temps: &[PathBuf], p2_done: usize, p1_done: usize, fs: &dyn Fs) -> Vec<String> {
    let mut errors = Vec::new();
    let mut stuck: HashSet<usize> = HashSet::new();
    for i in (0..p2_done).rev() {
        if let Err(e) = rename_retry(fs, &moves[i].new, &temps[i]) {
            stuck.insert(i);
            errors.push(format!("{} is now named {} (taking it back failed: {e})", moves[i].old.display(), moves[i].new.display()));
        }
    }
    for i in (0..p1_done).rev() {
        if stuck.contains(&i) {
            continue;
        }
        if let Err(e) = rename_retry(fs, &temps[i], &moves[i].old) {
            let why = if e.kind() == io::ErrorKind::AlreadyExists { "the old name is occupied".to_owned() } else { format!("taking it back failed: {e}") };
            errors.push(format!("{} is now named {} ({why})", moves[i].old.display(), temps[i].display()));
        }
    }
    errors
}

/// The two-phase rename with verification. `Ok` only when everything is renamed and verified.
pub fn run_moves(moves: &[Move], run_id: &str, fs: &dyn Fs) -> Result<(), RunError> {
    run_moves_ctx(moves, run_id, fs, Ctx::Preview, &Control::default(), &mut |_| Ok(()))
}

/// `journal` is told `Phase1Done` before the first final name is written (an error stops the run) and
/// `RollingBack` before anything is put back (best effort: recovery does not rely on it).
fn run_moves_ctx(moves: &[Move], run_id: &str, fs: &dyn Fs, ctx: Ctx, control: &Control, journal: &mut dyn FnMut(RunState) -> Result<(), String>) -> Result<(), RunError> {
    if moves.is_empty() {
        return Ok(());
    }
    let sizes = preflight(moves, fs, ctx)?;
    let temps: Vec<PathBuf> = moves.iter().enumerate().map(|(i, m)| temp_path(&m.old, run_id, i)).collect();
    if let Some(t) = temps.iter().find(|t| fs.exists(t)) {
        return Err(RunError::Blocked(format!("the temporary name {} is already in use", t.display())));
    }
    control.done.store(0, Ordering::Relaxed);
    control.total.store(moves.len() * 2, Ordering::Relaxed);
    // Phase 1: out of the way.
    for i in 0..moves.len() {
        if control.cancelled() {
            return Err({ let _ = journal(RunState::RollingBack); fail("Cancelled.".to_owned(), moves, &temps, 0, i, fs) });
        }
        if let Err(e) = fs.rename(&moves[i].old, &temps[i]) {
            return Err({ let _ = journal(RunState::RollingBack); fail(format!("Renaming {} failed in phase 1: {e}.", moves[i].old.display()), moves, &temps, 0, i, fs) });
        }
        control.step();
    }
    if let Err(e) = journal(RunState::Phase1Done) {
        return Err({ let _ = journal(RunState::RollingBack); fail(format!("The undo log could not be saved after phase 1 ({e})."), moves, &temps, 0, moves.len(), fs) });
    }
    // Phase 2: into place. Never replaces, even when a file appears after the check.
    for i in 0..moves.len() {
        if control.cancelled() {
            return Err({ let _ = journal(RunState::RollingBack); fail("Cancelled.".to_owned(), moves, &temps, i, moves.len(), fs) });
        }
        if let Err(e) = fs.rename(&temps[i], &moves[i].new) {
            let why = if e.kind() == io::ErrorKind::AlreadyExists { "a file with that name appeared meanwhile and was left untouched".to_owned() } else { e.to_string() };
            return Err({ let _ = journal(RunState::RollingBack); fail(format!("Renaming to {} failed in phase 2: {why}.", moves[i].new.display()), moves, &temps, i, moves.len(), fs) });
        }
        control.step();
    }
    // Verification.
    let problems = verify(moves, &temps, &sizes, fs);
    if !problems.is_empty() {
        let shown: Vec<&str> = problems.iter().take(5).map(String::as_str).collect();
        return Err({ let _ = journal(RunState::RollingBack); fail(format!("Verification failed ({} problem(s): {}).", problems.len(), shown.join("; ")), moves, &temps, moves.len(), moves.len(), fs) });
    }
    Ok(())
}

fn verify(moves: &[Move], temps: &[PathBuf], sizes: &[u64], fs: &dyn Fs) -> Vec<String> {
    use std::collections::HashMap;
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
            Ok(s) if s == sizes[i] => {}
            Ok(s) => problems.push(format!("{} has {s} bytes instead of {}", m.new.display(), sizes[i])),
            Err(e) => problems.push(format!("{} cannot be read: {e}", m.new.display())),
        }
    }
    problems
}

/// Saves the log, trying a few times (a sync client may hold the file for a moment).
fn save_retry(log: &RenameLog, path: &Path) -> Result<(), String> {
    let mut last = String::new();
    for i in 0..3 {
        match log.save(path) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
        if i < 2 {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    Err(last)
}

fn run_logged(moves: &[Move], fs: &dyn Fs, log_path: Option<&Path>, label: &str, kind: RunKind, control: &Control) -> Result<RunReport, RunError> {
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
            entries: moves
                .iter()
                .map(|m| LogEntry { old: m.old.clone(), new: m.new.clone(), size: m.size, exact_size: m.exact_size, stamp: if m.exact_size { fs.stamp(&m.old) } else { None } })
                .collect(),
        });
        idx = log.runs.len() - 1;
        log.save(path).map_err(RunError::Log)?;
    }
    let ctx = if kind == RunKind::Undo { Ctx::Undo } else { Ctx::Preview };
    let result = {
        let log_ref = &mut log;
        run_moves_ctx(moves, &run_id, fs, ctx, control, &mut |state| match log_path {
            Some(path) => {
                log_ref.runs[idx].state = state;
                if state == RunState::Phase1Done { log_ref.save(path) } else { save_retry(log_ref, path) }
            }
            None => Ok(()),
        })
    };
    match result {
        Ok(()) => {
            let mut warn = None;
            if let Some(path) = log_path {
                log.runs[idx].state = RunState::Done;
                warn = save_retry(&log, path).err();
            }
            Ok(RunReport {
                run_id,
                moved: moves.to_vec(),
                log_warning: warn.map(|e| format!("The files are renamed, but the undo log could not be saved ({e}). Use Recover in the Rename window to finish the book-keeping before you do anything else with these files.")),
            })
        }
        Err(e) => {
            // Anything but a failed rollback leaves every file under its old name; a failed one stays
            // `RollingBack` (or what the journal said last), so Recover looks at every file.
            if let Some(path) = log_path {
                if !matches!(&e, RunError::Failed(f) if !f.rolled_back) {
                    log.runs[idx].state = RunState::RolledBack;
                } else if log.runs[idx].state != RunState::RollingBack {
                    log.runs[idx].state = RunState::RollingBack;
                }
                let _ = save_retry(&log, path);
            }
            Err(e)
        }
    }
}

/// Renames every changed file of a plan without conflicts. The undo log (when given) is written first.
pub fn execute(plan: &Plan, fs: &dyn Fs, log_path: Option<&Path>, label: &str) -> Result<RunReport, RunError> {
    execute_with(plan, fs, log_path, label, &Control::default())
}

/// [`execute`] with a cancel flag and progress counters.
pub fn execute_with(plan: &Plan, fs: &dyn Fs, log_path: Option<&Path>, label: &str, control: &Control) -> Result<RunReport, RunError> {
    if plan.blocked() {
        return Err(RunError::Blocked(format!("the preview has {} file(s) with conflicts; nothing was renamed", plan.problem_count())));
    }
    let moves = plan.moves();
    if moves.is_empty() {
        return Err(RunError::Blocked("no file would get a new name".into()));
    }
    if let Some(path) = log_path {
        let n = RenameLog::load(path).map_err(RunError::Log)?.interrupted();
        if n > 0 {
            return Err(RunError::Blocked(format!("{n} interrupted rename(s) are in the log; use Recover first")));
        }
    }
    run_logged(&moves, fs, log_path, label, RunKind::Rename, control)
}

/// Reverses the last finished rename of the log the same way it was done, with the same checks.
pub fn undo_last(log_path: &Path, fs: &dyn Fs) -> Result<RunReport, RunError> {
    undo_last_with(log_path, fs, &Control::default())
}

pub fn undo_last_with(log_path: &Path, fs: &dyn Fs, control: &Control) -> Result<RunReport, RunError> {
    let log = RenameLog::load(log_path).map_err(RunError::Log)?;
    if log.interrupted() > 0 {
        return Err(RunError::Blocked("an interrupted rename is in the log; use Recover first".into()));
    }
    let Some(pos) = log.runs.iter().rposition(|r| r.kind == RunKind::Rename && r.state == RunState::Done) else {
        return Err(RunError::Blocked("there is no finished rename to undo".into()));
    };
    let moves: Vec<Move> = log.runs[pos].entries.iter().map(|e| Move { old: e.new.clone(), new: e.old.clone(), size: e.size, exact_size: e.exact_size }).collect();
    // (The undo's own record takes fresh stamps of the files as they are now.)
    let label = format!("undo of {}", log.runs[pos].label);
    let mut report = run_logged(&moves, fs, Some(log_path), &label, RunKind::Undo, control)?;
    // The undo wrote its own record; mark the original one on a fresh copy of the log.
    match RenameLog::load(log_path) {
        Ok(mut fresh) => {
            if let Some(r) = fresh.runs.iter_mut().find(|r| r.id == log.runs[pos].id) {
                r.state = RunState::Undone;
            }
            if let Err(e) = save_retry(&fresh, log_path) {
                report.log_warning = Some(format!("the undo is done but the log could not be saved: {e}"));
            }
        }
        Err(e) => report.log_warning = Some(format!("the undo is done but the log could not be read: {e}")),
    }
    Ok(report)
}

/// What recovery found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecoverReport {
    /// Moves for the project, in the direction "from the name the project may still have to the name the file
    /// has now": for a completed run old -> new, for a reverted run new -> old.
    pub relink: Vec<Move>,
    pub completed_runs: usize,
    pub reverted_runs: usize,
    pub lines: Vec<String>,
    /// Runs that could not be finished (files are named in `lines`).
    pub stuck_runs: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Loc {
    Old,
    Temp,
    New,
}

/// Decides from the journal and the disk what an interrupted run left, then either records it as completed
/// (journal `Phase1Done`, every file at its final name) or puts every file back through the temporary names
/// with the normal checks. Never a direct new -> old rename of a chain. Every file is looked for at its
/// temporary, old and new name whatever the journal says (a failed rollback leaves a mix); size and
/// modification time tell which file a name holds.
pub fn recover_interrupted(log_path: &Path, fs: &dyn Fs) -> Result<RecoverReport, RunError> {
    let mut log = RenameLog::load(log_path).map_err(RunError::Log)?;
    let mut rep = RecoverReport::default();
    for run in log.runs.iter_mut().filter(|r| matches!(r.state, RunState::Started | RunState::Phase1Done | RunState::RollingBack)) {
        let phase2 = run.state == RunState::Phase1Done;
        let temps: Vec<PathBuf> = run.entries.iter().enumerate().map(|(i, e)| temp_path(&e.old, &run.id, i)).collect();
        let holds = |p: &Path, e: &LogEntry| {
            fs.size(p).is_ok_and(|s| s == e.size || !e.exact_size)
                && match (e.stamp, fs.stamp(p)) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                }
        };
        let mut locs = Vec::new();
        let mut lost = Vec::new();
        let mut claimed: HashSet<PathBuf> = HashSet::new();
        for (i, e) in run.entries.iter().enumerate() {
            // A link-then-unlink rename that stopped half way leaves the temporary and one other name on the
            // same file: keep the other name.
            if fs.exists(&temps[i]) {
                for other in [&e.new, &e.old] {
                    if fs.exists(other) && fs.drop_duplicate_name(other, &temps[i]).unwrap_or(false) {
                        break;
                    }
                }
            }
            // In `Phase1Done` a file is more likely at its new name, otherwise at its old one.
            let order: [(Loc, &Path); 3] = if phase2 {
                [(Loc::Temp, &temps[i]), (Loc::New, &e.new), (Loc::Old, &e.old)]
            } else {
                [(Loc::Temp, &temps[i]), (Loc::Old, &e.old), (Loc::New, &e.new)]
            };
            match order.iter().find(|(_, p)| fs.exists(p) && !claimed.contains(*p) && holds(p, e)) {
                Some((loc, p)) => {
                    claimed.insert(p.to_path_buf());
                    locs.push(*loc);
                }
                None => {
                    lost.push(format!("{} cannot be found at {}, {} or {} with its logged size", e.old.display(), e.old.display(), temps[i].display(), e.new.display()));
                    locs.push(Loc::Old);
                }
            }
        }
        // Without modification times two files of the same size can be told apart only by the journal:
        // in a run that was being put back, a file that seems to be at its new name may be the other
        // file at its old one. Refuse to guess.
        if run.state == RunState::RollingBack && locs.contains(&Loc::New) && locs.contains(&Loc::Old) && run.entries.iter().any(|e| e.stamp.is_none()) {
            lost.push("the files of this run cannot be told apart safely (no modification times in the log); check them by hand".to_owned());
        }
        if !lost.is_empty() {
            rep.lines.extend(lost);
            rep.stuck_runs += 1;
            continue;
        }
        let moves_of = |reverse: bool| -> Vec<Move> {
            run.entries
                .iter()
                .map(|e| if reverse { Move { old: e.new.clone(), new: e.old.clone(), size: e.size, exact_size: e.exact_size } } else { Move { old: e.old.clone(), new: e.new.clone(), size: e.size, exact_size: e.exact_size } })
                .collect()
        };
        if locs.iter().all(|l| *l == Loc::Old) {
            run.state = RunState::RolledBack;
            rep.reverted_runs += 1;
            rep.relink.extend(moves_of(true));
            rep.lines.push(format!("{} file(s) of an interrupted run are under their old names; the run is recorded as put back", run.entries.len()));
            continue;
        }
        if phase2 && locs.iter().all(|l| *l == Loc::New) {
            run.state = RunState::Done;
            rep.completed_runs += 1;
            rep.relink.extend(moves_of(false));
            rep.lines.push(format!("{} file(s) of an interrupted run were already at their new names; the run is recorded as done", run.entries.len()));
            continue;
        }
        // Reverse: everything that is at its new name goes back to the temporary name first, then every temporary
        // name becomes the old name (no-replace renames; an occupied name is reported, never overwritten).
        let mut problems = Vec::new();
        for (i, e) in run.entries.iter().enumerate() {
            if locs[i] == Loc::New {
                match rename_retry(fs, &e.new, &temps[i]) {
                    Ok(()) => locs[i] = Loc::Temp,
                    Err(err) => problems.push(format!("{} could not be taken back ({err}); it is still named {}", e.old.display(), e.new.display())),
                }
            }
        }
        for (i, e) in run.entries.iter().enumerate() {
            if locs[i] == Loc::Temp {
                match rename_retry(fs, &temps[i], &e.old) {
                    Ok(()) => locs[i] = Loc::Old,
                    Err(err) => problems.push(format!("{} is now named {} ({err})", e.old.display(), temps[i].display())),
                }
            }
        }
        if problems.is_empty() && locs.iter().all(|l| *l == Loc::Old) {
            run.state = RunState::RolledBack;
            rep.reverted_runs += 1;
            rep.relink.extend(moves_of(true));
            rep.lines.push(format!("{} file(s) of an interrupted run were put back under their old names", run.entries.len()));
        } else {
            rep.stuck_runs += 1;
            rep.lines.extend(problems);
        }
    }
    save_retry(&log, log_path).map_err(RunError::Log)?;
    Ok(rep)
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
        Move { old: old.to_path_buf(), new: new.to_path_buf(), size: std::fs::metadata(old).unwrap().len(), exact_size: true }
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
        let m = Move { old: a.clone(), new: t.path().join("b.jpg"), size: 1, exact_size: true };
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

    /// Journal of a hand-made run `moves` in `state`.
    fn journal(log: &Path, id: &str, state: RunState, moves: &[Move]) {
        let mut l = RenameLog::default();
        l.runs.push(RunRecord {
            id: id.into(),
            started_unix_ms: 0,
            label: "x".into(),
            kind: RunKind::Rename,
            state,
            entries: moves.iter().map(|m| LogEntry { old: m.old.clone(), new: m.new.clone(), size: m.size, exact_size: true, stamp: None }).collect(),
        });
        l.save(log).unwrap();
    }

    #[test]
    fn the_rename_never_replaces_an_existing_file() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let err = RealFs.rename(&a, &b).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!((read(&a), read(&b)), ("A".into(), "BB".into()));
        RealFs.rename(&a, &t.path().join("c.jpg")).unwrap();
        assert_eq!(read(&t.path().join("c.jpg")), "A");
    }

    /// Creates a file at the target of a rename just before the rename (after every existence check).
    struct Racy {
        /// Rename targets that get an intruder first (once).
        appear_at: Vec<PathBuf>,
        /// The rename to this target fails.
        fail_to: Option<PathBuf>,
        fired: std::cell::RefCell<Vec<PathBuf>>,
    }

    impl Fs for Racy {
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            if self.fail_to.as_deref() == Some(to) {
                return Err(io::Error::other("injected failure"));
            }
            if self.appear_at.iter().any(|p| p == to) && !self.fired.borrow().iter().any(|p| p == to) {
                self.fired.borrow_mut().push(to.to_path_buf());
                std::fs::write(to, "INTRUDER").unwrap();
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

    #[test]
    fn a_file_that_appears_at_a_target_after_the_check_is_never_destroyed() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let (x, y) = (t.path().join("x.jpg"), t.path().join("y.jpg"));
        let fs = Racy { appear_at: vec![y.clone()], fail_to: None, fired: Default::default() };
        let err = run_moves(&[mv(&a, &x), mv(&b, &y)], "race", &fs).unwrap_err();
        assert!(matches!(&err, RunError::Failed(f) if f.rolled_back && f.message.contains("appeared meanwhile")), "{err}");
        assert_eq!(read(&y), "INTRUDER", "the newcomer is untouched");
        assert_eq!((read(&a), read(&b)), ("A".into(), "BB".into()), "the run's files are back");
        assert!(!x.exists());
    }

    #[test]
    fn a_file_that_appears_at_an_old_name_during_the_rollback_is_never_destroyed() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let (x, y) = (t.path().join("x.jpg"), t.path().join("y.jpg"));
        // Phase 2 fails for y; during the rollback something is written at a.jpg.
        let fs = Racy { appear_at: vec![a.clone()], fail_to: Some(y.clone()), fired: Default::default() };
        let err = run_moves(&[mv(&a, &x), mv(&b, &y)], "race2", &fs).unwrap_err();
        match &err {
            RunError::Failed(f) => {
                assert!(!f.rolled_back, "{err}");
                assert!(f.rollback_errors.iter().any(|e| e.contains("occupied") && e.contains(".rgrename-race2-0")), "{:?}", f.rollback_errors);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(read(&a), "INTRUDER");
        assert_eq!(read(&t.path().join(".rgrename-race2-0.tmp")), "A", "the file is kept under its temporary name, and reported");
        assert_eq!(read(&b), "BB");
    }

    /// Chain a->b, b->c, c->d interrupted at `p1` phase-1 renames (state Started), or after phase 1 with `p2`
    /// phase-2 renames (state Phase1Done). Returns the listing contents after recovery.
    fn crash(p1: usize, p2: Option<usize>) -> (tempfile::TempDir, PathBuf, RecoverReport) {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let names = ["a.jpg", "b.jpg", "c.jpg", "d.jpg"];
        let paths: Vec<PathBuf> = names.iter().map(|n| t.path().join(n)).collect();
        for (p, c) in paths.iter().zip(["A", "BB", "CCC"]) {
            std::fs::write(p, c).unwrap();
        }
        let moves: Vec<Move> = (0..3).map(|i| mv(&paths[i], &paths[i + 1])).collect();
        journal(&log, "crash", if p2.is_some() { RunState::Phase1Done } else { RunState::Started }, &moves);
        let n1 = if p2.is_some() { 3 } else { p1 };
        for (i, p) in paths.iter().enumerate().take(n1) {
            std::fs::rename(p, temp_path(p, "crash", i)).unwrap();
        }
        for (j, p) in paths.iter().enumerate().take(p2.unwrap_or(0)) {
            std::fs::rename(temp_path(p, "crash", j), &paths[j + 1]).unwrap();
        }
        let rep = recover_interrupted(&log, &RealFs).unwrap();
        (t, log, rep)
    }

    #[test]
    fn every_crash_point_of_a_chain_ends_completed_or_fully_reverted() {
        // Started: crash after 0..=3 phase-1 renames.
        for p1 in 0..=3 {
            let (t, log, rep) = crash(p1, None);
            assert_eq!((rep.reverted_runs, rep.stuck_runs), (1, 0), "p1={p1} {:?}", rep.lines);
            assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg", "c.jpg", "l.json"], "p1={p1}");
            assert_eq!((read(&t.path().join("a.jpg")), read(&t.path().join("b.jpg")), read(&t.path().join("c.jpg"))), ("A".into(), "BB".into(), "CCC".into()), "p1={p1}");
            assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::RolledBack);
            assert_eq!(rep.relink.len(), 3);
            assert_eq!(rep.relink[0].new, t.path().join("a.jpg"), "the project is pointed back at the old names");
        }
        // Phase1Done: crash after 0..=3 phase-2 renames.
        for p2 in 0..=3 {
            let (t, log, rep) = crash(3, Some(p2));
            if p2 == 3 {
                assert_eq!((rep.completed_runs, rep.reverted_runs), (1, 0), "{:?}", rep.lines);
                assert_eq!(listing(t.path()), vec!["b.jpg", "c.jpg", "d.jpg", "l.json"]);
                assert_eq!((read(&t.path().join("b.jpg")), read(&t.path().join("c.jpg")), read(&t.path().join("d.jpg"))), ("A".into(), "BB".into(), "CCC".into()));
                assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::Done, "a completed run is recorded as done");
                assert_eq!(rep.relink[0].new, t.path().join("b.jpg"), "the project follows the new names");
                // ... and "Undo last rename" works on it.
                undo_last(&log, &RealFs).unwrap();
                assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg", "c.jpg", "l.json"]);
            } else {
                assert_eq!((rep.completed_runs, rep.reverted_runs, rep.stuck_runs), (0, 1, 0), "p2={p2} {:?}", rep.lines);
                assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg", "c.jpg", "l.json"], "p2={p2}");
                assert_eq!((read(&t.path().join("a.jpg")), read(&t.path().join("b.jpg")), read(&t.path().join("c.jpg"))), ("A".into(), "BB".into(), "CCC".into()), "p2={p2}");
            }
        }
    }

    #[test]
    fn a_completed_swap_is_not_half_reverted() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let moves = [mv(&a, &b), mv(&b, &a)];
        journal(&log, "sw", RunState::Phase1Done, &moves);
        // phase 1 and 2 both done: a.jpg holds the old b, b.jpg the old a
        std::fs::write(&a, "BB").unwrap();
        std::fs::write(&b, "A").unwrap();
        let rep = recover_interrupted(&log, &RealFs).unwrap();
        assert_eq!((rep.completed_runs, rep.reverted_runs), (1, 0));
        assert_eq!((read(&a), read(&b)), ("BB".into(), "A".into()), "the swap stays swapped");
        assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::Done);
    }

    #[test]
    fn recovery_reports_files_it_cannot_find_and_keeps_the_journal_open() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let (a, b) = (touch(t.path(), "a.jpg", "A"), t.path().join("b.jpg"));
        journal(&log, "lost", RunState::Phase1Done, &[mv(&a, &b)]);
        std::fs::remove_file(&a).unwrap();
        let rep = recover_interrupted(&log, &RealFs).unwrap();
        assert_eq!(rep.stuck_runs, 1);
        assert!(rep.lines[0].contains("cannot be found"));
        assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::Phase1Done);
        // a new rename is refused while the journal holds an interrupted run
        let c = touch(t.path(), "c.jpg", "C");
        let plan = build(&[src(&c, 1)], &RenameRules { template: "d".into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
        assert!(matches!(execute(&plan, &RealFs, Some(&log), "x"), Err(RunError::Blocked(m)) if m.contains("Recover")));
        assert!(matches!(undo_last(&log, &RealFs), Err(RunError::Blocked(_))));
    }

    #[test]
    fn a_cancelled_run_puts_everything_back() {
        let t = tempfile::tempdir().unwrap();
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let control = Control::default();
        control.cancel.store(true, Ordering::Relaxed);
        let err = run_moves_ctx(&[mv(&a, &t.path().join("x.jpg")), mv(&b, &t.path().join("y.jpg"))], "c", &RealFs, Ctx::Preview, &control, &mut |_| Ok(())).unwrap_err();
        assert!(matches!(&err, RunError::Failed(f) if f.rolled_back && f.message.contains("Cancelled")), "{err}");
        assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg"]);
    }

    #[test]
    fn a_sidecar_may_change_size_but_a_picture_may_not() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let raw = touch(t.path(), "IMG_1.CR2", "raw");
        touch(t.path(), "IMG_1.xmp", "x");
        let plan = build(&[src(&raw, 1)], &RenameRules { template: "N".into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
        execute(&plan, &RealFs, Some(&log), "x").unwrap();
        std::fs::write(t.path().join("N.xmp"), "an editor rewrote this sidecar").unwrap();
        undo_last(&log, &RealFs).unwrap();
        assert_eq!(read(&t.path().join("IMG_1.xmp")), "an editor rewrote this sidecar");
        execute(&plan, &RealFs, Some(&log), "x").unwrap();
        std::fs::write(t.path().join("N.CR2"), "edited").unwrap();
        let err = undo_last(&log, &RealFs).unwrap_err();
        assert!(matches!(&err, RunError::Stale(m) if m.contains("since the rename")), "{err}");
    }

    /// Sets distinct modification times so the stamps tell the files apart.
    fn age(p: &Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + secs)).unwrap();
    }

    #[test]
    fn a_rollback_that_failed_half_way_is_finished_by_recover() {
        // Swap-free chain a->x, b->y; phase 2 fails at its second rename and the rollback cannot take the
        // first file back from its temporary name (the old name is held for a moment, then free again).
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let (a, b) = (touch(t.path(), "a.jpg", "A"), touch(t.path(), "b.jpg", "BB"));
        let (x, y) = (t.path().join("x.jpg"), t.path().join("y.jpg"));
        // calls: p1 a, p1 b, p2 a->x, p2 b->y FAILS; rollback x->tmp0 ok, tmp1->b ok, tmp0->a FAILS (x3 retries)
        struct Script {
            calls: Cell<usize>,
        }
        impl Fs for Script {
            fn rename(&self, f: &Path, to: &Path) -> io::Result<()> {
                let c = self.calls.get() + 1;
                self.calls.set(c);
                if c == 4 || (7..=9).contains(&c) {
                    return Err(io::Error::other("sharing violation"));
                }
                RealFs.rename(f, to)
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
            fn stamp(&self, p: &Path) -> Option<u64> {
                RealFs.stamp(p)
            }
        }
        let plan_moves = [mv(&a, &x), mv(&b, &y)];
        let err = run_logged(&plan_moves, &Script { calls: Cell::new(0) }, Some(&log), "x", RunKind::Rename, &Control::default()).unwrap_err();
        assert!(matches!(&err, RunError::Failed(f) if !f.rolled_back), "{err}");
        let l = RenameLog::load(&log).unwrap();
        assert_eq!(l.runs[0].state, RunState::RollingBack);
        assert_eq!(l.interrupted(), 1, "new runs and undo stay blocked");
        let rep = recover_interrupted(&log, &RealFs).unwrap();
        assert_eq!((rep.reverted_runs, rep.stuck_runs), (1, 0), "{:?}", rep.lines);
        assert_eq!(listing(t.path()), vec!["a.jpg", "b.jpg", "l.json"]);
        assert_eq!((read(&a), read(&b)), ("A".into(), "BB".into()));
        assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::RolledBack);
    }

    #[test]
    fn a_swap_put_back_before_the_journal_said_so_is_not_taken_for_done() {
        // a <-> b with equal sizes; the run was fully put back but the journal still says Phase1Done.
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let (a, b) = (touch(t.path(), "a.jpg", "AA"), touch(t.path(), "b.jpg", "BB"));
        age(&a, 10);
        age(&b, 20);
        let mut moves = vec![mv(&a, &b), mv(&b, &a)];
        journal(&log, "sw2", RunState::Phase1Done, &moves);
        let mut l = RenameLog::load(&log).unwrap();
        for (e, m) in l.runs[0].entries.iter_mut().zip(moves.iter_mut()) {
            e.stamp = RealFs.stamp(&m.old);
        }
        l.save(&log).unwrap();
        let rep = recover_interrupted(&log, &RealFs).unwrap();
        assert_eq!((rep.completed_runs, rep.reverted_runs, rep.stuck_runs), (0, 1, 0), "{:?}", rep.lines);
        assert_eq!((read(&a), read(&b)), ("AA".into(), "BB".into()), "nothing moved");
        assert_eq!(rep.relink[0].new, a, "the project keeps the old names");
        assert_eq!(RenameLog::load(&log).unwrap().runs[0].state, RunState::RolledBack);
    }

    #[test]
    fn a_completed_swap_with_stamps_is_recorded_as_done() {
        let t = tempfile::tempdir().unwrap();
        let log = t.path().join("l.json");
        let (a, b) = (touch(t.path(), "a.jpg", "AA"), touch(t.path(), "b.jpg", "BB"));
        age(&a, 10);
        age(&b, 20);
        let moves = [mv(&a, &b), mv(&b, &a)];
        journal(&log, "sw3", RunState::Phase1Done, &moves);
        let mut l = RenameLog::load(&log).unwrap();
        l.runs[0].entries[0].stamp = RealFs.stamp(&a);
        l.runs[0].entries[1].stamp = RealFs.stamp(&b);
        l.save(&log).unwrap();
        // do the swap by hand (renames keep the times)
        let tmp = t.path().join("t.jpg");
        std::fs::rename(&a, &tmp).unwrap();
        std::fs::rename(&b, &a).unwrap();
        std::fs::rename(&tmp, &b).unwrap();
        let rep = recover_interrupted(&log, &RealFs).unwrap();
        assert_eq!((rep.completed_runs, rep.reverted_runs, rep.stuck_runs), (1, 0, 0), "{:?}", rep.lines);
        assert_eq!((read(&a), read(&b)), ("BB".into(), "AA".into()));
    }
}
