//! Recipes: named groups of nodes stored as `<name>.rawgraph-recipe.json` in
//! the recipes folder of the application's data directory. Only files inside
//! that folder with that extension are ever created, renamed or deleted, and
//! names are reduced to a safe file stem before they touch the file system.

use std::path::{Path, PathBuf};

use rawgraph_core::Subgraph;

pub const RECIPE_EXT: &str = ".rawgraph-recipe.json";
const MAX_STEM_CHARS: usize = 80;
const MAX_RECIPE_BYTES: u64 = 4 << 20;

/// Reduces free text to a safe file stem: separators, reserved characters and
/// control characters become `_`, leading/trailing dots and spaces go, reserved
/// Windows device names get a `_` suffix. `None` when nothing is left.
pub fn safe_name(name: &str) -> Option<String> {
    let mapped: String = name
        .trim()
        .chars()
        .map(|c| if c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let trimmed = mapped.trim_matches(|c: char| c == '.' || c == ' ');
    let mut stem: String = trimmed.chars().take(MAX_STEM_CHARS).collect();
    stem = stem.trim_matches(|c: char| c == '.' || c == ' ').to_owned();
    if stem.is_empty() {
        return None;
    }
    let upper = stem.to_ascii_uppercase();
    let base = upper.split('.').next().unwrap_or("");
    let reserved = matches!(base, "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT")) && base.len() == 4 && base.as_bytes()[3].is_ascii_digit());
    if reserved {
        stem.push('_');
    }
    Some(stem)
}

pub fn recipe_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}{RECIPE_EXT}"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecipeInfo {
    pub stem: String,
    pub path: PathBuf,
    /// The file parses as a valid group of nodes.
    pub usable: bool,
    pub nodes: usize,
    /// Why a recipe is unusable.
    pub note: String,
}

fn read_limited(path: &Path) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".to_owned());
    }
    if meta.len() > MAX_RECIPE_BYTES {
        return Err("file is too large".to_owned());
    }
    std::fs::read_to_string(path).map_err(|e| e.to_string())
}

fn parse_file(path: &Path) -> Result<Subgraph, String> {
    let text = read_limited(path)?;
    let sub = Subgraph::from_json(&text).map_err(|e| e.to_string())?;
    if sub.is_empty() {
        return Err("the recipe holds no nodes".to_owned());
    }
    Ok(sub)
}

/// All recipe files in `dir`, sorted by name (case-insensitive). A missing
/// folder is an empty list; unreadable or corrupt files are listed as unusable.
pub fn list(dir: &Path) -> Vec<RecipeInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<RecipeInfo> = rd
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            let stem = name.strip_suffix(RECIPE_EXT)?.to_owned();
            if stem.is_empty() || !e.file_type().ok()?.is_file() {
                return None;
            }
            Some(match parse_file(&path) {
                Ok(sub) => RecipeInfo { stem, path, usable: true, nodes: sub.node_count(), note: String::new() },
                Err(note) => RecipeInfo { stem, path, usable: false, nodes: 0, note },
            })
        })
        .collect();
    out.sort_by_key(|r| r.stem.to_lowercase());
    out
}

fn ensure_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))
}

/// A stem that is already safe (so it cannot reach outside the folder).
fn checked_stem(stem: &str) -> Result<&str, String> {
    match safe_name(stem) {
        Some(s) if s == stem => Ok(stem),
        _ => Err(format!("\"{stem}\" is not a valid recipe name")),
    }
}

/// Stores a new recipe; never overwrites. Returns the file written.
pub fn save(dir: &Path, name: &str, sub: &Subgraph) -> Result<PathBuf, String> {
    let stem = safe_name(name).ok_or_else(|| "Give the recipe a name".to_owned())?;
    if sub.is_empty() {
        return Err("Nothing to save: the selection is empty".to_owned());
    }
    ensure_dir(dir)?;
    let path = recipe_path(dir, &stem);
    if path.exists() || name_taken(dir, &stem, None) {
        return Err(format!("A recipe named \"{stem}\" already exists"));
    }
    write_new(&path, &sub.to_json())?;
    Ok(path)
}

/// True when a recipe file in `dir` has the same stem as `stem` ignoring case (other than `except`, the
/// exact stem of the file being renamed). Names are compared case-insensitively on every OS, so a folder
/// copied between Linux and Windows/macOS never holds two recipes that collide there.
fn name_taken(dir: &Path, stem: &str, except: Option<&str>) -> bool {
    let wanted = stem.to_lowercase();
    let Ok(rd) = std::fs::read_dir(dir) else { return false };
    rd.flatten().any(|e| {
        let name = e.file_name().to_string_lossy().into_owned();
        name.strip_suffix(RECIPE_EXT).is_some_and(|other| Some(other) != except && other.to_lowercase() == wanted)
    })
}

