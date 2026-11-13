//! Media-file source node (`org.rbs.media`, ADR-0108).
//!
//! The descriptor always has one video pin and one audio pin. A file with no audio
//! stream still emits silence. Pins do not change after a probe.
//!
//! FFmpeg adapter not linked in this tree yet (ADR-0007). The worker enqueues
//! straight-alpha sRGB `Rgba8` system frames from the decode port. The render executor
//! uploads them with [`SourceUpload`] before `process` publishes a texture (the
//! conformance suite has no device and tests dequeue without uploading). NV12 and P010
//! are checked with `check_color` and then refused here until worker-side YUV upload
//! lands, so this node does not grow a second YUV decoder.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rbs_core::media::{
    ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::{Clock, Timestamp};
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_gpu::yuv_decode::check_color;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, RestartPolicy, WarmCtx};
use rbs_media_decode::{
    AUDIO_LOOKAHEAD, DecodeError, MAX_FRAME_BYTES, MediaDecoder, MediaDecoderFactory, MediaSample,
    PictureLayout, SampleColor, VIDEO_LOOKAHEAD, refuse_picture,
};

/// Node type id. Every media source instance uses this descriptor.
pub const MEDIA_NODE_TYPE: &str = "org.rbs.media";
/// Video output pin.
pub const VIDEO_PIN: usize = 0;
/// Audio output pin. Always present (ADR-0108 decision 2).
pub const AUDIO_PIN: usize = 1;

/// The one descriptor in `node_types()` for `org.rbs.source.media`.
pub fn media_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(MEDIA_NODE_TYPE),
        NodeKind::Source,
        Vec::new(),
        vec![
            Pin::one("video", LinkKind::Video),
            Pin::one("audio", LinkKind::Audio),
        ],
    )
}

/// Counters. Uploads, dropped video, repeated video, audio underruns.
#[derive(Debug, Default)]
pub struct MediaStats {
    /// Pictures the worker handed to the lookahead queue.
    pub uploaded: AtomicU64,
    /// Pictures skipped because a later frame was already due.
    pub dropped_video: AtomicU64,
    /// Ticks that showed the same picture again.
    pub repeated_video: AtomicU64,
    /// Audio ticks that wrote silence.
    pub audio_underruns: AtomicU64,
}

/// Why wake failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaNodeError {
    /// Settings or a picture the host must refuse (`InvalidSetting`).
    Invalid(&'static str),
    /// Open or decode failed (`SourceError::Media`).
    Failed(String),
}

impl std::fmt::Display for MediaNodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MediaNodeError::Invalid(text) => f.write_str(text),
            MediaNodeError::Failed(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for MediaNodeError {}

/// Where playback is. Standby holds no decoder and no queues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPhase {
    /// No decoder, no queues. Cursor, loop, and restart are kept.
    Standby,
    /// Worker running, queues allocated.
    Playing,
    /// End of file with `loop` false. Last picture kept, silence, no worker.
    Hold,
}

struct Pending {
    frame: VideoFrame,
    start: Duration,
    end: Duration,
    index: u32,
    mark: u8,
    bytes: usize,
}

struct VideoQueues {
    video: rtrb::Consumer<Pending>,
    audio: rtrb::Consumer<Arc<[f32]>>,
}

