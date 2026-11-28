//! Collection awareness: how many images each node carries, which images are
//! part of the viewed node's collection, and the Metadata Filter / Merge
//! statistics. Computed from metadata only (`Graph::contains`), off the UI
//! thread, latest request wins.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use rawgraph_core::graph::SPLIT_UNMATCHED_PORT;
use rawgraph_core::{CaptureInfo, Catalog, Graph, ImageId, Measure, NodeId, NodeKind};

pub type Captures = Arc<HashMap<ImageId, CaptureInfo>>;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FilterStats {
    /// Images that reach the filter.
    pub reaching: usize,
    /// Images the filter keeps.
    pub matching: usize,
    /// Reaching images without a value for the chosen measure.
    pub missing: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MergeStats {
    /// Images supplied by each input port (A..D), after port-order resolution.
    pub supplied: [usize; 4],
    /// Images contained in more than one input (resolved by port order).
    pub duplicates: usize,
    pub total: usize,
    /// Images the "Same file name" rule left out (0 for "Keep both").
    pub dropped: usize,
}

/// What a Layer node stacks: the images of every connected layer, in the order of that layer's input.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerStats {
    /// Index of the layer that drives the collection (the connected one with the most images).
    pub driver: Option<usize>,
    /// Images of each layer (`None`: nothing is connected to that input).
    pub lists: Vec<Option<Arc<Vec<ImageId>>>>,
}

impl LayerStats {
    /// Number of images in the output collection (the driver's).
    pub fn output_len(&self) -> usize {
        self.driver.and_then(|d| self.lists.get(d)).and_then(|l| l.as_ref()).map_or(0, |l| l.len())
    }

    /// The image each layer shows for the output image `image` (the same pairing as
    /// `Graph::layer_sources`): `None` when `image` is not in the output.
    pub fn sources(&self, image: ImageId) -> Option<Vec<Option<ImageId>>> {
        let driver = self.lists.get(self.driver?)?.as_ref()?;
        let k = driver.iter().position(|i| *i == image)?;
        Some(self.lists.iter().map(|l| l.as_ref().filter(|l| !l.is_empty()).map(|l| l[k % l.len()])).collect())
    }
}

/// Where the images of a Split by Metadata leave it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitStats {
    /// Images reaching the split.
    pub reaching: usize,
    /// Images leaving through each output port (six ranges, then Unmatched).
    pub per_port: [usize; 7],
    /// Images that reach the split but leave through no port (missing metadata, "Exclude").
    pub excluded: usize,
}

impl SplitStats {
    pub fn unmatched(&self) -> usize {
        self.per_port[SPLIT_UNMATCHED_PORT]
    }
}

#[derive(Clone, Debug, Default)]
pub struct CollectionResult {
    pub counts: HashMap<NodeId, usize>,
    /// Members of the viewed node's collection.
    pub view_members: HashSet<ImageId>,
    pub view_node: Option<NodeId>,
    pub filters: HashMap<NodeId, FilterStats>,
    pub merges: HashMap<NodeId, MergeStats>,
    pub splits: HashMap<NodeId, SplitStats>,
    pub layers: HashMap<NodeId, LayerStats>,
    /// Size of the image group each image of the viewed node stands for, when an HDR Merge at or above
    /// the viewed node makes groups (references only; singletons are left out).
    pub view_groups: HashMap<ImageId, usize>,
    pub millis: f32,
}

/// The collection entering input `port` of `node` (empty when nothing is connected).
fn input_members(graph: &Graph, catalog: &Catalog, node: NodeId, port: usize) -> Vec<ImageId> {
    let mut seen = HashSet::new();
    graph.input_edges(node, port)
        .flat_map(|edge| graph.members(edge.from, edge.from_port, catalog))
        .filter(|image| seen.insert(*image))
        .collect()
}

