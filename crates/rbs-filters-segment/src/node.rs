//! The background removal node (ADR-0179; spec `person-mask-port.md`, "Filter").
//!
//! Each tick with a new input picture the node draws it at the session's small size
//! (sRGB, BGRA) on the GPU, copies that into one of a ring of staging buffers and maps
//! it without waiting. A finished readback becomes a small system frame lent to the
//! session; the newest matte the session has finished is uploaded to a one-channel
//! texture; the composite pass multiplies the full-size picture by the refined matte.
//! Nothing waits: a ring that is full or a session that is busy drops the new picture,
//! and the picture is drawn with the matte of an earlier one (ADR-0179 decision 4). Until
//! the first matte exists the node publishes nothing, so the unfiltered picture is
//! never shown.
//!
//! The only readback is the small picture (named exception of ADR-0179 decision 3).
//! The full-size picture never leaves the GPU.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use bytes::BytesMut;
use rbs_core::media::{
    ColorSpace, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, TextureId, VideoFormat,
    VideoFrame, next_generation,
};
use rbs_core::time::Timestamp;
use rbs_filters::PRELUDE;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::yuv_decode::YuvToRgb;
use rbs_gpu::{GpuContext, GpuError, TextureDesc, TexturePool, Transfer, readback};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::{self, ParamReceiver, ParamSender};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};
use rbs_params::Values;
use rbs_segment::{Matte, MattePlane, PersonMaskSession, SegmentError, SubmitOutcome};

use crate::{EDGE_SHIFT, FEATHER, SMOOTHING, THRESHOLD, TYPE_ID, edges};

/// Index of the "in" pin.
pub const IN_PIN: usize = 0;
/// Index of the "out" pin.
pub const OUT_PIN: usize = 0;
/// Staging buffers in flight between the GPU and the session. The session keeps one
/// picture pending at most, so three leave room for a slow map.
pub const RING: usize = 3;
/// Output textures kept by the node (the one read downstream and the one drawn next).
const OUTPUT_RING: usize = 2;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

const DOWNSCALE: &str = include_str!("shaders/downscale.wgsl");
const COMPOSITE: &str = include_str!("shaders/composite.wgsl");
/// The small picture's format: `Bgra8Unorm` is BGRA in memory, which is what a backend
/// takes as `PixelFormat::Bgra8`.
const SMALL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const MATTE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Opens a session. Called on the control thread each time the node enters the schedule
/// (`warm`), so a disabled filter holds no model and no worker (P13).
pub type SessionFactory =
    Box<dyn FnMut() -> Result<Box<dyn PersonMaskSession>, SegmentError> + Send>;

/// The descriptor: kind `VideoFilter`, video `in` and `out`.
pub fn descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(TYPE_ID),
        NodeKind::VideoFilter,
        vec![Pin::one("in", LinkKind::Video)],
        vec![Pin::one("out", LinkKind::Video)],
    )
}

/// What the node shares with the control thread. Atomics only.
#[derive(Debug, Default)]
pub struct RemovalStats {
    /// Pictures published.
    frames: AtomicU64,
    /// Small pictures read back and lent to the session.
    submitted: AtomicU64,
    /// Pictures the session dropped because it was busy.
    dropped_busy: AtomicU64,
    /// Pictures not drawn small because every staging buffer was in flight.
    ring_full: AtomicU64,
    /// Mattes uploaded.
    mattes: AtomicU64,
    /// Mattes the node could not use (wrong size, GPU matte).
    bad_mattes: AtomicU64,
    /// Readbacks that failed.
    failed: AtomicU64,
    /// Times the session was lost.
    lost: AtomicU64,
}

