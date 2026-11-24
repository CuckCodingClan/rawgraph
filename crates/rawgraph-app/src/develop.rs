//! Pure rules behind the Develop and Capture EV inspector blocks: the texts
//! for what is applied to the selected image, seeding of the custom white
//! balance, and the absolute <-> relative mapping of the capture EV.

use rawgraph_core::{BaselineSource, CaptureInfo, Graph, ImageId, Node, NodeId, NodeKind};

/// Temperature assumed when a file has no as-shot white balance.
pub const ASSUMED_KELVIN: f32 = 6500.0;

/// "As shot: 5250 K, tint +8", or "As shot: unknown — using 6500 K".
pub fn as_shot_line(capture: Option<&CaptureInfo>) -> String {
    match capture.and_then(|c| c.as_shot_kelvin).filter(|k| k.is_finite() && *k > 0.0) {
        Some(k) => {
            let tint = capture.and_then(|c| c.as_shot_tint).filter(|t| t.is_finite()).unwrap_or(0.0);
            format!("As shot: {k:.0} K, tint {tint:+.0}")
        }
        None => format!("As shot: unknown \u{2014} using {ASSUMED_KELVIN:.0} K"),
    }
}

/// What the baseline exposure does for the selected image.
pub fn baseline_line(source: BaselineSource, gain_ev: f32) -> String {
    match source {
        BaselineSource::File => format!("{gain_ev:+.2} EV from the file"),
        BaselineSource::Default => format!("{gain_ev:+.2} EV default (file has none)"),
        BaselineSource::Off => "off".to_owned(),
        BaselineSource::NotRaw => "not applied: rendered file".to_owned(),
    }
}

/// "Total applied: +1.05 EV" (baseline plus brightness), or the reason nothing is applied.
pub fn total_line(source: BaselineSource, total_ev: f32) -> String {
    match source {
        BaselineSource::NotRaw => "not applied: rendered file".to_owned(),
        _ => format!("Total applied: {total_ev:+.2} EV"),
    }
}

/// Note shown for JPEG / PNG / TIFF sources.
pub const RENDERED_NOTE: &str = "Rendered file: the white balance is already baked in. The temperature here is relative to an assumed 6500 K.";

/// Everything the Develop block shows besides the sliders.
#[derive(Clone, Debug, PartialEq)]
pub struct DevelopView {
    pub as_shot: String,
    pub baseline: String,
    pub total: String,
    pub rendered_note: Option<&'static str>,
    /// The mode combo is on "Custom": temperature and tint are editable.
    pub custom: bool,
}

pub fn develop_view(custom: bool, capture: Option<&CaptureInfo>, rendered: bool, source: BaselineSource, baseline_ev: f32, total_ev: f32) -> DevelopView {
    DevelopView {
        as_shot: as_shot_line(capture),
        baseline: baseline_line(source, baseline_ev),
        total: total_line(source, total_ev),
        rendered_note: rendered.then_some(RENDERED_NOTE),
        custom,
    }
}

/// Values to seed temperature and tint with when the mode switches to Custom:
/// only while both are still at their defaults, so nothing is overwritten and
/// the picture does not jump.
pub fn seed_custom(temperature: f32, tint: f32, default_temperature: f32, default_tint: f32, as_shot: (f32, f32)) -> Option<(f32, f32)> {
    let at_default = (temperature - default_temperature).abs() < 0.5 && (tint - default_tint).abs() < 0.05;
    (at_default && as_shot.0.is_finite() && as_shot.1.is_finite()).then_some(as_shot)
}

/// Seeds `node` (a Develop node) after the switch to Custom. Returns true if
/// values were written.
pub fn seed_node(node: &mut Node, image: ImageId, as_shot: (f32, f32)) -> bool {
    let (Some(t), Some(ti)) = (node.kind.param_spec("temperature"), node.kind.param_spec("tint")) else { return false };
    let (t_def, ti_def) = (t.default, ti.default);
    match seed_custom(node.param("temperature", image), node.param("tint", image), t_def, ti_def, as_shot) {
        Some((k, tint)) => {
            node.set_param("temperature", image, k);
            node.set_param("tint", image, tint);
            true
        }
        None => false,
    }
}

// --------------------------------------------------------------- capture EV

/// Absolute EV100 of the image after all Capture EV changes up to and
/// including the node: `capture EV100 + delta`. `None` when the capture EV is unknown.
pub fn absolute_ev(capture_ev100: Option<f32>, delta_up_to_here: f32) -> Option<f32> {
    capture_ev100.map(|ev| ev + delta_up_to_here)
}

/// The `delta` of one Capture EV node that makes the absolute value at that
/// node equal `target`, given the changes upstream of it.
pub fn delta_for_absolute(capture_ev100: f32, upstream_delta: f32, target: f32) -> f32 {
    target - capture_ev100 - upstream_delta
}

