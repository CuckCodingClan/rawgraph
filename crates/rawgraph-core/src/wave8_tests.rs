//! Tests of the wave 8 additions: Source folders, Merge collisions, the Layer
//! node, Develop tone, Curves, Lens Correction, Export, output spaces and the
//! version 2 graph format.

use std::collections::HashMap;
use std::time::Instant;

use crate::color::{
    acescg_to_output, acescg_to_rec709, invert, mul, mul_vec, rec709_to_acescg, rec709_to_output, rgb_to_xyz,
    srgb_oetf, xy_to_xyz, Mat3, D65_XY, REC709_TO_XYZ,
};
use crate::graph::SPLIT_UNMATCHED_PORT;
use crate::*;

const fn id(n: u64) -> ImageId {
    ImageId(n)
}

/// A project: images in order with their capture info.
struct Lib {
    images: Vec<ImageId>,
    caps: HashMap<ImageId, CaptureInfo>,
}

impl Lib {
    /// (id, path, iso)
    fn new(entries: &[(u64, Option<&str>, Option<f32>)]) -> Lib {
        let mut images = Vec::new();
        let mut caps = HashMap::new();
        for (n, path, iso) in entries {
            images.push(id(*n));
            caps.insert(id(*n), CaptureInfo { path: path.map(str::to_owned), iso: *iso, ..Default::default() });
        }
        Lib { images, caps }
    }

    fn with<R>(&self, f: impl FnOnce(&Catalog) -> R) -> R {
        let cap = |i: ImageId| self.caps.get(&i).cloned();
        f(&Catalog { images: &self.images, capture: &cap, memo: None })
    }

    fn members(&self, g: &Graph, node: NodeId, port: usize) -> Vec<u64> {
        self.with(|c| g.members(node, port, c)).iter().map(|i| i.0).collect()
    }

    fn contains(&self, g: &Graph, node: NodeId, image: u64) -> bool {
        self.with(|c| g.contains_in(node, id(image), c))
    }
}

fn source_in(g: &mut Graph, folder: Option<&str>, subfolders: bool) -> NodeId {
    let s = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let n = g.node_mut(s).unwrap();
    if let Some(f) = folder {
        assert!(n.set_data(id(0), Some(NodeData::Folder(f.to_owned()))));
    }
    n.set_param("subfolders", id(0), if subfolders { 1.0 } else { 0.0 });
    s
}

fn set_collision(g: &mut Graph, merge: NodeId, option: f32) {
    g.node_mut(merge).unwrap().set_param("collision", id(0), option);
}

// ---------------------------------------------------------------------------
// Paths and Source folders

#[test]
fn empty_source_is_distinct_from_legacy_catalog_and_round_trips() {
    let lib = folder_lib();
    let mut g = Graph::new();
    let legacy = source_in(&mut g, None, true);
    let empty = source_in(&mut g, Some(""), true);
    let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
    g.connect(empty, out, 0).unwrap();
    assert_eq!(lib.members(&g, legacy, 0).len(), lib.images.len());
    for node in [empty, out] {
        assert!(lib.members(&g, node, 0).is_empty());
        assert!(!lib.contains(&g, node, 1));
    }
    let restored = Graph::from_json(&g.to_json()).unwrap();
    assert_eq!(lib.members(&restored, legacy, 0).len(), lib.images.len());
    assert!(lib.members(&restored, out, 0).is_empty());
    assert!(g.node_mut(empty).unwrap().set_data(id(0), Some(NodeData::Folder("C:/Photos/Day1".into()))));
    assert_eq!(lib.members(&g, out, 0), vec![1, 2, 3]);
    assert!(g.node_mut(empty).unwrap().set_data(id(0), Some(NodeData::Folder(String::new()))));
    assert!(lib.members(&g, out, 0).is_empty());
}

#[test]
fn path_in_folder_rules() {
    // Separators are the same, case is ignored, trailing separators are ignored.
    assert!(path_in_folder("C:\\Photos\\2026\\a.CR3", "c:/photos/2026", false));
    assert!(path_in_folder("C:/Photos/2026/a.cr3", "C:\\PHOTOS\\2026\\", false));
    assert!(path_in_folder("C:/Photos/2026/a.cr3", "C:/Photos/2026///", false));
    assert!(path_in_folder("/home/m/a.jpg", "/home/m/", false));
    // Subfolders only when recursive.
    assert!(!path_in_folder("C:/Photos/2026/day1/a.cr3", "C:/Photos/2026", false));
    assert!(path_in_folder("C:/Photos/2026/day1/a.cr3", "C:/Photos/2026", true));
    assert!(path_in_folder("C:/Photos/2026/day1/day2/a.cr3", "C:/Photos/2026", true));
    // A sibling with the same prefix is not inside.
    assert!(!path_in_folder("C:/Photos/2026-old/a.cr3", "C:/Photos/2026", true));
    // The folder itself is not inside itself; unrelated paths are not either.
    assert!(!path_in_folder("C:/Photos/2026", "C:/Photos/2026", true));
    assert!(!path_in_folder("C:/Photos/2026/", "C:/Photos/2026", true));
    assert!(!path_in_folder("D:/Photos/2026/a.cr3", "C:/Photos/2026", true));
    // Roots.
    assert!(path_in_folder("/a.jpg", "/", false));
    assert!(path_in_folder("C:\\a.jpg", "C:\\", false));
    assert!(!path_in_folder("C:\\x\\a.jpg", "C:\\", false));
    assert!(path_in_folder("C:\\x\\a.jpg", "C:\\", true));
    // Empty inputs are never inside anything.
    assert!(!path_in_folder("", "C:/Photos", true));
    assert!(!path_in_folder("C:/Photos/a.jpg", "", true));
    // Unicode case folding.
    assert!(path_in_folder("C:/Bilder/\u{c4}rger/a.jpg", "c:/bilder/\u{e4}rger", false));
}

fn folder_lib() -> Lib {
    Lib::new(&[
        (1, Some("C:\\Photos\\Day1\\a.CR3"), Some(100.0)),
        (2, Some("c:/photos/day1/b.cr3"), Some(200.0)),
        (3, Some("C:/Photos/Day1/sub/c.cr3"), Some(400.0)),
        (4, Some("C:/Photos/Day2/d.cr3"), Some(800.0)),
        (5, None, Some(100.0)),
        (6, Some("D:/Other/e.cr3"), None),
    ])
}

#[test]
fn legacy_source_without_folder_contains_every_image() {
    let lib = folder_lib();
    let mut g = Graph::new();
    let none = source_in(&mut g, None, true);
    assert_eq!(lib.members(&g, none, 0), vec![1, 2, 3, 4, 5, 6]);
    for i in 1..=6 {
        assert!(lib.contains(&g, none, i));
    }
    // The old API behaves the same with an empty image list.
    let cap = |_: ImageId| -> Option<CaptureInfo> { None };
    assert!(g.contains(none, id(99), &cap));
}

#[test]
fn source_folder_recursive_and_flat() {
    let lib = folder_lib();
    let mut g = Graph::new();
    let rec = source_in(&mut g, Some("C:/Photos/Day1"), true);
    let flat = source_in(&mut g, Some("c:\\PHOTOS\\day1\\"), false);
    assert_eq!(lib.members(&g, rec, 0), vec![1, 2, 3]);
    assert_eq!(lib.members(&g, flat, 0), vec![1, 2]);
    for i in 1..=6 {
        assert_eq!(lib.contains(&g, rec, i), [1, 2, 3].contains(&i), "recursive, image {i}");
        assert_eq!(lib.contains(&g, flat, i), [1, 2].contains(&i), "flat, image {i}");
    }
    // The whole Photos folder, recursively and not.
    let all = source_in(&mut g, Some("C:/Photos"), true);
    let top = source_in(&mut g, Some("C:/Photos"), false);
    assert_eq!(lib.members(&g, all, 0), vec![1, 2, 3, 4]);
    assert_eq!(lib.members(&g, top, 0), Vec::<u64>::new());
}

#[test]
fn source_folder_with_unknown_path_or_unknown_folder() {
    let lib = folder_lib();
    let mut g = Graph::new();
    let s = source_in(&mut g, Some("C:/Photos/Day1"), true);
    assert!(!lib.contains(&g, s, 5), "image without a path is not contained");
    assert!(!lib.contains(&g, s, 6));
    assert!(!lib.contains(&g, s, 77), "image the lookup does not know");
    let nowhere = source_in(&mut g, Some("Z:/nothing"), true);
    assert!(lib.members(&g, nowhere, 0).is_empty());
    // Without a catalog the capture lookup still decides.
    let cap = |i: ImageId| folder_lib().caps.get(&i).cloned();
    assert!(g.contains(s, id(1), &cap));
    assert!(!g.contains(s, id(4), &cap));
}

#[test]
fn source_folder_options_ignore_individual_mode() {
    let lib = folder_lib();
    let mut g = Graph::new();
    let s = source_in(&mut g, Some("C:/Photos/Day1"), false);
    g.node_mut(s).unwrap().mode = Mode::Individual;
    assert_eq!(lib.members(&g, s, 0), vec![1, 2]);
    assert!(g.node(s).unwrap().data(id(5)).is_some(), "data set before the mode switch stays");
}

#[test]
fn source_folder_data_limits() {
    let mut g = Graph::new();
    let s = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let n = g.node_mut(s).unwrap();
    assert!(n.set_data(id(1), Some(NodeData::Folder("a".repeat(1024)))));
    assert!(!n.set_data(id(1), Some(NodeData::Folder("a".repeat(1025)))));
    assert!(n.set_data(id(1), Some(NodeData::Folder("\u{e4}".repeat(1024)))), "limit counts characters");
    assert!(!n.set_data(id(1), Some(NodeData::Folder("C:/a\0b".to_owned()))));
    assert!(!n.set_data(id(1), Some(NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]]))));
    assert!(n.set_data(id(1), None));
    assert!(n.data(id(1)).is_none());
    // Other kinds do not take folders.
    let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert!(!g.node_mut(e).unwrap().set_data(id(1), Some(NodeData::Folder("C:/x".into()))));
    // Param table.
    let spec = NodeKind::Source.param_spec("subfolders").unwrap();
    assert_eq!((spec.label, spec.kind, spec.default), ("Include subfolders", ParamKind::Toggle, 1.0));
}

// ---------------------------------------------------------------------------
// Merge collisions

fn merge_lib() -> Lib {
    Lib::new(&[
        (1, Some("/a/IMG_1.CR3"), Some(100.0)),
        (2, Some("/b/img_1.cr3"), Some(100.0)),
        (3, Some("/a/IMG_2.CR3"), Some(100.0)),
        (4, Some("/c/IMG_2.CR3"), Some(100.0)),
        (5, Some("/c/img_2.jpg"), Some(100.0)),
        (6, None, Some(100.0)),
        (7, Some("/a/sub/IMG_1.CR3"), Some(100.0)),
    ])
}

/// Merge with A = /a (flat), B = /b, C = /c.
fn folder_merge(g: &mut Graph) -> NodeId {
    let a = source_in(g, Some("/a"), false);
    let b = source_in(g, Some("/b"), false);
    let c = source_in(g, Some("/c"), false);
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(a, m, 0).unwrap();
    g.connect(b, m, 1).unwrap();
    g.connect(c, m, 2).unwrap();
    m
}

