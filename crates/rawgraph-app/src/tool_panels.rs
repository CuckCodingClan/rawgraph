//! Inspector panels of the nodes that have a tool: Split by Metadata (range
//! table), Draw (brush controls) and Crop / Rotate (aspect, reset).

use eframe::egui::{self, Color32, RichText};
use rawgraph_core::{ImageId, Mode, NamedRange, NodeData, NodeId};

use crate::brush_logic::{self, count_text};
use crate::crop_logic::{self, Aspect, CropRect};
use crate::ranges_logic::{self, number_text};
use crate::session::Session;
use crate::tools::{crop_rect_of, set_crop_rect, set_strokes, strokes_of, transform_params};

const WARN: Color32 = Color32::from_rgb(255, 190, 90);
const ERROR: Color32 = Color32::from_rgb(255, 150, 120);

// ------------------------------------------------------------ split by metadata

/// The editing buffer of the range table. The graph only receives valid tables;
/// an invalid one stays here, with its message, until it is fixed.
#[derive(Clone, Default, PartialEq)]
struct RangeBuf {
    key: Option<(NodeId, ImageId, bool)>,
    applied: Vec<NamedRange>,
    rows: Vec<NamedRange>,
    message: Option<String>,
}

fn ranges_of(session: &Session, id: NodeId, image: ImageId) -> Vec<NamedRange> {
    match session.project.graph.node(id).and_then(|n| n.data(image)) {
        Some(NodeData::Ranges(r)) => r.clone(),
        _ => Vec::new(),
    }
}

/// Applies an edited table: stored when valid, otherwise the message says why not.
pub fn apply_ranges(session: &mut Session, id: NodeId, image: ImageId, rows: &[NamedRange]) -> Result<(), String> {
    ranges_logic::validate(rows).map_err(|e| e.to_string())?;
    let Some(n) = session.project.graph.node_mut(id) else { return Err("The node no longer exists".to_owned()) };
    if n.set_data(image, Some(NodeData::Ranges(rows.to_vec()))) {
        Ok(())
    } else {
        Err("The ranges were refused".to_owned())
    }
}

pub fn split_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let image = session.current_image();
    let individual = session.project.graph.node(id).is_some_and(|n| n.mode == Mode::Individual);
    let key = (id, image, individual);
    let mem = egui::Id::NULL.with(("range-table", id.0));
    let mut buf: RangeBuf = ui.data_mut(|d| d.get_temp(mem)).unwrap_or_default();
    let graph_rows = ranges_of(session, id, image);
    if buf.key != Some(key) || buf.applied != graph_rows {
        buf = RangeBuf { key: Some(key), applied: graph_rows.clone(), rows: graph_rows, message: None };
    }

    ui.add_space(8.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new("Ranges").strong());
        ui.label(RichText::new("An image leaves through the first range that contains its value; the others go to Unmatched.").weak());
        let mut remove = None;
        if buf.rows.is_empty() {
            ui.label(RichText::new("No ranges yet: every image is Unmatched.").weak());
        } else {
            egui::Grid::new(("ranges-grid", id.0)).num_columns(4).spacing([6.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Name").weak());
                ui.label(RichText::new("Min").weak());
                ui.label(RichText::new("Max").weak());
                ui.label("");
                ui.end_row();
                for (i, row) in buf.rows.iter_mut().enumerate() {
                    ui.add_sized([112.0, 20.0], egui::TextEdit::singleline(&mut row.name).char_limit(60));
                    for v in [&mut row.min, &mut row.max] {
                        let speed = (v.abs() * 0.01).max(0.001);
                        ui.add(egui::DragValue::new(v).speed(speed).custom_formatter(|n, _| number_text(n as f32)).custom_parser(|t| t.trim().parse::<f64>().ok()));
                    }
                    if crate::icons::button(ui, crate::icons::Icon::Delete, "Delete this range", true).clicked() {
                        remove = Some(i);
                    }
                    ui.end_row();
                }
            });
        }
        if let Some(i) = remove {
            ranges_logic::remove_row(&mut buf.rows, i);
        }
        let can_add = buf.rows.len() < rawgraph_core::graph::MAX_RANGES;
        ui.horizontal(|ui| {
            if ui.add_enabled(can_add, egui::Button::new("Add range")).on_hover_text("Up to six ranges").clicked() {
                if let Some(row) = ranges_logic::next_row(&buf.rows) {
                    buf.rows.push(row);
                }
            }
            if !can_add {
                ui.label(RichText::new("Six ranges is the maximum.").weak());
            }
        });
        if buf.rows != buf.applied {
            match apply_ranges(session, id, image, &buf.rows) {
                Ok(()) => {
                    buf.applied = buf.rows.clone();
                    buf.message = None;
                }
                Err(e) => buf.message = Some(e),
            }
        }
        if let Some(m) = &buf.message {
            ui.label(RichText::new(m).color(ERROR));
            ui.label(RichText::new("The graph keeps the last valid ranges until this is fixed.").weak().small());
        }
    });

    ui.add_space(6.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        let missing_exclude = session.project.graph.node(id).is_some_and(|n| n.choice("missing", image) == 1);
        match session.collection.splits.get(&id) {
            Some(s) => {
                ui.label(RichText::new(format!("{} image(s) reach this node", crate::collection::with_thousands(s.reaching))).strong());
                for (port, n) in s.per_port.iter().take(rawgraph_core::graph::SPLIT_RANGES).enumerate() {
                    if let Some(row) = graph_rows_name(session, id, image, port) {
                        ui.label(format!("{row}: {} image(s)", crate::collection::with_thousands(*n)));
                    }
                }
                ui.label(format!("Unmatched: {} image(s)", crate::collection::with_thousands(s.unmatched())));
                if missing_exclude {
                    ui.label(format!("Excluded for missing metadata: {}", crate::collection::with_thousands(s.excluded)));
                }
            }
            None => {
                ui.label(RichText::new("Counting images...").weak());
            }
        }
        ui.label(RichText::new("Images without a value for the measure go to Unmatched, or leave the graph when Missing metadata is set to Exclude.").weak());
    });
    ui.data_mut(|d| d.insert_temp(mem, buf));
}

