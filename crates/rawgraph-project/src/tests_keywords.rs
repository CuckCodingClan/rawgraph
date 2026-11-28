//! Keywords: schema 3 (migration from 2, in-place diff), editing rules, filter, XMP read.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rawgraph_core::{CaptureInfo, ImageId};

use super::*;
use crate::writemode;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("rawgraph-project-keywords-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn entries(count: u64) -> Vec<ImageEntry> {
    (1..=count)
        .map(|n| ImageEntry {
            id: ImageId(n),
            uuid: crate::ids::new_uuid(),
            path: PathBuf::from(format!("/photos/shoot/IMG_{n:05}.RAW")),
            content_hash: format!("{n:064x}"),
            file_size: 1000 + n,
            modified_unix_ms: Some(1_700_000_000_000 + n as i64),
            capture: CaptureInfo { iso: Some(100.0), ..Default::default() },
            rating: 0,
            flag: Flag::None,
            color_label: None,
            keywords: Vec::new(),
            missing: false,
        })
        .collect()
}

fn project(count: u64) -> Project {
    let mut p = Project::new();
    p.merge_found(&entries(count));
    p
}

fn sql(path: &Path, query: &str) -> i64 {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row(query, [], |r| r.get(0)).unwrap()
}

fn schema_of(path: &Path) -> String {
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0)).unwrap()
}

fn changed_pages(before: &[u8], after: &[u8]) -> usize {
    let pages = before.len().max(after.len()).div_ceil(4096);
    (0..pages)
        .filter(|i| {
            let range = |b: &[u8]| b.get(i * 4096..((i + 1) * 4096).min(b.len())).map(<[u8]>::to_vec);
            range(before) != range(after)
        })
        .count()
}

/// A genuine schema-2 file: what the previous version wrote (no `keywords` table).
fn write_v2_file(path: &Path, p: &mut Project) {
    p.commit(path, "v2").unwrap();
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch("DROP TABLE keywords; UPDATE meta SET value = '2' WHERE key = 'schema_version';").unwrap();
    conn.close().unwrap();
}

// ------------------------------------------------------------------ schema

#[test]
fn migration_2_to_3_leaves_the_file_alone_and_backs_it_up_on_the_first_write() {
    let t = Tmp::new();
    let file = t.path("old.rawgraph");
    let mut base = project(4);
    write_v2_file(&file, &mut base);
    assert_eq!(schema_of(&file), "2");
    let original = fs::read(&file).unwrap();

    let mut p = Project::load(&file).unwrap();
    assert_eq!(fs::read(&file).unwrap(), original, "loading must not modify the file");
    assert_eq!(fs::read_dir(&t.0).unwrap().count(), 1, "loading must not leave other files behind");
    assert_eq!(p.images().len(), 4);
    assert!(p.images().iter().all(|e| e.keywords.is_empty()));

    // Keywords can be set on the migrated project; the first write converts the file.
    assert!(p.set_keywords(ImageId(2), &v(&["Reise|Graz"])));
    p.commit(&file, "keywords").unwrap();
    assert_eq!(schema_of(&file), "3");
    let bak = t.path("old.rawgraph.v2.bak");
    assert_eq!(fs::read(&bak).unwrap(), original, "the backup is the untouched old file");
    assert_eq!(Project::load(&file).unwrap().image(ImageId(2)).unwrap().keywords, v(&["Reise|Graz"]));

    // A file that is schema 3 gets no further backup.
    p.commit(&file, "again").unwrap();
    let baks = fs::read_dir(&t.0).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".bak")).count();
    assert_eq!(baks, 1);
}

#[test]
fn migration_chain_reaches_schema_3() {
    assert_eq!(SCHEMA_VERSION, 3);
    assert_eq!(crate::store::migration_steps(), vec![1, 2]);
}

#[test]
fn keywords_survive_a_round_trip_normalised() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project(3);
    let messy = v(&["  Reise | Österreich|Wörthersee ", "food", "FOOD", "Tom & <Jerry>", "日本", "", " | "]);
    assert!(p.set_keywords(ImageId(1), &messy));
    assert_eq!(p.image(ImageId(1)).unwrap().keywords, v(&["food", "Reise|Österreich|Wörthersee", "Tom & <Jerry>", "日本"]));
    assert!(!p.set_keywords(ImageId(1), &messy), "the same set again changes nothing");
    p.commit(&file, "kw").unwrap();
    let back = Project::load(&file).unwrap();
    assert_eq!(back, p);
    assert_eq!(back.image(ImageId(1)).unwrap().keywords.len(), 4, "case-insensitive duplicates were merged, case kept");
    // Written as a whole file (save as) the same.
    let other = t.path("copy.rawgraph");
    p.save(&other).unwrap();
    assert_eq!(Project::load(&other).unwrap(), p);
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords"), 4);
}