#[test]
fn merge_collision_option_table() {
    let spec = NodeKind::Merge.param_spec("collision").unwrap();
    assert_eq!(spec.label, "Same file name");
    assert_eq!(spec.kind, ParamKind::Choice(&["Keep both", "First input wins", "Last input wins"]));
    assert_eq!(spec.default, 0.0);
}

#[test]
fn merge_keep_both_first_wins_last_wins() {
    let lib = merge_lib();
    let mut g = Graph::new();
    let m = folder_merge(&mut g);
    // Keep both: everything, in port order.
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 2, 4, 5]);
    // First: IMG_1 of /a beats /b, IMG_2.CR3 of /a beats /c.
    set_collision(&mut g, m, 1.0);
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 5]);
    // Last: /b beats /a, /c beats /a.
    set_collision(&mut g, m, 2.0);
    assert_eq!(lib.members(&g, m, 0), vec![2, 4, 5]);
}

#[test]
fn merge_collision_contains_and_sources_agree_with_members() {
    let lib = merge_lib();
    let mut g = Graph::new();
    let m = folder_merge(&mut g);
    for (option, kept) in [(0.0, vec![1, 2, 3, 4, 5]), (1.0, vec![1, 3, 5]), (2.0, vec![2, 4, 5])] {
        set_collision(&mut g, m, option);
        for i in 1..=7 {
            assert_eq!(lib.contains(&g, m, i), kept.contains(&i), "option {option} image {i}");
            let src = lib.with(|c| g.merge_source_in(m, id(i), c));
            assert_eq!(src.is_some(), kept.contains(&i), "option {option} image {i}");
        }
    }
    set_collision(&mut g, m, 1.0);
    assert_eq!(lib.with(|c| g.merge_source_in(m, id(1), c)), Some(0));
    assert_eq!(lib.with(|c| g.merge_source_in(m, id(5), c)), Some(2));
    assert_eq!(lib.with(|c| g.merge_source_in(m, id(2), c)), None);
    assert!(lib.with(|c| g.contains_output_in(m, 0, id(5), c)));
    assert!(!lib.with(|c| g.contains_output_in(m, 1, id(5), c)));
}

#[test]
fn merge_collision_keeps_several_same_named_images_of_the_winning_input() {
    let lib = merge_lib();
    let mut g = Graph::new();
    // A = /a recursively (1, 3, 7: two files named IMG_1.CR3), B = /b (2).
    let a = source_in(&mut g, Some("/a"), true);
    let b = source_in(&mut g, Some("/b"), false);
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(a, m, 0).unwrap();
    g.connect(b, m, 1).unwrap();
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 7, 2]);
    set_collision(&mut g, m, 1.0);
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 7], "both IMG_1 of input A stay");
    set_collision(&mut g, m, 2.0);
    assert_eq!(lib.members(&g, m, 0), vec![3, 2], "IMG_2 stays from A, IMG_1 of B beats both of A");
}

#[test]
fn merge_collision_images_without_a_path_never_collide() {
    let lib = Lib::new(&[(1, None, None), (2, None, None), (3, Some("/x/A.jpg"), None), (4, Some("/y/a.JPG"), None)]);
    let mut g = Graph::new();
    let all = source_in(&mut g, None, true);
    let flt = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
    g.connect(all, flt, 0).unwrap();
    g.node_mut(flt).unwrap().set_param("missing", id(0), 1.0);
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(all, m, 0).unwrap();
    g.connect(flt, m, 1).unwrap();
    for option in [0.0, 1.0, 2.0] {
        set_collision(&mut g, m, option);
        assert_eq!(lib.members(&g, m, 0), vec![1, 2, 3, 4], "option {option}");
    }
    // Same name in two different inputs, image 2 has no path: only 3/4 collide.
    let s1 = source_in(&mut g, Some("/x"), false);
    let s2 = source_in(&mut g, Some("/y"), false);
    let m2 = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(s1, m2, 0).unwrap();
    g.connect(s2, m2, 1).unwrap();
    set_collision(&mut g, m2, 1.0);
    assert_eq!(lib.members(&g, m2, 0), vec![3]);
    set_collision(&mut g, m2, 2.0);
    assert_eq!(lib.members(&g, m2, 0), vec![4]);
}

#[test]
fn merge_identity_duplicates_keep_the_first_input() {
    let lib = merge_lib();
    let mut g = Graph::new();
    let a = source_in(&mut g, Some("/a"), true); // 1, 3, 7
    let all = source_in(&mut g, None, true); // 1..=7
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(a, m, 0).unwrap();
    g.connect(all, m, 1).unwrap();
    // Keep both: every image once, the identity duplicates come from input A.
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 7, 2, 4, 5, 6]);
    assert_eq!(lib.with(|c| g.merge_source_in(m, id(1), c)), Some(0));
    assert_eq!(lib.with(|c| g.merge_source_in(m, id(2), c)), Some(1));
    // First input wins: IMG_1 (1, 7) and IMG_2.CR3 (3) of A beat 2 and 4 of B.
    set_collision(&mut g, m, 1.0);
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 7, 5, 6]);
    assert_eq!(lib.with(|c| g.merge_source_in(m, id(1), c)), Some(0), "identity duplicate: the first input");
    // Last input wins: all of B's names beat A's, the duplicates are not resurrected.
    set_collision(&mut g, m, 2.0);
    assert_eq!(lib.members(&g, m, 0), vec![2, 4, 5, 6]);
    // Without any same-named rival an identity duplicate stays and comes from the first input.
    let d1 = source_in(&mut g, Some("/b"), false);
    let d2 = source_in(&mut g, Some("/b"), false);
    let m2 = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(d1, m2, 0).unwrap();
    g.connect(d2, m2, 1).unwrap();
    for option in [0.0, 1.0, 2.0] {
        set_collision(&mut g, m2, option);
        assert_eq!(lib.members(&g, m2, 0), vec![2], "option {option}");
        assert_eq!(lib.with(|c| g.merge_source_in(m2, id(2), c)), Some(0));
    }
}

#[test]
fn merge_with_an_empty_catalog_keeps_both() {
    let lib = merge_lib();
    let mut g = Graph::new();
    let m = folder_merge(&mut g);
    set_collision(&mut g, m, 1.0);
    let cap = |i: ImageId| lib.caps.get(&i).cloned();
    let empty = Catalog { images: &[], capture: &cap, memo: None };
    for i in [1, 2, 3, 4, 5] {
        assert!(g.contains_in(m, id(i), &empty), "image {i}");
        assert!(g.contains(m, id(i), &cap));
    }
    assert_eq!(g.merge_source(m, id(2), &cap), Some(1));
    assert_eq!(g.merge_candidates(m, id(1), &cap), 1);
    assert!(g.members(m, 0, &empty).is_empty(), "nothing to list without a catalog");
}

// ---------------------------------------------------------------------------
// members: order for every node type

fn iso_lib() -> Lib {
    Lib::new(&[
        (1, Some("/p/1.jpg"), Some(800.0)),
        (2, Some("/p/2.jpg"), Some(100.0)),
        (3, Some("/p/3.jpg"), Some(400.0)),
        (4, Some("/p/4.jpg"), None),
        (5, Some("/p/5.jpg"), Some(200.0)),
        (6, Some("/q/6.jpg"), Some(100.0)),
    ])
}

fn filter_between(g: &mut Graph, input: NodeId, min: f32, max: f32) -> NodeId {
    let f = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
    g.connect(input, f, 0).unwrap();
    let n = g.node_mut(f).unwrap();
    n.set_param("min", id(0), min);
    n.set_param("max", id(0), max);
    f
}

#[test]
fn members_order_for_every_node_type() {
    let lib = iso_lib();
    let mut g = Graph::new();
    let src = source_in(&mut g, None, true);
    assert_eq!(lib.members(&g, src, 0), vec![1, 2, 3, 4, 5, 6], "source: catalog order");

    // Pass-through nodes keep the order.
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    g.connect(src, exp, 0).unwrap();
    let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
    g.connect(exp, out, 0).unwrap();
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
    g.connect(exp, export, 0).unwrap();
    for n in [exp, out, export] {
        assert_eq!(lib.members(&g, n, 0), vec![1, 2, 3, 4, 5, 6]);
    }

    // Filter keeps the input order, not the numeric order of the values.
    let f = filter_between(&mut g, exp, 100.0, 800.0);
    assert_eq!(lib.members(&g, f, 0), vec![1, 2, 3, 5, 6]);
    g.node_mut(f).unwrap().set_param("invert", id(0), 1.0);
    assert_eq!(lib.members(&g, f, 0), Vec::<u64>::new(), "all present values are inside; missing is excluded");
    g.node_mut(f).unwrap().set_param("invert", id(0), 0.0);

    // Split: each port keeps the order.
    let sp = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
    g.connect(f, sp, 0).unwrap();
    let ranges = vec![
        NamedRange { name: "low".into(), min: 0.0, max: 200.0 },
        NamedRange { name: "mid".into(), min: 201.0, max: 500.0 },
    ];
    assert!(g.node_mut(sp).unwrap().set_data(id(0), Some(NodeData::Ranges(ranges))));
    assert_eq!(lib.members(&g, sp, 0), vec![2, 5, 6]);
    assert_eq!(lib.members(&g, sp, 1), vec![3]);
    assert_eq!(lib.members(&g, sp, 2), Vec::<u64>::new());
    assert_eq!(lib.members(&g, sp, SPLIT_UNMATCHED_PORT), vec![1]);
    assert_eq!(lib.members(&g, sp, 7), Vec::<u64>::new(), "no such port");

    // Merge: port order, then input order; ids already listed are skipped.
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect_ports(sp, SPLIT_UNMATCHED_PORT, m, 0).unwrap();
    g.connect_ports(sp, 1, m, 1).unwrap();
    g.connect_ports(sp, 0, m, 3).unwrap();
    g.connect(f, m, 2).unwrap();
    assert_eq!(lib.members(&g, m, 0), vec![1, 3, 2, 5, 6]);

    // Layer: the driver's order (here input 2, it has the most images).
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    g.connect(src, layer, 1).unwrap();
    g.connect(m, layer, 0).unwrap();
    assert_eq!(lib.members(&g, layer, 0), vec![1, 2, 3, 4, 5, 6]);

    // Standalone generated images and unknown nodes.
    let mask = g.add_node(NodeKind::Generator, [0.0, 0.0]);
    assert_eq!(lib.members(&g, mask, 0), vec![u64::MAX - mask.0]);
    assert!(lib.members(&g, NodeId(9999), 0).is_empty());
    assert!(lib.members(&g, exp, 3).is_empty(), "single-output nodes only have port 0");
}

#[test]
fn members_of_unconnected_nodes_are_empty() {
    let lib = iso_lib();
    let mut g = Graph::new();
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
    for n in [exp, layer, merge, split] {
        assert!(lib.members(&g, n, 0).is_empty());
        assert!(!lib.contains(&g, n, 1));
    }
}

