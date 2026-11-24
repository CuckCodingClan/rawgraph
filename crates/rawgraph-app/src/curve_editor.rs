//! The curves editor of the Curves node's inspector: one tab per channel (Luminance, Red, Green,
//! Blue, Sat vs Sat, Hue vs Hue, Hue vs Sat), a plot for the visible curve.
//!
//! Click on empty space adds a point, dragging moves it, double-click or right-click removes it.
//! The picture updates live while dragging; the history records one undo step when the pointer is
//! released. The node stores its curves as one `NodeData::CurveSet` (an older single `Curve` is
//! still read and becomes a set on the first edit); the `channel` parameter says which tab is
//! shown. All rules live in `curve_logic`.

use eframe::egui::{self, Color32, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use rawgraph_core::{neutral_curve, CurveEntry, ImageId, Node, NodeData, NodeId, CURVE_CHANNELS};

use crate::curve_logic::{self, PlotRect};
use crate::session::Session;

/// Key of a value kept in egui's temporary memory.
fn mem_id(tag: &'static str, node: u64) -> egui::Id {
    egui::Id::NULL.with((tag, node))
}

const HIT_RADIUS: f32 = 9.0;
const MARGIN: f32 = 8.0;
/// Height of the hue colour strip under the plot of the hue channels.
const STRIP_H: f32 = 12.0;

/// The tab (channel) the editor shows for `image`.
pub fn visible_channel(node: &Node, image: ImageId) -> u8 {
    node.choice("channel", image).min(CURVE_CHANNELS.len() - 1) as u8
}

/// Every curve the node holds for `image`, as stored (an older single curve is a set of one on the
/// channel of the `channel` parameter). Neutral entries are not dropped here.
pub fn entries_of(node: &Node, image: ImageId) -> Vec<CurveEntry> {
    match node.data(image) {
        Some(NodeData::Curve(p)) => vec![CurveEntry { channel: visible_channel(node, image), points: p.clone() }],
        Some(NodeData::CurveSet(e)) => e.clone(),
        _ => Vec::new(),
    }
}

/// The curve of one channel (its neutral curve when the node holds none).
pub fn curve_for(node: &Node, image: ImageId, channel: u8) -> Vec<[f32; 2]> {
    entries_of(node, image).into_iter().find(|e| e.channel == channel).map_or_else(|| neutral_curve(channel), |e| e.points)
}

/// The curve the editor shows (the one of the visible channel).
pub fn curve_of(node: &Node, image: ImageId) -> Vec<[f32; 2]> {
    curve_for(node, image, visible_channel(node, image))
}

/// Which channels hold a curve that changes the picture (for the dot on the tabs).
pub fn modified_channels(node: &Node, image: ImageId) -> [bool; 7] {
    let mut out = [false; 7];
    for e in entries_of(node, image) {
        if let Some(slot) = out.get_mut(e.channel as usize) {
            *slot = e.points != neutral_curve(e.channel);
        }
    }
    out
}

/// Stores the curve of one channel; a curve equal to the neutral one removes the channel's entry.
/// The other channels are kept.
pub fn store_channel(session: &mut Session, id: NodeId, image: ImageId, channel: u8, points: Vec<[f32; 2]>) -> Result<(), String> {
    let Some(n) = session.project.graph.node_mut(id) else { return Err("The node no longer exists".to_owned()) };
    let mut entries = entries_of(n, image);
    entries.retain(|e| e.channel != channel);
    if points != neutral_curve(channel) {
        entries.push(CurveEntry { channel, points });
    }
    entries.sort_by_key(|e| e.channel);
    if n.set_data(image, Some(NodeData::CurveSet(entries))) {
        Ok(())
    } else {
        Err("The curve was refused: x values must increase and every value must lie in 0..1".to_owned())
    }
}

/// Stores the curve of the visible channel.
fn store(session: &mut Session, id: NodeId, image: ImageId, points: Vec<[f32; 2]>) -> Result<(), String> {
    let channel = session.project.graph.node(id).map_or(0, |n| visible_channel(n, image));
    store_channel(session, id, image, channel, points)
}

/// Shows another channel: first an older single curve is made a set (so it stays on its channel),
/// then the `channel` parameter moves.
pub fn select_channel(session: &mut Session, id: NodeId, image: ImageId, channel: u8) {
    let legacy = session.project.graph.node(id).is_some_and(|n| matches!(n.data(image), Some(NodeData::Curve(_))));
    if legacy {
        let current = session.project.graph.node(id).map_or(0, |n| visible_channel(n, image));
        let points = session.project.graph.node(id).map(|n| curve_for(n, image, current)).unwrap_or_default();
        let _ = store_channel(session, id, image, current, points);
    }
    if let Some(n) = session.project.graph.node_mut(id) {
        n.set_param("channel", image, f32::from(channel));
    }
}

/// Side length of the square plot for an inspector of `available` points.
pub fn plot_side(available: f32) -> f32 {
    ((available - 8.0) * 0.68).clamp(180.0, 360.0)
}

/// What a point's y value means on the colour channels ("+12 deg", "x1.40"); `None` on the others.
pub fn y_readout(channel: u8, y: f32) -> Option<String> {
    match channel {
        5 => Some(format!("{:+.0}\u{b0}", (y - 0.5) * 120.0)),
        4 | 6 => Some(format!("x{:.2}", 2.0 * y)),
        _ => None,
    }
}

/// The axis label drawn in the plot of the colour channels.
pub fn y_axis_label(channel: u8) -> Option<&'static str> {
    match channel {
        5 => Some("\u{b1}60\u{b0}"),
        4 | 6 => Some("0 \u{2013} 2x"),
        _ => None,
    }
}

