use super::*;
use crate::history::History;
use eframe::egui::{Event, Modifiers, PointerButton};
use rawgraph_core::NodeKind;

struct Harness {
    ctx: egui::Context,
    s: Session,
    node: NodeId,
    history: History,
    t: f64,
    pos: Pos2,
    steps: usize,
}

impl Harness {
    fn new() -> Self {
        let mut s = crate::session::tests::demo_session();
        let node = s.project.graph.add_node(NodeKind::PointCurve, [0.0, 0.0]);
        s.selected_node = Some(node);
        let history = History::new(&s.project.graph);
        let mut h = Self { ctx: egui::Context::default(), s, node, history, t: 0.0, pos: Pos2::new(5.0, 5.0), steps: 0 };
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    fn frame(&mut self, events: Vec<Event>) {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(420.0, 700.0))), time: Some(self.t), events, ..Default::default() };
        self.t += 0.04;
        let (s, node) = (&mut self.s, self.node);
        let out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| show(ui, s, node));
        });
        out.drop_without_applying_deltas();
        // The same rule as the app: a step is recorded when no gesture is running.
        let gesture = self.ctx.input(|i| i.pointer.any_down()) || self.ctx.dragged_id().is_some();
        if self.history.commit_if_changed(&self.s.project.graph, gesture) {
            self.steps += 1;
        }
    }

    fn plot(&self) -> PlotRect {
        let r: Rect = self.ctx.data(|d| d.get_temp(mem_id("curve-plot", self.node.0))).expect("the editor drew itself");
        PlotRect { x: r.min.x, y: r.min.y, w: r.width(), h: r.height() }
    }

    fn at(&self, p: [f32; 2]) -> Pos2 {
        let w = self.plot().to_widget(p);
        Pos2::new(w[0], w[1])
    }

    fn points(&self) -> Vec<[f32; 2]> {
        curve_of(self.s.project.graph.node(self.node).unwrap(), self.s.current_image())
    }

    fn move_to(&mut self, p: Pos2) {
        self.pos = p;
        self.frame(vec![Event::PointerMoved(p)]);
    }

    fn button(&mut self, button: PointerButton, pressed: bool) {
        let ev = Event::PointerButton { pos: self.pos, button, pressed, modifiers: Modifiers::NONE };
        self.frame(vec![ev]);
    }

    fn drag(&mut self, from: [f32; 2], to: [f32; 2]) {
        let (a, b) = (self.at(from), self.at(to));
        self.move_to(a);
        self.button(PointerButton::Primary, true);
        self.move_to(a + (b - a) * 0.3);
        self.move_to(a + (b - a) * 0.8);
        self.move_to(b);
        self.button(PointerButton::Primary, false);
        self.frame(vec![]);
        self.frame(vec![]);
    }
}

fn close(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() < 0.01 && (a[1] - b[1]).abs() < 0.01
}

#[test]
fn a_click_on_empty_space_adds_a_point_as_one_undo_step() {
    let mut h = Harness::new();
    assert_eq!(h.points(), curve_logic::identity());
    h.drag([0.5, 0.7], [0.5, 0.7]);
    let pts = h.points();
    assert_eq!(pts.len(), 3);
    assert!(close(pts[1], [0.5, 0.7]), "{pts:?}");
    assert_eq!(h.steps, 1, "the add is one undo step");
}

#[test]
fn dragging_moves_the_point_live_and_is_one_undo_step() {
    let mut h = Harness::new();
    h.drag([0.3, 0.3], [0.3, 0.3]);
    let before = h.steps;
    // Press on the new point, move while the button is down: the data changes live.
    let (a, b) = (h.at([0.3, 0.3]), h.at([0.4, 0.8]));
    h.move_to(a);
    h.button(PointerButton::Primary, true);
    h.move_to(b);
    assert!(close(h.points()[1], [0.4, 0.8]), "live while dragging: {:?}", h.points());
    assert_eq!(h.steps, before, "no step recorded while the button is down");
    h.button(PointerButton::Primary, false);
    h.frame(vec![]);
    assert_eq!(h.steps, before + 1, "one step for the whole drag");
    // Undo brings back the previous curve in one go.
    let g = h.history.undo().unwrap();
    let back = curve_of(g.node(h.node).unwrap(), h.s.current_image());
    assert!(close(back[1], [0.3, 0.3]), "{back:?}");
}

#[test]
fn a_point_cannot_be_dragged_past_its_neighbours() {
    let mut h = Harness::new();
    h.drag([0.3, 0.3], [0.3, 0.3]);
    h.drag([0.7, 0.6], [0.7, 0.6]);
    h.drag([0.3, 0.3], [0.95, 0.2]);
    let pts = h.points();
    assert_eq!(pts.len(), 4);
    assert!(pts.windows(2).all(|w| w[0][0] < w[1][0]), "order kept: {pts:?}");
    assert!(pts[1][0] < pts[2][0] && (pts[1][1] - 0.2).abs() < 0.01, "{pts:?}");
}

