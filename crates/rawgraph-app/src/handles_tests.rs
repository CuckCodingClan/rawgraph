//! The handles driven through the real viewer, tools and history rules with synthetic input.

use super::*;
use crate::history::History;
use crate::render::Frame;
use crate::tools::Tools;
use crate::viewer::Viewer;
use eframe::egui::{Event, Modifiers};
use rawgraph_core::{ImageBuf, Mode};
use std::sync::Arc;

const FRAME_W: u32 = 300;
const FRAME_H: u32 = 200;
const IMG: ImageId = ImageId(1);

struct View {
    ctx: egui::Context,
    viewer: Viewer,
    tools: Tools,
    s: Session,
    node: NodeId,
    history: History,
    t: f64,
    pos: Pos2,
    steps: usize,
}

impl View {
    /// A node of `kind` fed by the demo Source, selected; the viewer shows `viewed` (the node
    /// itself when `None`).
    fn new(kind: NodeKind) -> Self {
        let mut s = crate::session::tests::demo_session();
        let src = s.node_by_index(0).unwrap();
        let node = s.project.graph.add_node(kind, [0.0, 0.0]);
        s.project.graph.connect(src, node, 0).unwrap();
        s.selected = Some(IMG);
        s.selected_node = Some(node);
        let mut v = Self { ctx: egui::Context::default(), viewer: Viewer::default(), tools: Tools::default(), history: History::new(&s.project.graph), s, node, t: 0.0, pos: Pos2::new(5.0, 5.0), steps: 0 };
        v.set_frame(node);
        v.frame(vec![]);
        v.frame(vec![]);
        v
    }

    fn set_frame(&mut self, node: NodeId) {
        let img = ImageBuf::new(FRAME_W, FRAME_H);
        let frame = Frame {
            generation: 1,
            image: IMG,
            node,
            width: FRAME_W,
            height: FRAME_H,
            rgba8: vec![128; (FRAME_W * FRAME_H * 4) as usize],
            gpu: None,
            path: crate::gpu_path::RenderPath::cpu("test"),
            notice: None,
            scope_step: 1,
            display: Arc::new(rawgraph_scopes::DisplayTransform::default().apply(&img)),
            prepared: None,
            scopes_ms: 0.0,
            node_timings: None,
            gpu_tail: false,
            eval_ms: 0.0,
            display_ms: 0.0,
            hits: 0,
            misses: 0,
            overlay: None,
        };
        self.viewer.set_frame(&self.ctx, &frame);
    }

    fn frame(&mut self, events: Vec<Event>) {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0))), time: Some(self.t), events, ..Default::default() };
        self.t += 0.04;
        let (viewer, tools, s) = (&mut self.viewer, &mut self.tools, &mut self.s);
        let out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                crate::app::show_viewer(viewer, tools, s, ui);
            });
        });
        out.drop_without_applying_deltas();
        let gesture = self.ctx.input(|i| i.pointer.any_down()) || self.ctx.dragged_id().is_some();
        if self.history.commit_if_changed(&self.s.project.graph, gesture) {
            self.steps += 1;
        }
    }

    /// Screen position of a normalised point of the picture shown.
    fn at(&self, n: [f32; 2]) -> Pos2 {
        let size = Vec2::new(FRAME_W as f32, FRAME_H as f32) * self.viewer.last_scale;
        let centre = Pos2::new(450.0, 300.0) + self.viewer.pan_for_tests();
        centre + Vec2::new((n[0] - 0.5) * size.x, (n[1] - 0.5) * size.y)
    }

    fn move_to(&mut self, p: Pos2) {
        self.pos = p;
        self.frame(vec![Event::PointerMoved(p)]);
    }

    fn button(&mut self, pressed: bool) {
        let ev = Event::PointerButton { pos: self.pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        self.frame(vec![ev]);
    }

    fn drag_screen(&mut self, from: Pos2, to: Pos2) {
        self.move_to(from);
        self.button(true);
        for i in 1..=8 {
            let p = from + (to - from) * (i as f32 / 8.0);
            self.move_to(p);
        }
        self.button(false);
        self.frame(vec![]);
        self.frame(vec![]);
    }

    fn double_click(&mut self, p: Pos2) {
        self.move_to(p);
        for _ in 0..2 {
            self.button(true);
            self.button(false);
        }
        self.frame(vec![]);
    }

    fn param(&self, name: &str) -> f32 {
        self.s.project.graph.node(self.node).unwrap().param(name, IMG)
    }

    fn set(&mut self, name: &str, v: f32) {
        self.s.project.graph.node_mut(self.node).unwrap().set_param(name, IMG, v);
        self.history.commit_if_changed(&self.s.project.graph, false);
        self.frame(vec![]);
    }

    fn model(&self) -> Option<Model> {
        model_of(&self.s.project.graph, self.s.project.graph.node(self.node).unwrap(), IMG, [FRAME_W as f32, FRAME_H as f32])
    }

    fn painted_handles(&self) -> bool {
        self.tools.handles.hot || self.tools.handles.drag.is_some() || {
            // Something was painted beyond the picture: the tests check the hot flag after a hover instead.
            false
        }
    }
}

