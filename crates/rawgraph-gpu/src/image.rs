use rawgraph_core::ImageBuf;

use crate::context::GpuContext;
use crate::error::GpuError;
use crate::readback::read_texture;

/// Largest slice handed to `Queue::write_texture` at once (bounds the driver staging memory).
const UPLOAD_BAND_BYTES: usize = 64 * 1024 * 1024;

/// `Rgba32Float` texture holding a scene-referred image. Values are never clamped.
pub struct GpuImage {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
}

pub(crate) const FLOAT_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::TEXTURE_BINDING
    .union(wgpu::TextureUsages::STORAGE_BINDING)
    .union(wgpu::TextureUsages::COPY_SRC)
    .union(wgpu::TextureUsages::COPY_DST);

/// Checks the size against the device limit; returns the documented errors.
pub(crate) fn check_size(ctx: &GpuContext, width: u32, height: u32) -> Result<(), GpuError> {
    if width == 0 || height == 0 {
        return Err(GpuError::EmptyImage { width, height });
    }
    let max_dimension = ctx.max_texture_dimension();
    if width > max_dimension || height > max_dimension {
        return Err(GpuError::TooLarge { width, height, max_dimension });
    }
    Ok(())
}

/// Creates a 2D texture, turning an allocation failure into an error instead of a panic.
pub(crate) fn create_texture(
    ctx: &GpuContext,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> Result<wgpu::Texture, GpuError> {
    check_size(ctx, width, height)?;
    let scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
    let texture = ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    });
    match pollster::block_on(scope.pop()) {
        Some(e) => Err(GpuError::OutOfMemory(e.to_string())),
        None => {
            ctx.count_allocation();
            Ok(texture)
        }
    }
}

impl GpuImage {
    /// Uploads a CPU image bit-exactly.
    pub fn upload(ctx: &GpuContext, img: &ImageBuf) -> Result<GpuImage, GpuError> {
        let (w, h) = (img.width(), img.height());
        let texture = create_texture(ctx, "rawgraph image", w, h, wgpu::TextureFormat::Rgba32Float, FLOAT_USAGE)?;
        let row_bytes = w as usize * 16;
        let rows_per_band = (UPLOAD_BAND_BYTES / row_bytes).max(1) as u32;
        let mut y = 0u32;
        while y < h {
            let rows = rows_per_band.min(h - y);
            let start = y as usize * w as usize * 4;
            let end = start + rows as usize * w as usize * 4;
            ctx.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&img.data()[start..end]),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes as u32),
                    rows_per_image: Some(rows),
                },
                wgpu::Extent3d { width: w, height: rows, depth_or_array_layers: 1 },
            );
            y += rows;
        }
        // Flush so the staging copies are scheduled now, not at the next unrelated submit.
        ctx.queue.submit(std::iter::empty());
        Ok(GpuImage::from_texture(texture, w, h))
    }

    /// An uninitialised (all-zero on every driver we know, but not guaranteed by this API)
    /// image to render into with [`crate::apply_point_ops_into`] / [`crate::render_into`].
    /// Fails with [`GpuError::OutOfMemory`] (the context stays usable) or `TooLarge`.
    pub fn new_empty(ctx: &GpuContext, width: u32, height: u32) -> Result<GpuImage, GpuError> {
        let texture = create_texture(ctx, "rawgraph image", width, height, wgpu::TextureFormat::Rgba32Float, FLOAT_USAGE)?;
        Ok(GpuImage::from_texture(texture, width, height))
    }

    /// Reads the image back to the CPU.
    pub fn download(&self, ctx: &GpuContext) -> Result<ImageBuf, GpuError> {
        let mut data = vec![0.0f32; self.width as usize * self.height as usize * 4];
        read_texture(ctx, &self.texture, self.width, self.height, 16, bytemuck::cast_slice_mut(&mut data))?;
        ImageBuf::from_rgba(self.width, self.height, data)
            .ok_or_else(|| GpuError::Readback("pixel count mismatch".into()))
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The underlying `Rgba32Float` texture.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Default view (`Rgba32Float`, whole texture), created once.
    pub(crate) fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    pub(crate) fn from_texture(texture: wgpu::Texture, width: u32, height: u32) -> Self {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Self { texture, view, width, height }
    }
}