/// Sum of the Capture EV changes upstream of `node` (without the node's own).
pub fn upstream_delta(graph: &Graph, node: NodeId, image: ImageId) -> f32 {
    let own = graph.node(node).filter(|n| n.kind == NodeKind::CaptureEv).map_or(0.0, |n| n.param("delta", image));
    graph.capture_ev_delta(node, image) - own
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(kelvin: Option<f32>, tint: Option<f32>) -> CaptureInfo {
        CaptureInfo { as_shot_kelvin: kelvin, as_shot_tint: tint, ..Default::default() }
    }

    #[test]
    fn as_shot_texts() {
        assert_eq!(as_shot_line(Some(&capture(Some(5250.0), Some(8.0)))), "As shot: 5250 K, tint +8");
        assert_eq!(as_shot_line(Some(&capture(Some(3200.4), Some(-12.0)))), "As shot: 3200 K, tint -12");
        assert_eq!(as_shot_line(Some(&capture(Some(5000.0), None))), "As shot: 5000 K, tint +0");
        assert_eq!(as_shot_line(Some(&capture(None, None))), "As shot: unknown \u{2014} using 6500 K");
        assert_eq!(as_shot_line(None), "As shot: unknown \u{2014} using 6500 K");
        assert!(as_shot_line(Some(&capture(Some(0.0), Some(1.0)))).contains("unknown"));
    }

    #[test]
    fn baseline_texts_per_source() {
        assert_eq!(baseline_line(BaselineSource::File, 0.35), "+0.35 EV from the file");
        assert_eq!(baseline_line(BaselineSource::Default, 1.0), "+1.00 EV default (file has none)");
        assert_eq!(baseline_line(BaselineSource::Off, 0.0), "off");
        assert_eq!(baseline_line(BaselineSource::NotRaw, 0.0), "not applied: rendered file");
        assert_eq!(baseline_line(BaselineSource::File, -0.2), "-0.20 EV from the file");
    }

    #[test]
    fn develop_view_maps_the_state() {
        let raw = capture(Some(5250.0), Some(8.0));
        let v = develop_view(false, Some(&raw), false, BaselineSource::File, 0.35, 1.05);
        assert_eq!(v.as_shot, "As shot: 5250 K, tint +8");
        assert_eq!(v.baseline, "+0.35 EV from the file");
        assert_eq!(v.total, "Total applied: +1.05 EV");
        assert!(v.rendered_note.is_none() && !v.custom);
        let v = develop_view(true, Some(&capture(None, None)), false, BaselineSource::Default, 0.35, 0.35);
        assert!(v.custom && v.as_shot.contains("unknown") && v.baseline.contains("default"));
        let v = develop_view(false, None, true, BaselineSource::NotRaw, 0.0, 0.0);
        assert_eq!(v.baseline, "not applied: rendered file");
        assert_eq!(v.total, "not applied: rendered file");
        assert!(v.rendered_note.is_some_and(|n| n.contains("baked in") && n.contains("6500 K")));
        let v = develop_view(false, Some(&raw), false, BaselineSource::Off, 0.0, 0.7);
        assert_eq!(v.baseline, "off");
        assert_eq!(v.total, "Total applied: +0.70 EV", "brightness still applies with the baseline off");
    }

    #[test]
    fn custom_values_are_seeded_only_while_still_default() {
        assert_eq!(seed_custom(6500.0, 0.0, 6500.0, 0.0, (5250.0, 8.0)), Some((5250.0, 8.0)));
        assert_eq!(seed_custom(4000.0, 0.0, 6500.0, 0.0, (5250.0, 8.0)), None, "temperature already edited");
        assert_eq!(seed_custom(6500.0, 12.0, 6500.0, 0.0, (5250.0, 8.0)), None, "tint already edited");
        assert_eq!(seed_custom(6500.0, 0.0, 6500.0, 0.0, (f32::NAN, 0.0)), None);
    }

    #[test]
    fn seeding_writes_the_node_once() {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Develop, [0.0, 0.0]);
        let img = ImageId(1);
        let n = g.node_mut(id).unwrap();
        assert!(seed_node(n, img, (5250.0, 8.0)));
        assert_eq!(n.param("temperature", img), 5250.0);
        assert_eq!(n.param("tint", img), 8.0);
        // A second switch keeps the values the user has by now.
        assert!(!seed_node(n, img, (3000.0, -5.0)));
        assert_eq!(n.param("temperature", img), 5250.0);
    }

    #[test]
    fn absolute_and_delta_map_into_each_other() {
        let ev = 12.0;
        assert_eq!(absolute_ev(Some(ev), 0.5), Some(12.5));
        assert_eq!(absolute_ev(None, 0.5), None);
        let d = delta_for_absolute(ev, 0.5, 14.0);
        assert!((d - 1.5).abs() < 1e-6);
        assert!((absolute_ev(Some(ev), 0.5 + d).unwrap() - 14.0).abs() < 1e-6, "round trip");
    }

    #[test]
    fn upstream_capture_ev_nodes_count_but_the_nodes_own_delta_does_not() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let a = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        let b = g.add_node(NodeKind::CaptureEv, [0.0, 0.0]);
        g.connect(src, a, 0).unwrap();
        g.connect(a, b, 0).unwrap();
        let img = ImageId(1);
        g.node_mut(a).unwrap().set_param("delta", img, 1.0);
        g.node_mut(b).unwrap().set_param("delta", img, -0.5);
        assert!((upstream_delta(&g, a, img)).abs() < 1e-6);
        assert!((upstream_delta(&g, b, img) - 1.0).abs() < 1e-6);
        // Typing 13 into node b with a capture EV of 12 must give 13 at b.
        let d = delta_for_absolute(12.0, upstream_delta(&g, b, img), 13.0);
        g.node_mut(b).unwrap().set_param("delta", img, d);
        let at_b = absolute_ev(Some(12.0), g.capture_ev_delta(b, img)).unwrap();
        assert!((at_b - 13.0).abs() < 1e-5, "{at_b}");
        // Unknown capture EV: no absolute value, only the delta.
        assert_eq!(absolute_ev(None, g.capture_ev_delta(b, img)), None);
    }
}
