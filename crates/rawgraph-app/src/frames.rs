//! Picture frames along the graph: the size of the image entering a node (through all upstream
//! Crop / Rotate nodes) and where a brush mask is painted when a Crop / Rotate sits between the
//! node that uses the mask and the picture on screen. No UI code, so everything is testable.
//!
//! A brush mask belongs to the frame of the image it is applied to (its consumer). Three cases:
//! * `Direct`: no geometry-changing node lies between the consumer and the viewed node, the
//!   viewed picture is painted on as it is.
//! * `Mapped`: only Crop / Rotate nodes lie between; pointer positions are mapped back through
//!   them (`rawgraph_fx::transform_output_to_input`, last node first, each with its input size).
//! * `ShowConsumer`: anything else (Lens (manual) in between, consumers in different frames, an
//!   unrelated viewed node): the picture as it leaves the consumer is shown while painting.

use rawgraph_core::{Graph, ImageBuf, ImageId, MaskBuf, NodeData, NodeId, NodeKind, PortType};
use rawgraph_fx::{transform_output_to_input, transform_size, TransformParams};

use crate::tools::transform_params;

/// The node feeding the first connected image input of `id`.
pub fn image_parent(graph: &Graph, id: NodeId) -> Option<NodeId> {
    let node = graph.node(id)?;
    node.kind.inputs().iter().enumerate().filter(|(_, s)| s.ty == PortType::Images).find_map(|(port, _)| graph.input_edge(id, port).map(|(up, _)| up))
}

/// Nodes upstream of `id` along the image input, nearest first (never loops).
fn ancestors(graph: &Graph, id: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut cur = image_parent(graph, id);
    while let Some(n) = cur {
        if out.contains(&n) || n == id {
            break;
        }
        out.push(n);
        cur = image_parent(graph, n);
    }
    out
}

/// Nodes that change the picture geometry.
pub fn is_geometry(kind: NodeKind) -> bool {
    matches!(kind, NodeKind::Transform | NodeKind::LensManual)
}

/// Geometry-changing nodes upstream of `id`, source first.
fn geometry_upstream(graph: &Graph, id: NodeId) -> Vec<NodeId> {
    let mut v: Vec<NodeId> = ancestors(graph, id).into_iter().filter(|n| graph.node(*n).is_some_and(|n| is_geometry(n.kind))).collect();
    v.reverse();
    v
}

/// Crop / Rotate nodes upstream of `id`, source first.
pub fn transforms_upstream(graph: &Graph, id: NodeId) -> Vec<NodeId> {
    let mut v: Vec<NodeId> = ancestors(graph, id).into_iter().filter(|n| graph.node(*n).is_some_and(|n| n.kind == NodeKind::Transform)).collect();
    v.reverse();
    v
}

/// Size of the image after one Crop / Rotate node.
pub fn size_after(graph: &Graph, node: NodeId, image: ImageId, size: (u32, u32)) -> (u32, u32) {
    match graph.node(node) {
        Some(n) if n.kind == NodeKind::Transform => transform_size(size.0, size.1, &transform_params(n, image)),
        _ => size,
    }
}

/// Size of the image entering `node` when the source image has `source` pixels: through every
/// upstream Crop / Rotate node (all other nodes keep the size).
pub fn size_into(graph: &Graph, node: NodeId, image: ImageId, source: (u32, u32)) -> (u32, u32) {
    transforms_upstream(graph, node).into_iter().fold(source, |s, t| size_after(graph, t, image, s))
}

/// Where a brush mask is painted (see the module text).
#[derive(Clone, Debug, PartialEq)]
pub enum BrushPlan {
    Direct { consumer: Option<NodeId> },
    /// `chain`: the Crop / Rotate nodes between the consumer and the viewed node, consumer side first.
    Mapped { consumer: NodeId, chain: Vec<NodeId> },
    ShowConsumer { consumer: NodeId },
}

impl BrushPlan {
    #[cfg(test)]
    pub fn consumer(&self) -> Option<NodeId> {
        match self {
            BrushPlan::Direct { consumer } => *consumer,
            BrushPlan::Mapped { consumer, .. } | BrushPlan::ShowConsumer { consumer } => Some(*consumer),
        }
    }
}

