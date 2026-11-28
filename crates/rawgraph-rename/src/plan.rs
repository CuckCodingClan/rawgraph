//! The rename plan: for every file the old and the new name, and every reason why the run must not
//! start. Nothing here changes a file; the plan only looks.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::names::{check_name, key, NameProblem};
use crate::rules::{FileFacts, RenameRules};
use crate::sheet::{CaptionSheet, LookupError};

/// One file the user chose (an image of the project).
#[derive(Clone, Debug, PartialEq)]
pub struct SourceFile {
    /// The caller's identifier (the project's image id).
    pub key: u64,
    pub path: PathBuf,
    pub capture_unix_ms: Option<i64>,
    pub camera: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlanOptions {
    /// Files with the same stem in the same folder (RAW + JPEG) are renamed with the chosen file.
    /// `.xmp` sidecars always are.
    pub pairs: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// A file the user chose.
    Main,
    /// Its same-stem `.xmp` / `.XMP`.
    Sidecar,
    /// Another file with the same stem (RAW + JPEG).
    Pair,
}

/// A reason why a run must not start. Every one of them blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    MissingSource,
    NotAFile,
    /// The sheet has no (unique) row for this file, or the name has no number.
    Sheet(String),
    /// The template cannot be filled for this file (no capture date, negative counter, ...).
    Template(String),
    Name(NameProblem),
    /// Another entry of the plan gets the same new name (the other entry's old name).
    DuplicateTarget(PathBuf),
    /// A file with the new name exists and is not part of the plan.
    TargetExists,
}

impl Problem {
    pub fn message(&self) -> String {
        match self {
            Problem::MissingSource => "the file does not exist any more".into(),
            Problem::NotAFile => "this is not a file".into(),
            Problem::Sheet(m) | Problem::Template(m) => m.clone(),
            Problem::Name(n) => n.message(),
            Problem::DuplicateTarget(p) => format!("the same new name is also given to {}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            Problem::TargetExists => "a file with this name already exists and is not part of the rename".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// `key` of the chosen file this entry belongs to.
    pub item: u64,
    pub kind: EntryKind,
    pub old: PathBuf,
    /// Same folder as `old`. Equal to `old` for an entry that stays as it is (and for an entry with a problem
    /// that prevents a name).
    pub new: PathBuf,
    pub size: u64,
    pub problems: Vec<Problem>,
    /// Photo number found in the name (main entries).
    pub number: Option<u64>,
    /// Caption of the sheet row (main entries, when a sheet is used).
    pub caption: Option<String>,
}

impl Entry {
    pub fn changed(&self) -> bool {
        self.old != self.new
    }

    pub fn old_name(&self) -> String {
        file_name(&self.old)
    }

    pub fn new_name(&self) -> String {
        file_name(&self.new)
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One rename the executor carries out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
    pub old: PathBuf,
    pub new: PathBuf,
    /// Size of the file at planning time; checked again before and after.
    pub size: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub entries: Vec<Entry>,
}

impl Plan {
    /// Some entry has a problem: the run is not allowed.
    pub fn blocked(&self) -> bool {
        self.entries.iter().any(|e| !e.problems.is_empty())
    }

    pub fn problem_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.problems.is_empty()).count()
    }

    /// Number of files whose name changes.
    pub fn change_count(&self) -> usize {
        self.entries.iter().filter(|e| e.changed()).count()
    }

    pub fn moves(&self) -> Vec<Move> {
        self.entries.iter().filter(|e| e.changed()).map(|e| Move { old: e.old.clone(), new: e.new.clone(), size: e.size }).collect()
    }
}

/// File names of a folder, indexed by stem (read once per folder).
struct DirCache(HashMap<PathBuf, HashMap<String, Vec<OsString>>>);

impl DirCache {
    fn with_stem(&mut self, dir: &Path, stem: &str) -> Vec<OsString> {
        let index = self.0.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut m: HashMap<String, Vec<OsString>> = HashMap::new();
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let name = e.file_name();
                    if let Some(s) = Path::new(&name).file_stem() {
                        m.entry(s.to_string_lossy().into_owned()).or_default().push(name);
                    }
                }
            }
            for v in m.values_mut() {
                v.sort();
            }
            m
        });
        index.get(stem).cloned().unwrap_or_default()
    }
}

