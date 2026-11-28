//! Macros: groups saved as `<name>.rawgraph-macro.json` in the recipes folder. Same safety rules as
//! recipes (see `recipes`): only files with this extension inside that folder are created or deleted,
//! names are reduced to a safe file stem, nothing is overwritten.
//!
//! Inserting a macro makes a copy; later edits of the file never reach groups already inserted.

use std::path::{Path, PathBuf};

use rawgraph_core::MacroFile;

use crate::recipes::safe_name;

pub const MACRO_EXT: &str = ".rawgraph-macro.json";
const MAX_MACRO_BYTES: u64 = 8 << 20;
/// Said wherever a macro is inserted or offered.
pub const COPY_NOTE: &str = "Inserting a macro makes an independent copy: later edits of the macro file do not update groups that were already inserted.";

#[derive(Clone, Debug, PartialEq)]
pub struct MacroInfo {
    pub stem: String,
    pub path: PathBuf,
    pub usable: bool,
    pub description: String,
    pub inputs: usize,
    pub outputs: usize,
    pub published: usize,
    pub note: String,
}

fn read_limited(path: &Path) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    if meta.len() > MAX_MACRO_BYTES {
        return Err("file is too large".to_owned());
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

fn parse_file(path: &Path) -> Result<(MacroFile, Vec<String>), String> {
    MacroFile::from_json_with_notes(&read_limited(path)?).map_err(|e| e.to_string())
}

pub fn macro_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}{MACRO_EXT}"))
}

fn checked_stem(stem: &str) -> Result<&str, String> {
    match safe_name(stem) {
        Some(s) if s == stem => Ok(stem),
        _ => Err(format!("\"{stem}\" is not a valid macro name")),
    }
}

type ParseCache = std::collections::BTreeMap<PathBuf, (Option<std::time::SystemTime>, u64, MacroInfo)>;

/// What the last listing found per file; a file is parsed again only when its size or time changed.
fn parse_cache() -> &'static std::sync::Mutex<ParseCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ParseCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(ParseCache::new()))
}

fn info_of(stem: String, path: PathBuf) -> MacroInfo {
    match parse_file(&path) {
        Ok((m, _)) => MacroInfo {
            stem,
            path,
            usable: true,
            description: m.description.clone(),
            inputs: m.input_labels().len(),
            outputs: m.output_labels().len(),
            published: m.published_labels().len(),
            note: String::new(),
        },
        Err(note) => MacroInfo { stem, path, usable: false, description: String::new(), inputs: 0, outputs: 0, published: 0, note },
    }
}

/// All macro files of `dir`, sorted by name; unreadable ones are listed as unusable.
/// Files are parsed only when they changed since the last call (size and modification time).
pub fn list(dir: &Path) -> Vec<MacroInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut guard = parse_cache().lock().ok();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out: Vec<MacroInfo> = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(stem) = name.strip_suffix(MACRO_EXT).map(str::to_owned) else { continue };
        if stem.is_empty() || !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let (mtime, size) = e.metadata().map(|m| (m.modified().ok(), m.len())).unwrap_or((None, 0));
        let cached = guard.as_ref().and_then(|c| c.get(&path)).filter(|(t, n, _)| mtime.is_some() && *t == mtime && *n == size).map(|(_, _, i)| i.clone());
        let info = match cached {
            Some(i) => i,
            None => {
                let i = info_of(stem, path.clone());
                if let Some(c) = guard.as_mut() {
                    c.insert(path.clone(), (mtime, size, i.clone()));
                }
                i
            }
        };
        seen.push(path);
        out.push(info);
    }
    if let Some(c) = guard.as_mut() {
        c.retain(|p, _| p.parent() != Some(dir) || seen.contains(p));
    }
    out.sort_by_key(|m| m.stem.to_lowercase());
    out
}

fn name_taken(dir: &Path, stem: &str) -> bool {
    let wanted = stem.to_lowercase();
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| e.file_name().to_string_lossy().strip_suffix(MACRO_EXT).is_some_and(|o| o.to_lowercase() == wanted))
}

