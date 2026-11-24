//! Shared rig of the background removal tests: feed -> node -> probe on one render
//! executor with a fake session (no model, no vendor).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::ColorSpace;
use rbs_core::media::{
    Bytes, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, TextureId, VideoFormat,
    VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_filters::conformance::{Picture, f32_to_f16};
use rbs_filters_segment::{BackgroundRemovalNode, RemovalStats, fields};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, GpuError, GpuRequest, SourceUpload, TextureDesc, readback};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::param::ParamSender;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_params::Values;
use rbs_segment::{Matte, MattePlane, PersonMaskSession, SegmentError, SubmitOutcome};

pub fn gpu() -> Option<Arc<GpuContext>> {
    static CTX: OnceLock<Option<Arc<GpuContext>>> = OnceLock::new();
    let ctx = CTX.get_or_init(|| {
        match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => Some(Arc::new(ctx)),
            Err(GpuError::NoAdapter) => None,
            Err(e) => panic!("GPU context failed: {e}"),
        }
    });
    if ctx.is_none() {
        assert!(
            std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
            "RBS_REQUIRE_GPU is set but no GPU adapter was found"
        );
        eprintln!("SKIPPED: no GPU adapter");
    }
    ctx.clone()
}

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the fake session saw.
#[derive(Default)]
pub struct Log {
    pub opened: AtomicU32,
    pub closed: AtomicU32,
    pub warmed: AtomicU32,
    pub submits: AtomicU32,
    pub resets: AtomicU32,
    /// Size, pixel format and first pixel of the last small frame.
    pub last: Mutex<Option<(Size, PixelFormat, [u8; 4])>>,
    /// The matte the session answers with: left half person.
    pub fail_open: AtomicBool,
}

struct Fake {
    pub log: Arc<Log>,
    /// The picture a matte is made for and the polls left before it is ready.
    pending: std::collections::VecDeque<(Timestamp, u32)>,
}

/// The next NV12 picture the feed makes: width, height and the three sample values.
type Nv12Slot = Arc<Mutex<Option<(u32, u32, [u8; 3])>>>;

const MATTE: [u32; 2] = [8, 4];
/// Polls (ticks) a matte takes after its picture was lent: the real ones take two or three.
const MATTE_LATENCY: u32 = 2;

impl PersonMaskSession for Fake {
    fn warm(&mut self) -> Result<(), SegmentError> {
        self.log.warmed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
        self.log.submits.fetch_add(1, Ordering::Relaxed);
        let FramePayload::System(system) = frame.payload() else {
            return SubmitOutcome::NotShareable;
        };
        let (bytes, _) = system.plane(0).unwrap();
        *lock(&self.log.last) = Some((
            frame.format().size,
            frame.format().pixel_format,
            [bytes[0], bytes[1], bytes[2], bytes[3]],
        ));
        self.pending.push_back((frame.timestamp(), MATTE_LATENCY));
        SubmitOutcome::Accepted
    }
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        for entry in &mut self.pending {
            entry.1 = entry.1.saturating_sub(1);
        }
        if self.pending.front().is_none_or(|e| e.1 > 0) {
            return Ok(None);
        }
        let Some((time, _)) = self.pending.pop_front() else {
            return Ok(None);
        };
        let mut data = vec![0u8; (MATTE[0] * MATTE[1]) as usize];
        for row in data.chunks_exact_mut(MATTE[0] as usize) {
            row[..MATTE[0] as usize / 2].fill(255);
        }
        Ok(Some(Matte::new(
            time,
            Size::new(MATTE[0], MATTE[1]),
            MattePlane::Cpu(data.into()),
        )))
    }
    fn reset(&mut self) {
        self.log.resets.fetch_add(1, Ordering::Relaxed);
        self.pending.clear();
    }
    fn close(self: Box<Self>) {
        self.log.closed.fetch_add(1, Ordering::Relaxed);
    }
}