impl RemovalStats {
    /// Pictures published.
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
    /// Small pictures lent to the session.
    pub fn submitted(&self) -> u64 {
        self.submitted.load(Ordering::Relaxed)
    }
    /// Pictures the session dropped because it was busy.
    pub fn dropped_busy(&self) -> u64 {
        self.dropped_busy.load(Ordering::Relaxed)
    }
    /// Pictures skipped because the staging ring was full.
    pub fn ring_full(&self) -> u64 {
        self.ring_full.load(Ordering::Relaxed)
    }
    /// Mattes uploaded.
    pub fn mattes(&self) -> u64 {
        self.mattes.load(Ordering::Relaxed)
    }
    /// Mattes that could not be used.
    pub fn bad_mattes(&self) -> u64 {
        self.bad_mattes.load(Ordering::Relaxed)
    }
    /// Readbacks that failed.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
    /// Times the session reported it could no longer produce mattes.
    pub fn lost(&self) -> u64 {
        self.lost.load(Ordering::Relaxed)
    }
}

/// One staging buffer. `state` is written by the map callback too.
struct Slot {
    buffer: wgpu::Buffer,
    state: Arc<AtomicU8>,
}

/// What the render thread knows of the picture in a staging buffer: its order in the
/// ring and the input's timestamp.
#[derive(Debug, Clone, Copy, Default)]
struct Meta {
    seq: u64,
    time: Timestamp,
}

/// GPU objects made in `prepare`.
struct Prepared {
    down: wgpu::RenderPipeline,
    comp: wgpu::RenderPipeline,
    down_buffer: wgpu::Buffer,
    down_group: wgpu::BindGroup,
    comp_buffer: wgpu::Buffer,
    comp_group: wgpu::BindGroup,
    slots: Vec<Slot>,
    /// Bytes per row of the staging buffers.
    stride: u32,
}

/// What the uniform buffers hold.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Written {
    edges: [f32; 2],
    size: [u32; 2],
    decode: Transfer,
}

/// What the last published picture was made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drawn {
    input: TextureId,
    time: Timestamp,
    matte_generation: u64,
}

/// Cuts the person out of its input with the matte of a person mask session.
pub struct BackgroundRemovalNode {
    descriptor: NodeDescriptor,
    values: ParamReceiver<Values>,
    stats: Arc<RemovalStats>,
    factory: SessionFactory,
    session: Option<Box<dyn PersonMaskSession>>,
    /// The small picture's size: what the session's level asks for.
    small: [u32; 2],
    gpu: Option<Prepared>,
    /// The small render target, kept for the node's life in the schedule.
    small_texture: Option<TextureId>,
    /// The matte texture and its size; made when the first matte arrives.
    matte: Option<(TextureId, [u32; 2])>,
    /// The smoothed matte as uploaded, and how many mattes made it.
    smooth: Vec<u8>,
    matte_generation: u64,
    /// The input size the pictures are for.
    size: Option<[u32; 2]>,
    down_written: Option<([u32; 2], Transfer)>,
    written: Option<Written>,
    /// The input picture last queued for readback.
    submitted: Option<(TextureId, Timestamp)>,
    drawn: Option<Drawn>,
    last_output: Option<TextureId>,
    seq: u64,
    meta: [Meta; RING],
    /// The buffer the small frame is built in; given back by the session's drop.
    pixels: Option<BytesMut>,
    rgb: YuvToRgb,
}

impl BackgroundRemovalNode {
    /// A node whose session comes from `factory`, with the session's small picture size
    /// `small` and the first `values`. The sender replaces the values while the node
    /// runs (latest wins).
    pub fn new(
        factory: SessionFactory,
        small: [u32; 2],
        values: Values,
    ) -> (Self, ParamSender<Values>, Arc<RemovalStats>) {
        let (sender, receiver) = param::channel(values);
        let stats = Arc::new(RemovalStats::default());
        let node = Self {
            descriptor: descriptor(),
            values: receiver,
            stats: Arc::clone(&stats),
            factory,
            session: None,
            small: [small[0].max(1), small[1].max(1)],
            gpu: None,
            small_texture: None,
            matte: None,
            smooth: Vec::new(),
            matte_generation: 0,
            size: None,
            down_written: None,
            written: None,
            submitted: None,
            drawn: None,
            last_output: None,
            seq: 0,
            meta: [Meta::default(); RING],
            pixels: None,
            rgb: YuvToRgb::new(),
        };
        (node, sender, stats)
    }

    fn small_desc(&self) -> TextureDesc {
        TextureDesc {
            width: self.small[0],
            height: self.small[1],
            format: SMALL_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        }
    }
}

