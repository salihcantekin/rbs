//! The filter node: runs any [`FilterType`] on the render executor (spec
//! `video-filters.md`, "Filter node"; ADR-0062 decisions 4 and 5).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, Size, TextureId, TransferFunction, VideoFormat,
};
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::{GpuContext, GpuError, TextureDesc, TexturePool};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, Pin};
use rbs_graph::runtime::param::{self, ParamReceiver, ParamSender};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};

use crate::types::{FilterType, MAX_PASSES, PassSource, UNIFORM_VECS, Uniforms, Values};

/// Index of the "in" pin.
pub const IN_PIN: usize = 0;
/// Index of the "out" pin.
pub const OUT_PIN: usize = 0;
/// Output textures kept on the pool's free list by `warm`.
const OUTPUT_RING: usize = 2;
/// The prelude every pass is compiled with.
pub const PRELUDE: &str = include_str!("shaders/prelude.wgsl");

/// The descriptor of a filter node of `ty`: kind `VideoFilter`, video `in` and `out`.
pub fn descriptor(ty: &dyn FilterType) -> NodeDescriptor {
    NodeDescriptor::new(
        ty.type_id(),
        NodeKind::VideoFilter,
        vec![Pin::one("in", LinkKind::Video)],
        vec![Pin::one("out", LinkKind::Video)],
    )
}

/// What a filter node shares with the control thread. Atomics only.
#[derive(Debug, Default)]
pub struct FilterStats {
    /// `width << 32 | height` of the size its textures are for; 0 when unknown.
    size: AtomicU64,
    resizes: AtomicU64,
    frames: AtomicU64,
}

fn pack(size: [u32; 2]) -> u64 {
    (u64::from(size[0]) << 32) | u64::from(size[1])
}

impl FilterStats {
    /// The size its textures are for.
    pub fn warm_size(&self) -> Option<[u32; 2]> {
        let v = self.size.load(Ordering::Acquire);
        let size = [(v >> 32) as u32, v as u32];
        (size[0] > 0 && size[1] > 0).then_some(size)
    }

    /// Size changes handled in `process` (each allocated textures of the new size).
    pub fn resizes(&self) -> u64 {
        self.resizes.load(Ordering::Relaxed)
    }

    /// Frames published.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// GPU objects made in `prepare`.
struct Prepared {
    pipelines: Vec<wgpu::RenderPipeline>,
    buffers: Vec<wgpu::Buffer>,
    groups: Vec<wgpu::BindGroup>,
}

/// Runs one filter type between its "in" and "out" pins.
pub struct FilterNode {
    ty: Arc<dyn FilterType>,
    descriptor: NodeDescriptor,
    values: ParamReceiver<Values>,
    stats: Arc<FilterStats>,
    gpu: Option<Prepared>,
    /// The size `warm` acquired textures for.
    warmed: Option<[u32; 2]>,
    /// What the uniform buffers hold: values, input size, decode flag.
    written: Option<(Values, [u32; 2], bool)>,
    /// The input frame (texture, time) behind the frame published last.
    published: Option<(TextureId, Timestamp)>,
    /// The output texture published last (still read by the output slot).
    last_output: Option<TextureId>,
    uniforms: [Uniforms; MAX_PASSES],
}

/// The description of each pass's target for an input of `size`.
fn targets_of(passes: &[crate::types::Pass], size: [u32; 2]) -> [Option<TextureDesc>; MAX_PASSES] {
    let mut out = [None; MAX_PASSES];
    for (i, (slot, pass)) in out.iter_mut().zip(passes).enumerate() {
        let s = if i + 1 == passes.len() {
            size
        } else {
            pass.target.of(size)
        };
        *slot = Some(TextureDesc::canvas(s[0], s[1]));
    }
    out
}

/// Gives back the textures this node kept for `size`: the output it published last
/// (dropped when its readers let go), its free spare outputs and its intermediates
/// (ADR-0062 decisions 5 and 7). Does not allocate.
fn give_back(
    pool: &mut TexturePool,
    passes: &[crate::types::Pass],
    size: [u32; 2],
    last_output: Option<TextureId>,
) {
    let last = passes.len().saturating_sub(1);
    for (i, desc) in targets_of(passes, size).iter().enumerate() {
        let Some(desc) = desc else { continue };
        let mut count = kept(i, passes.len());
        if i == last {
            if let Some(id) = last_output {
                pool.forget_texture(id);
                count -= 1;
            }
        }
        pool.forget(desc, count);
    }
}

/// How many textures of each target this node keeps: the output ring for the last
/// pass, one for each intermediate.
fn kept(i: usize, passes: usize) -> usize {
    if i + 1 == passes { OUTPUT_RING } else { 1 }
}

impl FilterNode {
    /// A node of `ty` starting with `values`, warmed at `size` when it is known. The
    /// sender replaces the values while the node runs (latest wins).
    pub fn new(
        ty: Arc<dyn FilterType>,
        values: Values,
        size: Option<[u32; 2]>,
    ) -> (Self, ParamSender<Values>, Arc<FilterStats>) {
        let (sender, receiver) = param::channel(values);
        let stats = Arc::new(FilterStats::default());
        if let Some(s) = size.filter(|s| s[0] > 0 && s[1] > 0) {
            stats.size.store(pack(s), Ordering::Release);
        }
        let node = Self {
            descriptor: descriptor(ty.as_ref()),
            ty,
            values: receiver,
            stats: Arc::clone(&stats),
            gpu: None,
            warmed: None,
            written: None,
            published: None,
            last_output: None,
            uniforms: [Uniforms::default(); MAX_PASSES],
        };
        (node, sender, stats)
    }

