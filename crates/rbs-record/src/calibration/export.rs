//! The export part of *Test this computer* (ADR-0087 step D2, spec
//! `output-estimate.md` "Export measurement"): what one separate video file's export
//! adds to the render tick, per size class and format, measured in a throwaway graph on
//! a throwaway GPU device.
//!
//! [`run_exports`] is a plain function over an injected [`ExportRig`] (the real one is
//! [`GpuExportRig`]; tests use a fake that advances a `TestClock`), the run's clock, a
//! [`Pacer`] and a stop flag. `time_us` is the render thread's time of one track's
//! export (the clock the give-way rule reads); `gpu_us`, the GPU's sustained time per
//! export, is stored for diagnosis and judged by nothing (owner record, ADR-0087
//! Review, 2026-11-15).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    Bytes, ColorSpace, FramePayload, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat,
    VideoFrame,
};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{Cadence, FrameRate};
use rbs_filters::{FilterNode, FilterType, Normalise, Values};
use rbs_gpu::node::{GpuRuntime, RenderLocal};
use rbs_gpu::{GpuContext, SourceUpload, YuvFormat};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, WarmCtx, channel,
};
use rbs_nodes::export::{ExportNodeStats, VideoExportNode};

use super::driver::{CalibrationEvent, Pacer};
use crate::estimate::{ExportFormat, SizeClass};

/// The export part's limit, the throwaway device included. It comes out of the
/// encoder part's first pass; the run's own limits do not change.
pub const EXPORT_LIMIT: Duration = Duration::from_secs(6);

/// Kept for the throwaway device before the first class.
pub const SIBLING_RESERVE: Duration = Duration::from_secs(1);

/// Unpaced conversions the GPU part aims for.
pub const GPU_FRAMES: u64 = 60;

/// Fewer conversions than this give no `gpu_us`.
pub const GPU_MIN_FRAMES: u64 = 8;

/// The GPU part of a class stops after this long.
pub const GPU_PART_LIMIT: Duration = Duration::from_millis(500);

/// One bounded wait for the GPU, repeated while time remains and no cancel is set.
pub const WAIT_SLICE: Duration = Duration::from_millis(250);

/// Kept per class for setup and the waits.
const CLASS_OVERHEAD: Duration = Duration::from_millis(250);

/// Moving frames the feed uploads and shows in turn.
pub const EXPORT_FEED_FRAMES: usize = 3;

/// Unjudged ticks before the paced part: a quarter second, 8 to 30 ticks.
pub fn export_warmup(period: Duration) -> u32 {
    ticks_in(Duration::from_millis(250), period).clamp(8, 30)
}

/// Paced ticks of a class: half a second, 16 to 60 ticks.
pub fn export_rounds(period: Duration) -> u32 {
    ticks_in(Duration::from_millis(500), period).clamp(16, 60)
}

fn ticks_in(span: Duration, period: Duration) -> u32 {
    if period.is_zero() {
        return u32::MAX;
    }
    u32::try_from(span.as_nanos() / period.as_nanos()).unwrap_or(u32::MAX)
}

/// The time a class needs at most (the skip test of the budget).
pub fn class_estimate(period: Duration) -> Duration {
    period.saturating_mul(export_warmup(period) + export_rounds(period))
        + GPU_PART_LIMIT
        + CLASS_OVERHEAD
}

/// One export class of a run: a size class at its bound size and the profile's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportClass {
    /// The size class.
    pub size: SizeClass,
    /// Width measured (even).
    pub width: u32,
    /// Height measured (even).
    pub height: u32,
    /// The profile's export format.
    pub format: YuvFormat,
    /// The profile's colour.
    pub color: ColorSpace,
    /// The profile's open-shadows grade.
    pub open_shadows: bool,
}

