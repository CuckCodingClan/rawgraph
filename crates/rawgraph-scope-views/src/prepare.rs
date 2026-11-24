//! CPU-side preparation of scope pixels. Pure and thread-safe: run [`prepare`] on any
//! thread (for example the render worker) and hand the result to
//! [`crate::ScopePanel::set_prepared`] on the UI thread.

use crate::readout::mean_saturation;
use crate::render::{
    colored_trace_to_rgba, edge_flags, layers_to_rgba_glow, merge_edges, reference_of,
    vectorscope_to_rgba, Layer,
};
use crate::style::{plane_tints, WaveformMode, CHANNEL_TINTS, LUMA_TINT, PRIMARY_TINTS};
use rawgraph_scopes::{
    ClipStats, ParadeMode, ScopeRequest, ScopeSet, Waveform, WaveformChannel,
};

/// Waveform resolution (columns x levels).
pub const WAVEFORM_COLUMNS: usize = 512;
pub const WAVEFORM_LEVELS: usize = 256;
/// Parade resolution per channel (512 columns when the waveform shows the RGB overlay).
pub const PARADE_COLUMNS: usize = 256;
pub const PARADE_LEVELS: usize = 256;
pub const VECTORSCOPE_SIZE: usize = 256;
pub const HISTOGRAM_BINS: usize = 256;

/// Everything about the panel that affects the prepared pixels.
#[derive(Clone, Debug, PartialEq)]
pub struct PrepareSettings {
    pub waveform: bool,
    pub parade: bool,
    pub vectorscope: bool,
    pub histogram: bool,
    /// Waveform shows the additive R/G/B overlay (needs the parade data at waveform width).
    /// Same as `waveform_mode == WaveformMode::Rgb`; either one switches the overlay on.
    pub rgb_overlay: bool,
    pub gain: f32,
    /// "Colorize" (default on): trace colours follow the picture, see
    /// [`crate::ScopeStyle::colourise`]. Off reproduces the previous monochrome look.
    pub colourise: bool,
    pub glow: bool,
    /// Pixel sampling step passed to `analyze` (1 = every pixel).
    pub step: usize,
    /// What the waveform shows (single channels need no extra data).
    pub waveform_mode: WaveformMode,
    /// Parade planes.
    pub parade_mode: ParadeMode,
    /// Horizontal low-pass over waveform / parade columns.
    pub lowpass: bool,
    /// Compute the clipping statistics.
    pub clipping: bool,
}

impl Default for PrepareSettings {
    fn default() -> Self {
        Self {
            waveform: true,
            parade: true,
            vectorscope: true,
            histogram: true,
            rgb_overlay: false,
            gain: 1.0,
            colourise: true,
            glow: true,
            step: 1,
            waveform_mode: WaveformMode::Luma,
            parade_mode: ParadeMode::Rgb,
            lowpass: false,
            clipping: true,
        }
    }
}

impl PrepareSettings {
    /// The waveform shows the additive RGB overlay.
    fn overlay(&self) -> bool {
        self.waveform && (self.rgb_overlay || self.waveform_mode == WaveformMode::Rgb)
    }

    /// What `rawgraph_scopes::analyze` must compute for these settings: only visible
    /// scopes (the RGB overlay is drawn from parade data at waveform resolution, from
    /// a separate overlay set when the parade is in YCbCr mode).
    pub fn request(&self) -> ScopeRequest {
        let overlay = self.overlay();
        let wave_dims = (WAVEFORM_COLUMNS, WAVEFORM_LEVELS);
        let (parade, parade_mode, extra_overlay) = if overlay {
            match (self.parade, self.parade_mode) {
                (true, ParadeMode::YCbCr) => {
                    (Some((PARADE_COLUMNS, PARADE_LEVELS)), ParadeMode::YCbCr, Some(wave_dims))
                }
                (true, ParadeMode::Yrgb) => (Some(wave_dims), ParadeMode::Yrgb, None),
                _ => (Some(wave_dims), ParadeMode::Rgb, None),
            }
        } else if self.parade {
            (Some((PARADE_COLUMNS, PARADE_LEVELS)), self.parade_mode, None)
        } else {
            (None, ParadeMode::Rgb, None)
        };
        let waveform_channel = self.waveform_mode.channel().unwrap_or(WaveformChannel::Luma);
        ScopeRequest {
            waveform: (self.waveform && !overlay).then_some(wave_dims),
            parade,
            vectorscope: self.vectorscope.then_some(VECTORSCOPE_SIZE),
            histogram: self.histogram.then_some(HISTOGRAM_BINS),
            step: self.step.max(1),
            waveform_channel,
            parade_mode,
            overlay: extra_overlay,
            lowpass: self.lowpass && (self.waveform || self.parade),
            clip_stats: self.clipping,
            // Only the luma waveform takes the colour of the picture content.
            colorize: self.colourise && waveform_channel == WaveformChannel::Luma,
        }
    }
}

