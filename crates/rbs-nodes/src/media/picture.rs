//! The media picture node `org.rbs.media` (ADR-0108 decisions 1, 5, 7, and 9).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_core::media::{FramePayload, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_gpu::node::GpuRuntime;
use rbs_gpu::upload::SourceUpload;
use rbs_graph::node::NodeDescriptor;
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, WarmCtx};

use super::control::{Verdict, View};
use super::player::{Player, Rings, User};
use super::worker::{Pending, Progress};
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

/// Shows one local file's pictures on the master clock.
///
/// `process` only dequeues, uploads a new picture, and hands the replaced one back on
/// the return ring. Playback is started and stopped on the control path
/// ([`Self::wake`], [`Self::service`], [`Self::standby`]) through the player the sound
/// node shares, never from `process`.
pub struct MediaPictureNode {
    descriptor: NodeDescriptor,
    shared: Arc<Shared>,
    loop_flag: Arc<AtomicBool>,
    /// The player's phase as this node last saw it on the control path.
    phase: MediaPhase,
    /// The picture rings of the run this node shows, with that run's counters.
    rings: Option<(Rings, Arc<Progress>)>,
    /// The control records as last read (ADR-0108 decision 10).
    view: View,
    /// The previous render tick, for the render period the control handle uses.
    last_tick: Option<Timestamp>,
    /// The picture shown now, or the first one before its time.
    current: Option<Pending>,
    /// The next picture, popped but not yet due.
    next: Option<Pending>,
    held: Option<VideoFrame>,
    held_mark: u8,
    published: Option<u32>,
    /// The last `dequeue` published a picture it had not published before.
    changed: bool,
    /// `current` has been published. A new run numbers its pictures from 0 again, so
    /// the index alone cannot tell.
    shown: bool,
    upload: SourceUpload,
    /// Last picture already on the GPU, reused while the clock repeats a picture.
    shown_texture: Option<VideoFrame>,
}

