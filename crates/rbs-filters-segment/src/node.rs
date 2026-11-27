//! The background removal node (ADR-0179; spec `person-mask-port.md`, "Filter").
//!
//! Each tick with a new input picture the node draws it at the session's small size
//! (sRGB, BGRA) on the GPU, copies that into one of a ring of staging buffers and maps
//! it without waiting. A helper thread (`helper.rs`) waits for each copy, reads the bytes
//! and lends them to the session as a small system frame at once, not at the next tick;
//! the newest matte the session has finished is uploaded to a one-channel
//! texture; the composite pass multiplies the full-size picture by the refined matte.
//! Nothing waits: a ring that is full or a session that is busy drops the new picture,
//! and the picture is drawn with the matte of an earlier one (ADR-0179 decision 4). Until
//! the first matte exists the node publishes nothing, so the unfiltered picture is
//! never shown; then the result fades in over 0.2 s of master-clock time. The session
//! is opened in `prepare` and closed in `release`, on the control thread.
//!
//! The only readback is the small picture (named exception of ADR-0179 decision 3).
//! The full-size picture never leaves the GPU.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
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

use crate::{
    BACKGROUND, BACKGROUND_COLOUR, BLUR, EDGE_SHIFT, FEATHER, SYNC, THRESHOLD, TYPE_ID, edges,
};

/// Index of the "in" pin.
pub const IN_PIN: usize = 0;
/// Index of the "out" pin.
pub const OUT_PIN: usize = 0;
/// Staging buffers in flight between the GPU and the session. The session keeps one
/// picture pending at most, so three leave room for a slow map.
pub const RING: usize = 3;
/// Output textures kept by the node (the one read downstream and the one drawn next).
const OUTPUT_RING: usize = 2;
/// Frame time over which the person fades in after the first matte (owner, 2026-11-27):
/// the filter shows nothing before it, and the fade softens the jump from nothing.
const FADE_IN_NANOS: u64 = 200_000_000;

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const MAPPED: u8 = 2;
const FAILED: u8 = 3;

const DOWNSCALE: &str = include_str!("shaders/downscale.wgsl");
use crate::COMPOSITE;
mod blur;
mod helper;
use crate::ring::{COPY, CopyPass, Ring, picture_to_draw};
use blur::{BlurPass, render_output};
use helper::{Job, Readback, Setup, SharedSession, SlotView};
/// The small picture's format: `Bgra8Unorm` is BGRA in memory, which is what a backend
/// takes as `PixelFormat::Bgra8`.
const SMALL_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;
const MATTE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Opens a session. Called on the control thread each time the node is prepared to enter
/// the schedule (`prepare`), so a disabled filter holds no model and no worker (P13).
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

/// GPU objects made in `prepare`.
struct Prepared {
    down: wgpu::RenderPipeline,
    copy: CopyPass,
    comp: wgpu::RenderPipeline,
    blur: BlurPass,
    down_buffer: wgpu::Buffer,
    down_group: wgpu::BindGroup,
    comp_buffer: wgpu::Buffer,
    comp_group: wgpu::BindGroup,
    slots: Vec<SlotView>,
    /// Finishes the readbacks on its own thread.
    readback: Readback,
    /// Bytes per row of the staging buffers.
    stride: u32,
}

