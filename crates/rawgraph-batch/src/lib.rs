//! RawGraph batch export: the UI-independent pipeline that renders one node of a graph for a list
//! of images and writes one file per image.
//!
//! Per item: evaluate the graph under the item's own [`ImageId`] at full resolution,
//! optionally shrink (area average, linear light), convert for output in the chosen colour space
//! and write atomically, with the matching profile and the selected capture metadata of the source.
//! Items run strictly one after another and are reported in input order; a failure in one item
//! never stops the others. Cancellation is cooperative and leaves no partial files.
//!
//! [`export_node`] drives a whole Export node: the settings of every image come from the node
//! (`export_spec`, so Individual values and different folders per image work) and the graph is
//! evaluated against the full catalog of the project, so Source folders, Merge name collisions and
//! Layer pairing see every image. [`export`] is the plain variant with one set of settings.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rawgraph_core::{
    export_spec, CaptureInfo, ExportCollision, ExportFileFormat, ExportSize, ExportSpec, Graph, ImageBuf, ImageId,
    LensProfile, MetaSelection, NodeId, NodeKind, OutputSpace, PortType,
};
use rawgraph_engine::{CancelToken, EvalError, Evaluator, ImageProvider};
use rawgraph_io::IoError;
use rawgraph_scopes::DisplayTransform;
use rayon::prelude::*;

pub mod resize;
mod template;

