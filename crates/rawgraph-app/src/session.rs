//! Editing session state: the project (graph + image collection), the
//! filtered image list, selection, view settings and unsaved-changes tracking.
//!
//! Pure logic, no UI code.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use rawgraph_core::{CaptureInfo, Graph, ImageBuf, ImageId, Mode, NodeId, NodeKind};
use rawgraph_engine::ImageProvider;
use rawgraph_project::{ColorLabel, Flag, ImageEntry, Project, ProjectError};
use rawgraph_scopes::{DisplayTransform, ToneMap};

use crate::brush_logic::BrushSettings;
use crate::collection::{CollectionResult, Captures};
use crate::crop_logic::Aspect;
use crate::filter::FilterUi;
use crate::provider::FileProvider;
use crate::revisions::Baseline;
use crate::selection::{plan_delete, NodeSelection};
use crate::testimages;

/// Three synthetic scene-referred test images (generated lazily).
pub struct TestImages {
    entries: Vec<ImageEntry>,
    images: Vec<OnceLock<Arc<ImageBuf>>>,
}

impl TestImages {
    pub fn new() -> Self {
        let entries = testimages::entries();
        let images = entries.iter().map(|_| OnceLock::new()).collect();
        Self { entries, images }
    }

}

impl Default for TestImages {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageProvider for TestImages {
    fn image(&self, id: ImageId) -> Result<Arc<ImageBuf>, String> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.id == id)
            .ok_or_else(|| format!("image {} is not in the collection", id.0))?;
        Ok(self.images[idx].get_or_init(|| Arc::new(testimages::generate(idx))).clone())
    }
}

/// Tone map of the viewer (view-only, never part of the graph).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewTone {
    Clip,
    Soft,
    /// Approximate ACES 1.x SDR rendering.
    Aces,
}

impl ViewTone {
    pub const ALL: [ViewTone; 3] = [ViewTone::Clip, ViewTone::Soft, ViewTone::Aces];

    pub fn label(self) -> &'static str {
        match self {
            ViewTone::Clip => "Clip",
            ViewTone::Soft => "Soft highlights",
            ViewTone::Aces => "ACES (approx.)",
        }
    }

    pub fn tone_map(self) -> ToneMap {
        match self {
            ViewTone::Clip => ToneMap::Clip,
            ViewTone::Soft => ToneMap::SoftHighlights,
            ViewTone::Aces => ToneMap::AcesFit,
        }
    }
}

/// Settings of the display transform. View-only, never part of the graph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewSettings {
    pub exposure_ev: f32,
    pub tone: ViewTone,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self { exposure_ev: 0.0, tone: ViewTone::Aces }
    }
}

impl ViewSettings {
    pub fn transform(&self) -> DisplayTransform {
        DisplayTransform { exposure_ev: self.exposure_ev, tone_map: self.tone.tone_map() }
    }
}

type Edit = (u8, Flag, Option<ColorLabel>);

/// The capture metadata of an entry with the source path filled in (what the collection queries need).
pub fn capture_with_path(e: &ImageEntry) -> CaptureInfo {
    let mut c = e.capture.clone();
    c.path = Some(e.path.to_string_lossy().into_owned());
    c
}

/// UI state of the interactive crop (not part of the graph).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CropUi {
    pub aspect: Aspect,
    /// Portrait / landscape swap of the fixed ratios.
    pub swap: bool,
    /// The straighten angle seen last frame, and the time it last changed (for the fine grid).
    pub last_angle: f32,
    pub angle_changed_at: Option<f64>,
    /// Node and image the angle was last read for: a different one starts without the grid.
    pub seen: Option<(NodeId, ImageId)>,
}

/// The histogram of the picture entering a Curve node (256 heights, 0..1).
#[derive(Clone, Debug, PartialEq)]
pub struct CurveHist {
    pub node: NodeId,
    pub image: ImageId,
    pub bins: Vec<f32>,
}

/// Something the inspector asks the application to do (it cannot open dialogs or start jobs itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiRequest {
    /// "Choose folder..." of a Source node.
    PickSourceFolder(NodeId),
    /// "Choose..." of an Export node's output folder.
    PickExportFolder(NodeId),
    /// "Choose..." of a LUT node: pick a `.cube` file.
    PickLutFile(NodeId),
    /// "Export..." of an Export node: opens the export dialog in node mode.
    OpenNodeExport(NodeId),
}

/// Lower-cased path with `/` separators and no trailing separator (for comparing folders).
pub fn folder_key(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// What the viewer does with the pointer: shows the picture, paints a brush
/// image, or edits the crop of a Crop / Rotate node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolMode {
    None,
    Brush(NodeId),
    Crop(NodeId),
}

pub struct Session {
    pub project: Project,
    pub provider: Arc<FileProvider>,
    pub selected: Option<ImageId>,
    /// The primary node: the one selected last (the inspector shows it).
    pub selected_node: Option<NodeId>,
    /// All selected nodes; its primary node is `selected_node`.
    pub node_sel: NodeSelection,
    /// Bumped by every change of the document (autosave looks at it).
    pub edit_gen: u64,
    /// Pinned viewer target; None follows the primary selection (session-only).
    pub viewed_node: Option<NodeId>,
    pub view: ViewSettings,
    pub project_path: Option<PathBuf>,
    /// Unsaved changes (graph, ratings, flags, labels, scanned images).
    pub dirty: bool,
    /// The three built-in test images instead of the project's images.
    pub demo: bool,
    pub filter: FilterUi,
    /// Brush settings of the Draw tool.
    pub brush: BrushSettings,
    pub crop: CropUi,
    /// The node whose white-balance picker is armed (a click in the viewer picks a neutral point).
    pub wb_pick: Option<NodeId>,
    /// Luminance histogram behind the curve editor, made by the render worker.
    pub curve_hist: Option<CurveHist>,
    /// Requests of the inspector for the application (dialogs, export), taken once per frame.
    pub ui_requests: Vec<UiRequest>,
    /// An export job is running (the Export button of a node is off meanwhile).
    pub export_busy: bool,
    /// The caption sheet loaded for renaming and export names (session only, not saved).
    pub caption_sheet: Option<Arc<rawgraph_batch::SheetBinding>>,
    /// Summary for the next saved revision instead of the generated one (set after a batch rename).
    pub next_summary: Option<String>,
    /// `--inspector-scroll`: keeps the inspector scrolled to this offset (screenshots).
    pub inspector_scroll: Option<f32>,
    demo_entries: Vec<ImageEntry>,
    visible: Vec<ImageId>,
    // Folder scan in progress: entries that finished so far (shown immediately).
    pub scanning: bool,
    scan_map: HashMap<ImageId, ImageEntry>,
    pub scan_count: Option<(String, bool, usize)>,
    scan_edits: HashMap<ImageId, Edit>,
    /// Folder and sub-folder flag of the running scan (recorded in the project when it is saved meanwhile).
    pub scan_folder: Option<(PathBuf, bool)>,
    /// Collection info for the graph (filled by the background worker).
    pub collection: CollectionResult,
    /// Bumped whenever the set of images or their metadata changes.
    pub images_gen: u64,
    capture_cache: Option<(u64, Arc<Vec<ImageId>>, Captures)>,
    /// The document as of the last commit or load (the summary of the next revision is derived from it).
    baseline: Baseline,
}

/// The node collection and image shown together by a viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewerTarget {
    pub node: Option<NodeId>,
    pub image: Option<ImageId>,
    pub image_pinned: bool,
}

impl Session {
    pub fn new(project: Project, provider: Arc<FileProvider>) -> Self {
        let baseline = Baseline::of(&project);
        let mut s = Self {
            baseline,
            project,
            provider,
            selected: None,
            selected_node: None,
            node_sel: NodeSelection::default(),
            edit_gen: 0,
            viewed_node: None,
            view: ViewSettings::default(),
            project_path: None,
            dirty: false,
            demo: false,
            filter: FilterUi::default(),
            brush: BrushSettings::default(),
            crop: CropUi::default(),
            wb_pick: None,
            curve_hist: None,
            ui_requests: Vec::new(),
            export_busy: false,
            caption_sheet: None,
            next_summary: None,
            inspector_scroll: None,
            demo_entries: Vec::new(),
            visible: Vec::new(),
            scanning: false,
            scan_map: HashMap::new(),
            scan_count: None,
            scan_edits: HashMap::new(),
            scan_folder: None,
            collection: CollectionResult::default(),
            images_gen: 1,
            capture_cache: None,
        };
        s.sync_paths();
        s.refresh_visible();
        s
    }

