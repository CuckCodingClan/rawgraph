//! Finding moved files again: "Locate file..." for one image and "Relink
//! folder..." for a whole moved job folder. Only the project's own paths
//! change; the image files are never touched.

use std::path::{Path, PathBuf};

use rawgraph_core::ImageId;
use rawgraph_project::{Project, ProjectError};

use crate::session::Session;

/// Why a relink did not happen.
#[derive(Clone, Debug, PartialEq)]
pub enum RelinkError {
    /// The chosen file differs from the original; the core's message. It can be forced.
    Mismatch(String),
    Other(String),
}

impl RelinkError {
    pub fn message(&self) -> &str {
        match self {
            RelinkError::Mismatch(m) | RelinkError::Other(m) => m,
        }
    }
}

fn busy(session: &Session) -> Option<RelinkError> {
    if session.demo {
        return Some(RelinkError::Other("The demo images are not files".to_owned()));
    }
    session.scanning.then(|| RelinkError::Other("Wait until the scan has finished".to_owned()))
}

/// Points image `id` at `path`. Without `force` the file must match the
/// original (size and content); with it the entry takes over the new file.
pub fn locate(session: &mut Session, id: ImageId, path: &Path, force: bool) -> Result<(), RelinkError> {
    if let Some(e) = busy(session) {
        return Err(e);
    }
    match session.project.relink(id, path, force) {
        Ok(()) => {
            session.sync_paths();
            session.touch();
            session.refresh_visible();
            Ok(())
        }
        Err(ProjectError::Mismatch(m)) => Err(RelinkError::Mismatch(ProjectError::Mismatch(m).to_string())),
        Err(e) => Err(RelinkError::Other(e.to_string())),
    }
}

/// "n of m missing files are found there".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderPreview {
    /// Missing entries under the old folder.
    pub missing_total: usize,
    /// Of those, how many exist under the new folder.
    pub missing_found: usize,
    /// Entries under the old folder that are not missing but would be re-pointed too
    /// (`Project::relink_folder` moves every entry below the old folder whose file exists at the new place).
    pub also_moved: usize,
}

/// Computes the preview on a copy of the project.
pub fn folder_preview(project: &Project, old: &Path, new: &Path) -> FolderPreview {
    let missing_ids: Vec<ImageId> = project.images().iter().filter(|e| e.missing && under(&e.path, old)).map(|e| e.id).collect();
    let before: Vec<(ImageId, PathBuf)> = project.images().iter().map(|e| (e.id, e.path.clone())).collect();
    let mut copy = project.clone();
    copy.relink_folder(old, new);
    let mut found = 0;
    let mut also = 0;
    for (id, old_path) in before {
        let Some(after) = copy.image(id) else { continue };
        if after.path != old_path {
            if missing_ids.contains(&id) {
                found += 1;
            } else {
                also += 1;
            }
        }
    }
    FolderPreview { missing_total: missing_ids.len(), missing_found: found, also_moved: also }
}

/// Whether `path` lies below `prefix` (case-insensitive, like the project compares paths).
fn under(path: &Path, prefix: &Path) -> bool {
    let key = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let (p, f) = (key(path), key(prefix));
    let f = f.trim_end_matches('\\');
    p.len() > f.len() && p.starts_with(f) && p.as_bytes()[f.len()] == b'\\'
}

