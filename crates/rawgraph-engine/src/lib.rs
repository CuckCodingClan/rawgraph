//! RawGraph engine: caching, cancellable evaluation of a node graph.
//!
//! The [`Evaluator`] renders one node of a [`Graph`] for one image identity.
//! Every `(node, image)` result is cached together with a fingerprint of
//! everything that influenced it, so changing a parameter only recomputes the
//! nodes downstream of the change, and Individual-mode edits of one image never
//! touch the cache entries of another.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rawgraph_core::{
    cdl_params, color, curve_set, develop_settings, develop_tone, iso_noise_scale, layer_settings, ops, CaptureInfo, Catalog, CdlParams,
    DevelopSettings, DevelopTone, Graph, ImageBuf, ImageId, LayerSettings, LensProfile, MaskBuf, NodeData, NodeId, NodeKind, PortType,
    MAX_LAYERS,
};
use rawgraph_fx as fx;
use rawgraph_scopes::{InverseDisplayTransform, ToneMap};

/// Supplies base pixels for Source nodes. Called from a worker thread.
pub trait ImageProvider: Send + Sync {
    /// Scene-referred linear ACEScg pixels of `id` at the current preview size.
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String>;
    /// Changes whenever `image(id)` would return different pixels (e.g. new proxy size).
    fn revision(&self, _id: ImageId) -> u64 {
        0
    }
    /// Capture metadata of `id`, used by metadata filters. `None` if unknown.
    fn capture(&self, _id: ImageId) -> Option<CaptureInfo> {
        None
    }
    /// Every image of the project in project order (the catalog Source folders, Merge name
    /// collisions and Layer pairing are resolved against). Empty by default.
    fn images(&self) -> Vec<ImageId> {
        Vec::new()
    }
    /// Upright size of the full-resolution file of `id` (width, height), if known. Used to
    /// scale a "Crop" layer so one layer pixel stays one full-resolution pixel.
    fn full_size(&self, _id: ImageId) -> Option<(u32, u32)> {
        None
    }
    /// Lens profile (DNG opcodes) of `id` in the upright image frame, if the file has one.
    fn lens_profile(&self, _id: ImageId) -> Option<LensProfile> {
        None
    }
}

/// Result of evaluating a node.
#[derive(Clone, Debug)]
pub enum Value {
    Images(Arc<ImageBuf>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvalError {
    UnknownNode(NodeId),
    /// Required input not connected (also reported for a cyclic connection).
    MissingInput { node: NodeId, port: usize },
    /// Connected value has the wrong type.
    WrongType { node: NodeId, port: usize },
    Provider(String),
    /// The image is filtered out before it reaches `node`.
    NotInCollection { node: NodeId },
    Cancelled,
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::UnknownNode(n) => write!(f, "node {} does not exist", n.0),
            EvalError::MissingInput { node, port } => {
                write!(f, "node {}: required input {} is not connected", node.0, port)
            }
            EvalError::WrongType { node, port } => {
                write!(f, "node {}: input {} received a value of the wrong type", node.0, port)
            }
            EvalError::Provider(e) => write!(f, "could not load image: {e}"),
            EvalError::NotInCollection { node } => {
                write!(f, "this image is not part of the collection at node {}", node.0)
            }
            EvalError::Cancelled => write!(f, "evaluation was cancelled"),
        }
    }
}

impl std::error::Error for EvalError {}

/// Cooperative cancellation flag, cheap to clone and share between threads.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Cache key: node, image and whether it is evaluated as control data.
type Key = (NodeId, ImageId, bool);

struct Entry {
    fingerprint: u64,
    value: Value,
}

/// One node's exclusive CPU evaluation work for a single preview request.
#[derive(Clone, Debug)]
pub struct NodeTiming {
    pub node: NodeId,
    pub cpu: std::time::Duration,
    pub computed: u64,
    pub cache_hits: u64,
}

/// Caching graph evaluator.
pub struct Evaluator {
    cache: HashMap<Key, Entry>,
    view_tone: ToneMap,
    /// Output of a Develop node before its tone part, one entry per (node, image). It is
    /// what [`Evaluator::eval_base`] returns for a plan with `base_before_tone`, and what
    /// the Develop node itself builds on, so moving a tone slider never redoes the base.
    pre_tone: HashMap<(NodeId, ImageId, bool), Entry>,
    hits: u64,
    misses: u64,
    keep_intermediates: bool,
    timings: Option<Vec<NodeTiming>>,
    timing_children: Vec<std::time::Duration>,
    timing_hit: Vec<bool>,
}

impl Default for Evaluator {
    fn default() -> Self {
        Self { cache: HashMap::new(), view_tone: ToneMap::AcesFit, pre_tone: HashMap::new(), hits: 0, misses: 0, keep_intermediates: true, timings: None, timing_children: Vec::new(), timing_hit: Vec::new() }
    }
}

/// What every evaluation step needs besides the graph: the provider and the catalog
/// (every image of the project in project order), fetched once per public call.
#[derive(Clone, Copy)]
struct Env<'a> {
    provider: &'a dyn ImageProvider,
    images: &'a [ImageId],
    view_tone: ToneMap,
    control: bool,
}

impl Env<'_> {
    fn control(self, control: bool) -> Self { Self { control: self.control || control, ..self } }

    /// Runs `f` with the catalog of this environment.
    fn cat<R>(&self, f: impl FnOnce(&Catalog) -> R) -> R {
        let capture = |i: ImageId| self.provider.capture(i);
        let catalog = Catalog { images: self.images, capture: &capture };
        f(&catalog)
    }

    fn contains_output(&self, graph: &Graph, node: NodeId, port: usize, image: ImageId) -> bool {
        self.cat(|c| graph.contains_output_in(node, port, image, c))
    }

    /// Resolve image inputs by collection membership, retaining wire order.
    fn input_edge(&self, graph: &Graph, node: NodeId, port: usize, image: ImageId) -> Option<(NodeId, usize)> {
        graph.input_edges(node, port).find(|e| self.contains_output(graph, e.from, e.from_port, image)).map(|e| (e.from, e.from_port))
    }

    /// Resolve auxiliary controls without adding their images to the main collection.
    fn control_edge(&self, graph: &Graph, node: NodeId, port: usize, image: ImageId) -> Option<(NodeId, usize, ImageId)> {
        let (up, output) = graph.input_edge(node, port)?;
        if self.contains_output(graph, up, output, image) { return Some((up, output, image)); }
        let members = self.cat(|c| graph.members(up, output, c));
        if members.len() == 1 { Some((up, output, members[0])) } else { None }
    }

    /// Membership for viewing `node` (output port 0). Viewing a Metadata Split passes its
    /// whole input collection through.
    fn viewing_member(&self, graph: &Graph, node: NodeId, image: ImageId) -> bool {
        let is_split = graph.node(node).is_some_and(|n| n.kind == NodeKind::MetadataSplit);
        if is_split {
            self.input_edge(graph, node, 0, image).is_some()
        } else {
            self.contains_output(graph, node, 0, image)
        }
    }
}

