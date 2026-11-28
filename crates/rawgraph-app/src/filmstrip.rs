//! Filmstrip: filter/sort header and a virtualised image list. Only the rows
//! that are on screen are laid out; thumbnails load lazily in the background.

use std::ops::Range;

use eframe::egui::{self, Align2, Color32, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use rawgraph_io::SourceKind;
use rawgraph_project::{ColorLabel, Flag, ImageEntry};

use crate::filter::{sort_label, SORT_KEYS};
use crate::inspector::capture_summary;
use crate::session::{Session, ViewerTarget};
use crate::testimages::display_name;
use crate::textfit::{fit_start, text_width};
use crate::thumbs::{ThumbLoader, Wanted};

const THUMB_H: f32 = 92.0;
const ROW_H: f32 = THUMB_H + 4.0 + 2.0 * 17.0 + 8.0;
/// Rows loaded beyond the visible ones, in each direction.
const MARGIN_ROWS: usize = 6;

pub fn label_color(l: ColorLabel) -> Color32 {
    match l {
        ColorLabel::Red => Color32::from_rgb(224, 64, 64),
        ColorLabel::Yellow => Color32::from_rgb(232, 202, 56),
        ColorLabel::Green => Color32::from_rgb(70, 190, 92),
        ColorLabel::Blue => Color32::from_rgb(74, 134, 232),
        ColorLabel::Purple => Color32::from_rgb(172, 94, 212),
    }
}

/// Things the user asked for in the filmstrip that the app carries out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilmAction {
    SetReference(rawgraph_core::ImageId),
    Locate(rawgraph_core::ImageId),
    RelinkFolder(rawgraph_core::ImageId),
    XmpImport,
    XmpDismiss,
}

pub struct Filmstrip {
    pub thumbs: ThumbLoader,
    pub(crate) live: crate::live_thumbs::LiveThumbs,
    live_wanted: Vec<rawgraph_core::ImageId>,
    live_visible: usize,
    last_selected: Option<rawgraph_core::ImageId>,
    last_range: Range<usize>,
    scan_wanted: Vec<rawgraph_core::ImageId>,
    scan_hash_pending: bool,
    /// Filled while drawing; the app drains it every frame.
    pub actions: Vec<FilmAction>,
    /// "n images have ratings in XMP" banner above the list (set by the app).
    pub xmp_banner: Option<usize>,
}

pub(crate) fn gallery_keyboard_available(ctx: &egui::Context) -> bool {
    !ctx.egui_wants_keyboard_input() && !ctx.memory(|m| m.focused()).is_some_and(|id|
        ctx.data(|d| d.get_temp::<bool>(id.with("fine-slider")).unwrap_or(false)))
}

impl Filmstrip {
    pub fn with_thumbs(thumbs: ThumbLoader, live: crate::live_thumbs::LiveThumbs) -> Self {
        Self { thumbs, live, live_wanted: Vec::new(), live_visible: 0, last_selected: None, last_range: 0..0, scan_wanted: Vec::new(), scan_hash_pending: false, actions: Vec::new(), xmp_banner: None }
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        self.thumbs.poll(ctx);
        self.live.poll(ctx);
    }

    /// True when the thumbnails that are visible or about to be are in memory.
    pub fn all_loaded(&self) -> bool {
        self.thumbs.wanted_loaded()
    }

    pub fn scan_wanted(&self) -> &[rawgraph_core::ImageId] { &self.scan_wanted }

    pub fn scan_thumbs_ready(&self) -> bool { !self.scan_hash_pending && self.all_loaded() }

