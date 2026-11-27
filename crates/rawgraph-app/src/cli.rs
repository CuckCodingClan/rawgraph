//! Command line parsing (no external parser, no panics on bad input).

use std::path::PathBuf;

use crate::app::{ScopesPos, Workspace};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cli {
    pub demo: bool,
    pub screenshot: Option<PathBuf>,
    pub select_node: Option<usize>,
    pub select_image: Option<usize>,
    pub view_node: Option<usize>,
    pub individual: Vec<usize>,
    /// `(node index, parameter name, value)`
    pub set: Vec<(usize, String, f32)>,
    /// `(node index, input port)`: removes the connection (test states).
    pub disconnect: Vec<(usize, usize)>,
    /// Like `disconnect`, but applied after the first preview arrived
    /// (shows the dimmed last frame behind the error).
    pub late_disconnect: Vec<(usize, usize)>,
    pub workspace: Option<Workspace>,
    pub clean_feed: bool,
    /// Show secondary windows embedded in the main window (for screenshots).
    pub embed_windows: bool,
    /// Initial window size in points.
    pub size: Option<(f32, f32)>,
    /// Forces `pixels_per_point` (reproducible screenshots).
    pub scale: Option<f32>,
    pub help: bool,
    pub open_folder: Option<PathBuf>,
    pub project: Option<PathBuf>,
    pub save_project: Option<PathBuf>,
    /// `(image index in the filtered list, rating)`
    pub rate: Vec<(usize, u8)>,
    /// Inclusive ISO bounds for the filmstrip filter.
    pub filter_iso: Option<(Option<f32>, Option<f32>)>,
    pub preview_size: Option<u32>,
    /// Automated timing run (stepping through all images), prints `BENCH` lines.
    pub bench: bool,
    /// Do not descend into sub-folders when scanning.
    pub no_recursive: bool,
    /// Show the viewer at 100 % (noise and detail checks).
    pub actual_size: bool,
    /// Nodes to add, with an optional position.
    pub add_node: Vec<(rawgraph_core::NodeKind, Option<[f32; 2]>)>,
    /// `(from node, to node, input port)` by index (first output port).
    pub connect: Vec<(usize, usize, usize)>,
    /// `(from node, output port, to node, input port)`: `--connect <from>.<out>:<to>:<in>`.
    pub connect_ports: Vec<(usize, usize, usize, usize)>,
    /// `(node, ranges)`: sets the ranges of a Split by Metadata for the selected image.
    pub split_ranges: Vec<(usize, Vec<rawgraph_core::NamedRange>)>,
    /// Brush strokes for the selected image: `(node, points, Some([size %, hardness, flow]))`.
    pub paint: Vec<PaintArg>,
    /// Do not embed the sRGB profile in exported files.
    pub export_no_profile: bool,
    /// Do not copy capture metadata into exported files.
    pub export_no_metadata: bool,
    /// Timing run of the paint and curve tools (prints `BENCH` lines).
    pub bench_tools: bool,
    /// `--no-gpu-preload`: do not upload neighbour images to the GPU in the background (measuring).
    pub no_gpu_preload: bool,
    /// `(node, x, y)`: moves a node in the graph editor.
    pub move_node: Vec<(usize, f32, f32)>,
    /// Nodes to delete once everything else is applied (indices as they were before).
    pub delete_node: Vec<usize>,
    /// `(node, points)`: curve of a Curve node for the selected image.
    pub curve: Vec<(usize, Vec<[f32; 2]>)>,
    /// `(node, channel, points)`: one channel of a Curves node (`--curve <node>:<channel>:<points>`).
    pub curves: Vec<(usize, u8, Vec<[f32; 2]>)>,
    /// `(node, folder)`: the folder of a Source node.
    pub set_folder: Vec<(usize, PathBuf)>,
    /// `(node, field, value)`: a text field of an Export node.
    pub set_text: Vec<(usize, String, String)>,
    /// Runs this Export node for the filtered images after start-up, prints the report and exits.
    pub run_export_node: Option<usize>,
    /// Opens the export dialog in node mode for this Export node at start (does not export).
    pub open_node_export: Option<usize>,
    /// Opens the "Run Export nodes" dialog at start.
    pub open_run_nodes: bool,
    /// Scrolls the inspector to this offset in points (screenshots of long inspectors).
    pub inspector_scroll: Option<f32>,
    /// Where the scopes live at start.
    pub scopes: Option<ScopesPos>,
    /// Export after the start-up actions into this folder.
    pub export: Option<PathBuf>,
    pub export_format: Option<crate::export::Fmt>,
    pub export_long_edge: Option<u32>,
    pub export_scope: Option<crate::export::Scope>,
    pub open_export_dialog: bool,
    /// Pre-fills the output folder of the export dialog (does not export).
    pub export_folder: Option<PathBuf>,
    /// Use the demo graph (Capture EV, White Balance, Exposure, ...) in the project.
    pub demo_graph: bool,
    /// Start with View > Show mask overlay switched on.
    pub mask_overlay: bool,
    /// `(node index, absolute EV100)`: sets a Capture EV node like typing into the inspector field.
    pub set_ev: Vec<(usize, f32)>,
    /// `(image index, flag)`
    pub flag: Vec<(usize, rawgraph_project::Flag)>,
    /// `(image index, label)`; `None` clears the label.
    pub label: Vec<(usize, Option<rawgraph_project::ColorLabel>)>,
    /// Redirects recovery files, the thumbnail cache and recipes.
    pub data_dir: Option<PathBuf>,
    /// Write a recovery copy and exit at once, without the clean-quit cleanup.
    pub simulate_crash: bool,
    /// Recover the newest crashed session at start without asking.
    pub recover_latest: bool,
    /// Node indices to select at start (the last one is the primary node).
    pub select_nodes: Vec<usize>,
    /// Copy the selected nodes (to the editor's own clipboard only) at start.
    pub copy_selection: bool,
    /// Paste what was copied (needs `--copy-selection`) once, offset from the originals.
    pub paste_selection: bool,
    /// Save the selected nodes as a recipe with this name.
    pub save_recipe: Vec<String>,
    /// Open the "Manage recipes" window at start.
    pub open_recipes: bool,
    /// Run "Import ratings from XMP" with the dialog defaults after the scan and show its dialog.
    pub import_xmp: bool,
    /// Open the XMP import dialog at start (does not import).
    pub open_xmp_dialog: bool,
    /// Show only missing files in the filmstrip.
    pub missing_only: bool,
    /// Open the "Relink folder" dialog for the (missing) image with this index.
    pub relink_dialog: Option<usize>,
    /// Pre-fills the new folder of the relink dialog.
    pub relink_new: Option<PathBuf>,
    /// Compare view at start.
    pub compare: bool,
    /// Filmstrip filter: at least this many stars.
    pub min_rating: Option<u8>,
    /// Seconds between recovery copies (default 60; for tests).
    pub autosave_interval: Option<u64>,
    /// Image index (in the filtered list) used as the compare reference.
    pub reference: Option<usize>,
    /// `--renderer auto|cpu|gpu`; `None` keeps the saved setting.
    pub renderer: Option<crate::gpu_path::RendererMode>,
    /// Writes the viewer picture itself (not a window screenshot) as a PNG, then exits.
    pub dump_frame: Option<PathBuf>,
    /// Hidden: injects a GPU failure (`oom`, `lost`, `other`) to test the fallbacks.
    pub gpu_fail: Option<crate::gpu_path::GpuFailure>,
    /// Open the History window at start.
    pub open_history: bool,
    /// Select this revision in the History window (opens it).
    pub select_revision: Option<u64>,
    /// Before `--save-project`: commit a change to the project file through a second `Project`
    /// value, as if somebody else had saved since this copy was opened.
    pub simulate_conflict: bool,
}

