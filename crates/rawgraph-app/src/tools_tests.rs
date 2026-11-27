//! Synthetic pointer input on the viewer: the brush and crop tools are driven
//! like a user would, through the real viewer, tools and history rules.

use super::*;
use crate::crop_logic::Aspect;
use crate::history::History;
use crate::render::Frame;
use eframe::egui::{Event, PointerButton};
use rawgraph_core::{ImageBuf, NodeKind};
use std::sync::Arc;

const FRAME_W: u32 = 300;
const FRAME_H: u32 = 200;

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
    alt: bool,
    sent_alt: bool,
}

impl View {
    fn new(kind: NodeKind) -> Self {
        let mut s = crate::session::tests::demo_session();
        let src = s.node_by_index(0).unwrap();
        let node = s.project.graph.add_node(kind, [0.0, 0.0]);
        if matches!(kind, NodeKind::Transform | NodeKind::Draw) {
            s.project.graph.connect(src, node, 0).unwrap();
        }
        s.selected_node = Some(node);
        let ctx = egui::Context::default();
        let mut viewer = Viewer::default();
        let img = ImageBuf::new(FRAME_W, FRAME_H);
        let frame = Frame {
            generation: 1,
            image: ImageId(1),
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
        viewer.set_frame(&ctx, &frame);
        let history = History::new(&s.project.graph);
        let mut v = Self { ctx, viewer, tools: Tools::default(), s, node, history, t: 0.0, pos: Pos2::new(5.0, 5.0), steps: 0, alt: false, sent_alt: false };
        v.frame(vec![]);
        v.frame(vec![]);
        v
    }

    fn frame(&mut self, mut events: Vec<Event>) {
        if self.alt != self.sent_alt {
            self.sent_alt = self.alt;
            events.insert(0, Event::ModifiersChanged(if self.alt { Modifiers::ALT } else { Modifiers::NONE }));
        }
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

    /// Screen position of a normalised picture coordinate (picture centred in the 900 x 600 viewer).
    fn at(&self, n: [f32; 2]) -> Pos2 {
        let scale = self.viewer.last_scale;
        let size = Vec2::new(FRAME_W as f32, FRAME_H as f32) * scale;
        let centre = Pos2::new(450.0, 300.0) + self.viewer.pan_for_tests();
        centre + Vec2::new((n[0] - 0.5) * size.x, (n[1] - 0.5) * size.y)
    }

    fn move_to(&mut self, p: Pos2) {
        self.pos = p;
        self.frame(vec![Event::PointerMoved(p)]);
    }

    fn button(&mut self, button: PointerButton, pressed: bool) {
        let modifiers = if self.alt { Modifiers::ALT } else { Modifiers::NONE };
        let ev = Event::PointerButton { pos: self.pos, button, pressed, modifiers };
        self.frame(vec![ev]);
    }

    fn drag_path(&mut self, path: &[[f32; 2]]) {
        let first = self.at(path[0]);
        self.move_to(first);
        self.button(PointerButton::Primary, true);
        for p in &path[1..] {
            let at = self.at(*p);
            self.move_to(at);
        }
        self.button(PointerButton::Primary, false);
        self.frame(vec![]);
        self.frame(vec![]);
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        let ev = Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers };
        self.frame(vec![ev]);
    }

    fn strokes(&self) -> Vec<BrushStroke> {
        strokes_of(self.s.project.graph.node(self.node).unwrap(), self.s.current_image())
    }
}

/// A horizontal pointer path of `n` equal steps across the middle of the picture.
fn line(n: usize) -> Vec<[f32; 2]> {
    (0..=n).map(|i| [0.2 + 0.6 * i as f32 / n as f32, 0.5]).collect()
}

#[test]
fn a_drag_paints_one_stroke_with_normalised_points_and_one_undo_step() {
    let mut v = View::new(NodeKind::Draw);
    assert!(v.strokes().is_empty());
    v.drag_path(&line(40));
    let strokes = v.strokes();
    assert_eq!(strokes.len(), 1, "one drag, one stroke");
    let s = &strokes[0];
    assert!(!s.erase);
    let first = s.points[0];
    let last = *s.points.last().unwrap();
    assert!((first[0] - 0.2).abs() < 0.01 && (first[1] - 0.5).abs() < 0.01, "{first:?}");
    assert!((last[0] - 0.8).abs() < 0.01 && (last[1] - 0.5).abs() < 0.01, "{last:?}");
    // 6 % brush: radius 0.03, spacing 0.0075 long-edge units; far fewer points than pointer samples.
    assert!(s.points.len() >= 8 && s.points.len() <= 41, "{} points", s.points.len());
    assert!((s.radius - 0.03).abs() < 1e-5 && s.flow == 1.0);
    assert_eq!(v.steps, 1, "the whole stroke is one undo step");
    let g = v.history.undo().unwrap();
    assert!(strokes_of(g.node(v.node).unwrap(), v.s.current_image()).is_empty(), "undo removes the whole stroke");
}

#[test]
fn the_stroke_is_visible_while_the_button_is_still_down() {
    let mut v = View::new(NodeKind::Draw);
    let (a, b) = (v.at([0.2, 0.5]), v.at([0.5, 0.5]));
    v.move_to(a);
    v.button(PointerButton::Primary, true);
    v.move_to(b);
    assert_eq!(v.strokes().len(), 1, "live while painting");
    assert!(v.strokes()[0].points.len() >= 2);
    assert_eq!(v.steps, 0, "nothing recorded for the history yet");
    v.button(PointerButton::Primary, false);
    v.frame(vec![]);
    assert_eq!(v.steps, 1);
}

#[test]
fn alt_drag_erases_and_the_toggle_flips_it() {
    let mut v = View::new(NodeKind::Draw);
    v.alt = true;
    v.drag_path(&line(10));
    v.alt = false;
    assert!(v.strokes()[0].erase, "Alt+drag erases");
    v.s.brush.erase = true;
    v.drag_path(&[[0.3, 0.3], [0.6, 0.3]]);
    assert!(v.strokes()[1].erase, "the Erase toggle erases");
    v.alt = true;
    v.drag_path(&[[0.3, 0.7], [0.6, 0.7]]);
    assert!(!v.strokes()[2].erase, "Alt flips the toggle back to painting");
}

#[test]
fn coordinates_stay_right_after_zooming_and_panning() {
    let mut v = View::new(NodeKind::Draw);
    v.viewer.mode = crate::viewer::ZoomMode::Custom(2.0);
    v.frame(vec![]);
    let before = v.at([0.5, 0.5]);
    v.move_to(before);
    v.frame(vec![Event::PointerButton { pos: before, button: PointerButton::Middle, pressed: true, modifiers: Modifiers::NONE }]);
    v.move_to(before + Vec2::new(60.0, -40.0));
    v.frame(vec![Event::PointerButton { pos: before + Vec2::new(60.0, -40.0), button: PointerButton::Middle, pressed: false, modifiers: Modifiers::NONE }]);
    v.frame(vec![]);
    assert!(v.strokes().is_empty(), "the middle button pans and does not paint");
    let moved = v.at([0.5, 0.5]);
    assert!((moved - before - Vec2::new(60.0, -40.0)).length() < 1.5, "the picture moved with the pointer: {before:?} -> {moved:?}");
    v.drag_path(&[[0.4, 0.45], [0.6, 0.55]]);
    let s = &v.strokes()[0];
    assert!((s.points[0][0] - 0.4).abs() < 0.01 && (s.points[0][1] - 0.45).abs() < 0.01, "{:?}", s.points[0]);
    let last = s.points.last().unwrap();
    assert!((last[0] - 0.6).abs() < 0.01 && (last[1] - 0.55).abs() < 0.01, "{last:?}");
    assert!((s.radius - 0.03).abs() < 1e-5, "the radius is a fraction of the long edge, whatever the zoom");
}

#[test]
fn space_drag_pans_instead_of_painting() {
    let mut v = View::new(NodeKind::Draw);
    v.viewer.mode = crate::viewer::ZoomMode::Custom(2.0);
    v.frame(vec![]);
    let a = v.at([0.5, 0.5]);
    v.move_to(a);
    v.key(Key::Space, Modifiers::NONE);
    v.button(PointerButton::Primary, true);
    v.move_to(a + Vec2::new(50.0, 0.0));
    v.button(PointerButton::Primary, false);
    assert!(v.strokes().is_empty(), "Space+drag does not paint");
}

#[test]
fn bracket_keys_change_the_size_and_shift_brackets_the_hardness() {
    let mut v = View::new(NodeKind::Draw);
    let centre = v.at([0.5, 0.5]);
    v.move_to(centre);
    let start = v.s.brush.size_pct;
    v.key(Key::CloseBracket, Modifiers::NONE);
    assert!((v.s.brush.size_pct - start * crate::brush_logic::SIZE_STEP).abs() < 1e-4, "{}", v.s.brush.size_pct);
    v.key(Key::OpenBracket, Modifiers::NONE);
    v.key(Key::OpenBracket, Modifiers::NONE);
    assert!(v.s.brush.size_pct < start);
    let h0 = v.s.brush.hardness;
    v.key(Key::CloseBracket, Modifiers::SHIFT);
    assert!((v.s.brush.hardness - h0 - 0.1).abs() < 1e-4, "{}", v.s.brush.hardness);
    v.key(Key::OpenBracket, Modifiers::SHIFT);
    assert!((v.s.brush.hardness - h0).abs() < 1e-4);
    v.drag_path(&[[0.3, 0.3], [0.5, 0.3]]);
    assert!((v.strokes()[0].radius - v.s.brush.radius()).abs() < 1e-6, "a new stroke takes the new size");
}

#[test]
fn strokes_belong_to_the_selected_image_in_individual_mode() {
    let mut v = View::new(NodeKind::Draw);
    assert_eq!(v.s.project.graph.node(v.node).unwrap().mode, rawgraph_core::Mode::Individual, "a drawing starts per image");
    let first = v.s.current_image();
    v.drag_path(&line(10));
    let n = v.s.project.graph.node(v.node).unwrap();
    assert!(n.has_individual_data(first));
    let other = ImageId(first.0 + 1);
    assert!(strokes_of(n, other).is_empty(), "another image has no strokes");
    v.s.set_mode(v.node, rawgraph_core::Mode::All);
    v.drag_path(&[[0.1, 0.1], [0.3, 0.1]]);
    let n = v.s.project.graph.node(v.node).unwrap();
    assert_eq!(strokes_of(n, other).len(), 1, "mode All: every image sees the stroke");
}

#[test]
fn clearing_and_undoing_strokes_through_the_node_data() {
    let mut v = View::new(NodeKind::Draw);
    v.drag_path(&line(10));
    v.drag_path(&[[0.3, 0.2], [0.6, 0.2]]);
    assert_eq!(v.strokes().len(), 2);
    let (node, img) = (v.node, v.s.current_image());
    let mut strokes = v.strokes();
    assert!(crate::brush_logic::undo_last(&mut strokes));
    assert!(set_strokes(&mut v.s, node, img, strokes));
    assert_eq!(v.strokes().len(), 1);
    assert!(set_strokes(&mut v.s, node, img, Vec::new()));
    assert!(v.strokes().is_empty());
    let too_many: Vec<BrushStroke> = (0..rawgraph_core::graph::MAX_STROKES + 1).map(|_| v.s.brush.new_stroke(false, [0.5, 0.5])).collect();
    assert!(!set_strokes(&mut v.s, node, img, too_many), "the core refuses more strokes than it allows");
}

// ---- crop tool ----

fn crop_screen(v: &View) -> [f32; 4] {
    let n = v.s.project.graph.node(v.node).unwrap();
    let r = crop_rect_of(n, v.s.current_image());
    let p = transform_params(n, v.s.current_image());
    let picture = crop_logic::picture_size(FRAME_W, FRAME_H, p.angle_deg, p.quarter_turns);
    let size = Vec2::new(picture[0], picture[1]) * v.viewer.last_scale;
    let c = Pos2::new(450.0, 300.0);
    [c.x - size.x / 2.0 + r.l * size.x, c.y - size.y / 2.0 + r.t * size.y, c.x - size.x / 2.0 + r.r * size.x, c.y - size.y / 2.0 + r.b * size.y]
}

fn drag_screen(v: &mut View, from: Pos2, to: Pos2) {
    v.move_to(from);
    v.button(PointerButton::Primary, true);
    v.move_to(from + (to - from) * 0.5);
    v.move_to(to);
    v.button(PointerButton::Primary, false);
    v.frame(vec![]);
    v.frame(vec![]);
}

fn crop_of(v: &View) -> CropRect {
    crop_rect_of(v.s.project.graph.node(v.node).unwrap(), v.s.current_image())
}

#[test]
fn dragging_a_corner_handle_writes_the_crop_as_one_undo_step() {
    let mut v = View::new(NodeKind::Transform);
    assert_eq!(v.s.tool_mode(), ToolMode::Crop(v.node));
    assert!(crop_of(&v).is_full());
    let [_, _, x1, y1] = crop_screen(&v);
    let corner = Pos2::new(x1, y1);
    drag_screen(&mut v, corner, corner + Vec2::new(-90.0, -60.0));
    let r = crop_of(&v);
    assert!(r.l == 0.0 && r.t == 0.0, "the opposite corner stays: {r:?}");
    assert!((r.r - 0.9).abs() < 0.01 && (r.b - 0.9).abs() < 0.01, "the corner moved in by 90 and 60 points: {r:?}");
    assert_eq!(v.steps, 1, "one undo step per completed drag");
    let n = v.s.project.graph.node(v.node).unwrap();
    assert_eq!(n.param("angle", v.s.current_image()), 0.0, "only the crop parameters were written");
}

#[test]
fn an_edge_handle_moves_one_edge_and_the_inside_moves_the_rectangle() {
    let mut v = View::new(NodeKind::Transform);
    let [x0, y0, _, y1] = crop_screen(&v);
    let left = Pos2::new(x0, (y0 + y1) / 2.0);
    drag_screen(&mut v, left, left + Vec2::new(100.0, 0.0));
    let r = crop_of(&v);
    assert!(r.l > 0.1 && r.t == 0.0 && r.r == 1.0 && r.b == 1.0, "only the left edge moved: {r:?}");
    let before = crop_of(&v);
    let [x0, y0, x1, y1] = crop_screen(&v);
    let inside = Pos2::new((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    // The rectangle touches the right edge, so it can only move left.
    drag_screen(&mut v, inside, inside + Vec2::new(-30.0, 0.0));
    let r = crop_of(&v);
    assert!((r.w() - before.w()).abs() < 1e-4 && (r.h() - before.h()).abs() < 1e-4, "moving keeps the size: {r:?}");
    assert!(r.l < before.l, "{r:?} vs {before:?}");
}

#[test]
fn a_locked_aspect_ratio_holds_while_dragging_a_handle() {
    let mut v = View::new(NodeKind::Transform);
    v.s.crop.aspect = Aspect::Square;
    let [_, _, x1, y1] = crop_screen(&v);
    let corner = Pos2::new(x1, y1);
    drag_screen(&mut v, corner, corner + Vec2::new(-120.0, -10.0));
    let r = crop_of(&v);
    let ratio = (r.w() * FRAME_W as f32) / (r.h() * FRAME_H as f32);
    assert!((ratio - 1.0).abs() < 0.02, "square crop: {ratio} for {r:?}");
    assert!(r.l >= 0.0 && r.t >= 0.0 && r.r <= 1.0 + 1e-6 && r.b <= 1.0 + 1e-6);
}

#[test]
fn a_crop_drag_cannot_leave_the_picture_or_shrink_below_the_minimum() {
    let mut v = View::new(NodeKind::Transform);
    let [x0, y0, _, _] = crop_screen(&v);
    let corner = Pos2::new(x0, y0);
    drag_screen(&mut v, corner, corner + Vec2::new(-200.0, -200.0));
    let r = crop_of(&v);
    assert!(r.l >= 0.0 && r.t >= 0.0, "{r:?}");
    let [x0, y0, x1, y1] = crop_screen(&v);
    drag_screen(&mut v, Pos2::new(x1, y1), Pos2::new(x0 - 50.0, y0 - 50.0));
    let r = crop_of(&v);
    assert!(r.w() * FRAME_W as f32 >= crop_logic::MIN_SIDE_PX - 0.1 && r.h() * FRAME_H as f32 >= crop_logic::MIN_SIDE_PX - 0.1, "{r:?}");
}

#[test]
fn the_crop_tool_shows_the_input_of_the_node_and_other_nodes_show_the_result() {
    let mut v = View::new(NodeKind::Transform);
    let src = v.s.node_by_index(0).unwrap();
    assert_eq!(v.s.render_target(), Some((src, Some(0))), "the picture entering the Crop / Rotate node");
    v.s.selected_node = Some(src);
    assert_eq!(v.s.tool_mode(), ToolMode::None);
    assert_eq!(v.s.render_target(), v.s.target_node().map(|n| (n, None)), "the normal result again");
    let lonely = v.s.project.graph.add_node(NodeKind::Transform, [0.0, 0.0]);
    v.s.selected_node = Some(lonely);
    assert_eq!(v.s.tool_mode(), ToolMode::None, "without a connected input there is no crop view");
}

#[test]
fn the_viewer_turns_the_picture_with_the_straighten_angle() {
    let mut v = View::new(NodeKind::Transform);
    let id = v.node;
    let img = v.s.current_image();
    v.s.project.graph.node_mut(id).unwrap().set_param("angle", img, 7.0);
    v.s.project.graph.node_mut(id).unwrap().set_param("rotate", img, 1.0);
    prepare_viewer(&mut v.viewer, &v.s);
    let o = v.viewer.orient.expect("oriented");
    assert_eq!((o.quarter_turns, o.angle_deg), (1, 7.0));
    assert!(v.viewer.tool_active && !v.viewer.pinch_to_tool);
    v.s.selected_node = v.s.node_by_index(0);
    prepare_viewer(&mut v.viewer, &v.s);
    assert!(v.viewer.orient.is_none() && !v.viewer.tool_active);
}

/// The demo graph with the drawing feeding its Exposure node and a Crop / Rotate (and more)
/// inserted between the Exposure and the Saturation node that follows it.
fn brush_behind(v: &mut View, after: &[NodeKind]) -> (NodeId, Vec<NodeId>) {
    let g = &mut v.s.project.graph;
    let find = |g: &rawgraph_core::Graph, k: NodeKind| g.nodes().find(|n| n.kind == k).map(|n| n.id).unwrap();
    let exp = find(g, NodeKind::Exposure);
    let sat = find(g, NodeKind::Saturation);
    let port = g.expose_parameter(exp, "ev", rawgraph_core::PortType::Images).unwrap();
    g.connect(v.node, exp, port).unwrap();
    let mut prev = exp;
    let mut added = Vec::new();
    for k in after {
        let n = g.add_node(*k, [0.0, 0.0]);
        g.connect(prev, n, 0).unwrap();
        prev = n;
        added.push(n);
    }
    g.connect(prev, sat, 0).unwrap();
    // The proxy is in memory, as it is once the first frame has been made.
    rawgraph_engine::ImageProvider::image(v.s.provider.as_ref(), v.s.current_image()).unwrap();
    (exp, added)
}

#[test]
fn a_stroke_painted_behind_a_crop_is_stored_in_the_frame_of_the_node_that_uses_the_mask() {
    let mut v = View::new(NodeKind::Draw);
    let (exp, crops) = brush_behind(&mut v, &[NodeKind::Transform]);
    v.s.viewed_node = Some(crops[0]);
    let img = v.s.current_image();
    {
        let n = v.s.project.graph.node_mut(crops[0]).unwrap();
        n.set_param("left", img, 0.5);
        n.set_param("top", img, 0.25);
    }
    assert!(matches!(v.s.brush_plan(v.node), crate::frames::BrushPlan::Mapped { consumer, .. } if consumer == exp));
    assert_eq!(v.s.render_target().map(|t| t.0), v.s.target_node(), "the cropped result stays on screen while painting");
    v.frame(vec![]);
    assert!(v.viewer.banner.is_none(), "nothing to explain: the mapping is exact");
    v.drag_path(&[[0.2, 0.3], [0.8, 0.7]]);
    let s = &v.strokes()[0];
    // Oracle: the fx mapping of the crop with the size of the picture entering it.
    let (w, h) = v.s.proxy_size(img).unwrap();
    let params = transform_params(v.s.project.graph.node(crops[0]).unwrap(), img);
    let (ex, ey) = rawgraph_fx::transform_output_to_input(w, h, &params, 0.2, 0.3);
    let first = s.points[0];
    assert!((first[0] - ex).abs() < 0.01 && (first[1] - ey).abs() < 0.01, "first point {first:?}, expected ({ex}, {ey})");
    assert!(first[0] > 0.5, "it lies in the right half of the consumer frame, where the crop is");
    let last = *s.points.last().unwrap();
    let (lx, ly) = rawgraph_fx::transform_output_to_input(w, h, &params, 0.8, 0.7);
    assert!((last[0] - lx).abs() < 0.01 && (last[1] - ly).abs() < 0.01, "last point {last:?}, expected ({lx}, {ly})");
}

#[test]
fn a_stroke_behind_a_quarter_turn_and_a_fine_rotation_lands_where_the_oracle_says() {
    let mut v = View::new(NodeKind::Draw);
    let (_, crops) = brush_behind(&mut v, &[NodeKind::Transform, NodeKind::Transform]);
    v.s.viewed_node = Some(crops[1]);
    let img = v.s.current_image();
    v.s.project.graph.node_mut(crops[0]).unwrap().set_param("rotate", img, 1.0);
    v.s.project.graph.node_mut(crops[1]).unwrap().set_param("angle", img, 6.0);
    v.frame(vec![]);
    v.drag_path(&[[0.4, 0.45], [0.55, 0.5]]);
    let (w, h) = v.s.proxy_size(img).unwrap();
    let p0 = transform_params(v.s.project.graph.node(crops[0]).unwrap(), img);
    let p1 = transform_params(v.s.project.graph.node(crops[1]).unwrap(), img);
    let mid = rawgraph_fx::transform_size(w, h, &p0);
    let (ax, ay) = rawgraph_fx::transform_output_to_input(mid.0, mid.1, &p1, 0.4, 0.45);
    let (bx, by) = rawgraph_fx::transform_output_to_input(w, h, &p0, ax, ay);
    let first = v.strokes()[0].points[0];
    assert!((first[0] - bx).abs() < 0.01 && (first[1] - by).abs() < 0.01, "first point {first:?}, expected ({bx}, {by})");
}

#[test]
fn a_lens_behind_the_mask_shows_the_consumer_picture_with_a_label_while_painting() {
    let mut v = View::new(NodeKind::Draw);
    let (exp, _) = brush_behind(&mut v, &[NodeKind::Transform, NodeKind::LensManual]);
    assert!(matches!(v.s.brush_plan(v.node), crate::frames::BrushPlan::ShowConsumer { consumer } if consumer == exp));
    assert_eq!(v.s.render_target(), Some((exp, None)), "the picture as it leaves the node that uses the mask");
    v.frame(vec![]);
    assert_eq!(v.viewer.banner.as_deref(), Some("Painting on: Exposure \u{2014} later geometry is not shown while painting"));
    // With another node selected the normal result is shown again and the label is gone.
    v.s.selected_node = v.s.node_by_index(0);
    assert_eq!(v.s.render_target(), v.s.target_node().map(|n| (n, None)));
    v.frame(vec![]);
    assert!(v.viewer.banner.is_none());
}
