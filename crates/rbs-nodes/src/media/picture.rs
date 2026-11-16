//! The media picture node `org.rbs.media` (ADR-0108 decisions 1, 5, 7, and 9).

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rbs_audio::AudioGraphConfig;
use rbs_core::media::VideoFrame;
use rbs_core::time::{Clock, Timestamp};
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_graph::node::NodeDescriptor;
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, RestartPolicy, WarmCtx};
use rbs_media_decode::MediaDecoderFactory;

use super::worker::{self, Job, Pending, Progress};
use super::{MediaStats, Shared, VIDEO_PIN, media_descriptor};

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

/// Where playback is. Standby holds no decoder and no rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPhase {
    /// No decoder, no rings. Cursor, loop, and restart are kept.
    Standby,
    /// Worker running, rings allocated.
    Playing,
    /// End of the file with `loop` false, or a fault: the last picture is kept, the
    /// sound is silence, and there is no worker.
    Hold,
}

struct Rings {
    pictures: rtrb::Consumer<Pending>,
    returns: rtrb::Producer<Pending>,
}

struct Worker {
    join: JoinHandle<()>,
    shutdown: Arc<AtomicBool>,
}

/// Shows one local file's pictures on the master clock and drives its decode worker.
///
/// `process` only dequeues, uploads a new picture, and hands the replaced one back on
/// the return ring. The worker is started and joined on the control path
/// ([`Self::wake`], [`Self::service`], [`Self::standby`]), never from `process`.
pub struct MediaPictureNode {
    descriptor: NodeDescriptor,
    factory: Arc<dyn MediaDecoderFactory>,
    path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
    config: AudioGraphConfig,
    shared: Arc<Shared>,
    loop_flag: Arc<AtomicBool>,
    restart_when_shown: bool,
    cursor: Duration,
    phase: MediaPhase,
    rings: Option<Rings>,
    worker: Option<Worker>,
    ended: Arc<AtomicBool>,
    progress: Arc<Progress>,
    /// The picture shown now, or the first one before its time.
    current: Option<Pending>,
    /// The next picture, popped but not yet due.
    next: Option<Pending>,
    held: Option<VideoFrame>,
    held_mark: u8,
    published: Option<u32>,
    queue_allocations: u32,
    attempts: u32,
    retry_after: Option<Duration>,
    upload: SourceUpload,
    /// Last picture already on the GPU, reused while the clock repeats a picture.
    shown_texture: Option<VideoFrame>,
}

