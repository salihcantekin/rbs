//! Bounded queues between partitions, and the endpoint nodes that use them
//! (ADR-0025).
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
//! Packet queues are `std::sync::mpsc::sync_channel`s: bounded and preallocated;
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

use rbs_core::media::{EncodedPacket, VideoFrame};

use crate::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

use super::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Default capacity of a packet queue, in packets (about four seconds of 60 fps video).
pub const PACKET_QUEUE_PACKETS: usize = 256;

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
    /// them, or because an endpoint was released while holding them.
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

    /// Packets lost because the packet queue was full. Each loss starts a resync: the
    /// writer drops packets until the next keyframe. An encoder may compare this with
    /// the value it saw last and make a keyframe early.
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
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

/// The descriptor of a queue writer for links of `kind` (video or packets).
pub fn writer_descriptor(kind: LinkKind) -> NodeDescriptor {
    descriptor("in", kind, true)
}

/// The descriptor of a queue reader for links of `kind` (video or packets).
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
            doorbell,
            resync: true,
            stats: Arc::clone(&stats),
        },
        PacketReader {
            desc: reader_descriptor(LinkKind::Packets),
            rx,
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
/// stay queued for the next tick. After a release it starts again at a keyframe.
pub struct PacketReader {
    desc: NodeDescriptor,
    rx: Receiver<EncodedPacket>,
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
                    break;
                }
            }
        }
        Ok(())
    }
}
