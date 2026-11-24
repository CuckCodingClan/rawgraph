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

CI (`.github/workflows/ci.yml`): Linux clippy + tests on every push to `main` and every pull request, Windows tests on pull requests, macOS only when started manually.

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
