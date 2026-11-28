//! The inspector sections and node-box texts of the collection and output nodes of wave 8:
//! Source (folder), Merge (same-name rule), Layer, Lens Correction (Auto) and Export. The rules
//! (texts, which controls are disabled and why) are plain functions so they can be tested
//! without a window; the `*_section` functions only draw them.

#[cfg(test)]
use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use rawgraph_core::{
    export_spec, ExportFileFormat, ExportSpec, ExportText, Graph, ImageId, LensProfile, Mode, Node, NodeData, NodeId, NodeKind, OutputSpace, ParamKind,
    LAYER_BLEND_MODES, LAYER_FILTERS, LAYER_FITS, MAX_LAYERS, OUTPUT_SPACES,
};

use crate::collection::{with_thousands as th, LayerStats, MergeStats};
use crate::session::{Session, UiRequest};

const WARN: Color32 = Color32::from_rgb(255, 190, 90);
const ERROR: Color32 = Color32::from_rgb(255, 150, 120);

// ------------------------------------------------------------------ collection-level values

/// The node as if it were in All mode: collection-level settings (a Source's folder, a Merge's
/// rule, a Layer's mode) are always read from the All values, whatever the mode of the node.
pub fn all_view(node: &Node) -> Node {
    let mut n = node.clone();
    n.mode = Mode::All;
    n
}

/// Writes a parameter of the All values whatever the mode of the node is.
pub fn set_all_param(graph: &mut Graph, id: NodeId, name: &str, value: f32) -> bool {
    let Some(n) = graph.node_mut(id) else { return false };
    let mode = n.mode;
    n.mode = Mode::All;
    let ok = n.set_param(name, ImageId(0), value);
    n.mode = mode;
    ok
}

/// The folder a Source restricts its images to, or `None` when unset.
pub fn source_folder(node: &Node) -> Option<String> {
    match all_view(node).data(ImageId(0)) {
        Some(NodeData::Folder(f)) if !f.is_empty() => Some(f.clone()),
        _ => None,
    }
}

/// Whether the Source includes the sub-folders of its folder.
pub fn source_subfolders(node: &Node) -> bool {
    all_view(node).toggle("subfolders", ImageId(0))
}

/// Sets (or clears) the folder of a Source, in the All values.
pub fn set_source_folder(graph: &mut Graph, id: NodeId, folder: Option<String>) -> bool {
    let Some(n) = graph.node_mut(id).filter(|n| n.kind == NodeKind::Source) else { return false };
    let mode = n.mode;
    n.mode = Mode::All;
    let ok = n.set_data(ImageId(0), Some(NodeData::Folder(folder.unwrap_or_default())));
    n.mode = mode;
    ok
}

/// Last component of a folder path ("D:\Photos\2026\Wedding" -> "Wedding").
pub fn folder_name(folder: &str) -> String {
    let trimmed = folder.trim_end_matches(['/', '\\']);
    trimmed.rsplit(['/', '\\']).next().filter(|s| !s.is_empty()).unwrap_or(trimmed).to_owned()
}

/// What a Source shows in its box and in the inspector heading: the folder name or "All images".
pub fn source_label(node: &Node) -> String {
    source_folder(node).map_or_else(|| if matches!(all_view(node).data(ImageId(0)), Some(NodeData::Folder(_))) { "No folder".to_owned() } else { "All images".to_owned() }, |f| folder_name(&f))
}

/// The folder of every Source node with its sub-folder switch, for noticing changes.
pub fn source_states(graph: &Graph) -> Vec<(NodeId, String, bool)> {
    graph.nodes().filter(|n| n.kind == NodeKind::Source).map(|n| (n.id, source_folder(n).unwrap_or_default(), source_subfolders(n))).collect()
}

/// The folders to scan because a Source was pointed at them (or got "Include subfolders" switched
/// on) between `before` and `now`: new or changed folder states that `covered` says the project
/// does not hold yet. A Source that is unchanged, has no folder, or whose folder is covered never
/// scans; nothing scans on load because the baseline is taken then.
pub fn scans_needed(before: &[(NodeId, String, bool)], now: &[(NodeId, String, bool)], covered: &dyn Fn(&str, bool) -> bool) -> Vec<(std::path::PathBuf, bool)> {
    let mut out: Vec<(std::path::PathBuf, bool)> = Vec::new();
    for (id, folder, sub) in now {
        let changed = before.iter().find(|(i, _, _)| i == id).is_none_or(|(_, f, s)| f != folder || s != sub);
        if changed && !folder.is_empty() && !covered(folder, *sub) {
            let item = (std::path::PathBuf::from(folder), *sub);
            if !out.contains(&item) {
                out.push(item);
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------------ merge

/// One line that says what the "Same file name" option does.
pub fn collision_note(option: usize) -> &'static str {
    match option {
        1 => "Of images with the same file name, only those of the lowest-numbered input that has that name stay.",
        2 => "Of images with the same file name, only those of the highest-numbered input that has that name stay.",
        _ => "Images with the same file name from different inputs all stay.",
    }
}

/// "3 same-named images dropped", `None` when the rule dropped nothing.
pub fn dropped_text(dropped: usize) -> Option<String> {
    (dropped > 0).then(|| format!("{} same-named image{} dropped", th(dropped), if dropped == 1 { "" } else { "s" }))
}

// ------------------------------------------------------------------------------------ preview

/// The first node upstream of `node` (itself included) whose work is not pointwise, so the CPU
/// makes the base picture even when the last nodes and the display run on the GPU: "layer",
/// "lens correction", "blur", ... `None` when only pointwise nodes lie between the Source and `node`.
pub fn cpu_stage(graph: &Graph, node: NodeId) -> Option<&'static str> {
    let mut cur = node;
    for _ in 0..512 {
        let n = graph.node(cur)?;
        let label = match n.kind {
            NodeKind::Layer => Some("layer"),
            NodeKind::LensManual => Some("lens correction"),
            NodeKind::Blur => Some("blur"),
            NodeKind::NoiseReduction => Some("noise reduction"),
            NodeKind::Sharpen => Some("sharpen"),
            NodeKind::Clarity => Some("clarity"),
            NodeKind::Transform => Some("crop / rotate"),
            _ => None,
        };
        if label.is_some() {
            return label;
        }
        cur = n.kind.inputs().iter().enumerate().filter(|(_, s)| s.ty == rawgraph_core::PortType::Images).find_map(|(p, _)| graph.input_of(cur, p))?;
    }
    None
}

// ------------------------------------------------------------------------------------ layer

/// The connected layers of a Layer node (0-based), bottom layer first.
pub fn connected_layers(graph: &Graph, id: NodeId) -> Vec<usize> {
    (0..MAX_LAYERS).filter(|p| graph.input_of(id, *p).is_some()).collect()
}

/// Box summary of a Layer node: "3 layers, Match".
pub fn layer_summary(graph: &Graph, node: &Node) -> String {
    let n = connected_layers(graph, node.id).len();
    let mode = if all_view(node).choice("mode", ImageId(0)) == 1 { "All combinations" } else { "Match" };
    format!("{n} layer{}, {mode}", if n == 1 { "" } else { "s" })
}

/// "Layer 2 drives the collection (4 images)".
pub fn layer_driver_line(stats: &LayerStats) -> String {
    match stats.driver {
        Some(d) => {
            let n = stats.output_len();
            format!("Layer {} drives the collection ({} image{})", d + 1, th(n), if n == 1 { "" } else { "s" })
        }
        None => "No layer has images: connect a Source or another collection to a layer input".to_owned(),
    }
}

/// "Layer 2: IMG_0007.dng" for every connected layer, for the selected image. `name` gives the
/// file name of an image id. `None` when the image is not in the Layer's output.
pub fn layer_pairing_lines(stats: &LayerStats, image: ImageId, name: &dyn Fn(ImageId) -> String) -> Option<Vec<String>> {
    let sources = stats.sources(image)?;
    Some(
        stats
            .lists
            .iter()
            .enumerate()
            .filter(|(_, l)| l.is_some())
            .map(|(i, _)| match sources.get(i).copied().flatten() {
                Some(src) => format!("Layer {}: {}", i + 1, name(src)),
                None => format!("Layer {}: no image", i + 1),
            })
            .collect(),
    )
}

/// Which controls of a layer row are greyed out, and why (the bottom layer defines the canvas).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerRowState {
    pub blend_enabled: bool,
    pub fit_enabled: bool,
    pub filter_enabled: bool,
}

pub const BOTTOM_LAYER_HINT: &str = "The bottom layer defines the canvas: its size is the size of the result, and nothing lies below it to blend with.";

pub fn layer_row_state(is_bottom: bool) -> LayerRowState {
    LayerRowState { blend_enabled: !is_bottom, fit_enabled: !is_bottom, filter_enabled: !is_bottom }
}

pub const ALL_COMBINATIONS_HINT: &str = "Not available yet \u{2014} Match is used";

// ------------------------------------------------------------------------------------- lens

/// The status line of a Lens Correction node in Auto mode for the selected image. `profile` is
/// what the file's lens data read as (`None`: not read yet).
pub fn lens_status(auto_usable: bool, profile: Option<&Option<LensProfile>>) -> String {
    if !auto_usable {
        return "Auto needs the uncropped frame: place this node before Crop / Rotate".to_owned();
    }
    match profile {
        None => "Reading the lens data of this file...".to_owned(),
        Some(None) => "No lens data in this file (only DNG files with embedded corrections have it)".to_owned(),
        Some(Some(p)) => {
            let mut parts: Vec<&str> = Vec::new();
            if let Some(w) = &p.warp {
                parts.push("distortion");
                if w.planes.len() >= 3 {
                    parts.push("fringe correction");
                }
            }
            if p.vignette.is_some() {
                parts.push("vignette");
            }
            if parts.is_empty() {
                "No lens data in this file (only DNG files with embedded corrections have it)".to_owned()
            } else {
                format!("Lens data in file: {}", parts.join(", "))
            }
        }
    }
}

// ----------------------------------------------------------------------------------- export

/// Why a combination of the Export node cannot work, if it cannot.
pub fn export_conflict(format: ExportFileFormat, space: OutputSpace) -> Option<&'static str> {
    (format == ExportFileFormat::Avif && space == OutputSpace::AdobeRgb).then_some("AVIF cannot carry Adobe RGB (1998): choose another colour space or another format")
}

