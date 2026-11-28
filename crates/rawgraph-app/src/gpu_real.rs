//! The real [`Backend`]: the `rawgraph-gpu` crate on the device eframe already owns, with the
//! display targets registered as native egui textures.

use std::sync::Arc;

use eframe::egui::TextureId;
use eframe::egui_wgpu::RenderState;
use eframe::wgpu;
use rawgraph_core::ImageBuf;
use rawgraph_gpu::{display_samples, render_into, DisplayTarget, GpuContext, GpuError, GpuImage, GpuToneMap, PointOp};
use rawgraph_scopes::DisplayImage;

use crate::gpu_path::GpuFailure;
use crate::gpu_session::Backend;

/// Everything the render worker needs to run the GPU path; built on the UI thread.
#[derive(Clone)]
pub struct GpuSetup {
    /// `None` when eframe did not create a wgpu render state.
    pub render_state: Option<RenderState>,
    /// A failure to inject (tests, `--gpu-fail`).
    pub inject: Option<GpuFailure>,
}

pub struct RealClass {
    scratch: GpuImage,
    target: DisplayTarget,
    id: Option<TextureId>,
}

pub struct RealBackend {
    ctx: Arc<GpuContext>,
    render_state: Option<RenderState>,
    inject: Option<GpuFailure>,
}

impl RealBackend {
    /// Shares eframe's device and queue. `None` when there is no render state.
    pub fn from_setup(setup: &GpuSetup) -> Option<Self> {
        let rs = setup.render_state.clone()?;
        let ctx = Arc::new(GpuContext::from_device(rs.device.clone(), rs.queue.clone()));
        Some(Self { ctx, render_state: Some(rs), inject: setup.inject })
    }

    /// A context without egui (tests); the textures stay unregistered.
    #[cfg(test)]
    pub fn headless(ctx: Arc<GpuContext>) -> Self {
        Self { ctx, render_state: None, inject: None }
    }

    pub fn summary(&self) -> String {
        self.ctx.adapter_summary()
    }
}

impl Backend for RealBackend {
    type Image = GpuImage;
    type Class = RealClass;

    fn limits_ok(&self, width: u32, height: u32) -> bool {
        self.ctx.limits_ok_for(width, height)
    }

    fn device_lost(&self) -> bool {
        self.inject == Some(GpuFailure::Lost) || self.ctx.is_device_lost()
    }

    fn upload(&mut self, image: &ImageBuf) -> Result<GpuImage, GpuError> {
        match self.inject {
            Some(GpuFailure::Oom) => return Err(GpuError::OutOfMemory("injected by --gpu-fail oom".into())),
            Some(GpuFailure::Other) => return Err(GpuError::Readback("injected by --gpu-fail other".into())),
            _ => {}
        }
        GpuImage::upload(&self.ctx, image)
    }

    fn new_class(&mut self, width: u32, height: u32) -> Result<RealClass, GpuError> {
        let scratch = GpuImage::new_empty(&self.ctx, width, height)?;
        let target = DisplayTarget::new(&self.ctx, width, height)?;
        // Magnified pixels stay crisp, reduced pictures are smooth: like the CPU texture.
        let id = self.render_state.as_ref().map(|rs| {
            rs.renderer.write().register_native_texture_with_sampler_options(
                &rs.device,
                target.egui_view(),
                wgpu::SamplerDescriptor {
                    label: Some("rawgraph preview"),
                    mag_filter: wgpu::FilterMode::Nearest,
                    min_filter: wgpu::FilterMode::Linear,
                    ..Default::default()
                },
            )
        });
        Ok(RealClass { scratch, target, id })
    }

    fn texture_id(&self, class: &RealClass) -> TextureId {
        class.id.unwrap_or(TextureId::User(u64::MAX))
    }

    fn release_class(&mut self, class: RealClass) {
        if let (Some(id), Some(rs)) = (class.id, &self.render_state) {
            rs.renderer.write().free_texture(&id);
        }
    }

    fn render(&mut self, base: &GpuImage, class: &RealClass, ops: &[PointOp], exposure_ev: f32, tone: GpuToneMap) -> Result<(), GpuError> {
        render_into(&self.ctx, base, &class.scratch, &class.target, ops, exposure_ev, tone)
    }

    fn samples(&mut self, base: &GpuImage, class: &RealClass, ops_empty: bool, exposure_ev: f32, tone: GpuToneMap, step: u32) -> Result<DisplayImage, GpuError> {
        let src = if ops_empty { base } else { &class.scratch };
        display_samples(&self.ctx, src, exposure_ev, tone, step)
    }

    fn read_rgba8(&mut self, class: &RealClass) -> Result<Vec<u8>, GpuError> {
        class.target.read_rgba8(&self.ctx)
    }