pub use resize::{resize_area, resize_to_long_edge, target_size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Jpeg { quality: u8 },
    Png,
    Tiff,
    /// 16-bit PNG in the chosen output space (display image quantised to u16, round to nearest,
    /// clamped to 0..1).
    Png16,
    /// 16-bit TIFF (same quantisation as `Png16`).
    Tiff16,
    /// Linear ACEScg float, no display transform, the output colour space is ignored.
    Exr,
    /// 8-bit AVIF in the chosen output space; `quality` is 1..=100.
    Avif { quality: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnConflict {
    Skip,
    Overwrite,
    /// Appends " (2)", " (3)", ... before the extension.
    Rename,
}

/// Construct it with `..Default::default()` so that fields added later do not break your code.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportSettings {
    pub out_dir: PathBuf,
    pub format: ExportFormat,
    /// Placeholders: `{name}` source file stem, `{index}` 1-based position in the batch,
    /// `{index:04}` zero-padded, `{id}` image id. Default `{name}`.
    pub name_template: String,
    /// Exact long edge of the output in pixels; `None` = see `percent`. Never upscales. Wins over
    /// `percent`.
    pub long_edge: Option<u32>,
    pub on_conflict: OnConflict,
    /// Applied for Jpeg/Png/Tiff/Png16/Tiff16/Avif. Ignored for Exr.
    pub display: DisplayTransform,
    /// Embed the ICC profile of `space` in Jpeg/Png/Tiff/Png16/Tiff16/Avif files. Exr is unchanged:
    /// linear ACEScg with chromaticities.
    pub embed_profile: bool,
    /// Carry the selected capture metadata of the source (see `meta`) into the exported file as
    /// EXIF (Orientation is always 1). `false` means no metadata block at all. A source without
    /// readable metadata still exports; the file then only gets Orientation and Software.
    pub copy_metadata: bool,
    /// Output colour space of the display-referred formats.
    pub space: OutputSpace,
    /// Which metadata groups are written when `copy_metadata` is on.
    pub meta: MetaSelection,
    /// Artist override; written only while the `author` group is on. `None` or empty = keep the source's.
    pub artist: Option<String>,
    /// Copyright override; written only while the `author` group is on. `None` or empty = keep the source's.
    pub copyright: Option<String>,
    /// Image description; written whenever it is set and not empty.
    pub description: Option<String>,
    /// Output size as a percentage (1..=100) of the rendered size when `long_edge` is `None`.
    /// Never upscales.
    pub percent: Option<f32>,
}

/// JPEG quality 92, `{name}`, full size, skip existing files, default display transform, sRGB,
/// profile on, metadata on (camera, exposure, date, author and software; GPS off), no overrides.
/// `out_dir` is empty and must be set.
impl Default for ExportSettings {
    fn default() -> Self {
        ExportSettings {
            out_dir: PathBuf::new(),
            format: ExportFormat::Jpeg { quality: 92 },
            name_template: "{name}".into(),
            long_edge: None,
            on_conflict: OnConflict::Skip,
            display: DisplayTransform::default(),
            embed_profile: true,
            copy_metadata: true,
            space: OutputSpace::Srgb,
            meta: MetaSelection { camera: true, exposure: true, date: true, gps: false, author: true, software: true },
            artist: None,
            copyright: None,
            description: None,
            percent: None,
        }
    }
}

/// Maps the settings of an Export node (one image) to [`ExportSettings`].
///
/// Metadata: `copy_metadata` is off when no group is selected and there is no description, so an
/// export with every metadata switch off carries no metadata block at all.
pub fn settings_from_spec(spec: &ExportSpec, display: DisplayTransform) -> ExportSettings {
    let format = match spec.format {
        ExportFileFormat::Jpeg => ExportFormat::Jpeg { quality: spec.quality },
        ExportFileFormat::Png => ExportFormat::Png,
        ExportFileFormat::Png16 => ExportFormat::Png16,
        ExportFileFormat::Tiff => ExportFormat::Tiff,
        ExportFileFormat::Tiff16 => ExportFormat::Tiff16,
        ExportFileFormat::Exr => ExportFormat::Exr,
        ExportFileFormat::Avif => ExportFormat::Avif { quality: spec.quality },
    };
    let (long_edge, percent) = match spec.size {
        ExportSize::Original => (None, None),
        ExportSize::LongEdge(n) => (Some(n), None),
        ExportSize::Percent(p) => (None, Some(p)),
    };
    let on_conflict = match spec.collision {
        ExportCollision::Skip => OnConflict::Skip,
        ExportCollision::Overwrite => OnConflict::Overwrite,
        ExportCollision::Rename => OnConflict::Rename,
    };
    let non_empty = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let m = spec.meta;
    let description = non_empty(&spec.text.description);
    ExportSettings {
        out_dir: PathBuf::from(&spec.text.out_dir),
        format,
        name_template: spec.text.name_template.clone(),
        long_edge,
        on_conflict,
        display,
        embed_profile: spec.embed_profile,
        copy_metadata: m.camera || m.exposure || m.date || m.gps || m.author || m.software || description.is_some(),
        space: spec.space,
        meta: m,
        artist: non_empty(&spec.text.artist),
        copyright: non_empty(&spec.text.copyright),
        description,
        percent,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExportItem {
    pub id: ImageId,
    pub source: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemStatus {
    Written { path: PathBuf, width: u32, height: u32 },
    SkippedExisting(PathBuf),
    Failed(String),
    Cancelled,
    /// The image is not part of the collection that reaches the export node (filtered out by the
    /// graph). Nothing was decoded or written; this is not a failure and is not retried.
    Excluded,
}

/// `current` is the source file the event concerns: the first item in the initial call
/// (`done == 0`), afterwards the item that just finished.
#[derive(Clone, Debug, PartialEq)]
pub struct ExportProgress {
    pub done: usize,
    pub total: usize,
    pub current: PathBuf,
    pub last: Option<(ImageId, ItemStatus)>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExportReport {
    pub items: Vec<(ImageId, ItemStatus)>,
}

impl ExportReport {
    fn count(&self, f: impl Fn(&ItemStatus) -> bool) -> usize {
        self.items.iter().filter(|(_, s)| f(s)).count()
    }

    pub fn written(&self) -> usize {
        self.count(|s| matches!(s, ItemStatus::Written { .. }))
    }

    pub fn skipped(&self) -> usize {
        self.count(|s| matches!(s, ItemStatus::SkippedExisting(_)))
    }

    pub fn failed(&self) -> usize {
        self.count(|s| matches!(s, ItemStatus::Failed(_)))
    }

    pub fn cancelled(&self) -> usize {
        self.count(|s| matches!(s, ItemStatus::Cancelled))
    }

    pub fn excluded(&self) -> usize {
        self.count(|s| matches!(s, ItemStatus::Excluded))
    }

    /// The items that failed or were cancelled, in batch order (excluded items are not retried).
    pub fn retry_items(&self, original: &[ExportItem]) -> Vec<ExportItem> {
        original
            .iter()
            .enumerate()
            .filter(|(i, item)| {
                let status = match self.items.get(*i) {
                    Some((id, s)) if *id == item.id => Some(s),
                    _ => self.items.iter().find(|(id, _)| *id == item.id).map(|(_, s)| s),
                };
                matches!(status, Some(ItemStatus::Failed(_) | ItemStatus::Cancelled))
            })
            .map(|(_, item)| item.clone())
            .collect()
    }
}

#[derive(Debug)]
pub enum ExportError {
    InvalidSettings(String),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::InvalidSettings(m) => write!(f, "invalid export settings: {m}"),
        }
    }
}

impl std::error::Error for ExportError {}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ExportError> {
    Err(ExportError::InvalidSettings(msg.into()))
}

fn extension(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Jpeg { .. } => "jpg",
        ExportFormat::Png | ExportFormat::Png16 => "png",
        ExportFormat::Tiff | ExportFormat::Tiff16 => "tif",
        ExportFormat::Exr => "exr",
        ExportFormat::Avif { .. } => "avif",
    }
}

fn plan(item: &ExportItem, index: usize, settings: &ExportSettings, tokens: &[template::Token]) -> Result<PathBuf, ExportError> {
    let Some(stem) = item.source.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
        return invalid(format!("source {} has no file name", item.source.display()));
    };
    let name = template::expand(tokens, &stem, index, item.id.0);
    template::check_stem(&name).map_err(|e| ExportError::InvalidSettings(format!("{e} (source {})", item.source.display())))?;
    Ok(settings.out_dir.join(format!("{name}.{}", extension(settings.format))))
}

/// The output path `export` would use for this item before conflict handling. `index` is the
/// 1-based position of the item in the batch (the value of `{index}`).
pub fn planned_path(item: &ExportItem, index: usize, settings: &ExportSettings) -> Result<PathBuf, ExportError> {
    let tokens = template::parse(&settings.name_template).map_err(ExportError::InvalidSettings)?;
    plan(item, index, settings, &tokens)
}

/// Comparable identity of a path: absolute, symlinks resolved where the file or its folder
/// exists, `\\?\` prefix removed, lower-cased on Windows.
fn norm_key(p: &Path) -> String {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let canon = fs::canonicalize(&abs)
        .ok()
        .or_else(|| {
            let parent = abs.parent()?;
            let name = abs.file_name()?;
            Some(fs::canonicalize(parent).ok()?.join(name))
        })
        .unwrap_or(abs);
    let s = canon.to_string_lossy().into_owned();
    let s = s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s);
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn abs_key(p: &Path) -> String {
    let s = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().into_owned();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

fn check_settings(settings: &ExportSettings) -> Result<Vec<template::Token>, ExportError> {
    let tokens = template::parse(&settings.name_template).map_err(ExportError::InvalidSettings)?;
    if settings.long_edge == Some(0) {
        return invalid("the long edge must be at least 1 pixel");
    }
    if let Some(p) = settings.percent {
        if !p.is_finite() || p <= 0.0 {
            return invalid(format!("the size percentage {p} must be greater than 0"));
        }
    }
    if let ExportFormat::Jpeg { quality } | ExportFormat::Avif { quality } = settings.format {
        if !(1..=100).contains(&quality) {
            return invalid(format!("quality {quality} is outside 1..=100"));
        }
    }
    Ok(tokens)
}

fn check_node(graph: &Graph, node: NodeId) -> Result<(), ExportError> {
    match graph.node(node) {
        None => invalid(format!("node {} does not exist in the graph", node.0)),
        Some(n) if !matches!(n.kind, NodeKind::Output | NodeKind::Export) && n.kind.output() != Some(PortType::Images) => {
            invalid(format!("node {} ({}) does not produce images", node.0, n.kind.label()))
        }
        Some(_) => Ok(()),
    }
}

type CaptureReader<'a> = &'a (dyn Fn(&Path) -> Option<CaptureInfo> + Sync);
type LensReader<'a> = &'a (dyn Fn(&Path) -> Option<LensProfile> + Sync);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Serves every image of the project: its capture metadata (with the source path), its lens
/// profile and, decoded on demand at full resolution, its pixels. The decoded pixels are kept only
/// until [`CatalogProvider::release_pixels`], which the batch calls after every item, so at most
/// the images one item needs are alive (one per layer for a Layer node).
pub(crate) struct CatalogProvider<'a> {
    order: Vec<ImageId>,
    paths: HashMap<ImageId, PathBuf>,
    read_capture: CaptureReader<'a>,
    read_lens: LensReader<'a>,
    captures: Mutex<HashMap<ImageId, CaptureInfo>>,
    lenses: Mutex<HashMap<ImageId, Option<LensProfile>>>,
    pixels: Mutex<HashMap<ImageId, Arc<ImageBuf>>>,
}

impl<'a> CatalogProvider<'a> {
    pub(crate) fn new(catalog: &[ExportItem], read_capture: CaptureReader<'a>, read_lens: LensReader<'a>) -> Self {
        let mut order = Vec::with_capacity(catalog.len());
        let mut paths = HashMap::with_capacity(catalog.len());
        for it in catalog {
            if let std::collections::hash_map::Entry::Vacant(v) = paths.entry(it.id) {
                order.push(it.id);
                v.insert(it.source.clone());
            }
        }
        CatalogProvider {
            order,
            paths,
            read_capture,
            read_lens,
            captures: Mutex::new(HashMap::new()),
            lenses: Mutex::new(HashMap::new()),
            pixels: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn release_pixels(&self) {
        lock(&self.pixels).clear();
    }

    #[cfg(test)]
    pub(crate) fn loaded_images(&self) -> usize {
        lock(&self.pixels).len()
    }
}

impl ImageProvider for CatalogProvider<'_> {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        if let Some(img) = lock(&self.pixels).get(&id) {
            return Ok(img.clone());
        }
        let Some(path) = self.paths.get(&id) else {
            return Err(format!("image {} is not part of the project", id.0));
        };
        #[cfg(test)]
        tests::DECODES.with(|c| c.set(c.get() + 1));
        let loaded = rawgraph_io::load(path, None).map_err(|e| format!("decoding {}: {e}", path.display()))?;
        let img = Arc::new(loaded.image);
        lock(&self.pixels).insert(id, img.clone());
        Ok(img)
    }

    fn capture(&self, id: ImageId) -> Option<CaptureInfo> {
        let path = self.paths.get(&id)?;
        if let Some(c) = lock(&self.captures).get(&id) {
            return Some(c.clone());
        }
        // Unreadable metadata means "unknown": an empty record, the graph decides (per the
        // filter's Missing option).
        let mut info = (self.read_capture)(path).unwrap_or_default();
        info.path = Some(path.to_string_lossy().into_owned());
        lock(&self.captures).insert(id, info.clone());
        Some(info)
    }

    fn images(&self) -> Vec<ImageId> {
        self.order.clone()
    }

    fn full_size(&self, id: ImageId) -> Option<(u32, u32)> {
        let img = self.image(id).ok()?;
        Some((img.width(), img.height()))
    }

    fn lens_profile(&self, id: ImageId) -> Option<LensProfile> {
        let path = self.paths.get(&id)?;
        if let Some(p) = lock(&self.lenses).get(&id) {
            return p.clone();
        }
        let profile = (self.read_lens)(path);
        lock(&self.lenses).insert(id, profile.clone());
        profile
    }
}

