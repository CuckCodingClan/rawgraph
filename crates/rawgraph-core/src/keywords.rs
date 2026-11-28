//! Keywords (Stichwörter) of an image: normalisation, comparison and matching rules shared by the
//! project, the export and the graph nodes.
//!
//! A keyword is plain text or a hierarchical path `Parent|Child|Grandchild` (the convention of
//! Lightroom's `lr:hierarchicalSubject`). Keywords keep the case they were typed in and are compared
//! case-insensitively (Unicode lower-casing); a list never holds two keywords that differ only in
//! case.

/// Separator of the levels of a hierarchical keyword.
pub const KEYWORD_SEPARATOR: char = '|';
/// Longest keyword path in characters (longer ones are cut when normalised).
pub const MAX_KEYWORD_CHARS: usize = 200;

/// Cleans one keyword: control characters become spaces, each level is trimmed, empty levels are
/// dropped, runs of blanks inside a level collapse to one blank. `None` when nothing is left.
pub fn normalize_keyword(text: &str) -> Option<String> {
    let mut levels: Vec<String> = Vec::new();
    for level in text.split(KEYWORD_SEPARATOR) {
        let cleaned: Vec<&str> = level.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect();
        if !cleaned.is_empty() {
            levels.push(cleaned.join(" "));
        }
    }
    if levels.is_empty() {
        return None;
    }
    let joined = levels.join("|");
    Some(if joined.chars().count() > MAX_KEYWORD_CHARS { joined.chars().take(MAX_KEYWORD_CHARS).collect::<String>().trim_end_matches(['|', ' ']).to_owned() } else { joined })
}

/// The key two keywords are compared by: the normalised text, lower-cased.
pub fn keyword_key(keyword: &str) -> String {
    keyword.to_lowercase()
}

/// Whether two keywords are the same (case-insensitive).
pub fn keywords_equal(a: &str, b: &str) -> bool {
    a == b || keyword_key(a) == keyword_key(b)
}

/// Normalises every keyword, drops empty ones and duplicates (case-insensitive, the first spelling
/// wins) and sorts the list (case-insensitive, then by spelling), so equal sets compare equal.
pub fn normalize_keywords<I, S>(keywords: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for k in keywords {
        if let Some(n) = normalize_keyword(k.as_ref()) {
            if seen.insert(keyword_key(&n)) {
                out.push(n);
            }
        }
    }
    out.sort_by_cached_key(|k| (keyword_key(k), k.clone()));
    out
}

/// The last level of a keyword path (`Travel|Italy` -> `Italy`).
pub fn keyword_leaf(keyword: &str) -> &str {
    keyword.rsplit(KEYWORD_SEPARATOR).next().unwrap_or(keyword)
}

/// The parent path of a keyword (`Travel|Italy|Rome` -> `Travel|Italy`), `None` for a top-level keyword.
pub fn keyword_parent(keyword: &str) -> Option<&str> {
    keyword.rfind(KEYWORD_SEPARATOR).map(|i| &keyword[..i])
}

/// Number of levels of a keyword path.
pub fn keyword_depth(keyword: &str) -> usize {
    keyword.matches(KEYWORD_SEPARATOR).count() + 1
}

/// Whether `keyword` is `ancestor` itself or lies below it (`Travel|Italy` is below `Travel`).
pub fn keyword_is_within(keyword: &str, ancestor: &str) -> bool {
    let (k, a) = (keyword_key(keyword), keyword_key(ancestor));
    k == a || (k.len() > a.len() && k.starts_with(&a) && k[a.len()..].starts_with(KEYWORD_SEPARATOR))
}

/// Whether the image carries `selected`: the keyword itself or any keyword below it.
pub fn has_keyword(keywords: &[String], selected: &str) -> bool {
    keywords.iter().any(|k| keyword_is_within(k, selected))
}

/// The rule of the "Keyword" measure of Metadata Filter and Split by Metadata: some keyword of the
/// image contains `text` (case-insensitive, any part of the path). An empty `text` matches every
/// image that has at least one keyword.
pub fn keywords_contain_text(keywords: &[String], text: &str) -> bool {
    let needle = keyword_key(text.trim());
    keywords.iter().any(|k| needle.is_empty() || keyword_key(k).contains(&needle))
}

