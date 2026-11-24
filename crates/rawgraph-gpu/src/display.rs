//! Display transform: exposure, ACEScg -> Rec.709, tone map, sRGB OETF, 8-bit quantisation.
//!
//! Mirrors `rawgraph_scopes::DisplayTransform::apply_with_rgba8` (the CPU reference). Differences
//! by design: the sRGB encoding is computed exactly per pixel instead of through the CPU's
//! 4096-entry lookup table, which can move a value across a rounding boundary by one code.

use rawgraph_core::color::{acescg_to_rec709, mul, to_f32};

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::image::{check_size, create_texture, GpuImage};
use crate::pass::{build_pass, run_pass, Pass};
use crate::point_ops::{apply_point_ops_into, PointOp};
use crate::readback::{read_texture, MAX_STAGING_BYTES};
use rawgraph_scopes::DisplayImage;

/// Highlight handling of the display transform (same meaning as `rawgraph_scopes::ToneMap`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuToneMap {
    /// No change; the 8-bit conversion clamps.
    Clip,
    /// Extended Reinhard per channel, white point 8.0, negatives set to 0.
    SoftHighlights,
    /// Stephen Hill's fit of the ACES 1.x RRT + sRGB ODT (not a certified ACES transform).
    AcesFit,
}

impl GpuToneMap {
    fn code(self) -> u32 {
        match self {
            GpuToneMap::Clip => 0,
            GpuToneMap::SoftHighlights => 1,
            GpuToneMap::AcesFit => 2,
        }
    }
}

/// Copy of the (private) input matrix of the CPU ACES fit.
const ACES_FIT_IN: [[f64; 3]; 3] = [
    [0.59719, 0.35458, 0.04823],
    [0.07600, 0.90834, 0.01566],
    [0.02840, 0.13383, 0.83777],
];

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct DisplayParams {
    m: [[f32; 4]; 3],
    gain: f32,
    tone: u32,
    /// `display_samples`: distance between the visited source pixels (0 and unused otherwise).
    step: u32,
    pad: u32,
}

pub(crate) struct DisplayPipeline {
    pass: Pass,
}

/// Shared part of both display shaders: parameters, helpers and `encode`, which maps one
/// scene-referred pixel to the UNQUANTISED, unclamped display-encoded value (what the CPU
/// `DisplayTransform::apply` produces).
const WGSL_COMMON: &str = "
@group(0) @binding(0) var src: texture_2d<f32>;

struct Params {
    m0: vec4<f32>,
    m1: vec4<f32>,
    m2: vec4<f32>,
    gain: f32,
    tone: u32,
    stride: u32,
    pad: u32,
}
@group(0) @binding(2) var<uniform> U: Params;

// Shader compilers may assume there are no NaNs, so test the bit pattern instead of x != x.
fn is_nan(v: f32) -> bool {
    return (bitcast<u32>(v) & 0x7fffffffu) > 0x7f800000u;
}

fn nan_to_zero(v: f32) -> f32 {
    return select(v, 0.0, is_nan(v));
}

fn srgb_oetf(x: f32) -> f32 {
    let a = abs(x);
    var r: f32;
    if (a <= 0.0031308) {
        r = 12.92 * a;
    } else {
        r = 1.055 * pow(a, 1.0 / 2.4) - 0.055;
    }
    return select(r, -r, x < 0.0);
}

fn soft_highlights(x: f32) -> f32 {
    let v = min(max(nan_to_zero(x), 0.0), 1.0e15);
    return v * (1.0 + v / 64.0) / (1.0 + v);
}

fn aces_curve(x: f32) -> f32 {
    let v = clamp(nan_to_zero(x), 0.0, 1.0e15);
    return (v * (v + 0.0245786) - 0.000090537) / (v * (0.983729 * v + 0.4329510) + 0.238081);
}

fn quantize(v: f32) -> f32 {
    let c = clamp(nan_to_zero(v), 0.0, 1.0);
    return floor(c * 255.0 + 0.5) / 255.0;
}

// Same as the CPU: NaN -> 0, then +-1e30 ceiling so the matrix products never form inf - inf.
fn scale_in(v: f32) -> f32 {
    return clamp(nan_to_zero(v * U.gain), -1.0e30, 1.0e30);
}

