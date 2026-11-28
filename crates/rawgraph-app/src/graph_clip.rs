//! Copy / paste of graph nodes as text. The text is `Subgraph::to_json()`;
//! per-image values are never part of it (the core leaves them out).

use rawgraph_core::{Graph, NodeId, Subgraph};

/// Clipboard text above this size is not even parsed.
pub const MAX_CLIP_BYTES: usize = 4 << 20;
/// Shift of a paste that has no pointer position, per repeated paste.
pub const PASTE_STEP: f32 = 48.0;

/// The text for the clipboard, or `None` when no node of `ids` exists.
pub fn copy_text(graph: &Graph, ids: &[NodeId]) -> Option<String> {
    let sub = graph.extract_subgraph(ids);
    (!sub.is_empty()).then(|| sub.to_json())
}

/// Parses clipboard text. The error is one line for the status bar.
#[cfg(test)]
pub fn parse_clip(text: &str) -> Result<Subgraph, String> {
    parse_clip_notes(text).map(|(s, _)| s)
}

/// Like [`parse_clip`], with the notes of the migration of an older group of nodes.
pub fn parse_clip_notes(text: &str) -> Result<(Subgraph, Vec<String>), String> {
    if text.len() > MAX_CLIP_BYTES {
        return Err("The clipboard text is too large to be nodes".to_owned());
    }
    let t = text.trim_start();
    if !t.starts_with('{') {
        return Err("The clipboard does not contain RawGraph nodes".to_owned());
    }
    match Subgraph::from_json_with_notes(text) {
        Ok((s, _)) if s.is_empty() => Err("The clipboard holds no nodes".to_owned()),
        Ok(s) => Ok(s),
        Err(e) => Err(format!("The clipboard does not contain RawGraph nodes ({e})")),
    }
}

/// Where a pasted group goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    /// Top-left corner of the group at this graph position (pointer or menu position).
    At([f32; 2]),
    /// Shifted from the original positions; `n` counts earlier pastes of the same text.
    Offset(u32),
}

/// Inserts `sub` as unconnected new nodes and returns their ids.
pub fn insert_at(graph: &mut Graph, sub: &Subgraph, anchor: Anchor) -> Vec<NodeId> {
    match anchor {
        Anchor::Offset(n) => {
            let d = PASTE_STEP * (n + 1) as f32;
            graph.insert_subgraph(sub, [d, d])
        }
        Anchor::At(p) => {
            let ids = graph.insert_subgraph(sub, [0.0, 0.0]);
            let (mut min_x, mut min_y) = (f32::INFINITY, f32::INFINITY);
            // Only the boxes that are shown count: a pasted group's inner nodes are hidden inside it.
            for id in &graph.top_level_of(&ids) {
                if let Some(n) = graph.node(*id) {
                    min_x = min_x.min(n.pos[0]);
                    min_y = min_y.min(n.pos[1]);
                }
            }
            if min_x.is_finite() && min_y.is_finite() {
                let (dx, dy) = (p[0] - min_x, p[1] - min_y);
                for id in &ids {
                    if let Some(n) = graph.node_mut(*id) {
                        n.pos[0] += dx;
                        n.pos[1] += dy;
                    }
                }
            }
            ids
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::{ImageId, NodeKind};

    fn chain() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [300.0, 40.0]);
        let sat = g.add_node(NodeKind::Saturation, [600.0, 40.0]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, sat, 0).unwrap();
        (g, src, exp, sat)
    }

    #[test]
    fn a_round_trip_keeps_kinds_edges_and_all_values() {
        let (mut g, _, exp, sat) = chain();
        g.node_mut(exp).unwrap().set_param("ev", ImageId(0), 1.25);
        let text = copy_text(&g, &[exp, sat]).unwrap();
        let sub = parse_clip(&text).unwrap();
        assert_eq!(sub.kinds(), vec![NodeKind::Exposure, NodeKind::Saturation]);
        let before = g.nodes().count();
        let ids = insert_at(&mut g, &sub, Anchor::Offset(0));
        assert_eq!(g.nodes().count(), before + 2);
        assert_eq!(g.input_of(ids[1], 0), Some(ids[0]), "the edge between the copied nodes is kept");
        assert_eq!(g.input_of(ids[0], 0), None, "never connected to the original upstream node");
        assert!((g.node(ids[0]).unwrap().param("ev", ImageId(0)) - 1.25).abs() < 1e-6);
    }

    #[test]
    fn per_image_values_are_not_copied() {
        let (mut g, _, exp, _) = chain();
        g.node_mut(exp).unwrap().mode = rawgraph_core::Mode::Individual;
        g.node_mut(exp).unwrap().set_param("ev", ImageId(7), 2.0);
        assert!(g.node(exp).unwrap().has_individual(ImageId(7)), "the source node has the per-image value");
        let sub = parse_clip(&copy_text(&g, &[exp]).unwrap()).unwrap();
        let ids = insert_at(&mut g, &sub, Anchor::Offset(0));
        let n = g.node(ids[0]).unwrap();
        assert!(!n.has_individual(ImageId(7)), "the other image's value stays behind");
        assert_eq!(n.individual_count(), 0);
    }

    #[test]
    fn the_offset_rule_and_the_pointer_rule() {
        let (mut g, _, exp, sat) = chain();
        let sub = parse_clip(&copy_text(&g, &[exp, sat]).unwrap()).unwrap();
        let a = insert_at(&mut g, &sub, Anchor::Offset(0));
        assert_eq!(g.node(a[0]).unwrap().pos, [300.0 + PASTE_STEP, 40.0 + PASTE_STEP]);
        let b = insert_at(&mut g, &sub, Anchor::Offset(1));
        assert_eq!(g.node(b[0]).unwrap().pos, [300.0 + 2.0 * PASTE_STEP, 40.0 + 2.0 * PASTE_STEP], "a repeated paste does not land on the first copy");
        let c = insert_at(&mut g, &sub, Anchor::At([1000.0, 500.0]));
        assert_eq!(g.node(c[0]).unwrap().pos, [1000.0, 500.0], "the group's top-left corner is at the pointer");
        assert_eq!(g.node(c[1]).unwrap().pos, [1300.0, 500.0], "relative layout is kept");
    }

    #[test]
    fn invalid_clipboard_text_is_refused_without_a_panic() {
        for bad in ["", "hello", "{}", "{\"format_version\":1}", "[1,2]", "{\"format_version\":1,\"nodes\":[],\"edges\":[]}", "{\"format_version\":1,\"nodes\":[{\"kind\":\"Nope\"}],\"edges\":[]}", &"x".repeat(MAX_CLIP_BYTES + 1)] {
            assert!(parse_clip(bad).is_err(), "{:?}", &bad[..bad.len().min(30)]);
        }
        assert!(copy_text(&Graph::new(), &[NodeId(3)]).is_none());
    }

    #[test]
    fn a_whole_selection_pasted_is_one_graph_change() {
        let (mut g, src, exp, sat) = chain();
        let sub = parse_clip(&copy_text(&g, &[src, exp, sat]).unwrap()).unwrap();
        let mut hist = crate::history::History::new(&g);
        insert_at(&mut g, &sub, Anchor::Offset(0));
        assert!(hist.commit_if_changed(&g, false));
        assert!(!hist.commit_if_changed(&g, false));
        assert!(hist.undo().is_some() && !hist.can_undo(), "one step takes the whole paste back");
    }
}
