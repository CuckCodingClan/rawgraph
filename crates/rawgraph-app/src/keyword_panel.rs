//! The keyword panel under the filmstrip: keywords of the selected images (several at once) with
//! counts ("3 of 5"), an add field with autocomplete from all keywords of the project, removal,
//! project-wide rename and a hierarchical display (`Parent|Child`).
//!
//! The logic (rows, suggestions, applying edits to a [`Session`]) is separate from the drawing so
//! it can be tested without a window. Keyboard: `K` focuses the add field (when no text field has
//! the focus); Enter adds, Tab completes the first suggestion, Escape leaves the field.

use eframe::egui::{self, Color32, Id, Key, RichText};
use rawgraph_core::keywords::{
    has_keyword, keyword_depth, keyword_is_within, keyword_key, keyword_leaf, normalize_keyword, normalize_keywords, tree_paths,
};
use rawgraph_project::{add_keywords_to_list, remove_keyword_from_list, ImageEntry, KeywordCount};

use crate::session::Session;

/// One line of the list: a keyword path of the selection (or an ancestor of one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Full path (`Parent|Child`).
    pub path: String,
    pub depth: usize,
    /// Last level, shown.
    pub name: String,
    /// Selected images that carry this keyword or one below it.
    pub count: usize,
    /// Selected images that carry exactly this keyword (0 for a path that only exists as a parent).
    pub exact: usize,
}

impl Row {
    /// "3 of 5" for a selection of several images, empty for one image.
    pub fn count_text(&self, total: usize) -> String {
        if total <= 1 {
            String::new()
        } else {
            format!("{} of {total}", self.count)
        }
    }
}

/// The rows for the keywords of `selection`: the tree of all their paths, parents first.
pub fn rows(selection: &[&ImageEntry]) -> Vec<Row> {
    let all: Vec<String> = selection.iter().flat_map(|e| e.keywords.iter().cloned()).collect();
    tree_paths(&normalize_keywords(&all))
        .into_iter()
        .map(|path| Row {
            depth: keyword_depth(&path) - 1,
            name: keyword_leaf(&path).to_owned(),
            count: selection.iter().filter(|e| has_keyword(&e.keywords, &path)).count(),
            exact: selection.iter().filter(|e| e.keywords.iter().any(|k| keyword_key(k) == keyword_key(&path))).count(),
            path,
        })
        .collect()
}

/// Text of the add field -> keywords: separated by comma, semicolon or new line; `|` makes levels.
pub fn parse_input(text: &str) -> Vec<String> {
    normalize_keywords(text.split([',', ';', '\n']))
}

/// Keywords of the project that match what is typed (a prefix of the whole path or of its last
/// level first, then any part), leaving out those that every selected image carries already.
pub fn suggestions(counts: &[KeywordCount], typed: &str, selection: &[&ImageEntry], limit: usize) -> Vec<String> {
    // Only the part after the last separator of the typed text selects among the keywords.
    let needle = keyword_key(typed.rsplit([',', ';']).next().unwrap_or("").trim());
    if needle.is_empty() {
        return Vec::new();
    }
    let carried_by_all = |k: &str| !selection.is_empty() && selection.iter().all(|e| e.keywords.iter().any(|x| keyword_key(x) == keyword_key(k)));
    let mut starts: Vec<&KeywordCount> = Vec::new();
    let mut contains: Vec<&KeywordCount> = Vec::new();
    for c in counts.iter().filter(|c| !carried_by_all(&c.keyword)) {
        let (path, leaf) = (keyword_key(&c.keyword), keyword_key(keyword_leaf(&c.keyword)));
        if path.starts_with(&needle) || leaf.starts_with(&needle) {
            starts.push(c);
        } else if path.contains(&needle) {
            contains.push(c);
        }
    }
    // The more images use a keyword, the earlier it is offered within its group.
    starts.sort_by_key(|c| std::cmp::Reverse(c.images));
    contains.sort_by_key(|c| std::cmp::Reverse(c.images));
    starts.into_iter().chain(contains).take(limit).map(|c| c.keyword.clone()).collect()
}