/// `contains_in`, `contains_output_in` and `members` must tell the same story.
#[test]
fn contains_and_members_agree_across_a_busy_graph() {
    let lib = iso_lib();
    let mut g = Graph::new();
    let src = source_in(&mut g, Some("/p"), true);
    let all = source_in(&mut g, None, true);
    let f = filter_between(&mut g, src, 150.0, 900.0);
    let sp = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
    g.connect(f, sp, 0).unwrap();
    g.node_mut(sp)
        .unwrap()
        .set_data(id(0), Some(NodeData::Ranges(vec![NamedRange { name: "x".into(), min: 0.0, max: 300.0 }])));
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect_ports(sp, 0, m, 0).unwrap();
    g.connect(all, m, 1).unwrap();
    set_collision(&mut g, m, 1.0);
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    g.connect(sp, layer, 0).unwrap();
    g.connect(m, layer, 2).unwrap();
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    g.connect(layer, exp, 0).unwrap();
    for n in g.nodes().map(|n| n.id).collect::<Vec<_>>() {
        for port in 0..8 {
            let listed = lib.members(&g, n, port);
            for i in 1..=6 {
                assert_eq!(
                    lib.with(|c| g.contains_output_in(n, port, id(i), c)),
                    listed.contains(&i),
                    "node {n:?} port {port} image {i}"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Layer

fn layer_lib(a: u64, b: u64) -> Lib {
    let mut entries: Vec<(u64, String)> = (1..=a).map(|i| (i, format!("/x/{i}.jpg"))).collect();
    entries.extend((a + 1..=a + b).map(|i| (i, format!("/y/{i}.jpg"))));
    let borrowed: Vec<(u64, Option<&str>, Option<f32>)> =
        entries.iter().map(|(i, p)| (*i, Some(p.as_str()), None)).collect();
    Lib::new(&borrowed)
}

fn layer_graph(folders: &[(usize, &str)]) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    for (port, folder) in folders {
        let s = source_in(&mut g, Some(folder), false);
        g.connect(s, layer, *port).unwrap();
    }
    (g, layer)
}

fn sources_of(lib: &Lib, g: &Graph, layer: NodeId, image: u64) -> Option<Vec<Option<u64>>> {
    lib.with(|c| g.layer_sources(layer, id(image), c)).map(|a| a.iter().map(|o| o.map(|i| i.0)).collect())
}

#[test]
fn layer_node_shape() {
    assert_eq!(MAX_LAYERS, 8);
    let k = NodeKind::Layer;
    assert_eq!((k.label(), k.category()), ("Layer", "Collection"));
    assert_eq!(k.inputs().len(), 8);
    for (i, input) in k.inputs().iter().enumerate() {
        assert_eq!((input.label.to_owned(), input.ty, input.required), (format!("Layer {}", i + 1), PortType::Images, false));
    }
    assert_eq!(k.outputs().len(), 1);
    assert_eq!(k.params().len(), 1 + 5 * 8);
    let mode = k.param_spec("mode").unwrap();
    assert_eq!(mode.kind, ParamKind::Choice(&["Match", "All combinations"]));
    for i in 1..=8 {
        let on = k.param_spec(&format!("l{i}_on")).unwrap();
        assert_eq!((on.kind, on.default), (ParamKind::Toggle, 1.0));
        let blend = k.param_spec(&format!("l{i}_blend")).unwrap();
        assert_eq!(
            blend.kind,
            ParamKind::Choice(&["Normal", "Add", "Subtract", "Multiply", "Screen", "Overlay", "Darken", "Lighten", "Difference"])
        );
        let op = k.param_spec(&format!("l{i}_opacity")).unwrap();
        assert_eq!((op.kind, op.min, op.max, op.default), (ParamKind::Continuous, 0.0, 1.0, 1.0));
        assert_eq!(k.param_spec(&format!("l{i}_fit")).unwrap().kind, ParamKind::Choice(&["Fit", "Fill", "Crop", "Stretch"]));
        assert_eq!(
            k.param_spec(&format!("l{i}_filter")).unwrap().kind,
            ParamKind::Choice(&["Linear", "Bicubic", "Nearest"])
        );
    }
    assert!(k.param_spec("l9_on").is_none());
    assert!(k.data_kinds().is_empty());
    // A layer's inputs take image collections only.
    let mut g = Graph::new();
    let mask = g.add_node(NodeKind::Generator, [0.0, 0.0]);
    let layer = g.add_node(k, [0.0, 0.0]);
    assert!(g.connect(mask, layer, 0).is_ok());
    assert!(matches!(g.connect(mask, layer, 8), Err(GraphError::NoSuchPort { .. })));
}

#[test]
fn layer_match_pairs_ten_with_five() {
    let lib = layer_lib(10, 5);
    let (g, layer) = layer_graph(&[(0, "/x"), (1, "/y")]);
    assert_eq!(lib.members(&g, layer, 0), (1..=10).collect::<Vec<_>>());
    for k in 1..=10u64 {
        let s = sources_of(&lib, &g, layer, k).expect("in the output");
        assert_eq!(s[0], Some(k));
        // Images 1-5 pair with 11-15, 6-10 pair with 11-15 again.
        assert_eq!(s[1], Some(10 + (k - 1) % 5 + 1), "image {k}");
        assert!(s[2..].iter().all(|o| o.is_none()));
    }
    assert_eq!(sources_of(&lib, &g, layer, 6).unwrap()[1], Some(11));
    assert_eq!(sources_of(&lib, &g, layer, 10).unwrap()[1], Some(15));
    // Not in the output: images of the shorter input and unknown ones.
    assert_eq!(sources_of(&lib, &g, layer, 12), None);
    assert_eq!(sources_of(&lib, &g, layer, 99), None);
    // Contains follows the output.
    assert!(lib.contains(&g, layer, 3));
    assert!(!lib.contains(&g, layer, 12));
}

#[test]
fn layer_driver_is_the_largest_input_and_ties_go_to_the_lowest_layer() {
    let lib = layer_lib(5, 10);
    // The bigger folder sits on layer 1: driver is layer 2 (images 6..=15).
    let (g, layer) = layer_graph(&[(0, "/x"), (1, "/y")]);
    assert_eq!(lib.members(&g, layer, 0), (6..=15).collect::<Vec<_>>());
    let s = sources_of(&lib, &g, layer, 8).unwrap();
    assert_eq!((s[0], s[1]), (Some(3), Some(8)), "layer 1 shows its 3rd image, layer 2 is the driver");
    // The driver may sit on a high layer too.
    let (g, layer) = layer_graph(&[(1, "/x"), (6, "/y")]);
    assert_eq!(lib.members(&g, layer, 0), (6..=15).collect::<Vec<_>>());

    // A tie: layer 2 and layer 4 have five images each, layer 2 drives.
    let lib = layer_lib(5, 5);
    let (g, layer) = layer_graph(&[(3, "/y"), (1, "/x")]);
    assert_eq!(lib.members(&g, layer, 0), (1..=5).collect::<Vec<_>>());
    let s = sources_of(&lib, &g, layer, 2).unwrap();
    assert_eq!((s[1], s[3]), (Some(2), Some(7)));
    let (g, layer) = layer_graph(&[(0, "/y"), (7, "/x")]);
    assert_eq!(lib.members(&g, layer, 0), (6..=10).collect::<Vec<_>>());
}

#[test]
fn layer_with_three_layers_an_empty_input_and_a_gap() {
    let lib = layer_lib(6, 2);
    // Layers 1 (6 images), 3 (2 images) and 4 (nothing); layer 2 is not connected.
    let (g, layer) = layer_graph(&[(0, "/x"), (2, "/y"), (3, "/nowhere")]);
    assert_eq!(lib.members(&g, layer, 0), (1..=6).collect::<Vec<_>>());
    let s = sources_of(&lib, &g, layer, 5).unwrap();
    assert_eq!(s[0], Some(5));
    assert_eq!(s[1], None, "unconnected layer");
    assert_eq!(s[2], Some(7), "layer 3 wraps: position 5 -> 1st of 2");
    assert_eq!(s[3], None, "empty input");
    assert!(s[4..].iter().all(|o| o.is_none()));
    // Three real layers.
    let lib = layer_lib(4, 8);
    let mut g = Graph::new();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    for (port, folder) in [(0, "/x"), (1, "/y"), (2, "/x")] {
        let s = source_in(&mut g, Some(folder), false);
        g.connect(s, layer, port).unwrap();
    }
    assert_eq!(lib.members(&g, layer, 0), (5..=12).collect::<Vec<_>>());
    let s = sources_of(&lib, &g, layer, 11).unwrap();
    assert_eq!((s[0], s[1], s[2]), (Some(3), Some(11), Some(3)));
}

#[test]
fn layer_everything_empty_and_no_inputs() {
    let lib = layer_lib(3, 0);
    let (g, layer) = layer_graph(&[(0, "/nowhere"), (1, "/elsewhere")]);
    assert!(lib.members(&g, layer, 0).is_empty());
    assert_eq!(sources_of(&lib, &g, layer, 1), None);
    let (g, layer) = layer_graph(&[]);
    assert!(lib.members(&g, layer, 0).is_empty());
    assert_eq!(sources_of(&lib, &g, layer, 1), None);
    // layer_sources of other nodes.
    let (g2, _) = layer_graph(&[(0, "/x")]);
    let src = g2.nodes().find(|n| n.kind == NodeKind::Source).unwrap().id;
    assert_eq!(sources_of(&lib, &g2, src, 1), None);
    assert_eq!(sources_of(&lib, &g2, NodeId(500), 1), None);
}

#[test]
fn layer_with_an_empty_catalog() {
    let lib = layer_lib(4, 2);
    let (g, layer) = layer_graph(&[(0, "/x"), (3, "/y")]);
    let cap = |i: ImageId| lib.caps.get(&i).cloned();
    let empty = Catalog { images: &[], capture: &cap, memo: None };
    // Contained if any connected input contains it.
    for i in 1..=6 {
        assert!(g.contains_in(layer, id(i), &empty), "image {i}");
        assert!(g.contains(layer, id(i), &cap));
    }
    assert!(!g.contains_in(layer, id(7), &empty), "unknown to the lookup: no path, in no folder");
    assert_eq!(g.layer_sources(layer, id(1), &empty), None);
    assert!(g.members(layer, 0, &empty).is_empty());
}

#[test]
fn layer_settings_read_the_node() {
    let mut g = Graph::new();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    let n = g.node_mut(layer).unwrap();
    assert_eq!(layer_settings(n, 0, id(1)), LayerSettings::default());
    let d = LayerSettings::default();
    assert_eq!(
        (d.on, d.blend, d.opacity, d.fit, d.filter),
        (true, BlendMode::Normal, 1.0, FitMode::Fit, ResampleFilter::Linear)
    );
    n.set_param("l3_on", id(1), 0.0);
    n.set_param("l3_blend", id(1), 3.0);
    n.set_param("l3_opacity", id(1), 0.25);
    n.set_param("l3_fit", id(1), 2.0);
    n.set_param("l3_filter", id(1), 1.0);
    n.set_param("l8_blend", id(1), 8.0);
    n.set_param("l8_filter", id(1), 2.0);
    let s = layer_settings(n, 2, id(1));
    assert_eq!(
        s,
        LayerSettings {
            on: false,
            blend: BlendMode::Multiply,
            opacity: 0.25,
            fit: FitMode::Crop,
            filter: ResampleFilter::Bicubic
        }
    );
    assert_eq!(layer_settings(n, 0, id(1)), LayerSettings::default(), "other layers keep their defaults");
    let s8 = layer_settings(n, 7, id(1));
    assert_eq!((s8.blend, s8.filter), (BlendMode::Difference, ResampleFilter::Nearest));
    assert_eq!(layer_settings(n, 8, id(1)), LayerSettings::default(), "no layer 9");
    assert_eq!(layer_settings(n, 999, id(1)), LayerSettings::default());

    // Individual values belong to the output image.
    n.mode = Mode::Individual;
    n.set_param("l1_opacity", id(5), 0.5);
    assert_eq!(layer_settings(n, 0, id(5)).opacity, 0.5);
    assert_eq!(layer_settings(n, 0, id(6)).opacity, 1.0);
    assert_eq!(layer_settings(n, 2, id(6)).blend, BlendMode::Multiply, "inherits the All value");

    // Other node kinds.
    let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert_eq!(layer_settings(g.node(e).unwrap(), 0, id(1)), LayerSettings::default());
}

#[test]
fn layer_settings_read_non_finite_opacity_as_one() {
    let mut g = Graph::new();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    let n = g.node_mut(layer).unwrap();
    n.set_raw("l1_opacity", f32::NAN);
    assert_eq!(layer_settings(n, 0, id(1)).opacity, 1.0);
    n.set_raw("l1_opacity", f32::INFINITY);
    assert_eq!(layer_settings(n, 0, id(1)).opacity, 1.0);
    n.set_raw("l1_opacity", 7.0);
    assert_eq!(layer_settings(n, 0, id(1)).opacity, 1.0, "clamped");
    n.set_raw("l1_opacity", -1.0);
    assert_eq!(layer_settings(n, 0, id(1)).opacity, 0.0, "clamped");
}

#[test]
fn layer_enums_from_index_and_labels() {
    for (i, mode) in BlendMode::ALL.iter().enumerate() {
        assert_eq!(BlendMode::from_index(i), *mode);
        assert_eq!(mode.label(), LAYER_BLEND_MODES[i]);
    }
    for (i, fit) in FitMode::ALL.iter().enumerate() {
        assert_eq!(FitMode::from_index(i), *fit);
        assert_eq!(fit.label(), LAYER_FITS[i]);
    }
    for (i, f) in ResampleFilter::ALL.iter().enumerate() {
        assert_eq!(ResampleFilter::from_index(i), *f);
        assert_eq!(f.label(), LAYER_FILTERS[i]);
    }
    assert_eq!(BlendMode::from_index(99), BlendMode::Normal);
    assert_eq!(FitMode::from_index(99), FitMode::Fit);
    assert_eq!(ResampleFilter::from_index(99), ResampleFilter::Linear);
    assert_eq!(BlendMode::from_index(8), BlendMode::Difference);
}

#[test]
fn layer_all_combinations_behaves_like_match_and_is_reported() {
    let lib = layer_lib(4, 2);
    let (mut g, layer) = layer_graph(&[(0, "/x"), (1, "/y")]);
    let before = lib.members(&g, layer, 0);
    let sources = sources_of(&lib, &g, layer, 3);
    assert!(g.validate().is_empty());
    g.node_mut(layer).unwrap().set_param("mode", id(0), 1.0);
    assert_eq!(lib.members(&g, layer, 0), before);
    assert_eq!(sources_of(&lib, &g, layer, 3), sources);
    let issues = g.validate();
    assert_eq!(issues.len(), 1);
    assert_eq!((issues[0].node, issues[0].message.as_str()), (layer, "All combinations is not available yet; Match is used"));
}

#[test]
fn capture_ev_delta_follows_layers_and_exports() {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let low = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
    let high = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
    g.connect(src, low, 0).unwrap();
    g.connect(src, high, 0).unwrap();
    g.node_mut(low).unwrap().set_param("delta", id(1), 1.5);
    g.node_mut(high).unwrap().set_param("delta", id(1), -2.0);
    assert_eq!(g.capture_ev_delta(layer, id(1)), 0.0, "nothing connected");
    g.connect(high, layer, 3).unwrap();
    assert_eq!(g.capture_ev_delta(layer, id(1)), -2.0);
    g.connect(low, layer, 1).unwrap();
    assert_eq!(g.capture_ev_delta(layer, id(1)), 1.5, "the lowest connected layer counts");
    g.connect(layer, export, 0).unwrap();
    assert_eq!(g.capture_ev_delta(export, id(1)), 1.5);
    assert_eq!(g.capture_ev_delta(NodeId(77), id(1)), 0.0);
}

// ---------------------------------------------------------------------------
// Develop tone

#[test]
fn develop_tone_table_and_defaults() {
    let d = NodeKind::Develop;
    let names: Vec<&str> = d.params().iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "exposure",
            "contrast",
            "highlights",
            "shadows",
            "whites",
            "blacks",
            "wb",
            "temperature",
            "tint",
            "vibrance",
            "saturation",
            "baseline",
            "baseline_default",
            "brightness",
            "legacy_tone"
        ]
    );
    let labels: Vec<&str> = d.params().iter().take(11).map(|s| s.label).collect();
    assert_eq!(
        labels,
        ["Exposure", "Contrast", "Lights", "Shadows", "White", "Black", "White balance", "Temperature", "Tint", "Dynamic", "Saturation"]
    );
    let ex = d.param_spec("exposure").unwrap();
    assert_eq!((ex.min, ex.max, ex.default, ex.unit), (-5.0, 5.0, 0.0, "EV"));
    for name in ["contrast", "highlights", "shadows", "whites", "blacks", "vibrance", "saturation"] {
        let s = d.param_spec(name).unwrap();
        assert_eq!((s.min, s.max, s.default, s.kind), (-1.0, 1.0, 0.0, ParamKind::Continuous), "{name}");
    }
    let mut g = Graph::new();
    let n = g.add_node(d, [0.0, 0.0]);
    let node = g.node(n).unwrap();
    let t = develop_tone(node, id(1));
    assert!(t.is_identity());
    assert_eq!(t, DevelopTone::default());
}

#[test]
fn develop_tone_reads_every_value_and_follows_individual_mode() {
    let mut g = Graph::new();
    let n = g.add_node(NodeKind::Develop, [0.0, 0.0]);
    let node = g.node_mut(n).unwrap();
    for (name, v) in [
        ("exposure", 1.5),
        ("contrast", 0.1),
        ("highlights", -0.2),
        ("shadows", 0.3),
        ("whites", 0.4),
        ("blacks", -0.5),
        ("vibrance", 0.6),
        ("saturation", -0.7),
    ] {
        assert!(node.set_param(name, id(1), v));
    }
    let t = develop_tone(node, id(1));
    assert_eq!(
        t,
        DevelopTone {
            exposure: 1.5,
            contrast: 0.1,
            highlights: -0.2,
            shadows: 0.3,
            whites: 0.4,
            blacks: -0.5,
            vibrance: 0.6,
            saturation: -0.7
        }
    );
    assert!(!t.is_identity());
    // Each value alone breaks the identity.
    for name in ["exposure", "contrast", "highlights", "shadows", "whites", "blacks", "vibrance", "saturation"] {
        let mut g = Graph::new();
        let n = g.add_node(NodeKind::Develop, [0.0, 0.0]);
        g.node_mut(n).unwrap().set_param(name, id(1), 0.25);
        assert!(!develop_tone(g.node(n).unwrap(), id(1)).is_identity(), "{name}");
    }
    node.mode = Mode::Individual;
    node.set_param("exposure", id(2), -1.0);
    assert_eq!(develop_tone(node, id(2)).exposure, -1.0);
    assert_eq!(develop_tone(node, id(3)).exposure, 1.5);
    // The base part is untouched by the tone values.
    let s = develop_settings(node, id(3), None);
    assert_eq!(s.gain_ev, 0.0);
    // Other kinds.
    let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert!(develop_tone(g.node(e).unwrap(), id(1)).is_identity());
}

#[test]
fn develop_tone_reads_non_finite_values_as_zero() {
    let mut g = Graph::new();
    let n = g.add_node(NodeKind::Develop, [0.0, 0.0]);
    let node = g.node_mut(n).unwrap();
    node.set_raw("exposure", f32::NAN);
    node.set_raw("whites", f32::INFINITY);
    node.set_raw("saturation", f32::NEG_INFINITY);
    node.set_raw("contrast", 0.5);
    let t = develop_tone(node, id(1));
    assert_eq!((t.exposure, t.whites, t.saturation, t.contrast), (0.0, 0.0, 0.0, 0.5));
    assert!(!t.is_identity());
}

// ---------------------------------------------------------------------------
// Curves

fn curves_node(g: &mut Graph) -> NodeId {
    g.add_node(NodeKind::PointCurve, [0.0, 0.0])
}

fn bend(a: f32) -> Vec<[f32; 2]> {
    vec![[0.0, 0.0], [0.5, a], [1.0, 1.0]]
}

#[test]
fn curve_channels_and_neutral_curves() {
    assert_eq!(CURVE_CHANNELS, ["Luminance", "Red", "Green", "Blue", "Sat vs Sat", "Hue vs Hue", "Hue vs Sat"]);
    for c in 0..=3u8 {
        assert_eq!(neutral_curve(c), vec![[0.0, 0.0], [1.0, 1.0]]);
    }
    for c in 4..=6u8 {
        assert_eq!(neutral_curve(c), vec![[0.0, 0.5], [1.0, 0.5]]);
    }
    let mut g = Graph::new();
    let n = curves_node(&mut g);
    assert_eq!(g.node(n).unwrap().kind.label(), "Curves");
    assert_eq!(g.node(n).unwrap().kind.data_kinds(), &[DataKind::Curve, DataKind::CurveSet]);
    let ch = NodeKind::PointCurve.param_spec("channel").unwrap();
    assert_eq!(ch.max, 6.0);
}

#[test]
fn curve_set_from_legacy_curve_uses_the_channel_param() {
    let mut g = Graph::new();
    let n = curves_node(&mut g);
    let node = g.node_mut(n).unwrap();
    assert!(curve_set(node, id(1)).is_empty(), "no data");
    assert!(node.set_data(id(1), Some(NodeData::Curve(bend(0.7)))));
    assert_eq!(curve_set(node, id(1)), vec![CurveEntry { channel: 0, points: bend(0.7) }]);
    node.set_param("channel", id(1), 2.0);
    assert_eq!(curve_set(node, id(1)), vec![CurveEntry { channel: 2, points: bend(0.7) }]);
    node.set_param("channel", id(1), 5.0);
    assert_eq!(curve_set(node, id(1))[0].channel, 5);
    // A neutral legacy curve is dropped; the neutral of the channel counts.
    assert!(node.set_data(id(1), Some(default_curve())));
    node.set_param("channel", id(1), 1.0);
    assert!(curve_set(node, id(1)).is_empty());
    node.set_param("channel", id(1), 4.0);
    assert_eq!(curve_set(node, id(1)).len(), 1, "[[0,0],[1,1]] is not neutral for Sat vs Sat");
    assert!(node.set_data(id(1), Some(NodeData::Curve(neutral_curve(4)))));
    assert!(curve_set(node, id(1)).is_empty());
    // Other node kinds.
    let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert!(curve_set(g.node(e).unwrap(), id(1)).is_empty());
}

#[test]
fn curve_set_orders_and_drops_neutral_entries() {
    let mut g = Graph::new();
    let n = curves_node(&mut g);
    let node = g.node_mut(n).unwrap();
    let hue = |a: f32| vec![[0.0, a], [1.0, a]];
    let entries: Vec<CurveEntry> = [4u8, 6, 5, 3, 2, 1, 0]
        .into_iter()
        .map(|channel| CurveEntry { channel, points: if channel >= 4 { hue(0.6) } else { bend(0.4) } })
        .collect();
    assert!(node.set_data(id(1), Some(NodeData::CurveSet(entries))));
    let order: Vec<u8> = curve_set(node, id(1)).iter().map(|e| e.channel).collect();
    assert_eq!(order, vec![0, 1, 2, 3, 5, 6, 4]);

    // Neutral entries vanish, the rest keeps the application order.
    let entries = vec![
        CurveEntry { channel: 4, points: neutral_curve(4) },
        CurveEntry { channel: 3, points: bend(0.3) },
        CurveEntry { channel: 0, points: neutral_curve(0) },
        CurveEntry { channel: 5, points: hue(0.55) },
        CurveEntry { channel: 6, points: neutral_curve(6) },
        CurveEntry { channel: 1, points: neutral_curve(1) },
    ];
    assert!(node.set_data(id(1), Some(NodeData::CurveSet(entries))));
    let kept = curve_set(node, id(1));
    assert_eq!(kept.iter().map(|e| e.channel).collect::<Vec<_>>(), vec![3, 5]);
    assert_eq!(kept[0].points, bend(0.3));
    // An empty set and an all-neutral set are the identity.
    assert!(node.set_data(id(1), Some(NodeData::CurveSet(vec![]))));
    assert!(curve_set(node, id(1)).is_empty());
    // The channel parameter does not matter for a set.
    node.set_param("channel", id(1), 6.0);
    assert!(node.set_data(id(1), Some(NodeData::CurveSet(vec![CurveEntry { channel: 1, points: bend(0.2) }]))));
    assert_eq!(curve_set(node, id(1))[0].channel, 1);
}

#[test]
fn curve_set_follows_individual_mode() {
    let mut g = Graph::new();
    let n = curves_node(&mut g);
    let node = g.node_mut(n).unwrap();
    node.set_data(id(0), Some(NodeData::CurveSet(vec![CurveEntry { channel: 0, points: bend(0.4) }])));
    node.mode = Mode::Individual;
    node.set_data(id(2), Some(NodeData::CurveSet(vec![CurveEntry { channel: 2, points: bend(0.6) }])));
    assert_eq!(curve_set(node, id(1))[0].channel, 0);
    assert_eq!(curve_set(node, id(2))[0].channel, 2);
}

#[test]
fn curve_set_validation() {
    let mut g = Graph::new();
    let n = curves_node(&mut g);
    let node = g.node_mut(n).unwrap();
    let set = |entries: Vec<CurveEntry>| Some(NodeData::CurveSet(entries));
    let entry = |channel: u8, points: Vec<[f32; 2]>| CurveEntry { channel, points };
    assert!(node.set_data(id(1), set(vec![])));
    assert!(node.set_data(id(1), set((0..7).map(|c| entry(c, neutral_curve(c))).collect())));
    // Channel out of range, duplicates, too many entries.
    assert!(!node.set_data(id(1), set(vec![entry(7, bend(0.5))])));
    assert!(!node.set_data(id(1), set(vec![entry(255, bend(0.5))])));
    assert!(!node.set_data(id(1), set(vec![entry(1, bend(0.5)), entry(1, bend(0.4))])));
    let mut eight: Vec<CurveEntry> = (0..7).map(|c| entry(c, bend(0.5))).collect();
    eight.push(entry(0, bend(0.4)));
    assert!(!node.set_data(id(1), set(eight)));
    // Point limits apply per entry.
    assert!(!node.set_data(id(1), set(vec![entry(0, vec![[0.5, 0.5]])])));
    let many = |k: usize| (0..k).map(|i| [i as f32 / 40.0, 0.5]).collect::<Vec<_>>();
    assert!(node.set_data(id(1), set(vec![entry(5, many(32))])));
    assert!(!node.set_data(id(1), set(vec![entry(5, many(33))])));
    assert!(!node.set_data(id(1), set(vec![entry(0, vec![[0.0, 0.0], [0.5, 0.5], [0.5, 0.7]])])));
    assert!(!node.set_data(id(1), set(vec![entry(0, vec![[0.0, 0.0], [1.0, 1.5]])])));
    assert!(!node.set_data(id(1), set(vec![entry(0, vec![[0.0, 0.0], [f32::NAN, 1.0]])])));
    // Other kinds do not take sets.
    let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert!(!g.node_mut(e).unwrap().set_data(id(1), set(vec![])));
    // The loader enforces the same rules.
    let mut g = Graph::new();
    let n = curves_node(&mut g);
    g.node_mut(n).unwrap().set_data(id(1), set(vec![entry(1, bend(0.5))]));
    let good = serde_json::from_str::<serde_json::Value>(&g.to_json()).unwrap();
    assert!(Graph::from_json(&good.to_string()).is_ok());
    let bad = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut v = good.clone();
        edit(&mut v);
        Graph::from_json(&v.to_string())
    };
    let msg = |r: Result<Graph, GraphError>| match r {
        Err(GraphError::Parse(m)) => m,
        other => panic!("expected a parse error, got {other:?}"),
    };
    let set_json = |v: &mut serde_json::Value, entries: serde_json::Value| {
        v["nodes"]["1"]["data"] = serde_json::json!({ "CurveSet": entries });
    };
    assert!(msg(bad(&|v| set_json(v, serde_json::json!([{"channel": 9, "points": [[0.0, 0.0], [1.0, 1.0]]}]))))
        .contains("does not exist"));
    assert!(msg(bad(&|v| set_json(
        v,
        serde_json::json!([{"channel": 1, "points": [[0.0, 0.0], [1.0, 1.0]]}, {"channel": 1, "points": [[0.0, 0.0], [1.0, 1.0]]}])
    )))
    .contains("used twice"));
    assert!(msg(bad(&|v| set_json(v, serde_json::json!([{"channel": 1, "points": [[0.0, 0.0]]}])))).contains("curve has 1 points"));
    assert!(!msg(bad(&|v| set_json(v, serde_json::json!([{"channel": 1, "points": [[0.0, 0.0], [1e39, 1.0]]}])))).is_empty());
}

// ---------------------------------------------------------------------------
// Lens Correction

#[test]
fn lens_node_shape() {
    let k = NodeKind::LensManual;
    assert_eq!((k.label(), k.category()), ("Lens Correction", "Geometry"));
    let first = &k.params()[0];
    assert_eq!((first.name, first.kind, first.default), ("mode", ParamKind::Choice(&["Manual", "Auto"]), 0.0));
    assert_eq!(LENS_MODES, ["Manual", "Auto"]);
    assert_eq!(k.params().len(), 5);
}

fn lens_chain(through: &[NodeKind]) -> (Graph, NodeId) {
    let mut g = Graph::new();
    let mut cur = g.add_node(NodeKind::Source, [0.0, 0.0]);
    for kind in through {
        let n = g.add_node(*kind, [0.0, 0.0]);
        g.connect(cur, n, 0).unwrap();
        cur = n;
    }
    let lens = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
    g.connect(cur, lens, 0).unwrap();
    (g, lens)
}

#[test]
fn lens_auto_usable_needs_an_uncropped_path_to_a_source() {
    // Straight from the source and through frame-keeping nodes.
    for chain in [
        vec![],
        vec![NodeKind::Develop],
        vec![NodeKind::Exposure, NodeKind::Saturation, NodeKind::Blur],
        vec![NodeKind::MetadataFilter, NodeKind::MetadataSplit, NodeKind::Merge],
        vec![NodeKind::Vignette, NodeKind::Sharpen, NodeKind::Clarity, NodeKind::PointCurve],
    ] {
        let (g, lens) = lens_chain(&chain);
        assert!(g.lens_auto_usable(lens), "{chain:?}");
    }
    // A crop or rotation anywhere upstream breaks it, so does another lens correction or a layer.
    for chain in [
        vec![NodeKind::Transform],
        vec![NodeKind::Exposure, NodeKind::Transform, NodeKind::Saturation],
        vec![NodeKind::LensManual],
        vec![NodeKind::Develop, NodeKind::LensManual, NodeKind::Exposure],
        vec![NodeKind::Layer],
        vec![NodeKind::Exposure, NodeKind::Layer],
    ] {
        let (g, lens) = lens_chain(&chain);
        assert!(!g.lens_auto_usable(lens), "{chain:?}");
    }
}

#[test]
fn lens_auto_usable_edge_cases() {
    let mut g = Graph::new();
    let lens = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
    assert!(!g.lens_auto_usable(lens), "no input");
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    g.connect(exp, lens, 0).unwrap();
    assert!(!g.lens_auto_usable(lens), "chain without a source");
    assert!(!g.lens_auto_usable(exp), "not a lens node");
    assert!(!g.lens_auto_usable(NodeId(404)));
    // Merge: the first connected input counts.
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let crop = g.add_node(NodeKind::Transform, [0.0, 0.0]);
    g.connect(src, crop, 0).unwrap();
    let m = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    g.connect(src, m, 0).unwrap();
    g.connect(crop, m, 1).unwrap();
    let lens2 = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
    g.connect(m, lens2, 0).unwrap();
    assert!(g.lens_auto_usable(lens2));
    g.connect(crop, m, 0).unwrap();
    assert!(!g.lens_auto_usable(lens2));
}

#[test]
fn validate_reports_lens_auto_that_cannot_work_only_in_auto_mode() {
    let (mut g, lens) = lens_chain(&[NodeKind::Transform]);
    assert!(g.validate().is_empty(), "manual mode is always fine");
    g.node_mut(lens).unwrap().set_param("mode", id(0), 1.0);
    let issues = g.validate();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].node, lens);
    assert!(issues[0].message.starts_with("Lens Correction: Auto"), "{}", issues[0].message);
    let (mut g, lens) = lens_chain(&[NodeKind::Develop]);
    g.node_mut(lens).unwrap().set_param("mode", id(0), 1.0);
    assert!(g.validate().is_empty());
}

