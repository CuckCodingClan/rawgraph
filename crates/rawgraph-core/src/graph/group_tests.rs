use super::*;
use crate::graph::{ImageId, Mode, PortType, GRAPH_FORMAT_VERSION, SUBGRAPH_FORMAT_VERSION};

const A: ImageId = ImageId(1);
const B: ImageId = ImageId(2);

/// Source -> Develop -> Exposure -> Contrast -> Saturation -> Output, with distinct values and an
/// Individual value on Exposure and Contrast.
#[allow(dead_code)]
struct Chain {
    g: Graph,
    src: NodeId,
    dev: NodeId,
    exp: NodeId,
    con: NodeId,
    sat: NodeId,
    out: NodeId,
}

fn chain() -> Chain {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let dev = g.add_node(NodeKind::Develop, [220.0, 0.0]);
    let exp = g.add_node(NodeKind::Exposure, [440.0, 10.0]);
    let con = g.add_node(NodeKind::Contrast, [660.0, 20.0]);
    let sat = g.add_node(NodeKind::Saturation, [880.0, 30.0]);
    let out = g.add_node(NodeKind::Output, [1100.0, 0.0]);
    for (a, b) in [(src, dev), (dev, exp), (exp, con), (con, sat), (sat, out)] {
        g.connect(a, b, 0).unwrap();
    }
    g.node_mut(exp).unwrap().set_param("ev", A, 0.75);
    g.node_mut(con).unwrap().set_param("amount", A, 0.4);
    g.node_mut(exp).unwrap().mode = Mode::Individual;
    g.node_mut(exp).unwrap().set_param("ev", B, -1.25);
    Chain { g, src, dev, exp, con, sat, out }
}

/// Same real nodes and wires (the id counter may have moved on: ids are never reused).
fn same_content(a: &Graph, b: &Graph) -> bool {
    a.nodes().filter(|n| n.kind != NodeKind::Group).eq(b.nodes().filter(|n| n.kind != NodeKind::Group)) && a.edges() == b.edges()
}

fn group_of(g: &Graph, id: NodeId) -> &GroupDef {
    g.node(id).unwrap().group().unwrap()
}

#[test]
fn group_and_ungroup_restore_everything_exactly() {
    let c = chain();
    let before = c.g.clone();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    assert_ne!(g, before);
    assert_eq!(g.node(grp).unwrap().kind, NodeKind::Group);
    // Nothing of the inner nodes changed: ids, positions, values, Individual values, wires.
    for id in [c.exp, c.con] {
        assert_eq!(g.node(id), before.node(id));
    }
    assert_eq!(g.edges(), before.edges());
    let members = g.ungroup(grp).unwrap();
    assert_eq!(members, vec![c.exp, c.con]);
    assert!(same_content(&g, &before), "a group followed by an ungroup is the identical graph");
    assert!(g.nodes().eq(before.nodes()));
    assert_eq!(g.node(c.exp).unwrap().param("ev", B), -1.25, "Individual data is kept");
    assert!(!g.has_groups());
}

#[test]
fn a_chain_in_the_middle_gets_one_input_and_one_output() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    let d = group_of(&g, grp);
    assert_eq!(d.label, "Tone");
    assert_eq!(d.members, vec![c.exp, c.con]);
    assert_eq!(d.inputs.len(), 1);
    assert_eq!((d.inputs[0].node, d.inputs[0].port), (c.exp, 0));
    assert_eq!(d.inputs[0].label, "Exposure: Images");
    assert_eq!(d.outputs.len(), 1);
    assert_eq!((d.outputs[0].node, d.outputs[0].port), (c.con, 0));
    assert_eq!(d.outputs[0].label, "Contrast: Images", "labels come from the inner port names");
    // The inner wire stays inside, the two crossing wires are the ports.
    let level = g.level_edges(None);
    assert_eq!(level.len(), 4, "src-dev, dev-group, group-sat, sat-out");
    assert!(level.iter().any(|e| e.from == End::Node { node: c.dev, port: 0 } && e.to == End::Node { node: grp, port: 0 }));
    assert!(level.iter().any(|e| e.from == End::Node { node: grp, port: 0 } && e.to == End::Node { node: c.sat, port: 0 }));
    assert!(!level.iter().any(|e| e.edge.from == c.exp && e.edge.to == c.con), "the inner wire is hidden");
    assert_eq!(g.children(None).len(), 5);
    assert_eq!(g.children(Some(grp)), vec![c.exp, c.con]);
}

