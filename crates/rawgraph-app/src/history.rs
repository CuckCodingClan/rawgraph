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