/// The plan for painting on `brush` while `viewed` (the node whose picture is on screen) is shown.
pub fn brush_plan(graph: &Graph, brush: NodeId, viewed: Option<NodeId>) -> BrushPlan {
    let mut consumers: Vec<NodeId> = graph.edges().iter().filter(|e| e.from == brush).map(|e| e.to).collect();
    consumers.sort_by_key(|n| n.0);
    consumers.dedup();
    let Some(&consumer) = consumers.first() else { return BrushPlan::Direct { consumer: None } };
    let Some(viewed) = viewed else { return BrushPlan::Direct { consumer: Some(consumer) } };
    // The consumer's own picture is painted on as it is (also the picture a ShowConsumer plan shows).
    if viewed == consumer {
        return BrushPlan::Direct { consumer: Some(consumer) };
    }
    // Consumers in different frames: the strokes mean different things in each of them.
    let frame = geometry_upstream(graph, consumer);
    if consumers.iter().any(|c| geometry_upstream(graph, *c) != frame) {
        return BrushPlan::ShowConsumer { consumer };
    }
    let kind_of = |n: NodeId| graph.node(n).map(|n| n.kind);
    // The viewed node lies behind the consumer: the nodes in between, viewed node included.
    let mut down = vec![viewed];
    down.extend(ancestors(graph, viewed));
    if let Some(at) = down.iter().position(|n| *n == consumer) {
        let between: Vec<NodeId> = down[..at].to_vec();
        let geometry: Vec<NodeId> = between.iter().copied().filter(|n| kind_of(*n).is_some_and(is_geometry)).collect();
        if geometry.is_empty() {
            return BrushPlan::Direct { consumer: Some(consumer) };
        }
        if geometry.iter().all(|n| kind_of(*n) == Some(NodeKind::Transform)) {
            let mut chain = geometry;
            chain.reverse();
            return BrushPlan::Mapped { consumer, chain };
        }
        return BrushPlan::ShowConsumer { consumer };
    }
    // The viewed node lies in front of the consumer.
    let up = ancestors(graph, consumer);
    if let Some(at) = up.iter().position(|n| *n == viewed) {
        if up[..at].iter().any(|n| kind_of(*n).is_some_and(is_geometry)) {
            return BrushPlan::ShowConsumer { consumer };
        }
        return BrushPlan::Direct { consumer: Some(consumer) };
    }
    BrushPlan::ShowConsumer { consumer }
}

/// Text of the label shown while the picture is the one leaving the consumer.
pub fn painting_label(graph: &Graph, plan: &BrushPlan) -> Option<String> {
    let BrushPlan::ShowConsumer { consumer } = plan else { return None };
    let name = graph.node(*consumer).map_or_else(|| format!("node {}", consumer.0), |n| n.kind.label().to_owned());
    Some(format!("Painting on: {name} \u{2014} later geometry is not shown while painting"))
}

/// Maps a point of the viewed picture (normalised) back into the frame of the consumer.
#[derive(Clone, Debug, PartialEq)]
pub struct Mapper {
    /// Parameters and input size of every Crop / Rotate between consumer and viewed picture,
    /// consumer side first.
    stages: Vec<(TransformParams, (u32, u32))>,
    /// Size of the consumer's image.
    pub consumer_size: (u32, u32),
}

impl Mapper {
    pub fn from_stages(consumer_size: (u32, u32), params: &[TransformParams]) -> Self {
        let mut cur = consumer_size;
        let mut stages = Vec::with_capacity(params.len());
        for p in params {
            stages.push((*p, cur));
            cur = transform_size(cur.0, cur.1, p);
        }
        Self { stages, consumer_size }
    }

    /// `source`: pixel size of the decoded source picture.
    pub fn new(graph: &Graph, consumer: NodeId, chain: &[NodeId], image: ImageId, source: (u32, u32)) -> Self {
        let params: Vec<TransformParams> = chain.iter().filter_map(|id| graph.node(*id)).map(|n| transform_params(n, image)).collect();
        Self::from_stages(size_into(graph, consumer, image, source), &params)
    }

    /// Size of the viewed picture.
    #[cfg(test)]
    pub fn viewed_size(&self) -> (u32, u32) {
        self.stages.last().map_or(self.consumer_size, |(p, s)| transform_size(s.0, s.1, p))
    }