/// Re-points everything below `old` to `new`. Returns the ids that changed.
pub fn relink_folder(session: &mut Session, old: &Path, new: &Path) -> Result<Vec<ImageId>, RelinkError> {
    if let Some(e) = busy(session) {
        return Err(e);
    }
    let before: Vec<(ImageId, PathBuf)> = session.project.images().iter().map(|e| (e.id, e.path.clone())).collect();
    let n = session.project.relink_folder(old, new);
    if n == 0 {
        return Err(RelinkError::Other("No file of the project was found in that folder".to_owned()));
    }
    let changed: Vec<ImageId> = before.into_iter().filter(|(id, p)| session.project.image(*id).is_some_and(|e| e.path != *p)).map(|(id, _)| id).collect();
    session.sync_paths();
    session.touch();
    session.refresh_visible();
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::provider;

    fn write_png(path: &Path, shade: u8) {
        image::RgbaImage::from_pixel(16, 12, image::Rgba([shade, 60, 90, 255])).save(path).unwrap();
    }

    /// Project of three PNGs in `job`, plus the ids in file order.
    fn setup(root: &Path) -> (Session, PathBuf, Vec<ImageId>) {
        let job = root.join("job");
        std::fs::create_dir_all(&job).unwrap();
        for (i, n) in ["a.png", "b.png", "c.png"].iter().enumerate() {
            write_png(&job.join(n), 20 + i as u8 * 40);
        }
        let mut p = Project::new();
        p.scan_folder(&job, false, &rawgraph_io::is_supported, &|_| None);
        let s = Session::new(p, provider());
        let ids = s.visible().to_vec();
        (s, job, ids)
    }

    #[test]
    fn locating_the_same_file_elsewhere_succeeds_and_keeps_the_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path());
        s.set_rating(ids[0], 4);
        let moved = tmp.path().join("moved");
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::rename(job.join("a.png"), moved.join("renamed.png")).unwrap();
        s.project.refresh_missing();
        s.refresh_visible();
        assert!(s.entry(ids[0]).unwrap().missing);
        s.dirty = false;
        locate(&mut s, ids[0], &moved.join("renamed.png"), false).unwrap();
        let e = s.entry(ids[0]).unwrap();
        assert!(!e.missing && e.rating == 4, "id, rating and the rest are kept");
        assert!(e.path.ends_with("renamed.png"));
        assert!(s.dirty, "the project is marked unsaved");
        use rawgraph_engine::ImageProvider;
        assert!(s.provider.image(ids[0]).is_ok(), "the provider now reads the new file");
    }

    #[test]
    fn a_different_file_is_a_mismatch_that_can_be_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path());
        let other = tmp.path().join("other.png");
        write_png(&other, 250);
        std::fs::remove_file(job.join("a.png")).unwrap();
        s.project.refresh_missing();
        let err = locate(&mut s, ids[0], &other, false).unwrap_err();
        match &err {
            RelinkError::Mismatch(m) => assert!(m.contains("differs from the original"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(s.entry(ids[0]).unwrap().missing, "nothing changed on a mismatch");
        locate(&mut s, ids[0], &other, true).unwrap();
        let e = s.entry(ids[0]).unwrap();
        assert!(!e.missing && e.path.ends_with("other.png"));
        // A path used by another entry, a directory and a missing file are refused (not mismatches).
        assert!(matches!(locate(&mut s, ids[1], &other, true), Err(RelinkError::Mismatch(_))), "already used by another image");
        assert!(matches!(locate(&mut s, ids[1], tmp.path(), true), Err(RelinkError::Other(_))));
        assert!(matches!(locate(&mut s, ids[1], &tmp.path().join("nope.png"), false), Err(RelinkError::Other(_))));
        // No image with that id.
        assert!(locate(&mut s, ImageId(999), &other, true).is_err());
    }

    #[test]
    fn the_folder_preview_counts_missing_files_found_there() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path());
        let new = tmp.path().join("archive");
        std::fs::create_dir_all(&new).unwrap();
        // a and b moved to the new place, c is gone for good.
        std::fs::rename(job.join("a.png"), new.join("a.png")).unwrap();
        std::fs::rename(job.join("b.png"), new.join("b.png")).unwrap();
        std::fs::remove_file(job.join("c.png")).unwrap();
        s.project.refresh_missing();
        let pre = folder_preview(&s.project, &job, &new);
        assert_eq!(pre, FolderPreview { missing_total: 3, missing_found: 2, also_moved: 0 });
        // The preview did not change the project.
        assert!(s.project.images().iter().all(|e| e.missing && e.path.starts_with(&job)));
        let changed = relink_folder(&mut s, &job, &new).unwrap();
        assert_eq!(changed, vec![ids[0], ids[1]]);
        assert!(!s.entry(ids[0]).unwrap().missing && s.entry(ids[2]).unwrap().missing);
        // A folder that holds none of the files is reported, not applied.
        assert!(relink_folder(&mut s, &job, &tmp.path().join("empty")).is_err());
        // Paths that merely start with the same letters are not "below" the folder.
        assert!(!under(&tmp.path().join("job2").join("x.png"), &job));
        assert!(under(&job.join("x.png"), &job));
    }

    #[test]
    fn live_files_below_the_old_folder_are_reported_as_also_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, _ids) = setup(tmp.path());
        let new = tmp.path().join("copy");
        std::fs::create_dir_all(&new).unwrap();
        for n in ["a.png", "b.png", "c.png"] {
            std::fs::copy(job.join(n), new.join(n)).unwrap();
        }
        std::fs::remove_file(job.join("a.png")).unwrap();
        s.project.refresh_missing();
        let pre = folder_preview(&s.project, &job, &new);
        assert_eq!((pre.missing_total, pre.missing_found, pre.also_moved), (1, 1, 2));
    }

    #[test]
    fn relinking_is_refused_while_scanning_and_for_the_demo() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path());
        s.begin_scan();
        assert!(matches!(locate(&mut s, ids[0], &job.join("a.png"), true), Err(RelinkError::Other(_))));
        s.cancel_scan_state();
        let mut d = crate::session::tests::demo_session();
        assert!(relink_folder(&mut d, &job, &job).is_err());
    }
}
