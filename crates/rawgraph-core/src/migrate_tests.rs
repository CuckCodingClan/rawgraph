//! Structure of migrated graphs: one format-2 graph per old mask kind and per old mask port,
//! written by hand from the old schema.

use serde_json::{json, Value};

use crate::*;

const A: ImageId = ImageId(1);

fn node(id: u64, kind: &str, all: Value) -> Value {
    json!({"id": id, "kind": kind, "mode": "All", "all": all, "individual": {}, "pos": [id as f64 * 100.0, 0.0]})
}

fn with(mut n: Value, key: &str, v: Value) -> Value {
    n[key] = v;
    n
}

/// (from, from_port, to, to_port)
fn doc_v(version: u32, nodes: Vec<Value>, edges: &[(u64, u64, u64, u64)]) -> String {
    let next = nodes.iter().map(|n| n["id"].as_u64().unwrap()).max().unwrap_or(0) + 1;
    let nodes: serde_json::Map<String, Value> = nodes.into_iter().map(|n| (n["id"].to_string(), n)).collect();
    let edges: Vec<Value> = edges
        .iter()
        .map(|&(f, fp, t, tp)| {
            let mut e = json!({"from": f, "to": t, "to_port": tp});
            if fp != 0 {
                e["from_port"] = json!(fp);
            }
            e
        })
        .collect();
    json!({"format_version": version, "nodes": nodes, "edges": edges, "next_id": next}).to_string()
}

fn doc(nodes: Vec<Value>, edges: &[(u64, u64, u64, u64)]) -> String {
    doc_v(2, nodes, edges)
}

fn load(text: &str) -> (Graph, Vec<String>) {
    let (g, notes) = Graph::from_json_with_notes(text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(g.to_json().matches("\"format_version\": 3").count(), 1, "saved as format 3");
    (g, notes)
}

fn n(g: &Graph, id: u64) -> &Node {
    g.node(NodeId(id)).unwrap_or_else(|| panic!("node {id} is gone"))
}

fn has_edge(g: &Graph, from: u64, to: u64, to_port: usize) -> bool {
    g.edges().iter().any(|e| e.from == NodeId(from) && e.to == NodeId(to) && e.to_port == to_port)
}

fn exposed(g: &Graph, id: u64) -> Vec<String> {
    n(g, id).exposed_inputs().iter().flatten().map(|e| e.parameter.clone()).collect()
}

/// Source -> consumer -> Output with the given mask nodes and extra edges.
fn consumer_doc(consumer: &str, params: Value, masks: Vec<Value>, edges: &[(u64, u64, u64, u64)]) -> String {
    let mut nodes = vec![node(1, "Source", json!({})), node(2, consumer, params), node(9, "Output", json!({}))];
    nodes.extend(masks);
    let mut all = vec![(1, 0, 2, 0), (2, 0, 9, 0)];
    all.extend_from_slice(edges);
    doc(nodes, &all)
}

#[test]
fn gradient_mask_becomes_a_linear_data_generator_on_the_consumers_frame() {
    let mask = node(3, "GradientMask", json!({"angle": 30.0, "center": 0.4, "width": 0.6}));
    let (g, notes) = load(&consumer_doc("Exposure", json!({"ev": 1.0}), vec![mask], &[(3, 0, 2, 1)]));
    assert!(notes.is_empty(), "{notes:?}");
    let gen = n(&g, 3);
    assert_eq!(gen.kind, NodeKind::Generator);
    assert_eq!(gen.choice("gradient_style", A), 0);
    assert_eq!(gen.choice("output", A), 1, "Data");
    assert_eq!(gen.param("gradient_angle", A), 30.0);
    assert!((gen.param("gradient_scale", A) - 60.0).abs() < 1e-4);
    // The origin sits on the gradient axis: the old centre 0.4 is 0.1 before the middle.
    let (s, c) = 30f32.to_radians().sin_cos();
    assert!((gen.param("gradient_center_x", A) - (0.5 + (0.4 - 0.5) * c) * 100.0).abs() < 1e-3);
    assert!((gen.param("gradient_center_y", A) - (0.5 + (0.4 - 0.5) * s) * 100.0).abs() < 1e-3);
    assert_eq!(exposed(&g, 2), ["ev"]);
    assert!(has_edge(&g, 3, 2, 1), "the mask feeds the exposed Exposure input");
    assert!(has_edge(&g, 1, 3, 0), "the generator takes the frame the Exposure sees");
    assert!(g.validate().is_empty(), "{:?}", g.validate());
}

#[test]
fn gradient_angles_above_180_wrap_and_a_hard_edge_is_reported() {
    let mask = node(3, "GradientMask", json!({"angle": 270.0, "width": 0.0}));
    let (g, notes) = load(&consumer_doc("Exposure", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    assert_eq!(n(&g, 3).param("gradient_angle", A), -90.0);
    assert_eq!(n(&g, 3).param("gradient_scale", A), 1.0);
    assert!(notes.iter().any(|t| t.starts_with("Gradient Mask (node 3):") && t.contains("hard edge")), "{notes:?}");
}

#[test]
fn every_old_mask_port_becomes_an_exposed_input_of_the_same_parameter() {
    for (kind, port, param) in [
        ("Exposure", 1, "ev"),
        ("Contrast", 1, "amount"),
        ("Saturation", 1, "amount"),
        ("Sharpen", 1, "amount"),
        ("Blur", 1, "radius"),
        ("Blur", 2, "direction"),
    ] {
        // The direction input took an image, every other port a mask.
        let source = if param == "direction" { node(3, "Source", json!({})) } else { node(3, "GradientMask", json!({})) };
        let nodes = vec![node(1, "Source", json!({})), node(2, kind, json!({})), node(9, "Output", json!({})), source];
        let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 9, 0), (3, 0, 2, port)]));
        assert!(notes.is_empty(), "{kind} {param}: {notes:?}");
        assert_eq!(exposed(&g, 2), [param], "{kind} port {port}");
        assert!(has_edge(&g, 3, 2, 1), "{kind} {param}: first exposed slot is port 1");
        assert!(g.validate().is_empty(), "{kind} {param}: {:?}", g.validate());
    }
}

