//! Pure rules for long parameter lists: which parameters form a group in the
//! inspector, which ones fit into a node box, and which image control to overlay.

use rawgraph_core::{Graph, ImageId, Node, NodeId, NodeKind, ParamKind, ParamSpec};

/// Most parameters a node box shows; the rest is summarised as "... +n".
pub const MAX_BOX_ROWS: usize = 4;

/// One collapsible or labelled block of parameters in the inspector.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamGroup {
    pub title: String,
    pub names: Vec<&'static str>,
    /// Colour swatch shown next to the title (colour bands).
    pub swatch: Option<[u8; 3]>,
}

/// The six colour bands of Hue / Saturation: (title, parameter prefix, swatch).
pub const BANDS: [(&str, &str, [u8; 3]); 6] = [
    ("Red", "red", [230, 64, 64]),
    ("Yellow", "yellow", [236, 208, 60]),
    ("Green", "green", [84, 200, 96]),
    ("Cyan", "cyan", [70, 206, 216]),
    ("Blue", "blue", [78, 112, 236]),
    ("Magenta", "magenta", [222, 84, 206]),
];

/// The three groups of Lift / Gamma / Gain: (title, parameter prefix).
pub const WHEELS: [(&str, &str); 3] = [("Lift", "lift"), ("Gamma", "gamma"), ("Gain", "gain")];

/// Colour of the swatch that shows the direction of a group's tint: the per-channel values
/// `[r, g, b]` relative to their mean, as a colour around mid grey. `range` is the largest value a
/// channel slider can take, so a full-range tint gives a strong colour and a neutral group grey.
pub fn tint_rgb(values: [f32; 3], range: f32) -> [u8; 3] {
    let v = values.map(|x| if x.is_finite() { x } else { 0.0 });
    let mean = (v[0] + v[1] + v[2]) / 3.0;
    let d = [v[0] - mean, v[1] - mean, v[2] - mean];
    let s = d.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    if s < 1e-4 || range <= 0.0 {
        return [128, 128, 128];
    }
    // Strength grows quickly so that a small tint is still visible.
    let strength = (0.45 + 0.55 * (s / range).clamp(0.0, 1.0).sqrt()).clamp(0.0, 1.0);
    d.map(|x| (128.0 + 127.0 * (x / s) * strength).round().clamp(0.0, 255.0) as u8)
}

/// Whether any of a group's parameters differs from its default.
pub fn group_changed(node: &Node, image: ImageId, group: &ParamGroup) -> bool {
    group.names.iter().any(|name| is_changed(node, image, name))
}

/// The three rows of the node box of a Lift / Gamma / Gain node: one value per group, "-" while
/// the group is neutral, else the master value and the channels that have a tint.
pub fn wheels_rows(node: &Node, image: ImageId) -> Vec<Row> {
    WHEELS
        .iter()
        .map(|(title, prefix)| {
            let master = node.param(prefix, image);
            let ch: Vec<String> = ["r", "g", "b"]
                .iter()
                .filter_map(|c| {
                    let v = node.param(&format!("{prefix}_{c}"), image);
                    (v.abs() > 1e-4).then(|| format!("{c}{v:+.2}"))
                })
                .collect();
            let mut parts = Vec::new();
            if master.abs() > 1e-4 {
                parts.push(format!("m{master:+.2}"));
            }
            parts.extend(ch);
            let changed = !parts.is_empty();
            let value = if changed { parts.join(" ") } else { "-".to_owned() };
            Row { key: None, label: (*title).to_owned(), value, changed }
        })
        .collect()
}

/// The line under Noise Reduction's "ISO adaptive" check box: what the strength is multiplied by
/// for the selected image.
pub fn iso_scale_line(iso: Option<f32>) -> String {
    let scale = rawgraph_core::graph::iso_noise_scale(iso);
    match iso.filter(|v| v.is_finite() && *v > 0.0) {
        Some(v) => format!("ISO {v:.0} -> strength x {scale:.2}"),
        None => format!("ISO unknown -> x {scale:.2}"),
    }
}

