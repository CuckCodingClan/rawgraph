//! Undo/redo of the graph as a bounded stack of snapshots.
//!
//! The history tracks a *baseline*: the last committed graph. Call
//! [`History::commit_if_changed`] every frame; it records a step only when the
//! graph differs from the baseline and no gesture (slider drag, node drag) is
//! still in progress. So a whole drag is exactly one undo step.

use std::collections::VecDeque;

use rawgraph_core::Graph;

pub const MAX_STEPS: usize = 200;

pub struct History {
    baseline: Graph,
    undo: VecDeque<Graph>,
    redo: Vec<Graph>,
    /// Counts every change of the graph that went through the history (commit, undo, redo).
    changes: u64,
}

impl History {
    pub fn new(initial: &Graph) -> Self {
        Self { baseline: initial.clone(), undo: VecDeque::new(), redo: Vec::new(), changes: 0 }
    }

    /// Forgets all steps and starts over from `graph` (new/opened file).
    pub fn reset(&mut self, graph: &Graph) {
        self.baseline = graph.clone();
        self.undo.clear();
        self.redo.clear();
        self.changes += 1;
    }

    /// Number of recorded graph changes so far (for autosave).
    pub fn changes(&self) -> u64 {
        self.changes
    }

    /// Records `graph` as a new step if it differs from the last committed
    /// state. Does nothing while `gesture_active`. Returns true if a step was
    /// recorded.
    pub fn commit_if_changed(&mut self, graph: &Graph, gesture_active: bool) -> bool {
        if gesture_active || *graph == self.baseline {
            return false;
        }
        let previous = std::mem::replace(&mut self.baseline, graph.clone());
        self.undo.push_back(previous);
        while self.undo.len() > MAX_STEPS {
            self.undo.pop_front();
        }
        self.redo.clear();
        self.changes += 1;
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    #[cfg(test)]
    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// Steps back; returns the graph to restore (the live graph is replaced,
    /// so uncommitted edits are discarded).
    pub fn undo(&mut self) -> Option<Graph> {
        let previous = self.undo.pop_back()?;
        let undone = std::mem::replace(&mut self.baseline, previous.clone());
        self.redo.push(undone);
        self.changes += 1;
        Some(previous)
    }

    pub fn redo(&mut self) -> Option<Graph> {
        let next = self.redo.pop()?;
        let before = std::mem::replace(&mut self.baseline, next.clone());
        self.undo.push_back(before);
        self.changes += 1;
        Some(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::{ImageId, NodeKind};

    fn graph_with_exposure() -> (Graph, rawgraph_core::NodeId) {
        let mut g = Graph::new();
        let id = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        (g, id)
    }

    #[test]
    fn a_whole_drag_is_one_step() {
        let (mut g, id) = graph_with_exposure();
        let mut h = History::new(&g);
        for i in 1..=30 {
            g.node_mut(id).unwrap().set_param("ev", ImageId(1), i as f32 * 0.1);
            assert!(!h.commit_if_changed(&g, true), "no step while dragging");
        }
        assert!(h.commit_if_changed(&g, false));
        assert_eq!(h.undo_len(), 1);
        assert!(!h.commit_if_changed(&g, false), "no change, no step");
        let back = h.undo().unwrap();
        assert_eq!(back.node(id).unwrap().param("ev", ImageId(1)), 0.0);
        assert!(h.undo().is_none());
        let again = h.redo().unwrap();
        assert!((again.node(id).unwrap().param("ev", ImageId(1)) - 3.0).abs() < 1e-5);
    }

    #[test]
    fn stack_is_bounded() {
        let (mut g, id) = graph_with_exposure();
        let mut h = History::new(&g);
        for i in 0..(MAX_STEPS + 50) {
            g.node_mut(id).unwrap().pos = [i as f32 + 1.0, 0.0];
            assert!(h.commit_if_changed(&g, false));
        }
        assert_eq!(h.undo_len(), MAX_STEPS);
        let mut steps = 0;
        while h.undo().is_some() {
            steps += 1;
        }
        assert_eq!(steps, MAX_STEPS);
    }

    #[test]
    fn new_edit_clears_redo() {
        let (mut g, id) = graph_with_exposure();
        let mut h = History::new(&g);
        g.node_mut(id).unwrap().pos = [1.0, 1.0];
        h.commit_if_changed(&g, false);
        g = h.undo().unwrap();
        assert!(h.can_redo());
        g.node_mut(id).unwrap().pos = [5.0, 5.0];
        h.commit_if_changed(&g, false);
        assert!(!h.can_redo());
    }

    #[test]
    fn reset_forgets_everything() {
        let (mut g, id) = graph_with_exposure();
        let mut h = History::new(&g);
        g.node_mut(id).unwrap().pos = [1.0, 1.0];
        h.commit_if_changed(&g, false);
        h.reset(&g);
        assert!(!h.can_undo() && !h.can_redo());
    }
}

#[cfg(test)]
mod memory_tests {
    use std::collections::HashSet;

    use rawgraph_core::{BrushStroke, ImageId, NodeData, NodeKind};

    use super::*;

    /// Estimate by counting: a fixed 512 bytes per node plus, for stroke data
    /// of images 1 and 2, 8 bytes per point and 32 bytes per stroke.
    fn node_bytes(node: &rawgraph_core::Node) -> usize {
        let strokes = [ImageId(1), ImageId(2)].into_iter().map(|image| match node.data(image) {
            Some(NodeData::Strokes(s)) => s.iter().map(|k| 32 + k.points.len() * 8).sum(),
            _ => 0usize,
        });
        512 + strokes.sum::<usize>()
    }

    fn snapshots(h: &History) -> impl Iterator<Item = &Graph> {
        std::iter::once(&h.baseline).chain(h.undo.iter()).chain(h.redo.iter())
    }

    /// `(shared, unshared)`: bytes of node data kept by the history when every
    /// node allocation counts once (what is really held), and when every
    /// snapshot would hold private copies of all its nodes (the old full copies).
    fn retained_bytes(h: &History) -> (usize, usize) {
        let mut seen = HashSet::new();
        let (mut shared, mut unshared) = (0, 0);
        for g in snapshots(h) {
            for n in g.nodes() {
                let bytes = node_bytes(n);
                unshared += bytes;
                if g.node_identity(n.id).is_some_and(|p| seen.insert(p)) {
                    shared += bytes;
                }
            }
        }
        (shared, unshared)
    }

    #[test]
    fn two_hundred_parameter_edits_share_the_big_draw_node() {
        let mut g = Graph::new();
        let draw = g.add_node(NodeKind::Draw, [0.0, 0.0]);
        let exposure = g.add_node(NodeKind::Exposure, [300.0, 0.0]);
        // 3,000 strokes of 50 points: the data limit is 2,000 strokes per image, so two images carry 1,500 each.
        let strokes = |offset: usize| -> Vec<BrushStroke> {
            (0..1500)
                .map(|s| BrushStroke {
                    points: (0..50).map(|p| [((s + offset) % 97) as f32 / 97.0, p as f32 / 50.0]).collect(),
                    radius: 0.01,
                    hardness: 0.5,
                    flow: 1.0,
                    erase: false,
                })
                .collect()
        };
        assert!(g.node_mut(draw).unwrap().set_data(ImageId(1), Some(NodeData::Strokes(strokes(0)))));
        assert!(g.node_mut(draw).unwrap().set_data(ImageId(2), Some(NodeData::Strokes(strokes(1)))));
        let mut h = History::new(&g);
        for i in 0..MAX_STEPS {
            g.node_mut(exposure).unwrap().set_param("ev", ImageId(1), (i + 1) as f32 * 0.01);
            assert!(h.commit_if_changed(&g, false));
        }
        assert_eq!(h.undo_len(), MAX_STEPS);
        let (shared, unshared) = retained_bytes(&h);
        eprintln!("undo memory, 3000 strokes x 50 points, {MAX_STEPS} edits of another node: before (full copies) {unshared} bytes, after (shared nodes) {shared} bytes, factor {:.0}", unshared as f64 / shared as f64);
        let draw_bytes = node_bytes(g.node(draw).unwrap());
        assert!(draw_bytes > 1_000_000, "the Draw node is big ({draw_bytes} bytes)");
        assert!(shared < draw_bytes * 2, "one copy of the Draw node plus small nodes, got {shared}");
        assert!(unshared > draw_bytes * MAX_STEPS, "full copies would hold it in every step");
        // Every snapshot still has the full data, and undo/redo walk all steps.
        let back = h.undo().unwrap();
        assert!(matches!(back.node(draw).unwrap().data(ImageId(1)), Some(NodeData::Strokes(s)) if s.len() == 1500));
        assert!((back.node(exposure).unwrap().param("ev", ImageId(1)) - 1.99).abs() < 1e-4);
        assert!(h.redo().is_some());
    }

    #[test]
    fn an_edit_copies_only_the_node_it_changes() {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Exposure, [100.0, 0.0]);
        let mut h = History::new(&g);
        g.node_mut(a).unwrap().set_param("ev", ImageId(1), 1.0);
        assert!(h.commit_if_changed(&g, false));
        let before = h.undo.back().unwrap();
        assert_eq!(before.node_identity(b), g.node_identity(b), "the untouched node is shared");
        assert_ne!(before.node_identity(a), g.node_identity(a), "the edited node was copied");
        assert_eq!(before.node(a).unwrap().param("ev", ImageId(1)), 0.0, "the snapshot keeps the old value");
    }

    #[test]
    fn multi_node_edit_and_restore_graph_are_single_steps() {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Exposure, [100.0, 0.0]);
        let mut h = History::new(&g);
        // Two nodes changed in the same frame: one step, both restored by undo.
        g.node_mut(a).unwrap().set_param("ev", ImageId(1), 1.0);
        g.node_mut(b).unwrap().pos = [5.0, 5.0];
        assert!(h.commit_if_changed(&g, false));
        // A restored whole graph (new nodes, other edges) is one step as well.
        let mut other = Graph::new();
        other.add_node(NodeKind::Source, [0.0, 0.0]);
        assert!(h.commit_if_changed(&other, false));
        assert_eq!(h.undo_len(), 2);
        let first = h.undo().unwrap();
        assert!(first.node(a).unwrap().param("ev", ImageId(1)) == 1.0 && first.nodes().count() == 2 && first.nodes().all(|n| n.kind != NodeKind::Source));
        let zero = h.undo().unwrap();
        assert_eq!(zero.node(a).unwrap().param("ev", ImageId(1)), 0.0);
        assert_eq!(zero.node(b).unwrap().pos, [100.0, 0.0]);
        assert_eq!(h.redo().unwrap(), first);
    }
}
