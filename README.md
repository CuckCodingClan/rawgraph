# RawGraph

Local-first RAW photo analysis, organization, and non-destructive editing through reusable node graphs. A professional tool inspired by the node workflows of Cinema 4D Xpresso and Fusion.

## Requirements

- **Desktop platforms:** native desktop application for Windows, macOS, and Linux, with consistent project/workflow behavior across platforms.
- **Project storage and collaboration:** store catalogs, graphs, parameters, and project metadata in a versioned database-backed project format. Keep source images external and referenced, not embedded by default. Design the data model for future collaboration (stable IDs, transactions, migrations, change history, conflict handling); choose local and shared database deployment after evaluating single-user and multi-user needs.
- **Rust performance core:** RAW processing, image operations, graph evaluation, metadata/catalog ingestion, caching, batch execution, and export. Keep pixel data and hot paths in Rust; any UI boundary must be efficient and versioned.
- **Professional scopes:** histogram, waveform, RGB parade, and vectorscope, targeting DaVinci Resolve-level capability and usability. Colorized. Define color-space semantics and validate accuracy with known test patterns.
- **Display layouts:** provide Resolve-like selectable single- and dual-monitor workspaces, with optional clean feed and/or scopes on a third or fourth display; restore sensibly when displays are disconnected or rearranged.
- **Node workflows:** typed data flow for images, collections, metadata, and scopes; editable graphs, batch execution, reusable recipes, and clear validation/errors.
- **RAW compatibility:** prioritize broad camera/file compatibility in the initial product; matching Adobe Camera Raw or Capture One rendering quality is a later optimization, not an MVP gate.
- **Parametric, non-destructive editing:** preserve editable operation parameters and source data throughout the graph; make edits reproducible, adjustable, and reversible.
- **Batch and per-image control:** nodes are created in **All** mode (same parameters for every incoming image) or **Individual** mode. Individual-mode nodes store parameter values keyed to each adjusted image identity, so settings persist independently of the current selection. Nodes compose in graph order, allowing shared edits and per-image tweaks together.
- **RAW and JPEG editing:** JPEGs use the same adjustment controls and graph workflow as RAWs, while respecting their lower latitude and already-rendered image data. White balance supports absolute settings and relative offsets on both formats; RAW absolute WB uses capture metadata as reference, while JPEG behavior accounts for baked-in WB and limited recovery.
- **Exposure values:** calculate and expose a capture-derived exposure value per image from ISO, shutter time, and aperture metadata. Let users edit values per image and apply collection-wide offsets (adding the same offset to each image's value), using parametric nodes that compose in graph order. Define units/reference and missing/inaccurate metadata behavior. Keep this distinct from pixel-based exposure compensation; JPEGs share controls but cannot recover clipped/discarded data.
- **Image controls:** effects can expose image inputs to modulate numeric parameters where spatial variation is meaningful (for example, blur radius). Map values linearly to a parameter range by default; curves/remapping nodes can reshape the mapping. Discrete or categorical controls need explicit semantics rather than blindly interpolating values. This is distinct from merely blending the effect output.
- **HDR and precision:** use ACES as the internal working-space foundation, with a 32-bit floating-point image-processing flow and correct HDR handling throughout preview, scopes, and export. Specify the ACES variant, transfer/display transforms, and output targets during architecture work.

## Roadmap

### Performance is a standing priority

RawGraph should feel snappy on the first view of a RAW, not just after it is cached. A wait of over two seconds for the first visible frame from an unedited Source is unacceptable. Treat responsiveness as a requirement for new features, and prioritize measured performance improvements when no feature requests are queued.

When you select a photo, the viewer replaces the old picture with a cached proxy or
filmstrip thumbnail if one is ready. Otherwise it shows **Rendering…** and tries the
on-disk thumbnail cache and the camera's embedded preview on a background worker.
**Preview** means the displayed picture is provisional; the graph render replaces it
and clears the badge. Edits to the current photo retain the last frame and show the
badge only after 150 ms. Scopes wait for the graph result; Clean Feed can show the preview.

Gallery edits use a separate, lower-priority worker and 512-pixel linear image
bases, not the viewer's large proxies. Visible rows are scheduled first, nearby
rows next. Slider drags schedule at most ten updates per second, with a final pass
on release. This is a scheduling cap, not a frame-rate guarantee. The gallery
worker uses half the CPU cores (2 to 8) at below-normal priority. Run the synthetic
CPU and available GPU benchmark with:

```text
cargo test -p rawgraph-app gallery_benchmark -- --ignored --nocapture
```

Source scans run in stages so a large folder is usable at once:

1. **Count**: the directory listing alone (no image file is opened). The Source
   node and status bar show a running count while subfolders are listed.
2. **List**: every image appears in the gallery with a placeholder thumbnail, in
   natural name order, sortable by name or modified date. Unchanged files keep
   their hash, thumbnail and metadata without any file access.
3. **Thumbnails**: new or changed files are hashed (64 KiB head and tail),
   on-screen gallery items first; thumbnails load as hashes arrive.
4. **Metadata**: full capture details are read last, one file at a time, and pause
   while visible thumbnails are still loading.

On a synthetic folder of 10,000 tiny PNGs in 100 subfolders (Linux, NFS share,
warm cache) the count arrives after about 55 ms and the full list after about
100 ms. The previous scanner added images one at a time, each only after its hash
and metadata were read; the full list took about 950 ms here with 8 workers and
far longer for RAWs over SMB. A cold NFS cache made the first listing much slower
(0.3–9.6 s for the same folder), so cold numbers vary with the server.
Each later file stage costs one network open per file. Listing speed still
depends on the NAS and the number of subfolders; Windows SMB numbers are
unmeasured. Reproduce with:

```text
cargo run --release -p rawgraph-project --example scan_stages -- <folder-of-pngs> [staged|legacy|rescan]
```

Measure RAW decode stages and embedded preview extraction on your own files:

```text
cargo run --release -p rawgraph-io --example first_draw -- <RAW-folder>
```

The example times the 2560-pixel proxy and embedded preview separately; it does not
flush OS caches or measure click-to-display. Baseline gain is timed with the matrix
because it is applied in the same pixel loop. To run the generated DNG fixture:

```text
cargo test -p rawgraph-io --test synthetic_raw synthetic_first_draw -- --ignored --nocapture
```

- [ ] Benchmark selection-to-first-visible-frame on Windows with representative RAWs on both local SSD and NAS. Record median and p95 separately for uncached and cached views, along with camera format, preview size, and hardware. Use the baseline to set explicit regression budgets.
- [ ] Profile the uncached path first: file I/O, decompression, proxy generation, display conversion, and upload. Remove redundant pixel passes, publish a useful preview before scopes or other secondary work, and make prefetch yield to the selected image.
- [ ] Compare before/after timings for changes to preview hot paths. Preserve colour accuracy and full-resolution export fidelity; don't use fast cache hits to hide slow first draws.

### 1. Define and benchmark
- [ ] Set target cameras/formats, minimum hardware, display/GPU expectations, and project/license requirements; support Windows, macOS, and Linux.
- [ ] Build a permitted reference RAW corpus covering cameras, ISO, skin, saturated colors, highlights, fine detail, and difficult lighting.
- [ ] Benchmark candidate RAW engines and converters for rendering quality, camera coverage, performance, licensing, and distribution constraints.
- [ ] Specify color management, output formats/bit depths, scope behavior, and measurable performance budgets.

### 2. Architecture and interaction prototype
- [ ] Prototype a Rust-first processing core and select desktop UI technology based on performance, packaging, accessibility, and multi-monitor behavior.
- [ ] Define typed graph schema, execution model, caching, incremental recomputation, cancellation, errors, and project versioning.
- [ ] Design a versioned database-backed project/catalog model with stable IDs, transactions, migrations, edit history, and image-path references; decide local database format now and a collaboration-ready shared-store/sync strategy later.
- [ ] Define collaboration foundations: concurrent edits, locking or conflict resolution, permissions, change history, and asset availability; defer network/service implementation until core single-user workflows are proven.
- [ ] Define All/Individual node semantics, with Individual-mode parameters stored per image identity and persistent across selection changes; specify how nodes compose and serialize.
- [ ] Define RAW/JPEG input behavior and white-balance semantics: absolute settings use RAW capture metadata when available; relative-offset nodes remain composable after absolute WB; JPEG handling accounts for baked-in WB and limited recovery latitude.
- [ ] Specify ACES configuration, 32-bit float processing, HDR preview/display transforms, scope interpretation, and HDR export targets.
- [ ] Prototype Resolve-like single-/dual-monitor layouts with optional clean feed and scopes on third/fourth displays; verify display loss/reconnect behavior.
- [ ] Prototype graph interaction in an Xpresso/Fusion-inspired style: navigation, selection, typed ports, previews, inspector, search, grouping, undo/redo.

### 3. Library and graph MVP
- [ ] Rust-based folder scanning, metadata extraction, thumbnail/preview cache, filtering, sorting, selection, comparison, and culling.
- [ ] Source/folder and image-sequence nodes; metadata filter/split nodes with multiple named ranges and explicit unmatched handling.
- [ ] Analysis filters for image measures such as brightness; make heuristic nature clear. Support configurable flash-related filtering.
- [ ] Merge/collect and export nodes with defined ordering, duplicate, partial-failure, and retry behavior.
- [ ] Rust graph evaluator with validation, bounded scheduling, progress, cancellation, caching, logs, and reproducible execution.
- [ ] Demonstrate end-to-end: folder → ISO ranges → separate noise-reduction settings → merge → export.

### 4. RAW development and scopes
- [ ] Integrate and benchmark RAW decode/demosaic for priority cameras, using Rust libraries or bounded FFI where necessary.
- [ ] Support RAW and JPEG with the same adjustment controls; absolute WB uses RAW capture metadata when available, while relative WB offsets compose afterward. Preserve JPEG baked-in-WB limitations.
- [ ] Define exposure-value calculation from ISO, shutter time, and aperture; specify units/reference, per-image edits, collection-wide offsets, graph composition, and missing/inaccurate metadata behavior. Distinguish this capture-derived value from pixel-based exposure compensation.
- [ ] Add All- and Individual-mode adjustment nodes; persist Individual parameters per image identity and verify sequential composition without flattening edits.
- [ ] Extend per-pixel image controls to more effects and add composable curve/range remapping.
- [ ] Establish camera/lens profile and color-managed preview/export strategy using ACES and a 32-bit floating-point pipeline; validate HDR scopes, display transforms, and HDR export.
- [ ] Implement histogram, waveform, RGB parade, and vectorscope in the Rust rendering path; define sampling, modes, scales, color-space interpretation, and update behavior.
- [ ] Verify scope values against generated reference patterns and high-bit-depth inputs; compare capabilities and workflow with DaVinci Resolve.
- [ ] Compare RAW output against selected reference converters; document camera-specific limitations rather than claiming unverified parity.

### 5. Professional release
- [ ] Expand local adjustments, batch presets, graph groups/macros, and reusable templates.
- [ ] Profile Rust hot paths and enforce performance/regression budgets for import, preview, scopes, graph evaluation, batches, memory, and export.
- [ ] Add project recovery, crash handling, accessibility, keyboard workflows, packaging, privacy/licensing review, and documentation.
- [ ] Test representative photo batches and Resolve-like single-/dual-monitor layouts, optional third-/fourth-screen clean feed/scopes, and display reconnect behavior on supported systems before release.

## Structure

```text
README.md                      # Requirements and roadmap
node_structure.md              # List of required nodes and their capabilities
Cargo.toml                     # Workspace; pixel crates are fully optimised in dev builds
crates/
  rawgraph-core/               # Float image buffers (ACEScg), colour math, capture EV, typed graph
                               #   with All/Individual parameters, CPU reference pixel operations
  rawgraph-engine/             # Caching, cancellable graph evaluator per image identity
  rawgraph-scopes/             # Provisional SDR display transform, histogram/waveform/parade/
                               #   vectorscope computation, test patterns
  rawgraph-scope-views/        # egui widgets that draw the scopes with graticules
  rawgraph-io/                 # RAW/DNG (rawler) and JPEG/PNG/TIFF decoding, metadata, export writers
  rawgraph-project/            # Project file (SQLite), stable image ids with relinking, culling data,
                               #   filter/sort, parallel folder scan
  rawgraph-fx/                 # Effects on the CPU reference path: tone curve, hue/saturation, sharpen,
                               #   vignette, crop/rotate, lens correction, brush rasterization, curves,
                               #   colour wheels, clarity, layer placement and blending
  rawgraph-batch/              # Export pipeline: progress, cancel, partial failure, retry, 16-bit, ICC, metadata
  rawgraph-gpu/                # GPU pixel path (wgpu): display transform and pointwise operations, verified
                               #   against the CPU reference; used by the application's interactive preview
  rawgraph-app/                # Desktop application (eframe/egui 0.37, wgpu)
docs/offene-punkte.md          # Open questions and open points (German)
docs/project-format.md         # Project file format 2: schema, migration, revisions, conflict rule
packaging/                     # Script that builds the distributable folder and zip (static runtime, licences)
```

Build and run (Windows, Rust stable with MSVC build tools):

```text
cargo run --release -p rawgraph-app -- --open-folder <folder with RAW files>
cargo test --workspace
```

For quick testing on Windows, double-click `RawGraph Dev.cmd` in the repository root (or press Ctrl+Shift+B in VS Code). It builds and runs the current source checkout with the `iter` profile: release optimisation without link-time optimisation and with incremental compilation, so a rebuild after a small change takes seconds instead of minutes. Build output goes to the local disk (`%LOCALAPPDATA%\rawgraph-target`; set `RAWGRAPH_TARGET` to change it), because compiling into a network share is much slower. Shipped preview builds still use `--release`. If the build fails, the window stays open to show the error.

CI (`.github/workflows/ci.yml`): Linux clippy + tests on every push to `main` and every pull request, Windows tests on pull requests, macOS only when started manually.

### Windows startup and console

The Windows GUI starts without creating a console and shows a small startup splash while the GPU and editor initialize. Automated capture, benchmark and export runs skip the splash. With command-line arguments, RawGraph attempts to attach to the parent console for help, errors and other output; GUI-subsystem programs may return the shell prompt before their output appears. A launch without a parent console does not create one. The splash uses the primary display and is currently Windows-only.

### Graph workspace

Click a node to preview its result. Pin the viewer to keep that stage visible while editing another node. With no selected or pinned node, there is no preview. New graphs no longer include an Output node; old graphs can still be opened. Use an Export node to save files or Quick Export to write from the stage you are viewing.

The gallery, scopes, and Clean Feed follow the single viewer or the right **Primary** pane in Split Viewer, not the inspector’s selected node. The gallery lists the primary node’s collection and highlights the primary image. Both split panes have independent image and node pins; unpinned dimensions follow selection (an image-only pin can retain its last compatible node). The left **Reference** pane does not drive the gallery, scopes, or Clean Feed. Wheel and arrow navigation and gallery clicks stay in the primary collection; unpin the primary image to step through it. The single viewer uses fit-relative click zoom (2× or 3×), and the View menu switches wheel behavior between zooming and stepping images. Drag a connection from a port to blank canvas to add and wire a compatible node. You can delete any node, including the last Source, and undo the deletion. An empty graph has a blank viewer and an empty gallery.

### Gallery thumbnails

Each tile uses the Primary viewer's stage and display settings, but renders its
own photo even when the Primary viewer pins a different photo. Group edits update
all affected photos; Individual edits only update the affected photo. Camera JPEGs
are provisional until the editable render is ready. The tile retains its last
picture while rendering. Depth Map, Pixel Sort and stages measured as slow defer
updates during a drag; a small amber dot means the picture is stale.

**View → Editable thumbnail cache** controls the persistent disk budget (default
**8 GiB**, oldest access evicted first). Set it to zero to keep editable bases in
memory only. These small scene-linear bases are separate from embedded JPEGs and
are reused when you change nodes or settings. Reduced-resolution effects can
look different from a full viewer proxy or export.

### Export node preview

Viewing an Export node set to JPEG or 8-bit PNG automatically shows the result of encoding and decoding the viewer image with the node’s settings. It writes no files. The preview can use a reduced-resolution image, so compression artifacts may differ from a full-resolution export. XMP has no image output; other formats do not yet support this preview. For PNG and TIFF, select 8 or 16 bits per channel separately from the format.

### Data-map export

Choose **PNG 16-bit data map** on an Export node for RGB normal maps or other
encoded data. RGB values reaching the node are clamped to 0–1 and rounded to
16-bit directly, without display transform, colour-space conversion, or ICC
profile or EXIF metadata. Alpha is discarded. Use Original size: resizing averages values and
does not renormalize normals. This does not undo transforms applied upstream.
The normal PNG 16-bit option and photo export defaults remain colour-managed.

### Adobe XMP sidecars

Select **Adobe XMP sidecar** on an Export node. This metadata-only export writes
`<RAW stem>.xmp` into the chosen output folder for proprietary RAW files in the
node collection. DNG and rendered images are not supported. Move the sidecar
beside its matching RAW for Adobe to discover it. The output folder must not be
a source-photo folder. Existing sidecars are always skipped, regardless of the
node collision setting; output names ignore the filename template. Nothing is
rendered and originals are not modified. Duplicate stems share a sidecar name;
the first wins and subsequent entries are skipped.

Only the project star rating and colour-label text are written. Reject maps to
Adobe rating -1 (so an accompanying star rating cannot also be represented);
pick is not exported. Colour names use English label text and may not match
custom Adobe label sets. Graph edits, including tone and colour adjustments, are
not exported: Adobe processing is not equivalent to RawGraph. Other export
settings (size, colour space, profile and metadata groups) do not apply. A run
with mixed per-image XMP and rendered formats reports rendered items as failed;
run those separately. Quick Export remains a rendered-image export.

### Per-node timing

Enable **Debug → Per-node CPU timing** to profile the previews. Each node then shows a
small label just under its bottom-right corner with its exclusive CPU wall time for the
last preview (upstream node evaluation excluded), or **cached** when its result came from
the cache. The node taking at least half of the preview's time is red, one taking at least
a fifth is amber. Hover a label for the exact time, its share, and the number of
computations and result-cache hits. Cache hits are reported separately,
not as operation timings. Times include evaluation overhead and are diagnostic, not a
benchmark. The GPU tail is batched and has no per-node GPU timings; a GPU-resident
base or Develop pre-tone base may not appear. Turning the option off disables the
per-node timing clocks. Overlay, histogram, and scope work is not included.

### Clean feed

Open **View → Clean feed** for a fullscreen, image-only output. It shows
the current preview aspect-fitted on black and hides when RawGraph loses focus.
The Dual Screen viewer stays fullscreen and does not hide on focus loss.
Neither output adds a Windows taskbar entry. Press
**Escape** (or F11) while the feed has focus to close it; the editor stays open.
Choose a display under **View → Clean feed display**, before or while it is open. Placement depends on the
OS/window manager; verify it on your display setup, especially on Wayland.

Drop a folder onto the graph to create a Source there and scan that folder. Viewer pinning, resets, and delete actions use icons with descriptive tooltips.

### Multiple image inputs

Connect multiple Sources or edit branches to the same ordinary image input. The node processes their combined collection; if an image arrives more than once, the first connected branch carrying it supplies its pixels. Use Merge for explicit filename collision rules. Exposed control inputs and the individual Merge and Layer ports accept one wire each.

### RAW input exposure

RAW decoding applies DNG BaselineExposure when present (otherwise +0.35 EV),
plus the existing +1.6 EV rendering offset, once at input for both preview and
export. Rendered inputs receive neither gain. White balance remains separate.
Develop exposure is an additional editable adjustment; a neutral Develop does
not add input gain again. Older projects still load, but their per-Develop
baseline toggle, default and brightness overrides are ignored, including
individual-image overrides. Projects using non-default values or multiple
Develop nodes can therefore render differently; use visible Develop exposure
adjustments to recreate any desired compensation.

The inset blue **+** tab at the left of the title bar adds supported input maps. Each added input appears on the left edge beside its parameter name and value, even when the value is at its default. The inset red **+** tab at the far right, beyond the A/I badge, adds output channel aliases as labelled rows on the right edge. Right-click an added port to remove it. These edits support undo.

### Source nodes

A new Source, including the starter Source, supplies no images until you choose a folder. Clearing the folder makes it empty again. **File → Open folder** assigns the path to a Source before scanning; it does not create an input shared by all nodes. Older saved Sources with no folder setting retain their catalog-wide behavior so saved graphs keep their inputs.

The Source count updates as files are discovered. Once enumeration finishes,
the gallery gets the list together; thumbnails and capture details fill in
later. **Capture date** uses file modification time until an EXIF date is
available. Cancelling keeps the listed images and a subsequent scan can finish
pending work. A file that might be a renamed catalog image only joins the
gallery after its hash is checked, so edits and graph references keep the
original identity.

### Per-pixel image controls

Click the blue title-bar **+** tab to add a control input. Develop exposes Exposure, Contrast, Lights, Shadows, White and Black; Exposure is available on the standalone Exposure node; Contrast, Saturation and Sharpen each expose their amount; Filter exposes Radius and Direction (RGB). A connected map scales that slider's effect per pixel: black gives no effect (the parameter's neutral value), white gives the slider value, and values in between interpolate linearly; values outside 0–1 are clamped. The slider still sets the strength. Image inputs use linear ACEScg luminance; use an exposed image channel if you need a particular channel. Both Legacy and Local v1 tone processing support these inputs. An unconnected input leaves the slider in control.

