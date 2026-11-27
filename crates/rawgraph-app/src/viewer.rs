//! Image viewer: fit / 100 % / wheel zoom around the cursor / drag to pan.

use eframe::egui::{self, Color32, ColorImage, Pos2, Rect, Sense, TextureHandle, TextureOptions, Vec2};

use crate::render::Frame;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ZoomMode {
    Fit,
    /// Screen points per image pixel.
    Custom(f32),
    /// Magnification relative to the current aspect fit.
    Relative(f32),
}

/// Pure zoom/pan maths, unit-testable without a window.
pub fn fit_scale(avail: Vec2, image: Vec2) -> f32 {
    if image.x <= 0.0 || image.y <= 0.0 {
        return 1.0;
    }
    (avail.x / image.x).min(avail.y / image.y).max(f32::MIN_POSITIVE)
}

/// New pan offset after changing the scale from `old` to `new` such that the
/// image point under `cursor` (relative to the viewport centre) stays put.
pub fn zoom_around(pan: Vec2, cursor_from_centre: Vec2, old: f32, new: f32) -> Vec2 {
    let ratio = new / old;
    cursor_from_centre - (cursor_from_centre - pan) * ratio
}

/// How the viewer showed the picture this frame; the tools paint on top of it.
#[derive(Clone)]
pub struct Geometry {
    /// The whole viewer area.
    pub viewport: Rect,
    /// Where the (oriented, unrotated) picture sits.
    pub image_rect: Rect,
    /// Screen points per picture pixel.
    pub scale: f32,
    pub response: egui::Response,
}

/// Flips, quarter turns and straightening the picture is shown with (crop tool).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Orient {
    pub quarter_turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
    pub angle_deg: f32,
}

pub struct Viewer {
    texture: Option<TextureHandle>,
    /// The picture of a GPU frame (a native egui texture owned by the render worker).
    gpu_tex: Option<egui::TextureId>,
    pub frame_size: [u32; 2],
    pub click_zoom: f32,
    pub wheel_navigates: bool,
    pub navigation_step: i32,
    scroll_accum: f32,
    pub mode: ZoomMode,
    /// Offset of the image centre from the viewport centre, in points.
    pan: Vec2,
    /// Last evaluation error; the last good frame stays visible, dimmed.
    pub error: Option<String>,
    /// Progress note while a proxy decodes (the previous frame stays, dimmed).
    pub busy: Option<String>,
    /// Neutral message: the image is not part of the viewed collection.
    pub excluded: Option<String>,
    /// Zoom in points per image pixel as last painted, for the status bar.
    pub last_scale: f32,
    /// Red mask overlay painted above the image (view only).
    overlay: Option<TextureHandle>,
    /// The viewed node produces a mask: a small "MASK" label is drawn in the corner.
    pub shows_mask: bool,
    /// A tool owns the primary button: the picture pans only with Space+drag or the middle button.
    pub tool_active: bool,
    /// Pinch / Ctrl+scroll belong to the tool (brush size) instead of zooming.
    pub pinch_to_tool: bool,
    /// Show the picture turned and straightened (crop tool).
    pub orient: Option<Orient>,
    /// A note drawn in the bottom-left corner (what the picture shown is while painting).
    pub banner: Option<String>,
}

impl Default for Viewer {
    fn default() -> Self {
        Self {
            texture: None,
            gpu_tex: None,
            frame_size: [0, 0],
            click_zoom: 2.0,
            wheel_navigates: false,
            navigation_step: 0,
            scroll_accum: 0.0,
            mode: ZoomMode::Fit,
            pan: Vec2::ZERO,
            error: None,
            busy: None,
            excluded: None,
            last_scale: 1.0,
            overlay: None,
            shows_mask: false,
            tool_active: false,
            pinch_to_tool: false,
            orient: None,
            banner: None,
        }
    }
}

impl Viewer {
    pub fn with_preferences(click_zoom: f32, wheel_navigates: bool) -> Self {
        Self { click_zoom: if click_zoom == 3.0 { 3.0 } else { 2.0 }, wheel_navigates, ..Self::default() }
    }

    /// The texture that shows the current picture (GPU or CPU frame).
    pub fn texture_id(&self) -> Option<egui::TextureId> {
        self.gpu_tex.or_else(|| self.texture.as_ref().map(|t| t.id()))
    }

