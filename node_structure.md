# RawGraph node structure

Current inventory for graph format 4, not a list of what shipped in `2611.001`. Node labels, stable keys, port types, choice order and defaults come from the core graph definitions. Tables describe stored settings; the inspector may group settings or hide ones that do not apply.

Graphs of format 1 and 2 (and sub-graphs of format 1) are rewritten to these nodes when loaded; see the README section "Per-pixel image controls" for the mapping of the removed mask kinds and ports.

## Propose changes here

Edit the relevant node section in Git. To distinguish a request from an implemented setting, add a `**Requested changes:**` block under the node. For a new node, give its category, input and output types, defaults, and expected behavior. Keep the node ID and setting keys when proposing a display-name change. Git diffs will then show exactly what to implement.

## Shared graph controls

- **All / Individual:** One shared setting set, or per-image overrides that inherit shared values when unset. These are distinct from a node’s own mode selector.
- **Ports:** All fixed ports carry `Images` collections. Merge and Layer have multiple collection inputs; Split by Metadata has multiple collection outputs.
- **Add Input:** Use the inset blue **+** tab at the left of the title bar. Each exposed input sits on the left edge beside its parameter name and value, which remain visible even at the default value. A scalar control reads clamped linear ACEScg luminance and scales the slider's effect per pixel (black = no effect / neutral value, white = the slider value). Filter Direction instead reads encoded XY in R/G. The matching image is used, otherwise a single-image output applies to every photo; a disjoint multi-image output is ignored. Controls do not join the main collection and a disconnected control leaves its slider in charge.
- **Add Output:** Use the inset red **+** tab at the far right of the title bar, outside the A/I badge; image-producing nodes can expose Luma, R, G, B and A from the first built-in image output. These are additional labelled edge ports, not replacements for the original image output. Right-click an added port to remove it; additions and removals are undoable.

## Collection

### Metadata Filter (`MetadataFilter`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Measure | `measure` | ISO | ISO / Shutter time (s) / Aperture (f) / EV100 / Flash |
| Minimum | `min` | 0 | -100 to 1000000 (log scale) |
| Maximum | `max` | 1000000 | -100 to 1000000 (log scale) |
| Invert | `invert` | Off | Off / On |
| Missing metadata | `missing` | Exclude | Exclude / Include |

### Merge (`Merge`)

- **Inputs:** A: Images (optional); B: Images (optional); C: Images (optional); D: Images (optional)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Same file name | `collision` | Keep both | Keep both / First input wins / Last input wins |

### Split by Metadata (`MetadataSplit`)

Up to six editable named ranges (name, min, max); range names label their output ports. Unmatched has its own output.

- **Inputs:** Images: Images (required)
- **Outputs:** Range 1: Images; Range 2: Images; Range 3: Images; Range 4: Images; Range 5: Images; Range 6: Images; Unmatched: Images
- **Structured settings:** `Ranges`

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Measure | `measure` | ISO | ISO / Shutter time (s) / Aperture (f) / EV100 / Flash |
| Missing metadata | `missing` | Unmatched | Unmatched / Exclude |

### Layer (`Layer`)