impl MediaPictureNode {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        let loop_flag = Arc::clone(&shared.player().loop_flag);
        Self {
            descriptor: media_descriptor(),
            shared,
            loop_flag,
            phase: MediaPhase::Standby,
            rings: None,
            view: View::new(),
            last_tick: None,
            current: None,
            next: None,
            held: None,
            held_mark: 0,
            published: None,
            changed: false,
            shown: false,
            upload: SourceUpload::new(),
            shown_texture: None,
        }
    }

    /// Stored `loop`. A change does not replace the node. While the worker plays it
    /// takes effect at the next end of the range. While held, `true` starts from the
    /// range start.
    pub fn set_loop(&mut self, on: bool) {
        self.loop_flag.store(on, Ordering::Release);
        if on && self.phase == MediaPhase::Hold {
            self.shared.move_cursor(self.shared.range_start());
            let _ = self.restart();
        }
    }

    /// Stored until the next start. Not applied to a worker that is already playing.
    pub fn set_restart_when_shown(&mut self, on: bool) {
        self.shared.player().restart_when_shown = on;
    }

    /// `loop` as stored now.
    pub fn loops(&self) -> bool {
        self.loop_flag.load(Ordering::Acquire)
    }

    /// Media cursor kept with no decoder while Standby.
    pub fn cursor(&self) -> Duration {
        self.shared.cursor()
    }

    /// Playback phase.
    pub fn phase(&self) -> MediaPhase {
        self.shared.player().phase
    }

    /// Times a picture ring was allocated. Stays 0 while Standby and when a picture
    /// is refused.
    pub fn queue_allocations(&self) -> u32 {
        self.shared.player().queue_allocations
    }

    /// True while the worker holds an open decoder.
    pub fn decoder_held(&self) -> bool {
        self.shared
            .player()
            .progress
            .decoder_held
            .load(Ordering::Acquire)
    }

    /// True while this node holds the picture rings.
    pub fn queues_live(&self) -> bool {
        self.rings.is_some()
    }

    /// Pictures the worker has enqueued in this run.
    pub fn enqueued(&self) -> u32 {
        self.shared
            .player()
            .progress
            .enqueued
            .load(Ordering::Acquire)
    }

    /// The worker has returned (it may not be joined yet).
    pub fn worker_ended(&self) -> bool {
        self.shared.player().ended.load(Ordering::Acquire)
    }

    /// The latest master time at which the worker found nothing due: every picture
    /// and sound block up to that time plus [`super::LEAD`] is queued or dropped.
    /// For tests and diagnostics.
    pub fn worker_settled_at(&self) -> Option<Timestamp> {
        match self
            .shared
            .player()
            .progress
            .settled_at
            .load(Ordering::Acquire)
        {
            0 => None,
            n => Some(Timestamp::from_nanos(n - 1)),
        }
    }

    /// True after [`Self::service`] or [`Self::standby`] has joined the thread.
    pub fn worker_joined(&self) -> bool {
        self.shared.player().worker_joined()
    }

    /// First byte of the picture last chosen, when there is one.
    pub fn last_mark(&self) -> Option<u8> {
        self.published.map(|_| self.held_mark)
    }

    /// Failures in a row after a mid-play fault. Back-off uses ADR-0024's default curve.
    pub fn attempts(&self) -> u32 {
        self.shared.player().attempts
    }

    /// Wait before the next wake after a fault, when the node is holding.
    pub fn retry_after(&self) -> Option<Duration> {
        self.shared.player().retry_after
    }

    /// Counters, shared with the sound node.
    pub fn stats(&self) -> Arc<MediaStats> {
        Arc::clone(&self.shared.stats)
    }

    /// Joins playback: starts it from Standby or Hold (opening the file, seeking, and
    /// starting the worker), or takes the picture rings of a worker the sound node
    /// started. Control thread only. The worker anchors the cursor's picture at its
    /// first picture's master time plus the lead.
    pub fn wake(&mut self, _now: Timestamp) -> Result<(), MediaNodeError> {
        let shared = Arc::clone(&self.shared);
        shared.picture_attached.store(true, Ordering::Release);
        let mut player = shared.player();
        let joined = player.acquire(User::Picture, &shared);
        let result = match joined {
            Ok(()) if player.phase == MediaPhase::Playing => Ok(()),
            Ok(()) => {
                self.forget();
                player.start_shown(&shared)
            }
            Err(error) => Err(error),
        };
        self.take_rings(&player);
        result
    }

    /// Starts playback again from the stored cursor (or 0 with `restart_when_shown`).
    fn restart(&mut self) -> Result<(), MediaNodeError> {
        let shared = Arc::clone(&self.shared);
        let mut player = shared.player();
        self.forget();
        let result = player.start(&shared);
        self.take_rings(&player);
        result
    }

    /// Takes the rings of a run this node does not hold yet. Control path.
    fn take_rings(&mut self, player: &Player) {
        let mut slot = self.shared.handoff();
        if let Some(rings) = slot.rings.take() {
            self.shared.handoff_ready.store(false, Ordering::Release);
            slot.retired = self.rings.replace(rings);
        }
        drop(slot);
        self.shared.handoff().retired = None;
        self.phase = player.phase;
    }

    /// Takes the rings of a run the control path started (a restart, a seek from the
    /// end hold) without blocking: `try_lock`, and only when the control path has
    /// freed the rings this node let go of last time. The old rings move into the
    /// slot; nothing is freed here.
    fn adopt_handoff(&mut self) {
        if !self.shared.handoff_ready.load(Ordering::Acquire) {
            return;
        }
        let Ok(mut slot) = self.shared.handoff.try_lock() else {
            return;
        };
        if slot.retired.is_some() {
            return;
        }
        let Some(rings) = slot.rings.take() else {
            return;
        };
        self.shared.handoff_ready.store(false, Ordering::Release);
        slot.retired = self.rings.replace(rings);
        drop(slot);
        // An item popped from the old ring goes back to the new worker, which frees it.
        if let Some(item) = self.next.take() {
            self.give_back(item);
        }
        if self.phase == MediaPhase::Hold {
            // Shown until the new run's first picture.
            self.phase = MediaPhase::Playing;
        }
    }

    /// Drops this node's pictures and rings.
    fn forget(&mut self) {
        self.rings = None;
        self.current = None;
        self.next = None;
        self.held = None;
        self.published = None;
        self.shown = false;
        self.shown_texture = None;
    }

    /// Control thread. After a fault, or once a worker that reached the end of the
    /// file has returned and every picture it queued was shown, joins it, frees the
    /// rings, and holds the last picture.
    pub fn service(&mut self) {
        let shared = Arc::clone(&self.shared);
        let mut player = shared.player();
        if self.rings.is_none() {
            self.take_rings(&player);
        }
        if self.phase != MediaPhase::Playing || !player.ended.load(Ordering::Acquire) {
            return;
        }
        let failed = player.progress.failed.load(Ordering::Acquire);
        let drained = self.next.is_none()
            && self
                .rings
                .as_ref()
                .is_none_or(|(rings, _)| rings.pictures.is_empty());
        if !failed && !drained {
            return;
        }
        player.hold(&shared);
        // The held picture is the last one that can be shown without the GPU: a system
        // picture, or the texture of an imported one. A native picture never imported
        // is dropped with its slot, and the hold keeps the picture shown before it, with
        // that picture's mark, cursor and index.
        let mut last = self.current.take();
        let showable = |item: &Pending| !item.frame.as_ref().is_some_and(is_native);
        if let Some((rings, _)) = self.rings.as_mut() {
            while let Ok(item) = rings.pictures.pop() {
                if showable(&item) {
                    last = Some(item);
                }
            }
        }
        if let Some(item) = self.next.take()
            && showable(&item)
        {
            last = Some(item);
        }
        if let Some(current) = last {
            self.held_mark = current.mark;
            self.shared.set_cursor(current.media);
            self.published = Some(current.index);
            self.held = match current.frame {
                Some(frame) if !is_native(&frame) => Some(frame),
                _ => self.shown_texture.as_ref().and_then(VideoFrame::share),
            };
        }
        self.shown = false;
        self.rings = None;
        self.phase = player.phase;
    }

    /// Leaves playback: frees this node's rings and, unless the sound node is still
    /// prepared, the worker and the decoder. Keeps the cursor, `loop`, and
    /// `restart_when_shown`.
    pub fn standby(&mut self) {
        // Pictures go back to the worker, which frees them on its own thread.
        if let Some(item) = self.current.take() {
            self.give_back(item);
        }
        if let Some(item) = self.next.take() {
            self.give_back(item);
        }
        // The worker stops queueing; what it queued already goes back too, so a later
        // wake starts from fresh pictures, not stale ones.
        self.shared.picture_attached.store(false, Ordering::Release);
        while let Some(item) = self.pop_picture() {
            self.give_back(item);
        }
        let shared = Arc::clone(&self.shared);
        let mut player = shared.player();
        // A worker that plays on for the sound node keeps its picture rings for a later
        // wake.
        if player.phase == MediaPhase::Playing {
            let mut slot = shared.handoff();
            if slot.rings.is_none() {
                slot.rings = self.rings.take();
                shared
                    .handoff_ready
                    .store(slot.rings.is_some(), Ordering::Release);
            }
            slot.retired = None;
        }
        self.forget();
        player.release(User::Picture, &shared);
        self.phase = MediaPhase::Standby;
    }

    /// Chooses the picture for master time `now`: the latest one whose stamp is due,
    /// or the first one before its time (the cued picture is on air at once; motion
    /// starts one lead later). Does not allocate, free, block, or read the file.
    pub fn dequeue(&mut self, now: Timestamp) -> Option<VideoFrame> {
        self.changed = false;
        // A run the control path started (a restart from the end hold) is taken first.
        self.adopt_handoff();
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
        if !self.view.refresh(&self.shared.controls) {
            self.shared
                .stats
                .torn_controls
                .fetch_add(1, Ordering::Relaxed);
        }
        // Bounded by the ring: every pass takes or skips one picture, or stops.
        for _ in 0..=2 * super::PICTURE_SLOTS {
            if self.next.is_none() {
                self.next = self.pop_picture();
            }
            let verdict = match &self.next {
                Some(next) => self.view.records().verdict(next.generation, next.start),
                None => break,
            };
            match verdict {
                Verdict::Play => {}
                Verdict::Wait => break,
                Verdict::Skip => {
                    // A later command ended this picture's generation before it.
                    if let Some(item) = self.next.take() {
                        self.shared.stats.superseded.fetch_add(1, Ordering::Relaxed);
                        self.give_back(item);
                    }
                    continue;
                }
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
            let was_shown = std::mem::replace(&mut self.shown, false);
            if let Some(old) = self.current.replace(next) {
                if !was_shown {
                    self.shared
                        .stats
                        .dropped_video
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.give_back(old);
            }
        }
        let Some(current) = self.current.as_ref() else {
            // A new run after the end hold shows the held picture until its first.
            return self.held.as_ref().and_then(VideoFrame::share);
        };
        self.changed = !self.shown;
        if self.shown {
            self.shared
                .stats
                .repeated_video
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.shown = true;
            self.published = Some(current.index);
            self.shared.set_cursor(current.media);
            self.held_mark = current.mark;
        }
        // `None` for a native picture: `dequeue_texture` imports it.
        current.frame.as_ref().and_then(VideoFrame::share)
    }

    /// Chooses the picture for `now`, uploads or imports it when it changes, and
    /// returns a texture the compositor can sample. A native picture is imported once
    /// (zero-copy, ADR-0108 decision 4); when the import fails the last texture repeats
    /// and the failure is counted.
    fn dequeue_texture(
        &mut self,
        ctx: &mut ProcessCtx<'_, GpuRuntime>,
        now: Timestamp,
    ) -> Result<Option<VideoFrame>, NodeError> {
        let chosen = self.dequeue(now);
        if !self.changed && self.shown_texture.is_some() {
            return Ok(self.shown_texture.as_ref().and_then(VideoFrame::share));
        }
        let source = match chosen {
            Some(frame) => frame,
            None => {
                let Some(current) = self.current.as_mut() else {
                    return Ok(None);
                };
                if !current.frame.as_ref().is_some_and(is_native) {
                    return Ok(None);
                }
                // The surface is the importer's from here; the slot is released.
                current.native = None;
                match current.frame.take() {
                    Some(frame) => frame,
                    None => return Ok(None),
                }
            }
        };
        let gpu = ctx.shared();
        let local = ctx.local();
        let texture = match source.payload() {
            FramePayload::Native { .. } => {
                match local.importer.import(gpu, &mut local.pool, source) {
                    Ok(texture) => {
                        self.shared.stats.imported.fetch_add(1, Ordering::Relaxed);
                        // The current picture keeps a share of its texture, so it
                        // repeats (paused, cued, held) after `cool` let go of the shown
                        // texture.
                        if let Some(current) = self.current.as_mut() {
                            current.frame = texture.share();
                        }
                        texture
                    }
                    Err(_) => {
                        // The frame and its lease are gone; the last texture stays.
                        self.shared
                            .stats
                            .import_failures
                            .fetch_add(1, Ordering::Relaxed);
                        return Ok(self.shown_texture.as_ref().and_then(VideoFrame::share));
                    }
                }
            }
            // The held texture of an imported picture.
            FramePayload::Texture(_) => source,
            _ => self
                .upload
                .upload(gpu, &mut local.pool, source)
                .map_err(|_| NodeError::Backend("media picture upload failed"))?,
        };
        self.shown_texture = Some(texture);
        Ok(self.shown_texture.as_ref().and_then(VideoFrame::share))
    }

    fn pop_picture(&mut self) -> Option<Pending> {
        let (rings, progress) = self.rings.as_mut()?;
        let item = rings.pictures.pop().ok()?;
        progress.queued_frames.fetch_sub(1, Ordering::Relaxed);
        progress
            .queued_bytes
            .fetch_sub(item.bytes as u64, Ordering::Relaxed);
        Some(item)
    }

    /// Hands a picture back to the worker, which frees it on its own thread. The return
    /// ring holds two more than the picture ring, so it is never full.
    fn give_back(&mut self, mut item: Pending) {
        // An imported picture's texture belongs to this thread's pool: it is let go
        // here, not on the worker.
        if item
            .frame
            .as_ref()
            .is_some_and(|f| matches!(f.payload(), FramePayload::Texture(_)))
        {
            item.frame = None;
        }
        if let Some((rings, _)) = self.rings.as_mut() {
            let _ = rings.returns.push(item);
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

    fn prepare(&mut self, ctx: &mut PrepareCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        // Native pictures must be on the render adapter (port invariant 11).
        let luid = match ctx.shared.adapter().get_info().backend {
            wgpu::Backend::Dx12 => super::luid_slot(ctx.shared.adapter_luid()),
            _ => super::luid_slot(None),
        };
        let previous = self.shared.import_luid.swap(luid, Ordering::AcqRel);
        if previous != 0 && previous != luid && self.phase == MediaPhase::Playing {
            // The render device moved to another adapter (a device loss): a run opened
            // for the old one hands out surfaces the new device cannot import, so it
            // opens again at the cursor.
            return self
                .restart()
                .map_err(|error| NodeError::Failed(error.to_string()));
        }
        let now = self.shared.player().clock().now();
        self.wake(now)
            .map_err(|error| NodeError::Failed(error.to_string()))
    }

    fn warm(&mut self, ctx: &mut WarmCtx<'_, GpuRuntime>) -> Result<(), NodeError> {
        if let Some(system) = self
            .held
            .as_ref()
            .and_then(VideoFrame::share)
            .or_else(|| {
                self.current
                    .as_ref()
                    .and_then(|item| item.frame.as_ref().and_then(VideoFrame::share))
            })
            .filter(|frame| matches!(frame.payload(), FramePayload::System(_)))
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
        // The render period, for the instant of the next control command.
        if let Some(previous) = self.last_tick.replace(now)
            && now > previous
        {
            let period = now.as_nanos() - previous.as_nanos();
            self.shared
                .controls
                .render_period
                .store(period, Ordering::Relaxed);
        }
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

/// True for a native surface the importer still has to take.
fn is_native(frame: &VideoFrame) -> bool {
    matches!(frame.payload(), FramePayload::Native { .. })
}