/// Name of range `port` if it is defined for `image`.
fn graph_rows_name(session: &Session, id: NodeId, image: ImageId, port: usize) -> Option<String> {
    ranges_of(session, id, image).get(port).map(|r| format!("{} ({}..{})", r.name, number_text(r.min), number_text(r.max)))
}

// ---------------------------------------------------------------- drawing

pub fn brush_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let image = session.draw_image(id, session.current_image());
    let (mode, count) = session.project.graph.node(id).map_or((Mode::All, 0), |n| (n.mode, strokes_of(n, image).len()));
    ui.add_space(8.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new("Paint in the viewer").strong());
        match mode {
            Mode::Individual => {
                ui.label(RichText::new("Individual mode: the strokes belong to this image only. Other images keep their own strokes (or none).").color(Color32::from_rgb(255, 196, 120)));
            }
            Mode::All => {
                ui.label(RichText::new("Mode All: one set of strokes is shared by every image, so a stroke painted here shows on all of them. Switch the mode to Individual to paint per image.").color(WARN));
            }
        }
        ui.add_space(4.0);
        let b = &mut session.brush;
        ui.spacing_mut().slider_width = (ui.available_width() - 90.0).max(80.0);
        let l = ui.label("Size (% of long edge)");
        ui.add(egui::Slider::new(&mut b.size_pct, brush_logic::SIZE_PCT_MIN..=40.0).logarithmic(true).clamping(egui::SliderClamping::Always).max_decimals(2)).labelled_by(l.id);
        let l = ui.label("Hardness");
        ui.add(egui::Slider::new(&mut b.hardness, 0.0..=1.0).clamping(egui::SliderClamping::Always).max_decimals(2)).labelled_by(l.id);
        let l = ui.label("Flow");
        ui.add(egui::Slider::new(&mut b.flow, 0.0..=1.0).clamping(egui::SliderClamping::Always).max_decimals(2)).labelled_by(l.id);
        ui.checkbox(&mut b.erase, "Erase").on_hover_text("Strokes erase the drawing. Holding Alt while painting does the same.");
        ui.add_space(4.0);
        ui.label(RichText::new(count_text(count)).strong());
        let mut undo = false;
        let mut clear = false;
        ui.horizontal(|ui| {
            undo = ui.add_enabled(count > 0, egui::Button::new("Undo last stroke")).clicked();
            clear = ui.add_enabled(count > 0 && !session.brush.confirm_clear, egui::Button::new("Clear all strokes...")).clicked();
        });
        if clear {
            session.brush.confirm_clear = true;
        }
        if count == 0 {
            session.brush.confirm_clear = false;
        }
        if session.brush.confirm_clear {
            ui.label(RichText::new(format!("Remove all {} for this image?", count_text(count))).color(WARN));
            ui.horizontal(|ui| {
                if ui.button("Yes, clear").clicked() {
                    set_strokes(session, id, image, Vec::new());
                    session.brush.confirm_clear = false;
                }
                if ui.button("Cancel").clicked() {
                    session.brush.confirm_clear = false;
                }
            });
        }
        if undo {
            let mut strokes = session.project.graph.node(id).map(|n| strokes_of(n, image)).unwrap_or_default();
            if brush_logic::undo_last(&mut strokes) {
                set_strokes(session, id, image, strokes);
            }
        }
        ui.add_space(4.0);
        ui.label(RichText::new("Drag in the viewer to paint, Alt+drag erases. [ ] changes the size, Shift+[ ] the hardness, Ctrl+scroll the size. Space+drag or the middle button pans, the wheel zooms.").weak());
    });
}

// ---------------------------------------------------------------------- crop

