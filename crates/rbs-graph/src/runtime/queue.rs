//! Bounded queues between partitions, and the endpoint nodes that use them
//! (ADR-0026).
//!
//! A link between nodes on different executors becomes a queue: a writer node in the
//! producer's partition and a reader node in the consumer's. Both are ordinary
//! [`Node`]s for any [`Runtime`], so executors, `Inputs` and `Outputs` do not change.
//!
//! - **Video** (render to encoder): the newest frame wins. The queue is a one-frame
//!   mailbox: the writer replaces a frame the reader has not taken yet, so after a
//!   stall the encoder gets the newest frame, never a stale one. Replaced frames are
//!   counted in [`QueueStats`], which the consumer (an encoder) can read to keep
//!   timestamps honest and ask for a keyframe. Both ends only `try_lock` the mailbox
//!   to move an `Option` (as parameter channels do, ADR-0023); a writer that meets the
//!   reader there keeps its frame and offers it again on its next tick. Video writers
//!   never wake the consumer: encoders run at frame cadence.
//! - **Packets** (encoder to output): the consumer never gets a broken stream. When
//!   the queue is full the writer loses that packet, counts it in
//!   [`QueueStats::overflows`], and then drops every packet up to the next keyframe
//!   ([`QueueStats::resync_drops`]). The consumer (an output) sees a gap in time but a
//!   decodable stream, without having to watch any counter. An encoder may watch
//!   `overflows` to make the next keyframe sooner. Each output has its own queue, so a
//!   stalled output never holds back the encoder or other outputs. The writer rings
//!   the consumer partition's [`Doorbell`].
//!
//! - **Audio** (audio partition to an audio encoder): every block once, in order. The
//!   queue holds a fixed number of pre-allocated block buffers that travel between a
//!   *free* and a *full* channel, so nothing allocates per block. With no free buffer
//!   the writer loses that block and counts it in [`QueueStats::overflows`]; the
//!   reader fills the gap with silent blocks (up to one second, counted in
//!   [`QueueStats::filled`]) so the consumer sees contiguous time. The writer rings
//!   the consumer partition's [`Doorbell`]; the reader passes one block per tick and
//!   rings its own doorbell while more are queued.
//!
//! Packet and audio queues are `std::sync::mpsc::sync_channel`s: bounded and preallocated;
//! `try_send` and `try_recv` never allocate and never wait for the other side (they
//! may spin briefly, and in rare cases yield, when both ends touch the same slot at
//! once; packet endpoints never run on the render thread). A value dropped by a
//! writer is dropped on the writer's thread: for video that releases a frame lease,
//! which lease owners must do without locks (media types spec).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::Thread;
use std::time::Duration;

use rbs_core::media::{AudioBlock, AudioFormat, EncodedPacket, MAX_CHANNELS, VideoFrame};
use rbs_core::time::Timestamp;

use crate::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

use super::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Default capacity of a packet queue, in packets (about four seconds of 60 fps video).
pub const PACKET_QUEUE_PACKETS: usize = 256;

/// Default capacity of an audio queue, in blocks (500 ms of 10 ms blocks).
pub const AUDIO_QUEUE_BLOCKS: usize = 50;

/// Counters of one queue, shared by both endpoints and readable by the consumer node
/// and the engine. All counters only grow.
#[derive(Debug, Default)]
pub struct QueueStats {
    sent: AtomicU64,
    received: AtomicU64,
    skipped: AtomicU64,
    unshareable: AtomicU64,
    overflows: AtomicU64,
    resync_drops: AtomicU64,
    filled: AtomicU64,
    /// Audio blocks in the queue now (sent, not yet taken or discarded).
    queued: AtomicU64,
}

impl QueueStats {
    /// Values the writer put into the queue.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Values the reader passed on.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Video frames that never reached the consumer because a newer frame replaced
    /// them, or because an endpoint was released while holding them; audio blocks
    /// discarded when the reader was released.
    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    /// Video frames that could not cross because their payload cannot be shared
    /// (native surfaces must be imported by the render partition first).
    pub fn unshareable(&self) -> u64 {
        self.unshareable.load(Ordering::Relaxed)
    }