/// One `--paint` option: node index, normalised points, optional `[size %, hardness, flow]`.
pub type PaintArg = (usize, Vec<[f32; 2]>, Option<[f32; 3]>);

pub const USAGE: &str = "\
Usage: rawgraph-app [options]
  --demo                       start with the demo graph (default)
  --screenshot <path.png>      render, save a screenshot of the main window, exit
  --select-node <n>            select node n (0-based, creation order)
  --select-image <n>           select image n (0-based)
  --view-node <n>              view node n in the viewer
  --individual <n>             switch node n to Individual mode (repeatable)
  --set <node>:<param>=<value> set a parameter for the selected image (repeatable)
  --disconnect <node>:<port>   remove the connection into an input (repeatable)
  --late-disconnect <n>:<port> like --disconnect, but after the first preview arrived
  --open-folder <path>         scan a folder into the project at start
  --project <file.rawgraph>    open a project at start
  --save-project <file>        save the project as a new revision after the start-up actions
  --rate <image>:<n>           rate image (index in the list) 0-5 (repeatable)
  --filter-iso <min>:<max>     ISO filter, either side may be empty
  --preview-size <n>           preview long edge (1280, 1920, 2560, 3840)
  --no-recursive               do not scan sub-folders
  --demo-graph                 use the demo graph in the project
  --mask-overlay               start with the mask overlay switched on
  --set-ev <node>:<ev100>      set a Capture EV node so the image's absolute EV100 matches
  --flag <image>:pick|reject|none   set a flag (repeatable)
  --label <image>:red|yellow|green|blue|purple|none   set a colour label (repeatable)
  --export <folder>            export after start-up (see --export-format/-long-edge/-scope)
  --export-format jpg|png|tif|exr|png16|tif16
  --export-no-profile          do not embed the sRGB profile
  --export-no-metadata         do not copy capture metadata
  --export-long-edge <n>
  --export-scope selected|filtered|all
  --data-dir <folder>          keep recovery files, thumbnail cache and recipes here
  --simulate-crash             write a recovery copy and exit without the clean-quit cleanup
  --recover-latest             recover the newest crashed session at start
  --select-nodes <a,b,c>       select nodes by index (the last one is the primary node)
  --copy-selection             copy the selected nodes (internal clipboard only)
  --paste-selection            paste them once, offset from the originals
  --save-recipe <name>         save the selected nodes as a recipe (repeatable)
  --open-recipes               show the Manage recipes window
  --import-xmp                 import ratings from XMP after the scan, then show the dialog
  --open-xmp-dialog            show the XMP import dialog (does not import)
  --missing-only               show only missing files in the filmstrip
  --relink-dialog <image>      open the Relink folder dialog for a missing image
  --relink-new <folder>        pre-fill the new folder of that dialog
  --compare                    start in the compare view
  --min-rating <0-5>           filmstrip filter: at least this many stars
  --autosave-interval <secs>   seconds between recovery copies (default 60)
  --reference <image>          image (index in the list) used as the compare reference
  --export-folder <folder>     pre-fill the export dialog's folder
  --open-export-dialog         show the export dialog at start
  --open-history               show the History window at start
  --select-revision <n>        select revision n in the History window (opens it)
  --simulate-conflict          before --save-project: save a change through a second copy of the
                               project file, so the save finds a conflict
  --add-node <Kind>[@x,y]      add a node (Contrast, Highlights, Noise, Filter, Merge, ...)
  --connect <from>[.<out>]:<to>:<port>  connect node indices (output port 0 unless given)
  --split-ranges <node>:<name>=<min>..<max>[,...]  ranges of a Split by Metadata (selected image)
  --paint <node>:<x0>,<y0>;<x1>,<y1>;...[:<size>,<hardness>,<flow>]  add a brush stroke
                               (normalised coordinates, size in % of the long edge; repeatable)
  --move-node <n>:<x>,<y>      move node n in the graph editor (repeatable)
  --delete-node <n>            delete node n after all other start-up actions (repeatable)
  --curve <node>:<x>,<y>;<x>,<y>;...  set the points of a Curve node (selected image)
  --curve <node>:<channel>:<x>,<y>;...  one channel of a Curves node (0-6 or lum, red, green, blue,
                               satsat, huehue, huesat); that tab is shown
  --set-folder <node>:<path>   the folder of a Source node (scanned into the project if needed)
  --set-text <node>:<field>=<value>  a text field of an Export node: folder, name, artist,
                               copyright or description
  --run-export-node <node>     run that Export node for the filtered images after start-up, print
                               the report and exit
  --open-node-export <node>    open the export dialog in node mode for that Export node
  --open-run-nodes             open the Run Export nodes dialog
  --inspector-scroll <points>  scroll the inspector (screenshots of long inspectors)
  --scopes bottom|right|window|hidden  where the scopes live
  --bench-tools                timing run of the paint and curve tools, then exit
  --no-gpu-preload             do not upload the neighbour images to the GPU in the background
  --actual-size                show the viewer at 100 %
  --bench                      timing run over all images, then exit
  --renderer auto|cpu|gpu      preview renderer (gpu: fail instead of falling back; for tests)
  --dump-frame <path.png>      write the viewer picture (GPU or CPU frame) as a PNG, then exit
  --workspace single|dual      workspace layout
  --embed-windows              show secondary windows inside the main window (screenshots)
  --clean-feed                 open the clean feed window
  --size <W>x<H>               window size in points
  --scale <factor>             force pixels per point
  -h, --help                   show this text";

