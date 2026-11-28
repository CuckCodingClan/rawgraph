use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::display::{DisplayPipeline, SampleTargets};
use crate::error::GpuError;
use crate::point_ops::{OpKind, PointPipeline};

/// Cached point-op pipelines by op-kind sequence, with the tick of their last use.
pub(crate) type PointCache = HashMap<Vec<OpKind>, (Arc<PointPipeline>, u64)>;

/// wgpu device + queue and the shader/pipeline caches.
pub struct GpuContext {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    adapter_info: Option<wgpu::AdapterInfo>,
    pub(crate) point_cache: Mutex<PointCache>,
    pub(crate) display_pipeline: OnceLock<DisplayPipeline>,
    pipelines_created: AtomicUsize,
    allocations_created: AtomicUsize,
    lost: Arc<AtomicBool>,
    /// Display shader variant that writes unquantised `Rgba32Float` (for `display_samples`).
    pub(crate) display_float_pipeline: OnceLock<DisplayPipeline>,
    /// Reusable targets of `display_samples` (one size at a time).
    pub(crate) sample_targets: Mutex<Option<SampleTargets>>,
    /// Uniform buffers waiting for the next pass, by size, so a slider drag allocates none.
    uniform_pool: Mutex<Vec<(u64, wgpu::Buffer)>>,
    /// Compiled neighbourhood passes (filter, local tone, sharpen), one per kind.
    pub(crate) neigh_cache: Mutex<HashMap<crate::chain::NeighKind, Arc<crate::pass::Pass>>>,
    /// Uploaded local tone coefficient grids by id, oldest first (a few small textures).
    pub(crate) grid_cache: Mutex<Vec<(u64, Arc<crate::image::GpuImage>)>>,
}

/// Most uniform buffers kept for reuse (one per distinct op-kind sequence in use).
const UNIFORM_POOL_MAX: usize = 16;