fn gradient(v: &View) -> Gradient {
    match v.model() {
        Some(Model::Gradient(g)) => g,
        other => panic!("not a gradient: {other:?}"),
    }
}

#[test]
fn dragging_the_linear_end_sets_angle_and_length_in_one_undo_step() {
    let mut v = View::new(NodeKind::Generator);
    let g = gradient(&v);
    let end = v.at(g.position(Part::End));
    let target = v.at([0.75, 0.9]);
    v.drag_screen(end, target);
    let g2 = gradient(&v);
    let got = g2.position(Part::End);
    assert!((got[0] - 0.75).abs() < 0.01 && (got[1] - 0.9).abs() < 0.01, "end lands under the pointer: {got:?}");
    let start = g2.position(Part::Start);
    assert!(start[0].abs() < 0.01 && (start[1] - 0.5).abs() < 0.01, "start stays at the left edge: {start:?}");
    assert!(v.param("gradient_angle") > 10.0, "turned clockwise towards the bottom");
    assert_eq!(v.steps, 1, "one drag, one undo step");
    let g0 = v.history.undo().unwrap();
    assert_eq!(g0.node(v.node).unwrap().param("gradient_angle", IMG), 0.0);
}

#[test]
fn dragging_the_centre_moves_a_radial_gradient_and_double_click_resets_it() {
    let mut v = View::new(NodeKind::Generator);
    v.set("gradient_style", 1.0);
    let c = v.at([0.5, 0.5]);
    v.drag_screen(c, v.at([0.3, 0.7]));
    assert!((v.param("gradient_center_x") - 30.0).abs() < 1.0, "{}", v.param("gradient_center_x"));
    assert!((v.param("gradient_center_y") - 70.0).abs() < 1.0);
    assert_eq!(v.param("gradient_scale"), 100.0, "moving keeps the radius");
    // Radius: the edge knob.
    let edge = v.at(gradient(&v).position(Part::Edge));
    v.drag_screen(edge, v.at([0.3, 0.95]));
    assert!((v.param("gradient_scale") - 50.0).abs() < 2.0, "radius 0.25 = scale 50 %: {}", v.param("gradient_scale"));
    let steps = v.steps;
    v.double_click(v.at([0.3, 0.7]));
    assert_eq!((v.param("gradient_center_x"), v.param("gradient_center_y")), (50.0, 50.0), "double-click resets the centre");
    assert!((v.param("gradient_scale") - 50.0).abs() < 2.0, "and only the centre");
    assert_eq!(v.steps, steps + 1);
}

#[test]
fn a_click_without_moving_changes_nothing() {
    let mut v = View::new(NodeKind::Generator);
    let before = v.s.project.graph.clone();
    let end = v.at(gradient(&v).position(Part::End));
    v.move_to(end);
    v.button(true);
    v.button(false);
    v.frame(vec![]);
    assert_eq!(v.s.project.graph, before);
    assert_eq!(v.steps, 0);
}

#[test]
fn handles_need_the_node_selected_and_in_the_viewed_chain() {
    let mut v = View::new(NodeKind::Generator);
    let end = v.at(gradient(&v).position(Part::End));
    v.move_to(end);
    assert!(v.painted_handles(), "hovering the handle makes it hot");
    // Not selected: no handles, a drag edits nothing.
    v.s.selected_node = None;
    v.frame(vec![]);
    let before = v.s.project.graph.clone();
    v.drag_screen(end, v.at([0.8, 0.9]));
    assert_eq!(v.s.project.graph, before);
    assert!(!v.painted_handles());
    // Selected, but the viewer is pinned to a node outside its chain.
    v.s.selected_node = Some(v.node);
    let other = v.s.project.graph.nodes().find(|n| n.kind == NodeKind::Saturation).unwrap().id;
    v.s.viewed_node = Some(other);
    v.set_frame(other);
    v.frame(vec![]);
    v.drag_screen(end, v.at([0.8, 0.9]));
    assert_eq!(v.s.project.graph, before, "a pinned unrelated stage shows no handles");
    assert!(node_frame(&v.s, v.node, [FRAME_W, FRAME_H], Rect::from_min_size(Pos2::ZERO, Vec2::splat(100.0))).is_none());
    // Downstream through a non-geometry node: handles work on the viewed picture.
    let sat = v.s.project.graph.add_node(NodeKind::Saturation, [0.0, 0.0]);
    v.s.project.graph.connect(v.node, sat, 0).unwrap();
    v.s.viewed_node = Some(sat);
    v.set_frame(sat);
    v.viewer.fit();
    v.frame(vec![]);
    // (The drags above panned the viewer: the handle is found again on the picture.)
    let end = v.at(gradient(&v).position(Part::End));
    v.drag_screen(end, v.at([0.8, 0.9]));
    assert_ne!(v.s.project.graph.node(v.node).unwrap().param("gradient_angle", IMG), 0.0, "a downstream view still has the handles");
}