/// A finished premultiplied-RGBA8 buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedTrace {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    /// Per column flags ([`crate::render::EDGE_HIGH`] / [`crate::render::EDGE_LOW`]) of
    /// columns whose top / bottom row holds counts (clipped pixels). Empty when the
    /// trace has no such notion (Cb / Cr planes, vectorscope).
    pub edges: Vec<u8>,
}

/// Histogram counts for drawing and readout.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedHistogram {
    pub r: Vec<u32>,
    pub g: Vec<u32>,
    pub b: Vec<u32>,
    pub luma: Vec<u32>,
}

/// Result of [`prepare`]; contains no egui types and is `Send`.
#[derive(Clone, Debug, PartialEq)]
pub struct PreparedScopes {
    /// The settings this data was prepared with.
    pub settings: PrepareSettings,
    pub waveform: Option<PreparedTrace>,
    /// The RGB parade ([`ParadeMode::Rgb`]); other modes fill `parade_planes`.
    pub parade: Option<[PreparedTrace; 3]>,
    /// Planes of the YRGB / YCbCr parade, left to right.
    pub parade_planes: Option<Vec<PreparedTrace>>,
    /// Mode of the prepared parade (taken from the data, not from `settings`).
    pub parade_mode: ParadeMode,
    pub vectorscope: Option<PreparedTrace>,
    pub histogram: Option<PreparedHistogram>,
    /// Clipping statistics, when the set had them.
    pub clipping: Option<ClipStats>,
    /// Mean saturation of the image in percent of the 100 % gamut edge.
    pub mean_saturation: Option<f32>,
}

impl PreparedScopes {
    /// The parade traces, left to right, whatever the mode.
    pub fn parade_traces(&self) -> Option<&[PreparedTrace]> {
        match (&self.parade_planes, &self.parade) {
            (Some(p), _) => Some(p),
            (None, Some(p)) => Some(p),
            _ => None,
        }
    }
}

/// Parade mode a set was computed with: the planes tell.
fn parade_mode_of(set: &ScopeSet) -> ParadeMode {
    match &set.parade_planes {
        Some(p) if p.len() == 4 => ParadeMode::Yrgb,
        Some(_) => ParadeMode::YCbCr,
        None => ParadeMode::Rgb,
    }
}

/// The R, G, B waveforms for the overlay, wherever the set carries them.
fn overlay_planes(set: &ScopeSet) -> Option<[&Waveform; 3]> {
    if let Some(o) = &set.overlay {
        return Some([&o[0], &o[1], &o[2]]);
    }
    if let Some(p) = &set.parade {
        return Some([&p[0], &p[1], &p[2]]);
    }
    match &set.parade_planes {
        Some(p) if p.len() == 4 => Some([&p[1], &p[2], &p[3]]),
        _ => None,
    }
}

/// Marker flags of a trace: the unfiltered flags of the set when it has them (low-pass),
/// otherwise derived from the counts.
fn edges_of(w: &Waveform, unfiltered: Option<&Vec<u8>>) -> Vec<u8> {
    match unfiltered {
        Some(f) if f.len() == w.columns => f.clone(),
        _ => edge_flags(&w.counts, w.columns, w.levels),
    }
}