enum Output {
    Rgb8 { width: u32, height: u32, rgb: Vec<u8> },
    Rgb16 { width: u32, height: u32, rgb: Vec<u16> },
    Float(ImageBuf),
}

impl Output {
    fn size(&self) -> (u32, u32) {
        match self {
            Output::Rgb8 { width, height, .. } | Output::Rgb16 { width, height, .. } => (*width, *height),
            Output::Float(img) => (img.width(), img.height()),
        }
    }

    fn write(&self, path: &Path, settings: &ExportSettings, overwrite: bool, metadata: &Option<rawgraph_io::ExportMetadata>) -> Result<(), IoError> {
        let quality = match settings.format {
            ExportFormat::Jpeg { quality } | ExportFormat::Avif { quality } => quality,
            _ => 100,
        };
        let options = rawgraph_io::SaveOptions {
            jpeg_quality: quality,
            overwrite,
            embed_profile: settings.embed_profile,
            space: settings.space,
            metadata: metadata.clone(),
        };
        match self {
            Output::Float(img) => rawgraph_io::save_exr(path, img, overwrite),
            Output::Rgb8 { width, height, rgb } => match settings.format {
                ExportFormat::Avif { .. } => rawgraph_io::save_avif8(path, *width, *height, rgb, &options),
                _ => rawgraph_io::save_srgb8_with(path, *width, *height, rgb, &options),
            },
            Output::Rgb16 { width, height, rgb } => rawgraph_io::save_srgb16_with(path, *width, *height, rgb, &options),
        }
    }
}