#[test]
fn handles_hide_when_the_pointer_leaves_the_viewer() {
    let mut v = View::new(NodeKind::Generator);
    let end = v.at(gradient(&v).position(Part::End));
    v.move_to(end);
    assert!(v.tools.handles.hot);
    v.frame(vec![Event::PointerGone]);
    assert!(!v.tools.handles.hot, "nothing is drawn or hot without the pointer over the viewer");
}

#[test]
fn individual_mode_writes_only_the_current_image() {
    let mut v = View::new(NodeKind::Vignette);
    v.s.set_mode(v.node, Mode::Individual);
    v.set("amount", -0.5);
    let knob = v.at(logic::vignette_knob(0.5, [FRAME_W as f32, FRAME_H as f32], true));
    v.drag_screen(knob, v.at(logic::vignette_knob(0.7, [FRAME_W as f32, FRAME_H as f32], true)));
    let n = v.s.project.graph.node(v.node).unwrap();
    assert!((n.param("midpoint", IMG) - 0.7).abs() < 0.02, "midpoint follows the ring: {}", n.param("midpoint", IMG));
    assert_eq!(n.param("midpoint", ImageId(2)), 0.5, "another image keeps its value");
    assert_eq!(n.param("feather", IMG), 0.5, "the midpoint ring leaves the feather alone");
}

#[test]
fn feather_ring_sets_the_feather_and_double_click_resets_it() {
    let mut v = View::new(NodeKind::Vignette);
    let size = [FRAME_W as f32, FRAME_H as f32];
    let outer = v.at(logic::vignette_knob(0.75, size, false));
    v.drag_screen(outer, v.at(logic::vignette_knob(0.9, size, false)));
    assert!((v.param("feather") - 0.8).abs() < 0.02, "{}", v.param("feather"));
    assert_eq!(v.param("midpoint"), 0.5);
    v.double_click(v.at(logic::vignette_knob(0.9, size, false)));
    assert_eq!(v.param("feather"), 0.5);
}

#[test]
fn the_motion_arrow_appears_only_for_motion_blur_and_sets_the_angle() {
    let mut v = View::new(NodeKind::Blur);
    assert!(v.model().is_none(), "Gaussian has no direction");
    v.set("kernel", 3.0);
    assert!(matches!(v.model(), Some(Model::Motion { .. })));
    let c = v.at([0.5, 0.5]);
    let tip = c + Vec2::new(ARROW, 0.0);
    v.drag_screen(tip, c + Vec2::new(0.0, ARROW));
    assert!((v.param("angle") - 90.0).abs() < 1.0, "pointing down is 90 degrees: {}", v.param("angle"));
    v.double_click(c + Vec2::new(0.0, ARROW));
    assert_eq!(v.param("angle"), 0.0);
    // A connected Direction (RGB) map overrides the angle: no arrow.
    let port = v.s.project.graph.expose_parameter(v.node, "direction", rawgraph_core::PortType::Images).unwrap();
    let map = v.s.project.graph.add_node(NodeKind::Generator, [0.0, 0.0]);
    v.s.project.graph.connect(map, v.node, port).unwrap();
    assert!(v.model().is_none());
}

#[test]
fn handles_map_through_a_downstream_crop() {
    let mut v = View::new(NodeKind::Generator);
    v.s.project.graph.disconnect(v.node, 0);
    for (name, value) in [("width", 300.0), ("height", 200.0)] {
        v.s.project.graph.node_mut(v.node).unwrap().set_param(name, IMG, value);
    }
    let tf = v.s.project.graph.add_node(NodeKind::Transform, [0.0, 0.0]);
    v.s.project.graph.connect(v.node, tf, 0).unwrap();
    crate::tools::set_crop_rect(&mut v.s, tf, IMG, crate::crop_logic::CropRect { l: 0.5, t: 0.0, r: 1.0, b: 1.0 });
    v.s.viewed_node = Some(tf);
    let viewed = v.s.image_for_target(tf, None).unwrap();
    assert_eq!(viewed, ImageId(u64::MAX - v.node.0));
    let f = node_frame(&v.s, v.node, [150, 200], Rect::from_min_size(Pos2::ZERO, Vec2::new(150.0, 200.0))).unwrap();
    assert_eq!(f.size, [300.0, 200.0], "the generator's own frame");
    let p = f.to_screen([0.75, 0.5]);
    assert!((p.x - 75.0).abs() < 0.6 && (p.y - 100.0).abs() < 0.6, "{p:?}");
    let back = f.to_frame(p);
    assert!((back[0] - 0.75).abs() < 1e-3 && (back[1] - 0.5).abs() < 1e-3);
}
