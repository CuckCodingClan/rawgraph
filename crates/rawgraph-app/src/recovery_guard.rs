//! Which recovery files belong to a crash? Every running session keeps a lock
//! file in the recovery folder (a heartbeat plus the recovery files it owns), so
//! a second running instance is not mistaken for a crashed one. Files that no
//! live session owns are offered for recovery at the next start.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rawgraph_project::{list_recoveries, RecoveryInfo};

const LOCK_PREFIX: &str = "session-";
const LOCK_EXT: &str = "lock";
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
/// A heartbeat older than this means the session is gone (when its process cannot be asked).
pub const FRESH_MS: i64 = 20_000;

fn lock_file_name(key: &str) -> String {
    let safe: String = key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    format!("{LOCK_PREFIX}{safe}.{LOCK_EXT}")
}

fn is_lock_name(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == LOCK_EXT) && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(LOCK_PREFIX))
}

/// What a lock file says.
#[derive(Clone, Debug, PartialEq)]
pub struct LockInfo {
    pub path: PathBuf,
    pub pid: u32,
    pub heartbeat_ms: i64,
    pub exe: String,
    /// File names (no folders) of the recovery files the session owns.
    pub files: Vec<String>,
}

pub fn read_lock(path: &Path) -> Option<LockInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let pid = lines.next()?.trim().parse().ok()?;
    let heartbeat_ms = lines.next()?.trim().parse().ok()?;
    let exe = lines.next()?.trim().to_owned();
    let files = lines.map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect();
    Some(LockInfo { path: path.to_path_buf(), pid, heartbeat_ms, exe, files })
}

fn lock_text(pid: u32, now_ms: i64, exe: &str, files: &[String]) -> String {
    let mut t = format!("{pid}\n{now_ms}\n{exe}\n");
    for f in files {
        t.push_str(f);
        t.push('\n');
    }
    t
}

fn write_lock(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("lock.tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    drop(f);
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn exe_name() -> String {
    std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default()
}

/// This session's lock: written at once and refreshed by a heartbeat thread.
pub struct SessionLock {
    path: PathBuf,
    files: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    pid: u32,
    exe: String,
}

impl SessionLock {
    pub fn create(dir: &Path, key: &str) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(lock_file_name(key));
        let (pid, exe) = (std::process::id(), exe_name());
        write_lock(&path, &lock_text(pid, crate::clock::now_unix_ms(), &exe, &[])).map_err(|e| format!("{}: {e}", path.display()))?;
        let lock = Self { path: path.clone(), files: Arc::new(Mutex::new(Vec::new())), stop: Arc::new(AtomicBool::new(false)), pid, exe: exe.clone() };
        let (files, stop) = (lock.files.clone(), lock.stop.clone());
        let spawned = std::thread::Builder::new().name("rawgraph-heartbeat".into()).spawn(move || {
            let mut waited = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                waited += Duration::from_millis(100);
                if waited >= HEARTBEAT_EVERY {
                    waited = Duration::ZERO;
                    let list = files.lock().map(|f| f.clone()).unwrap_or_default();
                    let _ = write_lock(&path, &lock_text(pid, crate::clock::now_unix_ms(), &exe, &list));
                }
            }
        });
        if spawned.is_err() {
            // Without a heartbeat the process check still tells a live session from a crashed one.
        }
        Ok(lock)
    }

    /// The recovery files this session owns right now.
    pub fn set_files(&self, files: &[PathBuf]) {
        let names: Vec<String> = files.iter().filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        if let Ok(mut f) = self.files.lock() {
            f.clone_from(&names);
        }
        let _ = write_lock(&self.path, &lock_text(self.pid, crate::clock::now_unix_ms(), &self.exe, &names));
    }

    pub fn file_name(&self) -> Option<String> {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned())
    }

    /// Clean shutdown: stops the heartbeat and removes the lock.
    pub fn release(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Asks the operating system whether a process with this id and program name
/// exists. `None` when that cannot be found out.
pub fn process_alive(pid: u32, exe: &str) -> Option<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).creation_flags(0x0800_0000).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
        let wanted_pid = format!("\"{pid}\"");
        let exe = exe.to_lowercase();
        // tasklist shortens long image names, so the listed name may be a prefix of ours.
        Some(text.lines().any(|l| {
            let name = l.trim_start_matches('"').split('"').next().unwrap_or("");
            l.contains(&wanted_pid) && (exe.is_empty() || (!name.is_empty() && exe.starts_with(name)))
        }))
    }
    #[cfg(target_os = "linux")]
    {
        linux_process_alive(pid, exe)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        unix_ps_alive(pid, exe)
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = (pid, exe);
        None
    }
}