#[test]
fn blur_radius_and_direction_get_separate_slots() {
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Blur", json!({"radius": 20.0})),
        node(3, "GradientMask", json!({})),
        node(4, "Source", json!({})),
        node(9, "Output", json!({})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (3, 0, 2, 1), (4, 0, 2, 2), (2, 0, 9, 0)]));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(exposed(&g, 2), ["radius", "direction"]);
    assert!(has_edge(&g, 3, 2, 1) && has_edge(&g, 4, 2, 2));
    assert_eq!(n(&g, 2).param("radius", A), 20.0);
}

#[test]
fn radial_mask_becomes_a_radial_generator_with_a_smooth_ramp() {
    let mask = node(3, "RadialMask", json!({"center_x": 0.3, "center_y": 0.6, "radius_x": 0.25, "radius_y": 0.25, "feather": 0.4}));
    let (g, notes) = load(&consumer_doc("Exposure", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    assert!(notes.is_empty(), "{notes:?}");
    let gen = n(&g, 3);
    assert_eq!((gen.kind, gen.choice("gradient_style", A), gen.choice("ramp_interpolation", A)), (NodeKind::Generator, 1, 1));
    assert!((gen.param("gradient_scale", A) - 50.0).abs() < 1e-3, "scale is the diameter in percent");
    assert!((gen.param("gradient_center_x", A) - 30.0).abs() < 1e-3 && (gen.param("gradient_center_y", A) - 60.0).abs() < 1e-3);
    let Some(NodeData::GradientRamp(ramp)) = gen.data(A) else { panic!("ramp data") };
    let positions: Vec<f32> = ramp.colors.iter().map(|c| c.position).collect();
    assert!((positions[1] - 0.6).abs() < 1e-5 && positions.len() == 3, "{positions:?}");
    assert_eq!((ramp.colors[0].color, ramp.colors[2].color), ([1.0; 3], [0.0; 3]));
    // Inverted: the colours swap.
    let mask = node(3, "RadialMask", json!({"invert": 1.0}));
    let (g, _) = load(&consumer_doc("Exposure", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    let Some(NodeData::GradientRamp(ramp)) = n(&g, 3).data(A) else { panic!("ramp data") };
    assert_eq!(ramp.colors[0].color, [0.0; 3]);
}

#[test]
fn unequal_radial_radii_are_approximated_and_reported() {
    let mask = node(3, "RadialMask", json!({"radius_x": 0.4, "radius_y": 0.1, "angle": 45.0}));
    let (g, notes) = load(&consumer_doc("Exposure", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    assert!((n(&g, 3).param("gradient_scale", A) - 40.0).abs() < 1e-3, "2 * sqrt(0.4 * 0.1)");
    assert!(notes.len() == 1 && notes[0].starts_with("Radial Mask (node 3):") && notes[0].contains("angle is dropped"), "{notes:?}");
}

#[test]
fn brush_mask_becomes_draw_and_keeps_the_strokes_per_image() {
    let stroke = json!({"Strokes": [{"points": [[0.1, 0.1], [0.5, 0.5]], "radius": 0.05, "hardness": 0.5, "flow": 1.0, "erase": false}]});
    let mask = with(with(node(3, "BrushMask", json!({})), "mode", json!("Individual")), "individual_data", json!({"1": stroke.clone()}));
    let (g, notes) = load(&consumer_doc("Blur", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    assert!(notes.is_empty(), "{notes:?}");
    let draw = n(&g, 3);
    assert_eq!((draw.kind, draw.mode), (NodeKind::Draw, Mode::Individual));
    assert!(matches!(draw.data(A), Some(NodeData::Strokes(s)) if s.len() == 1 && s[0].points.len() == 2));
    assert!(draw.data(ImageId(2)).is_none());
    assert!(has_edge(&g, 1, 3, 0), "Draw takes the frame, hence the photos, of the Filter");
    assert!(has_edge(&g, 3, 2, 1));
    assert_eq!(g.nodes().count(), 4);
}

#[test]
fn inverted_brush_mask_gets_an_inversion_curve() {
    let mask = with(node(3, "BrushMask", json!({"invert": 1.0})), "data", json!({"Strokes": []}));
    let (g, notes) = load(&consumer_doc("Exposure", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(n(&g, 3).kind, NodeKind::PointCurve, "the old id keeps the output");
    let draw = g.nodes().find(|x| x.kind == NodeKind::Draw).expect("a Draw node");
    assert!(has_edge(&g, draw.id.0, 3, 0) && has_edge(&g, 3, 2, 1) && has_edge(&g, 1, draw.id.0, 0));
    assert_eq!(draw.param("width", A), 1024.0, "Draw keeps only its own parameters");
    let entries = curve_set(n(&g, 3), A);
    assert_eq!(entries.iter().map(|e| e.channel).collect::<Vec<_>>(), [1, 2, 3]);
    let p = &entries[0].points;
    assert!(p[0][0] == 0.0 && p[0][1] > 0.84, "m = 0 becomes 1 (t = 0.847)");
    assert!(p.last().unwrap()[1] == 0.0, "m = 1 becomes 0");
}

#[test]
fn luminance_mask_becomes_luma_grey_plus_a_curve() {
    let mask = node(3, "LuminanceMask", json!({"low": 0.3, "high": 0.8, "feather": 0.2}));
    let nodes = vec![node(1, "Source", json!({})), node(2, "Exposure", json!({})), node(9, "Output", json!({})), mask];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 9, 0), (1, 0, 3, 0), (3, 0, 2, 1)]));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(n(&g, 3).kind, NodeKind::PointCurve);
    let bw = g.nodes().find(|x| x.kind == NodeKind::BlackWhite).expect("grey node");
    assert!((bw.param("green", A) - 0.674_081_8).abs() < 1e-6);
    assert!(has_edge(&g, 1, bw.id.0, 0) && has_edge(&g, bw.id.0, 3, 0) && has_edge(&g, 3, 2, 1));
    let entries = curve_set(n(&g, 3), A);
    assert_eq!(entries.len(), 3);
    assert!(entries[0].points.len() <= MAX_CURVE_POINTS_FOR_TESTS);
}

const MAX_CURVE_POINTS_FOR_TESTS: usize = 32;

#[test]
fn mask_adjust_becomes_rgb_curves_and_a_neutral_adjust_is_an_empty_curve_set() {
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Exposure", json!({})),
        node(9, "Output", json!({})),
        node(3, "GradientMask", json!({})),
        node(4, "MaskAdjust", json!({"low": 0.2, "high": 0.9, "gamma": 2.0, "invert": 1.0})),
        node(5, "MaskAdjust", json!({})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 9, 0), (3, 0, 4, 0), (4, 0, 5, 0), (5, 0, 2, 1)]));
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!((n(&g, 4).kind, n(&g, 5).kind), (NodeKind::PointCurve, NodeKind::PointCurve));
    assert!(has_edge(&g, 3, 4, 0) && has_edge(&g, 4, 5, 0) && has_edge(&g, 5, 2, 1));
    assert_eq!(curve_set(n(&g, 4), A).len(), 3);
    assert!(curve_set(n(&g, 5), A).is_empty(), "the default adjust does nothing");
    assert!(has_edge(&g, 1, 3, 0), "the chain's generator takes the consumer's frame");
}

#[test]
fn mask_combine_becomes_a_layer_with_the_matching_blend() {
    for (mode, blend) in [(0.0, 3.0), (1.0, 1.0), (2.0, 2.0), (3.0, 7.0), (4.0, 6.0)] {
        let nodes = vec![
            node(1, "Source", json!({})),
            node(2, "Exposure", json!({})),
            node(9, "Output", json!({})),
            node(3, "GradientMask", json!({})),
            node(4, "RadialMask", json!({})),
            node(5, "MaskCombine", json!({"mode": mode})),
        ];
        let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 9, 0), (3, 0, 5, 0), (4, 0, 5, 1), (5, 0, 2, 1)]));
        assert!(notes.is_empty(), "{notes:?}");
        let layer = n(&g, 5);
        assert_eq!(layer.kind, NodeKind::Layer);
        assert_eq!((layer.param("l2_blend", A), layer.param("l2_fit", A)), (blend, 3.0), "mode {mode}");
        assert!(has_edge(&g, 3, 5, 0) && has_edge(&g, 4, 5, 1) && has_edge(&g, 5, 2, 1));
    }
}