    /// Header controls. Returns true if the filter or sort changed.
    fn header(ui: &mut egui::Ui, session: &mut Session) -> bool {
        let before = session.filter.clone();
        let f = &mut session.filter;
        ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
        ui.add(egui::TextEdit::singleline(&mut f.text).hint_text("Filter by name").desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("flt-rating")
                .width(62.0)
                .selected_text(if f.min_rating == 0 { "Any *".to_owned() } else { format!("{}+ *", f.min_rating) })
                .show_ui(ui, |ui| {
                    for r in 0..=5u8 {
                        ui.selectable_value(&mut f.min_rating, r, if r == 0 { "Any rating".to_owned() } else { format!("{r}+ stars") });
                    }
                });
            egui::ComboBox::from_id_salt("flt-flag")
                .width(70.0)
                .selected_text(match f.flag {
                    None => "Any flag",
                    Some(Flag::Pick) => "Picked",
                    Some(Flag::Reject) => "Rejected",
                    Some(Flag::None) => "Unflagged",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut f.flag, None, "Any flag");
                    ui.selectable_value(&mut f.flag, Some(Flag::Pick), "Picked");
                    ui.selectable_value(&mut f.flag, Some(Flag::Reject), "Rejected");
                    ui.selectable_value(&mut f.flag, Some(Flag::None), "Unflagged");
                });
        });
        ui.horizontal(|ui| {
            ui.label("ISO");
            ui.add(egui::TextEdit::singleline(&mut f.iso_min).hint_text("min").desired_width(46.0));
            ui.add(egui::TextEdit::singleline(&mut f.iso_max).hint_text("max").desired_width(46.0));
            egui::ComboBox::from_id_salt("flt-flash")
                .width(66.0)
                .selected_text(match f.flash {
                    None => "Flash: any",
                    Some(true) => "Flash: yes",
                    Some(false) => "Flash: no",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut f.flash, None, "Any");
                    ui.selectable_value(&mut f.flash, Some(true), "Flash fired");
                    ui.selectable_value(&mut f.flash, Some(false), "No flash");
                });
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("flt-sort").width(66.0).selected_text(format!("Sort: {}", sort_label(f.sort))).show_ui(ui, |ui| {
                for k in SORT_KEYS {
                    ui.selectable_value(&mut f.sort, k, sort_label(k));
                }
            });
            if ui.button(if f.descending { "Desc" } else { "Asc" }).on_hover_text("Sort direction").clicked() {
                f.descending = !f.descending;
            }
            if ui.add_enabled(f.is_active(), egui::Button::new("Clear")).clicked() {
                let (sort, desc) = (f.sort, f.descending);
                *f = crate::filter::FilterUi { sort, descending: desc, ..Default::default() };
            }
        });
        session.filter != before
    }

    pub fn show(&mut self, ui: &mut egui::Ui, session: &mut Session, mut target: ViewerTarget, members: Option<&std::collections::HashSet<rawgraph_core::ImageId>>) {
        self.scan_wanted.clear();
        self.scan_hash_pending = false;
        if Self::header(ui, session) {
            session.refresh_visible();
        }
        let ids = members.map_or_else(Vec::new, |members| session.gallery_images(members));
        if members.is_some() && !target.image_pinned && target.image.is_none_or(|id| !ids.contains(&id)) {
            session.selected = ids.first().copied();
            target.image = session.selected;
        }
        ui.label(RichText::new(format!("{} images", ids.len())).strong());
        if target.node.is_none() {
            ui.label("Select a node to see its images.");
            self.thumbs.set_wanted(&[]);
            self.live_wanted.clear();
            return;
        }
        if members.is_none() {
            ui.label("Loading node collection…");
            self.thumbs.set_wanted(&[]);
            self.live_wanted.clear();
            return;
        }
        if ids.is_empty() {
            ui.label("No images in this node match the filters.");
            self.thumbs.set_wanted(&[]);
            self.live_wanted.clear();
            return;
        }
        if !target.image_pinned && gallery_keyboard_available(ui.ctx()) {
            let (left, right) = ui.ctx().input_mut(|i| (i.consume_key(egui::Modifiers::NONE, Key::ArrowLeft), i.consume_key(egui::Modifiers::NONE, Key::ArrowRight)));
            if left || right {
                session.step_gallery(target, &ids, if left { -1 } else { 1 });
                target.image = session.selected;
            }
        }
        if let Some(n) = self.xmp_banner {
            egui::Frame::group(ui.style()).fill(Color32::from_rgb(40, 52, 70)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(format!("{n} image{} {} ratings in XMP", if n == 1 { "" } else { "s" }, if n == 1 { "has" } else { "have" })).size(12.5));
                ui.horizontal(|ui| {
                    if ui.button("Import").on_hover_text("Reads the ratings from the XMP sidecars and the files; nothing is written to them").clicked() {
                        self.actions.push(FilmAction::XmpImport);
                    }
                    if ui.button("Dismiss").clicked() {
                        self.actions.push(FilmAction::XmpDismiss);
                    }
                });
            });
        }
        ui.add_space(2.0);

        let available = (ui.available_width() - 6.0).max(100.0);
        let columns = ((available + 4.0) / 124.0).floor().max(1.0) as usize;
        let width = (available - (columns - 1) as f32 * 4.0) / columns as f32;
        let rows = ids.len().div_ceil(columns);
        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if target.image != self.last_selected {
            if let Some(i) = target.image.and_then(|id| ids.iter().position(|x| *x == id)) {
                let row = i / columns;
                if row < self.last_range.start || row >= self.last_range.end {
                    scroll = scroll.vertical_scroll_offset((row as f32 * ROW_H - ROW_H).max(0.0));
                }
            }
            self.last_selected = target.image;
        }
        let mut clicked = None;
        let out = scroll.show_rows(ui, ROW_H, rows, |ui, range| {
            for row in range.clone() {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for &id in ids.iter().skip(row * columns).take(columns) {
                        if let Some(entry) = session.entry(id).cloned() {
                            let tex = self.live.texture(id).or_else(|| self.thumbs.texture(id));
                            let resp = draw_row(ui, width, &entry, target.image == Some(id), tex.as_ref(), self.thumbs.has_failed(id), false);
                            if self.live.stale(id) {
                                ui.painter().circle_filled(resp.rect.right_top() + egui::vec2(-8.0, 8.0), 2.5, Color32::from_rgb(255, 184, 77));
                            }
                            if resp.clicked() { clicked = Some(id); }
                            resp.context_menu(|ui| {
                                if ui.button("Set as reference").clicked() { self.actions.push(FilmAction::SetReference(id)); ui.close(); }
                                if entry.missing {
                                    if ui.button("Locate file...").clicked() { self.actions.push(FilmAction::Locate(id)); ui.close(); }
                                    if ui.button("Relink folder...").clicked() { self.actions.push(FilmAction::RelinkFolder(id)); ui.close(); }
                                }
                            });
                        } else {
                            let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, ROW_H - 4.0), Sense::click());
                            if let Some(tex) = self.live.texture(id) {
                                let size = tex.size_vec2();
                                let fitted = size * (rect.width() / size.x).min(rect.height() / size.y);
                                ui.painter().image(tex.id(), Rect::from_center_size(rect.center(), fitted), Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                            } else {
                                ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, format!("Generated {}", id.0), NAME_FONT, Color32::GRAY);
                            }
                            if target.image == Some(id) { ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, Color32::LIGHT_BLUE), egui::StrokeKind::Inside); }
                            if self.live.stale(id) { ui.painter().circle_filled(rect.right_top() + egui::vec2(-8.0, 8.0), 2.5, Color32::from_rgb(255, 184, 77)); }
                            if resp.clicked() { clicked = Some(id); }
                        }
                    }
                });
            }
            range
        });
        let range = out.inner;
        self.last_range = range.clone();
        if let Some(id) = clicked.filter(|_| !target.image_pinned) { session.selected = Some(id); }
        let (lo, hi) = (range.start.saturating_sub(MARGIN_ROWS), (range.end + MARGIN_ROWS).min(rows));
        let mut order: Vec<usize> = (range.start..range.end).collect();
        order.extend((range.end..hi).chain((lo..range.start).rev()));
        let ordered: Vec<_> = order.into_iter().flat_map(|r| ids.iter().skip(r * columns).take(columns).copied()).collect();
        let mut wanted: Vec<_> = ordered.iter().copied().filter_map(|id| session.entry(id).filter(|e| !e.missing).map(|e| Wanted { id, path: e.path.clone(), hash: e.content_hash.clone() })).collect();
        self.scan_wanted.extend(wanted.iter().map(|w| w.id));
        if session.scanning && !session.demo {
            // Thumbnails need the content hash; unhashed files wait for the scan.
            wanted.retain(|w| !w.hash.is_empty());
            self.scan_hash_pending = wanted.len() != self.scan_wanted.len();
        }
        self.thumbs.set_wanted(&wanted);
        self.live_wanted = ordered;
        self.live_visible = ((range.end - range.start) * columns).min(self.live_wanted.len());
    }

    pub(crate) fn request_live(&mut self, ctx: &egui::Context, session: &mut Session, target: ViewerTarget, gpu: bool) {
        if self.live_wanted.is_empty() { self.live.suspend(); }
        else { self.live.request(ctx, session, target, (&self.live_wanted, self.live_visible), gpu); }
    }

    /// Details of the selected image, shown under the list.
    pub fn capture_info(ui: &mut egui::Ui, session: &Session, image: Option<rawgraph_core::ImageId>) {
        let Some(e) = image.and_then(|id| session.entry(id)) else { return };
        ui.label(RichText::new(display_name(e)).strong());
        ui.label(capture_summary(&e.capture));
        ui.label(e.capture.ev100().map_or("EV100 unknown".to_owned(), |v| format!("EV100 {v:+.2}")));
        if e.capture.flash_fired == Some(true) {
            ui.label("Flash fired");
        }
        let profile = (rawgraph_io::source_kind(&e.path) == Some(SourceKind::Rendered)).then(|| session.provider.profile_of(e.id)).flatten();
        if let Some(profile) = profile {
            let (text, warn) = crate::inspector::profile_text(&profile);
            let text = RichText::new(text);
            ui.label(if warn { text.color(Color32::from_rgb(255, 170, 90)) } else { text.weak() });
        }
    }
}