/// Linux: the pid must exist, must not be a zombie, and its program name must be ours. A pid that exists
/// but belongs to another program (pid reuse after a crash) is "not running", like on Windows.
#[cfg(target_os = "linux")]
fn linux_process_alive(pid: u32, exe: &str) -> Option<bool> {
    let dir = PathBuf::from(format!("/proc/{pid}"));
    if !dir.exists() {
        return Some(false);
    }
    // /proc/<pid>/stat: "pid (comm) S ..."; the state follows the last ')' (comm may contain anything).
    if let Ok(stat) = std::fs::read_to_string(dir.join("stat")) {
        let state = stat.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next());
        if matches!(state, Some('Z' | 'X')) {
            return Some(false);
        }
    }
    if exe.is_empty() {
        return Some(true);
    }
    // Full name from the exe link (a deleted/replaced binary shows " (deleted)").
    if let Ok(target) = std::fs::read_link(dir.join("exe")) {
        let full = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let full = full.strip_suffix(" (deleted)").unwrap_or(&full);
        return Some(full == exe);
    }
    // The link is unreadable for processes of other users: /proc/<pid>/comm holds the name cut to 15 bytes.
    if let Ok(comm) = std::fs::read_to_string(dir.join("comm")) {
        let comm = comm.trim_end_matches(&['\n', '\r'][..]);
        return Some(!comm.is_empty() && exe.starts_with(comm));
    }
    // Exists but cannot be identified: let the heartbeat decide.
    None
}

/// Other Unix systems (macOS, BSD) have no /proc: ask `ps` for the program of the pid. If `ps` cannot be
/// run the answer is `None`, so the heartbeat decides (a live session is not discarded).
#[cfg(all(unix, not(target_os = "linux")))]
fn unix_ps_alive(pid: u32, exe: &str) -> Option<bool> {
    let out = std::process::Command::new("ps").args(["-p", &pid.to_string(), "-o", "comm="]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let comm = text.trim();
    if comm.is_empty() {
        // `ps` exits non-zero and prints nothing for an unknown pid.
        return if out.status.success() { None } else { Some(false) };
    }
    if exe.is_empty() {
        return Some(true);
    }
    let name = Path::new(comm).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some(!name.is_empty() && (exe.starts_with(&name) || name.starts_with(exe)))
}

/// Whether the session behind a lock is still running. The process check wins
/// when it is possible; otherwise the heartbeat decides.
pub fn is_live(info: &LockInfo, now_ms: i64, alive: &dyn Fn(u32, &str) -> Option<bool>) -> bool {
    let fresh = now_ms.saturating_sub(info.heartbeat_ms) <= FRESH_MS;
    match alive(info.pid, &info.exe) {
        Some(true) => true,
        Some(false) => false,
        None => fresh,
    }
}

/// Recovery files nobody owns, plus the lock files of dead sessions.
#[derive(Debug, Default)]
pub struct Orphans {
    pub recoveries: Vec<RecoveryInfo>,
    pub stale_locks: Vec<PathBuf>,
}

pub fn scan_orphans(dir: &Path, own_lock_name: &str, now_ms: i64, alive: &dyn Fn(u32, &str) -> Option<bool>) -> Orphans {
    let mut owned: HashSet<String> = HashSet::new();
    let mut stale = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !is_lock_name(&path) {
                continue;
            }
            let is_own = path.file_name().is_some_and(|n| n.to_string_lossy() == own_lock_name);
            match read_lock(&path) {
                Some(info) if is_own || is_live(&info, now_ms, alive) => owned.extend(info.files),
                _ if is_own => {}
                _ => stale.push(path),
            }
        }
    }
    let recoveries = list_recoveries(dir).into_iter().filter(|r| r.file.file_name().is_none_or(|n| !owned.contains(n.to_string_lossy().as_ref()))).collect();
    Orphans { recoveries, stale_locks: stale }
}

