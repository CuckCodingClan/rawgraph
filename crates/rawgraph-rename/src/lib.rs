//! RawGraph batch rename: caption sheets, naming rules and the safe two-phase rename of files on
//! disk. UI-independent; the application maps it onto the project (ids, relinking) and the dialogs.
//!
//! - [`sheet`]: reads a caption sheet (.xlsx / .csv): photo number -> target name and caption.
//! - [`numbers`]: finds the photo number in a file name.
//! - [`rules`]: template tokens, find/replace, case change.
//! - [`plan`]: old -> new for every file (with sidecars and pairs) and every conflict.
//! - [`blocks`]: the block-boundary check for sheet-based renames.
//! - [`exec`]: the two-phase rename with verification, rollback, undo and journal recovery.
//! - [`log`]: the undo log file.

pub mod blocks;
pub mod exec;
pub mod log;
pub mod names;
pub mod numbers;
pub mod plan;
pub mod rules;
pub mod sheet;

#[cfg(test)]
mod xlsx_tests;

pub use blocks::{check_blocks, BlockReport, BlockView};
pub use exec::{execute, execute_with, recover_interrupted, run_moves, undo_last, undo_last_with, Control, Fs, RealFs, RecoverReport, RunError, RunFailure, RunReport};
pub use log::{RenameLog, RunKind, RunRecord, RunState};
pub use numbers::NumberPattern;
pub use plan::{build, Entry, EntryKind, Move, Plan, PlanOptions, Problem, SourceFile};
pub use rules::{CaseChange, Counter, ExtCase, RenameRules};
pub use sheet::{CaptionSheet, Resolved, SheetBinding, SheetColumns, SheetError};
