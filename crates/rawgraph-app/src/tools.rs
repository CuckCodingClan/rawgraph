//! Interactive viewer tools: painting a drawing and editing the crop of a
//! Crop / Rotate node. The pure rules live in `brush_logic` and `crop_logic`;
//! this file connects them to the pointer, the graph and the painter.

use eframe::egui::{self, Color32, CursorIcon, Key, Modifiers, Pos2, Rect, Stroke, Vec2};
use rawgraph_core::{BrushStroke, ImageId, Node, NodeData, NodeId};
use rawgraph_fx::TransformParams;

use crate::brush_logic::{self, ImageView, StrokeBuilder};
use crate::crop_logic::{self, CropRect, Handle};
use crate::session::{Session, ToolMode};
use crate::viewer::{Geometry, Orient, Viewer};

/// Pointer state of the tools that lasts longer than a frame.
#[derive(Default)]
pub struct Tools {
    paint: Option<PaintDrag>,
    crop: Option<CropDrag>,
    /// Dragging the straighten knob: (pointer angle at the press in degrees, angle then, moved).
    straighten: Option<(f32, f32, bool)>,
    /// On-picture handles of the other nodes (gradients, vignette, motion direction, picker).
    pub handles: crate::handles::Handles,
}

struct PaintDrag {
    node: NodeId,
    image: ImageId,
    /// Strokes that existed when the pointer went down.
    base: Vec<BrushStroke>,
    builder: StrokeBuilder,
}

struct CropDrag {
    handle: Handle,
    start: CropRect,
    grab: [f32; 2],
}

/// Strokes the node has for `image` (its own in Individual mode, else the shared ones).
pub fn strokes_of(node: &Node, image: ImageId) -> Vec<BrushStroke> {
    match node.data(image) {
        Some(NodeData::Strokes(v)) => v.clone(),
        _ => Vec::new(),
    }
}

/// Stores the stroke list; `false` when the core refuses it (too many strokes or points).
pub fn set_strokes(session: &mut Session, node: NodeId, image: ImageId, strokes: Vec<BrushStroke>) -> bool {
    session.project.graph.node_mut(node).is_some_and(|n| n.set_data(image, Some(NodeData::Strokes(strokes))))
}

/// The Crop / Rotate parameters of `node` for `image` as the engine reads them.
pub fn transform_params(node: &Node, image: ImageId) -> TransformParams {
    TransformParams {
        left: node.param("left", image),
        top: node.param("top", image),
        right: node.param("right", image),
        bottom: node.param("bottom", image),
        angle_deg: node.param("angle", image),
        quarter_turns: node.choice("rotate", image) as u8,
        flip_h: node.toggle("flip_h", image),
        flip_v: node.toggle("flip_v", image),
    }
}

/// The crop rectangle stored in the node.
pub fn crop_rect_of(node: &Node, image: ImageId) -> CropRect {
    let p = transform_params(node, image);
    CropRect { l: p.left, t: p.top, r: p.right, b: p.bottom }
}

/// Writes the crop rectangle; only when it changed, so an idle tool never edits the graph.
pub fn set_crop_rect(session: &mut Session, node: NodeId, image: ImageId, rect: CropRect) -> bool {
    let Some(n) = session.project.graph.node_mut(node) else { return false };
    let now = crop_rect_of(n, image);
    let same = |a: f32, b: f32| (a - b).abs() < 1e-6;
    if same(now.l, rect.l) && same(now.t, rect.t) && same(now.r, rect.r) && same(now.b, rect.b) {
        return false;
    }
    n.set_param("left", image, rect.l);
    n.set_param("top", image, rect.t);
    n.set_param("right", image, rect.r);
    n.set_param("bottom", image, rect.b);
    true
}

