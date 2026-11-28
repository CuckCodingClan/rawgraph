//! WGSL of the neighbourhood passes. Every shader reads texture 0 (`src`), writes `dst`, has its
//! parameters in `P` (an array of `vec4<u32>`; floats travel as bit patterns) and, where noted, a second
//! read-only texture at binding 4.

/// Shared helpers: AP1 luminance, the finite test on bit patterns (see `point_ops` for why the
/// mask comes from the uniform buffer) and the CPU's smoothstep.
pub(crate) fn prelude() -> String {
    let l = rawgraph_core::color::AP1_LUMA;
    format!("\nconst LUMA = vec3<f32>({:?}, {:?}, {:?});\n{PRELUDE_BODY}", l[0], l[1], l[2])
}

const PRELUDE_BODY: &str = r#"
var<private> FM: u32 = 0u;

fn finm(x: f32) -> bool {
    return (bitcast<u32>(x) & FM) != FM;
}

fn sstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn pf(v: u32) -> f32 {
    return bitcast<f32>(v);
}
"#;

/// One separable 1-D convolution with clamped edges. P[0] = (axis 0 x / 1 y, radius, 0, 0);
/// P[1..=9] = up to 36 tap weights (tap `k` belongs to offset `k - radius`).
pub(crate) const CONV: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<u32>, 10>;

fn tap(i: u32) -> f32 {
    let v = P[1u + i / 4u];
    return bitcast<f32>(v[i % 4u]);
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = vec2<i32>(textureDimensions(src));
    if (i32(gid.x) >= dims.x || i32(gid.y) >= dims.y) {
        return;
    }
    let p = vec2<i32>(gid.xy);
    let r = i32(P[0].y);
    var acc = vec4<f32>(0.0);
    for (var k = 0; k <= 2 * r; k = k + 1) {
        var q = p;
        if (P[0].x == 0u) {
            q.x = clamp(p.x + k - r, 0, dims.x - 1);
        } else {
            q.y = clamp(p.y + k - r, 0, dims.y - 1);
        }
        acc = acc + tap(u32(k)) * textureLoad(src, q, 0);
    }
    textureStore(dst, p, acc);
}
"#;

/// Mean of every `scale` x `scale` block (blocks at the border may be smaller), like `box_downscale`
/// of the CPU. P[0] = (scale, 0, 0, 0). `dst` is the small image.
pub(crate) const DOWN: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<u32>, 1>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let sd = textureDimensions(src);
    let dd = textureDimensions(dst);
    if (gid.x >= dd.x || gid.y >= dd.y) {
        return;
    }
    let s = P[0].x;
    let x0 = gid.x * s;
    let x1 = min((gid.x + 1u) * s, sd.x);
    let y0 = gid.y * s;
    let y1 = min((gid.y + 1u) * s, sd.y);
    var acc = vec4<f32>(0.0);
    for (var y = y0; y < y1; y = y + 1u) {
        for (var x = x0; x < x1; x = x + 1u) {
            acc = acc + textureLoad(src, vec2<i32>(i32(x), i32(y)), 0);
        }
    }
    let inv = 1.0 / f32((x1 - x0) * (y1 - y0));
    textureStore(dst, vec2<i32>(gid.xy), acc * inv);
}
"#;

/// Brings a (possibly low-resolution) blurred image back to full size with the bilinear taps of the
/// CPU (`taps` / `LowRes::sample`) and, in mode 1, turns it into the high pass `0.5 + original - low`.
/// P[0] = (scale, mode, 0, 0); `src` is the low image, binding 4 the original, `dst` the full image.
pub(crate) const RESOLVE: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<u32>, 1>;
@group(0) @binding(4) var orig: texture_2d<f32>;

fn centre(i: u32, n: u32, scale: u32) -> f32 {
    let s = i * scale;
    let e = min((i + 1u) * scale, n);
    return f32(s + e - 1u) * 0.5;
}

// (first tap, second tap, weight of the second), as `taps` of the CPU.
fn taps(x: u32, n: u32, ln: u32, scale: u32) -> vec3<f32> {
    var i0 = min(u32(max((f32(x) - f32(scale - 1u) * 0.5) / f32(scale), 0.0)), ln - 1u);
    for (var k = 0; k < 2; k = k + 1) {
        if (i0 + 1u < ln && centre(i0 + 1u, n, scale) <= f32(x)) {
            i0 = i0 + 1u;
        }
    }
    for (var k = 0; k < 2; k = k + 1) {
        if (i0 > 0u && centre(i0, n, scale) > f32(x)) {
            i0 = i0 - 1u;
        }
    }
    let i1 = min(i0 + 1u, ln - 1u);
    let span = centre(i1, n, scale) - centre(i0, n, scale);
    var f = 0.0;
    if (span > 0.0) {
        f = clamp((f32(x) - centre(i0, n, scale)) / span, 0.0, 1.0);
    }
    return vec3<f32>(f32(i0), f32(i1), f);
}

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(dst);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let pos = vec2<i32>(gid.xy);
    let scale = P[0].x;
    var low = vec4<f32>(0.0);
    if (scale == 1u) {
        low = textureLoad(src, pos, 0);
    } else {
        let ld = textureDimensions(src);
        let tx = taps(gid.x, dims.x, ld.x, scale);
        let ty = taps(gid.y, dims.y, ld.y, scale);
        let a = textureLoad(src, vec2<i32>(i32(tx.x), i32(ty.x)), 0);
        let b = textureLoad(src, vec2<i32>(i32(tx.y), i32(ty.x)), 0);
        let c = textureLoad(src, vec2<i32>(i32(tx.x), i32(ty.y)), 0);
        let d = textureLoad(src, vec2<i32>(i32(tx.y), i32(ty.y)), 0);
        let top = a + (b - a) * tx.z;
        let bot = c + (d - c) * tx.z;
        low = top + (bot - top) * ty.z;
    }
    if (P[0].y == 1u) {
        let o = textureLoad(orig, pos, 0);
        textureStore(dst, pos, vec4<f32>(vec3<f32>(0.5) + o.rgb - low.rgb, o.a));
    } else {
        textureStore(dst, pos, low);
    }
}
"#;

