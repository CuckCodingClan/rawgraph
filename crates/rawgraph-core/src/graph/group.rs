//! Node groups and macros.
//!
//! A group is an organisational overlay on the flat graph: the nodes inside a
//! group keep their ids, their wires and all their values, and the evaluator,
//! the caches and the fingerprints see exactly the graph they saw before the
//! group was made. A [`NodeKind::Group`] node only adds a box in the editor,
//! the list of its members, the ports the box shows and the parameters it
//! publishes.
//!
//! * A **group input** is an inner input port that receives a wire from outside
//!   the group (or a required input nobody feeds yet); a **group output** is an
//!   inner output port that feeds something outside. Wires are always stored
//!   between real nodes, so a wire "into the group" is a wire into the inner
//!   port the group input stands for.
//! * Groups nest; a node belongs to at most one group.
//! * Every wire that crosses a group boundary has a port on every group it
//!   crosses (`check_groups`); connecting through the API adds missing ports.
//! * A **macro** is a group saved with everything inside it ([`MacroFile`]);
//!   inserting one makes an independent copy.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{Edge, Graph, GraphError, Node, NodeId, NodeKind, Subgraph};

/// Deepest nesting of groups (a group in a group in ...).
pub const MAX_GROUP_DEPTH: usize = 16;
/// Format version of macro files.
pub const MACRO_FORMAT_VERSION: u32 = 1;
/// Most nodes (group boxes included) a macro file may hold.
pub const MAX_MACRO_NODES: usize = 2000;
const MAX_LABEL_CHARS: usize = 128;
const MAX_MACRO_NAME_CHARS: usize = 80;
const MAX_DESCRIPTION_CHARS: usize = 600;

/// One port of a group box: the inner node port it stands for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupPort {
    pub label: String,
    pub node: NodeId,
    pub port: usize,
}

/// A parameter of an inner node that the group shows and edits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    pub label: String,
    pub node: NodeId,
    pub param: String,
}

/// What a Group node is: members, ports and published parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroupDef {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[u8; 3]>,
    /// Name of the macro this group was inserted from. Only a note: the group is a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Direct children (nodes and nested groups), in a stable order.
    pub members: Vec<NodeId>,
    pub inputs: Vec<GroupPort>,
    pub outputs: Vec<GroupPort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published: Vec<Published>,
}

/// One end of a wire as a graph level shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// A node (or closed group) box of the level and one of its ports.
    Node { node: NodeId, port: usize },
    /// A port of the group that is open: its input stub (as a source) or output stub (as a target).
    Boundary { port: usize },
}

/// A real wire as one graph level shows it: between the visible boxes of that level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LevelEdge {
    pub from: End,
    pub to: End,
    /// The stored wire between the real nodes.
    pub edge: Edge,
}

fn clean_label(label: &str, what: &str) -> Result<String, GraphError> {
    let t = label.trim();
    if t.is_empty() {
        return Err(GraphError::Group(format!("The {what} needs a name")));
    }
    if t.chars().count() > MAX_LABEL_CHARS || t.chars().any(char::is_control) {
        return Err(GraphError::Group(format!("The {what} name is too long or has control characters")));
    }
    Ok(t.to_owned())
}

fn bad_group(msg: impl Into<String>) -> GraphError {
    GraphError::Group(msg.into())
}

/// Makes an automatic label safe: no control characters, at most `MAX_LABEL_CHARS` characters, never empty.
fn tidy_label(text: &str, fallback: &str) -> String {
    let t: String = text.chars().filter(|c| !c.is_control()).collect();
    let t = t.trim();
    let t: String = t.chars().take(MAX_LABEL_CHARS).collect();
    let t = t.trim_end().to_owned();
    if t.is_empty() { fallback.to_owned() } else { t }
}

/// Label of input `port` of `node` for a group port: "Exposure: Images".
fn input_port_label(node: &Node, port: usize) -> String {
    let own = match node.kind.inputs().get(port) {
        Some(spec) => spec.label.to_owned(),
        None => port
            .checked_sub(node.kind.inputs().len())
            .and_then(|i| node.exposed_inputs().get(i))
            .and_then(Option::as_ref)
            .and_then(|p| Graph::parameter_input_label(node.kind, &p.parameter))
            .unwrap_or("Input")
            .to_owned(),
    };
    tidy_label(&format!("{}: {own}", node.kind.label()), "Input")
}

fn output_port_label(node: &Node, port: usize) -> String {
    let own = match node.kind.outputs().get(port) {
        Some(spec) => spec.label.to_owned(),
        None => port
            .checked_sub(node.kind.outputs().len())
            .and_then(|i| node.exposed_outputs().get(i))
            .and_then(Option::as_ref)
            .map_or_else(|| "Output".to_owned(), |p| p.label.clone()),
    };
    tidy_label(&format!("{}: {own}", node.kind.label()), "Output")
}

/// Appends " (2)", " (3)" ... to repeated labels so every port of a box reads differently.
fn dedupe_labels(ports: &mut [GroupPort]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in ports.iter_mut() {
        let n = seen.entry(p.label.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            let suffix = format!(" ({n})");
            let room = MAX_LABEL_CHARS.saturating_sub(suffix.chars().count());
            let base: String = p.label.chars().take(room).collect();
            p.label = format!("{}{suffix}", base.trim_end());
        }
    }
}

impl Graph {
    /// True if the graph holds at least one group.
    pub fn has_groups(&self) -> bool {
        self.nodes.values().any(|n| n.group.is_some())
    }

    /// `child -> its group` for every node that is inside a group.
    pub fn parent_map(&self) -> HashMap<NodeId, NodeId> {
        let mut map = HashMap::new();
        for n in self.nodes.values() {
            if let Some(g) = &n.group {
                for m in &g.members {
                    map.insert(*m, n.id);
                }
            }
        }
        map
    }

    /// The group `node` is directly inside (`None` at the top level).
    pub fn parent_of(&self, node: NodeId) -> Option<NodeId> {
        self.nodes.values().find(|n| n.group.as_ref().is_some_and(|g| g.members.contains(&node))).map(|n| n.id)
    }