/// Groups of a node kind and the parameters that belong to none of them.
/// Kinds without groups return an empty group list and all parameters.
pub fn param_groups(kind: NodeKind) -> (Vec<ParamGroup>, Vec<&'static str>) {
    let all: Vec<&'static str> = kind.params().iter().map(|s| s.name).collect();
    let mut groups: Vec<ParamGroup> = Vec::new();
    match kind {
        NodeKind::HueSaturation => {
            for (title, prefix, swatch) in BANDS {
                let mut names: Vec<&'static str> = all.iter().copied().filter(|n| n.strip_prefix(prefix).is_some_and(|r| r.starts_with('_'))).collect();
                // Hue, Saturation, Lightness in this order.
                names.sort_by_key(|n| match n.rsplit('_').next() {
                    Some("h") => 0,
                    Some("s") => 1,
                    _ => 2,
                });
                if !names.is_empty() {
                    groups.push(ParamGroup { title: title.to_owned(), names, swatch: Some(swatch) });
                }
            }
        }
        NodeKind::Transform => {
            let crop: Vec<&'static str> = ["left", "top", "right", "bottom"]
                .iter()
                .filter_map(|side| all.iter().copied().find(|n| n.contains(side)))
                .collect();
            let rotation: Vec<&'static str> = all.iter().copied().filter(|n| n.contains("angle") || n.contains("rotate") || n.contains("flip")).collect();
            if !crop.is_empty() {
                groups.push(ParamGroup { title: "Crop".to_owned(), names: crop, swatch: None });
            }
            if !rotation.is_empty() {
                groups.push(ParamGroup { title: "Rotation".to_owned(), names: rotation, swatch: None });
            }
        }
        NodeKind::ColorWheels => {
            for (title, prefix) in WHEELS {
                let names: Vec<&'static str> = all.iter().copied().filter(|n| *n == prefix || n.strip_prefix(prefix).is_some_and(|r| r.starts_with('_'))).collect();
                if !names.is_empty() {
                    groups.push(ParamGroup { title: title.to_owned(), names, swatch: None });
                }
            }
        }
        NodeKind::Develop => {
            let pick = |names: &[&str]| -> Vec<&'static str> { names.iter().filter_map(|n| all.iter().copied().find(|a| a == n)).collect() };
            for (title, names) in [
                ("Tone", pick(&["exposure", "contrast", "highlights", "shadows", "whites", "blacks"])),
                ("Colour", pick(&["wb", "temperature", "tint", "vibrance", "saturation"])),

            ] {
                if !names.is_empty() {
                    groups.push(ParamGroup { title: title.to_owned(), names, swatch: None });
                }
            }
        }
        _ => {}
    }
    let rest: Vec<&'static str> = all.iter().copied().filter(|n| !(kind == NodeKind::Develop && ["baseline", "baseline_default", "brightness"].contains(n)) && !groups.iter().any(|g| g.names.contains(n))).collect();
    (groups, rest)
}

/// Whether a parameter differs from its default (for highlighting changed bands).
pub fn is_changed(node: &Node, image: ImageId, name: &str) -> bool {
    node.kind.param_spec(name).is_some_and(|s| (node.param(name, image) - s.default).abs() > 1e-4)
}

/// One row of a node box.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: Option<&'static str>,
    pub label: String,
    pub value: String,
    pub changed: bool,
}

fn short_unit(unit: &str) -> &str {
    match unit {
        "\u{2030} of long edge" => "\u{2030}",
        "stops" => "EV",
        other => other,
    }
}

fn format_value(v: f32, unit: &str) -> String {
    let short = short_unit(unit);
    if short.is_empty() {
        format!("{v:.2}")
    } else {
        format!("{v:.2} {short}")
    }
}

pub fn parameter_row(node: &Node, image: ImageId, spec: &ParamSpec) -> Row {
    let v = node.param(spec.name, image);
    let value = match spec.kind {
        ParamKind::Choice(opts) => opts.get(v.max(0.0).round() as usize).copied().unwrap_or("?").to_owned(),
        ParamKind::Toggle => if v >= 0.5 { "on" } else { "off" }.to_owned(),
        ParamKind::Continuous => format_value(v, spec.unit),
    };
    Row { key: Some(spec.name), label: spec.label.to_owned(), value, changed: is_changed(node, image, spec.name) }
}