    /// Normalised point of the viewed picture -> normalised point of the consumer's image.
    pub fn to_consumer(&self, p: [f32; 2]) -> [f32; 2] {
        let mut q = (p[0], p[1]);
        for (params, size) in self.stages.iter().rev() {
            q = transform_output_to_input(size.0, size.1, params, q.0, q.1);
        }
        [q.0, q.1]
    }
}

/// What the render worker needs to draw the overlay of a brush mask on the viewed picture.
#[derive(Clone, Debug, PartialEq)]
pub struct OverlayFrame {
    pub consumer: NodeId,
    /// Crop / Rotate nodes between the consumer and the picture shown, consumer side first.
    pub chain: Vec<NodeId>,
}

/// The overlay request for the node that is rendered: `(mask to overlay, how to frame it)`.
/// A brush overlay that cannot be shown correctly on the rendered picture is dropped.
pub fn overlay_for_render(graph: &Graph, overlay: Option<NodeId>, rendered: NodeId) -> (Option<NodeId>, Option<OverlayFrame>) {
    let Some(mask) = overlay else { return (None, None) };
    if graph.node(mask).is_none_or(|n| n.kind != NodeKind::BrushMask) {
        return (Some(mask), None);
    }
    match brush_plan(graph, mask, Some(rendered)) {
        BrushPlan::Direct { consumer: Some(consumer) } => (Some(mask), Some(OverlayFrame { consumer, chain: Vec::new() })),
        BrushPlan::Mapped { consumer, chain } => (Some(mask), Some(OverlayFrame { consumer, chain })),
        BrushPlan::Direct { consumer: None } => (Some(mask), None),
        BrushPlan::ShowConsumer { .. } => (None, None),
    }
}

fn mask_to_image(mask: &MaskBuf) -> Option<ImageBuf> {
    let mut data = Vec::with_capacity(mask.data.len() * 4);
    for v in &mask.data {
        data.extend_from_slice(&[*v, *v, *v, 1.0]);
    }
    ImageBuf::from_rgba(mask.width, mask.height, data)
}

fn image_to_mask(img: &ImageBuf) -> MaskBuf {
    MaskBuf { width: img.width(), height: img.height(), data: img.data().as_chunks::<4>().0.iter().map(|p| p[0]).collect() }
}