#[test]
fn boundary_ports_for_fan_in_fan_out_control_inputs_and_split_outputs() {
    let mut g = Graph::new();
    let s1 = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let s2 = g.add_node(NodeKind::Source, [0.0, 100.0]);
    let ctl = g.add_node(NodeKind::Source, [0.0, 200.0]);
    let merge = g.add_node(NodeKind::Merge, [300.0, 0.0]);
    let split = g.add_node(NodeKind::MetadataSplit, [500.0, 0.0]);
    let exp = g.add_node(NodeKind::Exposure, [300.0, 200.0]);
    let o1 = g.add_node(NodeKind::Output, [800.0, 0.0]);
    let o2 = g.add_node(NodeKind::Output, [800.0, 100.0]);
    let o3 = g.add_node(NodeKind::Output, [800.0, 200.0]);
    let o4 = g.add_node(NodeKind::Output, [800.0, 300.0]);
    g.connect_ports(s1, 0, merge, 0).unwrap();
    g.connect_ports(s2, 0, merge, 1).unwrap();
    g.connect(merge, split, 0).unwrap();
    // Control input: the Exposure amount is driven by the image from `ctl`.
    let ev_port = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
    g.connect_ports(ctl, 0, exp, ev_port).unwrap();
    g.connect(s1, exp, 0).unwrap();
    // Split ports 0 and 2 feed outside; port 0 feeds two consumers (fan-out).
    g.connect_ports(split, 0, o1, 0).unwrap();
    g.connect_ports(split, 0, o2, 0).unwrap();
    g.connect_ports(split, 2, o3, 0).unwrap();
    g.connect(exp, o4, 0).unwrap();
    let before = g.clone();

    let grp = g.group_nodes(&[merge, split, exp], "Mix").unwrap();
    let d = group_of(&g, grp);
    let inputs: Vec<(NodeId, usize)> = d.inputs.iter().map(|p| (p.node, p.port)).collect();
    // Fan-in on Merge input 0 and 1 (two ports), Exposure image input and its control input; s1 feeds two inner inputs.
    assert_eq!(inputs.len(), 4, "{inputs:?}");
    assert!(inputs.contains(&(merge, 0)) && inputs.contains(&(merge, 1)));
    assert!(inputs.contains(&(exp, 0)) && inputs.contains(&(exp, ev_port)), "control inputs are ports too");
    let outputs: Vec<(NodeId, usize)> = d.outputs.iter().map(|p| (p.node, p.port)).collect();
    assert_eq!(outputs.len(), 3, "{outputs:?}");
    assert!(outputs.contains(&(split, 0)) && outputs.contains(&(split, 2)) && outputs.contains(&(exp, 0)));
    // The fan-out of split port 0 is one port with two wires.
    let level = g.level_edges(None);
    let from_p0 = d.outputs.iter().position(|p| (p.node, p.port) == (split, 0)).unwrap();
    assert_eq!(level.iter().filter(|e| e.from == End::Node { node: grp, port: from_p0 }).count(), 2);
    assert_eq!(level.len(), before.edges().len() - 1, "only the Merge-to-Split wire is hidden inside");
    // Labels are unique and readable.
    let labels: BTreeSet<&str> = d.inputs.iter().map(|p| p.label.as_str()).collect();
    assert_eq!(labels.len(), 4);
    assert!(d.outputs.iter().any(|p| p.label.starts_with("Split by Metadata")));
    // Everything crossing has a port: the structure is valid and ungrouping gives the original back.
    g.check_groups().unwrap();
    g.ungroup(grp).unwrap();
    assert!(same_content(&g, &before));
}

#[test]
fn unconnected_required_inputs_and_dangling_outputs_get_ports() {
    let mut g = Graph::new();
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let con = g.add_node(NodeKind::Contrast, [200.0, 0.0]);
    g.connect(exp, con, 0).unwrap();
    let grp = g.group_nodes(&[exp, con], "Pair").unwrap();
    let d = group_of(&g, grp);
    assert_eq!(d.inputs.len(), 1, "the open image input of Exposure");
    assert_eq!(d.outputs.len(), 1, "the end of the chain is the output, so the group can be viewed");
    assert_eq!(g.view_target(grp), Some((con, 0)));
    assert_eq!(g.view_target(exp), Some((exp, 0)));
}

#[test]
fn wires_through_group_ports_are_real_wires_between_the_inner_nodes() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    let extra = g.add_node(NodeKind::Source, [0.0, 300.0]);
    // Re-wire the group input to another source through the group port.
    g.connect_ports(extra, 0, grp, 0).unwrap();
    assert!(g.input_edges(grp, 0).any(|e| e.from == extra && e.to == c.exp), "a group input answers for the inner input");
    assert_eq!(g.input_edges(c.exp, 0).count(), 2, "the Exposure input takes several wires: the old one stays");
    g.disconnect(grp, 0);
    assert!(g.input_edge(c.exp, 0).is_none());
    // Wiring from a group output.
    let o = g.add_node(NodeKind::Output, [1200.0, 100.0]);
    g.connect_ports(grp, 0, o, 0).unwrap();
    assert_eq!(g.input_edge(o, 0), Some((c.con, 0)));
    // Bad ports are errors, never panics.
    assert!(matches!(g.connect_ports(grp, 5, o, 0), Err(GraphError::NoSuchPort { .. })));
    assert!(matches!(g.connect_ports(extra, 0, grp, 7), Err(GraphError::NoSuchPort { .. })));
    g.check_groups().unwrap();
}

#[test]
fn connecting_real_nodes_across_a_boundary_adds_the_missing_ports() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    let extra = g.add_node(NodeKind::Source, [0.0, 300.0]);
    let o = g.add_node(NodeKind::Output, [1200.0, 100.0]);
    let (inputs, outputs) = (group_of(&g, grp).inputs.len(), group_of(&g, grp).outputs.len());
    g.connect(extra, c.con, 0).unwrap();
    g.connect(c.exp, o, 0).unwrap();
    assert_eq!(group_of(&g, grp).inputs.len(), inputs + 1);
    assert_eq!(group_of(&g, grp).outputs.len(), outputs + 1);
    g.check_groups().unwrap();
}

#[test]
fn nested_groups_ports_and_ungrouping_in_either_order() {
    let c = chain();
    let before = c.g.clone();
    let mut g = c.g;
    let inner = g.group_nodes(&[c.exp, c.con], "Inner").unwrap();
    let outer = g.group_nodes(&[c.dev, inner, c.sat], "Outer").unwrap();
    g.check_groups().unwrap();
    assert_eq!(g.parent_of(inner), Some(outer));
    assert_eq!(g.parent_of(c.exp), Some(inner));
    assert_eq!(g.ancestors(c.exp), vec![inner, outer]);
    assert_eq!(g.leaves_of(outer), vec![c.dev, c.exp, c.con, c.sat]);
    assert_eq!(group_of(&g, outer).inputs.len(), 1, "the source feeds Develop");
    assert_eq!(group_of(&g, outer).outputs.len(), 1, "Saturation feeds the output");
    assert_eq!(g.children(None).len(), 3, "source, outer, output");
    assert_eq!(g.children(Some(outer)).len(), 3);
    // The outer level shows the inner group as one box with a port in and out.
    let level = g.level_edges(Some(outer));
    assert!(level.iter().any(|e| e.from == End::Node { node: c.dev, port: 0 } && e.to == End::Node { node: inner, port: 0 }));
    assert!(level.iter().any(|e| e.from == End::Boundary { port: 0 }), "the stub of the open group");
    assert!(level.iter().any(|e| e.to == End::Boundary { port: 0 }));
    // Ungroup the outer first, then the inner.
    let mut a = g.clone();
    a.ungroup(outer).unwrap();
    assert_eq!(a.parent_of(inner), None);
    assert_eq!(a.parent_of(c.exp), Some(inner));
    a.ungroup(inner).unwrap();
    assert!(a.nodes().eq(before.nodes()) && a.edges() == before.edges());
    // Ungroup the inner first, then the outer.
    let mut b = g.clone();
    b.ungroup(inner).unwrap();
    assert_eq!(b.parent_of(c.exp), Some(outer));
    b.check_groups().unwrap();
    b.ungroup(outer).unwrap();
    assert!(b.nodes().eq(before.nodes()) && b.edges() == before.edges(), "same nodes and wires (only the id counter moved on)");
    // Deleting the outer group removes everything inside it.
    g.remove_node(outer);
    assert_eq!(g.nodes().count(), 2);
    assert!(g.edges().is_empty());
}