/// Whether the quality setting applies to `format`.
pub fn quality_applies(format: ExportFileFormat) -> bool {
    matches!(format, ExportFileFormat::Jpeg | ExportFileFormat::Avif | ExportFileFormat::Webp | ExportFileFormat::Heic)
}

/// Whether the colour space (and the embedded profile) apply to `format`.
pub fn colour_space_applies(format: ExportFileFormat) -> bool {
    !matches!(format, ExportFileFormat::Exr | ExportFileFormat::Png16Data)
}

pub const EXR_SPACE_NOTE: &str = "EXR files are linear ACEScg: the colour space and the profile do not apply.";
pub const NAME_TEMPLATE_HELP: &str = "{name} source name, {index} / {index:04} position, {id} image id";

/// The two rows of the box of an Export node: format and quality, then the folder.
pub fn export_box_rows(node: &Node, image: ImageId) -> Vec<(String, String)> {
    let spec = export_spec(node, image);
    let format = if quality_applies(spec.format) { format!("{} {}", spec.format.label(), spec.quality) } else { spec.format.label().to_owned() };
    let folder = if spec.text.out_dir.trim().is_empty() { "not set".to_owned() } else { folder_name(&spec.text.out_dir) };
    vec![("Format".to_owned(), format), ("Folder".to_owned(), folder)]
}

/// A one-line description of an Export node's settings for the node-mode export dialog.
pub fn export_lines(spec: &ExportSpec) -> Vec<(String, String)> {
    use rawgraph_core::{ExportCollision, ExportSize};
    let mut v = vec![
        ("Format".to_owned(), if quality_applies(spec.format) { format!("{}, quality {}", spec.format.label(), spec.quality) } else { spec.format.label().to_owned() }),
        (
            "Size".to_owned(),
            match spec.size {
                ExportSize::Original => "original".to_owned(),
                ExportSize::LongEdge(n) => format!("long edge {n} px"),
                ExportSize::Percent(p) => format!("{p:.0} %"),
            },
        ),
        ("Colour space".to_owned(), if colour_space_applies(spec.format) { format!("{}{}", spec.space.label(), if spec.embed_profile { ", profile embedded" } else { "" }) } else { "linear (EXR)".to_owned() }),
        (
            "If the file exists".to_owned(),
            match spec.collision {
                ExportCollision::Skip => "skip",
                ExportCollision::Overwrite => "overwrite",
                ExportCollision::Rename => "rename",
            }
            .to_owned(),
        ),
    ];
    let m = spec.meta;
    let on: Vec<&str> = [(m.camera, "camera"), (m.exposure, "exposure"), (m.date, "date"), (m.gps, "GPS"), (m.author, "author"), (m.software, "software")]
        .into_iter()
        .filter(|(b, _)| *b)
        .map(|(_, n)| n)
        .collect();
    v.push(("Metadata".to_owned(), if on.is_empty() { "none".to_owned() } else { on.join(", ") }));
    v.push(("Folder".to_owned(), if spec.text.out_dir.is_empty() { "not set".to_owned() } else { spec.text.out_dir.clone() }));
    v.push(("File name".to_owned(), spec.text.name_template.clone()));
    v
}

/// Names of the text fields of an Export node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportField {
    OutDir,
    Template,
    Artist,
    Copyright,
    Description,
}

impl ExportField {
    #[cfg(test)]
    pub const ALL: [ExportField; 5] = [ExportField::OutDir, ExportField::Template, ExportField::Artist, ExportField::Copyright, ExportField::Description];