struct Worker {
    join: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

/// Plays one local file on the master clock.
///
/// `process` only dequeues. The worker is joined from [`MediaSourceNode::service`]
/// and [`MediaSourceNode::standby`], never from `process`.
pub struct MediaSourceNode {
    descriptor: NodeDescriptor,
    factory: Arc<dyn MediaDecoderFactory>,
    path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
    loop_flag: Arc<AtomicBool>,
    restart_when_shown: bool,
    cursor: Duration,
    origin: Timestamp,
    sample_rate: u32,
    channels: u16,
    block_frames: u32,
    phase: MediaPhase,
    queues: Option<VideoQueues>,
    worker: Option<Worker>,
    pending: Option<Pending>,
    held: Option<VideoFrame>,
    held_mark: u8,
    published: Option<u32>,
    has_audio: bool,
    queue_allocations: u32,
    decoder_held: Arc<AtomicBool>,
    enqueued: Arc<AtomicU32>,
    queued_frames: Arc<AtomicU32>,
    queued_bytes: Arc<AtomicU64>,
    ended: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    attempts: u32,
    retry_after: Option<Duration>,
    stats: Arc<MediaStats>,
    upload: SourceUpload,
    /// Last picture already on the GPU, reused while the clock repeats a frame.
    shown_texture: Option<VideoFrame>,
}

impl MediaSourceNode {
    /// A standby node. Nothing is opened and no queue is allocated.
    // Each argument is a distinct source setting from the scene; a builder would only move them.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        factory: Arc<dyn MediaDecoderFactory>,
        path: std::path::PathBuf,
        loop_playback: bool,
        restart_when_shown: bool,
        clock: Arc<dyn Clock>,
        sample_rate: u32,
        channels: u16,
        block_frames: u32,
    ) -> Self {
        Self {
            descriptor: media_descriptor(),
            factory,
            path,
            clock,
            loop_flag: Arc::new(AtomicBool::new(loop_playback)),
            restart_when_shown,
            cursor: Duration::ZERO,
            origin: Timestamp::ZERO,
            sample_rate,
            channels: channels.max(1),
            block_frames: block_frames.max(1),
            phase: MediaPhase::Standby,
            queues: None,
            worker: None,
            pending: None,
            held: None,
            held_mark: 0,
            published: None,
            has_audio: false,
            queue_allocations: 0,
            decoder_held: Arc::new(AtomicBool::new(false)),
            enqueued: Arc::new(AtomicU32::new(0)),
            queued_frames: Arc::new(AtomicU32::new(0)),
            queued_bytes: Arc::new(AtomicU64::new(0)),
            ended: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            attempts: 0,
            retry_after: None,
            stats: Arc::new(MediaStats::default()),
            upload: SourceUpload::new(),
            shown_texture: None,
        }
    }

    /// Stored `loop`. A change does not replace the node. While the worker is playing
    /// it takes effect at the next end of file. While held, `true` starts from cursor 0.
    pub fn set_loop(&mut self, on: bool) {
        self.loop_flag.store(on, Ordering::Release);
        if on && self.phase == MediaPhase::Hold {
            self.cursor = Duration::ZERO;
            let now = self.clock.now();
            let _ = self.wake(now);
        }
    }

    /// Stored until the next wake. Not applied to a worker that is already playing.
    pub fn set_restart_when_shown(&mut self, on: bool) {
        self.restart_when_shown = on;
    }

    /// `loop` as stored now.
    pub fn loops(&self) -> bool {
        self.loop_flag.load(Ordering::Acquire)
    }

    /// Media cursor kept with no decoder while Standby.
    pub fn cursor(&self) -> Duration {
        self.cursor
    }

    /// Playback phase.
    pub fn phase(&self) -> MediaPhase {
        self.phase
    }

    /// Times a queue pair was allocated. Stays 0 while Standby and when a picture is
    /// refused.
    pub fn queue_allocations(&self) -> u32 {
        self.queue_allocations
    }

    /// True while the worker holds an open decoder.
    pub fn decoder_held(&self) -> bool {
        self.decoder_held.load(Ordering::Acquire)
    }

    /// True while the lookahead queues exist.
    pub fn queues_live(&self) -> bool {
        self.queues.is_some()
    }

    /// Pictures the worker has enqueued.
    pub fn enqueued(&self) -> u32 {
        self.enqueued.load(Ordering::Acquire)
    }

    /// The worker has returned (it may not be joined yet).
    pub fn worker_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// True after [`Self::service`] or [`Self::standby`] has joined the thread.
    pub fn worker_joined(&self) -> bool {
        self.worker.is_none()
    }

    /// First byte of the picture last chosen, when there is one.
    pub fn last_mark(&self) -> Option<u8> {
        self.published.map(|_| self.held_mark)
    }

    /// Failures in a row after a mid-play fault. Back-off uses ADR-0024's default curve.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Wait before the next wake after a fault, when the node is holding.
    pub fn retry_after(&self) -> Option<Duration> {
        self.retry_after
    }

