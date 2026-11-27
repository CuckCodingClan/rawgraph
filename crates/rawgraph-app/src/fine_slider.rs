//! Fine slider: an explicit click-and-drag target, independent of the value editor.
use eframe::egui::{self, Pos2, Sense, Stroke, Vec2};

// Scope the gesture to one control: egui's pointer click count is shared
// across widgets, so a click on another control can continue that sequence.
fn double_click(ui: &egui::Ui, response: &egui::Response) -> bool {
    if !response.clicked() { return false; }
    let Some(pos) = response.interact_pointer_pos() else { return false; };
    let time = ui.input(|i| i.time);
    let id = response.id.with("reset-click");
    ui.data_mut(|data| {
        let previous = data.get_temp::<(f64, Pos2)>(id);
        let reset = previous.is_some_and(|(t, p)| time - t <= 0.35 && p.distance(pos) <= 6.0);
        if reset { data.remove::<(f64, Pos2)>(id); } else { data.insert_temp(id, (time, pos)); }
        reset
    })
}

fn position(value: f32, min: f32, max: f32, log: bool) -> f32 {
    let v = value.clamp(min, max);
    if log && min > 0.0 { (v / min).ln() / (max / min).ln() }
    else if log && min == 0.0 { (v / max).cbrt() }
    else { (v - min) / (max - min) }
}

fn value_at(t: f32, min: f32, max: f32, log: bool) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if log && min > 0.0 { min * (max / min).powf(t) }
    else if log && min == 0.0 { max * t.powi(3) }
    else { min + t * (max - min) }
}

