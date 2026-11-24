use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use rawgraph_core::color::AP1_LUMA;
use rawgraph_fx::curve_eval;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::image::{check_size, create_texture, GpuImage, FLOAT_USAGE};
use crate::pass::{build_pass, run_pass, Pass, WORKGROUP};

/// Pointwise operations, applied in order, in ONE pass over the pixels.
#[derive(Clone, Debug, PartialEq)]
pub enum PointOp {
    /// RGB * gain.
    Gain(f32),
    /// RGB matrix (row-major, `out = m * rgb`).
    Matrix([[f32; 3]; 3]),
    /// `rawgraph_core::ops::saturation`.
    Saturation(f32),
    /// `rawgraph_core::ops::contrast`.
    Contrast { amount: f32, pivot: f32 },
    /// `rawgraph_core::ops::highlights_shadows`.
    HighlightsShadows { highlights: f32, shadows: f32 },
    /// ASC CDL per channel: positive `x * slope + offset` goes to the power `power`; values
    /// `<= 0` after slope and offset keep that linear value. Non-finite parameters count as
    /// the neutral value (slope 1, offset 0, power 1), the power is limited to 0.01..20 and a result
    /// above `f32::MAX` is capped to it, all like `rawgraph_fx::color_wheels`. Analytic.
    Cdl { slope: [f32; 3], offset: [f32; 3], power: [f32; 3] },
    /// `rawgraph_fx::tone_curve`. The 1024-entry gain table is baked on the host with the
    /// same code as the CPU and read with the same linear interpolation.
    ToneCurve { shadows: f32, darks: f32, lights: f32, highlights: f32 },
    /// `rawgraph_fx::point_curve`; `channel` 0 luminance, 1 red, 2 green, 3 blue, 4 Sat vs Sat,
    /// 5 Hue vs Hue, 6 Hue vs Sat (any other value is the identity). The 4097-entry table is baked
    /// with `curve_eval` exactly like the CPU and read with the same linear interpolation. Channels
    /// 4..=6 use the hue/saturation definition of `rawgraph_fx::point_curve`; their neutral curve is
    /// `[[0, 0.5], [1, 0.5]]` (a bit-exact no-op, dropped from the shader work).
    PointCurve { points: Vec<[f32; 2]>, channel: u8 },
    /// `rawgraph_fx::hue_saturation`; per band `[hue, saturation, lightness]`. Analytic.
    HueSaturation { bands: [[f32; 3]; 6] },
    /// `rawgraph_fx::vignette`; uses the pixel position and the size of the image. Analytic.
    Vignette { amount: f32, midpoint: f32, feather: f32 },
    /// `rawgraph_fx::whites_blacks`. Non-finite parameters count as 0. Analytic.
    WhitesBlacks { whites: f32, blacks: f32 },
    /// `rawgraph_fx::vibrance`. A non-finite amount counts as 0. Analytic.
    Vibrance(f32),
}

/// The kind of an op, i.e. what is baked into the shader. Parameters are not part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum OpKind {
    Gain,
    Matrix,
    Saturation,
    Contrast,
    HighlightsShadows,
    Cdl,
    ToneCurve,
    PointCurve,
    HueSaturation,
    Vignette,
    WhitesBlacks,
    Vibrance,
}

/// Entries of the tone curve gain table (as the CPU: 1024).
const TONE_LUT: usize = 1024;
/// Intervals of the point curve table (as the CPU: 4096, so 4097 entries).
const CURVE_N: usize = 4096;
/// Value that `t = 1` of the point curve maps back to (as the CPU).
const CURVE_V_MAX: f32 = 1.0e6;
const GREY: f32 = 0.18;

const BAND_CENTRES: [f32; 4] = [0.125, 0.375, 0.625, 0.875];
const BAND_SIGMA: f32 = 0.16;
const BAND_STOPS: f32 = 1.5;

impl OpKind {
    /// `vec4` slots of parameters in the uniform buffer.
    fn slot_count(self) -> usize {
        match self {
            OpKind::Matrix | OpKind::Cdl => 3,
            OpKind::PointCurve => 2,
            OpKind::HueSaturation => 5,
            _ => 1,
        }
    }

    /// Floats of lookup table in the shared table buffer.
    fn table_len(self) -> usize {
        match self {
            OpKind::ToneCurve => TONE_LUT,
            OpKind::PointCurve => CURVE_N + 1,
            _ => 0,
        }
    }
}

fn finite_or(x: f32, fallback: f32) -> f32 {
    if x.is_finite() {
        x
    } else {
        fallback
    }
}