#[test]
fn hue_mask_is_removed_and_the_slider_applies_everywhere() {
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Saturation", json!({"amount": 1.5})),
        node(9, "Output", json!({})),
        node(3, "HueMask", json!({"hue": 30.0})),
        node(4, "MaskAdjust", json!({"invert": 1.0})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 9, 0), (1, 0, 3, 0), (3, 0, 4, 0), (4, 0, 2, 1)]));
    assert!(g.node(NodeId(3)).is_none() && g.node(NodeId(4)).is_none(), "the dependent adjust goes too");
    assert!(exposed(&g, 2).is_empty());
    assert_eq!(n(&g, 2).param("amount", A), 1.5);
    assert!(notes.iter().any(|t| t.starts_with("Hue Mask (node 3):") && t.contains("no equivalent")), "{notes:?}");
    assert!(notes.iter().any(|t| t.starts_with("Mask Adjust (node 4):")), "{notes:?}");
    assert!(notes.iter().any(|t| t.starts_with("Saturation (node 2):") && t.contains("slider now applies everywhere")), "{notes:?}");
    assert!(g.validate().is_empty(), "{:?}", g.validate());
}

#[test]
fn mask_combine_with_a_removed_input_is_removed() {
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Exposure", json!({})),
        node(9, "Output", json!({})),
        node(3, "GradientMask", json!({})),
        node(4, "HueMask", json!({})),
        node(5, "MaskCombine", json!({})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 9, 0), (1, 0, 4, 0), (3, 0, 5, 0), (4, 0, 5, 1), (5, 0, 2, 1)]));
    assert!(g.node(NodeId(5)).is_none() && g.node(NodeId(4)).is_none());
    assert!(g.node(NodeId(3)).is_some(), "an independent mask stays");
    assert!(notes.iter().any(|t| t.starts_with("Mask Combine (node 5):")), "{notes:?}");
}