/// Sets the viewer up for the selected node's tool (before `Viewer::show`).
pub fn prepare_viewer(viewer: &mut Viewer, session: &Session) {
    viewer.tool_active = false;
    viewer.pinch_to_tool = false;
    viewer.orient = None;
    viewer.banner = None;
    match session.tool_mode() {
        ToolMode::Brush(id) => {
            viewer.tool_active = true;
            viewer.pinch_to_tool = true;
            viewer.banner = crate::frames::painting_label(&session.project.graph, &session.brush_plan(id));
        }
        ToolMode::Crop(id) => {
            viewer.tool_active = true;
            if let Some(n) = session.project.graph.node(id) {
                let p = transform_params(n, session.current_image());
                viewer.orient = Some(Orient { quarter_turns: p.quarter_turns % 4, flip_h: p.flip_h, flip_v: p.flip_v, angle_deg: p.angle_deg });
            }
        }
        ToolMode::None => {}
    }
}

impl Tools {
    /// Handles the pointer for the tool of the selected node. Returns a message
    /// for the status bar when something could not be done.
    pub fn show(&mut self, ui: &egui::Ui, geom: &Geometry, session: &mut Session, frame: [u32; 2]) -> Option<String> {
        let mode = session.tool_mode();
        if !matches!(mode, ToolMode::Brush(_)) {
            self.paint = None;
        }
        if !matches!(mode, ToolMode::Crop(_)) {
            self.crop = None;
            self.straighten = None;
        }
        match mode {
            ToolMode::Brush(node) => self.brush(ui, geom, session, node),
            ToolMode::Crop(node) => {
                self.crop(ui, geom, session, node, frame);
                None
            }
            ToolMode::None => self.handles.show(ui, geom, session, frame),
        }
    }

    // ------------------------------------------------------------------ brush