fn parent_of(p: &Path) -> PathBuf {
    p.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

/// Builds the plan for `files` (in the order given: the counter follows it).
///
/// `Err` only for unusable rules (bad template or expression, a sheet token without a sheet).
pub fn build(files: &[SourceFile], rules: &RenameRules, sheet: Option<&CaptionSheet>, opts: &PlanOptions) -> Result<Plan, String> {
    let compiled = rules.compile()?;
    if compiled.uses_sheet && sheet.is_none() {
        return Err("the template uses {sheet_name} or {caption}, but no caption sheet is loaded".into());
    }
    let in_set: HashSet<String> = files.iter().map(|f| key(&f.path)).collect();
    let mut dirs = DirCache(HashMap::new());
    let mut entries: Vec<Entry> = Vec::new();
    let mut added: HashSet<String> = HashSet::new();

    for (index, f) in files.iter().enumerate() {
        if !added.insert(key(&f.path)) {
            continue; // the same file chosen twice
        }
        let dir = parent_of(&f.path);
        let stem = f.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = f.path.extension().map(|s| s.to_string_lossy().into_owned());
        let mut main = Entry { item: f.key, kind: EntryKind::Main, old: f.path.clone(), new: f.path.clone(), size: 0, problems: Vec::new(), number: None, caption: None };
        match std::fs::metadata(&f.path) {
            Err(_) => main.problems.push(Problem::MissingSource),
            Ok(m) if !m.is_file() => main.problems.push(Problem::NotAFile),
            Ok(m) => main.size = m.len(),
        }
        let number = compiled.number_pattern.extract(&stem);
        main.number = number;
        let resolved = sheet.map(|s| match number {
            None => Err(format!("no photo number found in the file name {stem:?}")),
            Some(n) => match s.lookup(n) {
                Ok(r) => Ok(crate::sheet::Resolved { number: n, name: r.name.clone(), caption: r.caption.clone() }),
                Err(LookupError::NotFound) => Err(format!("photo {n} has no row in the caption sheet")),
                Err(LookupError::Ambiguous(rows)) => Err(format!("photo {n} has several different rows in the caption sheet (rows {})", rows.iter().map(|r| r.to_string()).collect::<Vec<_>>().join(", "))),
            },
        });
        if let Some(Ok(r)) = &resolved {
            main.caption = Some(r.caption.clone());
        }
        let facts = FileFacts { stem: &stem, index, capture_unix_ms: f.capture_unix_ms, camera: f.camera.as_deref(), number, sheet: resolved.as_ref().and_then(|r| r.as_ref().ok()) };
        let mut new_stem = None;
        if main.problems.is_empty() {
            match (&resolved, compiled.uses_sheet) {
                (Some(Err(m)), true) => main.problems.push(Problem::Sheet(m.clone())),
                _ => match compiled.new_stem(&facts) {
                    Ok(mut s) => {
                        if compiled.uses_sheet {
                            if let Some(e) = &ext {
                                let tail = format!(".{e}");
                                if s.len() > tail.len() && s.to_lowercase().ends_with(&tail.to_lowercase()) {
                                    s.truncate(s.len() - tail.len());
                                }
                            }
                        }
                        new_stem = Some(s);
                    }
                    Err(m) => main.problems.push(Problem::Template(m)),
                },
            }
        }
        if let Some(ns) = &new_stem {
            let new_ext = ext.as_deref().map(|e| compiled.new_ext(e));
            let name = join_name(ns, new_ext.as_deref());
            main.problems.extend(check_name(ns, &name).into_iter().map(Problem::Name));
            main.new = dir.join(&name);
        }
        entries.push(main);

        // Companions: sidecars always, same-stem pairs on request.
        let is_xmp = ext.as_deref().is_some_and(|e| e.eq_ignore_ascii_case("xmp"));
        if is_xmp {
            continue;
        }
        for n in dirs.with_stem(&dir, &stem) {
            let p = dir.join(&n);
            let (cs, ce) = (p.file_stem().map(|s| s.to_string_lossy().into_owned()), p.extension().map(|s| s.to_string_lossy().into_owned()));
            if cs.as_deref() != Some(stem.as_str()) || key(&p) == key(&f.path) || in_set.contains(&key(&p)) {
                continue;
            }
            let Some(ce) = ce else { continue };
            let sidecar = ce.eq_ignore_ascii_case("xmp");
            if !sidecar && !opts.pairs {
                continue;
            }
            if !added.insert(key(&p)) {
                continue;
            }
            let mut e = Entry { item: f.key, kind: if sidecar { EntryKind::Sidecar } else { EntryKind::Pair }, old: p.clone(), new: p.clone(), size: 0, problems: Vec::new(), number: None, caption: None };
            match std::fs::metadata(&p) {
                Ok(m) if m.is_file() => e.size = m.len(),
                Ok(_) => continue,
                Err(_) => e.problems.push(Problem::MissingSource),
            }
            if let Some(ns) = &new_stem {
                let this_ext = if sidecar { ce.clone() } else { compiled.new_ext(&ce) };
                let name = join_name(ns, Some(&this_ext));
                e.problems.extend(check_name(ns, &name).into_iter().map(Problem::Name));
                e.new = dir.join(name);
            }
            entries.push(e);
        }
    }

    // Duplicates among the new names (unchanged entries count: they keep their name).
    let mut by_new: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, e) in entries.iter().enumerate() {
        by_new.entry(key(&e.new)).or_default().push(i);
    }
    for group in by_new.values().filter(|g| g.len() > 1) {
        for &i in group {
            // Entries that stay as they are and carry no change do not get blamed for a clash they did not cause,
            // but the entry that wants their name does.
            let other = group.iter().copied().find(|&j| j != i).map(|j| entries[j].old.clone()).unwrap_or_default();
            entries[i].problems.push(Problem::DuplicateTarget(other));
        }
    }
    // Existing files that are not part of the plan.
    let old_keys: HashSet<String> = entries.iter().map(|e| key(&e.old)).collect();
    for e in entries.iter_mut().filter(|e| e.changed()) {
        if !old_keys.contains(&key(&e.new)) && std::fs::symlink_metadata(&e.new).is_ok() {
            e.problems.push(Problem::TargetExists);
        }
    }
    Ok(Plan { entries })
}