#[test]
fn depth_estimate_and_normals_are_renamed() {
    // Depth Estimate feeds Normals (depth input) which has a height image too.
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "DepthEstimate", json!({})),
        node(3, "Normals", json!({"strength": 2.0})),
        node(4, "Normals", json!({"mode": 1.0, "strength": 3.0})),
        node(9, "Output", json!({})),
        node(8, "Output", json!({})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 3, 0), (1, 0, 4, 1), (3, 0, 9, 0), (4, 0, 8, 0)]));
    assert_eq!((n(&g, 2).kind, n(&g, 3).kind), (NodeKind::DepthMap, NodeKind::NormalMap));
    assert!(has_edge(&g, 2, 3, 0), "the depth input is the only input");
    assert!(has_edge(&g, 1, 4, 0), "the height image moves to port 0");
    assert_eq!((n(&g, 3).param("strength", A), n(&g, 4).param("strength", A)), (2.0, 3.0));
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn a_mask_node_feeding_a_depth_estimate_consumer_keeps_its_image_source_frame_rules() {
    // A depth map used as a control of Blur.
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Blur", json!({})),
        node(3, "DepthEstimate", json!({})),
        node(9, "Output", json!({})),
    ];
    let (g, _) = load(&doc(nodes, &[(1, 0, 2, 0), (1, 0, 3, 0), (3, 0, 2, 1), (2, 0, 9, 0)]));
    assert_eq!(n(&g, 3).kind, NodeKind::DepthMap);
    assert_eq!(exposed(&g, 2), ["radius"]);
    assert!(has_edge(&g, 3, 2, 1) && has_edge(&g, 1, 3, 0));
    assert_eq!(g.edges().len(), 4, "no frame edge is added to a depth map");
}