    fn brush(&mut self, ui: &egui::Ui, geom: &Geometry, session: &mut Session, node: NodeId) -> Option<String> {
        let ctx = ui.ctx();
        let viewed_image = session.render_target().and_then(|(node, port)| session.image_for_target(node, port)).unwrap_or_else(|| session.current_image());
        let image = session.draw_image(node, viewed_image);
        if self.paint.as_ref().is_some_and(|p| p.node != node || p.image != image) {
            self.paint = None;
        }
        let view = ImageView { min: [geom.image_rect.min.x, geom.image_rect.min.y], size: [geom.image_rect.width(), geom.image_rect.height()] };
        // Strokes belong to the frame of the node that uses the mask. Behind Crop / Rotate nodes the
        // pointer is mapped back through them (see `frames`).
        let mapper = match session.brush_plan(node) {
            crate::frames::BrushPlan::Mapped { consumer, chain } => session.source_size_at(node, viewed_image).map(|src| crate::frames::Mapper::new(&session.project.graph, consumer, &chain, image, src)),
            _ => None,
        };
        let to_frame = |p: Pos2| -> Option<[f32; 2]> {
            let n = view.to_norm([p.x, p.y])?;
            Some(mapper.as_ref().map_or(n, |m| m.to_consumer(n)))
        };
        let aspect = match &mapper {
            Some(m) if m.consumer_size.1 > 0 => m.consumer_size.0 as f32 / m.consumer_size.1 as f32,
            _ if geom.image_rect.height() > 0.0 => geom.image_rect.width() / geom.image_rect.height(),
            _ => 1.0,
        };
        // A radius is a fraction of the long edge of the consumer's frame; one pixel of that frame is
        // one pixel of the picture shown (crops do not scale).
        let screen_radius = |r: f32| match &mapper {
            Some(m) => r * m.consumer_size.0.max(m.consumer_size.1) as f32 * geom.scale,
            None => view.screen_radius(r),
        };
        let typing = ctx.egui_wants_keyboard_input();
        let (pos, latest, space, middle, down, pressed, alt) = ctx.input(|i| {
            (i.pointer.hover_pos(), i.pointer.latest_pos(), i.key_down(Key::Space), i.pointer.middle_down(), i.pointer.primary_down(), i.pointer.primary_pressed(), i.modifiers.alt)
        });
        let space = space && !typing;
        let inside = geom.response.hovered() && pos.is_some_and(|p| geom.viewport.contains(p));
        let active = inside || self.paint.is_some();
        let mut message = None;

        // Brush size and hardness keys; Ctrl+scroll arrives as a zoom factor.
        if active && !typing {
            ctx.input_mut(|i| {
                let b = &mut session.brush;
                if i.consume_key(Modifiers::SHIFT, Key::OpenBracket) || i.consume_key(Modifiers::NONE, Key::OpenCurlyBracket) || i.consume_key(Modifiers::SHIFT, Key::OpenCurlyBracket) {
                    b.nudge_hardness(-brush_logic::HARDNESS_STEP);
                } else if i.consume_key(Modifiers::SHIFT, Key::CloseBracket) || i.consume_key(Modifiers::NONE, Key::CloseCurlyBracket) || i.consume_key(Modifiers::SHIFT, Key::CloseCurlyBracket) {
                    b.nudge_hardness(brush_logic::HARDNESS_STEP);
                }
                if i.consume_key(Modifiers::NONE, Key::OpenBracket) {
                    b.scale_size(1.0 / brush_logic::SIZE_STEP);
                }
                if i.consume_key(Modifiers::NONE, Key::CloseBracket) {
                    b.scale_size(brush_logic::SIZE_STEP);
                }
            });
            let zoom = ctx.input(|i| i.zoom_delta());
            if inside && (zoom - 1.0).abs() > 1e-4 {
                session.brush.scale_size(zoom);
            }
        }

        // Start a stroke.
        if pressed && inside && !space && !middle && self.paint.is_none() {
            let base = session.project.graph.node(node).map(|n| strokes_of(n, image)).unwrap_or_default();
            if !brush_logic::can_add_stroke(base.len()) {
                message = Some(format!("A drawing holds at most {} strokes", rawgraph_core::graph::MAX_STROKES));
            } else if let Some(first) = pos.and_then(to_frame) {
                self.paint = Some(PaintDrag { node, image, base, builder: StrokeBuilder::new(&session.brush, alt, first, aspect) });
            }
        }

        // Extend the stroke while the button is down, finish it when released.
        if let Some(drag) = &mut self.paint {
            let at = latest.and_then(to_frame);
            let mut changed = false;
            if let Some(n) = at {
                changed = if down { drag.builder.add(n) } else { drag.builder.finish(n) };
            }
            // The first point of a stroke is shown at once, not only after the first move.
            let live = brush_logic::with_stroke(&drag.base, &drag.builder.stroke);
            if (changed || drag.builder.stroke.points.len() == 1) && !set_strokes(session, node, image, live) {
                message = Some("This drawing cannot hold more points".to_owned());
            }
            if !down {
                self.paint = None;
            }
            ctx.request_repaint();
        }

        // The brush cursor.
        if inside && !space && !middle {
            ctx.set_cursor_icon(CursorIcon::None);
            if let Some(p) = pos {
                paint_brush_cursor(ui, geom.viewport, p, screen_radius(session.brush.radius()), session.brush.hardness, session.brush.erase != alt);
            }
        } else if inside {
            ctx.set_cursor_icon(CursorIcon::Grab);
        }
        message
    }

    // ------------------------------------------------------------------- crop