impl ExportClass {
    /// The bound size of `size` (16:9), or `planned` when that has more pixels; even.
    pub fn bound(size: SizeClass, planned: (u32, u32)) -> (u32, u32) {
        let bound = match size {
            SizeClass::L720 => (1280, 720),
            SizeClass::L1080 => (1920, 1080),
            SizeClass::L1440 => (2560, 1440),
            SizeClass::L2160 => (3840, 2160),
            SizeClass::Above2160 => (0, 0),
        };
        let pixels = |s: (u32, u32)| u64::from(s.0) * u64::from(s.1);
        let s = if pixels(planned) > pixels(bound) {
            planned
        } else {
            bound
        };
        ((s.0 + 1) & !1, (s.1 + 1) & !1)
    }

    /// Its format as the estimate names it.
    pub fn export_format(&self) -> ExportFormat {
        self.format.into()
    }
}

/// The export counters a rig reads ([`ExportNodeStats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportCounters {
    /// Frames delivered.
    pub frames: u64,
    /// Conversions lost (stale, no buffer, failed).
    pub lost: u64,
    /// Ticks whose canvas was not converted because every readback was in flight.
    pub busy: u64,
}

/// Why a rig call failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RigError {
    /// The graph could not be built or a node failed.
    Failed,
    /// The throwaway device was lost.
    DeviceLost,
}

/// The throwaway graph of the export part.
pub trait ExportRig: Send {
    /// Builds the graph of `class` (feed, normalisation, export, sink). Its frames are
    /// uploaded on the first tick, which is never timed.
    fn begin(&mut self, class: &ExportClass) -> Result<(), RigError>;
    /// One render tick of the graph, ending with a non-blocking poll.
    fn tick(&mut self) -> Result<(), RigError>;
    /// Waits at most `timeout` for the work submitted so far; `Ok(false)` on timeout.
    fn wait(&mut self, timeout: Duration) -> Result<bool, RigError>;
    /// The export node's counters.
    fn counters(&self) -> ExportCounters;
    /// Drops the class's graph.
    fn end(&mut self);
}

/// One measured class (`[[exports]]`).
#[derive(Clone, Debug, PartialEq)]
pub struct ExportResult {
    /// What was measured.
    pub class: ExportClass,
    /// The render thread's mean time of one tick with the export; at least one frame
    /// period when it did not keep up.
    pub time: Duration,
    /// The GPU's sustained time per export; `None` with too few conversions.
    pub gpu: Option<Duration>,
    /// No conversion found every readback in flight and the frames arrived.
    pub kept_up: bool,
}

/// Why an export class has no result: the closed code list of `exports_skipped`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportSkipCode {
    /// No throwaway GPU device.
    GpuUnavailable,
    /// The graph could not be built or a node failed.
    ExportFailed,
    /// The throwaway device was lost (the run went on).
    DeviceLost,
    /// The time budget ended it first, or the GPU did not answer in time.
    TimeLimit,
    /// A cancel ended it first.
    Cancelled,
}

impl ExportSkipCode {
    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GpuUnavailable => "gpu_unavailable",
            Self::ExportFailed => "export_failed",
            Self::DeviceLost => "device_lost",
            Self::TimeLimit => "time_limit",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A skipped export class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExportSkip {
    /// The class.
    pub class: ExportClass,
    /// Why.
    pub code: ExportSkipCode,
}

/// How the export part ended, and what to do with the rig.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportsEnd {
    /// Nothing waits on the GPU: drop the rig where it is.
    Clean,
    /// A wait timed out or the throwaway device was lost: dropping the rig may block
    /// without a bound, so hand it to a thread that can be abandoned.
    Reap,
    /// The live GPU device was lost too: the whole run fails, nothing is written.
    LiveLost,
}

enum Measured {
    Done(ExportResult),
    Skip(ExportSkipCode),
    /// A wait timed out or a cancel came while the GPU still had work.
    Abandon(ExportSkipCode),
    Lost,
}

