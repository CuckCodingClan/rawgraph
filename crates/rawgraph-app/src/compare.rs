//! Side-by-side compare view: the state machine (which image is the reference,
//! when compare is on) and the painting of the two halves with a shared zoom
//! and pan. The right half is the selected image; culling keys and Left/Right
//! act on it.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Vec2};
use rawgraph_core::{ImageId, graph::NodeId};
use rawgraph_project::{Flag, ImageEntry};

use crate::viewer::{fit_scale, zoom_around, ZoomMode};

/// A pane independently follows or pins each selection dimension.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pane {
    pub image: Option<ImageId>,
    pub node: Option<NodeId>,
}
impl Pane {
    pub fn target(&self, image: Option<ImageId>, node: Option<NodeId>) -> (Option<ImageId>, Option<NodeId>) {
        (self.image.or(image), self.node.or(node))
    }
    pub fn toggle_image(&mut self, selected: Option<ImageId>) {
        self.image = if self.image.is_some() { None } else { selected };
    }
    pub fn toggle_node(&mut self, selected: Option<NodeId>) {
        self.node = if self.node.is_some() { None } else { selected };
    }
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compare {
    active: bool,
    pub panes: [Pane; 2],
}
impl Compare {
    pub fn active(&self) -> bool { self.active }
    pub fn toggle(&mut self) { self.active = !self.active; }
    pub fn leave(&mut self) { self.active = false; }
    pub fn set_reference(&mut self, id: ImageId) { self.panes[0].image = Some(id); }
    pub fn forget(&mut self) { *self = Self::default(); }
}

/// Zoom and pan shared by both halves.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SharedView {
    pub mode: ZoomMode,
    pub pan: Vec2,
}

impl Default for SharedView {
    fn default() -> Self {
        Self { mode: ZoomMode::Fit, pan: Vec2::ZERO }
    }
}

/// What one half shows.
pub struct Half<'a> {
    pub texture: Option<egui::TextureId>,
    pub size: [u32; 2],
    pub uv: Option<[Pos2; 4]>,
    pub badge: Option<&'static str>,
    pub label: String,
    pub entry: Option<&'a ImageEntry>,
    /// Message instead of the picture (error, still loading).
    pub note: Option<String>,
    pub tag: &'static str,
}

/// Splits `rect` into the left and right half around a 2 point gap.
pub fn halves(rect: Rect) -> (Rect, Rect) {
    let mid = rect.center().x;
    (Rect::from_min_max(rect.min, Pos2::new(mid - 1.0, rect.max.y)), Rect::from_min_max(Pos2::new(mid + 1.0, rect.min.y), rect.max))
}

/// The scale both halves use: the common fit while in Fit mode.
pub fn shared_scale(mode: ZoomMode, avail: Vec2, sizes: [Option<Vec2>; 2]) -> f32 {
    match mode {
        ZoomMode::Custom(s) => s.max(shared_scale(ZoomMode::Fit, avail, sizes)),
        ZoomMode::Relative(factor) => shared_scale(ZoomMode::Fit, avail, sizes) * factor,
        ZoomMode::Fit => sizes.into_iter().flatten().map(|s| fit_scale(avail, s)).fold(f32::INFINITY, f32::min).clamp(1e-4, 1e6),
    }
}

/// Paints both halves; handles wheel zoom, drag pan and double-click for the pair.
pub fn show(ui: &mut egui::Ui, view: &mut SharedView, viewer: &mut crate::viewer::Viewer, left: &Half<'_>, right: &Half<'_>) -> f32 {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, "Compare view"));
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::from_gray(14));
    let (lr, rr) = halves(rect);
    let size_of = |h: &Half<'_>| h.texture.map(|_| Vec2::new(h.size[0] as f32, h.size[1] as f32)).filter(|s| s.x > 0.0 && s.y > 0.0);
    let sizes = [size_of(left), size_of(right)];
    let ppp = ui.ctx().pixels_per_point();
    let fit = shared_scale(ZoomMode::Fit, lr.size(), sizes);
    let mut scale = shared_scale(view.mode, lr.size(), sizes);

    if response.clicked() {
        if matches!(view.mode, ZoomMode::Fit) {
            view.mode = ZoomMode::Relative(viewer.click_zoom);
            scale = fit * viewer.click_zoom;
        } else {
            view.mode = ZoomMode::Fit;
            scale = fit;
        }
        view.pan = Vec2::ZERO;
    }
    if response.hovered() {
        let (scroll, pinch, cursor) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.pointer.hover_pos()));
        if viewer.wheel_navigates { viewer.accumulate_navigation(scroll); }
        let factor = pinch * (if viewer.wheel_navigates { 0.0 } else { scroll.y } * 0.0025).exp();
        if (factor - 1.0).abs() > 1e-4 {
            let new = (scale * factor).clamp(fit, (64.0 / ppp).max(fit));
            // Zoom around the cursor measured from the centre of the half it is over.
            let centre = cursor.map_or(lr.center(), |c| if c.x < rect.center().x { lr.center() } else { rr.center() });
            let from_centre = cursor.map_or(Vec2::ZERO, |c| c - centre);
            view.pan = zoom_around(view.pan, from_centre, scale, new);
            scale = new;
            view.mode = if new <= fit { ZoomMode::Fit } else { ZoomMode::Custom(scale) };
        }
    }
    if response.dragged() {
        view.pan += response.drag_delta();
        if matches!(view.mode, ZoomMode::Fit) {
            view.mode = ZoomMode::Custom(scale);
        }
    }
    if matches!(view.mode, ZoomMode::Fit) {
        view.pan = Vec2::ZERO;
    }

    for (half, area) in [(left, lr), (right, rr)] {
        let p = ui.painter_at(area);
        match (half.texture, size_of(half)) {
            (Some(t), Some(sz)) => {
                let img = Rect::from_center_size(area.center() + view.pan, sz * scale);
                if let Some(uv) = half.uv { crate::quick_preview::paint(&p, t, img, uv, Color32::WHITE); }
                else { p.image(t, img, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE); }
                if let Some(note) = &half.note {
                    let pos = area.right_bottom() - Vec2::new(10.0, 10.0);
                    p.text(pos, egui::Align2::RIGHT_BOTTOM, note, egui::FontId::proportional(12.0), Color32::WHITE);
                }
            }
            _ if half.note.is_some() || half.badge.is_none() => {
                let text = half.note.clone().unwrap_or_else(|| "Rendering...".to_owned());
                p.text(area.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(15.0), Color32::from_gray(170));
            }
            _ => {}
        }
        paint_label(&p, area, half);
        if let Some(badge) = half.badge { crate::viewer::paint_progress(&p, area, badge); }
    }
    painter.rect_filled(Rect::from_min_max(Pos2::new(rect.center().x - 1.0, rect.min.y), Pos2::new(rect.center().x + 1.0, rect.max.y)), 0.0, Color32::from_gray(60));
    scale
}

