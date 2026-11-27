# RawGraph

Local-first RAW photo analysis, organization, and non-destructive editing through reusable node graphs. A professional tool inspired by the node workflows of Cinema 4D Xpresso and Fusion.

## Requirements

- **Desktop platforms:** native desktop application for Windows, macOS, and Linux, with consistent project/workflow behavior across platforms.
- **Project storage and collaboration:** store catalogs, graphs, parameters, and project metadata in a versioned database-backed project format. Keep source images external and referenced, not embedded by default. Design the data model for future collaboration (stable IDs, transactions, migrations, change history, conflict handling); choose local and shared database deployment after evaluating single-user and multi-user needs.
- **Rust performance core:** RAW processing, image operations, graph evaluation, metadata/catalog ingestion, caching, batch execution, and export. Keep pixel data and hot paths in Rust; any UI boundary must be efficient and versioned.
- **Professional scopes:** histogram, waveform, RGB parade, and vectorscope, targeting DaVinci Resolve-level capability and usability. Colorized. Define color-space semantics and validate accuracy with known test patterns.
- **Display layouts:** provide Resolve-like selectable single- and dual-monitor workspaces, with optional clean feed and/or scopes on a third or fourth display; restore sensibly when displays are disconnected or rearranged.
- **Node workflows:** typed data flow for images, collections, metadata, masks, and scopes; editable graphs, batch execution, reusable recipes, and clear validation/errors.
- **RAW compatibility:** prioritize broad camera/file compatibility in the initial product; matching Adobe Camera Raw or Capture One rendering quality is a later optimization, not an MVP gate.
- **Parametric, non-destructive editing:** preserve editable operation parameters and source data throughout the graph; make edits reproducible, adjustable, and reversible.
- **Batch and per-image control:** nodes are created in **All** mode (same parameters for every incoming image) or **Individual** mode. Individual-mode nodes store parameter values keyed to each adjusted image identity, so settings persist independently of the current selection. Nodes compose in graph order, allowing shared edits and per-image tweaks together.
- **RAW and JPEG editing:** JPEGs use the same adjustment controls and graph workflow as RAWs, while respecting their lower latitude and already-rendered image data. White balance supports absolute settings and relative offsets on both formats; RAW absolute WB uses capture metadata as reference, while JPEG behavior accounts for baked-in WB and limited recovery.
- **Exposure values:** calculate and expose a capture-derived exposure value per image from ISO, shutter time, and aperture metadata. Let users edit values per image and apply collection-wide offsets (adding the same offset to each image's value), using parametric nodes that compose in graph order. Define units/reference and missing/inaccurate metadata behavior. Keep this distinct from pixel-based exposure compensation; JPEGs share controls but cannot recover clipped/discarded data.
- **Masks as effect controls:** effects accept single-channel masks/maps to modulate numeric parameters where spatial variation is meaningful (for example, blur radius). Map values linearly to a parameter range by default; curves/remapping nodes can reshape the mapping. Discrete or categorical controls need explicit semantics rather than blindly interpolating values. This is distinct from merely blending the effect output.
- **HDR and precision:** use ACES as the internal working-space foundation, with a 32-bit floating-point image-processing flow and correct HDR handling throughout preview, scopes, and export. Specify the ACES variant, transfer/display transforms, and output targets during architecture work.

## Roadmap

### Performance is a standing priority

RawGraph should feel snappy on the first view of a RAW, not just after it is cached. A wait of over two seconds for the first visible frame from an unedited Source is unacceptable. Treat responsiveness as a requirement for new features, and prioritize measured performance improvements when no feature requests are queued.

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
- [ ] Add mask/map inputs to effects, with linear modulation of supported numeric parameters and composable curve/range remapping; test with spatially varying blur radius as a reference case.
- [ ] Establish camera/lens profile and color-managed preview/export strategy using ACES and a 32-bit floating-point pipeline; validate HDR scopes, display transforms, and HDR export.
- [ ] Implement histogram, waveform, RGB parade, and vectorscope in the Rust rendering path; define sampling, modes, scales, color-space interpretation, and update behavior.
- [ ] Verify scope values against generated reference patterns and high-bit-depth inputs; compare capabilities and workflow with DaVinci Resolve.
- [ ] Compare RAW output against selected reference converters; document camera-specific limitations rather than claiming unverified parity.

### 5. Professional release
- [ ] Add masks/local adjustments, batch presets, graph groups/macros, and reusable templates.
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
                               #   vignette, crop/rotate, lens correction, masks, brush strokes, curves,
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

For quick testing on Windows, double-click `RawGraph Dev.cmd` in the repository root. It builds and runs the current source checkout in release mode, with build output in `..\rawgraph-target-windows`. Later runs rebuild only what changed. If the build fails, the window stays open to show the error.

CI (`.github/workflows/ci.yml`): Linux clippy + tests on every push to `main` and every pull request, Windows tests on pull requests, macOS only when started manually.

### Graph workspace

Click a node to preview its result. Pin the viewer to keep that stage visible while editing another node. With no selected or pinned node, there is no preview. New graphs no longer include an Output node; old graphs can still be opened. Use an Export node to save files or Quick Export to write from the stage you are viewing.

The gallery on the left shows only images that pass through the selected node. Split Viewer has independent image and node pins for both panes; whichever dimension is unpinned follows the current selection. The single viewer uses fit-relative click zoom (2× or 3×), and the View menu switches wheel behavior between zooming and stepping images. Drag a connection from a port to blank canvas to add and wire a compatible node.

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

Enable **Debug → Per-node CPU timing** to profile the next preview in a floating window.
Rows report exclusive CPU wall time (upstream node evaluation excluded), number of
computations, and result-cache hits for that preview. Cache hits are reported separately,
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

Connect multiple Sources or edit branches to the same ordinary image input. The node processes their combined collection; if an image arrives more than once, the first connected branch carrying it supplies its pixels. Use Merge for explicit filename collision rules. Mask ports and the individual Merge and Layer ports still accept one wire each.

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

### Source nodes

A new Source, including the starter Source, supplies no images until you choose a folder. Clearing the folder makes it empty again. **File → Open folder** assigns the path to a Source before scanning; it does not create an input shared by all nodes. Older saved Sources with no folder setting retain their catalog-wide behavior so saved graphs keep their inputs.

Right-click a Develop node and choose **Add Input** to expose Exposure, Contrast, Highlights, Shadows, Whites, or Blacks as an image-intensity or mask input. A connected map replaces that slider per pixel: 0–1 maps linearly across the slider's full range, with values outside 0–1 clamped. Image inputs use linear ACEScg luminance; use an exposed image channel if you need a particular channel. Both Legacy and Local v1 tone processing support these inputs. An unconnected input leaves the slider in control.

Right-click a Filter node and choose **Add Input → Radius** to drive radius per pixel from image luminance. Values 0–1 replace the radius slider across its full range; the built-in Radius mask, if connected, additionally scales that radius. Gaussian and Box use the existing multi-level radius approximation; other kernels use the existing bounded spatial sampler. Unsupported controls are not listed.

The **Filter** node offers low pass and high pass with Gaussian (the original three-box approximation), Box (average), Lens Disk, Motion, Radial Zoom, Radial Spin, Diamond, Median, and Bilateral. High pass shows the signed detail around a neutral midpoint; it is not a sharpening operation. Older Blur nodes load as Gaussian low pass. **Preview Exposure** lives in the viewer toolbar and affects only the preview and display scopes, not exports.

### Adobe XMP export

Choose **Adobe XMP** in an Export node to write ratings and colour labels for the proprietary RAW images that reach it. It writes sidecars to the chosen output folder without rendering or modifying the originals, and skips existing sidecars. Put each sidecar beside its matching RAW before reading metadata in Lightroom Classic or Bridge. Develop adjustments are not exported by this mode.

### Depth and normals

**Depth Estimate** outputs a relative-depth mask (black near, white far). It requires the opt-in CPU model and worker described in [Depth Estimate setup](docs/depth-estimate.md). Use Mask Adjust to remap or invert it. **Normals** converts a depth mask or an explicitly selected luminance height map to RGB normals, with strength, smoothing, and Flip Y. Export via **PNG 16-bit data map** at Original size to preserve the encoded channels without photo color transforms.

### Generator

Generator uses an optional input's dimensions and collection, or produces a standalone image at the configured dimensions. It offers Gradient, seeded Noise, and checker Pattern. Gradient supports Linear, Radial, Angle, Reflected, and Diamond styles, with angle, scale, reverse, interpolation, smoothness, and optional dithering. Click in either ramp lane to add a colour or opacity stop, drag it to move it, or select it to edit its value, position, midpoint to the next stop, or delete it. The default ramp has exact black and white data endpoints; Reverse does not move stored stops. Noise is a separate Generator mode. Viewer tone mapping can make data values look different on screen.

Preview filenames, the window title, and `--version` use `YYMM.###` (three-digit monthly build number, starting at `001`). Cargo stores the equivalent unpadded SemVer, such as `26.11.1` for `2611.001`.

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
with 33 node kinds, including every node of `node_structure.md`: Source (folder, sub-folders),
Metadata Filter, Merge (same-name rule), Layer (Match pairing, blend modes, opacity, fit, filter),
Develop (exposure, contrast, lights, shadows, white, black, white balance, tint, dynamic,
saturation), Curves (luminance, RGB, Sat vs Sat, Hue vs Hue, Hue vs Sat), Lens Correction (manual
and auto from DNG correction data) and Export (format incl. AVIF, size, output colour space,
metadata selection, folder, collision rule). Also: masks incl. painted brush masks, viewer with
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

The Filter node’s optional Direction normal (RGB) image input overrides Motion angle.
It reads scene-linear R and G directly as X and Y, decoded by `2 * channel - 1`,
then normalizes XY; B and map alpha are ignored. Neutral (0.5, 0.5), near-zero
or non-finite XY leaves the pixel unblurred. Without a map for an image, Motion
angle applies. Maps are nearest-neighbour scaled; positive Y points down. Motion
blur preserves the source alpha. Other kernels ignore the direction map.

### WebP export

Export nodes and Quick Export support 8-bit WebP with alpha. Choose lossless or lossy compression; quality affects only lossy encoding. Pixels use the chosen display transform and output colour space. EXIF metadata is supported, but ICC profiles are not embedded in WebP; use sRGB for maximum interoperability.