fn backend(e: GpuError) -> NodeError {
    NodeError::Backend(match e {
        GpuError::PoolFull { .. } => "texture pool full",
        GpuError::StaleTexture(_) | GpuError::UnknownTexture(_) => "stale texture",
        _ => "gpu error",
    })
}

fn matte_desc(size: [u32; 2]) -> TextureDesc {
    TextureDesc::source(size[0], size[1], MATTE_FORMAT)
}

fn pipeline(
    ctx: &GpuContext,
    layout: &wgpu::PipelineLayout,
    source: &str,
    format: wgpu::TextureFormat,
) -> Result<wgpu::RenderPipeline, NodeError> {
    let device = ctx.device();
    // Shader errors are reported through the device's error scope rather than a panic.
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("rbs-filters-segment pass"),
        source: wgpu::ShaderSource::Wgsl(format!("{PRELUDE}\n{source}").into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("rbs-filters-segment pass"),
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
                format,
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
        Some(e) => Err(NodeError::Failed(format!("background removal shader: {e}"))),
    }
}

fn uniform_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("rbs-filters-segment uniforms"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: wgpu::BufferSize::new(UNIFORM_BYTES),
            },
            count: None,
        }],
    })
}

/// 16 `vec4<f32>`, as every filter pass.
const UNIFORM_BYTES: u64 = 256;
type UniformBlock = [[f32; 4]; 16];

fn uniform(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
) -> (wgpu::Buffer, wgpu::BindGroup) {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("rbs-filters-segment uniforms"),
        size: UNIFORM_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("rbs-filters-segment uniforms"),
        layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    (buffer, group)
}

/// Gives back the output textures of `size` this node kept.
fn give_back(pool: &mut TexturePool, size: [u32; 2], last_output: Option<TextureId>) {
    let mut count = OUTPUT_RING;
    if let Some(id) = last_output {
        pool.forget_texture(id);
        count -= 1;
    }
    pool.forget(&TextureDesc::canvas(size[0], size[1]), count);
}