#[derive(Clone, Copy, Default)]
struct EditorState {
    selected: Option<usize>,
    dragging: Option<usize>,
}

/// Colour of the curve line of a channel.
fn line_color(channel: u8) -> Color32 {
    match channel {
        1 => Color32::from_rgb(255, 110, 100),
        2 => Color32::from_rgb(120, 230, 130),
        3 => Color32::from_rgb(120, 160, 255),
        _ => Color32::from_rgb(255, 214, 120),
    }
}

/// The tab strip. A small dot marks every tab whose curve is not neutral.
fn tabs(ui: &mut egui::Ui, current: u8, modified: [bool; 7]) -> Option<u8> {
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        for (i, name) in CURVE_CHANNELS.iter().enumerate() {
            let r = ui.selectable_label(i as u8 == current, *name);
            if modified[i] {
                let c = r.rect.right_top() + Vec2::new(-3.0, 4.0);
                ui.painter().circle_filled(c, 2.6, Color32::from_rgb(255, 196, 120));
            }
            if r.clicked() && i as u8 != current {
                picked = Some(i as u8);
            }
        }
    });
    picked
}

pub fn show(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let image = session.current_image();
    let Some(node) = session.project.graph.node(id) else { return };
    let channel = visible_channel(node, image);
    let mut points = curve_of(node, image);
    let modified = modified_channels(node, image);
    let key = mem_id("curve-editor", id.0);
    let mut state: EditorState = ui.data_mut(|d| d.get_temp(key)).unwrap_or_default();
    if state.selected.is_some_and(|i| i >= points.len()) {
        state.selected = None;
    }
    let mut message: Option<String> = None;
    let mut changed = false;

    ui.add_space(6.0);
    ui.label(RichText::new("Curves").strong());
    let tab_pick = tabs(ui, channel, modified);
    ui.add_space(2.0);
    let wrap = channel == 5;
    let side = plot_side(ui.available_width());
    let (resp, painter) = ui.allocate_painter(Vec2::splat(side), Sense::click_and_drag());
    let area = resp.rect.shrink(MARGIN);
    let plot = PlotRect { x: area.min.x, y: area.min.y, w: area.width(), h: area.height() };
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Group, true, "Curve editor"));
    // Where the plot is, for the synthetic-input tests.
    ui.data_mut(|d| d.insert_temp(mem_id("curve-plot", id.0), area));

    // Interaction.
    let (pos, latest, pressed, down) = ui.input(|i| (i.pointer.hover_pos(), i.pointer.latest_pos(), i.pointer.primary_pressed(), i.pointer.primary_down()));
    let to_array = |p: Pos2| [p.x, p.y];
    if pressed && resp.hovered() {
        if let Some(p) = pos {
            match curve_logic::hit_point(&points, plot, to_array(p), HIT_RADIUS) {
                Some(i) => {
                    state.selected = Some(i);
                    state.dragging = Some(i);
                }
                None => {
                    if let Some(i) = curve_logic::insert_point(&mut points, plot.to_curve(to_array(p))) {
                        state.selected = Some(i);
                        state.dragging = Some(i);
                        changed = true;
                    } else {
                        message = Some(format!("The curve holds at most {} points", curve_logic::MAX_POINTS));
                    }
                }
            }
        }
    }
    if let Some(i) = state.dragging {
        if down {
            if let Some(p) = latest {
                let target = plot.to_curve(to_array(p));
                if let Some(set) = curve_logic::move_point(&mut points, i, target) {
                    if wrap {
                        curve_logic::wrap_ends(&mut points, i);
                    }
                    let _ = set;
                    changed |= curve_of(node, image) != points;
                }
            }
        } else {
            state.dragging = None;
        }
    }
    if resp.secondary_clicked() || resp.double_clicked() {
        if let Some(p) = resp.interact_pointer_pos() {
            if let Some(i) = curve_logic::hit_point(&points, plot, to_array(p), HIT_RADIUS) {
                if curve_logic::remove_point(&mut points, i) {
                    changed = true;
                    state.selected = None;
                    state.dragging = None;
                } else {
                    message = Some("A curve needs at least two points".to_owned());
                }
            }
        }
    }

    // Drawing: for Luminance the histogram of the picture entering this node sits behind the curve.
    let hist: Option<Vec<f32>> = if channel == 0 { session.curve_hist.as_ref().filter(|h| h.node == id && h.image == image).map(|h| h.bins.clone()) } else { None };
    draw_plot(&painter, resp.rect, plot, &points, channel, state.selected, pos.and_then(|p| curve_logic::hit_point(&points, plot, to_array(p), HIT_RADIUS)).filter(|_| resp.hovered()), hist.as_deref());
    ui.data_mut(|d| d.insert_temp(mem_id("curve-hist-drawn", id.0), hist.is_some()));
    if matches!(channel, 5 | 6) {
        hue_strip(ui, plot);
    }

    // Numeric fields of the selected point.
    ui.add_space(4.0);
    ui.horizontal(|ui| match state.selected.and_then(|i| points.get(i).map(|p| (i, *p))) {
        Some((i, p)) => {
            let (mut x, mut y) = (p[0], p[1]);
            ui.label("Point");
            let rx = ui.add(egui::DragValue::new(&mut x).range(0.0..=1.0).speed(0.002).max_decimals(3).prefix("in "));
            let ry = ui.add(egui::DragValue::new(&mut y).range(0.0..=1.0).speed(0.002).max_decimals(3).prefix("out "));
            if (rx.changed() || ry.changed()) && curve_logic::move_point(&mut points, i, [x, y]).is_some() {
                if wrap {
                    curve_logic::wrap_ends(&mut points, i);
                }
                changed = true;
            }
            if let Some(t) = y_readout(channel, points.get(i).map_or(y, |p| p[1])) {
                ui.label(RichText::new(t).weak());
            }
        }
        None => {
            ui.label(RichText::new("Click the plot to add a point, drag to move it.").weak());
        }
    });

    ui.horizontal(|ui| {
        let neutral = points == neutral_curve(channel);
        let tip = if channel >= 4 { "Back to the flat line (no change); the other curves stay" } else { "Back to the straight diagonal; the other curves stay" };
        if ui.add_enabled(!neutral, egui::Button::new("Reset")).on_hover_text(tip).clicked() {
            points = neutral_curve(channel);
            state = EditorState::default();
            changed = true;
        }
        ui.label(RichText::new(format!("{} points", points.len())).weak());
    });
    let note = match channel {
        5 => "Hue vs Hue: the curve height turns the hue (middle = no change, top +60 degrees, bottom -60); the first and last point stay at the same height because the hue wraps around.",
        6 => "Hue vs Sat: the curve height scales the saturation of that hue (middle = no change, top 2x, bottom 0).",
        4 => "Sat vs Sat: the curve height scales the saturation of pixels by how saturated they are (middle = no change, top 2x, bottom 0).",
        _ => "Double-click or right-click a point removes it. Both axes use the perceptual scale t = Y / (Y + 0.18).",
    };
    ui.label(RichText::new(note).weak().small());

    if changed {
        if let Err(e) = store(session, id, image, points) {
            message = Some(e);
        }
    }
    if let Some(c) = tab_pick {
        select_channel(session, id, image, c);
        state = EditorState::default();
    }
    if let Some(m) = &message {
        ui.label(RichText::new(m).color(Color32::from_rgb(255, 150, 120)));
    }
    // Keep a message for a few frames: remember it next to the state.
    let msg_key = mem_id("curve-editor-message", id.0);
    if message.is_some() {
        ui.data_mut(|d| d.insert_temp(msg_key, message));
    } else if changed || pressed {
        ui.data_mut(|d| d.remove::<Option<String>>(msg_key));
    } else if let Some(Some(m)) = ui.data(|d| d.get_temp::<Option<String>>(msg_key)) {
        ui.label(RichText::new(m).color(Color32::from_rgb(255, 150, 120)));
    }
    ui.data_mut(|d| d.insert_temp(key, state));
}