/// Measures every class of `classes` in order (largest first) from the run's `start`
/// within [`EXPORT_LIMIT`]. `open` makes the rig (the throwaway device); `live_lost`
/// says whether the live device was lost too, after a lost throwaway one. Results and
/// skips go to `events`. Returns the rig, if one was made, and how it may be dropped.
#[allow(clippy::too_many_arguments)]
pub fn run_exports(
    classes: &[ExportClass],
    period: Duration,
    open: &mut dyn FnMut() -> Result<Box<dyn ExportRig>, RigError>,
    live_lost: &mut dyn FnMut() -> bool,
    clock: &dyn Clock,
    pacer: &dyn Pacer,
    stop: &AtomicBool,
    start: Timestamp,
    events: &mut dyn FnMut(CalibrationEvent),
) -> (Option<Box<dyn ExportRig>>, ExportsEnd) {
    let skip_all = |from: usize, code: ExportSkipCode, events: &mut dyn FnMut(CalibrationEvent)| {
        for class in &classes[from..] {
            events(CalibrationEvent::ExportSkipped(ExportSkip {
                class: *class,
                code,
            }));
        }
    };
    if classes.is_empty() {
        return (None, ExportsEnd::Clean);
    }
    let end = start + EXPORT_LIMIT;
    if stop.load(Ordering::Relaxed) {
        skip_all(0, ExportSkipCode::Cancelled, events);
        return (None, ExportsEnd::Clean);
    }
    if end.saturating_since(clock.now()) < SIBLING_RESERVE + class_estimate(period) {
        skip_all(0, ExportSkipCode::TimeLimit, events);
        return (None, ExportsEnd::Clean);
    }
    let mut rig = match open() {
        Ok(rig) => rig,
        Err(_) => {
            skip_all(0, ExportSkipCode::GpuUnavailable, events);
            return (None, ExportsEnd::Clean);
        }
    };
    for (i, class) in classes.iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            skip_all(i, ExportSkipCode::Cancelled, events);
            break;
        }
        if end.saturating_since(clock.now()) < class_estimate(period) {
            events(CalibrationEvent::ExportSkipped(ExportSkip {
                class: *class,
                code: ExportSkipCode::TimeLimit,
            }));
            continue;
        }
        events(CalibrationEvent::ExportStep {
            step: u32::try_from(i + 1).unwrap_or(u32::MAX),
        });
        let measured = measure(&mut *rig, class, period, clock, pacer, stop, end);
        match measured {
            Measured::Done(r) => {
                rig.end();
                events(CalibrationEvent::Export(r));
            }
            Measured::Skip(code) => {
                rig.end();
                events(CalibrationEvent::ExportSkipped(ExportSkip {
                    class: *class,
                    code,
                }));
            }
            Measured::Abandon(code) => {
                events(CalibrationEvent::ExportSkipped(ExportSkip {
                    class: *class,
                    code,
                }));
                let rest = if code == ExportSkipCode::Cancelled {
                    ExportSkipCode::Cancelled
                } else {
                    ExportSkipCode::TimeLimit
                };
                skip_all(i + 1, rest, events);
                return (Some(rig), ExportsEnd::Reap);
            }
            Measured::Lost => {
                skip_all(i, ExportSkipCode::DeviceLost, events);
                let how = if live_lost() {
                    ExportsEnd::LiveLost
                } else {
                    ExportsEnd::Reap
                };
                return (Some(rig), how);
            }
        }
    }
    (Some(rig), ExportsEnd::Clean)
}