    fn wait_idle(&self) {
        self.ctx.wait_idle();
    }
}

/// Limits the preview needs beyond egui's defaults, never above what the adapter offers.
///
/// egui-wgpu asks for the wgpu defaults (256 MiB buffers, 8192 px textures). A 3840 px preview
/// needs a float texture of 3840x2560 and a readback buffer of 40 MB, which fits, but one
/// padded float row and the staging buffers of larger previews want room, so the texture
/// size and the buffer limits are raised as far as the adapter allows.
pub fn wanted_limits(adapter: &wgpu::Limits, gl_backend: bool) -> wgpu::Limits {
    let base = if gl_backend { wgpu::Limits::downlevel_webgl2_defaults() } else { wgpu::Limits::default() };
    // Never below egui's own request (as far as the adapter allows), never above the adapter.
    wgpu::Limits {
        max_texture_dimension_2d: adapter.max_texture_dimension_2d.min(16384).max(base.max_texture_dimension_2d.min(adapter.max_texture_dimension_2d)),
        max_buffer_size: adapter.max_buffer_size.min(1 << 30).max(base.max_buffer_size.min(adapter.max_buffer_size)),
        max_storage_buffer_binding_size: adapter.max_storage_buffer_binding_size.min(1 << 30).max(base.max_storage_buffer_binding_size.min(adapter.max_storage_buffer_binding_size)),
        ..base
    }
}

/// The device request eframe uses: egui's own settings with [`wanted_limits`].
pub fn device_descriptor(adapter: &wgpu::Adapter) -> wgpu::DeviceDescriptor<'static> {
    let gl = adapter.get_info().backend == wgpu::Backend::Gl;
    wgpu::DeviceDescriptor { label: Some("rawgraph wgpu device"), required_limits: wanted_limits(&adapter.limits(), gl), ..Default::default() }
}

