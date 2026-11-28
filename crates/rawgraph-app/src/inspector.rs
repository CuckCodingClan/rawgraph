//! Inspector: parameters of the selected node for the selected image, plus the
//! view-only display settings.

use eframe::egui::{self, Color32, RichText};
use rawgraph_core::{CaptureInfo, ImageId, Mode, NodeId, NodeKind};

use crate::develop;
use crate::groups::{self, is_changed, ParamGroup};
use crate::node_ui;
use crate::session::Session;

pub fn format_ev(v: Option<f32>) -> String {
    v.map_or_else(|| "unknown".to_owned(), |v| format!("{v:+.2}"))
}

/// Why a capture has no EV100 (names the missing fields).
pub fn missing_fields(c: &CaptureInfo) -> Vec<&'static str> {
    let mut m = Vec::new();
    if c.iso.is_none_or(|v| v <= 0.0) {
        m.push("ISO");
    }
    if c.shutter_s.is_none_or(|v| v <= 0.0) {
        m.push("shutter");
    }
    if c.aperture.is_none_or(|v| v <= 0.0) {
        m.push("aperture");
    }
    m
}

pub fn capture_summary(c: &CaptureInfo) -> String {
    let iso = c.iso.map_or("ISO ?".to_owned(), |v| format!("ISO {v:.0}"));
    let shutter = c.shutter_s.map_or("? s".to_owned(), |t| {
        if t > 0.0 && t < 1.0 {
            format!("1/{:.0} s", 1.0 / t)
        } else {
            format!("{t:.1} s")
        }
    });
    let aperture = c.aperture.filter(|v| *v > 0.0).map_or("f/?".to_owned(), |n| format!("f/{n:.1}"));
    format!("{iso}  {shutter}  {aperture}")
}

/// True if the selected image is a JPEG/PNG/TIFF (already developed).
fn is_rendered(session: &Session) -> bool {
    session.current_entry().is_some_and(|e| rawgraph_io::source_kind(&e.path) == Some(rawgraph_io::SourceKind::Rendered))
}

pub fn show(ui: &mut egui::Ui, session: &mut Session) {
    let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if let Some(y) = session.inspector_scroll {
        // `--inspector-scroll`: screenshots of a long inspector.
        area = area.vertical_scroll_offset(y);
    }
    area.show(ui, |ui| {
        let develop_selected = session.selected_node.and_then(|id| session.project.graph.node(id)).is_some_and(|n| n.kind == NodeKind::Develop);
        if is_rendered(session) && !develop_selected {
            ui.group(|ui| {
                ui.label(
                    RichText::new("Rendered source (JPEG/PNG/TIFF): limited latitude, white balance is already baked in. Highlights that are clipped cannot be recovered.")
                        .color(Color32::from_rgb(235, 190, 120)),
                );
                profile_line(ui, session);
            });
            ui.add_space(6.0);
        }
        node_section(ui, session);
        ui.add_space(10.0);
        ui.separator();
        view_section(ui, session);
    });
}

/// "Colour profile: ..." for the selected rendered image, once its proxy has been decoded.
pub fn profile_line(ui: &mut egui::Ui, session: &Session) {
    if !is_rendered(session) {
        return;
    }
    if let Some(profile) = session.provider.profile_of(session.current_image()) {
        let (text, warn) = profile_text(&profile);
        let rich = RichText::new(text);
        ui.label(if warn { rich.color(Color32::from_rgb(255, 170, 90)) } else { rich.weak() });
    }
}

/// Text for an embedded colour profile and whether it deserves a warning.
pub fn profile_text(profile: &rawgraph_io::InputProfile) -> (String, bool) {
    use rawgraph_io::InputProfile;
    match profile {
        InputProfile::Untagged => ("Colour profile: untagged \u{2014} treated as sRGB".to_owned(), false),
        InputProfile::Srgb => ("Colour profile: sRGB".to_owned(), false),
        InputProfile::Matrix(name) => (format!("Colour profile: {name}"), false),
        InputProfile::UnsupportedTreatedAsSrgb(why) => (format!("Warning: the embedded colour profile cannot be used ({why}); the file is read as sRGB, so its colours may be off"), true),
    }
}