- **Inputs:** Layer 1: Images (optional); Layer 2: Images (optional); Layer 3: Images (optional); Layer 4: Images (optional); Layer 5: Images (optional); Layer 6: Images (optional); Layer 7: Images (optional); Layer 8: Images (optional)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Mode | `mode` | Match | Match / All combinations |
| Layer 1 on | `l1_on` | On | Off / On |
| Layer 1 blend | `l1_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 1 opacity | `l1_opacity` | 1 | 0 to 1 |
| Layer 1 fit | `l1_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 1 filter | `l1_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 2 on | `l2_on` | On | Off / On |
| Layer 2 blend | `l2_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 2 opacity | `l2_opacity` | 1 | 0 to 1 |
| Layer 2 fit | `l2_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 2 filter | `l2_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 3 on | `l3_on` | On | Off / On |
| Layer 3 blend | `l3_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 3 opacity | `l3_opacity` | 1 | 0 to 1 |
| Layer 3 fit | `l3_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 3 filter | `l3_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 4 on | `l4_on` | On | Off / On |
| Layer 4 blend | `l4_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 4 opacity | `l4_opacity` | 1 | 0 to 1 |
| Layer 4 fit | `l4_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 4 filter | `l4_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 5 on | `l5_on` | On | Off / On |
| Layer 5 blend | `l5_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 5 opacity | `l5_opacity` | 1 | 0 to 1 |
| Layer 5 fit | `l5_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 5 filter | `l5_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 6 on | `l6_on` | On | Off / On |
| Layer 6 blend | `l6_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 6 opacity | `l6_opacity` | 1 | 0 to 1 |
| Layer 6 fit | `l6_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 6 filter | `l6_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 7 on | `l7_on` | On | Off / On |
| Layer 7 blend | `l7_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 7 opacity | `l7_opacity` | 1 | 0 to 1 |
| Layer 7 fit | `l7_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 7 filter | `l7_filter` | Linear | Linear / Bicubic / Nearest |
| Layer 8 on | `l8_on` | On | Off / On |
| Layer 8 blend | `l8_blend` | Normal | Normal / Add / Subtract / Multiply / Screen / Overlay / Darken / Lighten / Difference |
| Layer 8 opacity | `l8_opacity` | 1 | 0 to 1 |
| Layer 8 fit | `l8_fit` | Fit | Fit / Fill / Crop / Stretch |
| Layer 8 filter | `l8_filter` | Linear | Linear / Bicubic / Nearest |

## Colour

### White Balance (relative) (`WhiteBalance`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Temperature shift | `temp` | 0 | -150 to 150 mired |
| Tint shift | `tint` | 0 | -100 to 100 |

### Saturation (`Saturation`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Add Input:** Saturation (`amount`)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Saturation | `amount` | 1 | 0 to 2 x |

### Hue / Saturation (`HueSaturation`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Red hue | `red_h` | 0 | -1 to 1 |
| Red saturation | `red_s` | 0 | -1 to 1 |
| Red luminance | `red_l` | 0 | -1 to 1 |
| Yellow hue | `yellow_h` | 0 | -1 to 1 |
| Yellow saturation | `yellow_s` | 0 | -1 to 1 |
| Yellow luminance | `yellow_l` | 0 | -1 to 1 |
| Green hue | `green_h` | 0 | -1 to 1 |
| Green saturation | `green_s` | 0 | -1 to 1 |
| Green luminance | `green_l` | 0 | -1 to 1 |
| Cyan hue | `cyan_h` | 0 | -1 to 1 |
| Cyan saturation | `cyan_s` | 0 | -1 to 1 |
| Cyan luminance | `cyan_l` | 0 | -1 to 1 |
| Blue hue | `blue_h` | 0 | -1 to 1 |
| Blue saturation | `blue_s` | 0 | -1 to 1 |
| Blue luminance | `blue_l` | 0 | -1 to 1 |
| Magenta hue | `magenta_h` | 0 | -1 to 1 |
| Magenta saturation | `magenta_s` | 0 | -1 to 1 |
| Magenta luminance | `magenta_l` | 0 | -1 to 1 |

### Black & White (`BlackWhite`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Red | `red` | 0.3 | -1 to 2 |
| Green | `green` | 0.59 | -1 to 2 |
| Blue | `blue` | 0.11 | -1 to 2 |

### Lift / Gamma / Gain (`ColorWheels`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Lift | `lift` | 0 | -0.5 to 0.5 |
| Lift red | `lift_r` | 0 | -0.5 to 0.5 |
| Lift green | `lift_g` | 0 | -0.5 to 0.5 |
| Lift blue | `lift_b` | 0 | -0.5 to 0.5 |
| Gamma | `gamma` | 0 | -1 to 1 |
| Gamma red | `gamma_r` | 0 | -1 to 1 |
| Gamma green | `gamma_g` | 0 | -1 to 1 |
| Gamma blue | `gamma_b` | 0 | -1 to 1 |
| Gain | `gain` | 0 | -2 to 2 EV |
| Gain red | `gain_r` | 0 | -2 to 2 EV |
| Gain green | `gain_g` | 0 | -2 to 2 EV |
| Gain blue | `gain_b` | 0 | -2 to 2 EV |

### LUT (`Lut`)