#[test]
fn nodes_of_different_groups_and_deep_nesting_are_refused() {
    let c = chain();
    let mut g = c.g;
    let inner = g.group_nodes(&[c.exp, c.con], "Inner").unwrap();
    assert!(matches!(g.group_nodes(&[c.exp, c.dev], "x"), Err(GraphError::Group(_))), "one inside, one outside");
    assert!(g.group_nodes(&[], "x").is_err());
    assert!(g.group_nodes(&[NodeId(999)], "x").is_err());
    let mut top = inner;
    for i in 0..MAX_GROUP_DEPTH - 1 {
        top = g.group_nodes(&[top], &format!("level {i}")).unwrap();
    }
    assert!(g.group_nodes(&[top], "too deep").is_err());
    g.check_groups().unwrap();
}

#[test]
fn publishing_and_unpublishing_parameters() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    let i = g.publish_param(grp, c.exp, "ev", None).unwrap();
    assert_eq!(group_of(&g, grp).published[i].label, "Exposure");
    let j = g.publish_param(grp, c.con, "amount", Some("Punch")).unwrap();
    assert_eq!(group_of(&g, grp).published[j].label, "Punch");
    assert!(g.publish_param(grp, c.exp, "ev", None).is_err(), "twice");
    assert!(g.publish_param(grp, c.exp, "nonsense", None).is_err());
    assert!(g.publish_param(grp, c.sat, "amount", None).is_err(), "not inside the group");
    assert!(g.publish_param(c.exp, c.exp, "ev", None).is_err(), "not a group");
    assert_eq!(g.published_index(grp, c.con, "amount"), Some(j));
    g.rename_published(grp, j, "Contrast boost").unwrap();
    assert_eq!(group_of(&g, grp).published[j].label, "Contrast boost");
    assert!(g.rename_published(grp, j, "  ").is_err());
    g.unpublish_param(grp, i).unwrap();
    assert_eq!(group_of(&g, grp).published.len(), 1);
    assert!(g.unpublish_param(grp, 5).is_err());
    // The published value is the inner value itself.
    g.node_mut(c.con).unwrap().set_param("amount", A, 0.9);
    assert_eq!(g.node(c.con).unwrap().param("amount", A), 0.9);
    g.check_groups().unwrap();
}

#[test]
fn published_parameters_survive_grouping_and_ungrouping_around_them() {
    let c = chain();
    let mut g = c.g;
    let inner = g.group_nodes(&[c.exp, c.con], "Inner").unwrap();
    g.publish_param(inner, c.exp, "ev", Some("Brightness")).unwrap();
    let published = group_of(&g, inner).published.clone();
    let outer = g.group_nodes(&[inner, c.sat], "Outer").unwrap();
    assert_eq!(group_of(&g, inner).published, published);
    g.publish_param(outer, c.sat, "amount", None).unwrap();
    g.ungroup(outer).unwrap();
    assert_eq!(group_of(&g, inner).published, published, "the nested group keeps what it published");
    g.check_groups().unwrap();
}

#[test]
fn removing_inner_nodes_cleans_ports_and_published_entries() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    g.publish_param(grp, c.con, "amount", None).unwrap();
    g.remove_node(c.con);
    let d = group_of(&g, grp);
    assert_eq!(d.members, vec![c.exp]);
    assert!(d.outputs.is_empty() && d.published.is_empty());
    g.check_groups().unwrap();
    g.remove_node(c.exp);
    assert!(g.node(grp).is_none(), "a group without members disappears");
    assert!(!g.has_groups());
}

#[test]
fn group_ports_can_be_exposed_renamed_and_removed() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    let k = g.expose_group_port(grp, true, c.con, 0).unwrap();
    assert_eq!(k, 1);
    assert_eq!(g.expose_group_port(grp, true, c.con, 0).unwrap(), 1, "no duplicate");
    assert!(g.expose_group_port(grp, true, c.sat, 0).is_err(), "outside the group");
    assert!(g.expose_group_port(grp, true, c.con, 9).is_err());
    g.set_group_port_label(grp, true, k, "Boost source").unwrap();
    assert_eq!(group_of(&g, grp).inputs[k].label, "Boost source");
    let extra = g.add_node(NodeKind::Source, [0.0, 400.0]);
    g.connect_ports(extra, 0, grp, k).unwrap();
    assert_eq!(g.edges().len(), 6);
    g.remove_group_port(grp, true, k).unwrap();
    assert_eq!(g.edges().len(), 5, "the wire through the removed port goes too");
    g.remove_group_port(grp, true, 0).unwrap();
    assert!(g.input_of(c.exp, 0).is_none());
    g.set_group_label(grp, "Renamed").unwrap();
    g.set_group_color(grp, Some([10, 20, 30])).unwrap();
    assert_eq!(group_of(&g, grp).label, "Renamed");
    assert_eq!(group_of(&g, grp).color, Some([10, 20, 30]));
    assert!(g.set_group_label(grp, "").is_err());
    assert!(g.set_group_label(c.exp, "x").is_err());
}