#[test]
fn exposed_inputs_of_the_old_file_keep_their_wires_and_lose_the_mask_type() {
    let mut dev = node(2, "Develop", json!({}));
    dev["exposed_inputs"] = json!([{"parameter": "exposure", "ty": "Mask"}, null, {"parameter": "blacks", "ty": "Images"}]);
    let nodes = vec![node(1, "Source", json!({})), dev, node(3, "GradientMask", json!({})), node(4, "Source", json!({})), node(9, "Output", json!({}))];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (3, 0, 2, 1), (4, 0, 2, 3), (2, 0, 9, 0)]));
    assert_eq!(exposed(&g, 2), ["exposure", "blacks"]);
    assert!(has_edge(&g, 3, 2, 1) && has_edge(&g, 4, 2, 3));
    assert!(notes.iter().filter(|t| t.starts_with("Develop (node 2):") && t.contains("scales the slider")).count() == 2, "{notes:?}");
    assert!(g.validate().is_empty(), "{:?}", g.validate());
}

#[test]
fn an_old_exposed_blur_radius_sets_the_slider_to_its_maximum() {
    let mut blur = node(2, "Blur", json!({"radius": 12.0}));
    blur["exposed_inputs"] = json!([{"parameter": "radius", "ty": "Mask"}]);
    let nodes = vec![node(1, "Source", json!({})), blur, node(3, "RadialMask", json!({})), node(9, "Output", json!({}))];
    // Old ports: 0 image, 1 radius mask, 2 direction, 3 first exposed input.
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (3, 0, 2, 3), (2, 0, 9, 0)]));
    assert_eq!(n(&g, 2).param("radius", A), 1000.0, "the old exposed radius was the maximum");
    assert!(has_edge(&g, 3, 2, 1));
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn both_an_old_mask_port_and_an_exposed_input_keep_only_the_mask_port() {
    let mut exp = node(2, "Exposure", json!({}));
    exp["exposed_inputs"] = json!([{"parameter": "ev", "ty": "Images"}]);
    let nodes = vec![
        node(1, "Source", json!({})),
        exp,
        node(3, "GradientMask", json!({})),
        node(4, "RadialMask", json!({})),
        node(9, "Output", json!({})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (3, 0, 2, 1), (4, 0, 2, 2), (2, 0, 9, 0)]));
    assert_eq!(exposed(&g, 2), ["ev"]);
    assert!(has_edge(&g, 3, 2, 1) && !has_edge(&g, 4, 2, 1));
    assert!(notes.iter().any(|t| t.contains("connected twice")), "{notes:?}");
}