/// 16-bit RGB: clamp 0..1 (NaN -> 0), round(x * 65535).
fn to_rgb16(display: &rawgraph_scopes::DisplayImage) -> Vec<u16> {
    let mut out = vec![0u16; display.rgb.len()];
    out.par_chunks_mut(3 * 4096).zip(display.rgb.par_chunks(3 * 4096)).for_each(|(dst, src)| {
        for (d, s) in dst.iter_mut().zip(src) {
            let v = if s.is_nan() { 0.0 } else { s.clamp(0.0, 1.0) };
            *d = (v * 65535.0).round() as u16;
        }
    });
    out
}

/// 8-bit RGB with the same rounding as `DisplayImage::to_rgba8` (clamp 0..1, round(x * 255)).
fn to_rgb8(display: &rawgraph_scopes::DisplayImage) -> Vec<u8> {
    let mut out = vec![0u8; display.rgb.len()];
    out.par_chunks_mut(3 * 4096).zip(display.rgb.par_chunks(3 * 4096)).for_each(|(dst, src)| {
        for (d, s) in dst.iter_mut().zip(src) {
            let v = if s.is_nan() { 0.0 } else { s.clamp(0.0, 1.0) };
            *d = (v * 255.0).round() as u8;
        }
    });
    out
}

fn with_suffix(planned: &Path, n: usize) -> PathBuf {
    let stem = planned.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = planned.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    planned.with_file_name(format!("{stem} ({n}).{ext}"))
}

