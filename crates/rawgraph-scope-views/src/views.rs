//! The four stateful scope views.

use crate::geometry::{
    centred_square, histogram_x, level_to_y, parade_rects_n, scope_uv_to_pos, skin_line_dir,
    stacked_rows, thin_labels, vectorscope_target_pos, vectorscope_uv,
};
use crate::prepare::{PreparedHistogram, PreparedTrace};
use crate::readout::{
    format_level, histogram_bin_at, histogram_bin_level, level_at_y, vectorscope_inner,
    vectorscope_readout, waveform_ticks_for_height,
};
use crate::render::{
    edge_flags, flag_runs, histogram_fills, histogram_height, layers_to_rgba_glow, merge_edges,
    reference_of, vectorscope_to_rgba, Layer, EDGE_HIGH, EDGE_LOW,
};
use crate::style::{plane_tints, HistogramMode, ScopeStyle, CHANNEL_TINTS, LUMA_TINT, PRIMARY_TINTS};
use egui::{
    pos2, vec2, Align2, Color32, ColorImage, Context, FontId, Mesh, Painter, Pos2, Rect, Response,
    Sense, Shape, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui,
};
use rawgraph_scopes::{vectorscope_targets, Histogram, ParadeMode, Vectorscope, Waveform};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Width reserved at the left of waveform / parade for scale labels and the cursor value.
const GUTTER: f32 = 34.0;
/// Vertical inset so that labels on the 0 and 100 % lines are not clipped.
const V_INSET: f32 = 7.0;
/// Height reserved below the histogram for its x labels.
const HIST_FOOTER: f32 = 15.0;

static NEXT_TEXTURE: AtomicUsize = AtomicUsize::new(0);

fn texture_name(kind: &str) -> String {
    format!("rawgraph-scope-views/{kind}/{}", NEXT_TEXTURE.fetch_add(1, Ordering::Relaxed))
}

fn alloc(ui: &mut Ui) -> (Rect, Response) {
    let avail = ui.available_size();
    let size = vec2(avail.x.clamp(1.0, 4000.0), avail.y.clamp(1.0, 4000.0));
    ui.allocate_exact_size(size, Sense::hover())
}

fn upload(
    ctx: &Context,
    slot: &mut Option<TextureHandle>,
    name: &str,
    rgba: &[u8],
    w: usize,
    h: usize,
) {
    if w == 0 || h == 0 || rgba.len() != w * h * 4 {
        *slot = None;
        return;
    }
    let image = ColorImage::from_rgba_premultiplied([w, h], rgba);
    match slot {
        Some(t) => t.set(image, TextureOptions::LINEAR),
        None => *slot = Some(ctx.load_texture(name, image, TextureOptions::LINEAR)),
    }
}

fn upload_trace(ctx: &Context, slot: &mut Option<TextureHandle>, name: &str, t: &PreparedTrace) {
    upload(ctx, slot, name, &t.rgba, t.width, t.height);
}

fn label_font(style: &ScopeStyle) -> FontId {
    FontId::proportional(style.label_size)
}

fn pointer(resp: &Response, style: &ScopeStyle) -> Option<Pos2> {
    if !style.readout {
        return None;
    }
    style.pointer_override.or_else(|| resp.hover_pos())
}

/// Draws `text` in a dark badge; `anchor` says which corner of the badge sits at `pos`.
fn badge(p: &Painter, pos: Pos2, anchor: Align2, text: String, style: &ScopeStyle, color: Color32) {
    let galley = p.layout_no_wrap(text, label_font(style), color);
    let size = galley.size();
    let rect = anchor.anchor_size(pos, size);
    p.rect_filled(rect.expand(2.0), 2.0, Color32::from_black_alpha(215));
    p.galley(rect.min, galley, color);
}

const CURSOR: Color32 = Color32::from_rgb(255, 214, 90);

/// Colour of the markers at the top edge of columns with clipped highlights.
pub(crate) const CLIP_HIGH: Color32 = Color32::from_rgb(255, 176, 32);
/// Colour of the markers at the bottom edge of columns with crushed blacks.
pub(crate) const CLIP_LOW: Color32 = Color32::from_rgb(190, 96, 255);
/// Thickness of the clip markers in points.
const MARKER_H: f32 = 2.0;
/// Extra height below the parade for the column names of YRGB / YCbCr.
const PARADE_LABEL_H: f32 = 11.0;