#[test]
fn per_image_values_of_old_mask_nodes_are_converted_per_image() {
    let mut mask = node(3, "GradientMask", json!({"angle": 0.0}));
    mask["mode"] = json!("Individual");
    mask["individual"] = json!({"2": {"angle": 180.0, "width": 1.0}});
    let (g, _) = load(&consumer_doc("Exposure", json!({}), vec![mask], &[(3, 0, 2, 1)]));
    let gen = n(&g, 3);
    assert_eq!(gen.mode, Mode::Individual);
    assert_eq!(gen.param("gradient_angle", ImageId(1)), 0.0);
    assert_eq!(gen.param("gradient_angle", ImageId(2)), 180.0);
    assert_eq!(gen.param("gradient_scale", ImageId(2)), 100.0);
    assert_eq!(gen.param("gradient_scale", ImageId(1)), 50.0);
}

#[test]
fn a_mask_shared_by_several_consumers_with_different_frames_reports_it() {
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Exposure", json!({})),
        node(3, "Transform", json!({"left": 0.1})),
        node(4, "Exposure", json!({})),
        node(5, "GradientMask", json!({})),
        node(9, "Output", json!({})),
        node(8, "Output", json!({})),
    ];
    let (g, notes) = load(&doc(nodes, &[(1, 0, 2, 0), (1, 0, 3, 0), (3, 0, 4, 0), (5, 0, 2, 1), (5, 0, 4, 1), (2, 0, 9, 0), (4, 0, 8, 0)]));
    assert!(has_edge(&g, 1, 5, 0), "the first consumer decides");
    assert!(notes.iter().any(|t| t.starts_with("Gradient Mask (node 5):") && t.contains("different frames")), "{notes:?}");
}

#[test]
fn a_mask_is_never_wired_to_a_frame_that_depends_on_it() {
    // The second Exposure takes the first one's output, which already used the mask.
    let nodes = vec![
        node(1, "Source", json!({})),
        node(2, "Exposure", json!({})),
        node(3, "Exposure", json!({})),
        node(4, "BrushMask", json!({})),
        node(9, "Output", json!({})),
    ];
    let (g, _) = load(&doc(nodes, &[(1, 0, 2, 0), (2, 0, 3, 0), (4, 0, 2, 1), (4, 0, 3, 1), (3, 0, 9, 0)]));
    assert!(has_edge(&g, 1, 4, 0), "the first consumer's frame is used and that is upstream");
    assert!(!has_edge(&g, 2, 4, 0));
    assert!(g.validate().is_empty(), "{:?}", g.validate());
}