Intensity maps are resized to the photo. A control uses the matching photo when present; otherwise a single-image output such as a standalone Generator or Draw applies to every photo. A disjoint multi-image output is ignored. Control images do not enter the main gallery. **View → Show mask overlay** (M) tints the first connected intensity control red without affecting the render, scopes or export.

Graph format 3 removes the Mask category and all fixed mask ports. Use Generator for gradients or radial maps, Draw for painted images, and the image/channel outputs of ordinary nodes for other control maps. Old graphs using removed kinds or ports must be rebuilt; there is no migration.

Click the blue title-bar **+** tab on a Filter node and choose **Radius** to drive radius per pixel from image luminance. Black gives no blur and white gives the slider radius, so the slider still sets the maximum radius. Gaussian and Box use the existing multi-level radius approximation; other kernels use the existing bounded spatial sampler. Unsupported controls are not listed.

The **Filter** node offers low pass and high pass with Gaussian (the original three-box approximation), Box (average), Lens Disk, Motion, Radial Zoom, Radial Spin, Diamond, Median, and Bilateral. High pass shows the signed detail around a neutral midpoint; it is not a sharpening operation. Older Blur nodes load as Gaussian low pass. **Preview Exposure** lives in the viewer toolbar and affects only the preview and display scopes, not exports.