#[test]
fn lens_profile_types_round_trip() {
    let profile = LensProfile {
        warp: Some(WarpRectilinear { planes: vec![[1.0, 0.01, 0.0, 0.0, 0.0, 0.0]], center: [0.5, 0.5] }),
        vignette: Some(VignetteRadial { k: [0.1, 0.0, 0.0, 0.0, 0.0], center: [0.5, 0.5] }),
    };
    let back: LensProfile = serde_json::from_str(&serde_json::to_string(&profile).unwrap()).unwrap();
    assert_eq!(back, profile);
    assert_eq!(LensProfile::default(), LensProfile { warp: None, vignette: None });
}

// ---------------------------------------------------------------------------
// Export

fn export_node(g: &mut Graph) -> NodeId {
    g.add_node(NodeKind::Export, [0.0, 0.0])
}

#[test]
fn export_node_shape() {
    let k = NodeKind::Export;
    assert_eq!((k.label(), k.category()), ("Export", "Input/Output"));
    assert_eq!(k.inputs().len(), 1);
    assert_eq!((k.inputs()[0].ty, k.inputs()[0].required), (PortType::Images, true));
    assert!(k.outputs().is_empty());
    assert_eq!(k.output(), None);
    assert_eq!(k.data_kind(), Some(DataKind::Export));
    let names: Vec<&str> = k.params().iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        [
            "format",
            "quality",
            "webp_lossless",
            "size",
            "long_edge",
            "percent",
            "color_space",
            "profile",
            "collision",
            "meta_camera",
            "meta_exposure",
            "meta_date",
            "meta_gps",
            "meta_author",
            "meta_software",
            "meta_keywords"
        ]
    );
    assert_eq!(
        k.param_spec("format").unwrap().kind,
        ParamKind::Choice(&["JPEG", "PNG", "PNG 16-bit", "TIFF", "TIFF 16-bit", "EXR", "AVIF", "Adobe XMP sidecar", "PNG 16-bit data map", "WebP", "HEIC"])
    );
    let q = k.param_spec("quality").unwrap();
    assert_eq!((q.min, q.max, q.default), (1.0, 100.0, 92.0));
    assert_eq!(k.param_spec("size").unwrap().kind, ParamKind::Choice(&["Original", "Long edge", "Percent"]));
    let le = k.param_spec("long_edge").unwrap();
    assert_eq!((le.min, le.max, le.default, le.logarithmic, le.unit), (16.0, 32000.0, 2048.0, true, "px"));
    let pc = k.param_spec("percent").unwrap();
    assert_eq!((pc.min, pc.max, pc.default), (1.0, 100.0, 100.0));
    assert_eq!(
        k.param_spec("color_space").unwrap().kind,
        ParamKind::Choice(&["sRGB", "Display P3", "Adobe RGB (1998)", "Rec. 2020"])
    );
    assert_eq!(k.param_spec("profile").unwrap().label, "Embed colour profile");
    assert_eq!(k.param_spec("collision").unwrap().kind, ParamKind::Choice(&["Skip", "Overwrite", "Rename"]));
    for (name, on) in [
        ("profile", true),
        ("meta_camera", true),
        ("meta_exposure", true),
        ("meta_date", true),
        ("meta_gps", false),
        ("meta_author", true),
        ("meta_software", true),
        ("meta_keywords", true),
    ] {
        let s = k.param_spec(name).unwrap();
        assert_eq!((s.kind, s.default), (ParamKind::Toggle, if on { 1.0 } else { 0.0 }), "{name}");
    }
    // Nothing connects out of an Export.
    let mut g = Graph::new();
    let e = export_node(&mut g);
    let x = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    assert_eq!(g.connect(e, x, 0), Err(GraphError::NoOutput(e)));
}

