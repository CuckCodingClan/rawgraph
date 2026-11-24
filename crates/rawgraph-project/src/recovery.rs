//! Crash-recovery copies of a project.
//!
//! Layout of a recovery directory:
//! - `.rawgraph-recovery-dir`: marker file written when the directory is first used;
//! - `recovery-<32 hex>.rgrecovery`: one file per project path (or per session key
//!   for an unsaved project), a normal project database plus these `meta` rows:
//!   `recovery_marker`, `recovery_saved_unix_ms`, `recovery_project_path` (absent
//!   for an unsaved project) and `recovery_key`.

use std::path::{Path, PathBuf};

use crate::{store, util, Project, ProjectError};

const DIR_MARKER: &str = ".rawgraph-recovery-dir";
const FILE_EXT: &str = "rgrecovery";
const FILE_PREFIX: &str = "recovery-";
const META_MARKER: &str = "recovery_marker";
const MARKER_VALUE: &str = "rawgraph-recovery-v1";

#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryInfo {
    pub file: PathBuf,
    pub project_path: Option<PathBuf>,
    pub saved_unix_ms: i64,
    pub image_count: usize,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn io_err(path: &Path, e: std::io::Error) -> ProjectError {
    ProjectError::Io(format!("{}: {e}", path.display()))
}

fn is_recovery_name(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == FILE_EXT)
        && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with(FILE_PREFIX))
}

fn info_of(file: &Path) -> Result<RecoveryInfo, ProjectError> {
    let (meta, image_count) = store::read_meta(file)?;
    if meta.get(META_MARKER).map(String::as_str) != Some(MARKER_VALUE) {
        return Err(ProjectError::Corrupt("not a RawGraph recovery file".into()));
    }
    let saved_unix_ms = meta
        .get("recovery_saved_unix_ms")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| ProjectError::Corrupt("recovery file has no save time".into()))?;
    Ok(RecoveryInfo {
        file: file.to_path_buf(),
        project_path: meta.get("recovery_project_path").map(PathBuf::from),
        saved_unix_ms,
        image_count,
    })
}

impl Project {
    /// Writes a recovery copy of this project into `dir` (created if needed).
    /// There is one recovery file per `project_path`; `None` (an unsaved new
    /// project) is keyed by `session_key`. A newer call replaces the older file
    /// atomically.
    pub fn save_recovery(
        &self,
        dir: &Path,
        project_path: Option<&Path>,
        session_key: &str,
    ) -> Result<PathBuf, ProjectError> {
        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;
        let marker = dir.join(DIR_MARKER);
        if !marker.is_file() {
            std::fs::write(&marker, "RawGraph recovery directory\n").map_err(|e| io_err(&marker, e))?;
        }
        let key = match project_path {
            Some(p) => format!("path:{}", util::path_key(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))),
            None => format!("session:{session_key}"),
        };
        let digest = blake3::hash(key.as_bytes()).to_hex();
        let file = dir.join(format!("{FILE_PREFIX}{}.{FILE_EXT}", &digest.as_str()[..32]));
        let mut extra = vec![
            (META_MARKER, MARKER_VALUE.to_string()),
            ("recovery_saved_unix_ms", now_ms().to_string()),
            ("recovery_key", key),
        ];
        if let Some(p) = project_path {
            let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
            extra.push(("recovery_project_path", abs.to_string_lossy().into_owned()));
        }
        store::save_with_meta(self, &file, &extra)?;
        Ok(file)
    }
}

/// Recovery files in `dir`, newest first. Unreadable files are skipped.
pub fn list_recoveries(dir: &Path) -> Vec<RecoveryInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<RecoveryInfo> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_recovery_name(p) && p.is_file())
        .filter_map(|p| info_of(&p).ok())
        .collect();
    out.sort_by(|a, b| b.saved_unix_ms.cmp(&a.saved_unix_ms).then_with(|| a.file.cmp(&b.file)));
    out
}

/// Loads the project stored in a recovery file.
pub fn load_recovery(file: &Path) -> Result<(Project, RecoveryInfo), ProjectError> {
    let info = info_of(file)?;
    let project = Project::load(file)?;
    Ok((project, info))
}

/// Deletes a recovery file. Only files inside a directory carrying this
/// crate's marker, named like a recovery file and containing the recovery
/// marker are deleted; anything else is refused.
pub fn discard_recovery(file: &Path) -> Result<(), ProjectError> {
    let refuse = |why: &str| ProjectError::Mismatch(format!("{} is not a RawGraph recovery file ({why})", file.display()));
    if !is_recovery_name(file) {
        return Err(refuse("wrong name"));
    }
    let meta = std::fs::symlink_metadata(file).map_err(|e| io_err(file, e))?;
    if !meta.is_file() {
        return Err(refuse("not a regular file"));
    }
    let dir = file.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !dir.join(DIR_MARKER).is_file() {
        return Err(refuse("not in a recovery directory"));
    }
    info_of(file).map_err(|_| refuse("missing recovery marker"))?;
    std::fs::remove_file(file).map_err(|e| io_err(file, e))
}
