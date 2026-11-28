//! Which way a project file is saved: updated in place (fast, little data) or
//! rewritten as a whole new file that replaces the old one (the file is complete
//! at every instant).
//!
//! In place is used only where it is safe: a local fixed drive that is not
//! inside a folder a sync client watches. Everything else gets the full
//! rewrite: network drives (UNC paths, mapped network drives, SMB/NFS and similar
//! mounts), drives Windows does not call fixed, and sync or cloud folders
//! (Nextcloud/ownCloud, Dropbox, OneDrive incl. business tenants, Google Drive,
//! iCloud, Syncthing, Synology Drive). The reason is the rollback journal: a
//! sync client can upload the main file and `<name>-journal` separately, and a
//! stray journal that reaches another machine can be replayed onto a healthy
//! file; locking on shares can be weak.
//!
//! Limits of the detection: an external USB disk reports "fixed" to Windows and is
//! treated like an internal one; a share mounted in an unusual way, or a sync
//! client without a marker this module knows, is not recognised. The setting
//! "Always rewrite the whole project file" ([`set_always_rewrite`], also the
//! environment variable `RAWGRAPH_ALWAYS_REWRITE=1`) covers those cases.

use std::collections::HashMap;
use std::ffi::OsString;
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

/// The chosen (or used) mode and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteChoice {
    pub mode: WriteMode,
    /// Short text for a status line, e.g. "local drive" or "network drive".
    pub reason: String,
}

impl WriteChoice {
    pub(crate) fn new(mode: WriteMode, reason: impl Into<String>) -> Self {
        Self { mode, reason: reason.into() }
    }

    /// For the status line: `in place, local drive` / `whole file rewritten, sync folder`.
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

/// `RAWGRAPH_ALWAYS_REWRITE`: anything but empty or "0" switches the full rewrite on.
pub(crate) fn env_forces_rewrite(value: Option<OsString>) -> bool {
    value.is_some_and(|v| v != "0" && !v.is_empty())
}

#[cfg(test)]
thread_local! {
    /// Tests choose the mode themselves; without an override they always get in-place.
    pub(crate) static TEST_OVERRIDE: std::cell::Cell<Option<WriteMode>> = const { std::cell::Cell::new(None) };
}

/// Answers per folder, so the place is looked at once in a while and not at every save.
pub(crate) struct ModeCache(Mutex<Option<HashMap<PathBuf, (Instant, WriteChoice)>>>);

impl ModeCache {
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn get(&self, dir: &Path, ttl: Duration) -> Option<WriteChoice> {
        let guard = self.0.lock().ok()?;
        let (at, choice) = guard.as_ref()?.get(dir)?;
        (at.elapsed() < ttl).then(|| choice.clone())
    }