fn draw_row(ui: &mut egui::Ui, width: f32, e: &ImageEntry, selected: bool, tex: Option<&egui::TextureHandle>, failed: bool, dim: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H - 2.0), Sense::click());
    let name = display_name(e);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Image {name}")));
    let painter = ui.painter_at(rect);
    let fill = if selected {
        Color32::from_rgb(52, 70, 104)
    } else if resp.hovered() {
        Color32::from_gray(44)
    } else {
        Color32::from_gray(32)
    };
    painter.rect_filled(rect, 5.0, fill);
    if selected {
        painter.rect_stroke(rect, 5.0, Stroke::new(2.0, Color32::from_rgb(255, 210, 90)), StrokeKind::Inside);
    }
    let img_rect = Rect::from_min_size(rect.min + Vec2::splat(4.0), Vec2::new(width - 8.0, THUMB_H));
    painter.rect_filled(img_rect, 3.0, Color32::from_gray(18));
    match tex {
        Some(t) => {
            let ts = t.size_vec2();
            let s = (img_rect.width() / ts.x).min(img_rect.height() / ts.y);
            let r = Rect::from_center_size(img_rect.center(), ts * s);
            let tint = if e.missing || dim { Color32::from_gray(70) } else { Color32::WHITE };
            painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), tint);
        }
        None => {
            let text = if failed { "no preview" } else { "..." };
            painter.text(img_rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(12.0), Color32::GRAY);
        }
    }
    if e.missing {
        painter.text(img_rect.center(), Align2::CENTER_CENTER, "MISSING", FontId::proportional(14.0), Color32::from_rgb(255, 120, 100));
    }
    // Badges on the thumbnail.
    let mut x = img_rect.min.x + 5.0;
    if let Some(l) = e.color_label {
        painter.circle_filled(Pos2::new(x + 5.0, img_rect.min.y + 10.0), 5.5, label_color(l));
        painter.circle_stroke(Pos2::new(x + 5.0, img_rect.min.y + 10.0), 5.5, Stroke::new(1.0, Color32::BLACK));
        x += 15.0;
    }
    match e.flag {
        Flag::Pick => {
            painter.text(Pos2::new(x, img_rect.min.y + 10.0), Align2::LEFT_CENTER, "P", FontId::proportional(13.0), Color32::from_rgb(110, 230, 120));
        }
        Flag::Reject => {
            painter.text(Pos2::new(x, img_rect.min.y + 10.0), Align2::LEFT_CENTER, "X", FontId::proportional(13.0), Color32::from_rgb(240, 100, 90));
        }
        Flag::None => {}
    }
    if rawgraph_io::source_kind(&e.path) == Some(SourceKind::Rendered) {
        let b = Rect::from_min_size(Pos2::new(img_rect.max.x - 38.0, img_rect.min.y + 3.0), Vec2::new(35.0, 15.0));
        painter.rect_filled(b, 3.0, Color32::from_rgb(160, 110, 30));
        painter.text(b.center(), Align2::CENTER_CENTER, "JPEG", FontId::proportional(10.5), Color32::WHITE);
    }
    // Text lines: name, then rating.
    let y1 = img_rect.max.y + 10.0;
    let text_color = if dim { Color32::from_gray(110) } else { Color32::from_gray(228) };
    let line = name_line(&painter, rect, y1, &name, dim);
    painter.text(line.name_rect.left_center(), Align2::LEFT_CENTER, &line.shown, NAME_FONT, text_color);
    if let Some(r) = line.note_rect {
        painter.text(r.right_center(), Align2::RIGHT_CENTER, NOTE, NOTE_FONT, Color32::from_gray(120));
    }
    for i in 0..5u8 {
        let c = Pos2::new(rect.min.x + 14.0 + f32::from(i) * 15.0, y1 + 17.0);
        draw_star(&painter, c, 6.5, i < e.rating);
    }
    if let Some(iso) = e.capture.iso {
        let text = format!("ISO {iso:.0}");
        let w = text_width(&painter, &text, &FontId::proportional(11.5));
        // Never over the stars (a narrow panel drops the ISO value).
        if rect.right() - 6.0 - w > rect.min.x + STARS_END {
            painter.text(Pos2::new(rect.max.x - 6.0, y1 + 17.0), Align2::RIGHT_CENTER, text, FontId::proportional(11.5), Color32::from_gray(140));
        }
    }
    if line.shown == name {
        resp
    } else {
        resp.on_hover_text(name)
    }
}

