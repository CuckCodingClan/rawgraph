//! Graph-level tests of the HDR Merge node: membership, groups, agreement of
//! `members` and `contains_in`, settings and format round trip.

use std::collections::HashMap;

use crate::*;

const fn id(n: u64) -> ImageId {
    ImageId(n)
}

struct Lib {
    images: Vec<ImageId>,
    caps: HashMap<ImageId, CaptureInfo>,
}

impl Lib {
    /// (id, capture ms, shutter s); ISO 100, f/1.
    fn new(entries: &[(u64, Option<i64>, Option<f32>)]) -> Lib {
        let mut images = Vec::new();
        let mut caps = HashMap::new();
        for &(n, t, sh) in entries {
            images.push(id(n));
            caps.insert(
                id(n),
                CaptureInfo {
                    captured_unix_ms: t,
                    shutter_s: sh,
                    iso: sh.map(|_| 100.0),
                    aperture: sh.map(|_| 1.0),
                    ..Default::default()
                },
            );
        }
        Lib { images, caps }
    }

    fn with<R>(&self, f: impl FnOnce(&Catalog) -> R) -> R {
        let cap = |i: ImageId| self.caps.get(&i).cloned();
        f(&Catalog { images: &self.images, capture: &cap, memo: None })
    }

    fn members(&self, g: &Graph, node: NodeId) -> Vec<u64> {
        self.with(|c| g.members(node, 0, c)).iter().map(|i| i.0).collect()
    }
}

/// Two brackets of three (-2, 0, +2 EV), 10 s apart, plus a lone frame without time.
fn bracket_lib() -> Lib {
    let (a, b, c) = (Some(0.25), Some(1.0), Some(4.0));
    Lib::new(&[
        (1, Some(0), a),
        (2, Some(500), b),
        (3, Some(1_000), c),
        (4, Some(20_000), a),
        (5, Some(20_500), b),
        (6, Some(21_000), c),
        (7, None, Some(1.0)),
    ])
}

fn hdr_graph() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let hdr = g.add_node(NodeKind::HdrMerge, [0.0, 0.0]);
    g.connect(src, hdr, 0).unwrap();
    (g, src, hdr)
}

#[test]
fn node_shape_and_defaults() {
    let k = NodeKind::HdrMerge;
    assert_eq!((k.label(), k.category()), ("HDR Merge", "Collection"));
    assert_eq!(k.inputs().len(), 1);
    assert!(k.inputs()[0].required);
    assert_eq!(k.outputs().len(), 1);
    let d = |n: &str| k.param_spec(n).unwrap().default;
    assert_eq!(d("group_by"), 1.0, "Bracket");
    assert_eq!((d("gap_s"), d("max_size"), d("group"), d("exposure")), (2.0, 9.0, 0.0, 0.0));
    assert_eq!((d("align"), d("max_shift_pct"), d("deghost"), d("clip")), (1.0, 3.0, 0.5, 0.95));
    assert_eq!(k.param_spec("group_by").unwrap().kind, ParamKind::Choice(&GROUP_RULES));
    assert_eq!(serde_json::to_string(&k).unwrap(), "\"HdrMerge\"");
}

#[test]
fn output_collection_is_the_references_in_input_order() {
    let lib = bracket_lib();
    let (g, _src, hdr) = hdr_graph();
    // Bracket rule: {1,2,3} -> 2, {4,5,6} -> 5, 7 alone (no time).
    assert_eq!(lib.members(&g, hdr), vec![2, 5, 7]);
}

#[test]
fn rules_change_the_collection() {
    let lib = bracket_lib();
    let (mut g, _src, hdr) = hdr_graph();
    let n = g.node_mut(hdr).unwrap();
    n.set_param("group_by", id(0), 0.0); // capture time, gap 2 s
    assert_eq!(lib.members(&g, hdr), vec![2, 5, 7]);
    let n = g.node_mut(hdr).unwrap();
    n.set_param("gap_s", id(0), 60.0);
    assert_eq!(lib.members(&g, hdr), vec![2, 7], "one group of six, median exposure (ties in input order)");
    let n = g.node_mut(hdr).unwrap();
    n.set_param("max_size", id(0), 2.0);
    // Groups of two in capture order: [1,2] [3,4] [5,6]; the reference is the darker frame of each.
    assert_eq!(lib.members(&g, hdr), vec![1, 4, 5, 7]);
    let n = g.node_mut(hdr).unwrap();
    n.set_param("group_by", id(0), 3.0); // whole collection, capped at 2
    assert_eq!(lib.members(&g, hdr), vec![1, 3, 4, 5, 6, 7]);
}

#[test]
fn manual_rule_reads_the_individual_group_number() {
    let lib = bracket_lib();
    let (mut g, _src, hdr) = hdr_graph();
    let n = g.node_mut(hdr).unwrap();
    n.set_param("group_by", id(0), 2.0);
    // All mode: everything shares number 0 -> every image alone.
    assert_eq!(lib.members(&g, hdr), vec![1, 2, 3, 4, 5, 6, 7]);
    let n = g.node_mut(hdr).unwrap();
    n.mode = Mode::Individual;
    for i in [1, 3, 5] {
        n.set_param("group", id(i), 4.0);
    }
    n.set_param("group", id(2), 9.0);
    n.set_param("group", id(4), 9.0);
    // Group 4 = {1,3,5} (EV -2, +2, 0) -> median is 5; group 9 = {2,4} (EV 0, -2) -> lower middle is 4.
    let m = lib.members(&g, hdr);
    assert_eq!(m, vec![4, 5, 6, 7]);
    let grp = lib.with(|c| g.image_group(hdr, id(5), c)).unwrap();
    assert_eq!(grp.members, vec![id(1), id(3), id(5)]);
}