    fn put(&self, dir: PathBuf, choice: WriteChoice) {
        if let Ok(mut guard) = self.0.lock() {
            guard.get_or_insert_with(HashMap::new).insert(dir, (Instant::now(), choice));
        }
    }
}

static CACHE: ModeCache = ModeCache::new();
pub(crate) const CACHE_TTL: Duration = Duration::from_secs(300);

/// The mode for saving `file` (the project file, which need not exist yet).
/// The place is looked at once per 5 minutes per folder.
pub fn write_mode(file: &Path) -> WriteChoice {
    // Tests choose the mode themselves; without an override they get in-place.
    #[cfg(test)]
    let forced = Some(TEST_OVERRIDE.with(|o| o.get()).unwrap_or(WriteMode::InPlace));
    #[cfg(not(test))]
    let forced: Option<WriteMode> = None;
    if let Some(mode) = forced {
        return WriteChoice::new(mode, "test");
    }
    choose(file, always_rewrite(), env_forces_rewrite(std::env::var_os("RAWGRAPH_ALWAYS_REWRITE")), &CACHE, CACHE_TTL, &|dir| {
        detect(dir, &onedrive_roots())
    })
}

/// The decision with all inputs given (testable): the setting and the environment variable
/// win, then the cached answer for the folder, then `detector` (run without any lock held).
pub(crate) fn choose(file: &Path, always: bool, env: bool, cache: &ModeCache, ttl: Duration, detector: &dyn Fn(&Path) -> WriteChoice) -> WriteChoice {
    if always {
        return WriteChoice::new(WriteMode::FullRewrite, "setting \"always rewrite the whole file\"");
    }
    if env {
        return WriteChoice::new(WriteMode::FullRewrite, "environment variable RAWGRAPH_ALWAYS_REWRITE");
    }
    let dir = std::path::absolute(file).ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    if let Some(choice) = cache.get(&dir, ttl) {
        return choice;
    }
    let choice = detector(&dir);
    cache.put(dir, choice.clone());
    choice
}

/// Looks at the place without caches or settings.
pub(crate) fn detect(dir: &Path, onedrive: &[PathBuf]) -> WriteChoice {
    let given = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let real = std::fs::canonicalize(dir).unwrap_or_else(|_| given.clone());
    if let Some(why) = network_or_removable(&real) {
        return WriteChoice::new(WriteMode::FullRewrite, why);
    }
    // Both spellings: a link or a substituted drive can hide the sync folder from one of them.
    let mut places = vec![real];
    if key(&given) != key(&places[0]) {
        places.push(given);
    }
    for place in &places {
        if let Some(why) = sync_folder(place, onedrive) {
            return WriteChoice::new(WriteMode::FullRewrite, why);
        }
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

/// Names of sync clients' marker files, probed directly in each folder.
const MARKERS: [(&str, &str); 7] = [
    (".nextcloudsync.log", "Nextcloud"),
    (".owncloudsync.log", "ownCloud"),
    (".csync_journal.db", "ownCloud/Nextcloud"),
    (".dropbox", "Dropbox"),
    (".dropbox.cache", "Dropbox"),
    (".stfolder", "Syncthing"),
    (".SynologyDrive", "Synology Drive"),
];

/// Most entries looked at in one folder when searching for `.sync_*.db`.
const LISTING_CAP: usize = 50_000;

/// The sync or cloud client that handles `real` or a parent, if any: OneDrive env folders,
/// path names of cloud folders, cloud placeholders (Windows), marker files.
fn sync_folder(real: &Path, onedrive: &[PathBuf]) -> Option<String> {
    let here = key(real);
    for root in onedrive {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let r = key(&root);
        let r = r.trim_end_matches('/');
        if !r.is_empty() && (here == r || here.starts_with(&format!("{r}/"))) {
            return Some("inside a OneDrive folder".to_owned());
        }
    }
    if let Some(who) = cloud_by_name(&here) {
        return Some(format!("inside a {who} folder"));
    }
    for dir in real.ancestors() {
        if dir.parent().is_none() {
            continue; // the drive root
        }
        if cloud_placeholder(dir) {
            return Some("inside a cloud-synced folder (files on demand)".to_owned());
        }
    }
    for dir in real.ancestors() {
        for (marker, who) in MARKERS {
            if std::fs::symlink_metadata(dir.join(marker)).is_ok() {
                return Some(format!("inside a {who} sync folder"));
            }
        }
        if has_sync_db(dir) {
            return Some("inside a Nextcloud/ownCloud sync folder".to_owned());
        }
    }
    None
}

/// `.sync_<id>.db` or `._sync_<id>.db` in `dir` (the journal of the Nextcloud/ownCloud client).
fn has_sync_db(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.take(LISTING_CAP).flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.ends_with(".db") && (name.starts_with(".sync_") || name.starts_with("._sync_"))
    })
}

/// Cloud folders recognised by their names (`key` is lower-case with `/`, see [`key`]).
pub(crate) fn cloud_by_name(key: &str) -> Option<&'static str> {
    let segments: Vec<&str> = key.split('/').filter(|s| !s.is_empty()).collect();
    for (i, s) in segments.iter().enumerate() {
        let s = s.to_lowercase();
        if matches!(s.as_str(), "my drive" | "google drive" | "googledrive") {
            return Some("Google Drive");
        }
        if matches!(s.as_str(), "iclouddrive" | "icloud drive") {
            return Some("iCloud");
        }
        if s.starts_with("onedrive") {
            return Some("OneDrive");
        }
        if s == "library" {
            match segments.get(i + 1).map(|n| n.to_lowercase()).as_deref() {
                Some("mobile documents") => return Some("iCloud"),
                Some("cloudstorage") => return Some("cloud storage (macOS)"),
                _ => {}
            }
        }
    }
    None
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const ATTR_RECALL_ON_OPEN: u32 = 0x0004_0000;
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
const ATTR_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// A Windows "files on demand" placeholder (OneDrive incl. business, Google Drive, iCloud for Windows ...).
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn cloud_attributes(attrs: u32) -> bool {
    attrs & (ATTR_RECALL_ON_OPEN | ATTR_RECALL_ON_DATA_ACCESS) != 0
}

/// `IO_REPARSE_TAG_CLOUD` (0x9000001A) and `IO_REPARSE_TAG_CLOUD_1` ... `_F` (0x9000X01A).
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn cloud_reparse_tag(tag: u32) -> bool {
    tag & 0xFFFF_0FFF == 0x9000_001A
}

#[cfg(windows)]
#[repr(C)]
struct FindDataW {
    attributes: u32,
    // Three FILETIMEs: pairs of u32 with 4-byte alignment (a u64 here would add padding and shift
    // every later field).
    times: [u32; 6],
    size_high: u32,
    size_low: u32,
    reserved0: u32,
    reserved1: u32,
    file_name: [u16; 260],
    alternate: [u16; 14],
}

// WIN32_FIND_DATAW is 592 bytes; `reserved0` (dwReserved0, the reparse tag) sits at offset 36.
#[cfg(windows)]
const _: () = assert!(std::mem::size_of::<FindDataW>() == 592);
#[cfg(windows)]
const _: () = assert!(std::mem::offset_of!(FindDataW, reserved0) == 36);

#[cfg(windows)]
fn cloud_placeholder(dir: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileAttributesW(name: *const u16) -> u32;
        fn FindFirstFileW(name: *const u16, data: *mut FindDataW) -> isize;
        fn FindClose(handle: isize) -> i32;
    }
    const INVALID_ATTRIBUTES: u32 = 0xFFFF_FFFF;
    const ATTR_REPARSE_POINT: u32 = 0x400;

    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated wide string that outlives the calls; `data` is a
    // properly sized and aligned WIN32_FIND_DATAW that FindFirstFileW fills.
    unsafe {
        let attrs = GetFileAttributesW(wide.as_ptr());
        if attrs == INVALID_ATTRIBUTES {
            return false;
        }
        if cloud_attributes(attrs) {
            return true;
        }
        if attrs & ATTR_REPARSE_POINT != 0 {
            let mut data: FindDataW = std::mem::zeroed();
            let handle = FindFirstFileW(wide.as_ptr(), &mut data);
            if handle != -1 {
                let tag = data.reserved0;
                FindClose(handle);
                return cloud_reparse_tag(tag);
            }
        }
    }
    false
}