    pub fn parse(name: &str) -> Option<ExportField> {
        Some(match name.trim().to_lowercase().as_str() {
            "dir" | "folder" | "out_dir" | "outdir" | "output" => ExportField::OutDir,
            "name" | "template" | "name_template" | "file" => ExportField::Template,
            "artist" | "author" => ExportField::Artist,
            "copyright" => ExportField::Copyright,
            "description" | "desc" => ExportField::Description,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            ExportField::OutDir => "Output folder",
            ExportField::Template => "File name",
            ExportField::Artist => "Artist",
            ExportField::Copyright => "Copyright",
            ExportField::Description => "Description",
        }
    }

    fn slot(self, t: &mut ExportText) -> &mut String {
        match self {
            ExportField::OutDir => &mut t.out_dir,
            ExportField::Template => &mut t.name_template,
            ExportField::Artist => &mut t.artist,
            ExportField::Copyright => &mut t.copyright,
            ExportField::Description => &mut t.description,
        }
    }

    pub fn get(self, t: &ExportText) -> &str {
        match self {
            ExportField::OutDir => &t.out_dir,
            ExportField::Template => &t.name_template,
            ExportField::Artist => &t.artist,
            ExportField::Copyright => &t.copyright,
            ExportField::Description => &t.description,
        }
    }
}

/// Writes one text field of an Export node for `image` (for every image in All mode). Refused
/// text (too long, containing NUL) leaves the node alone and says so.
pub fn set_export_text(graph: &mut Graph, id: NodeId, image: ImageId, field: ExportField, value: &str) -> Result<(), String> {
    let Some(n) = graph.node_mut(id).filter(|n| n.kind == NodeKind::Export) else { return Err("The node is not an Export node".to_owned()) };
    let mut text = export_spec(n, image).text;
    *field.slot(&mut text) = value.to_owned();
    if n.set_data(image, Some(NodeData::Export(text))) {
        Ok(())
    } else {
        Err(format!("{} was refused: at most 1024 characters, no control characters", field.label()))
    }
}

// ---------------------------------------------------------------------------------- drawing

fn changed_color(changed: bool, text: &str) -> RichText {
    let t = RichText::new(text).strong();
    if changed {
        t.color(Color32::from_rgb(255, 196, 120))
    } else {
        t
    }
}

/// A combo box over `options` where some options are disabled with a reason. Returns the new
/// index when the user picked another one.
fn combo(ui: &mut egui::Ui, salt: impl std::hash::Hash + std::fmt::Debug, current: usize, options: &[&str], disabled: &dyn Fn(usize) -> Option<String>, enabled: bool) -> Option<usize> {
    let mut picked = None;
    ui.add_enabled_ui(enabled, |ui| {
        egui::ComboBox::from_id_salt(salt)
            .selected_text(options.get(current).copied().unwrap_or(""))
            .width((ui.available_width() - 8.0).max(80.0))
            .show_ui(ui, |ui| {
                for (i, o) in options.iter().enumerate() {
                    match disabled(i) {
                        Some(why) => {
                            ui.add_enabled(false, egui::Button::selectable(i == current, *o)).on_disabled_hover_text(why.as_str());
                        }
                        None => {
                            if ui.selectable_label(i == current, *o).clicked() && i != current {
                                picked = Some(i);
                            }
                        }
                    }
                }
            });
    });
    picked
}

fn slider(ui: &mut egui::Ui, value: &mut f32, min: f32, max: f32, enabled: bool) -> egui::Response {
    ui.spacing_mut().slider_width = (ui.available_width() - 76.0).max(80.0);
    ui.add_enabled(enabled, egui::Slider::new(value, min..=max).clamping(egui::SliderClamping::Always).max_decimals(3))
}

// ------------------------------------------------------------------------------- Source UI

pub fn source_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let Some(node) = session.project.graph.node(id) else { return };
    let folder = source_folder(node);
    let legacy_catalog = !matches!(all_view(node).data(ImageId(0)), Some(NodeData::Folder(_)));
    let subfolders = source_subfolders(node);
    let count = session.count_for(node);
    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new("Folder").strong());
        match &folder {
            Some(f) => {
                ui.label(crate::export::elide_middle(f, 44)).on_hover_text(f.as_str());
            }
            None => {
                ui.label(if legacy_catalog { "All project images (legacy)" } else { "No folder selected" });
            }
        }
        ui.horizontal(|ui| {
            if ui.button("Choose folder\u{2026}").on_hover_text("Show only the images of one folder. A folder that is not part of the project yet is scanned into it.").clicked() {
                session.ui_requests.push(UiRequest::PickSourceFolder(id));
            }
            if ui.add_enabled(folder.is_some(), egui::Button::new("Clear")).on_hover_text("Clear the folder and show no images").clicked() {
                set_source_folder(&mut session.project.graph, id, None);
            }
        });
        let mut on = subfolders;
        if ui.checkbox(&mut on, "Include subfolders").changed() {
            set_all_param(&mut session.project.graph, id, "subfolders", if on { 1.0 } else { 0.0 });
        }
        match count {
            Some(n) => {
                ui.label(RichText::new(format!("{} image{}", th(n), if n == 1 { "" } else { "s" })).color(Color32::from_rgb(130, 190, 175)));
                if n == 0 && folder.is_some() && !session.scanning {
                    ui.label(RichText::new("No project image lies in this folder.").color(WARN));
                }
            }
            None => {
                ui.label(RichText::new("Counting images...").weak());
            }
        }
    });
}

// -------------------------------------------------------------------------------- Merge UI

pub fn merge_rule_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId) {
    let Some(node) = session.project.graph.node(id) else { return };
    let current = all_view(node).choice("collision", ImageId(0));
    let Some(spec) = NodeKind::Merge.param_spec("collision") else { return };
    let ParamKind::Choice(options) = spec.kind else { return };
    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new(spec.label).strong());
        if let Some(i) = combo(ui, (id.0, "collision"), current, options, &|_| None, true) {
            set_all_param(&mut session.project.graph, id, "collision", i as f32);
        }
        ui.label(RichText::new(collision_note(current)).weak());
        if let Some(text) = session.collection.merges.get(&id).and_then(|s: &MergeStats| dropped_text(s.dropped)) {
            ui.label(RichText::new(text).color(WARN));
        }
    });
}

// -------------------------------------------------------------------------------- Layer UI

pub fn layer_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId) {
    let Some(node) = session.project.graph.node(id) else { return };
    let mode = all_view(node).choice("mode", ImageId(0));
    let connected = connected_layers(&session.project.graph, id);
    let stats = session.collection.layers.get(&id).cloned();
    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.label(RichText::new("Layer mode").strong());
        ui.horizontal(|ui| {
            if ui.add(egui::Button::selectable(mode == 0, "Match")).on_hover_text("Layer j shows its k-th image for the k-th image of the layer with the most images").clicked() {
                set_all_param(&mut session.project.graph, id, "mode", 0.0);
            }
            ui.add_enabled(false, egui::Button::selectable(mode == 1, "All combinations")).on_disabled_hover_text(ALL_COMBINATIONS_HINT);
        });
        if mode == 1 {
            ui.label(RichText::new(ALL_COMBINATIONS_HINT).color(WARN));
        }
        ui.add_space(2.0);
        match &stats {
            Some(s) => {
                ui.label(layer_driver_line(s));
                let name = |img: ImageId| session.entry(img).map(crate::testimages::display_name).unwrap_or_else(|| format!("image {}", img.0));
                match layer_pairing_lines(s, image, &name) {
                    Some(lines) => {
                        for l in lines {
                            ui.label(RichText::new(l).monospace());
                        }
                    }
                    None => {
                        if session.selected.is_some() && s.driver.is_some() {
                            ui.label(RichText::new("The selected image is not in this Layer's output.").color(WARN));
                        }
                    }
                }
            }
            None => {
                ui.label(RichText::new("Counting images...").weak());
            }
        }
    });
    if connected.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("Connect collections to the Layer inputs. Layer 1 is the bottom layer.").color(WARN));
        return;
    }
    for (pos, layer) in connected.iter().copied().enumerate() {
        layer_row(ui, session, id, image, layer, pos == 0);
    }
}