    /// Session showing the built-in test images with the demo graph.
    pub fn demo(provider: Arc<FileProvider>) -> Self {
        let mut s = Self::new(Project::new(), provider);
        s.enter_demo();
        s
    }

    pub fn enter_demo(&mut self) {
        let mut project = Project::new();
        project.graph = demo_graph();
        self.baseline = Baseline::of(&project);
        self.project = project;
        self.demo_entries = testimages::entries();
        self.demo = true;
        self.project_path = None;
        self.dirty = false;
        self.scanning = false;
        self.scan_folder = None;
        self.scan_map.clear();
        self.scan_edits.clear();
        self.selected_node = None;
        self.node_sel.clear();
        self.viewed_node = None;
        self.provider.reset_paths();
        self.sync_paths();
        self.selected = None;
        self.refresh_visible();
    }

    // ------------------------------------------------------------ image list

    /// Entry for `id`: scan results first, then the project (or the demo images).
    pub fn entry(&self, id: ImageId) -> Option<&ImageEntry> {
        if self.demo {
            return self.demo_entries.iter().find(|e| e.id == id);
        }
        self.scan_map.get(&id).or_else(|| self.project.image(id))
    }

    /// Ids shown in the filmstrip, filtered and sorted.
    pub fn visible(&self) -> &[ImageId] {
        &self.visible
    }

    pub fn total_images(&self) -> usize {
        if self.demo {
            return self.demo_entries.len();
        }
        let extra = self.scan_map.keys().filter(|id| self.project.image(**id).is_none()).count();
        self.project.images().len() + extra
    }

    /// Whether `image` is part of the collection at the viewed node; `None`
    /// while that is not known yet.
    #[cfg(test)]
    pub fn in_view(&self, image: ImageId) -> Option<bool> {
        let ready = self.collection.view_node.is_some() && self.collection.view_node == self.target_node();
        ready.then(|| self.collection.view_members.contains(&image))
    }

    /// Number of listed images that are part of the viewed collection.
    #[cfg(test)]
    pub fn in_view_count(&self) -> Option<usize> {
        self.in_view(ImageId(0))?;
        Some(self.visible.iter().filter(|id| self.collection.view_members.contains(id)).count())
    }

    pub fn current_entry(&self) -> Option<&ImageEntry> {
        self.selected.and_then(|id| self.entry(id))
    }

    /// Identity of the selected image (a placeholder id when nothing is selected).
    pub fn current_image(&self) -> ImageId {
        self.selected.unwrap_or(ImageId(0))
    }

    /// Recomputes the filtered, sorted list. Keeps the selection if it is still
    /// listed, otherwise selects the first listed image.
    pub fn refresh_visible(&mut self) {
        self.visible = if self.demo {
            self.demo_entries.iter().map(|e| e.id).collect()
        } else {
            let filter = self.filter.to_filter();
            let entries = self.project.images().iter().filter_map(|e| self.entry(e.id))
                .chain(self.scan_map.values().filter(|e| self.project.image(e.id).is_none()));
            Project::sort_entries(entries.filter(|e| filter.matches(e) && (!self.filter.missing_only || e.missing)), self.filter.sort, self.filter.descending)

        };
        if self.selected.is_none_or(|id| !self.visible.contains(&id)) {
            self.selected = self.visible.first().copied();
        }
    }

    pub fn select_index(&mut self, index: usize) {
        if let Some(id) = self.visible.get(index) {
            self.selected = Some(*id);
        }
    }

    #[cfg(test)]
    pub fn select_id(&mut self, id: ImageId) {
        if self.visible.contains(&id) {
            self.selected = Some(id);
        }
    }

    pub fn selected_index(&self) -> Option<usize> {
        let sel = self.selected?;
        self.visible.iter().position(|i| *i == sel)
    }

    /// Steps through the filtered list (clamped).
    pub fn step_image(&mut self, delta: i32) {
        if self.visible.is_empty() {
            return;
        }
        let cur = self.selected_index().unwrap_or(0) as i32;
        let next = (cur + delta).clamp(0, self.visible.len() as i32 - 1) as usize;
        self.selected = Some(self.visible[next]);
    }

    /// Catalog members in filmstrip order, followed by generated images.
    pub fn gallery_images(&self, members: &HashSet<ImageId>) -> Vec<ImageId> {
        let mut images: Vec<_> = self.visible.iter().copied().filter(|id| members.contains(id)).collect();
        let mut generated: Vec<_> = members.iter().copied().filter(|id| self.entry(*id).is_none()).collect();
        generated.sort();
        images.extend(generated);
        images
    }

    /// Navigate only the viewer's current collection, never stale membership.
    pub fn step_viewer_image(&mut self, delta: i32) {
        self.step_viewer_target(self.viewer_target(None, self.viewed_node), delta);
    }

    pub fn step_viewer_target(&mut self, target: ViewerTarget, delta: i32) {
        if target.image_pinned || delta == 0 { return; }
        let Some(node) = target.node else { return };
        let (ids, captures) = self.id_captures();
        let members = crate::collection::view_members(&self.project.graph, &ids, &captures, node);
        let images = self.gallery_images(&members);
        self.step_gallery(target, &images, delta);
    }

    pub fn step_gallery(&mut self, target: ViewerTarget, images: &[ImageId], delta: i32) {
        if target.image_pinned || images.is_empty() || delta == 0 { return; }
        let index = target.image.and_then(|id| images.iter().position(|x| *x == id));
        let next = match index { Some(i) => (i as i64 + i64::from(delta)).clamp(0, images.len() as i64 - 1) as usize, None if delta > 0 => 0, None => images.len() - 1 };
        self.selected = Some(images[next]);
    }

    /// Neighbours of the selection in the filtered order, for prefetching.
    pub fn neighbours(&self) -> Vec<ImageId> {
        let Some(i) = self.selected_index() else { return Vec::new() };
        let mut out = Vec::new();
        if i + 1 < self.visible.len() {
            out.push(self.visible[i + 1]);
        }
        if i > 0 {
            out.push(self.visible[i - 1]);
        }
        out
    }

    /// Ids and capture metadata of every image (for collection queries), cached
    /// until the image set changes. While a scan runs the images found so far
    /// are included and nothing is cached.
    pub fn id_captures(&mut self) -> (Arc<Vec<ImageId>>, Captures) {
        if self.scanning && !self.demo {
            let mut ids: Vec<ImageId> = self.project.images().iter().map(|e| e.id).collect();
            let mut caps: HashMap<ImageId, CaptureInfo> = self.project.images().iter().map(|e| (e.id, capture_with_path(e))).collect();
            let mut found: Vec<&ImageEntry> = self.scan_map.values().collect();
            found.sort_by_key(|e| e.id);
            for e in found {
                if caps.insert(e.id, capture_with_path(e)).is_none() {
                    ids.push(e.id);
                }
            }
            return (Arc::new(ids), Arc::new(caps));
        }
        if let Some((generation, ids, caps)) = &self.capture_cache {
            if *generation == self.images_gen {
                return (ids.clone(), caps.clone());
            }
        }
        let entries: Vec<&ImageEntry> = if self.demo { self.demo_entries.iter().collect() } else { self.project.images().iter().collect() };
        let ids: Arc<Vec<ImageId>> = Arc::new(entries.iter().map(|e| e.id).collect());
        let caps: Captures = Arc::new(entries.iter().map(|e| (e.id, capture_with_path(e))).collect());
        self.capture_cache = Some((self.images_gen, ids.clone(), caps.clone()));
        (ids, caps)
    }

    /// Whether the images of `folder` (with its sub-folders when `subfolders`) are in the project
    /// already: some folder that was scanned into it covers it. A folder that is not covered has to
    /// be scanned before a Source can show it.
    pub fn folder_covered(&self, folder: &str, subfolders: bool) -> bool {
        if self.demo {
            return true;
        }
        let key = folder_key(folder);
        self.project.folders().iter().any(|f| {
            let scanned = folder_key(&f.path.to_string_lossy());
            if scanned == key {
                f.recursive || !subfolders
            } else {
                // A folder inside a recursively scanned one is covered, with its own sub-folders.
                f.recursive && rawgraph_core::path_in_folder(&key, &scanned, true)
            }
        })
    }

