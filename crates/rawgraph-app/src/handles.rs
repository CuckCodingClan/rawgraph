//! In-viewer handles: drag parts of the selected node's settings directly on the picture.
//!
//! Each node kind declares its handles in [`model_of`]; the pure rules (positions, what a
//! drag means) live in `handles_logic`. Handles show only while the node is selected, the
//! pointer is over the viewer and the viewer shows the node's own frame (the node itself or a
//! node downstream of it through nothing but Crop / Rotate nodes, see
//! `frames::handle_chain`). Values are written through the node's per-image API, so Individual
//! mode only changes the image on screen; the application's history turns every finished
//! gesture into one undo step. Double-clicking a handle resets what it controls.
//!
//! The white-balance picker (White Balance and Develop) is a click instead of a drag: it reads
//! the node's input around the clicked point on a worker thread and makes it neutral.

use std::sync::mpsc::{channel, Receiver};

use eframe::egui::{self, Color32, CursorIcon, Key, Modifiers, PointerButton, Pos2, Rect, Stroke, Vec2};
use rawgraph_core::{Graph, ImageId, Node, NodeId, NodeKind};
use rawgraph_engine::{CancelToken, Evaluator, ImageProvider, Value};

use crate::frames::{self, Mapper};
use crate::handles_logic::{self as logic, Gradient, Part};
use crate::session::Session;
use crate::viewer::Geometry;

/// Hit radius of a handle, in screen points.
const HIT: f32 = 10.0;
/// Screen length of the motion direction arrow.
const ARROW: f32 = 56.0;
const HOT: Color32 = Color32::from_rgb(255, 210, 90);

/// The settings a node's handles stand for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Model {
    Gradient(Gradient),
    Vignette { midpoint: f32, feather: f32 },
    /// Motion angle in degrees; `radius` of the blur in pixels of the node's frame.
    Motion { angle: f32, radius: f32 },
}

/// The handle settings of `node` for `image`, `None` when the node has no handles (or they
/// mean nothing in its current state). `size`: pixel size of the node's frame.
pub fn model_of(graph: &Graph, node: &Node, image: ImageId, size: [f32; 2]) -> Option<Model> {
    let p = |name: &str| node.param(name, image);
    match node.kind {
        NodeKind::Generator if node.choice("type", image) == 0 => Some(Model::Gradient(Gradient {
            style: node.choice("gradient_style", image) as u32,
            angle: p("gradient_angle"),
            scale: p("gradient_scale") / 100.0,
            center: [p("gradient_center_x") / 100.0, p("gradient_center_y") / 100.0],
            vertical: node.toggle("vertical", image),
        })),
        NodeKind::Vignette => Some(Model::Vignette { midpoint: p("midpoint"), feather: p("feather") }),
        NodeKind::Blur if node.choice("kernel", image) == 3 && !direction_connected(graph, node) => {
            Some(Model::Motion { angle: p("angle"), radius: p("radius") / 1000.0 * size[0].max(size[1]) })
        }
        _ => None,
    }
}

/// A connected Direction (RGB) map overrides the motion angle.
fn direction_connected(graph: &Graph, node: &Node) -> bool {
    node.exposed_inputs().iter().enumerate().any(|(i, e)| e.as_ref().is_some_and(|e| e.parameter == "direction") && graph.input_edge(node.id, node.kind.inputs().len() + i).is_some())
}

/// The parts of a model, in hit priority order.
fn parts(model: &Model) -> &'static [Part] {
    match model {
        Model::Gradient(g) => g.parts(),
        Model::Vignette { .. } => &[Part::Midpoint, Part::FeatherOuter, Part::FeatherInner],
        Model::Motion { .. } => &[Part::Motion],
    }
}

/// Parameter values after dragging `part` of `start` to `n` (normalised frame coordinates).
pub fn drag_values(start: &Model, part: Part, n: [f32; 2], size: [f32; 2]) -> Vec<(&'static str, f32)> {
    match *start {
        Model::Gradient(g) => {
            let g = g.drag(part, n);
            vec![("gradient_angle", g.angle), ("gradient_scale", g.scale * 100.0), ("gradient_center_x", g.center[0] * 100.0), ("gradient_center_y", g.center[1] * 100.0)]
        }
        Model::Vignette { midpoint, feather } => {
            let (m, f) = logic::vignette_drag(part, logic::vignette_distance(n, size), midpoint, feather);
            vec![("midpoint", m), ("feather", f)]
        }
        Model::Motion { angle, .. } => vec![("angle", logic::motion_angle(n, size, angle))],
    }
}