fn node_section(ui: &mut egui::Ui, session: &mut Session) {
    let image_name = session.current_entry().map(crate::testimages::display_name).unwrap_or_default();
    let Some(id) = session.selected_node.filter(|id| session.project.graph.node(*id).is_some()) else {
        ui.heading("Inspector");
        ui.label(RichText::new("Select a node in the graph to edit its parameters.").weak());
        return;
    };
    let kind = session.project.graph.node(id).map_or(NodeKind::Source, |n| n.kind);
    let image = if kind == NodeKind::Draw { session.draw_image(id, session.current_image()) } else { session.current_image() };
    ui.heading(kind.label());
    if session.node_sel.len() > 1 {
        ui.label(RichText::new(format!("{} nodes selected", session.node_sel.len())).color(Color32::from_rgb(255, 210, 90)));
    }
    ui.label(RichText::new(format!("Node {} | image: {}", id.0, image_name)).weak());
    ui.add_space(4.0);

    let specs = kind.params();
    if specs.is_empty() {
        ui.label(RichText::new("This node has no parameters.").weak());
    } else {
        // All | Individual toggle. Switching only changes the mode; the core keeps all values.
        // A Source and a Merge only have collection-level settings, which cannot differ per image.
        let mut mode = session.project.graph.node(id).map_or(Mode::All, |n| n.mode);
        let before = mode;
        if !matches!(kind, NodeKind::Source | NodeKind::Merge) {
            ui.horizontal(|ui| {
                ui.label("Mode");
                ui.selectable_value(&mut mode, Mode::All, "All").on_hover_text("One set of values for every image");
                ui.selectable_value(&mut mode, Mode::Individual, "Individual")
                    .on_hover_text("Values stored per image; images without their own values inherit the All values");
            });
        }
        if mode != before {
            session.set_mode(id, mode);
        }
        ui.add_space(4.0);

        let (grouped, rest) = groups::param_groups(kind);
        if kind == NodeKind::Transform {
            // The crop tool comes first: its aspect combo belongs next to the picture.
            let size = source_size(session, image);
            crate::tool_panels::crop_section(ui, session, id, size);
            // The result sizes sit right under the crop tool, where they are always in view.
            transform_size_line(ui, session, id, image);
        }
        if kind == NodeKind::Develop {
            develop_section(ui, session, id, image, &grouped, &rest);
        } else if kind == NodeKind::ColorWheels {
            for group in &grouped {
                wheel_group_ui(ui, session, id, image, group);
            }
        } else if kind == NodeKind::Source {
            node_ui::source_section(ui, session, id);
        } else if kind == NodeKind::Merge {
            node_ui::merge_rule_section(ui, session, id);
        } else if kind == NodeKind::Layer {
            node_ui::layer_section(ui, session, id, image);
        } else if kind == NodeKind::Export {
            node_ui::export_section(ui, session, id, image);
        } else if kind == NodeKind::LensManual {
            if let Some(spec) = kind.param_spec("mode") {
                ui.label("Correction");
                param_widget_opts(ui, session, id, spec, image, false);
            }
            node_ui::lens_status_ui(ui, session, id, image);
            ui.add_space(4.0);
            let auto = session.project.graph.node(id).is_some_and(|n| n.choice("mode", image) == 1);
            ui.label(RichText::new(if auto { "Fine-tuning" } else { "Manual correction" }).strong());
            for name in rest.iter().filter(|n| **n != "mode") {
                if let Some(spec) = kind.param_spec(name) {
                    param_widget(ui, session, id, spec, image);
                }
            }
        } else {
            for name in rest {
                if kind == NodeKind::Generator && (name.starts_with("start_") || name.starts_with("end_") || (name.starts_with("ramp_") && !matches!(name, "ramp_interpolation" | "ramp_smoothness" | "ramp_dither"))) { continue; }
                if kind == NodeKind::Blur && name == "angle" && !session.project.graph.node(id).is_some_and(|n| n.choice("kernel", image) == 3) {
                    continue;
                }
                // The tabs of the curves editor choose the channel.
                if kind == NodeKind::PointCurve && name == "channel" {
                    continue;
                }
                if let Some(spec) = kind.param_spec(name) {
                    param_widget(ui, session, id, spec, image);
                }
            }
            for group in &grouped {
                group_ui(ui, session, id, image, group);
            }
        }
        if kind == NodeKind::Clarity {
            ui.add_space(6.0);
            ui.label(RichText::new(CLARITY_NOTE).weak());
        }
        if kind == NodeKind::NoiseReduction && session.project.graph.node(id).is_some_and(|n| n.toggle("iso_adaptive", image)) {
            let iso = session.current_entry().and_then(|e| e.capture.iso);
            ui.label(RichText::new(groups::iso_scale_line(iso)).monospace());
            ui.label(RichText::new("The strength is scaled with the capture ISO of this image (tuned at ISO 2500).").weak());
        }
        if kind == NodeKind::Sharpen {
            ui.add_space(6.0);
            ui.label(RichText::new("Sharpening acts at pixel level: judge it at 100 % zoom; in a reduced preview it is weaker than in the full-size export.").color(Color32::from_rgb(235, 190, 120)));
        }

        let node = session.project.graph.node(id);
        if let (Some(node), Mode::Individual) = (node, mode) {
            ui.add_space(6.0);
            let has = node.has_individual(image);
            let count = node.individual_count();
            ui.group(|ui| {
                if has {
                    ui.label(RichText::new("This image has its own values.").color(Color32::from_rgb(255, 196, 120)));
                } else {
                    ui.label(RichText::new("This image inherits the All values; moving a slider gives it its own.").weak());
                }
                ui.label(format!("{count} image{} adjusted", if count == 1 { "" } else { "s" }));
                if ui.add_enabled(has, egui::Button::new("Reset this image")).clicked() {
                    if let Some(n) = session.project.graph.node_mut(id) {
                        n.clear_individual(image);
                    }
                }
            });
        }
    }

    match kind {
        NodeKind::Generator if session.project.graph.node(id).is_some_and(|n| n.choice("type", image) == 0) => crate::gradient_editor::show(ui, session, id),
        NodeKind::PointCurve => crate::curve_editor::show(ui, session, id),
        NodeKind::Draw => crate::tool_panels::brush_section(ui, session, id),
        NodeKind::WhiteBalance => {
            ui.add_space(6.0);
            crate::tool_panels::wb_picker_button(ui, session, id);
        }
        NodeKind::MetadataSplit => crate::tool_panels::split_section(ui, session, id),
        NodeKind::Lut => node_ui::lut_section(ui, session, id, image),
        _ => {}
    }
    if kind == NodeKind::MetadataFilter {
        filter_section(ui, session, id);
    }
    if kind == NodeKind::Merge {
        merge_section(ui, session, id);
    }
    if kind == NodeKind::CaptureEv {
        capture_section(ui, session, id);
    }
    if kind == NodeKind::Blur {
        ui.add_space(8.0);
        ui.label(RichText::new("High pass: 0.5 + input − low pass (unclipped RGB); alpha is unchanged.").weak());
        ui.label(RichText::new("Add Input can expose Radius as a full-range intensity map or Direction (RGB) for Motion Blur.").weak());
    }
}