    /// Number of images found by the running scan (changes as images arrive).
    pub fn scan_found(&self) -> usize {
        self.scan_map.len()
    }

    /// Entries whose capture metadata predates the as-shot fields; the worker
    /// skips the ones whose file does not exist.
    pub fn capture_refresh_targets(&self) -> Vec<(ImageId, PathBuf)> {
        if self.demo {
            return Vec::new();
        }
        self.project.images().iter().filter(|e| !e.capture.scan_pending && e.capture.raw.is_none()).map(|e| (e.id, e.path.clone())).collect()
    }

    /// Applies re-read capture metadata. Marks the project unsaved when
    /// something changed and returns how many entries changed.
    pub fn apply_capture_updates(&mut self, updates: Vec<(ImageId, CaptureInfo)>) -> usize {
        if self.demo || updates.is_empty() {
            return 0;
        }
        let mut changed = 0;
        let mut pushed = Vec::new();
        for (id, info) in updates {
            let mut hit = false;
            for entry in self.project.image_mut(id).into_iter().chain(self.scan_map.get_mut(&id)) {
                if entry.capture != info {
                    entry.capture = info.clone();
                    hit = true;
                }
            }
            if hit {
                changed += 1;
                pushed.push((id, info));
            }
        }
        if changed > 0 {
            self.provider.set_captures(pushed);
            self.images_gen += 1;
            self.touch();
        }
        changed
    }

    /// Applies the result of a background existence check of the files. Returns how many entries changed.
    pub fn apply_missing(&mut self, updates: &[(ImageId, bool)]) -> usize {
        if self.demo {
            return 0;
        }
        let mut changed = 0;
        for (id, missing) in updates {
            if let Some(e) = self.project.image_mut(*id) {
                if e.missing != *missing {
                    e.missing = *missing;
                    changed += 1;
                }
            }
        }
        if changed > 0 && !self.scanning {
            self.images_gen += 1;
            self.refresh_visible();
        }
        changed
    }

    /// `(id, path)` of every project entry, for the missing-file check.
    pub fn id_paths(&self) -> Vec<(ImageId, PathBuf)> {
        if self.demo {
            return Vec::new();
        }
        self.project.images().iter().map(|e| (e.id, e.path.clone())).collect()
    }

    /// Pushes id -> path and capture metadata of every known entry to the provider.
    pub fn sync_paths(&mut self) {
        self.images_gen += 1;
        self.provider.reset_paths();
        if self.demo {
            self.provider.set_paths(self.demo_entries.iter().map(|e| (e.id, e.path.clone())));
            self.provider.set_captures(self.demo_entries.iter().map(|e| (e.id, e.capture.clone())));
        } else {
            self.provider.set_paths(self.project.images().iter().map(|e| (e.id, e.path.clone())));
            self.provider.set_captures(self.project.images().iter().map(|e| (e.id, e.capture.clone())));
            self.provider.set_paths(self.scan_map.values().map(|e| (e.id, e.path.clone())));
            self.provider.set_captures(self.scan_map.values().map(|e| (e.id, e.capture.clone())));
        }
    }

    // ------------------------------------------------------------ culling

    fn edit_entry(&mut self, id: ImageId, f: impl Fn(&mut ImageEntry)) -> bool {
        if self.demo {
            return false;
        }
        let mut found = false;
        if let Some(e) = self.scan_map.get_mut(&id) {
            f(e);
            found = true;
        }
        if let Some(e) = self.project.image_mut(id) {
            f(e);
            found = true;
        }
        if !found {
            return false;
        }
        self.edit_gen += 1;
        if self.scanning {
            if let Some(e) = self.entry(id) {
                let edit = (e.rating, e.flag, e.color_label);
                self.scan_edits.insert(id, edit);
            }
        }
        self.dirty = true;
        if !self.scanning {
            self.refresh_visible();
        }
        true
    }

    pub fn set_rating(&mut self, id: ImageId, rating: u8) -> bool {
        let r = rating.min(5);
        self.edit_entry(id, |e| e.rating = r)
    }

    pub fn set_flag(&mut self, id: ImageId, flag: Flag) -> bool {
        self.edit_entry(id, |e| e.flag = flag)
    }

    /// Sets the colour label; the same label again clears it.
    pub fn toggle_label(&mut self, id: ImageId, label: ColorLabel) -> bool {
        let new = if self.entry(id).and_then(|e| e.color_label) == Some(label) { None } else { Some(label) };
        self.edit_entry(id, |e| e.color_label = new)
    }

    // ------------------------------------------------------------ scan

    pub fn begin_scan(&mut self) {
        self.scanning = true;
        self.scan_count = None;
        self.scan_map.clear();
        self.scan_edits.clear();
        self.scan_folder = None;
    }


    /// A copy of the project as it would be saved right now: while a scan runs it
    /// includes the images found so far. For recovery copies, which must not
    /// change the session.
    pub fn saveable_project(&self) -> Project {
        let mut project = self.project.clone();
        if self.scanning && !self.demo {
            if let Some((folder, recursive)) = &self.scan_folder {
                project.note_scanned_folder(folder, *recursive);
            }
            let found: Vec<ImageEntry> = self.scan_map.values().cloned().collect();
            project.merge_found(&found);
        }
        project
    }

    pub fn count_for(&self, node: &rawgraph_core::Node) -> Option<usize> {
        if node.kind == NodeKind::Source {
            if let Some((folder, recursive, count)) = &self.scan_count {
                if crate::node_ui::source_subfolders(node) == *recursive
                    && crate::node_ui::source_folder(node).is_some_and(|p| {
                        let path = std::path::absolute(&p).unwrap_or_else(|_| p.into());
                        folder_key(&path.to_string_lossy()) == *folder
                    })
                { return Some(*count); }
            }
        }
        self.collection.counts.get(&node.id).copied()
    }

    pub fn scan_catalog(&mut self, entries: Vec<ImageEntry>) {
        self.provider.set_paths(entries.iter().map(|e| (e.id, e.path.clone())));
        self.provider.set_captures(entries.iter().map(|e| (e.id, e.capture.clone())));
        self.scan_map.extend(entries.into_iter().map(|e| (e.id, e)));
        self.images_gen += 1;
        self.refresh_visible();
    }

    pub fn scan_metadata(&mut self, entries: Vec<ImageEntry>) {
        self.provider.set_captures(entries.iter().map(|e| (e.id, e.capture.clone())));
        for entry in entries {
            if let Some(e) = self.scan_map.get_mut(&entry.id) { e.capture = entry.capture; }
        }
        self.images_gen += 1;
        self.refresh_visible();
    }

    #[cfg(test)]
    pub fn scan_entry(&mut self, entry: ImageEntry) {
        self.scan_entries(vec![entry]);
    }

    pub fn scan_entries(&mut self, entries: Vec<ImageEntry>) {
        let mut paths = Vec::new();
        let mut captures = Vec::new();
        for mut entry in entries {
            let id = entry.id;
            let old = self.entry(id);
            if old.is_none_or(|e| e.path != entry.path) { paths.push((id, entry.path.clone())); }
            if old.is_none_or(|e| e.capture != entry.capture) { captures.push((id, entry.capture.clone())); }
            if let Some((r, f, l)) = self.scan_edits.get(&id) {
                entry.rating = *r;
                entry.flag = *f;
                entry.color_label = *l;
            }
            self.scan_map.insert(id, entry);
        }
        if !paths.is_empty() || !captures.is_empty() {
            self.provider.set_paths(paths);
            self.provider.set_captures(captures);
            self.images_gen += 1;
            self.refresh_visible();
        }
    }

    /// The scan worker is done: swap in the finished project, keeping what the
    /// user changed meanwhile (graph, ratings, flags, labels).
    pub fn finish_scan(&mut self, mut finished: Project, force_dirty: bool) {
        // The scan worked on a copy: carry over the commit state (revision, history) of the
        // project the user may have saved meanwhile, or the next save would conflict with it.
        finished.inherit_versioning(&self.project);
        finished.graph = self.project.graph.clone();
        for (id, (r, f, l)) in std::mem::take(&mut self.scan_edits) {
            if let Some(e) = finished.image_mut(id) {
                e.rating = r;
                e.flag = f;
                e.color_label = l;
            }
        }
        // Compared after everything the user did meanwhile is taken over: a scan that brought nothing
        // new after the last save leaves the project saved.
        let changed = force_dirty || finished != self.project;
        self.project = finished;
        self.scan_map.clear();
        self.scanning = false;
        self.scan_folder = None;
        self.scan_count = None;
        if changed {
            self.touch();
        }
        self.sync_paths();
        self.refresh_visible();
    }