/// A linear-light, premultiplied pass: input at group 1, matte at group 2 when given.
fn draw(
    encoder: &mut wgpu::CommandEncoder,
    pipeline: &wgpu::RenderPipeline,
    target: &wgpu::TextureView,
    uniforms: &wgpu::BindGroup,
    input: &wgpu::BindGroup,
    matte: Option<&wgpu::BindGroup>,
) {
    let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("rbs-filters-segment pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: target,
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
    rp.set_pipeline(pipeline);
    rp.set_bind_group(0, uniforms, &[]);
    rp.set_bind_group(1, input, &[]);
    if let Some(matte) = matte {
        rp.set_bind_group(2, matte, &[]);
    }
    rp.draw(0..3, 0..1);
}

impl Node<GpuRuntime> for BackgroundRemovalNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared;
        let device = gpu.device();
        let uniforms = uniform_layout(device);
        let sampled = gpu.sampled_layout();
        let down_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-filters-segment downscale"),
            bind_group_layouts: &[Some(&uniforms), Some(sampled)],
            immediate_size: 0,
        });
        let comp_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-filters-segment composite"),
            bind_group_layouts: &[Some(&uniforms), Some(sampled), Some(sampled)],
            immediate_size: 0,
        });
        let down = pipeline(gpu, &down_layout, DOWNSCALE, SMALL_FORMAT)?;
        let comp = pipeline(gpu, &comp_layout, COMPOSITE, rbs_gpu::CANVAS_FORMAT)?;
        let (down_buffer, down_group) = uniform(device, &uniforms);
        let (comp_buffer, comp_group) = uniform(device, &uniforms);
        let stride = (self.small[0] * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let slots = (0..RING)
            .map(|_| Slot {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rbs-filters-segment staging"),
                    size: u64::from(stride) * u64::from(self.small[1]),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
            })
            .collect();
        self.gpu = Some(Prepared {
            down,
            comp,
            down_buffer,
            down_group,
            comp_buffer,
            comp_group,
            slots,
            stride,
        });
        self.down_written = None;
        self.written = None;
        self.drawn = None;
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Control thread: the only place a model is loaded and a worker started (P13).
        if self.session.is_none() {
            let mut session = (self.factory)()
                .map_err(|e| NodeError::Failed(format!("person mask session: {e}")))?;
            session
                .warm()
                .map_err(|e| NodeError::Failed(format!("person mask session: {e}")))?;
            self.session = Some(session);
        }
        if self.small_texture.is_none() {
            let id = ctx
                .local
                .pool
                .acquire(ctx.shared, &self.small_desc())
                .map_err(backend)?;
            self.small_texture = Some(id);
        }
        // The small frame's buffer, once.
        if self.pixels.is_none() {
            self.pixels = Some(BytesMut::with_capacity(
                self.small[0] as usize * self.small[1] as usize * 4,
            ));
        }
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        // Leaving the schedule: give back every texture and stop the session, so a
        // disabled filter holds no memory, no model and no worker (P13).
        let pool = &mut ctx.local.pool;
        if let Some(id) = self.small_texture.take() {
            let _ = pool.release(id);
            pool.forget_texture(id);
        }
        if let Some((id, _)) = self.matte.take() {
            let _ = pool.release(id);
            pool.forget_texture(id);
        }
        if let Some(size) = self.size.take() {
            give_back(pool, size, self.last_output.take());
        }
        self.last_output = None;
        self.drawn = None;
        self.submitted = None;
        self.smooth = Vec::new();
        self.pixels = None;
        self.rgb.release();
        if let Some(session) = self.session.take() {
            session.close();
        }
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared();
        let changed = self.values.update();
        let values = *self.values.get();
        let small_id = self.small_texture.ok_or(NodeError::NotReady)?;
        let (inputs, outputs, local) = ctx.parts();

        // What the input offers; a 4:2:0 input is decoded to RGB first, once per picture.
        let mut frame = inputs.video(IN_PIN, 0);
        if let Some(f) = frame
            && YuvToRgb::needs_decode(&local.pool, f)
        {
            frame = self.rgb.rgb(gpu, &mut local.pool, f).ok();
        }
        let input = frame.and_then(|frame| {
            let FramePayload::Texture(id) = *frame.payload() else {
                return None;
            };
            let tex = local.pool.get(id)?;
            tex.sampled_bind_group()?;
            let d = tex.desc();
            Some((
                id,
                [d.width, d.height],
                Transfer::of(frame.format(), d.format),
                frame.timestamp(),
            ))
        });
        let Some((input_id, size, decode, time)) = input else {
            self.drawn = None;
            drop(outputs.set_video(OUT_PIN, None)?);
            return Ok(());
        };
        let prepared = self.gpu.as_ref().ok_or(NodeError::NotReady)?;
        let session = self.session.as_mut().ok_or(NodeError::NotReady)?;
        if self.size != Some(size) {
            // A new picture size (spec rule 5): the old size's textures go back at once.
            // The matte has no size of its own, so it stays in use while the aspect is
            // the same; a different aspect starts the session and the matte over.
            if let Some(old) = self.size {
                give_back(&mut local.pool, old, self.last_output.take());
                if aspect_changed(old, size) {
                    session.reset();
                    self.matte_generation = 0;
                    self.smooth.clear();
                    if let Some((id, _)) = self.matte.take() {
                        let _ = local.pool.release(id);
                        local.pool.forget_texture(id);
                    }
                }
            }
            self.size = Some(size);
            self.submitted = None;
            self.drawn = None;
        }

        // 1. A finished readback becomes a small frame for the session.
        deliver(
            prepared,
            session.as_mut(),
            self.small,
            &self.meta,
            &mut self.pixels,
            &self.stats,
        );

        // 2. A new picture is drawn small and read back, when a staging buffer is free.
        if self.submitted != Some((input_id, time)) {
            let free = prepared
                .slots
                .iter()
                .position(|s| s.state.load(Ordering::Acquire) == IDLE);
            match free {
                None => {
                    self.stats.ring_full.fetch_add(1, Ordering::Relaxed);
                }
                Some(index) => {
                    if self.down_written != Some((size, decode)) {
                        let mut u: UniformBlock = [[0.0; 4]; 16];
                        u[0] = [
                            1.0 / self.small[0] as f32,
                            1.0 / self.small[1] as f32,
                            0.0,
                            0.0,
                        ];
                        u[15] = [
                            1.0 / size[0] as f32,
                            1.0 / size[1] as f32,
                            0.0,
                            decode.code(),
                        ];
                        gpu.queue()
                            .write_buffer(&prepared.down_buffer, 0, bytemuck::bytes_of(&u));
                        self.down_written = Some((size, decode));
                    }
                    let queued = queue_readback(
                        gpu,
                        &local.pool,
                        prepared,
                        index,
                        input_id,
                        small_id,
                        self.small,
                    );
                    if queued.is_ok() {
                        self.seq += 1;
                        self.meta[index] = Meta {
                            seq: self.seq,
                            time,
                        };
                        self.submitted = Some((input_id, time));
                    } else {
                        self.stats.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // 3. The newest finished matte, smoothed and uploaded.
        match session.latest() {
            Ok(Some(matte)) => {
                if take_matte(
                    gpu,
                    &mut local.pool,
                    matte,
                    values.number(SMOOTHING),
                    &mut self.smooth,
                    &mut self.matte,
                    &self.stats,
                ) {
                    self.matte_generation += 1;
                }
            }
            Ok(None) => {}
            Err(_) => {
                self.stats.lost.fetch_add(1, Ordering::Relaxed);
                self.drawn = None;
                drop(outputs.set_video(OUT_PIN, None)?);
                return Err(NodeError::Backend("person mask session lost"));
            }
        }

        // 4. Nothing is published before the first matte (ADR-0179 decision 4).
        let Some((matte_id, _)) = self.matte else {
            self.drawn = None;
            drop(outputs.set_video(OUT_PIN, None)?);
            return Ok(());
        };
        let edge = edges(
            values.number(THRESHOLD),
            values.number(FEATHER),
            values.number(EDGE_SHIFT),
        );
        let state = Written {
            edges: edge,
            size,
            decode,
        };
        let now = Drawn {
            input: input_id,
            time,
            matte_generation: self.matte_generation,
        };
        if !changed && self.written == Some(state) && self.drawn == Some(now) {
            // The same picture, matte and settings: the output slot still holds the frame.
            local.pool.touch(&TextureDesc::canvas(size[0], size[1]));
            return Ok(());
        }
        if self.written != Some(state) {
            write_composite(gpu, prepared, edge, size, decode);
            self.written = Some(state);
        }
        let result = render_output(
            gpu,
            &mut local.pool,
            prepared,
            (input_id, matte_id),
            size,
            time,
        )?;
        match result {
            Ok((output, frame)) => {
                let frame = frame.with_sequence(next_generation());
                self.stats.frames.fetch_add(1, Ordering::Relaxed);
                self.drawn = Some(now);
                self.last_output = Some(output);
                drop(outputs.set_video(OUT_PIN, Some(frame))?);
                Ok(())
            }
            Err(e) => {
                self.drawn = None;
                drop(outputs.set_video(OUT_PIN, None)?);
                Err(backend(e))
            }
        }
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.gpu = None;
        self.down_written = None;
        self.written = None;
        self.drawn = None;
        self.submitted = None;
        if let Some(session) = self.session.take() {
            session.close();
        }
    }
}

/// Draws the input at the small size and starts the copy and the map of staging buffer
/// `index`. Never waits.
fn queue_readback(
    gpu: &GpuContext,
    pool: &TexturePool,
    prepared: &Prepared,
    index: usize,
    input: TextureId,
    small: TextureId,
    size: [u32; 2],
) -> Result<(), GpuError> {
    let src = pool
        .get(input)
        .and_then(|t| t.sampled_bind_group())
        .ok_or(GpuError::StaleTexture(input))?;
    let target = pool.get(small).ok_or(GpuError::StaleTexture(small))?;
    let slot = &prepared.slots[index];
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rbs-filters-segment readback"),
        });
    draw(
        &mut encoder,
        &prepared.down,
        target.view(),
        &prepared.down_group,
        src,
        None,
    );
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: target.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &slot.buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(prepared.stride),
                rows_per_image: Some(size[1]),
            },
        },
        wgpu::Extent3d {
            width: size[0],
            height: size[1],
            depth_or_array_layers: 1,
        },
    );
    gpu.queue().submit(Some(encoder.finish()));
    slot.state.store(MAPPING, Ordering::Release);
    let state = Arc::clone(&slot.state);
    slot.buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            state.store(
                if result.is_ok() { MAPPED } else { FAILED },
                Ordering::Release,
            );
        });
    Ok(())
}