/// Turns scope data into pixels. Deterministic; identical for every caller.
pub fn prepare(set: &ScopeSet, settings: &PrepareSettings) -> PreparedScopes {
    let s = settings;
    let gain = s.gain;
    let mode = parade_mode_of(set);
    let mut out = PreparedScopes {
        settings: s.clone(),
        waveform: None,
        parade: None,
        parade_planes: None,
        parade_mode: mode,
        vectorscope: None,
        histogram: None,
        clipping: set.clipping,
        mean_saturation: None,
    };

    if s.waveform {
        if s.overlay() {
            if let Some(p) = overlay_planes(set) {
                let tints = if s.colourise { PRIMARY_TINTS } else { CHANNEL_TINTS };
                let layers: Vec<Layer<'_>> = (0..3)
                    .map(|i| Layer { counts: &p[i].counts, tint: tints[i] })
                    .collect();
                let reference = reference_of(&layers);
                let un = set.unfiltered_edges.as_ref();
                // Where the R, G, B planes sit among the flags of their data set.
                let flags: [Option<&Vec<u8>>; 3] = std::array::from_fn(|i| {
                    let e = un?;
                    if set.overlay.is_some() {
                        e.overlay.get(i)
                    } else if set.parade.is_some() {
                        e.parade.get(i)
                    } else {
                        e.parade.get(i + 1)
                    }
                });
                let edges = merge_edges(&[
                    &edges_of(p[0], flags[0]),
                    &edges_of(p[1], flags[1]),
                    &edges_of(p[2], flags[2]),
                ]);
                out.waveform = Some(PreparedTrace {
                    width: p[0].columns,
                    height: p[0].levels,
                    rgba: layers_to_rgba_glow(&layers, p[0].columns, p[0].levels, gain, reference, s.glow),
                    edges,
                });
            }
        } else if let Some(w) = &set.waveform {
            let tint = match s.waveform_mode {
                WaveformMode::Red => CHANNEL_TINTS[0],
                WaveformMode::Green => CHANNEL_TINTS[1],
                WaveformMode::Blue => CHANNEL_TINTS[2],
                _ => LUMA_TINT,
            };
            let layers = [Layer { counts: &w.counts, tint }];
            // Colorize: every bin takes the mean colour of the pixels behind it.
            let colours = set
                .waveform_colors
                .as_ref()
                .filter(|c| s.colourise && s.waveform_mode == WaveformMode::Luma && (c.columns, c.levels) == (w.columns, w.levels));
            let rgba = match colours {
                Some(c) => {
                    colored_trace_to_rgba(&w.counts, c, w.columns, w.levels, gain, reference_of(&layers), s.glow)
                }
                None => layers_to_rgba_glow(&layers, w.columns, w.levels, gain, reference_of(&layers), s.glow),
            };
            out.waveform = Some(PreparedTrace {
                width: w.columns,
                height: w.levels,
                rgba,
                edges: edges_of(w, set.unfiltered_edges.as_ref().map(|e| &e.waveform)),
            });
        }
    }
    if s.parade {
        if let Some(planes) = set.parade_columns() {
            let tints = plane_tints(mode);
            let layers: Vec<Layer<'_>> = planes
                .iter()
                .enumerate()
                .map(|(i, p)| Layer { counts: &p.counts, tint: tints.get(i).copied().unwrap_or(LUMA_TINT) })
                .collect();
            let reference = reference_of(&layers);
            let traces: Vec<PreparedTrace> = planes
                .iter()
                .enumerate()
                .map(|(i, p)| PreparedTrace {
                    width: p.columns,
                    height: p.levels,
                    rgba: layers_to_rgba_glow(&layers[i..=i], p.columns, p.levels, gain, reference, s.glow),
                    // Cb / Cr values leave 0..1 only by chroma limits, not by clipping.
                    edges: if mode == ParadeMode::YCbCr && i > 0 {
                        Vec::new()
                    } else {
                        edges_of(p, set.unfiltered_edges.as_ref().and_then(|e| e.parade.get(i)))
                    },
                })
                .collect();
            if mode == ParadeMode::Rgb {
                let mut it = traces.into_iter();
                if let (Some(a), Some(b), Some(c)) = (it.next(), it.next(), it.next()) {
                    out.parade = Some([a, b, c]);
                }
            } else {
                out.parade_planes = Some(traces);
            }
        }
    }
    if s.vectorscope {
        if let Some(v) = &set.vectorscope {
            out.vectorscope = Some(PreparedTrace {
                width: v.size,
                height: v.size,
                rgba: vectorscope_to_rgba(&v.counts, v.size, gain, s.colourise),
                edges: Vec::new(),
            });
            out.mean_saturation = mean_saturation(&v.counts, v.size);
        }
    }
    if s.histogram {
        if let Some(h) = &set.histogram {
            out.histogram = Some(PreparedHistogram {
                r: h.r.clone(),
                g: h.g.clone(),
                b: h.b.clone(),
                luma: h.luma.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_scopes::{analyze, patterns};

    #[test]
    fn request_asks_only_for_visible_scopes() {
        let none = PrepareSettings {
            waveform: false,
            parade: false,
            vectorscope: false,
            histogram: false,
            ..Default::default()
        };
        let r = none.request();
        assert!(r.waveform.is_none() && r.parade.is_none() && r.vectorscope.is_none() && r.histogram.is_none());
        let vs = PrepareSettings { vectorscope: true, ..none.clone() };
        let r = vs.request();
        assert_eq!(r.vectorscope, Some(VECTORSCOPE_SIZE));
        assert!(r.waveform.is_none() && r.parade.is_none() && r.histogram.is_none());
        let wf = PrepareSettings { waveform: true, ..none.clone() };
        assert_eq!(wf.request().waveform, Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)));
        // Overlay: parade data at waveform width, no luma waveform.
        let ov = PrepareSettings { waveform: true, rgb_overlay: true, ..none.clone() };
        let r = ov.request();
        assert!(r.waveform.is_none());
        assert_eq!(r.parade, Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)));
        // Overlay is irrelevant when the waveform is hidden.
        let hidden = PrepareSettings { rgb_overlay: true, ..none };
        assert!(hidden.request().parade.is_none());
    }

    #[test]
    fn prepare_is_deterministic_and_matches_direct_rendering() {
        let img = patterns::color_bars(256, 64, 0.75);
        let st = PrepareSettings { glow: false, colourise: false, ..Default::default() };
        let set = analyze(&img, &st.request());
        let a = prepare(&set, &st);
        let b = prepare(&set, &st);
        assert_eq!(a, b);
        // Same pixels as the plain render functions (glow off).
        let w = set.waveform.as_ref().unwrap();
        let layers = [Layer { counts: &w.counts, tint: LUMA_TINT }];
        let direct = crate::render::layers_to_rgba(&layers, w.columns, w.levels, st.gain);
        assert_eq!(a.waveform.as_ref().unwrap().rgba, direct);
        let v = set.vectorscope.as_ref().unwrap();
        assert_eq!(
            a.vectorscope.as_ref().unwrap().rgba,
            vectorscope_to_rgba(&v.counts, v.size, st.gain, st.colourise)
        );
        assert_eq!(a.histogram.as_ref().unwrap().r, set.histogram.as_ref().unwrap().r);
        // Glow only ever adds light.
        let sg = PrepareSettings { glow: true, ..st.clone() };
        let g = prepare(&set, &sg);
        let (pa, pg) = (&a.waveform.as_ref().unwrap().rgba, &g.waveform.as_ref().unwrap().rgba);
        assert!(pa.iter().zip(pg).all(|(x, y)| y >= x));
        assert_ne!(pa, pg);
    }

    #[test]
    fn prepare_skips_hidden_scopes_and_handles_empty_sets() {
        let st = PrepareSettings { waveform: false, parade: false, histogram: false, ..Default::default() };
        let img = patterns::flat(16, 16, [0.5; 3]);
        let p = prepare(&analyze(&img, &st.request()), &st);
        assert!(p.waveform.is_none() && p.parade.is_none() && p.histogram.is_none());
        assert!(p.vectorscope.is_some());
        let empty = rawgraph_scopes::DisplayImage { width: 0, height: 0, rgb: vec![] };
        let all = PrepareSettings::default();
        let _ = prepare(&analyze(&empty, &all.request()), &all);
    }

    #[test]
    fn rgb_overlay_comes_from_parade_data() {
        let st = PrepareSettings { rgb_overlay: true, ..Default::default() };
        let img = patterns::color_bars(64, 16, 1.0);
        let p = prepare(&analyze(&img, &st.request()), &st);
        assert_eq!(p.waveform.as_ref().unwrap().width, WAVEFORM_COLUMNS);
        assert_eq!(p.parade.as_ref().unwrap()[0].width, WAVEFORM_COLUMNS);
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;
    use crate::render::{EDGE_HIGH, EDGE_LOW};
    use rawgraph_scopes::{analyze, patterns};

    fn only(waveform: bool, parade: bool) -> PrepareSettings {
        PrepareSettings {
            waveform,
            parade,
            vectorscope: false,
            histogram: false,
            clipping: false,
            ..Default::default()
        }
    }

    #[test]
    fn default_request_is_the_old_request_plus_clip_stats() {
        let r = PrepareSettings::default().request();
        assert_eq!(r.waveform, Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)));
        assert_eq!(r.parade, Some((PARADE_COLUMNS, PARADE_LEVELS)));
        assert_eq!(r.vectorscope, Some(VECTORSCOPE_SIZE));
        assert_eq!(r.histogram, Some(HISTOGRAM_BINS));
        assert_eq!((r.step, r.parade_mode, r.waveform_channel), (1, ParadeMode::Rgb, WaveformChannel::Luma));
        assert!(r.overlay.is_none() && !r.lowpass && r.clip_stats);
    }

    #[test]
    fn request_asks_exactly_for_each_parade_mode() {
        for mode in [ParadeMode::Rgb, ParadeMode::Yrgb, ParadeMode::YCbCr] {
            let r = PrepareSettings { parade_mode: mode, ..only(false, true) }.request();
            assert_eq!(r.parade, Some((PARADE_COLUMNS, PARADE_LEVELS)));
            assert_eq!(r.parade_mode, mode);
            assert!(r.waveform.is_none() && r.overlay.is_none());
            // Hidden parade: no parade data whatever the mode.
            let hidden = PrepareSettings { parade_mode: mode, ..only(false, false) }.request();
            assert!(hidden.parade.is_none());
        }
    }

    #[test]
    fn request_for_waveform_modes() {
        let wf = |m| PrepareSettings { waveform_mode: m, ..only(true, false) }.request();
        for (m, ch) in [
            (WaveformMode::Luma, WaveformChannel::Luma),
            (WaveformMode::Red, WaveformChannel::Red),
            (WaveformMode::Green, WaveformChannel::Green),
            (WaveformMode::Blue, WaveformChannel::Blue),
        ] {
            let r = wf(m);
            assert_eq!(r.waveform, Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)), "{m:?}");
            assert_eq!(r.waveform_channel, ch);
            assert!(r.parade.is_none());
        }
        // RGB overlay: no luma waveform, parade data at waveform width; the mode alone
        // switches it on, like the old flag.
        let r = wf(WaveformMode::Rgb);
        assert!(r.waveform.is_none());
        assert_eq!(r.parade, Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)));
        assert_eq!(r, PrepareSettings { rgb_overlay: true, ..only(true, false) }.request());
        // Overlay next to a YRGB parade: one set at waveform width serves both.
        let r = PrepareSettings { waveform_mode: WaveformMode::Rgb, parade_mode: ParadeMode::Yrgb, ..only(true, true) }
            .request();
        assert_eq!((r.parade, r.parade_mode, r.overlay), (Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)), ParadeMode::Yrgb, None));
        // Overlay next to a YCbCr parade needs its own RGB set.
        let r = PrepareSettings { waveform_mode: WaveformMode::Rgb, parade_mode: ParadeMode::YCbCr, ..only(true, true) }
            .request();
        assert_eq!(r.parade, Some((PARADE_COLUMNS, PARADE_LEVELS)));
        assert_eq!(r.overlay, Some((WAVEFORM_COLUMNS, WAVEFORM_LEVELS)));
        // Overlay with a hidden parade ignores the parade mode.
        let r = PrepareSettings { waveform_mode: WaveformMode::Rgb, parade_mode: ParadeMode::YCbCr, ..only(true, false) }
            .request();
        assert_eq!((r.parade_mode, r.overlay), (ParadeMode::Rgb, None));
    }

    #[test]
    fn request_lowpass_and_clipping_follow_settings() {
        let s = PrepareSettings { lowpass: true, ..only(true, true) };
        assert!(s.request().lowpass && !s.request().clip_stats);
        // Nothing to filter: not requested.
        assert!(!PrepareSettings { lowpass: true, ..only(false, false) }.request().lowpass);
        assert!(PrepareSettings { clipping: true, ..only(false, false) }.request().clip_stats);
    }

    #[test]
    fn prepared_sizes_per_mode() {
        let img = patterns::color_bars(256, 32, 0.75);
        let size = |s: &PrepareSettings| prepare(&analyze(&img, &s.request()), s);
        for (mode, n) in [(ParadeMode::Rgb, 3), (ParadeMode::Yrgb, 4), (ParadeMode::YCbCr, 3)] {
            let p = size(&PrepareSettings { parade_mode: mode, ..only(false, true) });
            let t = p.parade_traces().unwrap();
            assert_eq!(t.len(), n, "{mode:?}");
            assert_eq!(p.parade_mode, mode);
            assert_eq!(p.parade.is_some(), mode == ParadeMode::Rgb);
            for tr in t {
                assert_eq!((tr.width, tr.height), (PARADE_COLUMNS, PARADE_LEVELS));
                assert_eq!(tr.rgba.len(), PARADE_COLUMNS * PARADE_LEVELS * 4);
            }
        }
        for m in [WaveformMode::Luma, WaveformMode::Rgb, WaveformMode::Green] {
            let p = size(&PrepareSettings { waveform_mode: m, ..only(true, false) });
            let w = p.waveform.as_ref().unwrap();
            assert_eq!((w.width, w.height), (WAVEFORM_COLUMNS, WAVEFORM_LEVELS), "{m:?}");
            assert_eq!(w.rgba.len(), WAVEFORM_COLUMNS * WAVEFORM_LEVELS * 4);
        }
        // Overlay next to a YCbCr parade works from the extra set.
        let both = PrepareSettings { waveform_mode: WaveformMode::Rgb, parade_mode: ParadeMode::YCbCr, ..only(true, true) };
        let p = size(&both);
        assert_eq!(p.waveform.as_ref().unwrap().width, WAVEFORM_COLUMNS);
        assert_eq!(p.parade_traces().unwrap()[0].width, PARADE_COLUMNS);
        // ... and next to a YRGB parade from its R, G, B planes.
        let both = PrepareSettings { waveform_mode: WaveformMode::Rgb, parade_mode: ParadeMode::Yrgb, ..only(true, true) };
        let p = size(&both);
        assert_eq!(p.waveform.as_ref().unwrap().width, WAVEFORM_COLUMNS);
        assert_eq!(p.parade_traces().unwrap().len(), 4);
    }

    #[test]
    fn single_channel_trace_uses_the_channel_tint() {
        let img = patterns::flat(64, 8, [0.5, 0.5, 0.5]);
        let st = PrepareSettings { waveform_mode: WaveformMode::Red, glow: false, ..only(true, false) };
        let p = prepare(&analyze(&img, &st.request()), &st);
        let px = &p.waveform.unwrap().rgba;
        let lit: Vec<&[u8]> = px.chunks(4).filter(|c| c[3] > 0).collect();
        assert!(!lit.is_empty());
        assert!(lit.iter().all(|c| c[0] > c[1] && c[0] > c[2]), "red trace is red");
    }

    #[test]
    fn edges_mark_clipped_columns_only() {
        // Left half pure white, right half black; middle grey never touches the edges.
        let mut rgb = Vec::new();
        for _ in 0..4 {
            for x in 0..512 {
                rgb.extend(match x / 128 {
                    0 => [1.0; 3],
                    1 => [0.5; 3],
                    2 => [0.2; 3],
                    _ => [0.0; 3],
                });
            }
        }
        let img = rawgraph_scopes::DisplayImage { width: 512, height: 4, rgb };
        let st = PrepareSettings { parade_mode: ParadeMode::Yrgb, ..only(true, true) };
        let p = prepare(&analyze(&img, &st.request()), &st);
        let w = p.waveform.as_ref().unwrap();
        assert_eq!(w.edges.len(), WAVEFORM_COLUMNS);
        let cols = WAVEFORM_COLUMNS;
        assert!(w.edges[..cols / 4].iter().all(|&f| f == EDGE_HIGH));
        assert!(w.edges[cols / 4..3 * cols / 4].iter().all(|&f| f == 0));
        assert!(w.edges[3 * cols / 4..].iter().all(|&f| f == EDGE_LOW));
        // YCbCr: only the luma column carries markers.
        let st = PrepareSettings { parade_mode: ParadeMode::YCbCr, ..only(false, true) };
        let p = prepare(&analyze(&img, &st.request()), &st);
        let t = p.parade_traces().unwrap();
        assert!(!t[0].edges.is_empty() && t[1].edges.is_empty() && t[2].edges.is_empty());
        // Flat mid grey has no markers at all.
        let g = patterns::flat(32, 4, [0.5; 3]);
        let st = only(true, true);
        let p = prepare(&analyze(&g, &st.request()), &st);
        assert!(p.waveform.unwrap().edges.iter().all(|&f| f == 0));
    }

    #[test]
    fn lowpass_keeps_the_clip_markers() {
        let mut img = patterns::flat(512, 4, [0.5; 3]);
        for y in 0..4usize {
            let i = (y * 512 + 200) * 3;
            img.rgb[i..i + 3].copy_from_slice(&[1.0; 3]);
        }
        for (lowpass, parade_mode) in [(false, ParadeMode::Rgb), (true, ParadeMode::Rgb), (true, ParadeMode::Yrgb)] {
            let st = PrepareSettings { lowpass, parade_mode, ..only(true, true) };
            let p = prepare(&analyze(&img, &st.request()), &st);
            let w = p.waveform.as_ref().unwrap();
            assert_eq!(w.edges[200], EDGE_HIGH, "lowpass {lowpass} {parade_mode:?}");
            assert_eq!(w.edges.iter().filter(|&&f| f != 0).count(), 1);
            let t = p.parade_traces().unwrap();
            assert!(t.iter().all(|t| t.edges[200 / 2] == EDGE_HIGH), "parade columns are 256 wide");
        }
        // Overlay from the unfiltered flags too.
        let st = PrepareSettings { lowpass: true, waveform_mode: WaveformMode::Rgb, ..only(true, false) };
        let p = prepare(&analyze(&img, &st.request()), &st);
        assert_eq!(p.waveform.unwrap().edges[200], EDGE_HIGH);
    }

    #[test]
    fn clipping_and_saturation_arrive_in_the_prepared_data() {
        let img = patterns::color_bars(256, 16, 0.75);
        let st = PrepareSettings::default();
        let p = prepare(&analyze(&img, &st.request()), &st);
        let c = p.clipping.unwrap();
        // 75 % bars: black bar crushed (1/8 of the pixels), nothing clipped high.
        assert_eq!(c.high, [0; 4]);
        assert!((c.low_share(0) - 0.5).abs() < 1e-6, "R is 0 in 4 of 8 bars");
        let sat = p.mean_saturation.unwrap();
        assert!((sat - 56.25).abs() < 2.0, "mean saturation {sat}");
        // Hidden vectorscope: no saturation; clipping off: no stats.
        let off = PrepareSettings { vectorscope: false, clipping: false, ..Default::default() };
        let p = prepare(&analyze(&img, &off.request()), &off);
        assert!(p.mean_saturation.is_none() && p.clipping.is_none());
        let _ = (EDGE_HIGH, EDGE_LOW);
    }
}