fn measure(
    rig: &mut dyn ExportRig,
    class: &ExportClass,
    period: Duration,
    clock: &dyn Clock,
    pacer: &dyn Pacer,
    stop: &AtomicBool,
    end: Timestamp,
) -> Measured {
    let fail = |e: RigError| match e {
        RigError::Failed => Measured::Skip(ExportSkipCode::ExportFailed),
        RigError::DeviceLost => Measured::Lost,
    };
    let stopped = || stop.load(Ordering::Relaxed);
    if let Err(e) = rig.begin(class) {
        return fail(e);
    }
    // Warm-up and the paced part, one tick per period.
    let warmup = export_warmup(period);
    let rounds = export_rounds(period);
    // A cancel leaves conversions in flight whose end is not awaited here, so the rig
    // is then reaped like after a timeout.
    let t0 = clock.now();
    for k in 0..warmup {
        if stopped() {
            return Measured::Abandon(ExportSkipCode::Cancelled);
        }
        pacer.wait_until(t0 + period.saturating_mul(k));
        if let Err(e) = rig.tick() {
            return fail(e);
        }
    }
    let base = rig.counters();
    let t1 = clock.now();
    let mut busy_time = Duration::ZERO;
    for k in 0..rounds {
        if stopped() {
            return Measured::Abandon(ExportSkipCode::Cancelled);
        }
        pacer.wait_until(t1 + period.saturating_mul(k));
        let s = clock.now();
        if let Err(e) = rig.tick() {
            return fail(e);
        }
        busy_time += clock.now().saturating_since(s);
    }
    let after = rig.counters();
    let ring = rbs_gpu::download::DOWNLOAD_RING as u64;
    let kept_up = after.busy == base.busy
        && after.frames - base.frames >= u64::from(rounds).saturating_sub(ring);
    let mean = busy_time / rounds.max(1);
    let time = if kept_up { mean } else { mean.max(period) };

    // Drain what the paced part left in flight, then collect it (untimed).
    match wait_bounded(rig, clock, stop, end) {
        Ok(()) => {}
        Err(m) => return m,
    }
    if let Err(e) = rig.tick() {
        return fail(e);
    }

    // The GPU part: back to back, conversions are ticks without `busy`.
    let base = rig.counters();
    let t2 = clock.now();
    let mut ticks = 0u64;
    loop {
        if stopped() {
            return Measured::Abandon(ExportSkipCode::Cancelled);
        }
        let converted = ticks - (rig.counters().busy - base.busy);
        if converted >= GPU_FRAMES || clock.now().saturating_since(t2) >= GPU_PART_LIMIT {
            break;
        }
        if let Err(e) = rig.tick() {
            return fail(e);
        }
        ticks += 1;
    }
    let converted = ticks - (rig.counters().busy - base.busy);
    match wait_bounded(rig, clock, stop, end) {
        Ok(()) => {}
        Err(m) => return m,
    }
    let elapsed = clock.now().saturating_since(t2);
    // Collects the last readbacks; neither timed nor counted.
    if let Err(e) = rig.tick() {
        return fail(e);
    }
    let gpu = (converted >= GPU_MIN_FRAMES)
        .then(|| elapsed / u32::try_from(converted).unwrap_or(u32::MAX));
    Measured::Done(ExportResult {
        class: *class,
        time,
        gpu,
        kept_up,
    })
}

/// Waits for the GPU in slices of at most [`WAIT_SLICE`], never past `end`.
fn wait_bounded(
    rig: &mut dyn ExportRig,
    clock: &dyn Clock,
    stop: &AtomicBool,
    end: Timestamp,
) -> Result<(), Measured> {
    loop {
        if stop.load(Ordering::Relaxed) {
            return Err(Measured::Abandon(ExportSkipCode::Cancelled));
        }
        let left = end.saturating_since(clock.now());
        if left.is_zero() {
            return Err(Measured::Abandon(ExportSkipCode::TimeLimit));
        }
        match rig.wait(left.min(WAIT_SLICE)) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(RigError::DeviceLost) => return Err(Measured::Lost),
            Err(RigError::Failed) => return Err(Measured::Abandon(ExportSkipCode::ExportFailed)),
        }
    }
}

// ---------------------------------------------------------------------------------
// The real rig: a throwaway executor on a throwaway device.

/// Publishes the uploaded moving frames in turn, one per tick, as a source's own
/// picture (8-bit sRGB, uploaded as a capture is).
struct Feed {
    desc: NodeDescriptor,
    /// The frames in system memory, uploaded on the first tick and then dropped.
    pending: Vec<VideoFrame>,
    /// The uploaded frames (their leases keep the textures).
    uploaded: Vec<VideoFrame>,
    upload: SourceUpload,
    next: usize,
}

