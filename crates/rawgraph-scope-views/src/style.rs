//! Visual style shared by all scope views.

use egui::Color32;

/// Unit of the vertical axis of waveform / parade scales.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScaleKind {
    /// 10-bit code values: 0, 128, 256, ... 896, 1023.
    #[default]
    TenBit,
    /// Percent, 0 to 100.
    Percent,
}

impl ScaleKind {
    pub fn label(self) -> &'static str {
        match self {
            ScaleKind::TenBit => "10-bit",
            ScaleKind::Percent => "%",
        }
    }
}

/// What the waveform shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WaveformMode {
    /// Luma (Y') trace.
    #[default]
    Luma,
    /// Additive R/G/B overlay.
    Rgb,
    Red,
    Green,
    Blue,
}

impl WaveformMode {
    pub const ALL: [WaveformMode; 5] = [
        WaveformMode::Luma,
        WaveformMode::Rgb,
        WaveformMode::Red,
        WaveformMode::Green,
        WaveformMode::Blue,
    ];

    pub fn label(self) -> &'static str {
        match self {
            WaveformMode::Luma => "Luma",
            WaveformMode::Rgb => "RGB",
            WaveformMode::Red => "R",
            WaveformMode::Green => "G",
            WaveformMode::Blue => "B",
        }
    }

    /// Single channel the waveform is computed for (`None` for the RGB overlay).
    pub fn channel(self) -> Option<rawgraph_scopes::WaveformChannel> {
        use rawgraph_scopes::WaveformChannel as C;
        match self {
            WaveformMode::Luma => Some(C::Luma),
            WaveformMode::Rgb => None,
            WaveformMode::Red => Some(C::Red),
            WaveformMode::Green => Some(C::Green),
            WaveformMode::Blue => Some(C::Blue),
        }
    }
}

/// How the histogram is drawn (same data in every mode).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum HistogramMode {
    /// R, G, B and luma over each other.
    #[default]
    Overlay,
    /// Four rows: R, G, B, luma.
    Stacked,
    /// Luma only.
    Luma,
}

impl HistogramMode {
    pub const ALL: [HistogramMode; 3] =
        [HistogramMode::Overlay, HistogramMode::Stacked, HistogramMode::Luma];

    pub fn label(self) -> &'static str {
        match self {
            HistogramMode::Overlay => "Overlay",
            HistogramMode::Stacked => "Stacked",
            HistogramMode::Luma => "Luma",
        }
    }
}

/// Menu / title text of a parade mode.
pub fn parade_mode_label(mode: rawgraph_scopes::ParadeMode) -> &'static str {
    match mode {
        rawgraph_scopes::ParadeMode::Rgb => "RGB",
        rawgraph_scopes::ParadeMode::Yrgb => "YRGB",
        rawgraph_scopes::ParadeMode::YCbCr => "YCbCr",
    }
}

/// Appearance and behaviour switches for the scope views.
#[derive(Clone, Debug, PartialEq)]
pub struct ScopeStyle {
    /// Trace brightness multiplier (1.0 = neutral). Also scales histogram height.
    pub gain: f32,
    /// Draw graticule lines, targets and labels.
    pub graticule: bool,
    /// Waveform / parade vertical scale.
    pub scale: ScaleKind,
    /// "Colorize" (default on): the traces are coloured like the picture. Luma waveform:
    /// every point takes the colour of the content that produced it; RGB overlay: pure
    /// red, green and blue, added; vectorscope: hue and saturation of the position;
    /// histogram: bright R, G, B areas that add up to white. Off gives the previous look
    /// (single phosphor colour; tinted R, G, B). Needs new data when changed.
    pub colourise: bool,
    /// Vectorscope: magnify the centre region by 2x.
    pub zoom: bool,
    /// Waveform: show the additive R/G/B overlay instead of the luma trace.
    pub waveform_rgb: bool,
    /// Histogram: logarithmic vertical scale.
    pub histogram_log: bool,
    /// Vectorscope: also draw the 100 % target boxes.
    pub targets_100: bool,
    /// Vectorscope: draw the skin-tone line.
    pub skin_line: bool,
    /// Soft glow around single-texel waveform / parade traces.
    pub glow: bool,
    /// Hover readouts (cursor line / values under the pointer).
    pub readout: bool,
    /// Waveform mode; `waveform_rgb = true` still forces the RGB overlay.
    pub waveform_mode: WaveformMode,
    /// Parade layout (needs new data when changed).
    pub parade_mode: rawgraph_scopes::ParadeMode,
    /// Histogram drawing mode.
    pub histogram_mode: HistogramMode,
    /// Horizontal low-pass over waveform / parade columns (needs new data when changed).
    pub lowpass: bool,
    /// One-line readout under the toolbar: clipping shares (only when something is
    /// clipped) and the mean saturation.
    pub clip_readout: bool,
    /// Thin marker lines at the top / bottom edge of waveform / parade columns that
    /// contain clipped pixels.
    pub clip_markers: bool,
    /// Replaces the real pointer position (screen coordinates); for tests and screenshots.
    pub pointer_override: Option<egui::Pos2>,
    pub background: Color32,
    pub graticule_color: Color32,
    /// Slightly brighter graticule colour used for outer rings, targets, 0 / 100 lines.
    pub graticule_strong: Color32,
    pub label_color: Color32,
    pub label_size: f32,
}

impl Default for ScopeStyle {
    fn default() -> Self {
        Self {
            gain: 1.0,
            graticule: true,
            scale: ScaleKind::TenBit,
            colourise: true,
            zoom: false,
            waveform_rgb: false,
            histogram_log: false,
            targets_100: false,
            skin_line: true,
            glow: true,
            readout: true,
            waveform_mode: WaveformMode::Luma,
            parade_mode: rawgraph_scopes::ParadeMode::Rgb,
            histogram_mode: HistogramMode::Overlay,
            lowpass: false,
            clip_readout: true,
            clip_markers: true,
            pointer_override: None,
            background: Color32::BLACK,
            graticule_color: Color32::from_rgb(62, 62, 66),
            graticule_strong: Color32::from_rgb(98, 98, 104),
            label_color: Color32::from_rgb(140, 140, 146),
            label_size: 10.0,
        }
    }
}

/// Tint of the luma waveform / vectorscope trace (near-white with a green cast).
pub const LUMA_TINT: [f32; 3] = [0.72, 1.0, 0.78];
/// Tints of the R, G, B waveform / parade traces.
pub const CHANNEL_TINTS: [[f32; 3]; 3] = [[1.0, 0.22, 0.22], [0.28, 1.0, 0.32], [0.30, 0.48, 1.0]];

/// Pure red, green and blue: the traces of the RGB overlay in the Colorize look, so that
/// equal channels add up to a neutral trace.
pub const PRIMARY_TINTS: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Tint of the Cb trace of the YCbCr parade.
pub const CB_TINT: [f32; 3] = [0.30, 0.55, 1.0];
/// Tint of the Cr trace of the YCbCr parade.
pub const CR_TINT: [f32; 3] = [1.0, 0.36, 0.30];

/// Trace tints of the parade planes, left to right.
pub fn plane_tints(mode: rawgraph_scopes::ParadeMode) -> Vec<[f32; 3]> {
    use rawgraph_scopes::ParadeMode as M;
    match mode {
        M::Rgb => CHANNEL_TINTS.to_vec(),
        M::Yrgb => vec![LUMA_TINT, CHANNEL_TINTS[0], CHANNEL_TINTS[1], CHANNEL_TINTS[2]],
        M::YCbCr => vec![LUMA_TINT, CB_TINT, CR_TINT],
    }
}