    /// The groups around `node`, innermost first.
    pub fn ancestors(&self, node: NodeId) -> Vec<NodeId> {
        if !self.has_groups() {
            return Vec::new();
        }
        let map = self.parent_map();
        let mut out = Vec::new();
        let mut cur = node;
        while let Some(p) = map.get(&cur).copied() {
            if out.len() > MAX_GROUP_DEPTH + 1 || out.contains(&p) {
                break;
            }
            out.push(p);
            cur = p;
        }
        out
    }

    /// The nodes shown at a graph level: the top level (`None`) or the inside of a group.
    pub fn children(&self, level: Option<NodeId>) -> Vec<NodeId> {
        match level {
            Some(g) => self.node(g).and_then(Node::group).map(|d| d.members.clone()).unwrap_or_default(),
            None => {
                let inner: HashSet<NodeId> = self.parent_map().into_keys().collect();
                self.nodes.keys().copied().filter(|id| !inner.contains(id)).collect()
            }
        }
    }

    /// The real (non-group) nodes inside `node`; `[node]` for an ordinary node.
    pub fn leaves_of(&self, node: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_leaves(node, &mut out, 0);
        out
    }

    fn collect_leaves(&self, node: NodeId, out: &mut Vec<NodeId>, depth: usize) {
        match self.node(node).and_then(Node::group) {
            Some(g) if depth <= MAX_GROUP_DEPTH + 1 => {
                for m in &g.members {
                    self.collect_leaves(*m, out, depth + 1);
                }
            }
            Some(_) => {}
            None => {
                if self.nodes.contains_key(&node) {
                    out.push(node);
                }
            }
        }
    }

    /// Every node inside `node` at any depth (groups included), not `node` itself.
    pub fn descendants(&self, node: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut seen: HashSet<NodeId> = HashSet::new();
        seen.insert(node);
        let mut stack = vec![node];
        while let Some(n) = stack.pop() {
            if let Some(g) = self.node(n).and_then(Node::group) {
                for m in &g.members {
                    if seen.insert(*m) {
                        out.push(*m);
                        stack.push(*m);
                    }
                }
            }
        }
        out
    }

    /// The inner input a group input port stands for; every other port is its own.
    pub fn resolve_input(&self, node: NodeId, port: usize) -> (NodeId, usize) {
        match self.node(node).and_then(Node::group).and_then(|g| g.inputs.get(port)) {
            Some(p) => (p.node, p.port),
            None => (node, port),
        }
    }

    /// The inner output a group output port stands for; every other port is its own.
    pub fn resolve_output(&self, node: NodeId, port: usize) -> (NodeId, usize) {
        match self.node(node).and_then(Node::group).and_then(|g| g.outputs.get(port)) {
            Some(p) => (p.node, p.port),
            None => (node, port),
        }
    }

    /// What viewing `node` shows: for a group the node behind its first output port.
    pub fn view_target(&self, node: NodeId) -> Option<(NodeId, usize)> {
        let n = self.node(node)?;
        match n.group() {
            None => Some((node, 0)),
            Some(g) => g.outputs.first().map(|p| (p.node, p.port)),
        }
    }

    /// Adds the ports every group around a new wire needs.
    pub(super) fn ensure_boundary_ports(&mut self, e: Edge) {
        if !self.has_groups() {
            return;
        }
        let from_anc = self.ancestors(e.from);
        let to_anc = self.ancestors(e.to);
        for g in to_anc.iter().filter(|g| !from_anc.contains(g)) {
            self.add_port(*g, true, e.to, e.to_port);
        }
        for g in from_anc.iter().filter(|g| !to_anc.contains(g)) {
            self.add_port(*g, false, e.from, e.from_port);
        }
    }

    fn add_port(&mut self, group: NodeId, input: bool, node: NodeId, port: usize) {
        let Some(inner) = self.nodes.get(&node) else { return };
        let label = if input { input_port_label(inner, port) } else { output_port_label(inner, port) };
        let Some(g) = self.nodes.get_mut(&group).map(Arc::make_mut).and_then(|n| n.group.as_mut()) else { return };
        let list = if input { &mut g.inputs } else { &mut g.outputs };
        if list.iter().any(|p| p.node == node && p.port == port) {
            return;
        }
        // Only the new label is made unique: names the user gave other ports stay as they are.
        let mut unique = label.clone();
        let mut n = 1;
        while list.iter().any(|p| p.label == unique) {
            n += 1;
            let suffix = format!(" ({n})");
            let room = MAX_LABEL_CHARS.saturating_sub(suffix.chars().count());
            let base: String = label.chars().take(room).collect();
            unique = format!("{}{suffix}", base.trim_end());
        }
        list.push(GroupPort { label: unique, node, port });
    }

    /// Removes `id` with everything inside it, and every mention of it in groups.
    pub(super) fn remove_node_tree(&mut self, id: NodeId) {
        let mut doomed = vec![id];
        doomed.extend(self.descendants(id));
        let gone: HashSet<NodeId> = doomed.iter().copied().collect();
        for d in &doomed {
            self.nodes.remove(d);
        }
        self.edges.retain(|e| !gone.contains(&e.from) && !gone.contains(&e.to));
        self.scrub(&gone);
    }