    /// Counters.
    pub fn stats(&self) -> Arc<MediaStats> {
        Arc::clone(&self.stats)
    }

    /// Opens the file, seeks, and starts the worker. Control thread only.
    pub fn wake(&mut self, now: Timestamp) -> Result<(), MediaNodeError> {
        if self.phase == MediaPhase::Playing {
            return Ok(());
        }
        self.standby();
        let cursor = if self.restart_when_shown {
            Duration::ZERO
        } else {
            self.cursor
        };
        self.cursor = cursor;
        self.origin = now - cursor;
        self.ended.store(false, Ordering::Release);
        self.failed.store(false, Ordering::Release);
        self.enqueued.store(0, Ordering::Release);
        self.published = None;
        self.pending = None;
        self.held = None;

        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let factory = Arc::clone(&self.factory);
        let path = self.path.clone();
        let flag = Arc::clone(&self.decoder_held);
        let enqueued = Arc::clone(&self.enqueued);
        let queued_frames = Arc::clone(&self.queued_frames);
        let queued_bytes = Arc::clone(&self.queued_bytes);
        let ended = Arc::clone(&self.ended);
        let failed = Arc::clone(&self.failed);
        let loop_flag = Arc::clone(&self.loop_flag);
        let stop = Arc::clone(&shutdown);
        let stats = Arc::clone(&self.stats);
        let origin = self.origin;
        let channels = self.channels;
        let block_frames = self.block_frames;

        let join = thread::Builder::new()
            .name("rbs-media".into())
            .spawn(move || {
                let job = AssertUnwindSafe(|| {
                    worker(
                        factory,
                        path,
                        cursor,
                        origin,
                        channels,
                        block_frames,
                        flag,
                        enqueued,
                        queued_frames,
                        queued_bytes,
                        failed.clone(),
                        loop_flag,
                        stop,
                        stats,
                        tx,
                    )
                });
                if std::panic::catch_unwind(job).is_err() {
                    failed.store(true, Ordering::Release);
                }
                ended.store(true, Ordering::Release);
            })
            .map_err(|error| MediaNodeError::Failed(error.to_string()))?;

        let ready = rx.recv().unwrap_or(Err(MediaNodeError::Failed(
            "media worker stopped before it queued a picture".into(),
        )));
        match ready {
            Ok(queues) => {
                self.has_audio = queues.has_audio;
                self.queues = Some(VideoQueues {
                    video: queues.video,
                    audio: queues.audio,
                });
                self.queue_allocations = self.queue_allocations.saturating_add(1);
                self.worker = Some(Worker { join, shutdown });
                self.phase = MediaPhase::Playing;
                Ok(())
            }
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                let _ = join.join();
                self.phase = MediaPhase::Standby;
                Err(error)
            }
        }
    }

    /// Joins a worker that has already finished and drops the queues. Keeps the last
    /// picture. Control thread only. A fault counts toward back-off.
    pub fn service(&mut self) {
        if !self.ended.load(Ordering::Acquire) {
            return;
        }
        self.join_worker();
        if let Some(queues) = self.queues.as_mut() {
            while let Ok(item) = queues.video.pop() {
                self.pending = Some(item);
            }
        }
        if let Some(pending) = self.pending.take() {
            self.held_mark = pending.mark;
            self.cursor = pending.start;
            self.published = Some(pending.index);
            self.held = Some(pending.frame);
        }
        self.queues = None;
        if self.failed.load(Ordering::Acquire) {
            self.attempts = self.attempts.saturating_add(1);
            self.retry_after = Some(RestartPolicy::default().delay(self.attempts));
        }
        if self.phase == MediaPhase::Playing {
            self.phase = MediaPhase::Hold;
        }
    }

    /// Releases the worker, the decoder, and the queues. Keeps the cursor, `loop`,
    /// and `restart_when_shown`.
    pub fn standby(&mut self) {
        if let Some(worker) = self.worker.as_ref() {
            worker.shutdown.store(true, Ordering::Release);
        }
        self.join_worker();
        self.queues = None;
        self.pending = None;
        self.held = None;
        self.shown_texture = None;
        self.phase = MediaPhase::Standby;
        if let Some(worker) = self.worker.as_ref() {
            worker.shutdown.store(false, Ordering::Release);
        }
    }

    /// Chooses the picture for master time `now`. Does not allocate, block, or read
    /// the file. `None` before the first picture.
    pub fn dequeue(&mut self, now: Timestamp) -> Option<VideoFrame> {
        let media_now = now.saturating_since(self.origin);
        if self.phase == MediaPhase::Hold {
            self.stats.repeated_video.fetch_add(1, Ordering::Relaxed);
            return self.held.as_ref().and_then(VideoFrame::share);
        }
        if self.phase != MediaPhase::Playing {
            return None;
        }
        if self.pending.is_none() {
            self.pending = self.pop_video();
        }
        loop {
            let due = self
                .pending
                .as_ref()
                .is_some_and(|item| item.end <= media_now);
            if !due {
                break;
            }
            let Some(next) = self.pop_video() else {
                break;
            };
            if self
                .pending
                .as_ref()
                .is_some_and(|item| Some(item.index) != self.published)
            {
                self.stats.dropped_video.fetch_add(1, Ordering::Relaxed);
            }
            self.pending = Some(next);
        }
        let pending = self.pending.as_ref()?;
        if pending.start > media_now {
            return None;
        }
        if self.published == Some(pending.index) {
            self.stats.repeated_video.fetch_add(1, Ordering::Relaxed);
        } else {
            self.published = Some(pending.index);
            self.cursor = pending.start;
            self.held_mark = pending.mark;
        }
        pending.frame.share()
    }

    /// Chooses the picture for `now`, uploads when the index changes, and returns a
    /// texture the compositor can sample. `process` only publishes the result.
    fn dequeue_texture(
        &mut self,
        ctx: &mut ProcessCtx<'_, GpuRuntime>,
        now: Timestamp,
    ) -> Result<Option<VideoFrame>, NodeError> {
        let before = self.published;
        let Some(system) = self.dequeue(now) else {
            return Ok(None);
        };
        if before == self.published {
            return Ok(self.shown_texture.as_ref().and_then(VideoFrame::share));
        }
        let texture = self
            .upload
            .upload(ctx.shared(), &mut ctx.local().pool, system)
            .map_err(|_| NodeError::Backend("media picture upload failed"))?;
        self.shown_texture = Some(texture);
        Ok(self.shown_texture.as_ref().and_then(VideoFrame::share))
    }

    /// Writes this tick's audio into `interleaved`, or silence when the stream is
    /// missing, the queue is empty, or the clip is holding the last picture.
    pub fn write_audio(&mut self, interleaved: &mut [f32]) {
        if self.phase == MediaPhase::Standby {
            interleaved.fill(0.0);
            return;
        }
        let block = if self.has_audio && self.phase == MediaPhase::Playing {
            self.pop_audio()
        } else {
            None
        };
        if let Some(block) = block {
            let n = interleaved.len().min(block.len());
            interleaved[..n].copy_from_slice(&block[..n]);
            interleaved[n..].fill(0.0);
        } else {
            interleaved.fill(0.0);
            self.stats.audio_underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn pop_video(&mut self) -> Option<Pending> {
        let item = self
            .queues
            .as_mut()
            .and_then(|queues| queues.video.pop().ok())?;
        self.queued_frames.fetch_sub(1, Ordering::Relaxed);
        self.queued_bytes
            .fetch_sub(item.bytes as u64, Ordering::Relaxed);
        Some(item)
    }

    fn pop_audio(&mut self) -> Option<Arc<[f32]>> {
        self.queues
            .as_mut()
            .and_then(|queues| queues.audio.pop().ok())
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown.store(true, Ordering::Release);
            let _ = worker.join.join();
        }
    }
}