### Adobe XMP export

Choose **Adobe XMP** in an Export node to write ratings and colour labels for the proprietary RAW images that reach it. It writes sidecars to the chosen output folder without rendering or modifying the originals, and skips existing sidecars. Put each sidecar beside its matching RAW before reading metadata in Lightroom Classic or Bridge. Develop adjustments are not exported by this mode.

### Depth Map and Normal Map

**Depth Map** outputs a grayscale relative-depth image (black near, white far; opaque alpha). It requires the opt-in CPU model and worker described in [Depth Map setup](docs/depth-estimate.md). **Normal Map** interprets the linear luminance of its single image input as height and encodes normals as RGB, with strength, smoothing, and Flip Y. If you want near depth to represent higher terrain, invert luminance before the Normal Map. For predictable numeric heights from Generator, choose Data output. Export via **PNG 16-bit data map** at Original size to preserve the encoded channels without photo color transforms.

### Draw

Draw paints white strokes on an opaque black RGBA canvas. Select it to paint in the viewer; set Size, Hardness and Flow in the inspector and hold Alt to erase. With an image input, Draw inherits the collection and dimensions without copying the source pixels. Unconnected, it uses its own Width and Height and produces one image without needing a photo. Draw starts in Individual mode. To paint a photo-specific control, give Draw the same input as the effect, then wire its output to the added control; a compatible pinned preview lets you paint over downstream Crop / Rotate.