fn layer_row(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId, layer: usize, bottom: bool) {
    let n = layer + 1;
    let state = layer_row_state(bottom);
    let Some(node) = session.project.graph.node(id) else { return };
    let settings = rawgraph_core::layer_settings(node, layer, image);
    let changed = (0..5).any(|i| {
        let name = ["on", "blend", "opacity", "fit", "filter"][i];
        crate::groups::is_changed(node, image, &format!("l{n}_{name}"))
    });
    let mut on = settings.on;
    let mut opacity = settings.opacity;
    let (mut blend, mut fit, mut filter) = (settings.blend as usize, settings.fit as usize, settings.filter as usize);
    let (b0, f0, l0) = (blend, fit, filter);
    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.checkbox(&mut on, "");
            ui.label(changed_color(changed, &if bottom { format!("Layer {n} (bottom)") } else { format!("Layer {n}") }));
        });
        ui.add_enabled_ui(on, |ui| {
            ui.label("Blend mode");
            if let Some(i) = combo(ui, (id.0, n, "blend"), blend, &LAYER_BLEND_MODES, &|_| None, state.blend_enabled) {
                blend = i;
            }
            ui.label("Opacity");
            slider(ui, &mut opacity, 0.0, 1.0, true);
            ui.label("Fit");
            if let Some(i) = combo(ui, (id.0, n, "fit"), fit, &LAYER_FITS, &|_| None, state.fit_enabled) {
                fit = i;
            }
            ui.label("Filter");
            if let Some(i) = combo(ui, (id.0, n, "filter"), filter, &LAYER_FILTERS, &|_| None, state.filter_enabled) {
                filter = i;
            }
        });
        if bottom {
            ui.label(RichText::new(BOTTOM_LAYER_HINT).weak().small());
        }
        if changed && crate::icons::button(ui, crate::icons::Icon::Reset, "Reset layer", true).clicked() {
            if let Some(nd) = session.project.graph.node_mut(id) {
                for name in ["on", "blend", "opacity", "fit", "filter"] {
                    let key = format!("l{n}_{name}");
                    if let Some(spec) = NodeKind::Layer.param_spec(&key) {
                        nd.set_param(&key, image, spec.default);
                    }
                }
            }
        }
    });
    if let Some(nd) = session.project.graph.node_mut(id) {
        if on != settings.on {
            nd.set_param(&format!("l{n}_on"), image, if on { 1.0 } else { 0.0 });
        }
        if (opacity - settings.opacity).abs() > f32::EPSILON {
            nd.set_param(&format!("l{n}_opacity"), image, opacity);
        }
        if blend != b0 {
            nd.set_param(&format!("l{n}_blend"), image, blend as f32);
        }
        if fit != f0 {
            nd.set_param(&format!("l{n}_fit"), image, fit as f32);
        }
        if filter != l0 {
            nd.set_param(&format!("l{n}_filter"), image, filter as f32);
        }
    }
}

// ---------------------------------------------------------------------------------- Lens UI

/// The Auto status line and the heading above the manual sliders.
pub fn lens_status_ui(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId) {
    let Some(node) = session.project.graph.node(id) else { return };
    let auto = node.choice("mode", image) == 1;
    if !auto {
        ui.label(RichText::new("Manual: only the sliders below correct the picture.").weak());
        return;
    }
    let usable = session.project.graph.lens_auto_usable(id);
    let profile = session.provider.lens_known(image);
    if usable && profile.is_none() && session.selected.is_some() {
        session.provider.request_lens(ui.ctx(), image);
    }
    let text = lens_status(usable, profile.as_ref());
    let has_data = profile.as_ref().is_some_and(Option::is_some);
    ui.label(RichText::new(text).color(if usable && has_data { Color32::from_rgb(130, 190, 175) } else { WARN }));
    ui.label(RichText::new("Fine-tuning: the sliders below are applied on top of the lens data.").weak());
}

// -------------------------------------------------------------------------------- Export UI

/// Per-frame edit buffers of the text fields live in egui memory; a field is written to the node
/// when it loses focus (one undo step), not on every key.
pub(crate) fn text_field(ui: &mut egui::Ui, key: egui::Id, current: &str, width: f32, hint: &str) -> Option<String> {
    let mut buf: String = ui.data(|d| d.get_temp(key)).unwrap_or_else(|| current.to_owned());
    let focused = ui.memory(|m| m.has_focus(key));
    if !focused && buf != current {
        buf = current.to_owned();
    }
    // A text that does not fit and is not being edited shows its END (the last folders of a
    // path matter most); the full text is in the tooltip.
    let font = egui::TextStyle::Body.resolve(ui.style());
    let measure = |s: &str| ui.painter().layout_no_wrap(s.to_owned(), font.clone(), egui::Color32::WHITE).size().x;
    let mut shown = if focused { buf.clone() } else { crate::textfit::fit_end(&buf, (width - 14.0).max(8.0), &measure) };
    let resp = ui.add(egui::TextEdit::singleline(&mut shown).id(key).desired_width(width).hint_text(hint));
    if focused {
        buf = shown;
    } else if resp.gained_focus() {
        // The click landed in the shortened text; start at the end of the real one.
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), key) {
            let end = buf.chars().count();
            state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
            state.store(ui.ctx(), key);
        }
    }
    let resp = if buf.is_empty() { resp } else { resp.on_hover_text(buf.as_str()) };
    let mut out = None;
    if resp.lost_focus() && buf != current {
        out = Some(buf.clone());
    }
    ui.data_mut(|d| d.insert_temp(key, buf));
    out
}

// The stored format indices remain unchanged for existing projects. PNG/TIFF depth
// is presented independently, but mapped back to the corresponding legacy index.
const PHOTO_FORMATS: &[&str] = &["JPEG", "PNG", "TIFF", "EXR", "AVIF", "Adobe XMP sidecar", "PNG 16-bit data map", "WebP", "HEIC"];
const FORMAT_INDICES: [usize; 9] = [0, 1, 3, 5, 6, 7, 8, 9, 10];

fn export_format_control(format: ExportFileFormat) -> (usize, usize) {
    match format {
        ExportFileFormat::Jpeg => (0, 0),
        ExportFileFormat::Png => (1, 0),
        ExportFileFormat::Png16 => (1, 1),
        ExportFileFormat::Tiff => (2, 0),
        ExportFileFormat::Tiff16 => (2, 1),
        ExportFileFormat::Exr => (3, 0),
        ExportFileFormat::Avif => (4, 0),
        ExportFileFormat::Xmp => (5, 0),
        ExportFileFormat::Png16Data => (6, 0),
        ExportFileFormat::Webp => (7, 0),
        ExportFileFormat::Heic => (8, 0),
    }
}

fn export_format_index(format: usize, depth: usize) -> usize {
    match (format, depth) {
        (1, 1) => 2,
        (2, 1) => 4,
        _ => FORMAT_INDICES.get(format).copied().unwrap_or(0),
    }
}