impl Node<GpuRuntime> for Feed {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let gpu = ctx.shared();
        let (_, outputs, local) = ctx.parts();
        if self.uploaded.is_empty() {
            // The first tick is a warm-up tick: never timed.
            for frame in self.pending.drain(..) {
                let up = self
                    .upload
                    .upload(gpu, &mut local.pool, frame)
                    .map_err(|_| NodeError::Backend("export test: upload failed"))?;
                self.uploaded.push(up);
            }
        }
        let n = self.uploaded.len();
        let Some(shown) = self.uploaded.get(self.next % n.max(1)) else {
            return Err(NodeError::NotReady);
        };
        self.next = self.next.wrapping_add(1);
        let FramePayload::Texture(id) = *shown.payload() else {
            return Err(NodeError::NotReady);
        };
        let frame = local
            .pool
            .frame(id, *shown.format(), tick.time)
            .map_err(|_| NodeError::Backend("export test: no frame"))?
            .with_sequence(tick.index);
        drop(outputs.set_video(0, Some(frame))?);
        Ok(())
    }
    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.uploaded.clear();
        self.upload.release(&mut ctx.local.pool);
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// Takes each frame off its input and drops it, as an encoder queue would.
struct Sink {
    desc: NodeDescriptor,
}

impl Node<GpuRuntime> for Sink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let _ = ctx.inputs().video(0, 0);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, GpuRuntime>) {}
}

/// RGBA frames of the moving picture D1 encodes: a diagonal gradient that shifts each
/// frame and a bright block that moves across it.
fn moving_rgba(width: u32, height: u32, frames: usize) -> Result<Vec<VideoFrame>, RigError> {
    let (w, h) = (width as usize, height as usize);
    let format = VideoFormat::new(
        Size::new(width, height),
        PixelFormat::Rgba8,
        ColorSpace::SRGB,
    );
    (0..frames)
        .map(|f| {
            let shift = f * 24;
            let mut data = Vec::with_capacity(w * h * 4);
            for y in 0..h {
                for x in 0..w {
                    let block = (x + shift) % w < w / 6 && y > h / 3 && y < h / 2;
                    let v = if block {
                        250
                    } else {
                        ((x + y + shift) % 220 + 16) as u8
                    };
                    let c = ((x + 2 * y + shift) % 200 + 30) as u8;
                    data.extend_from_slice(&[v, c, v / 2 + c / 2, 255]);
                }
            }
            let planes = [PlaneLayout {
                offset: 0,
                stride: w * 4,
            }];
            let system = SystemFrame::new(format, Bytes::from(data), &planes)
                .map_err(|_| RigError::Failed)?;
            Ok(VideoFrame::system(Timestamp::ZERO, system, None))
        })
        .collect()
}

struct ClassGraph {
    ctl: Controller<GpuRuntime>,
    exec: Executor<GpuRuntime>,
    nodes: Vec<NodeId>,
    stats: Arc<ExportNodeStats>,
}

/// The real [`ExportRig`]: a throwaway executor of the take's track nodes
/// (`Normalise`, `VideoExportNode`) on `ctx`, a device of its own
/// ([`GpuContext::sibling`]), ticked on the calling thread.
pub struct GpuExportRig {
    ctx: Arc<GpuContext>,
    cadence: Cadence,
    index: u64,
    graph: Option<ClassGraph>,
}

impl GpuExportRig {
    /// A rig on `ctx` ticking at `rate`.
    pub fn new(ctx: Arc<GpuContext>, rate: FrameRate) -> Self {
        Self {
            ctx,
            cadence: Cadence::frames(Timestamp::ZERO, rate),
            index: 0,
            graph: None,
        }
    }

    fn device_lost(&self) -> bool {
        self.ctx.metrics().device_lost()
    }
}

fn add(
    graph: &mut GraphDescription,
    ctl: &mut Controller<GpuRuntime>,
    node: Box<dyn Node<GpuRuntime>>,
) -> Result<NodeId, RigError> {
    let id = graph
        .add_node(Arc::new(node.descriptor().clone()))
        .map_err(|_| RigError::Failed)?;
    graph
        .set_state(id, Lifecycle::Standby)
        .map_err(|_| RigError::Failed)?;
    graph
        .set_state(id, Lifecycle::Active)
        .map_err(|_| RigError::Failed)?;
    ctl.adopt(id, node).map_err(|_| RigError::Failed)?;
    Ok(id)
}