#[test]
fn right_click_and_double_click_remove_a_point_but_never_below_two() {
    let mut h = Harness::new();
    h.drag([0.5, 0.7], [0.5, 0.7]);
    assert_eq!(h.points().len(), 3);
    let p = h.at([0.5, 0.7]);
    h.move_to(p);
    h.button(PointerButton::Secondary, true);
    h.button(PointerButton::Secondary, false);
    h.frame(vec![]);
    assert_eq!(h.points(), curve_logic::identity(), "removed");
    // An end point: refused.
    let p = h.at([0.0, 0.0]);
    h.move_to(p);
    h.button(PointerButton::Secondary, true);
    h.button(PointerButton::Secondary, false);
    assert_eq!(h.points().len(), 2, "never fewer than two points");
    // Double-click on a point removes it.
    h.drag([0.25, 0.4], [0.25, 0.4]);
    assert_eq!(h.points().len(), 3);
    let p = h.at([0.25, 0.4]);
    h.move_to(p);
    // The click that added the point must be older than the double-click time.
    for _ in 0..12 {
        h.frame(vec![]);
    }
    for _ in 0..2 {
        h.button(PointerButton::Primary, true);
        h.button(PointerButton::Primary, false);
    }
    h.frame(vec![]);
    assert_eq!(h.points().len(), 2, "double-click removed it: {:?}", h.points());
}

#[test]
fn the_curve_is_stored_per_image_in_individual_mode() {
    use rawgraph_core::Mode;
    let mut h = Harness::new();
    h.s.set_mode(h.node, Mode::Individual);
    h.drag([0.5, 0.8], [0.5, 0.8]);
    let own = h.s.current_image();
    let other = ImageId(own.0 + 100);
    let n = h.s.project.graph.node(h.node).unwrap();
    assert!(n.has_individual_data(own), "this image has its own curve");
    assert_eq!(curve_of(n, other), curve_logic::identity(), "another image keeps the identity");
}

#[test]
fn the_identity_can_be_stored_again_and_the_plot_follows_the_inspector_width() {
    let mut h = Harness::new();
    h.drag([0.5, 0.8], [0.5, 0.8]);
    let img = h.s.current_image();
    assert!(store(&mut h.s, h.node, img, curve_logic::identity()).is_ok());
    assert_eq!(h.points(), curve_logic::identity());
    assert!(store(&mut h.s, NodeId(999), img, curve_logic::identity()).is_err());
    assert!(store(&mut h.s, h.node, img, vec![[0.5, 0.5]]).is_err(), "the core refuses a one-point curve");
    assert!(plot_side(300.0) >= 180.0 && plot_side(2000.0) <= 360.0 && plot_side(500.0) > plot_side(400.0));
    assert!((plot_side(390.0) - 260.0).abs() < 12.0, "about 260 pt in a default-width inspector: {}", plot_side(390.0));
}

#[test]
fn the_hue_vs_hue_tab_keeps_the_ends_level_and_the_tab_dot_marks_the_changed_curve() {
    let mut h = Harness::new();
    let img = h.s.current_image();
    select_channel(&mut h.s, h.node, img, 5);
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(visible_channel(h.s.project.graph.node(h.node).unwrap(), img), 5);
    assert_eq!(h.points(), neutral_curve(5), "the colour channels start at the flat middle line");
    // Drag the first point up: the last one follows, so the hue wraps around smoothly.
    h.drag([0.0, 0.5], [0.0, 0.8]);
    let pts = h.points();
    assert_eq!(pts.len(), 2);
    assert!((pts[0][1] - 0.8).abs() < 0.02 && pts[0][1] == pts[1][1], "{pts:?}");
    let node = h.s.project.graph.node(h.node).unwrap();
    assert_eq!(modified_channels(node, img), [false, false, false, false, false, true, false], "only the Hue vs Hue tab carries a dot");
    // A point in the middle may move freely; the ends stay level.
    h.drag([0.5, 0.8], [0.5, 0.2]);
    let pts = h.points();
    assert_eq!(pts.len(), 3);
    assert!(pts[0][1] == pts[2][1] && (pts[1][1] - 0.2).abs() < 0.02, "{pts:?}");
    // Moving the last point moves the first.
    h.drag([1.0, pts[2][1]], [1.0, 0.35]);
    let pts = h.points();
    assert!(pts[0][1] == pts[2][1] && (pts[0][1] - 0.35).abs() < 0.02, "{pts:?}");
    // Another tab starts neutral and leaves this curve alone.
    select_channel(&mut h.s, h.node, img, 0);
    h.frame(vec![]);
    assert_eq!(h.points(), vec![[0.0, 0.0], [1.0, 1.0]]);
    let node = h.s.project.graph.node(h.node).unwrap();
    assert_eq!(curve_for(node, img, 5).len(), 3);
    assert!(h.steps >= 3, "every gesture is its own undo step");
}
