//! Migration of graphs and sub-graphs saved before graph format 3.
//!
//! Format 3 removed the Mask category and every fixed mask port. This module
//! rewrites the stored JSON of an older graph, before it is deserialised, so
//! that it opens and renders as close to before as the format-3 nodes allow:
//!
//! | Old                              | Format 3                                              |
//! |----------------------------------|-------------------------------------------------------|
//! | Gradient Mask                    | Generator (Linear gradient, Output Data)              |
//! | Radial Mask                      | Generator (Radial gradient with a smoothstep ramp)    |
//! | Brush Mask                       | Draw (+ Curves when inverted)                         |
//! | Luminance Mask                   | Black & White (AP1 luma) + Curves                     |
//! | Mask Adjust                      | Curves on Red, Green and Blue                         |
//! | Mask Combine                     | Layer (blend mode, layer 2 stretched)                 |
//! | Hue Mask                         | no equivalent: removed, the consumer's slider rules   |
//! | Depth Estimate / Normals         | Depth Map / Normal Map                                |
//! | fixed mask ports (Exposure ...)  | the same parameter exposed as a control input         |
//!
//! Every deviation is reported as a human-readable note.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::color::AP1_LUMA;
use crate::graph::{GraphError, GRAPH_FORMAT_VERSION, SUBGRAPH_FORMAT_VERSION};

/// First graph / sub-graph format without the old mask nodes and ports.
const MASK_FREE_GRAPH_FORMAT: u64 = 3;
const MASK_FREE_SUBGRAPH_FORMAT: u64 = 2;

type Obj = Map<String, Value>;

/// Grey value of mid-grey on the perceptual axis of the Curves node.
const GREY: f32 = 0.18;
/// Longest curve a Curves node stores.
const MAX_KNOTS: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct E {
    from: u64,
    from_port: u64,
    to: u64,
    to_port: u64,
}

fn bad<T>(msg: impl Into<String>) -> Result<T, GraphError> {
    Err(GraphError::Parse(msg.into()))
}

/// Kinds that produced a mask in format 2 and 1 (Depth Estimate is renamed, not rebuilt).
const MASK_KINDS: [&str; 8] =
    ["GradientMask", "RadialMask", "BrushMask", "LuminanceMask", "HueMask", "MaskAdjust", "MaskCombine", "DepthEstimate"];

fn is_mask_kind(k: &str) -> bool {
    MASK_KINDS.contains(&k)
}

/// Mask kinds that are rebuilt (their input edges are rewired by the migration).
fn is_rebuilt_kind(k: &str) -> bool {
    is_mask_kind(k) && k != "DepthEstimate"
}

fn old_label(kind: &str) -> &str {
    match kind {
        "GradientMask" => "Gradient Mask",
        "RadialMask" => "Radial Mask",
        "BrushMask" => "Brush Mask",
        "LuminanceMask" => "Luminance Mask",
        "HueMask" => "Hue Mask",
        "MaskAdjust" => "Mask Adjust",
        "MaskCombine" => "Mask Combine",
        "DepthEstimate" => "Depth Estimate",
        "Normals" => "Normals",
        "Blur" => "Filter",
        "PointCurve" => "Curves",
        "HighlightsShadows" => "Highlights / Shadows",
        other => other,
    }
}

/// Input ports of a kind in format 2 and earlier (fixed mask ports included).
fn old_input_len(kind: &str) -> u64 {
    match kind {
        "Source" | "GradientMask" | "RadialMask" | "BrushMask" => 0,
        "Exposure" | "Contrast" | "Saturation" | "Sharpen" | "Normals" | "MaskCombine" => 2,
        "Blur" => 3,
        "Merge" => 4,
        "Layer" => 8,
        _ => 1,
    }
}

/// Input ports of a kind in format 3 (exposed control inputs come after them).
fn new_input_len(kind: &str) -> u64 {
    match kind {
        "Source" => 0,
        "Merge" => 4,
        "Layer" => 8,
        _ => 1,
    }
}

/// Parameter a fixed old input port modulated, now an exposed control input.
fn fixed_port_param(kind: &str, port: u64) -> Option<&'static str> {
    match (kind, port) {
        ("Exposure", 1) => Some("ev"),
        ("Contrast" | "Saturation" | "Sharpen", 1) => Some("amount"),
        ("Blur", 1) => Some("radius"),
        ("Blur", 2) => Some("direction"),
        _ => None,
    }
}

fn num(v: &Value) -> Option<f32> {
    v.as_f64().map(|v| v as f32)
}