/// The long edge the output is shrunk to: `long_edge` wins, else `percent` of the rendered long
/// edge (at least 1 pixel), else full size. Never larger than the rendered image.
fn effective_long_edge(settings: &ExportSettings, width: u32, height: u32) -> Option<u32> {
    if let Some(le) = settings.long_edge {
        return Some(le);
    }
    let p = settings.percent?;
    if !p.is_finite() || p >= 100.0 {
        return None;
    }
    let long = width.max(height) as f64;
    Some(((long * p as f64 / 100.0).round() as u32).max(1))
}

/// The metadata block of one export: the source's, narrowed to the selected groups, plus the
/// overrides. `None` when metadata is switched off.
fn build_metadata(source: &Path, settings: &ExportSettings) -> Option<rawgraph_io::ExportMetadata> {
    if !settings.copy_metadata {
        return None;
    }
    // Reading metadata never fails an item: an unreadable source gives an (almost) empty block.
    let mut meta = rawgraph_io::read_export_metadata(source).select(&settings.meta);
    let set = |v: &Option<String>| v.as_ref().filter(|s| !s.is_empty()).cloned();
    if settings.meta.author {
        if let Some(a) = set(&settings.artist) {
            meta.artist = Some(a);
        }
        if let Some(c) = set(&settings.copyright) {
            meta.copyright = Some(c);
        }
    }
    if let Some(d) = set(&settings.description) {
        meta.description = Some(d);
    }
    Some(meta)
}

/// One image of a batch with its own settings.
struct Job<'a> {
    item: ExportItem,
    settings: &'a ExportSettings,
    /// Set when the image cannot be exported at all (for example it is not in the catalog).
    pre_failure: Option<String>,
}

struct Ctx<'a> {
    graph: &'a Graph,
    node: NodeId,
    cancel: &'a CancelToken,
    sources: HashSet<String>,
    /// The graph's structural problem, if any: fails every item that is not excluded.
    problem: Option<String>,
}

/// First structural problem (missing required connection) among the nodes `node` depends on.
fn graph_problem(graph: &Graph, node: NodeId) -> Option<String> {
    let mut upstream = HashSet::from([node]);
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        for e in graph.edges().iter().filter(|e| e.to == n) {
            if upstream.insert(e.from) {
                stack.push(e.from);
            }
        }
    }
    graph
        .validate()
        .into_iter()
        // Notes that only say a fallback is used do not stop an export.
        .filter(|i| !i.message.contains("not available yet"))
        .find(|i| upstream.contains(&i.node))
        .map(|i| i.message)
}