/// Membership used by the viewer and gallery. A Split shows its input
/// collection, regardless of which output port each image leaves through.
fn node_members(graph: &Graph, catalog: &Catalog, node: NodeId, kind: NodeKind) -> Vec<ImageId> {
    if kind == NodeKind::MetadataSplit { input_members(graph, catalog, node, 0) } else { graph.members(node, 0, catalog) }
}

pub fn view_members(graph: &Graph, ids: &[ImageId], captures: &Captures, view_node: NodeId) -> HashSet<ImageId> {
    let capture = |i: ImageId| captures.get(&i).cloned();
    let catalog = Catalog { images: ids, capture: &capture, memo: None };
    graph.node(view_node).into_iter().flat_map(|node| node_members(graph, &catalog, node.id, node.kind)).collect()
}

/// Pure computation of everything above. Every collection is listed once per node with
/// `Graph::members` (the catalog-based query), never asked image by image.
pub fn compute(graph: &Graph, ids: &[ImageId], captures: &Captures, view_node: Option<NodeId>) -> CollectionResult {
    let started = Instant::now();
    let capture = |i: ImageId| captures.get(&i).cloned();
    let catalog = Catalog { images: ids, capture: &capture, memo: None };
    let mut r = CollectionResult { view_node, ..Default::default() };
    for node in graph.nodes() {
        let own = node_members(graph, &catalog, node.id, node.kind);
        let n = own.len();
        r.counts.insert(node.id, n);
        if view_node == Some(node.id) {
            r.view_members = own.iter().copied().collect();
            if let Some(hdr) = graph.upstream_hdr_merge(node.id) {
                r.view_groups = graph
                    .image_groups(hdr, &catalog)
                    .into_iter()
                    .filter(|g| g.members.len() > 1 && r.view_members.contains(&g.reference))
                    .map(|g| (g.reference, g.members.len()))
                    .collect();
            }
        }
        match node.kind {
            NodeKind::MetadataFilter => {
                let reaching = input_members(graph, &catalog, node.id, 0);
                let mut s = FilterStats { reaching: reaching.len(), matching: n, missing: 0 };
                for id in &reaching {
                    let measure = Measure::from_index(node.choice("measure", *id)).unwrap_or(Measure::Iso);
                    if captures.get(id).and_then(|c| c.measure(measure)).is_none() {
                        s.missing += 1;
                    }
                }
                r.filters.insert(node.id, s);
            }
            NodeKind::Merge => {
                let mut s = MergeStats { total: n, ..Default::default() };
                let kept: HashSet<ImageId> = own.iter().copied().collect();
                let mut supplier: HashMap<ImageId, usize> = HashMap::new();
                let mut seen_in: HashMap<ImageId, usize> = HashMap::new();
                for port in 0..node.kind.inputs().len() {
                    for id in input_members(graph, &catalog, node.id, port) {
                        *seen_in.entry(id).or_insert(0) += 1;
                        supplier.entry(id).or_insert(port);
                    }
                }
                for (id, port) in &supplier {
                    if kept.contains(id) {
                        if let Some(c) = s.supplied.get_mut(*port) {
                            *c += 1;
                        }
                    }
                }
                s.duplicates = seen_in.values().filter(|c| **c > 1).count();
                s.dropped = supplier.len().saturating_sub(n);
                r.merges.insert(node.id, s);
            }
            NodeKind::MetadataSplit => {
                let mut s = SplitStats { reaching: n, ..Default::default() };
                for (port, slot) in s.per_port.iter_mut().enumerate() {
                    *slot = graph.members(node.id, port, &catalog).len();
                }
                s.excluded = n.saturating_sub(s.per_port.iter().sum());
                r.splits.insert(node.id, s);
            }
            NodeKind::Layer => {
                let lists: Vec<Option<Arc<Vec<ImageId>>>> = (0..node.kind.inputs().len())
                    .map(|port| graph.input_edge(node.id, port).map(|(up, up_port)| Arc::new(graph.members(up, up_port, &catalog))))
                    .collect();
                // The connected layer with the most images drives; a tie goes to the lowest number.
                let mut driver: Option<(usize, usize)> = None;
                for (i, l) in lists.iter().enumerate() {
                    if let Some(l) = l {
                        if driver.is_none_or(|(_, len)| l.len() > len) {
                            driver = Some((i, l.len()));
                        }
                    }
                }
                r.layers.insert(node.id, LayerStats { driver: driver.map(|(i, _)| i), lists });
            }
            _ => {}
        }
    }
    r.millis = started.elapsed().as_secs_f32() * 1000.0;
    r
}

