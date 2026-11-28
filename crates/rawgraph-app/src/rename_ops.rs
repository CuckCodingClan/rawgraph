//! Batch rename of files on disk, as the application drives it: choosing the files, refusing while a
//! scan or export runs, carrying the rename out and pointing the project at the new names. The file
//! work itself (preview, two-phase rename, verification, rollback, undo log) is `rawgraph_rename`;
//! this module adds the project: every renamed file that is an image of the project keeps its id,
//! hash, rating and edits, only its path changes, and the project is marked changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rawgraph_core::ImageId;
use rawgraph_rename::names::key;
use rawgraph_rename::{
    CaptionSheet, CaseChange, Control, Counter, ExtCase, Move, NumberPattern, Plan, RealFs, RenameLog, RenameRules, RunState, SheetBinding, SheetColumns, SourceFile,
};

use crate::datadir::DataDirs;
use crate::session::Session;

/// What the user typed in the naming section of the dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleFields {
    pub template: String,
    pub counter_start: i64,
    pub counter_step: i64,
    pub counter_pad: usize,
    pub find: String,
    pub replace: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub case: CaseChange,
    pub ext_case: ExtCase,
    /// Rename same-stem files (RAW + JPEG) together.
    pub pairs: bool,
    /// Where the photo number sits in a file name: `last`, `first` or a regular expression with a group.
    pub pattern: String,
}

impl Default for RuleFields {
    fn default() -> Self {
        RuleFields {
            template: "{stem}".to_owned(),
            counter_start: 1,
            counter_step: 1,
            counter_pad: 3,
            find: String::new(),
            replace: String::new(),
            regex: false,
            case_sensitive: true,
            case: CaseChange::None,
            ext_case: ExtCase::Keep,
            pairs: false,
            pattern: "last".to_owned(),
        }
    }
}

impl RuleFields {
    pub fn rules(&self) -> Result<RenameRules, String> {
        Ok(RenameRules {
            template: self.template.clone(),
            counter: Counter { start: self.counter_start, step: self.counter_step, pad: self.counter_pad },
            find: self.find.clone(),
            replace: self.replace.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            case: self.case,
            ext_case: self.ext_case,
            number_pattern: NumberPattern::parse(&self.pattern)?,
        })
    }
}

/// The sheet section of the dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct SheetFields {
    pub path: String,
    pub col_number: String,
    pub col_name: String,
    pub col_caption: String,
    /// Sheet (tab) name; empty = the first one.
    pub tab: String,
}

impl Default for SheetFields {
    fn default() -> Self {
        SheetFields { path: String::new(), col_number: "A".into(), col_name: "B".into(), col_caption: "C".into(), tab: String::new() }
    }
}

/// Reads the sheet file with the chosen columns and number pattern.
pub fn load_sheet(f: &SheetFields, pattern: &str) -> Result<SheetBinding, String> {
    let col = |s: &str, what: &str| rawgraph_rename::sheet::column_from_letters(s).ok_or_else(|| format!("{what} column {s:?} is not a column letter (A, B, ...)"));
    let cols = SheetColumns { number: col(&f.col_number, "Number")?, name: col(&f.col_name, "Name")?, caption: col(&f.col_caption, "Caption")? };
    let pattern = NumberPattern::parse(pattern)?;
    let tab = f.tab.trim();
    let sheet = CaptionSheet::load(Path::new(f.path.trim()), &cols, (!tab.is_empty()).then_some(tab)).map_err(|e| e.to_string())?;
    Ok(SheetBinding { sheet, pattern })
}

/// The chosen images as rename sources, in the given order (the counter follows it). Images the session does
/// not know are left out.
pub fn sources(session: &Session, ids: &[ImageId]) -> Vec<SourceFile> {
    ids.iter()
        .filter_map(|id| session.entry(*id))
        .map(|e| {
            let camera = match (&e.capture.make, &e.capture.model) {
                (_, Some(model)) if !model.trim().is_empty() => Some(model.clone()),
                (Some(make), _) if !make.trim().is_empty() => Some(make.clone()),
                _ => None,
            };
            SourceFile { key: e.id.0, path: e.path.clone(), capture_unix_ms: e.capture.captured_unix_ms, camera }
        })
        .collect()
}