/// Removes lock files of dead sessions (only files named like locks).
pub fn remove_locks(paths: &[PathBuf]) {
    for p in paths {
        if is_lock_name(p) {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_project::Project;

    const NOW: i64 = 1_800_000_000_000;

    fn dead(_: u32, _: &str) -> Option<bool> {
        Some(false)
    }
    fn alive(_: u32, _: &str) -> Option<bool> {
        Some(true)
    }
    fn unknown(_: u32, _: &str) -> Option<bool> {
        None
    }

    fn lock_for(dir: &Path, key: &str, pid: u32, beat: i64, files: &[&PathBuf]) {
        let names: Vec<String> = files.iter().map(|f| f.file_name().unwrap().to_string_lossy().into_owned()).collect();
        std::fs::write(dir.join(lock_file_name(key)), lock_text(pid, beat, "rawgraph-app.exe", &names)).unwrap();
    }

    #[test]
    fn lock_files_round_trip_and_bad_ones_are_unreadable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("session-x.lock");
        std::fs::write(&p, lock_text(42, NOW, "rawgraph-app.exe", &["a.rgrecovery".into(), "b.rgrecovery".into()])).unwrap();
        let info = read_lock(&p).unwrap();
        assert_eq!((info.pid, info.heartbeat_ms, info.exe.as_str(), info.files.len()), (42, NOW, "rawgraph-app.exe", 2));
        std::fs::write(&p, "garbage").unwrap();
        assert!(read_lock(&p).is_none());
        std::fs::write(&p, "").unwrap();
        assert!(read_lock(&p).is_none());
    }

    #[test]
    fn liveness_prefers_the_process_check_then_the_heartbeat() {
        let info = LockInfo { path: PathBuf::new(), pid: 1, heartbeat_ms: NOW - 1000, exe: String::new(), files: vec![] };
        assert!(is_live(&info, NOW, &alive));
        assert!(!is_live(&info, NOW, &dead), "fresh heartbeat but the process is gone (a crash just now)");
        assert!(is_live(&info, NOW, &unknown));
        let old = LockInfo { heartbeat_ms: NOW - FRESH_MS - 1, ..info.clone() };
        assert!(!is_live(&old, NOW, &unknown));
        assert!(is_live(&old, NOW, &alive), "a stalled but running process is still running");
    }

    #[test]
    fn own_session_other_live_session_and_crashed_session() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mine = Project::new().save_recovery(dir, None, "mine").unwrap();
        let theirs = Project::new().save_recovery(dir, None, "theirs").unwrap();
        let crashed = Project::new().save_recovery(dir, Some(&dir.join("job.rawgraph")), "x").unwrap();
        lock_for(dir, "mine", 10, NOW, &[&mine]);
        lock_for(dir, "theirs", 20, NOW - 2000, &[&theirs]);
        lock_for(dir, "dead", 30, NOW - 3_600_000, &[&crashed]);
        let found = scan_orphans(dir, &lock_file_name("mine"), NOW, &|pid, _| Some(pid != 30));
        assert_eq!(found.recoveries.len(), 1, "only the crashed session's file is offered");
        assert_eq!(found.recoveries[0].file, crashed);
        assert_eq!(found.stale_locks.len(), 1);
        assert!(found.stale_locks[0].ends_with(lock_file_name("dead")));
        // The same data, but the other instance cannot be asked: its fresh heartbeat protects it.
        let found = scan_orphans(dir, &lock_file_name("mine"), NOW, &unknown);
        let files: Vec<&PathBuf> = found.recoveries.iter().map(|r| &r.file).collect();
        assert_eq!(files, vec![&crashed]);
        // Remove the stale lock only.
        remove_locks(&found.stale_locks);
        assert!(!dir.join(lock_file_name("dead")).exists() && dir.join(lock_file_name("theirs")).exists());
        assert!(crashed.exists(), "the recovery file itself is not touched");
        // Without any locks (an older version, or all sessions gone) everything is an orphan.
        for k in ["mine", "theirs"] {
            std::fs::remove_file(dir.join(lock_file_name(k))).unwrap();
        }
        assert_eq!(scan_orphans(dir, "none", NOW, &dead).recoveries.len(), 3);
    }

    #[test]
    fn a_missing_or_empty_folder_is_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan_orphans(&tmp.path().join("nope"), "x", NOW, &alive).recoveries.is_empty());
        assert!(scan_orphans(tmp.path(), "x", NOW, &alive).stale_locks.is_empty());
    }

    #[test]
    fn a_live_session_lock_is_created_updated_and_released() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = SessionLock::create(tmp.path(), "k-1").unwrap();
        let path = tmp.path().join(lock.file_name().unwrap());
        let info = read_lock(&path).unwrap();
        assert_eq!(info.pid, std::process::id());
        let rec = Project::new().save_recovery(tmp.path(), None, "k-1").unwrap();
        lock.set_files(std::slice::from_ref(&rec));
        assert_eq!(read_lock(&path).unwrap().files, vec![rec.file_name().unwrap().to_string_lossy().into_owned()]);
        // This very process protects its own recovery file from the start-up scan.
        let found = scan_orphans(tmp.path(), &lock.file_name().unwrap(), crate::clock::now_unix_ms(), &process_alive);
        assert!(found.recoveries.is_empty());
        // A second instance sees the live lock through the real process check.
        let found = scan_orphans(tmp.path(), "someone-else.lock", crate::clock::now_unix_ms(), &process_alive);
        assert!(found.recoveries.is_empty(), "this process is running");
        lock.release();
        assert!(!path.exists());
    }

    #[test]
    fn the_real_process_check_knows_this_process() {
        let me = std::process::id();
        let exe = exe_name();
        assert_ne!(process_alive(me, &exe), Some(false));
        // A name that does not fit the pid is not "alive" (pid reuse).
        if process_alive(me, &exe).is_some() {
            assert_eq!(process_alive(me, "definitely-not-this-program.exe"), Some(false));
        }
    }
}