    pub fn set_frame(&mut self, ctx: &egui::Context, frame: &Frame) {
        if let Some(pic) = frame.gpu {
            if pic.width == 0 || pic.height == 0 {
                return;
            }
            // The picture stays on the GPU; the CPU texture is not needed any more.
            self.gpu_tex = Some(pic.texture);
            self.texture = None;
            self.frame_size = [pic.width, pic.height];
            self.error = None;
            self.excluded = None;
            self.set_overlay(ctx, None);
            return;
        }
        let size = [frame.width as usize, frame.height as usize];
        if size[0] == 0 || size[1] == 0 || frame.rgba8.len() != size[0] * size[1] * 4 {
            return;
        }
        let image = ColorImage::from_rgba_unmultiplied(size, &frame.rgba8);
        // Pixels are shown crisp when magnified, smooth when reduced.
        let options = TextureOptions {
            magnification: egui::TextureFilter::Nearest,
            minification: egui::TextureFilter::Linear,
            ..Default::default()
        };
        match &mut self.texture {
            Some(t) => t.set(image, options),
            None => self.texture = Some(ctx.load_texture("preview", image, options)),
        }
        self.gpu_tex = None;
        self.frame_size = [frame.width, frame.height];
        self.error = None;
        self.excluded = None;
        self.set_overlay(ctx, frame.overlay.as_ref());
    }

    fn set_overlay(&mut self, ctx: &egui::Context, overlay: Option<&crate::render::Overlay>) {
        let valid = overlay.filter(|o| o.width > 0 && o.height > 0 && o.rgba8.len() == (o.width * o.height * 4) as usize);
        let Some(o) = valid else {
            self.overlay = None;
            return;
        };
        let image = ColorImage::from_rgba_unmultiplied([o.width as usize, o.height as usize], &o.rgba8);
        let options = TextureOptions::LINEAR;
        match &mut self.overlay {
            Some(t) => t.set(image, options),
            None => self.overlay = Some(ctx.load_texture("mask-overlay", image, options)),
        }
    }


    pub fn set_error(&mut self, message: String) {
        self.error = Some(message);
        self.excluded = None;
    }

    pub fn set_excluded(&mut self, message: String) {
        self.excluded = Some(message);
        self.error = None;
    }

    #[cfg(test)]
    pub fn pan_for_tests(&self) -> Vec2 {
        self.pan
    }

    pub fn fit(&mut self) {
        self.mode = ZoomMode::Fit;
        self.pan = Vec2::ZERO;
    }

    /// 100 %: one image pixel per physical screen pixel.
    pub fn actual_size(&mut self, pixels_per_point: f32) {
        self.mode = ZoomMode::Custom(1.0 / pixels_per_point.max(0.1));
        self.pan = Vec2::ZERO;
    }

    pub fn zoom_percent(&self, pixels_per_point: f32) -> f32 {
        self.last_scale * pixels_per_point * 100.0
    }

    pub fn accumulate_navigation(&mut self, scroll: Vec2) {
        let delta = if scroll.x.abs() > scroll.y.abs() { scroll.x } else { scroll.y };
        if self.scroll_accum.signum() != delta.signum() && delta != 0.0 { self.scroll_accum = 0.0; }
        self.scroll_accum += delta;
        if self.scroll_accum.abs() >= 30.0 {
            self.navigation_step = if self.scroll_accum < 0.0 { 1 } else { -1 };
            self.scroll_accum = 0.0;
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<Geometry> {
        let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(14));
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, "Image viewer"));

        if response.hovered() && self.wheel_navigates && !self.tool_active {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            self.accumulate_navigation(scroll);
        }
        if let Some(msg) = &self.excluded {
            paint_notice(&painter, rect, msg, false);
            return None;
        }
        let Some(texture_id) = self.texture_id() else {
            match (&self.error, &self.busy) {
                (Some(err), _) => paint_notice(&painter, rect, err, true),
                (None, Some(msg)) => paint_notice(&painter, rect, msg, false),
                (None, None) => {}
            }
            return None;
        };
        let mut image_size = Vec2::new(self.frame_size[0] as f32, self.frame_size[1] as f32);
        if self.orient.is_some_and(|o| o.quarter_turns % 2 == 1) {
            image_size = Vec2::new(image_size.y, image_size.x);
        }
        let ppp = ui.ctx().pixels_per_point();
        let fit = fit_scale(rect.size(), image_size);
        let mut scale = match self.mode {
            ZoomMode::Fit => fit,
            ZoomMode::Custom(s) => s.max(fit),
            ZoomMode::Relative(m) => fit * m,
        };

        if response.clicked() && !self.tool_active {
            if matches!(self.mode, ZoomMode::Fit) {
                let new = fit * self.click_zoom;
                let anchor = response.interact_pointer_pos().map_or(Vec2::ZERO, |p| p - rect.center());
                self.pan = zoom_around(self.pan, anchor, scale, new);
                self.mode = ZoomMode::Relative(self.click_zoom);
                scale = new;
            } else {
                self.fit();
                scale = fit;
            }
        }