    fn crop(&mut self, ui: &egui::Ui, geom: &Geometry, session: &mut Session, node: NodeId, frame: [u32; 2]) {
        let ctx = ui.ctx();
        let image = session.current_image();
        let Some(n) = session.project.graph.node(node) else { return };
        let params = transform_params(n, image);
        let rect = crop_rect_of(n, image);
        let picture = crop_logic::picture_size(frame[0], frame[1], params.angle_deg, params.quarter_turns);
        if picture[0] <= 0.0 || picture[1] <= 0.0 {
            return;
        }
        let ratio = session.crop.aspect.ratio(picture, session.crop.swap);
        let center = geom.image_rect.center();
        let pic = Rect::from_center_size(center, Vec2::new(picture[0], picture[1]) * geom.scale);
        let to_screen = |r: CropRect| [pic.min.x + r.l * pic.width(), pic.min.y + r.t * pic.height(), pic.min.x + r.r * pic.width(), pic.min.y + r.b * pic.height()];
        let to_frac = |p: Pos2| [(p.x - pic.min.x) / pic.width(), (p.y - pic.min.y) / pic.height()];

        let typing = ctx.egui_wants_keyboard_input();
        let (pos, latest, space, middle, down, pressed, time, double) = ctx.input(|i| {
            (i.pointer.hover_pos(), i.pointer.latest_pos(), i.key_down(Key::Space), i.pointer.middle_down(), i.pointer.primary_down(), i.pointer.primary_pressed(), i.time, i.pointer.button_double_clicked(egui::PointerButton::Primary))
        });
        let space = space && !typing;
        let inside = geom.response.hovered() && pos.is_some_and(|p| geom.viewport.contains(p));

        // The fine grid shows while the straighten angle moves.
        if session.crop.seen != Some((node, image)) {
            // A different node or image: remember its angle without showing the grid.
            session.crop.seen = Some((node, image));
            session.crop.last_angle = params.angle_deg;
            session.crop.angle_changed_at = None;
        } else if (session.crop.last_angle - params.angle_deg).abs() > 1e-4 {
            session.crop.last_angle = params.angle_deg;
            session.crop.angle_changed_at = Some(time);
        }
        let grid = session.crop.angle_changed_at.is_some_and(|t| time - t < 0.8);
        if grid {
            ctx.request_repaint_after(std::time::Duration::from_millis(120));
        }

        // The straighten knob above the crop: drag around the picture centre to turn it.
        let screen = to_screen(rect);
        let knob = straighten_knob(screen, geom.viewport);
        let pointer_deg = |p: Pos2| (p.y - center.y).atan2(p.x - center.x).to_degrees();
        if pressed && inside && !space && !middle && self.crop.is_none() && self.straighten.is_none() && pos.is_some_and(|p| p.distance(knob) <= 10.0) {
            self.straighten = pos.map(|p| (pointer_deg(p), params.angle_deg, false));
        }
        if let Some((press_deg, start, moved)) = &mut self.straighten {
            if let Some(p) = latest {
                let delta = (pointer_deg(p) - *press_deg + 180.0).rem_euclid(360.0) - 180.0;
                *moved |= delta.abs() > 0.05;
                if *moved {
                    let angle = crop_logic::straighten_angle(*start, delta);
                    if let Some(n) = session.project.graph.node_mut(node) {
                        if (n.param("angle", image) - angle).abs() > 1e-5 {
                            n.set_param("angle", image, angle);
                        }
                    }
                }
            }
            if !down {
                if double && !*moved {
                    if let Some(n) = session.project.graph.node_mut(node) {
                        if n.param("angle", image) != 0.0 {
                            n.set_param("angle", image, 0.0);
                        }
                    }
                }
                self.straighten = None;
            }
            ctx.request_repaint();
        }
        let straightening = self.straighten.is_some();

        // Grab a handle.
        if pressed && inside && !space && !middle && self.crop.is_none() && !straightening {
            if let Some(h) = pos.and_then(|p| crop_logic::hit_handle(screen, [p.x, p.y], 11.0)) {
                let press = pos.map_or([0.0, 0.0], to_frac);
                let handle_at = h.position([rect.l, rect.t, rect.r, rect.b]);
                let grab = if h == Handle::Move { press } else { [press[0] - handle_at[0], press[1] - handle_at[1]] };
                self.crop = Some(CropDrag { handle: h, start: rect, grab });
            }
        }
        let mut rect = rect;
        let mut dragging = None;
        if let Some(drag) = &self.crop {
            dragging = Some(drag.handle);
            if let Some(p) = latest {
                let new = crop_logic::resize_rect(drag.start, drag.handle, to_frac(p), drag.grab, ratio, picture);
                if set_crop_rect(session, node, image, new) {
                    rect = new;
                }
            }
            if !down {
                self.crop = None;
            }
            ctx.request_repaint();
        }
        let screen = to_screen(rect);

        // Cursor.
        let knob = straighten_knob(screen, geom.viewport);
        let knob_hot = straightening || (inside && !space && self.crop.is_none() && pos.is_some_and(|p| p.distance(knob) <= 10.0));
        let hot = if knob_hot { None } else { dragging.or_else(|| if inside && !space { pos.and_then(|p| crop_logic::hit_handle(screen, [p.x, p.y], 11.0)) } else { None }) };
        if inside && space {
            ctx.set_cursor_icon(CursorIcon::Grab);
        } else if knob_hot {
            ctx.set_cursor_icon(if straightening { CursorIcon::Grabbing } else { CursorIcon::Grab });
        } else if let Some(h) = hot {
            ctx.set_cursor_icon(match h {
                Handle::TopLeft | Handle::BottomRight => CursorIcon::ResizeNwSe,
                Handle::TopRight | Handle::BottomLeft => CursorIcon::ResizeNeSw,
                Handle::Top | Handle::Bottom => CursorIcon::ResizeVertical,
                Handle::Left | Handle::Right => CursorIcon::ResizeHorizontal,
                Handle::Move => CursorIcon::Move,
            });
        }
        paint_crop(ui, geom.viewport, pic, screen, grid || straightening, hot);
        if inside || straightening {
            paint_straighten(ui, geom.viewport, screen, knob_hot);
        }
        if let Some(n) = session.project.graph.node(node) {
            let (w, h) = crop_logic::result_size(frame[0], frame[1], &transform_params(n, image));
            let text = format!("{w} x {h}");
            let painter = ui.painter_at(geom.viewport);
            let at = Pos2::new((screen[0] + screen[2]) * 0.5, (screen[3] + 14.0).min(geom.viewport.bottom() - 12.0));
            let galley = painter.layout_no_wrap(text, egui::FontId::monospace(12.0), Color32::WHITE);
            let r = Rect::from_center_size(at, galley.size() + Vec2::new(12.0, 6.0));
            painter.rect_filled(r, 4.0, Color32::from_rgba_unmultiplied(12, 14, 18, 215));
            painter.galley(r.center() - galley.size() / 2.0, galley, Color32::WHITE);
        }
    }
}