    /// The filter type.
    pub fn filter_type(&self) -> &Arc<dyn FilterType> {
        &self.ty
    }
}

fn backend(e: GpuError) -> NodeError {
    NodeError::Backend(match e {
        GpuError::PoolFull { .. } => "texture pool full",
        GpuError::StaleTexture(_) | GpuError::UnknownTexture(_) => "stale texture",
        _ => "gpu error",
    })
}

fn pipeline(
    ctx: &GpuContext,
    layout: &wgpu::PipelineLayout,
    source: &str,
) -> Result<wgpu::RenderPipeline, NodeError> {
    let device = ctx.device();
    // Shader errors are reported through the device's error scope rather than a panic.
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("rbs-filters pass"),
        source: wgpu::ShaderSource::Wgsl(format!("{PRELUDE}\n{source}").into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("rbs-filters pass"),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: rbs_gpu::CANVAS_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    match pollster::block_on(scope.pop()) {
        None => Ok(pipeline),
        Some(e) => Err(NodeError::Failed(format!("filter shader: {e}"))),
    }
}

impl Node<GpuRuntime> for FilterNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared;
        let device = gpu.device();
        let passes = self.ty.passes();
        if passes.is_empty() || passes.len() > MAX_PASSES {
            return Err(NodeError::Permanent("filter type has no usable passes"));
        }
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-filters uniforms"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(size_of::<Uniforms>() as u64),
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-filters"),
            bind_group_layouts: &[Some(&uniform_layout), Some(gpu.sampled_layout())],
            immediate_size: 0,
        });
        let mut prepared = Prepared {
            pipelines: Vec::with_capacity(passes.len()),
            buffers: Vec::with_capacity(passes.len()),
            groups: Vec::with_capacity(passes.len()),
        };
        for pass in passes {
            prepared
                .pipelines
                .push(pipeline(gpu, &layout, pass.shader)?);
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rbs-filters uniforms"),
                size: size_of::<Uniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            prepared
                .groups
                .push(device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("rbs-filters uniforms"),
                    layout: &uniform_layout,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                }));
            prepared.buffers.push(buffer);
        }
        self.gpu = Some(prepared);
        self.written = None;
        self.published = None;
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        self.warmed = None;
        let Some(size) = self.stats.warm_size() else {
            return Ok(());
        };
        let pool = &mut ctx.local.pool;
        let mut held: [Option<TextureId>; OUTPUT_RING + MAX_PASSES] =
            [None; OUTPUT_RING + MAX_PASSES];
        let result = (|| {
            for slot in held.iter_mut().take(OUTPUT_RING) {
                *slot = Some(pool.acquire(ctx.shared, &TextureDesc::canvas(size[0], size[1]))?);
            }
            let passes = self.ty.passes();
            let last = passes.len().saturating_sub(1);
            for (i, pass) in passes.iter().enumerate().take(last) {
                let s = pass.target.of(size);
                held[OUTPUT_RING + i] =
                    Some(pool.acquire(ctx.shared, &TextureDesc::canvas(s[0], s[1]))?);
            }
            Ok::<(), GpuError>(())
        })();
        for id in held.into_iter().flatten() {
            pool.release(id).map_err(backend)?;
        }
        result.map_err(backend)?;
        self.warmed = Some(size);
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        // Leaving the schedule (disabled, removed, or a failed warm): give back what
        // this node kept, so a size the compositor also uses does not keep it alive.
        if let Some(size) = self.warmed.take() {
            give_back(
                &mut ctx.local.pool,
                self.ty.passes(),
                size,
                self.last_output.take(),
            );
        }
        self.last_output = None;
        self.published = None;
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared();
        let changed = self.values.update();
        let prepared = self.gpu.as_ref().ok_or(NodeError::NotReady)?;
        let (inputs, outputs, local) = ctx.parts();

        // What the input offers; nothing usable publishes nothing.
        let input = inputs.video(IN_PIN, 0).and_then(|frame| {
            let FramePayload::Texture(id) = *frame.payload() else {
                return None;
            };
            let tex = local.pool.get(id)?;
            tex.sampled_bind_group()?;
            let d = tex.desc();
            let decode =
                frame.format().color.transfer == TransferFunction::Srgb && !d.format.is_srgb();
            Some((id, [d.width, d.height], decode, frame.timestamp()))
        });
        let Some((input_id, size, decode, time)) = input else {
            self.published = None;
            drop(outputs.set_video(OUT_PIN, None)?);
            return Ok(());
        };
        let passes = self.ty.passes();
        if self.warmed != Some(size) {
            // A new size: `render` acquires textures of it below, which the pool
            // creates on this frame only (ADR-0062 decision 5). The old size's
            // textures are given back now, so a window being resized costs one size's
            // worth of memory, not one per size it passed through.
            if let Some(old) = self.warmed {
                give_back(&mut local.pool, passes, old, self.last_output.take());
            }
            self.warmed = Some(size);
            self.stats.size.store(pack(size), Ordering::Release);
            self.stats.resizes.fetch_add(1, Ordering::Relaxed);
        }

        let values = *self.values.get();
        let state = (values, size, decode);
        // The same input frame and settings: the output slot still holds the frame
        // published for them (video slots keep their frame), so skip the passes and
        // keep this size's textures from turning idle.
        if !changed && self.written == Some(state) && self.published == Some((input_id, time)) {
            for desc in targets_of(passes, size).iter().flatten() {
                local.pool.touch(desc);
            }
            return Ok(());
        }
        if changed || self.written != Some(state) {
            let count = passes.len();
            for u in &mut self.uniforms[..count] {
                *u = Uniforms::default();
            }
            self.ty.uniforms(&values, size, &mut self.uniforms[..count]);
            let mut source = size;
            for (i, pass) in passes.iter().enumerate() {
                if pass.source == PassSource::Previous && i > 0 {
                    source = passes[i - 1].target.of(size);
                }
                let first = pass.source == PassSource::Input;
                self.uniforms[i].0[UNIFORM_VECS - 1] = [
                    1.0 / source[0] as f32,
                    1.0 / source[1] as f32,
                    0.0,
                    if first && decode { 1.0 } else { 0.0 },
                ];
                gpu.queue().write_buffer(
                    &prepared.buffers[i],
                    0,
                    bytemuck::bytes_of(&self.uniforms[i]),
                );
            }
            self.written = Some(state);
        }

        // Targets: intermediates for every pass but the last, then the output.
        let mut targets: [Option<TextureId>; MAX_PASSES] = [None; MAX_PASSES];
        let result = render(
            gpu,
            &mut local.pool,
            prepared,
            passes,
            size,
            input_id,
            &mut targets,
        );
        let output = targets[passes.len() - 1];
        let frame = match (result, output) {
            (Ok(()), Some(out)) => {
                let format = VideoFormat::new(
                    Size::new(size[0], size[1]),
                    PixelFormat::Rgba16Float,
                    ColorSpace::LINEAR_BT709,
                );
                Some(local.pool.frame(out, format, time))
            }
            (Err(e), _) => Some(Err(e)),
            (Ok(()), None) => None,
        };
        // The frame's lease keeps the output alive; the pool reuses the rest at once.
        for id in targets.into_iter().flatten() {
            local.pool.release(id).map_err(backend)?;
        }
        match frame {
            Some(Ok(frame)) => {
                self.stats.frames.fetch_add(1, Ordering::Relaxed);
                self.published = Some((input_id, time));
                self.last_output = output;
                drop(outputs.set_video(OUT_PIN, Some(frame))?);
                Ok(())
            }
            Some(Err(e)) => {
                self.published = None;
                drop(outputs.set_video(OUT_PIN, None)?);
                Err(backend(e))
            }
            None => {
                self.published = None;
                drop(outputs.set_video(OUT_PIN, None)?);
                Ok(())
            }
        }
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.gpu = None;
        self.warmed = None;
        self.written = None;
        self.published = None;
    }
}

