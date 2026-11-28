//! Keywords of the images of a project: editing, counting, renaming and filtering.
//!
//! The rules (normalisation, case-insensitive comparison, hierarchy `Parent|Child`) are in
//! `rawgraph_core::keywords`; this module applies them to a [`Project`]. Keywords are stored in the
//! `keywords` table of the project file (schema 3, see `docs/project-format.md`).

use rawgraph_core::keywords::{has_keyword, keyword_key, normalize_keyword, normalize_keywords, rename_in_list};
use rawgraph_core::ImageId;

use crate::{ImageEntry, Project};

/// How the keywords of a [`crate::Filter`] are combined.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeywordMatch {
    /// The image carries at least one of the keywords.
    #[default]
    Any,
    /// The image carries all of them.
    All,
}

impl ImageEntry {
    /// Whether the image carries `keyword` or a keyword below it (`Travel` finds `Travel|Italy`).
    pub fn has_keyword(&self, keyword: &str) -> bool {
        has_keyword(&self.keywords, keyword)
    }
}

/// A keyword of the project with the number of images that carry exactly this keyword.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeywordCount {
    pub keyword: String,
    pub images: usize,
}

impl Project {
    /// Every keyword in use with the number of images carrying exactly it, sorted
    /// case-insensitively. A parent that only exists as part of a path is not listed.
    pub fn keyword_counts(&self) -> Vec<KeywordCount> {
        count_keywords(self.images.iter())
    }

    /// Replaces the keywords of one image (normalised). Returns whether they changed.
    pub fn set_keywords(&mut self, id: ImageId, keywords: &[String]) -> bool {
        let new = normalize_keywords(keywords);
        match self.image_mut(id) {
            Some(e) if e.keywords != new => {
                e.keywords = new;
                true
            }
            _ => false,
        }
    }

    /// Adds keywords to the images; returns how many images changed.
    pub fn add_keywords(&mut self, ids: &[ImageId], keywords: &[String]) -> usize {
        ids.iter().filter(|id| self.edit_keywords(**id, |list| add_to_list(list, keywords))).count()
    }

    /// Removes one keyword (exactly this path, case-insensitive) from the images; returns how many changed.
    pub fn remove_keyword(&mut self, ids: &[ImageId], keyword: &str) -> usize {
        ids.iter().filter(|id| self.edit_keywords(**id, |list| remove_from_list(list, keyword))).count()
    }

    /// Renames a keyword in the whole project: the keyword and every keyword below it (renaming
    /// `Travel` to `Trips` turns `Travel|Italy` into `Trips|Italy`); a rename into an existing
    /// keyword merges the two. Returns how many images changed.
    pub fn rename_keyword(&mut self, from: &str, to: &str) -> usize {
        let ids: Vec<ImageId> = self.images.iter().map(|e| e.id).collect();
        ids.iter().filter(|id| self.edit_keywords(**id, |list| rename_in_list(list, from, to))).count()
    }

    fn edit_keywords(&mut self, id: ImageId, edit: impl FnOnce(&mut Vec<String>) -> bool) -> bool {
        self.image_mut(id).is_some_and(|e| edit(&mut e.keywords))
    }
}

/// Adds normalised keywords to a list; returns whether it changed.
pub fn add_to_list(list: &mut Vec<String>, add: &[String]) -> bool {
    let merged = normalize_keywords(list.iter().chain(add.iter()));
    let changed = merged != *list;
    *list = merged;
    changed
}

/// Removes one keyword (case-insensitive, exactly this path) from a list; returns whether it changed.
pub fn remove_from_list(list: &mut Vec<String>, keyword: &str) -> bool {
    let Some(target) = normalize_keyword(keyword) else { return false };
    let key = keyword_key(&target);
    let before = list.len();
    list.retain(|k| keyword_key(k) != key);
    list.len() != before
}

/// Counts the keywords of some entries (exact paths).
pub fn count_keywords<'a>(entries: impl IntoIterator<Item = &'a ImageEntry>) -> Vec<KeywordCount> {
    let mut map: std::collections::HashMap<String, (String, usize)> = std::collections::HashMap::new();
    for e in entries {
        for k in &e.keywords {
            map.entry(keyword_key(k)).or_insert_with(|| (k.clone(), 0)).1 += 1;
        }
    }
    let mut out: Vec<KeywordCount> = map.into_values().map(|(keyword, images)| KeywordCount { keyword, images }).collect();
    out.sort_by_cached_key(|c| (keyword_key(&c.keyword), c.keyword.clone()));
    out
}

/// The keyword part of [`crate::Filter::matches`].
pub(crate) fn matches_filter(e: &ImageEntry, selected: &[String], mode: KeywordMatch, none: bool) -> bool {
    if none {
        return e.keywords.is_empty();
    }
    if selected.is_empty() {
        return true;
    }
    match mode {
        KeywordMatch::Any => selected.iter().any(|k| e.has_keyword(k)),
        KeywordMatch::All => selected.iter().all(|k| e.has_keyword(k)),
    }
}