#[test]
fn an_in_place_commit_writes_only_the_changed_keyword_rows() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project(400);
    for n in 1..=400u64 {
        p.set_keywords(ImageId(n), &[format!("group {}", n % 20), "common".into(), format!("Reise|Land {}|Ort {n}", n % 7)]);
    }
    p.commit(&file, "first").unwrap();
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords"), 1200);

    // Baseline: a commit that changes nothing.
    let before = fs::read(&file).unwrap();
    p.commit(&file, "nothing").unwrap();
    let after_nothing = fs::read(&file).unwrap();
    let baseline = changed_pages(&before, &after_nothing);

    // One keyword added to one image.
    p.add_keywords(&[ImageId(200)], &v(&["extra"]));
    p.commit(&file, "one keyword").unwrap();
    assert_eq!(p.last_write().unwrap().mode, WriteMode::InPlace);
    let after_one = fs::read(&file).unwrap();
    let pages = changed_pages(&after_nothing, &after_one);
    assert!(pages <= baseline + 3, "{pages} pages changed for one keyword row (an empty commit changes {baseline})");
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords"), 1201);
    assert_eq!(Project::load(&file).unwrap(), p);

    // Removing a keyword from one image, renaming one for all, removing an image: rows follow exactly.
    assert_eq!(p.remove_keyword(&[ImageId(200)], "EXTRA"), 1);
    assert_eq!(p.rename_keyword("group 3", "Gruppe drei"), 20);
    p.remove_images(&[ImageId(1)]);
    p.commit(&file, "edits").unwrap();
    assert_eq!(p.last_write().unwrap().mode, WriteMode::InPlace);
    let loaded = Project::load(&file).unwrap();
    assert_eq!(loaded, p);
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords WHERE image_id = 1"), 0, "rows of a removed image are gone");
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords WHERE keyword = 'group 3'"), 0);
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords WHERE keyword = 'Gruppe drei'"), 20);
    // An in-place result equals a full write of the same project.
    let whole = t.path("whole.rawgraph");
    p.save(&whole).unwrap();
    assert_eq!(Project::load(&whole).unwrap(), loaded);
}

#[test]
fn a_full_rewrite_place_writes_the_keywords_too() {
    let t = Tmp::new();
    let file = t.path("p.rawgraph");
    let mut p = project(5);
    p.commit(&file, "first").unwrap();
    p.add_keywords(&[ImageId(2), ImageId(3)], &v(&["Hochzeit|Kirche"]));
    writemode::TEST_OVERRIDE.with(|o| o.set(Some(WriteMode::FullRewrite)));
    p.commit(&file, "network").unwrap();
    writemode::TEST_OVERRIDE.with(|o| o.set(None));
    assert_eq!(p.last_write().unwrap().mode, WriteMode::FullRewrite);
    assert_eq!(Project::load(&file).unwrap(), p);
    assert_eq!(sql(&file, "SELECT COUNT(*) FROM keywords"), 2);
}

// ------------------------------------------------------------------ editing

#[test]
fn add_remove_and_count() {
    let mut p = project(4);
    let all: Vec<ImageId> = (1..=4).map(ImageId).collect();
    assert_eq!(p.add_keywords(&all[..3], &v(&["Reise|Graz", "reise|graz", "Food"])), 3);
    assert_eq!(p.add_keywords(&all[..3], &v(&["Food"])), 0, "already there");
    assert_eq!(p.add_keywords(&all[3..], &v(&["Food"])), 1);
    assert_eq!(p.image(ImageId(1)).unwrap().keywords, v(&["Food", "Reise|Graz"]));
    let counts = p.keyword_counts();
    assert_eq!(counts, vec![KeywordCount { keyword: "Food".into(), images: 4 }, KeywordCount { keyword: "Reise|Graz".into(), images: 3 }]);
    assert_eq!(p.remove_keyword(&all, "food"), 4, "case-insensitive");
    assert_eq!(p.remove_keyword(&all, "Reise"), 0, "only the exact path is removed, not the parent of a path");
    assert_eq!(p.remove_keyword(&all, "Reise|Graz"), 3);
    assert!(p.keyword_counts().is_empty());
    assert_eq!(p.add_keywords(&[ImageId(99)], &v(&["x"])), 0, "unknown image");
}

