//! Which way a project file is saved: updated in place (fast, little data) or
//! rewritten as a whole new file that replaces the old one (the file is complete
//! at every instant).
//!
//! In place is used only where it is safe: a local fixed drive that is not
//! inside a folder a sync client watches. Everything else gets the full
//! rewrite: network drives (UNC paths, mapped drives, SMB/NFS and similar mounts),
//! removable drives and sync folders (Nextcloud/ownCloud, Dropbox, OneDrive). The
//! reason is the rollback journal: a sync client can upload the main file and
//! `<name>-journal` separately, and a stray journal that reaches another machine
//! can be replayed onto a healthy file; locking on shares can be weak.
//!
//! The user can force the full rewrite with [`set_always_rewrite`] (also the
//! environment variable `RAWGRAPH_ALWAYS_REWRITE=1`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// One transaction updates the existing file where it lies.
    InPlace,
    /// A complete new file is written next to the target and renamed over it.
    FullRewrite,
}

/// The chosen mode and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteChoice {
    pub mode: WriteMode,
    /// Short text for a status line, e.g. "local drive" or "network drive".
    pub reason: String,
}

impl WriteChoice {
    fn new(mode: WriteMode, reason: impl Into<String>) -> Self {
        Self { mode, reason: reason.into() }
    }

    /// For the status line: `in place (local drive)` / `whole file rewritten (sync folder)`.
    pub fn describe(&self) -> String {
        match self.mode {
            WriteMode::InPlace => format!("in place, {}", self.reason),
            WriteMode::FullRewrite => format!("whole file rewritten, {}", self.reason),
        }
    }
}

static ALWAYS_REWRITE: AtomicBool = AtomicBool::new(false);

/// "Always rewrite the whole file" (default off).
pub fn set_always_rewrite(on: bool) {
    ALWAYS_REWRITE.store(on, Ordering::Relaxed);
}

pub fn always_rewrite() -> bool {
    ALWAYS_REWRITE.load(Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    /// Tests choose the mode themselves; without an override they always get in-place.
    pub(crate) static TEST_OVERRIDE: std::cell::Cell<Option<WriteMode>> = const { std::cell::Cell::new(None) };
}

static CACHE: Mutex<Option<HashMap<PathBuf, (Instant, WriteChoice)>>> = Mutex::new(None);
const CACHE_TTL: Duration = Duration::from_secs(30);

/// The mode for saving `file` (the project file, which need not exist yet).
/// The place is looked at once per 30 s per folder.
pub fn write_mode(file: &Path) -> WriteChoice {
    // Tests choose the mode themselves; without an override they get in-place.
    #[cfg(test)]
    let forced = Some(TEST_OVERRIDE.with(|o| o.get()).unwrap_or(WriteMode::InPlace));
    #[cfg(not(test))]
    let forced: Option<WriteMode> = None;
    if let Some(mode) = forced {
        return WriteChoice::new(mode, "test");
    }
    if always_rewrite() || std::env::var_os("RAWGRAPH_ALWAYS_REWRITE").is_some_and(|v| v != "0" && !v.is_empty()) {
        return WriteChoice::new(WriteMode::FullRewrite, "setting \"always rewrite the whole file\"");
    }
    let dir = std::path::absolute(file).ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    if let Ok(mut guard) = CACHE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some((at, choice)) = map.get(&dir) {
            if at.elapsed() < CACHE_TTL {
                return choice.clone();
            }
        }
        let choice = detect(&dir, &onedrive_roots());
        map.insert(dir, (Instant::now(), choice.clone()));
        return choice;
    }
    detect(&dir, &onedrive_roots())
}

/// Looks at the place without caches or settings (the pure part, testable).
pub(crate) fn detect(dir: &Path, onedrive: &[PathBuf]) -> WriteChoice {
    let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if let Some(why) = network_or_removable(&real) {
        return WriteChoice::new(WriteMode::FullRewrite, why);
    }
    if let Some(why) = sync_folder(&real, onedrive) {
        return WriteChoice::new(WriteMode::FullRewrite, why);
    }
    WriteChoice::new(WriteMode::InPlace, "local drive")
}

fn onedrive_roots() -> Vec<PathBuf> {
    ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].iter().filter_map(std::env::var_os).filter(|v| !v.is_empty()).map(PathBuf::from).collect()
}

fn key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.strip_prefix("//?/").unwrap_or(&s).to_owned();
    if cfg!(any(windows, target_os = "macos")) { s.to_lowercase() } else { s }
}

/// Marker files of sync clients, looked for in the folder and every parent folder.
fn sync_folder(real: &Path, onedrive: &[PathBuf]) -> Option<String> {
    let here = key(real);
    for root in onedrive {
        let r = key(root);
        let r = r.trim_end_matches('/');
        if !r.is_empty() && (here == r || here.starts_with(&format!("{r}/"))) {
            return Some("inside a OneDrive folder".to_owned());
        }
    }
    for dir in real.ancestors() {
        for (marker, who) in [
            (".nextcloudsync.log", "Nextcloud"),
            (".owncloudsync.log", "ownCloud"),
            (".dropbox", "Dropbox"),
            (".dropbox.cache", "Dropbox"),
        ] {
            if dir.join(marker).exists() {
                return Some(format!("inside a {who} sync folder"));
            }
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(".sync_") && name.ends_with(".db") {
                    return Some("inside a Nextcloud/ownCloud sync folder".to_owned());
                }
            }
        }
    }
    None
}