const NAME_FONT: FontId = FontId::proportional(12.5);
const NOTE_FONT: FontId = FontId::proportional(10.5);
const NOTE: &str = "not in view";
/// Right end of the five stars, from the left edge of the row.
const STARS_END: f32 = 14.0 + 4.0 * 15.0 + 6.5 + 4.0;

/// Where the file name and the note on the right of the same line go.
pub(crate) struct NameLine {
    /// The name as shown: cut with an ellipsis when it does not fit.
    pub shown: String,
    pub name_rect: Rect,
    pub note_rect: Option<Rect>,
}

/// Lays out the name line of a row: the name never reaches the note ("not in view") or the edge.
pub(crate) fn name_line(painter: &egui::Painter, rect: Rect, y: f32, name: &str, dim: bool) -> NameLine {
    let left = rect.min.x + 6.0;
    let right = rect.max.x - 6.0;
    let note = dim.then(|| {
        let galley = painter.layout_no_wrap(NOTE.to_owned(), NOTE_FONT, Color32::WHITE);
        Rect::from_min_size(Pos2::new(right - galley.size().x, y - galley.size().y / 2.0), galley.size())
    });
    let limit = note.map_or(right, |n| n.left() - 8.0);
    let shown = fit_start(name, (limit - left).max(0.0), &|s| text_width(painter, s, &NAME_FONT));
    let galley = painter.layout_no_wrap(shown.clone(), NAME_FONT, Color32::WHITE);
    NameLine { shown, name_rect: Rect::from_min_size(Pos2::new(left, y - galley.size().y / 2.0), galley.size()), note_rect: note }
}