/// What the Clarity / Texture inspector says about where the node runs.
pub const CLARITY_NOTE: &str = "Clarity and Texture are calculated on the CPU; nodes after them still use the GPU.";

/// One group of Lift / Gamma / Gain: master slider, red / green / blue sliders, a swatch with the
/// direction of the tint, and "Reset group".
fn wheel_group_ui(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId, group: &ParamGroup) {
    let kind = NodeKind::ColorWheels;
    let (changed, tint) = match session.project.graph.node(id) {
        Some(n) => {
            let c = ["_r", "_g", "_b"].map(|suffix| {
                let name = group.names.iter().find(|x| x.ends_with(suffix)).copied().unwrap_or("");
                n.param(name, image)
            });
            let range = group.names.last().and_then(|name| kind.param_spec(name)).map_or(1.0, |s| s.max);
            (groups::group_changed(n, image, group), groups::tint_rgb(c, range))
        }
        None => (false, [128; 3]),
    };
    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 4.0, Color32::from_rgb(tint[0], tint[1], tint[2]));
            ui.painter().rect_stroke(r, 4.0, egui::Stroke::new(1.0, Color32::from_gray(150)), egui::StrokeKind::Inside);
            resp.on_hover_text("The direction of the colour tint of this group (grey: no tint).");
            let title = RichText::new(group.title.as_str()).strong();
            ui.label(if changed { title.color(Color32::from_rgb(255, 196, 120)) } else { title });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(changed, egui::Button::new("Reset group")).on_hover_text("Back to the defaults of this group").clicked() {
                    if let Some(n) = session.project.graph.node_mut(id) {
                        for name in &group.names {
                            if let Some(spec) = kind.param_spec(name) {
                                n.set_param(name, image, spec.default);
                            }
                        }
                    }
                }
            });
        });
        for name in &group.names {
            if let Some(spec) = kind.param_spec(name) {
                param_widget(ui, session, id, spec, image);
            }
        }
    });
}

/// A block of related parameters: a collapsible header with a colour swatch
/// for colour bands, a framed block otherwise.
fn group_ui(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId, group: &ParamGroup) {
    let kind = session.project.graph.node(id).map_or(NodeKind::Source, |n| n.kind);
    let changed = session.project.graph.node(id).is_some_and(|n| group.names.iter().any(|name| is_changed(n, image, name)));
    match group.swatch {
        Some(rgb) => {
            let state_id = ui.make_persistent_id(("param-group", id.0, group.title.as_str()));
            let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), state_id, changed);
            state
                .show_header(ui, |ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 6.0, Color32::from_rgb(rgb[0], rgb[1], rgb[2]));
                    let text = RichText::new(group.title.as_str()).strong();
                    ui.label(if changed { text.color(Color32::from_rgb(255, 196, 120)) } else { text });
                    if changed {
                        ui.label(RichText::new("changed").weak().small());
                    }
                })
                .body(|ui| {
                    for name in &group.names {
                        if let Some(spec) = kind.param_spec(name) {
                            param_widget(ui, session, id, spec, image);
                        }
                    }
                });
        }
        None => {
            ui.add_space(4.0);
            ui.group(|ui| {
                ui.set_min_width(ui.available_width());
                ui.label(RichText::new(group.title.as_str()).strong());
                for name in &group.names {
                    if let Some(spec) = kind.param_spec(name) {
                        param_widget(ui, session, id, spec, image);
                    }
                }
            });
        }
    }
}

/// "Resulting size" line below the Transform parameters (preview resolution).
fn transform_size_line(ui: &mut egui::Ui, session: &Session, id: NodeId, image: ImageId) {
    let sizes = transform_result_sizes(session, id, image);
    ui.add_space(4.0);
    for line in size_lines(&sizes) {
        ui.label(RichText::new(line).monospace());
    }
}

/// What the Crop / Rotate node makes of the picture, for the preview proxy and for the file at
/// full resolution (both run through every Crop / Rotate node upstream first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ResultSizes {
    /// (result, source) at preview resolution.
    pub preview: Option<((u32, u32), (u32, u32))>,
    /// Result at full resolution.
    pub full: Option<(u32, u32)>,
}

