//! `ScopePanel`: all four views plus a compact toolbar and an adaptive layout.

use crate::layout::{arrange, content_rect, PanelLayout, ScopeKind};
use crate::prepare::{prepare, PrepareSettings, PreparedScopes};
use crate::readout::{clip_spans, saturation_spans, ReadoutSpan, SpanTone};
use crate::style::{parade_mode_label, HistogramMode, ScaleKind, ScopeStyle, WaveformMode};
use crate::views::{
    HistogramView, ParadeView, VectorscopeView, WaveformView, CLIP_HIGH, CLIP_LOW,
};
use egui::text::{LayoutJob, TextFormat};
use egui::{pos2, Align2, Color32, FontId, Rect, Ui, UiBuilder, Vec2};
use rawgraph_scopes::{analyze, ClipStats, DisplayImage, ParadeMode};

/// Height of the one-line clipping / saturation readout under the toolbar.
const READOUT_ROW_H: f32 = 14.0;

/// Width from which the toolbar shows all options inline instead of in a menu.
const WIDE_TOOLBAR: f32 = 900.0;

/// All four scopes with a toolbar. Embed with [`ScopePanel::show`] and feed it with
/// [`ScopePanel::set_image`], or (off the UI thread) with
/// [`ScopePanel::prepare_settings`] -> [`prepare`] -> [`ScopePanel::set_prepared`].
pub struct ScopePanel {
    pub layout: PanelLayout,
    /// Scope shown when the arrangement has fewer than four cells (Auto fallback).
    pub focus: ScopeKind,
    pub style: ScopeStyle,
    /// Pixel sampling step handed to `analyze` (1 = every pixel).
    pub sample_step: usize,
    /// Simulated pointer, relative to the panel's top-left corner (tests, screenshots).
    pub simulated_pointer: Option<Vec2>,
    pub waveform: WaveformView,
    pub parade: ParadeView,
    pub vectorscope: VectorscopeView,
    pub histogram: HistogramView,
    /// Scopes drawn in the last frame (index = `ScopeKind`).
    visible: [bool; 4],
    /// Settings the displayed data was prepared with.
    shown: Option<PrepareSettings>,
    /// Clipping statistics of the shown data.
    clipping: Option<ClipStats>,
    /// Mean saturation (percent) of the shown data.
    mean_saturation: Option<f32>,
}

impl Default for ScopePanel {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopePanel {
    pub fn new() -> Self {
        Self {
            layout: PanelLayout::Auto,
            focus: ScopeKind::Waveform,
            style: ScopeStyle::default(),
            sample_step: 1,
            simulated_pointer: None,
            waveform: WaveformView::new(),
            parade: ParadeView::new(),
            vectorscope: VectorscopeView::new(),
            histogram: HistogramView::new(),
            visible: [true; 4],
            shown: None,
            clipping: None,
            mean_saturation: None,
        }
    }

    /// Current state as the settings the pixels have to be prepared with.
    pub fn prepare_settings(&self) -> PrepareSettings {
        PrepareSettings {
            waveform: self.visible[ScopeKind::Waveform.index()],
            parade: self.visible[ScopeKind::Parade.index()],
            vectorscope: self.visible[ScopeKind::Vectorscope.index()],
            histogram: self.visible[ScopeKind::Histogram.index()],
            rgb_overlay: self.style.waveform_rgb,
            gain: self.style.gain,
            colourise: self.style.colourise,
            glow: self.style.glow,
            step: self.sample_step.max(1),
            waveform_mode: self.style.waveform_mode,
            parade_mode: self.style.parade_mode,
            lowpass: self.style.lowpass,
            clipping: self.style.clip_readout || self.style.clip_markers,
        }
    }

    /// True when the panel's settings differ from those of the shown data: ask the worker
    /// (or call [`ScopePanel::set_image`]) again.
    pub fn needs_refresh(&self) -> bool {
        self.shown.as_ref() != Some(&self.prepare_settings())
    }

    /// Same as [`ScopePanel::needs_refresh`] (kept for the first API).
    pub fn needs_image(&self) -> bool {
        self.needs_refresh()
    }

    /// Uploads prepared pixels to the GPU textures. No analysis, no pixel conversion.
    pub fn set_prepared(&mut self, ctx: &egui::Context, prepared: PreparedScopes) {
        match &prepared.waveform {
            Some(t) => self.waveform.set_pixels(ctx, t),
            None => self.waveform.clear(),
        }
        match prepared.parade_traces() {
            Some(t) => self.parade.set_planes(ctx, prepared.parade_mode, t),
            None => self.parade.clear(),
        }
        self.clipping = prepared.clipping;
        self.mean_saturation = prepared.mean_saturation;
        match &prepared.vectorscope {
            Some(t) => self.vectorscope.set_pixels(ctx, t),
            None => self.vectorscope.clear(),
        }
        match &prepared.histogram {
            Some(h) => self.histogram.set_prepared(h),
            None => self.histogram.clear(),
        }
        self.shown = Some(prepared.settings);
    }

