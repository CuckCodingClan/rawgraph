//! Multi-selection of graph nodes: the selected set, rubber-band hit testing
//! and the rules for deleting a selection. Pure logic, no UI.

use eframe::egui::Rect;
use rawgraph_core::{Graph, NodeId, NodeKind};

/// The selected nodes in the order they were selected; the last one is the
/// "primary" node the inspector shows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeSelection {
    ids: Vec<NodeId>,
}

impl NodeSelection {
    pub fn single(id: NodeId) -> Self {
        Self { ids: vec![id] }
    }

    pub fn ids(&self) -> &[NodeId] {
        &self.ids
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.ids.contains(&id)
    }

    /// The node selected last.
    pub fn primary(&self) -> Option<NodeId> {
        self.ids.last().copied()
    }

    pub fn clear(&mut self) {
        self.ids.clear();
    }

    pub fn set_single(&mut self, id: NodeId) {
        self.ids.clear();
        self.ids.push(id);
    }

    /// Adds `id` (or makes it the primary node when it is already selected).
    pub fn add(&mut self, id: NodeId) {
        self.ids.retain(|x| *x != id);
        self.ids.push(id);
    }

    /// Shift/Ctrl+click: removes a selected node, adds an unselected one.
    pub fn toggle(&mut self, id: NodeId) {
        if self.contains(id) {
            self.ids.retain(|x| *x != id);
        } else {
            self.ids.push(id);
        }
    }

    pub fn replace(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        self.ids.clear();
        for id in ids {
            if !self.ids.contains(&id) {
                self.ids.push(id);
            }
        }
    }

    /// Ctrl+A: every node, in graph order.
    pub fn select_all(&mut self, graph: &Graph) {
        self.ids = graph.nodes().map(|n| n.id).collect();
    }

    /// Drops ids of nodes that no longer exist.
    pub fn retain_existing(&mut self, graph: &Graph) {
        self.ids.retain(|id| graph.node(*id).is_some());
    }
}

/// Nodes whose box touches the rubber band, in the order given.
pub fn band_hits(nodes: impl IntoIterator<Item = (NodeId, Rect)>, band: Rect) -> Vec<NodeId> {
    nodes.into_iter().filter(|(_, r)| r.intersects(band)).map(|(id, _)| id).collect()
}

/// What deleting a selection does.
#[derive(Clone, Debug, PartialEq)]
pub struct DeletePlan {
    pub delete: Vec<NodeId>,
    /// A Source that was spared because it is the last one.
    pub kept_source: Option<NodeId>,
}

/// The last Source node of a graph is never deleted. When the selection holds
/// all Sources, the first of them (in graph order) stays.
pub fn plan_delete(graph: &Graph, ids: &[NodeId]) -> DeletePlan {
    let mut delete: Vec<NodeId> = Vec::new();
    for id in ids {
        if graph.node(*id).is_some() && !delete.contains(id) {
            delete.push(*id);
        }
    }
    let sources: Vec<NodeId> = graph.nodes().filter(|n| n.kind == NodeKind::Source).map(|n| n.id).collect();
    let doomed_sources: Vec<NodeId> = sources.iter().copied().filter(|s| delete.contains(s)).collect();
    let mut kept_source = None;
    if !sources.is_empty() && doomed_sources.len() == sources.len() {
        let keep = doomed_sources[0];
        delete.retain(|id| *id != keep);
        kept_source = Some(keep);
    }
    DeletePlan { delete, kept_source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{pos2, vec2};

    fn graph3() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Exposure, [300.0, 0.0]);
        let c = g.add_node(NodeKind::Output, [600.0, 0.0]);
        (g, a, b, c)
    }

    #[test]
    fn toggling_adds_and_removes_and_the_last_is_primary() {
        let (_, a, b, c) = graph3();
        let mut s = NodeSelection::default();
        assert_eq!(s.primary(), None);
        s.toggle(a);
        s.toggle(b);
        s.toggle(c);
        assert_eq!((s.len(), s.primary()), (3, Some(c)));
        s.toggle(c);
        assert_eq!((s.len(), s.primary()), (2, Some(b)), "the previous one becomes primary");
        s.add(a);
        assert_eq!(s.primary(), Some(a), "adding a selected node makes it primary");
        assert_eq!(s.len(), 2);
        s.set_single(c);
        assert_eq!((s.ids(), s.primary()), (&[c][..], Some(c)));
        s.replace([a, a, b]);
        assert_eq!(s.ids(), &[a, b], "no duplicates");
    }

    #[test]
    fn select_all_and_stale_ids() {
        let (mut g, a, b, c) = graph3();
        let mut s = NodeSelection::default();
        s.select_all(&g);
        assert_eq!(s.ids(), &[a, b, c]);
        g.remove_node(b);
        s.retain_existing(&g);
        assert_eq!(s.ids(), &[a, c]);
    }

    #[test]
    fn the_rubber_band_selects_touched_boxes_only() {
        let (_, a, b, c) = graph3();
        let boxes = [(a, Rect::from_min_size(pos2(0.0, 0.0), vec2(196.0, 80.0))), (b, Rect::from_min_size(pos2(300.0, 0.0), vec2(196.0, 80.0))), (c, Rect::from_min_size(pos2(600.0, 0.0), vec2(196.0, 80.0)))];
        let band = Rect::from_min_max(pos2(150.0, 20.0), pos2(350.0, 60.0));
        assert_eq!(band_hits(boxes, band), vec![a, b], "touching a corner is enough");
        let far = Rect::from_min_max(pos2(210.0, 200.0), pos2(250.0, 220.0));
        assert!(band_hits(boxes, far).is_empty());
        // A band drawn right-to-left is the same rectangle once normalised.
        let flipped = Rect::from_two_pos(pos2(350.0, 60.0), pos2(150.0, 20.0));
        assert_eq!(band_hits(boxes, flipped), vec![a, b]);
    }

    #[test]
    fn the_last_source_survives_a_multi_delete() {
        let (mut g, a, b, c) = graph3();
        let p = plan_delete(&g, &[a, b, c]);
        assert_eq!(p.delete, vec![b, c]);
        assert_eq!(p.kept_source, Some(a));
        // A lone last Source: nothing is deleted at all.
        let p = plan_delete(&g, &[a]);
        assert!(p.delete.is_empty() && p.kept_source == Some(a));
        // Two Sources: deleting both keeps the first; deleting one is fine.
        let a2 = g.add_node(NodeKind::Source, [0.0, 200.0]);
        let p = plan_delete(&g, &[a2, a]);
        assert_eq!((p.delete, p.kept_source), (vec![a2], Some(a)));
        let p = plan_delete(&g, &[a2]);
        assert_eq!((p.delete, p.kept_source), (vec![a2], None));
        // Unknown ids and repeats are ignored.
        let p = plan_delete(&g, &[b, b, NodeId(999)]);
        assert_eq!(p.delete, vec![b]);
    }
}