#[test]
fn version_1_fixture_loads_and_its_brush_mask_becomes_a_draw() {
    let text = include_str!("../tests/fixtures/graph_v1.json");
    let (g, notes) = load(text);
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(g.nodes().count(), 11);
    let draw = n(&g, 8);
    assert_eq!((draw.kind, draw.mode), (NodeKind::Draw, Mode::Individual));
    assert!(matches!(draw.data(A), Some(NodeData::Strokes(s)) if s.len() == 1));
    assert!(draw.data(ImageId(2)).is_none());
    assert_eq!(exposed(&g, 9), ["radius"]);
    assert!(has_edge(&g, 8, 9, 1) && has_edge(&g, 7, 8, 0), "the Merge feeding the Filter frames the Draw");
    assert_eq!(g.edges().len(), 12);
    assert_eq!(Graph::from_json(&g.to_json()), Ok(g));
}

#[test]
fn format_3_documents_are_not_touched_and_report_nothing() {
    let (g, notes) = Graph::from_json_with_notes(include_str!("../tests/fixtures/graph_v3.json")).unwrap();
    assert!(notes.is_empty());
    assert_eq!(g.nodes().count(), 11);
}

#[test]
fn the_tone_version_choice_of_old_develop_nodes_is_ignored() {
    let dev = with(node(2, "Develop", json!({"tone_version": 1.0, "exposure": 0.5})), "individual", json!({"1": {"tone_version": 0.0}}));
    let (g, notes) = load(&doc(vec![node(1, "Source", json!({})), dev], &[(1, 0, 2, 0)]));
    assert!(notes.is_empty());
    assert_eq!(n(&g, 2).param("exposure", A), 0.5);
}

#[test]
fn unusable_documents_still_fail_with_the_normal_error() {
    assert!(matches!(Graph::from_json_with_notes("[1]"), Err(GraphError::Parse(_))));
    let text = doc(vec![node(1, "Nope", json!({}))], &[]);
    assert!(matches!(Graph::from_json_with_notes(&text), Err(GraphError::Parse(m)) if m.contains("Nope")));
    let text = doc_v(1, vec![node(1, "Exposure", json!({"ev": 99.0}))], &[]);
    assert!(Graph::from_json_with_notes(&text).is_err(), "out-of-range parameters of kept nodes are still rejected");
}

// ---------------------------------------------------------------- sub-graphs

fn sub_node(kind: &str, all: Value) -> Value {
    json!({"kind": kind, "mode": "All", "all": all, "pos": [0.0, 0.0]})
}

#[test]
fn version_1_sub_graphs_with_masks_migrate_and_paste() {
    let text = json!({
        "format_version": 1,
        "nodes": [
            sub_node("Exposure", json!({"ev": 1.0})),
            sub_node("GradientMask", json!({"angle": 0.0})),
            sub_node("Saturation", json!({})),
            sub_node("HueMask", json!({}))
        ],
        "edges": [{"from": 1, "to": 0, "to_port": 1}, {"from": 3, "to": 2, "to_port": 1}]
    })
    .to_string();
    let (sub, notes) = Subgraph::from_json_with_notes(&text).unwrap();
    assert!(notes.iter().any(|t| t.starts_with("Hue Mask")), "{notes:?}");
    assert_eq!(sub.kinds(), vec![NodeKind::Exposure, NodeKind::Generator, NodeKind::Saturation]);
    let mut g = Graph::new();
    let ids = g.insert_subgraph(&sub, [0.0, 0.0]);
    assert_eq!(ids.len(), 3);
    assert_eq!(g.node(ids[0]).unwrap().exposed_inputs().iter().flatten().count(), 1);
    assert!(g.edges().iter().any(|e| e.from == ids[1] && e.to == ids[0] && e.to_port == 1));
    assert!(sub.to_json().contains("\"format_version\": 2"));
}

#[test]
fn format_2_sub_graphs_load_unchanged() {
    let mut g = Graph::new();
    let a = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
    let b = g.add_node(NodeKind::Generator, [0.0, 0.0]);
    g.expose_parameter(a, "ev", PortType::Images).unwrap();
    g.connect(b, a, 1).unwrap();
    let sub = g.extract_subgraph(&[a, b]);
    let (back, notes) = Subgraph::from_json_with_notes(&sub.to_json()).unwrap();
    assert!(notes.is_empty());
    assert_eq!(back, sub);
}