/// Draws the clip markers of one waveform / parade panel: a thin line at the top edge
/// over the columns with `EDGE_HIGH` and at the bottom edge over those with `EDGE_LOW`.
fn draw_edge_markers(p: &Painter, panel: Rect, flags: &[u8]) {
    if flags.is_empty() || panel.width() < 2.0 || panel.height() < 2.0 * MARKER_H {
        return;
    }
    let cw = panel.width() / flags.len() as f32;
    for (flag, color, y) in [
        (EDGE_HIGH, CLIP_HIGH, panel.top()),
        (EDGE_LOW, CLIP_LOW, panel.bottom() - MARKER_H),
    ] {
        for (a, b) in flag_runs(flags, flag) {
            let x0 = panel.left() + a as f32 * cw;
            let x1 = (panel.left() + b as f32 * cw).max(x0 + 1.5).min(panel.right());
            p.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x1, y + MARKER_H)), 0.0, color);
        }
    }
}

/// Plot rectangle of the waveform / parade inside `outer`.
fn level_plot_rect(outer: Rect) -> Rect {
    Rect::from_min_max(
        pos2(outer.left() + GUTTER, outer.top() + V_INSET),
        pos2(outer.right() - 2.0, outer.bottom() - V_INSET),
    )
}

fn draw_level_graticule(p: &Painter, plot: Rect, outer_left: f32, style: &ScopeStyle) {
    let ticks = waveform_ticks_for_height(style.scale, plot.height());
    let ys: Vec<f32> = ticks.iter().map(|t| level_to_y(plot, t.level)).collect();
    for (t, &y) in ticks.iter().zip(&ys) {
        let edge = t.level <= 0.0 || t.level >= 1.0;
        let color = if edge { style.graticule_strong } else { style.graticule_color };
        p.line_segment([pos2(plot.left(), y), pos2(plot.right(), y)], Stroke::new(1.0, color));
    }
    for i in thin_labels(&ys, style.label_size + 3.0) {
        p.text(
            pos2(outer_left + GUTTER - 5.0, ys[i]),
            Align2::RIGHT_CENTER,
            &ticks[i].label,
            label_font(style),
            style.label_color,
        );
    }
}

/// Horizontal cursor line across `plot` with the level value in the left gutter
/// (so the trace is never covered).
fn draw_level_cursor(p: &Painter, plot: Rect, outer_left: f32, y: f32, style: &ScopeStyle) {
    let y = y.clamp(plot.top(), plot.bottom());
    p.line_segment(
        [pos2(plot.left(), y), pos2(plot.right(), y)],
        Stroke::new(1.0, CURSOR.gamma_multiply(0.8)),
    );
    let text = format_level(level_at_y(plot, y), style.scale);
    badge(p, pos2(outer_left + GUTTER - 3.0, y), Align2::RIGHT_CENTER, text, style, CURSOR);
}

#[derive(Clone, Debug, Default)]
struct Grid {
    w: usize,
    h: usize,
    counts: Vec<u32>,
}

impl Grid {
    fn from_waveform(w: &Waveform) -> Self {
        Self { w: w.columns, h: w.levels, counts: w.counts.clone() }
    }
}

type TraceKey = (u32, bool, bool, bool);

fn trace_key(style: &ScopeStyle, flag: bool) -> TraceKey {
    (style.gain.to_bits(), style.colourise, flag, style.glow)
}

// ---------------------------------------------------------------- waveform

/// Luma waveform with optional additive R/G/B overlay.
#[derive(Default)]
pub struct WaveformView {
    luma: Option<Grid>,
    rgb: Option<[Grid; 3]>,
    tex: Option<TextureHandle>,
    /// Clip marker flags per column (see [`PreparedTrace::edges`]).
    edges: Vec<u8>,
    key: Option<TraceKey>,
    style: ScopeStyle,
}