#[test]
fn export_spec_defaults() {
    let mut g = Graph::new();
    let e = export_node(&mut g);
    let spec = export_spec(g.node(e).unwrap(), id(1));
    assert_eq!(spec, ExportSpec::default());
    assert_eq!(
        spec,
        ExportSpec {
            format: ExportFileFormat::Jpeg,
            quality: 92,
            webp_lossless: false,
            size: ExportSize::Original,
            space: OutputSpace::Srgb,
            embed_profile: true,
            collision: ExportCollision::Skip,
            meta: MetaSelection { camera: true, exposure: true, date: true, gps: false, author: true, software: true, keywords: true },
            text: ExportText {
                out_dir: String::new(),
                name_template: "{name}".to_owned(),
                artist: String::new(),
                copyright: String::new(),
                description: String::new()
            },
        }
    );
    assert_eq!(ExportText::default().name_template, "{name}");
    let other = g.add_node(NodeKind::Output, [0.0, 0.0]);
    assert_eq!(export_spec(g.node(other).unwrap(), id(1)), ExportSpec::default());
}

#[test]
fn export_spec_reads_every_param() {
    let mut g = Graph::new();
    let e = export_node(&mut g);
    let n = g.node_mut(e).unwrap();
    for (i, f) in ExportFileFormat::ALL.iter().enumerate() {
        n.set_param("format", id(1), i as f32);
        assert_eq!(export_spec(n, id(1)).format, *f);
        assert_eq!(f.label(), EXPORT_FORMATS[i]);
    }
    assert_eq!(ExportFileFormat::from_index(9), ExportFileFormat::Webp);
    assert_eq!(ExportFileFormat::from_index(10), ExportFileFormat::Heic);
    assert_eq!(ExportFileFormat::from_index(11), ExportFileFormat::Jpeg);
    assert_eq!(ExportFileFormat::from_index(99), ExportFileFormat::Jpeg);
    for (v, expect) in [(1.0, 1u8), (92.4, 92), (92.6, 93), (100.0, 100)] {
        n.set_param("quality", id(1), v);
        assert_eq!(export_spec(n, id(1)).quality, expect);
    }
    n.set_param("quality", id(1), 0.0);
    assert_eq!(export_spec(n, id(1)).quality, 1, "clamped by the table");
    n.set_param("size", id(1), 1.0);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::LongEdge(2048));
    n.set_param("long_edge", id(1), 4000.4);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::LongEdge(4000));
    n.set_param("long_edge", id(1), 5.0);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::LongEdge(16));
    n.set_param("long_edge", id(1), 99999.0);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::LongEdge(32000));
    n.set_param("size", id(1), 2.0);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::Percent(100.0));
    n.set_param("percent", id(1), 37.5);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::Percent(37.5));
    for (i, space) in OutputSpace::ALL.iter().enumerate() {
        n.set_param("color_space", id(1), i as f32);
        assert_eq!(export_spec(n, id(1)).space, *space);
    }
    n.set_param("profile", id(1), 0.0);
    assert!(!export_spec(n, id(1)).embed_profile);
    for (v, c) in [(0.0, ExportCollision::Skip), (1.0, ExportCollision::Overwrite), (2.0, ExportCollision::Rename)] {
        n.set_param("collision", id(1), v);
        assert_eq!(export_spec(n, id(1)).collision, c);
    }
    for name in ["meta_camera", "meta_exposure", "meta_date", "meta_gps", "meta_author", "meta_software", "meta_keywords"] {
        let before = export_spec(n, id(1)).meta;
        let flipped = !n.toggle(name, id(1));
        n.set_param(name, id(1), if flipped { 1.0 } else { 0.0 });
        let after = export_spec(n, id(1)).meta;
        assert_ne!(before, after, "{name}");
        let count = |m: MetaSelection| [m.camera, m.exposure, m.date, m.gps, m.author, m.software, m.keywords].iter().filter(|b| **b).count();
        assert_eq!(count(after) as i32 - count(before) as i32, if flipped { 1 } else { -1 }, "{name}");
    }
    // Individual mode: per image.
    n.mode = Mode::Individual;
    n.set_param("format", id(7), 5.0);
    assert_eq!(export_spec(n, id(7)).format, ExportFileFormat::Exr);
    assert_eq!(export_spec(n, id(8)).format, ExportFileFormat::Heic, "inherits the All value (last loop value)");
}