pub fn result_sizes(graph: &rawgraph_core::Graph, node: NodeId, image: ImageId, proxy: Option<(u32, u32)>, full: Option<(u32, u32)>) -> ResultSizes {
    let through = |src: (u32, u32)| crate::frames::size_after(graph, node, image, crate::frames::size_into(graph, node, image, src));
    ResultSizes { preview: proxy.map(|p| (through(p), p)), full: full.map(through) }
}

fn transform_result_sizes(session: &Session, id: NodeId, image: ImageId) -> ResultSizes {
    result_sizes(&session.project.graph, id, image, source_size(session, image), session.provider.full_size_of(image))
}

/// The lines under the Crop / Rotate parameters.
pub fn size_lines(sizes: &ResultSizes) -> Vec<String> {
    let preview = match sizes.preview {
        Some(((w, h), (sw, sh))) => format!("Resulting size: {w} x {h} px (preview; source {sw} x {sh} px)"),
        None => "Resulting size: shown once the image is loaded".to_owned(),
    };
    let full = match sizes.full {
        Some((w, h)) => format!("full size: {w} x {h} px"),
        None => "full size: shown once the image is loaded".to_owned(),
    };
    vec![preview, full]
}

/// Pixel size of the decoded proxy of `image`, if it is in memory (the UI thread never decodes).
fn source_size(session: &Session, image: ImageId) -> Option<(u32, u32)> {
    use rawgraph_engine::ImageProvider;
    if !session.provider.is_cached(image) {
        return None;
    }
    let src = session.provider.image(image).ok()?;
    Some((src.width(), src.height()))
}

/// The Develop inspector: white balance and baseline exposure blocks.
fn develop_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId, groups_: &[ParamGroup], rest: &[&'static str]) {
    use rawgraph_core::BaselineSource;
    let capture = session.current_entry().map(|e| e.capture.clone());
    let rendered = is_rendered(session);
    let settings = session.project.graph.node(id).map(|n| rawgraph_core::develop_settings(n, image, capture.as_ref()));
    let custom = session.project.graph.node(id).is_some_and(|n| n.choice("wb", image) == 1);
    let (source, baseline, total) = settings.as_ref().map_or((BaselineSource::Off, 0.0, 0.0), |s| (s.baseline_source, s.baseline_ev, s.gain_ev));
    let view = develop::develop_view(custom, capture.as_ref(), rendered, source, baseline, total);
    let kind = NodeKind::Develop;

    for group in groups_ {
        match group.title.as_str() {
            "Tone" => group_frame(ui, session, id, image, group, |ui, session| {
                for name in &group.names {
                    if let Some(spec) = kind.param_spec(name) {
                        param_widget(ui, session, id, spec, image);
                    }
                }
            }),
            "Colour" => group_frame(ui, session, id, image, group, |ui, session| {
                if let Some(spec) = kind.param_spec("wb") {
                    ui.label("White balance");
                    param_widget_opts(ui, session, id, spec, image, false);
                }
                let now_custom = session.project.graph.node(id).is_some_and(|n| n.choice("wb", image) == 1);
                if now_custom && !custom {
                    // Just switched to Custom: start from the as-shot values so nothing jumps.
                    if let (Some(s), Some(n)) = (settings.as_ref(), session.project.graph.node_mut(id)) {
                        develop::seed_node(n, image, s.as_shot);
                    }
                }
                ui.label(RichText::new(view.as_shot.as_str()).weak());
                crate::tool_panels::wb_picker_button(ui, session, id);
                if now_custom {
                    for name in ["temperature", "tint"] {
                        if let Some(spec) = kind.param_spec(name) {
                            param_widget(ui, session, id, spec, image);
                        }
                    }
                }
                if let Some(note) = view.rendered_note {
                    ui.label(RichText::new(note).color(Color32::from_rgb(235, 190, 120)));
                    profile_line(ui, session);
                }
                for name in ["vibrance", "saturation"] {
                    if let Some(spec) = kind.param_spec(name) {
                        param_widget(ui, session, id, spec, image);
                    }
                }
            }),
            _ => {},
        }
    }
    // Ungrouped Develop settings (the Legacy tone toggle) sit at the very bottom.
    ui.add_space(4.0);
    for name in rest {
        if let Some(spec) = kind.param_spec(name) {
            param_widget(ui, session, id, spec, image);
        }
    }
}

/// Sets every parameter of `group` back to its default (one undo step).
pub fn reset_group(session: &mut Session, id: NodeId, image: ImageId, group: &ParamGroup) {
    let Some(node) = session.project.graph.node_mut(id) else { return };
    let kind = node.kind;
    for name in &group.names {
        if let Some(spec) = kind.param_spec(name) {
            node.set_param(name, image, spec.default);
        }
    }
}

/// A framed block of parameters with its title and a "Reset" button that sets the whole group back.
fn group_frame(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId, group: &ParamGroup, body: impl FnOnce(&mut egui::Ui, &mut Session)) {
    let changed = session.project.graph.node(id).is_some_and(|n| groups::group_changed(n, image, group));
    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            let title = RichText::new(group.title.as_str()).strong();
            ui.label(if changed { title.color(Color32::from_rgb(255, 196, 120)) } else { title });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(changed, egui::Button::new("Reset")).on_hover_text("Back to the defaults of this group").clicked() {
                    reset_group(session, id, image, group);
                }
            });
        });
        body(ui, session);
    });
}