### Generator

Generator uses an optional input's dimensions and collection, or produces a standalone image at the configured dimensions. It offers Gradient, seeded Noise, and checker Pattern. Gradient supports Linear, Radial, Angle, Reflected, and Diamond styles, with angle, scale, reverse, interpolation, smoothness, and optional dithering. Click in either ramp lane to add a colour or opacity stop, drag it to move it, or select it to edit its value, position, midpoint to the next stop, or delete it. **Output: Display** (default) interprets colours and stops as sRGB display values and inverts the current viewer tone map after interpolation; the default black-to-white ramp therefore reads 0–1023 on display scopes. Changing the view tone re-evaluates Display generators. Values are clamped to 0–1 before inversion; saturated colours outside the ACES fit’s invertible gamut are projected and can change hue. **Data** preserves the generated numeric channel values directly, for normal maps and other data. When feeding a control input, Display behaves as Data so white is 100% control. Export evaluation uses the session view tone as well; preview exposure is excluded from the inverse and from exports. Export tone-map and colour-space settings still affect the final file. Reverse does not move stored stops. Noise is a separate Generator mode.

Preview filenames, the window title, and `--version` use `YYMM.###` (three-digit monthly build number, starting at `001`). Cargo stores the equivalent unpadded SemVer, such as `26.11.1` for `2611.001`.