/// Log2 gain of the tone curve at `t` and its derivative (same code as `rawgraph_fx`, which keeps it private).
fn band_gain(p: &[f32; 4], t: f32) -> (f32, f32) {
    let (mut g, mut dg) = (0.0, 0.0);
    for (pi, c) in p.iter().zip(BAND_CENTRES) {
        let d = t - c;
        let w = (-0.5 * (d / BAND_SIGMA) * (d / BAND_SIGMA)).exp();
        g += pi * w;
        dg += pi * w * (-d / (BAND_SIGMA * BAND_SIGMA));
    }
    (BAND_STOPS * g, BAND_STOPS * dg)
}

fn monotonic_scale(p: &[f32; 4]) -> f32 {
    let mut worst = 0.0f32;
    for i in 0..=2048 {
        let t = i as f32 / 2048.0;
        let (_, dg) = band_gain(p, t);
        worst = worst.max(std::f32::consts::LN_2 * t * (1.0 - t) * (-dg));
    }
    if worst > 0.95 {
        0.95 / worst
    } else {
        1.0
    }
}

fn tone_params(shadows: f32, darks: f32, lights: f32, highlights: f32) -> [f32; 4] {
    [shadows, darks, lights, highlights].map(|v| finite_or(v, 0.0).clamp(-1.0, 1.0))
}

/// The curve through `points` changes something (mirrors the early returns of `rawgraph_fx::point_curve`).
fn curve_is_active(points: &[[f32; 2]], channel: u8) -> bool {
    if channel > 6 {
        return false;
    }
    if channel >= 4 {
        return colour_curve_is_active(points);
    }
    if points.len() < 2 || (points.len() == 2 && points[0] == [0.0, 0.0] && points[1] == [1.0, 1.0]) {
        return false;
    }
    // At least two distinct x among the finite points after clamping to 0..1.
    let mut first: Option<f32> = None;
    for p in points.iter().filter(|p| p[0].is_finite() && p[1].is_finite()) {
        let x = p[0].clamp(0.0, 1.0);
        match first {
            None => first = Some(x),
            Some(f) if f != x => return true,
            _ => {}
        }
    }
    false
}

/// Mirrors the early returns of the colour channels (4..=6) of `rawgraph_fx::point_curve`: the
/// cleaned points (non-finite dropped, clamped to 0..1, sorted by x, a later point with the same x
/// wins) have at least two distinct x and not all `y == 0.5`. `[[0,0],[1,1]]` is NOT neutral here.
fn colour_curve_is_active(points: &[[f32; 2]]) -> bool {
    let mut pts: Vec<(f32, f32)> = points
        .iter()
        .filter(|p| p[0].is_finite() && p[1].is_finite())
        .map(|p| (p[0].clamp(0.0, 1.0), p[1].clamp(0.0, 1.0)))
        .collect();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut clean: Vec<(f32, f32)> = Vec::with_capacity(pts.len());
    for p in pts {
        match clean.last_mut() {
            Some(last) if last.0 == p.0 => *last = p,
            _ => clean.push(p),
        }
    }
    clean.len() >= 2 && clean.iter().any(|p| p.1 != 0.5)
}

fn vignette_edges(midpoint: f32, feather: f32) -> (f32, f32) {
    let (mid, feather) = (finite_or(midpoint, 0.5), finite_or(feather, 0.5).max(0.0));
    let e0 = (mid - feather * 0.5).max(0.0);
    let e1 = (mid + feather * 0.5).max(e0);
    (e0, e1)
}

impl PointOp {
    fn kind(&self) -> OpKind {
        match self {
            PointOp::Gain(_) => OpKind::Gain,
            PointOp::Matrix(_) => OpKind::Matrix,
            PointOp::Saturation(_) => OpKind::Saturation,
            PointOp::Contrast { .. } => OpKind::Contrast,
            PointOp::HighlightsShadows { .. } => OpKind::HighlightsShadows,
            PointOp::Cdl { .. } => OpKind::Cdl,
            PointOp::ToneCurve { .. } => OpKind::ToneCurve,
            PointOp::PointCurve { .. } => OpKind::PointCurve,
            PointOp::HueSaturation { .. } => OpKind::HueSaturation,
            PointOp::Vignette { .. } => OpKind::Vignette,
            PointOp::WhitesBlacks { .. } => OpKind::WhitesBlacks,
            PointOp::Vibrance(_) => OpKind::Vibrance,
        }
    }