fn parse_index(flag: &str, value: Option<String>) -> Result<usize, String> {
    let v = value.ok_or_else(|| format!("{flag} needs a value"))?;
    v.trim().parse::<usize>().map_err(|_| format!("{flag}: \"{v}\" is not a non-negative integer"))
}

/// `1,2,3` as node or image indices.
fn parse_index_list(value: Option<String>) -> Result<Vec<usize>, String> {
    let v = value.ok_or("--select-nodes needs a list like 1,2,3")?;
    let bad = || format!("--select-nodes: \"{v}\" is not a comma-separated list of indices");
    let list: Result<Vec<usize>, String> = v.split(',').map(|p| p.trim().parse::<usize>().map_err(|_| bad())).collect();
    let list = list?;
    if list.is_empty() {
        return Err(bad());
    }
    Ok(list)
}

/// A curve channel by number (0-6) or by name.
fn parse_curve_channel(text: &str) -> Option<u8> {
    let t: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
    if let Ok(n) = t.parse::<u8>() {
        return (n < 7).then_some(n);
    }
    Some(match t.as_str() {
        "lum" | "luminance" | "luma" => 0,
        "red" | "r" => 1,
        "green" | "g" => 2,
        "blue" | "b" => 3,
        "satsat" | "satvssat" | "ss" => 4,
        "huehue" | "huevshue" | "hh" => 5,
        "huesat" | "huevssat" | "hs" => 6,
        _ => return None,
    })
}

fn parse_set(value: Option<String>) -> Result<(usize, String, f32), String> {
    let v = value.ok_or("--set needs <node>:<param>=<value>")?;
    let bad = || format!("--set: \"{v}\" is not of the form <node>:<param>=<value>");
    let (node, rest) = v.split_once(':').ok_or_else(bad)?;
    let (param, val) = rest.split_once('=').ok_or_else(bad)?;
    let node = node.trim().parse::<usize>().map_err(|_| bad())?;
    let val = val.trim().parse::<f32>().map_err(|_| bad())?;
    if param.trim().is_empty() || !val.is_finite() {
        return Err(bad());
    }
    Ok((node, param.trim().to_owned(), val))
}

fn parse_disconnect(value: Option<String>) -> Result<(usize, usize), String> {
    let v = value.ok_or("--disconnect needs <node>:<port>")?;
    let bad = || format!("--disconnect: \"{v}\" is not of the form <node>:<port>");
    let (n, p) = v.split_once(':').ok_or_else(bad)?;
    Ok((n.trim().parse().map_err(|_| bad())?, p.trim().parse().map_err(|_| bad())?))
}

fn parse_rate(value: Option<String>) -> Result<(usize, u8), String> {
    let v = value.ok_or("--rate needs <image>:<n>")?;
    let bad = || format!("--rate: \"{v}\" is not of the form <image>:<0-5>");
    let (i, n) = v.split_once(':').ok_or_else(bad)?;
    let n = n.trim().parse::<u8>().map_err(|_| bad())?;
    if n > 5 {
        return Err(bad());
    }
    Ok((i.trim().parse().map_err(|_| bad())?, n))
}

fn parse_iso(value: Option<String>) -> Result<(Option<f32>, Option<f32>), String> {
    let v = value.ok_or("--filter-iso needs <min>:<max>")?;
    let bad = || format!("--filter-iso: \"{v}\" is not of the form <min>:<max>");
    let (lo, hi) = v.split_once(':').ok_or_else(bad)?;
    let side = |s: &str| -> Result<Option<f32>, String> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        s.parse::<f32>().ok().filter(|x| x.is_finite() && *x >= 0.0).map(Some).ok_or_else(bad)
    };
    Ok((side(lo)?, side(hi)?))
}

fn parse_flag(value: Option<String>) -> Result<(usize, rawgraph_project::Flag), String> {
    use rawgraph_project::Flag;
    let v = value.ok_or("--flag needs <image>:<pick|reject|none>")?;
    let bad = || format!("--flag: \"{v}\" is not of the form <image>:<pick|reject|none>");
    let (i, f) = v.split_once(':').ok_or_else(bad)?;
    let flag = match f.trim() {
        "pick" => Flag::Pick,
        "reject" => Flag::Reject,
        "none" => Flag::None,
        _ => return Err(bad()),
    };
    Ok((i.trim().parse().map_err(|_| bad())?, flag))
}

fn parse_label(value: Option<String>) -> Result<(usize, Option<rawgraph_project::ColorLabel>), String> {
    use rawgraph_project::ColorLabel;
    let v = value.ok_or("--label needs <image>:<colour>")?;
    let bad = || format!("--label: \"{v}\" is not of the form <image>:<red|yellow|green|blue|purple|none>");
    let (i, l) = v.split_once(':').ok_or_else(bad)?;
    let label = match l.trim() {
        "red" => Some(ColorLabel::Red),
        "yellow" => Some(ColorLabel::Yellow),
        "green" => Some(ColorLabel::Green),
        "blue" => Some(ColorLabel::Blue),
        "purple" => Some(ColorLabel::Purple),
        "none" => None,
        _ => return Err(bad()),
    };
    Ok((i.trim().parse().map_err(|_| bad())?, label))
}