/// Writes `text` to a temporary file next to `path`, then puts it in place in one step, so a
/// crash never leaves a half-written macro. With `replace` false an existing file is never touched.
fn write_atomic(path: &Path, text: &str, replace: bool) -> Result<(), String> {
    use std::io::Write;
    let fail = |e: std::io::Error| format!("Cannot write {}: {e}", path.display());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp).map_err(fail)?;
        f.write_all(text.as_bytes()).map_err(fail)?;
        f.sync_all().map_err(fail)?;
        drop(f);
        if replace {
            std::fs::rename(&tmp, path).map_err(fail)
        } else {
            // A hard link fails when the name is taken, so nothing is overwritten.
            match std::fs::hard_link(&tmp, path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(format!("{} already exists", path.display())),
                Err(_) if !path.exists() => std::fs::rename(&tmp, path).map_err(fail),
                Err(e) => Err(fail(e)),
            }
        }
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Stores a new macro; never overwrites. The file name comes from the macro's name.
pub fn save(dir: &Path, m: &MacroFile) -> Result<PathBuf, String> {
    let stem = safe_name(&m.name).ok_or_else(|| "Give the macro a name".to_owned())?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let path = macro_path(dir, &stem);
    if path.exists() || name_taken(dir, &stem) {
        let damaged = path.exists() && parse_file(&path).is_err();
        return Err(if damaged {
            format!("A damaged macro file named \"{stem}\" is in the way: delete it in Manage recipes first")
        } else {
            format!("A macro named \"{stem}\" already exists")
        });
    }
    let text = m.to_json();
    if text.len() as u64 > MAX_MACRO_BYTES {
        return Err(format!("The macro would be {} MB; macros are limited to {} MB", text.len() >> 20, MAX_MACRO_BYTES >> 20));
    }
    write_atomic(&path, &text, false)?;
    Ok(path)
}

pub fn load_with_notes(dir: &Path, stem: &str) -> Result<(MacroFile, Vec<String>), String> {
    let stem = checked_stem(stem)?;
    parse_file(&macro_path(dir, stem)).map_err(|e| format!("Macro \"{stem}\" cannot be used: {e}"))
}

#[cfg(test)]
pub fn load(dir: &Path, stem: &str) -> Result<MacroFile, String> {
    load_with_notes(dir, stem).map(|(m, _)| m)
}