fn encode(pxin: vec4<f32>) -> vec3<f32> {
    let px = pxin;
    let rgb = vec3<f32>(scale_in(px.r), scale_in(px.g), scale_in(px.b));
    let v = vec3<f32>(
        U.m0.x * rgb.x + U.m0.y * rgb.y + U.m0.z * rgb.z,
        U.m1.x * rgb.x + U.m1.y * rgb.y + U.m1.z * rgb.z,
        U.m2.x * rgb.x + U.m2.y * rgb.y + U.m2.z * rgb.z);
    var e: vec3<f32>;
    if (U.tone == 2u) {
        let fr = aces_curve(v.x);
        let fg = aces_curve(v.y);
        let fb = aces_curve(v.z);
        let o = vec3<f32>(
            1.60475 * fr - 0.53108 * fg - 0.07367 * fb,
            -0.10208 * fr + 1.10813 * fg - 0.00605 * fb,
            -0.00327 * fr - 0.07276 * fg + 1.07602 * fb);
        let oc = clamp(o, vec3<f32>(0.0), vec3<f32>(1.0));
        e = vec3<f32>(srgb_oetf(oc.x), srgb_oetf(oc.y), srgb_oetf(oc.z));
    } else if (U.tone == 1u) {
        e = vec3<f32>(srgb_oetf(soft_highlights(v.x)), srgb_oetf(soft_highlights(v.y)), srgb_oetf(soft_highlights(v.z)));
    } else {
        e = vec3<f32>(srgb_oetf(v.x), srgb_oetf(v.y), srgb_oetf(v.z));
    }
    return e;
}
";

/// 8-bit variant: quantised, alpha 1, one thread per source pixel, written to `Rgba8Unorm`.
const WGSL_RGBA8: &str = "
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let dims = textureDimensions(src);
    if (gid.x >= dims.x || gid.y >= dims.y) {
        return;
    }
    let pos = vec2<i32>(gid.xy);
    let e = encode(textureLoad(src, pos, 0));
    textureStore(dst, pos, vec4<f32>(quantize(e.x), quantize(e.y), quantize(e.z), 1.0));
}
";

/// Float variant for the scopes: one thread per OUTPUT pixel, reads source pixel (x * step, y * step),
/// writes the unclamped encoded value to `Rgba32Float`.
const WGSL_SAMPLES: &str = "
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;

@compute @workgroup_size(16, 16, 1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let od = textureDimensions(dst);
    if (gid.x >= od.x || gid.y >= od.y) {
        return;
    }
    let sp = vec2<i32>(i32(gid.x * U.stride), i32(gid.y * U.stride));
    let e = encode(textureLoad(src, sp, 0));
    textureStore(dst, vec2<i32>(gid.xy), vec4<f32>(e, 1.0));
}
";

fn pipeline(ctx: &GpuContext) -> &DisplayPipeline {
    ctx.display_pipeline.get_or_init(|| DisplayPipeline {
        pass: build_pass(
            ctx,
            "rawgraph display",
            &format!("{WGSL_COMMON}{WGSL_RGBA8}"),
            wgpu::TextureFormat::Rgba8Unorm,
            std::mem::size_of::<DisplayParams>() as u64,
            None,
        ),
    })
}

fn float_pipeline(ctx: &GpuContext) -> &DisplayPipeline {
    ctx.display_float_pipeline.get_or_init(|| DisplayPipeline {
        pass: build_pass(
            ctx,
            "rawgraph display samples",
            &format!("{WGSL_COMMON}{WGSL_SAMPLES}"),
            wgpu::TextureFormat::Rgba32Float,
            std::mem::size_of::<DisplayParams>() as u64,
            None,
        ),
    })
}

fn params(exposure_ev: f32, tone: GpuToneMap) -> DisplayParams {
    let m709 = acescg_to_rec709();
    // Same as the CPU: for the ACES fit the input matrix is folded in once.
    let m = if tone == GpuToneMap::AcesFit { to_f32(&mul(&ACES_FIT_IN, &m709)) } else { to_f32(&m709) };
    DisplayParams {
        m: [
            [m[0][0], m[0][1], m[0][2], 0.0],
            [m[1][0], m[1][1], m[1][2], 0.0],
            [m[2][0], m[2][1], m[2][2], 0.0],
        ],
        gain: 2f32.powf(exposure_ev),
        tone: tone.code(),
        step: 0,
        pad: 0,
    }
}