struct Feed {
    desc: NodeDescriptor,
    next: Arc<Mutex<Option<Picture>>>,
    /// A 4:2:0 camera picture: width, height and the Y, Cb and Cr bytes of every texel.
    next_nv12: Nv12Slot,
    upload: SourceUpload,
    nv12: Option<VideoFrame>,
    current: Option<(TextureId, u32, u32, Timestamp)>,
}

impl Node<GpuRuntime> for Feed {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let time = ctx.tick().time;
        let gpu = ctx.shared();
        let (_, outputs, local) = ctx.parts();
        if let Some((w, h, [y, cb, cr])) = lock(&self.next_nv12).take() {
            let mut data = vec![y; (w * h) as usize];
            for _ in 0..(w * h / 4) {
                data.extend_from_slice(&[cb, cr]);
            }
            let format = VideoFormat::new(
                Size::new(w, h),
                PixelFormat::Nv12,
                ColorSpace::BT709_LIMITED,
            );
            let planes = [
                PlaneLayout {
                    offset: 0,
                    stride: w as usize,
                },
                PlaneLayout {
                    offset: (w * h) as usize,
                    stride: w as usize,
                },
            ];
            let system = SystemFrame::new(format, Bytes::from(data), &planes).unwrap();
            let frame = VideoFrame::system(time, system, None);
            let uploaded = self
                .upload
                .upload(gpu, &mut local.pool, frame)
                .map_err(|_| NodeError::Backend("feed"))?;
            self.nv12 = Some(uploaded);
            self.current = None;
        }
        if let Some(frame) = self.nv12.as_ref().and_then(VideoFrame::share) {
            drop(outputs.set_video(0, Some(frame))?);
            return Ok(());
        }
        if let Some(p) = lock(&self.next).take() {
            let mut desc = TextureDesc::source(p.width, p.height, wgpu::TextureFormat::Rgba16Float);
            desc.usage |= wgpu::TextureUsages::COPY_SRC;
            if let Some((old, ..)) = self.current.take() {
                let _ = local.pool.release(old);
                local.pool.forget_texture(old);
            }
            let id = local
                .pool
                .acquire(gpu, &desc)
                .map_err(|_| NodeError::Backend("feed"))?;
            let bytes: Vec<u8> = p
                .pixels
                .iter()
                .flat_map(|px| px.iter().flat_map(|c| f32_to_f16(*c).to_le_bytes()))
                .collect();
            readback::upload(gpu, &local.pool, id, &bytes)
                .map_err(|_| NodeError::Backend("feed"))?;
            self.current = Some((id, p.width, p.height, time));
        }
        let Some((id, w, h, time)) = self.current else {
            return Ok(());
        };
        let format = VideoFormat::new(
            Size::new(w, h),
            PixelFormat::Rgba16Float,
            ColorSpace::LINEAR_BT709,
        );
        let frame = local
            .pool
            .frame(id, format, time)
            .map_err(|_| NodeError::Backend("feed"))?;
        drop(outputs.set_video(0, Some(frame))?);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

struct Probe {
    desc: NodeDescriptor,
    pub last: Arc<Mutex<Option<VideoFrame>>>,
}

impl Node<GpuRuntime> for Probe {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        *lock(&self.last) = ctx.inputs().video(0, 0).and_then(VideoFrame::share);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

pub struct Rig {
    graph: GraphDescription,
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    pub ctx: Arc<GpuContext>,
    pub tick: u64,
    next: Arc<Mutex<Option<Picture>>>,
    next_nv12: Nv12Slot,
    pub last: Arc<Mutex<Option<VideoFrame>>>,
    pub node: NodeId,
    pub values: ParamSender<Values>,
    pub stats: Arc<RemovalStats>,
    pub log: Arc<Log>,
}

impl Rig {
    pub fn new(ctx: Arc<GpuContext>, small: [u32; 2]) -> Self {
        let log = Arc::new(Log::default());
        let factory_log = Arc::clone(&log);
        let factory = Box::new(
            move || -> Result<Box<dyn PersonMaskSession>, SegmentError> {
                if factory_log.fail_open.load(Ordering::Relaxed) {
                    return Err(SegmentError::Unavailable);
                }
                factory_log.opened.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(Fake {
                    log: Arc::clone(&factory_log),
                    pending: Default::default(),
                }))
            },
        );
        let (node, values, stats) =
            BackgroundRemovalNode::new(factory, small, Values::defaults(&Defaults));
        let (mut ctl, exec) = channel::<GpuRuntime>(Arc::clone(&ctx), RenderLocal::default());
        let mut graph = GraphDescription::new();
        let next = Arc::new(Mutex::new(None));
        let next_nv12 = Arc::new(Mutex::new(None));
        let last = Arc::new(Mutex::new(None));
        let feed = Feed {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.feed"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("out", LinkKind::Video)],
            ),
            next: Arc::clone(&next),
            next_nv12: Arc::clone(&next_nv12),
            upload: SourceUpload::new(),
            nv12: None,
            current: None,
        };
        let probe = Probe {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.probe"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Video)],
                vec![],
            ),
            last: Arc::clone(&last),
        };
        let mut add = |node: Box<dyn Node<GpuRuntime>>| -> NodeId {
            let id = graph.add_node(Arc::new(node.descriptor().clone())).unwrap();
            graph.set_state(id, Lifecycle::Standby).unwrap();
            graph.set_state(id, Lifecycle::Active).unwrap();
            ctl.adopt(id, node).unwrap();
            id
        };
        let feed = add(Box::new(feed));
        let filter = add(Box::new(node));
        let probe = add(Box::new(probe));
        graph.connect(feed, "out", filter, "in").unwrap();
        graph.connect(filter, "out", probe, "in").unwrap();
        let mut rig = Self {
            graph,
            ctl,
            exec,
            ctx,
            tick: 0,
            next,
            next_nv12,
            last,
            node: filter,
            values,
            stats,
            log,
        };
        let report = rig.ctl.apply(&rig.graph).unwrap();
        assert!(
            report.failed.is_empty(),
            "apply failed: {:?}",
            report.failed
        );
        rig
    }

    pub fn set_input(&self, p: Picture) {
        *lock(&self.next) = Some(p);
    }

    /// A solid 4:2:0 camera picture (limited-range BT.709 `y`, `cb`, `cr`).
    pub fn set_nv12(&self, width: u32, height: u32, ycc: [u8; 3]) {
        *lock(&self.next_nv12) = Some((width, height, ycc));
    }

    pub fn tick(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        self.exec.run_tick(Tick::of(&cadence, self.tick));
        self.tick += 1;
        let _ = self.ctl.collect();
    }

    /// Ticks until the probe has a frame, letting the GPU finish between ticks. The
    /// bound is a hang guard, not a timing assertion.
    pub fn until_output(&mut self) -> bool {
        for _ in 0..100 {
            self.tick();
            self.ctx.wait_idle().unwrap();
            if lock(&self.last).is_some() {
                return true;
            }
        }
        false
    }

    pub fn output(&mut self) -> Option<Picture> {
        let frame = lock(&self.last).as_ref().and_then(VideoFrame::share)?;
        let FramePayload::Texture(id) = *frame.payload() else {
            return None;
        };
        let size = frame.format().size;
        let pixels = readback::read_rgba16f(&self.ctx, &self.exec.local().pool, id).ok()?;
        Some(Picture {
            width: size.width,
            height: size.height,
            pixels,
            srgb8: false,
        })
    }

    pub fn disable(&mut self) {
        self.graph.set_state(self.node, Lifecycle::Standby).unwrap();
        self.ctl.apply(&self.graph).unwrap();
        self.tick();
        self.tick();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.ctx.wait_idle();
    }
}

struct Defaults;
impl rbs_params::Schema for Defaults {
    fn fields(&self) -> &'static [rbs_params::Field] {
        fields()
    }
}