    /// `OpKind::slot_count` `vec4` slots of parameters in the uniform buffer.
    fn slots(&self) -> Vec<[f32; 4]> {
        match self {
            PointOp::Gain(g) => vec![[*g, 0.0, 0.0, 0.0]],
            PointOp::Matrix(m) => vec![
                [m[0][0], m[0][1], m[0][2], 0.0],
                [m[1][0], m[1][1], m[1][2], 0.0],
                [m[2][0], m[2][1], m[2][2], 0.0],
            ],
            PointOp::Saturation(s) => vec![[*s, 0.0, 0.0, 0.0]],
            // The exponent is computed on the host exactly like the CPU does (`exp2` in f32).
            PointOp::Contrast { amount, pivot } => vec![[*amount, *pivot, amount.exp2(), 0.0]],
            PointOp::HighlightsShadows { highlights, shadows } => vec![[*highlights, *shadows, 0.0, 0.0]],
            PointOp::Cdl { slope, offset, power } => {
                let s = slope.map(|v| finite_or(v, 1.0));
                let o = offset.map(|v| finite_or(v, 0.0));
                let p = power.map(|v| finite_or(v, 1.0).clamp(0.01, 20.0));
                let active = s != [1.0; 3] || o != [0.0; 3] || p != [1.0; 3];
                vec![
                    [s[0], s[1], s[2], if active { 1.0 } else { 0.0 }],
                    [o[0], o[1], o[2], 0.0],
                    [p[0], p[1], p[2], 0.0],
                ]
            }
            PointOp::ToneCurve { shadows, darks, lights, highlights } => {
                let p = tone_params(*shadows, *darks, *lights, *highlights);
                vec![[if p == [0.0; 4] { 0.0 } else { 1.0 }, 0.0, 0.0, 0.0]]
            }
            PointOp::PointCurve { points, channel } => {
                let t_max = CURVE_V_MAX / (CURVE_V_MAX + GREY);
                let active = curve_is_active(points, *channel);
                // Second slot: the infinity bit pattern, used as an opaque mask by `finm` in the colour channels.
                vec![[if active { 1.0 } else { 0.0 }, f32::from(*channel), t_max, 1.0], [f32::INFINITY, 0.0, 0.0, 0.0]]
            }
            PointOp::HueSaturation { bands } => {
                let b = bands.map(|band| band.map(|v| finite_or(v, 0.0).clamp(-1.0, 1.0)));
                let active = b != [[0.0; 3]; 6];
                let mut out = vec![[0.0f32; 4]; 5];
                for (i, v) in b.iter().flatten().enumerate() {
                    out[i / 4][i % 4] = *v;
                }
                out[4][2] = if active { 1.0 } else { 0.0 };
                out
            }
            PointOp::Vignette { amount, midpoint, feather } => {
                let amount = finite_or(*amount, 0.0);
                let (e0, e1) = vignette_edges(*midpoint, *feather);
                vec![[amount, e0, e1, if amount == 0.0 { 0.0 } else { 1.0 }]]
            }
            PointOp::WhitesBlacks { whites, blacks } => {
                let (w, b) = (finite_or(*whites, 0.0).clamp(-1.0, 1.0), finite_or(*blacks, 0.0).clamp(-1.0, 1.0));
                vec![[w, b, f32::INFINITY, if w == 0.0 && b == 0.0 { 0.0 } else { 1.0 }]]
            }
            PointOp::Vibrance(a) => {
                let a = finite_or(*a, 0.0).clamp(-1.0, 1.0);
                vec![[a, 0.0, f32::INFINITY, if a == 0.0 { 0.0 } else { 1.0 }]]
            }
        }
    }