impl Drop for MediaSourceNode {
    fn drop(&mut self) {
        self.standby();
    }
}

impl Node<GpuRuntime> for MediaSourceNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        self.wake(self.clock.now())
            .map_err(|error| NodeError::Failed(error.to_string()))
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        if let Some(system) = self
            .held
            .as_ref()
            .and_then(VideoFrame::share)
            .or_else(|| self.pending.as_ref().and_then(|item| item.frame.share()))
        {
            let format = *system.format();
            self.upload
                .prepare_for(ctx.shared, &mut ctx.local.pool, &format)
                .map_err(|_| NodeError::Backend("media picture upload prepare failed"))?;
        }
        Ok(())
    }

    fn cool(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) {
        self.shown_texture = None;
        self.upload.release(&mut ctx.local.pool);
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Dequeue only. The slot buffers already exist; filling them does not allocate.
        let now = ctx.tick().time;
        if let Some(frame) = self.dequeue_texture(ctx, now)? {
            let _ = ctx.outputs().set_video(VIDEO_PIN, Some(frame))?;
        }
        if let Ok(mut audio) = ctx.outputs().audio(AUDIO_PIN) {
            let channels = usize::from(self.channels).min(rbs_core::media::MAX_CHANNELS);
            let frames = audio.capacity().min(self.block_frames as usize);
            let block = if self.has_audio && self.phase == MediaPhase::Playing {
                self.pop_audio()
            } else {
                None
            };
            if block.is_none() && self.phase != MediaPhase::Standby {
                self.stats.audio_underruns.fetch_add(1, Ordering::Relaxed);
            }
            for channel in 0..channels {
                if let Some(dest) = audio.channel_mut(channel) {
                    let dest = &mut dest[..frames];
                    if let Some(block) = block.as_ref() {
                        for (frame_index, sample) in dest.iter_mut().enumerate() {
                            let at = frame_index * channels + channel;
                            *sample = block.get(at).copied().unwrap_or(0.0);
                        }
                    } else {
                        dest.fill(0.0);
                    }
                }
            }
            let rate = rbs_core::timing::SampleRate::new(self.sample_rate)
                .unwrap_or(rbs_core::timing::SampleRate::HZ_48000);
            let layout = if channels == 2 {
                rbs_core::media::ChannelLayout::Stereo
            } else {
                rbs_core::media::ChannelLayout::Discrete(channels as u8)
            };
            if let Ok(format) = rbs_core::media::AudioFormat::new(rate, layout) {
                let _ = audio.commit(format, now, frames);
            }
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.standby();
        self.shown_texture = None;
        self.upload = SourceUpload::new();
    }
}