/// Acquires the pass targets into `targets` and records and submits every pass.
fn render(
    gpu: &GpuContext,
    pool: &mut TexturePool,
    prepared: &Prepared,
    passes: &[crate::types::Pass],
    size: [u32; 2],
    input: TextureId,
    targets: &mut [Option<TextureId>; MAX_PASSES],
) -> Result<(), GpuError> {
    for (i, pass) in passes.iter().enumerate() {
        let s = if i + 1 == passes.len() {
            size
        } else {
            pass.target.of(size)
        };
        targets[i] = Some(pool.acquire(gpu, &TextureDesc::canvas(s[0], s[1]))?);
    }
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rbs-filters"),
        });
    for (i, pass) in passes.iter().enumerate() {
        let source = match pass.source {
            PassSource::Input => input,
            PassSource::Previous => {
                targets[i.saturating_sub(1)].ok_or(GpuError::StaleTexture(input))?
            }
        };
        let source = pool
            .get(source)
            .and_then(|t| t.sampled_bind_group())
            .ok_or(GpuError::StaleTexture(source))?;
        let target_id = targets[i].ok_or(GpuError::StaleTexture(input))?;
        let target = pool
            .get(target_id)
            .ok_or(GpuError::StaleTexture(target_id))?;
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rbs-filters pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target.view(),
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(&prepared.pipelines[i]);
        rp.set_bind_group(0, &prepared.groups[i], &[]);
        rp.set_bind_group(1, source, &[]);
        rp.draw(0..3, 0..1);
    }
    gpu.queue().submit(Some(encoder.finish()));
    Ok(())
}