#[test]
fn adopting_new_nodes_into_an_open_group() {
    let c = chain();
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    let n = g.add_node(NodeKind::Blur, [500.0, 300.0]);
    assert!(g.adopt(grp, &[n, grp]).is_err(), "a group cannot adopt itself");
    g.adopt(grp, &[n]).unwrap();
    assert_eq!(group_of(&g, grp).members, vec![c.exp, c.con, n]);
    // Adopting a wired node adds the ports its wires now need.
    g.adopt(grp, &[c.dev]).unwrap();
    assert_eq!(g.parent_of(c.dev), Some(grp));
    g.check_groups().unwrap();
    let inner = g.group_nodes(&[c.sat], "Other").unwrap();
    g.adopt(grp, &[c.exp]).unwrap();
    assert_eq!(g.parent_of(c.exp), Some(grp), "nodes that already live in this group stay");
    g.adopt(inner, &[c.exp]).unwrap();
    assert_eq!(g.parent_of(c.exp), Some(grp), "a node of another group is not stolen");
    assert_eq!(g.top_level_of(&[n, grp, c.exp]), vec![grp]);
}

#[test]
fn json_round_trip_keeps_groups_and_writes_format_6() {
    let c = chain();
    let mut g = c.g;
    let inner = g.group_nodes(&[c.exp, c.con], "Inner").unwrap();
    g.publish_param(inner, c.exp, "ev", Some("Brightness")).unwrap();
    g.set_group_color(inner, Some([1, 2, 3])).unwrap();
    let outer = g.group_nodes(&[inner, c.dev], "Outer").unwrap();
    let text = g.to_json();
    assert!(text.contains(&format!("\"format_version\": {GRAPH_FORMAT_VERSION}")));
        let back = Graph::from_json(&text).unwrap();
    assert_eq!(back, g);
    assert_eq!(group_of(&back, outer).members, vec![inner, c.dev]);
    assert_eq!(back.node(c.exp).unwrap().param("ev", B), -1.25);
}

#[test]
fn format_5_files_load_unchanged_and_are_relabelled() {
    let c = chain();
    let mut doc: serde_json::Value = serde_json::from_str(&c.g.to_json()).unwrap();
    doc["format_version"] = (GRAPH_FORMAT_VERSION - 1).into();
    let (g, notes) = Graph::from_json_with_notes(&doc.to_string()).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(g, c.g, "same graph; the file is saved as format 6 from now on");
    assert!(g.to_json().contains(&format!("\"format_version\": {GRAPH_FORMAT_VERSION}")));
    // A format-5 file with Develop nodes must not get the legacy-tone step a second time.
    assert!(!g.to_json().contains("legacy_tone"));
}

#[test]
fn newer_formats_are_refused_with_the_version_message_even_when_they_hold_unknown_kinds() {
    let c = chain();
    let mut doc: serde_json::Value = serde_json::from_str(&c.g.to_json()).unwrap();
    doc["format_version"] = (GRAPH_FORMAT_VERSION + 1).into();
    doc["nodes"]["1"]["kind"] = "SomethingNew".into();
    let err = Graph::from_json(&doc.to_string()).unwrap_err();
    assert_eq!(err, GraphError::UnsupportedVersion(GRAPH_FORMAT_VERSION + 1));
    assert!(err.to_string().contains("newer than this build supports"));
}

fn grouped() -> (Graph, NodeId, Chain2) {
    let c = chain();
    let ids = Chain2 { exp: c.exp, con: c.con, sat: c.sat };
    let mut g = c.g;
    let grp = g.group_nodes(&[c.exp, c.con], "Tone").unwrap();
    (g, grp, ids)
}

struct Chain2 {
    exp: NodeId,
    con: NodeId,
    sat: NodeId,
}

fn rejects(edit: impl FnOnce(&mut serde_json::Value, NodeId, &Chain2), needle: &str) {
    let (g, grp, ids) = grouped();
    let mut doc: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
    edit(&mut doc, grp, &ids);
    let err = Graph::from_json(&doc.to_string()).expect_err(needle).to_string();
    assert!(err.contains(needle), "expected \"{needle}\" in \"{err}\"");
}

fn gnode(doc: &mut serde_json::Value, id: NodeId) -> &mut serde_json::Value {
    &mut doc["nodes"][id.0.to_string()]
}

#[test]
fn invalid_group_structures_are_rejected_on_load() {
    let (g, _, _) = grouped();
    Graph::from_json(&g.to_json()).unwrap();
    rejects(|d, grp, ids| gnode(d, grp)["group"]["inputs"][0]["node"] = (ids.sat.0).into(), "does not point at a node inside");
    rejects(|d, grp, _| gnode(d, grp)["group"]["inputs"][0]["node"] = 999.into(), "missing node");
    rejects(|d, grp, _| gnode(d, grp)["group"]["inputs"][0]["port"] = 9.into(), "port that does not exist");
    rejects(|d, grp, _| gnode(d, grp)["group"]["outputs"] = serde_json::json!([]), "no output port");
    rejects(|d, grp, _| gnode(d, grp)["group"]["inputs"] = serde_json::json!([]), "no input port");
    rejects(|d, grp, ids| { let v = gnode(d, grp)["group"]["inputs"][0].clone(); gnode(d, grp)["group"]["inputs"].as_array_mut().unwrap().push(v); let _ = ids; }, "same inner port");
    rejects(|d, grp, _| gnode(d, grp)["group"]["members"] = serde_json::json!([]), "no members");
    rejects(|d, grp, _| gnode(d, grp)["group"]["members"].as_array_mut().unwrap().push(777.into()), "missing node 777");
    rejects(|d, grp, ids| gnode(d, grp)["group"]["members"].as_array_mut().unwrap().push(ids.exp.0.into()), "twice");
    rejects(|d, grp, _| gnode(d, grp)["group"]["members"].as_array_mut().unwrap().push(grp.0.into()), "contains itself");
    rejects(|d, grp, _| gnode(d, grp)["group"]["label"] = "".into(), "name is empty");
    rejects(|d, grp, ids| gnode(d, grp)["group"]["published"] = serde_json::json!([{"label": "x", "node": ids.sat.0, "param": "amount"}]), "published parameter");
    rejects(|d, grp, ids| gnode(d, grp)["group"]["published"] = serde_json::json!([{"label": "x", "node": ids.exp.0, "param": "nope"}]), "published parameter");
    rejects(|d, grp, ids| { let p = serde_json::json!({"label": "x", "node": ids.exp.0, "param": "ev"}); gnode(d, grp)["group"]["published"] = serde_json::json!([p.clone(), p]); }, "published twice");
    rejects(|d, grp, _| { gnode(d, grp).as_object_mut().unwrap().remove("group"); }, "group definition");
    rejects(|d, _, ids| gnode(d, ids.sat)["group"] = serde_json::json!({"label": "x", "members": [], "inputs": [], "outputs": []}), "group definition");
    // A wire to a group node.
    rejects(|d, grp, ids| d["edges"].as_array_mut().unwrap().push(serde_json::json!({"from": ids.sat.0, "to": grp.0, "to_port": 0})), "never a group node");
}