/// One real-GPU test at a time in this test binary: several threads creating devices and
/// submitting work on Mesa's llvmpipe software adapter at once crash it (SIGSEGV), which
/// CI and the build host use. Same rule as `rawgraph-gpu/tests`.
#[cfg(test)]
pub(crate) fn gpu_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu_session::{Begin, DisplayArgs, GpuSession, Shown};
    use crate::gpu_path::{BaseKey, RendererMode};
    use rawgraph_core::ImageId;
    use rawgraph_engine::TailOp;

    #[test]
    fn wanted_limits_never_exceed_the_adapter() {
        let small = wgpu::Limits { max_texture_dimension_2d: 4096, max_buffer_size: 64 << 20, max_storage_buffer_binding_size: 64 << 20, ..wgpu::Limits::default() };
        let w = wanted_limits(&small, false);
        assert_eq!(w.max_texture_dimension_2d, 4096);
        assert_eq!(w.max_buffer_size, 64 << 20);
        let big = wgpu::Limits { max_texture_dimension_2d: 32768, max_buffer_size: 8 << 30, ..wgpu::Limits::default() };
        let w = wanted_limits(&big, false);
        assert_eq!(w.max_texture_dimension_2d, 16384);
        assert_eq!(w.max_buffer_size, 1 << 30);
    }

    fn headless() -> Option<Arc<GpuContext>> {
        match GpuContext::new() {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                eprintln!("note: no GPU adapter ({e}); the GPU comparison test is skipped");
                None
            }
        }
    }

    /// A synthetic scene-referred picture with values below 0 and far above 1.
    fn synthetic(w: u32, h: u32) -> ImageBuf {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32 / w as f32;
                let fy = y as f32 / h as f32;
                data.extend_from_slice(&[fx * fx * 6.0 - 0.05, 0.02 + fy * 1.4, (fx * 0.7 + fy * 0.3).powf(2.2) * 0.9, 1.0]);
            }
        }
        ImageBuf::from_rgba(w, h, data).expect("buffer size")
    }

    /// The real GPU frame equals the CPU frame within one code value, for each tone map.
    #[test]
    fn gpu_frame_equals_cpu_frame_for_each_tone_map() {
        let _gpu = super::gpu_test_lock();
        let Some(ctx) = headless() else { return };
        let base = synthetic(97, 61);
        let ops = [
            TailOp::Gain(2f32.powf(0.7)),
            TailOp::Saturation(1.3),
            TailOp::Contrast { amount: 0.3, pivot: 0.18 },
        ];
        let mut session = GpuSession::new(RealBackend::headless(ctx), RendererMode::Auto, None);
        let key = BaseKey { image: ImageId(1), fingerprint: 5, preview: 97, revision: 0 };
        for tone in [rawgraph_scopes::ToneMap::Clip, rawgraph_scopes::ToneMap::SoftHighlights, rawgraph_scopes::ToneMap::AcesFit] {
            let display = DisplayArgs { exposure_ev: 0.25, tone: crate::gpu_session::gpu_tone(tone) };
            match session.begin(&key, 1, 0) {
                Begin::Cpu { reason, .. } => panic!("unexpected CPU: {reason}"),
                Begin::HaveBase | Begin::NeedBase => {}
            }
            let shown = session.show(&key, Some(&base), &ops, display, 1, 0);
            assert!(matches!(shown, Shown::Gpu(_)), "{shown:?}");
            let gpu = session.read_current().expect("readback");
            let cpu_img = rawgraph_engine::apply_tail_cpu(&base, &ops);
            let t = rawgraph_scopes::DisplayTransform { exposure_ev: 0.25, tone_map: tone };
            let (_, cpu) = t.apply_with_rgba8(&cpu_img);
            assert_eq!(gpu.len(), cpu.len());
            let worst = gpu.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
            assert!(worst <= 1, "{tone:?}: largest difference {worst} code values");
        }
        assert_eq!(session.stats.uploads, 1, "all three tone maps used the same base");
    }

    /// Largest difference (code values) between the GPU frame and the CPU frame for `ops`, or
    /// `None` when there is no GPU adapter on this machine.
    fn worst_difference(ops: &[TailOp]) -> Option<u8> {
        let _gpu = super::gpu_test_lock();
        let ctx = headless()?;
        let base = synthetic(97, 61);
        let mut session = GpuSession::new(RealBackend::headless(ctx), RendererMode::Auto, None);
        let key = BaseKey { image: ImageId(1), fingerprint: 5, preview: 97, revision: 0 };
        let tone = rawgraph_scopes::ToneMap::AcesFit;
        let display = DisplayArgs { exposure_ev: 0.25, tone: crate::gpu_session::gpu_tone(tone) };
        assert!(!matches!(session.begin(&key, 1, 0), Begin::Cpu { .. }));
        let shown = session.show(&key, Some(&base), ops, display, 1, 0);
        assert!(matches!(shown, Shown::Gpu(_)), "{shown:?}");
        let gpu = session.read_current().expect("readback");
        let cpu_img = rawgraph_engine::apply_tail_cpu(&base, ops);
        let t = rawgraph_scopes::DisplayTransform { exposure_ev: 0.25, tone_map: tone };
        let (_, cpu) = t.apply_with_rgba8(&cpu_img);
        assert_eq!(gpu.len(), cpu.len());
        Some(gpu.iter().zip(&cpu).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0))
    }

    fn new_ops() -> Vec<(&'static str, TailOp)> {
        let mut bands = [[0.0f32; 3]; 6];
        bands[0] = [0.15, -0.35, 0.2];
        bands[4] = [-0.3, 0.5, -0.25];
        vec![
            ("cdl", TailOp::Cdl { slope: [2f32.powf(0.3), 1.0, 0.9], offset: [0.0, 0.0, 0.18 * 0.1], power: [2f32.powf(-0.2); 3] }),
            ("tone curve", TailOp::ToneCurve { shadows: 0.4, darks: -0.2, lights: 0.3, highlights: -0.5 }),
            ("point curve (luminance)", TailOp::PointCurve { points: vec![[0.0, 0.0], [0.3, 0.2], [0.65, 0.8], [1.0, 1.0]], channel: 0 }),
            ("point curve (green)", TailOp::PointCurve { points: vec![[0.0, 0.05], [0.5, 0.6], [1.0, 1.0]], channel: 2 }),
            ("hue / saturation", TailOp::HueSaturation { bands }),
            ("vignette", TailOp::Vignette { amount: -0.6, midpoint: 0.45, feather: 0.55 }),
        ]
    }

    #[test]
    fn each_new_tail_operation_gives_the_cpu_frame_on_the_gpu() {
        for (name, op) in new_ops() {
            match worst_difference(std::slice::from_ref(&op)) {
                Some(worst) => assert!(worst <= 1, "{name}: largest difference {worst} code values"),
                None => return,
            }
        }
    }

    #[test]
    fn all_operations_in_one_chain_give_the_cpu_frame_on_the_gpu() {
        let mut ops = vec![TailOp::Gain(2f32.powf(0.4)), TailOp::Saturation(1.2), TailOp::Contrast { amount: 0.2, pivot: 0.18 }];
        ops.extend(new_ops().into_iter().map(|(_, op)| op));
        ops.push(TailOp::HighlightsShadows { highlights: -0.3, shadows: 0.3 });
        ops.push(TailOp::Matrix([[0.3, 0.59, 0.11]; 3]));
        if let Some(worst) = worst_difference(&ops) {
            assert!(worst <= 1, "the combined chain differs by {worst} code values");
        }
    }
}