/// What else is running in the application.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    pub scanning: bool,
    pub exporting: bool,
    /// Another job that reads the image files (named for the message).
    pub other: Option<&'static str>,
}

/// Why a rename must not run now, in plain words. The rename tool refuses while anything works on the files.
pub fn refusal(session: &Session, a: &Activity) -> Option<String> {
    if session.demo {
        return Some("The demo images are not files; open a folder or a project first".to_owned());
    }
    if session.scanning || a.scanning {
        return Some("A folder scan is running; wait until it has finished".to_owned());
    }
    if session.export_busy || a.exporting {
        return Some("An export is running; wait until it has finished".to_owned());
    }
    a.other.map(|o| format!("{o} is running; wait until it has finished"))
}

/// The undo log of this project: `<project>.rename-log.json` next to the project file, or one file in the
/// application data folder while the project is not saved.
pub fn log_path(session: &Session, dirs: &DataDirs) -> PathBuf {
    match &session.project_path {
        Some(p) => {
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "project".to_owned());
            p.with_file_name(format!("{stem}.rename-log.json"))
        }
        None => dirs.rename_log(),
    }
}

/// Whether "Undo last rename" has something to undo, and how many interrupted runs the journal holds.
pub fn log_state(path: &Path) -> (bool, usize) {
    match RenameLog::load(path) {
        Ok(l) => (l.last_undoable().is_some(), l.interrupted()),
        Err(_) => (false, 0),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Files whose name changed on disk (sidecars and pairs included).
    pub files: usize,
    /// Images of the project that now point at the new names.
    pub images: usize,
    pub warning: Option<String>,
}

/// What [`heal_from_log`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Healed {
    pub relinked: usize,
    pub interrupted: usize,
}

/// For a project opened after a rename it was not saved after: every finished run of the log (oldest
/// first) whose file is gone from its old name and present at its new one moves the project's image to
/// the new name. Nothing on disk is touched. Also counts interrupted runs.
pub fn heal_from_log(session: &mut Session, log_path: &Path) -> Result<Healed, String> {
    let log = RenameLog::load(log_path)?;
    let mut healed = Healed { interrupted: log.interrupted(), ..Default::default() };
    for run in log.runs.iter().filter(|r| matches!(r.state, RunState::Done | RunState::Undone)) {
        let moves: Vec<Move> = run
            .entries
            .iter()
            .filter(|e| !e.old.exists() && e.new.is_file())
            .map(|e| Move { old: e.old.clone(), new: e.new.clone(), size: e.size, exact_size: e.exact_size })
            .collect();
        if !moves.is_empty() {
            healed.relinked += relink(session, &moves)?;
        }
    }
    Ok(healed)
}

/// Points the project at renamed files. All-or-nothing; files that are not images of the project (sidecars,
/// other formats) are skipped. Marks the project changed.
pub fn relink(session: &mut Session, moves: &[Move]) -> Result<usize, String> {
    let by_path: HashMap<String, ImageId> = session.project.images().iter().map(|e| (key(&e.path), e.id)).collect();
    let pairs: Vec<(ImageId, PathBuf)> = moves.iter().filter_map(|m| by_path.get(&key(&m.old)).map(|id| (*id, m.new.clone()))).collect();
    if pairs.is_empty() {
        return Ok(0);
    }
    let n = session.project.apply_renames(&pairs).map_err(|e| e.to_string())?;
    session.sync_paths();
    session.touch();
    session.refresh_visible();
    Ok(n)
}

/// How the project follows renamed files (replaceable in tests).
type Follow<'a> = &'a dyn Fn(&mut Session, &[Move]) -> Result<usize, String>;