#[cfg(not(windows))]
fn cloud_placeholder(_dir: &Path) -> bool {
    false
}

/// `GetDriveTypeW` result to a reason for the full rewrite (`None` = fixed local drive).
/// Note that external USB disks report DRIVE_FIXED and are not recognised.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(crate) fn drive_kind(code: u32) -> Option<String> {
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    match code {
        DRIVE_FIXED => None,
        DRIVE_REMOTE => Some("network drive".to_owned()),
        DRIVE_REMOVABLE => Some("removable drive".to_owned()),
        _ => Some("not a fixed local drive".to_owned()),
    }
}

#[cfg(windows)]
fn network_or_removable(real: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Component, Prefix};

    #[link(name = "kernel32")]
    extern "system" {
        fn GetDriveTypeW(root: *const u16) -> u32;
    }

    let Some(Component::Prefix(p)) = real.components().next() else { return Some("unknown drive".to_owned()) };
    match p.kind() {
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => Some("network share (UNC path)".to_owned()),
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            let root: Vec<u16> = std::ffi::OsString::from(format!("{}:\\", char::from(letter))).encode_wide().chain(std::iter::once(0)).collect();
            // SAFETY: `root` is a NUL-terminated wide string that outlives the call.
            drive_kind(unsafe { GetDriveTypeW(root.as_ptr()) })
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
    const NETWORK: [&str; 20] = [
        "nfs", "nfs4", "cifs", "smb", "smb3", "smbfs", "9p", "afs", "ceph", "glusterfs", "davfs", "fuse.davfs", "ncpfs", "lustre", "fuse.sshfs",
        "fuse.rclone", "fuse.s3fs", "fuse.gvfsd-fuse", "fuse.glusterfs", "fuse.cephfs",
    ];
    NETWORK.contains(&kind).then(|| kind.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

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
        let cases = [
            (".nextcloudsync.log", "Nextcloud"),
            (".owncloudsync.log", "ownCloud"),
            (".dropbox", "Dropbox"),
            (".dropbox.cache", "Dropbox"),
            (".sync_1a2b3c.db", "Nextcloud"),
            ("._sync_1a2b3c.db", "Nextcloud"),
            (".csync_journal.db", "ownCloud"),
            (".stfolder", "Syncthing"),
            (".SynologyDrive", "Synology"),
        ];
        for (n, (marker, who)) in cases.into_iter().enumerate() {
            let t = Tmp::new(&format!("marker{n}"));
            let deep = t.0.join("photos").join("2026");
            std::fs::create_dir_all(&deep).unwrap();
            std::fs::write(t.0.join(marker), "").unwrap();
            for dir in [&t.0, &deep] {
                let c = detect(dir, &[]);
                assert_eq!(c.mode, WriteMode::FullRewrite, "{marker}: {c:?}");
                assert!(c.reason.contains(who), "{marker}: {c:?}");
            }
        }
    }

    #[test]
    fn a_marker_is_found_along_the_given_path_and_the_real_path() {
        // A link into a sync folder: only the spelling through the link shows the marker.
        let t = Tmp::new("linked");
        let sync = t.0.join("sync");
        let real = t.0.join("elsewhere");
        std::fs::create_dir_all(&sync).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(sync.join(".dropbox"), "").unwrap();
        let linked = sync.join("project");
        let made = {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real, &linked).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(&real, &linked).is_ok()
            }
        };
        if made {
            assert_eq!(detect(&linked, &[]).mode, WriteMode::FullRewrite, "the given path leads through the sync folder");
        } else {
            // Creating links may need a privilege; the check of one spelling still holds.
            assert!(sync_folder(&sync, &[]).is_some());
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
        // The environment path is canonicalised before it is compared (here with a ".." in it).
        let roundabout = t.0.join("Pictures").join("..");
        assert_eq!(detect(&inside, &[roundabout]).mode, WriteMode::FullRewrite);
    }

    #[test]
    fn cloud_folders_are_recognised_by_name() {
        assert_eq!(cloud_by_name("g:/my drive/photos"), Some("Google Drive"));
        assert_eq!(cloud_by_name("c:/users/me/google drive/x"), Some("Google Drive"));
        assert_eq!(cloud_by_name("c:/users/me/iclouddrive/projects"), Some("iCloud"));
        assert_eq!(cloud_by_name("/users/me/library/mobile documents/com~apple~clouddocs/p"), Some("iCloud"));
        assert_eq!(cloud_by_name("/users/me/library/cloudstorage/dropbox/p"), Some("cloud storage (macOS)"));
        assert_eq!(cloud_by_name("c:/users/me/onedrive - contoso/p"), Some("OneDrive"));
        assert_eq!(cloud_by_name("d:/photos/2026/shoot"), None);
        assert_eq!(cloud_by_name("/home/me/library/books"), None);
    }

    #[test]
    fn windows_cloud_placeholders_are_recognised_by_attributes_and_reparse_tags() {
        assert!(cloud_attributes(0x0040_0000 | 0x10), "recall on data access");
        assert!(cloud_attributes(0x0004_0000), "recall on open");
        assert!(!cloud_attributes(0x10 | 0x400), "a plain directory or junction");
        assert!(cloud_reparse_tag(0x9000_001A));
        assert!(cloud_reparse_tag(0x9000_101A) && cloud_reparse_tag(0x9000_F01A));
        assert!(!cloud_reparse_tag(0xA000_000C), "a symbolic link");
        assert!(!cloud_reparse_tag(0x8000_0017));
    }

    #[test]
    fn network_file_systems_are_recognised_in_the_mount_table() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\nnas:/export /mnt/nas nfs4 rw 0 0\n//srv/share /mnt/smb cifs rw 0 0\n/dev/sdb1 /mnt/nas/local ext4 rw 0 0\n\
                      gvfsd-fuse /run/user/1000/gvfs fuse.gvfsd-fuse rw 0 0\ns3 /mnt/s3 fuse.s3fs rw 0 0\n//x/y /mnt/old smb rw 0 0\n";
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/nas/photos/p.rawgraph")).as_deref(), Some("nfs4"));
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/smb/x")).as_deref(), Some("cifs"));
        assert_eq!(network_fs_of(mounts, Path::new("/run/user/1000/gvfs/smb-share/p")).as_deref(), Some("fuse.gvfsd-fuse"));
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/s3/p")).as_deref(), Some("fuse.s3fs"));
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/old/p")).as_deref(), Some("smb"));
        assert_eq!(network_fs_of(mounts, Path::new("/home/me")), None);
        assert_eq!(network_fs_of(mounts, Path::new("/mnt/nas/local/a")), None, "the longest mount point decides");
    }

    #[test]
    fn drive_types_map_to_reasons() {
        assert_eq!(drive_kind(3), None, "fixed (also what an external USB disk reports)");
        assert_eq!(drive_kind(4).as_deref(), Some("network drive"), "a mapped network drive");
        assert_eq!(drive_kind(2).as_deref(), Some("removable drive"));
        assert!(drive_kind(5).is_some() && drive_kind(0).is_some());
    }

    #[cfg(windows)]
    #[test]
    fn unc_paths_are_network_shares() {
        assert!(network_or_removable(Path::new(r"\\nas\share\photos")).is_some());
        assert!(network_or_removable(Path::new(r"\\?\UNC\nas\share\photos")).is_some());
        // The folder of the running test lives on a real drive; asking about it must not panic.
        let _ = network_or_removable(&std::fs::canonicalize(std::env::temp_dir()).unwrap());
    }

    #[test]
    fn the_description_names_mode_and_reason() {
        let c = WriteChoice::new(WriteMode::FullRewrite, "network drive");
        assert_eq!(c.describe(), "whole file rewritten, network drive");
        assert_eq!(WriteChoice::new(WriteMode::InPlace, "local drive").describe(), "in place, local drive");
    }

    #[test]
    fn the_setting_the_environment_and_the_cache_decide_in_this_order() {
        let t = Tmp::new("choose");
        let file = t.0.join("p.rawgraph");
        let calls = AtomicUsize::new(0);
        let detector = |_: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            WriteChoice::new(WriteMode::InPlace, "local drive")
        };
        let cache = ModeCache::new();
        let ttl = Duration::from_secs(300);
        // The setting and the environment variable win and do not even look at the place.
        assert_eq!(choose(&file, true, false, &cache, ttl, &detector).mode, WriteMode::FullRewrite);
        let by_env = choose(&file, false, true, &cache, ttl, &detector);
        assert_eq!(by_env.mode, WriteMode::FullRewrite);
        assert!(by_env.reason.contains("RAWGRAPH_ALWAYS_REWRITE"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // Otherwise the place is looked at once and the answer is reused for the folder ...
        assert_eq!(choose(&file, false, false, &cache, ttl, &detector).mode, WriteMode::InPlace);
        assert_eq!(choose(&t.0.join("other.rawgraph"), false, false, &cache, ttl, &detector).mode, WriteMode::InPlace);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // ... until it is older than the time to live, or another folder is asked about.
        assert_eq!(choose(&file, false, false, &cache, Duration::ZERO, &detector).mode, WriteMode::InPlace);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(choose(&t.0.join("sub").join("p.rawgraph"), false, false, &cache, ttl, &detector).mode, WriteMode::InPlace);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        // The setting is switched back off: the cached answer is used again.
        assert_eq!(choose(&file, false, false, &cache, ttl, &detector).mode, WriteMode::InPlace);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn the_environment_variable_values_are_read_like_a_switch() {
        assert!(env_forces_rewrite(Some("1".into())));
        assert!(env_forces_rewrite(Some("yes".into())));
        assert!(!env_forces_rewrite(Some("0".into())));
        assert!(!env_forces_rewrite(Some("".into())));
        assert!(!env_forces_rewrite(None));
    }

    #[test]
    fn the_global_setting_is_read_back() {
        // Only this test touches the global flag; write_mode itself is overridden in tests.
        set_always_rewrite(true);
        assert!(always_rewrite());
        set_always_rewrite(false);
        assert!(!always_rewrite());
    }
}