    /// Drops removed nodes from the member lists, ports and published parameters of the groups left.
    fn scrub(&mut self, gone: &HashSet<NodeId>) {
        let touched: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| {
                n.group.as_ref().is_some_and(|g| {
                    g.members.iter().any(|m| gone.contains(m))
                        || g.inputs.iter().chain(&g.outputs).any(|p| gone.contains(&p.node))
                        || g.published.iter().any(|p| gone.contains(&p.node))
                })
            })
            .map(|n| n.id)
            .collect();
        for id in touched {
            if let Some(g) = self.nodes.get_mut(&id).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                g.members.retain(|m| !gone.contains(m));
                g.inputs.retain(|p| !gone.contains(&p.node));
                g.outputs.retain(|p| !gone.contains(&p.node));
                g.published.retain(|p| !gone.contains(&p.node));
            }
        }
        // A group without members has nothing left to show.
        let empty: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| n.group.as_ref().is_some_and(|g| g.members.is_empty()))
            .map(|n| n.id)
            .collect();
        for id in empty {
            self.remove_node_tree(id);
        }
    }

    // ------------------------------------------------------------ grouping

    /// Collapses sibling nodes (nodes of one level, groups included) into a new Group node.
    /// Wires that cross the selection become ports, wires inside stay inside; nothing is
    /// evaluated differently. Returns the id of the new group.
    pub fn group_nodes(&mut self, ids: &[NodeId], label: &str) -> Result<NodeId, GraphError> {
        let label = if label.trim().is_empty() { "Group".to_owned() } else { clean_label(label, "group")? };
        let mut sel: Vec<NodeId> = Vec::new();
        for id in ids {
            if self.nodes.contains_key(id) && !sel.contains(id) {
                sel.push(*id);
            }
        }
        if sel.is_empty() {
            return Err(bad_group("Select the nodes to group first"));
        }
        let pmap = self.parent_map();
        let parent = pmap.get(&sel[0]).copied();
        if sel.iter().any(|id| pmap.get(id).copied() != parent) {
            return Err(bad_group("Nodes that are in different groups cannot be grouped together"));
        }
        let outer_depth = parent.map_or(0, |p| self.ancestors(p).len() + 1);
        let inner_depth = sel.iter().map(|id| self.group_depth(*id)).max().unwrap_or(0);
        if outer_depth + 1 + inner_depth > MAX_GROUP_DEPTH {
            return Err(bad_group("Groups are nested too deeply"));
        }

        let mut leaves: Vec<NodeId> = Vec::new();
        for id in &sel {
            leaves.extend(self.leaves_of(*id));
        }
        let inside: HashSet<NodeId> = leaves.iter().copied().collect();
        // Stable order: top to bottom, then left to right.
        leaves.sort_by(|a, b| {
            let (na, nb) = (&self.nodes[a], &self.nodes[b]);
            na.pos[1].total_cmp(&nb.pos[1]).then(na.pos[0].total_cmp(&nb.pos[0])).then(a.cmp(b))
        });

        let mut inputs: Vec<GroupPort> = Vec::new();
        let mut outputs: Vec<GroupPort> = Vec::new();
        for id in &leaves {
            let n = &self.nodes[id];
            for port in 0..n.input_len() {
                if n.input_type(port).is_none() {
                    continue;
                }
                let crossing = self.edges.iter().any(|e| e.to == *id && e.to_port == port && !inside.contains(&e.from));
                let open_required = n.kind.inputs().get(port).is_some_and(|s| s.required)
                    && !self.edges.iter().any(|e| e.to == *id && e.to_port == port);
                if crossing || open_required {
                    inputs.push(GroupPort { label: input_port_label(n, port), node: *id, port });
                }
            }
            for port in 0..n.output_len() {
                if n.output_type(port).is_some()
                    && self.edges.iter().any(|e| e.from == *id && e.from_port == port && !inside.contains(&e.to))
                {
                    outputs.push(GroupPort { label: output_port_label(n, port), node: *id, port });
                }
            }
        }
        if outputs.is_empty() {
            // Nothing leaves the selection: the rightmost end of the chain is its output, so the group can be viewed.
            let sink = leaves
                .iter()
                .filter(|id| {
                    let n = &self.nodes[*id];
                    n.output_type(0).is_some() && !self.edges.iter().any(|e| e.from == **id)
                })
                .max_by(|a, b| {
                    let (na, nb) = (&self.nodes[*a], &self.nodes[*b]);
                    na.pos[0].total_cmp(&nb.pos[0]).then(b.cmp(a))
                });
            if let Some(id) = sink {
                outputs.push(GroupPort { label: output_port_label(&self.nodes[id], 0), node: *id, port: 0 });
            }
        }
        dedupe_labels(&mut inputs);
        dedupe_labels(&mut outputs);

        let pos = sel.iter().fold([f32::INFINITY, f32::INFINITY], |m, id| {
            let p = self.nodes[id].pos;
            [m[0].min(p[0]), m[1].min(p[1])]
        });
        let gid = self.add_node(NodeKind::Group, pos);
        let def = GroupDef { label, color: None, origin: None, members: sel.clone(), inputs, outputs, published: Vec::new() };
        if let Some(n) = self.nodes.get_mut(&gid).map(Arc::make_mut) {
            n.group = Some(def);
        }
        if let Some(p) = parent {
            if let Some(pg) = self.nodes.get_mut(&p).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                let at = pg.members.iter().position(|m| sel.contains(m)).unwrap_or(pg.members.len());
                pg.members.retain(|m| !sel.contains(m));
                pg.members.insert(at.min(pg.members.len()), gid);
            }
        }
        debug_assert!(self.check_groups().is_ok(), "grouping broke the group rules: {:?}", self.check_groups());
        Ok(gid)
    }

    /// Nesting depth of the group `id` itself (1 for a group of plain nodes, 0 for a plain node).
    fn group_depth(&self, id: NodeId) -> usize {
        match self.node(id).and_then(Node::group) {
            Some(g) => 1 + g.members.iter().map(|m| self.group_depth(*m)).max().unwrap_or(0).min(MAX_GROUP_DEPTH),
            None => 0,
        }
    }

    /// Dissolves a group: its members take its place in the level around it, with every
    /// wire, value and position exactly as it was. The group's ports and published
    /// parameters go away. Returns the former members.
    pub fn ungroup(&mut self, id: NodeId) -> Result<Vec<NodeId>, GraphError> {
        let parent = self.parent_of(id);
        let Some(def) = self.nodes.get(&id).and_then(|n| n.group.clone()) else {
            return Err(bad_group("This node is not a group"));
        };
        self.nodes.remove(&id);
        // The id is never reused: UI state keyed by node id (views, pins, timings) must not meet a new node.
        if let Some(p) = parent {
            if let Some(pg) = self.nodes.get_mut(&p).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                let at = pg.members.iter().position(|m| *m == id).unwrap_or(pg.members.len());
                pg.members.retain(|m| *m != id);
                for (k, m) in def.members.iter().enumerate() {
                    pg.members.insert((at + k).min(pg.members.len()), *m);
                }
            }
        }
        debug_assert!(self.check_groups().is_ok(), "ungrouping broke the group rules: {:?}", self.check_groups());
        Ok(def.members)
    }

    /// Puts nodes that are at the top level into the group `group` (new nodes added while a group is open).
    /// Refuses (and changes nothing) when a node is the group itself or one of its ancestors, or when the
    /// nesting would get deeper than [`MAX_GROUP_DEPTH`].
    pub fn adopt(&mut self, group: NodeId, ids: &[NodeId]) -> Result<(), GraphError> {
        if self.node(group).and_then(Node::group).is_none() {
            return Err(bad_group("This node is not a group"));
        }
        let around = self.ancestors(group);
        if ids.iter().any(|id| *id == group || around.contains(id)) {
            return Err(bad_group("A group cannot be put inside itself"));
        }
        let pmap = self.parent_map();
        let inside: HashSet<NodeId> = self.descendants(group).into_iter().collect();
        let fresh: Vec<NodeId> = ids
            .iter()
            .copied()
            .filter(|id| !pmap.contains_key(id) && !inside.contains(id) && self.nodes.contains_key(id))
            .collect();
        let inner = fresh.iter().map(|id| self.group_depth(*id)).max().unwrap_or(0);
        if around.len() + 1 + inner > MAX_GROUP_DEPTH {
            return Err(bad_group("Groups are nested too deeply"));
        }
        if let Some(g) = self.nodes.get_mut(&group).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
            for id in &fresh {
                if !g.members.contains(id) {
                    g.members.push(*id);
                }
            }
        }
        // Wires between the adopted nodes and the rest now cross the boundary.
        let moved: HashSet<NodeId> = fresh.iter().flat_map(|id| self.leaves_of(*id)).collect();
        let crossing: Vec<Edge> = self.edges.iter().copied().filter(|e| moved.contains(&e.from) || moved.contains(&e.to)).collect();
        for e in crossing {
            self.ensure_boundary_ports(e);
        }
        Ok(())
    }

    /// Drops every group port that stands for input (`input`) or output port `port` of `node`
    /// (the port itself is gone, e.g. an exposed parameter input that was removed).
    pub(super) fn drop_group_ports_of(&mut self, node: NodeId, input: bool, port: usize) {
        if !self.has_groups() {
            return;
        }
        let touched: Vec<NodeId> = self
            .nodes
            .values()
            .filter(|n| {
                n.group.as_ref().is_some_and(|g| {
                    let list = if input { &g.inputs } else { &g.outputs };
                    list.iter().any(|p| p.node == node && p.port == port)
                })
            })
            .map(|n| n.id)
            .collect();
        for id in touched {
            if let Some(g) = self.nodes.get_mut(&id).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                let list = if input { &mut g.inputs } else { &mut g.outputs };
                list.retain(|p| !(p.node == node && p.port == port));
            }
        }
    }

    /// The nodes of `ids` that no other node of `ids` contains.
    pub fn top_level_of(&self, ids: &[NodeId]) -> Vec<NodeId> {
        let pmap = self.parent_map();
        ids.iter()
            .copied()
            .filter(|id| {
                let mut cur = *id;
                for _ in 0..=MAX_GROUP_DEPTH + 1 {
                    match pmap.get(&cur) {
                        Some(p) if ids.contains(p) => return false,
                        Some(p) => cur = *p,
                        None => break,
                    }
                }
                true
            })
            .collect()
    }

    fn group_mut(&mut self, id: NodeId) -> Result<&mut GroupDef, GraphError> {
        self.nodes
            .get_mut(&id)
            .map(Arc::make_mut)
            .and_then(|n| n.group.as_mut())
            .ok_or_else(|| bad_group("This node is not a group"))
    }

    pub fn set_group_label(&mut self, id: NodeId, label: &str) -> Result<(), GraphError> {
        let label = clean_label(label, "group")?;
        self.group_mut(id)?.label = label;
        Ok(())
    }

    pub fn set_group_color(&mut self, id: NodeId, color: Option<[u8; 3]>) -> Result<(), GraphError> {
        self.group_mut(id)?.color = color;
        Ok(())
    }

    /// Renames a port of a group box.
    pub fn set_group_port_label(&mut self, id: NodeId, input: bool, index: usize, label: &str) -> Result<(), GraphError> {
        let label = clean_label(label, "port")?;
        let g = self.group_mut(id)?;
        let list = if input { &mut g.inputs } else { &mut g.outputs };
        list.get_mut(index).ok_or_else(|| bad_group("This group has no such port"))?.label = label;
        Ok(())
    }

    /// Adds a port to a group for an inner port that has no wire across the boundary yet.
    pub fn expose_group_port(&mut self, group: NodeId, input: bool, node: NodeId, port: usize) -> Result<usize, GraphError> {
        if !self.descendants(group).contains(&node) {
            return Err(bad_group("The node is not inside this group"));
        }
        let n = self.node(node).ok_or(GraphError::UnknownNode(node))?;
        if n.kind == NodeKind::Group {
            return Err(bad_group("A port belongs to an inner node, not to a nested group"));
        }
        let ok = if input { n.input_type(port).is_some() } else { n.output_type(port).is_some() };
        if !ok {
            return Err(GraphError::NoSuchPort { node, port });
        }
        // The nested groups on the way need the port too.
        let mut chain: Vec<NodeId> = self.ancestors(node).into_iter().take_while(|g| *g != group).collect();
        chain.push(group);
        for g in &chain {
            self.add_port(*g, input, node, port);
        }
        let g = self.nodes.get(&group).and_then(|n| n.group.as_ref()).ok_or_else(|| bad_group("This node is not a group"))?;
        let list = if input { &g.inputs } else { &g.outputs };
        list.iter().position(|p| p.node == node && p.port == port).ok_or_else(|| bad_group("The port could not be added"))
    }

    /// Removes a group port and every wire that crosses the boundary through it.
    pub fn remove_group_port(&mut self, group: NodeId, input: bool, index: usize) -> Result<(), GraphError> {
        let inside: HashSet<NodeId> = self.descendants(group).into_iter().collect();
        let port = {
            let g = self.nodes.get(&group).and_then(|n| n.group.as_ref()).ok_or_else(|| bad_group("This node is not a group"))?;
            let list = if input { &g.inputs } else { &g.outputs };
            list.get(index).cloned().ok_or_else(|| bad_group("This group has no such port"))?
        };
        self.edges.retain(|e| {
            if input {
                !(e.to == port.node && e.to_port == port.port && !inside.contains(&e.from))
            } else {
                !(e.from == port.node && e.from_port == port.port && !inside.contains(&e.to))
            }
        });
        let g = self.group_mut(group)?;
        if input {
            g.inputs.remove(index);
        } else {
            g.outputs.remove(index);
        }
        Ok(())
    }

    // ------------------------------------------------------------ published parameters

    /// Publishes parameter `param` of the inner node `node` on `group`; returns its index.
    pub fn publish_param(&mut self, group: NodeId, node: NodeId, param: &str, label: Option<&str>) -> Result<usize, GraphError> {
        if !self.descendants(group).contains(&node) {
            return Err(bad_group("The node is not inside this group"));
        }
        let n = self.node(node).ok_or(GraphError::UnknownNode(node))?;
        let Some(spec) = n.kind.param_spec(param) else {
            return Err(bad_group(format!("{} has no parameter \"{param}\"", n.kind.label())));
        };
        let label = match label {
            Some(l) => clean_label(l, "published parameter")?,
            None => spec.label.to_owned(),
        };
        let g = self.group_mut(group)?;
        if g.published.iter().any(|p| p.node == node && p.param == param) {
            return Err(bad_group("This parameter is already published"));
        }
        g.published.push(Published { label, node, param: param.to_owned() });
        Ok(g.published.len() - 1)
    }

    pub fn unpublish_param(&mut self, group: NodeId, index: usize) -> Result<(), GraphError> {
        let g = self.group_mut(group)?;
        if index >= g.published.len() {
            return Err(bad_group("There is no such published parameter"));
        }
        g.published.remove(index);
        Ok(())
    }

    pub fn rename_published(&mut self, group: NodeId, index: usize, label: &str) -> Result<(), GraphError> {
        let label = clean_label(label, "published parameter")?;
        let g = self.group_mut(group)?;
        g.published.get_mut(index).ok_or_else(|| bad_group("There is no such published parameter"))?.label = label;
        Ok(())
    }

    /// Index of the published entry for `node` / `param` on `group`.
    pub fn published_index(&self, group: NodeId, node: NodeId, param: &str) -> Option<usize> {
        self.node(group)?.group()?.published.iter().position(|p| p.node == node && p.param == param)
    }

    // ------------------------------------------------------------ levels

    /// The wires a graph level shows, between its visible boxes (the inside of a closed group
    /// is hidden, wires to the outside of an open group end at its port stubs).
    pub fn level_edges(&self, level: Option<NodeId>) -> Vec<LevelEdge> {
        let pmap = self.parent_map();
        let rep = |leaf: NodeId| -> Option<NodeId> {
            let mut cur = leaf;
            for _ in 0..=MAX_GROUP_DEPTH + 1 {
                let p = pmap.get(&cur).copied();
                if p == level {
                    return Some(cur);
                }
                cur = p?;
            }
            None
        };
        let level_def = level.and_then(|l| self.node(l)).and_then(Node::group);
        let mut out = Vec::new();
        for e in &self.edges {
            let (rf, rt) = (rep(e.from), rep(e.to));
            let from = match rf {
                Some(r) => {
                    let port = if r == e.from {
                        Some(e.from_port)
                    } else {
                        self.node(r).and_then(Node::group).and_then(|g| g.outputs.iter().position(|p| p.node == e.from && p.port == e.from_port))
                    };
                    port.map(|port| End::Node { node: r, port })
                }
                None => level_def
                    .and_then(|g| g.inputs.iter().position(|p| p.node == e.to && p.port == e.to_port))
                    .map(|port| End::Boundary { port }),
            };
            let to = match rt {
                Some(r) => {
                    let port = if r == e.to {
                        Some(e.to_port)
                    } else {
                        self.node(r).and_then(Node::group).and_then(|g| g.inputs.iter().position(|p| p.node == e.to && p.port == e.to_port))
                    };
                    port.map(|port| End::Node { node: r, port })
                }
                None => level_def
                    .and_then(|g| g.outputs.iter().position(|p| p.node == e.from && p.port == e.from_port))
                    .map(|port| End::Boundary { port }),
            };
            if rf.is_none() && rt.is_none() || (rf.is_some() && rf == rt) {
                continue;
            }
            if let (Some(from), Some(to)) = (from, to) {
                out.push(LevelEdge { from, to, edge: *e });
            }
        }
        out
    }

    /// The group box (at `level`) that contains `leaf`, or `leaf` itself when it is shown there.
    pub fn visible_at(&self, level: Option<NodeId>, leaf: NodeId) -> Option<NodeId> {
        let pmap = self.parent_map();
        let mut cur = leaf;
        for _ in 0..=MAX_GROUP_DEPTH + 1 {
            let p = pmap.get(&cur).copied();
            if p == level {
                return Some(cur);
            }
            cur = p?;
        }
        None
    }

    // ------------------------------------------------------------ repair

    /// Brings the group structure of a graph loaded from a project file into a state that passes
    /// `check_groups`, changing as little as possible: dangling or duplicate members, ports and
    /// published parameters are dropped, missing boundary ports are added, bad names are cleaned,
    /// and empty, cyclic or too deeply nested groups are dissolved (their nodes stay, in the level
    /// around the group). Groups are an overlay, so nothing evaluates differently afterwards.
    /// Returns one note per kind of repair.
    pub(super) fn repair_groups(&mut self) -> Vec<String> {
        #[derive(Default)]
        struct Tally(BTreeMap<&'static str, (usize, BTreeSet<u64>)>);
        impl Tally {
            fn add(&mut self, what: &'static str, group: NodeId, n: usize) {
                if n > 0 {
                    let e = self.0.entry(what).or_default();
                    e.0 += n;
                    e.1.insert(group.0);
                }
            }
        }
        let mut tally = Tally::default();
        let gids: Vec<NodeId> = self.nodes.values().filter(|n| n.kind == NodeKind::Group && n.group.is_some()).map(|n| n.id).collect();
        if gids.is_empty() {
            return Vec::new();
        }
        let bad_text = |t: &str| t.trim().is_empty() || t.chars().count() > MAX_LABEL_CHARS || t.chars().any(char::is_control);

        // 1. Names.
        for id in &gids {
            let Some(g) = self.nodes.get_mut(id).map(Arc::make_mut).and_then(|n| n.group.as_mut()) else { continue };
            let mut fixed = 0;
            let mut fix = |t: &mut String, fallback: &str| {
                if bad_text(t) {
                    *t = tidy_label(t, fallback);
                    fixed += 1;
                }
            };
            fix(&mut g.label, "Group");
            for p in &mut g.inputs {
                fix(&mut p.label, "Input");
            }
            for p in &mut g.outputs {
                fix(&mut p.label, "Output");
            }
            for p in &mut g.published {
                fix(&mut p.label, "Parameter");
            }
            if g.origin.as_ref().is_some_and(|o| o.chars().count() > MAX_LABEL_CHARS || o.chars().any(char::is_control)) {
                g.origin = None;
                fixed += 1;
            }
            tally.add("cleaned names", *id, fixed);
        }

        // 2. Members: only existing nodes, each listed once and claimed by one group.
        let mut claimed: HashSet<NodeId> = HashSet::new();
        for id in &gids {
            let members = self.nodes.get(id).and_then(|n| n.group.as_ref()).map(|g| g.members.clone()).unwrap_or_default();
            let mut kept = Vec::with_capacity(members.len());
            for m in &members {
                if *m != *id && self.nodes.contains_key(m) && claimed.insert(*m) {
                    kept.push(*m);
                }
            }
            tally.add("dropped invalid members", *id, members.len() - kept.len());
            if let Some(g) = self.nodes.get_mut(id).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                g.members = kept;
            }
        }

        // 3. Empty, cyclic and too deeply nested groups are dissolved.
        let mut pmap = self.parent_map();
        loop {
            let mut progress = false;
            let empty: Vec<NodeId> =
                self.nodes.values().filter(|n| n.group.as_ref().is_some_and(|g| g.members.is_empty())).map(|n| n.id).collect();
            for g in empty {
                self.dissolve_for_repair(g, &mut pmap);
                tally.add("dissolved empty groups", g, 1);
                progress = true;
            }
            let too_deep: BTreeSet<NodeId> = self
                .nodes
                .keys()
                .filter(|id| {
                    let mut cur = **id;
                    for _ in 0..MAX_GROUP_DEPTH {
                        match pmap.get(&cur) {
                            Some(p) => cur = *p,
                            None => return false,
                        }
                    }
                    pmap.contains_key(&cur)
                })
                .filter_map(|id| pmap.get(id).copied())
                .collect();
            for g in too_deep {
                if self.nodes.get(&g).is_some_and(|n| n.group.is_some()) {
                    self.dissolve_for_repair(g, &mut pmap);
                    tally.add("dissolved groups that contain each other or are nested too deeply", g, 1);
                    progress = true;
                }
            }
            if !progress {
                break;
            }
        }

        // 4. Ports and published parameters.
        let gids: Vec<NodeId> = self.nodes.values().filter(|n| n.group.is_some()).map(|n| n.id).collect();
        for id in &gids {
            let inside: HashSet<NodeId> = self.descendants(*id).into_iter().collect();
            let Some(def) = self.nodes.get(id).and_then(|n| n.group.clone()) else { continue };
            let valid = |p: &GroupPort, input: bool| {
                self.nodes.get(&p.node).is_some_and(|t| {
                    t.kind != NodeKind::Group
                        && inside.contains(&p.node)
                        && if input { t.input_type(p.port).is_some() } else { t.output_type(p.port).is_some() }
                })
            };
            let keep_ports = |list: &[GroupPort], input: bool| {
                let mut seen = HashSet::new();
                list.iter().filter(|p| valid(p, input) && seen.insert((p.node, p.port))).cloned().collect::<Vec<_>>()
            };
            let inputs = keep_ports(&def.inputs, true);
            let outputs = keep_ports(&def.outputs, false);
            let mut seen = HashSet::new();
            let published: Vec<Published> = def
                .published
                .iter()
                .filter(|p| {
                    inside.contains(&p.node)
                        && self.nodes.get(&p.node).is_some_and(|t| t.kind.param_spec(&p.param).is_some())
                        && seen.insert((p.node, p.param.clone()))
                })
                .cloned()
                .collect();
            tally.add("dropped invalid ports", *id, (def.inputs.len() - inputs.len()) + (def.outputs.len() - outputs.len()));
            tally.add("dropped invalid published parameters", *id, def.published.len() - published.len());
            if let Some(g) = self.nodes.get_mut(id).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                g.inputs = inputs;
                g.outputs = outputs;
                g.published = published;
            }
        }

        // 5. Every wire across a boundary needs its ports.
        let pmap = self.parent_map();
        let chain = |node: NodeId| -> Vec<NodeId> {
            let mut out = Vec::new();
            let mut cur = node;
            while let Some(p) = pmap.get(&cur) {
                if out.len() > MAX_GROUP_DEPTH {
                    break;
                }
                out.push(*p);
                cur = *p;
            }
            out
        };
        let mut wanted: Vec<(NodeId, bool, NodeId, usize)> = Vec::new();
        for e in &self.edges {
            let (Some(from), Some(to)) = (self.nodes.get(&e.from), self.nodes.get(&e.to)) else { continue };
            if from.kind == NodeKind::Group || to.kind == NodeKind::Group {
                continue;
            }
            let (fa, ta) = (chain(e.from), chain(e.to));
            wanted.extend(ta.iter().filter(|g| !fa.contains(g)).map(|g| (*g, true, e.to, e.to_port)));
            wanted.extend(fa.iter().filter(|g| !ta.contains(g)).map(|g| (*g, false, e.from, e.from_port)));
        }
        let mut added: BTreeMap<NodeId, usize> = BTreeMap::new();
        for (g, input, node, port) in wanted {
            let count = |s: &Graph| s.nodes.get(&g).and_then(|n| n.group.as_ref()).map_or(0, |d| d.inputs.len() + d.outputs.len());
            let had = count(self);
            self.add_port(g, input, node, port);
            if count(self) > had {
                *added.entry(g).or_default() += 1;
            }
        }
        for (g, n) in added {
            tally.add("added missing ports", g, n);
        }

        tally
            .0
            .into_iter()
            .map(|(what, (n, groups))| {
                let ids: Vec<String> = groups.iter().take(5).map(u64::to_string).collect();
                let more = if groups.len() > 5 { ", ..." } else { "" };
                format!("Groups (node {}{more}): repaired the file, {what} ({n})", ids.join(", "))
            })
            .collect()
    }

    /// Dissolves group `g` while repairing: its members move into the group around it, or to the
    /// top level. `pmap` (child -> group) is kept up to date.
    fn dissolve_for_repair(&mut self, g: NodeId, pmap: &mut HashMap<NodeId, NodeId>) {
        let members = self.nodes.get(&g).and_then(|n| n.group.as_ref()).map(|d| d.members.clone()).unwrap_or_default();
        self.nodes.remove(&g);
        self.edges.retain(|e| e.from != g && e.to != g);
        let parent = pmap.remove(&g);
        let target = parent.filter(|p| self.nodes.get(p).is_some_and(|n| n.group.is_some()));
        let movable: Vec<NodeId> = members.into_iter().filter(|m| Some(*m) != target && self.nodes.contains_key(m)).collect();
        for m in &movable {
            match target {
                Some(p) => pmap.insert(*m, p),
                None => pmap.remove(m),
            };
        }
        if let Some(p) = target {
            if let Some(pg) = self.nodes.get_mut(&p).map(Arc::make_mut).and_then(|n| n.group.as_mut()) {
                let at = pg.members.iter().position(|m| *m == g).unwrap_or(pg.members.len());
                pg.members.retain(|m| *m != g);
                for (k, m) in movable.iter().enumerate() {
                    if !pg.members.contains(m) {
                        pg.members.insert((at + k).min(pg.members.len()), *m);
                    }
                }
            }
            // A member that was the group around `g` is now at the top level.
            if pmap.get(&p) == Some(&g) {
                pmap.remove(&p);
            }
        }
    }

    // ------------------------------------------------------------ checks

    /// The group rules of a graph, for callers that edit graphs through the public API.
    pub fn check_group_structure(&self) -> Result<(), String> {
        self.check_groups()
    }

    /// The rules of a valid group structure (see the module docs); the message names the first break.
    pub(super) fn check_groups(&self) -> Result<(), String> {
        let mut parent: BTreeMap<NodeId, NodeId> = BTreeMap::new();
        for n in self.nodes.values() {
            let Some(g) = &n.group else { continue };
            if g.members.is_empty() {
                return Err(format!("group {} has no members", n.id.0));
            }
            for text in std::iter::once(&g.label).chain(g.inputs.iter().map(|p| &p.label)).chain(g.outputs.iter().map(|p| &p.label)).chain(g.published.iter().map(|p| &p.label)) {
                if text.trim().is_empty() || text.chars().count() > MAX_LABEL_CHARS || text.chars().any(char::is_control) {
                    return Err(format!("group {}: a name is empty, too long or has control characters", n.id.0));
                }
            }
            if g.origin.as_ref().is_some_and(|o| o.chars().count() > MAX_LABEL_CHARS || o.chars().any(char::is_control)) {
                return Err(format!("group {}: the origin note is not valid", n.id.0));
            }
            let mut seen = BTreeSet::new();
            for m in &g.members {
                if *m == n.id {
                    return Err(format!("group {} contains itself", n.id.0));
                }
                if !self.nodes.contains_key(m) {
                    return Err(format!("group {} lists the missing node {}", n.id.0, m.0));
                }
                if !seen.insert(*m) {
                    return Err(format!("group {} lists node {} twice", n.id.0, m.0));
                }
                if let Some(other) = parent.insert(*m, n.id) {
                    return Err(format!("node {} is inside both group {} and group {}", m.0, other.0, n.id.0));
                }
            }
        }
        // Nesting must end at the top level and stay within the depth limit (no group inside itself).
        for id in self.nodes.keys() {
            let mut cur = *id;
            let mut depth = 0;
            while let Some(p) = parent.get(&cur) {
                depth += 1;
                if depth > MAX_GROUP_DEPTH {
                    return Err(format!("node {} is nested deeper than {MAX_GROUP_DEPTH} groups, or the groups contain each other", id.0));
                }
                cur = *p;
            }
        }
        for n in self.nodes.values() {
            let Some(g) = &n.group else { continue };
            let inside: BTreeSet<NodeId> = self.descendants(n.id).into_iter().collect();
            for (what, list, input) in [("input", &g.inputs, true), ("output", &g.outputs, false)] {
                let mut seen = BTreeSet::new();
                for p in list {
                    let Some(target) = self.nodes.get(&p.node) else {
                        return Err(format!("group {}: {what} \"{}\" points at the missing node {}", n.id.0, p.label, p.node.0));
                    };
                    if target.kind == NodeKind::Group || !inside.contains(&p.node) {
                        return Err(format!("group {}: {what} \"{}\" does not point at a node inside the group", n.id.0, p.label));
                    }
                    let ok = if input { target.input_type(p.port).is_some() } else { target.output_type(p.port).is_some() };
                    if !ok {
                        return Err(format!("group {}: {what} \"{}\" points at a port that does not exist", n.id.0, p.label));
                    }
                    if !seen.insert((p.node, p.port)) {
                        return Err(format!("group {}: two {what}s point at the same inner port", n.id.0));
                    }
                }
            }
            let mut seen = BTreeSet::new();
            for p in &g.published {
                let Some(target) = self.nodes.get(&p.node) else {
                    return Err(format!("group {}: published parameter \"{}\" points at a missing node", n.id.0, p.label));
                };
                if !inside.contains(&p.node) || target.kind.param_spec(&p.param).is_none() {
                    return Err(format!("group {}: published parameter \"{}\" does not name a parameter of a node inside the group", n.id.0, p.label));
                }
                if !seen.insert((p.node, p.param.clone())) {
                    return Err(format!("group {}: parameter \"{}\" is published twice", n.id.0, p.param));
                }
            }
        }
        // Every wire across a boundary leaves or enters through a port of each group it crosses.
        let chain = |node: NodeId| -> Vec<NodeId> {
            let mut out = Vec::new();
            let mut cur = node;
            while let Some(p) = parent.get(&cur) {
                out.push(*p);
                cur = *p;
            }
            out
        };
        for e in &self.edges {
            let (fa, ta) = (chain(e.from), chain(e.to));
            for g in ta.iter().filter(|g| !fa.contains(g)) {
                let ok = self.nodes[g].group.as_ref().is_some_and(|d| d.inputs.iter().any(|p| p.node == e.to && p.port == e.to_port));
                if !ok {
                    return Err(format!("a wire enters group {} with no input port for it (node {} input {})", g.0, e.to.0, e.to_port));
                }
            }
            for g in fa.iter().filter(|g| !ta.contains(g)) {
                let ok = self.nodes[g].group.as_ref().is_some_and(|d| d.outputs.iter().any(|p| p.node == e.from && p.port == e.from_port));
                if !ok {
                    return Err(format!("a wire leaves group {} with no output port for it (node {} output {})", g.0, e.from.0, e.from_port));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------- macros

/// A group saved with everything inside it: the group node and its inner nodes as a
/// [`Subgraph`] (node 0 is the group), plus a name and a description. Inserting a macro
/// makes an independent copy; editing the file later never changes existing copies.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MacroFile {
    format_version: u32,
    pub name: String,
    #[serde(default)]
    pub description: String,
    group: Subgraph,
}

fn clean_macro_name(name: &str) -> Result<String, GraphError> {
    let t = name.trim();
    if t.is_empty() || t.chars().count() > MAX_MACRO_NAME_CHARS || t.chars().any(char::is_control) {
        return Err(bad_group("A macro needs a name of 1 to 80 characters"));
    }
    Ok(t.to_owned())
}

impl MacroFile {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("macro serializes to JSON")
    }

    pub fn from_json(text: &str) -> Result<MacroFile, GraphError> {
        Self::from_json_with_notes(text).map(|(m, _)| m)
    }

    /// Parses and validates a macro file; the notes come from migrating an older inner sub-graph.
    pub fn from_json_with_notes(text: &str) -> Result<(MacroFile, Vec<String>), GraphError> {
        let mut value: serde_json::Value = serde_json::from_str(text).map_err(|e| GraphError::Parse(e.to_string()))?;
        if let Some(v) = value.get("format_version").and_then(serde_json::Value::as_u64) {
            if v > u64::from(MACRO_FORMAT_VERSION) {
                return Err(GraphError::UnsupportedVersion(u32::try_from(v).unwrap_or(u32::MAX)));
            }
        }
        // Refuse a huge file before anything walks its nodes.
        let too_many = |v: &serde_json::Value| v.get("nodes").and_then(serde_json::Value::as_array).is_some_and(|n| n.len() > MAX_MACRO_NODES);
        if value.get("group").is_some_and(too_many) {
            return Err(GraphError::Parse(format!("the macro has more than {MAX_MACRO_NODES} nodes")));
        }
        let mut notes = Vec::new();
        if let Some(inner) = value.get_mut("group") {
            if let Some(v) = inner.get("format_version").and_then(serde_json::Value::as_u64) {
                if v > u64::from(super::SUBGRAPH_FORMAT_VERSION) {
                    return Err(GraphError::UnsupportedSubgraphVersion(u32::try_from(v).unwrap_or(u32::MAX)));
                }
            }
            notes = crate::migrate::migrate_subgraph(inner)?;
        }
        let m: MacroFile = serde_json::from_value(value).map_err(|e| GraphError::Parse(e.to_string()))?;
        if m.format_version > MACRO_FORMAT_VERSION {
            return Err(GraphError::UnsupportedVersion(m.format_version));
        }
        clean_macro_name(&m.name).map_err(|_| GraphError::Parse("the macro has no valid name".into()))?;
        if m.description.chars().count() > MAX_DESCRIPTION_CHARS || m.description.contains('\0') {
            return Err(GraphError::Parse("the macro description is too long".into()));
        }
        if m.group.node_count() > MAX_MACRO_NODES {
            return Err(GraphError::Parse(format!("the macro has more than {MAX_MACRO_NODES} nodes")));
        }
        m.group.validate()?;
        match m.group.root_group() {
            Some(_) => {}
            None => return Err(GraphError::Parse("the macro does not start with a group".into())),
        }
        Ok((m, notes))
    }

    fn def(&self) -> Option<&super::SubGroup> {
        self.group.root_group()
    }

    /// Labels of the input ports of the macro's group, in order.
    pub fn input_labels(&self) -> Vec<String> {
        self.def().map(|g| g.inputs.iter().map(|p| p.label.clone()).collect()).unwrap_or_default()
    }

    pub fn output_labels(&self) -> Vec<String> {
        self.def().map(|g| g.outputs.iter().map(|p| p.label.clone()).collect()).unwrap_or_default()
    }

    pub fn published_labels(&self) -> Vec<String> {
        self.def().map(|g| g.published.iter().map(|p| p.label.clone()).collect()).unwrap_or_default()
    }

    /// Number of nodes in the macro, the group boxes included.
    pub fn node_count(&self) -> usize {
        self.group.node_count()
    }
}

impl Graph {
    /// Saves the group `id` with everything inside it as a macro.
    pub fn extract_macro(&self, id: NodeId, name: &str, description: &str) -> Result<MacroFile, GraphError> {
        if self.node(id).and_then(Node::group).is_none() {
            return Err(bad_group("Only a group can be saved as a macro"));
        }
        let name = clean_macro_name(name)?;
        let description = description.trim().to_owned();
        if description.chars().count() > MAX_DESCRIPTION_CHARS || description.contains('\0') {
            return Err(bad_group("The description is too long"));
        }
        let mut group = self.extract_subgraph(&[id]);
        // The same limit as when a macro is read: a macro that could not be loaded is not written.
        if group.node_count() > MAX_MACRO_NODES {
            return Err(bad_group(format!("A group with more than {MAX_MACRO_NODES} nodes cannot be saved as a macro")));
        }
        group.normalize_origin();
        Ok(MacroFile { format_version: MACRO_FORMAT_VERSION, name, description, group })
    }

    /// Inserts a copy of the macro with its group box at `pos`; returns the new group.
    /// The copy is independent: later changes of the macro file do not reach it.
    pub fn insert_macro(&mut self, m: &MacroFile, pos: [f32; 2]) -> Result<NodeId, GraphError> {
        if m.group.node_count() > MAX_MACRO_NODES {
            return Err(GraphError::Parse(format!("the macro has more than {MAX_MACRO_NODES} nodes")));
        }
        m.group.validate()?;
        let ids = self.insert_subgraph(&m.group, pos);
        let top = *ids.first().ok_or_else(|| bad_group("The macro is empty"))?;
        let name = m.name.clone();
        let g = self.group_mut(top)?;
        g.origin = Some(name);
        Ok(top)
    }
}

#[cfg(test)]
#[path = "group_tests.rs"]
mod tests;