/// Lends the session the newest finished readback as a small system frame; older
/// finished ones are dropped. The frame's bytes come back to the node's buffer when the
/// session did not keep them.
fn deliver(
    prepared: &Prepared,
    session: &mut dyn PersonMaskSession,
    small: [u32; 2],
    meta: &[Meta; RING],
    pixels: &mut Option<BytesMut>,
    stats: &RemovalStats,
) {
    let mut newest: Option<usize> = None;
    for (i, slot) in prepared.slots.iter().enumerate() {
        match slot.state.load(Ordering::Acquire) {
            MAPPED if newest.is_none_or(|n| meta[i].seq > meta[n].seq) => newest = Some(i),
            FAILED => {
                stats.failed.fetch_add(1, Ordering::Relaxed);
                slot.state.store(IDLE, Ordering::Release);
            }
            _ => {}
        }
    }
    let Some(newest) = newest else { return };
    let row = small[0] as usize * 4;
    for (i, slot) in prepared.slots.iter().enumerate() {
        if slot.state.load(Ordering::Acquire) != MAPPED {
            continue;
        }
        if i == newest {
            let mut bytes = pixels.take().unwrap_or_default();
            bytes.clear();
            if let Ok(view) = slot.buffer.slice(..).get_mapped_range() {
                for y in 0..small[1] as usize {
                    let start = y * prepared.stride as usize;
                    bytes.extend_from_slice(&view[start..start + row]);
                }
            }
            let bytes = bytes.freeze();
            let format = VideoFormat::new(
                Size::new(small[0], small[1]),
                PixelFormat::Bgra8,
                ColorSpace::SRGB,
            );
            let planes = [PlaneLayout {
                offset: 0,
                stride: row,
            }];
            match SystemFrame::new(format, bytes.clone(), &planes) {
                Ok(system) => {
                    let frame = VideoFrame::system(meta[i].time, system, None);
                    match session.submit(&frame) {
                        SubmitOutcome::Accepted => {
                            stats.submitted.fetch_add(1, Ordering::Relaxed);
                        }
                        _ => {
                            stats.dropped_busy.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                Err(_) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Back to the node's buffer when the session did not keep a reference.
            *pixels = bytes.try_into_mut().ok();
        }
        slot.buffer.unmap();
        slot.state.store(IDLE, Ordering::Release);
    }
}

/// Smooths a CPU matte into `smooth` and uploads it to the matte texture (made, or
/// remade, when the matte's size changed: once per session and level). Returns whether
/// the texture holds a new matte.
fn take_matte(
    gpu: &GpuContext,
    pool: &mut TexturePool,
    matte: Matte,
    smoothing: f32,
    smooth: &mut Vec<u8>,
    texture: &mut Option<(TextureId, [u32; 2])>,
    stats: &RemovalStats,
) -> bool {
    let size = [matte.size.width, matte.size.height];
    let pixels = match &matte.plane {
        MattePlane::Cpu(pixels)
            if size[0] > 0
                && size[1] > 0
                && pixels.len() == size[0] as usize * size[1] as usize =>
        {
            Arc::clone(pixels)
        }
        other => {
            // A GPU matte is not read yet; the session may reuse its texture.
            if let MattePlane::Gpu(gpu_matte) = other {
                gpu_matte.release.release();
            }
            stats.bad_mattes.fetch_add(1, Ordering::Relaxed);
            return false;
        }
    };
    if texture.map(|(_, s)| s) != Some(size) {
        if let Some((old, _)) = texture.take() {
            let _ = pool.release(old);
            pool.forget_texture(old);
        }
        match pool.acquire(gpu, &matte_desc(size)) {
            Ok(id) => *texture = Some((id, size)),
            Err(_) => {
                stats.bad_mattes.fetch_add(1, Ordering::Relaxed);
                return false;
            }
        }
        smooth.clear();
    }
    let Some((id, _)) = *texture else {
        return false;
    };
    if smooth.len() == pixels.len() {
        // Each new matte keeps `smoothing` of the matte before it, in 1/256.
        let keep = (smoothing.clamp(0.0, 0.95) * 256.0) as u32;
        for (old, new) in smooth.iter_mut().zip(pixels.iter()) {
            *old = ((u32::from(*old) * keep + u32::from(*new) * (256 - keep)) >> 8) as u8;
        }
    } else {
        smooth.clear();
        smooth.extend_from_slice(&pixels);
    }
    if readback::upload(gpu, pool, id, smooth).is_err() {
        stats.bad_mattes.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    stats.mattes.fetch_add(1, Ordering::Relaxed);
    true
}

/// Draws the input times the matte into `output`.
fn composite(
    gpu: &GpuContext,
    pool: &TexturePool,
    prepared: &Prepared,
    input: TextureId,
    matte: TextureId,
    output: TextureId,
) -> Result<(), GpuError> {
    let src = pool
        .get(input)
        .and_then(|t| t.sampled_bind_group())
        .ok_or(GpuError::StaleTexture(input))?;
    let mask = pool
        .get(matte)
        .and_then(|t| t.sampled_bind_group())
        .ok_or(GpuError::StaleTexture(matte))?;
    let target = pool.get(output).ok_or(GpuError::StaleTexture(output))?;
    let mut encoder = gpu
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("rbs-filters-segment composite"),
        });
    draw(
        &mut encoder,
        &prepared.comp,
        target.view(),
        &prepared.comp_group,
        src,
        Some(mask),
    );
    gpu.queue().submit(Some(encoder.finish()));
    Ok(())
}

/// Draws the composite into a new output and makes the frame; the frame's lease keeps
/// the output alive and the pool reuses the rest at once. A failed draw is returned
/// inside `Ok` so the caller can publish nothing; a pool failure is the node's.
fn render_output(
    gpu: &GpuContext,
    pool: &mut TexturePool,
    prepared: &Prepared,
    (input, matte): (TextureId, TextureId),
    size: [u32; 2],
    time: Timestamp,
) -> Result<Result<(TextureId, VideoFrame), GpuError>, NodeError> {
    let output = pool
        .acquire(gpu, &TextureDesc::canvas(size[0], size[1]))
        .map_err(backend)?;
    let result = composite(gpu, pool, prepared, input, matte, output).and_then(|()| {
        let format = VideoFormat::new(
            Size::new(size[0], size[1]),
            PixelFormat::Rgba16Float,
            ColorSpace::LINEAR_BT709,
        );
        pool.frame(output, format, time)
    });
    pool.release(output).map_err(backend)?;
    Ok(result.map(|frame| (output, frame)))
}

/// Writes the composite pass's uniforms: the matte edges and the input's texel size and
/// transfer.
fn write_composite(
    gpu: &GpuContext,
    prepared: &Prepared,
    edge: [f32; 2],
    size: [u32; 2],
    decode: Transfer,
) {
    let mut u: UniformBlock = [[0.0; 4]; 16];
    u[0] = [edge[0], edge[1], 0.0, 0.0];
    u[15] = [
        1.0 / size[0] as f32,
        1.0 / size[1] as f32,
        0.0,
        decode.code(),
    ];
    gpu.queue()
        .write_buffer(&prepared.comp_buffer, 0, bytemuck::bytes_of(&u));
}

/// True when the aspect ratio of `new` differs from `old` by more than 2 percent.
fn aspect_changed(old: [u32; 2], new: [u32; 2]) -> bool {
    let a = f64::from(old[0]) * f64::from(new[1]);
    let b = f64::from(new[0]) * f64::from(old[1]);
    (a - b).abs() > 0.02 * a.max(b)
}