/// The thin label at the top of a half: tag, file name, stars, flag.
fn paint_label(p: &egui::Painter, area: Rect, half: &Half<'_>) {
    let h = 24.0;
    let bar = Rect::from_min_size(area.min + Vec2::new(8.0, 8.0), Vec2::new((area.width() - 16.0).max(40.0), h));
    p.rect_filled(bar, 4.0, Color32::from_rgba_unmultiplied(12, 14, 18, 215));
    let tag_color = if half.tag == "REFERENCE" { Color32::from_rgb(150, 205, 255) } else { Color32::from_rgb(255, 210, 90) };
    let tag = p.text(bar.left_center() + Vec2::new(8.0, 0.0), egui::Align2::LEFT_CENTER, half.tag, egui::FontId::proportional(11.0), tag_color);
    let name = p.text(Pos2::new(tag.right() + 10.0, bar.center().y), egui::Align2::LEFT_CENTER, &half.label, egui::FontId::proportional(12.5), Color32::from_gray(232));
    let mut x = name.right() + 12.0;
    if let Some(e) = half.entry {
        for i in 0..5u8 {
            crate::filmstrip::draw_star(p, Pos2::new(x + 6.0, bar.center().y), 5.5, i < e.rating);
            x += 13.0;
        }
        let (text, color) = match e.flag {
            Flag::Pick => ("PICK", Color32::from_rgb(110, 230, 120)),
            Flag::Reject => ("REJECT", Color32::from_rgb(240, 100, 90)),
            Flag::None => ("", Color32::GRAY),
        };
        if !text.is_empty() {
            p.text(Pos2::new(x + 6.0, bar.center().y), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(11.5), color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_pins_follow_and_unpin() {
        let mut c = Compare::default();
        c.toggle();
        let (a, b) = (ImageId(1), ImageId(2));
        let (x, y) = (NodeId(1), NodeId(2));
        c.panes[0].toggle_image(Some(a));
        c.panes[1].toggle_node(Some(x));
        assert_eq!(c.panes[0].target(Some(b), Some(y)), (Some(a), Some(y)));
        assert_eq!(c.panes[1].target(Some(b), Some(y)), (Some(b), Some(x)));
        c.panes[0].toggle_image(Some(b));
        c.panes[1].toggle_node(Some(y));
        assert_eq!(c.panes[0].target(Some(b), Some(y)), (Some(b), Some(y)));
        assert_eq!(c.panes[1].target(Some(b), Some(y)), (Some(b), Some(y)));
        c.toggle();
        assert!(!c.active());
        c.toggle();
        assert!(c.active());
        c.forget();
        assert_eq!(c, Compare::default());
    }
    #[test]
    fn absent_selection_does_not_pin_and_deleted_ids_stay_explicit() {
        let mut p = Pane::default();
        p.toggle_image(None);
        p.toggle_node(None);
        assert_eq!(p.target(None, None), (None, None));
        p.image = Some(ImageId(9));
        p.node = Some(NodeId(9));
        assert_eq!(p.target(None, None), (Some(ImageId(9)), Some(NodeId(9))));
    }

    #[test]
    fn both_halves_share_one_scale() {
        let avail = Vec2::new(800.0, 600.0);
        let wide = Some(Vec2::new(1600.0, 1000.0));
        let tall = Some(Vec2::new(1000.0, 1600.0));
        assert!((shared_scale(ZoomMode::Fit, avail, [wide, wide]) - 0.5).abs() < 1e-6);
        assert!((shared_scale(ZoomMode::Fit, avail, [wide, tall]) - 0.375).abs() < 1e-6, "the portrait image decides: both stay fully visible");
        assert!((shared_scale(ZoomMode::Fit, avail, [None, wide]) - 0.5).abs() < 1e-6);
        assert_eq!(shared_scale(ZoomMode::Custom(2.0), avail, [wide, tall]), 2.0);
        assert!(shared_scale(ZoomMode::Fit, avail, [None, None]) > 0.0);
        let (l, r) = halves(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0)));
        assert!(l.max.x < r.min.x && (l.width() - r.width()).abs() < 1e-3);
    }
}