#[test]
fn renaming_is_project_wide_hierarchical_and_merging() {
    let mut p = project(4);
    p.set_keywords(ImageId(1), &v(&["Travel|Italy|Rome", "Food"]));
    p.set_keywords(ImageId(2), &v(&["Travel|Italy"]));
    p.set_keywords(ImageId(3), &v(&["travel", "Trips"]));
    p.set_keywords(ImageId(4), &v(&["Travelling"]));
    assert_eq!(p.rename_keyword("TRAVEL", "Trips"), 3, "images 1, 2 and 3 change; the longer name is not a child");
    assert_eq!(p.image(ImageId(1)).unwrap().keywords, v(&["Food", "Trips|Italy|Rome"]));
    assert_eq!(p.image(ImageId(2)).unwrap().keywords, v(&["Trips|Italy"]));
    assert_eq!(p.image(ImageId(3)).unwrap().keywords, v(&["Trips"]), "the renamed keyword merged with the existing one");
    assert_eq!(p.image(ImageId(4)).unwrap().keywords, v(&["Travelling"]));
    assert_eq!(p.rename_keyword("Trips|Italy", "Trips|Italien"), 2);
    assert_eq!(p.image(ImageId(1)).unwrap().keywords, v(&["Food", "Trips|Italien|Rome"]));
    assert_eq!(p.rename_keyword("nothing", "x"), 0);
    assert_eq!(p.rename_keyword("Food", "  "), 0, "an empty new name does nothing");
}

#[test]
fn the_filter_finds_any_all_and_none() {
    let mut p = project(5);
    p.set_keywords(ImageId(1), &v(&["Reise|Italien|Rom", "Food"]));
    p.set_keywords(ImageId(2), &v(&["reise"]));
    p.set_keywords(ImageId(3), &v(&["Food"]));
    p.set_keywords(ImageId(4), &v(&["Food", "Reise|Italien"]));
    let find = |f: Filter| -> Vec<u64> { p.images().iter().filter(|e| f.matches(e)).map(|e| e.id.0).collect() };
    let base = Filter { include_missing: true, ..Default::default() };
    assert_eq!(find(base.clone()), vec![1, 2, 3, 4, 5], "no keyword filter");
    let any = |k: &[&str]| Filter { keywords: v(k), keyword_match: KeywordMatch::Any, ..base.clone() };
    let all = |k: &[&str]| Filter { keywords: v(k), keyword_match: KeywordMatch::All, ..base.clone() };
    assert_eq!(find(any(&["REISE"])), vec![1, 2, 4], "a parent finds its children, case-insensitive");
    assert_eq!(find(any(&["Reise|Italien"])), vec![1, 4]);
    assert_eq!(find(any(&["Rom", "Food"])), vec![1, 3, 4], "Rom is not a top-level keyword");
    assert_eq!(find(all(&["Food", "Reise"])), vec![1, 4]);
    assert_eq!(find(all(&["Food", "Reise|Italien|Rom"])), vec![1]);
    assert_eq!(find(Filter { no_keywords: true, ..base.clone() }), vec![5]);
    assert_eq!(find(Filter { no_keywords: true, keywords: v(&["Food"]), ..base.clone() }), vec![5], "none wins");
    assert_eq!(find(Filter { min_rating: Some(1), ..any(&["Food"]) }), Vec::<u64>::new(), "combines with the other filters");
}

// ------------------------------------------------------------------ XMP

const PACKET: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:lr="http://ns.adobe.com/lightroom/1.0/" xmlns:xmp="http://ns.adobe.com/xap/1.0/" xmp:Rating="3">
<dc:subject><rdf:Bag><rdf:li>Wörthersee</rdf:li><rdf:li>Tom &amp; Jerry</rdf:li><rdf:li>日本</rdf:li><rdf:li>Food</rdf:li><rdf:li>wörthersee</rdf:li></rdf:Bag></dc:subject>
<lr:hierarchicalSubject><rdf:Bag><rdf:li>Reise|Österreich|Wörthersee</rdf:li><rdf:li>Reise|Österreich</rdf:li></rdf:Bag></lr:hierarchicalSubject>
</rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;