/// Where the straighten knob sits: a short stem above the middle of the crop's top edge, or
/// just inside it when the edge is at the top of the viewer.
pub fn straighten_knob(c: [f32; 4], viewport: Rect) -> Pos2 {
    let above = c[1] - 22.0;
    Pos2::new((c[0] + c[2]) * 0.5, if above < viewport.top() + 10.0 { c[1] + 22.0 } else { above })
}

fn paint_straighten(ui: &egui::Ui, viewport: Rect, c: [f32; 4], hot: bool) {
    let painter = ui.painter_at(viewport);
    let knob = straighten_knob(c, viewport);
    let foot = Pos2::new(knob.x, c[1]);
    painter.line_segment([foot, knob], Stroke::new(2.4, Color32::from_black_alpha(110)));
    painter.line_segment([foot, knob], Stroke::new(1.0, Color32::from_white_alpha(170)));
    let color = if hot { Color32::from_rgb(255, 210, 90) } else { Color32::WHITE };
    painter.circle_filled(knob, 6.5, Color32::from_black_alpha(150));
    painter.circle_stroke(knob, 4.8, Stroke::new(1.8, color));
}

fn paint_brush_cursor(ui: &egui::Ui, viewport: Rect, p: Pos2, radius: f32, hardness: f32, erase: bool) {
    let painter = ui.painter_at(viewport);
    let base = if erase { Color32::from_rgb(255, 150, 90) } else { Color32::WHITE };
    let r = radius.max(1.5);
    painter.circle_stroke(p, r + 1.0, Stroke::new(1.0, Color32::from_black_alpha(190)));
    painter.circle_stroke(p, r, Stroke::new(1.4, base));
    if hardness < 0.98 && r > 8.0 {
        painter.circle_stroke(p, r * hardness.max(0.05), Stroke::new(1.0, base.gamma_multiply(0.55)));
    }
    painter.circle_filled(p, 1.6, base);
}