/// Whether any file of the journal entries is an image of the open project (a log of another project, or of
/// another unsaved session, must not be undone here).
fn belongs_to_project(session: &Session, entries: &[rawgraph_rename::log::LogEntry]) -> bool {
    let keys: std::collections::HashSet<String> = session.project.images().iter().map(|e| key(&e.path)).collect();
    entries.iter().any(|e| keys.contains(&key(&e.old)) || keys.contains(&key(&e.new)))
}

/// Why the log cannot be used for this project, if so (the message says why).
pub fn log_refusal(session: &Session, log: &Path, undo: bool) -> Option<String> {
    let l = match RenameLog::load(log) {
        Ok(l) => l,
        Err(e) => return Some(format!("The rename log is unusable: {e}")),
    };
    let runs: Vec<&rawgraph_rename::RunRecord> = if undo {
        l.last_undoable().into_iter().collect()
    } else {
        l.runs.iter().filter(|r| matches!(r.state, RunState::Started | RunState::Phase1Done | RunState::RollingBack)).collect()
    };
    if runs.iter().any(|r| !belongs_to_project(session, &r.entries)) {
        return Some("The last rename in the log was done for another project (none of its files are images of this project); it is not undone or recovered here".to_owned());
    }
    None
}

/// What happens after the files are renamed: the project follows; if it cannot, the files are renamed back.
fn follow_up(session: &mut Session, log: &Path, report: rawgraph_rename::RunReport, follow: Follow) -> Result<Outcome, String> {
    match follow(session, &report.moved) {
        Ok(images) => Ok(Outcome { files: report.moved.len(), images, warning: report.log_warning }),
        Err(e) => match rawgraph_rename::undo_last(log, &RealFs) {
            Ok(_) => Err(format!("The project could not follow the new names ({e}); the files were renamed back")),
            Err(u) => Err(format!("The project could not follow the new names ({e}) and putting the files back failed: {u}")),
        },
    }
}

/// Renames the files of `plan` on disk and re-points the project. If the project cannot follow, the files are
/// put back and the error says so.
#[cfg(test)]
pub fn run(session: &mut Session, plan: &Plan, log: &Path, activity: &Activity) -> Result<Outcome, String> {
    run_with(session, plan, log, activity, &relink)
}

#[cfg(test)]
pub(crate) fn run_with(session: &mut Session, plan: &Plan, log: &Path, activity: &Activity, follow: Follow) -> Result<Outcome, String> {
    if let Some(r) = refusal(session, activity) {
        return Err(r);
    }
    let label = format!("{} file(s) in {}", plan.change_count(), session.project_name());
    let report = rawgraph_rename::execute(plan, &RealFs, Some(log), &label).map_err(|e| e.to_string())?;
    follow_up(session, log, report, follow)
}

/// Reverses the last rename of the log (same two-phase method, same checks) and re-points the project.
#[cfg(test)]
pub fn undo(session: &mut Session, log: &Path, activity: &Activity) -> Result<Outcome, String> {
    if let Some(r) = refusal(session, activity) {
        return Err(r);
    }
    if let Some(r) = log_refusal(session, log, true) {
        return Err(r);
    }
    let report = rawgraph_rename::undo_last(log, &RealFs).map_err(|e| e.to_string())?;
    undo_follow_up(session, report)
}

fn undo_follow_up(session: &mut Session, report: rawgraph_rename::RunReport) -> Result<Outcome, String> {
    let images = relink(session, &report.moved).map_err(|e| format!("The files were renamed back, but the project could not follow: {e}"))?;
    Ok(Outcome { files: report.moved.len(), images, warning: report.log_warning })
}