#[test]
fn groups_that_contain_each_other_or_share_nodes_are_rejected() {
    let c = chain();
    let mut g = c.g;
    let a = g.group_nodes(&[c.exp], "A").unwrap();
    let b = g.group_nodes(&[c.con], "B").unwrap();
    let doc: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
    // a contains b and b contains a.
    let mut cyc = doc.clone();
    cyc["nodes"][a.0.to_string()]["group"]["members"].as_array_mut().unwrap().push(b.0.into());
    cyc["nodes"][b.0.to_string()]["group"]["members"].as_array_mut().unwrap().push(a.0.into());
    let err = Graph::from_json(&cyc.to_string()).unwrap_err().to_string();
    assert!(err.contains("nested deeper") || err.contains("inside both"), "{err}");
    // A node in two groups.
    let mut shared = doc.clone();
    shared["nodes"][b.0.to_string()]["group"]["members"].as_array_mut().unwrap().push(c.exp.0.into());
    let err = Graph::from_json(&shared.to_string()).unwrap_err().to_string();
    assert!(err.contains("inside both"), "{err}");
    // Self containing chain a -> b -> a only through members of members.
    let mut loop3 = doc.clone();
    loop3["nodes"][a.0.to_string()]["group"]["members"] = serde_json::json!([b.0]);
    loop3["nodes"][b.0.to_string()]["group"]["members"] = serde_json::json!([a.0]);
    assert!(Graph::from_json(&loop3.to_string()).is_err());
}

#[test]
fn flat_wire_cycles_through_groups_are_still_cycles() {
    let (g, _, ids) = grouped();
    let mut doc: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
    // Saturation back into Exposure closes a loop that passes through the group boundary twice.
    doc["edges"].as_array_mut().unwrap().push(serde_json::json!({"from": ids.sat.0, "to": ids.exp.0, "to_port": 0}));
    let err = Graph::from_json(&doc.to_string()).unwrap_err().to_string();
    assert!(err.contains("cycle"), "{err}");
}

#[test]
fn copying_a_group_copies_everything_inside_it_with_fresh_ids() {
    let (mut g, grp, ids) = grouped();
    g.publish_param(grp, ids.con, "amount", Some("Punch")).unwrap();
    let sub = g.extract_subgraph(&[grp]);
    assert_eq!(sub.node_count(), 3, "the group and both inner nodes");
    let text = sub.to_json();
    assert!(text.contains(&format!("\"format_version\": {SUBGRAPH_FORMAT_VERSION}")));
    let sub = Subgraph::from_json(&text).unwrap();
    let before = g.nodes().count();
    let new = g.insert_subgraph(&sub, [50.0, 50.0]);
    assert_eq!(new.len(), 3);
    assert_eq!(g.nodes().count(), before + 3);
    let top = g.top_level_of(&new);
    assert_eq!(top.len(), 1);
    let copy = group_of(&g, top[0]);
    assert_eq!(copy.label, "Tone");
    assert_eq!(copy.published.len(), 1);
    assert!(copy.members.iter().all(|m| new.contains(m)), "fresh ids");
    assert!(copy.members.iter().all(|m| !group_of(&g, grp).members.contains(m)));
    assert_eq!(copy.inputs.len(), 1);
    assert_eq!(g.input_edges(top[0], 0).count(), 0, "pasted unconnected");
    // Per-image values stay behind, the All values come along.
    let inner = copy.members[0];
    assert_eq!(g.node(inner).unwrap().param("ev", A), 0.75);
    assert_eq!(g.node(inner).unwrap().mode, Mode::Individual);
    g.check_groups().unwrap();
    // Copying only an inner node gives a plain node.
    assert_eq!(g.extract_subgraph(&[ids.con]).node_count(), 1);
}

#[test]
fn old_subgraph_files_still_load() {
    let (g, _, ids) = grouped();
    let sub = g.extract_subgraph(&[ids.sat]);
    let mut doc: serde_json::Value = serde_json::from_str(&sub.to_json()).unwrap();
    doc["format_version"] = (SUBGRAPH_FORMAT_VERSION - 1).into();
    let (s, notes) = Subgraph::from_json_with_notes(&doc.to_string()).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(s.node_count(), 1);
    doc["format_version"] = (SUBGRAPH_FORMAT_VERSION + 1).into();
    assert_eq!(Subgraph::from_json(&doc.to_string()).unwrap_err(), GraphError::UnsupportedSubgraphVersion(SUBGRAPH_FORMAT_VERSION + 1));
}