    /// What decides the table content (bit patterns of the parameters); empty when the op has no
    /// table or does nothing. A table is rewritten only when this changes.
    fn table_key(&self) -> Vec<u32> {
        match self {
            PointOp::ToneCurve { shadows, darks, lights, highlights } => {
                let p = tone_params(*shadows, *darks, *lights, *highlights);
                if p == [0.0; 4] {
                    Vec::new()
                } else {
                    p.iter().map(|v| v.to_bits()).collect()
                }
            }
            PointOp::PointCurve { points, channel } if curve_is_active(points, *channel) => {
                points.iter().flat_map(|p| [p[0].to_bits(), p[1].to_bits()]).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The table for a non-empty `table_key`.
    fn table_data(&self) -> Vec<f32> {
        match self {
            PointOp::ToneCurve { shadows, darks, lights, highlights } => {
                let p = tone_params(*shadows, *darks, *lights, *highlights);
                let k = monotonic_scale(&p);
                (0..TONE_LUT).map(|i| (band_gain(&p, i as f32 / (TONE_LUT - 1) as f32).0 * k).exp2()).collect()
            }
            PointOp::PointCurve { points, .. } => {
                (0..=CURVE_N).map(|i| curve_eval(points, i as f32 / CURVE_N as f32)).collect()
            }
            _ => Vec::new(),
        }
    }
}

pub(crate) struct PointPipeline {
    pass: Pass,
    uniform_size: u64,
    /// Per op: first float of its table in `table_buf`.
    table_offsets: Vec<Option<usize>>,
    /// Lookup tables of all table ops of the chain (None when the chain has none).
    table_buf: Option<wgpu::Buffer>,
    /// Per op: key of the table content currently in the buffer. Held while a pass is submitted, so
    /// concurrent callers of the same pipeline cannot overwrite each other's tables.
    table_keys: Mutex<Vec<Vec<u32>>>,
}

// Placeholders: `$0`..`$4` are slot indices (`P[$0]`), `$T` the first table float of the op.

const SNIPPET_CDL: &str = r#"    if (P[$0].w != 0.0) {
        let s = P[$0].xyz;
        let o = P[$1].xyz;
        let p = P[$2].xyz;
        c = vec3<f32>(cdl1(c.x, s.x, o.x, p.x), cdl1(c.y, s.y, o.y, p.y), cdl1(c.z, s.z, o.z, p.z));
    }
"#;

const SNIPPET_TONE: &str = r#"    if (P[$0].x != 0.0) {
        let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
        if (y > 0.0) {
            var pos = (y / (y + 0.18)) * 1023.0;
            if (!fin(pos)) {
                pos = 1.0;
            }
            let i = min(u32(pos), 1022u);
            let g0 = T[$Tu + i];
            let g1 = T[$Tu + i + 1u];
            c = c * (g0 + (g1 - g0) * min(pos - f32(i), 1.0));
        }
    }
"#;

const SNIPPET_CURVE: &str = r#"    if (P[$0].x != 0.0) {
        let ch = u32(P[$0].y);
        let tmax = P[$0].z;
        let one = P[$0].w;
        if (ch >= 4u) {
            FM = bitcast<u32>(P[$1].x);
            // Colour channels (Sat vs Sat 4, Hue vs Hue 5, Hue vs Sat 6), as `colour_curve` of rawgraph_fx.
            let r = c.x;
            let g = c.y;
            let bl = c.z;
            let a = r - 0.5 * (g + bl);
            let bb = SQRT3_2 * (g - bl);
            let m = max(abs(a), abs(bb));
            if (m > 0.0 && finm(m)) {
                let na = a / m;
                let nb = bb / m;
                let chroma = m * sqrt(na * na + nb * nb);
                let y = LUMA.x * r + LUMA.y * g + LUMA.z * bl;
                if (chroma > 0.0 && finm(chroma) && finm(y)) {
                    if (ch == 5u) {
                        var hh = atan2(nb, na) * (0.5 * INV_PI);
                        hh = min(hh - floor(hh), 1.0);
                        let shift = (curve_lookup(hh, $Tu, one) - 0.5) * 120.0;
                        let sa = sin(shift * DEG);
                        let ca = cos(shift * DEG);
                        let x2 = a * ca - bb * sa;
                        let y2 = a * sa + bb * ca;
                        let zo = vec3<f32>(a * (2.0 / 3.0), -a * (1.0 / 3.0) + bb * INV_SQRT3, -a * (1.0 / 3.0) - bb * INV_SQRT3);
                        let zn = vec3<f32>(x2 * (2.0 / 3.0), -x2 * (1.0 / 3.0) + y2 * INV_SQRT3, -x2 * (1.0 / 3.0) - y2 * INV_SQRT3);
                        var d = zn - zo;
                        let dy = LUMA.x * d.x + LUMA.y * d.y + LUMA.z * d.z;
                        d = d - vec3<f32>(dy);
                        c = vec3<f32>(r + d.x, g + d.y, bl + d.z);
                    } else {
                        var pos = 0.0;
                        if (ch == 6u) {
                            var hh = atan2(nb, na) * (0.5 * INV_PI);
                            hh = min(hh - floor(hh), 1.0);
                            pos = hh;
                        } else {
                            pos = chroma / (abs(y) + chroma);
                        }
                        let factor = 2.0 * curve_lookup(clamp(pos, 0.0, 1.0), $Tu, one);
                        c = vec3<f32>(y) + (c - vec3<f32>(y)) * factor;
                    }
                }
            }
        } else if (ch == 0u) {
            let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
            if (y > 0.0 && fin(y)) {
                let gain = divc(curve_to_v(curve_lookup(divc(y, y + 0.18), $Tu, one), tmax), y);
                if (fin(gain)) {
                    c = c * gain;
                }
            }
        } else {
            let v = c[ch - 1u];
            if (v >= 0.0 && fin(v)) {
                c[ch - 1u] = curve_to_v(curve_lookup(divc(v, v + 0.18), $Tu, one), tmax);
            }
        }
    }
"#;