#[test]
fn xmp_keywords_are_read_with_hierarchy_entities_and_non_ascii() {
    let c = parse_xmp_culling(PACKET).unwrap();
    assert_eq!(c.rating, Some(3));
    assert_eq!(c.keywords, v(&["Food", "Reise|Österreich", "Reise|Österreich|Wörthersee", "Tom & Jerry", "日本"]));

    // Other prefixes for the same namespaces; flat names only; Seq and Alt lists.
    let other = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
        xmlns:d="http://purl.org/dc/elements/1.1/"><rdf:Description><d:subject><rdf:Seq><rdf:li>eins</rdf:li><rdf:li>zwei</rdf:li></rdf:Seq></d:subject></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    assert_eq!(parse_xmp_culling(other).unwrap().keywords, v(&["eins", "zwei"]));
    // Items outside the two lists are not keywords.
    let foreign = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
        xmlns:dc="http://purl.org/dc/elements/1.1/"><rdf:Description><dc:creator><rdf:Seq><rdf:li>Someone</rdf:li></rdf:Seq></dc:creator></rdf:Description></rdf:RDF></x:xmpmeta>"#;
    assert_eq!(parse_xmp_culling(foreign).unwrap(), XmpCulling::default());
    // Character references and a truncated packet do not panic.
    let refs = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#" xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:subject><rdf:Bag><rdf:li>a&#228;&#x4E16;&apos;</rdf:li></rdf:Bag></dc:subject>"#;
    assert_eq!(parse_xmp_culling(refs).map(|c| c.keywords).unwrap_or_default(), v(&["aä世'"]));
    for n in (0..PACKET.len()).step_by(7).filter(|n| PACKET.is_char_boundary(*n)) {
        let _ = parse_xmp_culling(&PACKET[..n]);
    }
}

#[test]
fn xmp_keywords_merge_into_the_entry_and_never_remove() {
    let mut p = project(2);
    p.set_keywords(ImageId(1), &v(&["Mine", "food"]));
    let c = parse_xmp_culling(PACKET).unwrap();
    assert!(p.apply_xmp_keywords(ImageId(1), &c));
    assert_eq!(p.image(ImageId(1)).unwrap().keywords, v(&["food", "Mine", "Reise|Österreich", "Reise|Österreich|Wörthersee", "Tom & Jerry", "日本"]));
    assert!(!p.apply_xmp_keywords(ImageId(1), &c), "applying twice changes nothing");
    assert!(!p.apply_xmp_keywords(ImageId(1), &XmpCulling::default()));
    assert!(!p.apply_xmp_keywords(ImageId(77), &c));
}

#[test]
fn sidecar_import_merges_keywords_and_respects_only_unrated() {
    let t = Tmp::new();
    let dir = t.path("photos");
    fs::create_dir_all(&dir).unwrap();
    for n in ["a.arw", "b.arw"] {
        fs::write(dir.join(n), n).unwrap();
        fs::write(dir.join(n.replace(".arw", ".xmp")), PACKET).unwrap();
    }
    let mut p = Project::new();
    p.scan_folder(&dir, false, &|p| p.extension().is_some_and(|e| e == "arw"), &|_| None);
    p.image_mut(ImageId(2)).unwrap().rating = 5;
    p.set_keywords(ImageId(1), &v(&["Mine"]));
    let changed = p.import_xmp_sidecars(true);
    assert_eq!(changed, vec![ImageId(1)], "the rated image is skipped as a whole");
    let (a, b) = (p.image(ImageId(1)).unwrap(), p.image(ImageId(2)).unwrap());
    assert_eq!((a.rating, b.rating), (3, 5));
    assert_eq!(a.keywords.len(), 6, "the XMP keywords were added to the existing one");
    assert!(b.keywords.is_empty());
    let changed = p.import_xmp_sidecars(false);
    assert_eq!(changed, vec![ImageId(2)], "the first image has everything already; the second takes rating and keywords");
    assert_eq!(p.image(ImageId(2)).unwrap().keywords.len(), 5);
    assert_eq!(p.image(ImageId(2)).unwrap().rating, 3);
}