struct Ready {
    video: rtrb::Consumer<Pending>,
    audio: rtrb::Consumer<Arc<[f32]>>,
    has_audio: bool,
}

// The decode thread owns its copies of the node's shared counters; grouping them adds no clarity.
#[allow(clippy::too_many_arguments)]
fn worker(
    factory: Arc<dyn MediaDecoderFactory>,
    path: std::path::PathBuf,
    cursor: Duration,
    origin: Timestamp,
    channels: u16,
    block_frames: u32,
    decoder_held: Arc<AtomicBool>,
    enqueued: Arc<AtomicU32>,
    queued_frames: Arc<AtomicU32>,
    queued_bytes: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    loop_flag: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<MediaStats>,
    tx: mpsc::Sender<Result<Ready, MediaNodeError>>,
) {
    let mut decoder = match factory.open(&path) {
        Ok(decoder) => decoder,
        Err(error) => {
            let _ = tx.send(Err(MediaNodeError::Failed(error.to_string())));
            return;
        }
    };
    decoder_held.store(true, Ordering::Release);
    struct Released(Arc<AtomicBool>);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _released = Released(Arc::clone(&decoder_held));
    if let Err(error) = decoder.seek(cursor) {
        let _ = tx.send(Err(MediaNodeError::Failed(error.to_string())));
        return;
    }
    let has_audio = decoder.audio_stream();
    let sample = match first_picture(&mut *decoder) {
        Ok(sample) => sample,
        Err(error) => {
            let _ = tx.send(Err(error));
            return;
        }
    };
    if let Some(picture) = sample.picture.as_ref() {
        if let Some(why) = refuse_picture(picture.width, picture.height, picture.bytes.len()) {
            let _ = tx.send(Err(MediaNodeError::Invalid(why.expected())));
            return;
        }
    }
    let (mut video_prod, video_cons) = rtrb::RingBuffer::new(VIDEO_LOOKAHEAD);
    let (mut audio_prod, audio_cons) = rtrb::RingBuffer::new(AUDIO_LOOKAHEAD);
    if tx
        .send(Ok(Ready {
            video: video_cons,
            audio: audio_cons,
            has_audio,
        }))
        .is_err()
    {
        return;
    }
    let mut next_pts = cursor;
    let mut known_duration = None;
    let mut index = 0u32;
    let mut primed = Some(sample);
    loop {
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let sample = if let Some(sample) = primed.take() {
            sample
        } else {
            match decoder.pull() {
                Ok(Some(sample)) => sample,
                Ok(None) => {
                    if !loop_flag.load(Ordering::Acquire) {
                        break;
                    }
                    if decoder.seek(Duration::ZERO).is_err() {
                        break;
                    }
                    next_pts = Duration::ZERO;
                    index = 0;
                    continue;
                }
                Err(_) => {
                    failed.store(true, Ordering::Release);
                    break;
                }
            }
        };
        let (start, end) = match stamp(&sample, next_pts, known_duration) {
            Ok(stamp) => stamp,
            Err(_) => break,
        };
        known_duration = sample.frame_duration.or(known_duration);
        next_pts = end;
        if let Some(picture) = sample.picture {
            if refuse_picture(picture.width, picture.height, picture.bytes.len()).is_some() {
                break;
            }
            let mark = picture.bytes.first().copied().unwrap_or(0);
            let bytes = picture.bytes.len();
            while !admit(
                queued_frames.load(Ordering::Relaxed) as usize,
                queued_bytes.load(Ordering::Relaxed) as usize,
                bytes,
            ) {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                thread::yield_now();
            }
            if let Ok(frame) = system_frame(&picture_parts(&picture), origin + start) {
                let item = Pending {
                    frame,
                    start,
                    end,
                    index,
                    mark,
                    bytes,
                };
                if push_video(&mut video_prod, item, &shutdown) {
                    queued_frames.fetch_add(1, Ordering::Relaxed);
                    queued_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
                    enqueued.fetch_add(1, Ordering::Release);
                    stats.uploaded.fetch_add(1, Ordering::Relaxed);
                }
            }
            index = index.saturating_add(1);
        }
        if has_audio {
            if let Some(pcm) = sample.pcm {
                enqueue_pcm(
                    &mut audio_prod,
                    &pcm.samples,
                    pcm.channels,
                    channels,
                    block_frames,
                    &shutdown,
                );
            }
        }
    }
}