impl WaveformView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the luma waveform from raw counts (rebuilds the texture once). When the
    /// style changes afterwards the texture is rebuilt on the next `show`.
    pub fn set_data(&mut self, ctx: &Context, wf: &Waveform) {
        self.luma = Some(Grid::from_waveform(wf));
        let st = self.style.clone();
        self.rebuild(ctx, &st);
    }

    /// Sets the per-channel waveforms (R, G, B) used for the overlay mode.
    pub fn set_rgb_data(&mut self, ctx: &Context, rgb: [&Waveform; 3]) {
        self.rgb = Some(rgb.map(Grid::from_waveform));
        let st = self.style.clone();
        self.rebuild(ctx, &st);
    }

    /// Uploads finished pixels (cheap). The pixels were prepared with the style.
    pub fn set_pixels(&mut self, ctx: &Context, t: &PreparedTrace) {
        self.luma = None;
        self.rgb = None;
        self.key = None;
        self.edges = t.edges.clone();
        upload_trace(ctx, &mut self.tex, &texture_name("waveform"), t);
    }

    /// Drops all data; the empty graticule is drawn.
    pub fn clear(&mut self) {
        self.luma = None;
        self.rgb = None;
        self.tex = None;
        self.edges.clear();
        self.key = None;
    }

    pub fn has_data(&self) -> bool {
        self.tex.is_some()
    }

    fn has_counts(&self) -> bool {
        self.luma.is_some() || self.rgb.is_some()
    }

    fn wants_rgb(&self, style: &ScopeStyle) -> bool {
        style.waveform_rgb && self.rgb.is_some()
    }

    fn rebuild(&mut self, ctx: &Context, style: &ScopeStyle) {
        let use_rgb = self.wants_rgb(style);
        self.key = Some(trace_key(style, use_rgb));
        let name = texture_name("waveform");
        if use_rgb {
            let g = self.rgb.as_ref().expect("checked by wants_rgb");
            let tints = if style.colourise { PRIMARY_TINTS } else { CHANNEL_TINTS };
            let layers: Vec<Layer<'_>> = (0..3)
                .map(|i| Layer { counts: &g[i].counts, tint: tints[i] })
                .collect();
            let px = layers_to_rgba_glow(&layers, g[0].w, g[0].h, style.gain, reference_of(&layers), style.glow);
            upload(ctx, &mut self.tex, &name, &px, g[0].w, g[0].h);
            let flags: Vec<Vec<u8>> = g.iter().map(|g| edge_flags(&g.counts, g.w, g.h)).collect();
            self.edges = merge_edges(&[&flags[0], &flags[1], &flags[2]]);
        } else if let Some(g) = &self.luma {
            let layers = [Layer { counts: &g.counts, tint: LUMA_TINT }];
            let px = layers_to_rgba_glow(&layers, g.w, g.h, style.gain, reference_of(&layers), style.glow);
            upload(ctx, &mut self.tex, &name, &px, g.w, g.h);
            self.edges = edge_flags(&g.counts, g.w, g.h);
        } else {
            self.tex = None;
            self.edges.clear();
        }
    }

    /// Paints into the available space; never panics without data.
    pub fn show(&mut self, ui: &mut Ui, style: &ScopeStyle) -> Response {
        let (outer, resp) = alloc(ui);
        if self.has_counts() && self.key != Some(trace_key(style, self.wants_rgb(style))) {
            self.rebuild(ui.ctx(), style);
        }
        self.style = style.clone();
        let p = ui.painter().with_clip_rect(outer);
        let plot = level_plot_rect(outer);
        if plot.width() < 8.0 || plot.height() < 8.0 {
            return resp;
        }
        p.rect_filled(plot, 0.0, style.background);
        if let Some(t) = &self.tex {
            p.image(t.id(), plot, vectorscope_uv(false), Color32::WHITE);
        }
        if style.graticule {
            draw_level_graticule(&p, plot, outer.left(), style);
        }
        if style.clip_markers && self.tex.is_some() {
            draw_edge_markers(&p, plot, &self.edges);
        }
        if let Some(pos) = pointer(&resp, style).filter(|q| plot.contains(*q)) {
            draw_level_cursor(&p, plot, outer.left(), pos.y, style);
        }
        resp
    }
}