/// Develop local tone, per-pixel part (`develop_local_tone_mapped` of the CPU, scalar controls).
/// P[0] = (highlights, shadows, whites, blacks); P[1] = (grid width, grid height, bits of infinity, 0);
/// binding 4 = the coefficient grid ([a_fine, b_fine, a_coarse, b_coarse] per texel).
pub(crate) const LOCAL_TONE: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<u32>, 2>;
@group(0) @binding(4) var grid: texture_2d<f32>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(src);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    FM = P[1].z;
    let pos = vec2<i32>(gid.xy);
    let px = textureLoad(src, pos, 0);
    let hi = pf(P[0].x);
    let sh = pf(P[0].y);
    let wh = pf(P[0].z);
    let bl = pf(P[0].w);
    var c = px.rgb;
    let yl = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
    if ((hi != 0.0 || sh != 0.0 || wh != 0.0 || bl != 0.0) && finm(yl) && yl > 0.0) {
        let gw = P[1].x;
        let gh = P[1].y;
        let px_ = max((f32(gid.x) + 0.5) * (f32(gw) / f32(dims.x)) - 0.5, 0.0);
        let py_ = max((f32(gid.y) + 0.5) * (f32(gh) / f32(dims.y)) - 0.5, 0.0);
        let x0 = min(u32(floor(px_)), gw - 1u);
        let x1 = min(x0 + 1u, gw - 1u);
        let fx = select(0.0, px_ - f32(x0), x1 > x0);
        let y0 = min(u32(floor(py_)), gh - 1u);
        let y1 = min(y0 + 1u, gh - 1u);
        let fy = select(0.0, py_ - f32(y0), y1 > y0);
        let c00 = textureLoad(grid, vec2<i32>(i32(x0), i32(y0)), 0);
        let c10 = textureLoad(grid, vec2<i32>(i32(x1), i32(y0)), 0);
        let c01 = textureLoad(grid, vec2<i32>(i32(x0), i32(y1)), 0);
        let c11 = textureLoad(grid, vec2<i32>(i32(x1), i32(y1)), 0);
        let top = c00 + (c10 - c00) * fx;
        let bot = c01 + (c11 - c01) * fx;
        let k = top + (bot - top) * fy;
        let l = log2(yl / 0.18 + 0.000244140625);
        let fine = k.x * l + k.y;
        let d = k.z * l + k.w - fine;
        let q = d / 0.6;
        let base = fine + d * exp(-0.5 * (q * q));
        let lights = sstep(0.0, 5.0, base);
        let darks = sstep(0.0, -5.0, base) * (1.0 - sstep(-5.0, -9.0, base));
        let e = 0.5 * (l + fine);
        let ev = 2.0 * (hi * lights + sh * darks) + wh * sstep(1.0, 5.0, e) + bl * sstep(-1.0, -5.0, e);
        if (ev != 0.0) {
            let gain = exp2(ev);
            c = c * gain;
            if (ev < 0.0) {
                let kk = exp2(0.06 * ev);
                let yo = yl * gain;
                c = vec3<f32>(yo) + (c - vec3<f32>(yo)) * kk;
            }
        }
    }
    textureStore(dst, pos, vec4<f32>(c, px.a));
}
"#;

/// `L = ln(max(Y, 0) + 0.02)` of every pixel in all four lanes (the sharpen guide plane).
pub(crate) const SHARPEN_LOG: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<u32>, 1>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(src);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let pos = vec2<i32>(gid.xy);
    let c = textureLoad(src, pos, 0).rgb;
    let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
    textureStore(dst, pos, vec4<f32>(log(max(y, 0.0) + 0.02)));
}
"#;

/// The gain of `rawgraph_fx::sharpen`. P[0] = (amount, threshold, 0.25 threshold, 0.75 threshold);
/// P[1].x = bits of infinity; `src` is the original, binding 4 the blurred log plane.
pub(crate) const SHARPEN_APPLY: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> P: array<vec4<u32>, 2>;
@group(0) @binding(4) var blurred: texture_2d<f32>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(src);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    FM = P[1].x;
    let pos = vec2<i32>(gid.xy);
    let px = textureLoad(src, pos, 0);
    var c = px.rgb;
    let amt = pf(P[0].x);
    let threshold = pf(P[0].y);
    let y = LUMA.x * c.x + LUMA.y * c.y + LUMA.z * c.z;
    if (amt != 0.0 && y > 0.0) {
        let l = log(max(y, 0.0) + 0.02);
        let b = textureLoad(blurred, pos, 0).x;
        let d = l - b;
        var wt = 1.0;
        if (threshold > 0.0) {
            wt = sstep(pf(P[0].z), pf(P[0].w), abs(d));
        }
        let scaled = 2.0 * d;
        let limited = 0.4 * scaled / (0.4 + abs(scaled));
        let gain = exp(amt * wt * limited);
        if (finm(gain)) {
            c = c * gain;
        }
    }
    textureStore(dst, pos, vec4<f32>(c, px.a));
}
"#;