/// The parameters a double-click on `part` resets to their defaults.
pub fn reset_names(model: &Model, part: Part) -> &'static [&'static str] {
    match (model, part) {
        (Model::Gradient(_), Part::Center) => &["gradient_center_x", "gradient_center_y"],
        (Model::Gradient(_), Part::Direction) => &["gradient_angle"],
        (Model::Gradient(_), _) => &["gradient_angle", "gradient_scale"],
        (Model::Vignette { .. }, Part::Midpoint) => &["midpoint"],
        (Model::Vignette { .. }, _) => &["feather"],
        (Model::Motion { .. }, _) => &["angle"],
    }
}

/// Writes the values that differ from the node's current ones; `true` when anything changed.
pub fn set_params(session: &mut Session, node: NodeId, image: ImageId, values: &[(&str, f32)]) -> bool {
    let Some(n) = session.project.graph.node_mut(node) else { return false };
    let mut changed = false;
    for &(name, v) in values {
        if (n.param(name, image) - v).abs() > 1e-5 {
            changed |= n.set_param(name, image, v);
        }
    }
    changed
}

/// Resets parameters to their defaults (for this image in Individual mode).
pub fn reset_params(session: &mut Session, node: NodeId, image: ImageId, names: &[&str]) -> bool {
    let Some(kind) = session.project.graph.node(node).map(|n| n.kind) else { return false };
    let values: Vec<(&str, f32)> = names.iter().filter_map(|name| kind.param_spec(name).map(|s| (*name, s.default))).collect();
    set_params(session, node, image, &values)
}

/// How the node's own frame lies on the screen.
pub struct NodeFrame {
    pub image: ImageId,
    /// Pixel size of the node's frame.
    pub size: [f32; 2],
    mapper: Mapper,
    rect: Rect,
}

impl NodeFrame {
    pub fn to_screen(&self, n: [f32; 2]) -> Pos2 {
        let v = self.mapper.to_viewed(n);
        self.rect.min + Vec2::new(v[0] * self.rect.width(), v[1] * self.rect.height())
    }

    pub fn to_frame(&self, p: Pos2) -> [f32; 2] {
        let (w, h) = (self.rect.width().max(1e-6), self.rect.height().max(1e-6));
        self.mapper.to_consumer([(p.x - self.rect.min.x) / w, (p.y - self.rect.min.y) / h])
    }
}

/// The frame of `node` on the viewer picture (`frame_px`: size of the picture shown, `rect`:
/// where it is painted), or `None` when the viewer does not show the node's frame.
pub fn node_frame(session: &Session, node: NodeId, frame_px: [u32; 2], rect: Rect) -> Option<NodeFrame> {
    let graph = &session.project.graph;
    let (viewed, port) = session.render_target()?;
    let chain = frames::handle_chain(graph, node, viewed)?;
    let image = session.image_for_target(viewed, port).unwrap_or_else(|| session.current_image());
    let size = if chain.is_empty() {
        (frame_px[0], frame_px[1])
    } else {
        let n = graph.node(node)?;
        let standalone = matches!(n.kind, NodeKind::Generator | NodeKind::Draw) && graph.input_edge(node, 0).is_none();
        let source = if standalone { (n.param("width", image).round().max(1.0) as u32, n.param("height", image).round().max(1.0) as u32) } else { session.proxy_size(image)? };
        frames::size_into(graph, node, image, source)
    };
    if size.0 == 0 || size.1 == 0 || frame_px[0] == 0 || frame_px[1] == 0 {
        return None;
    }
    let params: Vec<_> = chain.iter().filter_map(|id| graph.node(*id)).map(|n| crate::tools::transform_params(n, image)).collect();
    Some(NodeFrame { image, size: [size.0 as f32, size.1 as f32], mapper: Mapper::from_stages(size, &params), rect })
}