// ------------------------------------------------------------------ parade

/// Three or four waveform columns side by side: R, G, B (default), YRGB or YCbCr.
#[derive(Default)]
pub struct ParadeView {
    grids: Option<Vec<Grid>>,
    mode: ParadeMode,
    tex: Vec<Option<TextureHandle>>,
    edges: Vec<Vec<u8>>,
    key: Option<TraceKey>,
    style: ScopeStyle,
}

/// Plot rectangle of the parade: the RGB parade keeps the waveform's, the other modes
/// reserve a strip below for the column names.
fn parade_plot_rect(outer: Rect, mode: ParadeMode) -> Rect {
    let mut r = level_plot_rect(outer);
    if mode != ParadeMode::Rgb {
        r.max.y -= PARADE_LABEL_H;
    }
    r
}

fn tint_color(t: [f32; 3]) -> Color32 {
    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    Color32::from_rgb(b(t[0]), b(t[1]), b(t[2]))
}

impl ParadeView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the RGB parade data (R, G, B), as returned by `rawgraph_scopes::parade`.
    pub fn set_data(&mut self, ctx: &Context, parade: &[Waveform; 3]) {
        self.set_data_planes(ctx, ParadeMode::Rgb, parade);
    }

    /// Sets the parade data of any mode: the planes left to right, as in
    /// `ScopeSet::parade_columns`.
    pub fn set_data_planes(&mut self, ctx: &Context, mode: ParadeMode, planes: &[Waveform]) {
        self.mode = mode;
        self.grids = Some(planes.iter().map(Grid::from_waveform).collect());
        let st = self.style.clone();
        self.rebuild(ctx, &st);
    }

    /// Uploads finished pixels (cheap): the RGB parade.
    pub fn set_pixels(&mut self, ctx: &Context, t: &[PreparedTrace; 3]) {
        self.set_planes(ctx, ParadeMode::Rgb, t);
    }

    /// Uploads finished pixels of any mode (cheap).
    pub fn set_planes(&mut self, ctx: &Context, mode: ParadeMode, t: &[PreparedTrace]) {
        self.mode = mode;
        self.grids = None;
        self.key = None;
        self.tex.resize_with(t.len(), || None);
        self.tex.truncate(t.len());
        for (slot, tr) in self.tex.iter_mut().zip(t) {
            upload_trace(ctx, slot, &texture_name("parade"), tr);
        }
        self.edges = t.iter().map(|tr| tr.edges.clone()).collect();
    }

    pub fn clear(&mut self) {
        self.grids = None;
        self.tex.clear();
        self.edges.clear();
        self.key = None;
    }

    pub fn has_data(&self) -> bool {
        self.tex.iter().any(|t| t.is_some())
    }

    /// Mode of the data on show.
    pub fn mode(&self) -> ParadeMode {
        self.mode
    }

    fn rebuild(&mut self, ctx: &Context, style: &ScopeStyle) {
        self.key = Some(trace_key(style, false));
        let Some(g) = &self.grids else {
            self.tex.clear();
            self.edges.clear();
            return;
        };
        let tints = plane_tints(self.mode);
        let all: Vec<Layer<'_>> = g
            .iter()
            .enumerate()
            .map(|(i, g)| Layer { counts: &g.counts, tint: tints.get(i).copied().unwrap_or(LUMA_TINT) })
            .collect();
        let reference = reference_of(&all);
        self.tex.resize_with(g.len(), || None);
        self.tex.truncate(g.len());
        for i in 0..g.len() {
            let px = layers_to_rgba_glow(&all[i..=i], g[i].w, g[i].h, style.gain, reference, style.glow);
            let name = texture_name("parade");
            upload(ctx, &mut self.tex[i], &name, &px, g[i].w, g[i].h);
        }
        let chroma = self.mode == ParadeMode::YCbCr;
        self.edges = g
            .iter()
            .enumerate()
            .map(|(i, g)| if chroma && i > 0 { Vec::new() } else { edge_flags(&g.counts, g.w, g.h) })
            .collect();
    }

    pub fn show(&mut self, ui: &mut Ui, style: &ScopeStyle) -> Response {
        let (outer, resp) = alloc(ui);
        if self.grids.is_some() && self.key != Some(trace_key(style, false)) {
            self.rebuild(ui.ctx(), style);
        }
        self.style = style.clone();
        let p = ui.painter().with_clip_rect(outer);
        // Without data the empty graticule already follows the chosen mode.
        let mode = if self.has_data() { self.mode } else { style.parade_mode };
        let plot = parade_plot_rect(outer, mode);
        if plot.width() < 12.0 || plot.height() < 8.0 {
            return resp;
        }
        p.rect_filled(plot, 0.0, style.background);
        let panels = parade_rects_n(plot, 2.0, mode.planes());
        for (panel, tex) in panels.iter().zip(&self.tex) {
            if let Some(t) = tex {
                p.image(t.id(), *panel, vectorscope_uv(false), Color32::WHITE);
            }
        }
        if style.graticule {
            draw_level_graticule(&p, plot, outer.left(), style);
        }
        if mode == ParadeMode::YCbCr {
            // Zero chroma sits at mid-height of the Cb and Cr columns.
            let y = level_to_y(plot, 0.5);
            for panel in panels.iter().skip(1) {
                p.line_segment(
                    [pos2(panel.left(), y), pos2(panel.right(), y)],
                    Stroke::new(1.0, style.graticule_strong.gamma_multiply(1.5)),
                );
            }
        }
        for pair in panels.windows(2) {
            let x = (pair[0].right() + pair[1].left()) * 0.5;
            p.line_segment(
                [pos2(x, plot.top()), pos2(x, plot.bottom())],
                Stroke::new(2.0, style.graticule_strong),
            );
        }
        if style.clip_markers && self.has_data() {
            for (panel, flags) in panels.iter().zip(&self.edges) {
                draw_edge_markers(&p, *panel, flags);
            }
        }
        if mode != ParadeMode::Rgb {
            let tints = plane_tints(mode);
            for ((panel, name), tint) in panels.iter().zip(mode.plane_names()).zip(tints) {
                if panel.width() >= 14.0 {
                    p.text(
                        pos2(panel.center().x, plot.bottom() + 3.0),
                        Align2::CENTER_TOP,
                        *name,
                        label_font(style),
                        tint_color(tint).gamma_multiply(0.85),
                    );
                }
            }
        }
        if let Some(pos) = pointer(&resp, style).filter(|q| plot.contains(*q)) {
            draw_level_cursor(&p, plot, outer.left(), pos.y, style);
        }
        resp
    }
}