impl GpuContext {
    /// Headless context on the high-performance adapter. Returns an error (never panics)
    /// when no adapter or device is available.
    pub fn new() -> Result<Self, GpuError> {
        // `with_env` leaves the defaults alone unless WGPU_BACKEND, WGPU_DX12_COMPILER etc. are set.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle().with_env());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            ..Default::default()
        }))
        .map_err(|e| GpuError::NoAdapter(e.to_string()))?;
        // Ask for everything the adapter offers: large images need its max_buffer_size and
        // max_texture_dimension_2d, not the conservative defaults.
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rawgraph-gpu"),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .map_err(|e| GpuError::DeviceRequest(e.to_string()))?;
        let mut ctx = Self::from_device(device, queue);
        ctx.adapter_info = Some(adapter.get_info());
        Ok(ctx)
    }

    /// Wraps an existing device (for example the one eframe/egui-wgpu already owns).
    ///
    /// Installs a device-lost callback on the device (see [`GpuContext::is_device_lost`]); a
    /// callback set earlier on the same device is replaced. Check [`GpuContext::limits_ok_for`]
    /// before uploading an image: a shared device may run with the conservative default limits.
    pub fn from_device(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let lost = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&lost);
        device.set_device_lost_callback(move |_reason, _message| flag.store(true, Ordering::SeqCst));
        Self {
            device,
            queue,
            adapter_info: None,
            point_cache: Mutex::new(HashMap::new()),
            display_pipeline: OnceLock::new(),
            pipelines_created: AtomicUsize::new(0),
            allocations_created: AtomicUsize::new(0),
            lost,
            display_float_pipeline: OnceLock::new(),
            sample_targets: Mutex::new(None),
            uniform_pool: Mutex::new(Vec::new()),
            neigh_cache: Mutex::new(HashMap::new()),
            grid_cache: Mutex::new(Vec::new()),
        }
    }

    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Largest width/height a [`crate::GpuImage`] may have on this device.
    pub fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }

    /// Number of compute pipelines (shader compilations) created so far.
    pub fn pipelines_created(&self) -> usize {
        self.pipelines_created.load(Ordering::Relaxed)
    }

    pub(crate) fn count_pipeline(&self) {
        self.pipelines_created.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of GPU textures and buffers this crate has allocated so far (images, display
    /// targets, readback staging buffers, uniform buffers). A slider drag through
    /// [`crate::render_into`] must leave it unchanged after the first call. Bind groups and
    /// pipelines are not counted here (pipelines have [`GpuContext::pipelines_created`]).
    pub fn allocations_created(&self) -> usize {
        self.allocations_created.load(Ordering::Relaxed)
    }

    pub(crate) fn count_allocation(&self) {
        self.allocations_created.fetch_add(1, Ordering::Relaxed);
    }

    /// True once the device has been lost (driver reset, GPU removed, `destroy`). Every GPU
    /// call fails from then on; the application should fall back to the CPU for good. An
    /// [`crate::GpuError::OutOfMemory`] does NOT lose the device: smaller work still runs.
    pub fn is_device_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    /// Whether the device's limits allow an image of this size, including its readback:
    /// both dimensions within `max_texture_dimension_2d`, and one padded row of the float
    /// image (readback is banded in rows) within `max_buffer_size`.
    pub fn limits_ok_for(&self, width: u32, height: u32) -> bool {
        if width == 0 || height == 0 {
            return false;
        }
        let limits = self.device.limits();
        let row = (u64::from(width) * 16).next_multiple_of(u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        width <= limits.max_texture_dimension_2d
            && height <= limits.max_texture_dimension_2d
            && row <= limits.max_buffer_size
    }

    /// Creates a buffer, turning an allocation failure into [`GpuError::OutOfMemory`].
    pub(crate) fn create_buffer(
        &self,
        label: &str,
        size: u64,
        usage: wgpu::BufferUsages,
    ) -> Result<wgpu::Buffer, GpuError> {
        if size > self.device.limits().max_buffer_size {
            return Err(GpuError::OutOfMemory(format!(
                "buffer of {size} bytes exceeds the device limit of {}",
                self.device.limits().max_buffer_size
            )));
        }
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage,
            mapped_at_creation: false,
        });
        match pollster::block_on(scope.pop()) {
            Some(e) => Err(GpuError::OutOfMemory(e.to_string())),
            None => {
                self.count_allocation();
                Ok(buffer)
            }
        }
    }

    /// A uniform buffer of exactly `size` bytes, from the pool when one is free.
    pub(crate) fn take_uniform(&self, size: u64) -> Result<wgpu::Buffer, GpuError> {
        {
            let mut pool = self.uniform_pool.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(i) = pool.iter().position(|(s, _)| *s == size) {
                return Ok(pool.swap_remove(i).1);
            }
        }
        self.create_buffer("rawgraph params", size, wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST)
    }

    pub(crate) fn give_back_uniform(&self, size: u64, buffer: wgpu::Buffer) {
        let mut pool = self.uniform_pool.lock().unwrap_or_else(|p| p.into_inner());
        if pool.len() < UNIFORM_POOL_MAX {
            pool.push((size, buffer));
        }
    }

    /// Frees the uploaded local tone grids (memory pressure, device reset of the session).
    pub fn clear_grid_cache(&self) {
        self.grid_cache.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    /// Blocks until all submitted work has finished (benchmarks, tests).
    pub fn wait_idle(&self) {
        // A failed poll only means the device is lost; nothing to wait for then.
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
    }

    /// One line: adapter name, backend, max texture size, float-filtering support.
    pub fn adapter_summary(&self) -> String {
        let limits = self.device.limits();
        let filterable = self.device.features().contains(wgpu::Features::FLOAT32_FILTERABLE);
        let who = match &self.adapter_info {
            Some(i) => format!("{} ({:?}, {:?}, driver {})", i.name, i.backend, i.device_type, i.driver),
            None => "shared device (adapter unknown)".to_string(),
        };
        format!(
            "{who}; max texture {}x{}; max buffer {} MiB; float32 filtering: {}",
            limits.max_texture_dimension_2d,
            limits.max_texture_dimension_2d,
            limits.max_buffer_size / (1024 * 1024),
            if filterable { "yes" } else { "no (not needed: pointwise passes use textureLoad)" },
        )
    }
}