/// Every visible parameter of a node as a row (no limit).
pub fn all_rows(node: &Node, image: ImageId) -> Vec<Row> {
    node.kind
        .params()
        .iter()
        .filter(|spec| !(node.kind == NodeKind::Develop && ["baseline", "baseline_default", "brightness"].contains(&spec.name)))
        .map(|spec| parameter_row(node, image, spec))
        .collect()
}

/// At most `max` rows: changed parameters first (kept in their own order),
/// then the first unchanged ones. When rows are left out, the last row reads
/// "... +n". The result always has `min(len, max) + (len > max) as usize` rows.
pub fn limit_rows(rows: Vec<Row>, max: usize) -> Vec<Row> {
    if rows.len() <= max {
        return rows;
    }
    let mut keep = vec![false; rows.len()];
    let mut left = max;
    for (i, r) in rows.iter().enumerate() {
        if r.changed && left > 0 {
            keep[i] = true;
            left -= 1;
        }
    }
    for k in keep.iter_mut() {
        if left == 0 {
            break;
        }
        if !*k {
            *k = true;
            left -= 1;
        }
    }
    let hidden = rows.len() - max;
    let mut out: Vec<Row> = rows.into_iter().zip(keep).filter(|(_, k)| *k).map(|(r, _)| r).collect();
    out.push(Row { key: None, label: format!("\u{2026} +{hidden}"), value: String::new(), changed: false });
    out
}

/// Number of rows the box of `kind` reserves.
pub fn box_row_count(kind: NodeKind) -> usize {
    let n = kind.params().len();
    n.min(MAX_BOX_ROWS) + usize::from(n > MAX_BOX_ROWS)
}