impl Evaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pixel-free cache identity for a node and one image, using the same inputs as evaluation.
    pub fn fingerprint(&self, graph: &Graph, node: NodeId, image: ImageId, provider: &dyn ImageProvider) -> Result<u64, EvalError> {
        let images = provider.images();
        let env = Env { provider, images: &images, view_tone: self.view_tone, control: false };
        node_fingerprint(graph, node, image, &env, &mut FingerprintMemo::new(), &mut Vec::new())
    }

    /// Evaluates output port 0 of `node`. For a Metadata Split node this means viewing
    /// the node: every image of its input collection is passed through unchanged.
    pub fn eval(
        &mut self,
        graph: &Graph,
        node: NodeId,
        image: ImageId,
        provider: &dyn ImageProvider,
        cancel: &CancelToken,
    ) -> Result<Value, EvalError> {
        let images = provider.images();
        self.eval_env(graph, node, image, &Env { provider, images: &images, view_tone: self.view_tone, control: false }, cancel)
    }

    fn eval_env(&mut self, graph: &Graph, node: NodeId, image: ImageId, env: &Env, cancel: &CancelToken) -> Result<Value, EvalError> {
        let member = env.viewing_member(graph, node, image);
        self.eval_checked(graph, node, member, image, env, cancel)
    }

    /// Evaluates output `port` of `node`. The image must leave through that port
    /// (`Graph::contains_output_in`), otherwise the result is `NotInCollection`.
    pub fn eval_output(
        &mut self,
        graph: &Graph,
        node: NodeId,
        port: usize,
        image: ImageId,
        provider: &dyn ImageProvider,
        cancel: &CancelToken,
    ) -> Result<Value, EvalError> {
        let images = provider.images();
        self.eval_output_env(graph, node, port, image, &Env { provider, images: &images, view_tone: self.view_tone, control: false }, cancel)
    }

    fn eval_output_env(
        &mut self,
        graph: &Graph,
        node: NodeId,
        port: usize,
        image: ImageId,
        env: &Env,
        cancel: &CancelToken,
    ) -> Result<Value, EvalError> {
        let member = env.contains_output(graph, node, port, image);
        self.eval_checked(graph, node, member, image, env, cancel).map(|v| output_value(graph, node, port, v))
    }

    fn eval_checked(
        &mut self,
        graph: &Graph,
        node: NodeId,
        member: bool,
        image: ImageId,
        env: &Env,
        cancel: &CancelToken,
    ) -> Result<Value, EvalError> {
        if !member {
            return Err(not_a_member_error(graph, node));
        }
        let mut visiting = Vec::new();
        let mut memo = FingerprintMemo::new();
        let result = self.eval_node(graph, node, image, env, cancel, &mut visiting, &mut memo).map(|(_, v)| v);
        if !self.keep_intermediates {
            self.cache.retain(|(n, im, _), _| *im != image || *n == node);
            self.pre_tone.retain(|(_, im, _), _| *im != image);
        }
        result
    }

    /// Splits evaluating `node` (output port 0) for `image` into a CPU base and a trailing
    /// run of pointwise operations (see [`TailOp`]), so a caller can keep the base image
    /// (for example on a GPU) and re-apply only the tail when a tail parameter changes.
    ///
    /// The walk goes upstream along the image input (following the edge's output port,
    /// and for a Merge the input the image comes from). Output, Export, Metadata Filter,
    /// Metadata Split and Merge are transparent. Capture EV, unmasked Exposure, Develop,
    /// White Balance, unmasked Saturation and Contrast, Highlights/Shadows, Color Wheels,
    /// Tone Curve, Curves (one operation per curve), Hue/Saturation and Vignette contribute
    /// operations (none when neutral, exactly where the evaluator returns its input). Black
    /// & White is always one `Matrix` with three identical rows (bit-identical to
    /// `fx::black_white`). Every tail operation keeps the image size, which is what makes
    /// the position-dependent Vignette valid in a tail. Any other node (Clarity, Blur, Noise
    /// Reduction, Sharpen, Transform, Lens Correction, Layer), and
    /// Exposure/Contrast/Saturation with a connected mask, is the base.
    ///
    /// A Develop node with a non-neutral tone part (exposure, contrast, lights, shadows,
    /// white, black, dynamic, saturation) ends the walk: its tone part becomes the first
    /// tail operations, the plan has `base_before_tone = true` and the Develop node as base.
    /// [`Evaluator::eval_base`] then returns the Develop output before the tone part, cached
    /// separately, so a tone slider never invalidates the base. A Develop node with a
    /// neutral tone part is transparent as before (its white balance and brightness become
    /// tail operations).
    ///
    /// Touches no pixels (`provider.image` is never called) and returns the structural
    /// errors `eval` would return. `base_fingerprint` is the fingerprint the cache stores for
    /// the base, so it changes exactly when `eval_base` would recompute.
    pub fn plan_tail(&self, graph: &Graph, node: NodeId, image: ImageId, provider: &dyn ImageProvider) -> Result<TailPlan, EvalError> {
        let images = provider.images();
        let env = Env { provider, images: &images, view_tone: self.view_tone, control: false };
        if !env.viewing_member(graph, node, image) {
            return Err(not_a_member_error(graph, node));
        }
        // Validates the whole structure below `node` exactly like `eval` and memoises
        // the fingerprints used for the base.
        let mut memo = FingerprintMemo::new();
        node_fingerprint(graph, node, image, &env, &mut memo, &mut Vec::new())?;

        let mut groups: Vec<Vec<TailOp>> = Vec::new();
        let mut visited: Vec<NodeId> = Vec::new();
        let (mut cur, mut cur_port) = (node, 0usize);
        let mut base_before_tone = false;
        loop {
            let n = graph.node(cur).ok_or(EvalError::UnknownNode(cur))?;
            if output_channel(n, cur_port).is_some() { break; }
            if visited.contains(&cur) {
                return Err(EvalError::MissingInput { node: cur, port: 0 });
            }
            visited.push(cur);
            match tail_step(graph, n, image, provider) {
                TailStep::Base => break,
                TailStep::Through(ops) => groups.push(ops),
                TailStep::Tone(ops) => {
                    groups.push(ops);
                    base_before_tone = true;
                    break;
                }
            }
            let port = if n.kind == NodeKind::Merge {
                env.cat(|c| graph.merge_source_in(cur, image, c)).ok_or(EvalError::NotInCollection { node: cur })?
            } else {
                0
            };
            let (up, up_port) = env.input_edge(graph, cur, port, image).ok_or(EvalError::MissingInput { node: cur, port })?;
            (cur, cur_port) = (up, up_port);
        }
        let ops: Vec<TailOp> = groups.into_iter().rev().flatten().collect();
        let base_fingerprint = if base_before_tone {
            let develop = graph.node(cur).ok_or(EvalError::UnknownNode(cur))?;
            pre_tone_fingerprint(graph, develop, image, &env, &mut memo)?
        } else {
            match memo.get(&(cur, false)) {
                Some(&fp) => fp,
                None => node_fingerprint(graph, cur, image, &env, &mut memo, &mut Vec::new())?,
            }
        };
        let base_fingerprint = if output_channel(graph.node(cur).unwrap(), cur_port).is_some() { let mut h = DefaultHasher::new(); base_fingerprint.hash(&mut h); cur_port.hash(&mut h); output_channel(graph.node(cur).unwrap(), cur_port).hash(&mut h); h.finish() } else { base_fingerprint };
        Ok(TailPlan { base: cur, base_port: cur_port, ops, base_fingerprint, base_before_tone })
    }

    /// Evaluates the plan's base on the CPU (cached like any evaluation).
    /// If `base_before_tone` is set, this is the Develop output before its tone part.
    pub fn eval_base(
        &mut self,
        graph: &Graph,
        plan: &TailPlan,
        image: ImageId,
        provider: &dyn ImageProvider,
        cancel: &CancelToken,
    ) -> Result<Arc<ImageBuf>, EvalError> {
        let images = provider.images();
        let env = Env { provider, images: &images, view_tone: self.view_tone, control: false };
        if plan.base_before_tone {
            if !env.contains_output(graph, plan.base, plan.base_port, image) {
                return Err(not_a_member_error(graph, plan.base));
            }
            let result = self.eval_develop_pre(graph, plan.base, image, &env, cancel);
            if !self.keep_intermediates {
                self.cache.retain(|(_, im, _), _| *im != image);
                self.pre_tone.retain(|(n, im, _), _| *im != image || *n == plan.base);
            }
            return result;
        }
        match self.eval_output_env(graph, plan.base, plan.base_port, image, &env, cancel)? {
            Value::Images(img) => Ok(img),
        }
    }

    /// True if `image` leaves output `port` of `node` (metadata only).
    pub fn contains_output(&self, graph: &Graph, node: NodeId, port: usize, image: ImageId, provider: &dyn ImageProvider) -> bool {
        let images = provider.images();
        Env { provider, images: &images, view_tone: self.view_tone, control: false }.contains_output(graph, node, port, image)
    }

    /// True if `image` is part of the collection `node` outputs (metadata only).
    pub fn contains(&self, graph: &Graph, node: NodeId, image: ImageId, provider: &dyn ImageProvider) -> bool {
        let images = provider.images();
        Env { provider, images: &images, view_tone: self.view_tone, control: false }.cat(|c| graph.contains_in(node, image, c))
    }

    /// Evaluate the same auxiliary image used by a consumer, including output
    /// aliases, singleton fallback, and Generator data interpretation.
    pub fn eval_control_image(&mut self, graph: &Graph, node: NodeId, port: usize, image: ImageId, provider: &dyn ImageProvider, cancel: &CancelToken) -> Result<Option<Arc<ImageBuf>>, EvalError> {
        let images = provider.images();
        let env = Env { provider, images: &images, view_tone: self.view_tone, control: true };
        if !graph.node(node).is_some_and(|n| n.is_control_input(port)) { return Ok(None); }
        let Some((up, output, image)) = env.control_edge(graph, node, port, image) else { return Ok(None) };
        let (_, value) = self.eval_node(graph, up, image, &env, cancel, &mut Vec::new(), &mut FingerprintMemo::new())?;
        let Value::Images(img) = output_value(graph, up, output, value);
        self.release(&[up], image);
        Ok(Some(img))
    }

    /// The ordered collection leaving output `port` of `node` (metadata only).
    /// Order depends on the node: catalog for Source, driver for Layer, ports for Merge.
    pub fn members(&self, graph: &Graph, node: NodeId, port: usize, provider: &dyn ImageProvider) -> Vec<ImageId> {
        let images = provider.images();
        Env { provider, images: &images, view_tone: self.view_tone, control: false }.cat(|c| graph.members(node, port, c))
    }

    /// When `false`, only the requested node's result stays cached after `eval`
    /// and upstream results are released once their consumer is computed.
    pub fn set_keep_intermediates(&mut self, keep: bool) {
        self.keep_intermediates = keep;
    }

    /// Set the viewer tone used by Display-mode generators; exposure is intentionally excluded.
    pub fn set_view_tone(&mut self, tone: ToneMap) { self.view_tone = tone; }

    /// Like `eval` but requires an image result.
    pub fn eval_image(
        &mut self,
        graph: &Graph,
        node: NodeId,
        image: ImageId,
        provider: &dyn ImageProvider,
        cancel: &CancelToken,
    ) -> Result<Arc<ImageBuf>, EvalError> {
        let images = provider.images();
        self.eval_image_env(graph, node, image, &Env { provider, images: &images, view_tone: self.view_tone, control: false }, cancel)
    }

    fn eval_image_env(&mut self, graph: &Graph, node: NodeId, image: ImageId, env: &Env, cancel: &CancelToken) -> Result<Arc<ImageBuf>, EvalError> {
        match self.eval_env(graph, node, image, env, cancel)? {
            Value::Images(img) => Ok(img),
        }
    }

    /// Renders `node` for every image, in input order. Failures are per image.
    pub fn render_batch(
        &mut self,
        graph: &Graph,
        node: NodeId,
        images: &[ImageId],
        provider: &dyn ImageProvider,
        cancel: &CancelToken,
        progress: &mut dyn FnMut(usize, usize),
    ) -> Vec<(ImageId, Result<Arc<ImageBuf>, EvalError>)> {
        let catalog = provider.images();
        let env = Env { provider, images: &catalog, view_tone: self.view_tone, control: false };
        let listed: HashSet<ImageId> = images.iter().copied().collect();
        let total = images.len();
        let mut out = Vec::with_capacity(total);
        for (i, &img) in images.iter().enumerate() {
            let result = if cancel.is_cancelled() {
                Err(EvalError::Cancelled)
            } else {
                self.eval_image_env(graph, node, img, &env, cancel)
            };
            self.cache.retain(|(_, im, _), _| *im != img);
            self.pre_tone.retain(|(_, im, _), _| *im != img);
            if !self.keep_intermediates {
                // A Layer node evaluates other images of the project; do not let them pile up.
                self.cache.retain(|(_, im, _), _| listed.contains(im));
                self.pre_tone.retain(|(_, im, _), _| listed.contains(im));
            }
            out.push((img, result));
            progress(i + 1, total);
        }
        out
    }

    /// Drops cached results of every image except `image`.
    pub fn retain_image(&mut self, image: ImageId) {
        self.cache.retain(|(_, im, _), _| *im == image);
        self.pre_tone.retain(|(_, im, _), _| *im == image);
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.pre_tone.clear();
    }

    /// Cache hits since creation.
    pub fn hits(&self) -> u64 {
        self.hits
    }

    /// Nodes actually computed since creation.
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Start or disable request-local profiling. Disabled evaluation reads no profiling clock.
    pub fn set_timing_enabled(&mut self, enabled: bool) {
        self.timings = enabled.then(Vec::new);
        self.timing_children.clear();
        self.timing_hit.clear();
    }

    pub fn timings(&self) -> Option<Vec<NodeTiming>> { self.timings.clone() }

    #[allow(clippy::too_many_arguments)]
    fn eval_node(
        &mut self,
        graph: &Graph,
        id: NodeId,
        image: ImageId,
        env: &Env,
        cancel: &CancelToken,
        visiting: &mut Vec<NodeId>,
        memo: &mut FingerprintMemo,
    ) -> Result<(u64, Value), EvalError> {
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        let node = graph.node(id).ok_or(EvalError::UnknownNode(id))?;
        if visiting.contains(&id) {
            // Graphs built through `connect` are acyclic; a hand-edited file may not be.
            return Err(EvalError::MissingInput { node: id, port: 0 });
        }
        let start = self.timings.as_ref().map(|_| std::time::Instant::now());
        if start.is_some() {
            self.timing_children.push(std::time::Duration::ZERO);
            self.timing_hit.push(false);
        }
        visiting.push(id);
        let result = self.eval_node_inner(graph, node, image, env, cancel, visiting, memo);
        visiting.pop();
        if let Some(start) = start {
            let elapsed = start.elapsed();
            let children = self.timing_children.pop().unwrap();
            let hit = self.timing_hit.pop().unwrap();
            if let Some(parent) = self.timing_children.last_mut() { *parent += elapsed; }
            if result.is_ok() {
                let rows = self.timings.as_mut().unwrap();
                let i = rows.iter().position(|r| r.node == id).unwrap_or_else(|| {
                    rows.push(NodeTiming { node: id, cpu: std::time::Duration::ZERO, computed: 0, cache_hits: 0 });
                    rows.len() - 1
                });
                if hit { rows[i].cache_hits += 1; } else {
                    rows[i].computed += 1;
                    rows[i].cpu += elapsed.saturating_sub(children);
                }
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_node_inner(
        &mut self,
        graph: &Graph,
        node: &rawgraph_core::Node,
        image: ImageId,
        env: &Env,
        cancel: &CancelToken,
        visiting: &mut Vec<NodeId>,
        memo: &mut FingerprintMemo,
    ) -> Result<(u64, Value), EvalError> {
        let id = node.id;
        let provider = env.provider;
        // Pure function of graph, image and provider metadata; memoised for this evaluation.
        let fingerprint = node_fingerprint(graph, id, image, env, memo, &mut Vec::new())?;
        if node.kind == NodeKind::Layer {
            return self.eval_layer(graph, node, image, fingerprint, env, cancel, visiting);
        }
        let chosen = if node.kind == NodeKind::Merge {
            Some(env.cat(|c| graph.merge_source_in(id, image, c)).ok_or(EvalError::NotInCollection { node: id })?)
        } else {
            None
        };
        let mut used: Vec<NodeId> = Vec::new();

        let specs = node.kind.inputs();
        let mut inputs: Vec<Option<Value>> = vec![None; specs.len() + node.exposed_inputs().len()];
        for (port, spec) in specs.iter().enumerate() {
            if chosen.is_some_and(|c| c != port) { continue; }
            match env.input_edge(graph, id, port, image) {
                Some((up, up_port)) => {
                    used.push(up);
                    let (_, value) = self.eval_node(graph, up, image, env, cancel, visiting, memo)?;
                    inputs[port] = Some(output_value(graph, up, up_port, value));
                }
                None if spec.required => return Err(EvalError::MissingInput { node: id, port }),
                None => {}
            }
        }
        for (i, exposed) in node.exposed_inputs().iter().enumerate() {
            if exposed.is_none() { continue; }
            let port = specs.len() + i;
            if let Some((up, up_port, up_image)) = env.control_edge(graph, id, port, image) {
                let mut other_memo = FingerprintMemo::new();
                let (_, value) = self.eval_node(graph, up, up_image, &env.control(true), cancel, visiting, if up_image == image { memo } else { &mut other_memo })?;
                inputs[port] = Some(output_value(graph, up, up_port, value));
                if up_image == image { used.push(up); } else { self.release(&[up], up_image); }
            }
        }

        let key: Key = (id, image, env.control);
        if let Some(entry) = self.cache.get(&key) {
            if entry.fingerprint == fingerprint {
                self.hits += 1;
                if let Some(hit) = self.timing_hit.last_mut() { *hit = true; }
                let value = entry.value.clone();
                self.release(&used, image);
                return Ok((fingerprint, value));
            }
        }
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        let value = if node.kind == NodeKind::Develop {
            self.compute_develop(graph, node, image, &inputs, env, memo)?
        } else {
            let lens = if node.kind == NodeKind::LensManual { lens_profile_for(graph, node, image, provider) } else { None };
            compute(node, image, &inputs, provider, lens.as_ref(), env)?
        };
        self.misses += 1;
        self.cache.insert(key, Entry { fingerprint, value: value.clone() });
        self.release(&used, image);
        Ok((fingerprint, value))
    }

    /// Develop: the base part (white balance, baseline, brightness) comes from the
    /// pre-tone cache whenever a tone part follows, then the tone operations are applied.
    fn compute_develop(
        &mut self,
        graph: &Graph,
        node: &rawgraph_core::Node,
        image: ImageId,
        inputs: &[Option<Value>],
        env: &Env,
        memo: &mut FingerprintMemo,
    ) -> Result<Value, EvalError> {
        let src = input_image(inputs, 0, node.id)?;
        let values = develop_tone(node, image);
        let local = node.choice("tone_version", image) == 1;
        let tone = tone_ops(&values);
        let mapped: HashMap<_, _> = Graph::supported_parameter_inputs(node.kind).iter()
            .filter_map(|&key| parameter_values(node, image, inputs, key, &src).map(|values| (key, values)))
            .collect();
        if !mapped.is_empty() {
            let fp_pre = pre_tone_fingerprint(graph, node, image, env, memo)?;
            let pre = self.develop_pre_from(node, image, src, fp_pre, env.provider, env.control);
            let get = |name| mapped.get(name).map(Vec::as_slice);
            let maps = ops::DevelopControlMaps { exposure: get("exposure"), contrast: get("contrast"), highlights: get("highlights"), shadows: get("shadows"), whites: get("whites"), blacks: get("blacks") };
            return Ok(modified(&pre, |img| {
                if local {
                    let mut exposure = DevelopTone::default(); exposure.exposure = values.exposure;
                    ops::develop_legacy_mapped(img, exposure, &ops::DevelopControlMaps { exposure: maps.exposure, ..Default::default() });
                    ops::develop_local_tone_mapped(img, values.highlights, values.shadows, values.whites, values.blacks, &maps);
                    let mut contrast = DevelopTone::default(); contrast.contrast = values.contrast;
                    ops::develop_legacy_mapped(img, contrast, &ops::DevelopControlMaps { contrast: maps.contrast, ..Default::default() });
                } else { ops::develop_legacy_mapped(img, values, &maps); }
                let mut rest = values;
                rest.exposure = 0.0; rest.whites = 0.0; rest.blacks = 0.0; rest.highlights = 0.0; rest.shadows = 0.0; rest.contrast = 0.0;
                for op in tone_ops(&rest) { apply_op(img, &op); }
            }));
        }
        if tone.is_empty() {
            return Ok(Value::Images(develop_base(node, image, src, env.provider)));
        }
        let fp_pre = pre_tone_fingerprint(graph, node, image, env, memo)?;
        let pre = self.develop_pre_from(node, image, src, fp_pre, env.provider, env.control);
        Ok(modified(&pre, |img| {
            if local {
                if values.exposure != 0.0 { ops::apply_gain(img, 2f32.powf(values.exposure)); }
                ops::develop_local_tone(img, values.highlights, values.shadows, values.whites, values.blacks);
                let mut rest = values;
                rest.exposure = 0.0; rest.whites = 0.0; rest.blacks = 0.0; rest.highlights = 0.0; rest.shadows = 0.0;
                for op in tone_ops(&rest) { apply_op(img, &op); }
            } else {
                for op in &tone { apply_op(img, op); }
            }
        }))
    }

    /// The Develop base part for an already evaluated input, through the pre-tone cache.
    /// A neutral base part returns the input itself and is not cached.
    fn develop_pre_from(
        &mut self,
        node: &rawgraph_core::Node,
        image: ImageId,
        src: Arc<ImageBuf>,
        fp_pre: u64,
        provider: &dyn ImageProvider,
        control: bool,
    ) -> Arc<ImageBuf> {
        let capture = provider.capture(image);
        if develop_base_is_neutral(&develop_settings(node, image, capture.as_ref())) {
            return src;
        }
        let key = (node.id, image, control);
        if let Some(entry) = self.pre_tone.get(&key) {
            if entry.fingerprint == fp_pre {
                let Value::Images(img) = &entry.value;
                self.hits += 1;
                return img.clone();
            }
        }
        let out = develop_base(node, image, src, provider);
        self.misses += 1;
        self.pre_tone.insert(key, Entry { fingerprint: fp_pre, value: Value::Images(out.clone()) });
        out
    }

    /// Output of the Develop node `id` before its tone part (see `plan_tail`).
    fn eval_develop_pre(
        &mut self,
        graph: &Graph,
        id: NodeId,
        image: ImageId,
        env: &Env,
        cancel: &CancelToken,
    ) -> Result<Arc<ImageBuf>, EvalError> {
        let node = graph.node(id).ok_or(EvalError::UnknownNode(id))?;
        if node.kind != NodeKind::Develop {
            return Err(EvalError::WrongType { node: id, port: 0 });
        }
        let mut memo = FingerprintMemo::new();
        node_fingerprint(graph, id, image, env, &mut memo, &mut Vec::new())?;
        let fp_pre = pre_tone_fingerprint(graph, node, image, env, &mut memo)?;
        if let Some(entry) = self.pre_tone.get(&(id, image, env.control)) {
            if entry.fingerprint == fp_pre {
                let Value::Images(img) = &entry.value;
                self.hits += 1;
                return Ok(img.clone());
            }
        }
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        let (up, up_port) = env.input_edge(graph, id, 0, image).ok_or(EvalError::MissingInput { node: id, port: 0 })?;
        let mut visiting = vec![id];
        let (_, value) = self.eval_node(graph, up, image, env, cancel, &mut visiting, &mut memo)?;
        let Value::Images(src) = output_value(graph, up, up_port, value);
        let out = self.develop_pre_from(node, image, src, fp_pre, env.provider, env.control);
        self.release(&[up], image);
        Ok(out)
    }

    /// Layer: every switched-on layer is evaluated for the image it contributes to this
    /// output image, placed on the canvas of the lowest one and blended in ascending order.
    #[allow(clippy::too_many_arguments)]
    fn eval_layer(
        &mut self,
        graph: &Graph,
        node: &rawgraph_core::Node,
        image: ImageId,
        fingerprint: u64,
        env: &Env,
        cancel: &CancelToken,
        visiting: &mut Vec<NodeId>,
    ) -> Result<(u64, Value), EvalError> {
        let key: Key = (node.id, image, env.control);
        if let Some(entry) = self.cache.get(&key) {
            if entry.fingerprint == fingerprint {
                self.hits += 1;
                if let Some(hit) = self.timing_hit.last_mut() { *hit = true; }
                return Ok((fingerprint, entry.value.clone()));
            }
        }
        if cancel.is_cancelled() {
            return Err(EvalError::Cancelled);
        }
        let plan = layer_plan(node, image, env, graph)?;
        let Some(lowest) = plan.lowest else {
            return Err(EvalError::NotInCollection { node: node.id });
        };
        let first = plan.active.first().copied();
        let canvas_layer = first.unwrap_or(lowest);
        let canvas_src = plan.srcs[canvas_layer].ok_or(EvalError::NotInCollection { node: node.id })?;
        let canvas_img = self.eval_layer_input(graph, node, canvas_layer, canvas_src, env, cancel, visiting)?;
        let (cw, ch) = (canvas_img.width(), canvas_img.height());
        let mut canvas = match first {
            Some(_) => {
                let mut c = (*canvas_img).clone();
                let opacity = plan.settings[canvas_layer].opacity;
                let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
                if opacity != 1.0 {
                    for px in c.data_mut().as_chunks_mut::<4>().0.iter_mut() {
                        px[0] *= opacity;
                        px[1] *= opacity;
                        px[2] *= opacity;
                    }
                }
                c
            }
            None => {
                // No layer switched on: opaque black in the size of the lowest connected layer.
                let mut c = ImageBuf::new(cw, ch);
                for px in c.data_mut().as_chunks_mut::<4>().0.iter_mut() {
                    px[3] = 1.0;
                }
                c
            }
        };
        let canvas_full = provider_full_size(env.provider, canvas_src);
        for &layer in plan.active.iter().skip(1) {
            if cancel.is_cancelled() {
                return Err(EvalError::Cancelled);
            }
            let src = plan.srcs[layer].ok_or(EvalError::NotInCollection { node: node.id })?;
            let layer_img = self.eval_layer_input(graph, node, layer, src, env, cancel, visiting)?;
            let s = &plan.settings[layer];
            let scale = crop_scale((cw, ch), canvas_full, (layer_img.width(), layer_img.height()), provider_full_size(env.provider, src));
            let placed = fx::place_layer(&layer_img, cw, ch, s.fit, s.filter, scale);
            fx::blend_layer(&mut canvas, &placed, s.blend, s.opacity);
        }
        let value = Value::Images(Arc::new(canvas));
        self.misses += 1;
        self.cache.insert(key, Entry { fingerprint, value: value.clone() });
        Ok((fingerprint, value))
    }

    /// Evaluates the input of layer `layer` for the image `src` it contributes.
    #[allow(clippy::too_many_arguments)]
    fn eval_layer_input(
        &mut self,
        graph: &Graph,
        node: &rawgraph_core::Node,
        layer: usize,
        src: ImageId,
        env: &Env,
        cancel: &CancelToken,
        visiting: &mut Vec<NodeId>,
    ) -> Result<Arc<ImageBuf>, EvalError> {
        let (up, up_port) = graph.input_edge(node.id, layer).ok_or(EvalError::MissingInput { node: node.id, port: layer })?;
        // Fingerprints are per image, so a source image gets its own memo.
        let mut memo = FingerprintMemo::new();
        let (_, value) = self.eval_node(graph, up, src, env, cancel, visiting, &mut memo)?;
        self.release(&[up], src);
        match output_value(graph, up, up_port, value) {
            Value::Images(img) => Ok(img),
        }
    }

    /// In lean mode, drops the cached results of nodes whose consumer is done.
    fn release(&mut self, nodes: &[NodeId], image: ImageId) {
        if !self.keep_intermediates {
            self.cache.retain(|(n, im, _), _| *im != image || !nodes.contains(n));
        }
    }
}

/// Layers of a Layer node for one output image.
struct LayerPlan {
    /// Source image per layer (`None`: not connected, empty input).
    srcs: [Option<ImageId>; MAX_LAYERS],
    settings: Vec<LayerSettings>,
    /// Switched-on layers with a source, bottom first. The first one is the canvas.
    active: Vec<usize>,
    /// The lowest connected layer with a source; sizes the black canvas when nothing is on.
    lowest: Option<usize>,
}

fn layer_plan(node: &rawgraph_core::Node, image: ImageId, env: &Env, graph: &Graph) -> Result<LayerPlan, EvalError> {
    let srcs = env.cat(|c| graph.layer_sources(node.id, image, c)).ok_or(EvalError::NotInCollection { node: node.id })?;
    let settings: Vec<LayerSettings> = (0..MAX_LAYERS).map(|i| layer_settings(node, i, image)).collect();
    let connected = |i: usize| srcs[i].is_some() && graph.input_edge(node.id, i).is_some();
    let lowest = (0..MAX_LAYERS).find(|&i| connected(i));
    let active = (0..MAX_LAYERS).filter(|&i| connected(i) && settings[i].on).collect();
    Ok(LayerPlan { srcs, settings, active, lowest })
}

fn provider_full_size(provider: &dyn ImageProvider, id: ImageId) -> Option<(u32, u32)> {
    provider.full_size(id).filter(|&(w, h)| w > 0 && h > 0)
}

/// Scale of a "Crop" layer: one layer pixel of the full-resolution file stays one canvas
/// pixel of the full-resolution file. `(canvas proxy / canvas full) / (layer proxy / layer
/// full)` over the long edges; 1.0 when a full size is unknown.
fn crop_scale(canvas: (u32, u32), canvas_full: Option<(u32, u32)>, layer: (u32, u32), layer_full: Option<(u32, u32)>) -> f32 {
    let (Some(cf), Some(lf)) = (canvas_full, layer_full) else { return 1.0 };
    let long = |(w, h): (u32, u32)| w.max(h) as f64;
    let ratio = (long(canvas) / long(cf)) / (long(layer) / long(lf));
    if ratio.is_finite() && ratio > 0.0 {
        ratio as f32
    } else {
        1.0
    }
}

/// The lens profile an Auto Lens Correction node applies to `image`, if any.
fn lens_profile_for(graph: &Graph, node: &rawgraph_core::Node, image: ImageId, provider: &dyn ImageProvider) -> Option<LensProfile> {
    if node.choice("mode", image) == 1 && graph.lens_auto_usable(node.id) {
        provider.lens_profile(image)
    } else {
        None
    }
}


/// Everything `develop_settings` reads from the capture metadata.
fn hash_develop_capture(capture: Option<&CaptureInfo>, hasher: &mut DefaultHasher) {
    let bits = |v: Option<f32>| v.map(f32::to_bits);
    match capture {
        None => 0u8.hash(hasher),
        Some(c) => {
            1u8.hash(hasher);
            c.raw.hash(hasher);
            bits(c.as_shot_kelvin).hash(hasher);
            bits(c.as_shot_tint).hash(hasher);
            bits(c.baseline_exposure).hash(hasher);
        }
    }
}

/// Hashes the content of a node's effective data (never an address): variant tags,
/// lengths, flags, strings and the bit patterns of floats, fed straight into the hasher.
fn hash_node_data(data: Option<&NodeData>, hasher: &mut DefaultHasher) {
    fn point(p: &[f32; 2], hasher: &mut DefaultHasher) {
        hasher.write_u32(p[0].to_bits());
        hasher.write_u32(p[1].to_bits());
    }
    match data {
        None => hasher.write_u8(0),
        Some(NodeData::GradientRamp(r)) => {
            hasher.write_u8(7);
            hasher.write_usize(r.colors.len());
            for s in &r.colors { hasher.write_u32(s.position.to_bits()); for v in s.color { hasher.write_u32(v.to_bits()); } hasher.write_u32(s.midpoint.to_bits()); }
            hasher.write_usize(r.opacities.len());
            for s in &r.opacities { hasher.write_u32(s.position.to_bits()); hasher.write_u32(s.opacity.to_bits()); hasher.write_u32(s.midpoint.to_bits()); }
        }
        Some(NodeData::Curve(points)) => {
            hasher.write_u8(1);
            hasher.write_usize(points.len());
            for p in points {
                point(p, hasher);
            }
        }
        Some(NodeData::Strokes(strokes)) => {
            hasher.write_u8(2);
            hasher.write_usize(strokes.len());
            for s in strokes {
                hasher.write_usize(s.points.len());
                for p in &s.points {
                    point(p, hasher);
                }
                hasher.write_u32(s.radius.to_bits());
                hasher.write_u32(s.hardness.to_bits());
                hasher.write_u32(s.flow.to_bits());
                hasher.write_u8(u8::from(s.erase));
            }
        }
        Some(NodeData::Ranges(ranges)) => {
            hasher.write_u8(3);
            hasher.write_usize(ranges.len());
            for r in ranges {
                r.name.hash(hasher);
                hasher.write_u32(r.min.to_bits());
                hasher.write_u32(r.max.to_bits());
            }
        }
        Some(NodeData::CurveSet(entries)) => {
            hasher.write_u8(4);
            hasher.write_usize(entries.len());
            for e in entries {
                hasher.write_u8(e.channel);
                hasher.write_usize(e.points.len());
                for p in &e.points {
                    point(p, hasher);
                }
            }
        }
        Some(NodeData::Folder(path)) => {
            hasher.write_u8(5);
            path.hash(hasher);
        }
        Some(NodeData::Export(text)) => {
            hasher.write_u8(6);
            text.out_dir.hash(hasher);
            text.name_template.hash(hasher);
            text.artist.hash(hasher);
            text.copyright.hash(hasher);
            text.description.hash(hasher);
        }
    }
}

/// The result of the lens profile: presence and every number.
fn hash_lens_profile(profile: Option<&LensProfile>, hasher: &mut DefaultHasher) {
    fn f(v: f64, hasher: &mut DefaultHasher) {
        hasher.write_u64(v.to_bits());
    }
    let Some(profile) = profile else {
        hasher.write_u8(0);
        return;
    };
    hasher.write_u8(1);
    match &profile.warp {
        None => hasher.write_u8(0),
        Some(w) => {
            hasher.write_u8(1);
            hasher.write_usize(w.planes.len());
            for plane in &w.planes {
                plane.iter().for_each(|&v| f(v, hasher));
            }
            w.center.iter().for_each(|&v| f(v, hasher));
        }
    }
    match &profile.vignette {
        None => hasher.write_u8(0),
        Some(v) => {
            hasher.write_u8(1);
            v.k.iter().for_each(|&x| f(x, hasher));
            v.center.iter().for_each(|&x| f(x, hasher));
        }
    }
}

/// Fingerprints computed during one evaluation (the image is fixed, so a node has one).
type FingerprintMemo = HashMap<(NodeId, bool), u64>;

/// Parameters of the Develop tone part; they are left out of the pre-tone fingerprint.
const TONE_PARAMS: [&str; 8] = ["exposure", "contrast", "highlights", "shadows", "whites", "blacks", "vibrance", "saturation"];

/// Fingerprint of everything the output of `id` depends on for `image`: structure,
/// effective parameters and node data, provider revision, capture metadata where a node
/// reads it, membership decisions (source folder, merge source, filter, split port, layer
/// pairing) and the fingerprints of its inputs. Pure: touches no pixels. The evaluator's
/// cache and [`Evaluator::plan_tail`] both use it, so they cannot disagree. It reports the
/// structural errors `eval` reports.
///
/// image (structure, parameters, provider revision) and is part of the cache key.
fn node_fingerprint(
    graph: &Graph,
    id: NodeId,
    image: ImageId,
    env: &Env,
    memo: &mut FingerprintMemo,
    visiting: &mut Vec<NodeId>,
) -> Result<u64, EvalError> {
    if let Some(&fp) = memo.get(&(id, env.control)) {
        return Ok(fp);
    }
    let node = graph.node(id).ok_or(EvalError::UnknownNode(id))?;
    if visiting.contains(&id) {
        // Graphs built through `connect` are acyclic; a hand-edited file may not be.
        return Err(EvalError::MissingInput { node: id, port: 0 });
    }
    visiting.push(id);
    let result = node_fingerprint_inner(graph, node, image, env, memo, visiting, false);
    visiting.pop();
    if let Ok(fp) = result {
        memo.insert((id, env.control), fp);
    }
    result
}

/// Fingerprint of a Develop node's output before its tone part (tone parameters left out).
/// This is the identity of the cached pre-tone base.
fn pre_tone_fingerprint(
    graph: &Graph,
    node: &rawgraph_core::Node,
    image: ImageId,
    env: &Env,
    memo: &mut FingerprintMemo,
) -> Result<u64, EvalError> {
    node_fingerprint_inner(graph, node, image, env, memo, &mut vec![node.id], true)
}

fn node_fingerprint_inner(
    graph: &Graph,
    node: &rawgraph_core::Node,
    image: ImageId,
    env: &Env,
    memo: &mut FingerprintMemo,
    visiting: &mut Vec<NodeId>,
    skip_tone: bool,
) -> Result<u64, EvalError> {
    let id = node.id;
    let provider = env.provider;
    let mut hasher = DefaultHasher::new();
    node.kind.hash(&mut hasher);
    env.control.hash(&mut hasher);
    if node.kind == NodeKind::Generator && node.choice("output", image) == 0 && !env.control {
        (env.view_tone as u8).hash(&mut hasher);
    }
    if node.kind == NodeKind::DepthMap {
        std::env::var_os("RAWGRAPH_DEPTH_WORKER").hash(&mut hasher);
        std::env::var_os("RAWGRAPH_DEPTH_SCRIPT").hash(&mut hasher);
        std::env::var_os("RAWGRAPH_DEPTH_MODEL").hash(&mut hasher);
    }
    image.hash(&mut hasher);
    provider.revision(image).hash(&mut hasher);
    for spec in node.kind.params() {
        if skip_tone && node.kind == NodeKind::Develop && TONE_PARAMS.contains(&spec.name) {
            continue;
        }
        node.param(spec.name, image).to_bits().hash(&mut hasher);
    }
    let chosen = if node.kind == NodeKind::Merge {
        let port = env.cat(|c| graph.merge_source_in(id, image, c)).ok_or(EvalError::NotInCollection { node: id })?;
        port.hash(&mut hasher);
        Some(port)
    } else {
        None
    };
    if matches!(node.kind, NodeKind::MetadataFilter | NodeKind::Source) {
        // Filter: the metadata decision. Source: the folder membership (the folder itself is
        // node data, but the answer also depends on the image's path).
        env.cat(|c| graph.contains_in(id, image, c)).hash(&mut hasher);
    }
    if node.kind == NodeKind::Develop {
        hash_develop_capture(provider.capture(image).as_ref(), &mut hasher);
    }
    if node.kind == NodeKind::NoiseReduction && node.toggle("iso_adaptive", image) {
        // The strengths are scaled by the capture ISO, so it is part of the result.
        provider.capture(image).and_then(|c| c.iso).map(f32::to_bits).hash(&mut hasher);
    }
    if node.kind == NodeKind::MetadataSplit {
        // Which range an image falls into depends on its metadata, not on the pixels.
        env.cat(|c| graph.split_port_in(id, image, c)).hash(&mut hasher);
    }
    if node.kind == NodeKind::LensManual {
        // Auto: the profile of the file, and whether the node may use it.
        let usable = graph.lens_auto_usable(id);
        usable.hash(&mut hasher);
        hash_lens_profile(lens_profile_for(graph, node, image, provider).as_ref(), &mut hasher);
    }
    // The effective data (curves, folder, strokes, ranges, export text) for this image, hashed by content.
    hash_node_data(node.data(image), &mut hasher);

    if node.kind == NodeKind::Layer {
        // A layer input is evaluated for the image that layer contributes, not for `image`.
        let lp = layer_plan(node, image, env, graph)?;
        lp.srcs.hash(&mut hasher);
        let mut used = lp.active.clone();
        if let Some(l) = lp.lowest {
            if !used.contains(&l) {
                used.push(l);
            }
        }
        let mut memos: HashMap<ImageId, FingerprintMemo> = HashMap::new();
        for &layer in &used {
            let Some(src) = lp.srcs[layer] else { continue };
            layer.hash(&mut hasher);
            src.hash(&mut hasher);
            provider_full_size(provider, src).hash(&mut hasher);
            let (up, up_port) = graph.input_edge(id, layer).ok_or(EvalError::MissingInput { node: id, port: layer })?;
            up_port.hash(&mut hasher);
            output_channel(graph.node(up).unwrap(), up_port).hash(&mut hasher);
            let fp = node_fingerprint(graph, up, src, env, memos.entry(src).or_default(), visiting)?;
            if graph.node(up).and_then(|u| u.kind.output()) != Some(PortType::Images) {
                return Err(EvalError::WrongType { node: id, port: layer });
            }
            fp.hash(&mut hasher);
        }
        return Ok(hasher.finish());
    }

    let specs = node.kind.inputs();
    for (port, spec) in specs.iter().enumerate() {
        if chosen.is_some_and(|c| c != port) { continue; }
        match env.input_edge(graph, id, port, image) {
            Some((up, up_port)) => {
                up_port.hash(&mut hasher);
                output_channel(graph.node(up).unwrap(), up_port).hash(&mut hasher);
                if graph.node(up).is_some_and(|u| u.kind == NodeKind::MetadataSplit) {
                    env.cat(|c| graph.split_port_in(up, image, c)).hash(&mut hasher);
                }
                node_fingerprint(graph, up, image, env, memo, visiting)?.hash(&mut hasher);
            }
            None if spec.required => return Err(EvalError::MissingInput { node: id, port }),
            None => u64::MAX.hash(&mut hasher),
        }
    }
    if skip_tone { return Ok(hasher.finish()); }
    for (i, exposed) in node.exposed_inputs().iter().enumerate() {
        let Some(exposed) = exposed else { continue };
        let port = specs.len() + i;
        port.hash(&mut hasher);
        exposed.parameter.hash(&mut hasher);
        if let Some((up, up_port, up_image)) = env.control_edge(graph, id, port, image) {
            up_port.hash(&mut hasher);
            output_channel(graph.node(up).unwrap(), up_port).hash(&mut hasher);
            let mut other_memo = FingerprintMemo::new();
            node_fingerprint(graph, up, up_image, &env.control(true), if up_image == image { memo } else { &mut other_memo }, visiting)?.hash(&mut hasher);
        } else { u64::MAX.hash(&mut hasher); }
    }
    Ok(hasher.finish())
}


// ---- tail plan -------------------------------------------------------------------------

/// One pointwise operation on RGB, in scene-referred linear ACEScg. Same maths as the CPU nodes.
#[derive(Clone, Debug, PartialEq)]
pub enum TailOp {
    /// RGB * gain (`ops::apply_gain`).
    Gain(f32),
    /// RGB matrix (`ops::apply_matrix`).
    Matrix([[f32; 3]; 3]),
    /// `ops::saturation(amount)`.
    Saturation(f32),
    /// `ops::contrast`.
    Contrast { amount: f32, pivot: f32 },
    /// `ops::highlights_shadows`.
    HighlightsShadows { highlights: f32, shadows: f32 },
    /// `fx::color_wheels` (ASC CDL: slope, offset, power per channel).
    Cdl { slope: [f32; 3], offset: [f32; 3], power: [f32; 3] },
    /// `fx::tone_curve`.
    ToneCurve { shadows: f32, darks: f32, lights: f32, highlights: f32 },
    /// `fx::point_curve`; `channel` is the `fx::CurveChannel` index (0 luminance, 1 red, 2 green,
    /// 3 blue, 4 sat vs sat, 5 hue vs hue, 6 hue vs sat).
    PointCurve { points: Vec<[f32; 2]>, channel: u8 },
    /// `fx::whites_blacks`.
    WhitesBlacks { whites: f32, blacks: f32 },
    /// `fx::vibrance`.
    Vibrance(f32),
    /// `fx::hue_saturation`: hue, saturation, luminance per band (red, yellow, green, cyan, blue, magenta).
    HueSaturation { bands: [[f32; 3]; 6] },
    /// `fx::vignette`. Depends on the pixel position in the frame. That is valid in a tail
    /// because every tail operation keeps the image size, so the base has the same frame as
    /// the image the Vignette node would see.
    Vignette { amount: f32, midpoint: f32, feather: f32 },
}

/// Evaluation of one node split into a CPU part (the base) and a trailing run of
/// pointwise operations.
#[derive(Clone, Debug, PartialEq)]
pub struct TailPlan {
    /// Node whose output is evaluated on the CPU.
    pub base: NodeId,
    /// Its output port.
    pub base_port: usize,
    /// Applied to the base image in this order (upstream first); may be empty.
    pub ops: Vec<TailOp>,
    /// Identifies the base pixels for this image: equal fingerprints <=> identical base
    /// image. It is exactly the fingerprint the evaluator's cache stores for the base.
    pub base_fingerprint: u64,
    /// `base` is a Develop node and the base image is its output BEFORE the tone part
    /// (exposure, contrast, lights, shadows, white, black, dynamic, saturation), which
    /// `ops` then contains. The base is cached separately from the Develop node's result.
    pub base_before_tone: bool,
}

/// What a node contributes to a tail when the walk passes through it.
enum TailStep {
    /// The node ends the walk: it is the base.
    Base,
    /// Pass through, contributing these operations (upstream-first order, possibly none).
    Through(Vec<TailOp>),
    /// A Develop node with a tone part: the node ends the walk as the pre-tone base and
    /// contributes the tone operations.
    Tone(Vec<TailOp>),
}

fn gain_op(ev: f32) -> Vec<TailOp> {
    let g = 2f32.powf(ev);
    if g == 1.0 {
        Vec::new()
    } else {
        vec![TailOp::Gain(g)]
    }
}

/// The tone part of a Develop node as tail operations, in the order the evaluator applies
/// them (exposure gain, whites/blacks, highlights/shadows, contrast, vibrance, saturation).
/// Neutral steps are left out, so a neutral tone part gives no operations at all.
fn tone_ops(tone: &DevelopTone) -> Vec<TailOp> {
    let mut ops = gain_op(tone.exposure);
    if tone.whites != 0.0 || tone.blacks != 0.0 {
        ops.push(TailOp::WhitesBlacks { whites: tone.whites, blacks: tone.blacks });
    }
    if tone.highlights != 0.0 || tone.shadows != 0.0 {
        ops.push(TailOp::HighlightsShadows { highlights: tone.highlights, shadows: tone.shadows });
    }
    if tone.contrast != 0.0 {
        ops.push(TailOp::Contrast { amount: tone.contrast, pivot: DEVELOP_CONTRAST_PIVOT });
    }
    if tone.vibrance != 0.0 {
        ops.push(TailOp::Vibrance(tone.vibrance));
    }
    let saturation = 1.0 + tone.saturation;
    if saturation != 1.0 {
        ops.push(TailOp::Saturation(saturation));
    }
    ops
}

/// Pivot of the Develop contrast (scene-linear mid grey).
const DEVELOP_CONTRAST_PIVOT: f32 = 0.18;

/// One tail operation per non-neutral curve of a Curves node, in application order.
fn curve_ops(node: &rawgraph_core::Node, image: ImageId) -> Vec<TailOp> {
    curve_set(node, image)
        .into_iter()
        // Fewer than two points never change the picture (`fx::point_curve`).
        .filter(|e| e.points.len() >= 2)
        .map(|e| TailOp::PointCurve { points: e.points, channel: e.channel })
        .collect()
}

/// Develop base part (white balance matrix, baseline + brightness gain) applied to `src`;
/// `src` itself when that part is neutral.
fn develop_base(node: &rawgraph_core::Node, image: ImageId, src: Arc<ImageBuf>, provider: &dyn ImageProvider) -> Arc<ImageBuf> {
    let capture = provider.capture(image);
    let settings = develop_settings(node, image, capture.as_ref());
    if develop_base_is_neutral(&settings) {
        return src;
    }
    let g = develop_gain(&settings);
    modified_arc(&src, |img| {
        if let Some(m) = settings.matrix {
            ops::apply_matrix(img, m);
        }
        if g != 1.0 {
            ops::apply_gain(img, g);
        }
    })
}

fn develop_gain(settings: &DevelopSettings) -> f32 {
    if settings.gain_ev != 0.0 {
        2f32.powf(settings.gain_ev)
    } else {
        1.0
    }
}

fn develop_base_is_neutral(settings: &DevelopSettings) -> bool {
    settings.matrix.is_none() && develop_gain(settings) == 1.0
}

/// Classifies `node` for the tail walk. The neutral cases are exactly the evaluator's
/// identity short-circuits in `compute`.
fn tail_step(graph: &Graph, node: &rawgraph_core::Node, image: ImageId, provider: &dyn ImageProvider) -> TailStep {
    let id = node.id;
    let param = |name: &str| node.param(name, image);
    let images = provider.images();
    let env = Env { provider, images: &images, view_tone: ToneMap::AcesFit, control: false }; // membership only
    let masked = (0..node.kind.inputs().len() + node.exposed_inputs().len()).any(|port| node.is_control_input(port) && env.control_edge(graph, id, port, image).is_some());
    match node.kind {
        NodeKind::Output | NodeKind::Export | NodeKind::MetadataFilter | NodeKind::MetadataSplit | NodeKind::Merge => {
            TailStep::Through(Vec::new())
        }
        NodeKind::CaptureEv => TailStep::Through(gain_op(param("delta"))),
        NodeKind::Exposure if !masked => TailStep::Through(gain_op(param("ev"))),
        NodeKind::Develop => {
            if masked { return TailStep::Base; }
            if node.choice("tone_version", image) == 1 && !develop_tone(node, image).is_identity() {
                return TailStep::Base;
            }
            let tone = tone_ops(&develop_tone(node, image));
            if !tone.is_empty() {
                return TailStep::Tone(tone);
            }
            let capture = provider.capture(image);
            let settings = develop_settings(node, image, capture.as_ref());
            let g = develop_gain(&settings);
            let mut ops = Vec::new();
            if let Some(m) = settings.matrix {
                ops.push(TailOp::Matrix(m));
            }
            if g != 1.0 {
                ops.push(TailOp::Gain(g));
            }
            TailStep::Through(ops)
        }
        NodeKind::WhiteBalance => {
            let (temp, tint) = (param("temp"), param("tint"));
            if temp == 0.0 && tint == 0.0 {
                TailStep::Through(Vec::new())
            } else {
                TailStep::Through(vec![TailOp::Matrix(color::to_f32(&color::white_balance_matrix(temp as f64, tint as f64)))])
            }
        }
        NodeKind::Saturation if !masked => {
            let amount = param("amount");
            TailStep::Through(if amount == 1.0 { Vec::new() } else { vec![TailOp::Saturation(amount)] })
        }
        NodeKind::Contrast if !masked => {
            let amount = param("amount");
            TailStep::Through(if amount == 0.0 { Vec::new() } else { vec![TailOp::Contrast { amount, pivot: param("pivot") }] })
        }
        NodeKind::HighlightsShadows => {
            let (h, s) = (param("highlights"), param("shadows"));
            TailStep::Through(if h == 0.0 && s == 0.0 {
                Vec::new()
            } else {
                vec![TailOp::HighlightsShadows { highlights: h, shadows: s }]
            })
        }
        NodeKind::ColorWheels => {
            let cdl = cdl_params(node, image);
            TailStep::Through(if cdl.is_identity() {
                Vec::new()
            } else {
                vec![TailOp::Cdl { slope: cdl.slope, offset: cdl.offset, power: cdl.power }]
            })
        }
        NodeKind::ToneCurve => {
            let (s, d, l, h) = (param("shadows"), param("darks"), param("lights"), param("highlights"));
            TailStep::Through(if s == 0.0 && d == 0.0 && l == 0.0 && h == 0.0 {
                Vec::new()
            } else {
                vec![TailOp::ToneCurve { shadows: s, darks: d, lights: l, highlights: h }]
            })
        }
        NodeKind::PointCurve => TailStep::Through(curve_ops(node, image)),
        NodeKind::HueSaturation => {
            let bands = hue_bands(node, image);
            TailStep::Through(if bands.iter().flatten().all(|v| *v == 0.0) { Vec::new() } else { vec![TailOp::HueSaturation { bands }] })
        }
        NodeKind::Vignette => {
            let amount = param("amount");
            TailStep::Through(if amount == 0.0 {
                Vec::new()
            } else {
                vec![TailOp::Vignette { amount, midpoint: param("midpoint"), feather: param("feather") }]
            })
        }
        NodeKind::BlackWhite => {
            // `fx::black_white` computes `w0*r + w1*g + w2*b` (non-finite weights count as 0)
            // and writes it to all three channels; `ops::apply_matrix` with three identical
            // rows evaluates the very same expression per row, so the result is bit-identical.
            let w = ["red", "green", "blue"].map(|n| {
                let v = param(n);
                if v.is_finite() {
                    v
                } else {
                    0.0
                }
            });
            TailStep::Through(vec![TailOp::Matrix([w, w, w])])
        }
        _ => TailStep::Base,
    }
}

/// Hue / saturation / luminance of the six bands of a HueSaturation node.
fn hue_bands(node: &rawgraph_core::Node, image: ImageId) -> [[f32; 3]; 6] {
    let mut bands = [[0.0f32; 3]; 6];
    for (band, name) in bands.iter_mut().zip(["red", "yellow", "green", "cyan", "blue", "magenta"]) {
        *band = [
            node.param(&format!("{name}_h"), image),
            node.param(&format!("{name}_s"), image),
            node.param(&format!("{name}_l"), image),
        ];
    }
    bands
}

/// CPU reference for a tail: applies the operations in order with the core functions.
///
/// Bit-identical to what the evaluator produces for the same nodes: it clones the base
/// once and applies the same `rawgraph_core::ops` functions in the same order (the
/// evaluator clones per node, which does not change any value).
pub fn apply_tail_cpu(base: &ImageBuf, ops: &[TailOp]) -> ImageBuf {
    let mut img = base.clone();
    for op in ops {
        apply_op(&mut img, op);
    }
    img
}

/// Applies one tail operation in place. The evaluator uses it too, so both paths agree.
fn apply_op(img: &mut ImageBuf, op: &TailOp) {
    match *op {
        TailOp::Gain(g) => ops::apply_gain(img, g),
        TailOp::Matrix(m) => ops::apply_matrix(img, m),
        TailOp::Saturation(amount) => ops::saturation(img, amount),
        TailOp::Contrast { amount, pivot } => ops::contrast(img, amount, pivot),
        TailOp::HighlightsShadows { highlights, shadows } => ops::highlights_shadows(img, highlights, shadows),
        TailOp::Cdl { slope, offset, power } => fx::color_wheels(img, &CdlParams { slope, offset, power }),
        TailOp::ToneCurve { shadows, darks, lights, highlights } => fx::tone_curve(img, shadows, darks, lights, highlights),
        TailOp::PointCurve { ref points, channel } => {
            let channel = fx::CurveChannel::from_index(channel as usize).unwrap_or(fx::CurveChannel::Luminance);
            fx::point_curve(img, points, channel);
        }
        TailOp::HueSaturation { ref bands } => fx::hue_saturation(img, bands),
        TailOp::Vignette { amount, midpoint, feather } => fx::vignette(img, amount, midpoint, feather),
        TailOp::WhitesBlacks { whites, blacks } => fx::whites_blacks(img, whites, blacks),
        TailOp::Vibrance(amount) => fx::vibrance(img, amount),
    }
}

/// The error for a node the image does not leave through. A structurally broken graph
/// is reported as such, not as filtering.
fn not_a_member_error(graph: &Graph, node: NodeId) -> EvalError {
    match missing_input(graph, node, &mut Vec::new()) {
        Some((n, port)) => EvalError::MissingInput { node: n, port },
        None if graph.node(node).is_none() => EvalError::UnknownNode(node),
        None => EvalError::NotInCollection { node },
    }
}

/// First required input that is not connected anywhere upstream of `id`.
fn missing_input(graph: &Graph, id: NodeId, seen: &mut Vec<NodeId>) -> Option<(NodeId, usize)> {
    if seen.contains(&id) {
        return None;
    }
    seen.push(id);
    let node = graph.node(id)?;
    for (port, spec) in node.kind.inputs().iter().enumerate() {
        match graph.input_edge(id, port).map(|(up, _)| up) {
            None if spec.required => return Some((id, port)),
            None => {}
            Some(up) => {
                if let Some(m) = missing_input(graph, up, seen) {
                    return Some(m);
                }
            }
        }
    }
    None
}

// Reserved exposed-output labels extract a linear data channel into RGB; alpha is opaque.
fn output_channel(node: &rawgraph_core::Node, port: usize) -> Option<u8> {
    if node.output_type(port) != Some(PortType::Images) { return None; }
    let alias = node.exposed_outputs().get(port.checked_sub(node.kind.outputs().len())?)?.as_ref()?;
    match alias.label.as_str() { "Luma" => Some(0), "R" => Some(1), "G" => Some(2), "B" => Some(3), "A" => Some(4), _ => None }
}
fn output_value(graph: &Graph, node: NodeId, port: usize, value: Value) -> Value {
    let Some(channel) = graph.node(node).and_then(|n| output_channel(n, port)) else { return value };
    match value { Value::Images(img) => modified(&img, |out| { for p in out.data_mut().as_chunks_mut::<4>().0 {
        let v = match channel { 0 => 0.272_228_7*p[0] + 0.674_081_8*p[1] + 0.053_689_5*p[2], n => p[n as usize - 1] };
        p.copy_from_slice(&[v, v, v, 1.0]);
    }}) }
}

fn luma(p: &[f32; 4]) -> f32 {
    color::AP1_LUMA[0] * p[0] + color::AP1_LUMA[1] * p[1] + color::AP1_LUMA[2] * p[2]
}

fn control_image<'a>(node: &rawgraph_core::Node, inputs: &'a [Option<Value>], key: &str) -> Option<&'a ImageBuf> {
    let i = node.exposed_inputs().iter().position(|port| port.as_ref().is_some_and(|p| p.parameter == key))?;
    let Value::Images(img) = inputs.get(node.kind.inputs().len() + i)?.as_ref()?;
    Some(img)
}

fn control_plane(map: &ImageBuf, target: &ImageBuf) -> MaskBuf {
    let mut plane = MaskBuf { width: map.width(), height: map.height(), data: map.data().as_chunks::<4>().0.iter().map(luma).collect() };
    if (plane.width, plane.height) != (target.width(), target.height()) {
        plane = fx::resize_mask(&plane, target.width(), target.height());
    }
    for v in &mut plane.data { *v = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 }; }
    plane
}