/// The stroke mask of `brush` in the consumer's frame, carried through the Crop / Rotate nodes to
/// the frame of the viewed picture. `source` is the pixel size of the decoded source picture.
pub fn brush_overlay_mask(graph: &Graph, brush: NodeId, frame: &OverlayFrame, image: ImageId, source: (u32, u32)) -> Option<MaskBuf> {
    let node = graph.node(brush)?;
    let strokes = match node.data(image) {
        Some(NodeData::Strokes(s)) => s.as_slice(),
        _ => &[],
    };
    let (cw, ch) = size_into(graph, frame.consumer, image, source);
    let mut mask = rawgraph_fx::stroke_mask(cw, ch, strokes, node.toggle("invert", image));
    for id in &frame.chain {
        let n = graph.node(*id)?;
        let params = transform_params(n, image);
        if params.is_identity() {
            continue;
        }
        let img = mask_to_image(&mask)?;
        mask = image_to_mask(&rawgraph_fx::transform(&img, &params));
    }
    Some(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::BrushStroke;

    const IMG: ImageId = ImageId(1);

    /// source -> [nodes...] -> exposure (brush as its mask) -> [after...] -> output
    struct Built {
        g: Graph,
        brush: NodeId,
        exposure: NodeId,
        output: NodeId,
    }

    fn chain(before: &[NodeKind], after: &[NodeKind]) -> (Built, Vec<NodeId>, Vec<NodeId>) {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let mut prev = src;
        let mut pre = Vec::new();
        for k in before {
            let n = g.add_node(*k, [0.0, 0.0]);
            g.connect(prev, n, 0).unwrap();
            prev = n;
            pre.push(n);
        }
        let exposure = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        g.connect(prev, exposure, 0).unwrap();
        let brush = g.add_node(NodeKind::BrushMask, [0.0, 0.0]);
        g.connect(brush, exposure, 1).unwrap();
        prev = exposure;
        let mut post = Vec::new();
        for k in after {
            let n = g.add_node(*k, [0.0, 0.0]);
            g.connect(prev, n, 0).unwrap();
            prev = n;
            post.push(n);
        }
        let output = g.add_node(NodeKind::Output, [0.0, 0.0]);
        g.connect(prev, output, 0).unwrap();
        (Built { g, brush, exposure, output }, pre, post)
    }

    #[test]
    fn case_a_no_geometry_between_consumer_and_viewed_node() {
        let (b, _, _) = chain(&[], &[NodeKind::Contrast]);
        assert_eq!(brush_plan(&b.g, b.brush, Some(b.output)), BrushPlan::Direct { consumer: Some(b.exposure) });
        assert_eq!(brush_plan(&b.g, b.brush, Some(b.exposure)), BrushPlan::Direct { consumer: Some(b.exposure) });
        // A crop BEFORE the consumer is part of the consumer's own frame: still direct.
        let (b, _, _) = chain(&[NodeKind::Transform], &[]);
        assert_eq!(brush_plan(&b.g, b.brush, Some(b.output)), BrushPlan::Direct { consumer: Some(b.exposure) });
        // A brush without consumers paints on whatever is shown.
        let mut g = Graph::new();
        let lone = g.add_node(NodeKind::BrushMask, [0.0, 0.0]);
        assert_eq!(brush_plan(&g, lone, None), BrushPlan::Direct { consumer: None });
    }

    #[test]
    fn case_b_only_crop_and_rotate_between_are_mapped_consumer_side_first() {
        let (b, _, post) = chain(&[], &[NodeKind::Transform, NodeKind::Saturation, NodeKind::Transform]);
        let plan = brush_plan(&b.g, b.brush, Some(b.output));
        assert_eq!(plan, BrushPlan::Mapped { consumer: b.exposure, chain: vec![post[0], post[2]] });
        assert_eq!(plan.consumer(), Some(b.exposure));
        // Viewing the node between the two crops only maps through the first one.
        assert_eq!(brush_plan(&b.g, b.brush, Some(post[1])), BrushPlan::Mapped { consumer: b.exposure, chain: vec![post[0]] });
    }

    #[test]
    fn case_c_a_lens_between_or_consumers_in_different_frames_show_the_consumer() {
        let (b, _, post_lens) = chain(&[], &[NodeKind::Transform, NodeKind::LensManual]);
        let plan = brush_plan(&b.g, b.brush, Some(b.output));
        assert_eq!(plan, BrushPlan::ShowConsumer { consumer: b.exposure });
        let label = painting_label(&b.g, &plan).unwrap();
        assert_eq!(label, "Painting on: Exposure \u{2014} later geometry is not shown while painting");
        assert!(painting_label(&b.g, &BrushPlan::Direct { consumer: None }).is_none());
        assert_eq!(post_lens.len(), 2);

        // Two consumers, one of them behind a crop: different frames.
        let (mut b, _, _) = chain(&[], &[]);
        let tf = b.g.add_node(NodeKind::Transform, [0.0, 0.0]);
        let src = b.g.nodes().find(|n| n.kind == NodeKind::Source).unwrap().id;
        b.g.connect(src, tf, 0).unwrap();
        let second = b.g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        b.g.connect(tf, second, 0).unwrap();
        b.g.connect(b.brush, second, 1).unwrap();
        assert!(matches!(brush_plan(&b.g, b.brush, Some(b.output)), BrushPlan::ShowConsumer { .. }));
        // The mask overlay is only drawn when it can be shown right.
        let (shown, frame) = overlay_for_render(&b.g, Some(b.brush), b.output);
        assert_eq!((shown, frame), (None, None));
        // While painting, the consumer's own picture is shown and its overlay is drawn.
        let (shown, frame) = overlay_for_render(&b.g, Some(b.brush), b.exposure);
        assert_eq!((shown, frame), (Some(b.brush), Some(OverlayFrame { consumer: b.exposure, chain: Vec::new() })));
    }

    #[test]
    fn a_viewed_node_in_front_of_the_consumer_is_direct_unless_geometry_lies_between() {
        let (b, pre, _) = chain(&[NodeKind::Saturation, NodeKind::Contrast], &[]);
        assert_eq!(brush_plan(&b.g, b.brush, Some(pre[0])), BrushPlan::Direct { consumer: Some(b.exposure) });
        let (b, pre, _) = chain(&[NodeKind::Saturation, NodeKind::Transform, NodeKind::Contrast], &[]);
        assert_eq!(brush_plan(&b.g, b.brush, Some(pre[0])), BrushPlan::ShowConsumer { consumer: b.exposure });
    }

    #[test]
    fn full_and_preview_sizes_run_through_all_upstream_crop_nodes() {
        let (b, pre, _) = chain(&[NodeKind::Transform, NodeKind::Saturation, NodeKind::Transform], &[]);
        let mut g = b.g;
        {
            let n = g.node_mut(pre[0]).unwrap();
            n.set_param("left", IMG, 0.1);
            n.set_param("right", IMG, 0.9);
        }
        {
            let n = g.node_mut(pre[2]).unwrap();
            n.set_param("rotate", IMG, 1.0);
            n.set_param("top", IMG, 0.25);
            n.set_param("bottom", IMG, 0.75);
        }
        // 5632 x 3755: the first crop keeps 80 % of the width, the quarter turn swaps the sides and the
        // second crop keeps half of the new height.
        let full = size_into(&g, b.exposure, IMG, (5632, 3755));
        let first = transform_size(5632, 3755, &transform_params(g.node(pre[0]).unwrap(), IMG));
        let second = transform_size(first.0, first.1, &transform_params(g.node(pre[2]).unwrap(), IMG));
        assert_eq!(full, second);
        assert_eq!(first.0, 4506, "{first:?}");
        assert_eq!((full.0, full.1), (3755, 2253), "{full:?}");
        // The size entering the first crop is the source size itself.
        assert_eq!(size_into(&g, pre[0], IMG, (5632, 3755)), (5632, 3755));
        assert_eq!(size_after(&g, pre[1], IMG, (10, 20)), (10, 20), "other nodes keep the size");
    }

    /// A picture with one bright square at `(cx, cy)` (pixel centre of its middle).
    fn marker_image(w: u32, h: u32, cx: u32, cy: u32) -> ImageBuf {
        let mut img = ImageBuf::new(w, h);
        for y in cy.saturating_sub(2)..=(cy + 2).min(h - 1) {
            for x in cx.saturating_sub(2)..=(cx + 2).min(w - 1) {
                let i = ((y * w + x) * 4) as usize;
                img.data_mut()[i..i + 4].copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
            }
        }
        img
    }

    /// Centre of the bright pixels, normalised.
    fn centroid(img: &ImageBuf) -> [f32; 2] {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let (mut sx, mut sy, mut sum) = (0.0f64, 0.0f64, 0.0f64);
        for y in 0..h {
            for x in 0..w {
                let v = f64::from(img.data()[(y * w + x) * 4]);
                if v > 0.05 {
                    sx += v * (x as f64 + 0.5);
                    sy += v * (y as f64 + 0.5);
                    sum += v;
                }
            }
        }
        assert!(sum > 0.0, "the marker was lost");
        [(sx / sum / w as f64) as f32, (sy / sum / h as f64) as f32]
    }

    /// A point painted on the viewed picture at the marker lands on the marker in the consumer frame.
    fn marker_lands(consumer: (u32, u32), marker: (u32, u32), stages: &[TransformParams]) {
        let source = marker_image(consumer.0, consumer.1, marker.0, marker.1);
        let truth = [(marker.0 as f32 + 0.5) / consumer.0 as f32, (marker.1 as f32 + 0.5) / consumer.1 as f32];
        let mut viewed = source.clone();
        for p in stages {
            viewed = rawgraph_fx::transform(&viewed, p);
        }
        let mapper = Mapper::from_stages(consumer, stages);
        assert_eq!(mapper.viewed_size(), (viewed.width(), viewed.height()), "the mapper knows the size of the viewed picture");
        let seen = centroid(&viewed);
        let back = mapper.to_consumer(seen);
        let tol_x = 1.5 / consumer.0 as f32;
        let tol_y = 1.5 / consumer.1 as f32;
        assert!((back[0] - truth[0]).abs() < tol_x && (back[1] - truth[1]).abs() < tol_y, "marker at {truth:?} came back as {back:?} (seen at {seen:?}, stages {stages:?})");
    }

    #[test]
    fn a_point_painted_on_a_cropped_picture_lands_on_the_feature_in_the_consumer_frame() {
        let crop = TransformParams { left: 0.25, top: 0.2, right: 0.9, bottom: 0.8, ..TransformParams::default() };
        marker_lands((240, 160), (150, 70), &[crop]);
        marker_lands((240, 160), (100, 100), &[crop]);
    }

    #[test]
    fn a_point_painted_on_a_turned_picture_lands_on_the_feature() {
        for quarter in 1..=3u8 {
            let turn = TransformParams { quarter_turns: quarter, ..TransformParams::default() };
            marker_lands((240, 160), (60, 40), &[turn]);
            marker_lands((240, 160), (200, 130), &[turn]);
        }
        let flip = TransformParams { quarter_turns: 1, flip_h: true, ..TransformParams::default() };
        marker_lands((240, 160), (60, 40), &[flip]);
    }

    #[test]
    fn a_point_painted_on_a_finely_rotated_picture_lands_on_the_feature() {
        let fine = TransformParams { angle_deg: 7.0, ..TransformParams::default() };
        marker_lands((240, 160), (120, 80), &[fine]);
        marker_lands((240, 160), (150, 90), &[fine]);
        let neg = TransformParams { angle_deg: -12.0, left: 0.1, right: 0.9, ..TransformParams::default() };
        marker_lands((240, 160), (130, 85), &[neg]);
    }

    #[test]
    fn chained_stages_map_back_through_each_one_with_its_own_input_size() {
        let crop = TransformParams { left: 0.1, top: 0.1, right: 0.8, bottom: 0.9, ..TransformParams::default() };
        let turn = TransformParams { quarter_turns: 1, ..TransformParams::default() };
        let fine = TransformParams { angle_deg: 5.0, ..TransformParams::default() };
        marker_lands((300, 200), (120, 90), &[crop, turn, fine]);
    }

    #[test]
    fn the_overlay_mask_follows_the_crop_like_the_picture_does() {
        let (b, _, post) = chain(&[], &[NodeKind::Transform]);
        let mut g = b.g;
        {
            let n = g.node_mut(post[0]).unwrap();
            n.set_param("left", IMG, 0.5);
            n.set_param("top", IMG, 0.25);
            n.set_param("right", IMG, 1.0);
            n.set_param("bottom", IMG, 1.0);
        }
        // A stroke at (0.75, 0.6) of the consumer frame lies at (0.5, 0.466..) of the cropped picture.
        let stroke = BrushStroke { points: vec![[0.75, 0.6]], radius: 0.03, hardness: 1.0, flow: 1.0, erase: false };
        assert!(g.node_mut(b.brush).unwrap().set_data(IMG, Some(NodeData::Strokes(vec![stroke]))));
        let frame = OverlayFrame { consumer: b.exposure, chain: post.clone() };
        let mask = brush_overlay_mask(&g, b.brush, &frame, IMG, (400, 200)).unwrap();
        assert_eq!((mask.width, mask.height), (200, 150), "the mask has the size of the cropped picture");
        // Centre of the painted area (the stroke is a flat disc, so the centre of mass is its middle).
        let (mut sx, mut sy, mut sum, mut best) = (0.0f64, 0.0f64, 0.0f64, 0.0f32);
        for y in 0..mask.height as usize {
            for x in 0..mask.width as usize {
                let v = mask.data[y * mask.width as usize + x];
                best = best.max(v);
                if v > 0.5 {
                    sx += x as f64 + 0.5;
                    sy += y as f64 + 0.5;
                    sum += 1.0;
                }
            }
        }
        assert!(best > 0.9 && sum > 0.0, "the stroke is there: {best}");
        let at = ((sx / sum) as f32, (sy / sum) as f32);
        let (ex, ey) = rawgraph_fx::transform_input_to_output(400, 200, &transform_params(g.node(post[0]).unwrap(), IMG), 0.75, 0.6);
        let expect = (ex * 200.0, ey * 150.0);
        assert!((at.0 - expect.0).abs() < 1.5 && (at.1 - expect.1).abs() < 1.5, "stroke centre {at:?} expected near {expect:?}");
        // Without a chain the mask keeps the consumer's frame.
        let direct = brush_overlay_mask(&g, b.brush, &OverlayFrame { consumer: b.exposure, chain: Vec::new() }, IMG, (400, 200)).unwrap();
        assert_eq!((direct.width, direct.height), (400, 200));
    }
}