const SNIPPET_VIGNETTE: &str = r#"    if (P[$0].w != 0.0) {
        let hw = f32(dims.x) * 0.5;
        let hh = f32(dims.y) * 0.5;
        let inv_diag = 1.0 / sqrt(hw * hw + hh * hh);
        let dx = f32(gid.x) + 0.5 - hw;
        let dy = f32(gid.y) + 0.5 - hh;
        let d = sqrt(dx * dx + dy * dy) * inv_diag;
        let e0 = P[$0].y;
        let e1 = P[$0].z;
        var s = 0.0;
        if (e1 <= e0) {
            if (d >= e0) {
                s = 1.0;
            }
        } else {
            s = sstep(e0, e1, d);
        }
        if (s > 0.0) {
            c = c * exp2(2.0 * P[$0].x * s);
        }
    }
"#;

// Mirrors `rawgraph_fx::whites_blacks`. `fpos` is Rust's `max(x, 0.0)` (NaN becomes 0).
const SNIPPET_WHITES_BLACKS: &str = r#"    if (P[$0].w != 0.0) {
        FM = bitcast<u32>(P[$0].z);
        let whites = P[$0].x;
        let blacks = P[$0].y;
        let y = LUMA.x * fpos(c.x) + LUMA.y * fpos(c.y) + LUMA.z * fpos(c.z);
        // Above 1e30 the CPU quotient is exactly 1; the division could use a reciprocal that
        // underflows for y > 8.5e37. An infinite y leaves the pixel alone (selected at the end).
        var t = 1.0;
        if (y <= 1.0e30) {
            t = y / (y + 0.18);
        }
        let wh = sstep(0.35, 0.95, t);
        let wb = 1.0 - sstep(0.0, 0.45, t);
        let gain = exp2(whites * wh);
        let add = blacks * 0.03 * wb;
        var o = c * gain + vec3<f32>(add);
        if (blacks < 0.0) {
            o = vec3<f32>(fmax(o.x, fmin(c.x, 0.0)), fmax(o.y, fmin(c.y, 0.0)), fmax(o.z, fmin(c.z, 0.0)));
        }
        c = select(c, o, finm(y));
    }
"#;

// Mirrors `rawgraph_fx::vibrance`.
const SNIPPET_VIBRANCE: &str = r#"    if (P[$0].w != 0.0) {
        FM = bitcast<u32>(P[$0].z);
        let p = vec3<f32>(fpos(c.x), fpos(c.y), fpos(c.z));
        let mx = fmax(p.x, fmax(p.y, p.z));
        let mn = fmin(p.x, fmin(p.y, p.z));
        var s = 0.0;
        if (mx > 1.0e-6) {
            // Scaled down above 1e30 so the division cannot hit a reciprocal that underflows.
            let sc = select(1.0, 1.0e-10, mx > 1.0e30);
            s = (mx * sc - mn * sc) / (mx * sc);
        }
        let k = 1.0 + P[$0].x * (1.0 - s);
        let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
        if (finm(y) && finm(k)) {
            c = vec3<f32>(y) + (c - vec3<f32>(y)) * k;
        }
    }
"#;