    pub fn cancel_scan_state(&mut self) {
        self.scan_count = None;
        self.scan_folder = None;
        self.scanning = false;
        self.scan_map.clear();
        self.scan_edits.clear();
        self.refresh_visible();
    }

    // ------------------------------------------------------------ project

    /// Marks the document as changed (unsaved).
    pub fn touch(&mut self) {
        self.dirty = true;
        self.edit_gen += 1;
    }

    pub fn project_name(&self) -> String {
        if self.demo {
            return "Demo images".to_owned();
        }
        self.project_path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map_or_else(|| "Untitled project".to_owned(), |s| s.to_string_lossy().into_owned())
    }

    pub fn window_title(&self) -> String {
        format!("{}{} - RawGraph {}", self.project_name(), if self.dirty { " *" } else { "" }, crate::display_version())
    }

    /// Replaces the document with an empty project and the default graph.
    pub fn new_project(&mut self) {
        self.replace_project(fresh_project(), None);
    }

    /// Replaces the whole document (new/opened project).
    pub fn replace_project(&mut self, project: Project, path: Option<PathBuf>) {
        self.baseline = Baseline::of(&project);
        self.project = project;
        self.project_path = path;
        self.dirty = false;
        self.demo = false;
        self.scanning = false;
        self.scan_folder = None;
        self.scan_map.clear();
        self.scan_edits.clear();
        self.selected_node = None;
        self.node_sel.clear();
        self.viewed_node = None;
        self.selected = None;
        self.edit_gen += 1;
        self.sync_paths();
        self.refresh_visible();
    }

    /// Saves as a new revision (with a generated summary of what changed since the last
    /// commit or load). `overwrite` skips the conflict and project checks. On success the
    /// project has this path and is no longer changed; on failure nothing changes.
    pub fn commit_project(&mut self, path: &Path, overwrite: bool) -> Result<u64, ProjectError> {
        let revision = if self.scanning && !self.demo {
            // A scan is still running: save what it has found so far; it continues afterwards.
            // The images go into a copy that replaces the session's project only if the save
            // succeeded, so a failed save (conflict, I/O error) changes nothing.
            let mut copy = self.saveable_project();
            let summary = self.baseline.summarize(&copy);
            let revision = if overwrite { copy.commit_overwrite(path, &summary)? } else { copy.commit(path, &summary)? };
            self.project = copy;
            revision
        } else {
            let summary = self.next_summary.clone().unwrap_or_else(|| self.baseline.summarize(&self.project));
            if overwrite { self.project.commit_overwrite(path, &summary)? } else { self.project.commit(path, &summary)? }
        };
        self.next_summary = None;
        self.project_path = Some(path.to_path_buf());
        self.dirty = false;
        self.baseline = Baseline::of(&self.project);
        Ok(revision)
    }

    /// Opens a project file; the note is set when the file was converted from format 1 or an
    /// interrupted save was rolled back first.
    pub fn load_project(&mut self, path: &Path) -> Result<Option<&'static str>, crate::revisions::OpenError> {
        let opened = crate::revisions::open_project_ex(path)?;
        self.replace_project(opened.project, Some(path.to_path_buf()));
        Ok(opened.note)
    }

    /// Replaces only the graph (import of a `.rawgraph.json`).
    pub fn set_graph(&mut self, graph: Graph) {
        self.project.graph = graph;
        self.selected_node = None;
        self.node_sel.clear();
        self.viewed_node = None;
        self.touch();
    }

    // ------------------------------------------------------------ nodes

    #[cfg(test)]
    pub fn output_node(&self) -> Option<NodeId> {
        self.project.graph.nodes().find(|n| n.kind == NodeKind::Output).map(|n| n.id)
    }

    /// Node the viewer renders: pinned target, otherwise the primary selection.
    pub fn target_node(&self) -> Option<NodeId> {
        self.viewer_target(None, self.viewed_node).node
    }

    pub fn viewer_target(&self, image_pin: Option<ImageId>, node_pin: Option<NodeId>) -> ViewerTarget {
        let (image, node) = crate::compare::Pane { image: image_pin, node: node_pin.filter(|id| self.project.graph.node(*id).is_some()) }
            .target(self.selected, self.selected_node.filter(|id| self.project.graph.node(*id).is_some()));
        ViewerTarget { node, image: node.and(image), image_pinned: image_pin.is_some() }
    }

    /// The interactive tool the viewer offers for the selected node.
    pub fn tool_mode(&self) -> ToolMode {
        let Some(node) = self.selected_node.and_then(|id| self.project.graph.node(id)) else { return ToolMode::None };
        match node.kind {
            NodeKind::Draw => ToolMode::Brush(node.id),
            NodeKind::Transform if self.viewed_node.is_none() && self.project.graph.input_edge(node.id, 0).is_some() => ToolMode::Crop(node.id),
            _ => ToolMode::None,
        }
    }

    /// What the viewer renders: the node and, when a specific output port is
    /// wanted, that port. While the crop tool is active this is the picture
    /// that enters the Crop / Rotate node.
    pub fn render_target(&self) -> Option<(NodeId, Option<usize>)> {
        self.render_viewer_target(self.viewer_target(None, self.viewed_node))
    }

    pub fn render_viewer_target(&self, target: ViewerTarget) -> Option<(NodeId, Option<usize>)> {
        if let ToolMode::Crop(id) = self.tool_mode() {
            let graph = &self.project.graph;
            let mut inputs = graph.input_edges(id, 0);
            if let Some(first) = inputs.next() {
                let image = target.image?;
                let evaluator = rawgraph_engine::Evaluator::new();
                let edge = std::iter::once(first).chain(inputs)
                    .find(|e| evaluator.contains_output(graph, e.from, e.from_port, image, self.provider.as_ref()))
                    .unwrap_or(first);
                return Some((edge.from, Some(edge.from_port)));
            }
        }
        // Painting a drawing whose picture cannot be shown right behind later geometry:
        // show the picture as it leaves the node that uses the drawing.
        if let ToolMode::Brush(id) = self.tool_mode() {
            if let crate::frames::BrushPlan::ShowConsumer { consumer } = crate::frames::brush_plan(&self.project.graph, id, target.node) {
                return Some((consumer, None));
            }
        }
        target.node.map(|n| (n, None))
    }

    /// Draw uses its generated identity when it has no image input.
    pub fn draw_image(&self, draw: NodeId, viewed_image: ImageId) -> ImageId {
        if self.project.graph.input_edge(draw, 0).is_none() { ImageId(u64::MAX - draw.0) } else { viewed_image }
    }

    /// The selected photo, or a generated member when this view does not carry it.
    pub fn image_for_target(&self, node: NodeId, port: Option<usize>) -> Option<ImageId> {
        let eval = rawgraph_engine::Evaluator::new();
        let graph = &self.project.graph;
        let output = port.unwrap_or(0);
        let provider = self.provider.as_ref();
        // Normal photo views preserve the selection, including excluded photos.
        // Metadata Split preview is the unfiltered input, not its first output.
        if let Some(image) = self.selected {
            if graph.node(node).is_some_and(|n| n.kind == NodeKind::MetadataSplit) && port.is_none()
                || eval.contains_output(graph, node, output, image, provider) {
                return Some(image);
            }
        }
        let standalone = rawgraph_core::graph::Catalog { images: &[], capture: &|_| None };
        graph.members(node, output, &standalone).first().copied().or(self.selected)
    }

    /// How the brush tool of `brush` paints on the picture that is shown.
    pub fn brush_plan(&self, brush: NodeId) -> crate::frames::BrushPlan {
        crate::frames::brush_plan(&self.project.graph, brush, self.target_node())
    }

    /// Pixel size of the decoded proxy of `image`, when it is in memory (the UI thread never decodes).
    pub fn proxy_size(&self, image: ImageId) -> Option<(u32, u32)> {
        if !self.provider.is_cached(image) {
            return None;
        }
        let img = self.provider.image(image).ok()?;
        Some((img.width(), img.height()))
    }

    pub fn node_by_index(&self, index: usize) -> Option<NodeId> {
        self.project.graph.nodes().nth(index).map(|n| n.id)
    }

    /// Drops selection/view references to nodes that no longer exist.
    pub fn sanitize(&mut self) {
        if self.selected_node.is_some_and(|id| self.project.graph.node(id).is_none()) {
            self.selected_node = None;
        }
        if self.viewed_node.is_some_and(|id| self.project.graph.node(id).is_none()) {
            self.viewed_node = None;
        }
        self.reconcile_selection();
    }

    /// Keeps `selected_node` and `node_sel` consistent: code that sets
    /// `selected_node` directly (CLI, inspector, tests) turns the selection
    /// into just that node; the editor sets both through `set_node_selection`.
    pub fn reconcile_selection(&mut self) {
        self.node_sel.retain_existing(&self.project.graph);
        match self.selected_node {
            None => self.node_sel.clear(),
            Some(p) => {
                if self.node_sel.primary() != Some(p) {
                    self.node_sel.set_single(p);
                }
            }
        }
    }

    /// Replaces the node selection; the last node becomes the primary one.
    pub fn set_node_selection(&mut self, sel: NodeSelection) {
        self.selected_node = sel.primary();
        self.node_sel = sel;
    }

    /// Deletes the selected nodes together (one graph history change).
    pub fn delete_selected(&mut self) -> Result<usize, String> {
        self.reconcile_selection();
        let ids: Vec<NodeId> = self.node_sel.ids().to_vec();
        self.delete_nodes(&ids)
    }

    pub fn delete_nodes(&mut self, ids: &[NodeId]) -> Result<usize, String> {
        let delete = plan_delete(&self.project.graph, ids);
        for id in &delete {
            self.project.graph.remove_node(*id);
        }
        self.sanitize();
        Ok(delete.len())
    }

    pub fn delete_node(&mut self, id: NodeId) -> Result<(), String> {
        self.delete_nodes(&[id]).map(|_| ())
    }

    /// Switches a node's mode (used by the CLI and tests).
    pub fn set_mode(&mut self, id: NodeId, mode: Mode) {
        if let Some(n) = self.project.graph.node_mut(id) {
            n.mode = mode;
        }
    }
}

