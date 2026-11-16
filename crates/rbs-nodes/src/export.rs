//! The video export node: canvas frames to encoder-ready frames, on the render partition
//! (spec `docs/specs/io-nodes.md`, "Video export node").

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rbs_core::media::{
    ColorRange, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
    next_generation,
};
use rbs_core::pause::{HoldPicture, HoldSwitch, PauseWindow, Place, Window};
use rbs_core::time::Timestamp;
use rbs_gpu::download::{DownloadStats, YuvDownload};
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::{ScaleKernel, YuvFormat};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};

/// Node type id of the video export.
pub const VIDEO_EXPORT_NODE_TYPE: &str = "org.rbs.video-export";
/// Index of the "canvas" input pin.
pub const CANVAS_PIN: usize = 0;
/// Index of the "video" output pin.
pub const VIDEO_PIN: usize = 0;

/// The video export's descriptor: kind `VideoFilter`, input "canvas", output "video".
pub fn video_export_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(VIDEO_EXPORT_NODE_TYPE),
        NodeKind::VideoFilter,
        vec![Pin::one("canvas", LinkKind::Video)],
        vec![Pin::one("video", LinkKind::Video)],
    )
}

/// Counters of a video export node, readable from any thread.
#[derive(Debug, Default)]
pub struct ExportNodeStats {
    /// Frames put on the video output.
    pub frames: AtomicU64,
    /// Canvas frames not converted because every readback was in flight.
    pub busy: AtomicU64,
    /// Converted frames lost before delivery (stale, no free buffer, map failure, or
    /// still in flight when the node was released).
    pub lost: AtomicU64,
    /// Timestamp in nanoseconds plus one of the last frame put on "video"; 0 before
    /// the first (ADR-0045: the studio compares it with the encoder's).
    pub last_frame: AtomicU64,
    /// Canvas frames not converted because the previous render tick ran longer than
    /// the export's budget ([`VideoExportNode::give_way_after`], ADR-0086 decision 9).
    /// Each is a gap in the file, never a shifted timestamp.
    pub gave_way: AtomicU64,
}

/// Ends a video export's input at a master-clock time (ADR-0045); shared between the
/// node and the control thread.
///
/// Until [`ExportEnd::end_at`] is called the export takes every canvas. Afterwards it
/// takes only canvases up to that time, keeps passing on the readbacks still in flight,
/// and reports [`ExportEnd::ended`] once every canvas it took is on its output. The
/// encoder then gets the last one on its own next tick, or on its final tick.
///
/// It also carries the export's one run-path value, `next` (ADR-0124 decision 3): the
/// earliest of the end time and the next pause instant, `u64::MAX` when neither is
/// set. The run path loads only `next`; at or past it the export reads the end and its
/// pause window. Only the end sets [`ExportEnd::ended`] and [`ExportEnd::requested`],
/// so a paused export never looks ended.
#[derive(Debug)]
pub struct ExportEnd {
    /// Nanoseconds of the last canvas time taken; `u64::MAX` while not ending.
    at: AtomicU64,
    /// The earliest of `at` and the next pause instant; `u64::MAX` for neither.
    next: AtomicU64,
    ended: AtomicBool,
}

impl Default for ExportEnd {
    fn default() -> Self {
        Self {
            at: AtomicU64::new(u64::MAX),
            next: AtomicU64::new(u64::MAX),
            ended: AtomicBool::new(false),
        }
    }
}

impl ExportEnd {
    /// Takes no canvas later than `at`. An earlier call with an earlier time wins.
    pub fn end_at(&self, at: Timestamp) {
        // `u64::MAX` means "open", so the latest representable end is one below it.
        let at = at.as_nanos().min(u64::MAX - 1);
        self.at.fetch_min(at, Ordering::SeqCst);
        self.next.fetch_min(at, Ordering::SeqCst);
    }

    /// Makes the export read its pause window from `at` on (ADR-0124): called by the
    /// control thread after it wrote a window whose pause begins at `at`. Lowers the
    /// run-path value with a compare-and-swap, never above an end.
    pub fn wake_at(&self, at: Timestamp) {
        self.next
            .fetch_min(at.as_nanos().min(u64::MAX - 1), Ordering::SeqCst);
    }