/// Which widget edits a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetKind {
    Slider,
    LogSlider,
    /// Numeric field for very wide ranges.
    Drag,
    Combo,
    Check,
}

/// Ranges wider than this get a numeric field instead of a slider.
const WIDE_RANGE: f32 = 5000.0;

pub fn widget_for(spec: &rawgraph_core::graph::ParamSpec) -> WidgetKind {
    use rawgraph_core::ParamKind;
    match spec.kind {
        ParamKind::Choice(_) => WidgetKind::Combo,
        ParamKind::Toggle => WidgetKind::Check,
        // Positive ranges use log scaling; ranges starting at zero use a power curve.
        ParamKind::Continuous if spec.logarithmic && spec.min >= 0.0 => WidgetKind::LogSlider,
        ParamKind::Continuous if spec.max - spec.min > WIDE_RANGE => WidgetKind::Drag,
        ParamKind::Continuous => WidgetKind::Slider,
    }
}

fn param_widget(ui: &mut egui::Ui, session: &mut Session, id: rawgraph_core::NodeId, spec: &rawgraph_core::graph::ParamSpec, image: rawgraph_core::ImageId) {
    param_widget_opts(ui, session, id, spec, image, true);
}

/// `show_label = false` leaves out the label above a combo box (the block title already says it).
fn param_widget_opts(ui: &mut egui::Ui, session: &mut Session, id: rawgraph_core::NodeId, spec: &rawgraph_core::graph::ParamSpec, image: rawgraph_core::ImageId, show_label: bool) {
    let current = session.project.graph.node(id).map_or(spec.default, |n| n.param(spec.name, image));
    let mut value = current;
    // The persisted Filter radius stays in per-mille for compatibility.
    let filter_radius = spec.name == "radius" && session.project.graph.node(id).is_some_and(|n| n.kind == NodeKind::Blur);
    let label = if filter_radius { "Radius (% of long edge)".to_owned() }
        else if spec.unit.is_empty() { spec.label.to_owned() } else { format!("{} ({})", spec.label, spec.unit) };
    let kind = widget_for(spec);
    match kind {
        WidgetKind::Check => {
            let mut on = current >= 0.5;
            if ui.checkbox(&mut on, label).changed() {
                value = if on { 1.0 } else { 0.0 };
            }
        }
        WidgetKind::Combo => {
            let rawgraph_core::ParamKind::Choice(options) = spec.kind else { return };
            let label_resp = show_label.then(|| ui.label(label.clone()));
            let mut idx = (current.max(0.0).round() as usize).min(options.len().saturating_sub(1));
            let combo = egui::ComboBox::from_id_salt((id.0, spec.name))
                .selected_text(options.get(idx).copied().unwrap_or(""))
                .width((ui.available_width() - 8.0).max(80.0))
                .show_ui(ui, |ui| {
                    for (i, o) in options.iter().enumerate() {
                        ui.selectable_value(&mut idx, i, *o);
                    }
                })
                .response;
            match label_resp {
                Some(l) => {
                    combo.labelled_by(l.id);
                }
                None => combo.widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, label.clone())),
            }
            value = idx as f32;
        }
        WidgetKind::Drag => {
            let label_resp = ui.label(label);
            let speed = ((current.abs() + 10.0) * 0.01).max(0.1);
            let resp = ui
                .add(egui::DragValue::new(&mut value).range(spec.min..=spec.max).speed(speed).max_decimals(2))
                .labelled_by(label_resp.id)
                .on_hover_text("Drag or type a value; double-click to reset");
            if resp.double_clicked() {
                value = spec.default;
            }
        }
        WidgetKind::Slider | WidgetKind::LogSlider => {
            let label_resp = ui.add(egui::Label::new(label).sense(egui::Sense::click())).on_hover_text("Double-click to reset to the default");
            let width = (ui.available_width() - 76.0).max(80.0);
            if filter_radius {
                let mut percent = value / 10.0;
                if crate::fine_slider::show(ui, &mut percent, 0.0..=100.0, spec.default / 10.0, true, &label_resp, width, 3).changed() {
                    value = percent * 10.0;
                }
            } else {
                crate::fine_slider::show(ui, &mut value, spec.min..=spec.max, spec.default, kind == WidgetKind::LogSlider, &label_resp, width, 3);
            }
        }
    }
    if (value - current).abs() > f32::EPSILON {
        if let Some(n) = session.project.graph.node_mut(id) {
            n.set_param(spec.name, image, value);
        }
    }
}

fn filter_section(ui: &mut egui::Ui, session: &Session, id: rawgraph_core::NodeId) {
    ui.add_space(8.0);
    ui.group(|ui| match session.collection.filters.get(&id) {
        Some(s) => {
            ui.label(RichText::new(filter_text(s)).strong());
            ui.label(RichText::new("Keeps images whose metadata value lies in the range. Images without the value follow the Missing metadata option.").weak());
        }
        None => {
            ui.label(RichText::new("Counting images...").weak());
        }
    });
}