/// Finishes the runs a crash or power loss interrupted: a run whose files are all at their new names is recorded as
/// done and the project follows; any other is put back through the temporary names and the project points at the
/// old names again. Refused while a scan, export or other job runs, and for a log of another project.
pub fn recover(session: &mut Session, log: &Path, activity: &Activity) -> Result<String, String> {
    if let Some(r) = refusal(session, activity) {
        return Err(r);
    }
    if let Some(r) = log_refusal(session, log, false) {
        return Err(r);
    }
    let rep = rawgraph_rename::recover_interrupted(log, &RealFs).map_err(|e| e.to_string())?;
    let followed = relink(session, &rep.relink).map_err(|e| format!("The files are in order, but the project could not follow: {e}"))?;
    session.project.refresh_missing();
    session.refresh_visible();
    let mut text = if rep.lines.is_empty() { "Nothing to put back".to_owned() } else { rep.lines.join("; ") };
    if followed > 0 {
        text.push_str(&format!("; {followed} image(s) of the project follow"));
    }
    if rep.stuck_runs > 0 {
        return Err(format!("{text}. {} run(s) could not be finished; the files are named above", rep.stuck_runs));
    }
    Ok(text)
}

/// A rename or undo running on its own thread (so the window stays alive; cancel puts everything back).
pub struct RenameJob {
    pub rx: std::sync::mpsc::Receiver<Result<rawgraph_rename::RunReport, rawgraph_rename::RunError>>,
    pub control: Control,
    pub undo: bool,
    pub log: PathBuf,
}

impl RenameJob {
    /// Starts the rename of `plan` (or the undo of the log's last rename when `plan` is `None`). `wake` is called
    /// when the job is done.
    pub fn start(plan: Option<Plan>, log: PathBuf, label: String, wake: impl Fn() + Send + 'static) -> RenameJob {
        let (tx, rx) = std::sync::mpsc::channel();
        let control = Control::default();
        let (c2, l2, undo) = (control.clone(), log.clone(), plan.is_none());
        let _ = std::thread::Builder::new().name("rawgraph-rename".into()).spawn(move || {
            let result = match plan {
                Some(p) => rawgraph_rename::execute_with(&p, &RealFs, Some(&l2), &label, &c2),
                None => rawgraph_rename::undo_last_with(&l2, &RealFs, &c2),
            };
            let _ = tx.send(result);
            wake();
        });
        RenameJob { rx, control, undo, log }
    }