impl ExportRig for GpuExportRig {
    fn begin(&mut self, class: &ExportClass) -> Result<(), RigError> {
        self.end();
        let (w, h) = (class.width, class.height);
        let (mut ctl, mut exec) =
            channel::<GpuRuntime>(Arc::clone(&self.ctx), RenderLocal::default());
        let mut graph = GraphDescription::new();
        let feed = add(
            &mut graph,
            &mut ctl,
            Box::new(Feed {
                desc: NodeDescriptor::new(
                    NodeTypeId::new_static("org.rbs.calibration.feed"),
                    NodeKind::Source,
                    vec![],
                    vec![Pin::one("video", LinkKind::Video)],
                ),
                pending: moving_rgba(w, h, EXPORT_FEED_FRAMES)?,
                uploaded: Vec::new(),
                upload: SourceUpload::new(),
                next: 0,
            }),
        )?;
        let ty: Arc<dyn FilterType> = Arc::new(Normalise);
        let values = Values::defaults(ty.as_ref());
        let (normalise, _, _) = FilterNode::new(ty, values, Some([w, h]));
        let normalise = add(&mut graph, &mut ctl, Box::new(normalise))?;
        let export =
            VideoExportNode::with_tone(class.format, w, h, w, h, class.color, class.open_shadows)
                .every_tick();
        let stats = export.stats();
        let export = add(&mut graph, &mut ctl, Box::new(export))?;
        let sink = add(
            &mut graph,
            &mut ctl,
            Box::new(Sink {
                desc: NodeDescriptor::new(
                    NodeTypeId::new_static("org.rbs.calibration.sink"),
                    NodeKind::Output,
                    vec![Pin::one("in", LinkKind::Video)],
                    vec![],
                ),
            }),
        )?;
        graph
            .connect(feed, "video", normalise, "in")
            .map_err(|_| RigError::Failed)?;
        graph
            .connect(normalise, "out", export, "canvas")
            .map_err(|_| RigError::Failed)?;
        graph
            .connect(export, "video", sink, "in")
            .map_err(|_| RigError::Failed)?;
        let report = ctl.apply(&graph).map_err(|_| RigError::Failed)?;
        if !report.failed.is_empty() {
            return Err(if self.device_lost() {
                RigError::DeviceLost
            } else {
                RigError::Failed
            });
        }
        exec.attach();
        exec.swap_pending();
        let _ = ctl.collect();
        self.graph = Some(ClassGraph {
            ctl,
            exec,
            nodes: vec![feed, normalise, export],
            stats,
        });
        Ok(())
    }

    fn tick(&mut self) -> Result<(), RigError> {
        let g = self.graph.as_mut().ok_or(RigError::Failed)?;
        g.exec.run_tick(Tick::of(&self.cadence, self.index));
        self.index += 1;
        let _ = g.ctl.collect();
        if self.ctx.metrics().device_lost() {
            return Err(RigError::DeviceLost);
        }
        let failed = g.nodes.iter().any(|id| {
            g.exec
                .health(*id)
                .is_none_or(|h| h.errors > 0 || h.bypassed)
        });
        if failed {
            return Err(RigError::Failed);
        }
        Ok(())
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, RigError> {
        match self.ctx.wait_until(timeout) {
            Ok(done) => Ok(done),
            Err(_) if self.device_lost() => Err(RigError::DeviceLost),
            Err(_) => Err(RigError::Failed),
        }
    }

    fn counters(&self) -> ExportCounters {
        self.graph
            .as_ref()
            .map_or_else(ExportCounters::default, |g| ExportCounters {
                frames: g.stats.frames.load(Ordering::Relaxed),
                lost: g.stats.lost.load(Ordering::Relaxed),
                busy: g.stats.busy.load(Ordering::Relaxed),
            })
    }

    fn end(&mut self) {
        if let Some(g) = self.graph.take() {
            let ClassGraph { ctl, mut exec, .. } = g;
            exec.detach();
            exec.retire();
            drop(ctl);
        }
    }
}

impl Drop for GpuExportRig {
    fn drop(&mut self) {
        self.end();
    }
}