/// Replaces the last item of the typed text with `suggestion` (what Tab and a click do).
pub fn complete(typed: &str, suggestion: &str) -> String {
    match typed.rfind([',', ';']) {
        Some(i) => format!("{} {suggestion}", &typed[..=i]),
        None => suggestion.to_owned(),
    }
}

// ------------------------------------------------------------------ edits on the session

/// Adds the keywords in `text` to every selected image. Returns the images that changed.
pub fn add_to_selection(session: &mut Session, text: &str) -> usize {
    let add = parse_input(text);
    if add.is_empty() {
        return 0;
    }
    let ids = session.selected_images();
    session.edit_keywords(&ids, |list| add_keywords_to_list(list, &add)).len()
}

/// Removes a keyword (and everything below it) from every selected image. Returns the images that changed.
pub fn remove_from_selection(session: &mut Session, path: &str) -> usize {
    let ids = session.selected_images();
    let path = path.to_owned();
    session
        .edit_keywords(&ids, move |list| {
            let before = list.len();
            list.retain(|k| !keyword_is_within(k, &path));
            list.len() != before
        })
        .len()
}

/// Removes exactly one keyword path (not what is below it) from the selected images.
pub fn remove_exact_from_selection(session: &mut Session, path: &str) -> usize {
    let ids = session.selected_images();
    let path = path.to_owned();
    session.edit_keywords(&ids, move |list| remove_keyword_from_list(list, &path)).len()
}

/// Renames a keyword in the whole project. The new name is a path as well (`Reise|Italien`); an
/// empty or unchanged name does nothing. Returns the images that changed.
pub fn rename_everywhere(session: &mut Session, from: &str, to: &str) -> usize {
    match normalize_keyword(to) {
        Some(to) if keyword_key(&to) != keyword_key(from) || to != from => session.rename_keyword(from, &to).len(),
        _ => 0,
    }
}

// ------------------------------------------------------------------ the panel

/// State of the panel (not part of the project).
#[derive(Default)]
pub struct KeywordPanel {
    input: String,
    /// `K` was pressed: the add field takes the focus when it is drawn next.
    pub focus_add: bool,
    /// A rename in progress: (keyword path, new name being typed).
    rename: Option<(String, String)>,
    /// What the last action did, shown under the list.
    note: String,
}

fn add_field_id() -> Id {
    Id::NULL.with("keyword-add-field")
}

