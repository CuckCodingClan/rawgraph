//! Graph-level tests of the Panorama node and of chains of group nodes (HDR Merge, then Panorama):
//! node shape, rule lists, grouping by capture time, membership, nearest group node, frame counts.

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
    /// (id, capture ms, shutter s, ISO 100 and f/1 when a shutter is given).
    fn new(entries: &[(u64, Option<i64>, Option<f32>)]) -> Lib {
        let mut images = Vec::new();
        let mut caps = HashMap::new();
        for &(n, t, sh) in entries {
            images.push(id(n));
            caps.insert(
                id(n),
                CaptureInfo { captured_unix_ms: t, shutter_s: sh, iso: sh.map(|_| 100.0), aperture: sh.map(|_| 1.0), ..Default::default() },
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

/// Frames 1..=5 shot 3 s apart, then 6..=8 a minute later.
fn sweep_lib() -> Lib {
    let mut list: Vec<(u64, Option<i64>, Option<f32>)> = (1..=5).map(|k| (k, Some(k as i64 * 3_000), Some(0.01))).collect();
    list.extend((6..=8).map(|k| (k, Some(60_000 + k as i64 * 3_000), Some(0.01))));
    Lib::new(&list)
}

fn pano_graph() -> (Graph, NodeId, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let pano = g.add_node(NodeKind::Panorama, [0.0, 0.0]);
    g.connect(src, pano, 0).unwrap();
    (g, src, pano)
}

#[test]
fn node_shape_and_defaults() {
    let k = NodeKind::Panorama;
    assert_eq!((k.label(), k.category()), ("Panorama", "Collection"));
    assert_eq!(k.inputs().len(), 1);
    assert!(k.inputs()[0].required);
    assert_eq!(k.outputs().len(), 1);
    assert_eq!(k.output(), Some(PortType::Images));
    let spec = |n: &str| k.param_spec(n).unwrap();
    let d = |n: &str| spec(n).default;
    assert_eq!((d("group_by"), d("gap_s"), d("max_size"), d("group")), (0.0, 5.0, 25.0, 0.0));
    assert_eq!((d("projection"), d("blend"), d("levels")), (0.0, 0.0, 5.0));
    assert_eq!((d("exposure_comp"), d("crop"), d("focal"), d("focal_mm_35"), d("work_px")), (1.0, 1.0, 0.0, 0.0, 1200.0));
    assert_eq!(spec("group_by").kind, ParamKind::Choice(&PANORAMA_GROUP_RULES));
    assert_eq!((spec("gap_s").min, spec("gap_s").max), (0.0, 120.0));
    assert_eq!((spec("max_size").min, spec("max_size").max), (2.0, 25.0));
    assert_eq!((spec("levels").min, spec("levels").max), (2.0, 6.0));
    assert_eq!((spec("focal_mm_35").min, spec("focal_mm_35").max), (0.0, 400.0));
    assert_eq!((spec("work_px").min, spec("work_px").max), (600.0, 3000.0));
    assert_eq!(serde_json::to_string(&k).unwrap(), "\"Panorama\"");
    assert!(NodeKind::ALL.contains(&k));
}

#[test]
fn every_kind_lists_its_own_rules_and_limits() {
    let hdr = group_kind(NodeKind::HdrMerge).unwrap();
    let pano = group_kind(NodeKind::Panorama).unwrap();
    assert!(group_kind(NodeKind::Source).is_none() && group_kind(NodeKind::Layer).is_none());
    assert_eq!((hdr.reference, hdr.default_rule, hdr.max_size), (Reference::MiddleExposure, GroupRule::Bracket, 25));
    assert_eq!((pano.reference, pano.default_rule, pano.max_size), (Reference::Middle, GroupRule::CaptureTime, 25));
    // The Choice index maps through the node's own list.
    assert_eq!(hdr.rules.len(), GROUP_RULES.len());
    assert_eq!(pano.rules.len(), PANORAMA_GROUP_RULES.len());
    assert_eq!(hdr.rule(1), GroupRule::Bracket);
    assert_eq!(pano.rule(1), GroupRule::Manual, "a Panorama has no Bracket: option 1 is Manual");
    assert_eq!(pano.rule(2), GroupRule::Whole);
    assert_eq!(hdr.rule(3), GroupRule::Whole);
    // An index outside the list falls back to the kind's default, not to Bracket.
    assert_eq!(pano.rule(7), GroupRule::CaptureTime);
    assert_eq!(hdr.rule(7), GroupRule::Bracket);
    // The node's option list and its rule list agree label by label.
    let labels: Vec<&str> = pano.rules.iter().map(|r| match r {
        GroupRule::CaptureTime => "Capture time",
        GroupRule::Manual => "Manual",
        GroupRule::Whole => "Whole collection",
        GroupRule::Bracket => "Bracket",
    }).collect();
    assert_eq!(labels, PANORAMA_GROUP_RULES.to_vec());
    // The parameter ranges of the nodes stay inside the kind's limits.
    assert_eq!(NodeKind::HdrMerge.param_spec("max_size").unwrap().max as usize, hdr.max_size);
    assert_eq!(NodeKind::Panorama.param_spec("max_size").unwrap().max as usize, pano.max_size);
}

#[test]
fn capture_time_makes_one_panorama_per_sweep_with_the_middle_frame_as_reference() {
    let lib = sweep_lib();
    let (g, _src, pano) = pano_graph();
    // 3 s apart inside a sweep (gap 5 s), a minute between sweeps: {1..5} -> 3, {6,7,8} -> 7.
    assert_eq!(lib.members(&g, pano), vec![3, 7]);
    let grp = lib.with(|c| g.image_group(pano, id(3), c)).unwrap();
    assert_eq!(grp.members, (1..=5).map(id).collect::<Vec<_>>());
    assert_eq!(grp.reference, id(3));
    assert!(lib.with(|c| g.image_group(pano, id(2), c)).is_none(), "not a reference");
    // Only references are in the output collection; contains_in agrees with members.
    for n in 1..=8 {
        let inside = lib.with(|c| g.contains_in(pano, id(n), c));
        assert_eq!(inside, n == 3 || n == 7, "image {n}");
    }
}

#[test]
fn the_gap_and_the_size_limit_cut_a_sweep() {
    let lib = sweep_lib();
    let (mut g, _src, pano) = pano_graph();
    g.node_mut(pano).unwrap().set_param("gap_s", id(0), 2.0);
    assert_eq!(lib.members(&g, pano), vec![1, 2, 3, 4, 5, 6, 7, 8], "3 s gaps are cut by a 2 s limit: every frame alone");
    g.node_mut(pano).unwrap().set_param("gap_s", id(0), 5.0);
    g.node_mut(pano).unwrap().set_param("max_size", id(0), 2.0);
    // Pairs in capture order: [1,2] [3,4] [5] [6,7] [8]; the reference is the first of a pair (lower middle).
    assert_eq!(lib.members(&g, pano), vec![1, 3, 5, 6, 8]);
    // Sizes above the node's limit are not accepted (clamped to 25).
    g.node_mut(pano).unwrap().set_param("max_size", id(0), 25.0);
    g.node_mut(pano).unwrap().set_param("gap_s", id(0), 120.0);
    let one = lib.with(|c| g.image_groups(pano, c));
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].members.len(), 8);
}

#[test]
fn manual_and_whole_rules_use_the_panoramas_own_option_list() {
    let lib = sweep_lib();
    let (mut g, _src, pano) = pano_graph();
    g.node_mut(pano).unwrap().set_param("group_by", id(0), 1.0); // Manual
    assert_eq!(lib.members(&g, pano), (1..=8).collect::<Vec<u64>>(), "no group numbers: every image alone");
    let n = g.node_mut(pano).unwrap();
    n.mode = Mode::Individual;
    for i in [2, 4, 6] {
        n.set_param("group", id(i), 11.0);
    }
    assert_eq!(lib.members(&g, pano), vec![1, 3, 4, 5, 7, 8], "group 11 = {{2,4,6}} (reference 4) listed at the position of its reference");
    g.node_mut(pano).unwrap().mode = Mode::All;
    g.node_mut(pano).unwrap().set_param("group_by", id(0), 2.0); // Whole collection
    g.node_mut(pano).unwrap().set_param("max_size", id(0), 25.0);
    assert_eq!(lib.members(&g, pano), vec![4]);
}

/// Four positions x three exposures (EV -2, 0, +2; 0.1 s apart), positions 3 s apart. Frames 1-3 = position 1, ...
fn hdr_pano_lib() -> Lib {
    let mut list = Vec::new();
    for p in 0..4u64 {
        for (k, shutter) in [0.0025f32, 0.01, 0.04].iter().enumerate() {
            list.push((p * 3 + k as u64 + 1, Some(p as i64 * 3_000 + k as i64 * 100), Some(*shutter)));
        }
    }
    Lib::new(&list)
}

fn chain_graph() -> (Graph, NodeId, NodeId, NodeId, NodeId) {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let hdr = g.add_node(NodeKind::HdrMerge, [0.0, 0.0]);
    let pano = g.add_node(NodeKind::Panorama, [0.0, 0.0]);
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    g.connect(src, hdr, 0).unwrap();
    g.connect(hdr, pano, 0).unwrap();
    g.connect(pano, exp, 0).unwrap();
    (g, src, hdr, pano, exp)
}

#[test]
fn a_panorama_behind_an_hdr_merge_groups_the_brackets() {
    let lib = hdr_pano_lib();
    let (g, src, hdr, pano, exp) = chain_graph();
    // HDR Merge: one reference (the 0 EV frame) per position.
    assert_eq!(lib.members(&g, hdr), vec![2, 5, 8, 11]);
    // Panorama: the HDR references form one sequence; its middle (lower) reference is the second.
    assert_eq!(lib.members(&g, pano), vec![5]);
    assert_eq!(lib.members(&g, exp), vec![5]);
    let grp = lib.with(|c| g.image_group(pano, id(5), c)).unwrap();
    assert_eq!(grp.members, vec![id(2), id(5), id(8), id(11)]);
    // The nearest group node of a node is the one closest to it.
    assert_eq!(g.upstream_group_node(exp), Some(pano));
    assert_eq!(g.upstream_group_node(pano), Some(pano));
    assert_eq!(g.upstream_group_node(hdr), Some(hdr));
    assert_eq!(g.upstream_group_node(src), None);
    assert_eq!(g.upstream_group_node_above(exp), Some(pano));
    assert_eq!(g.upstream_group_node_above(pano), Some(hdr));
    assert_eq!(g.upstream_group_node_above(hdr), None);
    assert!(g.has_upstream_kind(exp, NodeKind::Panorama) && g.has_upstream_kind(exp, NodeKind::HdrMerge));
    assert!(!g.has_upstream_kind(hdr, NodeKind::Panorama));
    // The panorama stands for 4 images and, with the brackets, for 12 frames.
    let counts = lib.with(|c| g.image_groups_with_frames(pano, c));
    assert_eq!(counts.len(), 1);
    assert_eq!((counts[0].0.members.len(), counts[0].1), (4, 12));
    // An HDR Merge alone has no nested groups: frames = images.
    let counts = lib.with(|c| g.image_groups_with_frames(hdr, c));
    assert!(counts.iter().all(|(grp, frames)| *frames == grp.members.len()));
}

#[test]
fn the_gap_runs_from_the_end_of_a_bracket_to_the_start_of_the_next() {
    // Bracket A: 1 s at 0, 2 s at 1.2 s, 4 s at 3.4 s (ends at 7.4 s); bracket B starts 1 s later.
    // The middle exposures start 8.4 s apart: only the span of the brackets keeps them in one panorama.
    let (e1, e2, e3) = (Some(1.0), Some(2.0), Some(4.0));
    let lib = Lib::new(&[
        (1, Some(0), e1),
        (2, Some(1_200), e2),
        (3, Some(3_400), e3),
        (4, Some(8_400), e1),
        (5, Some(9_600), e2),
        (6, Some(11_800), e3),
    ]);
    let (mut g, _src, hdr, pano, _exp) = chain_graph();
    g.node_mut(pano).unwrap().set_param("gap_s", id(0), 2.0);
    assert_eq!(lib.members(&g, hdr), vec![2, 5], "two brackets (the exposure repeats)");
    let groups = lib.with(|c| g.image_groups(pano, c));
    assert_eq!(groups.len(), 1, "{groups:?}");
    assert_eq!(groups[0].members, vec![id(2), id(5)]);
    // A real pause between the brackets still separates them.
    g.node_mut(pano).unwrap().set_param("gap_s", id(0), 0.5);
    assert_eq!(lib.members(&g, pano), vec![2, 5]);
}

#[test]
fn a_panorama_of_plain_frames_is_unchanged_by_an_hdr_merge_that_finds_no_brackets() {
    let lib = sweep_lib();
    let (g, _src, _hdr, pano, _exp) = chain_graph();
    // No bracket anywhere (one exposure): the HDR Merge passes every frame as its own group.
    assert_eq!(lib.members(&g, pano), vec![3, 7]);
    let counts = lib.with(|c| g.image_groups_with_frames(pano, c));
    assert_eq!(counts.iter().map(|(grp, f)| (grp.members.len(), *f)).collect::<Vec<_>>(), vec![(5, 5), (3, 3)]);
}

#[test]
fn old_capture_records_load_and_the_focal_length_round_trips() {
    let old = r#"{"make":"X","model":null,"lens":null,"iso":200.0,"shutter_s":null,"aperture":null,"flash_fired":null}"#;
    let c: CaptureInfo = serde_json::from_str(old).unwrap();
    assert_eq!((c.focal_mm, c.focal_35mm), (None, None));
    let with = CaptureInfo { focal_mm: Some(24.0), focal_35mm: Some(36.0), ..Default::default() };
    let back: CaptureInfo = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
    assert_eq!(back, with);
    assert!(!serde_json::to_string(&CaptureInfo::default()).unwrap().contains("focal"), "unset values stay out of the file");
    assert!(!c.focal_read, "an old record has not been read for the focal length");
}

#[test]
fn a_graph_with_a_panorama_round_trips() {
    let (g, ..) = chain_graph();
    let back = Graph::from_json(&g.to_json()).unwrap();
    assert_eq!(back.nodes().filter(|n| n.kind == NodeKind::Panorama).count(), 1);
    // Older builds do not know the kind and refuse the file instead of dropping the node.
    let unknown = g.to_json().replace("\"Panorama\"", "\"PanoramaFuture\"");
    assert!(Graph::from_json(&unknown).is_err());
}