impl MediaPictureNode {
    // Each argument is a distinct source setting; a builder would only move them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        factory: Arc<dyn MediaDecoderFactory>,
        path: std::path::PathBuf,
        loop_playback: bool,
        restart_when_shown: bool,
        clock: Arc<dyn Clock>,
        config: AudioGraphConfig,
        shared: Arc<Shared>,
    ) -> Self {
        Self {
            descriptor: media_descriptor(),
            factory,
            path,
            clock,
            config,
            shared,
            loop_flag: Arc::new(AtomicBool::new(loop_playback)),
            restart_when_shown,
            cursor: Duration::ZERO,
            phase: MediaPhase::Standby,
            rings: None,
            worker: None,
            ended: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Progress::default()),
            current: None,
            next: None,
            held: None,
            held_mark: 0,
            published: None,
            queue_allocations: 0,
            attempts: 0,
            retry_after: None,
            upload: SourceUpload::new(),
            shown_texture: None,
        }
    }

    /// Stored `loop`. A change does not replace the node. While the worker plays it
    /// takes effect at the next end of the file. While held, `true` starts from 0.
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

    /// Times a picture ring was allocated. Stays 0 while Standby and when a picture
    /// is refused.
    pub fn queue_allocations(&self) -> u32 {
        self.queue_allocations
    }

    /// True while the worker holds an open decoder.
    pub fn decoder_held(&self) -> bool {
        self.progress.decoder_held.load(Ordering::Acquire)
    }

    /// True while the picture rings exist.
    pub fn queues_live(&self) -> bool {
        self.rings.is_some()
    }

    /// Pictures the worker has enqueued in this run.
    pub fn enqueued(&self) -> u32 {
        self.progress.enqueued.load(Ordering::Acquire)
    }

    /// The worker has returned (it may not be joined yet).
    pub fn worker_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }

    /// The latest master time at which the worker found nothing due: every picture
    /// and sound block up to that time plus [`super::LEAD`] is queued or dropped.
    /// For tests and diagnostics.
    pub fn worker_settled_at(&self) -> Option<Timestamp> {
        match self.progress.settled_at.load(Ordering::Acquire) {
            0 => None,
            n => Some(Timestamp::from_nanos(n - 1)),
        }
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

    /// Counters, shared with the sound node.
    pub fn stats(&self) -> Arc<MediaStats> {
        Arc::clone(&self.shared.stats)
    }

    /// Opens the file, seeks, and starts the worker. Control thread only. The worker
    /// anchors the cursor's picture at its first picture's master time plus the lead.
    pub fn wake(&mut self, _now: Timestamp) -> Result<(), MediaNodeError> {
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
        self.ended.store(false, Ordering::Release);
        self.progress = Arc::new(Progress::default());
        self.published = None;
        self.current = None;
        self.next = None;
        self.held = None;
        // A new epoch: sound blocks a previous worker left in the ring are skipped.
        let epoch = self
            .shared
            .epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);

        let shutdown = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let job = Job {
            factory: Arc::clone(&self.factory),
            path: self.path.clone(),
            cursor,
            clock: Arc::clone(&self.clock),
            config: self.config,
            loop_flag: Arc::clone(&self.loop_flag),
            shutdown: Arc::clone(&shutdown),
            shared: Arc::clone(&self.shared),
            epoch,
            progress: Arc::clone(&self.progress),
            ready: tx,
        };
        let ended = Arc::clone(&self.ended);
        let progress = Arc::clone(&self.progress);
        let join = thread::Builder::new()
            .name("rbs-media".into())
            .spawn(move || {
                if std::panic::catch_unwind(AssertUnwindSafe(|| worker::run(job))).is_err() {
                    progress.failed.store(true, Ordering::Release);
                }
                ended.store(true, Ordering::Release);
            })
            .map_err(|error| MediaNodeError::Failed(error.to_string()))?;

        let ready = rx.recv().unwrap_or(Err(MediaNodeError::Failed(
            "media worker stopped before it queued a picture".into(),
        )));
        match ready {
            Ok(ready) => {
                self.rings = Some(Rings {
                    pictures: ready.pictures,
                    returns: ready.returns,
                });
                self.queue_allocations = self.queue_allocations.saturating_add(1);
                self.worker = Some(Worker { join, shutdown });
                self.phase = MediaPhase::Playing;
                Ok(())
            }
            Err(error) => {
                shutdown.store(true, Ordering::Release);
                join.thread().unpark();
                let _ = join.join();
                self.phase = MediaPhase::Standby;
                Err(error)
            }
        }
    }

    /// Control thread. After a fault, or once a worker that reached the end of the
    /// file has returned and every picture it queued was shown, joins it, frees the
    /// rings, and holds the last picture.
    pub fn service(&mut self) {
        if !self.ended.load(Ordering::Acquire) {
            return;
        }
        let failed = self.progress.failed.load(Ordering::Acquire);
        let drained = self.next.is_none()
            && self
                .rings
                .as_ref()
                .is_none_or(|rings| rings.pictures.is_empty());
        if !failed && !drained {
            return;
        }
        self.join_worker();
        if let Some(rings) = self.rings.as_mut() {
            while let Ok(item) = rings.pictures.pop() {
                self.current = Some(item);
            }
        }
        if let Some(item) = self.next.take() {
            self.current = Some(item);
        }
        if let Some(current) = self.current.take() {
            self.held_mark = current.mark;
            self.cursor = current.media;
            self.published = Some(current.index);
            self.held = Some(current.frame);
        }
        self.rings = None;
        if failed {
            self.attempts = self.attempts.saturating_add(1);
            self.retry_after = Some(RestartPolicy::default().delay(self.attempts));
        }
        if self.phase == MediaPhase::Playing {
            self.phase = MediaPhase::Hold;
        }
    }

    /// Releases the worker, the decoder, and the rings. Keeps the cursor, `loop`, and
    /// `restart_when_shown`.
    pub fn standby(&mut self) {
        self.join_worker();
        self.rings = None;
        self.current = None;
        self.next = None;
        self.held = None;
        self.shown_texture = None;
        self.phase = MediaPhase::Standby;
    }

    /// Chooses the picture for master time `now`: the latest one whose stamp is due,
    /// or the first one before its time (the cued picture is on air at once; motion
    /// starts one lead later). Does not allocate, free, block, or read the file.
    pub fn dequeue(&mut self, now: Timestamp) -> Option<VideoFrame> {
        if self.phase == MediaPhase::Hold {
            self.shared
                .stats
                .repeated_video
                .fetch_add(1, Ordering::Relaxed);
            return self.held.as_ref().and_then(VideoFrame::share);
        }
        if self.phase != MediaPhase::Playing {
            return None;
        }
        // Bounded by the ring: every pass takes one picture or stops.
        for _ in 0..=super::PICTURE_SLOTS {
            if self.next.is_none() {
                self.next = self.pop_picture();
            }
            let take = match (&self.next, &self.current) {
                (Some(next), Some(_)) => next.start <= now,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if !take {
                break;
            }
            let Some(next) = self.next.take() else {
                break;
            };
            if let Some(old) = self.current.replace(next) {
                if Some(old.index) != self.published {
                    self.shared
                        .stats
                        .dropped_video
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.give_back(old);
            }
        }
        let current = self.current.as_ref()?;
        if self.published == Some(current.index) {
            self.shared
                .stats
                .repeated_video
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.published = Some(current.index);
            self.cursor = current.media;
            self.held_mark = current.mark;
        }
        current.frame.share()
    }

    /// Chooses the picture for `now`, uploads when it changes, and returns a texture
    /// the compositor can sample.
    fn dequeue_texture(
        &mut self,
        ctx: &mut ProcessCtx<'_, GpuRuntime>,
        now: Timestamp,
    ) -> Result<Option<VideoFrame>, NodeError> {
        let before = self.published;
        let Some(system) = self.dequeue(now) else {
            return Ok(None);
        };
        if before == self.published && self.shown_texture.is_some() {
            return Ok(self.shown_texture.as_ref().and_then(VideoFrame::share));
        }
        let texture = self
            .upload
            .upload(ctx.shared(), &mut ctx.local().pool, system)
            .map_err(|_| NodeError::Backend("media picture upload failed"))?;
        self.shown_texture = Some(texture);
        Ok(self.shown_texture.as_ref().and_then(VideoFrame::share))
    }

    fn pop_picture(&mut self) -> Option<Pending> {
        let item = self.rings.as_mut()?.pictures.pop().ok()?;
        self.progress.queued_frames.fetch_sub(1, Ordering::Relaxed);
        self.progress
            .queued_bytes
            .fetch_sub(item.bytes as u64, Ordering::Relaxed);
        Some(item)
    }

    /// Hands a picture back to the worker, which frees it on its own thread. The return
    /// ring holds two more than the picture ring, so it is never full.
    fn give_back(&mut self, item: Pending) {
        if let Some(rings) = self.rings.as_mut() {
            let _ = rings.returns.push(item);
        }
    }

    fn join_worker(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown.store(true, Ordering::Release);
            worker.join.thread().unpark();
            let _ = worker.join.join();
        }
    }
}

impl Drop for MediaPictureNode {
    fn drop(&mut self) {
        self.standby();
    }
}

impl Node<GpuRuntime> for MediaPictureNode {
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
            .or_else(|| self.current.as_ref().and_then(|item| item.frame.share()))
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
        let now = ctx.tick().time;
        if let Some(frame) = self.dequeue_texture(ctx, now)? {
            let _ = ctx.outputs().set_video(VIDEO_PIN, Some(frame))?;
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, GpuRuntime>) {
        self.standby();
        self.shown_texture = None;
        self.upload = SourceUpload::new();
    }
}
