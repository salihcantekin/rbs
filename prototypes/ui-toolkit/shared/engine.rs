// Shared by all prototypes through `include!`, so that each one compiles it
// against the `wgpu` major version its toolkit requires. A `wgpu` module must
// be in scope at the include site, plus a `bgl_ref!` macro that adapts a
// bind group layout reference to that version's pipeline layout field.

/// Stand-in for the RBS engine compositor: renders the program output into a
/// small pool of GPU textures. The UI only ever receives texture handles; no
/// CPU readback on the default path (P4).
pub struct Engine {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    pool: Vec<wgpu::Texture>,
    next: usize,
    /// Output size in pixels.
    pub size: (u32, u32),
    /// Frames rendered so far.
    pub frames: u64,
}

impl Engine {
    /// Create the engine on an existing device (shared with the UI toolkit).
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, size: (u32, u32), usage_extra: wgpu::TextureUsages) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("engine"),
            source: wgpu::ShaderSource::Wgsl(include_str!("engine.wgsl").into()),
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[bgl_ref!(&bgl)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("engine"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::TextureFormat::Rgba8Unorm.into())],
                compilation_options: Default::default(),
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            // The field was renamed between wgpu 27 and wgpu 30.
            #[cfg(feature = "wgpu27")]
            multiview: None,
            #[cfg(not(feature = "wgpu27"))]
            multiview_mask: Default::default(),
            cache: None,
        });
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &bgl,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() }],
        });
        let pool = (0..2)
            .map(|_| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("program"),
                    size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | usage_extra,
                    view_formats: &[],
                })
            })
            .collect();
        Engine { device: device.clone(), queue: queue.clone(), pipeline, uniform, bind_group, pool, next: 0, size, frames: 0 }
    }

    /// The texture rendered most recently, without rendering a new frame.
    pub fn render_last(&self) -> &wgpu::Texture {
        let n = self.pool.len();
        &self.pool[(self.next + n - 1) % n]
    }

    /// Render one program frame at time `t` seconds and return its texture.
    pub fn render(&mut self, t: f32) -> &wgpu::Texture {
        let data: [f32; 4] = [t, self.size.0 as f32, self.size.1 as f32, 0.0];
        let mut bytes = [0u8; 16];
        for (i, v) in data.iter().enumerate() {
            bytes[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        self.queue.write_buffer(&self.uniform, 0, &bytes);
        let tex = &self.pool[self.next];
        self.next = (self.next + 1) % self.pool.len();
        let view = tex.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("engine"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
        self.frames += 1;
        tex
    }
}