/// Value of a parameter at which it has no effect (black on an intensity input).
fn neutral_value(kind: NodeKind, spec: &rawgraph_core::ParamSpec) -> f32 {
    match (kind, spec.name) {
        (NodeKind::Blur, "radius") => 0.0,
        _ => spec.default,
    }
}

/// Per-pixel values of an intensity input: the map scales the slider's effect.
/// Black (0) gives the neutral value, white (1) gives the slider value; in between
/// it interpolates linearly, so the slider still sets the strength.
fn parameter_values(node: &rawgraph_core::Node, image: ImageId, inputs: &[Option<Value>], key: &str, target: &ImageBuf) -> Option<Vec<f32>> {
    let spec = node.kind.param_spec(key)?;
    let plane = control_plane(control_image(node, inputs, key)?, target);
    let neutral = neutral_value(node.kind, spec);
    let slider = node.param(key, image);
    Some(plane.data.into_iter().map(|t| neutral + t * (slider - neutral)).collect())
}

fn grayscale_rgba(plane: MaskBuf) -> Value {
    let mut out = ImageBuf::new(plane.width, plane.height);
    for (px, v) in out.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(plane.data) {
        *px = [v, v, v, 1.0];
    }
    Value::Images(Arc::new(out))
}

fn input_image(inputs: &[Option<Value>], port: usize, node: NodeId) -> Result<Arc<ImageBuf>, EvalError> {
    match inputs.get(port) {
        Some(Some(Value::Images(img))) => Ok(img.clone()),
        Some(None) | None => Err(EvalError::MissingInput { node, port }),
    }
}

fn modified_arc(input: &Arc<ImageBuf>, f: impl FnOnce(&mut ImageBuf)) -> Arc<ImageBuf> {
    let mut copy = (**input).clone();
    f(&mut copy);
    Arc::new(copy)
}

fn modified(input: &Arc<ImageBuf>, f: impl FnOnce(&mut ImageBuf)) -> Value {
    Value::Images(modified_arc(input, f))
}

fn long_edge(img: &ImageBuf) -> f32 {
    img.width().max(img.height()) as f32
}


// One-shot, versioned binary protocol. No shell or network access is invoked by RawGraph.
fn depth_worker(image: &ImageBuf) -> Result<Value, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let worker = std::env::var_os("RAWGRAPH_DEPTH_WORKER").ok_or("Depth Map: set RAWGRAPH_DEPTH_WORKER to the configured executable; see docs/depth-estimate.md")?;
    let mut command = Command::new(worker);
    if let Some(script) = std::env::var_os("RAWGRAPH_DEPTH_SCRIPT") { command.arg(script); }
    let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| format!("Depth Map: cannot start worker: {e}"))?;
    let write_result = (|| -> std::io::Result<()> {
        let mut input = std::io::BufWriter::new(child.stdin.take().unwrap());
        input.write_all(b"RGD1")?;
        input.write_all(&image.width().to_le_bytes())?;
        input.write_all(&image.height().to_le_bytes())?;
        for v in image.data() { input.write_all(&v.to_le_bytes())?; }
        input.flush()?;
        Ok(())
    })();
    let out = child.wait_with_output().map_err(|e| format!("Depth Map worker: {e}"))?;
    if !out.status.success() { return Err(format!("Depth Map worker failed: {}", String::from_utf8_lossy(&out.stderr))); }
    write_result.map_err(|e| format!("Depth Map worker input: {e}"))?;
    decode_depth_reply(image.width(), image.height(), &out.stdout)
}

fn decode_depth_reply(width: u32, height: u32, bytes: &[u8]) -> Result<Value, String> {
    let expected = (width as usize).checked_mul(height as usize).and_then(|n| n.checked_mul(4)).and_then(|n| n.checked_add(12)).ok_or("Depth Map: image too large")?;
    if bytes.len() != expected || &bytes[..4] != b"RGD1" || bytes[4..8] != width.to_le_bytes() || bytes[8..12] != height.to_le_bytes() {
        return Err("Depth Map: invalid worker response".into());
    }
    let data: Vec<f32> = bytes[12..].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    if data.iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v)) { return Err("Depth Map: invalid depth values".into()); }
    Ok(grayscale_rgba(MaskBuf { width, height, data }))
}

/// Render a Generator independently of any source image or provider. Callers are
/// responsible for assigning the generated image an identity and choosing dimensions.
fn render_generator(node: &rawgraph_core::Node, image: ImageId, width: u32, height: u32) -> Result<Arc<ImageBuf>, EvalError> {
    use rawgraph_core::generator::{generate_with_ramp, Pattern};
    let param = |name: &str| node.param(name, image);
    let pattern = match node.choice("type", image) {
        1 => Pattern::Noise { seed: param("seed").round() as u32 },
        2 => Pattern::Checker { cell_size: (param("cell_size").round() as u32).max(1), invert: node.toggle("invert", image) },
        _ => Pattern::StyledGradient {
            ramp: rawgraph_core::generator::Ramp {
                color_middle: node.toggle("ramp_middle_color", image).then(|| (param("ramp_color_position") / 100.0, [param("ramp_color_r"), param("ramp_color_g"), param("ramp_color_b")])),
                color_midpoints: [param("ramp_color_midpoint_1") / 100.0, param("ramp_color_midpoint_2") / 100.0],
                alpha: [param("ramp_alpha_start") / 100.0, param("ramp_alpha_middle") / 100.0, param("ramp_alpha_end") / 100.0],
                alpha_middle: node.toggle("ramp_middle_alpha", image).then(|| param("ramp_alpha_position") / 100.0),
                alpha_midpoints: [param("ramp_alpha_midpoint_1") / 100.0, param("ramp_alpha_midpoint_2") / 100.0],
                smooth: if node.choice("ramp_interpolation", image) == 1 { param("ramp_smoothness") / 100.0 } else { 0.0 },
                dither: node.toggle("ramp_dither", image),
            },
            style: node.choice("gradient_style", image) as u32,
            angle: param("gradient_angle"),
            scale: param("gradient_scale") / 100.0,
            reverse: node.toggle("gradient_reverse", image),
            start: [param("start_r"), param("start_g"), param("start_b")],
            end: [param("end_r"), param("end_g"), param("end_b")],
            vertical: node.toggle("vertical", image),
        },
    };
    let ramp = match node.data(image) { Some(NodeData::GradientRamp(r)) => Some(r), _ => None };
    generate_with_ramp(width, height, pattern, ramp).map(Arc::new)
        .map_err(|e| EvalError::Provider(format!("Generator: {e}")))
}

fn compute(
    node: &rawgraph_core::Node,
    image: ImageId,
    inputs: &[Option<Value>],
    provider: &dyn ImageProvider,
    lens: Option<&LensProfile>,
    env: &Env,
) -> Result<Value, EvalError> {
    let id = node.id;
    let param = |name: &str| node.param(name, image);
    match node.kind {
        NodeKind::Generator | NodeKind::Draw => {
            let (width, height) = match inputs.first().and_then(|v| v.as_ref()) {
                Some(Value::Images(src)) => (src.width(), src.height()),
                _ => (param("width").round().clamp(1.0, 8192.0) as u32, param("height").round().clamp(1.0, 8192.0) as u32),
            };
            if node.kind == NodeKind::Draw {
                let strokes = match node.data(image) {
                    Some(NodeData::Strokes(s)) => s.as_slice(),
                    _ => &[],
                };
                return Ok(grayscale_rgba(fx::stroke_mask(width, height, strokes, false)));
            }
            let generated = render_generator(node, image, width, height)?;
            if node.choice("output", image) != 0 || env.control { return Ok(Value::Images(generated)); }
            let inverse = InverseDisplayTransform::new(env.view_tone);
            Ok(modified(&generated, |out| {
                for px in out.data_mut().as_chunks_mut::<4>().0.iter_mut() {
                    let rgb = inverse.apply([px[0], px[1], px[2]]);
                    px[..3].copy_from_slice(&rgb);
                }
            }))
        }
        NodeKind::NormalMap => {
            let src = input_image(inputs, 0, id)?;
            let values = src.data().as_chunks::<4>().0.iter().map(luma).collect();
            Ok(Value::Images(Arc::new(normals_map(src.width(), src.height(), values, param("strength"), param("smoothing"), node.toggle("flip_y", image)))))
        }
        NodeKind::Source => provider.image(image).map(Value::Images).map_err(EvalError::Provider),
        NodeKind::DepthMap => {
            let src = input_image(inputs, 0, id)?;
            depth_worker(&src).map_err(EvalError::Provider)
        }
        NodeKind::MetadataFilter | NodeKind::MetadataSplit | NodeKind::Output | NodeKind::Export => {
            Ok(Value::Images(input_image(inputs, 0, id)?))
        }
        NodeKind::CaptureEv | NodeKind::Exposure => {
            let src = input_image(inputs, 0, id)?;
            if let Some(values) = parameter_values(node, image, inputs, "ev", &src) {
                return Ok(modified(&src, |img| ops::exposure_mapped(img, &values)));
            }
            let ev = if node.kind == NodeKind::CaptureEv { param("delta") } else { param("ev") };
            Ok(if ev == 0.0 { Value::Images(src) } else { modified(&src, |img| ops::apply_gain(img, ev.exp2())) })
        }
        // Develop is computed by `Evaluator::compute_develop` (pre-tone cache), Layer by `Evaluator::eval_layer`.
        NodeKind::Develop | NodeKind::Layer => Err(EvalError::MissingInput { node: id, port: 0 }),
        NodeKind::WhiteBalance => {
            let src = input_image(inputs, 0, id)?;
            let (temp, tint) = (param("temp"), param("tint"));
            if temp == 0.0 && tint == 0.0 {
                return Ok(Value::Images(src));
            }
            let m = color::to_f32(&color::white_balance_matrix(temp as f64, tint as f64));
            Ok(modified(&src, |img| ops::apply_matrix(img, m)))
        }
        NodeKind::Saturation => {
            let src = input_image(inputs, 0, id)?;
            if let Some(values) = parameter_values(node, image, inputs, "amount", &src) {
                return Ok(modified(&src, |img| ops::saturation_mapped(img, &values)));
            }
            let amount = param("amount");
            Ok(if amount == 1.0 { Value::Images(src) } else { modified(&src, |img| ops::saturation(img, amount)) })
        }
        NodeKind::PixelSort => {
            let src = input_image(inputs, 0, id)?;
            let metric = match node.choice("metric", image) {
                1 => fx::SortMetric::Saturation,
                2 => fx::SortMetric::Hue,
                _ => fx::SortMetric::Brightness,
            };
            Ok(modified(&src, |img| fx::pixel_sort(img, metric, node.toggle("reverse", image))))
        }
        NodeKind::ColorWheels => {
            let src = input_image(inputs, 0, id)?;
            let cdl = cdl_params(node, image);
            if cdl.is_identity() {
                return Ok(Value::Images(src));
            }
            Ok(modified(&src, |img| fx::color_wheels(img, &cdl)))
        }
        NodeKind::Clarity => {
            let src = input_image(inputs, 0, id)?;
            let (clarity, texture) = (param("clarity"), param("texture"));
            if clarity == 0.0 && texture == 0.0 {
                return Ok(Value::Images(src));
            }
            Ok(Value::Images(Arc::new(fx::local_contrast(&src, clarity, texture))))
        }
        NodeKind::Merge => match inputs.iter().flatten().next() {
            Some(v) => Ok(v.clone()),
            None => Err(EvalError::NotInCollection { node: id }),
        },
        NodeKind::Contrast => {
            let src = input_image(inputs, 0, id)?;
            if let Some(values) = parameter_values(node, image, inputs, "amount", &src) {
                return Ok(modified(&src, |img| ops::contrast_mapped(img, &values, param("pivot"))));
            }
            let amount = param("amount");
            Ok(if amount == 0.0 { Value::Images(src) } else { modified(&src, |img| ops::contrast(img, amount, param("pivot"))) })
        }
        NodeKind::HighlightsShadows => {
            let src = input_image(inputs, 0, id)?;
            let (h, s) = (param("highlights"), param("shadows"));
            if h == 0.0 && s == 0.0 {
                return Ok(Value::Images(src));
            }
            Ok(modified(&src, |img| ops::highlights_shadows(img, h, s)))
        }
        NodeKind::NoiseReduction => {
            let src = input_image(inputs, 0, id)?;
            let (mut luma, mut chroma) = (param("luma"), param("chroma"));
            if node.toggle("iso_adaptive", image) {
                let scale = iso_noise_scale(provider.capture(image).and_then(|c| c.iso));
                luma = (luma * scale).clamp(0.0, 1.0);
                chroma = (chroma * scale).clamp(0.0, 1.0);
            }
            if luma == 0.0 && chroma == 0.0 {
                return Ok(Value::Images(src));
            }
            let radius_px = param("radius") / 1000.0 * long_edge(&src);
            Ok(Value::Images(Arc::new(ops::noise_reduction(&src, luma, chroma, radius_px))))
        }
        NodeKind::ToneCurve => {
            let src = input_image(inputs, 0, id)?;
            let (s, d, l, h) = (param("shadows"), param("darks"), param("lights"), param("highlights"));
            if s == 0.0 && d == 0.0 && l == 0.0 && h == 0.0 {
                return Ok(Value::Images(src));
            }
            Ok(modified(&src, |img| fx::tone_curve(img, s, d, l, h)))
        }
        NodeKind::HueSaturation => {
            let src = input_image(inputs, 0, id)?;
            let bands = hue_bands(node, image);
            if bands.iter().flatten().all(|v| *v == 0.0) {
                return Ok(Value::Images(src));
            }
            Ok(modified(&src, |img| fx::hue_saturation(img, &bands)))
        }
        NodeKind::Sharpen => {
            let src = input_image(inputs, 0, id)?;
            let map = control_image(node, inputs, "amount").map(|map| control_plane(map, &src));
            // Amount starts at zero, so the plane scales the slider amount (black = none, white = slider).
            let amount = param("amount");
            if amount == 0.0 { return Ok(Value::Images(src)); }
            let radius_px = param("radius") / 1000.0 * long_edge(&src);
            Ok(Value::Images(Arc::new(fx::sharpen(&src, amount, radius_px, param("threshold"), map.as_ref()))))
        }
        NodeKind::Vignette => {
            let src = input_image(inputs, 0, id)?;
            let amount = param("amount");
            if amount == 0.0 {
                return Ok(Value::Images(src));
            }
            let (mid, feather) = (param("midpoint"), param("feather"));
            Ok(modified(&src, |img| fx::vignette(img, amount, mid, feather)))
        }
        NodeKind::BlackWhite => {
            let src = input_image(inputs, 0, id)?;
            let w = [param("red"), param("green"), param("blue")];
            Ok(modified(&src, |img| fx::black_white(img, w)))
        }
        NodeKind::LensManual => {
            let src = input_image(inputs, 0, id)?;
            let (dist, vig, cr, cb) = (param("distortion"), param("vignetting"), param("ca_red"), param("ca_blue"));
            let manual = !(dist == 0.0 && vig == 0.0 && cr == 0.0 && cb == 0.0);
            let mut cur = src;
            // Auto: the profile of the file first (warp, then radial vignette), the manual values on top.
            if let Some(profile) = lens {
                if let Some(warp) = &profile.warp {
                    cur = Arc::new(fx::warp_rectilinear(&cur, warp));
                }
                if let Some(vignette) = &profile.vignette {
                    cur = modified_arc(&cur, |img| fx::vignette_radial(img, vignette));
                }
            }
            if manual {
                cur = Arc::new(fx::lens_manual(&cur, dist, vig, cr, cb));
            }
            Ok(Value::Images(cur))
        }
        NodeKind::Transform => {
            let src = input_image(inputs, 0, id)?;
            let params = transform_params(node, image);
            if params.is_identity() {
                return Ok(Value::Images(src));
            }
            Ok(Value::Images(Arc::new(fx::transform(&src, &params))))
        }
        NodeKind::PointCurve => {
            let src = input_image(inputs, 0, id)?;
            let curves = curve_ops(node, image);
            if curves.is_empty() {
                return Ok(Value::Images(src));
            }
            Ok(modified(&src, |img| {
                for op in &curves {
                    apply_op(img, op);
                }
            }))
        }
        NodeKind::Blur => {
            let src = input_image(inputs, 0, id)?;
            let radius_map = control_image(node, inputs, "radius").map(|map| control_plane(map, &src));
            // An intensity map scales the radius set on the slider (black = no blur, white = slider radius).
            let sigma = param("radius") / 1000.0 * long_edge(&src);
            let high = node.choice("pass", image) == 1;
            let kernel = node.choice("kernel", image);
            if sigma.round() <= 0.0 && !high {
                return Ok(Value::Images(src));
            }
            let low = match radius_map {
                Some(mask) if kernel < 2 => ops::filter_low_modulated(&src, sigma, &mask, kernel == 1),
                None if kernel < 2 => ops::filter_low(&src, sigma, kernel == 1),
                mask => {
                    let direction = control_image(node, inputs, "direction");
                    ops::filter_sampled_direction(&src, sigma, kernel, param("angle"), mask.as_ref(), direction)
                },
            };
            if !high {
                return Ok(Value::Images(Arc::new(low)));
            }
            // Center the signed RGB residual at 0.5 without clipping HDR values;
            // alpha is coverage, not a frequency component.
            let mut result = low;
            for (out, original) in result.data_mut().as_chunks_mut::<4>().0.iter_mut().zip(src.data().as_chunks::<4>().0) {
                for c in 0..3 {
                    out[c] = 0.5 + original[c] - out[c];
                }
                out[3] = original[3];
            }
            Ok(Value::Images(Arc::new(result)))
        }
    }
}