    /// Frames that did not reach the consumer, for any reason. A consumer compares it
    /// with the value it saw last to notice a gap.
    pub fn video_drops(&self) -> u64 {
        self.skipped() + self.unshareable()
    }

    /// Packets lost because the packet queue was full, or audio blocks lost because
    /// the audio queue had no free buffer (or the block did not fit one). For packets,
    /// each loss starts a resync: the writer drops packets until the next keyframe, and
    /// an encoder may compare this with the value it saw last and make a keyframe
    /// early. For audio, the reader fills the gap with silence instead.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    /// Blocks of silence an audio reader passed on in place of lost blocks, so the
    /// consumer's timeline has no gap (ADR-0037).
    pub fn filled(&self) -> u64 {
        self.filled.load(Ordering::Relaxed)
    }

    /// Packets dropped while waiting for a keyframe, after an overflow or when an
    /// endpoint (re)started in the middle of a group of pictures.
    pub fn resync_drops(&self) -> u64 {
        self.resync_drops.load(Ordering::Relaxed)
    }
}

/// Wakes a partition's worker thread when one of its packet queues gets data.
///
/// The worker calls [`Doorbell::wait`] between ticks; writers call
/// [`Doorbell::ring`]. Ringing is one atomic swap, plus an unpark only while the
/// worker is parked. Only non-real-time producers ring (encoders, muxers).
#[derive(Debug, Default)]
pub struct Doorbell {
    rung: AtomicBool,
    worker: Mutex<Option<Thread>>,
}

impl Doorbell {
    /// A doorbell with no worker yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks that data is waiting and wakes the worker if it is parked.
    pub fn ring(&self) {
        if !self.rung.swap(true, Ordering::AcqRel) {
            // `try_lock`: a worker registering itself at this moment is about to check
            // `rung` anyway.
            if let Ok(worker) = self.worker.try_lock() {
                if let Some(thread) = worker.as_ref() {
                    thread.unpark();
                }
            }
        }
    }

    /// Waits until the doorbell rings or `timeout` passes, on the worker's thread.
    /// Returns true if it rang. Consumes the ring.
    pub fn wait(&self, timeout: Duration) -> bool {
        if self.rung.swap(false, Ordering::AcqRel) {
            return true;
        }
        if let Ok(mut worker) = self.worker.lock() {
            // Registers the calling thread, again if the worker moved to a new thread.
            let me = std::thread::current();
            if worker.as_ref().map(Thread::id) != Some(me.id()) {
                *worker = Some(me);
            }
        }
        if !self.rung.load(Ordering::Acquire) {
            std::thread::park_timeout(timeout);
        }
        self.rung.swap(false, Ordering::AcqRel)
    }
}

impl Doorbell {
    /// True once a worker thread has waited on this doorbell, so a ring unparks it.
    pub fn has_worker(&self) -> bool {
        self.worker.lock().map(|w| w.is_some()).unwrap_or(false)
    }
}

fn descriptor(name: &'static str, kind: LinkKind, writer: bool) -> NodeDescriptor {
    let kind_name = match kind {
        LinkKind::Video => "video",
        LinkKind::Audio => "audio",
        _ => "packets",
    };
    if writer {
        NodeDescriptor::new(
            NodeTypeId::new(format!("org.rbs.queue.{kind_name}.writer")),
            NodeKind::Output,
            vec![Pin::one(name, kind)],
            vec![],
        )
    } else {
        NodeDescriptor::new(
            NodeTypeId::new(format!("org.rbs.queue.{kind_name}.reader")),
            NodeKind::Source,
            vec![],
            vec![Pin::one(name, kind)],
        )
    }
}

/// The descriptor of a queue writer for links of `kind` (video, audio or packets).
pub fn writer_descriptor(kind: LinkKind) -> NodeDescriptor {
    descriptor("in", kind, true)
}