    /// Analyses `img` and prepares + uploads every visible scope (analyze -> prepare ->
    /// set_prepared). Fine for tests and simple use; apps should run the first two
    /// steps on a worker thread.
    pub fn set_image(&mut self, ctx: &egui::Context, img: &DisplayImage) {
        let settings = self.prepare_settings();
        let set = analyze(img, &settings.request());
        let prepared = prepare(&set, &settings);
        self.set_prepared(ctx, prepared);
    }

    /// Forgets all data; the empty graticules are shown.
    pub fn clear(&mut self) {
        self.waveform.clear();
        self.parade.clear();
        self.vectorscope.clear();
        self.histogram.clear();
        self.clipping = None;
        self.mean_saturation = None;
        self.shown = None;
    }

    /// Title of a scope cell: the parade and waveform names follow their modes.
    fn title(&self, kind: ScopeKind) -> String {
        match kind {
            ScopeKind::Parade => {
                let mode = if self.parade.has_data() { self.parade.mode() } else { self.style.parade_mode };
                format!("{} Parade", parade_mode_label(mode))
            }
            ScopeKind::Waveform => match self.effective_waveform_mode() {
                WaveformMode::Luma => "Waveform".to_owned(),
                m => format!("Waveform ({})", m.label()),
            },
            k => k.title().to_owned(),
        }
    }

    /// The waveform mode in force: the old `waveform_rgb` flag wins over the mode field.
    fn effective_waveform_mode(&self) -> WaveformMode {
        if self.style.waveform_rgb {
            WaveformMode::Rgb
        } else {
            self.style.waveform_mode
        }
    }

    /// The readout row: clipping shares (when something is clipped) and the mean
    /// saturation, in the widest variant that fits `width`. Empty when there is nothing
    /// to say or the row is switched off.
    fn readout_spans(&self, ui: &Ui, width: f32) -> Vec<ReadoutSpan> {
        if !self.style.clip_readout {
            return Vec::new();
        }
        let sat = self.mean_saturation.filter(|_| self.visible[ScopeKind::Vectorscope.index()]);
        let clip = |compact| self.clipping.map(|c| clip_spans(&c, compact)).unwrap_or_default();
        let build = |compact: bool, with_sat: bool| -> Vec<ReadoutSpan> {
            let mut v = clip(compact);
            if let (true, Some(sat)) = (with_sat, sat) {
                v.extend(saturation_spans(sat, !v.is_empty()));
            }
            v
        };
        let font = FontId::proportional(self.style.label_size);
        for (compact, with_sat) in [(false, true), (false, false), (true, true), (true, false)] {
            let spans = build(compact, with_sat);
            if spans.is_empty() || ui.painter().layout_job(spans_job(&spans, &font, &self.style)).size().x <= width {
                return spans;
            }
        }
        build(true, false)
    }