/// Draw a slider and an independently editable numeric field. The caller draws the label.
pub fn show(ui: &mut egui::Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>, default: f32, log: bool, label: &egui::Response, width: f32, decimals: usize) -> egui::Response {
    ui.horizontal(|ui| {
    let (min, max) = (*range.start(), *range.end());
    let original = *value;
    let mut track_response = ui.allocate_response(Vec2::new(width.max(20.0), ui.spacing().interact_size.y), Sense::click_and_drag());
    ui.ctx().data_mut(|d| d.insert_temp(track_response.id.with("fine-slider"), true));
    let rect = track_response.rect;
    let left = rect.left() + 6.0;
    let right = rect.right() - 6.0;
    let reset_track = double_click(ui, &track_response);
    let reset_label = double_click(ui, label);
    if reset_track || reset_label {
        *value = default.clamp(min, max);
    } else if track_response.clicked() || track_response.dragged() {
        if let Some(p) = track_response.interact_pointer_pos() {
            *value = value_at((p.x - left) / (right - left), min, max, log);
        }
    }
    if track_response.has_focus() {
        // Arrow keys adjust this slider rather than navigating to another widget.
        ui.memory_mut(|m| m.set_focus_lock_filter(track_response.id, egui::EventFilter {
            horizontal_arrows: true,
            vertical_arrows: true,
            ..Default::default()
        }));
        let delta = ui.input(|i| {
            (i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::ArrowUp)) as i32
                - (i.key_pressed(egui::Key::ArrowLeft) || i.key_pressed(egui::Key::ArrowDown)) as i32
        });
        if delta != 0 { *value = value_at(position(*value, min, max, log) + delta as f32 * 0.01, min, max, log); }
        if ui.input(|i| i.key_pressed(egui::Key::Home)) { *value = min; }
        if ui.input(|i| i.key_pressed(egui::Key::End)) { *value = max; }
    }
    let y = rect.center().y;
    let x = left + (right - left) * position(*value, min, max, log);
    let base = left + (right - left) * position(default, min, max, log);
    let painter = ui.painter();
    if track_response.has_focus() {
        painter.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(2.0, ui.visuals().selection.stroke.color), egui::StrokeKind::Inside);
    }
    painter.line_segment([Pos2::new(left, y), Pos2::new(right, y)], Stroke::new(4.0, ui.visuals().widgets.inactive.bg_fill));
    painter.line_segment([Pos2::new(base, y), Pos2::new(x, y)], Stroke::new(4.0, ui.visuals().selection.bg_fill));
    painter.circle_filled(Pos2::new(x, y), 6.0, ui.visuals().widgets.active.fg_stroke.color);
    track_response.widget_info(|| egui::WidgetInfo::slider(ui.is_enabled(), *value as f64, ""));
    track_response = track_response.labelled_by(label.id);
    let edit = ui.add(egui::DragValue::new(value).range(min..=max).speed((max - min) as f64 / 500.0).max_decimals(decimals)).labelled_by(label.id);
    if *value != original { track_response.mark_changed(); }
    track_response.on_hover_text("Drag or use arrow keys; double-click to reset") | edit
    }).inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Modifiers, PointerButton, Rect};
    struct Harness { ctx: egui::Context, value: f32, t: f64, track: Rect, label: Rect, id: egui::Id }
    impl Harness {
        fn new() -> Self { let mut h = Self { ctx: egui::Context::default(), value: 0.5, t: 0.0, track: Rect::NOTHING, label: Rect::NOTHING, id: egui::Id::NULL }; h.frame(vec![]); h.frame(vec![]); h }
        fn frame(&mut self, events: Vec<Event>) {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0))), time: Some(self.t), events, ..Default::default() };
            self.t += 0.04;
            let out = self.ctx.run_ui(input, |ui| { egui::CentralPanel::default().show(ui, |ui| { ui.horizontal(|ui| {
                let label = ui.add(egui::Label::new("Test").sense(Sense::click())); self.label = label.rect;
                let response = show(ui, &mut self.value, 0.0..=1.0, 0.25, false, &label, 120.0, 3); self.track = response.rect; self.id = response.id;
            }); }); });
            out.drop_without_applying_deltas();
        }
        fn click(&mut self, p: Pos2) { self.frame(vec![Event::PointerMoved(p)]); for pressed in [true, false] { self.frame(vec![Event::PointerButton { pos: p, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]); } }
    }
    #[test]
    fn track_and_label_double_click_reset() {
        let mut h = Harness::new(); let p = h.track.left_center() + Vec2::new(75.0, 0.0);
        h.click(p); h.click(p); assert!((h.value - 0.25).abs() < 0.001);
        h.value = 0.8; let p = h.label.center(); h.click(p); h.click(p); assert!((h.value - 0.25).abs() < 0.001);
    }
    #[test]
    fn click_drag_and_keyboard_change_value() {
        let mut h = Harness::new(); let p = h.track.left_center() + Vec2::new(20.0, 0.0);
        h.click(p); assert!(h.value < 0.3);
        h.frame(vec![Event::PointerMoved(p), Event::PointerButton { pos: p, button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }]);
        let q = p + Vec2::new(60.0, 0.0); h.frame(vec![Event::PointerMoved(q)]); assert!(h.value > 0.5);
        h.frame(vec![Event::PointerButton { pos: q, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
        h.ctx.memory_mut(|m| m.request_focus(h.id));
        let before = h.value;
        h.frame(vec![Event::Key { key: egui::Key::ArrowRight, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
        assert!(h.value > before);
    }
    #[test]
    fn focused_slider_keeps_arrows_from_gallery() {
        let mut h = Harness::new();
        h.ctx.memory_mut(|m| m.request_focus(h.id));
        h.frame(vec![]); // Register the focused widget's arrow-key filter.
        assert!(!crate::filmstrip::gallery_keyboard_available(&h.ctx));
        let before = h.value;
        h.frame(vec![Event::Key { key: egui::Key::ArrowRight, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
        assert!(h.value > before);
        h.ctx.memory_mut(|m| m.surrender_focus(h.id));
        assert!(crate::filmstrip::gallery_keyboard_available(&h.ctx));
    }
    #[test]
    fn log_mapping_and_default_are_consistent() {
        assert_eq!(value_at(0.0, 0.0, 100.0, true), 0.0);
        assert_eq!(value_at(1.0, 0.0, 100.0, true), 100.0);
        assert!((value_at(0.1, 0.0, 100.0, true) - 0.1).abs() < 0.0001);
        assert!((position(10.0, 1.0, 100.0, true) - 0.5).abs() < 0.0001);
        assert!((value_at(0.5, 1.0, 100.0, true) - 10.0).abs() < 0.0001);
    }
}