fn write_new(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

pub fn load(dir: &Path, stem: &str) -> Result<Subgraph, String> {
    let stem = checked_stem(stem)?;
    parse_file(&recipe_path(dir, stem)).map_err(|e| format!("Recipe \"{stem}\" cannot be used: {e}"))
}

/// Renames a recipe inside the folder; the new name is made safe first and an
/// existing recipe is never replaced.
pub fn rename(dir: &Path, from_stem: &str, to_name: &str) -> Result<PathBuf, String> {
    let from = checked_stem(from_stem)?;
    let to = safe_name(to_name).ok_or_else(|| "Give the recipe a name".to_owned())?;
    let (src, dst) = (recipe_path(dir, from), recipe_path(dir, &to));
    if !src.is_file() {
        return Err(format!("Recipe \"{from}\" does not exist"));
    }
    let case_only = from != to && from.to_lowercase() == to.to_lowercase();
    // A case-only rename ("A" to "a") must work; any other existing name, in any case, is a clash.
    if from == to || name_taken(dir, &to, Some(from)) || (!case_only && dst.exists()) {
        return Err(format!("A recipe named \"{to}\" already exists"));
    }
    if case_only {
        // The only file that matches is the source itself: a plain rename cannot replace anything else.
        std::fs::rename(&src, &dst).map_err(|e| format!("Cannot rename: {e}"))?;
    } else {
        // `hard_link` fails when the target exists on every OS, unlike `rename`, which replaces it on
        // Unix. Remaining race: a file created under `to` between the check above and the link is still
        // never overwritten (the link fails); only the source could vanish in between, which errors out.
        match std::fs::hard_link(&src, &dst) {
            Ok(()) => std::fs::remove_file(&src).map_err(|e| format!("Cannot rename: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!("A recipe named \"{to}\" already exists"));
            }
            // No hard links on this file system: fall back to rename (check-then-rename, small race).
            Err(_) => std::fs::rename(&src, &dst).map_err(|e| format!("Cannot rename: {e}"))?,
        }
    }
    Ok(dst)
}