    fn layout_combo(&mut self, ui: &mut Ui) {
        let text = match self.layout {
            PanelLayout::Auto => "Auto",
            PanelLayout::Grid2x2 => "2 x 2",
            PanelLayout::Row4 => "Row",
            PanelLayout::Column4 => "Column",
            PanelLayout::Single(k) => k.short(),
        };
        egui::ComboBox::from_id_salt("scope-layout")
            .selected_text(text)
            .width(70.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.layout, PanelLayout::Auto, "Auto");
                ui.selectable_value(&mut self.layout, PanelLayout::Grid2x2, "2 x 2");
                ui.selectable_value(&mut self.layout, PanelLayout::Row4, "Row of four");
                ui.selectable_value(&mut self.layout, PanelLayout::Column4, "Column of four");
                for k in ScopeKind::ALL {
                    ui.selectable_value(&mut self.layout, PanelLayout::Single(k), k.title());
                }
            });
    }

    fn focus_combo(&mut self, ui: &mut Ui) {
        egui::ComboBox::from_id_salt("scope-focus")
            .selected_text(self.focus.short())
            .width(60.0)
            .show_ui(ui, |ui| {
                for k in ScopeKind::ALL {
                    ui.selectable_value(&mut self.focus, k, k.title());
                }
            });
    }

    /// Waveform mode picker; keeps the old `waveform_rgb` flag in step.
    fn waveform_mode_combo(&mut self, ui: &mut Ui) {
        let mut mode = self.effective_waveform_mode();
        egui::ComboBox::from_id_salt("scope-wave-mode")
            .selected_text(format!("Wave {}", mode.label()))
            .width(72.0)
            .show_ui(ui, |ui| {
                for m in WaveformMode::ALL {
                    ui.selectable_value(&mut mode, m, m.label());
                }
            });
        self.style.waveform_mode = mode;
        self.style.waveform_rgb = mode == WaveformMode::Rgb;
    }

    /// The mode pickers and clipping switches (waveform, parade, histogram, low-pass).
    fn modes(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label("Waveform");
            self.waveform_mode_combo(ui);
        });
        ui.horizontal(|ui| {
            ui.label("Parade");
            for m in [ParadeMode::Rgb, ParadeMode::Yrgb, ParadeMode::YCbCr] {
                ui.selectable_value(&mut self.style.parade_mode, m, parade_mode_label(m));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Histogram");
            for m in HistogramMode::ALL {
                ui.selectable_value(&mut self.style.histogram_mode, m, m.label());
            }
        });
        ui.checkbox(&mut self.style.lowpass, "Low-pass (wave / parade)");
        ui.checkbox(&mut self.style.clip_readout, "Clip + saturation readout");
        ui.checkbox(&mut self.style.clip_markers, "Clip edge markers");
    }

    fn options(&mut self, ui: &mut Ui) {
        ui.label("Gain");
        ui.add(egui::Slider::new(&mut self.style.gain, 0.25..=4.0).logarithmic(true).fixed_decimals(2));
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.style.scale, ScaleKind::TenBit, "10-bit");
            ui.selectable_value(&mut self.style.scale, ScaleKind::Percent, "%");
        });
        self.modes(ui);
        ui.separator();
        ui.checkbox(&mut self.style.colourise, "Colorize");
        ui.checkbox(&mut self.style.glow, "Glow");
        ui.checkbox(&mut self.style.zoom, "Zoom 2x");
        ui.checkbox(&mut self.style.histogram_log, "Log hist");
        ui.checkbox(&mut self.style.targets_100, "100 % targets");
        ui.checkbox(&mut self.style.skin_line, "Skin line");
        ui.checkbox(&mut self.style.graticule, "Graticule");
        ui.checkbox(&mut self.style.readout, "Readout");
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        let wide = ui.available_width() >= WIDE_TOOLBAR;
        let shown = self.visible.iter().filter(|v| **v).count();
        ui.horizontal(|ui| {
            self.layout_combo(ui);
            if self.layout == PanelLayout::Auto && shown < 4 {
                self.focus_combo(ui);
            }
            if wide {
                // Inline: slider and checkboxes in one row.
                ui.label("Gain");
                ui.add(egui::Slider::new(&mut self.style.gain, 0.25..=4.0).logarithmic(true).fixed_decimals(2));
                ui.selectable_value(&mut self.style.scale, ScaleKind::TenBit, "10-bit");
                ui.selectable_value(&mut self.style.scale, ScaleKind::Percent, "%");
                self.waveform_mode_combo(ui);
                ui.checkbox(&mut self.style.colourise, "Colorize");
                ui.checkbox(&mut self.style.glow, "Glow");
                ui.checkbox(&mut self.style.zoom, "Zoom 2x");
                ui.checkbox(&mut self.style.histogram_log, "Log hist");
                ui.checkbox(&mut self.style.targets_100, "100 %");
                ui.checkbox(&mut self.style.graticule, "Grid");
                ui.menu_button("Modes", |ui| self.modes(ui));
            } else {
                ui.menu_button("Options", |ui| self.options(ui));
            }
        });
    }

    /// Toolbar followed by the scopes, filling exactly the available rectangle
    /// (never draws outside it).
    pub fn show(&mut self, ui: &mut Ui) {
        let full = ui.available_rect_before_wrap();
        ui.scope_builder(UiBuilder::new().max_rect(full), |ui| {
            let clip = full.intersect(ui.clip_rect());
            ui.set_clip_rect(clip);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 2.0);
            self.toolbar(ui);
            let mut top = (ui.min_rect().bottom() + 4.0).min(full.bottom());
            let spans = self.readout_spans(ui, full.width());
            if !spans.is_empty() && full.bottom() - top > READOUT_ROW_H + 40.0 {
                let font = FontId::proportional(self.style.label_size);
                let galley = ui.painter().layout_job(spans_job(&spans, &font, &self.style));
                ui.painter().with_clip_rect(clip).galley(pos2(full.left() + 1.0, top), galley, self.style.label_color);
                top += READOUT_ROW_H;
            }
            let area = Rect::from_min_max(pos2(full.left(), top), full.max);
            let mut style = self.style.clone();
            style.pointer_override = self.simulated_pointer.map(|v| full.min + v);
            let cells = arrange(self.layout, area, self.focus);
            self.visible = [false; 4];
            for (kind, _) in &cells {
                self.visible[kind.index()] = true;
            }
            for (kind, cell) in cells {
                let title = ui.painter().with_clip_rect(clip.intersect(cell));
                title.text(cell.left_top(), Align2::LEFT_TOP, self.title(kind), egui::FontId::proportional(11.0), style.label_color);
                let content = content_rect(cell);
                ui.scope_builder(UiBuilder::new().max_rect(content), |ui| {
                    ui.set_clip_rect(clip.intersect(content));
                    match kind {
                        ScopeKind::Waveform => self.waveform.show(ui, &style),
                        ScopeKind::Parade => self.parade.show(ui, &style),
                        ScopeKind::Vectorscope => self.vectorscope.show(ui, &style),
                        ScopeKind::Histogram => self.histogram.show(ui, &style),
                    };
                });
            }
        });
        ui.advance_cursor_after_rect(full);
    }
}