const SNIPPET_HUESAT_BODY: &str = r#"        let r = c.x;
        let g = c.y;
        let bl = c.z;
        let a = r - 0.5 * (g + bl);
        let bb = SQRT3_2 * (g - bl);
        let chroma = sqrt(a * a + bb * bb);
        if (chroma > 0.0 && fin(chroma)) {
            let inv = 1.0 / chroma;
            let x = a * inv;
            let y = bb * inv;
            var dirs = array<vec2<f32>, 6>(
                vec2<f32>(1.0, 0.0), vec2<f32>(0.5, SQRT3_2), vec2<f32>(-0.5, SQRT3_2),
                vec2<f32>(-1.0, 0.0), vec2<f32>(-0.5, -SQRT3_2), vec2<f32>(0.5, -SQRT3_2));
            let sa = y >= 0.0;
            let sb = 0.5 * y - SQRT3_2 * x >= 0.0;
            let sc = -0.5 * y - SQRT3_2 * x >= 0.0;
            var idx = 6u;
            if (sa && !sb && !sc) { idx = 0u; }
            else if (sa && sb && !sc) { idx = 1u; }
            else if (sa && sb && sc) { idx = 2u; }
            else if (!sa && sb && sc) { idx = 3u; }
            else if (!sa && !sb && sc) { idx = 4u; }
            else if (!sa && !sb && !sc) { idx = 5u; }
            var i = 0u;
            var w1 = 0.0;
            if (idx < 6u) {
                let cp = min(x * dirs[idx].x + y * dirs[idx].y, 1.0);
                let cos3 = cp * (4.0 * cp * cp - 3.0);
                w1 = clamp(0.5 * (1.0 - cos3), 0.0, 1.0);
                i = idx;
            } else {
                var t = atan2(bb, a);
                if (t < 0.0) { t = t + TAU; }
                t = t * (6.0 / TAU);
                i = min(u32(t), 5u);
                w1 = 0.5 * (1.0 - cos(PI * clamp(t - f32(i), 0.0, 1.0)));
            }
            let w0 = 1.0 - w1;
            let j = (i + 1u) % 6u;
            let dh = w0 * bnd[i * 3u] + w1 * bnd[j * 3u];
            let ds = w0 * bnd[i * 3u + 1u] + w1 * bnd[j * 3u + 1u];
            let dl = w0 * bnd[i * 3u + 2u] + w1 * bnd[j * 3u + 2u];
            if (dh != 0.0 || ds != 0.0) {
                var na = a;
                var nb = bb;
                if (dh != 0.0) {
                    let xr = dh * FRAC_PI_6;
                    let x2 = xr * xr;
                    let sn = xr * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))));
                    let cs = 1.0 - x2 / 2.0 * (1.0 - x2 / 12.0 * (1.0 - x2 / 30.0 * (1.0 - x2 / 56.0)));
                    na = a * cs - bb * sn;
                    nb = a * sn + bb * cs;
                }
                let scl = 1.0 + ds;
                na = na * scl;
                nb = nb * scl;
                let zo = vec3<f32>(a * (2.0 / 3.0), -a * (1.0 / 3.0) + bb * INV_SQRT3, -a * (1.0 / 3.0) - bb * INV_SQRT3);
                let zn = vec3<f32>(na * (2.0 / 3.0), -na * (1.0 / 3.0) + nb * INV_SQRT3, -na * (1.0 / 3.0) - nb * INV_SQRT3);
                var d = zn - zo;
                let dy = LUMA.x * d.x + LUMA.y * d.y + LUMA.z * d.z;
                d = d - vec3<f32>(dy);
                c = vec3<f32>(r + d.x, g + d.y, bl + d.z);
            }
            if (dl != 0.0) {
                let ya = abs(LUMA.x * r + LUMA.y * g + LUMA.z * bl);
                let rel = chroma / (ya + chroma);
                c = c * exp2(dl * sstep(0.0, 0.4, rel));
            }
        }
"#;