Applies an Adobe / Resolve `.cube` lookup table (1D, 3D, or a 1D shaper followed by a 3D cube). Without a readable file the image passes through and `validate()` reports the reason.

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Structured settings:** `LutFile` (absolute path of the `.cube` file; per image in Individual mode)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Strength | `strength` | 1 | 0 to 1 |
| Input space | `input_space` | Display (view transform) | Display (view transform) / sRGB / Rec.709 display (clamped) / ACEScct (log) / Linear ACEScg |

## Detail

### Filter (`Blur`)

The inspector displays Radius as a percentage of the long edge; stored values are per mille. Direction (RGB) overrides Motion angle only for Motion Blur. It decodes R/G as XY in [−1, 1], normalizes XY and ignores blue and alpha. Neutral or invalid XY leaves a pixel unblurred.

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Add Input:** Radius (`radius`), Direction (RGB) (`direction`)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Radius | `radius` | 5 | 0 to 1000 ‰ of long edge (log scale) |
| Pass | `pass` | Low pass | Low pass / High pass |
| Kernel | `kernel` | Gaussian | Gaussian / Box / Lens (disk) / Motion / Radial zoom / Radial spin / Shape (diamond) / Median / Bilateral |
| Motion angle | `angle` | 0 | 0 to 360 deg |

### Noise Reduction (`NoiseReduction`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Luma | `luma` | 0 | 0 to 1 |
| Chroma | `chroma` | 0 | 0 to 1 |
| Radius | `radius` | 0.3 | 0.05 to 2 ‰ of long edge |
| ISO adaptive | `iso_adaptive` | Off | Off / On |

### Sharpen (`Sharpen`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Add Input:** Amount (`amount`)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Amount | `amount` | 0 | 0 to 3 |
| Radius | `radius` | 0.15 | 0.05 to 2 ‰ of long edge |
| Threshold | `threshold` | 0 | 0 to 1 |

### Clarity / Texture (`Clarity`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Clarity | `clarity` | 0 | -1 to 1 |
| Texture | `texture` | 0 | -1 to 1 |

## Develop

### Capture EV (`CaptureEv`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| EV change | `delta` | 0 | -6 to 6 EV |

### Develop (`Develop`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Add Input:** Exposure (`exposure`), Contrast (`contrast`), Lights (`highlights`), Shadows (`shadows`), White (`whites`), Black (`blacks`)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Exposure | `exposure` | 0 | -5 to 5 EV |
| Contrast | `contrast` | 0 | -1 to 1 |
| Lights | `highlights` | 0 | -1 to 1 |
| Shadows | `shadows` | 0 | -1 to 1 |
| White | `whites` | 0 | -1 to 1 |
| Black | `blacks` | 0 | -1 to 1 |
| White balance | `wb` | As shot | As shot / Custom |
| Temperature | `temperature` | 6500 | 2000 to 25000 K (log scale) |
| Tint | `tint` | 0 | -150 to 150 |
| Dynamic | `vibrance` | 0 | -1 to 1 |
| Saturation | `saturation` | 0 | -1 to 1 |
| Baseline exposure | `baseline` | On | Off / On |
| Default baseline | `baseline_default` | 0.35 | -2 to 3 EV |
| Brightness | `brightness` | 1.6 | -3 to 3 EV |
| Legacy tone | `legacy_tone` | Off | Off / On |

## Effects

### Vignette (`Vignette`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Amount | `amount` | 0 | -1 to 1 |
| Midpoint | `midpoint` | 0.5 | 0 to 1 |
| Feather | `feather` | 0.5 | 0 to 1 |

### Pixel Sort (`PixelSort`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Sort by | `metric` | Brightness | Brightness / Saturation / Hue |
| Reverse | `reverse` | Off | Off / On |

## Generators

### Draw (`Draw`)

Paint or erase in the viewer. Size, Hardness and Flow are brush-tool settings; the node stores normalized strokes. Draw starts in Individual mode and renders white coverage on opaque black. An optional input supplies dimensions and membership, not background pixels. With no input the canvas is a standalone image at Width × Height.

- **Inputs:** Images: Images (optional)
- **Outputs:** Images: Images
- **Structured settings:** `Strokes`

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Width | `width` | 1024 | 1 to 8192 px |
| Height | `height` | 1024 | 1 to 8192 px |

### Normal Map (`NormalMap`)