#[test]
fn macros_save_load_and_insert_independent_copies() {
    let (mut g, grp, ids) = grouped();
    g.publish_param(grp, ids.exp, "ev", Some("Brightness")).unwrap();
    g.set_group_color(grp, Some([200, 100, 50])).unwrap();
    let m = g.extract_macro(grp, "Warm look", "Exposure and contrast").unwrap();
    assert_eq!(m.input_labels(), vec!["Exposure: Images"]);
    assert_eq!(m.output_labels(), vec!["Contrast: Images"]);
    assert_eq!(m.published_labels(), vec!["Brightness"]);
    assert_eq!(m.node_count(), 3);
    let text = m.to_json();
    assert!(text.contains("\"format_version\": 1"));
    let back = MacroFile::from_json(&text).unwrap();
    assert_eq!(back, m);

    let mut target = Graph::new();
    let first = target.insert_macro(&back, [100.0, 40.0]).unwrap();
    let second = target.insert_macro(&back, [100.0, 400.0]).unwrap();
    assert_ne!(first, second);
    assert_eq!(target.node(first).unwrap().pos, [100.0, 40.0]);
    let d = group_of(&target, first);
    assert_eq!(d.origin.as_deref(), Some("Warm look"));
    assert_eq!((d.label.as_str(), d.color, d.inputs.len(), d.outputs.len(), d.published.len()), ("Tone", Some([200, 100, 50]), 1, 1, 1));
    assert_eq!(target.nodes().count(), 6);
    target.check_groups().unwrap();
    // Editing one copy touches neither the other copy nor the macro file.
    let inner = d.members[0];
    target.node_mut(inner).unwrap().set_param("ev", A, 2.0);
    let other = group_of(&target, second).members[0];
    assert_eq!(target.node(other).unwrap().param("ev", A), 0.75);
    assert_eq!(back.to_json(), text);
    // The copy is a real group: viewing it, grouping it again and ungrouping all work.
    assert!(target.view_target(first).is_some());
    let wrapped = target.group_nodes(&[first], "Wrapper").unwrap();
    target.ungroup(wrapped).unwrap();
    // The original graph never learns about the copies.
    assert_eq!(g.nodes().count(), 7);
}

#[test]
fn invalid_macros_are_rejected() {
    let (g, grp, ids) = grouped();
    let m = g.extract_macro(grp, "Tone", "").unwrap();
    let doc: serde_json::Value = serde_json::from_str(&m.to_json()).unwrap();
    let mut newer = doc.clone();
    newer["format_version"] = 2.into();
    assert_eq!(MacroFile::from_json(&newer.to_string()).unwrap_err(), GraphError::UnsupportedVersion(2));
    let mut nameless = doc.clone();
    nameless["name"] = " ".into();
    assert!(MacroFile::from_json(&nameless.to_string()).is_err());
    // The first node must be the group.
    let mut swapped = doc.clone();
    let nodes = swapped["group"]["nodes"].as_array_mut().unwrap();
    nodes.swap(0, 1);
    assert!(MacroFile::from_json(&swapped.to_string()).is_err());
    let mut dangling = doc.clone();
    dangling["group"]["nodes"][0]["group"]["inputs"][0]["node"] = 42.into();
    assert!(MacroFile::from_json(&dangling.to_string()).is_err());
    assert!(MacroFile::from_json("not json").is_err());
    assert!(g.extract_macro(ids.exp, "x", "").is_err(), "only a group can be a macro");
    assert!(g.extract_macro(grp, "", "").is_err());
}

#[test]
fn viewing_a_group_means_viewing_the_node_behind_its_first_output() {
    let (mut g, grp, ids) = grouped();
    assert_eq!(g.view_target(grp), Some((ids.con, 0)));
    g.remove_group_port(grp, false, 0).unwrap();
    assert_eq!(g.view_target(grp), None, "a group without outputs shows nothing");
    assert_eq!(g.view_target(NodeId(999)), None);
}

#[test]
fn validation_issues_of_inner_nodes_are_found_by_the_flat_graph() {
    let mut g = Graph::new();
    let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let con = g.add_node(NodeKind::Contrast, [200.0, 0.0]);
    g.connect(exp, con, 0).unwrap();
    let before = g.validate();
    let grp = g.group_nodes(&[exp, con], "Pair").unwrap();
    let after = g.validate();
    assert_eq!(before, after, "grouping neither adds nor hides an issue");
    assert_eq!(after.len(), 1);
    assert_eq!(g.visible_at(None, after[0].node), Some(grp), "the app shows it on the group");
}

// ---------------------------------------------------------------- fix round 1

fn flat_wires_and_nodes(g: &Graph) -> (Vec<Node>, Vec<Edge>) {
    (g.nodes().filter(|n| n.kind != NodeKind::Group).cloned().collect(), g.edges().to_vec())
}

#[test]
fn removing_an_exposed_port_drops_the_group_ports_that_stand_for_it() {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let mask = g.add_node(NodeKind::Generator, [0.0, 100.0]);
    let blur = g.add_node(NodeKind::Blur, [200.0, 0.0]);
    let out = g.add_node(NodeKind::Output, [400.0, 0.0]);
    g.connect(src, blur, 0).unwrap();
    let radius = g.expose_parameter(blur, "radius", PortType::Images).unwrap();
    g.connect(mask, blur, radius).unwrap();
    g.connect(blur, out, 0).unwrap();
    let alias = g.expose_output(blur, 0, "Copy").unwrap();
    let grp = g.group_nodes(&[blur], "Blurring").unwrap();
    g.expose_group_port(grp, false, blur, alias).unwrap();
    assert!(group_of(&g, grp).inputs.iter().any(|p| p.port == radius));
    assert!(group_of(&g, grp).outputs.iter().any(|p| p.port == alias));
    assert!(g.remove_exposed_input(blur, radius));
    assert!(g.remove_exposed_output(blur, alias));
    assert!(!group_of(&g, grp).inputs.iter().any(|p| p.port == radius));
    assert!(!group_of(&g, grp).outputs.iter().any(|p| p.port == alias));
    g.check_groups().unwrap();
    // The saved file opens again.
    Graph::from_json(&g.to_json()).unwrap();
}

