//! Shared plumbing for the one-texture-in, one-texture-out compute passes.

use crate::context::GpuContext;
use crate::error::GpuError;

pub(crate) const WORKGROUP: u32 = 16;

/// A compiled pass: pipeline plus its explicit bind group layout.
pub(crate) struct Pass {
    pub pipeline: wgpu::ComputePipeline,
    pub layout: wgpu::BindGroupLayout,
}

/// Binding 0: source `Rgba32Float` texture (read with `textureLoad`, no filtering needed),
/// binding 1: write-only storage texture of `out_format`, binding 2: uniform buffer. With
/// `table_size` a read-only storage buffer of at least that many bytes is binding 3 (lookup tables).
pub(crate) fn build_pass(
    ctx: &GpuContext,
    label: &str,
    wgsl: &str,
    out_format: wgpu::TextureFormat,
    uniform_size: u64,
    table_size: Option<u64>,
) -> Pass {
    build_pass_ext(ctx, label, wgsl, out_format, uniform_size, table_size, 0)
}

/// Like [`build_pass`] with `extra_textures` more read-only `Rgba32Float` textures at bindings 4, 5, ...
/// (neighbourhood passes read a second image, e.g. the original next to its blurred copy).
pub(crate) fn build_pass_ext(
    ctx: &GpuContext,
    label: &str,
    wgsl: &str,
    out_format: wgpu::TextureFormat,
    uniform_size: u64,
    table_size: Option<u64>,
    extra_textures: u32,
) -> Pass {
    ctx.count_pipeline();
    let device = &ctx.device;
    let mut entries = vec![
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: out_format,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(uniform_size),
                },
                count: None,
            },
    ];
    if let Some(size) = table_size {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 3,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(size),
            },
            count: None,
        });
    }
    for i in 0..extra_textures {
        entries.push(wgpu::BindGroupLayoutEntry {
            binding: 4 + i,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        });
    }
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: Some(label), entries: &entries });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&layout)],
        immediate_size: 0,
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    Pass { pipeline, layout }
}

/// Records and submits one dispatch covering `width` x `height` threads. The uniform buffer comes
/// from the context's pool and goes back to it, so repeated calls allocate no GPU memory.
pub(crate) fn run_pass(
    ctx: &GpuContext,
    pass: &Pass,
    src: &wgpu::TextureView,
    dst: &wgpu::TextureView,
    uniform: &[u8],
    table: Option<&wgpu::Buffer>,
    size: (u32, u32),
) -> Result<(), GpuError> {
    run_pass_ext(ctx, pass, src, dst, uniform, table, &[], size)
}

/// Like [`run_pass`] with the extra textures of [`build_pass_ext`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pass_ext(
    ctx: &GpuContext,
    pass: &Pass,
    src: &wgpu::TextureView,
    dst: &wgpu::TextureView,
    uniform: &[u8],
    table: Option<&wgpu::Buffer>,
    extra: &[&wgpu::TextureView],
    (width, height): (u32, u32),
) -> Result<(), GpuError> {
    let device = &ctx.device;
    let ubuf_size = (uniform.len() as u64).next_multiple_of(16);
    let ubuf = ctx.take_uniform(ubuf_size)?;
    ctx.queue.write_buffer(&ubuf, 0, uniform);
    let mut entries = vec![
        wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
        wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(dst) },
        wgpu::BindGroupEntry { binding: 2, resource: ubuf.as_entire_binding() },
    ];
    if let Some(t) = table {
        entries.push(wgpu::BindGroupEntry { binding: 3, resource: t.as_entire_binding() });
    }
    for (i, view) in extra.iter().enumerate() {
        entries.push(wgpu::BindGroupEntry { binding: 4 + i as u32, resource: wgpu::BindingResource::TextureView(view) });
    }
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pass.layout, entries: &entries });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: None });
        cpass.set_pipeline(&pass.pipeline);
        cpass.set_bind_group(0, &bind_group, &[]);
        cpass.dispatch_workgroups(width.div_ceil(WORKGROUP), height.div_ceil(WORKGROUP), 1);
    }
    ctx.queue.submit(std::iter::once(encoder.finish()));
    // The write above is ordered before this submit; the next user of the buffer writes after it.
    ctx.give_back_uniform(ubuf_size, ubuf);
    Ok(())
}