// ------------------------------------------------------------- vectorscope

/// Vectorscope with Resolve-style graticule.
#[derive(Default)]
pub struct VectorscopeView {
    size: usize,
    counts: Option<Vec<u32>>,
    tex: Option<TextureHandle>,
    key: Option<TraceKey>,
    style: ScopeStyle,
}

impl VectorscopeView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_data(&mut self, ctx: &Context, vs: &Vectorscope) {
        self.size = vs.size;
        self.counts = Some(vs.counts.clone());
        let st = self.style.clone();
        self.rebuild(ctx, &st);
    }

    /// Uploads finished pixels (cheap).
    pub fn set_pixels(&mut self, ctx: &Context, t: &PreparedTrace) {
        self.counts = None;
        self.key = None;
        self.size = t.width;
        upload_trace(ctx, &mut self.tex, &texture_name("vectorscope"), t);
    }

    pub fn clear(&mut self) {
        self.counts = None;
        self.tex = None;
        self.key = None;
        self.size = 0;
    }

    pub fn has_data(&self) -> bool {
        self.tex.is_some()
    }

    fn rebuild(&mut self, ctx: &Context, style: &ScopeStyle) {
        self.key = Some(trace_key(style, false));
        if let Some(c) = &self.counts {
            let px = vectorscope_to_rgba(c, self.size, style.gain, style.colourise);
            let name = texture_name("vectorscope");
            upload(ctx, &mut self.tex, &name, &px, self.size, self.size);
        } else {
            self.tex = None;
        }
    }

    pub fn show(&mut self, ui: &mut Ui, style: &ScopeStyle) -> Response {
        let (outer, resp) = alloc(ui);
        if self.counts.is_some() && self.key != Some(trace_key(style, false)) {
            self.rebuild(ui.ctx(), style);
        }
        self.style = style.clone();
        let p = ui.painter().with_clip_rect(outer);
        let sq = centred_square(outer);
        if sq.width() < 24.0 {
            return resp;
        }
        p.rect_filled(sq, 0.0, style.background);
        let inner = vectorscope_inner(sq);
        if let Some(t) = &self.tex {
            p.image(t.id(), inner, vectorscope_uv(style.zoom), Color32::WHITE);
        }
        let ps = p.with_clip_rect(sq);
        if style.graticule {
            self.draw_graticule(&ps, sq, inner, style);
        }
        if let Some(pos) = pointer(&resp, style) {
            if let Some((hue, sat)) = vectorscope_readout(inner, pos, style.zoom) {
                ps.circle_stroke(pos, 4.0, Stroke::new(1.0, CURSOR));
                let m = 4.0;
                let anchor = match (pos.x < sq.center().x, pos.y < sq.center().y) {
                    (true, true) => (Align2::RIGHT_BOTTOM, sq.right_bottom() - vec2(m, m)),
                    (false, true) => (Align2::LEFT_BOTTOM, sq.left_bottom() + vec2(m, -m)),
                    (true, false) => (Align2::RIGHT_TOP, sq.right_top() + vec2(-m, m)),
                    (false, false) => (Align2::LEFT_TOP, sq.left_top() + vec2(m, m)),
                };
                badge(&ps, anchor.1, anchor.0, format!("Hue {hue:.0}\u{b0}\nSat {sat:.0} %"), style, CURSOR);
            }
        }
        resp
    }

    fn draw_graticule(&self, p: &Painter, sq: Rect, inner: Rect, style: &ScopeStyle) {
        let zf = if style.zoom { 2.0 } else { 1.0 };
        let centre = scope_uv_to_pos(inner, 0.5, 0.5, style.zoom);
        let w = inner.width();
        let thin = Stroke::new(1.0, style.graticule_color);
        let strong = Stroke::new(1.0, style.graticule_strong);
        for (i, r) in [0.125f32, 0.25, 0.375, 0.5].into_iter().enumerate() {
            p.circle_stroke(centre, r * w * zf, if i == 3 { strong } else { thin });
        }
        p.line_segment([pos2(sq.left(), centre.y), pos2(sq.right(), centre.y)], thin);
        p.line_segment([pos2(centre.x, sq.top()), pos2(centre.x, sq.bottom())], thin);
        let big = sq.width() >= 150.0;
        if big {
            for k in -8i32..=8 {
                let d = k as f32 / 16.0 * w * zf;
                p.line_segment([pos2(centre.x + d, centre.y - 2.5), pos2(centre.x + d, centre.y + 2.5)], strong);
                p.line_segment([pos2(centre.x - 2.5, centre.y + d), pos2(centre.x + 2.5, centre.y + d)], strong);
            }
        }
        if style.skin_line {
            let end = centre + skin_line_dir() * (0.5 * w * zf);
            p.line_segment([centre, end], Stroke::new(1.0, Color32::from_rgb(168, 124, 78)));
        }
        let size = if self.size > 0 { self.size } else { 256 };
        let half = (0.022 * w * zf).max(3.0);
        for (name, cb, cr) in vectorscope_targets() {
            if style.targets_100 {
                let pos = vectorscope_target_pos(inner, cb, cr, size, style.zoom);
                p.rect_stroke(Rect::from_center_size(pos, vec2(half * 2.0, half * 2.0)), 0.0, thin, StrokeKind::Middle);
            }
            let pos = vectorscope_target_pos(inner, cb * 0.75, cr * 0.75, size, style.zoom);
            p.rect_stroke(
                Rect::from_center_size(pos, vec2(half * 2.0, half * 2.0)),
                0.0,
                strong,
                StrokeKind::Middle,
            );
            if sq.width() >= 120.0 {
                let out = pos - centre;
                let dir = if out.length() > 1e-3 { out.normalized() } else { vec2(0.0, -1.0) };
                let galley = p.layout_no_wrap(name.to_owned(), label_font(style), style.label_color);
                let gs = galley.size();
                let c = pos + dir * (half + style.label_size * 0.9);
                // Keep the whole label inside the square.
                let lo = sq.min + gs * 0.5 + vec2(1.0, 1.0);
                let hi = sq.max - gs * 0.5 - vec2(1.0, 1.0);
                let c = pos2(c.x.clamp(lo.x, hi.x.max(lo.x)), c.y.clamp(lo.y, hi.y.max(lo.y)));
                p.galley(c - gs * 0.5, galley, style.label_color);
            }
        }
    }
}