/// Colour of a readout span.
fn tone_color(tone: SpanTone, style: &ScopeStyle) -> Color32 {
    match tone {
        SpanTone::Label => style.label_color,
        SpanTone::Zero => Color32::from_rgb(98, 98, 104),
        SpanTone::High => CLIP_HIGH,
        SpanTone::Low => CLIP_LOW,
        SpanTone::Red => Color32::from_rgb(255, 112, 104),
        SpanTone::Green => Color32::from_rgb(110, 232, 124),
        SpanTone::Blue => Color32::from_rgb(124, 154, 255),
        SpanTone::Luma => Color32::from_rgb(235, 235, 235),
        SpanTone::Saturation => Color32::from_rgb(235, 200, 150),
    }
}

/// One-line layout job with a colour per span.
fn spans_job(spans: &[ReadoutSpan], font: &FontId, style: &ScopeStyle) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_rows = 1;
    job.wrap.max_width = f32::INFINITY;
    for s in spans {
        job.append(&s.text, 0.0, TextFormat::simple(font.clone(), tone_color(s.tone, style)));
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use rawgraph_scopes::patterns;

    #[test]
    fn needs_refresh_follows_data_modes_only() {
        let ctx = egui::Context::default();
        let img = patterns::color_bars(128, 16, 0.75);
        let mut panel = ScopePanel::new();
        assert!(panel.needs_refresh(), "nothing shown yet");
        panel.set_image(&ctx, &img);
        assert!(!panel.needs_refresh());
        // The histogram mode and marker switches only change drawing.
        panel.style.histogram_mode = HistogramMode::Stacked;
        panel.style.clip_markers = true;
        assert!(!panel.needs_refresh());
        let changes: [fn(&mut ScopePanel); 5] = [
            |p| p.style.parade_mode = ParadeMode::Yrgb,
            |p| p.style.parade_mode = ParadeMode::YCbCr,
            |p| p.style.waveform_mode = WaveformMode::Red,
            |p| p.style.waveform_mode = WaveformMode::Rgb,
            |p| p.style.lowpass = true,
        ];
        for change in changes {
            let mut p = ScopePanel::new();
            p.set_image(&ctx, &img);
            change(&mut p);
            assert!(p.needs_refresh());
            p.set_image(&ctx, &img);
            assert!(!p.needs_refresh(), "set_image brings it back in step");
        }
        // The old flag still switches the overlay on.
        let mut p = ScopePanel::new();
        p.style.waveform_rgb = true;
        assert!(p.prepare_settings().request().waveform.is_none());
        assert_eq!(p.effective_waveform_mode(), WaveformMode::Rgb);
    }

    #[test]
    fn modes_flow_into_the_settings_and_request() {
        let mut p = ScopePanel::new();
        p.style.parade_mode = ParadeMode::Yrgb;
        p.style.waveform_mode = WaveformMode::Blue;
        p.style.lowpass = true;
        let s = p.prepare_settings();
        assert_eq!((s.parade_mode, s.waveform_mode, s.lowpass), (ParadeMode::Yrgb, WaveformMode::Blue, true));
        let r = s.request();
        assert_eq!(r.parade_mode, ParadeMode::Yrgb);
        assert_eq!(r.waveform_channel, rawgraph_scopes::WaveformChannel::Blue);
        assert!(r.lowpass && r.clip_stats);
        p.style.clip_readout = false;
        p.style.clip_markers = false;
        assert!(!p.prepare_settings().request().clip_stats);
    }

    #[test]
    fn titles_follow_the_modes() {
        let mut p = ScopePanel::new();
        assert_eq!(p.title(ScopeKind::Parade), "RGB Parade");
        assert_eq!(p.title(ScopeKind::Waveform), "Waveform");
        p.style.parade_mode = ParadeMode::YCbCr;
        p.style.waveform_mode = WaveformMode::Green;
        assert_eq!(p.title(ScopeKind::Parade), "YCbCr Parade");
        assert_eq!(p.title(ScopeKind::Waveform), "Waveform (G)");
        assert_eq!(p.title(ScopeKind::Histogram), "Histogram");
    }
}