        if response.hovered() {
            let (scroll, pinch, cursor) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.pointer.hover_pos()));
            let scroll = if self.wheel_navigates { 0.0 } else { scroll };
            let pinch = if self.pinch_to_tool { 1.0 } else { pinch };
            let factor = pinch * (scroll * 0.0025).exp();
            if (factor - 1.0).abs() > 1e-4 {
                let new = (scale * factor).clamp(fit, (64.0 / ppp).max(fit));
                let from_centre = cursor.map_or(Vec2::ZERO, |c| c - rect.center());
                self.pan = zoom_around(self.pan, from_centre, scale, new);
                scale = new;
                if new <= fit { self.fit(); } else { self.mode = ZoomMode::Custom(scale); }
            }
        }
        let pan_delta = if self.tool_active {
            // The tool owns the primary button; Space+drag and the middle button still pan.
            let (space, middle, primary, delta) = ui.input(|i| (i.key_down(egui::Key::Space), i.pointer.middle_down(), i.pointer.primary_down(), i.pointer.delta()));
            let typing = ui.ctx().egui_wants_keyboard_input();
            (response.hovered() && (middle || (space && !typing && primary))).then_some(delta)
        } else {
            response.dragged().then(|| response.drag_delta())
        };
        if let Some(delta) = pan_delta {
            self.pan += delta;
            if matches!(self.mode, ZoomMode::Fit) {
                self.mode = ZoomMode::Custom(scale);
            }
        }
        if matches!(self.mode, ZoomMode::Fit) {
            self.pan = Vec2::ZERO;
        }
        self.last_scale = scale;

        let size = image_size * scale;
        let image_rect = Rect::from_center_size(rect.center() + self.pan, size);
        let tint = if self.error.is_some() { Color32::from_gray(70) } else { Color32::WHITE };
        match self.orient {
            Some(o) => paint_oriented(&painter, texture_id, image_rect, o, tint),
            None => {
                painter.image(texture_id, image_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
                if let Some(overlay) = &self.overlay {
                    painter.image(overlay.id(), image_rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
                }
            }
        }
        if self.shows_mask {
            paint_mask_label(&painter, rect);
        } else if self.overlay.is_some() {
            paint_tag(&painter, rect, "MASK OVERLAY", Color32::from_rgb(255, 120, 110));
        }

        if let Some(text) = &self.banner {
            paint_banner(&painter, rect, text);
        }
        if let Some(err) = &self.error {
            paint_notice(&painter, rect, err, true);
        }
        Some(Geometry { viewport: rect, image_rect, scale, response })
    }
}

/// Paints the texture turned by the flips / quarter turns and straightened by the angle.
fn paint_oriented(painter: &egui::Painter, texture: egui::TextureId, image_rect: Rect, o: Orient, tint: Color32) {
    let uv = crate::crop_logic::oriented_uv(o.quarter_turns, o.flip_h, o.flip_v);
    let c = image_rect.center();
    let corners = crate::crop_logic::rotated_corners([c.x, c.y], image_rect.width(), image_rect.height(), o.angle_deg);
    let mut mesh = egui::epaint::Mesh::with_texture(texture);
    for i in 0..4 {
        mesh.vertices.push(egui::epaint::Vertex { pos: Pos2::new(corners[i][0], corners[i][1]), uv: Pos2::new(uv[i][0], uv[i][1]), color: tint });
    }
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(egui::Shape::mesh(mesh));
}

/// A one-line note in the bottom-left corner of the viewer.
fn paint_banner(painter: &egui::Painter, rect: Rect, text: &str) {
    let color = Color32::from_rgb(255, 214, 140);
    let galley = painter.layout(text.to_owned(), egui::FontId::proportional(13.0), color, (rect.width() - 44.0).max(80.0));
    let tag = Rect::from_min_size(Pos2::new(rect.min.x + 10.0, rect.max.y - 10.0 - galley.size().y - 10.0), galley.size() + Vec2::new(16.0, 10.0));
    painter.rect_filled(tag, 4.0, Color32::from_rgba_unmultiplied(12, 14, 18, 225));
    painter.rect_stroke(tag, 4.0, egui::Stroke::new(1.0, color), egui::StrokeKind::Inside);
    painter.galley(tag.min + Vec2::new(8.0, 5.0), galley, color);
}

fn paint_mask_label(painter: &egui::Painter, rect: Rect) {
    paint_tag(painter, rect, "MASK", Color32::from_rgb(240, 165, 62));
}