struct Request {
    generation: u64,
    graph: Graph,
    ids: Arc<Vec<ImageId>>,
    captures: Captures,
    view_node: Option<NodeId>,
}

/// Background worker; the newest request wins.
pub struct CollectionWorker {
    tx: Sender<Request>,
    rx: Receiver<(u64, CollectionResult)>,
    generation: u64,
    applied: u64,
}

impl CollectionWorker {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, worker_rx) = channel::<Request>();
        let (worker_tx, rx) = channel();
        let _ = std::thread::Builder::new().name("rawgraph-collection".into()).spawn(move || {
            while let Ok(mut req) = worker_rx.recv() {
                while let Ok(newer) = worker_rx.try_recv() {
                    req = newer;
                }
                let result = compute(&req.graph, &req.ids, &req.captures, req.view_node);
                if worker_tx.send((req.generation, result)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
        Self { tx, rx, generation: 0, applied: 0 }
    }

    pub fn request(&mut self, graph: Graph, ids: Arc<Vec<ImageId>>, captures: Captures, view_node: Option<NodeId>) {
        self.generation += 1;
        let _ = self.tx.send(Request { generation: self.generation, graph, ids, captures, view_node });
    }

    /// Newest finished result, if any.
    pub fn poll(&mut self) -> Option<CollectionResult> {
        let mut latest = None;
        while let Ok((generation, r)) = self.rx.try_recv() {
            if generation > self.applied {
                self.applied = generation;
                latest = Some(r);
            }
        }
        latest
    }

    pub fn pending(&self) -> bool {
        self.applied < self.generation
    }
}

/// "1,204" style number.
pub fn with_thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A fake project: ISO 100 x n_low, ISO 800 x n_high, and n_none without ISO.
    pub fn fake_captures(n_low: u64, n_high: u64, n_none: u64) -> (Vec<ImageId>, Captures) {
        let mut ids = Vec::new();
        let mut map = HashMap::new();
        let mut next = 1u64;
        let mut add = |iso: Option<f32>, n: u64, ids: &mut Vec<ImageId>, map: &mut HashMap<ImageId, CaptureInfo>| {
            for _ in 0..n {
                let id = ImageId(next);
                next += 1;
                ids.push(id);
                map.insert(id, CaptureInfo { iso, ..Default::default() });
            }
        };
        add(Some(100.0), n_low, &mut ids, &mut map);
        add(Some(800.0), n_high, &mut ids, &mut map);
        add(None, n_none, &mut ids, &mut map);
        (ids, Arc::new(map))
    }

    /// Source -> two filters (ISO <= 400, ISO > 400) -> Merge(A, B) -> Output.
    pub fn split_graph() -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let low = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        let high = g.add_node(NodeKind::MetadataFilter, [0.0, 0.0]);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(src, low, 0).unwrap();
        g.connect(src, high, 0).unwrap();
        g.connect(low, merge, 0).unwrap();
        g.connect(high, merge, 1).unwrap();
        g.connect(merge, out, 0).unwrap();
        let first = ImageId(0);
        let n = g.node_mut(low).unwrap();
        n.set_param("max", first, 400.0);
        let n = g.node_mut(high).unwrap();
        n.set_param("invert", first, 1.0);
        n.set_param("max", first, 400.0);
        (g, src, low, high, merge, out)
    }

    #[test]
    fn counts_for_a_filter_and_merge_graph() {
        let (ids, caps) = fake_captures(3, 2, 4);
        let (g, src, low, high, merge, out) = split_graph();
        let r = compute(&g, &ids, &caps, Some(low));
        assert_eq!(r.counts[&src], 9);
        assert_eq!(r.counts[&low], 3, "ISO <= 400, images without ISO are excluded");
        assert_eq!(r.counts[&high], 2, "inverted: ISO > 400");
        assert_eq!(r.counts[&merge], 5);
        assert_eq!(r.counts[&out], 5);
        assert_eq!(r.view_members.len(), 3);
        assert_eq!(r.filters[&low], FilterStats { reaching: 9, matching: 3, missing: 4 });
        let m = &r.merges[&merge];
        assert_eq!((m.supplied[0], m.supplied[1], m.supplied[2], m.duplicates, m.total), (3, 2, 0, 0, 5));
    }

    /// Source -> Split (ISO: Low 0..400, High 401..6400) -> Unmatched ignored.
    fn split_only_graph(missing_exclude: bool) -> (Graph, NodeId) {
        use rawgraph_core::{NamedRange, NodeData};
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let split = g.add_node(NodeKind::MetadataSplit, [0.0, 0.0]);
        g.connect(src, split, 0).unwrap();
        let n = g.node_mut(split).unwrap();
        let rows = vec![NamedRange { name: "Low".into(), min: 0.0, max: 400.0 }, NamedRange { name: "High".into(), min: 401.0, max: 6400.0 }];
        assert!(n.set_data(ImageId(0), Some(NodeData::Ranges(rows))));
        if missing_exclude {
            n.set_param("missing", ImageId(0), 1.0);
        }
        (g, split)
    }

    #[test]
    fn per_port_counts_add_up_for_a_split() {
        let (ids, caps) = fake_captures(3, 2, 4); // ISO 100 x3, ISO 800 x2, no ISO x4
        let (g, split) = split_only_graph(false);
        let r = compute(&g, &ids, &caps, None);
        let s = &r.splits[&split];
        assert_eq!(s.reaching, 9);
        assert_eq!(s.per_port, [3, 2, 0, 0, 0, 0, 4], "Low, High, unused ports, Unmatched (the four without ISO)");
        assert_eq!(s.excluded, 0);
        assert_eq!(s.per_port.iter().sum::<usize>(), s.reaching, "everything is somewhere");
        assert_eq!(s.unmatched(), 4);
        // With missing = Exclude the four images without ISO leave through no port.
        let (g, split) = split_only_graph(true);
        let r = compute(&g, &ids, &caps, None);
        let s = &r.splits[&split];
        assert_eq!(s.per_port, [3, 2, 0, 0, 0, 0, 0]);
        assert_eq!(s.excluded, 4);
        assert_eq!(s.per_port.iter().sum::<usize>() + s.excluded, s.reaching);
    }

    #[test]
    fn downstream_counts_follow_the_port_they_hang_on() {
        let (ids, caps) = fake_captures(3, 2, 4);
        let (mut g, split) = split_only_graph(false);
        let low = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let high = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let rest = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        g.connect_ports(split, 0, low, 0).unwrap();
        g.connect_ports(split, 1, high, 0).unwrap();
        g.connect_ports(split, 6, rest, 0).unwrap();
        let r = compute(&g, &ids, &caps, Some(high));
        assert_eq!((r.counts[&low], r.counts[&high], r.counts[&rest]), (3, 2, 4));
        assert_eq!(r.view_members.len(), 2, "the filmstrip dims what is not on the viewed wire");
    }

    #[test]
    fn duplicates_are_resolved_by_port_order() {
        let (ids, caps) = fake_captures(4, 0, 0);
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        g.connect(src, merge, 0).unwrap();
        g.connect(src, merge, 2).unwrap();
        let r = compute(&g, &ids, &caps, None);
        let m = &r.merges[&merge];
        assert_eq!(m.supplied, [4, 0, 0, 0], "port A wins");
        assert_eq!(m.duplicates, 4);
        assert!(r.view_members.is_empty());
    }

    #[test]
    fn four_thousand_images_are_fast_enough() {
        let (ids, caps) = fake_captures(2000, 1500, 500);
        let (g, ..) = split_graph();
        let started = Instant::now();
        let r = compute(&g, &ids, &caps, None);
        assert_eq!(r.counts.len(), 5);
        eprintln!("collection counts for 4000 images and 5 nodes: {:.1} ms", started.elapsed().as_secs_f32() * 1000.0);
        assert!(started.elapsed().as_millis() < 2000, "took {} ms", started.elapsed().as_millis());
    }

    /// Images `n` per folder in `a`, `b` (same file names in both), `c` (other names), as a
    /// catalog with paths.
    fn path_catalog(n: u64) -> (Vec<ImageId>, Captures) {
        let mut ids = Vec::new();
        let mut map = HashMap::new();
        let mut next = 1u64;
        for (folder, prefix) in [("D:/photos/a", "img"), ("D:/photos/b", "img"), ("D:/photos/c/deep", "other")] {
            for i in 0..n {
                let id = ImageId(next);
                next += 1;
                ids.push(id);
                map.insert(id, CaptureInfo { path: Some(format!("{folder}/{prefix}{i:04}.dng")), iso: Some(100.0 + i as f32), ..Default::default() });
            }
        }
        (ids, Arc::new(map))
    }

    fn folder_source(g: &mut Graph, folder: &str, subfolders: bool) -> NodeId {
        let id = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let n = g.node_mut(id).unwrap();
        assert!(n.set_data(ImageId(0), Some(rawgraph_core::NodeData::Folder(folder.to_owned()))));
        n.set_param("subfolders", ImageId(0), if subfolders { 1.0 } else { 0.0 });
        id
    }

    #[test]
    fn counts_are_the_catalog_members_for_folders_merges_and_layers() {
        let (ids, caps) = path_catalog(6);
        let capture = |i: ImageId| caps.get(&i).cloned();
        let catalog = Catalog { images: &ids, capture: &capture, memo: None };
        let mut g = Graph::new();
        let a = folder_source(&mut g, "D:/photos/a", true);
        let b = folder_source(&mut g, "D:\\photos\\B\\", false);
        let c = folder_source(&mut g, "D:/photos/c", false);
        let c_deep = folder_source(&mut g, "D:/photos/c", true);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(a, merge, 0).unwrap();
        g.connect(b, merge, 1).unwrap();
        g.connect(a, layer, 0).unwrap();
        g.connect(c_deep, layer, 1).unwrap();
        g.connect(merge, out, 0).unwrap();
        // "First input wins": the six same-named images of B are dropped.
        g.node_mut(merge).unwrap().set_param("collision", ImageId(0), 1.0);
        let r = compute(&g, &ids, &caps, Some(layer));
        for node in g.nodes() {
            assert_eq!(r.counts[&node.id], g.members(node.id, 0, &catalog).len(), "{:?}", node.kind);
            assert_eq!(r.counts[&node.id], ids.iter().filter(|i| g.contains_in(node.id, **i, &catalog)).count(), "{:?} agrees with contains", node.kind);
        }
        assert_eq!((r.counts[&a], r.counts[&b], r.counts[&c], r.counts[&c_deep]), (6, 6, 0, 6), "subfolders switch, case and separators");
        let m = &r.merges[&merge];
        assert_eq!((m.supplied[0], m.supplied[1], m.total, m.dropped), (6, 0, 6, 6));
        assert_eq!(crate::node_ui::dropped_text(m.dropped).as_deref(), Some("6 same-named images dropped"));
        // The Layer: layer 1 (A, 6 images) and layer 2 (C, 6 images) pair by position.
        let l = &r.layers[&layer];
        assert_eq!(l.driver, Some(0));
        assert_eq!(l.output_len(), 6);
        for img in g.members(layer, 0, &catalog) {
            let expected = g.layer_sources(layer, img, &catalog).unwrap();
            let got = l.sources(img).unwrap();
            assert_eq!(got.len(), MAX_LAYERS_FOR_TEST);
            for (i, e) in expected.iter().enumerate() {
                assert_eq!(got[i], *e, "layer {}", i + 1);
            }
        }
        assert_eq!(l.sources(ImageId(99_999)), None, "an image outside the output has no pairing");
        assert_eq!(r.view_members.len(), 6, "the viewed node's members");
    }

    #[test]
    fn hdr_merge_groups_are_counted_and_labelled_downstream() {
        // Two brackets of three and a lone image.
        let shutters = [0.25f32, 1.0, 4.0, 0.25, 1.0, 4.0, 1.0];
        let ids: Vec<ImageId> = (1..=7).map(ImageId).collect();
        let mut map = HashMap::new();
        for (k, id) in ids.iter().enumerate() {
            let time = if k < 3 { Some(k as i64 * 400) } else if k < 6 { Some(30_000 + k as i64 * 400) } else { None };
            map.insert(*id, CaptureInfo { iso: Some(100.0), shutter_s: Some(shutters[k]), aperture: Some(1.0), captured_unix_ms: time, ..Default::default() });
        }
        let caps: Captures = Arc::new(map);
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let hdr = g.add_node(NodeKind::HdrMerge, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        g.connect(src, hdr, 0).unwrap();
        g.connect(hdr, exp, 0).unwrap();
        for view in [hdr, exp] {
            let r = compute(&g, &ids, &caps, Some(view));
            assert_eq!(r.counts[&hdr], 3, "two groups and one single image");
            assert_eq!(r.view_members.len(), 3);
            let mut groups: Vec<_> = r.view_groups.iter().map(|(i, n)| (i.0, *n)).collect();
            groups.sort();
            assert_eq!(groups, vec![(2, 3), (5, 3)], "references of the brackets; the single image has no badge");
        }
        // Upstream of the merge nothing is stacked.
        assert!(compute(&g, &ids, &caps, Some(src)).view_groups.is_empty());
    }

    const MAX_LAYERS_FOR_TEST: usize = rawgraph_core::MAX_LAYERS;

    #[test]
    fn the_layer_driver_is_the_connected_layer_with_the_most_images() {
        let (ids, caps) = path_catalog(4);
        let mut g = Graph::new();
        let small = folder_source(&mut g, "D:/photos/c", true);
        let big = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        g.connect(small, layer, 0).unwrap();
        g.connect(big, layer, 2).unwrap();
        let r = compute(&g, &ids, &caps, None);
        let l = &r.layers[&layer];
        assert_eq!((l.driver, l.output_len()), (Some(2), 12), "layer 3 has all 12 images, layer 1 only 4");
        assert!(l.lists[1].is_none() && l.lists[0].is_some());
        assert_eq!(r.counts[&layer], 12);
        let none = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        let r = compute(&g, &ids, &caps, None);
        assert_eq!((r.layers[&none].driver, r.counts[&none]), (None, 0));
    }

    #[test]
    fn counts_follow_graph_and_catalog_changes_through_the_worker() {
        let (ids, caps) = path_catalog(5);
        let mut g = Graph::new();
        let src = folder_source(&mut g, "D:/photos/a", true);
        let mut w = CollectionWorker::new(egui::Context::default());
        let wait = |w: &mut CollectionWorker| {
            let start = Instant::now();
            while start.elapsed().as_secs() < 10 {
                if let Some(r) = w.poll() {
                    return r;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("no result");
        };
        let ids = Arc::new(ids);
        w.request(g.clone(), ids.clone(), caps.clone(), Some(src));
        assert_eq!(wait(&mut w).counts[&src], 5);
        // The graph changes: another folder.
        g.node_mut(src).unwrap().set_data(ImageId(0), Some(rawgraph_core::NodeData::Folder("D:/photos/b".into())));
        w.request(g.clone(), ids.clone(), caps.clone(), Some(src));
        assert_eq!(wait(&mut w).counts[&src], 5);
        g.node_mut(src).unwrap().set_data(ImageId(0), Some(rawgraph_core::NodeData::Folder("D:/photos/zzz".into())));
        w.request(g.clone(), ids.clone(), caps.clone(), Some(src));
        assert_eq!(wait(&mut w).counts[&src], 0);
        // The catalog changes: a scan added an image to that folder.
        let mut more = (*caps).clone();
        more.insert(ImageId(1000), CaptureInfo { path: Some("D:/photos/zzz/new.dng".into()), ..Default::default() });
        let mut ids2 = (*ids).clone();
        ids2.push(ImageId(1000));
        w.request(g.clone(), Arc::new(ids2), Arc::new(more), Some(src));
        let r = wait(&mut w);
        assert_eq!(r.counts[&src], 1);
        assert!(r.view_members.contains(&ImageId(1000)));
    }

    /// 4,000 images and 30 nodes: every count at once stays far below a frame budget's worth of stutter,
    /// and it is made off the UI thread anyway.
    #[test]
    fn all_node_counts_for_four_thousand_images_and_thirty_nodes() {
        let (ids, caps) = path_catalog(1334);
        assert_eq!(ids.len(), 4002);
        let mut g = Graph::new();
        let a = folder_source(&mut g, "D:/photos/a", true);
        let b = folder_source(&mut g, "D:/photos/b", true);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        g.connect(a, merge, 0).unwrap();
        g.connect(b, merge, 1).unwrap();
        g.node_mut(merge).unwrap().set_param("collision", ImageId(0), 2.0);
        let mut prev = merge;
        for i in 0..25 {
            let kind = if i % 6 == 0 { NodeKind::MetadataFilter } else { NodeKind::Exposure };
            let n = g.add_node(kind, [0.0, 0.0]);
            g.connect(prev, n, 0).unwrap();
            prev = n;
        }
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        g.connect(prev, layer, 0).unwrap();
        g.connect(b, layer, 1).unwrap();
        let out = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(layer, out, 0).unwrap();
        assert_eq!(g.nodes().count(), 30);
        let started = Instant::now();
        let r = compute(&g, &ids, &caps, Some(out));
        let ms = started.elapsed().as_secs_f32() * 1000.0;
        eprintln!("all counts for {} images and {} nodes: {ms:.1} ms", ids.len(), g.nodes().count());
        assert_eq!(r.counts.len(), 30);
        assert_eq!(r.counts[&merge], 1334, "'Last input wins' keeps the images of B");
        assert!(ms < 1500.0, "took {ms} ms");
    }

    #[test]
    fn thousands_separator() {
        assert_eq!(with_thousands(0), "0");
        assert_eq!(with_thousands(999), "999");
        assert_eq!(with_thousands(1204), "1,204");
        assert_eq!(with_thousands(1234567), "1,234,567");
    }

    #[test]
    fn worker_delivers_the_newest_result() {
        let (ids, caps) = fake_captures(3, 2, 1);
        let (g, _, low, ..) = split_graph();
        let mut w = CollectionWorker::new(egui::Context::default());
        let ids = Arc::new(ids);
        w.request(g.clone(), ids.clone(), caps.clone(), None);
        w.request(g, ids, caps, Some(low));
        let start = Instant::now();
        let mut got = None;
        // The first request may be skipped; the newest one must always arrive.
        while start.elapsed().as_secs() < 10 && got.as_ref().is_none_or(|r: &CollectionResult| r.view_node.is_none()) {
            if let Some(r) = w.poll() {
                got = Some(r);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let r = got.expect("a result");
        assert_eq!(r.view_node, Some(low));
        assert_eq!(r.view_members.len(), 3);
        assert!(!w.pending());
    }
}