/// Dimmed outside, thirds, border, handles and the fine grid of the crop tool.
fn paint_crop(ui: &egui::Ui, viewport: Rect, pic: Rect, c: [f32; 4], grid: bool, hot: Option<Handle>) {
    let painter = ui.painter_at(viewport);
    let crop = Rect::from_min_max(Pos2::new(c[0], c[1]), Pos2::new(c[2], c[3]));
    let dim = Color32::from_black_alpha(150);
    // Four rectangles around the crop.
    painter.rect_filled(Rect::from_min_max(viewport.min, Pos2::new(viewport.max.x, crop.min.y.max(viewport.min.y))), 0.0, dim);
    painter.rect_filled(Rect::from_min_max(Pos2::new(viewport.min.x, crop.max.y.min(viewport.max.y)), viewport.max), 0.0, dim);
    painter.rect_filled(Rect::from_min_max(Pos2::new(viewport.min.x, crop.min.y), Pos2::new(crop.min.x.max(viewport.min.x), crop.max.y)), 0.0, dim);
    painter.rect_filled(Rect::from_min_max(Pos2::new(crop.max.x.min(viewport.max.x), crop.min.y), Pos2::new(viewport.max.x, crop.max.y)), 0.0, dim);
    if grid {
        let faint = Stroke::new(1.0, Color32::from_white_alpha(70));
        for i in 1..16 {
            let f = i as f32 / 16.0;
            let x = pic.min.x + pic.width() * f;
            let y = pic.min.y + pic.height() * f;
            painter.line_segment([Pos2::new(x, pic.min.y), Pos2::new(x, pic.max.y)], faint);
            painter.line_segment([Pos2::new(pic.min.x, y), Pos2::new(pic.max.x, y)], faint);
        }
    }
    let third = Stroke::new(1.0, Color32::from_white_alpha(130));
    for i in 1..3 {
        let f = i as f32 / 3.0;
        let x = crop.min.x + crop.width() * f;
        let y = crop.min.y + crop.height() * f;
        painter.line_segment([Pos2::new(x, crop.min.y), Pos2::new(x, crop.max.y)], third);
        painter.line_segment([Pos2::new(crop.min.x, y), Pos2::new(crop.max.x, y)], third);
    }
    painter.rect_stroke(crop, 0.0, Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Middle);
    for h in Handle::ALL {
        let p = h.position(c);
        let size = if h.is_corner() { 9.0 } else { 7.0 };
        let fill = if hot == Some(h) { Color32::from_rgb(255, 210, 90) } else { Color32::WHITE };
        let r = Rect::from_center_size(Pos2::new(p[0], p[1]), Vec2::splat(size));
        painter.rect_filled(r, 1.5, fill);
        painter.rect_stroke(r, 1.5, Stroke::new(1.0, Color32::from_black_alpha(200)), egui::StrokeKind::Outside);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::NodeKind;

    #[test]
    fn crop_rectangles_are_only_written_when_they_change() {
        let mut s = crate::session::tests::demo_session();
        let tf = s.project.graph.add_node(NodeKind::Transform, [0.0, 0.0]);
        let img = ImageId(1);
        let before = s.project.graph.clone();
        assert!(!set_crop_rect(&mut s, tf, img, CropRect::FULL), "an unchanged rectangle writes nothing");
        assert_eq!(s.project.graph, before);
        assert!(set_crop_rect(&mut s, tf, img, CropRect { l: 0.1, t: 0.2, r: 0.9, b: 0.8 }));
        let n = s.project.graph.node(tf).unwrap();
        assert_eq!(crop_rect_of(n, img), CropRect { l: 0.1, t: 0.2, r: 0.9, b: 0.8 });
        let p = transform_params(n, img);
        assert_eq!((p.angle_deg, p.quarter_turns, p.flip_h), (0.0, 0, false));
    }
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod interaction_tests;
