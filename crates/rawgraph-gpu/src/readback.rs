use crate::context::GpuContext;
use crate::error::GpuError;

/// Staging buffers are capped at this size; larger images are read back in row bands.
pub(crate) const MAX_STAGING_BYTES: u64 = 256 * 1024 * 1024;

fn align_up(v: u64, a: u64) -> u64 {
    v.div_ceil(a) * a
}

/// Copies a whole 2D texture into `out` (tightly packed rows, `bytes_per_px` per texel).
/// The texture must have `COPY_SRC`.
pub(crate) fn read_texture(
    ctx: &GpuContext,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
    bytes_per_px: u32,
    out: &mut [u8],
) -> Result<(), GpuError> {
    let row_bytes = u64::from(width) * u64::from(bytes_per_px);
    debug_assert_eq!(out.len() as u64, row_bytes * u64::from(height));
    let padded = align_up(row_bytes, u64::from(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
    let cap = MAX_STAGING_BYTES.min(ctx.device.limits().max_buffer_size);
    let rows_per_band = (cap / padded).max(1).min(u64::from(height)) as u32;

    let staging = ctx.create_buffer(
        "rawgraph readback",
        padded * u64::from(rows_per_band),
        wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
    )?;

    let mut y = 0u32;
    while y < height {
        let rows = rows_per_band.min(height - y);
        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x: 0, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded as u32),
                    rows_per_image: Some(rows),
                },
            },
            wgpu::Extent3d { width, height: rows, depth_or_array_layers: 1 },
        );
        ctx.queue.submit(std::iter::once(encoder.finish()));

        let slice = staging.slice(..padded * u64::from(rows));
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(|e| GpuError::Readback(e.to_string()))?;
        rx.recv()
            .map_err(|e| GpuError::Readback(e.to_string()))?
            .map_err(|e| GpuError::Readback(e.to_string()))?;
        {
            let mapped = slice.get_mapped_range().map_err(|e| GpuError::Readback(e.to_string()))?;
            for r in 0..rows as usize {
                let src = &mapped[r * padded as usize..r * padded as usize + row_bytes as usize];
                let dst_start = (y as usize + r) * row_bytes as usize;
                out[dst_start..dst_start + row_bytes as usize].copy_from_slice(src);
            }
        }
        staging.unmap();
        y += rows;
    }
    Ok(())
}
