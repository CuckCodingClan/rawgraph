//! The rename plan: for every file the old and the new name, and every reason why the run must not
//! start. Nothing here changes a file; the plan only looks.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::names::{check_name, key, NameProblem};
use crate::numbers::NumberPattern;
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
    /// A same-stem sidecar belongs to another file (the RAW) that is not part of the run.
    AmbiguousSidecar(String),
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
            Problem::AmbiguousSidecar(m) => m.clone(),
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
    /// Capture time of a chosen file (EXIF, camera-local), if known.
    pub capture_ms: Option<i64>,
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
    /// The size must be exactly the same before and after (false for sidecars: editors rewrite them).
    pub exact_size: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// Things to look at that do not block (mixed folders, very long paths).
    pub warnings: Vec<String>,
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
        self.entries.iter().filter(|e| e.changed()).map(|e| Move { old: e.old.clone(), new: e.new.clone(), size: e.size, exact_size: e.kind != EntryKind::Sidecar }).collect()
    }
}

/// Camera RAW extensions (lower case). A same-stem `.xmp` belongs to the RAW of that stem.
const RAW_EXTS: &[&str] = &[
    "3fr", "ari", "arw", "bay", "cap", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "eip", "erf", "fff", "iiq", "k25", "kdc", "mdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "pef", "ptx", "pxn", "r3d",
    "raf", "raw", "rw2", "rwl", "rwz", "sr2", "srf", "srw", "x3f",
];

/// Sidecar extensions written as `<full file name>.<ext>` (darktable `.xmp`, RawTherapee `.pp3`, DxO `.dop`).
const FULLNAME_SIDECARS: &[&str] = &["xmp", "pp3", "dop"];

fn is_raw(ext: &str) -> bool {
    RAW_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

/// File names of a folder, indexed by stem and by full name (read once per folder). Both keyed like paths.
#[derive(Default)]
struct DirIndex {
    by_stem: HashMap<String, Vec<OsString>>,
    by_name: HashMap<String, OsString>,
}

struct DirCache(HashMap<PathBuf, DirIndex>);

impl DirCache {
    fn index(&mut self, dir: &Path) -> &DirIndex {
        self.0.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut ix = DirIndex::default();
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let name = e.file_name();
                    if let Some(s) = Path::new(&name).file_stem() {
                        ix.by_stem.entry(key(Path::new(s))).or_default().push(name.clone());
                    }
                    ix.by_name.insert(key(Path::new(&name)), name);
                }
            }
            for v in ix.by_stem.values_mut() {
                v.sort();
            }
            ix
        })
    }
}

