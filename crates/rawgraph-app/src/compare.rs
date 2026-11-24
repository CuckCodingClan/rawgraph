//! Side-by-side compare view: the state machine (which image is the reference,
//! when compare is on) and the painting of the two halves with a shared zoom
//! and pan. The right half is the selected image; culling keys and Left/Right
//! act on it.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Vec2};
use rawgraph_core::ImageId;
use rawgraph_project::{Flag, ImageEntry};

use crate::viewer::{fit_scale, zoom_around, ZoomMode};

/// Compare state, independent of any UI.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compare {
    active: bool,
    /// Chosen with "Set as reference"; kept until another image is chosen.
    explicit: Option<ImageId>,
    /// The reference of the running compare (always valid while active).
    reference: Option<ImageId>,
    current: Option<ImageId>,
    /// The image that was selected before `current`.
    previous: Option<ImageId>,
}

impl Compare {
    pub fn active(&self) -> bool {
        self.active
    }

    pub fn reference(&self) -> Option<ImageId> {
        self.active.then_some(self.reference).flatten()
    }

    /// Call every frame with the selected image: remembers the one before it.
    pub fn note_selection(&mut self, selected: Option<ImageId>) {
        if selected != self.current {
            if self.current.is_some() {
                self.previous = self.current;
            }
            self.current = selected;
        }
        if self.active {
            self.fix_reference(selected, &[]);
        }
    }

    /// Image to compare with by default: the explicit choice, else the image
    /// selected before, else the neighbour in the list.
    fn pick_reference(&self, selected: ImageId, visible: &[ImageId]) -> Option<ImageId> {
        let ok = |id: &ImageId| *id != selected;
        self.explicit
            .filter(ok)
            .or(self.previous.filter(ok))
            .or_else(|| {
                let i = visible.iter().position(|x| *x == selected)?;
                visible.get(i.wrapping_sub(1)).or_else(|| visible.get(i + 1)).copied()
            })
            .or_else(|| visible.iter().copied().find(ok))
    }

    /// Turns compare on. Returns false when there is nothing to compare with.
    pub fn enter(&mut self, selected: Option<ImageId>, visible: &[ImageId]) -> bool {
        let Some(sel) = selected else { return false };
        self.current = Some(sel);
        match self.pick_reference(sel, visible) {
            Some(r) => {
                self.reference = Some(r);
                self.active = true;
                true
            }
            None => false,
        }
    }

    pub fn leave(&mut self) {
        self.active = false;
    }

    pub fn toggle(&mut self, selected: Option<ImageId>, visible: &[ImageId]) -> bool {
        if self.active {
            self.leave();
            true
        } else {
            self.enter(selected, visible)
        }
    }

    /// "Set as reference": used now when compare is on, and by the next compare otherwise.
    pub fn set_reference(&mut self, id: ImageId) {
        self.explicit = Some(id);
        if self.active {
            self.reference = Some(id);
        }
    }

    /// After the selection moved onto the reference, the previous image takes its place.
    fn fix_reference(&mut self, selected: Option<ImageId>, visible: &[ImageId]) {
        let Some(sel) = selected else { return };
        if self.reference == Some(sel) || self.reference.is_none() {
            // Stepping onto the reference: keep both halves different.
            let prev = self.previous.filter(|p| *p != sel);
            self.reference = prev.or_else(|| self.pick_reference(sel, visible));
            if self.reference.is_none() {
                self.active = false;
            }
        }
    }

    pub fn forget(&mut self) {
        *self = Self::default();
    }
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
        ZoomMode::Custom(s) => s,
        ZoomMode::Fit => sizes.into_iter().flatten().map(|s| fit_scale(avail, s)).fold(f32::INFINITY, f32::min).clamp(1e-4, 1e6),
    }
}