/// Finds a node kind by a loose name: label without spaces/punctuation, prefix match.
pub fn parse_kind(name: &str) -> Option<rawgraph_core::NodeKind> {
    use rawgraph_core::NodeKind;
    let norm = |s: &str| s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase();
    let want = norm(name);
    if want.is_empty() {
        return None;
    }
    // Names that differ from the label.
    match want.as_str() {
        "blur" => return Some(NodeKind::Blur),
        "transform" | "crop" | "rotate" => return Some(NodeKind::Transform),
        "huesat" | "hsl" => return Some(NodeKind::HueSaturation),
        "bw" | "blackandwhite" => return Some(NodeKind::BlackWhite),
        "lens" => return Some(NodeKind::LensManual),
        "radial" => return Some(NodeKind::RadialMask),
        "luminance" | "lum" => return Some(NodeKind::LuminanceMask),
        "gradient" => return Some(NodeKind::GradientMask),
        "tone" | "tonecurve" => return Some(NodeKind::ToneCurve),
        "curve" | "pointcurve" => return Some(NodeKind::PointCurve),
        "brush" => return Some(NodeKind::BrushMask),
        "split" | "metadatasplit" => return Some(NodeKind::MetadataSplit),
        "wheels" | "colorwheels" | "lgg" | "liftgammagain" => return Some(NodeKind::ColorWheels),
        "clarity" | "texture" | "claritytexture" => return Some(NodeKind::Clarity),
        _ => {}
    }
    let hits: Vec<_> = rawgraph_core::NodeKind::ALL.into_iter().filter(|k| norm(k.label()).starts_with(&want)).collect();
    match hits.as_slice() {
        [one] => Some(*one),
        _ => rawgraph_core::NodeKind::ALL.into_iter().find(|k| norm(k.label()) == want),
    }
}

fn parse_add_node(value: Option<String>) -> Result<(rawgraph_core::NodeKind, Option<[f32; 2]>), String> {
    let v = value.ok_or("--add-node needs a node kind")?;
    let bad = || format!("--add-node: \"{v}\" is not <Kind> or <Kind>@x,y");
    let (name, pos) = match v.split_once('@') {
        Some((n, p)) => {
            let (x, y) = p.split_once(',').ok_or_else(bad)?;
            (n, Some([x.trim().parse::<f32>().map_err(|_| bad())?, y.trim().parse::<f32>().map_err(|_| bad())?]))
        }
        None => (v.as_str(), None),
    };
    let kind = parse_kind(name).ok_or_else(|| format!("--add-node: unknown or ambiguous node kind \"{name}\""))?;
    Ok((kind, pos))
}

/// `<from>[.<out>]:<to>:<port>` as `(from, out port, to, input port)`.
fn parse_connect(value: Option<String>) -> Result<(usize, usize, usize, usize, bool), String> {
    let v = value.ok_or("--connect needs <from>[.<out>]:<to>:<port>")?;
    let bad = || format!("--connect: \"{v}\" is not of the form <from>[.<out>]:<to>:<port>");
    let mut it = v.split(':');
    let from = it.next().ok_or_else(bad)?;
    let (from, out, explicit) = match from.split_once('.') {
        Some((f, o)) => (f, o.trim().parse::<usize>().map_err(|_| bad())?, true),
        None => (from, 0, false),
    };
    let from = from.trim().parse::<usize>().map_err(|_| bad())?;
    let mut next = || it.next().and_then(|s| s.trim().parse::<usize>().ok()).ok_or_else(bad);
    let (b, c) = (next()?, next()?);
    if it.next().is_some() {
        return Err(bad());
    }
    Ok((from, out, b, c, explicit))
}

fn parse_split_ranges(value: Option<String>) -> Result<(usize, Vec<rawgraph_core::NamedRange>), String> {
    let v = value.ok_or("--split-ranges needs <node>:<name>=<min>..<max>[,...]")?;
    let (node, rest) = v.split_once(':').ok_or_else(|| format!("--split-ranges: \"{v}\" is not of the form <node>:<name>=<min>..<max>[,...]"))?;
    let node = node.trim().parse::<usize>().map_err(|_| format!("--split-ranges: \"{node}\" is not a node index"))?;
    let ranges = crate::ranges_logic::parse_ranges(rest).map_err(|e| format!("--split-ranges: {e}"))?;
    Ok((node, ranges))
}

fn parse_paint(value: Option<String>) -> Result<PaintArg, String> {
    let v = value.ok_or("--paint needs <node>:<x>,<y>;<x>,<y>...[:<size>,<hardness>,<flow>]")?;
    let bad = || format!("--paint: \"{v}\" is not of the form <node>:<x0>,<y0>;<x1>,<y1>;...[:<size>,<hardness>,<flow>]");
    let mut parts = v.split(':');
    let node = parts.next().and_then(|s| s.trim().parse::<usize>().ok()).ok_or_else(bad)?;
    let points_text = parts.next().ok_or_else(bad)?;
    let number = |s: &str| s.trim().parse::<f32>().ok().filter(|x| x.is_finite()).ok_or_else(bad);
    let mut points = Vec::new();
    for p in points_text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (x, y) = p.split_once(',').ok_or_else(bad)?;
        points.push([number(x)?, number(y)?]);
    }
    if points.is_empty() {
        return Err(bad());
    }
    let look = match parts.next() {
        None => None,
        Some(t) => {
            let nums: Vec<&str> = t.split(',').collect();
            if nums.len() != 3 {
                return Err(bad());
            }
            let (size, hardness, flow) = (number(nums[0])?, number(nums[1])?, number(nums[2])?);
            if size <= 0.0 || !(0.0..=1.0).contains(&hardness) || !(0.0..=1.0).contains(&flow) {
                return Err(format!("--paint: size must be above 0, hardness and flow within 0..1 (\"{v}\")"));
            }
            Some([size, hardness, flow])
        }
    };
    if parts.next().is_some() {
        return Err(bad());
    }
    Ok((node, points, look))
}

fn parse_size(value: Option<String>) -> Result<(f32, f32), String> {
    let v = value.ok_or("--size needs <W>x<H>")?;
    let bad = || format!("--size: \"{v}\" is not of the form <W>x<H>");
    let (w, h) = v.split_once(['x', 'X']).ok_or_else(bad)?;
    let (w, h) = (w.trim().parse::<f32>().map_err(|_| bad())?, h.trim().parse::<f32>().map_err(|_| bad())?);
    if !(320.0..=16384.0).contains(&w) || !(240.0..=16384.0).contains(&h) {
        return Err(bad());
    }
    Ok((w, h))
}

impl Cli {
    /// A run that captures the picture and exits (screenshot or frame dump).
    pub fn capture(&self) -> bool {
        self.screenshot.is_some() || self.dump_frame.is_some()
    }
}