/// Small tag in the top-left corner of the viewer.
fn paint_tag(painter: &egui::Painter, rect: Rect, text: &str, color: Color32) {
    let galley = painter.layout_no_wrap(text.to_owned(), egui::FontId::proportional(12.0), color);
    let tag = Rect::from_min_size(rect.min + Vec2::new(10.0, 10.0), galley.size() + Vec2::new(14.0, 8.0));
    painter.rect_filled(tag, 4.0, Color32::from_rgba_unmultiplied(12, 14, 18, 215));
    painter.rect_stroke(tag, 4.0, egui::Stroke::new(1.0, color), egui::StrokeKind::Inside);
    painter.galley(tag.min + Vec2::new(7.0, 4.0), galley, color);
}

/// Readable message box in the middle of the viewer (error or progress).
fn paint_notice(painter: &egui::Painter, rect: Rect, text: &str, error: bool) {
    let font = egui::FontId::proportional(17.0);
    let (fg, bg, edge) = if error {
        (Color32::from_rgb(255, 205, 190), Color32::from_rgba_unmultiplied(40, 18, 14, 235), Color32::from_rgb(220, 110, 80))
    } else {
        (Color32::from_rgb(210, 225, 245), Color32::from_rgba_unmultiplied(20, 28, 40, 235), Color32::from_rgb(90, 130, 190))
    };
    let galley = painter.layout(text.to_owned(), font, fg, (rect.width() - 60.0).max(80.0));
    let text_rect = Rect::from_center_size(rect.center(), galley.size()).expand2(Vec2::new(18.0, 12.0));
    painter.rect_filled(text_rect, 6.0, bg);
    painter.rect_stroke(text_rect, 6.0, egui::Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    painter.galley(text_rect.center() - galley.size() / 2.0, galley, Color32::WHITE);
}

/// Fits the texture centred on black, with no chrome at all (clean feed).
pub fn paint_clean(ui: &mut egui::Ui, texture: Option<egui::TextureId>, size: [u32; 2]) {
    let rect = ui.max_rect();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, Color32::BLACK);
    if let Some(t) = texture {
        let image = Vec2::new(size[0] as f32, size[1] as f32);
        let scale = fit_scale(rect.size(), image);
        let r = Rect::from_center_size(rect.center(), image * scale);
        painter.image(t, r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_scale_uses_the_tighter_axis() {
        assert_eq!(fit_scale(Vec2::new(960.0, 1000.0), Vec2::new(1920.0, 1280.0)), 0.5);
        assert!((fit_scale(Vec2::new(2000.0, 640.0), Vec2::new(1920.0, 1280.0)) - 0.5).abs() < 1e-6);
        assert_eq!(fit_scale(Vec2::new(100.0, 100.0), Vec2::ZERO), 1.0);
    }

    #[test]
    fn fit_and_relative_zoom_cover_varied_image_sizes() {
        let viewport = Vec2::new(800.0, 600.0);
        for image in [Vec2::new(12000.0, 8000.0), Vec2::new(100.0, 300.0), Vec2::new(1.0e8, 1.0e8)] {
            let fit = fit_scale(viewport, image);
            let size = image * fit;
            assert!(size.x <= viewport.x + 0.01 && size.y <= viewport.y + 0.01);
            for m in [2.0, 3.0] { assert!(((fit * m) / fit - m).abs() < 1e-5); }
        }
        let v = Viewer::with_preferences(3.0, true);
        assert_eq!(v.click_zoom, 3.0);
        assert!(v.wheel_navigates);
    }

    #[test]
    fn wheel_navigation_accumulates_and_changes_direction() {
        let mut v = Viewer::default();
        v.accumulate_navigation(Vec2::new(0.0, -20.0));
        assert_eq!(v.navigation_step, 0);
        v.accumulate_navigation(Vec2::new(0.0, -11.0));
        assert_eq!(v.navigation_step, 1);
        v.navigation_step = 0;
        v.accumulate_navigation(Vec2::new(-20.0, 0.0));
        v.accumulate_navigation(Vec2::new(31.0, 0.0));
        assert_eq!(v.navigation_step, -1);
    }

    #[test]
    fn zoom_keeps_the_point_under_the_cursor() {
        let (pan, cursor, old, new) = (Vec2::new(30.0, -10.0), Vec2::new(120.0, 80.0), 0.5, 2.0);
        // Image-space point under the cursor before and after.
        let before = (cursor - pan) / old;
        let new_pan = zoom_around(pan, cursor, old, new);
        let after = (cursor - new_pan) / new;
        assert!((before - after).length() < 1e-4);
    }
}