/// Deletes `<dir>/<stem>.rawgraph-recipe.json` and nothing else.
pub fn delete(dir: &Path, stem: &str) -> Result<(), String> {
    let stem = checked_stem(stem)?;
    let path = recipe_path(dir, stem);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| format!("Recipe \"{stem}\" does not exist"))?;
    if !meta.is_file() {
        return Err(format!("\"{stem}\" is not a recipe file"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("Cannot delete: {e}"))
}

/// Copies a recipe file from anywhere into the folder (validated first). The
/// name comes from the file name. Never overwrites.
pub fn import(dir: &Path, source: &Path) -> Result<PathBuf, String> {
    let sub = parse_file(source).map_err(|e| format!("{} is not a usable recipe: {e}", source.display()))?;
    let file = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let name = file.strip_suffix(RECIPE_EXT).or_else(|| file.rsplit_once('.').map(|(s, _)| s)).unwrap_or(&file).to_owned();
    save(dir, &name, &sub)
}

/// Writes a recipe to `dest` (chosen by the user); an existing file is replaced
/// only when it is itself a recipe file.
pub fn export(dir: &Path, stem: &str, dest: &Path) -> Result<(), String> {
    let sub = load(dir, stem)?;
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        let is_recipe = meta.is_file() && dest.file_name().is_some_and(|n| n.to_string_lossy().ends_with(RECIPE_EXT));
        if !is_recipe {
            return Err(format!("{} exists and is not a recipe file", dest.display()));
        }
    }
    std::fs::write(dest, sub.to_json()).map_err(|e| format!("Cannot write {}: {e}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::{Graph, NodeKind};

    fn sub() -> Subgraph {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Contrast, [250.0, 0.0]);
        g.connect(a, b, 0).unwrap();
        g.extract_subgraph(&[a, b])
    }

    #[test]
    fn names_become_safe_file_stems() {
        assert_eq!(safe_name("Warm look").as_deref(), Some("Warm look"));
        assert_eq!(safe_name("..\\x").as_deref(), Some("_x"), "no parent-folder escape");
        assert_eq!(safe_name("../../etc/passwd").as_deref(), Some("_.._etc_passwd"));
        assert_eq!(safe_name("a:b*c?d\"e<f>g|h").as_deref(), Some("a_b_c_d_e_f_g_h"));
        assert_eq!(safe_name("  .. ").as_deref(), None);
        assert_eq!(safe_name("").as_deref(), None);
        assert_eq!(safe_name("CON").as_deref(), Some("CON_"));
        assert_eq!(safe_name("nul.txt").as_deref(), Some("nul.txt_"));
        assert_eq!(safe_name("COM1").as_deref(), Some("COM1_"));
        assert_eq!(safe_name("com10").as_deref(), Some("com10"));
        assert_eq!(safe_name("tab\there").as_deref(), Some("tab_here"));
        assert_eq!(safe_name(&"x".repeat(300)).unwrap().chars().count(), MAX_STEM_CHARS);
        for evil in ["..\\x", "a/b", "C:\\Windows\\x", "\\\\server\\share", "x\u{0}y"] {
            let s = safe_name(evil).unwrap();
            assert!(!s.contains(['\\', '/', ':', '\0']), "{s}");
        }
    }

    #[test]
    fn save_list_rename_delete_stay_inside_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recipes");
        let p = save(&dir, "..\\Evil", &sub()).unwrap();
        assert_eq!(p.parent().unwrap(), dir, "written inside the recipes folder");
        assert!(p.file_name().unwrap().to_string_lossy().ends_with(RECIPE_EXT));
        assert!(!tmp.path().join("Evil.rawgraph-recipe.json").exists());
        save(&dir, "Alpha", &sub()).unwrap();
        assert!(save(&dir, "alpha", &sub()).is_err() || list(&dir).len() == 2, "no silent overwrite");
        let names: Vec<String> = list(&dir).into_iter().map(|r| r.stem).collect();
        assert_eq!(names, vec!["_Evil".to_owned(), "Alpha".to_owned()]);
        assert!(list(&dir).iter().all(|r| r.usable && r.nodes == 2));
        // Rename: safe name, no replacing.
        let renamed = rename(&dir, "Alpha", "../Beta").unwrap();
        assert_eq!(renamed, recipe_path(&dir, "_Beta"));
        assert!(rename(&dir, "_Beta", "_Evil").is_err());
        assert!(rename(&dir, "Nope", "X").is_err());
        assert!(rename(&dir, "..\\_Beta", "X").is_err(), "the source is a stem, not a path");
        // A file next to the folder is never touched by delete.
        let outside = tmp.path().join("keep.rawgraph-recipe.json");
        std::fs::write(&outside, "x").unwrap();
        assert!(delete(&dir, "..\\keep").is_err());
        assert!(delete(&dir, "../keep").is_err());
        assert!(outside.exists());
        delete(&dir, "_Beta").unwrap();
        assert!(delete(&dir, "_Beta").is_err());
        assert_eq!(list(&dir).len(), 1);
        // Other files in the folder are not recipes and are left alone.
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();
        std::fs::write(dir.join("plain.json"), "{}").unwrap();
        assert_eq!(list(&dir).len(), 1);
        assert!(delete(&dir, "notes.txt").is_err());
        assert!(dir.join("notes.txt").exists() && dir.join("plain.json").exists());
    }

    #[test]
    fn a_corrupt_recipe_is_listed_as_unusable() {
        let tmp = tempfile::tempdir().unwrap();
        save(tmp.path(), "good", &sub()).unwrap();
        std::fs::write(recipe_path(tmp.path(), "broken"), "{ not json").unwrap();
        std::fs::write(recipe_path(tmp.path(), "empty"), "").unwrap();
        std::fs::write(recipe_path(tmp.path(), "binary"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let all = list(tmp.path());
        assert_eq!(all.len(), 4);
        let by = |s: &str| all.iter().find(|r| r.stem == s).unwrap();
        assert!(by("good").usable && !by("broken").usable && !by("empty").usable && !by("binary").usable);
        assert!(!by("broken").note.is_empty());
        assert!(load(tmp.path(), "broken").is_err() && load(tmp.path(), "good").is_ok());
        // A corrupt recipe can still be deleted.
        delete(tmp.path(), "broken").unwrap();
        assert_eq!(list(tmp.path()).len(), 3);
        assert!(list(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn import_and_export_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("recipes");
        save(&dir, "Look", &sub()).unwrap();
        let out = tmp.path().join("share").join("Look.rawgraph-recipe.json");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        export(&dir, "Look", &out).unwrap();
        let other = tmp.path().join("other");
        let got = import(&other, &out).unwrap();
        assert_eq!(got, recipe_path(&other, "Look"));
        assert_eq!(load(&other, "Look").unwrap(), sub());
        assert!(import(&other, &out).is_err(), "no overwrite on import");
        let plain = tmp.path().join("plain.txt");
        std::fs::write(&plain, "hello").unwrap();
        assert!(import(&other, &plain).is_err());
        assert!(export(&dir, "Look", &plain).is_err(), "an unrelated file is never replaced");
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "hello");
    }
}