/// Parses the arguments after the program name.
pub fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => cli.demo = true,
            "--screenshot" => {
                cli.screenshot = Some(PathBuf::from(it.next().ok_or("--screenshot needs a path")?));
            }
            "--select-node" => cli.select_node = Some(parse_index("--select-node", it.next())?),
            "--select-image" => cli.select_image = Some(parse_index("--select-image", it.next())?),
            "--view-node" => cli.view_node = Some(parse_index("--view-node", it.next())?),
            "--individual" => cli.individual.push(parse_index("--individual", it.next())?),
            "--set" => cli.set.push(parse_set(it.next())?),
            "--disconnect" => cli.disconnect.push(parse_disconnect(it.next())?),
            "--open-folder" => cli.open_folder = Some(PathBuf::from(it.next().ok_or("--open-folder needs a path")?)),
            "--project" => cli.project = Some(PathBuf::from(it.next().ok_or("--project needs a path")?)),
            "--save-project" => cli.save_project = Some(PathBuf::from(it.next().ok_or("--save-project needs a path")?)),
            "--rate" => cli.rate.push(parse_rate(it.next())?),
            "--filter-iso" => cli.filter_iso = Some(parse_iso(it.next())?),
            "--preview-size" => {
                let v = it.next().ok_or("--preview-size needs a value")?;
                let n = v.parse::<u32>().map_err(|_| format!("--preview-size: \"{v}\" is not an integer"))?;
                if !(256..=8192).contains(&n) {
                    return Err(format!("--preview-size: {n} is outside 256..8192"));
                }
                cli.preview_size = Some(n);
            }
            "--bench" => cli.bench = true,
            "--renderer" => {
                let v = it.next().ok_or("--renderer needs auto, cpu or gpu")?;
                cli.renderer = Some(crate::gpu_path::RendererMode::parse(&v).ok_or_else(|| format!("--renderer: \"{v}\" is not auto, cpu or gpu"))?);
            }
            "--dump-frame" => cli.dump_frame = Some(PathBuf::from(it.next().ok_or("--dump-frame needs a path")?)),
            "--gpu-fail" => {
                let v = it.next().ok_or("--gpu-fail needs oom, lost or other")?;
                cli.gpu_fail = Some(crate::gpu_path::GpuFailure::parse(&v).ok_or_else(|| format!("--gpu-fail: \"{v}\" is not oom, lost or other"))?);
            }
            "--no-recursive" => cli.no_recursive = true,
            "--actual-size" => cli.actual_size = true,
            "--add-node" => cli.add_node.push(parse_add_node(it.next())?),
            "--connect" => {
                let (from, out, to, port, explicit) = parse_connect(it.next())?;
                if explicit {
                    cli.connect_ports.push((from, out, to, port));
                } else {
                    cli.connect.push((from, to, port));
                }
            }
            "--split-ranges" => cli.split_ranges.push(parse_split_ranges(it.next())?),
            "--paint" => cli.paint.push(parse_paint(it.next())?),
            "--bench-tools" => cli.bench_tools = true,
            "--no-gpu-preload" => cli.no_gpu_preload = true,
            "--move-node" => {
                let v = it.next().ok_or("--move-node needs <n>:<x>,<y>")?;
                let bad = || format!("--move-node: \"{v}\" is not of the form <n>:<x>,<y>");
                let (n, xy) = v.split_once(':').ok_or_else(bad)?;
                let (x, y) = xy.split_once(',').ok_or_else(bad)?;
                let num = |t: &str| t.trim().parse::<f32>().ok().filter(|x| x.is_finite()).ok_or_else(bad);
                cli.move_node.push((n.trim().parse().map_err(|_| bad())?, num(x)?, num(y)?));
            }
            "--curve" => {
                let v = it.next().ok_or("--curve needs <node>:<x>,<y>;<x>,<y>...")?;
                let bad = || format!("--curve: \"{v}\" is not of the form <node>:[<channel>:]<x>,<y>;<x>,<y>;... (x increasing, 0..1)");
                let (n, rest) = v.split_once(':').ok_or_else(bad)?;
                let (channel, pts) = match rest.split_once(':') {
                    Some((c, p)) => (Some(parse_curve_channel(c).ok_or_else(bad)?), p),
                    None => (None, rest),
                };
                let mut points = Vec::new();
                for p in pts.split(';').map(str::trim).filter(|p| !p.is_empty()) {
                    let (x, y) = p.split_once(',').ok_or_else(bad)?;
                    let num = |t: &str| t.trim().parse::<f32>().ok().filter(|v| (0.0..=1.0).contains(v)).ok_or_else(bad);
                    points.push([num(x)?, num(y)?]);
                }
                if points.len() < 2 || points.windows(2).any(|w| w[0][0] >= w[1][0]) {
                    return Err(bad());
                }
                let node = n.trim().parse().map_err(|_| bad())?;
                match channel {
                    Some(c) => cli.curves.push((node, c, points)),
                    None => cli.curve.push((node, points)),
                }
            }
            "--set-folder" => {
                let v = it.next().ok_or("--set-folder needs <node>:<path>")?;
                let bad = || format!("--set-folder: \"{v}\" is not of the form <node>:<path>");
                let (n, path) = v.split_once(':').ok_or_else(bad)?;
                if path.trim().is_empty() {
                    return Err(bad());
                }
                cli.set_folder.push((n.trim().parse().map_err(|_| bad())?, PathBuf::from(path.trim())));
            }
            "--set-text" => {
                let v = it.next().ok_or("--set-text needs <node>:<field>=<value>")?;
                let bad = || format!("--set-text: \"{v}\" is not of the form <node>:<field>=<value>");
                let (n, rest) = v.split_once(':').ok_or_else(bad)?;
                let (field, value) = rest.split_once('=').ok_or_else(bad)?;
                if crate::node_ui::ExportField::parse(field).is_none() {
                    return Err(format!("--set-text: \"{field}\" is not folder, name, artist, copyright or description"));
                }
                cli.set_text.push((n.trim().parse().map_err(|_| bad())?, field.trim().to_owned(), value.to_owned()));
            }
            "--run-export-node" => cli.run_export_node = Some(parse_index("--run-export-node", it.next())?),
            "--open-node-export" => cli.open_node_export = Some(parse_index("--open-node-export", it.next())?),
            "--open-run-nodes" => cli.open_run_nodes = true,
            "--inspector-scroll" => {
                let v = it.next().ok_or("--inspector-scroll needs a value")?;
                cli.inspector_scroll = Some(v.parse::<f32>().ok().filter(|x| x.is_finite() && *x >= 0.0).ok_or_else(|| format!("--inspector-scroll: \"{v}\" is not a non-negative number"))?);
            }
            "--scopes" => {
                cli.scopes = Some(match it.next().as_deref() {
                    Some("bottom") => ScopesPos::BottomRight,
                    Some("right") => ScopesPos::RightColumn,
                    Some("window") => ScopesPos::Window,
                    Some("hidden") => ScopesPos::Hidden,
                    other => return Err(format!("--scopes: expected bottom, right, window or hidden, got {other:?}")),
                });
            }
            "--delete-node" => cli.delete_node.push(parse_index("--delete-node", it.next())?),
            "--export-no-profile" => cli.export_no_profile = true,
            "--export-no-metadata" => cli.export_no_metadata = true,
            "--export" => cli.export = Some(PathBuf::from(it.next().ok_or("--export needs a folder")?)),
            "--export-format" => {
                use crate::export::Fmt;
                cli.export_format = Some(match it.next().as_deref() {
                    Some("jpg" | "jpeg") => Fmt::Jpeg,
                    Some("png") => Fmt::Png,
                    Some("tif" | "tiff") => Fmt::Tiff,
                    Some("exr") => Fmt::Exr,
                    Some("png16") => Fmt::Png16,
                    Some("tif16" | "tiff16") => Fmt::Tiff16,
                    other => return Err(format!("--export-format: expected jpg, png, tif, exr, png16 or tif16, got {other:?}")),
                });
            }
            "--export-long-edge" => {
                let v = it.next().ok_or("--export-long-edge needs a value")?;
                let n = v.parse::<u32>().map_err(|_| format!("--export-long-edge: \"{v}\" is not an integer"))?;
                if n == 0 {
                    return Err("--export-long-edge: must be at least 1".to_owned());
                }
                cli.export_long_edge = Some(n);
            }
            "--export-scope" => {
                use crate::export::Scope;
                cli.export_scope = Some(match it.next().as_deref() {
                    Some("selected") => Scope::Selected,
                    Some("filtered") => Scope::Filtered,
                    Some("all") => Scope::All,
                    other => return Err(format!("--export-scope: expected selected, filtered or all, got {other:?}")),
                });
            }
            "--open-export-dialog" => cli.open_export_dialog = true,
            "--open-history" => cli.open_history = true,
            "--simulate-conflict" => cli.simulate_conflict = true,
            "--select-revision" => {
                let v = it.next().ok_or("--select-revision needs a revision number")?;
                cli.select_revision = Some(v.parse::<u64>().map_err(|_| format!("--select-revision: \"{v}\" is not a revision number"))?);
            }
            "--export-folder" => cli.export_folder = Some(PathBuf::from(it.next().ok_or("--export-folder needs a folder")?)),
            "--demo-graph" => cli.demo_graph = true,
            "--mask-overlay" => cli.mask_overlay = true,
            "--set-ev" => {
                let v = it.next().ok_or("--set-ev needs <node>:<ev100>")?;
                let bad = || format!("--set-ev: \"{v}\" is not of the form <node>:<ev100>");
                let (n, e) = v.split_once(':').ok_or_else(bad)?;
                let ev = e.trim().parse::<f32>().ok().filter(|x| x.is_finite()).ok_or_else(bad)?;
                cli.set_ev.push((n.trim().parse().map_err(|_| bad())?, ev));
            }
            "--data-dir" => cli.data_dir = Some(PathBuf::from(it.next().ok_or("--data-dir needs a folder")?)),
            "--simulate-crash" => cli.simulate_crash = true,
            "--recover-latest" => cli.recover_latest = true,
            "--select-nodes" => cli.select_nodes = parse_index_list(it.next())?,
            "--copy-selection" => cli.copy_selection = true,
            "--paste-selection" => cli.paste_selection = true,
            "--save-recipe" => cli.save_recipe.push(it.next().ok_or("--save-recipe needs a name")?),
            "--open-recipes" => cli.open_recipes = true,
            "--import-xmp" => cli.import_xmp = true,
            "--open-xmp-dialog" => cli.open_xmp_dialog = true,
            "--missing-only" => cli.missing_only = true,
            "--relink-dialog" => cli.relink_dialog = Some(parse_index("--relink-dialog", it.next())?),
            "--relink-new" => cli.relink_new = Some(PathBuf::from(it.next().ok_or("--relink-new needs a folder")?)),
            "--compare" => cli.compare = true,
            "--autosave-interval" => {
                let v = it.next().ok_or("--autosave-interval needs seconds")?;
                let n = v.parse::<u64>().ok().filter(|n| (1..=3600).contains(n)).ok_or_else(|| format!("--autosave-interval: \"{v}\" is not 1-3600"))?;
                cli.autosave_interval = Some(n);
            }
            "--min-rating" => {
                let v = it.next().ok_or("--min-rating needs a value")?;
                let n = v.parse::<u8>().ok().filter(|n| *n <= 5).ok_or_else(|| format!("--min-rating: \"{v}\" is not 0-5"))?;
                cli.min_rating = Some(n);
            }
            "--reference" => cli.reference = Some(parse_index("--reference", it.next())?),
            "--flag" => cli.flag.push(parse_flag(it.next())?),
            "--label" => cli.label.push(parse_label(it.next())?),
            "--late-disconnect" => cli.late_disconnect.push(parse_disconnect(it.next())?),
            "--workspace" => {
                cli.workspace = Some(match it.next().as_deref() {
                    Some("single") => Workspace::Single,
                    Some("dual") => Workspace::Dual,
                    other => return Err(format!("--workspace: expected single or dual, got {other:?}")),
                });
            }
            "--clean-feed" => cli.clean_feed = true,
            "--embed-windows" => cli.embed_windows = true,
            "--size" => cli.size = Some(parse_size(it.next())?),
            "--scale" => {
                let v = it.next().ok_or("--scale needs a value")?;
                let f = v.parse::<f32>().map_err(|_| format!("--scale: \"{v}\" is not a number"))?;
                if !(0.5..=4.0).contains(&f) {
                    return Err(format!("--scale: {f} is outside 0.5..4"));
                }
                cli.scale = Some(f);
            }
            "-h" | "--help" => cli.help = true,
            other => return Err(format!("unknown argument \"{other}\"")),
        }
    }
    Ok(cli)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Cli, String> {
        parse_args(s.split_whitespace().map(str::to_owned))
    }

    #[test]
    fn renderer_dump_and_failure_flags() {
        use crate::gpu_path::{GpuFailure, RendererMode};
        let cli = parse("--renderer gpu --dump-frame a/b.png --gpu-fail oom").unwrap();
        assert_eq!(cli.renderer, Some(RendererMode::Gpu));
        assert_eq!(cli.dump_frame, Some(PathBuf::from("a/b.png")));
        assert_eq!(cli.gpu_fail, Some(GpuFailure::Oom));
        assert_eq!(parse("--renderer cpu").unwrap().renderer, Some(RendererMode::Cpu));
        assert_eq!(parse("--renderer auto").unwrap().renderer, Some(RendererMode::Auto));
        assert!(parse("--renderer metal").is_err());
        assert!(parse("--renderer").is_err());
        assert!(parse("--gpu-fail melted").is_err());
        assert!(parse("--dump-frame").is_err());
        assert_eq!(parse("").unwrap().renderer, None);
    }

    #[test]
    fn empty_is_default() {
        assert_eq!(parse("").unwrap(), Cli::default());
    }

    #[test]
    fn full_example() {
        let cli = parse(
            "--demo --screenshot out.png --select-node 3 --select-image 1 --view-node 5 --individual 3 \
             --set 3:ev=1.5 --set 5:radius=20 --workspace dual --clean-feed --size 1600x1000 --disconnect 5:0",
        )
        .unwrap();
        assert!(cli.demo && cli.clean_feed);
        assert_eq!(cli.screenshot, Some(PathBuf::from("out.png")));
        assert_eq!((cli.select_node, cli.select_image, cli.view_node), (Some(3), Some(1), Some(5)));
        assert_eq!(cli.individual, vec![3]);
        assert_eq!(cli.set, vec![(3, "ev".to_owned(), 1.5), (5, "radius".to_owned(), 20.0)]);
        assert_eq!(cli.workspace, Some(Workspace::Dual));
        assert_eq!(cli.size, Some((1600.0, 1000.0)));
        assert_eq!(cli.disconnect, vec![(5, 0)]);
    }

    #[test]
    fn wave_two_flags() {
        let cli = parse("--open-folder C:/x/y --project a.rawgraph --save-project b.rawgraph --rate 2:5 --rate 0:1 --filter-iso 100:800 --preview-size 1920 --bench").unwrap();
        assert_eq!(cli.open_folder, Some(PathBuf::from("C:/x/y")));
        assert_eq!(cli.project, Some(PathBuf::from("a.rawgraph")));
        assert_eq!(cli.save_project, Some(PathBuf::from("b.rawgraph")));
        assert_eq!(cli.rate, vec![(2, 5), (0, 1)]);
        assert_eq!(cli.filter_iso, Some((Some(100.0), Some(800.0))));
        assert_eq!(cli.preview_size, Some(1920));
        assert!(cli.bench);
        let more = parse("--demo-graph --flag 1:pick --flag 2:reject --label 0:green --label 3:none").unwrap();
        assert!(more.demo_graph);
        assert_eq!(more.flag, vec![(1, rawgraph_project::Flag::Pick), (2, rawgraph_project::Flag::Reject)]);
        assert_eq!(more.label, vec![(0, Some(rawgraph_project::ColorLabel::Green)), (3, None)]);
        assert!(parse("--flag 1:maybe").is_err() && parse("--label 1:pink").is_err());
        assert_eq!(parse("--filter-iso :800").unwrap().filter_iso, Some((None, Some(800.0))));
        assert_eq!(parse("--filter-iso 400:").unwrap().filter_iso, Some((Some(400.0), None)));
        for bad in ["--rate 1:9", "--rate 1", "--filter-iso 100", "--filter-iso a:b", "--preview-size 5", "--open-folder"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn node_kinds_by_loose_name() {
        use rawgraph_core::NodeKind;
        assert_eq!(parse_kind("Contrast"), Some(NodeKind::Contrast));
        assert_eq!(parse_kind("highlights"), Some(NodeKind::HighlightsShadows));
        assert_eq!(parse_kind("Noise"), Some(NodeKind::NoiseReduction));
        assert_eq!(parse_kind("metadatafilter"), Some(NodeKind::MetadataFilter));
        assert_eq!(parse_kind("merge"), Some(NodeKind::Merge));
        assert_eq!(parse_kind("exposure"), Some(NodeKind::Exposure));
        assert_eq!(parse_kind("nonsense"), None);
        assert_eq!(parse_kind("develop"), Some(NodeKind::Develop));
        assert_eq!(parse_kind("Hue / Saturation"), Some(NodeKind::HueSaturation));
        assert_eq!(parse_kind("transform"), Some(NodeKind::Transform));
        assert_eq!(parse_kind("RadialMask"), Some(NodeKind::RadialMask));
        assert_eq!(parse_kind("sharpen"), Some(NodeKind::Sharpen));
        assert_eq!(parse_kind("vignette"), Some(NodeKind::Vignette));
        assert_eq!(parse_kind("blackwhite"), Some(NodeKind::BlackWhite));
        assert_eq!(parse_kind("saturation"), Some(NodeKind::Saturation));
        let cli = parse("--add-node Merge@100,200 --add-node blur --connect 0:7:1").unwrap();
        assert_eq!(cli.add_node, vec![(NodeKind::Merge, Some([100.0, 200.0])), (NodeKind::Blur, None)]);
        assert_eq!(cli.connect, vec![(0, 7, 1)]);
        assert!(parse("--connect 1:2").is_err() && parse("--add-node Merge@x").is_err() && parse("--add-node zzz").is_err());
    }

    #[test]
    fn export_flags() {
        use crate::export::{Fmt, Scope};
        let cli = parse("--export C:/out --export-format tif --export-long-edge 1600 --export-scope filtered --open-export-dialog --export-folder C:/e --actual-size").unwrap();
        assert_eq!(cli.export, Some(PathBuf::from("C:/out")));
        assert_eq!((cli.export_format, cli.export_long_edge, cli.export_scope), (Some(Fmt::Tiff), Some(1600), Some(Scope::Filtered)));
        assert!(cli.open_export_dialog && cli.actual_size);
        assert_eq!(cli.export_folder, Some(PathBuf::from("C:/e")));
        for bad in ["--export-format bmp", "--export-scope some", "--export-long-edge 0", "--export-long-edge x", "--export"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wave_five_flags() {
        use crate::export::Fmt;
        let cli = parse("--connect 3.2:5:0 --connect 1:2:0 --split-ranges 4:Low=0..400,High=401..6400 --paint 6:0.1,0.2;0.5,0.5:8,0.4,0.9 --paint 6:0.7,0.7 --export-format png16 --export-no-profile --export-no-metadata --bench-tools").unwrap();
        assert_eq!(cli.connect_ports, vec![(3, 2, 5, 0)]);
        assert_eq!(cli.connect, vec![(1, 2, 0)]);
        assert_eq!(cli.split_ranges.len(), 1);
        assert_eq!(cli.split_ranges[0].0, 4);
        assert_eq!(cli.split_ranges[0].1[1].name, "High");
        assert_eq!(cli.paint.len(), 2);
        assert_eq!(cli.paint[0], (6, vec![[0.1, 0.2], [0.5, 0.5]], Some([8.0, 0.4, 0.9])));
        assert_eq!(cli.paint[1], (6, vec![[0.7, 0.7]], None));
        assert_eq!(cli.export_format, Some(Fmt::Png16));
        assert!(cli.export_no_profile && cli.export_no_metadata && cli.bench_tools);
        assert_eq!(parse("--export-format tif16").unwrap().export_format, Some(Fmt::Tiff16));
        let cli = parse("--move-node 3:120,-40.5 --delete-node 4 --delete-node 5").unwrap();
        assert_eq!((cli.move_node, cli.delete_node), (vec![(3, 120.0, -40.5)], vec![4, 5]));
        assert_eq!(parse("--curve 7:0,0;0.25,0.15;0.75,0.85;1,1").unwrap().curve[0].1.len(), 4);
        for bad in ["--curve 7", "--curve 7:0,0", "--curve 7:0,0;0,1", "--curve 7:0.5,0.5;0.2,0.2", "--curve 7:0,0;2,1", "--curve x:0,0;1,1"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert_eq!(parse("--scopes hidden").unwrap().scopes, Some(ScopesPos::Hidden));
        assert_eq!(parse("--scopes bottom").unwrap().scopes, Some(ScopesPos::BottomRight));
        assert!(parse("--scopes left").is_err() && parse("--scopes").is_err());
        for bad in ["--move-node 3", "--move-node 3:1", "--move-node x:1,2", "--move-node 3:1,nan", "--delete-node"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        for bad in [
            "--connect 1.x:2:0",
            "--connect 1.0:2",
            "--connect 1:2:0:9",
            "--split-ranges 4",
            "--split-ranges x:A=0..1",
            "--split-ranges 4:A=5..1",
            "--split-ranges 4:A=0..1,A=2..3",
            "--paint 1",
            "--paint 1:",
            "--paint 1:0.1;0.2",
            "--paint 1:0.1,0.2:8,0.4",
            "--paint 1:0.1,0.2:0,0.4,0.9",
            "--paint 1:0.1,0.2:8,1.4,0.9",
            "--paint x:0.1,0.2",
            "--export-format png32",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn new_node_kinds_by_loose_name() {
        use rawgraph_core::NodeKind;
        assert_eq!(parse_kind("curve"), Some(NodeKind::PointCurve));
        assert_eq!(parse_kind("tonecurve"), Some(NodeKind::ToneCurve));
        assert_eq!(parse_kind("brush"), Some(NodeKind::BrushMask));
        assert_eq!(parse_kind("split"), Some(NodeKind::MetadataSplit));
        assert_eq!(parse_kind("Split by Metadata"), Some(NodeKind::MetadataSplit));
        assert_eq!(parse_kind("wheels"), Some(NodeKind::ColorWheels));
        assert_eq!(parse_kind("Lift / Gamma / Gain"), Some(NodeKind::ColorWheels));
        assert_eq!(parse_kind("clarity"), Some(NodeKind::Clarity));
        assert_eq!(parse_kind("Clarity / Texture"), Some(NodeKind::Clarity));
    }

    #[test]
    fn the_preload_can_be_switched_off_for_measuring() {
        assert!(!parse("--bench").unwrap().no_gpu_preload);
        assert!(parse("--bench --no-gpu-preload").unwrap().no_gpu_preload);
    }

    #[test]
    fn library_and_safety_net_flags() {
        let cli = parse(
            "--data-dir C:/d/x --simulate-crash --recover-latest --select-nodes 1,2,3 --copy-selection --paste-selection \
             --save-recipe Warm --save-recipe Cool --open-recipes --import-xmp --open-xmp-dialog --missing-only --relink-dialog 2 \
             --relink-new D:/new --compare --reference 4 --min-rating 3 --autosave-interval 5",
        )
        .unwrap();
        assert_eq!(cli.data_dir, Some(PathBuf::from("C:/d/x")));
        assert!(cli.simulate_crash && cli.recover_latest && cli.copy_selection && cli.paste_selection && cli.open_recipes);
        assert!(cli.import_xmp && cli.open_xmp_dialog && cli.missing_only && cli.compare);
        assert_eq!(cli.select_nodes, vec![1, 2, 3]);
        assert_eq!(cli.min_rating, Some(3));
        assert_eq!(cli.autosave_interval, Some(5));
        assert_eq!(cli.save_recipe, vec!["Warm".to_owned(), "Cool".to_owned()]);
        assert_eq!((cli.relink_dialog, cli.relink_new, cli.reference), (Some(2), Some(PathBuf::from("D:/new")), Some(4)));
        for bad in ["--data-dir", "--select-nodes", "--select-nodes a,b", "--select-nodes 1,,2", "--select-nodes -1", "--save-recipe", "--relink-dialog x", "--relink-new", "--reference -2", "--min-rating 6", "--min-rating x", "--autosave-interval 0", "--autosave-interval x"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn negative_values_in_set() {
        assert_eq!(parse("--set 2:temp=-30").unwrap().set, vec![(2, "temp".to_owned(), -30.0)]);
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        for bad in [
            "--bogus",
            "--screenshot",
            "--select-node x",
            "--select-node -1",
            "--set 3:ev",
            "--set ev=1",
            "--set 3:ev=abc",
            "--set 3:ev=nan",
            "--workspace triple",
            "--size 10x10",
            "--size 1600",
            "--scale 99",
        ] {
            assert!(parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}