### Viewer handles

Some nodes can be edited directly on the picture. The handles appear only while that node
is selected and the pointer is over the viewer, and only when the viewer shows the node's
own result or a later stage reached through nothing but Crop / Rotate nodes (they then
follow the crop and rotation). Drag a handle to change the setting live; each drag is one
undo step. Double-click a handle to reset what it controls. In Individual mode a drag only
changes the photo on screen. Space+drag and the middle button still pan.

- **Generator (Gradient):** Linear shows a line from the start (hollow knob, where the ramp
  begins) to the end (filled knob); drag either end to set angle and length while the other
  end stays put. Radial and Diamond show their outer contour with an edge knob (radius and
  angle); Reflected shows both mirrored ends; Angle shows a direction knob. The small
  hollow centre knob moves the whole gradient (new **Gradient center X / Y** settings,
  default 50 % / 50 %, so older graphs look the same). Double-click the centre to recentre;
  double-click an end to reset angle and scale.
- **Crop / Rotate:** besides the crop frame, a knob above the top edge straightens: drag it
  around the picture to turn the image (−45° to 45°, the fine grid shows while turning).
  Double-click it for 0°. Scale and move are the crop frame itself (corners and inside);
  there are no separate scale or offset settings.
- **Vignette:** three rings at the exact distances the effect uses (the same
  centre-to-half-diagonal metric): the bright ring is the midpoint, the faint inner and
  outer rings are where the feather starts and reaches full strength. Drag the midpoint
  knob (top right) or a feather knob (bottom right).