/// What the uniform buffers hold.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Written {
    edges: [f32; 2],
    size: [u32; 2],
    decode: Transfer,
    look: Look,
    /// How far the fade-in is, 0 to 1.
    fade: f32,
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
    session: SharedSession,
    /// The small picture's size: what the session's level asks for.
    small: [u32; 2],
    gpu: Option<Prepared>,
    /// The small render target, kept for the node's life in the schedule.
    small_texture: Option<TextureId>,
    /// The matte texture and its size; made when the first matte arrives.
    matte: Option<(TextureId, [u32; 2])>,
    matte_generation: u64,
    /// The input size the pictures are for.
    size: Option<[u32; 2]>,
    down_written: Option<([u32; 2], Transfer)>,
    written: Option<Written>,
    /// The input picture last queued for readback.
    submitted: Option<(TextureId, Timestamp)>,
    drawn: Option<Drawn>,
    last_output: Option<TextureId>,
    rgb: YuvToRgb,
    /// The last input pictures, to draw a picture with the matte made from it (Exact).
    ring: Ring,
    /// The picture drawn last in Exact mode, kept alive while the ring moves on.
    held: Option<VideoFrame>,
    /// Tick time of the first picture drawn with a matte since the filter last showed
    /// nothing: the fade-in starts there.
    fade_from: Option<Timestamp>,
    /// The previous tick's time while in the schedule.
    last_tick: Option<Timestamp>,
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
            session: SharedSession::default(),
            small: [small[0].max(1), small[1].max(1)],
            gpu: None,
            small_texture: None,
            matte: None,
            matte_generation: 0,
            size: None,
            down_written: None,
            written: None,
            submitted: None,
            drawn: None,
            last_output: None,
            rgb: YuvToRgb::new(),
            ring: Ring::default(),
            held: None,
            fade_from: None,
            last_tick: None,
        };
        (node, sender, stats)
    }

    /// Opens the session and starts its worker, unless one is open. Control thread.
    fn open_session(&mut self) -> Result<(), NodeError> {
        let mut shared = self
            .session
            .lock()
            .map_err(|_| NodeError::Failed("person mask session lock".into()))?;
        if shared.is_none() {
            let mut session = (self.factory)()
                .map_err(|e| NodeError::Failed(format!("person mask session: {e}")))?;
            if let Err(e) = session.warm() {
                session.close();
                return Err(NodeError::Failed(format!("person mask session: {e}")));
            }
            *shared = Some(session);
        }
        Ok(())
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
pub(crate) type UniformBlock = [[f32; 4]; 16];

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
pub(crate) fn draw(
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
        let pass_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-filters-segment blur"),
            bind_group_layouts: &[Some(&uniforms), Some(sampled), Some(sampled)],
            immediate_size: 0,
        });
        let comp_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rbs-filters-segment composite"),
            bind_group_layouts: &[Some(&uniforms), Some(sampled), Some(sampled), Some(sampled)],
            immediate_size: 0,
        });
        let down = pipeline(gpu, &down_layout, DOWNSCALE, SMALL_FORMAT)?;
        let comp = pipeline(gpu, &comp_layout, COMPOSITE, rbs_gpu::CANVAS_FORMAT)?;
        let blur = BlurPass::new(gpu, &uniforms, &pass_layout)?;
        let (down_buffer, down_group) = uniform(device, &uniforms);
        let (copy_buffer, copy_group) = uniform(device, &uniforms);
        let copy = CopyPass {
            pipeline: pipeline(gpu, &down_layout, COPY, rbs_gpu::CANVAS_FORMAT)?,
            buffer: copy_buffer,
            group: copy_group,
        };
        let (comp_buffer, comp_group) = uniform(device, &uniforms);
        let stride = (self.small[0] * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let slots = (0..RING)
            .map(|_| SlotView {
                buffer: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("rbs-filters-segment staging"),
                    size: u64::from(stride) * u64::from(self.small[1]),
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
            })
            .collect::<Vec<_>>();
        // Control thread: the model is loaded and the session's worker started here,
        // before the node joins the schedule, so the render thread never reads a model
        // file or waits for a worker (spec rule 2, P6) and the first matte follows the
        // first picture within a few ticks. `release` closes it, so a disabled filter
        // holds no model and no worker (P13).
        self.open_session()?;
        let readback = Readback::start(Setup {
            device: device.clone(),
            slots: slots.clone(),
            stride,
            small: self.small,
            session: Arc::clone(&self.session),
            stats: Arc::clone(&self.stats),
        })?;
        self.gpu = Some(Prepared {
            down,
            copy,
            comp,
            blur,
            down_buffer,
            down_group,
            comp_buffer,
            comp_group,
            slots,
            readback,
            stride,
        });
        self.down_written = None;
        self.written = None;
        self.drawn = None;
        Ok(())
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Executor thread, between ticks: pool textures only. The session was opened in
        // `prepare` on the control thread.
        if self.small_texture.is_none() {
            let id = ctx
                .local
                .pool
                .acquire(ctx.shared, &self.small_desc())
                .map_err(backend)?;
            self.small_texture = Some(id);
        }
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        // Leaving the schedule: give back every texture. The session is closed in
        // `release` on the control thread, because closing joins its worker.
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
        self.ring.clear(pool);
        self.held = None;
        self.rgb.release();
        self.fade_from = None;
        self.last_tick = None;
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let gpu = ctx.shared();
        let changed = self.values.update();
        let values = *self.values.get();
        let small_id = self.small_texture.ok_or(NodeError::NotReady)?;
        let tick_time = ctx.tick().time;
        let previous_tick = self.last_tick.replace(tick_time).unwrap_or(tick_time);
        let (inputs, outputs, local) = ctx.parts();

        // What the input offers; a 4:2:0 input is decoded to RGB first, once per picture.
        let mut frame = inputs.video(IN_PIN, 0);
        if let Some(f) = frame
            && YuvToRgb::needs_decode(&local.pool, f)
        {
            frame = self.rgb.rgb(gpu, &mut local.pool, f).ok();
        }
        // Exact copies each picture into node-owned textures (the source lends only a few).
        let exact = values.choice(SYNC) == 0;
        let input = frame.and_then(|frame| describe(&local.pool, frame));
        let Some((input_id, size, decode, time)) = input else {
            self.drawn = None;
            drop(outputs.set_video(OUT_PIN, None)?);
            return Ok(());
        };
        let prepared = self.gpu.as_ref().ok_or(NodeError::NotReady)?;
        if self.size != Some(size) {
            // A new picture size (spec rule 5): the old size's textures go back at once.
            // The matte has no size of its own, so it stays in use while the aspect is
            // the same; a different aspect starts the session and the matte over.
            if let Some(old) = self.size {
                give_back(&mut local.pool, old, self.last_output.take());
                if aspect_changed(old, size) {
                    if let Ok(mut guard) = self.session.lock()
                        && let Some(session) = guard.as_mut()
                    {
                        session.reset();
                    }
                    self.matte_generation = 0;
                    if let Some((id, _)) = self.matte.take() {
                        let _ = local.pool.release(id);
                        local.pool.forget_texture(id);
                    }
                }
            }
            self.size = Some(size);
            self.submitted = None;
            self.drawn = None;
            self.held = None;
            self.ring.clear(&mut local.pool);
        }
        self.ring.sync(
            exact,
            &mut self.held,
            (gpu, &mut local.pool, &prepared.copy),
            (input_id, size, decode, time),
        );

        // 1. (The helper thread lends finished readbacks to the session.)
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
                    if let Ok(submission) = queued {
                        prepared.readback.send(Job {
                            slot: index,
                            time,
                            submission,
                        });
                        self.submitted = Some((input_id, time));
                    } else {
                        self.stats.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // 3. The newest finished matte, uploaded.
        let mut matte_time = None;
        let latest = match self.session.lock() {
            Ok(mut guard) => guard.as_mut().ok_or(NodeError::NotReady)?.latest(),
            Err(_) => return Err(NodeError::NotReady),
        };
        match latest {
            Ok(Some(matte)) => {
                let time = matte.timestamp;
                if take_matte(gpu, &mut local.pool, matte, &mut self.matte, &self.stats) {
                    self.matte_generation += 1;
                    matte_time = Some(time);
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
            self.fade_from = None;
            drop(outputs.set_video(OUT_PIN, None)?);
            return Ok(());
        };
        // The picture drawn with the matte: the newest one, or the one it was made from.
        let (input_id, size, decode, time) = picture_to_draw(
            &mut self.held,
            &self.ring,
            &mut local.pool,
            exact,
            matte_time,
            (input_id, size, decode, time),
        );
        let (edge, look) = (edges_of(&values), look_of(&values));
        // On the master clock, so a picture that does not change still fades in.
        // It starts at the tick before, the last one that showed nothing, so the first
        // frame is already one tick into the fade.
        let fade = fade_in(*self.fade_from.get_or_insert(previous_tick), tick_time);
        let state = Written {
            edges: edge,
            size,
            decode,
            look,
            fade,
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
            write_composite(gpu, prepared, (edge, look, fade), size, decode);
            self.written = Some(state);
        }
        let result = render_output(
            gpu,
            &mut local.pool,
            prepared,
            (input_id, matte_id),
            size,
            time,
            look.mode > 0.5 && look.mode < 1.5,
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
        self.held = None;
        if let Some(session) = self.session.lock().ok().and_then(|mut s| s.take()) {
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
) -> Result<wgpu::SubmissionIndex, GpuError> {
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
    slot.state.store(MAPPING, Ordering::Release);
    let submission = gpu.queue().submit(Some(encoder.finish()));
    let state = Arc::clone(&slot.state);
    slot.buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            state.store(
                if result.is_ok() { MAPPED } else { FAILED },
                Ordering::Release,
            );
        });
    Ok(submission)
}

/// Uploads a CPU matte to the matte texture (made, or
/// remade, when the matte's size changed: once per session and level). Returns whether
/// the texture holds a new matte.
fn take_matte(
    gpu: &GpuContext,
    pool: &mut TexturePool,
    matte: Matte,
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
    }
    let Some((id, _)) = *texture else {
        return false;
    };
    if readback::upload(gpu, pool, id, &pixels).is_err() {
        stats.bad_mattes.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    stats.mattes.fetch_add(1, Ordering::Relaxed);
    true
}

/// Writes the composite pass's uniforms: the matte edges and the input's texel size and
/// transfer.
fn write_composite(
    gpu: &GpuContext,
    prepared: &Prepared,
    (edge, look, fade): ([f32; 2], Look, f32),
    size: [u32; 2],
    decode: Transfer,
) {
    let mut u: UniformBlock = [[0.0; 4]; 16];
    u[0] = [edge[0], edge[1], 0.0, 0.0];
    u[1] = look.colour;
    u[2] = [look.mode, look.blur[0], look.blur[1], fade];
    u[15] = [
        1.0 / size[0] as f32,
        1.0 / size[1] as f32,
        0.0,
        decode.code(),
    ];
    gpu.queue()
        .write_buffer(&prepared.comp_buffer, 0, bytemuck::bytes_of(&u));
    blur::write(gpu, prepared, (edge, look), size, decode);
}

/// How far the fade-in is at `now` when it started at `from`: 0 to 1, on frame time.
fn fade_in(from: Timestamp, now: Timestamp) -> f32 {
    let done = now.as_nanos().saturating_sub(from.as_nanos());
    (done as f64 / FADE_IN_NANOS as f64).min(1.0) as f32
}

/// True when the aspect ratio of `new` differs from `old` by more than 2 percent.
fn aspect_changed(old: [u32; 2], new: [u32; 2]) -> bool {
    let a = f64::from(old[0]) * f64::from(new[1]);
    let b = f64::from(new[0]) * f64::from(old[1]);
    (a - b).abs() > 0.02 * a.max(b)
}

/// What one input picture is to the passes: texture, size, transfer and time; `None`
/// when it is not a sampleable texture.
pub(crate) type Picture = (TextureId, [u32; 2], Transfer, Timestamp);

pub(crate) fn describe(pool: &TexturePool, frame: &VideoFrame) -> Option<Picture> {
    let FramePayload::Texture(id) = *frame.payload() else {
        return None;
    };
    let tex = pool.get(id)?;
    tex.sampled_bind_group()?;
    let d = tex.desc();
    Some((
        id,
        [d.width, d.height],
        Transfer::of(frame.format(), d.format),
        frame.timestamp(),
    ))
}

/// What shows behind the person: the mode (0 transparent, 1 blur, 2 colour), the blur
/// radius in picture widths and heights, and the colour (linear, premultiplied).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Look {
    mode: f32,
    blur: [f32; 2],
    colour: [f32; 4],
}

fn look_of(values: &Values) -> Look {
    let mode = values.choice(BACKGROUND);
    let radius = 0.005 + 0.075 * values.number(BLUR);
    let [r, g, b, a] = values.color(BACKGROUND_COLOUR);
    let a = f32::from(a) / 255.0;
    let lin = |c: u8| {
        let e = f32::from(c) / 255.0;
        if e <= 0.040_45 {
            e / 12.92
        } else {
            ((e + 0.055) / 1.055).powf(2.4)
        }
    };
    Look {
        mode: f32::from(mode),
        // Wider than tall pictures blur by the same distance both ways.
        blur: [radius * 0.5625, radius],
        colour: [lin(r) * a, lin(g) * a, lin(b) * a, a],
    }
}

/// The composite edges for the settings.
fn edges_of(values: &Values) -> [f32; 2] {
    edges(
        values.number(THRESHOLD),
        values.number(FEATHER),
        values.number(EDGE_SHIFT),
    )
}