Interprets linear image luminance as height and encodes the resulting normals as RGB. Choose Data output on an upstream Generator to use its literal height values.

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Strength | `strength` | 1 | 0 to 100 |
| Smoothing | `smoothing` | 0 | 0 to 8 px |
| Flip Y | `flip_y` | Off | Off / On |

### Depth Map (`DepthMap`)

Outputs opaque grayscale relative depth (black near, white far). Requires the optional worker and model described in [Depth Map setup](docs/depth-estimate.md).

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

### Generator (`Generator`)

Image input is optional: it supplies dimensions and collection membership when connected; otherwise Generator produces a standalone image. Gradient has separate draggable colour and opacity ramps, each with 2–256 stops. Select a stop to edit its position, value or midpoint to the next stop, or to delete it. The structured setting is `GradientRamp`; old scalar ramp values are fallback data. Noise remains a separate mode. Display values are inverse-view transformed with the current session tone map (excluding preview exposure); Data preserves numeric values. Control-input evaluation uses data values. Export evaluation uses the session view tone. Saturated ACES-fit colours outside the invertible gamut are projected and can change hue. Gradient center X / Y place the gradient origin in percent of the frame (50 / 50 is the centre, the old fixed origin, so existing graphs render identically); the viewer handles edit center, angle and scale on the picture.

- **Inputs:** Images: Images (optional)
- **Outputs:** Images: Images
- **Structured settings:** `GradientRamp`

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Width | `width` | 1024 | 1 to 8192 px |
| Height | `height` | 1024 | 1 to 8192 px |
| Type | `type` | Gradient | Gradient / Noise / Pattern |
| Vertical gradient (legacy) | `vertical` | Off | Off / On |
| Gradient style | `gradient_style` | Linear | Linear / Radial / Angle / Reflected / Diamond |
| Gradient angle | `gradient_angle` | 0 | -180 to 180 deg |
| Gradient scale | `gradient_scale` | 100 | 1 to 400 % |
| Gradient center X | `gradient_center_x` | 50 | -100 to 200 % |
| Gradient center Y | `gradient_center_y` | 50 | -100 to 200 % |
| Reverse gradient | `gradient_reverse` | Off | Off / On |
| Start red | `start_r` | 0 | -4 to 4 |
| Start green | `start_g` | 0 | -4 to 4 |
| Start blue | `start_b` | 0 | -4 to 4 |
| End red | `end_r` | 1 | -4 to 4 |
| End green | `end_g` | 1 | -4 to 4 |
| End blue | `end_b` | 1 | -4 to 4 |
| Use middle colour stop | `ramp_middle_color` | Off | Off / On |
| Middle colour position | `ramp_color_position` | 50 | 0 to 100 % |
| Middle red | `ramp_color_r` | 0.5 | -4 to 4 |
| Middle green | `ramp_color_g` | 0.5 | -4 to 4 |
| Middle blue | `ramp_color_b` | 0.5 | -4 to 4 |
| Colour midpoint 1 | `ramp_color_midpoint_1` | 50 | 1 to 99 % |
| Colour midpoint 2 | `ramp_color_midpoint_2` | 50 | 1 to 99 % |
| Start opacity | `ramp_alpha_start` | 100 | 0 to 100 % |
| Use middle opacity stop | `ramp_middle_alpha` | Off | Off / On |
| Middle opacity position | `ramp_alpha_position` | 50 | 0 to 100 % |
| Middle opacity | `ramp_alpha_middle` | 100 | 0 to 100 % |
| End opacity | `ramp_alpha_end` | 100 | 0 to 100 % |
| Opacity midpoint 1 | `ramp_alpha_midpoint_1` | 50 | 1 to 99 % |
| Opacity midpoint 2 | `ramp_alpha_midpoint_2` | 50 | 1 to 99 % |
| Ramp interpolation | `ramp_interpolation` | Linear | Linear / Smooth |
| Ramp smoothness | `ramp_smoothness` | 100 | 0 to 100 % |
| Ramp dither | `ramp_dither` | Off | Off / On |
| Noise seed | `seed` | 0 | 0 to 65535 |
| Pattern cell size | `cell_size` | 32 | 1 to 1024 px |
| Invert pattern | `invert` | Off | Off / On |
| Output | `output` | Display | Display / Data |

## Geometry