/// "matches n of m (k without metadata)"
pub fn filter_text(s: &crate::collection::FilterStats) -> String {
    use crate::collection::with_thousands as th;
    format!("matches {} of {} ({} without metadata)", th(s.matching), th(s.reaching), th(s.missing))
}

fn merge_section(ui: &mut egui::Ui, session: &Session, id: rawgraph_core::NodeId) {
    ui.add_space(8.0);
    ui.group(|ui| {
        ui.label(RichText::new("Merge: an image comes from the first input that has it").strong());
        match session.collection.merges.get(&id) {
            Some(s) => {
                for line in merge_lines(s) {
                    ui.label(line);
                }
            }
            None => {
                ui.label(RichText::new("Counting images...").weak());
            }
        }
    });
}

pub fn merge_lines(s: &crate::collection::MergeStats) -> Vec<String> {
    use crate::collection::with_thousands as th;
    let mut v: Vec<String> = ["A", "B", "C", "D"].iter().zip(s.supplied).map(|(n, c)| format!("Input {n} supplies {} image(s)", th(c))).collect();
    v.push(format!("{} duplicate(s) resolved by port order", th(s.duplicates)));
    v.push(format!("{} image(s) in total", th(s.total)));
    v
}

fn capture_section(ui: &mut egui::Ui, session: &mut Session, id: rawgraph_core::NodeId) {
    let Some(entry) = session.current_entry() else { return };
    let image = entry.id;
    let c = entry.capture.clone();
    let ev = c.ev100();
    let delta = session.project.graph.capture_ev_delta(id, image);
    let upstream = develop::upstream_delta(&session.project.graph, id, image);
    let mut new_delta = None;
    ui.add_space(8.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new("Capture exposure value").strong());
        egui::Grid::new("capture_grid").num_columns(2).spacing([14.0, 3.0]).show(ui, |ui| {
            ui.label("EV100 (capture)");
            ui.label(RichText::new(format_ev(ev)).monospace());
            ui.end_row();
            ui.label("ISO");
            ui.label(c.iso.map_or("missing".to_owned(), |v| format!("{v:.0}")));
            ui.end_row();
            ui.label("Shutter");
            ui.label(c.shutter_s.map_or("missing".to_owned(), |t| if t < 1.0 && t > 0.0 { format!("1/{:.0} s", 1.0 / t) } else { format!("{t} s") }));
            ui.end_row();
            ui.label("Aperture");
            ui.label(c.aperture.filter(|v| *v > 0.0).map_or("missing".to_owned(), |n| format!("f/{n:.1}")));
            ui.end_row();
            ui.label("Flash");
            ui.label(match c.flash_fired {
                Some(true) => "fired",
                Some(false) => "off",
                None => "unknown",
            });
            ui.end_row();
        });
        ui.add_space(4.0);
        match ev {
            Some(v) => {
                let mut abs = develop::absolute_ev(Some(v), delta).unwrap_or(v);
                ui.horizontal(|ui| {
                    ui.label("EV100 of this image");
                    let r = ui
                        .add(egui::DragValue::new(&mut abs).speed(0.05).max_decimals(2).range((v + upstream - 6.0)..=(v + upstream + 6.0)))
                        .on_hover_text("Absolute value after all Capture EV changes up to and including this node. Type a value to set this node's change so the result matches.");
                    if r.changed() {
                        new_delta = Some(develop::delta_for_absolute(v, upstream, abs));
                    }
                });
                ui.label(RichText::new(format!("Capture {v:+.2}, changes up to here {delta:+.2} EV")).weak());
            }
            None => {
                let missing = missing_fields(&c).join(", ");
                let mut dummy = 0.0f32;
                ui.horizontal(|ui| {
                    ui.label("EV100 of this image");
                    ui.add_enabled(false, egui::DragValue::new(&mut dummy));
                    ui.label(RichText::new("unknown").color(Color32::from_rgb(255, 190, 110)));
                });
                ui.label(RichText::new(format!("EV100 unknown (missing {missing}); only the change slider works. Change up to here {delta:+.2} EV")).color(Color32::from_rgb(255, 190, 110)));
            }
        }
    });
    if let (Some(d), Some(n)) = (new_delta, session.project.graph.node_mut(id)) {
        n.set_param("delta", image, d);
    }
}