/// Graph of a new project: Source -> Develop -> Exposure -> Contrast ->
/// Highlights / Shadows -> Saturation, everything neutral except the
/// Develop defaults (as-shot white balance, baseline exposure on).
pub fn default_graph() -> Graph {
    let mut g = Graph::new();
    let kinds = [NodeKind::Source, NodeKind::Develop, NodeKind::Exposure, NodeKind::Contrast, NodeKind::HighlightsShadows, NodeKind::Saturation];
    let ids: Vec<NodeId> = kinds.iter().enumerate().map(|(i, k)| g.add_node(*k, [i as f32 * 240.0, 40.0])).collect();
    crate::node_ui::set_source_folder(&mut g, ids[0], None);
    for pair in ids.windows(2) {
        g.connect(pair[0], pair[1], 0).expect("default chain is valid");
    }
    g
}

/// A new, empty project with the default graph.
pub fn fresh_project() -> Project {
    let mut p = Project::new();
    p.graph = default_graph();
    p
}

/// Source -> Develop -> Capture EV -> White Balance -> Exposure -> Saturation
/// -> Blur -> Output, with a Generator gradient driving the Blur radius.
pub fn demo_graph() -> Graph {
    let mut g = Graph::new();
    let (dx, y) = (240.0, 40.0);
    let src = g.add_node(NodeKind::Source, [0.0, y]);
    let dev = g.add_node(NodeKind::Develop, [dx, y]);
    let cap = g.add_node(NodeKind::CaptureEv, [2.0 * dx, y]);
    let wb = g.add_node(NodeKind::WhiteBalance, [3.0 * dx, y]);
    let exp = g.add_node(NodeKind::Exposure, [4.0 * dx, y]);
    let sat = g.add_node(NodeKind::Saturation, [5.0 * dx, y]);
    let blur = g.add_node(NodeKind::Blur, [6.0 * dx, y]);
    let out = g.add_node(NodeKind::Output, [7.0 * dx, y]);
    let mask = g.add_node(NodeKind::Generator, [5.0 * dx, y + 170.0]);
    let chain = [src, dev, cap, wb, exp, sat, blur, out];
    for pair in chain.windows(2) {
        g.connect(pair[0], pair[1], 0).expect("demo chain is valid");
    }
    let port = g.expose_parameter(blur, "radius", rawgraph_core::PortType::Images).expect("Filter supports Radius");
    g.connect(mask, blur, port).expect("demo control is valid");
    // The map scales the Filter radius slider: a short gradient from no blur to the slider radius.
    g.node_mut(mask).unwrap().set_param("height", ImageId(0), 2.0);
    g
}