### Lens Correction (`LensManual`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Mode | `mode` | Manual | Manual / Auto |
| Distortion | `distortion` | 0 | -0.5 to 0.5 |
| Vignetting | `vignetting` | 0 | 0 to 3 EV at corner |
| Red fringe | `ca_red` | 0 | -5 to 5 ‰ |
| Blue fringe | `ca_blue` | 0 | -5 to 5 ‰ |

### Crop / Rotate (`Transform`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Crop left | `left` | 0 | 0 to 1 |
| Crop top | `top` | 0 | 0 to 1 |
| Crop right | `right` | 1 | 0 to 1 |
| Crop bottom | `bottom` | 1 | 0 to 1 |
| Straighten | `angle` | 0 | -45 to 45 deg |
| Rotate | `rotate` | 0° | 0° / 90° / 180° / 270° |
| Flip horizontal | `flip_h` | Off | Off / On |
| Flip vertical | `flip_v` | Off | Off / On |

## Input/Output

### Source (`Source`)

Choose a source folder; a new Source is empty until it is bound. Include subfolders is optional.

- **Inputs:** None
- **Outputs:** Images: Images
- **Structured settings:** `Folder`

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Include subfolders | `subfolders` | On | Off / On |

### Output (`Output`)

Legacy compatibility only: hidden from the add-node UI and no longer part of new graphs.

- **Inputs:** Images: Images (required)
- **Outputs:** None

### Export (`Export`)

Structured fields: output folder, filename template, artist, copyright, and description. The inspector groups PNG and TIFF by format with a separate 8/16-bit selector; the table lists their underlying stored choices. PNG 16-bit data map bypasses photo colour transforms. Viewing JPEG or 8-bit PNG shows an automatic encode/decode preview without writing a file. XMP is metadata-only; other codecs do not currently preview. HEIC is not implemented.

- **Inputs:** Images: Images (required)
- **Outputs:** None
- **Structured settings:** `Export`

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Format | `format` | JPEG | JPEG / PNG / PNG 16-bit / TIFF / TIFF 16-bit / EXR / AVIF / Adobe XMP sidecar / PNG 16-bit data map / WebP |
| Quality | `quality` | 92 | 1 to 100 |
| WebP lossless | `webp_lossless` | Off | Off / On |
| Size | `size` | Original | Original / Long edge / Percent |
| Long edge | `long_edge` | 2048 | 16 to 32000 px (log scale) |
| Percent | `percent` | 100 | 1 to 100 % |
| Colour space | `color_space` | sRGB | sRGB / Display P3 / Adobe RGB (1998) / Rec. 2020 |
| Embed colour profile | `profile` | On | Off / On |
| If the file exists | `collision` | Skip | Skip / Overwrite / Rename |
| Camera data | `meta_camera` | On | Off / On |
| Exposure data | `meta_exposure` | On | Off / On |
| Date | `meta_date` | On | Off / On |
| GPS location | `meta_gps` | Off | Off / On |
| Author | `meta_author` | On | Off / On |
| Software | `meta_software` | On | Off / On |

## Tone

### Exposure (`Exposure`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Add Input:** Exposure (`ev`)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Exposure | `ev` | 0 | -6 to 6 stops |

### Contrast (`Contrast`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Add Input:** Amount (`amount`)

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Amount | `amount` | 0 | -1 to 1 |
| Pivot | `pivot` | 0.18 | 0.02 to 1 scene linear (log scale) |

### Highlights / Shadows (`HighlightsShadows`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Highlights | `highlights` | 0 | -1 to 1 |
| Shadows | `shadows` | 0 | -1 to 1 |

### Tone Curve (`ToneCurve`)

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Shadows | `shadows` | 0 | -1 to 1 |
| Darks | `darks` | 0 | -1 to 1 |
| Lights | `lights` | 0 | -1 to 1 |
| Highlights | `highlights` | 0 | -1 to 1 |

### Curves (`PointCurve`)

Interactive control points per channel. The current curve set and the legacy single curve are both supported.

- **Inputs:** Images: Images (required)
- **Outputs:** Images: Images
- **Structured settings:** `Curve`, `CurveSet`

| Setting | Key | Default | Range or choices |
| --- | --- | --- | --- |
| Channel | `channel` | Luminance | Luminance / Red / Green / Blue / Sat vs Sat / Hue vs Hue / Hue vs Sat |
