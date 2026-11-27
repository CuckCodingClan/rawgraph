//! Font-independent action icons, with accessible labels.
use eframe::egui::{self, pos2, vec2, Response, Sense, Stroke, Ui};

pub enum Icon { Reset, Delete, Pin, Unpin }

pub fn button(ui: &mut Ui, icon: Icon, label: &str, enabled: bool) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(26.0, 24.0), if enabled { Sense::click() } else { Sense::hover() });
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    let visuals = ui.style().interact(&response);
    let stroke = Stroke::new(1.6, if enabled { visuals.fg_stroke.color } else { ui.visuals().weak_text_color() });
    let p = ui.painter();
    p.rect_filled(rect, 3.0, visuals.bg_fill);
    if enabled && response.has_focus() {
        p.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(2.0, ui.visuals().selection.stroke.color), egui::StrokeKind::Inside);
    }
    let c = rect.center();
    let pt = |x: f32, y: f32| pos2(c.x + x, c.y + y);
    let line = |points: Vec<egui::Pos2>| { p.add(egui::Shape::line(points, stroke)); };
    match icon {
        Icon::Reset => { line(vec![pt(-5.0,-3.0),pt(-2.0,-6.0),pt(3.0,-6.0),pt(6.0,-3.0),pt(6.0,2.0),pt(3.0,5.0),pt(-2.0,5.0),pt(-5.0,2.0)]); line(vec![pt(-6.0,-6.0),pt(-5.0,-3.0),pt(-2.0,-3.0)]); }
        Icon::Delete => { line(vec![pt(-6.0,-5.0),pt(6.0,-5.0)]); line(vec![pt(-3.0,-5.0),pt(-2.0,-7.0),pt(2.0,-7.0),pt(3.0,-5.0)]); line(vec![pt(-4.0,-3.0),pt(-3.0,6.0),pt(3.0,6.0),pt(4.0,-3.0)]); }
        Icon::Pin | Icon::Unpin => { line(vec![pt(-3.0,-6.0),pt(3.0,-6.0),pt(2.0,-1.0),pt(5.0,2.0),pt(-5.0,2.0),pt(-2.0,-1.0),pt(-3.0,-6.0)]); line(vec![pt(0.0,2.0),pt(0.0,7.0)]); if matches!(icon, Icon::Unpin) { line(vec![pt(-7.0,-7.0),pt(7.0,7.0)]); } }
    }
    response.on_hover_text(label)
}