/// Screen positions of the parts of `model`.
fn part_positions(model: &Model, f: &NodeFrame) -> Vec<(Part, Pos2)> {
    parts(model).iter().map(|&part| (part, part_position(model, part, f))).collect()
}

fn part_position(model: &Model, part: Part, f: &NodeFrame) -> Pos2 {
    match model {
        Model::Gradient(g) => f.to_screen(g.position(part)),
        Model::Vignette { midpoint, feather } => {
            let [e0, m, e1] = logic::vignette_rings(*midpoint, *feather);
            let (d, upper) = match part { Part::Midpoint => (m, true), Part::FeatherInner => (e0, false), _ => (e1, false) };
            f.to_screen(logic::vignette_knob(d, f.size, upper))
        }
        Model::Motion { angle, .. } => {
            let (c, dir) = motion_screen(*angle, f);
            c + dir * ARROW
        }
    }
}

/// Screen centre and screen unit direction of the motion blur.
fn motion_screen(angle: f32, f: &NodeFrame) -> (Pos2, Vec2) {
    let c = f.to_screen([0.5, 0.5]);
    let v = logic::motion_vector(angle, f.size[0].max(f.size[1]) * 0.1, f.size);
    let d = f.to_screen([0.5 + v[0], 0.5 + v[1]]) - c;
    (c, if d.length() > 1e-6 { d.normalized() } else { Vec2::X })
}

fn hit(positions: &[(Part, Pos2)], p: Pos2) -> Option<Part> {
    positions.iter().filter(|(_, q)| q.distance(p) <= HIT).min_by(|a, b| a.1.distance(p).total_cmp(&b.1.distance(p))).map(|(part, _)| *part)
}

struct Drag {
    node: NodeId,
    image: ImageId,
    part: Part,
    start: Model,
    /// Handle position minus the press position (the handle does not jump to the pointer).
    offset: Vec2,
    press: Pos2,
    moved: bool,
}