impl KeywordPanel {
    pub fn show(&mut self, ui: &mut egui::Ui, session: &mut Session) {
        if session.demo {
            return;
        }
        let ids = session.selected_images();
        let selection: Vec<ImageEntry> = ids.iter().filter_map(|id| session.entry(*id).cloned()).collect();
        let refs: Vec<&ImageEntry> = selection.iter().collect();
        let total = refs.len();

        ui.horizontal(|ui| {
            ui.label(RichText::new("Keywords").strong());
            match total {
                0 => ui.weak("no image selected"),
                1 => ui.weak("1 image"),
                n => ui.weak(format!("{n} images selected")),
            };
        });

        // The add field with autocomplete.
        let field = ui.add_enabled(total > 0, egui::TextEdit::singleline(&mut self.input).id(add_field_id()).hint_text("Add keyword (K), Parent|Child").desired_width(f32::INFINITY));
        if self.focus_add && total > 0 {
            field.request_focus();
            self.focus_add = false;
        }
        // The counts of all keywords are only needed while something is typed.
        let offered = if self.input.trim().is_empty() { Vec::new() } else { suggestions(&session.keyword_counts(), &self.input, &refs, 6) };
        if field.has_focus() && !offered.is_empty() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Tab)) {
            self.input = complete(&self.input, &offered[0]);
        }
        let submit = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if submit {
            self.add(session, &self.input.clone());
            field.request_focus();
        }
        if field.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
            field.surrender_focus();
        }
        let mut chosen = None;
        if !offered.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for s in &offered {
                    if ui.small_button(s).on_hover_text("Add this keyword").clicked() {
                        chosen = Some(s.clone());
                    }
                }
            });
        }
        if let Some(s) = chosen {
            self.add(session, &s);
        }

        // The list.
        let list = rows(&refs);
        if list.is_empty() {
            ui.weak(if total == 0 { "Select an image to see and edit its keywords." } else { "No keywords yet." });
        }
        let mut remove: Option<String> = None;
        let mut start_rename: Option<String> = None;
        egui::ScrollArea::vertical().id_salt("keyword-list").max_height(120.0).auto_shrink([false, true]).show(ui, |ui| {
            for row in &list {
                ui.horizontal(|ui| {
                    ui.add_space(row.depth as f32 * 14.0);
                    let name = RichText::new(&row.name);
                    let label = ui.label(if row.exact == 0 { name.weak() } else { name });
                    label.clone().on_hover_text(format!("{}\nRight-click: rename in the whole project", row.path));
                    label.context_menu(|ui| {
                        if ui.button("Rename in the whole project...").clicked() {
                            start_rename = Some(row.path.clone());
                            ui.close();
                        }
                        if row.exact > 0 && ui.button("Remove only this level").clicked() {
                            remove_exact_from_selection(session, &row.path);
                            ui.close();
                        }
                    });
                    let text = row.count_text(total);
                    if !text.is_empty() {
                        ui.add(egui::Label::new(RichText::new(text).size(11.5).color(Color32::from_gray(150))));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("x").on_hover_text("Remove from the selected images").clicked() {
                            remove = Some(row.path.clone());
                        }
                    });
                });
            }
        });
        if let Some(path) = remove {
            let n = remove_from_selection(session, &path);
            self.note = format!("Removed \"{}\" from {n} image{}", keyword_leaf(&path), if n == 1 { "" } else { "s" });
        }
        if let Some(path) = start_rename {
            self.rename = Some((path.clone(), path));
        }

        // Rename in the whole project.
        let mut done = false;
        if let Some((from, to)) = &mut self.rename {
            ui.horizontal(|ui| {
                ui.label("Rename to");
                let r = ui.add(egui::TextEdit::singleline(to).desired_width(150.0));
                r.request_focus();
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if ui.button("Rename").clicked() || enter {
                    let n = rename_everywhere(session, from, to);
                    self.note = format!("Renamed in {n} image{}", if n == 1 { "" } else { "s" });
                    done = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = true;
                }
            });
        }
        if done {
            self.rename = None;
        }
        if !self.note.is_empty() {
            ui.weak(&self.note);
        }
    }

    fn add(&mut self, session: &mut Session, text: &str) {
        let n = add_to_selection(session, text);
        self.note = match (parse_input(text).len(), n) {
            (0, _) => String::new(),
            (_, 0) => "Already there".to_owned(),
            (_, n) => format!("Added to {n} image{}", if n == 1 { "" } else { "s" }),
        };
        self.input.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::ImageId;
    use std::path::PathBuf;

    fn entry(id: u64, keywords: &[&str]) -> ImageEntry {
        ImageEntry {
            id: ImageId(id),
            uuid: format!("{id:032x}"),
            path: PathBuf::from(format!("img{id}.png")),
            content_hash: String::new(),
            file_size: 0,
            modified_unix_ms: None,
            capture: Default::default(),
            rating: 0,
            flag: rawgraph_project::Flag::None,
            color_label: None,
            keywords: normalize_keywords(keywords.iter().copied()),
            missing: false,
        }
    }

    fn texts(rows: &[Row]) -> Vec<(String, usize, usize, usize)> {
        rows.iter().map(|r| (r.path.clone(), r.depth, r.count, r.exact)).collect()
    }

    #[test]
    fn rows_count_the_selection_and_show_the_hierarchy() {
        let (a, b, c) = (entry(1, &["Reise|Italien|Rom", "Food"]), entry(2, &["Reise|Italien", "food"]), entry(3, &[]));
        let sel: Vec<&ImageEntry> = vec![&a, &b, &c];
        let r = rows(&sel);
        assert_eq!(
            texts(&r),
            vec![
                ("Food".into(), 0, 2, 2),
                ("Reise".into(), 0, 2, 0),
                ("Reise|Italien".into(), 1, 2, 1),
                ("Reise|Italien|Rom".into(), 2, 1, 1),
            ],
            "Food and food are one keyword; the parent Reise only exists as a path"
        );
        assert_eq!(r[3].name, "Rom");
        assert_eq!(r[3].count_text(3), "1 of 3");
        assert_eq!(r[3].count_text(1), "");
        assert!(rows(&[&c]).is_empty());
        assert_eq!(texts(&rows(&[&a])).len(), 4);
    }

    #[test]
    fn input_is_split_and_normalised() {
        assert_eq!(parse_input(" Reise | Graz , food;  FOOD\nneu "), vec!["food".to_owned(), "neu".to_owned(), "Reise|Graz".to_owned()]);
        assert!(parse_input(" , ; ").is_empty());
    }

    #[test]
    fn suggestions_prefer_prefixes_and_skip_what_everyone_has() {
        let counts = vec![
            KeywordCount { keyword: "Food".into(), images: 9 },
            KeywordCount { keyword: "Reise|Rom".into(), images: 2 },
            KeywordCount { keyword: "Roman".into(), images: 5 },
            KeywordCount { keyword: "Wiese".into(), images: 7 },
            KeywordCount { keyword: "Reise|Prom".into(), images: 1 },
        ];
        let a = entry(1, &["Food"]);
        let sel: Vec<&ImageEntry> = vec![&a];
        assert_eq!(suggestions(&counts, "ro", &sel, 10), vec!["Roman", "Reise|Rom", "Reise|Prom"], "prefix of the name or of its last level first, then any part; most used first");
        assert_eq!(suggestions(&counts, "FO", &sel, 10), Vec::<String>::new(), "Food is carried by the whole selection");
        assert_eq!(suggestions(&counts, "FO", &[], 10), vec!["Food"]);
        assert_eq!(suggestions(&counts, "x, ro", &sel, 1), vec!["Roman"], "only the last item counts, limited");
        assert!(suggestions(&counts, "  ", &sel, 5).is_empty());
        assert_eq!(complete("x, ro", "Roman"), "x, Roman");
        assert_eq!(complete("ro", "Roman"), "Roman");
    }

    fn session_with(n: usize) -> (Session, Vec<ImageId>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(crate::session::tests::png_project(dir.path(), n), crate::session::tests::provider());
        let ids = s.visible().to_vec();
        s.select_id(ids[0]);
        (s, ids, dir)
    }

    #[test]
    fn edits_apply_to_the_whole_selection_and_mark_the_project_unsaved() {
        let (mut s, ids, _dir) = session_with(4);
        assert!(!s.dirty);
        s.click_image(ids[0], false, false, &ids);
        s.click_image(ids[2], true, false, &ids);
        assert_eq!(s.selected_images(), vec![ids[0], ids[2]]);
        assert_eq!(add_to_selection(&mut s, "Reise|Graz, Food"), 2);
        assert!(s.dirty);
        assert_eq!(s.entry(ids[1]).unwrap().keywords, Vec::<String>::new(), "unselected images are untouched");
        assert_eq!(s.entry(ids[2]).unwrap().keywords, vec!["Food".to_owned(), "Reise|Graz".to_owned()]);
        assert_eq!(add_to_selection(&mut s, "food"), 0, "already there (case-insensitive)");
        // One image gets a different keyword; counts follow the selection.
        s.click_image(ids[2], false, false, &ids);
        assert_eq!(add_to_selection(&mut s, "extra"), 1);
        s.click_image(ids[0], false, false, &ids);
        s.click_image(ids[2], true, false, &ids);
        let sel: Vec<ImageEntry> = s.selected_images().iter().filter_map(|id| s.entry(*id).cloned()).collect();
        let r = rows(&sel.iter().collect::<Vec<_>>());
        let extra = r.iter().find(|x| x.path == "extra").unwrap();
        assert_eq!((extra.count, extra.count_text(2)), (1, "1 of 2".to_owned()));
        // Remove removes the subtree from the selected images only.
        assert_eq!(remove_from_selection(&mut s, "Reise"), 2);
        assert_eq!(s.entry(ids[0]).unwrap().keywords, vec!["Food".to_owned()]);
        assert_eq!(remove_exact_from_selection(&mut s, "food"), 2);
        assert!(s.entry(ids[0]).unwrap().keywords.is_empty());
        assert_eq!(s.entry(ids[2]).unwrap().keywords, vec!["extra".to_owned()]);
    }

    #[test]
    fn renaming_is_project_wide_and_reaches_the_graph() {
        let (mut s, ids, _dir) = session_with(3);
        s.click_image(ids[0], false, false, &ids);
        s.click_image(ids[2], true, false, &ids);
        add_to_selection(&mut s, "Reise|Italien|Rom");
        s.click_image(ids[1], false, false, &ids);
        add_to_selection(&mut s, "reise");
        let gen = s.images_gen;
        assert_eq!(rename_everywhere(&mut s, "Reise", "Trips"), 3, "a child and a lower-case spelling are renamed too");
        assert!(s.images_gen > gen, "the collection queries are recomputed");
        assert_eq!(s.entry(ids[0]).unwrap().keywords, vec!["Trips|Italien|Rom".to_owned()]);
        assert_eq!(s.entry(ids[1]).unwrap().keywords, vec!["Trips".to_owned()]);
        assert_eq!(s.provider_keywords(ids[0]), vec!["Trips|Italien|Rom".to_owned()], "the graph sees the new name");
        assert_eq!(rename_everywhere(&mut s, "Trips", "  "), 0);
        assert_eq!(rename_everywhere(&mut s, "Trips", "Trips"), 0, "same name");
        assert_eq!(rename_everywhere(&mut s, "Trips", "TRIPS"), 3, "only the capitalisation changes: still a rename");
        assert_eq!(s.entry(ids[1]).unwrap().keywords, vec!["TRIPS".to_owned()]);
        assert_eq!(s.entry(ids[0]).unwrap().keywords, vec!["TRIPS|Italien|Rom".to_owned()]);
        assert_eq!(s.keyword_counts().len(), 2);
    }

    #[test]
    fn click_selection_follows_ctrl_and_shift() {
        let (mut s, ids, _dir) = session_with(5);
        s.click_image(ids[1], false, false, &ids);
        assert_eq!(s.selected_images(), vec![ids[1]]);
        s.click_image(ids[3], false, true, &ids);
        assert_eq!(s.selected_images(), vec![ids[1], ids[2], ids[3]], "shift selects the range from the primary image");
        s.click_image(ids[2], true, false, &ids);
        assert_eq!(s.selected_images(), vec![ids[1], ids[3]], "ctrl removes one");
        s.click_image(ids[4], false, false, &ids);
        assert_eq!(s.selected_images(), vec![ids[4]], "a plain click starts over");
        s.click_image(ids[0], true, false, &ids);
        s.click_image(ids[0], true, false, &ids);
        assert_eq!(s.selected_images().len(), 1);
        // The primary image changes without the list (arrow keys): that image is the selection.
        s.click_image(ids[0], true, false, &ids);
        s.selected = Some(ids[3]);
        assert_eq!(s.selected_images(), vec![ids[3]]);
    }

    #[test]
    fn the_gallery_filter_finds_any_all_and_none() {
        let (mut s, ids, _dir) = session_with(5);
        s.click_image(ids[0], false, false, &ids);
        s.click_image(ids[1], true, false, &ids);
        add_to_selection(&mut s, "Reise|Italien, Food");
        s.click_image(ids[2], false, false, &ids);
        add_to_selection(&mut s, "Food");
        s.click_image(ids[3], false, false, &ids);
        add_to_selection(&mut s, "reise");
        let shown = |s: &Session| s.visible().to_vec();
        assert_eq!(shown(&s).len(), 5);
        s.filter.toggle_keyword("Reise");
        s.refresh_visible();
        assert_eq!(shown(&s), vec![ids[0], ids[1], ids[3]], "a parent finds its children, case-insensitive");
        s.filter.toggle_keyword("Food");
        s.refresh_visible();
        assert_eq!(shown(&s), vec![ids[0], ids[1], ids[2], ids[3]], "any");
        s.filter.keyword_match = rawgraph_project::KeywordMatch::All;
        s.refresh_visible();
        assert_eq!(shown(&s), vec![ids[0], ids[1]], "all");
        s.filter.no_keywords = true;
        s.refresh_visible();
        assert_eq!(shown(&s), vec![ids[4]], "no keywords");
        s.filter = Default::default();
        s.refresh_visible();
        assert_eq!(shown(&s).len(), 5);
    }

    #[test]
    fn the_keyword_measure_works_on_the_edited_keywords() {
        use rawgraph_core::{NodeData, NodeKind};
        let (mut s, ids, _dir) = session_with(4);
        s.click_image(ids[0], false, false, &ids);
        s.click_image(ids[2], true, false, &ids);
        add_to_selection(&mut s, "Wedding|Church");
        s.click_image(ids[1], false, false, &ids);
        add_to_selection(&mut s, "wedding");
        let graph = &mut s.project.graph;
        let existing = graph.nodes().find(|n| n.kind == NodeKind::Source).map(|n| n.id);
        let src = existing.unwrap_or_else(|| graph.add_node(NodeKind::Source, [0.0, 0.0]));
        let filter = graph.add_node(NodeKind::MetadataFilter, [1.0, 0.0]);
        graph.connect(src, filter, 0).unwrap();
        let split = graph.add_node(NodeKind::MetadataSplit, [1.0, 1.0]);
        graph.connect(src, split, 0).unwrap();
        let img = ImageId(0);
        let n = graph.node_mut(filter).unwrap();
        n.set_param("measure", img, 5.0);
        assert!(n.set_data(img, Some(NodeData::Keyword("church".into()))));
        let n = graph.node_mut(split).unwrap();
        n.set_param("measure", img, 5.0);
        let range = |name: &str| rawgraph_core::NamedRange { name: name.into(), min: 0.0, max: 0.0 };
        assert!(n.set_data(img, Some(NodeData::Ranges(vec![range("church"), range("wedding")]))));
        let (all, caps) = s.id_captures();
        let r = crate::collection::compute(&s.project.graph, &all, &caps, None);
        let f = &r.filters[&filter];
        assert_eq!((f.reaching, f.matching, f.missing), (4, 2, 0), "images 0 and 2 carry Church");
        let sp = &r.splits[&split];
        assert_eq!(sp.per_port[..2], [2, 1], "church, then wedding");
        assert_eq!(sp.unmatched(), 1);
        // An edit changes the answer at once (the captures are rebuilt).
        s.click_image(ids[3], false, false, &ids);
        add_to_selection(&mut s, "Church");
        let (all, caps) = s.id_captures();
        let r = crate::collection::compute(&s.project.graph, &all, &caps, None);
        assert_eq!(r.filters[&filter].matching, 3);
    }
}