    /// (done, total) of the files renamed so far.
    pub fn progress(&self) -> (usize, usize) {
        (self.control.done.load(std::sync::atomic::Ordering::Relaxed), self.control.total.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn cancel(&self) {
        self.control.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Applies a finished job to the session (the project follows, or the files are put back).
pub fn finish_job(session: &mut Session, job: &RenameJob, result: Result<rawgraph_rename::RunReport, rawgraph_rename::RunError>) -> Result<Outcome, String> {
    let report = result.map_err(|e| e.to_string())?;
    if job.undo {
        undo_follow_up(session, report)
    } else {
        follow_up(session, &job.log, report, &relink)
    }
}

/// The sheet as the export and the dialog share it.
pub fn shared(binding: SheetBinding) -> Arc<SheetBinding> {
    Arc::new(binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::{demo_session, provider};
    use rawgraph_project::Project;
    use rawgraph_rename::{build, PlanOptions, RunState};

    fn write_png(path: &Path, shade: u8) {
        image::RgbaImage::from_pixel(16, 12, image::Rgba([shade, 60, 90, 255])).save(path).unwrap();
    }

    /// Project of PNGs `names` (distinct pictures) in a folder of its own, plus the ids in file order.
    fn setup(root: &Path, names: &[&str]) -> (Session, PathBuf, Vec<ImageId>) {
        let job = root.join("job");
        std::fs::create_dir_all(&job).unwrap();
        for (i, n) in names.iter().enumerate() {
            write_png(&job.join(n), 20 + i as u8 * 40);
        }
        let mut p = Project::new();
        p.scan_folder(&job, false, &rawgraph_io::is_supported, &|_| None);
        let s = Session::new(p, provider());
        let ids = s.visible().to_vec();
        (s, job, ids)
    }

    fn plan_for(s: &Session, ids: &[ImageId], rules: &RuleFields) -> Plan {
        build(&sources(s, ids), &rules.rules().unwrap(), s.caption_sheet.as_ref().map(|b| &b.sheet), &PlanOptions { pairs: rules.pairs }).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn counter_rules(start: i64) -> RuleFields {
        RuleFields { template: "{counter}".into(), counter_start: start, counter_pad: 1, ..Default::default() }
    }

    #[test]
    fn project_images_keep_id_rating_and_pixels_through_a_chain_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["1.png", "2.png", "3.png"]);
        let hashes: Vec<String> = ids.iter().map(|i| s.entry(*i).unwrap().content_hash.clone()).collect();
        s.set_rating(ids[0], 4);
        s.set_rating(ids[2], 2);
        s.dirty = false;
        let log = log_path(&s, &DataDirs::new(tmp.path().join("data")));
        // 1 -> 2, 2 -> 3, 3 -> 4: every new name but the last is an old name of the set.
        let plan = plan_for(&s, &ids, &counter_rules(2));
        assert!(!plan.blocked());
        let out = run(&mut s, &plan, &log, &Activity::default()).unwrap();
        assert_eq!((out.files, out.images), (3, 3));
        assert_eq!(names(&job), vec!["2.png", "3.png", "4.png"]);
        for (i, id) in ids.iter().enumerate() {
            let e = s.entry(*id).unwrap();
            assert_eq!(e.file_name(), format!("{}.png", i + 2), "id {} follows its own file", id.0);
            assert_eq!(e.content_hash, hashes[i], "the hash is kept");
            assert!(!e.missing);
        }
        assert_eq!((s.entry(ids[0]).unwrap().rating, s.entry(ids[2]).unwrap().rating), (4, 2));
        assert!(s.dirty, "the project is marked changed");
        use rawgraph_engine::ImageProvider;
        assert!(s.provider.image(ids[0]).is_ok(), "the provider reads the new file");
        // The picture of image 1 is still the picture with shade 20.
        let img = image::open(&s.entry(ids[0]).unwrap().path).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(0, 0).0[0], 20);
    }

    #[test]
    fn undo_restores_names_and_project_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png", "b.png"]);
        let log = tmp.path().join("job.rename-log.json");
        let rules = RuleFields { template: "{counter}_x".into(), ..Default::default() };
        let plan = plan_for(&s, &ids, &rules);
        run(&mut s, &plan, &log, &Activity::default()).unwrap();
        assert_eq!(names(&job), vec!["001_x.png", "002_x.png"]);
        assert_eq!(log_state(&log), (true, 0));
        s.dirty = false;
        let out = undo(&mut s, &log, &Activity::default()).unwrap();
        assert_eq!((out.files, out.images), (2, 2));
        assert_eq!(names(&job), vec!["a.png", "b.png"]);
        assert_eq!(s.entry(ids[0]).unwrap().file_name(), "a.png");
        assert!(s.dirty);
        assert_eq!(log_state(&log), (false, 0), "nothing left to undo");
        assert!(undo(&mut s, &log, &Activity::default()).is_err());
        let l = RenameLog::load(&log).unwrap();
        assert!(l.runs.iter().any(|r| r.state == RunState::Undone));
    }

    #[test]
    fn a_swap_of_two_names_works_and_sidecars_follow() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["1.png", "2.png"]);
        std::fs::write(job.join("1.xmp"), "x1").unwrap();
        let log = tmp.path().join("l.json");
        // counter 2, step -1: 1 -> 2 and 2 -> 1 (a swap), the sidecar of 1 becomes 2.xmp
        let rules = RuleFields { template: "{counter}".into(), counter_start: 2, counter_step: -1, counter_pad: 1, ..Default::default() };
        let plan = plan_for(&s, &ids, &rules);
        assert!(!plan.blocked());
        run(&mut s, &plan, &log, &Activity::default()).unwrap();
        assert_eq!(names(&job), vec!["1.png", "2.png", "2.xmp"]);
        assert_eq!(std::fs::read_to_string(job.join("2.xmp")).unwrap(), "x1");
        assert_eq!(s.entry(ids[0]).unwrap().file_name(), "2.png");
        assert_eq!(s.entry(ids[1]).unwrap().file_name(), "1.png");
    }