// --------------------------------------------------------------- histogram

/// RGB + luma histogram.
#[derive(Default)]
pub struct HistogramView {
    data: Option<PreparedHistogram>,
}

impl HistogramView {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_data(&mut self, _ctx: &Context, h: &Histogram) {
        self.data = Some(PreparedHistogram {
            r: h.r.clone(),
            g: h.g.clone(),
            b: h.b.clone(),
            luma: h.luma.clone(),
        });
    }

    /// Stores prepared histogram counts (cheap).
    pub fn set_prepared(&mut self, h: &PreparedHistogram) {
        self.data = Some(h.clone());
    }

    pub fn clear(&mut self) {
        self.data = None;
    }

    pub fn has_data(&self) -> bool {
        self.data.is_some()
    }

    pub fn show(&mut self, ui: &mut Ui, style: &ScopeStyle) -> Response {
        let (outer, resp) = alloc(ui);
        let p = ui.painter().with_clip_rect(outer);
        let footer = if style.graticule { HIST_FOOTER } else { 0.0 };
        let plot = Rect::from_min_max(
            pos2(outer.left() + 6.0, outer.top() + 2.0),
            pos2(outer.right() - 6.0, outer.bottom() - footer - 1.0),
        );
        if plot.width() < 12.0 || plot.height() < 8.0 {
            return resp;
        }
        p.rect_filled(plot, 0.0, style.background);
        let mut max = 0;
        if let Some(d) = &self.data {
            max = [&d.r, &d.g, &d.b, &d.luma]
                .iter()
                .flat_map(|v| v.iter().copied())
                .max()
                .unwrap_or(0);
            // Alpha 0 premultiplied: the areas add up, so overlaps read as secondary
            // colours and, with all three, white.
            let f = histogram_fills(style.colourise);
            let fills: [(&Vec<u32>, Color32); 3] = [
                (&d.r, Color32::from_rgba_premultiplied(f[0][0], f[0][1], f[0][2], 0)),
                (&d.g, Color32::from_rgba_premultiplied(f[1][0], f[1][1], f[1][2], 0)),
                (&d.b, Color32::from_rgba_premultiplied(f[2][0], f[2][1], f[2][2], 0)),
            ];
            let line_color = Color32::from_rgb(225, 225, 225);
            match style.histogram_mode {
                HistogramMode::Overlay => {
                    for (bins, color) in fills {
                        p.add(Shape::mesh(histogram_mesh(plot, bins, max, style, color)));
                    }
                    let pts = histogram_points(plot, &d.luma, max, style);
                    if pts.len() > 1 {
                        p.add(Shape::line(pts, Stroke::new(1.2, line_color)));
                    }
                }
                HistogramMode::Luma => {
                    let grey = Color32::from_rgba_premultiplied(118, 118, 118, 0);
                    p.add(Shape::mesh(histogram_mesh(plot, &d.luma, max, style, grey)));
                    let pts = histogram_points(plot, &d.luma, max, style);
                    if pts.len() > 1 {
                        p.add(Shape::line(pts, Stroke::new(1.2, line_color)));
                    }
                }
                HistogramMode::Stacked => {
                    let gap = (plot.height() / 16.0).clamp(0.0, 2.0);
                    let rows = stacked_rows(plot, 4, gap);
                    let names = ["R", "G", "B", "Y"];
                    let tints = [
                        Color32::from_rgb(235, 90, 90),
                        Color32::from_rgb(90, 205, 100),
                        Color32::from_rgb(110, 140, 245),
                        Color32::from_rgb(225, 225, 225),
                    ];
                    let all = [&d.r, &d.g, &d.b, &d.luma];
                    for (i, row) in rows.iter().enumerate() {
                        if i < 3 {
                            let (bins, color) = fills[i];
                            p.add(Shape::mesh(histogram_mesh(*row, bins, max, style, color)));
                        } else {
                            let grey = Color32::from_rgba_premultiplied(118, 118, 118, 0);
                            p.add(Shape::mesh(histogram_mesh(*row, all[i], max, style, grey)));
                        }
                        let pts = histogram_points(*row, all[i], max, style);
                        if pts.len() > 1 {
                            p.add(Shape::line(pts, Stroke::new(1.0, tints[i].gamma_multiply(0.9))));
                        }
                        if row.height() >= 11.0 {
                            p.text(
                                row.left_top() + vec2(3.0, 1.0),
                                Align2::LEFT_TOP,
                                names[i],
                                label_font(style),
                                tints[i],
                            );
                        }
                    }
                }
            }
        }
        if style.graticule {
            let narrow = plot.width() < 200.0;
            for i in 0..=4 {
                let f = i as f32 / 4.0;
                let x = plot.left() + f * plot.width();
                let color = if i == 0 || i == 4 { style.graticule_strong } else { style.graticule_color };
                p.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, color));
                if narrow && i % 2 == 1 {
                    continue;
                }
                let (anchor, text) = match i {
                    0 => (Align2::LEFT_TOP, "0".to_owned()),
                    4 => (Align2::RIGHT_TOP, "100 %".to_owned()),
                    _ => (Align2::CENTER_TOP, format!("{}", i * 25)),
                };
                p.text(pos2(x, plot.bottom() + 2.0), anchor, text, label_font(style), style.label_color);
            }
        }
        if let (Some(d), Some(pos)) = (&self.data, pointer(&resp, style).filter(|q| plot.contains(*q))) {
            let bins = d.luma.len();
            if bins > 0 {
                let bin = histogram_bin_at(plot, pos.x, bins);
                let x = histogram_x(plot, bin, bins);
                p.line_segment([pos2(x, plot.top()), pos2(x, plot.bottom())], Stroke::new(1.0, CURSOR.gamma_multiply(0.8)));
                let at = |v: &Vec<u32>| v.get(bin).copied().unwrap_or(0);
                let text = format!(
                    "{}\nR {}  G {}\nB {}  Y {}",
                    format_level(histogram_bin_level(bin, bins), style.scale),
                    at(&d.r),
                    at(&d.g),
                    at(&d.b),
                    at(&d.luma)
                );
                let _ = max;
                let m = 4.0;
                let (a, c) = if pos.x < plot.center().x {
                    (Align2::RIGHT_TOP, plot.right_top() + vec2(-m, m))
                } else {
                    (Align2::LEFT_TOP, plot.left_top() + vec2(m, m))
                };
                badge(&p.with_clip_rect(plot), c, a, text, style, CURSOR);
            }
        }
        resp
    }
}

fn histogram_points(plot: Rect, bins: &[u32], max: u32, style: &ScopeStyle) -> Vec<Pos2> {
    bins.iter()
        .enumerate()
        .map(|(i, &v)| {
            let h = histogram_height(v, max, style.histogram_log, style.gain);
            pos2(histogram_x(plot, i, bins.len()), plot.bottom() - h * plot.height())
        })
        .collect()
}

fn histogram_mesh(plot: Rect, bins: &[u32], max: u32, style: &ScopeStyle, color: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    let pts = histogram_points(plot, bins, max, style);
    for pair in pts.windows(2) {
        let base = mesh.vertices.len() as u32;
        mesh.colored_vertex(pos2(pair[0].x, plot.bottom()), color);
        mesh.colored_vertex(pair[0], color);
        mesh.colored_vertex(pair[1], color);
        mesh.colored_vertex(pos2(pair[1].x, plot.bottom()), color);
        mesh.add_triangle(base, base + 1, base + 2);
        mesh.add_triangle(base, base + 2, base + 3);
    }
    mesh
}