/// Five-pointed star, filled (rated) or outlined (not rated). Drawn as
/// shapes because the default fonts have no star glyph.
pub(crate) fn draw_star(painter: &egui::Painter, c: Pos2, r: f32, filled: bool) {
    let pts: Vec<Pos2> = (0..10)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
            let rad = if i % 2 == 0 { r } else { r * 0.45 };
            Pos2::new(c.x + rad * a.cos(), c.y + rad * a.sin())
        })
        .collect();
    if filled {
        let color = Color32::from_rgb(255, 205, 80);
        let mut mesh = egui::epaint::Mesh::default();
        mesh.colored_vertex(c, color);
        for p in &pts {
            mesh.colored_vertex(*p, color);
        }
        for i in 0..10u32 {
            mesh.add_triangle(0, 1 + i, 1 + (i + 1) % 10);
        }
        painter.add(egui::Shape::mesh(mesh));
    } else {
        painter.add(egui::Shape::closed_line(pts, Stroke::new(1.0, Color32::from_gray(120))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_never_overlaps_the_note_or_the_edge() {
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))), ..Default::default() };
        let mut cases = 0;
        for width in [80.0f32, 200.0, 320.0] {
            for len in [5usize, 8, 12, 20, 33, 47, 60, 90, 120] {
                for dim in [false, true] {
                    // Wide and narrow letters mixed, ending like the real names.
                    let mut name: String = "20260926_Smith_Jones-WMi".chars().cycle().take(len.saturating_sub(8)).collect();
                    name.push_str(".jpg.png");
                    let name: String = name.chars().take(len).collect();
                    let out = ctx.run_ui(input.clone(), |ui| {
                        let rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(width, 100.0));
                        let line = name_line(ui.painter(), rect, 60.0, &name, dim);
                        assert!(line.name_rect.right() <= rect.right() - 6.0 + 0.01, "{width} {len}: name {:?} reaches the edge {:?}", line.name_rect, rect);
                        assert!(line.name_rect.left() >= rect.left());
                        match (dim, line.note_rect) {
                            (true, Some(note)) => assert!(line.name_rect.right() + 4.0 <= note.left(), "{width} {len}: name {:?} touches the note {:?}", line.name_rect, note),
                            (false, None) => {}
                            other => panic!("note rect does not follow the dim state: {other:?}"),
                        }
                        if line.shown != name {
                            assert!(line.shown.ends_with('\u{2026}'));
                        }
                        // A name that fits stays whole.
                        if len <= 8 && width >= 200.0 {
                            assert_eq!(line.shown, name);
                        }
                    });
                    out.drop_without_applying_deltas();
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 54);
    }
}