fn wgsl_for(kinds: &[OpKind], offsets: &[Option<usize>]) -> String {
    let slots: usize = kinds.iter().map(|k| k.slot_count()).sum::<usize>().max(1);
    let has_tables = offsets.iter().any(Option::is_some);
    let has_curve = kinds.contains(&OpKind::PointCurve);
    let mut s = String::new();
    let _ = write!(
        s,
        "@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<f32>, {slots}>;
",
    );
    if has_tables {
        s.push_str("@group(0) @binding(3) var<storage, read> T: array<f32>;\n");
    }
    let _ = write!(
        s,
        "
const LUMA = vec3<f32>({l0:?}, {l1:?}, {l2:?});
const SQRT3_2 = {sq:?};
const INV_SQRT3 = {isq:?};
const FRAC_PI_6 = {fp6:?};
const TAU = {tau:?};
const PI = {pi:?};
const INV_PI = {ipi:?};
const DEG = {deg:?};

fn sstep(e0: f32, e1: f32, x: f32) -> f32 {{
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}}

// Finite (not NaN, not infinite), decided on the bits so it does not depend on NaN comparisons.
fn fin(x: f32) -> bool {{
    return (bitcast<u32>(x) & 0x7f800000u) != 0x7f800000u;
}}

// The exponent mask 0x7f800000 comes from the uniform buffer of the op (the bit pattern of infinity) and
// is stored by the op before it uses these helpers. Shader compilers fold the constant-mask form of
// `fin` to `true` (they may assume there are no NaN / infinity); with a mask they cannot see, the test stays.
var<private> FM: u32 = 0u;

fn finm(x: f32) -> bool {{
    return (bitcast<u32>(x) & FM) != FM;
}}

fn isnanm(x: f32) -> bool {{
    return (bitcast<u32>(x) & 0x7fffffffu) > FM;
}}

// Rust's f32::max / f32::min: a NaN operand is ignored, so these do not depend on the driver's NaN rules.
fn fmax(a: f32, b: f32) -> f32 {{
    if (isnanm(a)) {{
        return b;
    }}
    if (isnanm(b)) {{
        return a;
    }}
    return select(b, a, a > b);
}}

fn fmin(a: f32, b: f32) -> f32 {{
    if (isnanm(a)) {{
        return b;
    }}
    if (isnanm(b)) {{
        return a;
    }}
    return select(b, a, a < b);
}}

fn fpos(x: f32) -> f32 {{
    return fmax(x, 0.0);
}}

fn cdl1(x: f32, s: f32, o: f32, p: f32) -> f32 {{
    let v = x * s + o;
    if (v > 0.0 && p != 1.0) {{
        return min(pow(v, p), 3.4028235e38);
    }}
    return v;
}}
",
        l0 = AP1_LUMA[0],
        l1 = AP1_LUMA[1],
        l2 = AP1_LUMA[2],
        sq = 0.866_025_4_f32,
        isq = 0.577_350_3_f32,
        fp6 = std::f32::consts::FRAC_PI_6,
        tau = std::f32::consts::TAU,
        pi = std::f32::consts::PI,
        ipi = std::f32::consts::FRAC_1_PI,
        deg = std::f32::consts::PI / 180.0,
    );
    if has_curve {
        let _ = write!(
            s,
            "
// The curve is evaluated near t = 1 where one ulp of t is a relative error of 1e-3 in the value, so the
// rounding sequence of the CPU is followed: correctly rounded division, and the product of the
// interpolation rounded before the add (`one` is 1.0 from the uniform buffer, so the compiler cannot
// fuse the product into the add).
fn divc(a: f32, b: f32) -> f32 {{
    let q = a / b;
    let r = fma(-q, b, a);
    return q + r / b;
}}

fn curve_lookup(t: f32, off: u32, one: f32) -> f32 {{
    let f = t * {n:?};
    let i = min(u32(f), {nm1}u);
    let a = T[off + i];
    let m = (T[off + i + 1u] - a) * (f - f32(i));
    return a + m * one;
}}

fn curve_to_v(t: f32, tmax: f32) -> f32 {{
    let t2 = min(t, tmax);
    return divc({grey:?} * t2, 1.0 - t2);
}}
",
            n = CURVE_N as f32,
            nm1 = CURVE_N - 1,
            grey = GREY,
        );
    }
    let _ = write!(
        s,
        "
@compute @workgroup_size({wg}, {wg}, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {{
    let dims = textureDimensions(src);
    if (gid.x >= dims.x || gid.y >= dims.y) {{
        return;
    }}
    let pos = vec2<i32>(gid.xy);
    let px = textureLoad(src, pos, 0);
    var c = px.rgb;
",
        wg = WORKGROUP,
    );
    let mut b = 0usize;
    for (k, kind) in kinds.iter().enumerate() {
        let sub = |t: &str| -> String {
            let mut out = t.replace("$T", &offsets[k].unwrap_or(0).to_string());
            for n in 0..5 {
                out = out.replace(&format!("${n}"), &(b + n).to_string());
            }
            out
        };
        match kind {
            OpKind::Gain => {
                let _ = writeln!(s, "    c = c * P[{b}].x;");
            }
            OpKind::Matrix => {
                let _ = writeln!(
                    s,
                    "    {{
        let r0 = P[{b}];
        let r1 = P[{b1}];
        let r2 = P[{b2}];
        c = vec3<f32>(
            r0.x * c.x + r0.y * c.y + r0.z * c.z,
            r1.x * c.x + r1.y * c.y + r1.z * c.z,
            r2.x * c.x + r2.y * c.y + r2.z * c.z);
    }}",
                    b1 = b + 1,
                    b2 = b + 2
                );
            }
            OpKind::Saturation => {
                let _ = writeln!(
                    s,
                    "    {{
        let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
        c = vec3<f32>(y) + (c - vec3<f32>(y)) * P[{b}].x;
    }}"
                );
            }
            OpKind::Contrast => {
                // CPU: amount == 0 is the exact identity; pixels with Y <= 0 are untouched.
                let _ = writeln!(
                    s,
                    "    if (P[{b}].x != 0.0) {{
        let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
        if (y > 0.0) {{
            let pivot = P[{b}].y;
            c = c * (pivot * pow(y / pivot, P[{b}].z) / y);
        }}
    }}"
                );
            }
            OpKind::HighlightsShadows => {
                let _ = writeln!(
                    s,
                    "    if (P[{b}].x != 0.0 || P[{b}].y != 0.0) {{
        let y = max(LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z, 0.0);
        let t = y / (y + 0.18);
        let w_h = sstep(0.5, 1.0, t);
        let w_s = 1.0 - sstep(0.0, 0.5, t);
        c = c * exp2(2.0 * P[{b}].x * w_h + 2.0 * P[{b}].y * w_s);
    }}"
                );
            }
            OpKind::Cdl => s.push_str(&sub(SNIPPET_CDL)),
            OpKind::ToneCurve => s.push_str(&sub(SNIPPET_TONE)),
            OpKind::PointCurve => s.push_str(&sub(SNIPPET_CURVE)),
            OpKind::Vignette => s.push_str(&sub(SNIPPET_VIGNETTE)),
            OpKind::WhitesBlacks => s.push_str(&sub(SNIPPET_WHITES_BLACKS)),
            OpKind::Vibrance => s.push_str(&sub(SNIPPET_VIBRANCE)),
            OpKind::HueSaturation => {
                let _ = writeln!(s, "    if (P[{}].z != 0.0) {{", b + 4);
                s.push_str("        var bnd: array<f32, 18>;\n");
                for f in 0..18 {
                    let _ = writeln!(s, "        bnd[{f}] = P[{}].{};", b + f / 4, ['x', 'y', 'z', 'w'][f % 4]);
                }
                s.push_str(SNIPPET_HUESAT_BODY);
                s.push_str("    }\n");
            }
        }
        b += kind.slot_count();
    }
    s.push_str("    textureStore(dst, pos, vec4<f32>(c, px.a));\n}\n");
    s
}