const DISPLAY_TARGET_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::TEXTURE_BINDING
    .union(wgpu::TextureUsages::STORAGE_BINDING)
    .union(wgpu::TextureUsages::COPY_SRC);

/// Display transform into an `Rgba8Unorm` texture (already sRGB-encoded, alpha 1), for direct
/// presentation. The texture has `TEXTURE_BINDING | STORAGE_BINDING | COPY_SRC` usage.
pub fn display_texture(
    ctx: &GpuContext,
    src: &GpuImage,
    exposure_ev: f32,
    tone: GpuToneMap,
) -> Result<wgpu::Texture, GpuError> {
    let (w, h) = src.size();
    check_size(ctx, w, h)?;
    let dst = create_texture(ctx, "rawgraph display", w, h, wgpu::TextureFormat::Rgba8Unorm, DISPLAY_TARGET_USAGE)?;
    display_into(ctx, src, &dst, exposure_ev, tone)?;
    Ok(dst)
}

/// Like [`display_texture`] but renders into an existing texture, which must be `Rgba8Unorm`,
/// have `STORAGE_BINDING` usage and the size of `src` (for example the texture the UI already
/// shows, so nothing is allocated per frame).
pub fn display_into(
    ctx: &GpuContext,
    src: &GpuImage,
    dst: &wgpu::Texture,
    exposure_ev: f32,
    tone: GpuToneMap,
) -> Result<(), GpuError> {
    let (w, h) = src.size();
    check_size(ctx, w, h)?;
    if (dst.width(), dst.height()) != (w, h) {
        return Err(GpuError::SizeMismatch { expected: (w, h), actual: (dst.width(), dst.height()) });
    }
    let view = dst.create_view(&wgpu::TextureViewDescriptor::default());
    display_into_view(ctx, src.view(), &view, (w, h), exposure_ev, tone)
}

fn display_into_view(
    ctx: &GpuContext,
    src: &wgpu::TextureView,
    dst: &wgpu::TextureView,
    size: (u32, u32),
    exposure_ev: f32,
    tone: GpuToneMap,
) -> Result<(), GpuError> {
    let pipe = pipeline(ctx);
    let p = params(exposure_ev, tone);
    run_pass(ctx, &pipe.pass, src, dst, bytemuck::bytes_of(&p), None, size)
}

/// Display transform with readback: sRGB-encoded RGBA8, 4 bytes per pixel, alpha 255.
/// Identical in meaning to `DisplayTransform::apply` + `to_rgba8` (see the module note).
pub fn display_rgba8(ctx: &GpuContext, src: &GpuImage, exposure_ev: f32, tone: GpuToneMap) -> Result<Vec<u8>, GpuError> {
    let tex = display_texture(ctx, src, exposure_ev, tone)?;
    let (w, h) = src.size();
    let mut out = vec![0u8; w as usize * h as usize * 4];
    read_texture(ctx, &tex, w, h, 4, &mut out)?;
    Ok(out)
}