/// Renames the keyword `from` to `to` in a list: the keyword itself and every keyword below it
/// (`Travel|Italy` renamed to `Trips` turns `Travel|Italy|Rome` into `Trips|Rome`). Returns whether
/// the list changed; the result is normalised (a rename into an existing keyword merges them).
pub fn rename_in_list(keywords: &mut Vec<String>, from: &str, to: &str) -> bool {
    let Some(to) = normalize_keyword(to) else { return false };
    let Some(from) = normalize_keyword(from) else { return false };
    let from_chars = from.chars().count();
    let mut changed = false;
    let renamed: Vec<String> = keywords
        .iter()
        .map(|k| {
            if keyword_is_within(k, &from) {
                changed = true;
                format!("{to}{}", k.chars().skip(from_chars).collect::<String>())
            } else {
                k.clone()
            }
        })
        .collect();
    if changed {
        let normalised = normalize_keywords(renamed);
        let different = normalised != *keywords;
        *keywords = normalised;
        return different;
    }
    false
}

/// One row of the tree shown for a list of keywords: every keyword path plus the paths of its
/// ancestors, sorted so that children follow their parent.
pub fn tree_paths(keywords: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut all: Vec<String> = Vec::new();
    for k in keywords {
        let mut path = String::new();
        for level in k.split(KEYWORD_SEPARATOR) {
            if !path.is_empty() {
                path.push(KEYWORD_SEPARATOR);
            }
            path.push_str(level);
            if seen.insert(keyword_key(&path)) {
                all.push(path.clone());
            }
        }
    }
    // Compare level by level so `A|B` sorts right after `A` and before `A2`.
    all.sort_by_cached_key(|p| p.split(KEYWORD_SEPARATOR).map(keyword_key).collect::<Vec<_>>());
    all
}

/// A file-name-safe text for the `{keywords}` token: the last level of every keyword joined with
/// `-`, characters that are not allowed in file names replaced by `_`, at most 80 characters.
pub fn keywords_file_token(keywords: &[String]) -> String {
    const INVALID: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
    let joined = keywords.iter().map(|k| keyword_leaf(k)).collect::<Vec<_>>().join("-");
    let cleaned: String = joined.chars().map(|c| if INVALID.contains(&c) || c.is_control() { '_' } else { c }).collect();
    let cut: String = cleaned.chars().take(80).collect();
    cut.trim_matches(|c: char| c == '.' || c == ' ' || c == '-').to_owned()
}

/// Namespace declarations that `xmp_keyword_elements` needs on the enclosing `rdf:Description`.
pub const XMP_KEYWORD_NAMESPACES: &str = " xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:lr=\"http://ns.adobe.com/lightroom/1.0/\"";

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The XMP properties of a keyword list, to be placed inside an `rdf:Description` that declares
/// [`XMP_KEYWORD_NAMESPACES`]: `dc:subject` (flat: the last level of every keyword, no duplicates) and
/// `lr:hierarchicalSubject` (the full path of every keyword that has more than one level, levels
/// joined by `|`, Lightroom's convention). Empty for an empty list. Text is escaped; non-ASCII
/// characters are written as they are (the packet is UTF-8).
pub fn xmp_keyword_elements(keywords: &[String]) -> String {
    let list = normalize_keywords(keywords);
    if list.is_empty() {
        return String::new();
    }
    let leaves = normalize_keywords(list.iter().map(|k| keyword_leaf(k)));
    let bag = |name: &str, items: &[String]| {
        let lis: String = items.iter().map(|i| format!("<rdf:li>{}</rdf:li>", xml_escape(i))).collect();
        format!("<{name}><rdf:Bag>{lis}</rdf:Bag></{name}>")
    };
    let mut out = bag("dc:subject", &leaves);
    let paths: Vec<String> = list.iter().filter(|k| k.contains(KEYWORD_SEPARATOR)).cloned().collect();
    if !paths.is_empty() {
        out.push_str(&bag("lr:hierarchicalSubject", &paths));
    }
    out
}