fn parent_of(p: &Path) -> PathBuf {
    p.parent().filter(|d| !d.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

/// The photo number of every stem. With the default pattern and several files the number is what follows the
/// prefix all the files have in common, so `IMG_1234-2` among `IMG_123x` files, `Job-076_v2` or `Smith-346 (1)` are
/// reported instead of being mapped to their last group of digits. Other patterns are applied as given.
fn extract_numbers(stems: &[String], pattern: &NumberPattern) -> Vec<Result<u64, String>> {
    let plain = |s: &str| pattern.extract(s).ok_or_else(|| format!("no photo number found in the file name {s:?}"));
    if !matches!(pattern, NumberPattern::LastDigits) || stems.len() < 2 {
        return stems.iter().map(|s| plain(s)).collect();
    }
    let mut prefix: Vec<char> = stems[0].chars().collect();
    for s in &stems[1..] {
        let n = prefix.iter().zip(s.chars()).take_while(|(a, b)| **a == *b).count();
        prefix.truncate(n);
    }
    while prefix.last().is_some_and(|c| c.is_ascii_digit()) {
        prefix.pop();
    }
    let prefix: String = prefix.into_iter().collect();
    stems
        .iter()
        .map(|s| match s.strip_prefix(prefix.as_str()) {
            Some(rest) if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) => rest.parse().map_err(|_| format!("the number in {s:?} is too large")),
            _ if prefix.is_empty() => Err(format!("the files have different name schemes ({:?} and {s:?}); choose files of one scheme", stems[0])),
            Some(rest) if rest.chars().any(|c| c.is_ascii_digit()) => Err(format!("{s:?} has more after the photo number ({rest:?}); the other files are named {prefix:?} + number")),
            _ => Err(format!("{s:?} does not follow the name scheme of the other files ({prefix:?} + number)")),
        })
        .collect()
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
    let stems: Vec<String> = files.iter().map(|f| f.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()).collect();
    let numbers = extract_numbers(&stems, &compiled.number_pattern);
    let mut warnings = Vec::new();
    let folders: HashSet<String> = files.iter().map(|f| key(&parent_of(&f.path))).collect();
    if folders.len() > 1 {
        warnings.push(format!("The files come from {} different folders (for example originals and edited pictures); a sheet rename or a counter mixes them. Choose the files of one folder.", folders.len()));
    }

    for (index, f) in files.iter().enumerate() {
        if !added.insert(key(&f.path)) {
            continue; // the same file chosen twice
        }
        let dir = parent_of(&f.path);
        let stem = stems[index].clone();
        let ext = f.path.extension().map(|s| s.to_string_lossy().into_owned());
        let mut main = Entry { item: f.key, kind: EntryKind::Main, old: f.path.clone(), new: f.path.clone(), size: 0, problems: Vec::new(), number: None, caption: None, capture_ms: f.capture_unix_ms };
        match std::fs::metadata(&f.path) {
            Err(_) => main.problems.push(Problem::MissingSource),
            Ok(m) if !m.is_file() => main.problems.push(Problem::NotAFile),
            Ok(m) => main.size = m.len(),
        }
        let number_res = &numbers[index];
        let number = number_res.as_ref().ok().copied();
        main.number = number;
        let resolved = sheet.map(|s| match number_res {
            Err(m) => Err(m.clone()),
            Ok(n) => match s.lookup(*n) {
                Ok(r) => Ok(crate::sheet::Resolved { number: *n, name: r.name.clone(), caption: r.caption.clone() }),
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
            match (&resolved, number_res) {
                (Some(Err(m)), _) if compiled.uses_sheet => main.problems.push(Problem::Sheet(m.clone())),
                (_, Err(m)) if compiled.uses_number => main.problems.push(Problem::Sheet(m.clone())),
                _ => {
                    if compiled.uses_sheet && facts.sheet.is_some_and(|r| r.caption.is_empty()) && compiled.uses_caption {
                        main.problems.push(Problem::Sheet("the caption cell of this photo's row is empty (merged cells?)".into()));
                    } else {
                        match compiled.new_stem(&facts) {
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
                        }
                    }
                }
            }
        }
        let new_ext = ext.as_deref().map(|e| compiled.new_ext(e));
        let new_name = new_stem.as_ref().map(|ns| join_name(ns, new_ext.as_deref()));
        if let (Some(ns), Some(name)) = (&new_stem, &new_name) {
            main.problems.extend(check_name(ns, name).into_iter().map(Problem::Name));
            main.new = dir.join(name);
        }
        let main_index = entries.len();
        entries.push(main);

        // Companions: sidecars always, same-stem pairs on request.
        let is_xmp = ext.as_deref().is_some_and(|e| e.eq_ignore_ascii_case("xmp"));
        if is_xmp {
            continue;
        }
        let this_is_raw = ext.as_deref().is_some_and(is_raw);
        let this_name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut companions: Vec<(PathBuf, EntryKind, String)> = Vec::new(); // (path, kind, tail kept after the new name)
        let mut ambiguous: Option<String> = None;
        {
            let ix = dirs.index(&dir);
            let same_stem: Vec<OsString> = ix.by_stem.get(&key(Path::new(&stem))).cloned().unwrap_or_default();
            let raw_of_stem = same_stem.iter().find(|n| Path::new(n).extension().is_some_and(|e| is_raw(&e.to_string_lossy()))).cloned();
            for n in &same_stem {
                let p = dir.join(n);
                if key(&p) == key(&f.path) || in_set.contains(&key(&p)) {
                    continue;
                }
                let Some(ce) = p.extension().map(|s| s.to_string_lossy().into_owned()) else { continue };
                if ce.eq_ignore_ascii_case("xmp") {
                    match &raw_of_stem {
                        Some(raw) if !this_is_raw => {
                            if !in_set.contains(&key(&dir.join(raw))) && ambiguous.is_none() {
                                ambiguous = Some(format!("the sidecar {} belongs to {}, which is not part of the rename; add that file or choose the RAW", n.to_string_lossy(), raw.to_string_lossy()));
                            }
                        }
                        _ => companions.push((p, EntryKind::Sidecar, ce)),
                    }
                } else if opts.pairs {
                    companions.push((p, EntryKind::Pair, ce));
                }
            }
            for sfx in FULLNAME_SIDECARS {
                for variant in [sfx.to_string(), sfx.to_ascii_uppercase()] {
                    let cand = format!("{this_name}.{variant}");
                    if let Some(real) = ix.by_name.get(&key(Path::new(&cand))) {
                        let p = dir.join(real);
                        if !in_set.contains(&key(&p)) {
                            let real_name = real.to_string_lossy();
                            let tail = real_name.get(this_name.len() + 1..).unwrap_or(&variant).to_owned();
                            companions.push((p, EntryKind::Sidecar, format!("FULL.{tail}")));
                        }
                    }
                }
            }
        }
        if let Some(m) = ambiguous {
            entries[main_index].problems.push(Problem::AmbiguousSidecar(m));
        }
        for (p, kind, tail) in companions {
            if !added.insert(key(&p)) {
                continue;
            }
            let mut e = Entry { item: f.key, kind, old: p.clone(), new: p.clone(), size: 0, problems: Vec::new(), number: None, caption: None, capture_ms: None };
            match std::fs::metadata(&p) {
                Ok(m) if m.is_file() => e.size = m.len(),
                Ok(_) => continue,
                Err(_) => e.problems.push(Problem::MissingSource),
            }
            if let (Some(ns), Some(nn)) = (&new_stem, &new_name) {
                let name = if let Some(variant) = tail.strip_prefix("FULL.") {
                    format!("{nn}.{variant}")
                } else if kind == EntryKind::Sidecar {
                    join_name(ns, Some(&tail))
                } else {
                    join_name(ns, Some(&compiled.new_ext(&tail)))
                };
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
    if cfg!(windows) {
        let long = entries.iter().filter(|e| e.changed() && e.new.to_string_lossy().chars().count() > 259).count();
        if long > 0 {
            warnings.push(format!("{long} new path(s) are longer than 259 characters; programs without long-path support cannot open them."));
        }
    }
    Ok(Plan { entries, warnings })
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
        let plan = build(&[src(&f, 1)], &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(plan.blocked());
        assert!(matches!(&plan.entries[0].problems[0], Problem::Sheet(m) if m.contains("77")));
        assert!(!plan.entries[0].changed(), "no silent fallback name");
        let plan = build(&[src(&g, 2)], &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(matches!(&plan.entries[0].problems[0], Problem::Sheet(m) if m.contains("no photo number")));
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

    fn first_problem(plan: &Plan, i: usize) -> String {
        plan.entries[i].problems.first().map(|p| p.message()).unwrap_or_default()
    }

    #[test]
    fn the_number_is_what_follows_the_common_prefix() {
        let t = tempfile::tempdir().unwrap();
        let s = sheet(&[("1234", "1234_A", "A"), ("1235", "1235_B", "B"), ("76", "076_C", "C"), ("2", "002_D", "D"), ("346", "346_E", "E")]);
        let names = ["IMG_1234-2.jpg", "IMG_1235.jpg"];
        let files: Vec<_> = names.iter().enumerate().map(|(i, n)| src(&touch(t.path(), n, "x"), i as u64)).collect();
        let plan = build(&files, &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(first_problem(&plan, 0).contains("more after the photo number"), "{}", first_problem(&plan, 0));
        assert!(plan.entries[1].problems.is_empty(), "the clean file is fine: {}", first_problem(&plan, 1));
        assert_eq!(plan.entries[1].new_name(), "1235_B.jpg");
        // a v2 suffix and a copy number are not mapped to their last digit group
        let t2 = tempfile::tempdir().unwrap();
        let files: Vec<_> = ["Job-076_v2.jpg", "Job-077_v2.jpg"].iter().enumerate().map(|(i, n)| src(&touch(t2.path(), n, "x"), i as u64)).collect();
        let plan = build(&files, &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(plan.blocked() && first_problem(&plan, 0).contains("more after"), "{}", first_problem(&plan, 0));
        let t3 = tempfile::tempdir().unwrap();
        let files: Vec<_> = ["Smith-346.jpg", "Smith-346 (1).jpg"].iter().enumerate().map(|(i, n)| src(&touch(t3.path(), n, "x"), i as u64)).collect();
        let plan = build(&files, &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(first_problem(&plan, 1).contains("more after"), "{}", first_problem(&plan, 1));
        assert_eq!(plan.entries[0].new_name(), "346_E.jpg");
        // the user can still choose another pattern, applied as given
        let mut r = rules("{sheet_name}");
        r.number_pattern = NumberPattern::parse(r"Job-(\d+)_").unwrap();
        let plan = build(&[src(&t2.path().join("Job-076_v2.jpg"), 1)], &r, Some(&s), &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries[0].new_name(), "076_C.jpg");
    }

    #[test]
    fn files_of_different_schemes_or_folders_are_not_mixed() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir(t.path().join("ROH")).unwrap();
        std::fs::create_dir(t.path().join("Bearbeitet")).unwrap();
        let raw = touch(&t.path().join("ROH"), "DSC_0076.NEF", "r");
        let jpg = touch(&t.path().join("Bearbeitet"), "Job-0076.jpg", "j");
        let s = sheet(&[("76", "076_A", "A")]);
        let plan = build(&[src(&jpg, 1), src(&raw, 2)], &rules("{sheet_name}"), Some(&s), &PlanOptions::default()).unwrap();
        assert!(plan.blocked(), "the RAW does not follow the scheme of the JPEG");
        assert!(first_problem(&plan, 0).contains("different name schemes"), "{}", first_problem(&plan, 0));
        assert!(plan.warnings.iter().any(|w| w.contains("2 different folders")), "{:?}", plan.warnings);
    }

    #[test]
    fn the_sidecar_belongs_to_the_raw() {
        let t = tempfile::tempdir().unwrap();
        touch(t.path(), "IMG_1.CR2", "raw");
        touch(t.path(), "IMG_1.xmp", "x");
        let jpg = touch(t.path(), "IMG_1.jpg", "jj");
        // Only the JPEG is chosen: the RAW's sidecar is not renamed with it; the entry is blocked.
        let plan = build(&[src(&jpg, 1)], &rules("N"), None, &PlanOptions::default()).unwrap();
        assert_eq!(plan.entries.len(), 1, "no sidecar entry");
        assert!(plan.blocked());
        assert!(matches!(&plan.entries[0].problems[0], Problem::AmbiguousSidecar(m) if m.contains("IMG_1.CR2")), "{:?}", plan.entries[0].problems);
        // Both are chosen: the sidecar goes with the RAW only.
        let raw = t.path().join("IMG_1.CR2");
        let plan = build(&[src(&jpg, 1), src(&raw, 2)], &rules("N"), None, &PlanOptions::default()).unwrap();
        assert!(!plan.blocked(), "{:?}", plan.entries.iter().map(|e| e.problems.clone()).collect::<Vec<_>>());
        let side: Vec<_> = plan.entries.iter().filter(|e| e.kind == EntryKind::Sidecar).collect();
        assert_eq!(side.len(), 1);
        assert_eq!(side[0].item, 2, "attached to the RAW");
    }

    #[test]
    fn full_name_sidecars_follow_and_stems_compare_like_paths() {
        let t = tempfile::tempdir().unwrap();
        let raw = touch(t.path(), "IMG_2.CR2", "raw");
        touch(t.path(), "IMG_2.CR2.xmp", "dt");
        touch(t.path(), "IMG_2.CR2.pp3", "rt");
        touch(t.path(), "IMG_2.CR2.DOP", "dxo");
        let plan = build(&[src(&raw, 1)], &rules("Z"), None, &PlanOptions::default()).unwrap();
        let mut n = names(&plan);
        n.sort();
        assert_eq!(n, vec![("IMG_2.CR2".into(), "Z.CR2".into()), ("IMG_2.CR2.DOP".into(), "Z.CR2.DOP".into()), ("IMG_2.CR2.pp3".into(), "Z.CR2.pp3".into()), ("IMG_2.CR2.xmp".into(), "Z.CR2.xmp".into())]);
        assert!(!plan.blocked());
        if crate::names::CASE_INSENSITIVE {
            let t2 = tempfile::tempdir().unwrap();
            let raw = touch(t2.path(), "img_3.cr2", "raw");
            touch(t2.path(), "IMG_3.XMP", "x");
            let plan = build(&[src(&raw, 1)], &rules("Y"), None, &PlanOptions::default()).unwrap();
            assert_eq!(plan.entries.len(), 2, "the stems IMG_3 and img_3 are the same file on this platform");
        }
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