fn param_of(node: &Obj, image: Option<&str>, name: &str, default: f32) -> f32 {
    let from = |key: &str, scope: Option<&str>| -> Option<f32> {
        let m = node.get(key)?.as_object()?;
        let m = match scope {
            Some(i) => m.get(i)?.as_object()?,
            None => m,
        };
        num(m.get(name)?)
    };
    image.and_then(|i| from("individual", Some(i))).or_else(|| from("all", None)).unwrap_or(default)
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    if e1 <= e0 {
        return if x >= e0 { 1.0 } else { 0.0 };
    }
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Value -> perceptual axis `t = v / (v + 0.18)`.
fn to_t(v: f32) -> f32 {
    let v = v.max(0.0);
    v / (v + GREY)
}

/// Perceptual axis -> value; `t >= 1` is unbounded.
fn to_v(t: f32) -> f32 {
    if t >= 1.0 {
        f32::INFINITY
    } else {
        GREY * t.max(0.0) / (1.0 - t.max(0.0))
    }
}

// ---------------------------------------------------------------- old formulas

fn luminance_mask_value(t: f32, low: f32, high: f32, feather: f32, invert: bool) -> f32 {
    let half = feather.max(0.0) * 0.5;
    let m = if half <= 0.0 {
        if t >= low && t < high {
            1.0
        } else {
            0.0
        }
    } else {
        smoothstep(low - half, low + half, t) * (1.0 - smoothstep(high - half, high + half, t))
    };
    if invert {
        1.0 - m
    } else {
        m
    }
}

fn mask_adjust_value(m: f32, low: f32, high: f32, gamma: f32, invert: bool) -> f32 {
    let m = m.clamp(0.0, 1.0);
    let n = if high <= low {
        if m >= low {
            1.0
        } else {
            0.0
        }
    } else {
        let t = ((m - low) / (high - low)).clamp(0.0, 1.0);
        if gamma == 1.0 {
            t
        } else {
            t.powf(gamma.max(1e-4))
        }
    };
    if invert {
        1.0 - n
    } else {
        n
    }
}

// ---------------------------------------------------------------- curves

/// Curve points `(x, y)` on the perceptual axis that map a value `v` (x = t(v)) to the
/// mask value `f(x)`: `y = t(f)`. Knots are sorted, merged and thinned to the limit.
fn curve_through(mut knots: Vec<f32>, f: impl Fn(f32) -> f32) -> Vec<[f32; 2]> {
    knots.retain(|k| k.is_finite());
    for k in &mut knots {
        *k = k.clamp(0.0, 1.0);
    }
    knots.push(0.0);
    knots.push(1.0);
    knots.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut merged: Vec<f32> = Vec::with_capacity(knots.len());
    for k in knots {
        if merged.last().is_none_or(|l| k - l > 1e-5) {
            merged.push(k);
        }
    }
    // Thin out by removing the closest pair's inner knot until the limit holds.
    while merged.len() > MAX_KNOTS {
        let i = (1..merged.len() - 1)
            .min_by(|&a, &b| (merged[a + 1] - merged[a - 1]).partial_cmp(&(merged[b + 1] - merged[b - 1])).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(1);
        merged.remove(i);
    }
    merged.iter().map(|&x| [x, to_t(f(x).clamp(0.0, 1.0))]).collect()
}

fn neutral_curve_points(points: &[[f32; 2]]) -> bool {
    points.len() == 2 && points[0] == [0.0, 0.0] && points[1] == [1.0, 1.0]
}

/// A Curves data value editing Red, Green and Blue with the same points.
fn rgb_curve_set(points: &[[f32; 2]]) -> Value {
    let entries: Vec<Value> = if neutral_curve_points(points) {
        Vec::new()
    } else {
        (1..=3).map(|c| json!({"channel": c, "points": points})).collect()
    };
    json!({ "CurveSet": entries })
}

/// Curves data that clamps values above 1 (a mask value of 1 is t = 0.847 on the perceptual axis).
fn clamp_curve_set() -> Value {
    // Dense near the corner: a monotone spline flattens before a plateau.
    let mut knots: Vec<f32> = (0..=12).map(|k| to_t(k as f32 / 12.0 * 0.9)).collect();
    knots.extend([0.95, 0.98, 0.99, 1.0].map(to_t));
    rgb_curve_set(&curve_through(knots, |x| to_v(x).min(1.0)))
}

fn adjust_curve(low: f32, high: f32, gamma: f32, invert: bool) -> Vec<[f32; 2]> {
    let (low, high) = (low.clamp(0.0, 1.0), high.clamp(0.0, 1.0));
    if low == 0.0 && high == 1.0 && gamma == 1.0 && !invert {
        // The default adjust does nothing; the control input clamps to 0..1 anyway.
        return vec![[0.0, 0.0], [1.0, 1.0]];
    }
    let mut knots: Vec<f32> = vec![0.0, to_t(low), to_t(high), to_t(1.0), 1.0];
    if high <= low {
        knots.push(to_t((low - 0.002).max(0.0)));
    } else {
        for k in 0..=14 {
            let f = k as f32 / 14.0;
            knots.push(to_t(low + (high - low) * f));
        }
        for k in 1..=3 {
            let f = (k as f32 / 14.0).powi(2);
            knots.push(to_t(low + (high - low) * f));
        }
    }
    curve_through(knots, |x| mask_adjust_value(to_v(x).min(1.0), low, high, gamma, invert))
}

fn luminance_curve(low: f32, high: f32, feather: f32, invert: bool) -> Vec<[f32; 2]> {
    let half = feather.max(0.0) * 0.5;
    let mut knots: Vec<f32> = (0..=8).map(|k| k as f32 / 8.0).collect();
    for edge in [low, high] {
        if half > 0.0 {
            for k in 0..=8 {
                knots.push(edge - half + 2.0 * half * k as f32 / 8.0);
            }
        } else {
            knots.push(edge - 0.001);
            knots.push(edge);
        }
    }
    curve_through(knots, |x| luminance_mask_value(x, low, high, feather, invert))
}

// ---------------------------------------------------------------- the working graph

/// The uses of a mask that share one frame source, each with the chain nodes behind it.
/// Frame source, "always copy" (the frame depends on the mask itself), and the uses.
type FrameGroup = ((u64, u64), bool, Vec<(E, BTreeSet<u64>)>);

/// Node kinds that keep the frame and the collection of their image input (pixels only).
const FRAME_KEEPING: &[&str] = &[
    "Exposure", "Contrast", "Saturation", "WhiteBalance", "HighlightsShadows", "ToneCurve", "PointCurve",
    "HueSaturation", "ColorWheels", "BlackWhite", "Develop", "CaptureEv", "Sharpen", "Clarity", "NoiseReduction",
    "Blur", "Vignette",
];

/// Reads one old parameter (name, default) for All or for one image.
type Param<'a> = &'a dyn Fn(&str, f32) -> f32;

struct Conv {
    params: Vec<(&'static str, f32)>,
    data: Option<Value>,
    notes: Vec<String>,
}

struct Work {
    nodes: BTreeMap<u64, Obj>,
    /// Edges as stored in the old file.
    old: Vec<E>,
    /// Edges of the migrated graph.
    edges: Vec<E>,
    next_id: u64,
    notes: Vec<String>,
    /// Kind of every node as it was stored.
    kinds: HashMap<u64, String>,
    /// For mask nodes: the node whose output now carries the mask (None = removed).
    resolved: HashMap<u64, Option<u64>>,
    in_progress: HashSet<u64>,
    /// Edges created while rebuilding mask nodes.
    produced: Vec<E>,
    /// Generators that should take the frame of their consumer.
    framed: Vec<u64>,
    /// Nodes created or converted by the mask migration.
    chain: BTreeSet<u64>,
}

impl Work {
    fn label(&self, id: u64) -> String {
        let kind = self.kinds.get(&id).map_or("node", String::as_str);
        format!("{} (node {id})", old_label(kind))
    }

    fn note(&mut self, text: String) {
        if !self.notes.contains(&text) {
            self.notes.push(text);
        }
    }

    fn new_node(&mut self, kind: &str, pos: [f64; 2]) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut o = Obj::new();
        o.insert("id".into(), json!(id));
        o.insert("kind".into(), json!(kind));
        o.insert("mode".into(), json!("All"));
        o.insert("all".into(), json!({}));
        o.insert("individual".into(), json!({}));
        o.insert("pos".into(), json!(pos));
        self.nodes.insert(id, o);
        self.chain.insert(id);
        id
    }

    fn pos_of(&self, id: u64) -> [f64; 2] {
        self.nodes
            .get(&id)
            .and_then(|n| n.get("pos"))
            .and_then(Value::as_array)
            .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
            .unwrap_or([0.0, 0.0])
    }

    fn images_of(&self, id: u64) -> Vec<String> {
        self.nodes
            .get(&id)
            .and_then(|n| n.get("individual"))
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Replaces the kind and every parameter and data value of `id` by what `convert`
    /// computes from the old values, for All and for every image with its own values.
    fn rebuild(&mut self, id: u64, new_kind: &str, convert: &dyn Fn(Param) -> Conv) {
        let images = self.images_of(id);
        let Some(node) = self.nodes.get_mut(&id) else { return };
        let all = convert(&|n, d| param_of(node, None, n, d));
        let per_image: Vec<(String, Conv)> =
            images.into_iter().map(|img| (img.clone(), convert(&|n, d| param_of(node, Some(&img), n, d)))).collect();
        let to_obj = |p: &[(&'static str, f32)]| -> Value {
            Value::Object(p.iter().map(|(k, v)| ((*k).to_owned(), json!(*v))).collect())
        };
        node.insert("kind".into(), json!(new_kind));
        node.insert("all".into(), to_obj(&all.params));
        // An image keeps only what differs from All.
        let mut individual = Obj::new();
        let mut individual_data = Obj::new();
        for (img, c) in &per_image {
            let diff: Vec<(&'static str, f32)> =
                c.params.iter().filter(|(k, v)| all.params.iter().find(|(k2, _)| k2 == k).is_none_or(|(_, v2)| v2 != v)).copied().collect();
            if !diff.is_empty() {
                individual.insert(img.clone(), to_obj(&diff));
            }
            if let Some(d) = c.data.as_ref().filter(|d| all.data.as_ref() != Some(*d)) {
                individual_data.insert(img.clone(), d.clone());
            }
        }
        node.insert("individual".into(), Value::Object(individual));
        match &all.data {
            Some(d) => node.insert("data".into(), d.clone()),
            None => node.remove("data"),
        };
        if individual_data.is_empty() {
            node.remove("individual_data");
        } else {
            node.insert("individual_data".into(), Value::Object(individual_data));
        }
        let label = self.label(id);
        for n in &all.notes {
            self.note(format!("{label}: {n}"));
        }
        for (img, c) in &per_image {
            for n in c.notes.iter().filter(|n| !all.notes.contains(n)) {
                self.note(format!("{label}: image {img}: {n}"));
            }
        }
        self.chain.insert(id);
    }

    fn input_edge(&self, node: u64, port: u64) -> Option<E> {
        self.old.iter().find(|e| e.to == node && e.to_port == port).copied()
    }

    /// Where the value arriving through `e` comes from after migration (`None` = removed mask).
    fn source_of(&mut self, e: E) -> Option<(u64, u64)> {
        if is_mask_kind(self.kinds.get(&e.from).map_or("", String::as_str)) {
            let out = self.resolve(e.from)?;
            Some((out, if out == e.from { e.from_port } else { 0 }))
        } else {
            Some((e.from, e.from_port))
        }
    }

    fn drop_node(&mut self, id: u64, why: &str) -> Option<u64> {
        let label = self.label(id);
        self.nodes.remove(&id);
        self.note(format!("{label}: {why}"));
        self.resolved.insert(id, None);
        None
    }

    /// Rebuilds the mask node `id`; returns the node that now outputs the mask.
    fn resolve(&mut self, id: u64) -> Option<u64> {
        if let Some(r) = self.resolved.get(&id) {
            return *r;
        }
        if !self.in_progress.insert(id) {
            return None;
        }
        let kind = self.kinds.get(&id).cloned().unwrap_or_default();
        let out = match kind.as_str() {
            "GradientMask" => {
                self.rebuild(id, "Generator", &convert_gradient);
                self.framed.push(id);
                Some(id)
            }
            "RadialMask" => {
                self.rebuild(id, "Generator", &convert_radial);
                self.framed.push(id);
                Some(id)
            }
            "BrushMask" => self.rebuild_brush(id),
            "LuminanceMask" => self.rebuild_luminance(id),
            "HueMask" => {
                self.drop_node(id, "has no equivalent in graph format 3 and was removed; the effect it controlled now applies without it");
                None
            }
            "MaskAdjust" => self.rebuild_adjust(id),
            "MaskCombine" => self.rebuild_combine(id),
            "DepthEstimate" => {
                if let Some(n) = self.nodes.get_mut(&id) {
                    n.insert("kind".into(), json!("DepthMap"));
                }
                Some(id)
            }
            _ => None,
        };
        self.in_progress.remove(&id);
        self.resolved.insert(id, out);
        out
    }

    fn rebuild_brush(&mut self, id: u64) -> Option<u64> {
        let images = self.images_of(id);
        let node = self.nodes.get(&id)?;
        let inv_all = param_of(node, None, "invert", 0.0) >= 0.5;
        let inv_img: Vec<(String, bool)> = images.iter().map(|i| (i.clone(), param_of(node, Some(i), "invert", 0.0) >= 0.5)).collect();
        let any_invert = inv_all || inv_img.iter().any(|(_, v)| *v);
        let pos = self.pos_of(id);
        // The node itself becomes the Draw; an inverted brush gets a Curves node behind it.
        let draw = if any_invert {
            let d = self.new_node("Draw", [pos[0] - 180.0, pos[1]]);
            let src = self.nodes.get(&id)?.clone();
            let dn = self.nodes.get_mut(&d)?;
            for key in ["mode", "data", "individual_data"] {
                if let Some(v) = src.get(key) {
                    dn.insert(key.to_owned(), v.clone());
                }
            }
            self.framed.push(d);
            d
        } else {
            self.framed.push(id);
            id
        };
        let strip = |n: &mut Obj| {
            for key in ["all", "individual"] {
                if let Some(Value::Object(m)) = n.get_mut(key) {
                    if key == "all" {
                        m.remove("invert");
                    } else {
                        for v in m.values_mut() {
                            if let Value::Object(p) = v {
                                p.remove("invert");
                            }
                        }
                    }
                }
            }
        };
        if let Some(n) = self.nodes.get_mut(&draw) {
            strip(n);
            n.insert("kind".into(), json!("Draw"));
        }
        self.chain.insert(draw);
        if !any_invert {
            return Some(id);
        }
        // n -> Curves (inversion), data per image where it differs.
        let invert = rgb_curve_set(&adjust_curve(0.0, 1.0, 1.0, true));
        let plain = rgb_curve_set(&adjust_curve(0.0, 1.0, 1.0, false));
        let n = self.nodes.get_mut(&id)?;
        for key in ["data", "individual_data", "all", "individual"] {
            n.remove(key);
        }
        n.insert("kind".into(), json!("PointCurve"));
        n.insert("all".into(), json!({}));
        n.insert("individual".into(), json!({}));
        n.insert("data".into(), if inv_all { invert.clone() } else { plain.clone() });
        let per: Obj = inv_img.iter().map(|(i, v)| (i.clone(), if *v { invert.clone() } else { plain.clone() })).collect();
        if !per.is_empty() {
            n.insert("individual_data".into(), Value::Object(per));
        }
        self.chain.insert(id);
        self.produced.push(E { from: draw, from_port: 0, to: id, to_port: 0 });
        Some(id)
    }

    fn rebuild_luminance(&mut self, id: u64) -> Option<u64> {
        let Some(e) = self.input_edge(id, 0) else {
            return self.drop_node(id, "has no input image; the mask could not be rebuilt and was removed");
        };
        let Some((s, sp)) = self.source_of(e) else {
            return self.drop_node(id, "its input image could not be rebuilt; it was removed");
        };
        let pos = self.pos_of(id);
        // Grey from the AP1 luminance, then the range curve on Red, Green and Blue.
        let bw = self.new_node("BlackWhite", [pos[0] - 180.0, pos[1]]);
        if let Some(n) = self.nodes.get_mut(&bw) {
            n.insert("all".into(), json!({"red": AP1_LUMA[0], "green": AP1_LUMA[1], "blue": AP1_LUMA[2]}));
        }
        self.rebuild(id, "PointCurve", &|p| {
            let (low, high, feather) = (p("low", 0.0), p("high", 1.0), p("feather", 0.1));
            let invert = p("invert", 0.0) >= 0.5;
            Conv { params: Vec::new(), data: Some(rgb_curve_set(&luminance_curve(low, high, feather, invert))), notes: Vec::new() }
        });
        self.produced.push(E { from: s, from_port: sp, to: bw, to_port: 0 });
        self.produced.push(E { from: bw, from_port: 0, to: id, to_port: 0 });
        Some(id)
    }

    fn rebuild_adjust(&mut self, id: u64) -> Option<u64> {
        let Some(e) = self.input_edge(id, 0) else {
            return self.drop_node(id, "has no input mask; it was removed");
        };
        let Some((s, sp)) = self.source_of(e) else {
            return self.drop_node(id, "its input mask was removed, so it was removed too");
        };
        self.rebuild(id, "PointCurve", &|p| {
            let (low, high, gamma) = (p("low", 0.0), p("high", 1.0), p("gamma", 1.0));
            let invert = p("invert", 0.0) >= 0.5;
            Conv { params: Vec::new(), data: Some(rgb_curve_set(&adjust_curve(low, high, gamma, invert))), notes: Vec::new() }
        });
        self.produced.push(E { from: s, from_port: sp, to: id, to_port: 0 });
        Some(id)
    }

    fn rebuild_combine(&mut self, id: u64) -> Option<u64> {
        let (Some(ea), Some(eb)) = (self.input_edge(id, 0), self.input_edge(id, 1)) else {
            return self.drop_node(id, "needs two input masks and one is not connected; it was removed");
        };
        let a = self.source_of(ea);
        let b = self.source_of(eb);
        let (Some((sa, pa)), Some((sb, pb))) = (a, b) else {
            return self.drop_node(id, "one of its input masks was removed, so it was removed too");
        };
        let images = self.images_of(id);
        let node = self.nodes.get(&id)?;
        let mode = param_of(node, None, "mode", 0.0).round() as i32;
        let differs = images.iter().any(|i| param_of(node, Some(i), "mode", 0.0).round() as i32 != mode);
        let pos = self.pos_of(id);
        if differs {
            let label = self.label(id);
            self.note(format!("{label}: the per-image mode is not kept; the mode for All images is used"));
        }
        let layer = |blend: f32| move |_: Param| Conv { params: vec![("l2_blend", blend), ("l2_fit", 3.0)], data: None, notes: Vec::new() };
        match mode {
            // The old combine clamped to 0..1 after every step; a Layer does not.
            // Subtract: a - min(a, b) is never negative.
            2 => {
                let h = self.new_node("Layer", [pos[0] - 180.0, pos[1] + 60.0]);
                if let Some(n) = self.nodes.get_mut(&h) {
                    n.insert("all".into(), json!({"l2_blend": 6.0, "l2_fit": 3.0}));
                }
                self.produced.push(E { from: sa, from_port: pa, to: h, to_port: 0 });
                self.produced.push(E { from: sb, from_port: pb, to: h, to_port: 1 });
                self.rebuild(id, "Layer", &layer(2.0));
                self.produced.push(E { from: sa, from_port: pa, to: id, to_port: 0 });
                self.produced.push(E { from: h, from_port: 0, to: id, to_port: 1 });
            }
            // Add: the Layer, then a curve that clamps at 1.
            1 => {
                let l = self.new_node("Layer", [pos[0] - 180.0, pos[1]]);
                if let Some(n) = self.nodes.get_mut(&l) {
                    n.insert("all".into(), json!({"l2_blend": 1.0, "l2_fit": 3.0}));
                }
                self.produced.push(E { from: sa, from_port: pa, to: l, to_port: 0 });
                self.produced.push(E { from: sb, from_port: pb, to: l, to_port: 1 });
                self.rebuild(id, "PointCurve", &|_| Conv { params: Vec::new(), data: Some(clamp_curve_set()), notes: Vec::new() });
                self.produced.push(E { from: l, from_port: 0, to: id, to_port: 0 });
            }
            // Multiply, Maximum and Minimum of masks within 0..1 stay within 0..1.
            m => {
                let blend = match m {
                    3 => 7.0,
                    4 => 6.0,
                    _ => 3.0,
                };
                self.rebuild(id, "Layer", &layer(blend));
                self.produced.push(E { from: sa, from_port: pa, to: id, to_port: 0 });
                self.produced.push(E { from: sb, from_port: pb, to: id, to_port: 1 });
            }
        }
        Some(id)
    }

    // ------------------------------------------------------------ consumers

    /// Slot of the exposed input for `parameter`, created when missing.
    fn ensure_slot(&mut self, node: u64, parameter: &str) -> usize {
        let Some(n) = self.nodes.get_mut(&node) else { return 0 };
        let list = n.entry("exposed_inputs").or_insert_with(|| json!([]));
        let Some(arr) = list.as_array_mut() else { return 0 };
        if let Some(i) = arr.iter().position(|e| e.get("parameter").and_then(Value::as_str) == Some(parameter)) {
            return i;
        }
        arr.push(json!({"parameter": parameter, "ty": "Images"}));
        arr.len() - 1
    }

    fn slot_parameter(&self, node: u64, slot: usize) -> Option<String> {
        self.nodes.get(&node)?.get("exposed_inputs")?.as_array()?.get(slot)?.get("parameter")?.as_str().map(str::to_owned)
    }

    /// Rewires every edge that ends in a node whose input layout changed.
    fn migrate_consumers(&mut self) {
        let old_edges = self.old.clone();
        let mut out: Vec<E> = Vec::new();
        // (consumer, slot) pairs that carry a fixed-port mask.
        let mut taken: HashSet<(u64, usize)> = HashSet::new();
        // Blur nodes whose exposed radius was wired in the old file.
        let mut blur_range: BTreeSet<u64> = BTreeSet::new();

        let ordered: Vec<(bool, E)> = old_edges
            .iter()
            .map(|e| (fixed_port_param(self.kinds.get(&e.to).map_or("", String::as_str), e.to_port).is_some(), *e))
            .collect();
        for pass_fixed in [true, false] {
            for (fixed, e) in ordered.iter().filter(|(f, _)| *f == pass_fixed) {
                let e = *e;
                let Some(to_kind) = self.kinds.get(&e.to).cloned() else { continue };
                if !self.nodes.contains_key(&e.to) || is_rebuilt_kind(&to_kind) || to_kind == "Normals" {
                    continue;
                }
                let src = self.source_of(e);
                let (old_len, new_len) = (old_input_len(&to_kind), new_input_len(&to_kind));
                if *fixed {
                    let param = fixed_port_param(&to_kind, e.to_port).unwrap_or("");
                    let to_label = self.label(e.to);
                    let Some((s, sp)) = src else {
                        self.note(format!(
                            "{to_label}: the control input \"{param}\" was connected to {} which was removed; the slider now applies everywhere",
                            self.label(e.from)
                        ));
                        continue;
                    };
                    let slot = self.ensure_slot(e.to, param);
                    taken.insert((e.to, slot));
                    out.push(E { from: s, from_port: sp, to: e.to, to_port: new_len + slot as u64 });
                } else if e.to_port < old_len {
                    // An ordinary image input keeps its number.
                    if let Some((s, sp)) = src {
                        if e.to_port < new_len {
                            out.push(E { from: s, from_port: sp, to: e.to, to_port: e.to_port });
                        }
                    } else {
                        let (a, b) = (self.label(e.to), self.label(e.from));
                        self.note(format!("{a}: its input from {b} was removed"));
                    }
                } else {
                    // An exposed input of the old file: slot numbers keep, ports shift.
                    let slot = (e.to_port - old_len) as usize;
                    let Some(param) = self.slot_parameter(e.to, slot) else { continue };
                    let to_label = self.label(e.to);
                    let Some((s, sp)) = src else {
                        self.note(format!("{to_label}: the control input \"{param}\" came from {} which was removed", self.label(e.from)));
                        continue;
                    };
                    if taken.contains(&(e.to, slot)) {
                        let port = new_len + slot as u64;
                        if to_kind == "Blur" && param == "radius" {
                            // Old: radius = maximum * exposed map * mask. Multiply the two maps.
                            if let Some(idx) = out.iter().position(|o| o.to == e.to && o.to_port == port) {
                                let fixed = out[idx];
                                let pos = self.pos_of(e.to);
                                let l = self.new_node("Layer", [pos[0] - 200.0, pos[1] + 80.0]);
                                if let Some(n) = self.nodes.get_mut(&l) {
                                    n.insert("all".into(), json!({"l2_blend": 3.0, "l2_fit": 3.0}));
                                }
                                out[idx] = E { from: l, from_port: 0, to: e.to, to_port: port };
                                out.push(E { from: fixed.from, from_port: fixed.from_port, to: l, to_port: 0 });
                                out.push(E { from: s, from_port: sp, to: l, to_port: 1 });
                                blur_range.insert(e.to);
                                self.note(format!(
                                    "{to_label}: the exposed radius input and the radius mask port were both connected; the two maps are multiplied and the radius slider is at its maximum, as before"
                                ));
                                continue;
                            }
                        }
                        let extra = if param == "ev" { "; the old result (the exposed map set the stops, the mask the strength) cannot be expressed in format 3 and differs" } else { "" };
                        self.note(format!(
                            "{to_label}: the exposed input \"{param}\" was connected twice (fixed mask port and exposed input); only the mask port is kept{extra}"
                        ));
                        continue;
                    }
                    out.push(E { from: s, from_port: sp, to: e.to, to_port: new_len + slot as u64 });
                    match (to_kind.as_str(), param.as_str()) {
                        ("Blur", "radius") => {
                            blur_range.insert(e.to);
                        }
                        ("Develop" | "Exposure", _) => self.note(format!(
                            "{to_label}: the exposed input \"{param}\" used to map the control over the whole parameter range; in format 3 it scales the slider (black = neutral, white = slider value). Check the slider"
                        )),
                        _ => {}
                    }
                }
            }
        }
        // Old Blur with an exposed radius: the slider was the maximum (1000), now it scales the map.
        for id in &blur_range {
            if let Some(n) = self.nodes.get_mut(id) {
                if let Some(Value::Object(all)) = n.get_mut("all") {
                    all.insert("radius".into(), json!(1000.0));
                }
                if let Some(Value::Object(ind)) = n.get_mut("individual") {
                    for v in ind.values_mut() {
                        if let Value::Object(p) = v {
                            p.remove("radius");
                        }
                    }
                }
            }
        }
        self.edges = out;
    }

    /// Normals -> Normal Map: the depth input and the height input become one image input.
    fn migrate_normals(&mut self) {
        let ids: Vec<u64> = self.nodes.keys().copied().filter(|id| self.kinds.get(id).is_some_and(|k| k == "Normals")).collect();
        for id in ids {
            let label = self.label(id);
            let images = self.images_of(id);
            let Some(node) = self.nodes.get_mut(&id) else { continue };
            let mode = param_of(node, None, "mode", 0.0).round() as i32;
            if images.iter().any(|i| param_of(node, Some(i), "mode", 0.0).round() as i32 != mode) {
                self.notes.push(format!("{label}: the per-image input choice is not available in format 3; the choice for All images is used"));
            }
            if let Some(Value::Object(all)) = node.get_mut("all") {
                all.remove("mode");
            }
            if let Some(Value::Object(ind)) = node.get_mut("individual") {
                for v in ind.values_mut() {
                    if let Value::Object(p) = v {
                        p.remove("mode");
                    }
                }
            }
            node.insert("kind".into(), json!("NormalMap"));
            let used = if mode == 1 { 1 } else { 0 };
            let other = 1 - used;
            if let Some(e) = self.input_edge(id, other) {
                let l = self.label(e.from);
                self.note(format!("{label}: the unused {} input from {l} was disconnected", if other == 0 { "depth" } else { "height" }));
            }
            let Some(e) = self.input_edge(id, used) else { continue };
            if let Some((s, sp)) = self.source_of(e) {
                self.produced.push(E { from: s, from_port: sp, to: id, to_port: 0 });
            } else {
                let l = self.label(e.from);
                self.note(format!("{label}: its input from {l} was removed"));
            }
        }
    }

    // ------------------------------------------------------------ frames

    fn reaches(&self, from: u64, target: u64) -> bool {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        while let Some(n) = stack.pop() {
            if n == target {
                return true;
            }
            if !seen.insert(n) {
                continue;
            }
            stack.extend(self.edges.iter().filter(|e| e.from == n).map(|e| e.to));
        }
        false
    }

    /// Chain nodes upstream of `q`, `q` included.
    fn chain_ancestors(&self, q: u64) -> BTreeSet<u64> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![q];
        while let Some(n) = stack.pop() {
            if !self.chain.contains(&n) || !seen.insert(n) {
                continue;
            }
            stack.extend(self.edges.iter().filter(|e| e.to == n).map(|e| e.from));
        }
        seen
    }

    /// Copies the nodes of `set` (and the edges into them); returns old id -> new id.
    fn clone_nodes(&mut self, set: &BTreeSet<u64>) -> HashMap<u64, u64> {
        let mut map = HashMap::new();
        for &old in set {
            let Some(mut n) = self.nodes.get(&old).cloned() else { continue };
            let id = self.next_id;
            self.next_id += 1;
            n.insert("id".into(), json!(id));
            let p = self.pos_of(old);
            let mut y = p[1] + 70.0;
            while self.nodes.values().any(|m| m.get("pos").and_then(Value::as_array).is_some_and(|a| {
                a.first().and_then(Value::as_f64) == Some(p[0]) && a.get(1).and_then(Value::as_f64) == Some(y)
            })) {
                y += 70.0;
            }
            n.insert("pos".into(), json!([p[0], y]));
            self.nodes.insert(id, n);
            self.chain.insert(id);
            if let Some(k) = self.kinds.get(&old).cloned() {
                self.kinds.insert(id, k);
            }
            if self.framed.contains(&old) {
                self.framed.push(id);
            }
            map.insert(old, id);
        }
        for e in self.edges.clone() {
            // The frame of a generated mask is wired per copy afterwards.
            if self.framed.contains(&e.to) && e.to_port == 0 {
                continue;
            }
            if let Some(&to) = map.get(&e.to) {
                self.edges.push(E { from: map.get(&e.from).copied().unwrap_or(e.from), from_port: e.from_port, to, to_port: e.to_port });
            }
        }
        map
    }

    /// The node whose frame and photos a source really carries: walks up through nodes that only
    /// change pixels (Exposure, Curves, ...), so consumers in one pixel chain share one frame.
    fn frame_root(&self, mut s: (u64, u64)) -> (u64, u64) {
        for _ in 0..10_000 {
            if s.1 != 0 || self.chain.contains(&s.0) {
                break;
            }
            let kind = self.nodes.get(&s.0).and_then(|n| n.get("kind")).and_then(Value::as_str).unwrap_or("");
            if !FRAME_KEEPING.contains(&kind) {
                break;
            }
            let Some(up) = self.edges.iter().find(|x| x.to == s.0 && x.to_port == 0) else { break };
            s = (up.from, up.from_port);
        }
        s
    }

    /// Generated masks take the frame (and so the photos) of the nodes they feed, like the old masks did.
    /// A mask used with different frames is duplicated, one copy per frame source.
    fn wire_frames(&mut self) {
        // The only Source of the graph frames a Normal Map that is fed by a mask alone.
        let sources: Vec<u64> =
            self.nodes.iter().filter(|(_, n)| n.get("kind").and_then(Value::as_str) == Some("Source")).map(|(id, _)| *id).collect();
        let single_source = (sources.len() == 1).then(|| (sources[0], 0));
        let mut uses: Vec<(E, Option<(u64, u64)>)> = Vec::new();
        for e in &self.edges {
            if !self.chain.contains(&e.from) || self.chain.contains(&e.to) {
                continue;
            }
            let kind = self.nodes.get(&e.to).and_then(|n| n.get("kind")).and_then(Value::as_str).unwrap_or("");
            let src = if kind == "NormalMap" && e.to_port == 0 {
                single_source
            } else {
                self.edges.iter().find(|x| x.to == e.to && x.to_port == 0 && e.to_port != 0).map(|x| (x.from, x.from_port))
            };
            uses.push((*e, src));
        }
        uses.sort_by_key(|(e, _)| (e.to, e.to_port, e.from));
        // Each use with the frame source it can take (one the mask does not depend on).
        let mut groups: Vec<FrameGroup> = Vec::new();
        for (e, src) in uses {
            let anc = self.chain_ancestors(e.from);
            let gens: Vec<u64> = anc.iter().copied().filter(|g| self.framed.contains(g)).collect();
            if gens.is_empty() {
                continue;
            }
            let Some(s) = src else { continue };
            let root = self.frame_root(s);
            // A frame that comes after the mask (e.g. behind Crop / Rotate) cannot frame the original
            // mask without a cycle: that use gets its own copy, framed by its own input.
            let (key, forced) = if gens.iter().any(|g| self.reaches(*g, root.0)) { (s, true) } else { (root, false) };
            match groups.iter_mut().find(|g| g.0 == key && g.1 == forced) {
                Some(g) => g.2.push((e, anc)),
                None => groups.push((key, forced, vec![(e, anc)])),
            }
        }
        // Ordinary groups first, so that a forced copy never takes the original nodes.
        groups.sort_by_key(|g| g.1);
        let mut used: BTreeSet<u64> = BTreeSet::new();
        for (src, forced, members) in groups {
            let union: BTreeSet<u64> = members.iter().flat_map(|(_, a)| a.iter().copied()).collect();
            let shared = forced || !union.is_disjoint(&used);
            if !forced {
                used.extend(union.iter().copied());
            }
            let map = if shared { self.clone_nodes(&union) } else { HashMap::new() };
            if shared {
                let masks: Vec<u64> = union.iter().copied().filter(|g| self.kinds.get(g).is_some_and(|k| is_mask_kind(k))).collect();
                for g in masks {
                    let l = self.label(g);
                    if forced {
                        let c = members.first().map(|(e, _)| self.label(e.to)).unwrap_or_default();
                        self.note(format!("{l}: {c} saw it in a frame that comes after the mask (e.g. behind Crop / Rotate); a copy was made in that frame, check its placement"));
                    } else {
                        self.note(format!("{l}: it feeds nodes with different frames; a copy was made for each frame"));
                    }
                }
                for (e, _) in &members {
                    if let Some(i) = self.edges.iter().position(|x| x == e) {
                        self.edges[i].from = map.get(&e.from).copied().unwrap_or(e.from);
                    }
                }
            }
            for g in union.iter().filter(|g| self.framed.contains(g)) {
                let g2 = map.get(g).copied().unwrap_or(*g);
                if !self.edges.iter().any(|x| x.to == g2 && x.to_port == 0) {
                    self.edges.push(E { from: src.0, from_port: src.1, to: g2, to_port: 0 });
                }
            }
        }
        // A generated mask that still has no image input renders at its own size and is stretched.
        let unframed: Vec<u64> = self
            .framed
            .iter()
            .copied()
            .filter(|g| self.nodes.contains_key(g) && !self.edges.iter().any(|x| x.to == *g && x.to_port == 0))
            .filter(|g| self.edges.iter().any(|x| x.from == *g))
            .collect();
        for g in unframed {
            let l = self.label(g);
            self.note(format!("{l}: has no image input to take its size and photos from; connect the image it should match"));
        }
    }
}

fn convert_gradient(p: Param) -> Conv {
    let angle = p("angle", 90.0);
    let center = p("center", 0.5);
    let width = p("width", 0.5);
    let a = if angle > 180.0 { angle - 360.0 } else { angle };
    let (sin, cos) = angle.to_radians().sin_cos();
    // Move the gradient origin along the gradient direction so that the old centre is at t = 0.5.
    let cx = 0.5 + (center - 0.5) * cos;
    let cy = 0.5 + (center - 0.5) * sin;
    let mut notes = Vec::new();
    if width < 0.01 {
        notes.push("the hard edge became a 1 % wide transition (the smallest Generator gradient scale)".to_owned());
    }
    Conv {
        params: vec![
            ("gradient_angle", a.clamp(-180.0, 180.0)),
            ("gradient_scale", (width * 100.0).clamp(1.0, 400.0)),
            ("gradient_center_x", (cx * 100.0).clamp(-100.0, 200.0)),
            ("gradient_center_y", (cy * 100.0).clamp(-100.0, 200.0)),
            ("output", 1.0),
        ],
        data: None,
        notes,
    }
}

fn convert_radial(p: Param) -> Conv {
    let (cx, cy) = (p("center_x", 0.5), p("center_y", 0.5));
    let (rx, ry) = (p("radius_x", 0.3), p("radius_y", 0.3));
    let feather = p("feather", 0.5).clamp(0.0, 1.0);
    let angle = p("angle", 0.0);
    let invert = p("invert", 0.0) >= 0.5;
    let mut notes = Vec::new();
    let r = if (rx - ry).abs() <= 0.01 * rx.max(ry).max(1e-6) {
        rx
    } else {
        notes.push(format!(
            "Radius X ({rx:.2}) and Y ({ry:.2}) differ; the Generator radial gradient is a circle in image proportions, so the geometric mean is used{}",
            if angle != 0.0 { " and the angle is dropped" } else { "" }
        ));
        (rx.max(0.0) * ry.max(0.0)).sqrt()
    };
    if r <= 0.0 {
        notes.push("a zero radius gives an empty mask; a 1 % radius is used".to_owned());
    }
    // Plateau up to d = 1 - feather, then a smoothstep to 0 at d = 1.
    let edge = (1.0 - feather).clamp(0.001, 0.999);
    let (inside, outside) = if invert { ([0.0; 3], [1.0; 3]) } else { ([1.0; 3], [0.0; 3]) };
    let colors = if feather >= 0.999 {
        json!([{"position": 0.0, "color": inside, "midpoint": 0.5}, {"position": 1.0, "color": outside, "midpoint": 0.5}])
    } else {
        json!([
            {"position": 0.0, "color": inside, "midpoint": 0.5},
            {"position": edge, "color": inside, "midpoint": 0.5},
            {"position": 1.0, "color": outside, "midpoint": 0.5}
        ])
    };
    let opacities = json!([{"position": 0.0, "opacity": 1.0, "midpoint": 0.5}, {"position": 1.0, "opacity": 1.0, "midpoint": 0.5}]);
    Conv {
        params: vec![
            ("gradient_style", 1.0),
            ("gradient_scale", (2.0 * r * 100.0).clamp(1.0, 400.0)),
            ("gradient_center_x", (cx * 100.0).clamp(-100.0, 200.0)),
            ("gradient_center_y", (cy * 100.0).clamp(-100.0, 200.0)),
            ("ramp_interpolation", 1.0),
            ("output", 1.0),
        ],
        data: Some(json!({"GradientRamp": {"colors": colors, "opacities": opacities}})),
        notes,
    }
}

// ---------------------------------------------------------------- entry points

fn parse_edges(v: &Value, subgraph: bool) -> Result<Vec<E>, GraphError> {
    let Some(arr) = v.as_array() else { return bad("graph file: \"edges\" is not a list") };
    let get = |e: &Value, k: &str, default: Option<u64>| -> Result<u64, GraphError> {
        match e.get(k) {
            Some(x) => x.as_u64().ok_or_else(|| GraphError::Parse(format!("graph file: edge field \"{k}\" is not a number"))),
            None => default.ok_or_else(|| GraphError::Parse(format!("graph file: edge field \"{k}\" is missing"))),
        }
    };
    arr.iter()
        .map(|e| {
            let shift = u64::from(subgraph);
            Ok(E { from: get(e, "from", None)? + shift, from_port: get(e, "from_port", Some(0))?, to: get(e, "to", None)? + shift, to_port: get(e, "to_port", None)? })
        })
        .collect()
}

fn edges_json(edges: &[E], map: &dyn Fn(u64) -> u64) -> Value {
    Value::Array(
        edges
            .iter()
            .map(|e| {
                let mut o = Obj::new();
                o.insert("from".into(), json!(map(e.from)));
                if e.from_port != 0 {
                    o.insert("from_port".into(), json!(e.from_port));
                }
                o.insert("to".into(), json!(map(e.to)));
                o.insert("to_port".into(), json!(e.to_port));
                Value::Object(o)
            })
            .collect(),
    )
}

fn run(nodes: BTreeMap<u64, Obj>, edges: Vec<E>, next_id: u64) -> Result<Work, GraphError> {
    let kinds: HashMap<u64, String> = nodes
        .iter()
        .map(|(id, n)| Ok((*id, n.get("kind").and_then(Value::as_str).ok_or_else(|| GraphError::Parse(format!("node {id} has no kind")))?.to_owned())))
        .collect::<Result<_, GraphError>>()?;
    let mut w = Work {
        nodes,
        old: edges,
        edges: Vec::new(),
        next_id,
        notes: Vec::new(),
        kinds,
        resolved: HashMap::new(),
        in_progress: HashSet::new(),
        produced: Vec::new(),
        framed: Vec::new(),
        chain: BTreeSet::new(),
    };
    // Exposed inputs carry images only.
    for n in w.nodes.values_mut() {
        if let Some(Value::Array(list)) = n.get_mut("exposed_inputs") {
            for e in list.iter_mut().filter_map(Value::as_object_mut) {
                e.insert("ty".into(), json!("Images"));
            }
        }
    }
    let masks: Vec<u64> = w.kinds.iter().filter(|(_, k)| is_mask_kind(k)).map(|(id, _)| *id).collect();
    let mut masks = masks;
    masks.sort_unstable();
    for id in masks {
        w.resolve(id);
    }
    w.migrate_consumers();
    w.migrate_normals();
    let produced = std::mem::take(&mut w.produced);
    w.edges.extend(produced);
    w.wire_frames();
    // Nodes that still carry an old kind name (Normals handled above) are not expected.
    Ok(w)
}

/// Develop's "Tone processing" choice (Legacy / Local v1, `tone_version`) became the Legacy tone toggle
/// (`legacy_tone`, off = local). Files up to graph format 3 / sub-graph format 2 without the choice meant
/// Legacy (the default was stored sparsely), so those get the toggle on; an explicit Local v1 keeps the
/// default (off). Graph format 4 / sub-graph format 3 mark files written with the toggle.
fn tone_step<'a>(nodes: impl Iterator<Item = (u64, &'a mut Obj)>, absent_means_legacy: bool, notes: &mut Vec<String>) {
    for (id, n) in nodes {
        if n.get("kind").and_then(Value::as_str) != Some("Develop") {
            continue;
        }
        let mut local_v1 = false;
        if let Some(Value::Object(all)) = n.get_mut("all") {
            let tv = all.remove("tone_version").and_then(|v| num(&v));
            local_v1 |= tv.is_some_and(|v| v >= 0.5);
            let legacy = tv.map_or(absent_means_legacy, |v| v < 0.5);
            if legacy && !all.contains_key("legacy_tone") {
                all.insert("legacy_tone".into(), json!(1.0));
            }
        }
        if let Some(Value::Object(ind)) = n.get_mut("individual") {
            for v in ind.values_mut() {
                if let Value::Object(p) = v {
                    if let Some(tv) = p.remove("tone_version").and_then(|v| num(&v)) {
                        local_v1 |= tv >= 0.5;
                        p.entry("legacy_tone").or_insert_with(|| json!(if tv < 0.5 { 1.0 } else { 0.0 }));
                    }
                }
            }
        }
        if local_v1 {
            let text = format!("Develop (node {id}): Local tone v1 now uses the improved local tone operator; the look can differ slightly");
            if !notes.contains(&text) {
                notes.push(text);
            }
        }
    }
}

/// Rewrites a graph document of format 1 or 2 in place into format 3 and returns the notes.
/// Documents of format 3 and later are left untouched.
pub(crate) fn migrate_graph(root: &mut Value) -> Result<Vec<String>, GraphError> {
    let Some(obj) = root.as_object_mut() else { return Ok(Vec::new()) };
    let Some(version) = obj.get("format_version").and_then(Value::as_u64) else { return Ok(Vec::new()) };
    let mut tone_notes = Vec::new();
    if version < u64::from(GRAPH_FORMAT_VERSION) {
        if let Some(Value::Object(nodes)) = obj.get_mut("nodes") {
            let it = nodes.iter_mut().filter_map(|(k, v)| Some((k.parse::<u64>().ok()?, v.as_object_mut()?)));
            tone_step(it, true, &mut tone_notes);
        }
    }
    if version >= MASK_FREE_GRAPH_FORMAT {
        if version < u64::from(GRAPH_FORMAT_VERSION) {
            obj.insert("format_version".into(), json!(GRAPH_FORMAT_VERSION));
        }
        return Ok(tone_notes);
    }
    let (Some(nodes_v), Some(edges_v)) = (obj.get("nodes"), obj.get("edges")) else { return Ok(tone_notes) };
    let Some(nodes_o) = nodes_v.as_object() else { return Ok(tone_notes) };
    let mut nodes = BTreeMap::new();
    for (key, v) in nodes_o {
        let Some(o) = v.as_object() else { return bad(format!("node {key} is not an object")) };
        let id: u64 = o.get("id").and_then(Value::as_u64).or_else(|| key.parse().ok()).ok_or_else(|| GraphError::Parse(format!("node {key} has no id")))?;
        if key.parse::<u64>().ok() != Some(id) {
            // Leave the mismatch to the normal consistency check.
            return Ok(tone_notes);
        }
        nodes.insert(id, o.clone());
    }
    let edges = parse_edges(edges_v, false)?;
    let next_id = obj.get("next_id").and_then(Value::as_u64).unwrap_or(1).max(nodes.keys().max().map_or(1, |m| m + 1));
    let w = run(nodes, edges, next_id)?;
    let nodes_out: Obj = w.nodes.iter().map(|(id, n)| (id.to_string(), Value::Object(n.clone()))).collect();
    obj.insert("nodes".into(), Value::Object(nodes_out));
    obj.insert("edges".into(), edges_json(&w.edges, &|i| i));
    obj.insert("next_id".into(), json!(w.next_id));
    obj.insert("format_version".into(), json!(GRAPH_FORMAT_VERSION));
    tone_notes.extend(w.notes);
    Ok(tone_notes)
}

/// Rewrites a sub-graph document of format 1 in place into format 2 and returns the notes.
pub(crate) fn migrate_subgraph(root: &mut Value) -> Result<Vec<String>, GraphError> {
    let Some(obj) = root.as_object_mut() else { return Ok(Vec::new()) };
    let Some(version) = obj.get("format_version").and_then(Value::as_u64) else { return Ok(Vec::new()) };
    let mut tone_notes = Vec::new();
    if version < u64::from(SUBGRAPH_FORMAT_VERSION) {
        if let Some(Value::Array(nodes)) = obj.get_mut("nodes") {
            let it = nodes.iter_mut().enumerate().filter_map(|(i, v)| Some((i as u64 + 1, v.as_object_mut()?)));
            tone_step(it, true, &mut tone_notes);
        }
    }
    if version >= MASK_FREE_SUBGRAPH_FORMAT {
        if version < u64::from(SUBGRAPH_FORMAT_VERSION) {
            obj.insert("format_version".into(), json!(SUBGRAPH_FORMAT_VERSION));
        }
        return Ok(tone_notes);
    }
    let (Some(nodes_v), Some(edges_v)) = (obj.get("nodes"), obj.get("edges")) else { return Ok(tone_notes) };
    let Some(list) = nodes_v.as_array() else { return Ok(tone_notes) };
    let mut nodes = BTreeMap::new();
    for (i, v) in list.iter().enumerate() {
        let Some(o) = v.as_object() else { return bad(format!("sub-graph node {i} is not an object")) };
        let mut o = o.clone();
        o.insert("id".into(), json!(i as u64 + 1));
        o.entry("all").or_insert_with(|| json!({}));
        o.entry("individual").or_insert_with(|| json!({}));
        nodes.insert(i as u64 + 1, o);
    }
    let edges = parse_edges(edges_v, true)?;
    let count = list.len() as u64;
    let w = run(nodes, edges, count + 1)?;
    // Surviving nodes keep their order; new ones follow.
    let order: Vec<u64> = w.nodes.keys().copied().collect();
    let index: HashMap<u64, u64> = order.iter().enumerate().map(|(i, id)| (*id, i as u64)).collect();
    let nodes_out: Vec<Value> = order
        .iter()
        .map(|id| {
            let mut n = w.nodes[id].clone();
            for key in ["id", "individual", "individual_data"] {
                n.remove(key);
            }
            Value::Object(n)
        })
        .collect();
    obj.insert("nodes".into(), Value::Array(nodes_out));
    obj.insert("edges".into(), edges_json(&w.edges, &|i| index.get(&i).copied().unwrap_or(0)));
    obj.insert("format_version".into(), json!(SUBGRAPH_FORMAT_VERSION));
    tone_notes.extend(w.notes);
    Ok(tone_notes)
}