fn join_name(stem: &str, ext: Option<&str>) -> String {
    match ext {
        Some(e) if !e.is_empty() => format!("{stem}.{e}"),
        _ => stem.to_owned(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::numbers::NumberPattern;
    use crate::rules::{Counter, ExtCase};
    use crate::sheet::SheetColumns;

    pub fn touch(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    pub fn src(path: &Path, key: u64) -> SourceFile {
        SourceFile { key, path: path.to_path_buf(), capture_unix_ms: None, camera: None }
    }

    fn rules(t: &str) -> RenameRules {
        RenameRules { template: t.into(), ..Default::default() }
    }

    fn names(plan: &Plan) -> Vec<(String, String)> {
        plan.entries.iter().map(|e| (e.old_name(), e.new_name())).collect()
    }

    fn sheet(rows: &[(&str, &str, &str)]) -> CaptionSheet {
        let cells: Vec<Vec<String>> = rows.iter().map(|r| vec![r.0.to_owned(), r.1.to_owned(), r.2.to_owned()]).collect();
        CaptionSheet::from_cells(&cells, &SheetColumns::default()).unwrap()
    }

    #[test]
    fn counter_template_renames_in_order() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "1");
        let b = touch(t.path(), "b.JPG", "22");
        let r = RenameRules { template: "Job_{counter}".into(), counter: Counter { start: 1, step: 1, pad: 3 }, ext_case: ExtCase::Lower, ..Default::default() };
        let plan = build(&[src(&a, 1), src(&b, 2)], &r, None, &PlanOptions::default()).unwrap();
        assert_eq!(names(&plan), vec![("a.jpg".into(), "Job_001.jpg".into()), ("b.JPG".into(), "Job_002.jpg".into())]);
        assert!(!plan.blocked());
        assert_eq!(plan.moves().len(), 2);
        assert_eq!(plan.moves()[1].size, 2);
    }

    #[test]
    fn sheet_names_come_from_the_number() {
        let t = tempfile::tempdir().unwrap();
        let f = touch(t.path(), "Job-0076.jpg", "x");
        let s = sheet(&[("76", "076_Anna Berger", "Anna Berger")]);
        let plan = build(&[src(&f, 1)], &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries[0].new_name(), "076_Anna Berger.jpg");
        assert_eq!(plan.entries[0].number, Some(76));
        assert_eq!(plan.entries[0].caption.as_deref(), Some("Anna Berger"));
        // The sheet name may carry the extension.
        let s2 = sheet(&[("76", "076_Anna.JPG", "Anna")]);
        let plan = build(&[src(&f, 1)], &rules("{sheet_name}"), Some(&s2), &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries[0].new_name(), "076_Anna.jpg");
        // A template with {no:03} and {caption}.
        let plan = build(&[src(&f, 1)], &rules("{no:03}_{caption}"), Some(&s), &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries[0].new_name(), "076_Anna Berger.jpg");
    }

    #[test]
    fn a_photo_without_sheet_row_is_a_blocking_problem() {
        let t = tempfile::tempdir().unwrap();
        let f = touch(t.path(), "Job-0077.jpg", "x");
        let g = touch(t.path(), "nonumber.jpg", "x");
        let s = sheet(&[("76", "076_Anna", "Anna")]);
        let plan = build(&[src(&f, 1), src(&g, 2)], &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(plan.blocked());
        assert!(matches!(&plan.entries[0].problems[0], Problem::Sheet(m) if m.contains("77")));
        assert!(matches!(&plan.entries[1].problems[0], Problem::Sheet(m) if m.contains("no photo number")));
        assert!(!plan.entries[0].changed(), "no silent fallback name");
        // A sheet token without a sheet is refused outright.
        assert!(build(&[src(&f, 1)], &rules("{caption}"), None, &PlanOptions::default()).is_err());
    }

    #[test]
    fn conflict_classes() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "1");
        let b = touch(t.path(), "b.jpg", "2");
        let other = touch(t.path(), "taken.jpg", "3");
        let _ = other;
        let opts = PlanOptions::default();
        // duplicate targets
        let plan = build(&[src(&a, 1), src(&b, 2)], &rules("same"), None, &opts).unwrap();
        assert!(plan.entries.iter().all(|e| e.problems.iter().any(|p| matches!(p, Problem::DuplicateTarget(_)))));
        assert!(plan.blocked());
        // existing file that is not in the set
        let plan = build(&[src(&a, 1)], &rules("taken"), None, &opts).unwrap();
        assert!(plan.entries[0].problems.contains(&Problem::TargetExists));
        // an existing file that IS in the set and moves away is fine
        let plan = build(&[src(&a, 1), src(&b, 2)], &RenameRules { template: "{counter}".into(), counter: Counter { start: 1, step: 1, pad: 1 }, ..Default::default() }, None, &opts).unwrap();
        assert!(!plan.blocked());
        // invalid, reserved, trailing, too long, empty
        for (tpl, want) in [("a?b", "InvalidChar"), ("con", "Reserved"), ("x.", "TrailingDotOrSpace"), ("x ", "TrailingDotOrSpace"), ("a/b", "InvalidChar")] {
            let plan = build(&[src(&a, 1)], &rules(tpl), None, &opts).unwrap();
            let msg = format!("{:?}", plan.entries[0].problems);
            assert!(msg.contains(want), "{tpl}: {msg}");
            assert!(plan.blocked());
        }
        let long = "x".repeat(300);
        let plan = build(&[src(&a, 1)], &rules(&long), None, &opts).unwrap();
        assert!(format!("{:?}", plan.entries[0].problems).contains("TooLong"));
        let plan = build(&[src(&a, 1)], &RenameRules { template: "{stem}".into(), find: "a".into(), ..Default::default() }, None, &opts).unwrap();
        assert!(format!("{:?}", plan.entries[0].problems).contains("Empty"), "an empty name is refused");
        // a missing source
        let ghost = t.path().join("ghost.jpg");
        let plan = build(&[src(&ghost, 3)], &rules("z"), None, &opts).unwrap();
        assert!(plan.entries[0].problems.contains(&Problem::MissingSource));
        // an unchanged name is no conflict with itself
        let plan = build(&[src(&a, 1)], &rules("{stem}"), None, &opts).unwrap();
        assert!(!plan.blocked() && plan.change_count() == 0);
    }

    #[test]
    fn a_name_taken_by_an_unchanged_file_of_the_set_is_a_duplicate() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "a.jpg", "1");
        let b = touch(t.path(), "b.jpg", "2");
        // a -> b, but b keeps its name (find/replace only touches "a")
        let r = RenameRules { template: "{stem}".into(), find: "a".into(), replace: "b".into(), ..Default::default() };
        let plan = build(&[src(&a, 1), src(&b, 2)], &r, None, &PlanOptions::default()).unwrap();
        assert!(plan.blocked());
    }

    #[test]
    fn case_only_changes_are_allowed() {
        let t = tempfile::tempdir().unwrap();
        let a = touch(t.path(), "abc.jpg", "1");
        let r = RenameRules { template: "{stem}".into(), case: crate::rules::CaseChange::Upper, ..Default::default() };
        let plan = build(&[src(&a, 1)], &r, None, &PlanOptions::default()).unwrap();
        assert!(!plan.blocked());
        assert_eq!(plan.entries[0].new_name(), "ABC.jpg");
    }

    #[test]
    fn sidecars_and_pairs_follow_their_file() {
        let t = tempfile::tempdir().unwrap();
        let raw = touch(t.path(), "IMG_1.CR2", "raw");
        touch(t.path(), "IMG_1.xmp", "x1");
        touch(t.path(), "IMG_1.jpg", "jj");
        touch(t.path(), "IMG_10.xmp", "other");
        touch(t.path(), "IMG_1.XMP.bak", "no");
        let r = rules("Job_{counter}");
        let plan = build(&[src(&raw, 1)], &r, None, &PlanOptions::default()).unwrap();
        assert_eq!(names(&plan), vec![("IMG_1.CR2".into(), "Job_001.CR2".into()), ("IMG_1.xmp".into(), "Job_001.xmp".into())], "no pair without the option; IMG_10.xmp is not a sidecar of IMG_1");
        assert_eq!(plan.entries[1].kind, EntryKind::Sidecar);
        let plan = build(&[src(&raw, 1)], &r, None, &PlanOptions { pairs: true }).unwrap();
        let mut n = names(&plan);
        n.sort();
        assert_eq!(n, vec![("IMG_1.CR2".into(), "Job_001.CR2".into()), ("IMG_1.jpg".into(), "Job_001.jpg".into()), ("IMG_1.xmp".into(), "Job_001.xmp".into())]);
        // upper-case .XMP keeps its case; ext case applies to the files, not the sidecar
        let t2 = tempfile::tempdir().unwrap();
        let f = touch(t2.path(), "A.JPG", "1");
        touch(t2.path(), "A.XMP", "2");
        let r = RenameRules { template: "B".into(), ext_case: ExtCase::Lower, ..Default::default() };
        let plan = build(&[src(&f, 1)], &r, None, &PlanOptions::default()).unwrap();
        assert_eq!(names(&plan), vec![("A.JPG".into(), "B.jpg".into()), ("A.XMP".into(), "B.XMP".into())]);
        // a sidecar whose new name is taken blocks too
        touch(t2.path(), "B.XMP", "taken");
        let plan = build(&[src(&f, 1)], &RenameRules { template: "B".into(), ..Default::default() }, None, &PlanOptions::default()).unwrap();
        assert!(plan.blocked(), "B.XMP exists and is not part of the rename");
    }

    #[test]
    fn a_pair_that_is_chosen_itself_is_not_added_twice() {
        let t = tempfile::tempdir().unwrap();
        let raw = touch(t.path(), "IMG_1.CR2", "raw");
        let jpg = touch(t.path(), "IMG_1.jpg", "jj");
        let plan = build(&[src(&raw, 1), src(&jpg, 2)], &rules("{stem}_x"), None, &PlanOptions { pairs: true }).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries.iter().all(|e| e.kind == EntryKind::Main));
        assert!(!plan.blocked());
        // the same file chosen twice counts once
        let plan = build(&[src(&raw, 1), src(&raw, 1)], &rules("{stem}_x"), None, &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries.len(), 1);
    }

    #[test]
    fn number_pattern_comes_from_the_rules() {
        let t = tempfile::tempdir().unwrap();
        let f = touch(t.path(), "076_Anna_2.jpg", "x");
        let s = sheet(&[("76", "076_A", "A"), ("2", "002_B", "B")]);
        let mut r = rules("{sheet_name}");
        r.number_pattern = NumberPattern::FirstDigits;
        let plan = build(&[src(&f, 1)], &r, Some(&s), &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries[0].new_name(), "076_A.jpg");
        r.number_pattern = NumberPattern::LastDigits;
        let plan = build(&[src(&f, 1)], &r, Some(&s), &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries[0].new_name(), "002_B.jpg");
    }
}