fn picture_parts(picture: &rbs_media_decode::Picture) -> PictureBits<'_> {
    PictureBits {
        width: picture.width,
        height: picture.height,
        layout: picture.layout,
        color: picture.color,
        bytes: &picture.bytes,
    }
}

struct PictureBits<'a> {
    width: u32,
    height: u32,
    layout: PictureLayout,
    color: SampleColor,
    bytes: &'a [u8],
}

fn first_picture(decoder: &mut dyn MediaDecoder) -> Result<MediaSample, MediaNodeError> {
    loop {
        match decoder.pull() {
            Ok(None) => {
                return Err(MediaNodeError::Invalid("a file with a picture stream"));
            }
            Ok(Some(sample)) if sample.picture.is_some() => return Ok(sample),
            Ok(Some(_)) => {}
            Err(error) => return Err(MediaNodeError::Failed(error.to_string())),
        }
    }
}

fn stamp(
    sample: &MediaSample,
    next_pts: Duration,
    known: Option<Duration>,
) -> Result<(Duration, Duration), DecodeError> {
    let start = sample.pts.unwrap_or(next_pts);
    let duration = sample
        .frame_duration
        .or(known)
        .ok_or(DecodeError::NoTimestamp)?;
    if sample.pts.is_none() && known.is_none() && sample.frame_duration.is_none() {
        return Err(DecodeError::NoTimestamp);
    }
    Ok((start, start.saturating_add(duration)))
}