/// The crop controls under the Crop / Rotate parameters. `size` is the pixel
/// size of the straightened picture when known.
pub fn crop_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId, size: Option<(u32, u32)>) {
    let image = session.current_image();
    ui.add_space(6.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new("Crop tool").strong());
        ui.label(RichText::new("The viewer shows the picture entering this node. Drag the handles or the inside of the rectangle; Space+drag pans.").weak());
        let before = (session.crop.aspect, session.crop.swap);
        ui.horizontal(|ui| {
            ui.label("Aspect");
            egui::ComboBox::from_id_salt(("crop-aspect", id.0)).selected_text(session.crop.aspect.label()).show_ui(ui, |ui| {
                for a in Aspect::ALL {
                    ui.selectable_value(&mut session.crop.aspect, a, a.label());
                }
            });
            let fixed = !matches!(session.crop.aspect, Aspect::Free | Aspect::Original | Aspect::Square);
            if ui.add_enabled(fixed, egui::Button::new("Portrait / landscape")).on_hover_text("Swap width and height of the ratio").clicked() {
                session.crop.swap = !session.crop.swap;
            }
        });
        if (session.crop.aspect, session.crop.swap) != before {
            // A new ratio fits the current rectangle at once.
            if let Some((w, h)) = size {
                let p = session.project.graph.node(id).map(|n| transform_params(n, image));
                if let Some(p) = p {
                    let picture = crop_logic::picture_size(w, h, p.angle_deg, p.quarter_turns);
                    if let Some(ratio) = session.crop.aspect.ratio(picture, session.crop.swap) {
                        let rect = session.project.graph.node(id).map(|n| crop_rect_of(n, image)).unwrap_or(CropRect::FULL);
                        set_crop_rect(session, id, image, crop_logic::fit_to_ratio(rect, ratio, picture));
                    }
                }
            }
        }
        let is_full = session.project.graph.node(id).is_some_and(|n| crop_rect_of(n, image).is_full());
        if ui.add_enabled(!is_full, egui::Button::new("Reset crop")).on_hover_text("Back to the whole picture").clicked() {
            set_crop_rect(session, id, image, CropRect::FULL);
        }
    });
}

// ------------------------------------------------------------ white-balance picker

/// The eyedropper of White Balance and Develop: arms a click in the viewer that makes the
/// clicked area of the node's input neutral. Clicking it again (or Escape) disarms it.
pub fn wb_picker_button(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let armed = crate::handles::picking(session) == Some(id);
    let r = ui.selectable_label(armed, "Pick neutral").on_hover_text("Click a grey or white area in the viewer to neutralise it (Escape cancels)");
    if r.clicked() {
        session.wb_pick = if armed { None } else { Some(id) };
    }
    if armed {
        ui.label(RichText::new("Click a neutral area in the viewer").weak());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_core::NodeKind;

    fn session_with(kind: NodeKind) -> (Session, NodeId) {
        let mut s = crate::session::tests::demo_session();
        let id = s.project.graph.add_node(kind, [0.0, 0.0]);
        (s, id)
    }

    fn r(name: &str, min: f32, max: f32) -> NamedRange {
        NamedRange { name: name.to_owned(), min, max }
    }

    #[test]
    fn valid_tables_are_stored_and_every_invalid_one_says_why() {
        let (mut s, id) = session_with(NodeKind::MetadataSplit);
        let img = s.current_image();
        assert!(apply_ranges(&mut s, id, img, &[r("Low", 0.0, 400.0), r("High", 401.0, 6400.0)]).is_ok());
        assert_eq!(ranges_of(&s, id, img).len(), 2);
        let kept = ranges_of(&s, id, img);
        for (rows, text) in [
            (vec![r("", 0.0, 1.0)], "no name"),
            (vec![r("A", 5.0, 1.0)], "minimum is above the maximum"),
            (vec![r("A", 0.0, 1.0), r("A", 2.0, 3.0)], "used twice"),
            ((0..7).map(|i| r(&format!("R{i}"), 0.0, 1.0)).collect(), "At most 6"),
        ] {
            let e = apply_ranges(&mut s, id, img, &rows).unwrap_err();
            assert!(e.contains(text), "{e} should mention {text}");
            assert_eq!(ranges_of(&s, id, img), kept, "an invalid table never reaches the graph");
        }
        assert!(apply_ranges(&mut s, NodeId(999), img, &[]).is_err());
    }

    #[test]
    fn ranges_are_per_image_in_individual_mode() {
        let (mut s, id) = session_with(NodeKind::MetadataSplit);
        s.set_mode(id, Mode::Individual);
        let img = s.current_image();
        apply_ranges(&mut s, id, img, &[r("Mine", 0.0, 100.0)]).unwrap();
        assert_eq!(ranges_of(&s, id, img).len(), 1);
        assert!(ranges_of(&s, id, ImageId(img.0 + 5)).is_empty());
        // The graph's labels follow the image's own ranges.
        assert_eq!(s.project.graph.output_label(id, 0, img), "Mine");
        assert_eq!(s.project.graph.output_label(id, 0, ImageId(img.0 + 5)), "Range 1");
    }

    #[test]
    fn graph_rows_names_show_the_range() {
        let (mut s, id) = session_with(NodeKind::MetadataSplit);
        let img = s.current_image();
        apply_ranges(&mut s, id, img, &[r("Low", 0.0, 400.0)]).unwrap();
        assert_eq!(graph_rows_name(&s, id, img, 0).as_deref(), Some("Low (0..400)"));
        assert_eq!(graph_rows_name(&s, id, img, 1), None);
    }
}