/// Paints both halves; handles wheel zoom, drag pan and double-click for the pair.
pub fn show(ui: &mut egui::Ui, view: &mut SharedView, left: &Half<'_>, right: &Half<'_>) -> f32 {
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

    if response.double_clicked() {
        if matches!(view.mode, ZoomMode::Fit) {
            view.mode = ZoomMode::Custom(1.0 / ppp);
            scale = 1.0 / ppp;
        } else {
            view.mode = ZoomMode::Fit;
            scale = fit;
        }
        view.pan = Vec2::ZERO;
    }
    if response.hovered() {
        let (scroll, pinch, cursor) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
        let factor = pinch * (scroll * 0.0025).exp();
        if (factor - 1.0).abs() > 1e-4 {
            let new = (scale * factor).clamp(fit.min(0.05) * 0.5, 64.0 / ppp);
            // Zoom around the cursor measured from the centre of the half it is over.
            let centre = cursor.map_or(lr.center(), |c| if c.x < rect.center().x { lr.center() } else { rr.center() });
            let from_centre = cursor.map_or(Vec2::ZERO, |c| c - centre);
            view.pan = zoom_around(view.pan, from_centre, scale, new);
            scale = new;
            view.mode = ZoomMode::Custom(scale);
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
                let tint = if half.note.is_some() { Color32::from_gray(70) } else { Color32::WHITE };
                p.image(t, img, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
            }
            _ => {
                let text = half.note.clone().unwrap_or_else(|| "Rendering...".to_owned());
                p.text(area.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(15.0), Color32::from_gray(170));
            }
        }
        paint_label(&p, area, half);
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

    const A: ImageId = ImageId(1);
    const B: ImageId = ImageId(2);
    const C: ImageId = ImageId(3);

    fn list() -> Vec<ImageId> {
        vec![A, B, C]
    }

    #[test]
    fn the_default_reference_is_the_image_selected_before() {
        let mut c = Compare::default();
        c.note_selection(Some(A));
        c.note_selection(Some(C));
        assert!(c.enter(Some(C), &list()));
        assert_eq!((c.active(), c.reference()), (true, Some(A)));
        c.leave();
        assert_eq!(c.reference(), None, "no reference while compare is off");
    }

    #[test]
    fn without_history_the_neighbour_is_used_and_a_lone_image_cannot_compare() {
        let mut c = Compare::default();
        assert!(c.enter(Some(B), &list()));
        assert_eq!(c.reference(), Some(A), "previous in the list");
        let mut d = Compare::default();
        assert!(d.enter(Some(A), &list()));
        assert_eq!(d.reference(), Some(B), "first image: the next one");
        let mut e = Compare::default();
        assert!(!e.enter(Some(A), &[A]) && !e.active(), "nothing to compare with");
        assert!(!e.enter(None, &list()));
    }

    #[test]
    fn set_as_reference_is_used_now_and_remembered() {
        let mut c = Compare::default();
        c.note_selection(Some(A));
        c.note_selection(Some(B));
        c.set_reference(C);
        assert!(!c.active());
        assert!(c.enter(Some(B), &list()));
        assert_eq!(c.reference(), Some(C), "the explicit choice beats the previous selection");
        c.set_reference(A);
        assert_eq!(c.reference(), Some(A), "changes at once while compare is on");
        // An explicit reference equal to the selected image falls back.
        c.leave();
        assert!(c.enter(Some(A), &list()));
        assert_ne!(c.reference(), Some(A));
    }

    #[test]
    fn stepping_moves_the_right_side_and_never_shows_one_image_twice() {
        let mut c = Compare::default();
        c.note_selection(Some(A));
        assert!(c.enter(Some(A), &list()));
        assert_eq!(c.reference(), Some(B));
        // Right arrow: the selection moves to B, which is the reference.
        c.note_selection(Some(B));
        assert_eq!(c.reference(), Some(A), "the image that was on the right becomes the reference");
        // Further stepping leaves the reference alone.
        c.note_selection(Some(C));
        assert_eq!(c.reference(), Some(A));
        assert!(c.active());
    }

    #[test]
    fn toggle_enters_and_leaves() {
        let mut c = Compare::default();
        assert!(c.toggle(Some(B), &list()) && c.active());
        assert!(c.toggle(Some(B), &list()) && !c.active());
        c.forget();
        assert_eq!(c, Compare::default());
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