fn admit(frames: usize, bytes: usize, add: usize) -> bool {
    frames < VIDEO_LOOKAHEAD && bytes.saturating_add(add) <= MAX_FRAME_BYTES
}

fn push_video(
    prod: &mut rtrb::Producer<Pending>,
    mut item: Pending,
    shutdown: &AtomicBool,
) -> bool {
    loop {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }
        match prod.push(item) {
            Ok(()) => return true,
            Err(rtrb::PushError::Full(returned)) => {
                item = returned;
                thread::yield_now();
            }
        }
    }
}

fn enqueue_pcm(
    prod: &mut rtrb::Producer<Arc<[f32]>>,
    samples: &[f32],
    src_channels: u16,
    dst_channels: u16,
    block_frames: u32,
    shutdown: &AtomicBool,
) {
    let src_channels = usize::from(src_channels.max(1));
    let dst_channels = usize::from(dst_channels.max(1));
    let width = dst_channels.saturating_mul(block_frames as usize);
    if width == 0 {
        return;
    }
    let mut interleaved = Vec::new();
    if src_channels == dst_channels {
        interleaved.extend_from_slice(samples);
    } else {
        let frames = samples.len() / src_channels;
        interleaved.reserve(frames.saturating_mul(dst_channels));
        for frame in 0..frames {
            for channel in 0..dst_channels {
                let from = channel.min(src_channels - 1);
                interleaved.push(samples[frame * src_channels + from]);
            }
        }
    }
    for chunk in interleaved.chunks(width) {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        let mut block = vec![0f32; width];
        block[..chunk.len()].copy_from_slice(chunk);
        let arc: Arc<[f32]> = Arc::from(block);
        let mut item = arc;
        loop {
            if shutdown.load(Ordering::Acquire) {
                return;
            }
            match prod.push(item) {
                Ok(()) => break,
                Err(rtrb::PushError::Full(returned)) => {
                    item = returned;
                    thread::yield_now();
                }
            }
        }
    }
}

fn system_frame(
    picture: &PictureBits<'_>,
    timestamp: Timestamp,
) -> Result<VideoFrame, DecodeError> {
    match picture.layout {
        PictureLayout::Rgba8 => rgba_frame(picture, timestamp),
        PictureLayout::Nv12 => {
            check_color(map_color(picture.color), 8).map_err(|error| {
                DecodeError::Failed(format!("colour is not decodable: {error}"))
            })?;
            Err(DecodeError::Unsupported(
                "nv12 is checked with check_color; YUV upload on the worker is not wired yet",
            ))
        }
        PictureLayout::P010 => {
            check_color(map_color(picture.color), 10).map_err(|error| {
                DecodeError::Failed(format!("colour is not decodable: {error}"))
            })?;
            Err(DecodeError::Unsupported(
                "p010 is checked with check_color; YUV upload on the worker is not wired yet",
            ))
        }
    }
}

fn map_color(color: SampleColor) -> ColorSpace {
    match color {
        SampleColor::Srgb => ColorSpace::SRGB,
        SampleColor::Bt709Limited => ColorSpace::BT709_LIMITED,
        SampleColor::Bt709Full => ColorSpace::BT709_FULL,
    }
}

fn rgba_frame(picture: &PictureBits<'_>, timestamp: Timestamp) -> Result<VideoFrame, DecodeError> {
    let format = VideoFormat::new(
        Size::new(picture.width, picture.height),
        PixelFormat::Rgba8,
        ColorSpace::SRGB,
    );
    let stride = (picture.width as usize).saturating_mul(4);
    let bytes = bytes::Bytes::copy_from_slice(picture.bytes);
    let planes = [PlaneLayout { offset: 0, stride }];
    let system = SystemFrame::new(format, bytes, &planes)
        .map_err(|error| DecodeError::Failed(format!("picture layout: {error}")))?;
    Ok(VideoFrame::system(timestamp, system, None))
}