- **Filter (Motion kernel):** an arrow from the image centre points along the blur; the
  faint line through the centre shows the streak length (the radius). Drag the arrow head
  to set Motion angle. Hidden when a Direction (RGB) map is connected or another kernel is
  chosen.

### Pixel Sort

Pixel Sort stably sorts all complete RGBA pixels across the image, filling each column top-to-bottom from left to right. Brightness is linear ACEScg luminance; saturation is RGB chroma (max minus min) divided by the largest absolute RGB component, or zero for black; hue is the ACEScg RGB chroma-vector angle (neutral hue zero). These metrics accept signed and HDR values without clamping. Ties retain source row-major order; reverse changes the key direction without reversing ties. Pixels with non-finite RGB sort last in either direction. Alpha travels unchanged with its pixel and does not affect its key.

Preview sorts the available proxy image, so its pattern can differ from the full-resolution export: downsampling and sorting do not commute. This global sort uses memory proportional to the number of pixels.

### Sample photos

Some tests and the examples under `crates/*/examples` work on private sample photos, which are
never part of the repository. Two environment variables point to them:

- `RAWGRAPH_SAMPLES`: folder with the sample photos. Default `%LOCALAPPDATA%\rawgraph\samples`
  (on other systems `$HOME/.local/share/rawgraph/samples`).