/// Where one item of a run goes, decided up front for the whole run.
#[derive(Clone, Debug, Default)]
struct Plan {
    /// The name the template gives (before conflict handling).
    base: PathBuf,
    /// The path to write first (for Rename: already free on disk and unique within the run).
    target: PathBuf,
    /// First suffix number `target` corresponds to (1 = `base` itself).
    start_n: usize,
    /// Set when an earlier item of this run owns the path and this item is to be skipped.
    duplicate_of: Option<PathBuf>,
}

fn run_item(ctx: &Ctx, ev: &mut Evaluator, provider: &CatalogProvider, job: &Job, plan: &Plan, reserved: &HashSet<String>) -> ItemStatus {
    let planned: &Path = &plan.base;
    let settings = job.settings;
    let item = &job.item;
    if ctx.cancel.is_cancelled() {
        return ItemStatus::Cancelled;
    }
    if let Some(m) = &job.pre_failure {
        return ItemStatus::Failed(m.clone());
    }
    if ctx.sources.contains(&norm_key(planned)) {
        return ItemStatus::Failed(format!(
            "refusing to write {}: it is the source file of an image of this project",
            planned.display()
        ));
    }
    if let Some(problem) = &ctx.problem {
        return ItemStatus::Failed(format!("the graph cannot be evaluated: {problem}"));
    }
    if let Some(first) = &plan.duplicate_of {
        return ItemStatus::SkippedExisting(first.clone());
    }
    if settings.on_conflict == OnConflict::Skip && planned.exists() {
        return ItemStatus::SkippedExisting(planned.to_path_buf());
    }

    let evaluated = ev.eval_image(ctx.graph, ctx.node, item.id, provider, ctx.cancel);
    ev.clear();
    provider.release_pixels();
    let image = match evaluated {
        Ok(img) => img,
        Err(EvalError::Cancelled) => return ItemStatus::Cancelled,
        Err(EvalError::NotInCollection { .. }) => return ItemStatus::Excluded,
        Err(e) => return ItemStatus::Failed(format!("evaluating the graph: {e}")),
    };

    let resized = match effective_long_edge(settings, image.width(), image.height()) {
        Some(le) => match resize_to_long_edge(&image, le) {
            std::borrow::Cow::Borrowed(_) => None,
            std::borrow::Cow::Owned(small) => Some(small),
        },
        None => None,
    };
    let output = match settings.format {
        ExportFormat::Exr => Output::Float(match resized {
            Some(small) => small,
            None => (*image).clone(),
        }),
        ExportFormat::Png16 | ExportFormat::Tiff16 => {
            let display = settings.display.apply_in_space(resized.as_ref().unwrap_or(&image), settings.space);
            Output::Rgb16 { width: display.width, height: display.height, rgb: to_rgb16(&display) }
        }
        _ => {
            let display = settings.display.apply_in_space(resized.as_ref().unwrap_or(&image), settings.space);
            Output::Rgb8 { width: display.width, height: display.height, rgb: to_rgb8(&display) }
        }
    };
    drop(image);
    let metadata = build_metadata(&item.source, settings);

    if ctx.cancel.is_cancelled() {
        return ItemStatus::Cancelled;
    }
    let (width, height) = output.size();
    let overwrite = settings.on_conflict == OnConflict::Overwrite;
    let write_err = |path: &Path, e: IoError| ItemStatus::Failed(format!("writing {}: {e}", path.display()));
    match settings.on_conflict {
        OnConflict::Skip | OnConflict::Overwrite => match output.write(planned, settings, overwrite, &metadata) {
            Ok(()) => ItemStatus::Written { path: planned.to_path_buf(), width, height },
            Err(IoError::Exists(p)) => ItemStatus::SkippedExisting(p),
            Err(e) => write_err(planned, e),
        },
        OnConflict::Rename => {
            for n in plan.start_n.max(1)..=100_000usize {
                let candidate = if n == 1 { planned.to_path_buf() } else { with_suffix(planned, n) };
                let is_target = n == plan.start_n.max(1);
                if candidate.exists() || ctx.sources.contains(&norm_key(&candidate)) || (!is_target && reserved.contains(&abs_key(&candidate))) {
                    continue;
                }
                match output.write(&candidate, settings, false, &metadata) {
                    Ok(()) => return ItemStatus::Written { path: candidate, width, height },
                    Err(IoError::Exists(_)) => continue,
                    Err(e) => return write_err(&candidate, e),
                }
            }
            ItemStatus::Failed(format!("no free file name found next to {}", planned.display()))
        }
    }
}