fn view_section(ui: &mut egui::Ui, session: &mut Session) {
    ui.add_space(4.0);
    ui.heading("View");
    ui.label(RichText::new("Tone map affects preview and scopes; exports can optionally apply the selected tone map (EXR stays linear).").weak());
    ui.horizontal(|ui| {
        ui.label("Tone map");
        egui::ComboBox::from_id_salt("view-tone").selected_text(session.view.tone.label()).show_ui(ui, |ui| {
            for t in crate::session::ViewTone::ALL {
                let r = ui.selectable_value(&mut session.view.tone, t, t.label());
                if t == crate::session::ViewTone::Aces {
                    r.on_hover_text("An approximation of the ACES 1.x SDR rendering (fitted curve), not the reference transform.");
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widgets_follow_the_parameter_kind() {
        use rawgraph_core::NodeKind;
        let spec = |k: NodeKind, name: &str| *k.param_spec(name).unwrap();
        assert_eq!(widget_for(&spec(NodeKind::Exposure, "ev")), WidgetKind::Slider);
        assert_eq!(widget_for(&spec(NodeKind::Contrast, "pivot")), WidgetKind::LogSlider, "logarithmic hint");
        assert_eq!(widget_for(&spec(NodeKind::MetadataFilter, "measure")), WidgetKind::Combo);
        assert_eq!(widget_for(&spec(NodeKind::MetadataFilter, "missing")), WidgetKind::Combo);
        assert_eq!(widget_for(&spec(NodeKind::MetadataFilter, "invert")), WidgetKind::Check);
        assert_eq!(widget_for(&spec(NodeKind::MetadataFilter, "min")), WidgetKind::Drag, "very wide range");
        assert_eq!(widget_for(&spec(NodeKind::NoiseReduction, "luma")), WidgetKind::Slider);
    }

    #[test]
    fn develop_params_form_the_two_blocks() {
        let (groups, _) = groups::param_groups(NodeKind::Develop);
        assert_eq!(groups.iter().map(|g| g.title.as_str()).collect::<Vec<_>>(), vec!["Tone", "Colour"]);
        assert_eq!(widget_for(&NodeKind::Develop.param_spec("brightness").copied().unwrap()), WidgetKind::Slider);
        let spec = |n: &str| *NodeKind::Develop.param_spec(n).unwrap();
        assert_eq!(widget_for(&spec("wb")), WidgetKind::Combo);
        assert_eq!(widget_for(&spec("baseline")), WidgetKind::Check);
        assert_eq!(widget_for(&spec("temperature")), WidgetKind::LogSlider, "temperature is logarithmic");
    }

    #[test]
    fn filter_and_merge_texts() {
        let f = crate::collection::FilterStats { reaching: 1204, matching: 900, missing: 7 };
        assert_eq!(filter_text(&f), "matches 900 of 1,204 (7 without metadata)");
        let m = crate::collection::MergeStats { supplied: [3, 2, 0, 0], duplicates: 1, total: 5, dropped: 0 };
        let lines = merge_lines(&m);
        assert_eq!(lines[0], "Input A supplies 3 image(s)");
        assert!(lines[4].starts_with("1 duplicate"));
    }

    #[test]
    fn input_profile_texts() {
        use rawgraph_io::InputProfile;
        assert_eq!(profile_text(&InputProfile::Untagged), ("Colour profile: untagged \u{2014} treated as sRGB".to_owned(), false));
        assert_eq!(profile_text(&InputProfile::Srgb), ("Colour profile: sRGB".to_owned(), false));
        assert_eq!(profile_text(&InputProfile::Matrix("Adobe RGB (1998)".to_owned())), ("Colour profile: Adobe RGB (1998)".to_owned(), false));
        let (text, warn) = profile_text(&InputProfile::UnsupportedTreatedAsSrgb("CMYK profile".to_owned()));
        assert!(warn && text.starts_with("Warning:") && text.contains("CMYK profile") && text.contains("read as sRGB"), "{text}");
    }

    #[test]
    fn ev_text() {
        assert_eq!(format_ev(None), "unknown");
        assert_eq!(format_ev(Some(1.0)), "+1.00");
    }

    #[test]
    fn missing_aperture_is_named() {
        let c = CaptureInfo { iso: Some(100.0), shutter_s: Some(0.01), aperture: None, ..Default::default() };
        assert_eq!(missing_fields(&c), vec!["aperture"]);
        assert!(capture_summary(&c).contains("f/?"));
    }

    // ---- what the inspector says for the new nodes (the text that is really painted)

    fn painted_text(out: &egui::FullOutput) -> String {
        fn walk(shape: &egui::Shape, into: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(t) => into.push(t.galley.text().to_owned()),
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, into)),
                _ => {}
            }
        }
        let mut lines = Vec::new();
        for clipped in &out.shapes {
            walk(&clipped.shape, &mut lines);
        }
        lines.join("\n")
    }

    fn inspector_text(session: &mut Session) -> String {
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 1400.0))), ..Default::default() };
        let first = ctx.run_ui(input.clone(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| show(ui, session));
        });
        first.drop_without_applying_deltas();
        // A second frame, so that everything that needs a frame to settle is there.
        let out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| show(ui, session));
        });
        let text = painted_text(&out);
        out.drop_without_applying_deltas();
        text
    }

    fn session_with(kind: NodeKind) -> (Session, NodeId) {
        let mut s = crate::session::tests::demo_session();
        let id = s.project.graph.add_node(kind, [0.0, 0.0]);
        s.selected_node = Some(id);
        (s, id)
    }

    #[test]
    fn the_wheels_inspector_has_three_groups_with_sliders_swatches_and_reset() {
        let (mut s, id) = session_with(NodeKind::ColorWheels);
        let img = s.current_image();
        let text = inspector_text(&mut s);
        for needle in ["Lift / Gamma / Gain", "Lift", "Gamma", "Gain", "Lift red", "Lift green", "Lift blue", "Gamma blue", "Gain green", "Reset group"] {
            assert!(text.contains(needle), "{needle:?} missing in:\n{text}");
        }
        assert_eq!(text.matches("Reset group").count(), 3, "one button per group");
        // The reset button of a group puts exactly its four values back.
        {
            let n = s.project.graph.node_mut(id).unwrap();
            n.set_param("gain_r", img, 0.3);
            n.set_param("lift_b", img, 0.1);
            n.set_param("gamma", img, 0.2);
        }
        let (groups, _) = groups::param_groups(NodeKind::ColorWheels);
        let node = s.project.graph.node(id).unwrap();
        assert!(groups::group_changed(node, img, &groups[0]) && groups::group_changed(node, img, &groups[1]) && groups::group_changed(node, img, &groups[2]));
        let swatch = groups::tint_rgb([node.param("gain_r", img), node.param("gain_g", img), node.param("gain_b", img)], 2.0);
        assert!(swatch[0] > swatch[1] && swatch[0] > swatch[2], "gain_r +0.3 tints towards red: {swatch:?}");
    }

    #[test]
    fn the_clarity_inspector_names_its_two_sliders_and_the_cpu_note() {
        let (mut s, _) = session_with(NodeKind::Clarity);
        let text = inspector_text(&mut s);
        for needle in ["Clarity / Texture", "Clarity", "Texture", CLARITY_NOTE] {
            assert!(text.contains(needle), "{needle:?} missing in:\n{text}");
        }
        assert_eq!(CLARITY_NOTE, "Clarity and Texture are calculated on the CPU; nodes after them still use the GPU.");
    }

    #[test]
    fn noise_reduction_shows_the_iso_factor_only_while_iso_adaptive_is_on() {
        let (mut s, id) = session_with(NodeKind::NoiseReduction);
        let img = s.current_image();
        let off = inspector_text(&mut s);
        assert!(off.contains("ISO adaptive") && !off.contains("strength x"), "{off}");
        s.project.graph.node_mut(id).unwrap().set_param("iso_adaptive", img, 1.0);
        let known = s.current_entry().and_then(|e| e.capture.iso);
        let on = inspector_text(&mut s);
        assert!(on.contains(&groups::iso_scale_line(known)), "{on}");
        // A project image without ISO: "ISO unknown -> x 1.00".
        let dir = tempfile::tempdir().unwrap();
        let project = crate::session::tests::png_project(dir.path(), 1);
        let mut s = Session::new(project, crate::session::tests::provider());
        let id = s.project.graph.add_node(NodeKind::NoiseReduction, [0.0, 0.0]);
        s.selected_node = Some(id);
        let img = s.current_image();
        s.project.graph.node_mut(id).unwrap().set_param("iso_adaptive", img, 1.0);
        let on = inspector_text(&mut s);
        assert!(on.contains("ISO unknown -> x 1.00"), "{on}");
    }

    #[test]
    fn the_crop_inspector_gives_the_preview_and_the_full_size_through_all_upstream_crops() {
        let mut g = rawgraph_core::Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let first = g.add_node(NodeKind::Transform, [0.0, 0.0]);
        let second = g.add_node(NodeKind::Transform, [0.0, 0.0]);
        g.connect(src, first, 0).unwrap();
        g.connect(first, second, 0).unwrap();
        let img = ImageId(1);
        {
            let n = g.node_mut(first).unwrap();
            n.set_param("left", img, 0.1);
            n.set_param("right", img, 0.9);
        }
        {
            let n = g.node_mut(second).unwrap();
            n.set_param("top", img, 0.25);
            n.set_param("bottom", img, 0.75);
        }
        // Full: 5632 x 3755 -> crop 80 % of the width -> crop half of the height.
        let sizes = result_sizes(&g, second, img, Some((2560, 1707)), Some((5632, 3755)));
        let lines = size_lines(&sizes);
        assert_eq!(lines[1], "full size: 4506 x 1877 px", "{lines:?}");
        assert!(lines[0].starts_with("Resulting size: 2048 x 853 px (preview; source 2560 x 1707 px)"), "{lines:?}");
        // The first crop alone only sees the file itself.
        let sizes = result_sizes(&g, first, img, None, Some((5632, 3755)));
        assert_eq!(size_lines(&sizes)[1], "full size: 4506 x 3755 px");
        assert!(size_lines(&sizes)[0].contains("once the image is loaded"));
        let none = size_lines(&ResultSizes::default());
        assert_eq!(none[1], "full size: shown once the image is loaded");
    }

    #[test]
    fn the_crop_inspector_paints_both_size_lines() {
        let (mut s, id) = session_with(NodeKind::Transform);
        let src = s.node_by_index(0).unwrap();
        s.project.graph.connect(src, id, 0).unwrap();
        // Decode the proxy so that both sizes are known (as the render worker does).
        use rawgraph_engine::ImageProvider as _;
        s.provider.image(s.current_image()).unwrap();
        let text = inspector_text(&mut s);
        assert!(text.contains("Resulting size:") && text.contains("full size:"), "{text}");
        assert!(!text.contains("full size: shown once"), "the full size of the decoded demo image is known: {text}");
    }
}