pub fn export_section(ui: &mut egui::Ui, session: &mut Session, id: NodeId, image: ImageId) {
    let Some(node) = session.project.graph.node(id).cloned() else { return };
    let spec = export_spec(&node, image);
    let mut message: Option<String> = None;
    let busy = session.export_busy;
    let mut new_values: Vec<(&'static str, f32)> = Vec::new();
    let label_w = 112.0;
    let ctrl_w = (ui.available_width() - label_w - 34.0).max(120.0);

    ui.add_space(4.0);
    ui.group(|ui| {
        ui.set_min_width(ui.available_width());
        egui::Grid::new(("export-grid", id.0)).num_columns(2).spacing([10.0, 5.0]).min_col_width(label_w).show(ui, |ui| {
            let (fmt_i, depth_i) = export_format_control(spec.format);
            let avif_blocked = spec.space == OutputSpace::AdobeRgb;
            // HEIC works only where the operating system can encode it (checked on a worker thread).
            let heic = crate::export::heic_status();
            if heic.is_none() {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
            let heic_why: Option<String> = match &heic {
                Some(Err(why)) => Some(why.clone()),
                None => Some("Checking whether this computer can write HEIC\u{2026}".to_owned()),
                Some(Ok(())) => None,
            };
            let why_not_avif: &dyn Fn(usize) -> Option<String> = &|i| match i {
                4 if avif_blocked => Some("AVIF cannot carry Adobe RGB (1998); pick another colour space first".to_owned()),
                8 if spec.format != ExportFileFormat::Heic => heic_why.clone(),
                _ => None,
            };
            ui.label("Format");
            ui.vertical(|ui| {
                ui.set_width(ctrl_w);
                if let Some(i) = combo(ui, (id.0, "format"), fmt_i, PHOTO_FORMATS, why_not_avif, true) {
                    new_values.push(("format", export_format_index(i, depth_i) as f32));
                }
                if let (ExportFileFormat::Heic, Some(why)) = (spec.format, &heic_why) {
                    ui.label(RichText::new(why.as_str()).color(ERROR));
                }
                if spec.format == ExportFileFormat::Heic && matches!(heic, Some(Err(_))) && ui.small_button("Check HEIC again").on_hover_text("Look again whether the system HEIC encoder is installed").clicked() {
                    crate::export::heic_recheck();
                }
            });
            ui.end_row();

            if matches!(fmt_i, 1 | 2) {
                ui.label("Bit depth").on_hover_text("Bits per channel");
                ui.vertical(|ui| {
                    ui.set_width(ctrl_w);
                    if let Some(depth) = combo(ui, (id.0, "bit_depth"), depth_i, &["8-bit", "16-bit"], &|_| None, true) {
                        new_values.push(("format", export_format_index(fmt_i, depth) as f32));
                    }
                });
                ui.end_row();
            }

            let q_ok = quality_applies(spec.format) && !(spec.format == ExportFileFormat::Webp && spec.webp_lossless);
            ui.label("Quality").on_hover_text("Applies to JPEG, AVIF, HEIC and lossy WebP");
            ui.vertical(|ui| {
                ui.set_width(ctrl_w);
                let mut q = f32::from(spec.quality);
                if slider(ui, &mut q, 1.0, 100.0, q_ok).changed() {
                    new_values.push(("quality", q.round()));
                }
            });
            ui.end_row();

            if spec.format == ExportFileFormat::Webp {
                ui.label("Compression");
                let mut lossless = spec.webp_lossless;
                if ui.checkbox(&mut lossless, "Lossless").changed() {
                    new_values.push(("webp_lossless", if lossless { 1.0 } else { 0.0 }));
                }
                ui.end_row();
            }

            let size_i = node.choice("size", image);
            ui.label("Size");
            ui.vertical(|ui| {
                ui.set_width(ctrl_w);
                if let Some(i) = combo(ui, (id.0, "size"), size_i, &rawgraph_core::EXPORT_SIZES, &|_| None, true) {
                    new_values.push(("size", i as f32));
                }
            });
            ui.end_row();
            match size_i {
                1 => {
                    ui.label("Long edge");
                    let mut v = node.param("long_edge", image);
                    let r = ui.add(egui::DragValue::new(&mut v).range(16.0..=32000.0).speed(8.0).max_decimals(0).suffix(" px")).on_hover_text("Never enlarges the picture");
                    if r.changed() {
                        new_values.push(("long_edge", v.round()));
                    }
                    ui.end_row();
                }
                2 => {
                    ui.label("Percent");
                    let mut v = node.param("percent", image);
                    ui.vertical(|ui| {
                        ui.set_width(ctrl_w);
                        if slider(ui, &mut v, 1.0, 100.0, true).changed() {
                            new_values.push(("percent", v.round()));
                        }
                    });
                    ui.end_row();
                }
                _ => {}
            }

            let space_ok = colour_space_applies(spec.format);
            let space_i = OUTPUT_SPACES.iter().position(|s| *s == spec.space.label()).unwrap_or(0);
            let avif = spec.format == ExportFileFormat::Avif;
            let why_not_adobe: &dyn Fn(usize) -> Option<String> = &|i| (avif && OutputSpace::from_index(i) == OutputSpace::AdobeRgb).then(|| "AVIF cannot carry Adobe RGB (1998)".to_owned());
            ui.label("Colour space");
            ui.vertical(|ui| {
                ui.set_width(ctrl_w);
                if let Some(i) = combo(ui, (id.0, "space"), space_i, &OUTPUT_SPACES, why_not_adobe, space_ok) {
                    new_values.push(("color_space", i as f32));
                }
            });
            ui.end_row();
            ui.label("");
            ui.vertical(|ui| {
                let mut embed = spec.embed_profile;
                if ui.add_enabled(space_ok, egui::Checkbox::new(&mut embed, "Embed colour profile")).changed() {
                    new_values.push(("profile", if embed { 1.0 } else { 0.0 }));
                }
                if spec.format == ExportFileFormat::Png16Data {
                    ui.label(RichText::new("Raw RGB data: no colour transform, profile or metadata; use original size for normal maps.").weak().small());
                } else if !space_ok {
                    ui.label(RichText::new(EXR_SPACE_NOTE).weak().small());
                }
                if let Some(why) = export_conflict(spec.format, spec.space) {
                    ui.label(RichText::new(why).color(ERROR));
                }
            });
            ui.end_row();

            ui.label("Output folder");
            ui.horizontal(|ui| {
                let key = ui.make_persistent_id(("export-field", id.0, 0u8));
                if let Some(v) = text_field(ui, key, &spec.text.out_dir, (ctrl_w - 80.0).max(60.0), "not set") {
                    if let Err(e) = set_export_text(&mut session.project.graph, id, image, ExportField::OutDir, &v) {
                        message = Some(e);
                    }
                }
                if ui.button("Choose\u{2026}").clicked() {
                    session.ui_requests.push(UiRequest::PickExportFolder(id));
                }
            });
            ui.end_row();

            ui.label("If file exists");
            ui.vertical(|ui| {
                ui.set_width(ctrl_w);
                if let Some(i) = combo(ui, (id.0, "collision"), node.choice("collision", image), &rawgraph_core::EXPORT_COLLISIONS, &|_| None, true) {
                    new_values.push(("collision", i as f32));
                }
            });
            ui.end_row();

            ui.label("File name").on_hover_text(NAME_TEMPLATE_HELP);
            let key = ui.make_persistent_id(("export-field", id.0, 1u8));
            if let Some(v) = text_field(ui, key, &spec.text.name_template, ctrl_w, "{name}") {
                if let Err(e) = set_export_text(&mut session.project.graph, id, image, ExportField::Template, &v) {
                    message = Some(e);
                }
            }
            ui.end_row();

            ui.label("Metadata");
            egui::Grid::new(("export-meta", id.0)).num_columns(2).spacing([14.0, 2.0]).show(ui, |ui| {
                let switches: [(&'static str, &str, bool); 6] = [
                    ("meta_camera", "Camera data", spec.meta.camera),
                    ("meta_exposure", "Exposure data", spec.meta.exposure),
                    ("meta_date", "Date", spec.meta.date),
                    ("meta_gps", "GPS location", spec.meta.gps),
                    ("meta_author", "Author", spec.meta.author),
                    ("meta_software", "Software", spec.meta.software),
                ];
                for (i, (name, label, value)) in switches.into_iter().enumerate() {
                    let mut on = value;
                    if ui.checkbox(&mut on, label).changed() {
                        new_values.push((name, if on { 1.0 } else { 0.0 }));
                    }
                    if i % 2 == 1 {
                        ui.end_row();
                    }
                }
            });
            ui.end_row();

            for (field, hint) in [(ExportField::Artist, "keep the artist of the file"), (ExportField::Copyright, "keep the copyright of the file"), (ExportField::Description, "none")] {
                ui.label(field.label()).on_hover_text("Artist and copyright are only written while the Author switch is on");
                let key = ui.make_persistent_id(("export-field", id.0, field as u8 + 2));
                let current = field.get(&spec.text).to_owned();
                if let Some(v) = text_field(ui, key, &current, ctrl_w, hint) {
                    if let Err(e) = set_export_text(&mut session.project.graph, id, image, field, &v) {
                        message = Some(e);
                    }
                }
                ui.end_row();
            }
        });
    });

    if spec.format == ExportFileFormat::Xmp {
        ui.label(RichText::new("XMP: proprietary RAW only; rating and colour label only. No edits or pixels are exported. Existing sidecars are always skipped. Place sidecars beside matching RAWs for Adobe.").color(ERROR));
    }
    ui.add_space(6.0);
    let dir_set = !spec.text.out_dir.trim().is_empty();
    let can_run = dir_set && !busy && export_conflict(spec.format, spec.space).is_none() && !session.demo;
    let tip = if session.demo {
        "The demo images cannot be exported"
    } else if !dir_set {
        "Choose an output folder first"
    } else if busy {
        "An export is already running"
    } else {
        export_conflict(spec.format, spec.space).unwrap_or("Exports the images of this node's collection (selected image, filtered list or all)")
    };
    if ui.add_enabled(can_run, egui::Button::new("Export\u{2026}")).on_hover_text(tip).on_disabled_hover_text(tip).clicked() {
        session.ui_requests.push(UiRequest::OpenNodeExport(id));
    }
    if let Some(m) = message {
        ui.label(RichText::new(m).color(ERROR));
    }
    if let Some(n) = session.project.graph.node_mut(id) {
        for (name, v) in new_values {
            n.set_param(name, image, v);
        }
    }
}

/// An `Arc` of a layer's images for tests that build `LayerStats` by hand.
#[cfg(test)]
pub fn layer_list(ids: &[u64]) -> Option<Arc<Vec<ImageId>>> {
    Some(Arc::new(ids.iter().map(|i| ImageId(*i)).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::History;
    use crate::session::tests::{demo_session, png_project, provider};
    use rawgraph_core::{VignetteRadial, WarpRectilinear};

    const IMG: ImageId = ImageId(1);

    fn warp(planes: usize) -> WarpRectilinear {
        WarpRectilinear { planes: vec![[1.0, 0.0, 0.0, 0.0, 0.0, 0.0]; planes], center: [0.5, 0.5] }
    }

    #[test]
    fn folder_names_and_source_labels() {
        assert_eq!(folder_name(r"D:\Photos\2026\Wedding"), "Wedding");
        assert_eq!(folder_name("D:/Photos/2026/Wedding/"), "Wedding");
        assert_eq!(folder_name("D:\\"), "D:");
        let mut g = Graph::new();
        let s = g.add_node(NodeKind::Source, [0.0, 0.0]);
        assert_eq!(source_label(g.node(s).unwrap()), "All images");
        assert!(set_source_folder(&mut g, s, Some(r"D:\Photos\Wedding".to_owned())));
        assert_eq!(source_label(g.node(s).unwrap()), "Wedding");
        assert_eq!(source_folder(g.node(s).unwrap()).as_deref(), Some(r"D:\Photos\Wedding"));
        assert!(set_source_folder(&mut g, s, None), "Clear");
        assert_eq!(source_folder(g.node(s).unwrap()), None);
        assert_eq!(source_label(g.node(s).unwrap()), "No folder");
        assert!(!set_source_folder(&mut g, NodeId(99), Some("x".into())));
    }

    #[test]
    fn collection_level_settings_are_written_to_the_all_values_in_individual_mode_too() {
        let mut g = Graph::new();
        let s = g.add_node(NodeKind::Source, [0.0, 0.0]);
        g.node_mut(s).unwrap().mode = Mode::Individual;
        assert!(set_source_folder(&mut g, s, Some("D:/x".into())));
        assert!(set_all_param(&mut g, s, "subfolders", 0.0));
        let n = g.node(s).unwrap();
        assert_eq!(n.mode, Mode::Individual, "the mode is left alone");
        assert_eq!(source_folder(n).as_deref(), Some("D:/x"));
        assert!(!source_subfolders(n));
        // The core reads exactly these All values.
        let caps = |_: ImageId| Some(rawgraph_core::CaptureInfo { path: Some("D:/x/sub/a.dng".into()), ..Default::default() });
        assert!(!g.contains(s, IMG, &caps), "no sub-folders: the file in D:/x/sub is outside");
        assert!(set_all_param(&mut g, s, "subfolders", 1.0));
        assert!(g.contains(s, IMG, &caps));
    }

    #[test]
    fn source_folder_choice_scans_only_when_the_project_does_not_cover_it_and_undo_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])).save(dir.path().join("top.png")).unwrap();
        image::RgbaImage::from_pixel(8, 8, image::Rgba([4, 5, 6, 255])).save(sub.join("deep.png")).unwrap();
        let other = tempfile::tempdir().unwrap();
        // The project was scanned without sub-folders.
        let mut project = rawgraph_project::Project::new();
        project.scan_folder(dir.path(), false, &rawgraph_io::is_supported, &|p| rawgraph_io::read_capture_info(p).ok());
        let mut s = Session::new(project, provider());
        let src = s.project.graph.add_node(NodeKind::Source, [0.0, 0.0]);
        let top = dir.path().to_string_lossy().into_owned();
        assert!(s.folder_covered(&top, false), "the scanned folder itself, without sub-folders");
        assert!(!s.folder_covered(&top, true), "its sub-folders were not scanned");
        assert!(!s.folder_covered(&sub.to_string_lossy(), false), "a sub-folder of a non-recursive scan");
        assert!(!s.folder_covered(&other.path().to_string_lossy(), false), "an unknown folder");

        let mut history = History::new(&s.project.graph);
        let before = source_states(&s.project.graph);
        // The sub-folder switch is on by default: choosing the scanned folder needs the sub-folders.
        assert!(set_all_param(&mut s.project.graph, src, "subfolders", 0.0));
        let base = source_states(&s.project.graph);
        assert!(set_source_folder(&mut s.project.graph, src, Some(top.clone())));
        let now = source_states(&s.project.graph);
        assert!(scans_needed(&base, &now, &|f, sub| s.folder_covered(f, sub)).is_empty(), "the folder is in the project already: no scan");
        // Switching "Include subfolders" on later scans the sub-folders.
        assert!(set_all_param(&mut s.project.graph, src, "subfolders", 1.0));
        let after = source_states(&s.project.graph);
        assert_eq!(scans_needed(&now, &after, &|f, sub| s.folder_covered(f, sub)), vec![(std::path::PathBuf::from(&top), true)]);
        // Same state again: nothing new.
        assert!(scans_needed(&after, &after, &|f, sub| s.folder_covered(f, sub)).is_empty());
        // A folder that is not in the project: scanned, recursively as the checkbox says.
        let new_folder = other.path().to_string_lossy().into_owned();
        assert!(set_source_folder(&mut s.project.graph, src, Some(new_folder.clone())));
        let moved = source_states(&s.project.graph);
        assert_eq!(scans_needed(&after, &moved, &|f, sub| s.folder_covered(f, sub)), vec![(std::path::PathBuf::from(&new_folder), true)]);
        // Folder and checkbox are undoable.
        assert!(history.commit_if_changed(&s.project.graph, false));
        s.project.graph = history.undo().expect("undo");
        assert_eq!(source_states(&s.project.graph), before, "undo goes back to the state before the changes");
        let redo = history.redo().expect("redo");
        assert_eq!(source_folder(redo.node(src).unwrap()).as_deref(), Some(new_folder.as_str()));
        assert!(source_subfolders(redo.node(src).unwrap()));
        // A cleared folder never scans.
        let mut cleared = redo.clone();
        set_source_folder(&mut cleared, src, None);
        assert!(scans_needed(&moved, &source_states(&cleared), &|_, _| false).is_empty());
    }

    #[test]
    fn merge_texts() {
        assert!(collision_note(0).contains("all stay"));
        assert!(collision_note(1).contains("lowest-numbered"));
        assert!(collision_note(2).contains("highest-numbered"));
        assert_eq!(collision_note(9), collision_note(0));
        assert_eq!(dropped_text(0), None);
        assert_eq!(dropped_text(1).as_deref(), Some("1 same-named image dropped"));
        assert_eq!(dropped_text(1204).as_deref(), Some("1,204 same-named images dropped"));
    }

    #[test]
    fn layer_inspector_texts() {
        let mut g = Graph::new();
        let a = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let b = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        g.connect(a, layer, 1).unwrap();
        g.connect(b, layer, 3).unwrap();
        assert_eq!(connected_layers(&g, layer), vec![1, 3], "only connected layers get rows, bottom first");
        assert_eq!(layer_summary(&g, g.node(layer).unwrap()), "2 layers, Match");
        g.node_mut(layer).unwrap().set_param("mode", IMG, 1.0);
        assert_eq!(layer_summary(&g, g.node(layer).unwrap()), "2 layers, All combinations");
        let empty = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        assert_eq!(layer_summary(&g, g.node(empty).unwrap()), "0 layers, Match");
        g.connect(a, empty, 0).unwrap();
        assert_eq!(layer_summary(&g, g.node(empty).unwrap()), "1 layer, Match");

        let stats = LayerStats { driver: Some(1), lists: vec![None, layer_list(&[10, 11, 12]), None, layer_list(&[20, 21])] };
        assert_eq!(layer_driver_line(&stats), "Layer 2 drives the collection (3 images)");
        let name = |i: ImageId| format!("IMG_{:04}.dng", i.0);
        assert_eq!(layer_pairing_lines(&stats, ImageId(10), &name).unwrap(), vec!["Layer 2: IMG_0010.dng", "Layer 4: IMG_0020.dng"]);
        assert_eq!(layer_pairing_lines(&stats, ImageId(12), &name).unwrap(), vec!["Layer 2: IMG_0012.dng", "Layer 4: IMG_0020.dng"], "the shorter layer repeats from its start");
        assert_eq!(layer_pairing_lines(&stats, ImageId(99), &name), None, "an image outside the output has no pairing");
        let none = LayerStats { driver: None, lists: vec![None; 8] };
        assert!(layer_driver_line(&none).starts_with("No layer has images"));
        let single = LayerStats { driver: Some(0), lists: vec![layer_list(&[7])] };
        assert_eq!(layer_driver_line(&single), "Layer 1 drives the collection (1 image)");
        // Layer rows: the bottom layer's blend, fit and filter are greyed out.
        assert_eq!(layer_row_state(true), LayerRowState { blend_enabled: false, fit_enabled: false, filter_enabled: false });
        assert_eq!(layer_row_state(false), LayerRowState { blend_enabled: true, fit_enabled: true, filter_enabled: true });
        assert!(BOTTOM_LAYER_HINT.contains("bottom layer defines the canvas"));
        assert_eq!(ALL_COMBINATIONS_HINT, "Not available yet \u{2014} Match is used");
    }

    /// Runs the inspector section of a node for two frames (it must not panic, whatever the graph).
    fn frame(session: &mut Session, f: impl FnOnce(&mut egui::Ui, &mut Session)) {
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(420.0, 900.0))), ..Default::default() };
        let mut f = Some(f);
        for _ in 0..2 {
            let out = ctx.run_ui(input.clone(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    if let Some(f) = f.take() {
                        f(ui, session);
                    }
                });
            });
            out.drop_without_applying_deltas();
        }
    }

    #[test]
    fn every_new_section_draws_without_panicking() {
        let mut s = demo_session();
        let g = &mut s.project.graph;
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let merge = g.add_node(NodeKind::Merge, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
        let lens = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
        g.connect(src, merge, 0).unwrap();
        g.connect(src, layer, 0).unwrap();
        g.connect(src, layer, 2).unwrap();
        g.connect(src, export, 0).unwrap();
        g.connect(src, lens, 0).unwrap();
        let image = s.current_image();
        frame(&mut s, |ui, s| source_section(ui, s, src));
        frame(&mut s, |ui, s| merge_rule_section(ui, s, merge));
        frame(&mut s, |ui, s| layer_section(ui, s, layer, image));
        frame(&mut s, |ui, s| export_section(ui, s, export, image));
        s.project.graph.node_mut(lens).unwrap().set_param("mode", image, 1.0);
        frame(&mut s, |ui, s| lens_status_ui(ui, s, lens, image));
        assert!(s.ui_requests.is_empty(), "nothing was clicked");
        // A node that is not there draws nothing.
        frame(&mut s, |ui, s| {
            source_section(ui, s, NodeId(9999));
            layer_section(ui, s, NodeId(9999), image);
            export_section(ui, s, NodeId(9999), image);
        });
    }

    #[test]
    fn lens_status_lines_for_the_three_cases() {
        let none: Option<LensProfile> = None;
        assert_eq!(lens_status(true, Some(&none)), "No lens data in this file (only DNG files with embedded corrections have it)");
        assert_eq!(lens_status(false, Some(&none)), "Auto needs the uncropped frame: place this node before Crop / Rotate", "the placement problem comes first");
        let full = LensProfile { warp: Some(warp(3)), vignette: None };
        assert_eq!(lens_status(true, Some(&Some(full))), "Lens data in file: distortion, fringe correction");
        let with_vignette = LensProfile { warp: Some(warp(3)), vignette: Some(VignetteRadial { k: [0.0; 5], center: [0.5, 0.5] }) };
        assert_eq!(lens_status(true, Some(&Some(with_vignette))), "Lens data in file: distortion, fringe correction, vignette");
        let plain = LensProfile { warp: Some(warp(1)), vignette: None };
        assert_eq!(lens_status(true, Some(&Some(plain))), "Lens data in file: distortion");
        let only_vignette = LensProfile { warp: None, vignette: Some(VignetteRadial { k: [0.0; 5], center: [0.5, 0.5] }) };
        assert_eq!(lens_status(true, Some(&Some(only_vignette))), "Lens data in file: vignette");
        assert_eq!(lens_status(true, Some(&Some(LensProfile::default()))), "No lens data in this file (only DNG files with embedded corrections have it)");
        assert!(lens_status(true, None).contains("Reading"));
    }

    #[test]
    fn the_lens_status_follows_the_node_position_in_the_graph() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let crop = g.add_node(NodeKind::Transform, [0.0, 0.0]);
        let lens = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
        g.connect(src, crop, 0).unwrap();
        g.connect(crop, lens, 0).unwrap();
        g.node_mut(lens).unwrap().set_param("mode", IMG, 1.0);
        let none: Option<LensProfile> = None;
        assert!(lens_status(g.lens_auto_usable(lens), Some(&none)).starts_with("Auto needs the uncropped frame"));
        assert!(g.validate().iter().any(|i| i.node == lens && i.message.contains("Auto needs the uncropped frame")), "the issue shows where issues show");
        g.disconnect(lens, 0); // Connecting another image wire now adds a branch.
        g.connect(src, lens, 0).unwrap();
        assert!(lens_status(g.lens_auto_usable(lens), Some(&none)).starts_with("No lens data"));
    }

    #[test]
    fn export_combinations_that_cannot_work_say_why() {
        assert!(export_conflict(ExportFileFormat::Avif, OutputSpace::AdobeRgb).is_some_and(|m| m.contains("AVIF") && m.contains("Adobe RGB")));
        assert_eq!(export_conflict(ExportFileFormat::Avif, OutputSpace::DisplayP3), None);
        assert_eq!(export_conflict(ExportFileFormat::Jpeg, OutputSpace::AdobeRgb), None);
        for f in ExportFileFormat::ALL {
            assert_eq!(quality_applies(f), matches!(f, ExportFileFormat::Jpeg | ExportFileFormat::Avif | ExportFileFormat::Webp | ExportFileFormat::Heic), "{f:?}");
            assert_eq!(colour_space_applies(f), !matches!(f, ExportFileFormat::Exr | ExportFileFormat::Png16Data), "{f:?}");
        }
        assert!(EXR_SPACE_NOTE.contains("EXR"));
    }

    #[test]
    fn export_format_depth_controls_preserve_legacy_indices() {
        for (index, format) in ExportFileFormat::ALL.iter().copied().enumerate() {
            let (kind, depth) = export_format_control(format);
            assert_eq!(export_format_index(kind, depth), index);
        }
        assert_eq!(export_format_index(1, 0), 1);
        assert_eq!(export_format_index(1, 1), 2);
        assert_eq!(export_format_index(2, 0), 3);
        assert_eq!(export_format_index(2, 1), 4);
        assert_eq!(export_format_index(PHOTO_FORMATS.len(), 0), 0);
    }

    #[test]
    fn every_export_field_round_trips_into_the_export_spec() {
        use rawgraph_core::{ExportCollision, ExportSize};
        let mut g = Graph::new();
        let e = g.add_node(NodeKind::Export, [0.0, 0.0]);
        for (name, v) in [
            ("format", 6.0),
            ("quality", 77.0),
            ("size", 1.0),
            ("long_edge", 1600.0),
            ("color_space", 3.0),
            ("profile", 0.0),
            ("collision", 2.0),
            ("meta_camera", 0.0),
            ("meta_exposure", 0.0),
            ("meta_date", 0.0),
            ("meta_gps", 1.0),
            ("meta_author", 0.0),
            ("meta_software", 0.0),
        ] {
            assert!(g.node_mut(e).unwrap().set_param(name, IMG, v), "{name}");
        }
        for (field, value) in [
            (ExportField::OutDir, "D:/out"),
            (ExportField::Template, "{name}_web"),
            (ExportField::Artist, "A. Photographer"),
            (ExportField::Copyright, "(c) 2026"),
            (ExportField::Description, "Wave 8"),
        ] {
            set_export_text(&mut g, e, IMG, field, value).unwrap();
        }
        let spec = export_spec(g.node(e).unwrap(), IMG);
        assert_eq!(spec.format, ExportFileFormat::Avif);
        assert_eq!(spec.quality, 77);
        assert_eq!(spec.size, ExportSize::LongEdge(1600));
        assert_eq!(spec.space, OutputSpace::Rec2020);
        assert!(!spec.embed_profile);
        assert_eq!(spec.collision, ExportCollision::Rename);
        assert!(spec.meta.gps && !spec.meta.camera && !spec.meta.exposure && !spec.meta.date && !spec.meta.author && !spec.meta.software);
        assert_eq!(
            (spec.text.out_dir.as_str(), spec.text.name_template.as_str(), spec.text.artist.as_str(), spec.text.copyright.as_str(), spec.text.description.as_str()),
            ("D:/out", "{name}_web", "A. Photographer", "(c) 2026", "Wave 8")
        );
        for f in ExportField::ALL {
            assert!(!f.get(&spec.text).is_empty(), "{f:?}");
        }
        assert_eq!(export_box_rows(g.node(e).unwrap(), IMG), vec![("Format".to_owned(), "AVIF 77".to_owned()), ("Folder".to_owned(), "out".to_owned())]);
        let lines = export_lines(&spec);
        assert!(lines.iter().any(|(k, v)| k == "Colour space" && v.contains("Rec. 2020") && !v.contains("profile embedded")));
        assert!(lines.iter().any(|(k, v)| k == "Metadata" && v == "GPS"));
        // Too long text is refused with a message and leaves the node alone.
        let long = "x".repeat(2000);
        assert!(set_export_text(&mut g, e, IMG, ExportField::Artist, &long).is_err());
        assert_eq!(export_spec(g.node(e).unwrap(), IMG).text.artist, "A. Photographer");
        assert!(set_export_text(&mut g, NodeId(99), IMG, ExportField::Artist, "x").is_err());
        assert_eq!(ExportField::parse("Folder"), Some(ExportField::OutDir));
        assert_eq!(ExportField::parse("desc"), Some(ExportField::Description));
        assert_eq!(ExportField::parse("nope"), None);
    }

    #[test]
    fn issues_of_the_new_nodes_are_reported() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        g.connect(src, export, 0).unwrap();
        let issues: Vec<String> = g.validate().into_iter().map(|i| i.message).collect();
        assert!(issues.iter().any(|m| m == "Export: no output folder set"), "{issues:?}");
        assert!(issues.iter().any(|m| m == "Layer: no input is connected"), "{issues:?}");
        set_export_text(&mut g, export, IMG, ExportField::OutDir, "D:/out").unwrap();
        g.connect(src, layer, 0).unwrap();
        assert!(g.validate().is_empty());
        g.node_mut(layer).unwrap().set_param("mode", IMG, 1.0);
        assert_eq!(g.validate()[0].message, "All combinations is not available yet; Match is used");
    }

    #[test]
    fn the_box_texts_of_source_layer_and_export() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        let export = g.add_node(NodeKind::Export, [0.0, 0.0]);
        g.connect(src, layer, 0).unwrap();
        let rows = |g: &Graph, id| crate::graph_editor::box_rows(g, g.node(id).unwrap(), IMG);
        assert_eq!(rows(&g, src), vec![("Folder".to_owned(), "All images".to_owned())]);
        set_source_folder(&mut g, src, Some(r"D:\Photos\A very long folder name that does not fit".to_owned()));
        let r = rows(&g, src);
        assert!(r[0].1.chars().count() <= 20 && r[0].1.contains('\u{2026}'), "{r:?}");
        assert_eq!(rows(&g, layer), vec![(String::new(), "1 layer, Match".to_owned())]);
        assert_eq!(rows(&g, export), vec![("Format".to_owned(), "JPEG 92".to_owned()), ("Folder".to_owned(), "not set".to_owned())]);
        // The box reserves a row for everything it draws.
        let plain = crate::graph_editor::node_size(NodeKind::Exposure).y - crate::graph_editor::node_size(NodeKind::Output).y;
        assert!(plain > 0.0);
        assert!(crate::graph_editor::node_size(NodeKind::Export).y > crate::graph_editor::node_size(NodeKind::Output).y);
    }

    #[test]
    fn the_cpu_stage_names_the_first_non_pointwise_node() {
        let mut g = Graph::new();
        let src = g.add_node(NodeKind::Source, [0.0, 0.0]);
        let exp = g.add_node(NodeKind::Exposure, [0.0, 0.0]);
        let layer = g.add_node(NodeKind::Layer, [0.0, 0.0]);
        let sat = g.add_node(NodeKind::Saturation, [0.0, 0.0]);
        g.connect(src, exp, 0).unwrap();
        g.connect(exp, layer, 0).unwrap();
        g.connect(layer, sat, 0).unwrap();
        assert_eq!(cpu_stage(&g, exp), None);
        assert_eq!(cpu_stage(&g, layer), Some("layer"));
        assert_eq!(cpu_stage(&g, sat), Some("layer"));
        let lens = g.add_node(NodeKind::LensManual, [0.0, 0.0]);
        g.connect(exp, lens, 0).unwrap();
        assert_eq!(cpu_stage(&g, lens), Some("lens correction"));
    }

    #[test]
    fn collection_captures_carry_the_source_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 3), provider());
        let (ids, caps) = s.id_captures();
        assert_eq!(ids.len(), 3);
        for id in ids.iter() {
            let p = caps[id].path.as_deref().expect("a path");
            assert!(rawgraph_core::path_in_folder(p, &dir.path().to_string_lossy(), false), "{p}");
        }
    }
}