- `RAWGRAPH_OUT`: folder for pictures the examples generate. Default `%LOCALAPPDATA%\rawgraph\shots`
  (on other systems `$HOME/.local/share/rawgraph/shots`).

Tests that need sample photos print `skipped: no sample folder (set RAWGRAPH_SAMPLES)` and pass
when the folder does not exist or holds no supported file.

State of the prototype (2026-11-15, after wave 8): interactive editing of real RAW/DNG/JPEG files
with the node kinds listed in `node_structure.md`: Source (folder, sub-folders),
Metadata Filter, Merge (same-name rule), Layer (Match pairing, blend modes, opacity, fit, filter),
Develop (exposure, contrast, lights, shadows, white, black, white balance, tint, dynamic,
saturation), Curves (luminance, RGB, Sat vs Sat, Hue vs Hue, Hue vs Sat), Lens Correction (manual
and auto from DNG correction data) and Export (format incl. AVIF, size, output colour space,
metadata selection, folder, collision rule). Also: exposed image controls and paintable Draw images, viewer with
crop and brush tools, node graph with multi-selection, copy/paste and recipes, filmstrip with
culling, XMP rating import and relinking, colourised scopes, project files in format 2 with
revisions, history, graph restore, conflict check and crash recovery, and a packaging script.
The interactive preview runs the trailing pointwise operations and the display transform on the GPU
with automatic CPU fallback; everything else, including export, uses the CPU reference path.
Not yet built: Layer mode "All combinations", HEIC, a lens database, GPU versions of the
non-pointwise effects, HDR output and ACES output transforms, merging of concurrent edits, node
groups and macros, installer and signing, macOS/Linux verification. Interactive use with a real
mouse and keyboard has not been tested by hand yet. Open questions: `docs/offene-punkte.md`.