#[test]
fn export_spec_reads_non_finite_numbers_as_the_defaults() {
    let mut g = Graph::new();
    let e = export_node(&mut g);
    let n = g.node_mut(e).unwrap();
    n.set_param("size", id(1), 1.0);
    n.set_raw("quality", f32::NAN);
    n.set_raw("long_edge", f32::INFINITY);
    assert_eq!(export_spec(n, id(1)).quality, 92);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::LongEdge(2048));
    n.set_param("size", id(1), 2.0);
    n.set_raw("percent", f32::NAN);
    assert_eq!(export_spec(n, id(1)).size, ExportSize::Percent(100.0));
}

#[test]
fn export_text_limits_and_per_image_text() {
    let mut g = Graph::new();
    let e = export_node(&mut g);
    let n = g.node_mut(e).unwrap();
    let text = |f: &dyn Fn(&mut ExportText)| {
        let mut t = ExportText::default();
        f(&mut t);
        Some(NodeData::Export(t))
    };
    assert!(n.set_data(id(1), text(&|t| {
        t.out_dir = "C:/out".into();
        t.name_template = "{name}_web".into();
        t.artist = "Jane Doe".into();
        t.copyright = "(c) 2026".into();
        t.description = "x".repeat(1024);
    })));
    let spec = export_spec(n, id(1));
    assert_eq!(spec.text.out_dir, "C:/out");
    assert_eq!(spec.text.name_template, "{name}_web");
    assert_eq!((spec.text.artist.as_str(), spec.text.copyright.as_str()), ("Jane Doe", "(c) 2026"));
    assert_eq!(spec.text.description.len(), 1024);
    for (what, f) in [
        ("out_dir", &(|t: &mut ExportText| t.out_dir = "a".repeat(1025)) as &dyn Fn(&mut ExportText)),
        ("name_template", &|t: &mut ExportText| t.name_template = "a".repeat(1025)),
        ("artist", &|t: &mut ExportText| t.artist = "a".repeat(1025)),
        ("copyright", &|t: &mut ExportText| t.copyright = "a".repeat(1025)),
        ("description", &|t: &mut ExportText| t.description = "a".repeat(1025)),
        ("nul out_dir", &|t: &mut ExportText| t.out_dir = "a\0b".into()),
        ("nul artist", &|t: &mut ExportText| t.artist = "\0".into()),
        ("nul copyright", &|t: &mut ExportText| t.copyright = "\0".into()),
        ("nul description", &|t: &mut ExportText| t.description = "\0".into()),
        ("nul template", &|t: &mut ExportText| t.name_template = "{name}\0".into()),
    ] {
        assert!(!n.set_data(id(1), text(f)), "{what}");
    }
    assert!(n.set_data(id(1), text(&|t| t.artist = "\u{e4}".repeat(1024))), "characters, not bytes");
    assert!(!n.set_data(id(1), Some(NodeData::Folder("x".into()))));
    // Per image in Individual mode; the text of an image without its own comes from All.
    assert!(n.set_data(id(0), text(&|t| t.out_dir = "C:/all".into())));
    n.mode = Mode::Individual;
    assert!(n.set_data(id(2), text(&|t| t.out_dir = "C:/two".into())));
    assert_eq!(export_spec(n, id(2)).text.out_dir, "C:/two");
    assert_eq!(export_spec(n, id(3)).text.out_dir, "C:/all");
    // Missing fields in a file read as the defaults.
    let partial: ExportText = serde_json::from_str(r#"{"out_dir": "D:/x"}"#).unwrap();
    assert_eq!((partial.out_dir.as_str(), partial.name_template.as_str()), ("D:/x", "{name}"));
}

#[test]
fn validate_reports_an_export_without_output_folder() {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let e = export_node(&mut g);
    g.connect(src, e, 0).unwrap();
    let issues = g.validate();
    assert_eq!(issues.len(), 1);
    assert_eq!((issues[0].node, issues[0].message.as_str()), (e, "Export: no output folder set"));
    let n = g.node_mut(e).unwrap();
    n.set_data(id(0), Some(NodeData::Export(ExportText { out_dir: "   ".into(), ..Default::default() })));
    assert_eq!(g.validate().len(), 1, "a blank folder is no folder");
    g.node_mut(e)
        .unwrap()
        .set_data(id(0), Some(NodeData::Export(ExportText { out_dir: "C:/out".into(), ..Default::default() })));
    assert!(g.validate().is_empty());
    // An unconnected input is reported too.
    let lone = g.add_node(NodeKind::Export, [0.0, 0.0]);
    let issues = g.validate();
    assert_eq!(issues.iter().filter(|i| i.node == lone).count(), 2);
}

#[test]
fn validate_reports_a_layer_without_input_and_a_merge_like_before() {
    let mut g = Graph::new();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    let issues = g.validate();
    assert_eq!(issues.len(), 1);
    assert_eq!((issues[0].node, issues[0].message.as_str()), (layer, "Layer: no input is connected"));
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    g.connect(src, layer, 5).unwrap();
    assert!(g.validate().is_empty(), "one connected layer is enough, wherever it sits");
}

// ---------------------------------------------------------------------------
// Output colour spaces

fn near(a: &Mat3, b: &Mat3, eps: f64) -> bool {
    (0..3).all(|r| (0..3).all(|c| (a[r][c] - b[r][c]).abs() < eps))
}

#[test]
fn output_space_basics() {
    assert_eq!(OutputSpace::default(), OutputSpace::Srgb);
    assert_eq!(OUTPUT_SPACES, ["sRGB", "Display P3", "Adobe RGB (1998)", "Rec. 2020"]);
    for (i, s) in OutputSpace::ALL.iter().enumerate() {
        assert_eq!(OutputSpace::from_index(i), *s);
        assert_eq!(s.label(), OUTPUT_SPACES[i]);
        assert_eq!(s.index(), i);
    }
    assert_eq!(OutputSpace::from_index(4), OutputSpace::Srgb);
    assert_eq!(OutputSpace::from_index(usize::MAX), OutputSpace::Srgb);
    assert_eq!(OutputSpace::Srgb.transfer(), crate::color::Transfer::Srgb);
    assert_eq!(OutputSpace::DisplayP3.transfer(), crate::color::Transfer::Srgb);
    assert_eq!(OutputSpace::AdobeRgb.transfer(), crate::color::Transfer::Gamma(563.0 / 256.0));
    assert_eq!(OutputSpace::Rec2020.transfer(), crate::color::Transfer::Gamma(2.4));
}

#[test]
fn output_space_primaries() {
    assert_eq!(OutputSpace::Srgb.primaries(), [(0.64, 0.33), (0.30, 0.60), (0.15, 0.06)]);
    assert_eq!(OutputSpace::DisplayP3.primaries(), [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)]);
    assert_eq!(OutputSpace::AdobeRgb.primaries(), [(0.64, 0.33), (0.21, 0.71), (0.15, 0.06)]);
    assert_eq!(OutputSpace::Rec2020.primaries(), [(0.708, 0.292), (0.170, 0.797), (0.131, 0.046)]);
}