/// Deletes `<dir>/<stem>.rawgraph-macro.json` and nothing else.
pub fn delete(dir: &Path, stem: &str) -> Result<(), String> {
    let stem = checked_stem(stem)?;
    let path = macro_path(dir, stem);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("Macro \"{stem}\" does not exist"))?;
    if !meta.is_file() {
        return Err(format!("\"{stem}\" is not a macro file"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("Cannot delete: {e}"))
}

/// Copies a macro file from anywhere into the folder (validated first). Never overwrites.
pub fn import(dir: &Path, source: &Path) -> Result<PathBuf, String> {
    let (m, _) = parse_file(source).map_err(|e| format!("{} is not a usable macro: {e}", source.display()))?;
    save(dir, &m)
}

/// Writes a macro to `dest`; an existing file is replaced only when it is itself a macro file.
pub fn export(dir: &Path, stem: &str, dest: &Path) -> Result<(), String> {
    let (m, _) = load_with_notes(dir, stem)?;
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        let is_macro = meta.is_file() && dest.file_name().is_some_and(|n| n.to_string_lossy().ends_with(MACRO_EXT));
        if !is_macro {
            return Err(format!("{} exists and is not a macro file", dest.display()));
        }
    }
    write_atomic(dest, &m.to_json(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::{Graph, NodeKind};

    fn sample(name: &str) -> MacroFile {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Contrast, [200.0, 0.0]);
        g.connect(a, b, 0).unwrap();
        let grp = g.group_nodes(&[a, b], "Tone").unwrap();
        g.publish_param(grp, a, "ev", None).unwrap();
        g.extract_macro(grp, name, "two nodes").unwrap()
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rawgraph-macros-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn save_list_load_export_import_and_delete_stay_in_the_folder() {
        let dir = temp("roundtrip");
        let m = sample("Warm look");
        let path = save(&dir, &m).unwrap();
        assert!(path.ends_with("Warm look.rawgraph-macro.json"));
        assert!(save(&dir, &sample("warm LOOK")).is_err(), "never overwrites, any case");
        let list = list(&dir);
        assert_eq!(list.len(), 1);
        assert!((list[0].usable, list[0].inputs, list[0].outputs, list[0].published) == (true, 1, 1, 1));
        assert_eq!(load(&dir, "Warm look").unwrap(), m);
        assert!(load(&dir, "../evil").is_err());
        // Export and import into a second folder.
        let out = dir.join("export.rawgraph-macro.json");
        export(&dir, "Warm look", &out).unwrap();
        let other = temp("other");
        let imported = import(&other, &out).unwrap();
        assert!(imported.starts_with(&other));
        assert_eq!(load(&other, "Warm look").unwrap(), m);
        delete(&dir, "Warm look").unwrap();
        assert!(delete(&dir, "Warm look").is_err());
        assert!(delete(&dir, "..").is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn an_existing_non_macro_file_is_never_overwritten_by_an_export() {
        let dir = temp("export-guard");
        save(&dir, &sample("A")).unwrap();
        let victim = dir.join("photo.jpg");
        std::fs::write(&victim, b"x").unwrap();
        assert!(export(&dir, "A", &victim).is_err());
        assert_eq!(std::fs::read(&victim).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_files_are_listed_unusable_and_recipes_are_not_macros() {
        let dir = temp("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("bad.rawgraph-macro.json"), "{ nope").unwrap();
        std::fs::write(dir.join("r.rawgraph-recipe.json"), "{}").unwrap();
        let l = list(&dir);
        assert_eq!(l.len(), 1);
        assert!(!l[0].usable && !l[0].note.is_empty());
        assert!(load_with_notes(&dir, "bad").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_are_atomic_and_a_damaged_file_is_listed_and_can_be_deleted() {
        let dir = temp("atomic");
        let path = save(&dir, &sample("Warm")).unwrap();
        let leftovers = |d: &Path| std::fs::read_dir(d).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".tmp")).count();
        assert_eq!(leftovers(&dir), 0, "no temporary file stays behind");
        export(&dir, "Warm", &dir.join("out.rawgraph-macro.json")).unwrap();
        assert_eq!(leftovers(&dir), 0);
        // A truncated file (power loss, full disk) is shown as damaged, says so when its name is used again, and can be deleted.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, &text[..text.len() / 2]).unwrap();
        let l = list(&dir);
        let broken = l.iter().find(|m| m.stem == "Warm").unwrap();
        assert!(!broken.usable && !broken.note.is_empty());
        let err = save(&dir, &sample("Warm")).unwrap_err();
        assert!(err.contains("damaged") && err.contains("delete"), "{err}");
        delete(&dir, "Warm").unwrap();
        save(&dir, &sample("Warm")).unwrap();
        assert!(list(&dir).iter().any(|m| m.stem == "Warm" && m.usable));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_listing_parses_only_changed_files() {
        let dir = temp("cache");
        let path = save(&dir, &sample("One")).unwrap();
        assert_eq!(list(&dir)[0].description, "two nodes");
        // Same size and time: the cached entry is returned, whatever the content says now.
        let meta = std::fs::metadata(&path).unwrap();
        let stamp = meta.modified().unwrap();
        let text = std::fs::read_to_string(&path).unwrap().replace("two nodes", "XXX nodes");
        std::fs::write(&path, &text).unwrap();
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(stamp).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), meta.len());
        assert_eq!(list(&dir)[0].description, "two nodes", "unchanged (size, time): not parsed again");
        // A real change is noticed.
        std::fs::write(&path, text.replace("XXX nodes", "a longer description now")).unwrap();
        assert_eq!(list(&dir)[0].description, "a longer description now");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