/// Parameter values of a white-balance pick and an optional note, or why there are none.
pub type PickResult = Result<(Vec<(&'static str, f32)>, Option<String>), String>;

/// Result of a white-balance pick for one node and image.
struct Pick {
    node: NodeId,
    image: ImageId,
    result: PickResult,
}

/// Pointer state of the handles that lasts longer than a frame.
#[derive(Default)]
pub struct Handles {
    drag: Option<Drag>,
    /// The pointer was over a handle last frame: the viewer leaves the primary button to us.
    hot: bool,
    pick: Option<Receiver<Pick>>,
}

impl Handles {
    /// Whether the primary button belongs to the handles this frame (no pan / click zoom).
    pub fn claims_pointer(&self, session: &Session) -> bool {
        self.drag.is_some() || self.hot || picking(session).is_some()
    }

    /// A white-balance pick is being computed.
    #[cfg(test)]
    pub fn pick_pending(&self) -> bool {
        self.pick.is_some()
    }

    /// Handles the pointer for the selected node's handles; returns a status message.
    pub fn show(&mut self, ui: &egui::Ui, geom: &Geometry, session: &mut Session, frame_px: [u32; 2]) -> Option<String> {
        let message = self.poll_pick(session);
        self.hot = false;
        let Some(node) = session.selected_node else {
            self.drag = None;
            session.wb_pick = None;
            return message;
        };
        if session.wb_pick.is_some_and(|n| n != node) {
            session.wb_pick = None;
        }
        let ctx = ui.ctx();
        let typing = ctx.egui_wants_keyboard_input();
        let (pos, latest, space, middle, down, pressed, double) = ctx.input(|i| {
            (i.pointer.hover_pos(), i.pointer.latest_pos(), i.key_down(Key::Space), i.pointer.middle_down(), i.pointer.primary_down(), i.pointer.primary_pressed(), i.pointer.button_double_clicked(PointerButton::Primary))
        });
        let space = space && !typing;
        let inside = geom.response.hovered() && pos.is_some_and(|p| geom.viewport.contains(p));
        let Some(f) = node_frame(session, node, frame_px, geom.image_rect) else {
            self.drag = None;
            return message;
        };

        if picking(session) == Some(node) {
            return self.picker(ui, geom, session, node, &f, inside && !space && !middle).or(message);
        }

        let Some(model) = session.project.graph.node(node).and_then(|n| model_of(&session.project.graph, n, f.image, f.size)) else {
            self.drag = None;
            return message;
        };
        if self.drag.as_ref().is_some_and(|d| d.node != node || d.image != f.image) {
            self.drag = None;
        }
        let positions = part_positions(&model, &f);

        // Grab a handle.
        if pressed && inside && !space && !middle && self.drag.is_none() {
            if let Some(p) = pos {
                if let Some(part) = hit(&positions, p) {
                    let at = positions.iter().find(|(q, _)| *q == part).map_or(p, |(_, q)| *q);
                    self.drag = Some(Drag { node, image: f.image, part, start: model, offset: at - p, press: p, moved: false });
                }
            }
        }
        let mut dragging = None;
        if let Some(drag) = &mut self.drag {
            dragging = Some(drag.part);
            if let Some(p) = latest {
                drag.moved |= p.distance(drag.press) > 2.0;
                if drag.moved {
                    let n = f.to_frame(p + drag.offset);
                    let values = drag_values(&drag.start, drag.part, n, f.size);
                    set_params(session, node, f.image, &values);
                }
            }
            if !down {
                if double && !drag.moved {
                    reset_params(session, node, f.image, reset_names(&drag.start, drag.part));
                }
                self.drag = None;
            }
            ctx.request_repaint();
        }

        // Handles show only while the pointer is over the viewer (or a drag is running).
        if !inside && dragging.is_none() {
            return message;
        }
        let model = session.project.graph.node(node).and_then(|n| model_of(&session.project.graph, n, f.image, f.size)).unwrap_or(model);
        let positions = part_positions(&model, &f);
        let hot = dragging.or_else(|| if inside && !space { pos.and_then(|p| hit(&positions, p)) } else { None });
        self.hot = hot.is_some();
        if let Some(h) = hot {
            ctx.set_cursor_icon(if dragging.is_some() { CursorIcon::Grabbing } else if h == Part::Center { CursorIcon::Move } else { CursorIcon::Grab });
        }
        paint(ui, geom.viewport, &model, &f, &positions, hot);
        message
    }

    fn picker(&mut self, ui: &egui::Ui, geom: &Geometry, session: &mut Session, node: NodeId, f: &NodeFrame, inside: bool) -> Option<String> {
        let ctx = ui.ctx();
        self.drag = None;
        let (pos, clicked, escape) = ctx.input_mut(|i| (i.pointer.hover_pos(), i.pointer.primary_clicked(), i.consume_key(Modifiers::NONE, Key::Escape)));
        if escape {
            session.wb_pick = None;
            return None;
        }
        if !inside {
            return None;
        }
        self.hot = true;
        ctx.set_cursor_icon(CursorIcon::Crosshair);
        if let Some(p) = pos {
            let painter = ui.painter_at(geom.viewport);
            painter.circle_stroke(p, 7.0, Stroke::new(2.5, Color32::from_black_alpha(150)));
            painter.circle_stroke(p, 7.0, Stroke::new(1.2, Color32::WHITE));
            if clicked && self.pick.is_none() {
                let at = f.to_frame(p);
                if !(0.0..=1.0).contains(&at[0]) || !(0.0..=1.0).contains(&at[1]) {
                    return Some("Click on the picture to pick a neutral point".to_owned());
                }
                let (tx, rx) = channel();
                let graph = session.project.graph.clone();
                let provider = session.provider.clone();
                let tone = session.view.tone.tone_map();
                let image = f.image;
                std::thread::spawn(move || {
                    let result = pick_values(&graph, provider.as_ref(), tone, node, image, at);
                    let _ = tx.send(Pick { node, image, result });
                });
                self.pick = Some(rx);
                session.wb_pick = None;
                ctx.request_repaint();
            }
        }
        None
    }

    /// Applies a finished white-balance pick (one undo step: no gesture is running).
    fn poll_pick(&mut self, session: &mut Session) -> Option<String> {
        let rx = self.pick.as_ref()?;
        match rx.try_recv() {
            Ok(pick) => {
                self.pick = None;
                match pick.result {
                    Ok((values, note)) => {
                        set_params(session, pick.node, pick.image, &values);
                        note
                    }
                    Err(e) => Some(e),
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pick = None;
                Some("The neutral point could not be read".to_owned())
            }
        }
    }
}

/// The node whose white-balance picker is armed (it must be the selected node).
pub fn picking(session: &Session) -> Option<NodeId> {
    session.wb_pick.filter(|n| session.selected_node == Some(*n))
}

/// Average linear colour of `img` in a small square around normalised `at`.
pub fn sample_patch(img: &rawgraph_core::ImageBuf, at: [f32; 2]) -> [f32; 3] {
    let (w, h) = (img.width(), img.height());
    let r = (w.max(h) / 300).max(2) as i64;
    let cx = ((at[0] * w as f32) as i64).clamp(0, w as i64 - 1);
    let cy = ((at[1] * h as f32) as i64).clamp(0, h as i64 - 1);
    let (mut sum, mut n) = ([0.0f64; 3], 0.0);
    for y in (cy - r).max(0)..=(cy + r).min(h as i64 - 1) {
        for x in (cx - r).max(0)..=(cx + r).min(w as i64 - 1) {
            let p = img.pixel(x as u32, y as u32);
            if p[..3].iter().all(|v| v.is_finite()) {
                for c in 0..3 {
                    sum[c] += f64::from(p[c]);
                }
                n += 1.0;
            }
        }
    }
    if n == 0.0 { [0.0; 3] } else { sum.map(|s| (s / n) as f32) }
}

/// Parameter values that make the input of `node` neutral around `at` (normalised).
pub fn pick_values(graph: &Graph, provider: &dyn ImageProvider, tone: rawgraph_scopes::ToneMap, node: NodeId, image: ImageId, at: [f32; 2]) -> PickResult {
    let n = graph.node(node).ok_or("The node no longer exists")?;
    let (up, port) = graph.input_edge(node, 0).ok_or("Connect an image to pick a neutral point")?;
    let mut evaluator = Evaluator::new();
    evaluator.set_view_tone(tone);
    let Value::Images(img) = evaluator.eval_output(graph, up, port, image, provider, &CancelToken::new()).map_err(|e| format!("The neutral point could not be read: {e}"))?;
    let c = sample_patch(&img, at);
    if c.iter().any(|v| !v.is_finite() || *v <= 1e-6) {
        return Err("Pick a grey area: every channel must be above black".to_owned());
    }
    let (values, error) = match n.kind {
        NodeKind::WhiteBalance => {
            let ((t, tn), e) = logic::neutral_shift(c, (n.param("temp", image), n.param("tint", image))).ok_or("No neutral setting found")?;
            (vec![("temp", t), ("tint", tn)], e)
        }
        NodeKind::Develop => {
            let s = rawgraph_core::develop_settings(n, image, provider.capture(image).as_ref());
            let ((k, tn), e) = logic::neutral_absolute(c, s.as_shot, s.effective).ok_or("No neutral setting found")?;
            (vec![("wb", 1.0), ("temperature", k), ("tint", tn)], e)
        }
        _ => return Err("This node has no white balance".to_owned()),
    };
    let note = (error > 0.02).then(|| "This colour is beyond the white-balance range; the closest setting was used".to_owned());
    Ok((values, note))
}

// ------------------------------------------------------------------- paint

fn line(painter: &egui::Painter, a: Pos2, b: Pos2, strong: bool) {
    painter.line_segment([a, b], Stroke::new(if strong { 3.0 } else { 2.4 }, Color32::from_black_alpha(if strong { 150 } else { 90 })));
    painter.line_segment([a, b], Stroke::new(if strong { 1.4 } else { 1.0 }, Color32::from_white_alpha(if strong { 230 } else { 140 })));
}

fn polyline(painter: &egui::Painter, pts: &[Pos2], strong: bool) {
    if pts.len() < 2 {
        return;
    }
    let mut closed = pts.to_vec();
    closed.push(pts[0]);
    painter.add(egui::Shape::line(closed.clone(), Stroke::new(if strong { 3.0 } else { 2.4 }, Color32::from_black_alpha(if strong { 140 } else { 80 }))));
    painter.add(egui::Shape::line(closed, Stroke::new(if strong { 1.3 } else { 1.0 }, Color32::from_white_alpha(if strong { 220 } else { 120 }))));
}

/// A handle: filled marks where the ramp ends (t = 1), hollow where it starts.
fn knob(painter: &egui::Painter, p: Pos2, filled: bool, hot: bool, r: f32) {
    let color = if hot { HOT } else { Color32::WHITE };
    painter.circle_filled(p, r + 1.5, Color32::from_black_alpha(150));
    if filled {
        painter.circle_filled(p, r, color);
    } else {
        painter.circle_stroke(p, r - 0.6, Stroke::new(1.8, color));
    }
}

/// A short tick across the direction `dir` at `p` (where a linear ramp starts or ends).
fn tick(painter: &egui::Painter, p: Pos2, dir: Vec2) {
    let n = Vec2::new(-dir.y, dir.x) * 9.0;
    line(painter, p - n, p + n, false);
}

fn paint(ui: &egui::Ui, viewport: Rect, model: &Model, f: &NodeFrame, positions: &[(Part, Pos2)], hot: Option<Part>) {
    let painter = ui.painter_at(viewport);
    let at = |part: Part| positions.iter().find(|(p, _)| *p == part).map(|(_, q)| *q);
    match model {
        Model::Gradient(g) => {
            let c = f.to_screen(g.center);
            match g.style {
                0 => {
                    let (Some(a), Some(b)) = (at(Part::Start), at(Part::End)) else { return };
                    line(&painter, a, b, true);
                    let dir = (b - a).normalized();
                    tick(&painter, a, dir);
                    tick(&painter, b, dir);
                    knob(&painter, a, false, hot == Some(Part::Start), 5.5);
                    knob(&painter, b, true, hot == Some(Part::End), 5.5);
                }
                2 => {
                    let Some(d) = at(Part::Direction) else { return };
                    line(&painter, c, d, true);
                    knob(&painter, d, true, hot == Some(Part::Direction), 5.5);
                }
                style => {
                    let Some(e) = at(Part::Edge) else { return };
                    let contour: Vec<Pos2> = g.contour(72).into_iter().map(|p| f.to_screen(p)).collect();
                    if contour.is_empty() {
                        // Reflected: the ramp ends on both sides of the centre line.
                        let back = c - (e - c);
                        line(&painter, back, e, true);
                        let dir = (e - c).normalized();
                        tick(&painter, back, dir);
                        tick(&painter, e, dir);
                    } else {
                        polyline(&painter, &contour, false);
                        line(&painter, c, e, style == 4);
                    }
                    knob(&painter, e, true, hot == Some(Part::Edge), 5.5);
                }
            }
            knob(&painter, c, false, hot == Some(Part::Center), 4.5);
        }
        Model::Vignette { midpoint, feather } => {
            let rings = logic::vignette_rings(*midpoint, *feather);
            for (i, d) in rings.iter().enumerate() {
                if *d <= 0.0 {
                    continue;
                }
                let pts: Vec<Pos2> = logic::vignette_ring(*d, f.size, 96).into_iter().map(|p| f.to_screen(p)).collect();
                polyline(&painter, &pts, i == 1);
            }
            for (part, p) in positions {
                knob(&painter, *p, *part == Part::Midpoint, hot == Some(*part), if *part == Part::Midpoint { 5.5 } else { 4.5 });
            }
        }
        Model::Motion { angle, radius } => {
            let (c, dir) = motion_screen(*angle, f);
            // The streak: how far the blur reaches on either side, in the picture's pixels.
            let v = logic::motion_vector(*angle, *radius, f.size);
            let (a, b) = (f.to_screen([0.5 - v[0], 0.5 - v[1]]), f.to_screen([0.5 + v[0], 0.5 + v[1]]));
            if a.distance(b) > 2.0 {
                line(&painter, a, b, false);
            }
            let tip = c + dir * ARROW;
            line(&painter, c, tip, true);
            let n = Vec2::new(-dir.y, dir.x);
            let color = if hot == Some(Part::Motion) { HOT } else { Color32::WHITE };
            let head = vec![tip + dir * 4.0, tip - dir * 7.0 + n * 5.5, tip - dir * 7.0 - n * 5.5];
            painter.add(egui::Shape::convex_polygon(head, color, Stroke::new(1.0, Color32::from_black_alpha(160))));
            knob(&painter, c, false, false, 3.5);
        }
    }
}

#[cfg(test)]
#[path = "handles_tests.rs"]
mod tests;