/// Colour of hue `h` (0..1) for the strip; the same hue circle the curve channels use (0 red,
/// 1/6 yellow, 1/3 green, 1/2 cyan, 2/3 blue, 5/6 magenta).
pub fn hue_color(h: f32) -> Color32 {
    Color32::from(egui::ecolor::Hsva::new(h.rem_euclid(1.0), 0.9, 0.9, 1.0))
}

/// A strip of hue colours along the x axis of the plot.
fn hue_strip(ui: &mut egui::Ui, plot: PlotRect) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), STRIP_H), Sense::hover());
    let strip = Rect::from_min_size(Pos2::new(plot.x, rect.top() + 2.0), Vec2::new(plot.w, STRIP_H - 2.0));
    let steps = 72;
    for i in 0..steps {
        let (a, b) = (i as f32 / steps as f32, (i + 1) as f32 / steps as f32);
        let r = Rect::from_min_max(Pos2::new(strip.left() + a * strip.width(), strip.top()), Pos2::new(strip.left() + b * strip.width() + 0.5, strip.bottom()));
        ui.painter().rect_filled(r, 0.0, hue_color((a + b) / 2.0));
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_plot(painter: &egui::Painter, outer: Rect, plot: PlotRect, points: &[[f32; 2]], channel: u8, selected: Option<usize>, hovered: Option<usize>, hist: Option<&[f32]>) {
    let area = Rect::from_min_size(Pos2::new(plot.x, plot.y), Vec2::new(plot.w, plot.h));
    painter.rect_filled(outer, 4.0, Color32::from_gray(22));
    painter.rect_filled(area, 0.0, Color32::from_gray(30));
    if let Some(bins) = hist.filter(|b| !b.is_empty()) {
        // Faint bars on the perceptual axis; the tallest bar fills 90 % of the plot.
        let n = bins.len() as f32;
        let fill = Color32::from_rgba_unmultiplied(130, 170, 230, 70);
        for (i, h) in bins.iter().enumerate() {
            let h = if h.is_finite() { h.clamp(0.0, 1.0) } else { 0.0 };
            if h <= 0.0 {
                continue;
            }
            let x0 = plot.x + plot.w * (i as f32 / n);
            let x1 = plot.x + plot.w * ((i + 1) as f32 / n);
            let top = plot.y + plot.h * (1.0 - 0.9 * h);
            painter.rect_filled(Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, plot.y + plot.h)), 0.0, fill);
        }
    }
    let to_pos = |p: [f32; 2]| {
        let w = plot.to_widget(p);
        Pos2::new(w[0], w[1])
    };
    // Light grid (quarters), stronger in the middle.
    for i in 0..=4 {
        let f = i as f32 / 4.0;
        let c = if i == 0 || i == 4 { Color32::from_gray(70) } else { Color32::from_gray(48) };
        painter.line_segment([to_pos([f, 0.0]), to_pos([f, 1.0])], Stroke::new(1.0, c));
        painter.line_segment([to_pos([0.0, f]), to_pos([1.0, f])], Stroke::new(1.0, c));
    }
    // The neutral curve: the diagonal, or for the colour channels the horizontal line in the middle.
    if channel >= 4 {
        painter.line_segment([to_pos([0.0, 0.5]), to_pos([1.0, 0.5])], Stroke::new(1.0, Color32::from_gray(110)));
        if let Some(label) = y_axis_label(channel) {
            painter.text(to_pos([0.0, 1.0]) + Vec2::new(4.0, 2.0), egui::Align2::LEFT_TOP, label, egui::FontId::proportional(11.0), Color32::from_gray(150));
            painter.text(to_pos([0.0, 0.5]) + Vec2::new(4.0, -2.0), egui::Align2::LEFT_BOTTOM, if channel == 5 { "0\u{b0}" } else { "1x" }, egui::FontId::proportional(10.0), Color32::from_gray(120));
        }
    } else {
        painter.line_segment([to_pos([0.0, 0.0]), to_pos([1.0, 1.0])], Stroke::new(1.0, Color32::from_gray(96)));
    }
    let line: Vec<Pos2> = curve_logic::sample(points, 160).into_iter().map(to_pos).collect();
    painter.add(egui::Shape::line(line, Stroke::new(2.0, line_color(channel))));
    for (i, p) in points.iter().enumerate() {
        let c = to_pos(*p);
        let hot = selected == Some(i) || hovered == Some(i);
        painter.circle_filled(c, if hot { 5.5 } else { 4.5 }, if selected == Some(i) { Color32::from_rgb(255, 120, 80) } else { Color32::WHITE });
        painter.circle_stroke(c, if hot { 5.5 } else { 4.5 }, Stroke::new(1.2, Color32::from_black_alpha(200)));
    }
}

#[cfg(test)]
#[path = "curve_editor_tests.rs"]
mod tests;