/// The descriptor of a queue reader for links of `kind` (video, audio or packets).
pub fn reader_descriptor(kind: LinkKind) -> NodeDescriptor {
    descriptor("out", kind, false)
}

/// The one-frame slot a video queue passes frames through.
#[derive(Debug, Default)]
struct Mailbox {
    /// Set when `slot` is filled and cleared when it is taken, both under the lock, so
    /// the reader's common case, nothing new, is one atomic load.
    fresh: AtomicBool,
    slot: Mutex<Option<VideoFrame>>,
}

/// Locks without waiting. A poisoned lock still holds a valid `Option` (nothing runs
/// under it but a move), so it is used as is.
fn try_lock<T>(m: &Mutex<T>) -> Option<std::sync::MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// Creates a video queue: a one-frame mailbox where the newest frame wins.
pub fn video() -> (VideoWriter, VideoReader, Arc<QueueStats>) {
    let mailbox = Arc::new(Mailbox::default());
    let stats = Arc::new(QueueStats::default());
    (
        VideoWriter {
            desc: writer_descriptor(LinkKind::Video),
            mailbox: Arc::clone(&mailbox),
            held: None,
            last: None,
            stats: Arc::clone(&stats),
        },
        VideoReader {
            desc: reader_descriptor(LinkKind::Video),
            mailbox,
            stats: Arc::clone(&stats),
        },
        stats,
    )
}

/// Creates a packet queue of `capacity` packets (at least 1) that rings `doorbell`.
pub fn packets(
    capacity: usize,
    doorbell: Arc<Doorbell>,
) -> (PacketWriter, PacketReader, Arc<QueueStats>) {
    let (tx, rx) = sync_channel(capacity.max(1));
    let stats = Arc::new(QueueStats::default());
    (
        PacketWriter {
            desc: writer_descriptor(LinkKind::Packets),
            tx,
            doorbell: Arc::clone(&doorbell),
            resync: true,
            stats: Arc::clone(&stats),
        },
        PacketReader {
            desc: reader_descriptor(LinkKind::Packets),
            rx,
            doorbell: Arc::clone(&doorbell),
            pending: None,
            resync: true,
            stats: Arc::clone(&stats),
        },
        stats,
    )
}

/// Offers the newest video frame of its input to a queue. Runs in the producer's
/// partition.
pub struct VideoWriter {
    desc: NodeDescriptor,
    mailbox: Arc<Mailbox>,
    /// A frame not yet in the mailbox because the reader held it at that moment.
    held: Option<VideoFrame>,
    /// `(timestamp, sequence)` of the last frame handled, to send each frame once.
    last: Option<(rbs_core::time::Timestamp, u64)>,
    stats: Arc<QueueStats>,
}