fn pipeline_for(ctx: &GpuContext, kinds: &[OpKind]) -> Result<Arc<PointPipeline>, GpuError> {
    let mut cache = ctx.point_cache.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(p) = cache.get(kinds) {
        return Ok(Arc::clone(p));
    }
    let mut table_offsets = Vec::with_capacity(kinds.len());
    let mut table_total = 0usize;
    for k in kinds {
        let len = k.table_len();
        table_offsets.push(if len > 0 { Some(table_total) } else { None });
        table_total += len;
    }
    let table_size = (table_total > 0).then_some(table_total as u64 * 4);
    let table_buf = match table_size {
        Some(size) => Some(ctx.create_buffer(
            "rawgraph tables",
            size,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        )?),
        None => None,
    };
    let slots: usize = kinds.iter().map(|k| k.slot_count()).sum::<usize>().max(1);
    let uniform_size = slots as u64 * 16;
    let pass = build_pass(
        ctx,
        "rawgraph point ops",
        &wgsl_for(kinds, &table_offsets),
        wgpu::TextureFormat::Rgba32Float,
        uniform_size,
        table_size,
    );
    let p = Arc::new(PointPipeline {
        pass,
        uniform_size,
        table_offsets,
        table_buf,
        table_keys: Mutex::new(vec![Vec::new(); kinds.len()]),
    });
    cache.insert(kinds.to_vec(), Arc::clone(&p));
    Ok(p)
}

/// Applies `ops` in order in a single pass. No clamping anywhere; alpha is copied.
///
/// The shader is built once per distinct sequence of op kinds; changing only the parameters
/// reuses the cached pipeline (see [`GpuContext::pipelines_created`]). Lookup tables (tone curve,
/// point curve) live in one buffer per pipeline that is rewritten only when the op's curve changes.
pub fn apply_point_ops(ctx: &GpuContext, src: &GpuImage, ops: &[PointOp]) -> Result<GpuImage, GpuError> {
    let (w, h) = src.size();
    check_size(ctx, w, h)?;
    let dst = create_texture(ctx, "rawgraph point result", w, h, wgpu::TextureFormat::Rgba32Float, FLOAT_USAGE)?;
    let dst = GpuImage::from_texture(dst, w, h);
    apply_point_ops_into(ctx, src, &dst, ops)?;
    Ok(dst)
}

/// Like [`apply_point_ops`] but writes into an existing image of the same size, so a slider
/// drag allocates nothing per frame. `src` and `dst` must be different images.
pub fn apply_point_ops_into(ctx: &GpuContext, src: &GpuImage, dst: &GpuImage, ops: &[PointOp]) -> Result<(), GpuError> {
    let (w, h) = src.size();
    check_size(ctx, w, h)?;
    if dst.size() != (w, h) {
        return Err(GpuError::SizeMismatch { expected: (w, h), actual: dst.size() });
    }
    if src.texture() == dst.texture() {
        return Err(GpuError::InvalidArgument("source and destination are the same image".into()));
    }
    let kinds: Vec<OpKind> = ops.iter().map(PointOp::kind).collect();
    let pipe = pipeline_for(ctx, &kinds)?;
    let mut params: Vec<[f32; 4]> = ops.iter().flat_map(PointOp::slots).collect();
    if params.is_empty() {
        params.push([0.0; 4]);
    }
    debug_assert_eq!(params.len() as u64 * 16, pipe.uniform_size);
    // Rewrite a lookup table only when its parameters changed. The lock is held until the pass is
    // submitted, so no other caller can replace a table between the write and the dispatch.
    let mut keys = pipe.table_keys.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(buf) = &pipe.table_buf {
        for (i, op) in ops.iter().enumerate() {
            let Some(offset) = pipe.table_offsets[i] else { continue };
            let key = op.table_key();
            if key != keys[i] {
                let data = op.table_data();
                if !data.is_empty() {
                    ctx.queue.write_buffer(buf, offset as u64 * 4, bytemuck::cast_slice(&data));
                }
                keys[i] = key;
            }
        }
    }
    run_pass(ctx, &pipe.pass, src.view(), dst.view(), bytemuck::cast_slice(&params), pipe.table_buf.as_ref(), (w, h))
}