/// The parameters of a Transform node for one image.
fn transform_params(node: &rawgraph_core::Node, image: ImageId) -> fx::TransformParams {
    fx::TransformParams {
        left: node.param("left", image),
        top: node.param("top", image),
        right: node.param("right", image),
        bottom: node.param("bottom", image),
        angle_deg: node.param("angle", image),
        quarter_turns: (node.choice("rotate", image) % 4) as u8,
        flip_h: node.toggle("flip_h", image),
        flip_v: node.toggle("flip_v", image),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Mutex;

    const A: ImageId = ImageId(1);
    const B: ImageId = ImageId(2);
    const C: ImageId = ImageId(3);

    #[derive(Default)]
    struct TestProvider {
        calls: AtomicU64,
        revision: AtomicU64,
        failing: Mutex<HashSet<u64>>,
        iso: Mutex<std::collections::HashMap<u64, f32>>,
        loaded: Mutex<Vec<u64>>,
        caps: Mutex<std::collections::HashMap<u64, CaptureInfo>>,
        dims: Mutex<Option<(u32, u32)>>,
        texture: AtomicBool,
        catalog: Mutex<Vec<ImageId>>,
        paths: Mutex<std::collections::HashMap<u64, String>>,
        flat: Mutex<std::collections::HashMap<u64, [f32; 3]>>,
        sizes: Mutex<std::collections::HashMap<u64, (u32, u32)>>,
        full: Mutex<std::collections::HashMap<u64, (u32, u32)>>,
        lens: Mutex<std::collections::HashMap<u64, LensProfile>>,
    }

    impl ImageProvider for TestProvider {
        fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.loaded.lock().unwrap().push(id.0);
            if self.failing.lock().unwrap().contains(&id.0) {
                return Err("boom".into());
            }
            let (w, h) = self
                .sizes
                .lock()
                .unwrap()
                .get(&id.0)
                .copied()
                .or(*self.dims.lock().unwrap())
                .unwrap_or((64, 64));
            let mut img = ImageBuf::new(w, h);
            if let Some(c) = self.flat.lock().unwrap().get(&id.0) {
                for px in img.data_mut().as_chunks_mut::<4>().0.iter_mut() {
                    px.copy_from_slice(&[c[0], c[1], c[2], 1.0]);
                }
                return Ok(Arc::new(img));
            }
            for y in 0..h {
                for x in 0..w {
                    let mut v = (x + y) as f32 / 8.0 + id.0 as f32 * 0.1;
                    if self.texture.load(Ordering::Relaxed) {
                        v *= if (x / 3 + y / 3) % 2 == 0 { 0.6 } else { 1.0 };
                    }
                    img.set_pixel(x, y, [v, v * 0.5 + 0.1, 0.2 + (x as f32) * 0.05, 1.0]);
                }
            }
            Ok(Arc::new(img))
        }
        fn revision(&self, _id: ImageId) -> u64 {
            self.revision.load(Ordering::Relaxed)
        }
        fn capture(&self, id: ImageId) -> Option<CaptureInfo> {
            let mut info = match self.caps.lock().unwrap().get(&id.0) {
                Some(c) => Some(c.clone()),
                None => self.iso.lock().unwrap().get(&id.0).map(|&iso| CaptureInfo { iso: Some(iso), ..Default::default() }),
            };
            if let Some(path) = self.paths.lock().unwrap().get(&id.0) {
                info.get_or_insert_with(CaptureInfo::default).path = Some(path.clone());
            }
            info
        }
        fn images(&self) -> Vec<ImageId> {
            self.catalog.lock().unwrap().clone()
        }
        fn full_size(&self, id: ImageId) -> Option<(u32, u32)> {
            self.full.lock().unwrap().get(&id.0).copied()
        }
        fn lens_profile(&self, id: ImageId) -> Option<LensProfile> {
            self.lens.lock().unwrap().get(&id.0).cloned()
        }
    }

    impl TestProvider {
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn standalone_generator_graph_members_and_pixels() {
        let mut graph = Graph::default();
        let id = graph.add_node(NodeKind::Generator, [0.0; 2]);
        let provider = TestProvider::default();
        let mut eval = Evaluator::new();
        let members = eval.members(&graph, id, 0, &provider);
        assert_eq!(members, vec![ImageId(u64::MAX - id.0)]);
        let result = run(&graph, id, members[0], &provider, &mut eval);
        assert_eq!((result.width(), result.height()), (1024, 1024));
    }

    #[test]
    fn normal_map_from_generator_gradient_has_expected_slope() {
        let mut graph = Graph::new();
        let generator = graph.add_node(NodeKind::Generator, [0.0; 2]);
        let normals = graph.add_node(NodeKind::NormalMap, [0.0; 2]);
        graph.connect(generator, normals, 0).unwrap();
        let image = ImageId(u64::MAX - generator.0);
        let provider = TestProvider::default();
        let mut eval = Evaluator::new();
        set(&mut graph, generator, "output", image, 1.0);
        set(&mut graph, generator, "width", image, 5.0);
        set(&mut graph, generator, "height", image, 3.0);
        let graph = Graph::from_json(&graph.to_json()).unwrap();
        assert_eq!(eval.members(&graph, normals, 0, &provider), vec![image]);
        let normals = run(&graph, normals, image, &provider, &mut eval);
        let z = 1.0 / 1.0625_f32.sqrt();
        let expected = [0.5 - 0.125 * z, 0.5, 0.5 + 0.5 * z, 1.0];
        for (actual, expected) in normals.pixel(2, 1).into_iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn generator_render_needs_no_provider_or_photo() {
        let mut graph = Graph::default();
        let id = graph.add_node(NodeKind::Generator, [0.0; 2]);
        let node = graph.node(id).unwrap();
        let generated = render_generator(node, ImageId(42), 5, 3).unwrap();
        assert_eq!((generated.width(), generated.height()), (5, 3));
        assert_eq!(generated.pixel(0, 0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(generated.pixel(4, 0), [1.0, 1.0, 1.0, 1.0]);
        assert!(render_generator(node, ImageId(42), 0, 3).is_err());
        assert!(render_generator(node, ImageId(42), u32::MAX, u32::MAX).is_err());
    }

    #[test]
    fn exposed_intensity_and_channels_affect_cpu_cache_and_tail() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, out, 0).unwrap();
        set(&mut g, exp, "ev", A, 4.0);
        let input = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        let red = g.expose_output(src, 0, "R").unwrap();
        let green = g.expose_output(src, 0, "G").unwrap();
        let blue = g.expose_output(src, 0, "B").unwrap();
        let alpha = g.expose_output(src, 0, "A").unwrap();
        let luma = g.expose_output(src, 0, "Luma").unwrap();
        g.connect_ports(src, red, exp, input).unwrap();
        let p = TestProvider::default();
        *p.catalog.lock().unwrap() = vec![A];
        *p.dims.lock().unwrap() = Some((2, 1));
        p.flat.lock().unwrap().insert(A.0, [0.25, 0.5, 0.75]);
        let mut ev = Evaluator::new();
        let cancel = CancelToken::new();
        for (port, value) in [(red, 0.25), (green, 0.5), (blue, 0.75), (alpha, 1.0), (luma, 0.272_228_7*0.25 + 0.674_081_8*0.5 + 0.053_689_5*0.75)] {
            let Value::Images(img) = ev.eval_output(&g, src, port, A, &p, &cancel).unwrap();
            assert!((img.pixel(0, 0)[0] - value).abs() < 1e-6);
            assert_eq!(img.pixel(0, 0)[3], 1.0);
        }
        let first = run(&g, out, A, &p, &mut ev);
        // Red 0.25 scales the +4 EV slider to +1 EV.
        assert!((first.pixel(0,0)[0] - 0.25 * 2f32.powf(1.0)).abs() < 1e-6);
        let fp = fingerprint_of(&g, out, A, &p);
        let plan = ev.plan_tail(&g, out, A, &p).unwrap();
        assert_eq!(plan.base, exp);
        let base = ev.eval_base(&g, &plan, A, &p, &cancel).unwrap();
        assert_eq!(apply_tail_cpu(&base, &plan.ops), *first);
        g.connect_ports(src, green, exp, input).unwrap();
        assert_ne!(fp, fingerprint_of(&g, out, A, &p));
        let second = run(&g, out, A, &p, &mut ev);
        assert!((second.pixel(0,0)[0] - 0.25 * 2f32.powf(2.0)).abs() < 1e-6);
        assert_ne!(*first, *second);
        let restored = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(*second, *run(&restored, out, A, &p, &mut ev));
    }

    #[test]
    fn develop_mapped_controls_match_scalar_and_invalidate_tail() {
        for local in [0.0, 1.0] {
            for name in ["exposure", "contrast", "highlights", "shadows", "whites", "blacks"] {
                let mut g = Graph::new();
                let src = g.add_node(NodeKind::Source, [0.0; 2]);
                let dev = g.add_node(NodeKind::Develop, [0.0; 2]);
                let out = g.add_node(NodeKind::Output, [0.0; 2]);
                g.connect(src, dev, 0).unwrap(); g.connect(dev, out, 0).unwrap();
                set(&mut g, dev, "tone_version", A, local);
                let spec = NodeKind::Develop.param_spec(name).unwrap();
                // The map scales the slider's effect: green 0.75 means 75 % of the slider value.
                let slider = spec.default + 0.8 * (spec.max - spec.default);
                set(&mut g, dev, name, A, slider);
                let port = g.expose_parameter(dev, name, PortType::Images).unwrap();
                let red = g.expose_output(src, 0, "R").unwrap();
                let green = g.expose_output(src, 0, "G").unwrap();
                g.connect_ports(src, red, dev, port).unwrap();
                let p = TestProvider::default(); *p.catalog.lock().unwrap() = vec![A]; *p.dims.lock().unwrap() = Some((2, 1));
                p.flat.lock().unwrap().insert(A.0, [0.25, 0.75, 0.5]);
                let mut ev = Evaluator::new();
                let mapped = run(&g, out, A, &p, &mut ev);
                let plan = ev.plan_tail(&g, out, A, &p).unwrap(); assert_eq!(plan.base, dev);
                let base = ev.eval_base(&g, &plan, A, &p, &CancelToken::new()).unwrap(); assert_eq!(apply_tail_cpu(&base, &plan.ops), *mapped);
                let restored = Graph::from_json(&g.to_json()).unwrap(); assert_eq!(*mapped, *run(&restored, out, A, &p, &mut ev));
                g.connect_ports(src, green, dev, port).unwrap(); let changed = run(&g, out, A, &p, &mut ev);
                if name == "exposure" || name == "contrast" { assert_ne!(*mapped, *changed, "{name} local={local}"); }
                g.remove_exposed_input(dev, port);
                set(&mut g, dev, name, A, spec.default + 0.75 * (slider - spec.default));
                let scalar = run(&g, out, A, &p, &mut ev);
                for (a,b) in changed.data().iter().zip(scalar.data()) { assert!((a-b).abs() < 1e-5, "{name} local={local}: {a} {b}"); }
            }
        }
    }

    #[test]
    fn standalone_generator_controls_develop_exposure() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
        let dev = g.add_node(NodeKind::Develop, [0.0; 2]);
        g.connect(src, dev, 0).unwrap();
        let p = TestProvider::default();
        *p.catalog.lock().unwrap() = vec![A];
        *p.dims.lock().unwrap() = Some((5, 1));
        let mut ev = Evaluator::new();
        set(&mut g, dev, "exposure", A, 2.0);
        let plain = run(&g, dev, A, &p, &mut ev);
        let port = g.expose_parameter(dev, "exposure", PortType::Images).unwrap();
        g.connect(generator, dev, port).unwrap();
        let mapped = run(&g, dev, A, &p, &mut ev);
        assert_ne!(*plain, *mapped);
        assert_ne!(mapped.pixel(0, 0), mapped.pixel(4, 0));
        assert!(matches!(tail_step(&g, g.node(dev).unwrap(), A, &p), TailStep::Base));
        let misses = ev.misses();
        set(&mut g, generator, "gradient_reverse", A, 1.0);
        let changed = run(&g, dev, A, &p, &mut ev);
        assert_ne!(*mapped, *changed);
        assert!(ev.misses() > misses);
    }

    #[test]
    fn standalone_generator_radius_control_accepts_luma_alias() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
        let blur = g.add_node(NodeKind::Blur, [0.0; 2]);
        g.connect(src, blur, 0).unwrap();
        set(&mut g, blur, "radius", A, 30.0);
        let p = TestProvider::default();
        *p.catalog.lock().unwrap() = vec![A];
        let mut ev = Evaluator::new();
        let plain = run(&g, blur, A, &p, &mut ev);
        let radius = g.expose_parameter(blur, "radius", PortType::Images).unwrap();
        let luma = g.expose_output(generator, 0, "Luma").unwrap();
        for port in [0, luma] {
            g.connect_ports(generator, port, blur, radius).unwrap();
            let mapped = run(&g, blur, A, &p, &mut ev);
            assert_ne!(*plain, *mapped);
            assert!(Graph::from_json(&g.to_json()).is_ok());
        }
    }

    #[test]
    fn disjoint_multi_image_control_is_unconnected() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let maps = g.add_node(NodeKind::Source, [0.0; 2]);
        let dev = g.add_node(NodeKind::Develop, [0.0; 2]);
        set_data(&mut g, maps, A, Some(NodeData::Folder("/maps".into())));
        g.connect(src, dev, 0).unwrap();
        let p = TestProvider::default();
        let c = ImageId(3);
        *p.catalog.lock().unwrap() = vec![A, B, c];
        p.paths.lock().unwrap().insert(A.0, "/photos/a.jpg".into());
        p.paths.lock().unwrap().insert(B.0, "/maps/b.jpg".into());
        p.paths.lock().unwrap().insert(c.0, "/maps/c.jpg".into());
        let mut ev = Evaluator::new();
        let plain = run(&g, dev, A, &p, &mut ev);
        let port = g.expose_parameter(dev, "exposure", PortType::Images).unwrap();
        g.connect(maps, dev, port).unwrap();
        assert_eq!(*plain, *run(&g, dev, A, &p, &mut ev));
        let env = Env { provider: &p, images: &[A, B, c], view_tone: ToneMap::AcesFit, control: false };
        assert!(env.control_edge(&g, dev, port, A).is_none());
        assert_eq!(env.cat(|catalog| g.members(dev, 0, catalog)), vec![A, B, c]);
    }

    #[test]
    fn develop_spatial_intensity_matches_full_frame_scalar_reference() {
        for local in [0.0, 1.0] {
            for name in ["exposure", "contrast", "highlights", "shadows", "whites", "blacks"] {
                let mut g = Graph::new();
                let src = g.add_node(NodeKind::Source, [0.0; 2]);
                let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
                let dev = g.add_node(NodeKind::Develop, [0.0; 2]);
                let out = g.add_node(NodeKind::Output, [0.0; 2]);
                g.connect(src, generator, 0).unwrap();
                g.connect(src, dev, 0).unwrap();
                g.connect(dev, out, 0).unwrap();
                set(&mut g, dev, "tone_version", A, local);
                let spec = NodeKind::Develop.param_spec(name).unwrap();
                let slider = spec.default + 0.8 * (spec.max - spec.default);
                set(&mut g, dev, name, A, slider);
                let port = g.expose_parameter(dev, name, PortType::Images).unwrap();
                g.connect(generator, dev, port).unwrap();
                let p = TestProvider::default();
                *p.catalog.lock().unwrap() = vec![A];
                *p.dims.lock().unwrap() = Some((5, 1));
                let mut ev = Evaluator::new();
                let mapped = run(&g, out, A, &p, &mut ev);
                let restored = Graph::from_json(&g.to_json()).unwrap();
                assert_eq!(*mapped, *run(&restored, out, A, &p, &mut ev));
                let plan = ev.plan_tail(&g, out, A, &p).unwrap();
                let base = ev.eval_base(&g, &plan, A, &p, &CancelToken::new()).unwrap();
                assert_eq!(apply_tail_cpu(&base, &plan.ops), *mapped);
                g.remove_exposed_input(dev, port);
                for x in 0..5 {
                    // Black = no effect (default), white = the slider value.
                    let value = spec.default + (x as f32 / 4.0) * (slider - spec.default);
                    set(&mut g, dev, name, A, value);
                    let scalar = run(&g, out, A, &p, &mut ev);
                    for (a, b) in mapped.pixel(x, 0).iter().zip(scalar.pixel(x, 0)) {
                        assert!((a-b).abs() < 1e-5, "{name} local={local} x={x}: {a} {b}");
                    }
                }
            }
        }
    }

    #[test]
    fn display_generator_waveform_cache_and_control_context() {
        use rawgraph_scopes::DisplayTransform;
        let mut g = Graph::new();
        let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
        let id = ImageId(u64::MAX - generator.0);
        set(&mut g, generator, "width", id, 1024.0);
        set(&mut g, generator, "height", id, 1.0);
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let cancel = CancelToken::new();
        let mut previous = None;
        for tone in [ToneMap::Clip, ToneMap::SoftHighlights, ToneMap::AcesFit] {
            ev.set_view_tone(tone);
            let plan = ev.plan_tail(&g, generator, id, &p).unwrap();
            assert_eq!(plan.base, generator);
            if let Some(fp) = previous { assert_ne!(fp, plan.base_fingerprint); }
            previous = Some(plan.base_fingerprint);
            let frame = ev.eval_image(&g, generator, id, &p, &cancel).unwrap();
            let display = DisplayTransform { exposure_ev: 0.0, tone_map: tone }.apply(&frame);
            for (x, rgb) in display.rgb.as_chunks::<3>().0.iter().enumerate() {
                for value in rgb { assert!((value * 1023.0 - x as f32).abs() <= 1.0, "{tone:?} x={x} value={value}"); }
            }
            assert_eq!((display.rgb[0]*1023.0).round() as u32, 0);
            assert_eq!((display.rgb[3069]*1023.0).round() as u32, 1023);
            let env = Env { provider: &p, images: &[], view_tone: tone, control: true };
            let (_, value) = ev.eval_node(&g, generator, id, &env, &cancel, &mut vec![], &mut FingerprintMemo::new()).unwrap();
            let Value::Images(control) = value;
            for x in 0..1024 { assert!((control.pixel(x, 0)[0] - x as f32/1023.0).abs() < 1e-6); }
        }
        set(&mut g, generator, "output", id, 1.0);
        let raw = render_generator(g.node(generator).unwrap(), id, 1024, 1).unwrap();
        let first = ev.eval_image(&g, generator, id, &p, &cancel).unwrap();
        assert_eq!(*raw, *first);
        let fp = ev.plan_tail(&g, generator, id, &p).unwrap().base_fingerprint;
        ev.set_view_tone(ToneMap::Clip);
        assert_eq!(fp, ev.plan_tail(&g, generator, id, &p).unwrap().base_fingerprint);
        assert!(Arc::ptr_eq(&first, &ev.eval_image(&g, generator, id, &p, &cancel).unwrap()));
    }

    #[test]
    fn generator_graph_pixels_serialization_and_cache() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let gen = g.add_node(NodeKind::Generator, [0.0; 2]);
        set(&mut g, gen, "output", A, 1.0);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, gen, 0).unwrap();
        g.connect(gen, out, 0).unwrap();
        let p = TestProvider::default();
        *p.dims.lock().unwrap() = Some((3, 2));
        let mut ev = Evaluator::new();
        let gradient = run(&g, out, A, &p, &mut ev);
        assert_eq!(gradient.pixel(0, 0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(gradient.pixel(1, 0), [0.5, 0.5, 0.5, 1.0]);
        assert_eq!(gradient.pixel(2, 1), [1.0; 4]);
        set(&mut g, gen, "type", A, 2.0);
        set(&mut g, gen, "cell_size", A, 1.0);
        let g2 = Graph::from_json(&g.to_json()).unwrap();
        let checker = run(&g2, out, A, &p, &mut ev);
        assert_eq!(checker.pixel(0, 0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(checker.pixel(1, 0), [1.0; 4]);
        assert_eq!(checker.pixel(0, 1), [1.0; 4]);
        set(&mut g, gen, "type", A, 1.0);
        set(&mut g, gen, "seed", A, 42.0);
        let noise = run(&g, out, A, &p, &mut ev);
        let restored = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(noise, run(&restored, out, A, &p, &mut ev));
        set(&mut g, gen, "seed", A, 43.0);
        assert_ne!(noise, run(&g, out, A, &p, &mut ev));
    }

    #[test]
    fn gradient_ramp_edits_render_roundtrip_and_invalidate_cache() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
        set(&mut g, generator, "output", A, 1.0);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, generator, 0).unwrap(); g.connect(generator, out, 0).unwrap();
        let p = TestProvider::default(); *p.dims.lock().unwrap() = Some((9, 1));
        let mut ev = Evaluator::new();
        let baseline = run(&g, out, A, &p, &mut ev);
        set(&mut g, generator, "ramp_middle_color", A, 1.0);
        set(&mut g, generator, "ramp_color_r", A, 1.0);
        set(&mut g, generator, "ramp_color_g", A, 0.0);
        set(&mut g, generator, "ramp_color_b", A, 0.0);
        set(&mut g, generator, "ramp_middle_alpha", A, 1.0);
        set(&mut g, generator, "ramp_alpha_middle", A, 25.0);
        let edited = run(&g, out, A, &p, &mut ev);
        assert_ne!(baseline, edited);
        assert_eq!(edited.pixel(4, 0), [1.0, 0.0, 0.0, 0.25]);
        set(&mut g, generator, "ramp_color_midpoint_1", A, 25.0);
        set(&mut g, generator, "ramp_alpha_midpoint_1", A, 25.0);
        let shifted = run(&g, out, A, &p, &mut ev);
        assert_ne!(edited.pixel(1, 0), shifted.pixel(1, 0));
        set(&mut g, generator, "ramp_interpolation", A, 1.0);
        let smooth = run(&g, out, A, &p, &mut ev);
        assert_ne!(shifted.pixel(2, 0), smooth.pixel(2, 0));
        set(&mut g, generator, "ramp_dither", A, 1.0);
        let dither = run(&g, out, A, &p, &mut ev);
        assert_ne!(smooth, dither);
        let restored = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(dither, run(&restored, out, A, &p, &mut Evaluator::new()));
        set(&mut g, generator, "ramp_dither", A, 0.0);
        set(&mut g, generator, "gradient_reverse", A, 1.0);
        let reverse = run(&g, out, A, &p, &mut ev);
        for x in 0..9 { assert_eq!(reverse.pixel(x, 0), smooth.pixel(8-x, 0)); }
    }

    #[test]
    fn structured_ramp_cache_inheritance_and_graph_roundtrip() {
        use rawgraph_core::{GradientRamp, ColorStop, OpacityStop, Mode};
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
        set(&mut g, generator, "output", A, 1.0);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, generator, 0).unwrap(); g.connect(generator, out, 0).unwrap();
        let p = TestProvider::default(); *p.dims.lock().unwrap() = Some((9, 1));
        let mut ramp = GradientRamp::default();
        ramp.colors.insert(1, ColorStop { position: 0.5, color: [1.0, 0.0, 0.0], midpoint: 0.5 });
        ramp.opacities.insert(1, OpacityStop { position: 0.5, opacity: 0.25, midpoint: 0.5 });
        assert!(g.node_mut(generator).unwrap().set_data(A, Some(NodeData::GradientRamp(ramp.clone()))));
        let mut ev = Evaluator::new();
        let original = run(&g, out, A, &p, &mut ev);
        assert_eq!(original.pixel(4, 0), [1.0, 0.0, 0.0, 0.25]);
        ramp.colors[0].midpoint = 0.25;
        assert!(g.node_mut(generator).unwrap().set_data(A, Some(NodeData::GradientRamp(ramp.clone()))));
        let edited = run(&g, out, A, &p, &mut ev);
        assert_ne!(original.pixel(1, 0), edited.pixel(1, 0));
        let n = g.node_mut(generator).unwrap(); n.mode = Mode::Individual;
        assert_eq!(n.data(A), Some(&NodeData::GradientRamp(ramp.clone())));
        ramp.colors[1].color = [0.0, 1.0, 0.0];
        assert!(n.set_data(A, Some(NodeData::GradientRamp(ramp))));
        let individual = run(&g, out, A, &p, &mut ev);
        assert_eq!(individual.pixel(4, 0), [0.0, 1.0, 0.0, 0.25]);
        let restored = Graph::from_json(&g.to_json()).unwrap();
        assert_eq!(individual, run(&restored, out, A, &p, &mut Evaluator::new()));
        g.node_mut(generator).unwrap().mode = Mode::All;
        assert_eq!(edited, run(&g, out, A, &p, &mut ev));
    }

    #[test]
    fn generator_styles_angle_scale_reverse_roundtrip_and_cache() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let generator = g.add_node(NodeKind::Generator, [0.0; 2]);
        set(&mut g, generator, "output", A, 1.0);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, generator, 0).unwrap();
        g.connect(generator, out, 0).unwrap();
        let p = TestProvider::default();
        *p.dims.lock().unwrap() = Some((5, 5));
        let mut ev = Evaluator::new();
        let base = run(&g, out, A, &p, &mut ev);
        assert_eq!(base.pixel(0, 0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(base.pixel(4, 0), [1.0; 4]);
        set(&mut g, generator, "gradient_reverse", A, 1.0);
        let rev = run(&g, out, A, &p, &mut ev);
        assert_eq!(rev.pixel(0, 0), [1.0; 4]);
        assert_eq!(rev.pixel(4, 0), [0.0, 0.0, 0.0, 1.0]);
        set(&mut g, generator, "gradient_reverse", A, 0.0);
        set(&mut g, generator, "gradient_angle", A, 90.0);
        let rotated = run(&g, out, A, &p, &mut ev);
        assert!((rotated.pixel(2, 0)[0] - 0.0).abs() < 1e-6);
        assert!((rotated.pixel(2, 4)[0] - 1.0).abs() < 1e-6);
        set(&mut g, generator, "gradient_scale", A, 200.0);
        let scaled = run(&g, out, A, &p, &mut ev);
        assert!((scaled.pixel(2, 0)[0] - 0.25).abs() < 1e-6);
        for style in 1..=4 {
            set(&mut g, generator, "gradient_style", A, style as f32);
            let frame = run(&g, out, A, &p, &mut ev);
            assert_ne!(frame, scaled);
            let restored = Graph::from_json(&g.to_json()).unwrap();
            assert_eq!(frame, run(&restored, out, A, &p, &mut ev));
            for y in 0..5 { for x in 0..5 { assert_eq!(frame.pixel(x, y)[3], 1.0); } }
        }
    }

    #[test]
    fn depth_reply_becomes_opaque_grayscale_image() {
        let reply = [*b"RGD1", 2_u32.to_le_bytes(), 1_u32.to_le_bytes(), 0.25_f32.to_le_bytes(), 0.75_f32.to_le_bytes()].concat();
        let Value::Images(image) = decode_depth_reply(2, 1, &reply).unwrap();
        assert_eq!(image.pixel(0, 0), [0.25, 0.25, 0.25, 1.0]);
        assert_eq!(image.pixel(1, 0), [0.75, 0.75, 0.75, 1.0]);
        assert!(decode_depth_reply(1, 2, &reply).is_err());
        assert!(decode_depth_reply(2, 1, &reply[..reply.len() - 1]).is_err());
    }

    #[test]
    fn depth_missing_worker_is_actionable() {
        if std::env::var_os("RAWGRAPH_DEPTH_WORKER").is_some() { return; }
        let err = depth_worker(&ImageBuf::new(1, 1)).unwrap_err();
        assert!(err.contains("RAWGRAPH_DEPTH_WORKER"), "{err}");
    }

    #[test]
    fn depth_schema_and_membership_follow_input() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let depth = g.add_node(NodeKind::DepthMap, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        set_data(&mut g, src, A, Some(NodeData::Folder("/a".into())));
        g.connect(src, depth, 0).unwrap();
        g.connect(src, exp, 0).unwrap();
        let ev = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(depth, exp, ev).unwrap();
        let g = Graph::from_json(&g.to_json()).unwrap();
        let p = TestProvider::default();
        *p.catalog.lock().unwrap() = vec![A, B];
        p.paths.lock().unwrap().insert(A.0, "/a/yes.jpg".into());
        p.paths.lock().unwrap().insert(B.0, "/b/no.jpg".into());
        let env = Env { provider: &p, images: &[A, B], view_tone: ToneMap::AcesFit, control: false };
        assert_eq!(g.node(depth).unwrap().kind.output(), Some(PortType::Images));
        assert_eq!(env.cat(|c| g.members(depth, 0, c)), vec![A]);
        assert!(env.contains_output(&g, depth, 0, A));
        assert!(!env.contains_output(&g, depth, 0, B));
    }

    #[test]
    #[ignore = "requires configured ONNX worker and RAWGRAPH_DEPTH_FIXTURE RGD1 float fixture"]
    fn depth_real_fixture_graph() {
        let bytes = std::fs::read(std::env::var("RAWGRAPH_DEPTH_FIXTURE").unwrap()).unwrap();
        assert_eq!(&bytes[..4], b"RGD1");
        let w = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        let h = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let pixels = bytes[12..].as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        struct Fixture(Arc<ImageBuf>);
        impl ImageProvider for Fixture {
            fn image(&self, _: ImageId) -> Result<Arc<ImageBuf>, String> { Ok(self.0.clone()) }
        }
        let p = Fixture(Arc::new(ImageBuf::from_rgba(w, h, pixels).unwrap()));
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let depth = g.add_node(NodeKind::DepthMap, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        g.connect(src, depth, 0).unwrap();
        g.connect(src, exp, 0).unwrap();
        let ev = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(depth, exp, ev).unwrap();
        set(&mut g, exp, "ev", A, 1.0);
        let g = Graph::from_json(&g.to_json()).unwrap();
        let mut ev = Evaluator::new();
        let depth = ev.eval_image(&g, depth, A, &p, &CancelToken::new()).unwrap();
        assert_eq!((depth.width(), depth.height()), (w, h));
        let mut lo = f32::INFINITY;
        let mut hi = f32::NEG_INFINITY;
        for &[r, g, b, alpha] in depth.data().as_chunks::<4>().0 {
            assert_eq!([g, b, alpha], [r, r, 1.0]);
            lo = lo.min(r);
            hi = hi.max(r);
        }
        assert!(lo >= 0.0 && hi <= 1.0 && hi - lo > 0.9, "range {lo}..{hi}");
        let rendered = ev.eval_image(&g, exp, A, &p, &CancelToken::new()).unwrap();
        assert!(rendered.data().iter().zip(p.0.data()).any(|(a,b)| (a-b).abs() > 0.01));
        let hits = ev.hits();
        ev.eval_image(&g, exp, A, &p, &CancelToken::new()).unwrap();
        assert!(ev.hits() > hits);
        println!("fixture {w}x{h} depth range {lo}..{hi}");
    }

    #[test]
    fn ordinary_image_fanin_routes_members_and_plans_consistently() {
        let mut g = Graph::new();
        let narrow = g.add_node(NodeKind::Source, [0.0; 2]);
        let broad = g.add_node(NodeKind::Source, [0.0; 2]);
        let first = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let second = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        set_data(&mut g, narrow, A, Some(NodeData::Folder("/a/b".into())));
        set_data(&mut g, broad, A, Some(NodeData::Folder("/a".into())));
        set(&mut g, first, "ev", A, 1.0);
        set(&mut g, second, "ev", A, 2.0);
        g.connect(narrow, first, 0).unwrap();
        g.connect(broad, second, 0).unwrap();
        g.connect(first, out, 0).unwrap();
        g.connect(second, out, 0).unwrap();
        let g = Graph::from_json(&g.to_json()).unwrap();
        let p = TestProvider::default();
        *p.catalog.lock().unwrap() = vec![A, B];
        p.paths.lock().unwrap().insert(A.0, "/a/b/one.jpg".into());
        p.paths.lock().unwrap().insert(B.0, "/a/two.jpg".into());
        let env = Env { provider: &p, images: &[A, B], view_tone: ToneMap::AcesFit, control: false };
        assert_eq!(env.cat(|c| g.members(out, 0, c)), vec![A, B]);
        assert_eq!(env.input_edge(&g, out, 0, A), Some((first, 0)));
        assert_eq!(env.input_edge(&g, out, 0, B), Some((second, 0)));
        let mut ev = Evaluator::new();
        for (image, branch) in [(A, first), (B, second)] {
            let rendered = ev.eval_image(&g, out, image, &p, &CancelToken::new()).unwrap();
            let expected = ev.eval_image(&g, branch, image, &p, &CancelToken::new()).unwrap();
            assert_eq!(*rendered, *expected);
            let plan = ev.plan_tail(&g, out, image, &p).unwrap();
            let branch_plan = ev.plan_tail(&g, branch, image, &p).unwrap();
            assert_eq!(plan.base, branch_plan.base);
            assert_eq!(plan.base_fingerprint, branch_plan.base_fingerprint);
        }
        let before = fingerprint_of(&g, out, B, &p);
        let mut changed = g.clone();
        set(&mut changed, second, "ev", A, 3.0);
        assert_ne!(before, fingerprint_of(&changed, out, B, &p));
        assert_eq!(fingerprint_of(&g, out, A, &p), fingerprint_of(&changed, out, A, &p));
    }

    #[test]
    fn pixel_sort_graph_evaluation_and_parameter_cache() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let sort = g.add_node(NodeKind::PixelSort, [0.0; 2]);
        g.connect(src, sort, 0).unwrap();
        let p = TestProvider::default();
        *p.dims.lock().unwrap() = Some((2, 2));
        let original = p.image(A).unwrap();
        let mut expected = (*original).clone();
        fx::pixel_sort(&mut expected, fx::SortMetric::Brightness, false);
        let mut evaluator = Evaluator::new();
        let actual = evaluator.eval_image(&g, sort, A, &p, &CancelToken::new()).unwrap();
        assert_eq!(*actual, expected);
        set(&mut g, sort, "reverse", A, 1.0);
        let mut expected_reverse = (*original).clone();
        fx::pixel_sort(&mut expected_reverse, fx::SortMetric::Brightness, true);
        let actual = evaluator.eval_image(&g, sort, A, &p, &CancelToken::new()).unwrap();
        assert_eq!(*actual, expected_reverse);
        assert_ne!(expected, expected_reverse);
    }

    /// Source -> Exposure -> Saturation -> Output.
    fn chain() -> (Graph, [NodeId; 4]) {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let sat = g.add_node(NodeKind::Saturation, [0.0; 2]);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, sat, 0).unwrap();
        g.connect(sat, out, 0).unwrap();
        (g, [src, exp, sat, out])
    }

    fn fingerprint_of(g: &Graph, n: NodeId, img: ImageId, p: &dyn ImageProvider) -> u64 {
        let images = p.images();
        node_fingerprint(g, n, img, &Env { provider: p, images: &images, view_tone: ToneMap::AcesFit, control: false }, &mut FingerprintMemo::new(), &mut Vec::new()).unwrap()
    }

    fn set(g: &mut Graph, n: NodeId, name: &str, img: ImageId, v: f32) {
        assert!(g.node_mut(n).unwrap().set_param(name, img, v));
    }

    #[test]
    fn exposure_doubles_without_clamping() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, out, 0).unwrap();
        set(&mut g, exp, "ev", A, 1.0);
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let base = p.image(A).unwrap();
        let res = ev.eval_image(&g, out, A, &p, &CancelToken::new()).unwrap();
        let (b, r) = (base.pixel(63, 63), res.pixel(63, 63));
        assert!(b[0] > 0.5 && r[0] > 1.0, "values above 1.0 must survive");
        for c in 0..3 {
            assert!((r[c] - b[c] * 2.0).abs() < 1e-6);
        }
        assert_eq!(r[3], 1.0);
    }

    #[test]
    fn nodes_compose_in_graph_order() {
        let (mut g, [_, exp, sat, out]) = chain();
        set(&mut g, exp, "ev", A, 1.5);
        set(&mut g, sat, "amount", A, 0.3);
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let res = ev.eval_image(&g, out, A, &p, &CancelToken::new()).unwrap();
        let mut manual = (*p.image(A).unwrap()).clone();
        ops::apply_gain(&mut manual, 2f32.powf(1.5));
        ops::saturation(&mut manual, 0.3);
        assert_eq!(*res, manual);
    }

    #[test]
    fn node_timings_are_request_local_and_distinguish_cache_hits() {
        let (g, ids) = chain();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        assert!(ev.timings().is_none());
        ev.set_timing_enabled(true);
        ev.eval(&g, ids[3], A, &p, &c).unwrap();
        let rows = ev.timings().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.computed == 1 && r.cache_hits == 0));
        ev.set_timing_enabled(true);
        ev.eval(&g, ids[3], A, &p, &c).unwrap();
        let rows = ev.timings().unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(|r| r.computed == 0 && r.cache_hits == 1 && r.cpu.is_zero()));
        ev.set_timing_enabled(false);
        ev.eval(&g, ids[3], A, &p, &c).unwrap();
        assert!(ev.timings().is_none());
    }

    #[test]
    fn second_eval_is_fully_cached() {
        let (g, [.., out]) = chain();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        ev.eval(&g, out, A, &p, &c).unwrap();
        let (misses, calls) = (ev.misses(), p.calls());
        assert_eq!(misses, 4);
        ev.eval(&g, out, A, &p, &c).unwrap();
        assert_eq!(ev.misses(), misses);
        assert_eq!(p.calls(), calls);
        assert!(ev.hits() > 0);
    }

    #[test]
    fn changing_last_node_recomputes_only_downstream() {
        let (mut g, [_, exp, sat, out]) = chain();
        set(&mut g, exp, "ev", A, 1.0);
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        ev.eval(&g, out, A, &p, &c).unwrap();
        let before = ev.misses();
        set(&mut g, sat, "amount", A, 0.5);
        ev.eval(&g, out, A, &p, &c).unwrap();
        // Saturation changed; Output sees a new input fingerprint. Source and Exposure stay cached.
        assert_eq!(ev.misses() - before, 2);
        assert_eq!(p.calls(), 1);
    }

    #[test]
    fn individual_mode_is_per_image() {
        let (mut g, [_, exp, _, out]) = chain();
        g.node_mut(exp).unwrap().mode = rawgraph_core::Mode::Individual;
        set(&mut g, exp, "ev", A, 2.0);
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        let a1 = ev.eval_image(&g, out, A, &p, &c).unwrap();
        let b1 = ev.eval_image(&g, out, B, &p, &c).unwrap();
        // B inherits ev = 0 (identity), A is +2 stops.
        assert_eq!(*b1, *p.image(B).unwrap());
        assert!(*a1 != *p.image(A).unwrap());
        let misses = ev.misses();
        set(&mut g, exp, "ev", A, 3.0);
        ev.eval(&g, out, B, &p, &c).unwrap();
        assert_eq!(ev.misses(), misses, "B stays cached when A's value changes");
        ev.eval(&g, out, A, &p, &c).unwrap();
        assert!(ev.misses() > misses);
    }

    #[test]
    fn identity_returns_same_arc() {
        let (g, [src, _, _, out]) = chain();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        let s = ev.eval_image(&g, src, A, &p, &c).unwrap();
        let o = ev.eval_image(&g, out, A, &p, &c).unwrap();
        assert!(Arc::ptr_eq(&s, &o));

        // White balance neutral and blur at 0 px are identities too.
        let mut g2 = Graph::new();
        let s2 = g2.add_node(NodeKind::Source, [0.0; 2]);
        let wb = g2.add_node(NodeKind::WhiteBalance, [0.0; 2]);
        let bl = g2.add_node(NodeKind::Blur, [0.0; 2]);
        g2.connect(s2, wb, 0).unwrap();
        g2.connect(wb, bl, 0).unwrap();
        set(&mut g2, bl, "radius", A, 0.0);
        let base = ev.eval_image(&g2, s2, A, &p, &c).unwrap();
        let res = ev.eval_image(&g2, bl, A, &p, &c).unwrap();
        assert!(Arc::ptr_eq(&base, &res));
    }

    #[test]
    fn error_cases() {
        let mut g = Graph::new();
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        assert_eq!(ev.eval(&g, exp, A, &p, &c).unwrap_err(), EvalError::MissingInput { node: exp, port: 0 });
        assert_eq!(ev.eval(&g, NodeId(999), A, &p, &c).unwrap_err(), EvalError::UnknownNode(NodeId(999)));
        assert!(!EvalError::Cancelled.to_string().is_empty());
    }

    #[test]
    fn exposed_filter_radius_uses_spatial_map_and_invalidates() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let map = g.add_node(NodeKind::Generator, [0.0; 2]);
        let filter = g.add_node(NodeKind::Blur, [0.0; 2]);
        g.connect(src, filter, 0).unwrap();
        set(&mut g, map, "width", A, 64.0);
        set(&mut g, map, "height", A, 64.0);
        set(&mut g, filter, "radius", A, 80.0);
        let port = g.expose_parameter(filter, "radius", PortType::Images).unwrap();
        g.connect(map, filter, port).unwrap();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        let out = ev.eval_image(&g, filter, A, &p, &c).unwrap();
        let m = control_plane(&render_generator(g.node(map).unwrap(), A, 64, 64).unwrap(), &p.image(A).unwrap());
        // White blurs with the slider radius, not the slider maximum.
        let slider = 80.0 / 1000.0 * 64.0;
        assert_eq!(*out, ops::filter_low_modulated(&p.image(A).unwrap(), slider, &m, false));
        let misses = ev.misses();
        set(&mut g, map, "gradient_angle", A, 90.0);
        let changed = ev.eval_image(&g, filter, A, &p, &c).unwrap();
        assert!(ev.misses() > misses);
        assert_ne!(*out, *changed);
    }

    #[test]
    fn motion_direction_map_is_auxiliary_and_invalidates_cache() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let map = g.add_node(NodeKind::Generator, [0.0; 2]);
        let filter = g.add_node(NodeKind::Blur, [0.0; 2]);
        g.connect(src, filter, 0).unwrap();
        g.connect(src, map, 0).unwrap();
        set(&mut g, filter, "radius", A, 60.0);
        set(&mut g, filter, "kernel", A, 3.0);
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        let base = p.image(A).unwrap();
        let port = g.expose_parameter(filter, "direction", PortType::Images).unwrap();
        g.connect(map, filter, port).unwrap();
        set(&mut g, map, "output", A, 1.0);
        set(&mut g, map, "start_r", A, 1.0);
        set(&mut g, map, "end_r", A, 1.0);
        set(&mut g, map, "start_g", A, 0.5);
        set(&mut g, map, "end_g", A, 0.5);
        let direction = ev.eval_image(&g, map, A, &p, &c).unwrap();
        let horizontal = ev.eval_image(&g, filter, A, &p, &c).unwrap();
        assert_eq!(*horizontal, ops::filter_sampled_direction(&base, 3.84, 3, 0.0, None, Some(&direction)));
        set(&mut g, map, "start_r", A, 0.5);
        set(&mut g, map, "end_r", A, 0.5);
        set(&mut g, map, "start_g", A, 1.0);
        set(&mut g, map, "end_g", A, 1.0);
        let vertical = ev.eval_image(&g, filter, A, &p, &c).unwrap();
        assert_ne!(*horizontal, *vertical);
        let direction = ev.eval_image(&g, map, A, &p, &c).unwrap();
        assert_eq!(*vertical, ops::filter_sampled_direction(&base, 3.84, 3, 0.0, None, Some(&direction)));
    }

    #[test]
    fn filter_modes_kernels_and_mask_use_centered_residual() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let mask = g.add_node(NodeKind::Generator, [0.0; 2]);
        let filter = g.add_node(NodeKind::Blur, [0.0; 2]);
        g.connect(src, filter, 0).unwrap();
        set(&mut g, filter, "radius", A, 30.0);
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        let base = p.image(A).unwrap();
        let port = g.expose_parameter(filter, "radius", PortType::Images).unwrap();
        for kernel in 0..=8 {
            set(&mut g, filter, "kernel", A, kernel as f32);
            for masked in [false, true] {
                if masked { g.connect(mask, filter, port).unwrap(); }
                set(&mut g, filter, "pass", A, 0.0);
                let low = ev.eval_image(&g, filter, A, &p, &c).unwrap();
                let expected = if masked {
                    let m = control_plane(&render_generator(g.node(mask).unwrap(), A, 1024, 1024).unwrap(), &base);
                    // The map scales the slider radius (30 = 1.92 px here), not the slider maximum.
                    if kernel < 2 { ops::filter_low_modulated(&base, 1.92, &m, kernel == 1) }
                    else { ops::filter_sampled(&base, 1.92, kernel, 0.0, Some(&m)) }
                } else if kernel < 2 { ops::filter_low(&base, 1.92, kernel == 1) }
                else { ops::filter_sampled(&base, 1.92, kernel, 0.0, None) };
                assert_eq!(*low, expected);
                set(&mut g, filter, "pass", A, 1.0);
                let high = ev.eval_image(&g, filter, A, &p, &c).unwrap();
                for ((a, b), h) in base.data().as_chunks::<4>().0.iter().zip(low.data().as_chunks::<4>().0).zip(high.data().as_chunks::<4>().0) {
                    for channel in 0..3 { assert!((h[channel] - (0.5 + a[channel] - b[channel])).abs() < 1e-6); }
                    assert_eq!(h[3], a[3]);
                }
                if masked { g.disconnect(filter, port); }
            }
        }
    }

    #[test]
    fn provider_revision_invalidates() {
        let (g, [.., out]) = chain();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        ev.eval(&g, out, A, &p, &c).unwrap();
        p.revision.store(1, Ordering::Relaxed);
        let before = ev.misses();
        ev.eval(&g, out, A, &p, &c).unwrap();
        assert_eq!(ev.misses() - before, 4);
        assert_eq!(p.calls(), 2);
    }

    #[test]
    fn cancellation() {
        let (mut g, [_, exp, _, out]) = chain();
        set(&mut g, exp, "ev", A, 1.0);
        let (p, mut ev) = (TestProvider::default(), Evaluator::new());
        let tok = CancelToken::new();
        tok.cancel();
        assert!(tok.is_cancelled());
        assert_eq!(ev.eval(&g, out, A, &p, &tok).unwrap_err(), EvalError::Cancelled);
        assert_eq!(ev.misses(), 0);
        let fresh = CancelToken::new();
        let res = ev.eval_image(&g, out, A, &p, &fresh).unwrap();
        let mut manual = (*p.image(A).unwrap()).clone();
        ops::apply_gain(&mut manual, 2.0);
        assert_eq!(*res, manual);
    }

    #[test]
    fn batch_partial_failure_and_cancel() {
        let (g, [.., out]) = chain();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        p.failing.lock().unwrap().insert(B.0);
        let mut log = Vec::new();
        let res = ev.render_batch(&g, out, &[A, B, C], &p, &c, &mut |d, t| log.push((d, t)));
        assert_eq!(res.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![A, B, C]);
        assert!(res[0].1.is_ok());
        assert!(matches!(res[1].1, Err(EvalError::Provider(_))));
        assert!(res[2].1.is_ok());
        assert_eq!(log, vec![(1, 3), (2, 3), (3, 3)]);
        // Cache is dropped per image.
        let before = ev.misses();
        ev.eval(&g, out, A, &p, &c).unwrap();
        assert!(ev.misses() > before);

        let tok = CancelToken::new();
        tok.cancel();
        let res = ev.render_batch(&g, out, &[A, B], &p, &tok, &mut |_, _| {});
        assert!(res.iter().all(|(_, r)| matches!(r, Err(EvalError::Cancelled))));
    }

    #[test]
    fn retain_image_drops_others() {
        let (g, [.., out]) = chain();
        let (p, mut ev, c) = (TestProvider::default(), Evaluator::new(), CancelToken::new());
        ev.eval(&g, out, A, &p, &c).unwrap();
        ev.eval(&g, out, B, &p, &c).unwrap();
        ev.retain_image(A);
        let m = ev.misses();
        ev.eval(&g, out, A, &p, &c).unwrap();
        assert_eq!(ev.misses(), m);
        ev.eval(&g, out, B, &p, &c).unwrap();
        assert_eq!(ev.misses() - m, 4);
    }

    fn run(g: &Graph, n: NodeId, img: ImageId, p: &TestProvider, ev: &mut Evaluator) -> Arc<ImageBuf> {
        ev.eval_image(g, n, img, p, &CancelToken::new()).unwrap()
    }

    #[test]
    fn new_pixel_nodes_match_ops_and_identities() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let con = g.add_node(NodeKind::Contrast, [0.0; 2]);
        let hs = g.add_node(NodeKind::HighlightsShadows, [0.0; 2]);
        let nr = g.add_node(NodeKind::NoiseReduction, [0.0; 2]);
        g.connect(src, con, 0).unwrap();
        g.connect(con, hs, 0).unwrap();
        g.connect(hs, nr, 0).unwrap();
        let (p, mut ev) = (TestProvider::default(), Evaluator::new());
        let base = run(&g, src, A, &p, &mut ev);
        assert!(Arc::ptr_eq(&base, &run(&g, nr, A, &p, &mut ev)));

        set(&mut g, con, "amount", A, 0.5);
        let mut m = (*base).clone();
        ops::contrast(&mut m, 0.5, g.node(con).unwrap().param("pivot", A));
        assert_eq!(*run(&g, con, A, &p, &mut ev), m);
        set(&mut g, hs, "highlights", A, -0.5);
        set(&mut g, hs, "shadows", A, 0.4);
        ops::highlights_shadows(&mut m, -0.5, 0.4);
        assert_eq!(*run(&g, hs, A, &p, &mut ev), m);
        set(&mut g, nr, "luma", A, 0.7);
        set(&mut g, nr, "chroma", A, 0.5);
        set(&mut g, nr, "radius", A, 2.0);
        let n = g.node(nr).unwrap();
        let (luma, chroma, radius) = (n.param("luma", A), n.param("chroma", A), n.param("radius", A));
        let expect = ops::noise_reduction(&m, luma, chroma, radius / 1000.0 * 64.0);
        assert_eq!(*run(&g, nr, A, &p, &mut ev), expect);
    }

    type MergeGraph = (Graph, NodeId, NodeId, NodeId, NodeId);

    /// src -> filterA(ISO 100..1000) -> exposure(+1) -> merge A;
    /// src -> filterB(ISO 500..3200) -> saturation(0.2) -> merge B; merge -> out.
    /// Returns (graph, filterA, filterB, exposure, out).
    fn merge_graph() -> MergeGraph {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0; 2]);
        let fa = g.add_node(NodeKind::MetadataFilter, [0.0; 2]);
        let fb = g.add_node(NodeKind::MetadataFilter, [0.0; 2]);
        let exp = g.add_node(NodeKind::Exposure, [0.0; 2]);
        let sat = g.add_node(NodeKind::Saturation, [0.0; 2]);
        let mg = g.add_node(NodeKind::Merge, [0.0; 2]);
        let out = g.add_node(NodeKind::Output, [0.0; 2]);
        g.connect(src, fa, 0).unwrap();
        g.connect(src, fb, 0).unwrap();
        g.connect(fa, exp, 0).unwrap();
        g.connect(fb, sat, 0).unwrap();
        g.connect(exp, mg, 0).unwrap();
        g.connect(sat, mg, 1).unwrap();
        g.connect(mg, out, 0).unwrap();
        set(&mut g, fa, "min", A, 100.0);
        set(&mut g, fa, "max", A, 1000.0);
        set(&mut g, fb, "min", A, 500.0);
        set(&mut g, fb, "max", A, 3200.0);
        set(&mut g, exp, "ev", A, 1.0);
        set(&mut g, sat, "amount", A, 0.2);
        (g, fa, fb, exp, out)
    }

    fn set_iso(p: &TestProvider, i: ImageId, iso: f32) {
        p.iso.lock().unwrap().insert(i.0, iso);
    }

    #[test]
    fn empty_source_excludes_preview_without_loading_catalog_image() {
        let (mut g, [src, _, _, out]) = chain();
        let p = TestProvider::default();
        p.catalog.lock().unwrap().push(A);
        g.node_mut(src).unwrap().set_data(A, Some(rawgraph_core::NodeData::Folder(String::new())));
        let mut ev = Evaluator::new();
        assert!(!ev.contains(&g, out, A, &p));
        assert!(matches!(ev.eval(&g, out, A, &p, &CancelToken::new()), Err(EvalError::NotInCollection { .. })));
        assert!(matches!(ev.plan_tail(&g, out, A, &p), Err(EvalError::NotInCollection { .. })));
        assert_eq!(p.calls(), 0);
    }

    #[test]
    fn filter_membership_and_no_pixel_load() {
        let (g, fa, _, exp, _) = merge_graph();
        let p = TestProvider::default();
        set_iso(&p, A, 200.0);
        set_iso(&p, B, 3000.0);
        let mut ev = Evaluator::new();
        assert!(ev.eval(&g, fa, A, &p, &CancelToken::new()).is_ok());
        let calls = p.calls();
        for n in [fa, exp] {
            assert_eq!(ev.eval(&g, n, B, &p, &CancelToken::new()).unwrap_err(), EvalError::NotInCollection { node: n });
            assert!(!ev.contains(&g, n, B, &p));
        }
        assert_eq!(p.calls(), calls, "no pixels loaded for a filtered-out image");
        assert!(ev.contains(&g, exp, A, &p));
        assert!(EvalError::NotInCollection { node: fa }.to_string().contains("not part of the collection"));
    }

    #[test]
    fn merge_picks_first_port_and_skips_unused_branch() {
        let (g, _, _, _, out) = merge_graph();
        let p = TestProvider::default();
        let (ia, ib, iboth, inone) = (ImageId(10), ImageId(11), ImageId(12), ImageId(13));
        for (i, iso) in [(ia, 200.0), (ib, 3000.0), (iboth, 800.0), (inone, 5000.0)] {
            set_iso(&p, i, iso);
        }
        let mut ev = Evaluator::new();
        let mut gain = (*p.image(ia).unwrap()).clone();
        ops::apply_gain(&mut gain, 2.0);
        assert_eq!(*run(&g, out, ia, &p, &mut ev), gain);
        // source, filterA, exposure, merge, output - never branch B.
        assert_eq!(ev.misses(), 5);

        let mut desat = (*p.image(ib).unwrap()).clone();
        ops::saturation(&mut desat, 0.2);
        assert_eq!(*run(&g, out, ib, &p, &mut ev), desat);

        let mut both = (*p.image(iboth).unwrap()).clone();
        ops::apply_gain(&mut both, 2.0);
        assert_eq!(*run(&g, out, iboth, &p, &mut ev), both);

        let r = ev.eval(&g, out, inone, &p, &CancelToken::new());
        assert!(matches!(r, Err(EvalError::NotInCollection { .. })));
        assert!(!ev.contains(&g, out, inone, &p));
        assert!(!p.loaded.lock().unwrap().contains(&inone.0));
    }

    #[test]
    fn filter_threshold_change_switches_branch_without_stale_cache() {
        let (mut g, fa, fb, _, out) = merge_graph();
        let p = TestProvider::default();
        set_iso(&p, A, 200.0);
        let mut ev = Evaluator::new();
        let mut gain = (*p.image(A).unwrap()).clone();
        ops::apply_gain(&mut gain, 2.0);
        assert_eq!(*run(&g, out, A, &p, &mut ev), gain);
        set(&mut g, fa, "min", A, 300.0);
        set(&mut g, fb, "min", A, 100.0);
        let mut desat = (*p.image(A).unwrap()).clone();
        ops::saturation(&mut desat, 0.2);
        assert_eq!(*run(&g, out, A, &p, &mut ev), desat);
        // A change of the capture metadata alone also takes effect.
        set_iso(&p, A, 5000.0);
        assert!(ev.eval(&g, out, A, &p, &CancelToken::new()).is_err());
    }

    #[test]
    fn lean_mode_keeps_one_entry_and_same_pixels() {
        let (g, [_, _, _, out]) = chain();
        let p = TestProvider::default();
        let mut full = Evaluator::new();
        let mut lean = Evaluator::new();
        lean.set_keep_intermediates(false);
        let a = run(&g, out, A, &p, &mut full);
        let b = run(&g, out, A, &p, &mut lean);
        assert_eq!(*a, *b);
        assert_eq!(lean.cache.len(), 1);
        assert_eq!(full.cache.len(), 4);

        // Diamond: exposure feeds both merge branches.
        let mut d = Graph::new();
        let src = d.add_node(NodeKind::Source, [0.0; 2]);
        let exp = d.add_node(NodeKind::Exposure, [0.0; 2]);
        let sa = d.add_node(NodeKind::Saturation, [0.0; 2]);
        let sb = d.add_node(NodeKind::Contrast, [0.0; 2]);
        let mg = d.add_node(NodeKind::Merge, [0.0; 2]);
        d.connect(src, exp, 0).unwrap();
        d.connect(exp, sa, 0).unwrap();
        d.connect(exp, sb, 0).unwrap();
        d.connect(sa, mg, 0).unwrap();
        d.connect(sb, mg, 1).unwrap();
        set(&mut d, exp, "ev", A, 1.0);
        set(&mut d, sa, "amount", A, 0.5);
        let (mut e1, mut e2) = (Evaluator::new(), Evaluator::new());
        e2.set_keep_intermediates(false);
        assert_eq!(*run(&d, mg, A, &p, &mut e1), *run(&d, mg, A, &p, &mut e2));
        assert_eq!(e2.cache.len(), 1);
        assert_eq!(*run(&d, mg, A, &p, &mut e1), *run(&d, mg, A, &p, &mut e2));
    }

    #[test]
    fn batch_reports_not_in_collection_and_continues() {
        let (g, _, _, _, out) = merge_graph();
        let p = TestProvider::default();
        set_iso(&p, A, 200.0);
        set_iso(&p, B, 9000.0);
        set_iso(&p, C, 3000.0);
        let mut ev = Evaluator::new();
        let res = ev.render_batch(&g, out, &[A, B, C], &p, &CancelToken::new(), &mut |_, _| {});
        assert!(res[0].1.is_ok());
        assert!(matches!(res[1].1, Err(EvalError::NotInCollection { .. })));
        assert!(res[2].1.is_ok());
    }

    // ---- wave 4: new node kinds, mask frames, Develop ----

    fn add(g: &mut Graph, kind: NodeKind) -> NodeId {
        g.add_node(kind, [0.0; 2])
    }

    /// Source -> `kind` (image port 0). Returns (graph, source, node).
    fn one(kind: NodeKind) -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new();
        let src = add(&mut g, NodeKind::Source);
        let n = add(&mut g, kind);
        g.connect(src, n, 0).unwrap();
        (g, src, n)
    }

    fn pv(g: &Graph, n: NodeId, name: &str) -> f32 {
        g.node(n).unwrap().param(name, A)
    }

    #[test]
    fn new_nodes_are_identities_at_defaults() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        for kind in [
            NodeKind::Develop,
            NodeKind::ToneCurve,
            NodeKind::HueSaturation,
            NodeKind::Sharpen,
            NodeKind::Vignette,
            NodeKind::LensManual,
            NodeKind::Transform,
        ] {
            let (g, src, n) = one(kind);
            let base = run(&g, src, A, &p, &mut ev);
            let res = run(&g, n, A, &p, &mut ev);
            assert!(Arc::ptr_eq(&base, &res), "{kind:?} must return the input Arc at its defaults");
        }
        // BlackWhite has no identity: its default weights change the picture.
        let (g, src, n) = one(NodeKind::BlackWhite);
        assert!(*run(&g, src, A, &p, &mut ev) != *run(&g, n, A, &p, &mut ev));
    }

    #[test]
    fn new_image_nodes_match_fx() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let base = p.image(A).unwrap();

        let (mut g, src, n) = one(NodeKind::ToneCurve);
        set(&mut g, n, "shadows", A, 0.4);
        set(&mut g, n, "lights", A, -0.3);
        let mut m = (*base).clone();
        fx::tone_curve(&mut m, pv(&g, n, "shadows"), pv(&g, n, "darks"), pv(&g, n, "lights"), pv(&g, n, "highlights"));
        assert_eq!(*run(&g, n, A, &p, &mut ev), m);
        assert!(*base != m);
        let _ = src;

        let (mut g, _, n) = one(NodeKind::HueSaturation);
        set(&mut g, n, "red_h", A, 0.2);
        set(&mut g, n, "blue_s", A, -0.5);
        set(&mut g, n, "green_l", A, 0.3);
        let mut bands = [[0.0f32; 3]; 6];
        for (b, name) in bands.iter_mut().zip(["red", "yellow", "green", "cyan", "blue", "magenta"]) {
            *b = [pv(&g, n, &format!("{name}_h")), pv(&g, n, &format!("{name}_s")), pv(&g, n, &format!("{name}_l"))];
        }
        let mut m = (*base).clone();
        fx::hue_saturation(&mut m, &bands);
        assert_eq!(*run(&g, n, A, &p, &mut ev), m);

        let (mut g, _, n) = one(NodeKind::Sharpen);
        set(&mut g, n, "amount", A, 1.5);
        set(&mut g, n, "radius", A, 2.0);
        set(&mut g, n, "threshold", A, 0.1);
        let radius_px = pv(&g, n, "radius") / 1000.0 * 64.0;
        let expect = fx::sharpen(&base, 1.5, radius_px, pv(&g, n, "threshold"), None);
        assert_eq!(*run(&g, n, A, &p, &mut ev), expect);

        let (mut g, _, n) = one(NodeKind::Vignette);
        set(&mut g, n, "amount", A, -0.6);
        let mut m = (*base).clone();
        fx::vignette(&mut m, pv(&g, n, "amount"), pv(&g, n, "midpoint"), pv(&g, n, "feather"));
        assert_eq!(*run(&g, n, A, &p, &mut ev), m);

        let (g, _, n) = one(NodeKind::BlackWhite);
        let mut m = (*base).clone();
        fx::black_white(&mut m, [pv(&g, n, "red"), pv(&g, n, "green"), pv(&g, n, "blue")]);
        assert_eq!(*run(&g, n, A, &p, &mut ev), m);

        let (mut g, _, n) = one(NodeKind::LensManual);
        set(&mut g, n, "distortion", A, 0.1);
        set(&mut g, n, "ca_red", A, 1.0);
        let expect = fx::lens_manual(&base, pv(&g, n, "distortion"), 0.0, pv(&g, n, "ca_red"), 0.0);
        assert_eq!(*run(&g, n, A, &p, &mut ev), expect);
    }

    #[test]
    fn transform_matches_fx_and_changes_size() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, _, n) = one(NodeKind::Transform);
        set(&mut g, n, "left", A, 0.1);
        set(&mut g, n, "right", A, 0.7);
        set(&mut g, n, "angle", A, 3.0);
        set(&mut g, n, "rotate", A, 1.0);
        set(&mut g, n, "flip_h", A, 1.0);
        let params = fx::TransformParams {
            left: pv(&g, n, "left"),
            top: pv(&g, n, "top"),
            right: pv(&g, n, "right"),
            bottom: pv(&g, n, "bottom"),
            angle_deg: pv(&g, n, "angle"),
            quarter_turns: 1,
            flip_h: true,
            flip_v: false,
        };
        let base = p.image(A).unwrap();
        let res = run(&g, n, A, &p, &mut ev);
        assert_eq!(*res, fx::transform(&base, &params));
        let (w, h) = fx::transform_size(64, 64, &params);
        assert_eq!((res.width(), res.height()), (w, h));
        assert!(w != 64 || h != 64);
    }

    /// Source -> `kind` (image) with `mask` on port 1. Returns (graph, source, node).
    fn raw_capture(kelvin: Option<f32>, baseline: Option<f32>) -> CaptureInfo {
        CaptureInfo {
            raw: Some(true),
            as_shot_kelvin: kelvin,
            as_shot_tint: kelvin.map(|_| 4.0),
            baseline_exposure: baseline,
            ..Default::default()
        }
    }

    #[test]
    fn neutral_develop_nodes_do_not_repeat_raw_input_gain() {
        let p = TestProvider::default();
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let mut ev = Evaluator::new();
        let (mut g, src, first) = one(NodeKind::Develop);
        let second = add(&mut g, NodeKind::Develop);
        g.connect(first, second, 0).unwrap();
        let base = run(&g, src, A, &p, &mut ev);
        assert_eq!(*run(&g, first, A, &p, &mut ev), *base);
        assert_eq!(*run(&g, second, A, &p, &mut ev), *base);
        let plan = ev.plan_tail(&g, second, A, &p).unwrap();
        assert!(plan.ops.is_empty());
    }

    #[test]
    fn develop_baseline_sources() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (g, src, n) = one(NodeKind::Develop);
        let base = run(&g, src, A, &p, &mut ev);

        // The provider supplies already-decoded pixels; Develop must not add input gain.
        for baseline in [Some(0.5), None] {
            p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), baseline));
            assert_eq!(*run(&g, n, A, &p, &mut ev), *base);
            let s = develop_settings(g.node(n).unwrap(), A, p.capture(A).as_ref());
            assert_eq!((s.baseline_ev, s.brightness_ev, s.gain_ev), (0.0, 0.0, 0.0));
        }
        let mut g2 = g.clone();
        set(&mut g2, n, "baseline", A, 0.0);
        set(&mut g2, n, "brightness", A, -1.0);
        assert_eq!(*run(&g2, n, A, &p, &mut ev), *base);

        // Rendered and unknown sources are never brightened.
        p.caps.lock().unwrap().insert(A.0, CaptureInfo { raw: Some(false), baseline_exposure: Some(0.5), ..Default::default() });
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)));
        p.caps.lock().unwrap().remove(&A.0);
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)));
    }

    #[test]
    fn develop_custom_white_balance_changes_pixels() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, src, n) = one(NodeKind::Develop);
        set(&mut g, n, "brightness", A, 0.0);
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.0)));
        let base = run(&g, src, A, &p, &mut ev);
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)), "as shot with baseline 0 is neutral");
        set(&mut g, n, "wb", A, 1.0);
        set(&mut g, n, "temperature", A, 3200.0);
        let res = run(&g, n, A, &p, &mut ev);
        assert!(*res != *base);
        let s = develop_settings(g.node(n).unwrap(), A, p.capture(A).as_ref());
        let mut expect = (*base).clone();
        ops::apply_matrix(&mut expect, s.matrix.expect("custom white balance has a matrix"));
        assert_eq!(*res, expect);
    }

    #[test]
    fn develop_never_serves_a_stale_capture() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, src, n) = one(NodeKind::Develop);
        set(&mut g, n, "brightness", A, 0.0);
        let base = run(&g, src, A, &p, &mut ev);
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let first = run(&g, n, A, &p, &mut ev);
        // Metadata changes without a provider revision bump.
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(1.5)));
        let second = run(&g, n, A, &p, &mut ev);
        assert_eq!(*first, *base);
        assert_eq!(*second, *base);
        // As-shot temperature matters for a custom white balance.
        set(&mut g, n, "wb", A, 1.0);
        set(&mut g, n, "temperature", A, 4000.0);
        let before = run(&g, n, A, &p, &mut ev);
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(7000.0), Some(1.5)));
        let after = run(&g, n, A, &p, &mut ev);
        assert!(*before != *after);
        // Back to as shot, metadata gone and a revision bump: plain pass-through again.
        set(&mut g, n, "wb", A, 0.0);
        p.revision.store(5, Ordering::Relaxed);
        p.caps.lock().unwrap().remove(&A.0);
        let again = run(&g, src, A, &p, &mut ev);
        assert!(Arc::ptr_eq(&again, &run(&g, n, A, &p, &mut ev)));
    }

    /// Source -> Transform (crop to the middle half horizontally and 3/4 vertically).
    fn cropped(g: &mut Graph) -> (NodeId, NodeId) {
        let src = add(g, NodeKind::Source);
        let t = add(g, NodeKind::Transform);
        g.connect(src, t, 0).unwrap();
        set(g, t, "left", A, 0.25);
        set(g, t, "right", A, 0.75);
        set(g, t, "bottom", A, 0.75);
        (src, t)
    }

    // ---- wave 5: multiple outputs, Split by Metadata, PointCurve, Draw ----

    fn ranges(list: &[(&str, f32, f32)]) -> NodeData {
        NodeData::Ranges(
            list.iter().map(|(n, a, b)| rawgraph_core::NamedRange { name: n.to_string(), min: *a, max: *b }).collect(),
        )
    }

    fn set_data(g: &mut Graph, n: NodeId, img: ImageId, d: Option<NodeData>) {
        assert!(g.node_mut(n).unwrap().set_data(img, d));
    }

    /// Source -> split (ranges: low 50..400, mid 401..1600); port 0 -> Exposure(+1),
    /// port 1 -> Saturation(0.2), port 6 -> Contrast(0.5); all merged into the output.
    /// Returns (graph, split, [exposure, saturation, contrast], out).
    fn split_graph() -> (Graph, NodeId, [NodeId; 3], NodeId) {
        let mut g = Graph::new();
        let src = add(&mut g, NodeKind::Source);
        let sp = add(&mut g, NodeKind::MetadataSplit);
        let exp = add(&mut g, NodeKind::Exposure);
        let sat = add(&mut g, NodeKind::Saturation);
        let con = add(&mut g, NodeKind::Contrast);
        let mg = add(&mut g, NodeKind::Merge);
        let out = add(&mut g, NodeKind::Output);
        g.connect(src, sp, 0).unwrap();
        g.connect_ports(sp, 0, exp, 0).unwrap();
        g.connect_ports(sp, 1, sat, 0).unwrap();
        g.connect_ports(sp, 6, con, 0).unwrap();
        g.connect(exp, mg, 0).unwrap();
        g.connect(sat, mg, 1).unwrap();
        g.connect(con, mg, 2).unwrap();
        g.connect(mg, out, 0).unwrap();
        set_data(&mut g, sp, A, Some(ranges(&[("low", 50.0, 400.0), ("mid", 401.0, 1600.0)])));
        set(&mut g, exp, "ev", A, 1.0);
        set(&mut g, sat, "amount", A, 0.2);
        set(&mut g, con, "amount", A, 0.5);
        (g, sp, [exp, sat, con], out)
    }

    fn expected_branch(p: &TestProvider, img: ImageId, g: &Graph, con: NodeId, branch: usize) -> ImageBuf {
        let mut m = (*p.image(img).unwrap()).clone();
        match branch {
            0 => ops::apply_gain(&mut m, 2.0),
            1 => ops::saturation(&mut m, 0.2),
            _ => ops::contrast(&mut m, 0.5, g.node(con).unwrap().param("pivot", A)),
        }
        m
    }

    #[test]
    fn split_routes_each_image_through_its_branch_and_skips_the_rest() {
        let (g, sp, [_, _, con], out) = split_graph();
        let p = TestProvider::default();
        let (lo, mid, hi, none) = (ImageId(10), ImageId(11), ImageId(12), ImageId(13));
        for (i, iso) in [(lo, 200.0), (mid, 800.0), (hi, 6400.0)] {
            set_iso(&p, i, iso);
        }
        let mut ev = Evaluator::new();
        for (img, branch) in [(lo, 0), (mid, 1), (hi, 2)] {
            let before = ev.misses();
            let res = run(&g, out, img, &p, &mut ev);
            assert_eq!(*res, expected_branch(&p, img, &g, con, branch), "image {img:?}");
            // source, split, one branch node, merge, output - the other two branches never run.
            assert_eq!(ev.misses() - before, 5, "unused branches are not computed");
        }
        // No capture data: the default is Unmatched (port 6 -> contrast).
        assert_eq!(*run(&g, out, none, &p, &mut ev), expected_branch(&p, none, &g, con, 2));
        // Every image appears exactly once: exactly one port carries it.
        for img in [lo, mid, hi, none] {
            let n = (0..7).filter(|&port| ev.contains_output(&g, sp, port, img, &p)).count();
            assert_eq!(n, 1);
        }
    }

    #[test]
    fn range_boundary_change_moves_image_without_stale_cache() {
        let (mut g, sp, [_, _, con], out) = split_graph();
        let p = TestProvider::default();
        set_iso(&p, A, 300.0);
        let mut ev = Evaluator::new();
        assert_eq!(*run(&g, out, A, &p, &mut ev), expected_branch(&p, A, &g, con, 0));
        set_data(&mut g, sp, A, Some(ranges(&[("low", 50.0, 200.0), ("mid", 201.0, 1600.0)])));
        assert_eq!(*run(&g, out, A, &p, &mut ev), expected_branch(&p, A, &g, con, 1));
        set_data(&mut g, sp, A, Some(ranges(&[("low", 50.0, 200.0)])));
        assert_eq!(*run(&g, out, A, &p, &mut ev), expected_branch(&p, A, &g, con, 2));
        // Back again: the earlier branch is recomputed, not served stale.
        set_data(&mut g, sp, A, Some(ranges(&[("low", 50.0, 400.0)])));
        assert_eq!(*run(&g, out, A, &p, &mut ev), expected_branch(&p, A, &g, con, 0));
    }

    #[test]
    fn consumer_on_a_port_the_image_does_not_leave_through() {
        let (g, _, [exp, sat, con], out) = split_graph();
        let p = TestProvider::default();
        set_iso(&p, B, 200.0); // leaves through port 0
        let mut ev = Evaluator::new();
        let calls = p.calls();
        for n in [sat, con] {
            assert_eq!(ev.eval(&g, n, B, &p, &CancelToken::new()).unwrap_err(), EvalError::NotInCollection { node: n });
        }
        assert_eq!(p.calls(), calls, "no pixels loaded");
        assert!(ev.eval(&g, exp, B, &p, &CancelToken::new()).is_ok());
        assert!(ev.eval(&g, out, B, &p, &CancelToken::new()).is_ok());
    }

    #[test]
    fn missing_metadata_exclude_vs_unmatched() {
        let (mut g, sp, [_, _, con], out) = split_graph();
        let p = TestProvider::default(); // no capture data at all
        let mut ev = Evaluator::new();
        assert_eq!(*run(&g, out, A, &p, &mut ev), expected_branch(&p, A, &g, con, 2));
        set(&mut g, sp, "missing", A, 1.0); // Exclude
        assert!((0..7).all(|port| !ev.contains_output(&g, sp, port, A, &p)));
        let r = ev.eval(&g, out, A, &p, &CancelToken::new());
        assert!(matches!(r, Err(EvalError::NotInCollection { .. })), "{r:?}");
        let r = ev.eval_output(&g, sp, 6, A, &p, &CancelToken::new());
        assert_eq!(r.unwrap_err(), EvalError::NotInCollection { node: sp });
    }

    #[test]
    fn viewing_the_split_and_eval_output_agree_with_membership() {
        let (g, sp, _, _) = split_graph();
        let p = TestProvider::default();
        let (lo, mid, hi) = (ImageId(10), ImageId(11), ImageId(12));
        for (i, iso) in [(lo, 200.0), (mid, 800.0), (hi, 6400.0)] {
            set_iso(&p, i, iso);
        }
        let mut ev = Evaluator::new();
        let c = CancelToken::new();
        for img in [lo, mid, hi] {
            let base = run(&g, g.input_edge(sp, 0).unwrap().0, img, &p, &mut ev);
            // Viewing the split passes the input through for every image.
            let viewed = run(&g, sp, img, &p, &mut ev);
            assert!(Arc::ptr_eq(&base, &viewed));
            for port in 0..7 {
                let r = ev.eval_output(&g, sp, port, img, &p, &c);
                if ev.contains_output(&g, sp, port, img, &p) {
                    match r.unwrap() {
                        Value::Images(i) => assert!(Arc::ptr_eq(&i, &base)),
                    }
                } else {
                    assert_eq!(r.unwrap_err(), EvalError::NotInCollection { node: sp });
                }
            }
        }
        // Non-split nodes: eval_output port 0 works like eval.
        let (g2, _, n) = one(NodeKind::Exposure);
        assert!(ev.eval_output(&g2, n, 0, A, &p, &c).is_ok());
    }

    fn curve_graph() -> (Graph, NodeId, NodeId) {
        let (mut g, src, n) = one(NodeKind::PointCurve);
        set_data(&mut g, n, A, Some(NodeData::Curve(vec![[0.0, 0.0], [0.5, 0.35], [1.0, 1.0]])));
        (g, src, n)
    }

    #[test]
    fn point_curve_matches_fx_and_identity_keeps_the_arc() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, src, n) = curve_graph();
        let base = run(&g, src, A, &p, &mut ev);
        for (idx, ch) in [(0.0, fx::CurveChannel::Luminance), (2.0, fx::CurveChannel::Green)] {
            set(&mut g, n, "channel", A, idx);
            let mut expect = (*base).clone();
            fx::point_curve(&mut expect, &[[0.0, 0.0], [0.5, 0.35], [1.0, 1.0]], ch);
            let res = run(&g, n, A, &p, &mut ev);
            assert_eq!(*res, expect);
            assert!(*res != *base);
        }
        set_data(&mut g, n, A, Some(NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]])));
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)));
        set_data(&mut g, n, A, None);
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)));
    }

    #[test]
    fn editing_the_curve_recomputes_from_that_node_on() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, _, n) = curve_graph();
        let out = add(&mut g, NodeKind::Output);
        g.connect(n, out, 0).unwrap();
        let first = run(&g, out, A, &p, &mut ev);
        let before = ev.misses();
        set_data(&mut g, n, A, Some(NodeData::Curve(vec![[0.0, 0.0], [0.5, 0.6], [1.0, 1.0]])));
        let second = run(&g, out, A, &p, &mut ev);
        assert_eq!(ev.misses() - before, 2, "curve + output; the source stays cached");
        assert!(*first != *second);
        assert_eq!(p.calls(), 1);
    }

    fn stroke() -> rawgraph_core::BrushStroke {
        rawgraph_core::BrushStroke { points: vec![[0.2, 0.2], [0.6, 0.5]], radius: 0.1, hardness: 0.5, flow: 1.0, erase: false }
    }

    #[test]
    fn standalone_draw_renders_and_drives_exposure() {
        let mut g = Graph::new();
        let draw = add(&mut g, NodeKind::Draw);
        let src = add(&mut g, NodeKind::Source);
        let exp = add(&mut g, NodeKind::Exposure);
        let image = ImageId(u64::MAX - draw.0);
        set(&mut g, draw, "width", image, 64.0);
        set(&mut g, draw, "height", image, 64.0);
        set_data(&mut g, draw, image, Some(NodeData::Strokes(vec![stroke()])));
        let provider = TestProvider::default();
        let mut eval = Evaluator::new();
        assert_eq!(eval.members(&g, draw, 0, &provider), vec![image]);
        let drawing = run(&g, draw, image, &provider, &mut eval);
        assert_eq!(drawing.pixel(0, 0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(drawing.pixel(13, 13), [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(provider.calls(), 0);
        set(&mut g, exp, "ev", A, 6.0);
        let port = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(src, exp, 0).unwrap();
        g.connect(draw, exp, port).unwrap();
        let base = provider.image(A).unwrap();
        let adjusted = run(&g, exp, A, &provider, &mut eval);
        // Unpainted (black) leaves the image alone; strokes (white) get the full +6 EV slider.
        assert_eq!(adjusted.pixel(0, 0)[0], base.pixel(0, 0)[0]);
        assert_eq!(adjusted.pixel(13, 13)[0], base.pixel(13, 13)[0] * 64.0);
        set_data(&mut g, draw, image, Some(NodeData::Strokes(vec![])));
        let changed = run(&g, exp, A, &provider, &mut eval);
        assert_ne!(adjusted.pixel(13, 13), changed.pixel(13, 13));
    }

    #[test]
    fn exposed_amount_matches_scalar_endpoints_and_invalidates() {
        for kind in [NodeKind::Contrast, NodeKind::Saturation, NodeKind::Sharpen] {
            let (mut g, _, effect) = one(kind);
            let map = add(&mut g, NodeKind::Generator);
            let width = if kind == NodeKind::Sharpen { 512 } else { 64 };
            set(&mut g, map, "width", A, width as f32);
            set(&mut g, map, "height", A, 64.0);
            set(&mut g, map, "gradient_scale", A, 50.0);
            if kind == NodeKind::Sharpen { set(&mut g, effect, "radius", A, 2.0); }
            let p = TestProvider::default();
            *p.dims.lock().unwrap() = Some((width, 64));
            p.texture.store(true, Ordering::Relaxed);
            let mut ev = Evaluator::new();
            let spec = kind.param_spec("amount").unwrap();
            set(&mut g, effect, "amount", A, spec.default);
            let black = run(&g, effect, A, &p, &mut ev);
            set(&mut g, effect, "amount", A, spec.max);
            let white = run(&g, effect, A, &p, &mut ev);
            assert!(black != white, "{kind:?}");
            // The slider stays at max: the map scales from no effect (black) to the slider (white).
            let port = g.expose_parameter(effect, "amount", PortType::Images).unwrap();
            g.connect(map, effect, port).unwrap();
            let spatial = run(&g, effect, A, &p, &mut ev);
            for y in 0..64 {
                for x in 0..width / 5 { assert_eq!(spatial.pixel(x, y), black.pixel(x, y), "{kind:?}"); }
                for x in (width - width / 5)..width { assert_eq!(spatial.pixel(x, y), white.pixel(x, y), "{kind:?}"); }
            }
            assert_ne!(spatial, black);
            assert_ne!(spatial, white);
            // Lowering the slider lowers the white end: the map is intensity, not an absolute value.
            let half = spec.default + 0.5 * (spec.max - spec.default);
            set(&mut g, effect, "amount", A, half);
            let half_spatial = run(&g, effect, A, &p, &mut ev);
            g.remove_exposed_input(effect, port);
            let half_scalar = run(&g, effect, A, &p, &mut ev);
            for y in 0..64 {
                for x in (width - width / 5)..width { assert_eq!(half_spatial.pixel(x, y), half_scalar.pixel(x, y), "{kind:?}"); }
            }
            let port = g.expose_parameter(effect, "amount", PortType::Images).unwrap();
            g.connect(map, effect, port).unwrap();
            set(&mut g, effect, "amount", A, spec.max);
            assert_eq!(ev.plan_tail(&g, effect, A, &p).unwrap().base, effect);
            let misses = ev.misses();
            set(&mut g, map, "gradient_reverse", A, 1.0);
            let reversed = run(&g, effect, A, &p, &mut ev);
            assert!(ev.misses() > misses);
            assert_ne!(spatial, reversed);
        }
    }

    #[test]
    fn individual_strokes_do_not_invalidate_other_images() {
        let p = TestProvider::default();
        let mut g = Graph::new();
        let src = add(&mut g, NodeKind::Source);
        let brush = add(&mut g, NodeKind::Draw);
        let exp = add(&mut g, NodeKind::Exposure);
        g.connect(src, exp, 0).unwrap();
        g.connect(src, brush, 0).unwrap();
        let ev = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(brush, exp, ev).unwrap();
        set(&mut g, exp, "ev", A, 1.0);
        set(&mut g, exp, "ev", B, 1.0);
        g.node_mut(brush).unwrap().mode = rawgraph_core::Mode::Individual;
        set_data(&mut g, brush, A, Some(NodeData::Strokes(vec![stroke()])));
        let mut other = stroke();
        other.points = vec![[0.8, 0.8], [0.9, 0.7]];
        set_data(&mut g, brush, B, Some(NodeData::Strokes(vec![other])));
        let mut ev = Evaluator::new();
        let a1 = run(&g, exp, A, &p, &mut ev);
        let b1 = run(&g, exp, B, &p, &mut ev);
        assert!(*a1 != *b1);
        let (hits, misses) = (ev.hits(), ev.misses());
        let mut changed = stroke();
        changed.radius = 0.3;
        set_data(&mut g, brush, A, Some(NodeData::Strokes(vec![changed])));
        let b2 = run(&g, exp, B, &p, &mut ev);
        assert!(Arc::ptr_eq(&b1, &b2));
        assert_eq!(ev.misses(), misses, "B untouched");
        assert!(ev.hits() > hits);
        let a2 = run(&g, exp, A, &p, &mut ev);
        assert!(*a1 != *a2);
    }

    #[test]
    fn lean_mode_matches_for_the_new_kinds() {
        let (mut g, _, _, out) = split_graph();
        let brush = add(&mut g, NodeKind::Draw);
        let curve = add(&mut g, NodeKind::PointCurve);
        let tail = add(&mut g, NodeKind::Exposure);
        let mg = g.input_edge(out, 0).unwrap().0;
        g.connect(mg, curve, 0).unwrap();
        g.connect(curve, tail, 0).unwrap();
        g.connect(mg, brush, 0).unwrap();
        let ev = g.expose_parameter(tail, "ev", PortType::Images).unwrap();
        g.connect(brush, tail, ev).unwrap();
        set(&mut g, tail, "ev", A, 0.5);
        set_data(&mut g, brush, A, Some(NodeData::Strokes(vec![stroke()])));
        set_data(&mut g, curve, A, Some(NodeData::Curve(vec![[0.0, 0.0], [0.5, 0.4], [1.0, 1.0]])));
        let p = TestProvider::default();
        set_iso(&p, A, 300.0);
        let (mut full, mut lean) = (Evaluator::new(), Evaluator::new());
        lean.set_keep_intermediates(false);
        let a = run(&g, tail, A, &p, &mut full);
        let b = run(&g, tail, A, &p, &mut lean);
        assert_eq!(*a, *b);
        assert_eq!(lean.cache.len(), 1);
    }

    // ---- wave 6: tail plan and node-data fingerprint ----

    fn plan(g: &Graph, n: NodeId, img: ImageId, p: &TestProvider) -> TailPlan {
        Evaluator::new().plan_tail(g, n, img, p).unwrap()
    }

    /// `apply_tail_cpu(eval_base(plan), plan.ops)` equals `eval_image(node)`, bit for bit.
    fn check_invariant(g: &Graph, n: NodeId, img: ImageId, p: &TestProvider) -> TailPlan {
        let mut ev = Evaluator::new();
        let c = CancelToken::new();
        let tp = ev.plan_tail(g, n, img, p).unwrap();
        let base = ev.eval_base(g, &tp, img, p, &c).unwrap();
        let tail = apply_tail_cpu(&base, &tp.ops);
        let full = Evaluator::new().eval_image(g, n, img, p, &c).unwrap();
        assert_eq!(tail, *full, "tail plan {tp:?} must reproduce the evaluation");
        tp
    }

    /// Source -> Develop -> Exposure -> Contrast -> HighlightsShadows -> Saturation -> Output.
    fn default_graph() -> (Graph, [NodeId; 7]) {
        let mut g = Graph::new();
        let ids = [
            add(&mut g, NodeKind::Source),
            add(&mut g, NodeKind::Develop),
            add(&mut g, NodeKind::Exposure),
            add(&mut g, NodeKind::Contrast),
            add(&mut g, NodeKind::HighlightsShadows),
            add(&mut g, NodeKind::Saturation),
            add(&mut g, NodeKind::Output),
        ];
        for w in ids.windows(2) {
            g.connect(w[0], w[1], 0).unwrap();
        }
        (g, ids)
    }

    #[test]
    fn plan_default_graph_omits_neutral_nodes() {
        let (mut g, [src, dev, exp, con, hs, sat, out]) = default_graph();
        let p = TestProvider::default();
        // Rendered / unknown source: Develop is neutral, so is everything else.
        let tp = plan(&g, out, A, &p);
        assert_eq!((tp.base, tp.base_port, tp.ops.len()), (src, 0, 0));
        // RAW input gain is already present in the source pixels.
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let tp = plan(&g, out, A, &p);
        assert_eq!(tp.base, src);
        assert!(tp.ops.is_empty());
        set(&mut g, exp, "ev", A, 1.0);
        set(&mut g, con, "amount", A, 0.4);
        set(&mut g, hs, "shadows", A, 0.3);
        set(&mut g, sat, "amount", A, 0.7);
        set(&mut g, dev, "wb", A, 1.0);
        set(&mut g, dev, "temperature", A, 3200.0);
        let tp = plan(&g, out, A, &p);
        let m = develop_settings(g.node(dev).unwrap(), A, p.capture(A).as_ref()).matrix.unwrap();
        let pivot = pv(&g, con, "pivot");
        assert_eq!(
            tp.ops,
            vec![
                TailOp::Matrix(m),
                TailOp::Gain(2.0),
                TailOp::Contrast { amount: 0.4, pivot },
                TailOp::HighlightsShadows { highlights: 0.0, shadows: 0.3 },
                TailOp::Saturation(0.7),
            ]
        );
        assert_eq!(p.calls(), 0);
        check_invariant(&g, out, A, &p);
    }

    #[test]
    fn plan_ends_at_non_pointwise_nodes() {
        let p = TestProvider::default();
        // Source -> Exposure -> Blur -> Contrast -> Saturation -> Output.
        let mut g = Graph::new();
        let [src, e1, blur, con, sat, out] = [
            NodeKind::Source,
            NodeKind::Exposure,
            NodeKind::Blur,
            NodeKind::Contrast,
            NodeKind::Saturation,
            NodeKind::Output,
        ]
        .map(|k| add(&mut g, k));
        for (a, b) in [(src, e1), (e1, blur), (blur, con), (con, sat), (sat, out)] {
            g.connect(a, b, 0).unwrap();
        }
        set(&mut g, e1, "ev", A, 1.0);
        set(&mut g, con, "amount", A, 0.5);
        set(&mut g, sat, "amount", A, 0.5);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!(tp.base, blur);
        assert_eq!(tp.ops.len(), 2);
        assert!(matches!(tp.ops[0], TailOp::Contrast { .. }) && tp.ops[1] == TailOp::Saturation(0.5));

        // Transform, Clarity, ... end the tail too (neutral defaults are still bases).
        // (ToneCurve, HueSaturation, Vignette and BlackWhite became tail-capable in wave 7.)
        for kind in [NodeKind::Transform, NodeKind::Clarity, NodeKind::NoiseReduction, NodeKind::Sharpen, NodeKind::LensManual] {
            let mut g = Graph::new();
            let src = add(&mut g, NodeKind::Source);
            let mid = add(&mut g, kind);
            let exp = add(&mut g, NodeKind::Exposure);
            g.connect(src, mid, 0).unwrap();
            g.connect(mid, exp, 0).unwrap();
            set(&mut g, exp, "ev", A, 1.0);
            let tp = check_invariant(&g, exp, A, &p);
            assert_eq!((tp.base, tp.ops.clone()), (mid, vec![TailOp::Gain(2.0)]), "{kind:?}");
        }
    }

    #[test]
    fn plan_masked_exposure_is_the_base_unmasked_is_not() {
        let p = TestProvider::default();
        let mut g = Graph::new();
        let mask = add(&mut g, NodeKind::Generator);
        let src = add(&mut g, NodeKind::Source);
        let masked_exp = add(&mut g, NodeKind::Exposure);
        let port = g.expose_parameter(masked_exp, "ev", PortType::Images).unwrap();
        g.connect(src, masked_exp, 0).unwrap();
        g.connect(mask, masked_exp, port).unwrap();
        let sat = add(&mut g, NodeKind::Saturation);
        g.connect(masked_exp, sat, 0).unwrap();
        set(&mut g, masked_exp, "ev", A, 1.0);
        set(&mut g, sat, "amount", A, 0.5);
        let tp = check_invariant(&g, sat, A, &p);
        assert_eq!((tp.base, tp.ops), (masked_exp, vec![TailOp::Saturation(0.5)]));

        g.disconnect(masked_exp, port);
        let tp = check_invariant(&g, sat, A, &p);
        assert_eq!((tp.base, tp.ops), (src, vec![TailOp::Gain(2.0), TailOp::Saturation(0.5)]));
    }

    #[test]
    fn plan_follows_filter_split_and_merge() {
        // Filter + merge: ISO 200 takes the exposure branch, ISO 3000 the saturation branch.
        let (g, _, _, exp, out) = merge_graph();
        let p = TestProvider::default();
        let (ia, ib) = (ImageId(10), ImageId(11));
        set_iso(&p, ia, 200.0);
        set_iso(&p, ib, 3000.0);
        let src = g.input_edge(g.input_edge(exp, 0).unwrap().0, 0).unwrap().0;
        let tp = check_invariant(&g, out, ia, &p);
        assert_eq!((tp.base, tp.ops), (src, vec![TailOp::Gain(2.0)]));
        let tp = check_invariant(&g, out, ib, &p);
        assert_eq!((tp.base, tp.ops), (src, vec![TailOp::Saturation(0.2)]));

        // Split: each range feeds a different branch; every port is followed.
        let (g, sp, [_, _, con], out) = split_graph();
        let p = TestProvider::default();
        let (lo, mid, hi) = (ImageId(10), ImageId(11), ImageId(12));
        for (i, iso) in [(lo, 200.0), (mid, 800.0), (hi, 6400.0)] {
            set_iso(&p, i, iso);
        }
        let pivot = pv(&g, con, "pivot");
        let src = g.input_edge(sp, 0).unwrap().0;
        for (img, ops) in [
            (lo, vec![TailOp::Gain(2.0)]),
            (mid, vec![TailOp::Saturation(0.2)]),
            (hi, vec![TailOp::Contrast { amount: 0.5, pivot }]),
        ] {
            let tp = check_invariant(&g, out, img, &p);
            assert_eq!((tp.base, tp.ops), (src, ops), "{img:?}");
            // Viewing the split itself passes the input through.
            let tp = check_invariant(&g, sp, img, &p);
            assert_eq!((tp.base, tp.ops.len()), (src, 0));
        }
    }

    #[test]
    fn plan_for_a_node_behind_a_split_port() {
        let (g, sp, [exp, sat, _], _) = split_graph();
        let p = TestProvider::default();
        set_iso(&p, A, 200.0);
        let tp = check_invariant(&g, exp, A, &p);
        assert_eq!(tp.ops, vec![TailOp::Gain(2.0)]);
        assert_eq!(tp.base, g.input_edge(sp, 0).unwrap().0);
        assert_eq!(Evaluator::new().plan_tail(&g, sat, A, &p).unwrap_err(), EvalError::NotInCollection { node: sat });
    }

    /// Mixed chain exercising every op, in an order that differs from the default one.
    fn all_ops_graph(g: &mut Graph) -> NodeId {
        let kinds = [
            NodeKind::Source,
            NodeKind::HighlightsShadows,
            NodeKind::WhiteBalance,
            NodeKind::CaptureEv,
            NodeKind::Saturation,
            NodeKind::Develop,
            NodeKind::Contrast,
            NodeKind::Exposure,
            NodeKind::Output,
        ];
        let ids: Vec<NodeId> = kinds.iter().map(|k| add(g, *k)).collect();
        for w in ids.windows(2) {
            g.connect(w[0], w[1], 0).unwrap();
        }
        set(g, ids[1], "highlights", A, -0.6);
        set(g, ids[1], "shadows", A, 0.4);
        set(g, ids[2], "temp", A, 40.0);
        set(g, ids[2], "tint", A, -10.0);
        set(g, ids[3], "delta", A, -0.7);
        set(g, ids[4], "amount", A, 1.4);
        set(g, ids[5], "wb", A, 1.0);
        set(g, ids[5], "temperature", A, 4300.0);
        set(g, ids[6], "amount", A, -0.3);
        set(g, ids[7], "ev", A, 0.8);
        *ids.last().unwrap()
    }

    #[test]
    fn invariant_every_op_and_develop_variants() {
        let p = TestProvider::default();
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let mut g = Graph::new();
        let out = all_ops_graph(&mut g);
        let tp = check_invariant(&g, out, A, &p);
        // HS, WB matrix, CaptureEv gain, Saturation, Develop matrix, Contrast, Exposure gain.
        assert_eq!(tp.ops.len(), 7, "{:?}", tp.ops);
        for kind in ["gain", "matrix", "sat", "contrast", "hs"] {
            let found = tp.ops.iter().any(|o| match o {
                TailOp::Gain(_) => kind == "gain",
                TailOp::Matrix(_) => kind == "matrix",
                TailOp::Saturation(_) => kind == "sat",
                TailOp::Contrast { .. } => kind == "contrast",
                TailOp::HighlightsShadows { .. } => kind == "hs",
                _ => false,
            });
            assert!(found, "{kind} missing");
        }

        // The same graph under every capture situation of Develop.
        for cap in [
            None,
            Some(raw_capture(Some(5200.0), None)),
            Some(raw_capture(None, Some(-0.3))),
            Some(CaptureInfo { raw: Some(false), baseline_exposure: Some(0.5), ..Default::default() }),
            Some(raw_capture(Some(7000.0), Some(1.0))),
        ] {
            match cap {
                Some(c) => p.caps.lock().unwrap().insert(A.0, c),
                None => p.caps.lock().unwrap().remove(&A.0),
            };
            check_invariant(&g, out, A, &p);
        }

        // Develop on its own: file baseline, default baseline, rendered source, custom white balance.
        let (mut g, src, dev) = one(NodeKind::Develop);
        for (cap, expect_ops) in [
            (raw_capture(Some(5200.0), Some(0.5)), 0usize),
            (raw_capture(Some(5200.0), None), 0),
            (CaptureInfo { raw: Some(false), ..Default::default() }, 0),
        ] {
            p.caps.lock().unwrap().insert(A.0, cap);
            let tp = check_invariant(&g, dev, A, &p);
            assert_eq!((tp.base, tp.ops.len()), (src, expect_ops));
        }
        set(&mut g, dev, "wb", A, 1.0);
        set(&mut g, dev, "temperature", A, 3200.0);
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let tp = check_invariant(&g, dev, A, &p);
        assert!(matches!(tp.ops.as_slice(), [TailOp::Matrix(_)]), "{:?}", tp.ops);
        set(&mut g, dev, "brightness", A, 0.0);
        set(&mut g, dev, "baseline", A, 0.0);
        let tp = check_invariant(&g, dev, A, &p);
        assert!(matches!(tp.ops.as_slice(), [TailOp::Matrix(_)]), "{:?}", tp.ops);
    }

    #[test]
    fn invariant_individual_mode_gives_each_image_its_own_ops() {
        let p = TestProvider::default();
        let (mut g, [_, exp, sat, out]) = chain();
        g.node_mut(exp).unwrap().mode = rawgraph_core::Mode::Individual;
        set(&mut g, exp, "ev", A, 1.0);
        set(&mut g, exp, "ev", B, -2.0);
        set(&mut g, sat, "amount", A, 0.5);
        let (ta, tb, tc) = (check_invariant(&g, out, A, &p), check_invariant(&g, out, B, &p), check_invariant(&g, out, C, &p));
        assert_eq!(ta.ops, vec![TailOp::Gain(2.0), TailOp::Saturation(0.5)]);
        assert_eq!(tb.ops, vec![TailOp::Gain(0.25), TailOp::Saturation(0.5)]);
        assert_eq!(tc.ops, vec![TailOp::Saturation(0.5)]);
        assert_ne!(ta.base_fingerprint, tb.base_fingerprint, "different images, different base identity");
    }

    #[test]
    fn invariant_on_blur_masks_crops_split_merge_and_empty_tails() {
        let p = TestProvider::default();
        // Source -> Output: empty tail.
        let mut g = Graph::new();
        let (src, out) = (add(&mut g, NodeKind::Source), add(&mut g, NodeKind::Output));
        g.connect(src, out, 0).unwrap();
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!((tp.base, tp.ops.len()), (src, 0));
        // Target is the base itself.
        let tp = check_invariant(&g, src, A, &p);
        assert_eq!((tp.base, tp.ops.len()), (src, 0));

        // Masked blur, a luminance-masked contrast, then an exposure tail.
        let mut g = Graph::new();
        let s = add(&mut g, NodeKind::Source);
        let gm = add(&mut g, NodeKind::Generator);
        let blur = add(&mut g, NodeKind::Blur);
        let hs = add(&mut g, NodeKind::HighlightsShadows);
        let con = add(&mut g, NodeKind::Contrast);
        let exp = add(&mut g, NodeKind::Exposure);
        g.connect(s, blur, 0).unwrap();
        let radius = g.expose_parameter(blur, "radius", PortType::Images).unwrap();
        g.connect(gm, blur, radius).unwrap();
        g.connect(blur, hs, 0).unwrap();
        g.connect(hs, con, 0).unwrap();
        let amount = g.expose_parameter(con, "amount", PortType::Images).unwrap();
        g.connect(hs, con, amount).unwrap();
        g.connect(con, exp, 0).unwrap();
        set(&mut g, blur, "radius", A, 20.0);
        set(&mut g, hs, "highlights", A, -0.5);
        set(&mut g, con, "amount", A, 0.6);
        set(&mut g, exp, "ev", A, 0.5);
        let tp = check_invariant(&g, exp, A, &p);
        assert_eq!((tp.base, tp.ops), (con, vec![TailOp::Gain(2f32.powf(0.5))]));
        let tp = check_invariant(&g, hs, A, &p);
        assert_eq!(tp.base, blur);
        assert_eq!(tp.ops.len(), 1);

        // A crop in the middle changes the frame size; the tail still matches.
        let mut g = Graph::new();
        let (_, t) = cropped(&mut g);
        let sat = add(&mut g, NodeKind::Saturation);
        g.connect(t, sat, 0).unwrap();
        set(&mut g, sat, "amount", A, 0.1);
        let tp = check_invariant(&g, sat, A, &p);
        assert_eq!((tp.base, tp.ops), (t, vec![TailOp::Saturation(0.1)]));

        // A tail after a merge inside a split graph.
        let (mut g, _, _, out) = split_graph();
        let extra = add(&mut g, NodeKind::Exposure);
        g.connect(out, extra, 0).unwrap_err(); // Output has no output port
        let merge = g.input_edge(out, 0).unwrap().0;
        g.connect(merge, extra, 0).unwrap();
        set(&mut g, extra, "ev", A, -1.0);
        set_iso(&p, A, 800.0);
        let tp = check_invariant(&g, extra, A, &p);
        assert_eq!(tp.ops, vec![TailOp::Saturation(0.2), TailOp::Gain(0.5)]);
    }

    #[test]
    fn plan_errors_match_eval_and_never_load_pixels() {
        let (p, q) = (TestProvider::default(), TestProvider::default());
        let ev = Evaluator::new();
        let mut real = Evaluator::new();
        let c = CancelToken::new();
        let mut g = Graph::new();
        let exp = add(&mut g, NodeKind::Exposure);
        assert_eq!(ev.plan_tail(&g, NodeId(999), A, &p).unwrap_err(), EvalError::UnknownNode(NodeId(999)));
        assert_eq!(ev.plan_tail(&g, exp, A, &p).unwrap_err(), real.eval(&g, exp, A, &q, &c).unwrap_err());
        assert_eq!(ev.plan_tail(&g, exp, A, &p).unwrap_err(), EvalError::MissingInput { node: exp, port: 0 });

        let (g2, fa, _, exp2, out2) = merge_graph();
        set_iso(&p, B, 9000.0);
        set_iso(&q, B, 9000.0);
        for n in [fa, exp2, out2] {
            let e = ev.plan_tail(&g2, n, B, &p).unwrap_err();
            assert_eq!(e, real.eval(&g2, n, B, &q, &c).unwrap_err());
            assert!(matches!(e, EvalError::NotInCollection { .. }));
        }
        // A required input missing further upstream is reported like eval does.
        let mut g3 = Graph::new();
        let (sat, out) = (add(&mut g3, NodeKind::Saturation), add(&mut g3, NodeKind::Output));
        g3.connect(sat, out, 0).unwrap();
        assert_eq!(ev.plan_tail(&g3, out, A, &p).unwrap_err(), real.eval(&g3, out, A, &q, &c).unwrap_err());
        assert_eq!(p.calls(), 0, "plan_tail never calls provider.image()");
        assert_eq!(real.misses(), 0);
    }

    #[test]
    fn base_fingerprint_follows_the_base_not_the_tail() {
        let p = TestProvider::default();
        // Source -> Develop -> Blur -> Exposure -> Saturation.
        let mut g = Graph::new();
        let [src, dev, blur, exp, sat] =
            [NodeKind::Source, NodeKind::Develop, NodeKind::Blur, NodeKind::Exposure, NodeKind::Saturation].map(|k| add(&mut g, k));
        for (a, b) in [(src, dev), (dev, blur), (blur, exp), (exp, sat)] {
            g.connect(a, b, 0).unwrap();
        }
        set(&mut g, exp, "ev", A, 1.0);
        let fp0 = plan(&g, sat, A, &p).base_fingerprint;
        let fp_of = |g: &Graph, img: ImageId, p: &TestProvider| plan(g, sat, img, p).base_fingerprint;

        // Tail parameters: no change, so nothing is re-uploaded.
        set(&mut g, exp, "ev", A, 2.0);
        set(&mut g, sat, "amount", A, 0.3);
        let tp = plan(&g, sat, A, &p);
        assert_eq!(tp.base_fingerprint, fp0);
        assert_eq!(tp.base, blur);
        // Base parameters, provider revision and image identity do change it.
        set(&mut g, blur, "radius", A, 9.0);
        let fp1 = fp_of(&g, A, &p);
        assert_ne!(fp1, fp0);
        p.revision.store(3, Ordering::Relaxed);
        let fp2 = fp_of(&g, A, &p);
        assert_ne!(fp2, fp1);
        assert_ne!(fp_of(&g, B, &p), fp2);
        // Capture metadata that the Develop below the base reads.
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let fp3 = fp_of(&g, A, &p);
        assert_ne!(fp3, fp2);
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.7)));
        assert_ne!(fp_of(&g, A, &p), fp3);
        assert_eq!(fp_of(&g, A, &p), fp_of(&g, A, &p), "deterministic");

        // Capture metadata read by a Develop in the tail changes the ops, not the base.
        let mut g2 = Graph::new();
        let [s2, b2, d2] = [NodeKind::Source, NodeKind::Blur, NodeKind::Develop].map(|k| add(&mut g2, k));
        g2.connect(s2, b2, 0).unwrap();
        g2.connect(b2, d2, 0).unwrap();
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let before = plan(&g2, d2, A, &p);
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(1.5)));
        let after = plan(&g2, d2, A, &p);
        assert_eq!(before.base_fingerprint, after.base_fingerprint);
        assert_eq!(before.ops, after.ops);
    }

    #[test]
    fn base_fingerprint_is_the_cache_fingerprint() {
        let p = TestProvider::default();
        // Source -> Exposure -> Blur -> Saturation -> Output.
        let (mut g, [_, exp, sat, out]) = chain();
        let blur = add(&mut g, NodeKind::Blur);
        g.connect(exp, blur, 0).unwrap();
        g.disconnect(sat, 0);
        g.connect(blur, sat, 0).unwrap();
        set(&mut g, exp, "ev", A, 1.0);
        set(&mut g, sat, "amount", A, 0.4);
        let (mut ev, c) = (Evaluator::new(), CancelToken::new());

        let tp = ev.plan_tail(&g, out, A, &p).unwrap();
        assert_eq!(tp.base, blur);
        let first = ev.eval_base(&g, &tp, A, &p, &c).unwrap();
        assert_eq!(ev.cache.get(&(blur, A, false)).unwrap().fingerprint, tp.base_fingerprint);
        // eval_base uses the cache.
        let misses = ev.misses();
        let again = ev.eval_base(&g, &tp, A, &p, &c).unwrap();
        assert_eq!(ev.misses(), misses);
        assert!(Arc::ptr_eq(&first, &again));
        // A tail change: same fingerprint, still cached.
        set(&mut g, sat, "amount", A, 0.9);
        let tp2 = ev.plan_tail(&g, out, A, &p).unwrap();
        assert_eq!(tp2.base_fingerprint, tp.base_fingerprint);
        assert!(Arc::ptr_eq(&first, &ev.eval_base(&g, &tp2, A, &p, &c).unwrap()));
        assert_eq!(ev.misses(), misses);
        // A base change: new fingerprint, recomputed (the blur only).
        set(&mut g, blur, "radius", A, 12.0);
        let tp3 = ev.plan_tail(&g, out, A, &p).unwrap();
        assert_ne!(tp3.base_fingerprint, tp.base_fingerprint);
        ev.eval_base(&g, &tp3, A, &p, &c).unwrap();
        assert_eq!(ev.misses(), misses + 1);
        assert_eq!(ev.cache.get(&(blur, A, false)).unwrap().fingerprint, tp3.base_fingerprint);
        // A revision bump recomputes the whole base chain, and the plan sees it.
        p.revision.store(7, Ordering::Relaxed);
        let tp4 = ev.plan_tail(&g, out, A, &p).unwrap();
        assert_ne!(tp4.base_fingerprint, tp3.base_fingerprint);
        let m = ev.misses();
        ev.eval_base(&g, &tp4, A, &p, &c).unwrap();
        assert_eq!(ev.misses() - m, 3, "source, exposure, blur");
    }

    #[test]
    fn eval_base_respects_cancellation() {
        let p = TestProvider::default();
        let (g, [.., out]) = chain();
        let mut ev = Evaluator::new();
        let tp = ev.plan_tail(&g, out, A, &p).unwrap();
        let tok = CancelToken::new();
        tok.cancel();
        assert_eq!(ev.eval_base(&g, &tp, A, &p, &tok).unwrap_err(), EvalError::Cancelled);
        assert_eq!(ev.misses(), 0);
        assert_eq!(p.calls(), 0);
        assert!(ev.eval_base(&g, &tp, A, &p, &CancelToken::new()).is_ok());
    }

    #[test]
    fn node_data_fingerprint_follows_content() {
        let hash = |x: Option<&NodeData>| {
            let mut h = DefaultHasher::new();
            hash_node_data(x, &mut h);
            h.finish()
        };
        let d = |s: &Vec<rawgraph_core::BrushStroke>| hash(Some(&NodeData::Strokes(s.clone())));
        let mut a = vec![stroke(), stroke()];
        let base = d(&a);
        assert_eq!(base, d(&a.clone()), "content, not address");
        a[1].points[1][0] += 1e-4;
        assert_ne!(d(&a), base);
        a[1].points[1][0] -= 1e-4;
        a[0].erase = true;
        assert_ne!(d(&a), base);
        a[0].erase = false;
        assert_eq!(d(&a), base);
        // Splitting the same points into strokes differently must not collide.
        let joined = vec![rawgraph_core::BrushStroke { points: vec![[0.1, 0.1]; 4], ..stroke() }];
        let split = vec![
            rawgraph_core::BrushStroke { points: vec![[0.1, 0.1]; 2], ..stroke() },
            rawgraph_core::BrushStroke { points: vec![[0.1, 0.1]; 2], ..stroke() },
        ];
        assert_ne!(d(&joined), d(&split));
        let curve = NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]]);
        assert_ne!(hash(None), hash(Some(&curve)));
        assert_ne!(hash(Some(&ranges(&[("a", 1.0, 2.0)]))), hash(Some(&ranges(&[("b", 1.0, 2.0)]))));
        assert_ne!(hash(Some(&ranges(&[("a", 1.0, 2.0)]))), hash(Some(&ranges(&[("a", 1.0, 3.0)]))));
    }

    /// The pre-wave-6 implementation, kept for the timing comparison only.
    fn old_hash_node_data(data: Option<&NodeData>, hasher: &mut DefaultHasher) {
        match data {
            None => 0u8.hash(hasher),
            Some(d) => {
                1u8.hash(hasher);
                format!("{d:?}").hash(hasher);
            }
        }
    }

    #[test]
    #[ignore = "timing report: run with --release -- --ignored --nocapture"]
    fn node_data_fingerprint_timing() {
        let strokes: Vec<rawgraph_core::BrushStroke> = (0..200)
            .map(|s| rawgraph_core::BrushStroke {
                points: (0..500).map(|i| [(i as f32 * 0.00173 + s as f32 * 0.003) % 1.0, (i as f32 * 0.00091) % 1.0]).collect(),
                radius: 0.05,
                hardness: 0.5,
                flow: 1.0,
                erase: s % 7 == 0,
            })
            .collect();
        let data = NodeData::Strokes(strokes);
        let measure = |f: &dyn Fn() -> u64| {
            let mut times: Vec<std::time::Duration> = (0..20)
                .map(|_| {
                    let t = std::time::Instant::now();
                    std::hint::black_box(f());
                    t.elapsed()
                })
                .collect();
            times.sort();
            (times[0], times[10])
        };
        let old = measure(&|| {
            let mut h = DefaultHasher::new();
            old_hash_node_data(Some(std::hint::black_box(&data)), &mut h);
            h.finish()
        });
        let new = measure(&|| {
            let mut h = DefaultHasher::new();
            hash_node_data(Some(std::hint::black_box(&data)), &mut h);
            h.finish()
        });
        // Whole node: Draw driving Exposure, fingerprint of the consumer.
        let mut g = Graph::new();
        let src = add(&mut g, NodeKind::Source);
        let brush = add(&mut g, NodeKind::Draw);
        let exp = add(&mut g, NodeKind::Exposure);
        g.connect(src, exp, 0).unwrap();
        g.connect(src, brush, 0).unwrap();
        let ev = g.expose_parameter(exp, "ev", PortType::Images).unwrap();
        g.connect(brush, exp, ev).unwrap();
        set_data(&mut g, brush, A, Some(data.clone()));
        let p = TestProvider::default();
        let whole = measure(&|| fingerprint_of(&g, exp, A, &p));
        println!("hash of 200 strokes x 500 points (min / median of 20 runs)");
        println!("  old (format!(\"{{:?}}\")): {:?} / {:?}", old.0, old.1);
        println!("  new (direct bits)      : {:?} / {:?}", new.0, new.1);
        println!("  whole node_fingerprint : {:?} / {:?}", whole.0, whole.1);
        assert!(new.1 < old.1);
    }

    // ---- wave 7: ColorWheels, Clarity, ISO-adaptive noise reduction, more tail operations ----

    type Spec<'a> = (NodeKind, &'a [(&'a str, f32)]);

    /// Source -> specs... -> Output; parameters are set for image `A` (and as defaults).
    fn build<'a>(specs: &[Spec<'a>]) -> (Graph, Vec<NodeId>, NodeId) {
        let mut g = Graph::new();
        let mut ids = vec![add(&mut g, NodeKind::Source)];
        for (kind, params) in specs {
            let n = add(&mut g, *kind);
            g.connect(*ids.last().unwrap(), n, 0).unwrap();
            for (name, v) in *params {
                set(&mut g, n, name, A, *v);
            }
            ids.push(n);
        }
        let out = add(&mut g, NodeKind::Output);
        g.connect(*ids.last().unwrap(), out, 0).unwrap();
        (g, ids, out)
    }

    fn curve3() -> NodeData {
        NodeData::Curve(vec![[0.0, 0.0], [0.3, 0.45], [0.7, 0.62], [1.0, 1.0]])
    }

    const WHEELS: &[(&str, f32)] = &[("lift", 0.1), ("lift_b", -0.05), ("gamma_r", 0.3), ("gain", 0.4), ("gain_g", -0.2)];

    #[test]
    fn color_wheels_matches_fx_and_identity() {
        let (mut g, src, n) = one(NodeKind::ColorWheels);
        let (p, mut ev) = (TestProvider::default(), Evaluator::new());
        let base = run(&g, src, A, &p, &mut ev);
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)), "identity returns the input Arc");
        for (name, v) in WHEELS {
            set(&mut g, n, name, A, *v);
        }
        let mut expect = (*base).clone();
        fx::color_wheels(&mut expect, &cdl_params(g.node(n).unwrap(), A));
        let got = run(&g, n, A, &p, &mut ev);
        assert_eq!(*got, expect);
        assert!(*got != *base);
    }

    #[test]
    fn clarity_matches_fx_and_identity() {
        let (mut g, src, n) = one(NodeKind::Clarity);
        let (p, mut ev) = (TestProvider::default(), Evaluator::new());
        p.texture.store(true, Ordering::Relaxed);
        let base = run(&g, src, A, &p, &mut ev);
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)), "both 0 returns the input Arc");
        for (c, t) in [(0.6, 0.0), (0.0, -0.5), (0.4, 0.7)] {
            set(&mut g, n, "clarity", A, c);
            set(&mut g, n, "texture", A, t);
            let expect = fx::local_contrast(&base, c, t);
            assert_eq!(*run(&g, n, A, &p, &mut ev), expect, "clarity {c} texture {t}");
        }
    }

    #[test]
    fn iso_adaptive_scales_strengths_and_never_serves_stale() {
        let (mut g, src, n) = one(NodeKind::NoiseReduction);
        set(&mut g, n, "luma", A, 0.3);
        set(&mut g, n, "chroma", A, 0.2);
        set(&mut g, n, "radius", A, 2.0);
        let (p, mut ev) = (TestProvider::default(), Evaluator::new());
        let base = run(&g, src, A, &p, &mut ev);
        let direct = |luma: f32, chroma: f32| ops::noise_reduction(&base, luma, chroma, 2.0 / 1000.0 * 64.0);

        // Toggle off: ISO is ignored, exactly the old behaviour.
        set_iso(&p, A, 10000.0);
        assert_eq!(*run(&g, n, A, &p, &mut ev), direct(0.3, 0.2));
        set(&mut g, n, "iso_adaptive", A, 1.0);
        assert!(g.node(n).unwrap().toggle("iso_adaptive", A));

        for (iso, scale) in [(100.0f32, 0.25f32), (2500.0, 1.0), (10000.0, 2.0)] {
            set_iso(&p, A, iso); // same graph, new provider ISO: must recompute
            let expect = direct((0.3 * scale).clamp(0.0, 1.0), (0.2 * scale).clamp(0.0, 1.0));
            assert_eq!(*run(&g, n, A, &p, &mut ev), expect, "ISO {iso}");
        }
        // Unknown ISO: factor 1.
        p.iso.lock().unwrap().clear();
        assert_eq!(*run(&g, n, A, &p, &mut ev), direct(0.3, 0.2));
        // Clamped back into 0..1: 0.8 * 2.0 -> 1.0.
        set(&mut g, n, "luma", A, 0.8);
        set_iso(&p, A, 10000.0);
        assert_eq!(*run(&g, n, A, &p, &mut ev), direct(1.0, 0.4));
        // Toggle off again returns to the unscaled result, with the ISO still set.
        set(&mut g, n, "iso_adaptive", A, 0.0);
        assert_eq!(*run(&g, n, A, &p, &mut ev), direct(0.8, 0.2));
    }

    #[test]
    fn iso_adaptive_fingerprint_follows_iso_only_when_on() {
        let (mut g, _, n) = one(NodeKind::NoiseReduction);
        set(&mut g, n, "luma", A, 0.5);
        let p = TestProvider::default();
        let fp = |g: &Graph, p: &TestProvider| fingerprint_of(g, n, A, p);
        set_iso(&p, A, 400.0);
        let off = fp(&g, &p);
        set_iso(&p, A, 6400.0);
        assert_eq!(fp(&g, &p), off, "off: ISO is not part of the fingerprint");
        set(&mut g, n, "iso_adaptive", A, 1.0);
        let on = fp(&g, &p);
        assert_ne!(on, off);
        set_iso(&p, A, 400.0);
        assert_ne!(fp(&g, &p), on);
        p.iso.lock().unwrap().clear();
        assert_ne!(fp(&g, &p), on);
    }

    #[test]
    fn tail_kinds_match_direct_fx_calls() {
        let p = TestProvider::default();
        let base = p.image(A).unwrap();
        let eval = |specs: &[Spec]| {
            let (g, _, out) = build(specs);
            let (mut ev, c) = (Evaluator::new(), CancelToken::new());
            ev.eval_image(&g, out, A, &p, &c).unwrap()
        };
        let mut expect = (*base).clone();
        fx::tone_curve(&mut expect, 0.3, -0.2, 0.1, -0.4);
        assert_eq!(*eval(&[(NodeKind::ToneCurve, &[("shadows", 0.3), ("darks", -0.2), ("lights", 0.1), ("highlights", -0.4)])]), expect);

        let params: Vec<(String, f32)> = [("red", 0.0f32), ("yellow", 0.0), ("green", 0.0), ("cyan", 0.0), ("blue", 0.0), ("magenta", 0.0)]
            .iter()
            .enumerate()
            .flat_map(|(i, (n, _))| {
                let v = (i as f32 - 2.0) * 0.15;
                [(format!("{n}_h"), v), (format!("{n}_s"), -v), (format!("{n}_l"), v * 0.5)]
            })
            .collect();
        let refs: Vec<(&str, f32)> = params.iter().map(|(n, v)| (n.as_str(), *v)).collect();
        let mut bands = [[0.0f32; 3]; 6];
        for (i, b) in bands.iter_mut().enumerate() {
            let v = (i as f32 - 2.0) * 0.15;
            *b = [v, -v, v * 0.5];
        }
        let mut expect = (*base).clone();
        fx::hue_saturation(&mut expect, &bands);
        assert_eq!(*eval(&[(NodeKind::HueSaturation, &refs)]), expect);

        let mut expect = (*base).clone();
        fx::vignette(&mut expect, -0.6, 0.4, 0.3);
        assert_eq!(*eval(&[(NodeKind::Vignette, &[("amount", -0.6), ("midpoint", 0.4), ("feather", 0.3)])]), expect);

        let mut expect = (*base).clone();
        fx::black_white(&mut expect, [0.5, 0.3, 0.2]);
        assert_eq!(*eval(&[(NodeKind::BlackWhite, &[("red", 0.5), ("green", 0.3), ("blue", 0.2)])]), expect);

        // Point curve per channel.
        for (idx, ch) in [fx::CurveChannel::Luminance, fx::CurveChannel::Red, fx::CurveChannel::Green, fx::CurveChannel::Blue].into_iter().enumerate() {
            let (mut g, ids, out) = build(&[(NodeKind::PointCurve, &[("channel", idx as f32)])]);
            set_data(&mut g, ids[1], A, Some(curve3()));
            let got = Evaluator::new().eval_image(&g, out, A, &p, &CancelToken::new()).unwrap();
            let mut expect = (*base).clone();
            fx::point_curve(&mut expect, &[[0.0, 0.0], [0.3, 0.45], [0.7, 0.62], [1.0, 1.0]], ch);
            assert_eq!(*got, expect, "channel {idx}");
        }
    }

    #[test]
    fn new_kinds_identity_short_circuits() {
        let p = TestProvider::default();
        let (mut ev, c) = (Evaluator::new(), CancelToken::new());
        for kind in [NodeKind::ColorWheels, NodeKind::Clarity, NodeKind::ToneCurve, NodeKind::HueSaturation, NodeKind::Vignette, NodeKind::PointCurve] {
            let (g, src, n) = one(kind);
            let (s, r) = (ev.eval_image(&g, src, A, &p, &c).unwrap(), ev.eval_image(&g, n, A, &p, &c).unwrap());
            assert!(Arc::ptr_eq(&s, &r), "{kind:?}");
        }
        // Noise reduction with the toggle on but zero strengths stays an identity.
        let (mut g, src, n) = one(NodeKind::NoiseReduction);
        set(&mut g, n, "iso_adaptive", A, 1.0);
        set_iso(&p, A, 10000.0);
        let (s, r) = (ev.eval_image(&g, src, A, &p, &c).unwrap(), ev.eval_image(&g, n, A, &p, &c).unwrap());
        assert!(Arc::ptr_eq(&s, &r));
    }

    #[test]
    fn plan_shapes_of_the_new_tail_kinds() {
        let p = TestProvider::default();
        // Neutral defaults contribute nothing (BlackWhite has no neutral case).
        for kind in [NodeKind::ColorWheels, NodeKind::ToneCurve, NodeKind::PointCurve, NodeKind::HueSaturation, NodeKind::Vignette] {
            let (g, ids, out) = build(&[(kind, &[])]);
            let tp = check_invariant(&g, out, A, &p);
            assert_eq!((tp.base, tp.ops.len()), (ids[0], 0), "{kind:?}");
        }
        // ColorWheels.
        let (g, ids, out) = build(&[(NodeKind::ColorWheels, WHEELS)]);
        let cdl = cdl_params(g.node(ids[1]).unwrap(), A);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!(tp.ops, vec![TailOp::Cdl { slope: cdl.slope, offset: cdl.offset, power: cdl.power }]);
        // ToneCurve.
        let (g, _, out) = build(&[(NodeKind::ToneCurve, &[("shadows", 0.2), ("highlights", -0.1)])]);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!(tp.ops, vec![TailOp::ToneCurve { shadows: 0.2, darks: 0.0, lights: 0.0, highlights: -0.1 }]);
        // PointCurve with a channel.
        let (mut g, ids, out) = build(&[(NodeKind::PointCurve, &[("channel", 2.0)])]);
        set_data(&mut g, ids[1], A, Some(curve3()));
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!(tp.ops, vec![TailOp::PointCurve { points: vec![[0.0, 0.0], [0.3, 0.45], [0.7, 0.62], [1.0, 1.0]], channel: 2 }]);
        // The straight diagonal is neutral.
        set_data(&mut g, ids[1], A, Some(NodeData::Curve(vec![[0.0, 0.0], [1.0, 1.0]])));
        assert!(plan(&g, out, A, &p).ops.is_empty());
        // HueSaturation.
        let (g, _, out) = build(&[(NodeKind::HueSaturation, &[("green_s", 0.5), ("blue_h", -0.25)])]);
        let tp = check_invariant(&g, out, A, &p);
        let mut bands = [[0.0f32; 3]; 6];
        bands[2][1] = 0.5;
        bands[4][0] = -0.25;
        assert_eq!(tp.ops, vec![TailOp::HueSaturation { bands }]);
        // Vignette.
        let (g, _, out) = build(&[(NodeKind::Vignette, &[("amount", -0.5), ("midpoint", 0.3), ("feather", 0.6)])]);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!(tp.ops, vec![TailOp::Vignette { amount: -0.5, midpoint: 0.3, feather: 0.6 }]);
        // BlackWhite -> Matrix with three identical rows, even with the default weights.
        let (g, ids, out) = build(&[(NodeKind::BlackWhite, &[("red", 0.4), ("green", 0.4), ("blue", 0.2)])]);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!((tp.base, tp.ops), (ids[0], vec![TailOp::Matrix([[0.4, 0.4, 0.2]; 3])]));
        let (g, _, out) = build(&[(NodeKind::BlackWhite, &[])]);
        assert_eq!(plan(&g, out, A, &p).ops.len(), 1);
        // Clarity and an ISO-adaptive NoiseReduction end the tail.
        for kind in [NodeKind::Clarity, NodeKind::NoiseReduction] {
            let (g, ids, out) = build(&[(kind, &[]), (NodeKind::ToneCurve, &[("lights", 0.3)])]);
            let tp = check_invariant(&g, out, A, &p);
            assert_eq!((tp.base, tp.ops.len()), (ids[1], 1), "{kind:?}");
        }
    }

    #[test]
    fn invariant_holds_for_the_new_operations() {
        let p = TestProvider::default();
        let all_bands: Vec<(&str, f32)> = vec![("red_s", 0.4), ("yellow_h", 0.2), ("green_l", -0.3), ("cyan_s", -0.5), ("blue_h", -0.4), ("magenta_l", 0.3)];
        let curve_graph = |channel: f32| {
            let (mut g, ids, out) = build(&[(NodeKind::Exposure, &[("ev", 0.5)]), (NodeKind::PointCurve, &[("channel", channel)]), (NodeKind::Vignette, &[("amount", 0.5)])]);
            set_data(&mut g, ids[2], A, Some(curve3()));
            (g, out)
        };
        let mut graphs: Vec<(Graph, NodeId)> = Vec::new();
        for specs in [
            // 1: just CDL
            vec![(NodeKind::ColorWheels, WHEELS)],
            // 2: old and new mixed
            vec![(NodeKind::Exposure, &[("ev", 1.0)][..]), (NodeKind::ColorWheels, WHEELS), (NodeKind::ToneCurve, &[("shadows", 0.4), ("darks", -0.3), ("lights", 0.2), ("highlights", -0.5)]), (NodeKind::Saturation, &[("amount", 0.6)])],
            // 3: develop + hue/sat + vignette
            vec![(NodeKind::Develop, &[]), (NodeKind::HueSaturation, &all_bands), (NodeKind::Vignette, &[("amount", -0.7), ("midpoint", 0.35), ("feather", 0.45)])],
            // 4: base in the middle
            vec![(NodeKind::Blur, &[("radius", 6.0)]), (NodeKind::ToneCurve, &[("shadows", 0.5), ("highlights", -0.2)]), (NodeKind::Vignette, &[("amount", 0.6), ("midpoint", 0.5), ("feather", 0.5)])],
            // 5: black and white then vignette and cdl
            vec![(NodeKind::Blur, &[("radius", 3.0)]), (NodeKind::BlackWhite, &[("red", 0.3), ("green", 0.59), ("blue", 0.11)]), (NodeKind::Vignette, &[("amount", -0.4)]), (NodeKind::ColorWheels, &[("gamma", -0.3), ("gain_b", 0.5)])],
            // 6: everything
            vec![
                (NodeKind::WhiteBalance, &[("temp", 0.3), ("tint", -0.2)]),
                (NodeKind::Saturation, &[("amount", 1.4)]),
                (NodeKind::ColorWheels, WHEELS),
                (NodeKind::HueSaturation, &all_bands),
                (NodeKind::Contrast, &[("amount", 0.3)]),
                (NodeKind::HighlightsShadows, &[("highlights", -0.3), ("shadows", 0.2)]),
                (NodeKind::BlackWhite, &[("red", 0.2), ("green", 0.7), ("blue", 0.1)]),
            ],
            // 7: clarity as base, then tail
            vec![(NodeKind::Clarity, &[("clarity", 0.5), ("texture", 0.3)]), (NodeKind::ColorWheels, WHEELS), (NodeKind::Vignette, &[("amount", 0.5)])],
            // 8: iso adaptive noise reduction as base
            vec![(NodeKind::NoiseReduction, &[("luma", 0.4), ("chroma", 0.3), ("iso_adaptive", 1.0)]), (NodeKind::ToneCurve, &[("darks", 0.4)]), (NodeKind::Vignette, &[("amount", -0.3)])],
            // 9: sharpen, transform (crop changes the frame) then vignette
            vec![(NodeKind::Transform, &[("left", 0.1), ("right", 0.8), ("top", 0.2), ("bottom", 0.9)]), (NodeKind::Vignette, &[("amount", -0.8)]), (NodeKind::ToneCurve, &[("lights", -0.4)])],
        ] {
            let (g, _, out) = build(&specs);
            graphs.push((g, out));
        }
        // 10-13: a point curve for each channel, mixed with old and new tail operations.
        for channel in 0..4 {
            graphs.push(curve_graph(channel as f32));
        }
        assert!(graphs.len() >= 10);
        for dims in [None, Some((80, 48)), Some((33, 71))] {
            *p.dims.lock().unwrap() = dims;
            set_iso(&p, A, 5000.0);
            for (i, (g, out)) in graphs.iter().enumerate() {
                let tp = check_invariant(g, *out, A, &p);
                assert!(!tp.ops.is_empty() || i == 8, "graph {i} {tp:?}");
            }
        }
        *p.dims.lock().unwrap() = None;

        // Individual mode: A and B carry different values, each equals its own evaluation.
        let (mut g, ids, out) = build(&[
            (NodeKind::ToneCurve, &[("shadows", 0.4)]),
            (NodeKind::ColorWheels, &[("gain", 0.3)]),
            (NodeKind::HueSaturation, &[]),
            (NodeKind::Vignette, &[("amount", 0.4)]),
            (NodeKind::PointCurve, &[("channel", 1.0)]),
        ]);
        for &n in &ids[1..] {
            g.node_mut(n).unwrap().mode = rawgraph_core::Mode::Individual;
        }
        set(&mut g, ids[2], "gain_r", B, -0.6);
        set(&mut g, ids[2], "lift", B, 0.1);
        set(&mut g, ids[4], "amount", B, -0.5);
        set(&mut g, ids[3], "green_s", B, 0.5);
        set_data(&mut g, ids[5], A, Some(curve3()));
        set_data(&mut g, ids[5], B, Some(NodeData::Curve(vec![[0.0, 0.1], [0.5, 0.5], [1.0, 0.9]])));
        let (ta, tb) = (check_invariant(&g, out, A, &p), check_invariant(&g, out, B, &p));
        assert_ne!(ta.ops, tb.ops);
        // Also the unmodified default A->C inheritance.
        check_invariant(&g, out, C, &p);
    }

    #[test]
    fn base_fingerprint_ignores_parameters_of_the_new_tail_kinds() {
        let p = TestProvider::default();
        let (mut g, ids, out) = build(&[
            (NodeKind::Blur, &[("radius", 4.0)]),
            (NodeKind::ColorWheels, &[]),
            (NodeKind::ToneCurve, &[]),
            (NodeKind::PointCurve, &[]),
            (NodeKind::HueSaturation, &[]),
            (NodeKind::Vignette, &[]),
            (NodeKind::BlackWhite, &[]),
        ]);
        let fp0 = plan(&g, out, A, &p).base_fingerprint;
        set(&mut g, ids[2], "gain", A, 0.5);
        set(&mut g, ids[3], "shadows", A, 0.4);
        set_data(&mut g, ids[4], A, Some(curve3()));
        set(&mut g, ids[4], "channel", A, 3.0);
        set(&mut g, ids[5], "red_h", A, 0.3);
        set(&mut g, ids[6], "amount", A, 0.5);
        set(&mut g, ids[7], "red", A, 0.9);
        let tp = plan(&g, out, A, &p);
        assert_eq!(tp.base, ids[1]);
        assert_eq!(tp.ops.len(), 6);
        assert_eq!(tp.base_fingerprint, fp0);
        set(&mut g, ids[1], "radius", A, 7.0);
        assert_ne!(plan(&g, out, A, &p).base_fingerprint, fp0);
    }

    // ---- wave 8: Source folders, Merge collisions, Layer, Develop tone, Curves, Lens, Export ----

    use rawgraph_core::{BlendMode, CurveEntry, FitMode, ResampleFilter, VignetteRadial, WarpRectilinear};

    fn colour(i: u64) -> [f32; 3] {
        [((i * 37) % 97) as f32 / 100.0 + 0.02, ((i * 53) % 89) as f32 / 100.0 + 0.02, ((i * 71) % 83) as f32 / 100.0 + 0.02]
    }

    /// Registers `ids` as flat-coloured images of `size` below `folder` and appends them to the catalog.
    fn add_images(p: &TestProvider, ids: std::ops::Range<u64>, folder: &str, size: (u32, u32)) {
        for i in ids {
            p.paths.lock().unwrap().insert(i, format!("{folder}/img{i}.jpg"));
            p.flat.lock().unwrap().insert(i, colour(i));
            p.sizes.lock().unwrap().insert(i, size);
            p.catalog.lock().unwrap().push(ImageId(i));
        }
    }

    fn folder_source(g: &mut Graph, folder: &str) -> NodeId {
        let n = add(g, NodeKind::Source);
        set_data(g, n, A, Some(NodeData::Folder(folder.to_string())));
        n
    }

    /// 10 images in folder a (100..110), 5 in folder b (200..205), 3 in folder c (300..303), all 8x8.
    fn layer_provider() -> TestProvider {
        let p = TestProvider::default();
        add_images(&p, 100..110, "C:/photos/a", (8, 8));
        add_images(&p, 200..205, "C:/photos/b", (8, 8));
        add_images(&p, 300..303, "C:/photos/c", (8, 8));
        p
    }

    /// Folder sources a, b -> Layer inputs 1, 2 -> Output. Returns (graph, layer, out, [a, b]).
    fn layer_graph() -> (Graph, NodeId, NodeId, [NodeId; 2]) {
        let mut g = Graph::new();
        let a = folder_source(&mut g, "C:/photos/a");
        let b = folder_source(&mut g, "C:/photos/b");
        let layer = add(&mut g, NodeKind::Layer);
        let out = add(&mut g, NodeKind::Output);
        g.connect(a, layer, 0).unwrap();
        g.connect(b, layer, 1).unwrap();
        g.connect(layer, out, 0).unwrap();
        (g, layer, out, [a, b])
    }

    fn centre(img: &ImageBuf) -> [f32; 4] {
        img.pixel(img.width() / 2, img.height() / 2)
    }

    fn assert_px(got: [f32; 4], want: [f32; 3], what: &str) {
        for c in 0..3 {
            assert!((got[c] - want[c]).abs() < 1e-4, "{what}: channel {c} is {} but {} expected", got[c], want[c]);
        }
    }

    fn mix(base: [f32; 3], layer: [f32; 3], a: f32) -> [f32; 3] {
        [0, 1, 2].map(|c| base[c] + a * (layer[c] - base[c]))
    }

    /// One placed layer of `hand_stack`.
    struct HandLayer {
        id: ImageId,
        blend: BlendMode,
        opacity: f32,
        fit: FitMode,
        filter: ResampleFilter,
        crop_scale: f32,
    }

    fn hand(id: u64, blend: BlendMode, opacity: f32) -> HandLayer {
        HandLayer { id: ImageId(id), blend, opacity, fit: FitMode::Fit, filter: ResampleFilter::Linear, crop_scale: 1.0 }
    }

    /// The Layer maths written out by hand.
    fn hand_stack(p: &TestProvider, canvas: u64, canvas_opacity: f32, rest: &[HandLayer]) -> ImageBuf {
        let mut c = (*p.image(ImageId(canvas)).unwrap()).clone();
        if canvas_opacity != 1.0 {
            for px in c.data_mut().as_chunks_mut::<4>().0.iter_mut() {
                px[0] *= canvas_opacity;
                px[1] *= canvas_opacity;
                px[2] *= canvas_opacity;
            }
        }
        for l in rest {
            let img = p.image(l.id).unwrap();
            let placed = fx::place_layer(&img, c.width(), c.height(), l.fit, l.filter, l.crop_scale);
            fx::blend_layer(&mut c, &placed, l.blend, l.opacity);
        }
        c
    }

    #[test]
    fn layer_match_pairing_end_to_end() {
        let (mut g, layer, out, _) = layer_graph();
        let p = layer_provider();
        let mut ev = Evaluator::new();
        // The collection is the driver's (folder a, 10 images).
        let a_ids: Vec<ImageId> = (100..110).map(ImageId).collect();
        assert_eq!(ev.members(&g, layer, 0, &p), a_ids);
        assert_eq!(ev.members(&g, out, 0, &p), a_ids);
        assert!(!ev.contains(&g, layer, ImageId(200), &p), "folder b images are not outputs of the layer");

        // Normal 100 %: output k shows layer 2's image k mod 5.
        for k in 0..10u64 {
            let res = run(&g, out, ImageId(100 + k), &p, &mut ev);
            assert_px(centre(&res), colour(200 + k % 5), &format!("output {k}"));
            assert_eq!(centre(&res)[3], 1.0);
        }
        // Normal 50 %: the exact expected mix of the pair.
        set(&mut g, layer, "l2_opacity", A, 0.5);
        for k in 0..10u64 {
            let res = run(&g, out, ImageId(100 + k), &p, &mut ev);
            assert_px(centre(&res), mix(colour(100 + k), colour(200 + k % 5), 0.5), &format!("mix {k}"));
            let expect = hand_stack(&p, 100 + k, 1.0, &[hand(200 + k % 5, BlendMode::Normal, 0.5)]);
            assert_eq!(*res, expect, "bit-exact against the hand-written sequence for output {k}");
        }
        // An image that is not an output.
        let r = ev.eval(&g, out, ImageId(200), &p, &CancelToken::new());
        assert!(matches!(r, Err(EvalError::NotInCollection { .. })), "{r:?}");
    }

    #[test]
    fn layer_on_off_bottom_opacity_and_nothing_on() {
        let (mut g, layer, out, _) = layer_graph();
        let p = layer_provider();
        p.sizes.lock().unwrap().insert(100, (10, 6));
        let mut ev = Evaluator::new();
        // Bottom layer off: layer 2 becomes the canvas (its own size, its own pixels).
        set(&mut g, layer, "l1_on", A, 0.0);
        let res = run(&g, out, ImageId(100), &p, &mut ev);
        assert_eq!((res.width(), res.height()), (8, 8));
        assert_eq!(*res, hand_stack(&p, 200, 1.0, &[]));
        // Layer 2 off instead: only the bottom layer.
        set(&mut g, layer, "l1_on", A, 1.0);
        set(&mut g, layer, "l2_on", A, 0.0);
        let res = run(&g, out, ImageId(100), &p, &mut ev);
        assert_eq!(*res, *p.image(ImageId(100)).unwrap());
        // Bottom opacity multiplies RGB, alpha stays.
        set(&mut g, layer, "l1_opacity", A, 0.4);
        let res = run(&g, out, ImageId(100), &p, &mut ev);
        let c = colour(100);
        assert_px(centre(&res), [c[0] * 0.4, c[1] * 0.4, c[2] * 0.4], "bottom opacity");
        assert_eq!(centre(&res)[3], 1.0);
        assert_eq!(*res, hand_stack(&p, 100, 0.4, &[]));
        // Nothing on: opaque black in the size of the lowest connected layer's image.
        set(&mut g, layer, "l1_on", A, 0.0);
        let res = run(&g, out, ImageId(100), &p, &mut ev);
        assert_eq!((res.width(), res.height()), (10, 6));
        assert!(res.data().as_chunks::<4>().0.iter().all(|px| *px == [0.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn layer_three_layers_in_ascending_order() {
        let (mut g, layer, out, _) = layer_graph();
        let c_src = folder_source(&mut g, "C:/photos/c");
        g.connect(c_src, layer, 2).unwrap();
        let p = layer_provider();
        let mut ev = Evaluator::new();
        set(&mut g, layer, "l2_opacity", A, 0.5);
        set(&mut g, layer, "l3_blend", A, 3.0); // Multiply
        for k in [0u64, 4, 7] {
            let res = run(&g, out, ImageId(100 + k), &p, &mut ev);
            let expect = hand_stack(&p, 100 + k, 1.0, &[hand(200 + k % 5, BlendMode::Normal, 0.5), hand(300 + k % 3, BlendMode::Multiply, 1.0)]);
            assert_eq!(*res, expect, "output {k}");
            let base = mix(colour(100 + k), colour(200 + k % 5), 0.5);
            let cc = colour(300 + k % 3);
            assert_px(centre(&res), [base[0] * cc[0], base[1] * cc[1], base[2] * cc[2]], "multiply on top of the mix");
        }
        // Swapping the blend modes of layers 2 and 3 gives another picture: the order matters.
        let before = run(&g, out, ImageId(100), &p, &mut ev);
        set(&mut g, layer, "l2_blend", A, 3.0);
        set(&mut g, layer, "l3_blend", A, 0.0);
        set(&mut g, layer, "l3_opacity", A, 0.5);
        set(&mut g, layer, "l2_opacity", A, 1.0);
        let after = run(&g, out, ImageId(100), &p, &mut ev);
        assert!(*before != *after);
        let expect = hand_stack(&p, 100, 1.0, &[hand(200, BlendMode::Multiply, 1.0), hand(300, BlendMode::Normal, 0.5)]);
        assert_eq!(*after, expect);
    }

    #[test]
    fn layer_unconnected_middle_and_empty_input() {
        let (mut g, layer, out, [_, b]) = layer_graph();
        let c_src = folder_source(&mut g, "C:/photos/c");
        g.disconnect(layer, 1);
        g.connect(c_src, layer, 2).unwrap();
        let p = layer_provider();
        let mut ev = Evaluator::new();
        // Layer 2 is not connected: layer 3 pairs with the driver (layers 1 and 3 are the connected ones).
        let res = run(&g, out, ImageId(103), &p, &mut ev);
        assert_eq!(*res, hand_stack(&p, 103, 1.0, &[hand(300, BlendMode::Normal, 1.0)]));

        // An input whose collection is empty contributes nothing.
        let empty = folder_source(&mut g, "C:/photos/nothing");
        g.connect(empty, layer, 1).unwrap();
        let res = run(&g, out, ImageId(103), &p, &mut ev);
        assert_eq!(*res, hand_stack(&p, 103, 1.0, &[hand(300, BlendMode::Normal, 1.0)]));
        let _ = b;
    }

    #[test]
    fn layer_fit_modes_and_crop_scale_from_full_sizes() {
        let (mut g, layer, out, _) = layer_graph();
        let p = layer_provider();
        for k in 200..205 {
            p.sizes.lock().unwrap().insert(k, (4, 2));
        }
        let mut ev = Evaluator::new();
        let mut seen = Vec::new();
        for (fit_idx, fit) in [FitMode::Fit, FitMode::Fill, FitMode::Crop, FitMode::Stretch].into_iter().enumerate() {
            set(&mut g, layer, "l2_fit", A, fit_idx as f32);
            set(&mut g, layer, "l2_filter", A, 1.0); // Bicubic
            let res = run(&g, out, ImageId(100), &p, &mut ev);
            let l = HandLayer { fit, filter: ResampleFilter::Bicubic, ..hand(200, BlendMode::Normal, 1.0) };
            assert_eq!(*res, hand_stack(&p, 100, 1.0, &[l]), "{fit:?}");
            seen.push((*res).clone());
        }
        assert!(seen[0] != seen[1] && seen[1] != seen[3], "the fit modes differ on different sizes");

        // Crop with known full sizes: canvas 8 px of 800 (0.01), layer 4 px of 200 (0.02) -> 0.5.
        p.full.lock().unwrap().insert(100, (800, 800));
        p.full.lock().unwrap().insert(200, (200, 100));
        set(&mut g, layer, "l2_fit", A, 2.0);
        set(&mut g, layer, "l2_filter", A, 0.0);
        let with_sizes = run(&g, out, ImageId(100), &p, &mut ev);
        let l = HandLayer { fit: FitMode::Crop, crop_scale: 0.5, ..hand(200, BlendMode::Normal, 1.0) };
        assert_eq!(*with_sizes, hand_stack(&p, 100, 1.0, &[l]));
        // Without a full size for one of them the scale is 1.
        p.full.lock().unwrap().remove(&200);
        let without = run(&g, out, ImageId(100), &p, &mut ev);
        let l = HandLayer { fit: FitMode::Crop, ..hand(200, BlendMode::Normal, 1.0) };
        assert_eq!(*without, hand_stack(&p, 100, 1.0, &[l]));
        assert!(*without != *with_sizes, "the full sizes are part of the cache key and of the result");
    }

    #[test]
    fn layer_individual_settings_follow_the_output_identity() {
        let (mut g, layer, out, _) = layer_graph();
        g.node_mut(layer).unwrap().mode = rawgraph_core::Mode::Individual;
        let p = layer_provider();
        let mut ev = Evaluator::new();
        set(&mut g, layer, "l2_opacity", ImageId(100), 0.0);
        set(&mut g, layer, "l2_opacity", ImageId(101), 0.25);
        let r0 = run(&g, out, ImageId(100), &p, &mut ev);
        let r1 = run(&g, out, ImageId(101), &p, &mut ev);
        let r2 = run(&g, out, ImageId(102), &p, &mut ev);
        assert_eq!(*r0, hand_stack(&p, 100, 1.0, &[hand(200, BlendMode::Normal, 0.0)]));
        assert_eq!(*r1, hand_stack(&p, 101, 1.0, &[hand(201, BlendMode::Normal, 0.25)]));
        assert_eq!(*r2, hand_stack(&p, 102, 1.0, &[hand(202, BlendMode::Normal, 1.0)]));
        // Changing one image's setting leaves the others cached.
        let misses = ev.misses();
        set(&mut g, layer, "l2_opacity", ImageId(101), 0.75);
        run(&g, out, ImageId(102), &p, &mut ev);
        assert_eq!(ev.misses(), misses);
        run(&g, out, ImageId(101), &p, &mut ev);
        assert!(ev.misses() > misses);
    }

    #[test]
    fn layer_cache_follows_the_source_image_and_repeats_hit() {
        let (mut g, layer, out, _) = layer_graph();
        let p = layer_provider();
        let mut ev = Evaluator::new();
        set(&mut g, layer, "l2_opacity", A, 0.5);
        let first = run(&g, out, ImageId(100), &p, &mut ev);
        let (misses, calls) = (ev.misses(), p.calls());
        let again = run(&g, out, ImageId(100), &p, &mut ev);
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!((ev.misses(), p.calls()), (misses, calls), "repeating is fully cached");
        assert!(ev.hits() > 0);

        // Layer 2's source for output 100 is image 200. Take 200 out of the catalog: now 201.
        p.catalog.lock().unwrap().retain(|i| i.0 != 200);
        let moved = run(&g, out, ImageId(100), &p, &mut ev);
        assert!(*moved != *first);
        assert_eq!(*moved, hand_stack(&p, 100, 1.0, &[hand(201, BlendMode::Normal, 0.5)]));
        assert!(ev.misses() > misses, "a changed source image re-evaluates");

        // A new picture of the same source image invalidates through the provider revision.
        let misses = ev.misses();
        p.flat.lock().unwrap().insert(201, [0.9, 0.1, 0.1]);
        p.revision.store(5, Ordering::Relaxed);
        let fresh = run(&g, out, ImageId(100), &p, &mut ev);
        assert!(*fresh != *moved);
        assert!(ev.misses() > misses);
        assert_eq!(*fresh, hand_stack(&p, 100, 1.0, &[hand(201, BlendMode::Normal, 0.5)]));
    }

    #[test]
    fn layer_cancellation_and_errors() {
        let (g, _, out, _) = layer_graph();
        let p = layer_provider();
        let mut ev = Evaluator::new();
        let tok = CancelToken::new();
        tok.cancel();
        assert_eq!(ev.eval(&g, out, ImageId(100), &p, &tok).unwrap_err(), EvalError::Cancelled);
        // A layer input that fails to load reports the provider error.
        p.failing.lock().unwrap().insert(200);
        assert!(matches!(ev.eval(&g, out, ImageId(100), &p, &CancelToken::new()), Err(EvalError::Provider(_))));
    }

    #[test]
    fn source_folder_membership_and_not_in_collection() {
        let p = TestProvider::default();
        add_images(&p, 100..103, "C:/photos/a", (8, 8));
        add_images(&p, 200..202, "C:/photos/b", (8, 8));
        p.paths.lock().unwrap().insert(103, "C:\\Photos\\A\\sub\\deep.jpg".to_string());
        p.flat.lock().unwrap().insert(103, colour(103));
        p.sizes.lock().unwrap().insert(103, (8, 8));
        p.catalog.lock().unwrap().push(ImageId(103));
        let mut g = Graph::new();
        let src = folder_source(&mut g, "C:/photos/a");
        let out = add(&mut g, NodeKind::Output);
        g.connect(src, out, 0).unwrap();
        let mut ev = Evaluator::new();
        let c = CancelToken::new();

        assert_eq!(ev.members(&g, out, 0, &p), vec![ImageId(100), ImageId(101), ImageId(102), ImageId(103)]);
        assert!(ev.eval(&g, out, ImageId(100), &p, &c).is_ok());
        assert_eq!(ev.eval(&g, src, ImageId(200), &p, &c).unwrap_err(), EvalError::NotInCollection { node: src });
        assert!(matches!(ev.eval(&g, out, ImageId(200), &p, &c), Err(EvalError::NotInCollection { .. })));
        assert!(ev.eval(&g, out, ImageId(103), &p, &c).is_ok(), "subfolders are included by default");

        // Subfolders off.
        set(&mut g, src, "subfolders", A, 0.0);
        assert!(ev.eval(&g, out, ImageId(103), &p, &c).is_err(), "no stale membership after the toggle");
        assert!(ev.eval(&g, out, ImageId(101), &p, &c).is_ok());
        // Another folder: the old pixels must not be served.
        set_data(&mut g, src, A, Some(NodeData::Folder("C:/photos/b".to_string())));
        assert!(ev.eval(&g, out, ImageId(101), &p, &c).is_err());
        assert!(ev.eval(&g, out, ImageId(200), &p, &c).is_ok());
        // No data = every catalog image.
        set_data(&mut g, src, A, None);
        assert!(ev.eval(&g, out, ImageId(101), &p, &c).is_ok());
        assert!(ev.eval(&g, out, ImageId(200), &p, &c).is_ok());
        // An image whose path is unknown is not in any folder.
        set_data(&mut g, src, A, Some(NodeData::Folder("C:/photos/a".to_string())));
        assert!(ev.eval(&g, out, ImageId(999), &p, &c).is_err());
    }

    #[test]
    fn merge_name_collisions_through_the_evaluator() {
        let p = TestProvider::default();
        for (id, path) in [(100u64, "C:/a/x.jpg"), (101, "C:/a/y.jpg"), (200, "D:/b/X.JPG"), (201, "D:/b/z.jpg")] {
            p.paths.lock().unwrap().insert(id, path.to_string());
            p.flat.lock().unwrap().insert(id, colour(id));
            p.sizes.lock().unwrap().insert(id, (8, 8));
            p.catalog.lock().unwrap().push(ImageId(id));
        }
        let mut g = Graph::new();
        let a = folder_source(&mut g, "C:/a");
        let b = folder_source(&mut g, "D:/b");
        let mg = add(&mut g, NodeKind::Merge);
        let out = add(&mut g, NodeKind::Output);
        g.connect(a, mg, 0).unwrap();
        g.connect(b, mg, 1).unwrap();
        g.connect(mg, out, 0).unwrap();
        let mut ev = Evaluator::new();
        let c = CancelToken::new();
        let ok = |ev: &mut Evaluator, g: &Graph, id: u64| ev.eval(g, out, ImageId(id), &p, &c).is_ok();
        let all = [100, 101, 200, 201];

        assert!(all.iter().all(|&i| ok(&mut ev, &g, i)), "Keep both");
        assert_eq!(ev.members(&g, mg, 0, &p).len(), 4);
        set(&mut g, mg, "collision", A, 1.0); // first input wins
        assert_eq!([100, 101, 200, 201].map(|i| ok(&mut ev, &g, i)), [true, true, false, true]);
        assert_eq!(ev.members(&g, mg, 0, &p), vec![ImageId(100), ImageId(101), ImageId(201)]);
        set(&mut g, mg, "collision", A, 2.0); // last input wins
        assert_eq!([100, 101, 200, 201].map(|i| ok(&mut ev, &g, i)), [false, true, true, true]);
        assert!(!ev.contains(&g, out, ImageId(100), &p));
        set(&mut g, mg, "collision", A, 0.0);
        assert!(all.iter().all(|&i| ok(&mut ev, &g, i)));
    }

    const TONE: [(&str, f32); 8] = [
        ("exposure", 0.6),
        ("contrast", 0.3),
        ("highlights", -0.4),
        ("shadows", 0.5),
        ("whites", 0.35),
        ("blacks", -0.25),
        ("vibrance", 0.45),
        ("saturation", -0.2),
    ];

    /// The Develop tone part written out by hand, in the contract's order.
    fn hand_tone(base: &ImageBuf, t: &[(&str, f32)]) -> ImageBuf {
        let v = |name: &str| t.iter().find(|(n, _)| *n == name).map_or(0.0, |(_, x)| *x);
        let mut m = base.clone();
        // Neutral steps are skipped, like the evaluator does.
        if v("exposure") != 0.0 {
            ops::apply_gain(&mut m, 2f32.powf(v("exposure")));
        }
        if v("whites") != 0.0 || v("blacks") != 0.0 {
            fx::whites_blacks(&mut m, v("whites"), v("blacks"));
        }
        if v("highlights") != 0.0 || v("shadows") != 0.0 {
            ops::highlights_shadows(&mut m, v("highlights"), v("shadows"));
        }
        if v("contrast") != 0.0 {
            ops::contrast(&mut m, v("contrast"), 0.18);
        }
        if v("vibrance") != 0.0 {
            fx::vibrance(&mut m, v("vibrance"));
        }
        if v("saturation") != 0.0 {
            ops::saturation(&mut m, 1.0 + v("saturation"));
        }
        m
    }

    #[test]
    fn develop_tone_order_and_neutral_identity() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, src, dev) = one(NodeKind::Develop);
        let base = run(&g, src, A, &p, &mut ev);
        // Untouched: bit-identical to the old result (the same Arc for a rendered file).
        assert!(Arc::ptr_eq(&base, &run(&g, dev, A, &p, &mut ev)));
        // RAW: the provider already applied input gain.
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        assert_eq!(*run(&g, dev, A, &p, &mut ev), *base);

        // Every tone slider, RAW and rendered (JPEG-like) alike, against the hand-written sequence.
        for caps in [CaptureInfo { raw: Some(false), ..Default::default() }, raw_capture(Some(5200.0), Some(0.5))] {
            let is_raw = caps.raw == Some(true);
            p.caps.lock().unwrap().insert(A.0, caps);
            for (name, v) in TONE {
                set(&mut g, dev, name, A, v);
            }
            let pre = (*base).clone();
            let got = run(&g, dev, A, &p, &mut ev);
            assert_eq!(*got, hand_tone(&pre, &TONE), "raw {is_raw}");
            assert!(*got != pre);
            // One slider at a time.
            for (name, v) in TONE {
                for (n, _) in TONE {
                    set(&mut g, dev, n, A, 0.0);
                }
                set(&mut g, dev, name, A, v);
                let got = run(&g, dev, A, &p, &mut ev);
                assert_eq!(*got, hand_tone(&pre, &[(name, v)]), "{name} alone, raw {is_raw}");
            }
            for (n, _) in TONE {
                set(&mut g, dev, n, A, 0.0);
            }
        }
        // Non-finite tone values read as 0.
        set(&mut g, dev, "exposure", A, f32::NAN);
        let _ = run(&g, dev, A, &p, &mut ev);
    }

    #[test]
    fn curve_sets_apply_in_the_contract_order() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, src, n) = one(NodeKind::PointCurve);
        let base = run(&g, src, A, &p, &mut ev);
        let lum = vec![[0.0, 0.0], [0.5, 0.4], [1.0, 1.0]];
        let blue = vec![[0.0, 0.1], [0.5, 0.5], [1.0, 0.9]];
        let hue_hue = vec![[0.0, 0.5], [0.3, 0.6], [1.0, 0.5]];
        let hue_sat = vec![[0.0, 0.5], [0.6, 0.7], [1.0, 0.5]];
        let sat_sat = vec![[0.0, 0.5], [0.5, 0.35], [1.0, 0.5]];
        let entry = |channel: u8, points: &Vec<[f32; 2]>| CurveEntry { channel, points: points.clone() };
        // Entries stored in a shuffled order, plus a neutral one that is dropped.
        let set_entries = vec![entry(4, &sat_sat), entry(3, &blue), entry(6, &hue_sat), entry(0, &lum), entry(5, &hue_hue), entry(1, &vec![[0.0, 0.0], [1.0, 1.0]])];
        set_data(&mut g, n, A, Some(NodeData::CurveSet(set_entries)));
        let got = run(&g, n, A, &p, &mut ev);
        let mut expect = (*base).clone();
        for (idx, points) in [(0usize, &lum), (3, &blue), (5, &hue_hue), (6, &hue_sat), (4, &sat_sat)] {
            fx::point_curve(&mut expect, points, fx::CurveChannel::from_index(idx).unwrap());
        }
        assert_eq!(*got, expect);
        assert!(*got != *base);

        // All neutral: the input Arc comes back.
        set_data(&mut g, n, A, Some(NodeData::CurveSet(vec![entry(0, &vec![[0.0, 0.0], [1.0, 1.0]]), entry(5, &vec![[0.0, 0.5], [1.0, 0.5]])])));
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)));
        // Editing one curve of the set recomputes.
        let a = vec![entry(2, &lum)];
        set_data(&mut g, n, A, Some(NodeData::CurveSet(a)));
        let first = run(&g, n, A, &p, &mut ev);
        let misses = ev.misses();
        set_data(&mut g, n, A, Some(NodeData::CurveSet(vec![entry(2, &blue)])));
        assert!(*first != *run(&g, n, A, &p, &mut ev));
        assert!(ev.misses() > misses);
    }

    fn test_profile() -> LensProfile {
        LensProfile {
            warp: Some(WarpRectilinear { planes: vec![[1.0, 0.03, 0.0, 0.0, 0.0, 0.0]], center: [0.5, 0.5] }),
            vignette: Some(VignetteRadial { k: [-0.3, 0.1, 0.0, 0.0, 0.0], center: [0.5, 0.5] }),
        }
    }

    #[test]
    fn lens_auto_with_profile_without_profile_and_behind_a_crop() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (mut g, src, n) = one(NodeKind::LensManual);
        let base = run(&g, src, A, &p, &mut ev);

        // Manual ignores the profile.
        p.lens.lock().unwrap().insert(A.0, test_profile());
        assert!(Arc::ptr_eq(&base, &run(&g, n, A, &p, &mut ev)));
        set(&mut g, n, "mode", A, 1.0); // Auto
        let profile = test_profile();
        let mut by_hand = fx::warp_rectilinear(&base, profile.warp.as_ref().unwrap());
        fx::vignette_radial(&mut by_hand, profile.vignette.as_ref().unwrap());
        let got = run(&g, n, A, &p, &mut ev);
        assert_eq!(*got, by_hand);
        assert!(*got != *base);

        // Profile first, the manual values on top.
        set(&mut g, n, "distortion", A, 0.05);
        set(&mut g, n, "ca_red", A, 1.0);
        let on_top = fx::lens_manual(&by_hand, pv(&g, n, "distortion"), 0.0, pv(&g, n, "ca_red"), 0.0);
        assert_eq!(*run(&g, n, A, &p, &mut ev), on_top);

        // Auto without a profile is Manual (and the cached result does not outlive the profile).
        p.lens.lock().unwrap().clear();
        let manual = fx::lens_manual(&base, pv(&g, n, "distortion"), 0.0, pv(&g, n, "ca_red"), 0.0);
        assert_eq!(*run(&g, n, A, &p, &mut ev), manual);
        // Only a warp, only a vignette.
        p.lens.lock().unwrap().insert(A.0, LensProfile { warp: None, vignette: profile.vignette.clone() });
        set(&mut g, n, "distortion", A, 0.0);
        set(&mut g, n, "ca_red", A, 0.0);
        let mut only_vignette = (*base).clone();
        fx::vignette_radial(&mut only_vignette, profile.vignette.as_ref().unwrap());
        assert_eq!(*run(&g, n, A, &p, &mut ev), only_vignette);

        // Behind a crop the profile no longer fits the frame: Auto is Manual.
        let mut g2 = Graph::new();
        let (_, t) = cropped(&mut g2);
        let lens = add(&mut g2, NodeKind::LensManual);
        g2.connect(t, lens, 0).unwrap();
        set(&mut g2, lens, "mode", A, 1.0);
        set(&mut g2, lens, "distortion", A, 0.04);
        p.lens.lock().unwrap().insert(A.0, test_profile());
        let crop = run(&g2, t, A, &p, &mut ev);
        let got = run(&g2, lens, A, &p, &mut ev);
        assert_eq!(*got, fx::lens_manual(&crop, 0.04, 0.0, 0.0, 0.0));
    }

    #[test]
    fn export_node_evaluates_like_output() {
        let p = TestProvider::default();
        let mut ev = Evaluator::new();
        let (g, src, _) = one(NodeKind::Output);
        let mut g = g;
        let exp = add(&mut g, NodeKind::Exposure);
        let export = add(&mut g, NodeKind::Export);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, export, 0).unwrap();
        set(&mut g, exp, "ev", A, 1.0);
        let res = ev.eval_image(&g, export, A, &p, &CancelToken::new()).unwrap();
        let via_exposure = ev.eval_image(&g, exp, A, &p, &CancelToken::new()).unwrap();
        assert!(Arc::ptr_eq(&res, &via_exposure));
        assert!(ev.eval_output(&g, export, 0, A, &p, &CancelToken::new()).is_ok());
        assert!(ev.contains(&g, export, A, &p));
        let tp = check_invariant(&g, export, A, &p);
        assert_eq!((tp.base, tp.ops), (src_of(&g, exp), vec![TailOp::Gain(2.0)]));
        let rendered = ev.render_batch(&g, export, &[A, B], &p, &CancelToken::new(), &mut |_, _| {});
        assert!(rendered.iter().all(|(_, r)| r.is_ok()));
    }

    fn src_of(g: &Graph, n: NodeId) -> NodeId {
        g.input_edge(n, 0).unwrap().0
    }

    #[test]
    fn tail_invariant_for_whites_blacks_vibrance_and_every_tone_op() {
        let p = TestProvider::default();
        // A Develop node with one tone slider after another, then all of them.
        for caps in [None, Some(CaptureInfo { raw: Some(false), ..Default::default() }), Some(raw_capture(Some(5200.0), Some(0.5)))] {
            match caps {
                Some(c) => p.caps.lock().unwrap().insert(A.0, c),
                None => p.caps.lock().unwrap().remove(&A.0),
            };
            let mut variants: Vec<Vec<(&str, f32)>> = TONE.iter().map(|t| vec![*t]).collect();
            variants.push(TONE.to_vec());
            for v in variants {
                let (mut g, src, dev) = one(NodeKind::Develop);
                let out = add(&mut g, NodeKind::Output);
                g.connect(dev, out, 0).unwrap();
                for (n, x) in &v {
                    set(&mut g, dev, n, A, *x);
                }
                let tp = check_invariant(&g, out, A, &p);
                assert!(tp.base_before_tone, "{v:?}");
                assert_eq!((tp.base, tp.base_port), (dev, 0));
                assert!(!tp.ops.is_empty());
                let _ = src;
            }
        }
        // The op shapes of the full tone part.
        let (mut g, _, dev) = one(NodeKind::Develop);
        for (n, x) in TONE {
            set(&mut g, dev, n, A, x);
        }
        let tp = plan(&g, dev, A, &p);
        assert_eq!(
            tp.ops,
            vec![
                TailOp::Gain(2f32.powf(0.6)),
                TailOp::WhitesBlacks { whites: 0.35, blacks: -0.25 },
                TailOp::HighlightsShadows { highlights: -0.4, shadows: 0.5 },
                TailOp::Contrast { amount: 0.3, pivot: 0.18 },
                TailOp::Vibrance(0.45),
                TailOp::Saturation(1.0 + -0.2),
            ]
        );
    }

    #[test]
    fn tail_with_develop_tone_and_further_tail_nodes() {
        let p = TestProvider::default();
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        // Source -> Blur -> Develop(tone) -> Exposure -> Curves -> Saturation -> Output.
        let (mut g, ids, out) = build(&[
            (NodeKind::Blur, &[("radius", 4.0)]),
            (NodeKind::Develop, &[("exposure", 0.4), ("whites", 0.3), ("vibrance", -0.5)]),
            (NodeKind::Exposure, &[("ev", 0.5)]),
            (NodeKind::PointCurve, &[]),
            (NodeKind::Saturation, &[("amount", 1.3)]),
        ]);
        set_data(&mut g, ids[4], A, Some(NodeData::CurveSet(vec![CurveEntry { channel: 3, points: vec![[0.0, 0.0], [0.5, 0.4], [1.0, 1.0]] }])));
        let tp = check_invariant(&g, out, A, &p);
        assert!(tp.base_before_tone);
        assert_eq!(tp.base, ids[2]);
        assert_eq!(tp.ops.len(), 3 + 1 + 1 + 1);
        assert!(tp.ops.iter().any(|o| matches!(o, TailOp::Vibrance(_))));
        // The base is the Develop output before its tone part: white balance of the RAW base, no tone.
        let mut ev = Evaluator::new();
        let blurred = ev.eval_image(&g, ids[1], A, &p, &CancelToken::new()).unwrap();
        let base = ev.eval_base(&g, &tp, A, &p, &CancelToken::new()).unwrap();
        assert_eq!(*base, *blurred);

        // Develop behind a non-pointwise node: the walk ends at the blur, nothing special.
        let (mut g, ids, out) = build(&[
            (NodeKind::Develop, &[("contrast", 0.4)]),
            (NodeKind::Blur, &[("radius", 3.0)]),
            (NodeKind::Exposure, &[("ev", 0.5)]),
        ]);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!((tp.base, tp.base_before_tone), (ids[2], false));
        set(&mut g, ids[1], "saturation", A, 0.3);
        check_invariant(&g, out, A, &p);
    }

    #[test]
    fn local_develop_is_a_cpu_tail_boundary() {
        let p = TestProvider::default();
        let (mut g, ids, out) = build(&[(NodeKind::Develop, &[("highlights", -0.5)]), (NodeKind::Exposure, &[("ev", 0.5)])]);
        set(&mut g, ids[1], "tone_version", A, 1.0);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!((tp.base, tp.base_before_tone), (ids[1], false));
        assert_eq!(tp.ops, vec![TailOp::Gain(2f32.powf(0.5))]);
    }

    #[test]
    fn neutral_develop_tone_keeps_the_old_plan() {
        let p = TestProvider::default();
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let (g, ids, out) = build(&[(NodeKind::Develop, &[])]);
        let tp = check_invariant(&g, out, A, &p);
        assert_eq!((tp.base, tp.base_before_tone), (ids[0], false));
        assert!(tp.ops.is_empty());
    }

    #[test]
    fn develop_pre_tone_base_is_cached_separately() {
        let p = TestProvider::default();
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(0.5)));
        let (mut g, ids, out) = build(&[(NodeKind::Develop, &[("exposure", 0.4)])]);
        let dev = ids[1];
        let mut ev = Evaluator::new();
        let c = CancelToken::new();

        let tp = ev.plan_tail(&g, out, A, &p).unwrap();
        assert!(tp.base_before_tone);
        assert_eq!((tp.base, tp.ops.clone()), (dev, vec![TailOp::Gain(2f32.powf(0.4))]));
        let base1 = ev.eval_base(&g, &tp, A, &p, &c).unwrap();
        let src = p.image(A).unwrap();
        assert_eq!(*base1, *src, "base = Develop before the tone part");
        let (calls, misses, hits) = (p.calls(), ev.misses(), ev.hits());
        assert_eq!(ev.eval_base(&g, &tp, A, &p, &c).unwrap().as_ref() as *const _, base1.as_ref() as *const _);
        assert_eq!(ev.hits(), hits + 1);

        // Dragging tone sliders: same base fingerprint, same base image, no provider call, no recompute of the base.
        for (name, v) in [("exposure", 0.8), ("contrast", 0.3), ("saturation", -0.4), ("whites", 0.2)] {
            set(&mut g, dev, name, A, v);
            let tp2 = ev.plan_tail(&g, out, A, &p).unwrap();
            assert_eq!(tp2.base_fingerprint, tp.base_fingerprint, "{name}");
            let base2 = ev.eval_base(&g, &tp2, A, &p, &c).unwrap();
            assert!(Arc::ptr_eq(&base1, &base2), "{name}");
            assert_eq!(apply_tail_cpu(&base2, &tp2.ops), *ev.eval_image(&g, out, A, &p, &c).unwrap());
        }
        assert_eq!(p.calls(), calls, "the image is loaded once");
        assert_eq!(ev.misses() - misses, 4 * 2, "only Develop and Output are recomputed per slider move");

        // A base parameter does change it.
        set(&mut g, dev, "wb", A, 1.0);
        set(&mut g, dev, "temperature", A, 3200.0);
        let tp3 = ev.plan_tail(&g, out, A, &p).unwrap();
        assert_ne!(tp3.base_fingerprint, tp.base_fingerprint);
        let base3 = ev.eval_base(&g, &tp3, A, &p, &c).unwrap();
        assert!(!Arc::ptr_eq(&base1, &base3));
        assert_eq!(p.calls(), calls);
        // Capture metadata the base reads, and the provider revision.
        p.caps.lock().unwrap().insert(A.0, raw_capture(Some(5200.0), Some(1.5)));
        assert_ne!(ev.plan_tail(&g, out, A, &p).unwrap().base_fingerprint, tp3.base_fingerprint);
        // eval_base without a plan from this evaluator state still validates membership.
        assert!(ev.eval_base(&g, &tp3, ImageId(77), &p, &c).is_ok());
    }

    #[test]
    fn layer_and_develop_fingerprints_follow_everything_that_changes_the_picture() {
        let (g, layer, out, _) = layer_graph();
        let p = layer_provider();
        let fp = |p: &TestProvider| fingerprint_of(&g, layer, ImageId(100), p);
        let f0 = fp(&p);
        assert_eq!(f0, fp(&p));
        let _ = out;
        // Full sizes feed the crop scale.
        p.full.lock().unwrap().insert(200, (100, 100));
        let f1 = fp(&p);
        assert_ne!(f1, f0);
        // The source image of a layer.
        p.catalog.lock().unwrap().retain(|i| i.0 != 200);
        assert_ne!(fp(&p), f1);
    }

    #[test]
    fn members_follows_catalog_and_provider_default_is_empty() {
        let p = TestProvider::default();
        let (g, _, n) = one(NodeKind::Exposure);
        let ev = Evaluator::new();
        assert!(ev.members(&g, n, 0, &p).is_empty(), "no catalog, no members");
        add_images(&p, 100..103, "C:/x", (4, 4));
        assert_eq!(ev.members(&g, n, 0, &p), vec![ImageId(100), ImageId(101), ImageId(102)]);
        assert!(ev.contains(&g, n, ImageId(101), &p));
    }

    #[test]
    fn tail_invariant_for_curve_sets_on_every_channel() {
        let p = TestProvider::default();
        let neutral_diagonal = vec![[0.0, 0.0], [1.0, 1.0]];
        let entries = vec![
            CurveEntry { channel: 6, points: vec![[0.0, 0.5], [0.6, 0.7], [1.0, 0.5]] },
            CurveEntry { channel: 2, points: vec![[0.0, 0.05], [0.5, 0.5], [1.0, 0.95]] },
            CurveEntry { channel: 0, points: vec![[0.0, 0.0], [0.5, 0.4], [1.0, 1.0]] },
            CurveEntry { channel: 4, points: vec![[0.0, 0.5], [0.5, 0.35], [1.0, 0.5]] },
            CurveEntry { channel: 5, points: vec![[0.0, 0.5], [0.3, 0.62], [1.0, 0.5]] },
            CurveEntry { channel: 1, points: neutral_diagonal },
        ];
        let (mut g, ids, out) = build(&[(NodeKind::Exposure, &[("ev", 0.5)]), (NodeKind::PointCurve, &[]), (NodeKind::Vignette, &[("amount", 0.5)])]);
        set_data(&mut g, ids[2], A, Some(NodeData::CurveSet(entries)));
        for dims in [None, Some((33, 71))] {
            *p.dims.lock().unwrap() = dims;
            let tp = check_invariant(&g, out, A, &p);
            let channels: Vec<u8> = tp.ops.iter().filter_map(|o| if let TailOp::PointCurve { channel, .. } = o { Some(*channel) } else { None }).collect();
            assert_eq!(channels, vec![0, 2, 5, 6, 4], "one op per non-neutral entry, in application order 0,1,2,3,5,6,4");
        }
    }

    #[test]
    fn split_behind_a_merge_collision_decides_with_the_catalog() {
        let p = TestProvider::default();
        for (id, path, iso) in [(100u64, "C:/a/x.jpg", 200.0f32), (200, "D:/b/x.jpg", 200.0), (101, "C:/a/y.jpg", 200.0)] {
            p.paths.lock().unwrap().insert(id, path.to_string());
            p.flat.lock().unwrap().insert(id, colour(id));
            p.sizes.lock().unwrap().insert(id, (8, 8));
            p.catalog.lock().unwrap().push(ImageId(id));
            set_iso(&p, ImageId(id), iso);
        }
        let mut g = Graph::new();
        let a = folder_source(&mut g, "C:/a");
        let b = folder_source(&mut g, "D:/b");
        let mg = add(&mut g, NodeKind::Merge);
        let sp = add(&mut g, NodeKind::MetadataSplit);
        let exp = add(&mut g, NodeKind::Exposure);
        g.connect(a, mg, 0).unwrap();
        g.connect(b, mg, 1).unwrap();
        g.connect(mg, sp, 0).unwrap();
        g.connect_ports(sp, 0, exp, 0).unwrap();
        set_data(&mut g, sp, A, Some(ranges(&[("low", 50.0, 400.0)])));
        set(&mut g, mg, "collision", A, 1.0); // first input wins
        let ev = Evaluator::new();
        let c = CancelToken::new();
        let mut ev2 = Evaluator::new();
        // Image 200 (b/x.jpg) is dropped by the merge: no split port, nothing behind the split.
        assert!((0..7).all(|port| !ev.contains_output(&g, sp, port, ImageId(200), &p)));
        assert_eq!(ev2.eval(&g, exp, ImageId(200), &p, &c).unwrap_err(), EvalError::NotInCollection { node: exp });
        // The survivors leave through their range.
        for id in [100, 101] {
            assert!(ev.contains_output(&g, sp, 0, ImageId(id), &p));
            assert!(ev2.eval(&g, exp, ImageId(id), &p, &c).is_ok());
        }
    }
}