    /// Raises the run-path value from `seen` to the end (or "open") once the export
    /// has left a pause window behind. A failed swap means the end or a new window
    /// lowered it meanwhile: the lower value stays.
    fn settle(&self, seen: u64) {
        let end = self.at.load(Ordering::SeqCst);
        if end > seen {
            let _ = self
                .next
                .compare_exchange(seen, end, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    /// The time asked for by [`ExportEnd::end_at`], if any.
    pub fn requested(&self) -> Option<Timestamp> {
        match self.at.load(Ordering::Relaxed) {
            u64::MAX => None,
            at => Some(Timestamp::from_nanos(at)),
        }
    }

    /// True once the export, asked to end, has put every canvas it took on its output.
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

/// A stream's hold as its export sees it (ADR-0124 decisions 12 and 13), shared with
/// the control thread.
///
/// While the switch is on, the export reads nothing back and sends a pinned picture
/// on each tick, stamped with the tick's time and a new sequence: the last frame it
/// sent ([`HoldPicture::Freeze`]) or the black frame the control thread put here
/// ([`HoldPicture::Black`]). The control thread turns the switch, then calls
/// [`ExportEnd::wake_at`] with [`Timestamp::ZERO`] so the export looks at it on its
/// next tick; the export goes back to its run path once the hold is over.
#[derive(Debug, Default)]
pub struct ExportHold {
    switch: Arc<HoldSwitch>,
    black: Mutex<Option<VideoFrame>>,
}

impl ExportHold {
    /// A hold following `switch` (the same switch the stream's audio encoder follows).
    pub fn new(switch: Arc<HoldSwitch>) -> Self {
        Self {
            switch,
            black: Mutex::new(None),
        }
    }

    /// The switch.
    pub fn switch(&self) -> &Arc<HoldSwitch> {
        &self.switch
    }

    /// Whether a black picture is ready for [`HoldPicture::Black`].
    pub fn has_black(&self) -> bool {
        self.black.lock().is_ok_and(|b| b.is_some())
    }

    /// Keeps `frame` as the black picture (made once by the control thread with
    /// [`black_frame`], never on the render thread).
    pub fn set_black(&self, frame: VideoFrame) {
        if let Ok(mut black) = self.black.lock() {
            *black = Some(frame);
        }
    }

    /// The black picture, without waiting: `None` when it is not there or the control
    /// thread holds the lock this instant.
    fn black(&self) -> Option<VideoFrame> {
        self.black.try_lock().ok()?.as_ref()?.share()
    }
}

/// A black picture in `format` (NV12 or P010, either range), in system memory as the
/// export's frames are. Allocates: for the control thread, before a black hold.
pub fn black_frame(format: VideoFormat) -> Option<VideoFrame> {
    let (w, h) = (format.size.width as usize, format.size.height as usize);
    let full = format.color.range == ColorRange::Full;
    let (luma, chroma): (u16, u16) = if full { (0, 512) } else { (64, 512) };
    let data = match format.pixel_format {
        PixelFormat::Nv12 => {
            let y = (luma >> 2) as u8;
            let c = (chroma >> 2) as u8;
            let mut d = vec![y; w * h];
            d.resize(w * h + w * h.div_ceil(2), c);
            d
        }
        PixelFormat::P010 => {
            let mut d = Vec::with_capacity((w * h + w * h.div_ceil(2)) * 2);
            for _ in 0..w * h {
                d.extend_from_slice(&(luma << 6).to_le_bytes());
            }
            for _ in 0..w * h.div_ceil(2) {
                d.extend_from_slice(&(chroma << 6).to_le_bytes());
            }
            d
        }
        _ => return None,
    };
    let sample = if format.pixel_format == PixelFormat::P010 {
        2
    } else {
        1
    };
    let stride = w * sample;
    let planes = [
        PlaneLayout { offset: 0, stride },
        PlaneLayout {
            offset: w * h * sample,
            stride,
        },
    ];
    let frame = SystemFrame::new(format, data.into(), &planes).ok()?;
    Some(VideoFrame::system(Timestamp::ZERO, frame, None))
}

/// A hold under way, as the export keeps it.
struct Held {
    picture: HoldPicture,
    /// The program's last frame, pinned when the hold began (taken out of the ring,
    /// no copy): what `freeze` sends, kept while `black` is sent so a change back to
    /// `freeze` has it.
    frozen: Option<VideoFrame>,
    /// The previous tick's time. On the run path a canvas leaves the export one tick
    /// after it was drawn, so a held frame carries the previous tick's time and the
    /// timestamps run on without a gap or a repeat on both edges of the hold.
    last_tick: Timestamp,
}

impl Held {
    /// The picture to send now, shared (no copy).
    fn picture(&self, hold: Option<&ExportHold>) -> Option<VideoFrame> {
        match self.picture {
            HoldPicture::Freeze => self.frozen.as_ref()?.share(),
            HoldPicture::Black => hold?.black(),
        }
    }
}

/// Converts the canvas on its input to NV12 or P010 for an encoder and puts the result
/// on "video", one or two ticks later.
///
/// Today the result is a system-memory frame read back without blocking
/// ([`YuvDownload`]): the encoders on Windows (NVENC, AMF) take only system memory.
/// Native GPU export (ADR-0025) replaces it per encoder once the encoder and the queue
/// between partitions can carry native frames (ADR-0038).
pub struct VideoExportNode {
    descriptor: NodeDescriptor,
    format: YuvFormat,
    /// Colour the planes are encoded with and the frames are tagged with.
    color: ColorSpace,
    /// Raises only the darkest tones before the Rec. 709 encode. The tag stays `color`.
    open_shadows: bool,
    /// Canvas the input frames are.
    canvas_width: u32,
    canvas_height: u32,
    /// Encoded picture. Equal to the canvas: the 1:1 download. Otherwise one scale.
    width: u32,
    height: u32,
    download: Option<YuvDownload>,
    last: Option<(Timestamp, u64)>,
    seen: DownloadStats,
    stats: Arc<ExportNodeStats>,
    end: Arc<ExportEnd>,
    /// An ISO track's rules (ADR-0086); `None` for the master's export, which then
    /// pays one branch per tick for them.
    track: Option<Box<Track>>,
    /// The recording's pause window (ADR-0124); `None` for an export that is never
    /// paused (a stream, the replay buffer, the virtual camera).
    pause: Option<Arc<PauseWindow>>,
    /// The last window read; kept when a read finds a write in progress.
    window: Window,
    /// Nanoseconds every frame is stamped earlier by: the time left out by the pauses
    /// already behind the export. 0 for an export never paused.
    shift: u64,
    /// A stream's hold (ADR-0124 decision 13); `None` for an export never held.
    hold: Option<Arc<ExportHold>>,
    /// The hold under way, from its first held tick to the first tick after it.
    held: Option<Held>,
}

/// How an ISO track's export differs from the master's (ADR-0086 decisions 3 and 9).
/// How far a track's hold grows when the master keeps dropping frames.
const MAX_HOLD_FACTOR: u64 = 8;

/// Reads how many frames the master's encoders dropped so far.
pub type MasterDrops = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Default)]
struct Track {
    /// Nanoseconds of render tick time after which a canvas is skipped; `u64::MAX`
    /// never skips.
    give_way_ns: u64,
    /// Takes the canvas on every tick, stamped with the tick.
    every_tick: bool,
    /// The frames the master's encoders dropped so far (an only-growing count): when
    /// it grows, this track skips canvases for `hold_ticks` ticks.
    master_drops: Option<MasterDrops>,
    hold_ticks: u64,
    /// The hold now: `hold_ticks`, doubled (up to `MAX_HOLD_FACTOR` times) each time
    /// the master drops again within `hold_ticks` of the last hold's end, so a track
    /// that keeps pushing shared hardware past its limit backs off further.
    hold: u64,
    /// The masters' drops seen so far, and the first tick index converted again.
    seen_drops: u64,
    hold_until: u64,
}

impl Track {
    /// Whether this tick gives way: after a long tick, or while the master's encoder
    /// lately dropped a frame. Atomics only; no allocation.
    fn gives_way(&mut self, tick: &rbs_graph::runtime::Tick) -> bool {
        let previous_ns = u64::try_from(tick.previous_busy.as_nanos()).unwrap_or(u64::MAX);
        if let Some(master_drops) = &self.master_drops {
            let drops = master_drops();
            if drops > self.seen_drops {
                self.seen_drops = drops;
                let soon = tick.index < self.hold_until.saturating_add(self.hold_ticks);
                self.hold = if soon && self.hold_until > 0 {
                    (self.hold * 2).min(self.hold_ticks * MAX_HOLD_FACTOR)
                } else {
                    self.hold_ticks
                };
                self.hold_until = tick.index.saturating_add(self.hold);
            }
        }
        previous_ns > self.give_way_ns || tick.index < self.hold_until
    }
}

impl VideoExportNode {
    /// An export of `canvas_width x canvas_height` canvases to `width x height`
    /// pictures (all even) in `format`, Rec. 709 limited range.
    ///
    /// Equal sizes are the 1:1 download. A different size scales once from the canvas
    /// (ADR-0102).
    pub fn new(
        format: YuvFormat,
        canvas_width: u32,
        canvas_height: u32,
        width: u32,
        height: u32,
    ) -> Self {
        Self::with_color(
            format,
            canvas_width,
            canvas_height,
            width,
            height,
            ColorSpace::BT709_LIMITED,
        )
    }

    /// An export that encodes and tags frames as `color`.
    ///
    /// `YuvDownload` accepts [`ColorSpace::BT709_LIMITED`] (the default) and
    /// [`ColorSpace::SRGB_VIDEO`] (sRGB, full range). Anything else fails in `prepare`.
    /// `width x height` is the encoded picture. Equal to the canvas: 1:1.
    pub fn with_color(
        format: YuvFormat,
        canvas_width: u32,
        canvas_height: u32,
        width: u32,
        height: u32,
        color: ColorSpace,
    ) -> Self {
        Self::with_tone(
            format,
            canvas_width,
            canvas_height,
            width,
            height,
            color,
            false,
        )
    }

    /// [`Self::with_color`] with the open-shadows grade. The frames stay tagged `color`.
    /// The grade applies only to Rec. 709 limited range and fails in `prepare` otherwise.
    #[allow(clippy::too_many_arguments)]
    pub fn with_tone(
        format: YuvFormat,
        canvas_width: u32,
        canvas_height: u32,
        width: u32,
        height: u32,
        color: ColorSpace,
        open_shadows: bool,
    ) -> Self {
        Self {
            descriptor: video_export_descriptor(),
            format,
            color,
            open_shadows,
            canvas_width,
            canvas_height,
            width,
            height,
            download: None,
            last: None,
            seen: DownloadStats::default(),
            stats: Arc::default(),
            end: Arc::default(),
            track: None,
            pause: None,
            window: Window::NONE,
            shift: 0,
            hold: None,
            held: None,
        }
    }

    /// An export that a stream's hold can take over (ADR-0124 decision 13). Nothing
    /// on the run path: the export looks at the hold only after
    /// [`ExportEnd::wake_at`].
    #[must_use]
    pub fn with_hold(mut self, hold: Arc<ExportHold>) -> Self {
        self.hold = Some(hold);
        self
    }

    /// An export that follows `pause` (ADR-0124 decision 4): it takes no canvas inside
    /// a window and stamps later frames earlier by the time left out. The control
    /// thread writes the window, then calls [`ExportEnd::wake_at`] with its start.
    #[must_use]
    pub fn with_pause(mut self, pause: Arc<PauseWindow>) -> Self {
        self.pause = Some(pause);
        self
    }

    fn track(&mut self) -> &mut Track {
        self.track.get_or_insert_with(|| {
            Box::new(Track {
                give_way_ns: u64::MAX,
                ..Track::default()
            })
        })
    }

    /// An export that gives way to the master (ADR-0086 decision 9): when the render
    /// partition's previous tick ran longer than `budget`, this tick's canvas is not
    /// converted and [`ExportNodeStats::gave_way`] counts it. Frames already read back
    /// are still passed on. For ISO tracks only; the master never gives way.
    #[must_use]
    pub fn give_way_after(mut self, budget: std::time::Duration) -> Self {
        self.track().give_way_ns = u64::try_from(budget.as_nanos()).unwrap_or(u64::MAX);
        self
    }

    /// An export that also gives way while the master's encoders fall behind (ADR-0086
    /// decision 9): when `master_drops` (frames dropped on the way into the master's
    /// video encoders so far, for example the sum of their queues'
    /// `QueueStats::video_drops`) grows, this export skips its canvases for the next
    /// `hold_ticks` ticks, counted in [`ExportNodeStats::gave_way`]. A drop within
    /// `hold_ticks` after a hold ended doubles the next hold, up to eight times
    /// `hold_ticks`; a drop later starts again from `hold_ticks`. Encoders that
    /// share hardware (one media engine for every session) then lose frames on the
    /// ISO tracks first. Drops before this call do not count. Called once per tick on
    /// the render thread: it must only read atomics.
    #[must_use]
    pub fn yield_to(mut self, master_drops: MasterDrops, hold_ticks: u64) -> Self {
        let track = self.track();
        track.seen_drops = master_drops();
        track.master_drops = Some(master_drops);
        track.hold_ticks = hold_ticks;
        track.hold = hold_ticks;
        self
    }

    /// An export that converts its input on every tick and stamps the frame with the
    /// tick's time and index, so a picture that does not change (a still, a paused
    /// source, a 30 fps camera on a 60 fps canvas) still gives one frame per tick
    /// (ADR-0086: a separate file at the canvas frame rate). For ISO tracks only: the
    /// master's canvas is new on every tick anyway.
    #[must_use]
    pub fn every_tick(mut self) -> Self {
        self.track().every_tick = true;
        self
    }

    /// The format of the frames on "video": the encoder's input format (the output size).
    pub fn frame_format(&self) -> VideoFormat {
        VideoFormat::new(
            Size::new(self.width, self.height),
            self.format.pixel_format(),
            self.color,
        )
    }

    /// The kernel [`Self::prepare`] builds.
    ///
    /// Before prepare this is [`rbs_gpu::yuv::scale_kernel`] of the canvas and the
    /// output. After, it is the download's converter (`YuvConverter::kernel`).
    pub fn kernel(&self) -> ScaleKernel {
        match &self.download {
            Some(download) => download.kernel(),
            None => rbs_gpu::yuv::scale_kernel(
                self.canvas_width,
                self.canvas_height,
                self.width,
                self.height,
            ),
        }
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<ExportNodeStats> {
        Arc::clone(&self.stats)
    }

    /// The handle that ends the export's input (ADR-0045).
    pub fn end(&self) -> Arc<ExportEnd> {
        Arc::clone(&self.end)
    }
}

/// `t` stamped earlier by a kept place's shift.
fn shifted(t: Timestamp, place: Place) -> Timestamp {
    match place {
        Place::Kept(shift) => Timestamp::from_nanos(t.as_nanos().saturating_sub(shift)),
        Place::Cut => t,
    }
}

impl VideoExportNode {
    /// Reads the pause window on the slow path. Returns the window to place this
    /// tick's canvas with, and, once the export has left the window behind, takes its
    /// shift and raises `next` again (ADR-0124 decision 3).
    fn read_window(&mut self, ctx_time: u64, seen_next: u64) -> Option<Window> {
        let pause = self.pause.as_ref()?;
        if let Some(w) = pause.read() {
            self.window = w;
        }
        let w = self.window;
        if w.is_over_at(ctx_time) {
            self.shift = w.total();
            self.end.settle(seen_next);
            // A window written after this read lowers `next` itself (`wake_at`); one
            // written between the read and the swap is caught here.
            if let Some(again) = pause.read()
                && again.epoch != w.epoch
                && !again.is_over_at(ctx_time)
            {
                self.end.wake_at(Timestamp::from_nanos(again.pause_at));
            }
        }
        Some(w)
    }
}

impl VideoExportNode {
    /// One tick while held: nothing is converted; readbacks still in flight finish.
    /// On the hold's first tick the newest frame goes out as on the run path and is
    /// pinned as the frozen picture; from then on the hold picture goes out each
    /// tick with the previous tick's time and a new sequence (ADR-0124 decision 13).
    fn hold_tick(
        &mut self,
        ctx: &mut ProcessCtx<'_, GpuRuntime>,
        picture: HoldPicture,
        end: u64,
    ) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let gpu = ctx.shared();
        let (_, outputs, _) = ctx.parts();
        let download = self.download.as_mut().ok_or(NodeError::NotReady)?;
        let mut result = Ok(());
        let ready = match download.take_ready(gpu) {
            Ok(frame) => frame,
            Err(_) => {
                result = Err(NodeError::Backend("video export readback failed"));
                None
            }
        };
        let send = match self.held.as_mut() {
            Some(held) => {
                held.picture = picture;
                let frame = held.picture(self.hold.as_deref());
                let at = held.last_tick;
                held.last_tick = tick.time;
                // A late readback finishing now is not sent: the hold is on.
                drop(ready);
                frame.map(|f| f.with_timestamp(at).with_sequence(next_generation()))
            }
            None => {
                // The newest frame of the program, else the one sent last.
                let frozen = match ready.as_ref() {
                    Some(f) => f.share(),
                    None => outputs.set_video(VIDEO_PIN, None)?,
                };
                self.held = Some(Held {
                    picture,
                    frozen,
                    last_tick: tick.time,
                });
                ready
            }
        };
        if let Some(frame) = send {
            send_frame(outputs, frame, &self.stats)?;
        }
        if end != u64::MAX
            && tick.time.as_nanos() > end
            && download.in_flight() == 0
            && !self.end.ended()
        {
            self.end.ended.store(true, Ordering::Release);
        }
        result
    }
}

/// Puts `frame` on the export's output and counts it.
fn send_frame(
    outputs: &mut rbs_graph::runtime::Outputs<'_>,
    frame: VideoFrame,
    stats: &ExportNodeStats,
) -> Result<(), NodeError> {
    let at = frame.timestamp().as_nanos().saturating_add(1);
    // Dropping the previous frame hands its buffer back for reuse.
    drop(outputs.set_video(VIDEO_PIN, Some(frame))?);
    stats.frames.fetch_add(1, Ordering::Relaxed);
    stats.last_frame.store(at, Ordering::Relaxed);
    Ok(())
}

impl Node<GpuRuntime> for VideoExportNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Equal size: the 1:1 `textureLoad` path. A different size: one scaled
        // download from this canvas (ADR-0102). The two outputs of one canvas each
        // read that canvas; neither reads the other's planes.
        let same = self.canvas_width == self.width && self.canvas_height == self.height;
        let download = if same {
            YuvDownload::with_tone(
                ctx.shared,
                self.format,
                self.width,
                self.height,
                self.color,
                self.open_shadows,
            )
        } else {
            YuvDownload::scaled_tone(
                ctx.shared,
                self.format,
                self.canvas_width,
                self.canvas_height,
                self.width,
                self.height,
                rbs_gpu::download::DOWNLOAD_BUFFERS,
                self.color,
                self.open_shadows,
            )
        }
        .map_err(|e| NodeError::Failed(format!("video export: {e}")))?;
        self.seen = download.stats();
        self.download = Some(download);
        self.last = None;
        // A restarted export has nothing in flight; it reports the end again once it
        // has passed on what it takes from now on.
        self.end.ended.store(false, Ordering::Release);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        let ctx_time = tick.time.as_nanos();
        // The master's export: one branch, then neither rule applies.
        let (give_way, every_tick) = match self.track.as_deref_mut() {
            None => (false, false),
            Some(track) => (track.gives_way(&tick), track.every_tick),
        };
        // `u64::MAX` (the run path) takes every canvas: one load and one compare. Set,
        // the end and the pause window are read (ADR-0124 decision 3).
        let next = self.end.next.load(Ordering::Acquire);
        let mut released = None;
        let (end, window) = if next == u64::MAX {
            (u64::MAX, None)
        } else {
            let end = self.end.at.load(Ordering::SeqCst);
            // A stream's hold (ADR-0124 decision 13) comes before everything else.
            if let Some(picture) = self.hold.as_ref().and_then(|h| h.switch.held()) {
                return self.hold_tick(ctx, picture, end);
            }
            released = self.held.take();
            if released.is_some() && self.pause.is_none() {
                // The hold is over: back to the run path (a pause window settles in
                // `read_window` once it is over).
                self.end.settle(next);
                // A hold taken again between the read above and the swap keeps the
                // slow path (its `wake_at` found `next` already lowered).
                if self.hold.as_ref().is_some_and(|h| h.switch.is_held()) {
                    self.end.wake_at(Timestamp::ZERO);
                }
            }
            (end, self.read_window(ctx_time, next))
        };
        let gpu = ctx.shared();
        let (inputs, outputs, local) = ctx.parts();
        let download = self.download.as_mut().ok_or(NodeError::NotReady)?;

        // Frames submitted on earlier ticks first, so their staging buffers are free.
        // Ending, every frame is passed on in order (ADR-0045); otherwise the newest.
        let ready = if end == u64::MAX {
            download.take_ready(gpu)
        } else {
            download.take_oldest(gpu)
        };
        let mut result = Ok(());
        match ready {
            Ok(Some(frame)) => send_frame(outputs, frame, &self.stats)?,
            Ok(None) => {
                // The first tick after a hold: nothing is read back yet, so the hold
                // picture goes out once more, with the previous tick's time.
                if let Some(frame) = released.and_then(|h| {
                    let at = h.last_tick;
                    h.picture(self.hold.as_deref())
                        .map(|f| f.with_timestamp(at))
                }) {
                    send_frame(outputs, frame.with_sequence(next_generation()), &self.stats)?;
                }
            }
            Err(_) => result = Err(NodeError::Backend("video export readback failed")),
        }

        if let Some(canvas) = inputs.video(CANVAS_PIN, 0) {
            // An ISO track stamps the tick, so it takes the same picture again.
            let key = if every_tick {
                (tick.time, tick.index)
            } else {
                (canvas.timestamp(), canvas.sequence())
            };
            let place = match window {
                Some(w) => w.place(key.0.as_nanos()),
                None => Place::Kept(self.shift),
            };
            if self.last != Some(key) && key.0.as_nanos() <= end {
                self.last = Some(key);
                if place == Place::Cut {
                    // Inside a pause window: nothing is converted (ADR-0124).
                } else if give_way {
                    self.stats.gave_way.fetch_add(1, Ordering::Relaxed);
                } else if download
                    .submit_at(gpu, &local.pool, canvas, shifted(key.0, place), key.1)
                    .is_err()
                {
                    result = Err(NodeError::Backend("video export conversion failed"));
                }
            }
        }
        // Ending (ADR-0045): done on a tick past the end once nothing taken is still
        // being read back (a tick at or before the end may still bring a canvas).
        // Readbacks that fail, are busy or find no buffer are counted as on the run path.
        if end != u64::MAX && ctx_time > end && download.in_flight() == 0 && !self.end.ended() {
            self.end.ended.store(true, Ordering::Release);
        }

        let now = download.stats();
        let lost = (now.stale - self.seen.stale)
            + (now.no_buffer - self.seen.no_buffer)
            + (now.failed - self.seen.failed);
        self.stats
            .busy
            .fetch_add(now.busy - self.seen.busy, Ordering::Relaxed);
        self.stats.lost.fetch_add(lost, Ordering::Relaxed);
        self.seen = now;
        result
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.held = None;
        // Readbacks still in flight never reach the encoder (a stop without
        // `ExportEnd`, or past the studio's tail limit): counted, never silent.
        if let Some(download) = self.download.take() {
            self.stats
                .lost
                .fetch_add(download.in_flight() as u64, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::media::FramePayload;
    use rbs_graph::runtime::Tick;
    use std::time::Duration;

    #[test]
    #[allow(clippy::expect_used)] // Tests fail by panicking.
    fn the_black_hold_picture_is_black_in_either_range_and_depth() {
        let system = |f: &VideoFrame| match f.payload() {
            FramePayload::System(s) => s.data().to_vec(),
            _ => panic!("a system frame"),
        };
        for (yuv, range, luma, chroma) in [
            (YuvFormat::Nv12, ColorRange::Limited, 16u16, 128u16),
            (YuvFormat::Nv12, ColorRange::Full, 0, 128),
            (YuvFormat::P010, ColorRange::Limited, 64 << 6, 512 << 6),
            (YuvFormat::P010, ColorRange::Full, 0, 512 << 6),
        ] {
            let mut format = VideoExportNode::new(yuv, 6, 4, 6, 4).frame_format();
            format.color.range = range;
            let frame = black_frame(format).expect("a black frame");
            let data = system(&frame);
            let wide = yuv == YuvFormat::P010;
            let at = |i: usize| {
                if wide {
                    u16::from_le_bytes([data[2 * i], data[2 * i + 1]])
                } else {
                    u16::from(data[i])
                }
            };
            let (y, c) = (6 * 4, 6 * 2);
            assert_eq!(data.len(), (y + c) * if wide { 2 } else { 1 });
            assert!((0..y).all(|i| at(i) == luma), "{yuv:?} {range:?}");
            assert!((y..y + c).all(|i| at(i) == chroma), "{yuv:?} {range:?}");
        }
        let mut rgba = VideoExportNode::new(YuvFormat::Nv12, 6, 4, 6, 4).frame_format();
        rgba.pixel_format = PixelFormat::Rgba8;
        assert!(black_frame(rgba).is_none());
    }

    #[test]
    #[allow(clippy::expect_used)] // Tests fail by panicking.
    fn a_track_gives_way_after_a_long_tick_and_while_the_master_drops() {
        let drops = Arc::new(AtomicU64::new(5));
        let read = Arc::clone(&drops);
        let export = VideoExportNode::new(YuvFormat::Nv12, 16, 8, 16, 8)
            .give_way_after(Duration::from_millis(8))
            .yield_to(Arc::new(move || read.load(Ordering::Relaxed)), 3);
        let mut track = export.track.expect("an ISO track");
        let at = |index: u64, busy_ms: u64| {
            Tick::new(index, Timestamp::from_nanos(index * 1_000))
                .after(Duration::from_millis(busy_ms))
        };
        // Drops before `yield_to` do not count; a tick at the budget converts.
        assert!(!track.gives_way(&at(0, 8)));
        assert!(track.gives_way(&at(1, 9)), "after a long tick");
        assert!(!track.gives_way(&at(2, 0)));
        drops.fetch_add(1, Ordering::Relaxed);
        // A master drop seen on tick 3 holds ticks 3, 4 and 5.
        assert!(track.gives_way(&at(3, 0)));
        assert!(track.gives_way(&at(4, 0)));
        assert!(track.gives_way(&at(5, 0)));
        assert!(!track.gives_way(&at(6, 0)));
        // Dropping again soon after (within 3 ticks of the end) doubles the hold.
        drops.fetch_add(1, Ordering::Relaxed);
        assert!(track.gives_way(&at(7, 0)));
        assert!(track.gives_way(&at(12, 0)));
        assert!(!track.gives_way(&at(13, 0)), "held 6 ticks: 7 to 12");
        // A drop long after starts again from 3.
        drops.fetch_add(1, Ordering::Relaxed);
        assert!(track.gives_way(&at(100, 0)));
        assert!(track.gives_way(&at(102, 0)));
        assert!(!track.gives_way(&at(103, 0)));
        assert!(
            VideoExportNode::new(YuvFormat::Nv12, 16, 8, 16, 8)
                .track
                .is_none()
        );
    }

    #[test]
    fn the_default_tag_is_bt709_and_screen_is_srgb_video() {
        let video = VideoExportNode::new(YuvFormat::Nv12, 16, 8, 16, 8);
        assert_eq!(video.frame_format().color, ColorSpace::BT709_LIMITED);
        assert_eq!(video.frame_format().size, Size::new(16, 8));
        assert_eq!(video.kernel(), ScaleKernel::OneToOne);
        let screen =
            VideoExportNode::with_color(YuvFormat::Nv12, 16, 8, 16, 8, ColorSpace::SRGB_VIDEO);
        assert_eq!(screen.frame_format().color, ColorSpace::SRGB_VIDEO);
        assert_eq!(
            screen.frame_format().color.range,
            rbs_core::media::ColorRange::Full
        );
        let opened = VideoExportNode::with_tone(
            YuvFormat::Nv12,
            16,
            8,
            16,
            8,
            ColorSpace::BT709_LIMITED,
            true,
        );
        assert_eq!(opened.frame_format().color, ColorSpace::BT709_LIMITED);
        assert!(opened.open_shadows);
    }

    #[test]
    fn the_kernel_follows_the_canvas_and_the_output() {
        let one = VideoExportNode::new(YuvFormat::Nv12, 64, 36, 64, 36);
        assert_eq!(one.kernel(), ScaleKernel::OneToOne);
        assert_eq!(one.frame_format().size, Size::new(64, 36));

        // Integer 2× downscale: box. A non-integer downscale, including 1080p to 720p,
        // stays bilinear. An upscale is Catmull-Rom. A different aspect is not refused
        // here; the scaled download letterboxes it.
        let half = VideoExportNode::new(YuvFormat::Nv12, 64, 36, 32, 18);
        assert_eq!(half.kernel(), ScaleKernel::Box(2));
        assert_eq!(half.frame_format().size, Size::new(32, 18));
        let hd = VideoExportNode::new(YuvFormat::Nv12, 1920, 1080, 1280, 720);
        assert_eq!(hd.kernel(), ScaleKernel::Bilinear);
        let up = VideoExportNode::new(YuvFormat::Nv12, 1280, 720, 1920, 1080);
        assert_eq!(up.kernel(), ScaleKernel::CatmullRom);
        let bars = VideoExportNode::new(YuvFormat::Nv12, 64, 36, 64, 64);
        assert_eq!(bars.kernel(), ScaleKernel::Bilinear);
    }
}