#[test]
fn image_group_returns_members_for_references_only() {
    let lib = bracket_lib();
    let (mut g, src, hdr) = hdr_graph();
    let grp = lib.with(|c| g.image_group(hdr, id(5), c)).unwrap();
    assert_eq!((grp.reference, grp.members), (id(5), vec![id(4), id(5), id(6)]));
    assert!(lib.with(|c| g.image_group(hdr, id(4), c)).is_none(), "not a reference");
    assert!(lib.with(|c| g.image_group(src, id(5), c)).is_none(), "not an HDR Merge");
    let solo = lib.with(|c| g.image_group(hdr, id(7), c)).unwrap();
    assert_eq!(solo.members, vec![id(7)]);
    let empty = Catalog { images: &[], capture: &|_| None, memo: None };
    assert!(g.image_group(hdr, id(5), &empty).is_none());
    g.remove_node(hdr);
    assert!(lib.with(|c| g.image_group(hdr, id(5), c)).is_none());
}

#[test]
fn members_and_contains_agree_for_every_rule_and_downstream() {
    let lib = bracket_lib();
    for rule in 0..4 {
        for gap in [0.0, 2.0, 60.0] {
            let (mut g, _src, hdr) = hdr_graph();
            let n = g.node_mut(hdr).unwrap();
            n.set_param("group_by", id(0), rule as f32);
            n.set_param("gap_s", id(0), gap);
            if rule == 2 {
                n.mode = Mode::Individual;
                for i in 1..=6 {
                    n.set_param("group", id(i), (i % 3 + 1) as f32);
                }
            }
            let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
            g.connect(hdr, exp, 0).unwrap();
            for node in [hdr, exp] {
                let listed = lib.members(&g, node);
                for i in 1..=7 {
                    assert_eq!(
                        lib.with(|c| g.contains_output_in(node, 0, id(i), c)),
                        listed.contains(&i),
                        "rule {rule} gap {gap} node {node:?} image {i}"
                    );
                }
            }
        }
    }
}

#[test]
fn without_a_catalog_everything_of_the_input_counts() {
    let (g, _src, hdr) = hdr_graph();
    assert!(g.contains(hdr, id(1), &|_| None));
}

#[test]
fn unconnected_input_gives_nothing_and_an_issue() {
    let mut g = Graph::new();
    let hdr = g.add_node(NodeKind::HdrMerge, [0.0, 0.0]);
    let lib = bracket_lib();
    assert!(lib.members(&g, hdr).is_empty());
    assert!(g.validate().iter().any(|i| i.node == hdr));
}

#[test]
fn graph_with_an_hdr_merge_round_trips_and_older_builds_would_refuse_it() {
    let (mut g, _src, hdr) = hdr_graph();
    g.node_mut(hdr).unwrap().set_param("max_size", id(0), 5.0);
    let json = g.to_json();
    assert_eq!(Graph::from_json(&json), Ok(g.clone()));
    // A reader that does not know the kind fails on the name instead of dropping the node.
    let unknown = json.replace("\"HdrMerge\"", "\"HdrMergeFuture\"");
    assert!(Graph::from_json(&unknown).unwrap_err().to_string().contains("unknown variant"));
}

#[test]
fn lens_auto_is_not_usable_behind_an_hdr_merge() {
    let (mut g, _src, hdr) = hdr_graph();
    let lens = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
    g.connect(hdr, lens, 0).unwrap();
    g.node_mut(lens).unwrap().set_param("mode", id(0), 1.0);
    assert!(!g.lens_auto_usable(lens));
}

#[test]
fn a_shared_memo_builds_the_group_plan_once_for_many_queries() {
    use crate::QueryMemo;
    use std::cell::Cell;
    let lib = bracket_lib();
    let (g, _src, hdr) = hdr_graph();
    let calls = Cell::new(0usize);
    let cap = |i: ImageId| {
        calls.set(calls.get() + 1);
        lib.caps.get(&i).cloned()
    };
    // Without a memo every query starts fresh and asks for the captures again.
    let fresh = Catalog { images: &lib.images, capture: &cap, memo: None };
    for r in [2, 5, 7] {
        assert!(g.image_group(hdr, id(r), &fresh).is_some());
        assert!(g.contains_in(hdr, id(r), &fresh));
    }
    let unshared = calls.replace(0);
    // With one the plan is computed once; each image's capture is fetched once.
    let memo = QueryMemo::default();
    let shared = Catalog { images: &lib.images, capture: &cap, memo: Some(&memo) };
    for r in [2, 5, 7] {
        assert!(g.image_group(hdr, id(r), &shared).is_some());
        assert!(g.contains_in(hdr, id(r), &shared));
        assert!(!g.contains_in(hdr, id(r - 1), &shared));
    }
    assert_eq!(calls.get(), lib.images.len(), "one capture lookup per image");
    assert!(unshared >= 3 * lib.images.len(), "{unshared}");
    assert_eq!(g.members(hdr, 0, &shared), vec![id(2), id(5), id(7)]);
}