/// Every mutation of the graph API keeps the group structure valid.
#[test]
fn every_graph_mutation_keeps_the_group_rules() {
    let c = chain();
    let mut g = c.g;
    let ok = |g: &Graph, what: &str| {
        if let Err(e) = g.check_groups() {
            panic!("{what} broke the group rules: {e}");
        }
        Graph::from_json(&g.to_json()).unwrap_or_else(|e| panic!("{what}: the saved graph does not load: {e}"));
    };
    let inner = g.group_nodes(&[c.exp, c.con], "Inner").unwrap();
    ok(&g, "group");
    let blur = g.add_node(NodeKind::Blur, [500.0, 300.0]);
    g.adopt(inner, &[blur]).unwrap();
    ok(&g, "adopt");
    let p = g.expose_parameter(blur, "radius", PortType::Images).unwrap();
    let a = g.expose_output(blur, 0, "Alias").unwrap();
    ok(&g, "expose");
    g.expose_group_port(inner, true, blur, p).unwrap();
    g.expose_group_port(inner, false, blur, a).unwrap();
    ok(&g, "expose group ports");
    g.publish_param(inner, blur, "radius", None).unwrap();
    let outer = g.group_nodes(&[inner, c.sat], "Outer").unwrap();
    ok(&g, "nest");
    g.disconnect(c.sat, 0);
    ok(&g, "disconnect");
    g.connect(c.dev, c.sat, 0).unwrap();
    ok(&g, "connect across the boundary");
    g.disconnect_edge(g.edges()[0]);
    ok(&g, "disconnect_edge");
    assert!(g.remove_exposed_input(blur, p));
    ok(&g, "remove exposed input");
    assert!(g.remove_exposed_output(blur, a));
    ok(&g, "remove exposed output");
    if !group_of(&g, inner).inputs.is_empty() {
        g.remove_group_port(inner, true, 0).unwrap();
        ok(&g, "remove group port");
    }
    g.unpublish_param(inner, 0).unwrap();
    ok(&g, "unpublish");
    let wrapped = g.group_nodes(&[outer], "Wrapped").unwrap();
    ok(&g, "wrap");
    g.ungroup(outer).unwrap();
    ok(&g, "ungroup");
    g.remove_node(c.con);
    ok(&g, "remove inner node");
    let sub = g.extract_subgraph(&[wrapped]);
    let ids = g.insert_subgraph(&sub, [10.0, 10.0]);
    ok(&g, "insert_subgraph");
    g.remove_node(ids[0]);
    ok(&g, "remove copy");
    g.remove_node(wrapped);
    ok(&g, "remove group");
}

fn damaged(edit: impl FnOnce(&mut serde_json::Value, NodeId, &Chain2)) -> (Graph, Vec<String>, Graph) {
    let (g, grp, ids) = grouped();
    let mut doc: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
    edit(&mut doc, grp, &ids);
    let text = doc.to_string();
    assert!(Graph::from_json(&text).is_err(), "the strict loader still refuses it");
    let (fixed, notes) = Graph::from_json_repaired(&text).unwrap_or_else(|e| panic!("repair failed: {e}"));
    fixed.check_groups().unwrap();
    assert!(!notes.is_empty(), "every repair says so");
    (fixed, notes, g)
}

#[test]
fn damaged_groups_in_project_files_are_repaired_not_refused() {
    // Dangling and duplicate ports, members and published entries.
    let (f, notes, orig) = damaged(|d, grp, ids| {
        let g = &mut gnode(d, grp)["group"];
        g["inputs"][0]["port"] = 9.into();
        g["members"].as_array_mut().unwrap().extend([serde_json::json!(777), ids.exp.0.into(), grp.0.into()]);
        g["published"] = serde_json::json!([{"label": "x", "node": ids.sat.0, "param": "amount"}, {"label": "y", "node": ids.exp.0, "param": "nope"}]);
    });
    assert!(notes.iter().any(|n| n.contains("dropped invalid members")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("ports")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("published")), "{notes:?}");
    assert_eq!(flat_wires_and_nodes(&f), flat_wires_and_nodes(&orig), "nothing evaluates differently");
    assert_eq!(f.validate(), orig.validate());
    let (gid, d) = f.nodes().find_map(|n| n.group().map(|d| (n.id, d))).unwrap();
    assert_eq!(d.inputs.len(), 1, "the missing boundary port was added again");
    assert!(f.view_target(gid).is_some());

    // Missing boundary ports.
    let (f, notes, orig) = damaged(|d, grp, _| {
        gnode(d, grp)["group"]["inputs"] = serde_json::json!([]);
        gnode(d, grp)["group"]["outputs"] = serde_json::json!([]);
    });
    assert!(notes.iter().any(|n| n.contains("added missing ports")), "{notes:?}");
    assert_eq!(flat_wires_and_nodes(&f), flat_wires_and_nodes(&orig));

    // Bad names.
    let (f, _, _) = damaged(|d, grp, _| {
        gnode(d, grp)["group"]["label"] = "  ".into();
        gnode(d, grp)["group"]["inputs"][0]["label"] = "a\nb".repeat(100).into();
        gnode(d, grp)["group"]["origin"] = "x\u{7}".into();
    });
    let d = f.nodes().find_map(|n| n.group()).unwrap();
    assert_eq!(d.label, "Group");
    assert!(d.inputs[0].label.chars().count() <= 128 && !d.inputs[0].label.chars().any(char::is_control));
    assert!(d.origin.is_none());

    // An empty group disappears, its place in the level around it too.
    let (f, notes, orig) = damaged(|d, grp, _| gnode(d, grp)["group"]["members"] = serde_json::json!([]));
    assert!(notes.iter().any(|n| n.contains("empty")), "{notes:?}");
    assert!(!f.has_groups());
    assert_eq!(flat_wires_and_nodes(&f), flat_wires_and_nodes(&orig));
}