/// The demo graph with Blur radius 0, so real photos are not blurred.
pub fn demo_graph_neutral() -> Graph {
    let mut g = demo_graph();
    let blur = g.nodes().find(|n| n.kind == NodeKind::Blur).map(|n| n.id);
    if let Some(n) = blur.and_then(|id| g.node_mut(id)) {
        n.set_param("radius", ImageId(0), 0.0);
    }
    g
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use rawgraph_project::SortKey;

    pub fn provider() -> Arc<FileProvider> {
        Arc::new(FileProvider::new(1 << 30))
    }

    pub fn demo_session() -> Session {
        Session::demo(provider())
    }

    /// A project with `n` small PNG files, scanned synchronously.
    pub fn png_project(dir: &Path, n: usize) -> Project {
        for i in 0..n {
            let path = dir.join(format!("img_{i:03}.png"));
            image::RgbaImage::from_pixel(32, 24, image::Rgba([10 + i as u8 * 5, 50, 90, 255])).save(&path).unwrap();
        }
        let mut p = Project::new();
        let report = p.scan_folder(dir, false, &rawgraph_io::is_supported, &|path| rawgraph_io::read_capture_info(path).ok());
        assert_eq!(report.added.len(), n);
        p
    }

    #[test]
    fn demo_graph_is_valid_and_complete() {
        let g = demo_graph();
        assert!(g.validate().is_empty());
        let kinds: Vec<NodeKind> = g.nodes().map(|n| n.kind).collect();
        assert_eq!(
            kinds,
            [
                NodeKind::Source,
                NodeKind::Develop,
                NodeKind::CaptureEv,
                NodeKind::WhiteBalance,
                NodeKind::Exposure,
                NodeKind::Saturation,
                NodeKind::Blur,
                NodeKind::Output,
                NodeKind::Generator
            ]
        );
        assert_eq!(g.edges().len(), 8);
        let blur = g.nodes().find(|n| n.kind == NodeKind::Blur).unwrap().id;
        let mask = g.nodes().find(|n| n.kind == NodeKind::Generator).unwrap().id;
        assert_eq!(g.input_of(blur, 1), Some(mask));
    }

    #[test]
    fn demo_nodes_do_not_overlap() {
        let g = demo_graph();
        let nodes: Vec<_> = g.nodes().collect();
        for (i, a) in nodes.iter().enumerate() {
            for b in &nodes[i + 1..] {
                let dx = (a.pos[0] - b.pos[0]).abs();
                let dy = (a.pos[1] - b.pos[1]).abs();
                assert!(dx >= 215.0 || dy >= 130.0, "{:?} overlaps {:?}", a.kind, b.kind);
            }
        }
    }

    #[test]
    fn last_source_can_be_deleted_and_clears_the_viewer_pin() {
        let mut s = demo_session();
        let source = s.node_by_index(0).unwrap();
        s.selected_node = Some(source);
        s.viewed_node = Some(source);
        assert!(s.delete_node(source).is_ok());
        assert!(s.project.graph.node(source).is_none());
        assert_eq!(s.viewed_node, None);
        assert_eq!(s.target_node(), None);
    }

    #[test]
    fn target_follows_selection_and_pin_is_session_only() {
        let mut s = demo_session();
        let exp = s.node_by_index(4).unwrap();
        let src = s.node_by_index(0).unwrap();
        assert_eq!(s.target_node(), None);
        s.selected_node = Some(exp);
        assert_eq!(s.target_node(), Some(exp));
        s.viewed_node = Some(exp);
        s.selected_node = Some(src);
        assert_eq!(s.target_node(), Some(exp));
        s.project.graph.remove_node(exp);
        s.sanitize();
        assert_eq!(s.viewed_node, None);
        assert_eq!(s.target_node(), Some(src));
        s.selected_node = None;
        assert_eq!(s.target_node(), None);
        s.viewed_node = Some(src);
        s.new_project();
        assert_eq!(s.target_node(), None);
        assert_eq!(s.viewed_node, None);
    }

    #[test]
    fn demo_images_are_listed_and_stepping_is_clamped() {
        let mut s = demo_session();
        assert_eq!(s.visible().len(), 3);
        s.step_image(-1);
        assert_eq!(s.selected_index(), Some(0));
        s.step_image(5);
        assert_eq!(s.selected_index(), Some(2));
        s.select_index(1);
        assert_eq!(s.current_image(), ImageId(2));
    }

    #[test]
    fn stepping_follows_the_filtered_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 6), provider());
        let ids: Vec<ImageId> = s.visible().to_vec();
        s.set_rating(ids[1], 4);
        s.set_rating(ids[4], 5);
        s.filter.min_rating = 4;
        s.refresh_visible();
        assert_eq!(s.visible(), &[ids[1], ids[4]]);
        assert_eq!(s.selected, Some(ids[1]), "selection moves into the filtered list");
        s.step_image(1);
        assert_eq!(s.selected, Some(ids[4]));
        s.step_image(1);
        assert_eq!(s.selected, Some(ids[4]), "clamped at the end");
        assert_eq!(s.neighbours(), vec![ids[1]]);
        s.filter.sort = SortKey::Rating;
        s.filter.descending = true;
        s.refresh_visible();
        assert_eq!(s.visible(), &[ids[4], ids[1]]);
    }

    #[test]
    fn viewer_wheel_stays_in_the_pinned_nodes_filtered_collection() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let mut project = png_project(&a, 3);
        std::fs::copy(a.join("img_000.png"), b.join("other.png")).unwrap();
        project.scan_folder(&b, false, &rawgraph_io::is_supported, &|_| None);
        let mut s = Session::new(project, provider());
        let source = s.project.graph.add_node(NodeKind::Source, [0.0; 2]);
        crate::node_ui::set_source_folder(&mut s.project.graph, source, Some(a.to_str().unwrap().to_owned()));
        let generator = s.project.graph.add_node(NodeKind::Generator, [0.0; 2]);
        s.selected_node = Some(generator);
        s.viewed_node = Some(source);
        let (ids, captures) = s.id_captures();
        let members = crate::collection::view_members(&s.project.graph, &ids, &captures, source);
        let order = s.gallery_images(&members);
        assert_eq!(order.len(), 3);
        s.selected = s.visible().iter().copied().find(|id| !members.contains(id));
        assert!(s.selected.is_some());
        s.step_viewer_image(1);
        assert_eq!(s.selected, Some(order[0]));
        s.step_viewer_image(1);
        assert_eq!(s.selected, Some(order[1]));
        s.step_viewer_image(10);
        assert_eq!(s.selected, Some(order[2]));
        s.step_viewer_image(1);
        assert_eq!(s.selected, Some(order[2]));
    }

    #[test]
    fn viewer_wheel_recomputes_membership_and_respects_empty_node() {
        let mut s = demo_session();
        let original = s.selected;
        s.selected_node = None;
        s.step_viewer_image(1);
        assert_eq!(s.selected, original);
        s.project.graph = default_graph();
        let source = s.node_by_index(0).unwrap();
        s.selected_node = Some(source);
        // An explicitly empty Source must not use stale cached membership.
        s.collection.view_node = Some(source);
        s.collection.view_members = s.visible().iter().copied().collect();
        s.step_viewer_image(1);
        assert_eq!(s.selected, original);
    }

    #[test]
    fn culling_edits_the_right_entry_and_marks_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 3), provider());
        let ids: Vec<ImageId> = s.visible().to_vec();
        assert!(!s.dirty);
        assert!(s.set_rating(ids[1], 3));
        assert!(s.dirty);
        assert!(s.set_flag(ids[2], Flag::Reject));
        assert!(s.toggle_label(ids[0], ColorLabel::Green));
        assert_eq!(s.entry(ids[1]).unwrap().rating, 3);
        assert_eq!(s.entry(ids[0]).unwrap().rating, 0);
        assert_eq!(s.entry(ids[2]).unwrap().flag, Flag::Reject);
        assert_eq!(s.entry(ids[0]).unwrap().color_label, Some(ColorLabel::Green));
        s.toggle_label(ids[0], ColorLabel::Green);
        assert_eq!(s.entry(ids[0]).unwrap().color_label, None, "same label again clears it");
        assert!(s.set_rating(ids[0], 9));
        assert_eq!(s.entry(ids[0]).unwrap().rating, 5, "rating is clamped");
    }

    #[test]
    fn filmstrip_dims_images_outside_the_viewed_collection() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 4), provider());
        let ids = s.visible().to_vec();
        let out = s.node_by_index(0).unwrap();
        s.selected_node = Some(out);
        assert_eq!(s.in_view(ids[0]), None, "unknown until the collection is computed");
        assert_eq!(s.in_view_count(), None);
        s.collection.view_node = Some(out);
        s.collection.view_members = [ids[0], ids[2]].into_iter().collect();
        assert_eq!(s.in_view(ids[0]), Some(true));
        assert_eq!(s.in_view(ids[1]), Some(false));
        assert_eq!(s.in_view_count(), Some(2));
        // The result belongs to another node: not applicable to the viewed one.
        let src = s.node_by_index(1).unwrap();
        s.viewed_node = Some(src);
        assert_eq!(s.in_view(ids[1]), None);
    }

    #[test]
    fn new_projects_start_with_the_default_graph() {
        let g = default_graph();
        assert!(g.validate().is_empty());
        let kinds: Vec<NodeKind> = g.nodes().map(|n| n.kind).collect();
        assert_eq!(
            kinds,
            [NodeKind::Source, NodeKind::Develop, NodeKind::Exposure, NodeKind::Contrast, NodeKind::HighlightsShadows, NodeKind::Saturation]
        );
        // A plain chain: every node is fed by the one before it.
        let ids: Vec<NodeId> = g.nodes().map(|n| n.id).collect();
        assert!(matches!(g.node(ids[0]).unwrap().data(ImageId(0)), Some(rawgraph_core::NodeData::Folder(f)) if f.is_empty()));
        assert!(!g.contains_in(ids[0], ImageId(42), &rawgraph_core::Catalog { images: &[ImageId(42)], capture: &|_| None }));
        assert_eq!(g.edges().len(), ids.len() - 1);
        for pair in ids.windows(2) {
            assert_eq!(g.input_of(pair[1], 0), Some(pair[0]));
        }
        for n in g.nodes() {
            for spec in n.kind.params() {
                assert_eq!(n.param(spec.name, ImageId(1)), spec.default, "{:?} {} is neutral", n.kind, spec.name);
            }
        }
        // Develop starts with the as-shot white balance and the baseline on.
        let dev = g.nodes().find(|n| n.kind == NodeKind::Develop).unwrap();
        assert_eq!(dev.choice("wb", ImageId(1)), 0, "As shot");
        assert!(dev.toggle("baseline", ImageId(1)), "baseline exposure on");
        assert!(demo_graph_neutral().nodes().nth(1).is_some_and(|n| n.kind == NodeKind::Develop));
        let mut s = demo_session();
        s.new_project();
        assert_eq!(s.project.graph, default_graph());
        assert!(!s.demo && !s.dirty);
        let blur = demo_graph_neutral().nodes().find(|n| n.kind == NodeKind::Blur).map(|n| n.param("radius", ImageId(1)));
        assert_eq!(blur, Some(0.0), "the demo graph does not blur real photos");
    }

    #[test]
    fn demo_images_cannot_be_rated() {
        let mut s = demo_session();
        let id = s.visible()[0];
        assert!(!s.set_rating(id, 3));
        assert!(!s.dirty);
    }

    #[test]
    fn unsaved_state_across_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 2), provider());
        let ids: Vec<ImageId> = s.visible().to_vec();
        s.set_rating(ids[0], 2);
        assert!(s.dirty);
        assert!(s.window_title().contains('*'));
        let file = dir.path().join("p.rawgraph");
        assert_eq!(s.commit_project(&file, false).unwrap(), 1);
        assert!(!s.dirty && !s.window_title().contains('*'));
        assert_eq!(s.project_name(), "p");
        let mut t = Session::new(Project::new(), provider());
        t.load_project(&file).unwrap();
        assert!(!t.dirty);
        assert_eq!(t.visible().len(), 2);
        assert_eq!(t.entry(ids[0]).unwrap().rating, 2);
        assert!(t.load_project(&dir.path().join("missing.rawgraph")).is_err());
    }

    #[test]
    fn scan_results_show_up_progressively_and_edits_survive_the_swap() {
        let dir = tempfile::tempdir().unwrap();
        let finished = png_project(dir.path(), 3);
        let mut s = Session::new(Project::new(), provider());
        s.begin_scan();
        let entries: Vec<ImageEntry> = finished.images().to_vec();
        s.scan_entry(entries[0].clone());
        assert_eq!(s.visible().len(), 1);
        s.scan_entry(entries[1].clone());
        assert_eq!(s.visible().len(), 2);
        assert_eq!(s.selected, Some(entries[0].id));
        s.set_rating(entries[1].id, 5);
        s.scan_entry(entries[2].clone());
        assert_eq!(s.total_images(), 3);
        s.finish_scan(finished, true);
        assert!(!s.scanning && s.dirty);
        assert_eq!(s.visible().len(), 3);
        assert_eq!(s.entry(entries[1].id).unwrap().rating, 5, "an edit made during the scan is kept");
    }

    #[test]
    fn capture_refresh_updates_entries_and_marks_the_project_unsaved() {
        let dir = tempfile::tempdir().unwrap();
        let mut project = png_project(dir.path(), 3);
        let ids: Vec<ImageId> = project.images().iter().map(|e| e.id).collect();
        // Old project file: the new fields are missing.
        for id in &ids {
            project.image_mut(*id).unwrap().capture.raw = None;
        }
        let mut s = Session::new(project, provider());
        assert_eq!(s.capture_refresh_targets().len(), 3);
        assert!(!s.dirty);
        let before = s.images_gen;
        let mut job = crate::capture_refresh::CaptureRefresh::start(eframe::egui::Context::default(), s.capture_refresh_targets());
        let started = std::time::Instant::now();
        let mut changed = 0;
        loop {
            let (items, finished) = job.poll();
            changed += s.apply_capture_updates(items);
            if finished || started.elapsed().as_secs() > 20 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(changed, 3);
        assert!(s.dirty, "the project is marked unsaved");
        assert!(s.images_gen > before, "collection info and renders are refreshed");
        assert!(s.capture_refresh_targets().is_empty());
        assert!(s.entry(ids[0]).unwrap().capture.raw.is_some());
        use rawgraph_engine::ImageProvider;
        assert_eq!(s.provider.capture(ids[1]).and_then(|c| c.raw), Some(false), "the provider sees the new metadata");
        // Applying the same data again changes nothing.
        let again = s.project.images().iter().map(|e| (e.id, e.capture.clone())).collect();
        assert_eq!(s.apply_capture_updates(again), 0);
    }

    #[test]
    fn missing_files_are_marked_by_the_check_and_can_be_listed_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new(png_project(dir.path(), 4), provider());
        let ids = s.visible().to_vec();
        std::fs::remove_file(s.entry(ids[1]).unwrap().path.clone()).unwrap();
        let updates: Vec<(ImageId, bool)> = s.id_paths().into_iter().map(|(id, p)| (id, !p.is_file())).collect();
        assert_eq!(s.apply_missing(&updates), 1);
        assert_eq!(s.apply_missing(&updates), 0, "nothing new the second time");
        assert!(s.entry(ids[1]).unwrap().missing && !s.dirty, "a derived flag does not make the project unsaved");
        assert_eq!(s.visible().len(), 4, "missing files stay listed with a marker");
        s.filter.missing_only = true;
        s.refresh_visible();
        assert_eq!(s.visible(), &[ids[1]]);
        assert!(s.filter.is_active(), "Clear resets it");
        assert_eq!(crate::session::tests::demo_session().apply_missing(&updates), 0);
    }

    #[test]
    fn the_node_selection_follows_direct_changes_of_the_primary_node() {
        let mut s = demo_session();
        let (a, b, c) = (s.node_by_index(1).unwrap(), s.node_by_index(2).unwrap(), s.node_by_index(3).unwrap());
        let mut sel = NodeSelection::default();
        sel.replace([a, b, c]);
        s.set_node_selection(sel);
        assert_eq!((s.selected_node, s.node_sel.len()), (Some(c), 3));
        s.reconcile_selection();
        assert_eq!(s.node_sel.len(), 3, "consistent state is left alone");
        // Code that sets the primary node directly (CLI, inspector) collapses the selection to it.
        s.selected_node = Some(a);
        s.sanitize();
        assert_eq!(s.node_sel.ids(), &[a]);
        s.selected_node = None;
        s.sanitize();
        assert!(s.node_sel.is_empty());
        // Removing a selected node drops it from the selection.
        let mut sel = NodeSelection::default();
        sel.replace([a, b]);
        s.set_node_selection(sel);
        s.project.graph.remove_node(a);
        s.sanitize();
        assert_eq!(s.node_sel.ids(), &[b]);
        assert_eq!(s.selected_node, Some(b));
    }

    #[test]
    fn deleting_a_selection_can_remove_the_entire_graph() {
        let mut s = demo_session();
        let ids: Vec<NodeId> = (1..=3).map(|i| s.node_by_index(i).unwrap()).collect();
        let before = s.project.graph.nodes().count();
        assert_eq!(s.delete_nodes(&ids), Ok(3));
        assert_eq!(s.project.graph.nodes().count(), before - 3);
        let all: Vec<NodeId> = s.project.graph.nodes().map(|n| n.id).collect();
        assert_eq!(s.delete_nodes(&all), Ok(all.len()));
        assert_eq!(s.project.graph.nodes().count(), 0);
        assert!(s.project.graph.validate().is_empty());
        assert_eq!(s.render_target(), None);
        assert_eq!(s.delete_nodes(&[]), Ok(0));
    }

    #[test]
    fn counts_include_images_found_by_a_running_scan() {
        let dir = tempfile::tempdir().unwrap();
        let finished = png_project(dir.path(), 4);
        let mut s = Session::new(Project::new(), provider());
        s.begin_scan();
        let entries: Vec<ImageEntry> = finished.images().to_vec();
        s.scan_entry(entries[0].clone());
        s.scan_entry(entries[1].clone());
        let (ids, caps) = s.id_captures();
        assert_eq!((ids.len(), caps.len(), s.scan_found()), (2, 2, 2));
        s.scan_entry(entries[2].clone());
        assert_eq!(s.id_captures().0.len(), 3, "not cached while scanning");
    }

    #[test]
    fn saving_during_a_scan_writes_what_was_found_and_keeps_the_revision_straight() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let scanned = png_project(dir.path(), 3);
        let entries: Vec<ImageEntry> = scanned.images().to_vec();
        // What the scan thread works on: a clone of the session's project.
        let base = Project::new();
        let mut finished = base.clone();
        finished.merge_found(&entries);
        let mut s = Session::new(base, provider());
        s.begin_scan();
        s.scan_folder = Some((dir.path().to_owned(), true));
        s.scan_entry(entries[0].clone());
        s.scan_entry(entries[1].clone());
        s.set_rating(entries[1].id, 4);
        let file = out.path().join("p.rawgraph");
        assert!(s.dirty);
        assert_eq!(s.commit_project(&file, false).unwrap(), 1, "saving is allowed while the scan runs");
        assert!(!s.dirty && s.scanning);
        let saved = Project::load(&file).unwrap();
        assert_eq!(saved.images().len(), 2, "the images found so far are in the file");
        assert_eq!(saved.image(entries[1].id).unwrap().rating, 4);
        assert_eq!(saved.folders().len(), 1, "and the folder that is being scanned");
        // A second save while it still runs.
        s.scan_entry(entries[2].clone());
        assert_eq!(s.commit_project(&file, false).unwrap(), 2);
        assert_eq!(Project::load(&file).unwrap().images().len(), 3);
        // The scan returns its copy, which knows nothing about the two saves.
        assert_eq!(finished.revision(), 0);
        s.finish_scan(finished, true);
        assert_eq!((s.project.revision(), s.project.history().len()), (2, 2), "revision and history carry over");
        assert!(s.dirty, "what the scan did after the last save is unsaved");
        // The next save is no false conflict and extends the history.
        assert_eq!(s.commit_project(&file, false).unwrap(), 3);
        let loaded = Project::load(&file).unwrap();
        assert_eq!((loaded.images().len(), loaded.history().len(), loaded.revision()), (3, 3, 3));
    }

    #[test]
    fn a_real_scan_can_be_saved_in_the_middle_and_merges_in_afterwards() {
        use crate::scan::{ScanJob, ScanMsg};
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        for i in 0..6 {
            image::RgbaImage::from_pixel(16, 16, image::Rgba([i * 20, 50, 90, 255])).save(dir.path().join(format!("p{i}.png"))).unwrap();
        }
        let mut s = Session::new(Project::new(), provider());
        s.begin_scan();
        s.scan_folder = Some((dir.path().to_owned(), true));
        let job = ScanJob::start(eframe::egui::Context::default(), s.project.clone(), dir.path().to_owned(), true);
        let file = out.path().join("p.rawgraph");
        let mut saved_at_catalog = false;
        let finished = loop {
            match job.rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap() {
                ScanMsg::Catalog(entries) => {
                    s.scan_catalog(entries);
                    // Save with the catalog only (hashes and metadata still pending).
                    assert_eq!(s.commit_project(&file, false).unwrap(), 1);
                    assert_eq!(Project::load(&file).unwrap().images().len(), 6);
                    saved_at_catalog = true;
                }
                ScanMsg::Hashed(entries) => s.scan_entries(entries),
                ScanMsg::Metadata { entries, .. } => s.scan_metadata(entries),
                ScanMsg::Done { project, .. } => break *project,
                ScanMsg::Listing(_) => {}
            }
        };
        assert!(saved_at_catalog);
        s.finish_scan(finished, true);
        assert_eq!(s.project.revision(), 1);
        assert!(s.project.images().iter().all(|e| !e.content_hash.is_empty() && !e.capture.scan_pending));
        assert_eq!(s.commit_project(&file, false).unwrap(), 2, "no false conflict");
        let loaded = Project::load(&file).unwrap();
        assert!(loaded.images().iter().all(|e| !e.content_hash.is_empty()));
        assert_eq!(loaded.history().len(), 2);
    }

    #[test]
    fn a_recovery_copy_made_during_a_scan_contains_the_images_found_so_far() {
        let dir = tempfile::tempdir().unwrap();
        let finished = png_project(dir.path(), 3);
        let mut s = Session::new(Project::new(), provider());
        s.begin_scan();
        s.scan_entry(finished.images()[0].clone());
        s.scan_entry(finished.images()[1].clone());
        let copy = s.saveable_project();
        assert_eq!(copy.images().len(), 2);
        assert_eq!(s.project.images().len(), 0, "making the copy does not change the session");
        let rec = tempfile::tempdir().unwrap();
        let file = copy.save_recovery(rec.path(), None, "session").unwrap();
        assert_eq!(rawgraph_project::load_recovery(&file).unwrap().0.images().len(), 2);
    }

    /// A session whose scan (a clone of the session's project) has found everything in `entries`.
    fn scanning_session(dir: &Path, n: usize) -> (Session, Vec<ImageEntry>, Project) {
        let scanned = png_project(dir, n);
        let entries: Vec<ImageEntry> = scanned.images().to_vec();
        let base = Project::new();
        let mut finished = base.clone();
        finished.merge_found(&entries);
        finished.note_scanned_folder(dir, true);
        let mut s = Session::new(base, provider());
        s.begin_scan();
        s.scan_folder = Some((dir.to_owned(), true));
        (s, entries, finished)
    }

    #[test]
    fn a_failed_save_during_a_scan_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (mut s, entries, _) = scanning_session(dir.path(), 3);
        let file = out.path().join("p.rawgraph");
        // Somebody else saved this project first: our copy is behind.
        let mut other = s.project.clone();
        other.commit(&file, "theirs").unwrap();
        s.scan_entry(entries[0].clone());
        s.scan_entry(entries[1].clone());
        s.set_rating(entries[0].id, 3);
        let before = s.project.clone();
        let err = s.commit_project(&file, false).unwrap_err();
        assert!(matches!(err, ProjectError::Conflict { .. }), "{err}");
        assert_eq!(s.project, before, "the images found so far were not taken into the project");
        assert_eq!(s.project.images().len(), 0);
        assert!(s.dirty && s.scanning && s.project_path.is_none());
        // The scan can still finish and the user can decide about the conflict later.
        assert_eq!(s.scan_found(), 2);
    }

    #[test]
    fn a_scan_that_brings_nothing_new_after_the_save_leaves_the_project_saved() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (mut s, entries, finished) = scanning_session(dir.path(), 3);
        for e in &entries {
            s.scan_entry(e.clone());
        }
        s.set_rating(entries[1].id, 4);
        s.set_rating(entries[2].id, 2);
        s.toggle_label(entries[2].id, ColorLabel::Red);
        let mut graph_edit = s.project.graph.clone();
        graph_edit.add_node(NodeKind::Contrast, [0.0, 0.0]);
        s.set_graph(graph_edit);
        let file = out.path().join("p.rawgraph");
        assert_eq!(s.commit_project(&file, false).unwrap(), 1);
        assert!(!s.dirty);
        s.finish_scan(finished, false);
        assert!(!s.dirty, "everything the scan delivered was in the saved file already");
        assert_eq!(s.project.revision(), 1, "no extra revision");
        assert_eq!(s.project.image(entries[1].id).unwrap().rating, 4);
        assert_eq!(s.project.image(entries[2].id).unwrap().color_label, Some(ColorLabel::Red));
        assert_eq!(Project::load(&file).unwrap(), s.project);
    }

    #[test]
    fn cancelling_after_a_save_keeps_what_was_saved_with_pending_entries_for_the_next_scan() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let (mut s, entries, _) = scanning_session(dir.path(), 3);
        // The catalog arrived (entries without hash, capture pending), nothing hashed yet.
        let pending: Vec<ImageEntry> = entries
            .iter()
            .cloned()
            .map(|mut e| {
                e.content_hash.clear();
                e.capture = rawgraph_core::CaptureInfo { scan_pending: true, ..Default::default() };
                e
            })
            .collect();
        s.scan_catalog(pending.clone());
        let file = out.path().join("p.rawgraph");
        s.commit_project(&file, false).unwrap();
        s.cancel_scan_state();
        assert!(!s.scanning);
        assert_eq!(s.project.images().len(), 3, "what was saved stays in the session");
        assert!(s.project.images().iter().all(|e| e.content_hash.is_empty() && e.capture.scan_pending), "marked pending");
        let saved = Project::load(&file).unwrap();
        assert_eq!(saved, s.project);
        assert_eq!(saved.folders().len(), 1);
        // The next scan of the folder reads the pending files (hash and metadata), and heals them.
        let mut healed = saved;
        let report = healed.scan_folder(dir.path(), true, &rawgraph_io::is_supported, &|p| rawgraph_io::read_capture_info(p).ok());
        assert_eq!(report.added.len(), 0, "the saved entries are recognised by path");
        assert!(healed.images().iter().all(|e| !e.content_hash.is_empty() && !e.capture.scan_pending));
    }

    #[test]
    fn saving_during_a_rescan_does_not_write_emptied_hashes_over_good_ones() {
        let dir = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let project = png_project(dir.path(), 3);
        let good: Vec<ImageEntry> = project.images().to_vec();
        let mut s = Session::new(project, provider());
        s.begin_scan();
        s.scan_folder = Some((dir.path().to_owned(), true));
        // The rescan found file 2 changed: its entry arrives with an empty hash and a pending capture.
        let mut changed = good[1].clone();
        changed.content_hash.clear();
        changed.file_size += 1;
        changed.capture = rawgraph_core::CaptureInfo { scan_pending: true, ..Default::default() };
        s.scan_catalog(vec![good[0].clone(), changed, good[2].clone()]);
        let file = out.path().join("p.rawgraph");
        s.commit_project(&file, false).unwrap();
        let saved = Project::load(&file).unwrap();
        assert_eq!(saved.image(good[1].id).unwrap(), &good[1], "the saved entry keeps its good hash and capture");
        assert_eq!(saved.images().len(), 3);
    }
}