Update this section as implementation files are added.

## Change log

- 2026-11-15 — Wave 8: nodes of `node_structure.md` (Source folders, Merge rule, Layer, Develop tone, Curves set, Lens Correction auto, Export node), colourised scopes, output colour spaces, AVIF, project format 2 with history and conflict check; new requirements (platforms, project storage and collaboration).
- 2026-11-15 — Wave 7: Lift/Gamma/Gain, Clarity/Texture, more GPU operations, brush behind crop, curve histogram, packaging script.
- 2026-11-15 — Wave 6: GPU preview path in the application with CPU fallback.
- 2026-11-15 — Waves 4 and 5: Develop stage, effects crate, masks, split node, curve, brush, crop, 16-bit export, GPU prototype (see Structure).
- 2026-11-13 — First working prototype: eight workspace crates, real RAW files in the application (see Structure).
- 2026-11-13 — Added persistent per-image Individual-node settings and RAW/JPEG parity of controls, including absolute/relative white-balance requirements.

### Develop local tone v1

Develop offers an opt-in **Local v1** tone processing choice; existing graphs default
to Legacy and the standalone Highlights / Shadows node is unchanged. Local v1 uses
an edge-aware luminance neighborhood for highlight/shadow gains, preserving RGB
detail and unclipped HDR values; Whites and Blacks separately affect endpoints.
This is a first Camera Raw-inspired approximation, not Adobe’s algorithm or a
parity claim. Preview uses the CPU result at this boundary. Compare representative
reference photos and exports against Camera Raw before drawing quality conclusions;
proxy resolution, halos, fine detail, color and performance need evaluation.

### Filter motion direction

Expose **Direction (RGB)** on Filter using Add Input. When connected, it overrides Motion angle.
It reads scene-linear R and G directly as X and Y, decoded by `2 * channel - 1`,
then normalizes XY; B and map alpha are ignored. Neutral (0.5, 0.5), near-zero
or non-finite XY leaves the pixel unblurred. Without a map for an image, Motion
angle applies. Maps are nearest-neighbour scaled; positive Y points down. Motion
blur preserves the source alpha. Other kernels ignore the direction map.

### WebP export

Export nodes and Quick Export support 8-bit WebP with alpha. Choose lossless or lossy compression; quality affects only lossy encoding. Pixels use the chosen display transform and output colour space. EXIF metadata is supported, but ICC profiles are not embedded in WebP; use sRGB for maximum interoperability.