#[test]
fn cyclic_and_too_deep_groups_are_dissolved_on_load() {
    let c = chain();
    let mut g = c.g;
    let a = g.group_nodes(&[c.exp], "A").unwrap();
    let b = g.group_nodes(&[c.con], "B").unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&g.to_json()).unwrap();
    doc["nodes"][a.0.to_string()]["group"]["members"].as_array_mut().unwrap().push(b.0.into());
    doc["nodes"][b.0.to_string()]["group"]["members"].as_array_mut().unwrap().push(a.0.into());
    let (f, notes) = Graph::from_json_repaired(&doc.to_string()).unwrap();
    f.check_groups().unwrap();
    assert!(notes.iter().any(|n| n.contains("contain each other")), "{notes:?}");
    assert_eq!(flat_wires_and_nodes(&f), flat_wires_and_nodes(&g));

    // 17 groups inside each other.
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let out = g.add_node(NodeKind::Output, [100.0, 0.0]);
    let exp = g.add_node(NodeKind::Exposure, [50.0, 0.0]);
    g.connect(src, exp, 0).unwrap();
    g.connect(exp, out, 0).unwrap();
    let mut top = g.group_nodes(&[exp], "G").unwrap();
    for _ in 1..MAX_GROUP_DEPTH {
        top = g.group_nodes(&[top], "G").unwrap();
    }
    g.check_groups().unwrap();
    let extra = g.add_node(NodeKind::Group, [0.0, 0.0]);
    let def = GroupDef { label: "Too deep".into(), color: None, origin: None, members: vec![top], inputs: vec![], outputs: vec![], published: vec![] };
    g.node_mut(extra).unwrap().group = Some(def);
    assert!(g.check_groups().is_err());
    let (f, notes) = Graph::from_json_repaired(&g.to_json()).unwrap();
    f.check_groups().unwrap();
    assert!(notes.iter().any(|n| n.contains("nested too deeply")), "{notes:?}");
    assert_eq!(f.validate(), g.validate());
    assert_eq!(f.nodes().filter(|n| n.kind != NodeKind::Group).count(), 3);
}

#[test]
fn a_good_file_needs_no_repair_and_macros_stay_strict() {
    let (g, grp, _) = grouped();
    let (back, notes) = Graph::from_json_repaired(&g.to_json()).unwrap();
    assert!(notes.is_empty());
    assert_eq!(back, g);
    let m = g.extract_macro(grp, "Tone", "").unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&m.to_json()).unwrap();
    doc["group"]["nodes"][0]["group"]["inputs"][0]["node"] = 42.into();
    assert!(MacroFile::from_json(&doc.to_string()).is_err(), "macros are not repaired");
}

#[test]
fn automatic_port_labels_are_clamped_and_cleaned() {
    let mut g = Graph::new();
    let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let exp = g.add_node(NodeKind::Exposure, [100.0, 0.0]);
    let out = g.add_node(NodeKind::Output, [200.0, 0.0]);
    g.connect(src, exp, 0).unwrap();
    g.connect(exp, out, 0).unwrap();
    let long = format!("{}\n{}", "x".repeat(60), "y".repeat(60));
    let alias = g.expose_output(exp, 0, &long).unwrap();
    let alias2 = g.expose_output(exp, 0, &long).unwrap();
    let c = g.add_node(NodeKind::Contrast, [300.0, 0.0]);
    g.connect_ports(exp, alias, c, 0).unwrap();
    let grp = g.group_nodes(&[exp], "G").unwrap();
    g.expose_group_port(grp, false, exp, alias2).unwrap();
    let d = group_of(&g, grp);
    for p in d.inputs.iter().chain(&d.outputs) {
        assert!(p.label.chars().count() <= 128 && !p.label.chars().any(char::is_control), "{:?}", p.label);
    }
    assert!(d.outputs.len() >= 2);
    g.check_groups().unwrap();
    Graph::from_json(&g.to_json()).unwrap();
    // Wires made later through the API add ports with clean labels too.
    let c2 = g.add_node(NodeKind::Contrast, [300.0, 100.0]);
    g.connect_ports(exp, alias2, c2, 0).unwrap();
    g.check_groups().unwrap();
}

#[test]
fn adopt_refuses_ancestors_and_too_deep_nesting_and_ids_are_not_reused() {
    let c = chain();
    let mut g = c.g;
    let inner = g.group_nodes(&[c.exp], "Inner").unwrap();
    let outer = g.group_nodes(&[inner, c.con], "Outer").unwrap();
    let before = g.clone();
    assert!(g.adopt(inner, &[outer]).is_err(), "an ancestor cannot move into its own group");
    assert!(g.adopt(inner, &[inner]).is_err());
    assert_eq!(g, before, "a refused adopt changes nothing");
    // Depth.
    let mut g = Graph::new();
    let n = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let mut top = g.group_nodes(&[n], "G").unwrap();
    let first = top;
    for _ in 1..MAX_GROUP_DEPTH {
        top = g.group_nodes(&[top], "G").unwrap();
    }
    let x = g.add_node(NodeKind::Exposure, [0.0, 50.0]);
    g.adopt(first, &[x]).unwrap();
    let y = g.add_node(NodeKind::Exposure, [0.0, 100.0]);
    let h = g.group_nodes(&[y], "H").unwrap();
    let before = g.clone();
    assert!(g.adopt(first, &[h]).unwrap_err().to_string().contains("too deeply"));
    assert_eq!(g, before);
    // Ungrouping never gives an id back.
    let next = g.add_node(NodeKind::Source, [0.0, 0.0]);
    let grp = g.group_nodes(&[next], "Temp").unwrap();
    g.ungroup(grp).unwrap();
    let again = g.add_node(NodeKind::Source, [0.0, 0.0]);
    assert!(again.0 > grp.0, "{again:?} must not reuse {grp:?}");
}

#[test]
fn huge_macros_are_refused_before_validation_and_descendants_are_fast() {
    let (g, grp, _) = grouped();
    let m = g.extract_macro(grp, "Tone", "").unwrap();
    let mut doc: serde_json::Value = serde_json::from_str(&m.to_json()).unwrap();
    let node = doc["group"]["nodes"][1].clone();
    let list = doc["group"]["nodes"].as_array_mut().unwrap();
    while list.len() <= 2000 {
        list.push(node.clone());
    }
    let err = MacroFile::from_json(&doc.to_string()).unwrap_err().to_string();
    assert!(err.contains("2000"), "{err}");
    // A wide group: descendants is linear.
    let mut g = Graph::new();
    let ids: Vec<NodeId> = (0..20_000).map(|i| g.add_node(NodeKind::Exposure, [i as f32, 0.0])).collect();
    let grp = g.group_nodes(&ids, "Wide").unwrap();
    let started = std::time::Instant::now();
    assert_eq!(g.descendants(grp).len(), 20_000);
    assert!(started.elapsed() < std::time::Duration::from_secs(2));
}