/// Combines the keywords read from `dc:subject` (flat names) and `lr:hierarchicalSubject` (paths)
/// of one packet: every path is kept; a flat name is added only when no path ends in it.
pub fn merge_xmp_keywords(flat: &[String], hierarchical: &[String]) -> Vec<String> {
    let paths = normalize_keywords(hierarchical);
    let leaves: std::collections::HashSet<String> = paths.iter().map(|p| keyword_key(keyword_leaf(p))).collect();
    let extra: Vec<String> = flat.iter().filter_map(|f| normalize_keyword(f)).filter(|f| !leaves.contains(&keyword_key(keyword_leaf(f)))).collect();
    normalize_keywords(paths.into_iter().chain(extra))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn normalising_cleans_dedupes_and_sorts() {
        assert_eq!(normalize_keyword("  Travel |  Italy\t|| Rome  "), Some("Travel|Italy|Rome".into()));
        assert_eq!(normalize_keyword(" | | "), None);
        assert_eq!(normalize_keyword(""), None);
        let list = normalize_keywords(["beta", "Alpha", "ALPHA", " beta ", "Ärger", "ärger", ""]);
        assert_eq!(list, v(&["Alpha", "beta", "Ärger"]), "first spelling wins, case-insensitive order");
        assert_eq!(normalize_keywords(["b", "a"]), normalize_keywords(["a", "b", "A"]));
    }

    #[test]
    fn leaf_parent_depth_and_hierarchy() {
        assert_eq!(keyword_leaf("Travel|Italy|Rome"), "Rome");
        assert_eq!(keyword_leaf("Rome"), "Rome");
        assert_eq!(keyword_parent("Travel|Italy|Rome"), Some("Travel|Italy"));
        assert_eq!(keyword_parent("Rome"), None);
        assert_eq!(keyword_depth("a|b|c"), 3);
        assert!(keyword_is_within("Travel|Italy", "travel"));
        assert!(keyword_is_within("Travel", "TRAVEL"));
        assert!(!keyword_is_within("Travelling", "Travel"), "a longer name is not a child");
        assert!(has_keyword(&v(&["Travel|Italy|Rome"]), "Travel|Italy"));
        assert!(!has_keyword(&v(&["Travel|Italy"]), "Travel|Italy|Rome"));
    }

    #[test]
    fn the_text_rule_is_a_case_insensitive_substring() {
        let k = v(&["Travel|Italy|Rome", "Ärger"]);
        assert!(keywords_contain_text(&k, "rom"));
        assert!(keywords_contain_text(&k, "ÄRGER"));
        assert!(keywords_contain_text(&k, "travel|italy"));
        assert!(!keywords_contain_text(&k, "paris"));
        assert!(keywords_contain_text(&k, ""), "empty text: any keyword");
        assert!(!keywords_contain_text(&[], ""), "empty text: no keyword, no match");
    }

    #[test]
    fn renaming_moves_the_subtree_and_merges() {
        let mut k = v(&["Travel|Italy", "Travel|Italy|Rome", "Travelling", "Food"]);
        assert!(rename_in_list(&mut k, "travel|italy", "Trips"));
        assert_eq!(k, v(&["Food", "Travelling", "Trips", "Trips|Rome"]));
        let mut k = v(&["A", "B"]);
        assert!(rename_in_list(&mut k, "A", "b"));
        assert_eq!(k, v(&["b"]), "renaming into an existing keyword merges");
        let mut k = v(&["A"]);
        assert!(!rename_in_list(&mut k, "Z", "Y"));
        assert!(!rename_in_list(&mut k, "A", " | "), "an empty new name does nothing");
        let mut k = v(&["Ärger|Süß"]);
        assert!(rename_in_list(&mut k, "ärger", "Ruhe"));
        assert_eq!(k, v(&["Ruhe|Süß"]));
    }

    #[test]
    fn the_tree_lists_ancestors_first() {
        let t = tree_paths(&v(&["B|x", "A|b|c", "A2"]));
        assert_eq!(t, v(&["A", "A|b", "A|b|c", "A2", "B", "B|x"]));
    }

    #[test]
    fn xmp_properties_and_merge() {
        let k = v(&["Travel|Italy|Rome", "Food", "Tom & <Jerry>", "Ärger"]);
        let x = xmp_keyword_elements(&k);
        assert!(x.starts_with("<dc:subject><rdf:Bag>"));
        assert!(x.contains("<rdf:li>Rome</rdf:li>") && x.contains("<rdf:li>Ärger</rdf:li>"));
        assert!(x.contains("<rdf:li>Tom &amp; &lt;Jerry&gt;</rdf:li>"));
        assert!(x.contains("<lr:hierarchicalSubject><rdf:Bag><rdf:li>Travel|Italy|Rome</rdf:li></rdf:Bag></lr:hierarchicalSubject>"));
        assert_eq!(xmp_keyword_elements(&[]), "");
        assert!(!xmp_keyword_elements(&v(&["Food"])).contains("hierarchicalSubject"));
        let merged = merge_xmp_keywords(&v(&["Rome", "Food", "rome"]), &v(&["Travel|Italy|Rome"]));
        assert_eq!(merged, v(&["Food", "Travel|Italy|Rome"]));
    }

    #[test]
    fn file_tokens_are_safe() {
        assert_eq!(keywords_file_token(&v(&["Travel|Italy", "a/b", "Ärger"])), "Italy-a_b-Ärger");
        assert_eq!(keywords_file_token(&[]), "");
        assert!(keywords_file_token(&v(&[&"x".repeat(200)])).chars().count() <= 80);
        assert_eq!(keywords_file_token(&v(&["..."])), "");
    }
}