/// Which mask the viewer should overlay: only when the toggle is on and the
/// selected node has a connected mask input.
pub fn overlay_request(enabled: bool, graph: &Graph, selected: Option<NodeId>) -> Option<NodeId> {
    // A selected Draw is being painted: its own mask is always shown.
    if let Some(node) = selected.and_then(|id| graph.node(id)).filter(|n| n.kind == NodeKind::Draw) {
        return Some(node.id);
    }
    if !enabled {
        return None;
    }
    let id = selected?;
    let node = graph.node(id)?;
    node.exposed_inputs().iter().enumerate().find_map(|(i, input)| {
        let input = input.as_ref()?;
        if input.parameter == "direction" { return None; }
        graph.input_of(id, node.kind.inputs().len() + i)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMG: ImageId = ImageId(1);

    #[test]
    fn hue_saturation_has_six_groups_of_three() {
        let (groups, rest) = param_groups(NodeKind::HueSaturation);
        assert_eq!(groups.len(), 6);
        assert!(rest.is_empty(), "{rest:?}");
        assert_eq!(groups[0].title, "Red");
        assert_eq!(groups[5].title, "Magenta");
        assert_eq!(groups[0].names, vec!["red_h", "red_s", "red_l"]);
        assert!(groups.iter().all(|g| g.names.len() == 3 && g.swatch.is_some()));
        let total: usize = groups.iter().map(|g| g.names.len()).sum();
        assert_eq!(total, NodeKind::HueSaturation.params().len());
    }

    #[test]
    fn transform_groups_crop_and_rotation() {
        let (groups, rest) = param_groups(NodeKind::Transform);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].title, "Crop");
        assert_eq!(groups[0].names.len(), 4);
        assert_eq!(groups[1].title, "Rotation");
        assert_eq!(groups[1].names.len(), 4, "angle, rotate, flip_h, flip_v: {:?}", groups[1].names);
        assert!(rest.is_empty(), "{rest:?}");
    }

    fn transform_size(node: &Node, image: ImageId, w: u32, h: u32) -> (u32, u32) {
        crate::crop_logic::result_size(w, h, &crate::tools::transform_params(node, image))
    }

    #[test]
    fn transform_size_follows_crop_rotation_and_straightening() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Transform, [0.0, 0.0]);
        assert_eq!(transform_size(g.node(id).unwrap(), IMG, 2000, 1000), (2000, 1000), "neutral");
        let n = g.node_mut(id).unwrap();
        n.set_param("left", IMG, 0.1);
        n.set_param("right", IMG, 0.9);
        n.set_param("top", IMG, 0.25);
        n.set_param("bottom", IMG, 0.75);
        assert_eq!(transform_size(n, IMG, 2000, 1000), (1600, 500));
        n.set_param("rotate", IMG, 1.0);
        // The engine turns the picture first (1000 x 2000) and crops the turned picture.
        assert_eq!(transform_size(n, IMG, 2000, 1000), (800, 1000), "90 degrees: the crop applies to the turned picture");
        n.set_param("rotate", IMG, 0.0);
        n.set_param("angle", IMG, 5.0);
        let (w, h) = transform_size(n, IMG, 2000, 1000);
        assert!(w < 1600 && h < 500, "straightening crops away the empty corners: {w}x{h}");
        assert!((f64::from(w) / f64::from(h) - 3.2).abs() < 0.02, "the aspect ratio is kept: {w}x{h}");
        n.set_param("left", IMG, 0.9);
        assert!(transform_size(n, IMG, 2000, 1000).0 >= 1, "an empty crop never gives a zero size");
    }

    #[test]
    fn color_wheels_have_three_groups_of_four() {
        let (groups, rest) = param_groups(NodeKind::ColorWheels);
        assert!(rest.is_empty(), "{rest:?}");
        assert_eq!(groups.iter().map(|g| g.title.as_str()).collect::<Vec<_>>(), vec!["Lift", "Gamma", "Gain"]);
        assert_eq!(groups[0].names, vec!["lift", "lift_r", "lift_g", "lift_b"]);
        assert_eq!(groups[1].names, vec!["gamma", "gamma_r", "gamma_g", "gamma_b"]);
        assert_eq!(groups[2].names, vec!["gain", "gain_r", "gain_g", "gain_b"]);
        let total: usize = groups.iter().map(|g| g.names.len()).sum();
        assert_eq!(total, NodeKind::ColorWheels.params().len());
    }

    #[test]
    fn the_swatch_shows_the_direction_of_the_tint() {
        assert_eq!(tint_rgb([0.0; 3], 0.5), [128, 128, 128], "neutral is grey");
        assert_eq!(tint_rgb([0.2, 0.2, 0.2], 0.5), [128, 128, 128], "the same on all channels is no tint");
        let red = tint_rgb([0.3, 0.0, 0.0], 2.0);
        assert!(red[0] > 200 && red[1] < 100 && red[2] < 100 && red[1] == red[2], "{red:?}");
        let blue = tint_rgb([0.0, 0.0, 0.1], 0.5);
        assert!(blue[2] > 200 && blue[0] < 100, "{blue:?}");
        let yellowish = tint_rgb([0.2, 0.2, 0.0], 1.0);
        assert!(yellowish[0] > 150 && yellowish[1] > 150 && yellowish[2] < 100, "{yellowish:?}");
        // A larger tint is stronger than a small one.
        assert!(tint_rgb([0.4, 0.0, 0.0], 0.5)[0] > tint_rgb([0.02, 0.0, 0.0], 0.5)[0]);
        assert_eq!(tint_rgb([f32::NAN, 0.0, 0.0], 0.5), [128, 128, 128], "never panics on bad input");
    }

    #[test]
    fn the_wheels_box_shows_one_compact_row_per_group() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::ColorWheels, [0.0, 0.0]);
        let rows = wheels_rows(g.node(id).unwrap(), IMG);
        assert_eq!(rows.iter().map(|r| (r.label.as_str(), r.value.as_str(), r.changed)).collect::<Vec<_>>(), vec![("Lift", "-", false), ("Gamma", "-", false), ("Gain", "-", false)]);
        let n = g.node_mut(id).unwrap();
        n.set_param("gain_r", IMG, 0.3);
        n.set_param("lift_b", IMG, 0.1);
        n.set_param("gamma", IMG, 0.2);
        let rows = wheels_rows(g.node(id).unwrap(), IMG);
        assert_eq!(rows[0].value, "b+0.10");
        assert_eq!(rows[1].value, "m+0.20");
        assert_eq!(rows[2].value, "r+0.30");
        assert!(rows.iter().all(|r| r.changed));
        let (groups, _) = param_groups(NodeKind::ColorWheels);
        let node = g.node(id).unwrap();
        assert!(group_changed(node, IMG, &groups[0]) && group_changed(node, IMG, &groups[2]));
    }

    #[test]
    fn the_iso_line_shows_the_factor_or_says_unknown() {
        assert_eq!(iso_scale_line(Some(2500.0)), "ISO 2500 -> strength x 1.00");
        assert_eq!(iso_scale_line(Some(10000.0)), "ISO 10000 -> strength x 2.00");
        assert_eq!(iso_scale_line(Some(100.0)), "ISO 100 -> strength x 0.25");
        assert_eq!(iso_scale_line(Some(400000.0)), "ISO 400000 -> strength x 2.50", "clamped like the engine");
        assert_eq!(iso_scale_line(None), "ISO unknown -> x 1.00");
        assert_eq!(iso_scale_line(Some(0.0)), "ISO unknown -> x 1.00");
        assert_eq!(iso_scale_line(Some(f32::NAN)), "ISO unknown -> x 1.00");
    }

    #[test]
    fn develop_groups_and_plain_kinds() {
        let (groups, rest) = param_groups(NodeKind::Develop);
        assert_eq!(groups.iter().map(|g| g.title.as_str()).collect::<Vec<_>>(), vec!["Tone", "Colour"]);
        assert_eq!(groups[0].names, vec!["exposure", "contrast", "highlights", "shadows", "whites", "blacks"]);
        assert_eq!(groups[1].names, vec!["wb", "temperature", "tint", "vibrance", "saturation"]);
        assert_eq!(rest, vec!["legacy_tone"], "only the Legacy toggle sits below the groups");
        let total: usize = groups.iter().map(|g| g.names.len()).sum();
        assert_eq!(total + 3 + 1, NodeKind::Develop.params().len());
        let (g, r) = param_groups(NodeKind::Exposure);
        assert!(g.is_empty());
        assert_eq!(r, vec!["ev"]);
    }

    #[test]
    fn node_boxes_show_at_most_four_rows_and_count_the_rest() {
        let mut g = Graph::new();
        let hs = g.add_node(NodeKind::HueSaturation, [0.0, 0.0]);
        let rows = limit_rows(all_rows(g.node(hs).unwrap(), IMG), MAX_BOX_ROWS);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[4].label, "\u{2026} +14");
        assert_eq!(rows.len(), box_row_count(NodeKind::HueSaturation));
        // A changed parameter later in the list takes a place among the four.
        let n = g.node_mut(hs).unwrap();
        n.set_param("blue_s", IMG, 0.5);
        let rows = limit_rows(all_rows(g.node(hs).unwrap(), IMG), MAX_BOX_ROWS);
        assert!(rows.iter().any(|r| r.changed), "{rows:?}");
        assert_eq!(rows.len(), 5);
        let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let rows = limit_rows(all_rows(g.node(e).unwrap(), IMG), MAX_BOX_ROWS);
        assert_eq!(rows.len(), 1, "short lists are untouched");
        assert_eq!(box_row_count(NodeKind::Exposure), 1);
    }

    #[test]
    fn limit_keeps_the_order_of_the_chosen_rows() {
        let row = |l: &str, c: bool| Row { key: None, label: l.to_owned(), value: String::new(), changed: c };
        let rows = vec![row("a", false), row("b", false), row("c", true), row("d", false), row("e", true), row("f", false)];
        let out = limit_rows(rows, 4);
        let labels: Vec<&str> = out.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b", "c", "e", "\u{2026} +2"]);
    }

    #[test]
    fn overlay_needs_the_toggle_and_a_connected_mask() {
        let mut g = Graph::new();
        let e = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let m = g.add_node(NodeKind::Generator, [0.0, 0.0]);
        assert_eq!(overlay_request(true, &g, Some(e)), None, "no mask connected");
        let port = g.expose_parameter(e, "ev", rawgraph_core::PortType::Images).unwrap();
        g.connect(m, e, port).unwrap();
        assert_eq!(overlay_request(true, &g, Some(e)), Some(m));
        assert_eq!(overlay_request(false, &g, Some(e)), None, "toggle off");
        assert_eq!(overlay_request(true, &g, None), None, "nothing selected");
        assert_eq!(overlay_request(true, &g, Some(m)), None, "the mask node itself has no mask input");
        // A drawing is the exception: while it is selected its mask is shown without the toggle.
        let b = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        assert_eq!(overlay_request(false, &g, Some(b)), Some(b));
        assert_eq!(overlay_request(true, &g, Some(b)), Some(b));
    }
}