/// Renders `node` of `graph` for every item, in the given order, and writes one file per item.
/// `progress` is called once before the first item (`done = 0`, `last = None`) and after each
/// item, including items reported as cancelled. The items are also the catalog of the project.
pub fn export(
    graph: &Graph,
    node: NodeId,
    items: &[ExportItem],
    settings: &ExportSettings,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&ExportProgress),
) -> Result<ExportReport, ExportError> {
    export_with(graph, node, items, settings, cancel, progress, &|p| rawgraph_io::read_capture_info(p).ok(), &rawgraph_io::read_lens_profile)
}

/// `export` with injectable metadata readers (capture metadata and lens profile, each asked once
/// per image, lazily, with the source path).
#[allow(clippy::too_many_arguments)]
pub(crate) fn export_with(
    graph: &Graph,
    node: NodeId,
    items: &[ExportItem],
    settings: &ExportSettings,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&ExportProgress),
    read_capture: CaptureReader,
    read_lens: LensReader,
) -> Result<ExportReport, ExportError> {
    check_node(graph, node)?;
    check_settings(settings)?;
    let jobs: Vec<Job> = items.iter().map(|it| Job { item: it.clone(), settings, pre_failure: None }).collect();
    run_batch(graph, node, items, &jobs, cancel, progress, read_capture, read_lens)
}