impl<X: Runtime> Node<X> for VideoWriter {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        self.last = None;
        if self.held.take().is_some() {
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        if let Some(frame) = ctx.inputs().video(0, 0) {
            let key = (frame.timestamp(), frame.sequence());
            if self.last != Some(key) {
                self.last = Some(key);
                match frame.share() {
                    Some(copy) => {
                        if self.held.replace(copy).is_some() {
                            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    None => {
                        self.stats.unshareable.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        if self.held.is_none() {
            return Ok(());
        }
        let Some(mut slot) = try_lock(&self.mailbox.slot) else {
            // The reader is taking the previous frame right now; offer this one on the
            // next tick (a newer frame replaces it then).
            return Ok(());
        };
        let replaced = slot.take();
        *slot = self.held.take();
        self.mailbox.fresh.store(true, Ordering::Release);
        drop(slot);
        self.stats.sent.fetch_add(1, Ordering::Relaxed);
        if replaced.is_some() {
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
        }
        // `replaced` drops here, outside the lock: a lease release, lock-free.
        Ok(())
    }
}

/// Passes on the frame waiting in a video queue. Runs in the consumer's partition;
/// its output slot keeps the last frame like any video producer.
pub struct VideoReader {
    desc: NodeDescriptor,
    mailbox: Arc<Mailbox>,
    stats: Arc<QueueStats>,
}

impl VideoReader {
    fn take(&self) -> Option<VideoFrame> {
        if !self.mailbox.fresh.load(Ordering::Acquire) {
            return None;
        }
        // Held by the writer: the frame is taken on the next tick.
        let mut slot = try_lock(&self.mailbox.slot)?;
        self.mailbox.fresh.store(false, Ordering::Release);
        slot.take()
    }
}

impl<X: Runtime> Node<X> for VideoReader {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        // A frame still waiting was for a schedule that no longer runs this node.
        if let Some(mut slot) = try_lock(&self.mailbox.slot) {
            self.mailbox.fresh.store(false, Ordering::Release);
            if slot.take().is_some() {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        if let Some(frame) = self.take() {
            self.stats.received.fetch_add(1, Ordering::Relaxed);
            // The replaced frame is dropped here, on this worker.
            let _previous = ctx.outputs().set_video(0, Some(frame))?;
        }
        Ok(())
    }
}

/// Sends every packet of its input into a queue and rings the consumer's doorbell.
/// After a lost packet it drops packets up to the next keyframe.
pub struct PacketWriter {
    desc: NodeDescriptor,
    tx: SyncSender<EncodedPacket>,
    doorbell: Arc<Doorbell>,
    /// Waiting for a keyframe: at start (the reader may hold the end of an older
    /// stream) and after an overflow.
    resync: bool,
    stats: Arc<QueueStats>,
}

impl<X: Runtime> Node<X> for PacketWriter {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        self.resync = true;
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let mut any = false;
        for packet in ctx.inputs().packets(0) {
            if self.resync && !packet.keyframe() {
                self.stats.resync_drops.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match self.tx.try_send(packet.clone()) {
                Ok(()) => {
                    self.stats.sent.fetch_add(1, Ordering::Relaxed);
                    self.resync = false;
                    any = true;
                }
                Err(TrySendError::Full(_)) => {
                    self.stats.overflows.fetch_add(1, Ordering::Relaxed);
                    self.resync = true;
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
        if any {
            self.doorbell.ring();
        }
        Ok(())
    }
}

/// Passes on packets waiting in a queue, up to the per-tick event capacity; the rest
/// stay queued for the next tick, and the reader rings its own partition's doorbell so
/// that tick comes at once. After a release it starts again at a keyframe.
pub struct PacketReader {
    desc: NodeDescriptor,
    rx: Receiver<EncodedPacket>,
    /// The consumer partition's doorbell, rung when packets are left for the next tick.
    doorbell: Arc<Doorbell>,
    /// A packet taken from the queue that did not fit into this tick's output.
    pending: Option<EncodedPacket>,
    /// Waiting for a keyframe: the queue was drained in the middle of a stream.
    resync: bool,
    stats: Arc<QueueStats>,
}

impl<X: Runtime> Node<X> for PacketReader {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        self.pending = None;
        while self.rx.try_recv().is_ok() {
            self.stats.resync_drops.fetch_add(1, Ordering::Relaxed);
        }
        self.resync = true;
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let outputs = ctx.outputs();
        loop {
            let packet = match self.pending.take() {
                Some(p) => p,
                None => match self.rx.try_recv() {
                    Ok(p) => p,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                },
            };
            if self.resync {
                if !packet.keyframe() {
                    self.stats.resync_drops.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                self.resync = false;
            }
            match outputs.push_packet(0, packet) {
                Ok(()) => {
                    self.stats.received.fetch_add(1, Ordering::Relaxed);
                }
                Err(rejected) => {
                    self.pending = Some(rejected.value);
                    // More are waiting: the worker ticks again without waiting for
                    // the writer's next ring or its timer.
                    self.doorbell.ring();
                    break;
                }
            }
        }
        Ok(())
    }
}

/// One pre-allocated audio block buffer travelling through an audio queue.
struct AudioBuffer {
    samples: Box<[f32]>,
    capacity: usize,
    format: AudioFormat,
    timestamp: Timestamp,
    frames: usize,
}

impl AudioBuffer {
    fn new(frames: u32) -> Self {
        let capacity = frames.clamp(1, super::MAX_BLOCK_FRAMES) as usize;
        Self {
            samples: vec![0.0; capacity * MAX_CHANNELS].into_boxed_slice(),
            capacity,
            format: AudioFormat::STEREO_48K,
            timestamp: Timestamp::ZERO,
            frames: 0,
        }
    }

    /// Copies `block` in. The caller checked that it fits (`frames <= capacity`).
    fn fill(&mut self, block: &AudioBlock<'_>) {
        let frames = block.frames().min(self.capacity);
        let n = block.format().channels.channels().min(MAX_CHANNELS);
        let cap = self.capacity;
        for (c, dst) in self.samples.chunks_exact_mut(cap).take(n).enumerate() {
            match block.channel(c) {
                Some(src) => dst[..frames].copy_from_slice(&src[..frames]),
                // Interleaved blocks have no channel slices.
                None => {
                    if let rbs_core::media::AudioSamples::Interleaved(s) = block.samples() {
                        for (f, d) in dst[..frames].iter_mut().enumerate() {
                            *d = s[f * n + c];
                        }
                    }
                }
            }
        }
        self.format = block.format();
        self.timestamp = block.timestamp();
        self.frames = frames;
    }
}

/// Creates an audio queue of `blocks` pre-allocated buffers (at least 1) of `frames`
/// frames per channel (clamped to `1..=MAX_BLOCK_FRAMES`) that rings `doorbell`.
pub fn audio(
    blocks: usize,
    frames: u32,
    doorbell: Arc<Doorbell>,
) -> (AudioWriter, AudioReader, Arc<QueueStats>) {
    let blocks = blocks.max(1);
    let (full_tx, full_rx) = sync_channel(blocks);
    let (free_tx, free_rx) = sync_channel(blocks);
    for _ in 0..blocks {
        // Room for every buffer: the channel's capacity is `blocks`.
        let _ = free_tx.try_send(AudioBuffer::new(frames));
    }
    let stats = Arc::new(QueueStats::default());
    (
        AudioWriter {
            desc: writer_descriptor(LinkKind::Audio),
            capacity: frames.clamp(1, super::MAX_BLOCK_FRAMES) as usize,
            full: full_tx,
            free: free_rx,
            spare: None,
            doorbell: Arc::clone(&doorbell),
            stats: Arc::clone(&stats),
        },
        AudioReader {
            desc: reader_descriptor(LinkKind::Audio),
            full: full_rx,
            free: free_tx,
            pending: None,
            expected: None,
            doorbell,
            stats: Arc::clone(&stats),
        },
        stats,
    )
}

/// Sends each audio block of its input into an audio queue and rings the consumer's
/// doorbell. Runs in the producer's partition (the audio thread): copying into a free
/// buffer and the two channel operations neither allocate nor wait.
pub struct AudioWriter {
    desc: NodeDescriptor,
    /// Frames per channel each buffer holds.
    capacity: usize,
    full: SyncSender<AudioBuffer>,
    free: Receiver<AudioBuffer>,
    /// A buffer kept because the reader was gone, so it is never freed on this thread.
    spare: Option<AudioBuffer>,
    doorbell: Arc<Doorbell>,
    stats: Arc<QueueStats>,
}

impl<X: Runtime> Node<X> for AudioWriter {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {}

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(block) = ctx.inputs().audio(0, 0) else {
            return Ok(());
        };
        // Checked before a buffer is taken, so `fill` below always fits (blocks never
        // have more than `MAX_CHANNELS` channels).
        if block.frames() > self.capacity {
            self.stats.overflows.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Some(mut buffer) = self.spare.take().or_else(|| self.free.try_recv().ok()) else {
            self.stats.overflows.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        buffer.fill(&block);
        // Counted before the send, so the reader never sees more blocks than `queued`.
        self.stats.queued.fetch_add(1, Ordering::AcqRel);
        match self.full.try_send(buffer) {
            Ok(()) => {
                self.stats.sent.fetch_add(1, Ordering::Relaxed);
                self.doorbell.ring();
            }
            // There are only as many buffers as the channel holds, so it is never full.
            // With the reader gone the buffer is kept, not freed on this thread.
            Err(TrySendError::Full(b) | TrySendError::Disconnected(b)) => {
                self.stats.queued.fetch_sub(1, Ordering::AcqRel);
                self.spare = Some(b);
            }
        }
        Ok(())
    }
}

/// Longest run of lost audio a reader fills with silence; a longer gap is passed on as
/// a gap (the encoder starts a new timeline).
pub const AUDIO_FILL_LIMIT: Duration = Duration::from_secs(1);

/// Passes on one queued audio block per tick; while more wait it rings its own
/// partition's doorbell so the next tick comes at once. Where blocks were lost (a full
/// queue, a bypassed producer) it passes on blocks of silence at the missing times
/// first, up to [`AUDIO_FILL_LIMIT`], so the consumer's timeline stays continuous.
pub struct AudioReader {
    desc: NodeDescriptor,
    full: Receiver<AudioBuffer>,
    free: SyncSender<AudioBuffer>,
    /// A block taken from the queue that waits while silence fills the gap before it.
    pending: Option<AudioBuffer>,
    /// Timestamp the next block should have, after the last one passed on.
    expected: Option<Timestamp>,
    doorbell: Arc<Doorbell>,
    stats: Arc<QueueStats>,
}

impl AudioReader {
    fn take(&mut self) -> Option<AudioBuffer> {
        if let Some(b) = self.pending.take() {
            return Some(b);
        }
        let b = self.full.try_recv().ok()?;
        self.stats.queued.fetch_sub(1, Ordering::AcqRel);
        Some(b)
    }
}

impl<X: Runtime> Node<X> for AudioReader {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        self.expected = None;
        while let Some(buffer) = self.take() {
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            let _ = self.free.try_send(buffer);
        }
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(buffer) = self.take() else {
            return Ok(());
        };
        let duration = buffer.format.sample_rate.duration_of(buffer.frames as u64);
        let mut out = ctx.outputs().audio(0)?;
        let n = buffer.format.channels.channels();
        if buffer.frames > out.capacity() {
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            let _ = self.free.try_send(buffer);
            return Err(NodeError::Output(super::OutputError::AudioTooLarge {
                pin: 0,
                capacity: out.capacity(),
            }));
        }
        let gap = self
            .expected
            .map(|e| buffer.timestamp.saturating_since(e))
            .unwrap_or_default();
        let result = if gap > duration / 2 && gap <= AUDIO_FILL_LIMIT {
            // Lost blocks before this one: silence at the expected time first.
            let at = self.expected.unwrap_or(buffer.timestamp);
            for dst in out.channels_mut(n) {
                dst[..buffer.frames].fill(0.0);
            }
            let r = out.commit(buffer.format, at, buffer.frames);
            self.expected = Some(at + duration);
            self.stats.filled.fetch_add(1, Ordering::Relaxed);
            self.pending = Some(buffer);
            self.doorbell.ring();
            return r.map_err(NodeError::from);
        } else {
            let cap = buffer.capacity;
            for (c, dst) in out.channels_mut(n).enumerate() {
                let src = &buffer.samples[c * cap..c * cap + buffer.frames];
                dst[..buffer.frames].copy_from_slice(src);
            }
            out.commit(buffer.format, buffer.timestamp, buffer.frames)
        };
        self.expected = Some(buffer.timestamp + duration);
        self.stats.received.fetch_add(1, Ordering::Relaxed);
        // The free channel has room for every buffer of this queue.
        let _ = self.free.try_send(buffer);
        if self.stats.queued.load(Ordering::Acquire) > 0 {
            self.doorbell.ring();
        }
        result.map_err(NodeError::from)
    }
}