/// Linear RGB encodes tangent XYZ from [-1,1] to [0,1]. Image Y points down.
fn normals_map(w: u32, h: u32, mut values: Vec<f32>, strength: f32, smoothing: f32, flip_y: bool) -> ImageBuf {
    let mut out = ImageBuf::new(w,h);
    if w == 0 || h == 0 { return out; }
    let radius = smoothing.round().clamp(0.0, 8.0) as i32;
    if radius > 0 {
        let mut blurred = values.clone();
        for y in 0..h { for x in 0..w {
            let mut sum = 0.0; let mut count = 0.0;
            for dy in -radius..=radius { for dx in -radius..=radius {
                let xx = (x as i32 + dx).clamp(0, w as i32 - 1) as usize;
                let yy = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
                sum += values[yy*w as usize+xx]; count += 1.0;
            }}
            blurred[y as usize*w as usize+x as usize] = sum/count;
        }}
        values = blurred;
    }
    let get = |x: u32,y: u32| values[y as usize*w as usize+x as usize];
    for y in 0..h { for x in 0..w {
        let l=x.saturating_sub(1); let r=(x+1).min(w-1); let t=y.saturating_sub(1); let b=(y+1).min(h-1);
        let dx = (get(r,y)-get(l,y)) / (r-l).max(1) as f32;
        let dy = (get(x,b)-get(x,t)) / (b-t).max(1) as f32;
        let nx=-dx*strength; let ny=-dy*strength * if flip_y {-1.0} else {1.0};
        let inv=(nx*nx+ny*ny+1.0).sqrt().recip();
        out.set_pixel(x,y,[0.5+0.5*nx*inv,0.5+0.5*ny*inv,0.5+0.5*inv,1.0]);
    }}
    out
}

#[cfg(test)]
mod normals_tests {
    use super::*;
    #[test] fn flat_and_slopes() {
        let flat = normals_map(3,3,vec![0.5;9],1.0,0.0,false);
        assert_eq!(flat.pixel(1,1),[0.5,0.5,1.0,1.0]);
        let x = normals_map(3,3,vec![0.,1.,2.,0.,1.,2.,0.,1.,2.],1.0,0.0,false);
        assert!(x.pixel(1,1)[0] < 0.5);
        let y = vec![0.,0.,0.,1.,1.,1.,2.,2.,2.];
        assert!(normals_map(3,3,y.clone(),1.0,0.0,false).pixel(1,1)[1] < 0.5);
        assert!(normals_map(3,3,y,1.0,0.0,true).pixel(1,1)[1] > 0.5);
    }
}