    #[test]
    fn the_tool_refuses_while_a_scan_or_export_runs_and_for_the_demo() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png", "b.png"]);
        let log = tmp.path().join("l.json");
        let plan = plan_for(&s, &ids, &counter_rules(5));
        let before = names(&job);
        s.begin_scan();
        let e = run(&mut s, &plan, &log, &Activity::default()).unwrap_err();
        assert!(e.contains("scan"), "{e}");
        s.cancel_scan_state();
        assert!(run(&mut s, &plan, &log, &Activity { scanning: true, ..Default::default() }).unwrap_err().contains("scan"));
        s.export_busy = true;
        assert!(run(&mut s, &plan, &log, &Activity::default()).unwrap_err().contains("export"));
        s.export_busy = false;
        assert!(run(&mut s, &plan, &log, &Activity { exporting: true, ..Default::default() }).unwrap_err().contains("export"));
        assert!(run(&mut s, &plan, &log, &Activity { other: Some("The XMP import"), ..Default::default() }).unwrap_err().contains("XMP import"));
        assert!(undo(&mut s, &log, &Activity { exporting: true, ..Default::default() }).is_err());
        assert_eq!(names(&job), before, "nothing was renamed");
        assert!(!log.exists(), "and no log was written");
        let mut d = demo_session();
        assert!(refusal(&d, &Activity::default()).unwrap().contains("demo"));
        d.demo = false;
        assert!(refusal(&d, &Activity::default()).is_none());
        // Once idle, the same plan runs.
        assert!(run(&mut s, &plan, &log, &Activity::default()).is_ok());
    }

    #[test]
    fn a_project_that_cannot_follow_gets_its_files_back() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png", "b.png"]);
        let log = tmp.path().join("l.json");
        let plan = plan_for(&s, &ids, &counter_rules(7));
        let fail = |_: &mut Session, _: &[Move]| -> Result<usize, String> { Err("boom".to_owned()) };
        let e = run_with(&mut s, &plan, &log, &Activity::default(), &fail).unwrap_err();
        assert!(e.contains("boom") && e.contains("renamed back"), "{e}");
        assert_eq!(names(&job), vec!["a.png", "b.png"]);
        assert_eq!(s.entry(ids[0]).unwrap().file_name(), "a.png");
    }

    #[test]
    fn relink_is_all_or_nothing_and_skips_files_outside_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png", "b.png"]);
        let a = s.entry(ids[0]).unwrap().path.clone();
        // A sidecar-like move for a file that is no image of the project is ignored.
        let other = Move { old: job.join("x.xmp"), new: job.join("y.xmp"), size: 1, exact_size: true };
        assert_eq!(relink(&mut s, std::slice::from_ref(&other)).unwrap(), 0);
        // A target that does not exist: error, nothing changed.
        let bad = Move { old: a.clone(), new: job.join("nope.png"), size: 1, exact_size: true };
        assert!(relink(&mut s, &[bad]).is_err());
        assert_eq!(s.entry(ids[0]).unwrap().path, a);
    }

    #[test]
    fn log_paths_sources_and_sheet_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, _job, ids) = setup(tmp.path(), &["Job-0076.png", "Job-0077.png"]);
        let dirs = DataDirs::new(tmp.path().join("data"));
        assert_eq!(log_path(&s, &dirs), dirs.rename_log(), "unsaved project: the data folder");
        s.project_path = Some(tmp.path().join("proj").join("wedding.rawgraph"));
        assert_eq!(log_path(&s, &dirs), tmp.path().join("proj").join("wedding.rename-log.json"));
        let src = sources(&s, &ids);
        assert_eq!(src.len(), 2);
        assert!(src[0].path.ends_with("Job-0076.png"));
        // sheet from csv with letters other than the defaults
        let csv = tmp.path().join("c.csv");
        std::fs::write(&csv, "x;Nr;Name;Caption\n;76;076_Anna;Anna\n;77;077_Bob;Bob\n").unwrap();
        let f = SheetFields { path: csv.to_string_lossy().into_owned(), col_number: "b".into(), col_name: "C".into(), col_caption: "D".into(), tab: String::new() };
        let b = load_sheet(&f, "last").unwrap();
        assert_eq!(b.sheet.lookup(77).unwrap().name, "077_Bob");
        assert!(load_sheet(&SheetFields { col_name: "1".into(), ..f.clone() }, "last").is_err());
        assert!(load_sheet(&f, "(").is_err());
        assert!(load_sheet(&SheetFields { path: "nope.csv".into(), ..f }, "last").is_err());
        // A sheet-based rename end to end.
        s.caption_sheet = Some(shared(b));
        let rules = RuleFields { template: "{sheet_name}".into(), ..Default::default() };
        let plan = plan_for(&s, &ids, &rules);
        assert_eq!(plan.entries.iter().map(|e| e.new_name()).collect::<Vec<_>>(), vec!["076_Anna.png", "077_Bob.png"]);
    }

    #[test]
    fn a_log_of_another_project_is_neither_undone_nor_recovered() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, _job, ids) = setup(tmp.path(), &["a.png", "b.png"]);
        let log = tmp.path().join("shared-unsaved.json");
        let plan = plan_for(&s, &ids, &counter_rules(5));
        run(&mut s, &plan, &log, &Activity::default()).unwrap();
        // A second session (another project) uses the same log file.
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        let (mut s2, _j2, _ids2) = setup(&other, &["c.png"]);
        let e = undo(&mut s2, &log, &Activity::default()).unwrap_err();
        assert!(e.contains("another project"), "{e}");
        let e = log_refusal(&s2, &log, true).unwrap();
        assert!(e.contains("not undone"), "{e}");
        assert!(log_refusal(&s, &log, true).is_none(), "its own project may undo");
        // a broken log says what is wrong
        std::fs::write(&log, "{ not json").unwrap();
        assert!(undo(&mut s, &log, &Activity::default()).unwrap_err().contains("log"));
        assert!(log_refusal(&s, &log, false).unwrap().contains("unusable"));
    }

    #[test]
    fn recovery_makes_the_project_follow_a_completed_swap() {
        use rawgraph_rename::log::LogEntry;
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png", "b.png"]);
        let (a, b) = (job.join("a.png"), job.join("b.png"));
        let (sa, sb) = (std::fs::metadata(&a).unwrap().len(), std::fs::metadata(&b).unwrap().len());
        let log = tmp.path().join("l.json");
        let mut l = RenameLog::default();
        l.runs.push(rawgraph_rename::RunRecord {
            id: "done-but-unsaved".into(),
            started_unix_ms: 0,
            label: "x".into(),
            kind: rawgraph_rename::RunKind::Rename,
            // the final save of the log never happened
            state: RunState::Phase1Done,
            entries: vec![LogEntry { old: a.clone(), new: b.clone(), size: sa, exact_size: true, stamp: None }, LogEntry { old: b.clone(), new: a.clone(), size: sb, exact_size: true, stamp: None }],
        });
        l.save(&log).unwrap();
        // On disk the swap is complete; the project still has the old paths.
        let tmpname = job.join("swap.tmp");
        std::fs::rename(&a, &tmpname).unwrap();
        std::fs::rename(&b, &a).unwrap();
        std::fs::rename(&tmpname, &b).unwrap();
        assert_eq!(log_state(&log).1, 1);
        // A new rename is refused while the journal holds the interrupted run.
        let plan = plan_for(&s, &ids, &counter_rules(5));
        assert!(run(&mut s, &plan, &log, &Activity::default()).unwrap_err().contains("Recover"));
        // Recovery is refused while something else runs ...
        assert!(recover(&mut s, &log, &Activity { exporting: true, ..Default::default() }).is_err());
        // ... and otherwise records the run as done and points the project at the new names.
        let msg = recover(&mut s, &log, &Activity::default()).unwrap();
        assert!(msg.contains("already at their new names") && msg.contains("2 image(s) of the project follow"), "{msg}");
        assert_eq!(s.entry(ids[0]).unwrap().file_name(), "b.png");
        assert_eq!(s.entry(ids[1]).unwrap().file_name(), "a.png");
        assert!(s.dirty);
        assert_eq!(log_state(&log), (true, 0), "the run is done and can be undone");
        undo(&mut s, &log, &Activity::default()).unwrap();
        assert_eq!(s.entry(ids[0]).unwrap().file_name(), "a.png");
    }

    #[test]
    fn a_crash_journal_is_recovered() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png"]);
        let log = tmp.path().join("l.json");
        // The rename failed in a way that left the journal "Started": simulate by writing it by hand.
        let mut l = RenameLog::default();
        l.runs.push(rawgraph_rename::RunRecord {
            id: "crash".into(),
            started_unix_ms: 0,
            label: "x".into(),
            kind: rawgraph_rename::RunKind::Rename,
            state: RunState::Started,
            entries: vec![rawgraph_rename::log::LogEntry { old: job.join("a.png"), new: job.join("b.png"), size: std::fs::metadata(job.join("a.png")).unwrap().len(), exact_size: true, stamp: None }],
        });
        l.save(&log).unwrap();
        std::fs::rename(job.join("a.png"), job.join(".rgrename-crash-0.tmp")).unwrap();
        assert_eq!(log_state(&log).1, 1);
        let msg = recover(&mut s, &log, &Activity::default()).unwrap();
        assert!(msg.contains("put back under their old names"), "{msg}");
        assert_eq!(names(&job), vec!["a.png"]);
        assert_eq!(log_state(&log).1, 0);
        assert!(!s.entry(ids[0]).unwrap().missing);
    }

    #[test]
    fn opening_a_project_that_was_not_saved_after_a_rename_follows_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let (mut s, job, ids) = setup(tmp.path(), &["a.png", "c.png"]);
        let log = tmp.path().join("l.json");
        let size = std::fs::metadata(job.join("a.png")).unwrap().len();
        let mut l = RenameLog::default();
        l.runs.push(rawgraph_rename::RunRecord {
            id: "done".into(),
            started_unix_ms: 0,
            label: "x".into(),
            kind: rawgraph_rename::RunKind::Rename,
            state: RunState::Done,
            entries: vec![rawgraph_rename::log::LogEntry { old: job.join("a.png"), new: job.join("b.png"), size, exact_size: true, stamp: None }],
        });
        l.save(&log).unwrap();
        std::fs::rename(job.join("a.png"), job.join("b.png")).unwrap();
        let h = heal_from_log(&mut s, &log).unwrap();
        assert_eq!(h, Healed { relinked: 1, interrupted: 0 });
        assert_eq!(s.entry(ids[0]).unwrap().file_name(), "b.png");
        assert_eq!(s.entry(ids[1]).unwrap().file_name(), "c.png", "untouched");
        // A second look changes nothing (the old name is gone, the project already follows).
        assert_eq!(heal_from_log(&mut s, &log).unwrap().relinked, 0);
        // Undone runs whose files are back at the old names are not followed.
        std::fs::rename(job.join("b.png"), job.join("a.png")).unwrap();
        let mut l = RenameLog::load(&log).unwrap();
        l.runs[0].state = RunState::Undone;
        l.save(&log).unwrap();
        let (mut s2, _, ids2) = setup(&tmp.path().join("second"), &["a.png"]);
        let _ = ids2;
        assert_eq!(heal_from_log(&mut s2, &log).unwrap().relinked, 0);
    }
}