/// Exports the images `selection` through the Export node `export_node`.
///
/// `catalog` is every image of the project in project order (Source folders, Merge name collisions
/// and Layer pairing need all of them); `selection` is the images to write, in the order they are
/// reported. The settings of every image come from `export_spec(node, image)`, so Individual values
/// work, also different folders per image. Images of the selection that the graph does not deliver
/// to the node are `Excluded`; images that are not in the catalog fail. The output folder of every
/// selected image must be set, else this is an error before anything is written.
pub fn export_node(
    graph: &Graph,
    export_node: NodeId,
    catalog: &[ExportItem],
    selection: &[ImageId],
    display: DisplayTransform,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&ExportProgress),
) -> Result<ExportReport, ExportError> {
    export_node_with(
        graph,
        export_node,
        catalog,
        selection,
        display,
        cancel,
        progress,
        &|p| rawgraph_io::read_capture_info(p).ok(),
        &rawgraph_io::read_lens_profile,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn export_node_with(
    graph: &Graph,
    export_node: NodeId,
    catalog: &[ExportItem],
    selection: &[ImageId],
    display: DisplayTransform,
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&ExportProgress),
    read_capture: CaptureReader,
    read_lens: LensReader,
) -> Result<ExportReport, ExportError> {
    let node = match graph.node(export_node) {
        Some(n) if n.kind == NodeKind::Export => n,
        Some(n) => return invalid(format!("node {} ({}) is not an Export node", export_node.0, n.kind.label())),
        None => return invalid(format!("node {} does not exist in the graph", export_node.0)),
    };
    let by_id: HashMap<ImageId, &ExportItem> = catalog.iter().rev().map(|it| (it.id, it)).collect();
    let mut settings = Vec::with_capacity(selection.len());
    for &id in selection {
        let s = settings_from_spec(&export_spec(node, id), display);
        if s.out_dir.as_os_str().is_empty() {
            return invalid(format!("the output folder of the Export node is empty (image {})", id.0));
        }
        check_settings(&s)?;
        settings.push(s);
    }
    let jobs: Vec<Job> = selection
        .iter()
        .zip(&settings)
        .map(|(&id, s)| match by_id.get(&id) {
            Some(it) => Job { item: (*it).clone(), settings: s, pre_failure: None },
            None => Job {
                item: ExportItem { id, source: PathBuf::new() },
                settings: s,
                pre_failure: Some(format!("image {} is not part of the project", id.0)),
            },
        })
        .collect();
    run_batch(graph, export_node, catalog, &jobs, cancel, progress, read_capture, read_lens)
}

#[allow(clippy::too_many_arguments)]
fn run_batch(
    graph: &Graph,
    node: NodeId,
    catalog: &[ExportItem],
    jobs: &[Job],
    cancel: &CancelToken,
    progress: &mut dyn FnMut(&ExportProgress),
    read_capture: CaptureReader,
    read_lens: LensReader,
) -> Result<ExportReport, ExportError> {
    let provider = CatalogProvider::new(catalog, read_capture, read_lens);
    let mut ev = Evaluator::new();
    ev.set_keep_intermediates(false);
    let ctx = Ctx {
        graph,
        node,
        cancel,
        sources: catalog.iter().map(|it| norm_key(&it.source)).collect(),
        problem: graph_problem(graph, node),
    };

    // Membership first, from metadata and paths only: no pixel is decoded for an excluded image.
    // A structurally broken graph fails every image instead.
    let included: Vec<bool> = jobs
        .iter()
        .map(|job| {
            if job.pre_failure.is_some() || ctx.problem.is_some() {
                return true;
            }
            catch_unwind(AssertUnwindSafe(|| ev.contains(graph, node, job.item.id, &provider))).unwrap_or(true)
        })
        .collect();

    let mut planned: Vec<Option<Plan>> = Vec::with_capacity(jobs.len());
    // Final paths of the items planned so far (first owner wins).
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, job) in jobs.iter().enumerate() {
        if !included[i] || job.pre_failure.is_some() {
            planned.push(None);
            continue;
        }
        let tokens = check_settings(job.settings)?;
        let path = plan(&job.item, i + 1, job.settings, &tokens)?;
        let mut entry = Plan { base: path.clone(), target: path.clone(), start_n: 1, duplicate_of: None };
        if let Some(&first) = seen.get(&abs_key(&path)) {
            match job.settings.on_conflict {
                OnConflict::Overwrite => {
                    return invalid(format!(
                        "{} and {} would both be written to {}; add {{index}} or {{id}} to the name template",
                        jobs[first].item.source.display(),
                        job.item.source.display(),
                        path.display()
                    ));
                }
                OnConflict::Skip => {
                    entry.duplicate_of = planned[first].as_ref().map(|p| p.target.clone()).or_else(|| Some(path.clone()));
                }
                OnConflict::Rename => {}
            }
        }
        if entry.duplicate_of.is_none() && job.settings.on_conflict == OnConflict::Rename {
            // Next name that is free on disk and not planned by another item of this run.
            let mut n = 1usize;
            loop {
                let cand = if n == 1 { path.clone() } else { with_suffix(&path, n) };
                if n > 100_000 {
                    break;
                }
                if !seen.contains_key(&abs_key(&cand)) && !cand.exists() {
                    entry.target = cand;
                    entry.start_n = n;
                    break;
                }
                n += 1;
            }
            // Kept as is when nothing was found: the run reports the failure for this item.
        }
        if entry.duplicate_of.is_none() {
            seen.insert(abs_key(&entry.target), i);
        }
        planned.push(Some(entry));
    }
    let reserved: HashSet<String> = planned.iter().flatten().filter(|p| p.duplicate_of.is_none()).map(|p| abs_key(&p.target)).collect();
    let mut made: HashSet<String> = HashSet::new();
    for (job, path) in jobs.iter().zip(&planned) {
        if path.is_some() && made.insert(abs_key(&job.settings.out_dir)) {
            if let Err(e) = fs::create_dir_all(&job.settings.out_dir) {
                return invalid(format!("cannot create output folder {}: {e}", job.settings.out_dir.display()));
            }
        }
    }

    let total = jobs.len();
    let mut report = ExportReport::default();
    progress(&ExportProgress {
        done: 0,
        total,
        current: jobs.first().map(|j| j.item.source.clone()).unwrap_or_default(),
        last: None,
    });
    for (i, (job, path)) in jobs.iter().zip(&planned).enumerate() {
        let status = if !included[i] {
            ItemStatus::Excluded
        } else {
            let plan = path.clone().unwrap_or_default();
            catch_unwind(AssertUnwindSafe(|| run_item(&ctx, &mut ev, &provider, job, &plan, &reserved)))
                .unwrap_or_else(|_| ItemStatus::Failed("internal error while processing this image".into()))
        };
        // Whatever the outcome, nothing stays decoded between two items.
        ev.clear();
        provider.release_pixels();
        report.items.push((job.item.id, status.clone()));
        progress(&ExportProgress { done: i + 1, total, current: job.item.source.clone(), last: Some((job.item.id, status)) });
    }
    Ok(report)
}

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
mod node_tests;