/// An 8-bit texture holding the display-encoded picture, ready to be shown in egui.
///
/// # Colour recipe for egui-wgpu 0.37 (read from its sources)
///
/// * The texture is `Rgba8Unorm`, the view is `Rgba8Unorm` (not `Rgba8UnormSrgb`), and the
///   pixels are the sRGB-ENCODED code values (what the CPU `to_rgba8` produces), alpha 255.
/// * `egui-wgpu-0.37.0/src/renderer.rs` lines 808 and 856: user textures "must have the texture
///   format `Rgba8Unorm`". Its own textures are created the same way (line 729, `Rgba8Unorm`,
///   `view_formats: [Rgba8Unorm]`) and hold gamma-encoded values.
/// * `egui-wgpu-0.37.0/src/egui.wgsl`, `fs_main_gamma_framebuffer` / `fs_main_linear_framebuffer`:
///   "We expect normal textures that are NOT sRGB-aware". The shader samples the texture,
///   multiplies it by the vertex colour (white for a plain image, so identity) and treats the
///   result as gamma values; only for an sRGB framebuffer does it convert to linear once at the
///   end (`linear_from_gamma_rgb`), to be re-encoded by the hardware. An `Rgba8UnormSrgb` view
///   would make the hardware decode the codes to linear BEFORE the shader, which then treats
///   them as gamma: the picture would be far too dark. So the stored encoded bytes appear as
///   the same code values on screen.
/// * Filtering happens on the encoded values, exactly as for every other egui image: `Nearest`
///   shows each code value untouched, `Linear` blends neighbours in gamma space.
/// * Usage is `TEXTURE_BINDING | STORAGE_BINDING | COPY_SRC` (storage for the display compute
///   pass, copy for [`DisplayTarget::read_rgba8`]); egui needs only the first.
///
/// Register once with `egui_wgpu::Renderer::register_native_texture(device, target.egui_view(),
/// wgpu::FilterMode::Linear)` (`Nearest` at 100 % zoom and above) and keep the returned
/// `TextureId`; the texture content changes in place, so re-registration is only needed when
/// the target is re-created at another size (use `update_egui_texture_from_wgpu_texture` with
/// the old id then). The egui bind group references the view, so keep the `DisplayTarget`
/// alive while the id is in use.
pub struct DisplayTarget {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

impl DisplayTarget {
    /// Allocates the target. Fails with `EmptyImage`/`TooLarge` or [`GpuError::OutOfMemory`];
    /// the context stays usable after an error.
    pub fn new(ctx: &GpuContext, width: u32, height: u32) -> Result<Self, GpuError> {
        let texture = create_texture(
            ctx,
            "rawgraph display target",
            width,
            height,
            wgpu::TextureFormat::Rgba8Unorm,
            DISPLAY_TARGET_USAGE,
        )?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Ok(Self { texture, view, width, height })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// The view to hand to `egui_wgpu::Renderer::register_native_texture` (format
    /// `Rgba8Unorm`, see the type documentation).
    pub fn egui_view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// Reads the target back as sRGB-encoded RGBA8 (tests, screenshots).
    pub fn read_rgba8(&self, ctx: &GpuContext) -> Result<Vec<u8>, GpuError> {
        let mut out = vec![0u8; self.width as usize * self.height as usize * 4];
        read_texture(ctx, &self.texture, self.width, self.height, 4, &mut out)?;
        Ok(out)
    }
}

/// Ops + display in one call into reusable targets; allocates no GPU memory (see
/// [`GpuContext::allocations_created`]). `scratch` receives the float result of the ops and must
/// be a different image of the size of `base`; `target` receives the display-encoded picture.
/// With an empty `ops` slice the op pass is skipped and the display reads `base` directly
/// (`scratch` is not touched).
pub fn render_into(
    ctx: &GpuContext,
    base: &GpuImage,
    scratch: &GpuImage,
    target: &DisplayTarget,
    ops: &[PointOp],
    exposure_ev: f32,
    tone: GpuToneMap,
) -> Result<(), GpuError> {
    let (w, h) = base.size();
    check_size(ctx, w, h)?;
    if target.size() != (w, h) {
        return Err(GpuError::SizeMismatch { expected: (w, h), actual: target.size() });
    }
    if ops.is_empty() {
        return display_into_view(ctx, base.view(), target.egui_view(), (w, h), exposure_ev, tone);
    }
    apply_point_ops_into(ctx, base, scratch, ops)?;
    display_into_view(ctx, scratch.view(), target.egui_view(), (w, h), exposure_ev, tone)
}

/// Reusable targets of [`display_samples`] for one output size.
pub(crate) struct SampleTargets {
    size: (u32, u32),
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// Single readback buffer; `None` when the picture is too big for one (banded fallback).
    staging: Option<wgpu::Buffer>,
    padded_row: u64,
}

impl SampleTargets {
    fn new(ctx: &GpuContext, (w, h): (u32, u32)) -> Result<Self, GpuError> {
        let texture = create_texture(
            ctx,
            "rawgraph display samples",
            w,
            h,
            wgpu::TextureFormat::Rgba32Float,
            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        )?;
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let padded_row = (u64::from(w) * 16).next_multiple_of(u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        let total = padded_row * u64::from(h);
        let staging = if total <= MAX_STAGING_BYTES.min(ctx.device.limits().max_buffer_size) {
            Some(ctx.create_buffer(
                "rawgraph samples readback",
                total,
                wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            )?)
        } else {
            None
        };
        Ok(Self { size: (w, h), texture, view, staging, padded_row })
    }
}

/// The display-encoded picture (the values of `rawgraph_scopes::DisplayTransform::apply`),
/// sampled at every `step`-th pixel in x and y starting at (0, 0): the pixels
/// `rawgraph_scopes::analyze` visits with `ScopeRequest.step = step` on the full picture.
/// The result is `ceil(w / step)` x `ceil(h / step)` and NOT clamped (values above 1.0 stay above
/// 1.0 for `Clip` and `SoftHighlights`); analyse it with `ScopeRequest.step = 1`.
///
/// `step` 0 counts as 1. The float target and the staging buffer are kept in the context and
/// reused while the output size stays the same. Differences to the CPU: the sRGB encoding is
/// exact instead of the CPU's 4096-entry table (at most ~1e-4 apart) and the GPU's `pow` and
/// fused multiply-adds add rounding noise of a few 1e-6 relative.
pub fn display_samples(
    ctx: &GpuContext,
    src: &GpuImage,
    exposure_ev: f32,
    tone: GpuToneMap,
    step: u32,
) -> Result<DisplayImage, GpuError> {
    let step = step.max(1);
    let (w, h) = src.size();
    check_size(ctx, w, h)?;
    let (ow, oh) = (w.div_ceil(step), h.div_ceil(step));
    let mut guard = ctx.sample_targets.lock().unwrap_or_else(|p| p.into_inner());
    if guard.as_ref().map(|t| t.size) != Some((ow, oh)) {
        // Free the old targets first so a size change never needs both at once.
        *guard = None;
        *guard = Some(SampleTargets::new(ctx, (ow, oh))?);
    }
    let Some(t) = guard.as_ref() else {
        return Err(GpuError::Readback("sample targets missing".into()));
    };
    let mut p = params(exposure_ev, tone);
    p.step = step;
    run_pass(ctx, &float_pipeline(ctx).pass, src.view(), &t.view, bytemuck::bytes_of(&p), None, (ow, oh))?;

    let mut rgb = Vec::with_capacity(ow as usize * oh as usize * 3);
    let result = match &t.staging {
        Some(staging) => read_samples(ctx, t, staging, &mut rgb),
        None => {
            let mut bytes = vec![0u8; ow as usize * oh as usize * 16];
            read_texture(ctx, &t.texture, ow, oh, 16, &mut bytes).map(|()| rgba_bytes_to_rgb(&bytes, &mut rgb))
        }
    };
    if let Err(e) = result {
        // A failed map leaves the staging buffer in an unknown state: start over next time.
        *guard = None;
        return Err(e);
    }
    Ok(DisplayImage { width: ow, height: oh, rgb })
}

/// Drops the alpha of tightly packed little-endian `Rgba32Float` texels.
fn rgba_bytes_to_rgb(bytes: &[u8], out: &mut Vec<f32>) {
    for px in bytes.as_chunks::<16>().0 {
        for c in 0..3 {
            out.push(f32::from_le_bytes([px[c * 4], px[c * 4 + 1], px[c * 4 + 2], px[c * 4 + 3]]));
        }
    }
}

fn read_samples(ctx: &GpuContext, t: &SampleTargets, staging: &wgpu::Buffer, rgb: &mut Vec<f32>) -> Result<(), GpuError> {
    let (w, h) = t.size;
    let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &t.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(t.padded_row as u32),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    ctx.queue.submit(std::iter::once(encoder.finish()));
    let slice = staging.slice(..t.padded_row * u64::from(h));
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    ctx.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| GpuError::Readback(e.to_string()))?;
    rx.recv()
        .map_err(|e| GpuError::Readback(e.to_string()))?
        .map_err(|e| GpuError::Readback(e.to_string()))?;
    {
        let mapped = slice.get_mapped_range().map_err(|e| GpuError::Readback(e.to_string()))?;
        let row_bytes = w as usize * 16;
        for r in 0..h as usize {
            let start = r * t.padded_row as usize;
            rgba_bytes_to_rgb(&mapped[start..start + row_bytes], rgb);
        }
    }
    staging.unmap();
    Ok(())
}