#[cfg(windows)]
fn network_or_removable(real: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;

    let Some(Component::Prefix(p)) = real.components().next() else { return Some("unknown drive".to_owned()) };
    match p.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => Some("network share (UNC path)".to_owned()),
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            let root: Vec<u16> = std::ffi::OsString::from(format!("{}:\\", char::from(letter))).encode_wide().chain(std::iter::once(0)).collect();
            // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
            match unsafe { GetDriveTypeW(root.as_ptr()) } {
                DRIVE_FIXED => None,
                DRIVE_REMOTE => Some("network drive".to_owned()),
                _ => Some("not a fixed local drive".to_owned()),
            }
        }
        _ => Some("unknown drive".to_owned()),
    }
}

#[cfg(not(windows))]
fn network_or_removable(real: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    if real.starts_with("/Volumes") {
        // Network shares and external disks are mounted here; the system disk is a link to "/".
        return Some("mounted volume".to_owned());
    }
    #[cfg(target_os = "linux")]
    if let Ok(mounts) = std::fs::read_to_string("/proc/self/mounts") {
        if let Some(fs) = network_fs_of(&mounts, real) {
            return Some(format!("network file system ({fs})"));
        }
    }
    let _ = real;
    None
}

/// The file system type of the mount that holds `path`, if it is a network one
/// (`/proc/self/mounts` text: device, mount point, type, ...).
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) fn network_fs_of(mounts: &str, path: &Path) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(_dev), Some(point), Some(kind)) = (f.next(), f.next(), f.next()) else { continue };
        let point = point.replace("\\040", " ");
        if path.starts_with(&point) && best.is_none_or(|(len, _)| point.len() >= len) {
            best = Some((point.len(), kind));
        }
    }
    let kind = best?.1;
    const NETWORK: [&str; 14] =
        ["nfs", "nfs4", "cifs", "smb3", "smbfs", "9p", "afs", "ceph", "glusterfs", "davfs", "ncpfs", "lustre", "fuse.sshfs", "fuse.rclone"];
    NETWORK.contains(&kind).then(|| kind.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("rawgraph-writemode-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_plain_local_folder_is_updated_in_place() {
        let t = Tmp::new("plain");
        let c = detect(&t.0, &[]);
        assert_eq!(c.mode, WriteMode::InPlace, "{c:?}");
    }

    #[test]
    fn sync_client_markers_in_the_folder_or_a_parent_force_the_full_rewrite() {
        for (marker, who) in [(".nextcloudsync.log", "Nextcloud"), (".owncloudsync.log", "ownCloud"), (".dropbox", "Dropbox"), (".dropbox.cache", "Dropbox"), (".sync_1a2b3c.db", "Nextcloud")] {
            let t = Tmp::new(&format!("marker{}", marker.len()));
            let deep = t.0.join("photos").join("2026");
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(t.0.join(marker), "").unwrap();
            for dir in [&t.0, &deep] {
                let c = detect(dir, &[]);
                assert_eq!(c.mode, WriteMode::FullRewrite, "{marker}: {c:?}");
                assert!(c.reason.contains(who), "{c:?}");
            }
        }
    }

    #[test]
    fn onedrive_folders_force_the_full_rewrite() {
        let t = Tmp::new("onedrive");
        let inside = t.0.join("Pictures");
        std::fs::create_dir_all(&inside).unwrap();
        assert_eq!(detect(&inside, std::slice::from_ref(&t.0)).mode, WriteMode::FullRewrite);
        assert_eq!(detect(&inside, &[t.0.join("elsewhere")]).mode, WriteMode::InPlace);
        // A folder next to the OneDrive folder with the same name prefix is not inside it.
        let sibling = Tmp::new("onedrive-2");
        assert_eq!(detect(&sibling.0, std::slice::from_ref(&t.0)).mode, WriteMode::InPlace);
    }

    #[test]
    fn network_file_systems_are_recognised_in_the_mount_table() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\nnas:/export /mnt/nas nfs4 rw 0 0\n//srv/share /mnt/smb cifs rw 0 0\n/dev/sdb1 /mnt/nas/local ext4 rw 0 0\n";
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/nas/photos/p.rawgraph")).as_deref(), Some("nfs4"));
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/smb/x")).as_deref(), Some("cifs"));
        assert_eq!(network_fs_of(mounts, Path::new("/home/me")), None);
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/nas/local/a")), None, "the longest mount point decides");
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_are_network_shares() {
        assert!(network_or_removable(Path::new(r"\\nas\share\photos")).is_some());
        assert!(network_or_removable(Path::new(r"\\?\UNC\nas\share\photos")).is_some());
    }

    #[test]
    fn the_description_names_mode_and_reason() {
        let c = WriteChoice::new(WriteMode::FullRewrite, "network drive");
        assert_eq!(c.describe(), "whole file rewritten, network drive");
    }
}