#[test]
fn rgb_to_xyz_matches_published_matrices() {
    // sRGB against the constant of the colour module.
    let srgb = rgb_to_xyz(OutputSpace::Srgb.primaries(), D65_XY);
    assert!(near(&srgb, &REC709_TO_XYZ, 2e-4), "{srgb:?}");
    // Published first rows (D65, Y of white = 1).
    let rows = [
        (OutputSpace::DisplayP3, [0.486_571, 0.265_668, 0.198_217]),
        (OutputSpace::AdobeRgb, [0.576_67, 0.185_56, 0.188_23]),
        (OutputSpace::Rec2020, [0.636_958, 0.144_617, 0.168_881]),
    ];
    for (space, row) in rows {
        let m = rgb_to_xyz(space.primaries(), D65_XY);
        for c in 0..3 {
            assert!((m[0][c] - row[c]).abs() < 5e-4, "{space:?} {m:?}");
        }
        // Y row sums to 1 (white has luminance 1), and white lands on D65.
        let white = mul_vec(&m, [1.0, 1.0, 1.0]);
        let d65 = xy_to_xyz(D65_XY);
        for c in 0..3 {
            assert!((white[c] - d65[c]).abs() < 1e-9, "{space:?} white {white:?}");
        }
    }
}

#[test]
fn acescg_to_output_srgb_is_exactly_rec709() {
    assert_eq!(acescg_to_output(OutputSpace::Srgb), acescg_to_rec709());
    assert_eq!(rec709_to_output(OutputSpace::Srgb), [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);
}

#[test]
fn white_maps_to_white_in_every_space() {
    for space in OutputSpace::ALL {
        let w = mul_vec(&acescg_to_output(space), [1.0, 1.0, 1.0]);
        for c in w {
            assert!((c - 1.0).abs() < 1e-3, "{space:?} {w:?}");
        }
        let w = mul_vec(&rec709_to_output(space), [1.0, 1.0, 1.0]);
        for c in w {
            assert!((c - 1.0).abs() < 1e-3, "{space:?} rec709 white {w:?}");
        }
    }
}

#[test]
fn rec709_to_output_is_the_acescg_route_and_round_trips() {
    for space in OutputSpace::ALL {
        // rec709 -> acescg -> output is the same matrix.
        let via = mul(&acescg_to_output(space), &rec709_to_acescg());
        assert!(near(&via, &rec709_to_output(space), 1e-12), "{space:?}");
        // acescg -> output equals acescg -> rec709 -> output.
        let other = mul(&rec709_to_output(space), &acescg_to_rec709());
        assert!(near(&other, &acescg_to_output(space), 1e-12), "{space:?}");
        // And back.
        let back = invert(&rec709_to_output(space));
        let id3 = mul(&back, &rec709_to_output(space));
        assert!(near(&id3, &[[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]], 1e-12));
        // The Rec.709 gamut lies inside every wider space (no negative components for its primaries).
        for primary in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
            let v = mul_vec(&rec709_to_output(space), primary);
            assert!(v.iter().all(|c| *c > -1e-3 && *c < 1.0 + 1e-3), "{space:?} {v:?}");
        }
    }
    // A wider space needs smaller numbers for the same saturated colour.
    let red_p3 = mul_vec(&rec709_to_output(OutputSpace::DisplayP3), [1.0, 0.0, 0.0]);
    assert!(red_p3[0] < 1.0 && red_p3[1] > 0.0);
    let red_2020 = mul_vec(&rec709_to_output(OutputSpace::Rec2020), [1.0, 0.0, 0.0]);
    assert!(red_2020[0] < red_p3[0]);
}

#[test]
fn encode_values() {
    for space in OutputSpace::ALL {
        assert_eq!(space.encode(0.0), 0.0, "{space:?}");
        assert!((space.encode(1.0) - 1.0).abs() < 1e-6, "{space:?}");
        assert_eq!(space.encode(-0.18), -space.encode(0.18), "mirrored negatives for {space:?}");
        let mut last = -1.0;
        for i in 0..=100 {
            let v = space.encode(i as f32 / 100.0);
            assert!(v >= last, "{space:?} monotonic");
            last = v;
        }
    }
    let s = srgb_oetf(0.18);
    assert!((OutputSpace::Srgb.encode(0.18) - s).abs() < 1e-7);
    assert!((OutputSpace::Srgb.encode(0.18) - 0.4614).abs() < 1e-3);
    assert_eq!(OutputSpace::DisplayP3.encode(0.18), OutputSpace::Srgb.encode(0.18));
    let adobe = 0.18f64.powf(256.0 / 563.0) as f32;
    assert!((OutputSpace::AdobeRgb.encode(0.18) - adobe).abs() < 1e-6);
    assert!((OutputSpace::AdobeRgb.encode(0.18) - 0.4586).abs() < 1e-3);
    let rec2020 = 0.18f64.powf(1.0 / 2.4) as f32;
    assert!((OutputSpace::Rec2020.encode(0.18) - rec2020).abs() < 1e-6);
    assert!((OutputSpace::Rec2020.encode(0.18) - 0.4893).abs() < 1e-3);
    // Linear segment of sRGB, and a value above 1 does not clamp.
    assert!((OutputSpace::Srgb.encode(0.001) - 0.012_92).abs() < 1e-6);
    assert!(OutputSpace::Rec2020.encode(4.0) > 1.0);
    assert!(OutputSpace::AdobeRgb.encode(f32::NAN).is_nan());
}

// ---------------------------------------------------------------------------
// Graph format

const FIXTURE_V3: &str = include_str!("../tests/fixtures/graph_v3.json");

#[test]
fn version_3_fixture_round_trips_and_routes_images() {
    assert_eq!(GRAPH_FORMAT_VERSION, 5);
    assert!(FIXTURE_V3.contains("\"format_version\": 3"));
    let g = Graph::from_json(FIXTURE_V3).expect("version 3 loads");
    assert_eq!(g.nodes().count(), 11);
    assert_eq!(g.edges().len(), 11);
    let find = |kind: NodeKind| g.nodes().find(|n| n.kind == kind).unwrap();
    let a = id(1);
    let b = id(2);
    // Parameters.
    assert_eq!(find(NodeKind::Exposure).param("ev", a), 1.25);
    let dev = find(NodeKind::Develop);
    assert_eq!((dev.choice("wb", a), dev.param("temperature", a)), (1, 5200.0));
    assert_eq!(dev.param("brightness", a), 1.6);
    assert!(develop_tone(dev, a).is_identity(), "tone values are new and default to neutral");
    assert_eq!(find(NodeKind::LensManual).param("distortion", a), 0.1);
    assert_eq!(find(NodeKind::LensManual).choice("mode", a), 0, "old lens nodes stay manual");
    // Data.
    let curve = find(NodeKind::PointCurve);
    assert_eq!(curve.data(a), Some(&NodeData::Curve(vec![[0.0, 0.0], [0.4, 0.5], [1.0, 1.0]])));
    assert_eq!(curve_set(curve, a), vec![CurveEntry { channel: 1, points: vec![[0.0, 0.0], [0.4, 0.5], [1.0, 1.0]] }]);
    let brush = find(NodeKind::Draw);
    assert!(matches!(brush.data(a), Some(NodeData::Strokes(s)) if s.len() == 1 && s[0].points.len() == 2));
    assert!(brush.data(b).is_none());
    let split = find(NodeKind::MetadataSplit);
    assert!(matches!(split.data(a), Some(NodeData::Ranges(r)) if r.len() == 2 && r[1].name == "high"));
    let filt = find(NodeKind::MetadataFilter);
    assert_eq!(filt.mode, Mode::Individual);
    assert_eq!((filt.param("min", a), filt.param("min", b), filt.toggle("invert", b)), (100.0, 200.0, true));
    // New settings read as their defaults.
    let src = find(NodeKind::Source);
    assert!(src.data(a).is_none());
    assert!(src.toggle("subfolders", a));
    assert_eq!(find(NodeKind::Merge).choice("collision", a), 0);

    // Membership works as before (old API, empty catalog).
    let caps = |i: ImageId| Some(CaptureInfo { iso: Some(i.0 as f32 * 100.0), ..Default::default() });
    let out = find(NodeKind::Output).id;
    let reached: Vec<u64> = (1..=8).filter(|i| g.contains(out, id(*i), &caps)).collect();
    // ISO 100 * n. Image 2 is turned away by its own inverted filter values, images 5 and up fall into
    // the second range, whose port is not wired to the merge.
    assert_eq!(reached, vec![1, 3, 4]);
    let src_id = src.id;
    assert!((1..=8).all(|i| g.contains(src_id, id(i), &caps)));

    // Saving writes the current version, which loads back to the same graph.
    let json = g.to_json();
    assert!(json.contains("\"format_version\": 5"));
    assert_eq!(Graph::from_json(&json), Ok(g.clone()));
    // Current files and no newer ones.
    assert!(Graph::from_json(&json).is_ok());
    let newer = json.replace("\"format_version\": 5", "\"format_version\": 6");
    assert_eq!(Graph::from_json(&newer), Err(GraphError::UnsupportedVersion(6)));
}

