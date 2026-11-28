//! What happens when saving a project as a new revision goes wrong in a way the
//! user has to decide: somebody else saved in between (conflict), or the target file
//! is another project. A small state machine; the window only draws it.
//!
//! `P` is what the application wanted to do after the save (quit, open another
//! project, ...); a save that does not finish never lets that happen.

use std::path::{Path, PathBuf};

use rawgraph_project::ProjectError;

use crate::revisions;
use crate::session::Session;

/// The question that is open.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage<P> {
    Idle,
    /// Somebody else saved since this copy was opened.
    Conflict { target: PathBuf, disk_revision: u64, loaded_revision: u64, then: Option<P> },
    /// "Overwrite their changes?" with what is lost.
    ConfirmOverwrite { target: PathBuf, lost: String, then: Option<P> },
    /// The target file belongs to another project.
    ConfirmReplace { target: PathBuf, then: Option<P> },
}

/// What a save attempt led to.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome<P> {
    /// The new revision is written; `then` may be carried out now.
    Saved { revision: u64, target: PathBuf, then: Option<P> },
    /// A question is open (see [`SaveFlow::stage`]); nothing was written.
    Asked,
    /// The project file has an unfinished save beside it (journal of an interrupted save): nothing
    /// was written; the user decides whether it may be rolled back, then the save is tried again.
    Unfinished { target: PathBuf, then: Option<P> },
    /// Nothing was written; the text is for the status bar.
    Failed(String),
}

#[derive(Debug)]
pub struct SaveFlow<P> {
    pub stage: Stage<P>,
}

impl<P> Default for SaveFlow<P> {
    fn default() -> Self {
        Self { stage: Stage::Idle }
    }
}

impl<P: Clone> SaveFlow<P> {
    pub fn is_open(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }

    /// Saves the project to `target` as a new revision.
    pub fn save(&mut self, session: &mut Session, target: &Path, then: Option<P>) -> Outcome<P> {
        self.attempt(session, target, then, false)
    }

    fn attempt(&mut self, session: &mut Session, target: &Path, then: Option<P>, overwrite: bool) -> Outcome<P> {
        match session.commit_project(target, overwrite) {
            Ok(revision) => {
                self.stage = Stage::Idle;
                Outcome::Saved { revision, target: target.to_path_buf(), then }
            }
            Err(ProjectError::Conflict { disk_revision, loaded_revision }) => {
                self.stage = Stage::Conflict { target: target.to_path_buf(), disk_revision, loaded_revision, then };
                Outcome::Asked
            }
            Err(ProjectError::Mismatch(m)) if m.contains("belongs to another project") || m.contains("old project file") => {
                self.stage = Stage::ConfirmReplace { target: target.to_path_buf(), then };
                Outcome::Asked
            }
            Err(ProjectError::UnfinishedSave(_)) => {
                self.stage = Stage::Idle;
                Outcome::Unfinished { target: target.to_path_buf(), then }
            }
            Err(ProjectError::TooNew { file_version, supported }) => {
                self.stage = Stage::Idle;
                Outcome::Failed(format!("Cannot save {}: {}", target.display(), revisions::too_new_text(file_version, supported)))
            }
            Err(e) => {
                self.stage = Stage::Idle;
                Outcome::Failed(format!("Cannot save {}: {e}", target.display()))
            }
        }
    }

    /// Conflict, "Save as copy...": closes the question. The name to offer in the Save as dialog
    /// and what was to happen afterwards.
    pub fn choose_copy(&mut self, project_name: &str, date: &str) -> Option<(String, Option<P>)> {
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Conflict { then, .. } => Some((revisions::conflict_copy_name(project_name, date), then)),
            other => {
                self.stage = other;
                None
            }
        }
    }

    /// Conflict, "Overwrite their changes": asks again, naming what is lost.
    pub fn choose_overwrite(&mut self) {
        if let Stage::Conflict { target, disk_revision, loaded_revision, then } = std::mem::replace(&mut self.stage, Stage::Idle) {
            let lost = revisions::lost_changes_text(&target, loaded_revision, disk_revision);
            self.stage = Stage::ConfirmOverwrite { target, lost, then };
        }
    }

    /// The second confirmation of an overwrite, or "Replace the other project": writes over the target.
    pub fn confirm(&mut self, session: &mut Session) -> Outcome<P> {
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::ConfirmOverwrite { target, then, .. } | Stage::ConfirmReplace { target, then } => self.attempt(session, &target, then, true),
            other => {
                self.stage = other;
                Outcome::Failed(String::new())
            }
        }
    }

    /// Closes any question; nothing is written and the project stays changed.
    pub fn cancel(&mut self) {
        self.stage = Stage::Idle;
    }
}