#[test]
fn every_new_data_kind_round_trips_in_both_scopes() {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [1.0, 2.0]);
    let curves = g.add_node(NodeKind::PointCurve, [3.0, 4.0]);
    let export = g.add_node(NodeKind::Export, [5.0, 6.0]);
    let layer = g.add_node(NodeKind::Layer, [7.0, 8.0]);
    let merge = g.add_node(NodeKind::Merge, [9.0, 1.0]);
    let lens = g.add_node(NodeKind::LensManual, [9.0, 2.0]);
    g.connect(src, layer, 0).unwrap();
    g.connect(src, merge, 1).unwrap();
    g.connect(layer, curves, 0).unwrap();
    g.connect(curves, lens, 0).unwrap();
    g.connect(lens, export, 0).unwrap();

    g.node_mut(src).unwrap().set_data(id(0), Some(NodeData::Folder("C:/Photos/\u{c4}rger".into())));
    g.node_mut(src).unwrap().set_param("subfolders", id(0), 0.0);
    g.node_mut(merge).unwrap().set_param("collision", id(0), 2.0);
    g.node_mut(lens).unwrap().set_param("mode", id(0), 1.0);
    {
        let n = g.node_mut(curves).unwrap();
        n.set_data(
            id(0),
            Some(NodeData::CurveSet(vec![
                CurveEntry { channel: 0, points: bend(0.3) },
                CurveEntry { channel: 5, points: vec![[0.0, 0.5], [0.5, 0.7], [1.0, 0.5]] },
            ])),
        );
        n.mode = Mode::Individual;
        n.set_data(id(9), Some(NodeData::CurveSet(vec![CurveEntry { channel: 4, points: vec![[0.0, 0.2], [1.0, 0.8]] }])));
    }
    {
        let n = g.node_mut(export).unwrap();
        n.set_data(
            id(0),
            Some(NodeData::Export(ExportText {
                out_dir: "D:/out".into(),
                name_template: "{name}-{n}".into(),
                artist: "A".into(),
                copyright: "C".into(),
                description: "D".into(),
            })),
        );
        n.set_param("format", id(0), 6.0);
        n.set_param("color_space", id(0), 3.0);
        n.mode = Mode::Individual;
        n.set_data(id(4), Some(NodeData::Export(ExportText { out_dir: "E:/four".into(), ..Default::default() })));
        n.set_param("quality", id(4), 50.0);
    }
    {
        let n = g.node_mut(layer).unwrap();
        n.set_param("l2_blend", id(0), 3.0);
        n.set_param("l2_opacity", id(0), 0.5);
        n.mode = Mode::Individual;
        n.set_param("l1_fit", id(3), 1.0);
    }
    let json = g.to_json();
    let back = Graph::from_json(&json).expect("loads");
    assert_eq!(back, g);
    assert_eq!(back.node(curves).unwrap().data(id(9)), g.node(curves).unwrap().data(id(9)));
    assert_eq!(export_spec(back.node(export).unwrap(), id(4)).text.out_dir, "E:/four");
    assert_eq!(export_spec(back.node(export).unwrap(), id(4)).quality, 50);
    assert_eq!(export_spec(back.node(export).unwrap(), id(5)).space, OutputSpace::Rec2020);
    // Sub-graphs carry the All data of the new kinds, not the per-image data.
    let sub = g.extract_subgraph(&[src, curves, export, layer]);
    let parsed = Subgraph::from_json(&sub.to_json()).expect("sub-graph loads");
    assert_eq!(parsed, sub);
    let mut other = Graph::new();
    let ids = other.insert_subgraph(&parsed, [0.0, 0.0]);
    assert_eq!(other.node(ids[0]).unwrap().data(id(1)), Some(&NodeData::Folder("C:/Photos/\u{c4}rger".into())));
    assert!(matches!(other.node(ids[1]).unwrap().data(id(1)), Some(NodeData::CurveSet(_))));
}

#[test]
fn loading_rejects_invalid_new_data() {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
    g.node_mut(src).unwrap().set_data(id(0), Some(NodeData::Folder("C:/x".into())));
    g.node_mut(export).unwrap().set_data(id(0), Some(NodeData::Export(ExportText::default())));
    let base = serde_json::from_str::<serde_json::Value>(&g.to_json()).unwrap();
    assert!(Graph::from_json(&base.to_string()).is_ok());
    let load = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut v = base.clone();
        edit(&mut v);
        Graph::from_json(&v.to_string())
    };
    let parse_err = |r: Result<Graph, GraphError>, needle: &str| match r {
        Err(GraphError::Parse(m)) => assert!(m.contains(needle), "{m:?} lacks {needle:?}"),
        other => panic!("expected a parse error about {needle:?}, got {other:?}"),
    };
    parse_err(load(&|v| v["nodes"]["1"]["data"] = serde_json::json!({"Folder": "a".repeat(1025)})), "longer than 1024");
    parse_err(load(&|v| v["nodes"]["1"]["data"] = serde_json::json!({"Folder": "a\u{0}b"})), "NUL");
    parse_err(load(&|v| v["nodes"]["1"]["data"] = serde_json::json!({"Export": {"out_dir": "x"}})), "takes Folder data");
    parse_err(load(&|v| v["nodes"]["2"]["data"] = serde_json::json!({"Folder": "x"})), "takes Export data");
    for field in ["out_dir", "name_template", "artist", "copyright", "description"] {
        parse_err(
            load(&|v| v["nodes"]["2"]["data"] = serde_json::json!({"Export": {field: "a".repeat(1025)}})),
            "longer than 1024",
        );
        parse_err(load(&|v| v["nodes"]["2"]["data"] = serde_json::json!({"Export": {field: "a\u{0}"}})), "NUL");
    }
    // The same through the per-image scope.
    parse_err(
        load(&|v| v["nodes"]["2"]["individual_data"] = serde_json::json!({"5": {"Export": {"artist": "a".repeat(1025)}}})),
        "image 5",
    );
    // Bad parameter values of the new parameters.
    parse_err(load(&|v| v["nodes"]["2"]["all"]["quality"] = serde_json::json!(101)), "outside 1..100");
    parse_err(load(&|v| v["nodes"]["2"]["all"]["format"] = serde_json::json!(11)), "outside 0..10");
    parse_err(load(&|v| v["nodes"]["2"]["all"]["long_edge"] = serde_json::json!(15)), "outside 16..32000");
    parse_err(load(&|v| v["nodes"]["2"]["all"]["percent"] = serde_json::json!(0)), "outside 1..100");
    parse_err(load(&|v| v["nodes"]["1"]["all"]["subfolders"] = serde_json::json!(2)), "outside 0..1");
    parse_err(load(&|v| v["nodes"]["1"]["all"]["collision"] = serde_json::json!(1)), "unknown parameter");
}

#[test]
fn new_kinds_are_in_every_list_and_round_trip() {
    for kind in [NodeKind::Layer, NodeKind::Export] {
        assert!(NodeKind::ALL.contains(&kind));
    }
    assert_eq!(NodeKind::ALL.len(), 33);
    let mut seen = std::collections::HashSet::new();
    for kind in NodeKind::ALL {
        assert!(seen.insert(kind), "{kind:?} listed twice");
        assert!(!kind.label().is_empty() && !kind.category().is_empty());
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(serde_json::from_str::<NodeKind>(&json).unwrap(), kind);
    }
    assert_eq!(serde_json::to_string(&NodeKind::Layer).unwrap(), "\"Layer\"");
    assert_eq!(serde_json::to_string(&NodeKind::Export).unwrap(), "\"Export\"");
    // Old serialised names are unchanged.
    assert_eq!(serde_json::to_string(&NodeKind::PointCurve).unwrap(), "\"PointCurve\"");
    assert_eq!(serde_json::to_string(&NodeKind::LensManual).unwrap(), "\"LensManual\"");
}

#[test]
fn capture_path_is_never_persisted() {
    let c = CaptureInfo { path: Some("C:/x.cr3".into()), iso: Some(100.0), ..Default::default() };
    let json = serde_json::to_string(&c).unwrap();
    assert!(!json.contains("x.cr3") && !json.contains("path"), "{json}");
    let back: CaptureInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(back.path, None);
    assert_eq!(back.iso, Some(100.0));
}

// ---------------------------------------------------------------------------
// Robustness and speed

#[test]
fn queries_never_panic_on_nonsense() {
    let lib = iso_lib();
    let mut g = Graph::new();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
    let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
    let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
    for node in [layer, split, merge, export, NodeId(0), NodeId(u64::MAX)] {
        for port in [0, 1, 7, 8, usize::MAX] {
            let _ = lib.members(&g, node, port);
            for i in [0, 1, u64::MAX] {
                let _ = lib.with(|c| g.contains_output_in(node, port, id(i), c));
                let _ = lib.with(|c| g.layer_sources(node, id(i), c));
                let _ = lib.with(|c| g.merge_source_in(node, id(i), c));
                let _ = lib.with(|c| g.split_port_in(node, id(i), c));
            }
        }
        let _ = g.capture_ev_delta(node, id(1));
        let _ = g.lens_auto_usable(node);
    }
    // A catalog with duplicated and unknown images.
    let images = vec![id(1), id(1), id(404), id(2)];
    let cap = |i: ImageId| lib.caps.get(&i).cloned();
    let cat = Catalog { images: &images, capture: &cap, memo: None };
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let _ = g.members(src, 0, &cat);
}

#[test]
fn members_of_four_thousand_images_through_ten_nodes_is_fast() {
    const N: u64 = 4000;
    let entries: Vec<(u64, String, f32)> = (1..=N)
        .map(|i| {
            let folder = if i % 3 == 0 { "b" } else { "a" };
            // Names repeat across folders so the Merge has real collisions.
            (i, format!("D:/Photos/{folder}/IMG_{:04}.CR3", i % 2500), 100.0 + (i % 40) as f32 * 100.0)
        })
        .collect();
    let borrowed: Vec<(u64, Option<&str>, Option<f32>)> =
        entries.iter().map(|(i, p, iso)| (*i, Some(p.as_str()), Some(*iso))).collect();
    let lib = Lib::new(&borrowed);

    let mut g = Graph::new();
    let src = source_in(&mut g, Some("D:/Photos"), true); // 1
    let filter = filter_between(&mut g, src, 100.0, 3500.0); // 2
    let develop = g.add_node(NodeKind::Develop, [0.0, 0.0]); // 3
    g.connect(filter, develop, 0).unwrap();
    let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]); // 4
    g.connect(develop, split, 0).unwrap();
    g.node_mut(split).unwrap().set_data(
        id(0),
        Some(NodeData::Ranges(vec![
            NamedRange { name: "low".into(), min: 0.0, max: 1500.0 },
            NamedRange { name: "high".into(), min: 1501.0, max: 5000.0 },
        ])),
    );
    let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]); // 5
    g.connect_ports(split, 1, merge, 0).unwrap();
    g.connect_ports(split, 0, merge, 1).unwrap();
    g.connect_ports(split, SPLIT_UNMATCHED_PORT, merge, 2).unwrap();
    set_collision(&mut g, merge, 1.0);
    let exposure = g.add_node(NodeKind::Exposure, [0.0, 0.0]); // 6
    g.connect(merge, exposure, 0).unwrap();
    let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]); // 7
    g.connect(exposure, layer, 0).unwrap();
    let all = source_in(&mut g, None, true); // 8
    g.connect(all, layer, 1).unwrap();
    let contrast = g.add_node(NodeKind::Contrast, [0.0, 0.0]); // 9
    g.connect(layer, contrast, 0).unwrap();
    let export = g.add_node(NodeKind::Export, [0.0, 0.0]); // 10
    g.connect(contrast, export, 0).unwrap();

    let mut best = f64::MAX;
    let mut count = 0;
    for _ in 0..7 {
        let t = Instant::now();
        let m = lib.with(|c| g.members(export, 0, c));
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        count = m.len();
    }
    eprintln!("members(4000 images, 10 nodes): {best:.2} ms, {count} members");
    assert!(count > 1000, "{count}");
    assert!(best < 30.0, "members took {best:.2} ms");

    // The same collection through the point queries, for a sample of images.
    let listed: std::collections::HashSet<ImageId> = lib.with(|c| g.members(export, 0, c)).into_iter().collect();
    for i in (1..=N).step_by(397) {
        assert_eq!(lib.contains(&g, export, i), listed.contains(&id(i)), "image {i}");
    }
}
