//! Bounded queues between partitions, and the endpoint nodes that use them
//! (ADR-0025).
//!
//! A link between nodes on different executors becomes a queue: a writer node in the
//! producer's partition and a reader node in the consumer's. Both are ordinary
//! [`Node`]s for any [`Runtime`], so executors, `Inputs` and `Outputs` do not change.
//!
//! - **Video** (render to encoder): the newest frame wins. When the queue is full the
//!   writer drops the frame it was about to send; the reader takes everything waiting
//!   and keeps only the newest. Both kinds of drop are counted in [`QueueStats`], which
//!   the consumer (an encoder) can read to keep timestamps honest and ask for a
//!   keyframe. Video writers never wake the consumer: encoders run at frame cadence.
//! - **Packets** (encoder to output): nothing is dropped silently. A full queue sets
//!   a [`QueueStats::overflows`] count, so the consumer (an output) fails and is restarted
//!   (ADR-0024) instead of sending a broken stream. Each output has its own queue, so a
//!   stalled output never holds back the encoder or other outputs. The writer rings the
//!   consumer partition's [`Doorbell`].
//!
//! Queues are `std::sync::mpsc::sync_channel`s: bounded, preallocated, and lock-free
//! for `try_send` and `try_recv`, which never allocate or wait. A value dropped by a
//! writer is dropped on the writer's thread: for video that releases a frame lease,
//! which lease owners must do without locks (media types spec).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::Thread;
use std::time::Duration;

use rbs_core::media::{EncodedPacket, VideoFrame};

use crate::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};

use super::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Default capacity of a video queue, in frames.
pub const VIDEO_QUEUE_FRAMES: usize = 2;
/// Default capacity of a packet queue, in packets (about four seconds of 60 fps video).
pub const PACKET_QUEUE_PACKETS: usize = 256;

/// Counters of one queue, shared by both endpoints and readable by the consumer node
/// and the engine. All counters only grow.
#[derive(Debug, Default)]
pub struct QueueStats {
    sent: AtomicU64,
    received: AtomicU64,
    dropped_full: AtomicU64,
    skipped: AtomicU64,
    unshareable: AtomicU64,
    overflows: AtomicU64,
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

    /// Video frames the writer dropped because the queue was full.
    pub fn dropped_full(&self) -> u64 {
        self.dropped_full.load(Ordering::Relaxed)
    }

    /// Video frames the reader skipped because a newer one was waiting.
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
        self.dropped_full() + self.skipped() + self.unshareable()
    }

    /// Times a packet queue was full and a packet was lost. The consumer must treat
    /// any increase as a broken stream (fail, so that it is restarted).
    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
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
            if worker.is_none() {
                *worker = Some(std::thread::current());
            }
        }
        if !self.rung.load(Ordering::Acquire) {
            std::thread::park_timeout(timeout);
        }
        self.rung.swap(false, Ordering::AcqRel)
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

/// Creates a video queue of `capacity` frames (at least 1).
pub fn video(capacity: usize) -> (VideoWriter, VideoReader, Arc<QueueStats>) {
    let (tx, rx) = sync_channel(capacity.max(1));
    let stats = Arc::new(QueueStats::default());
    (
        VideoWriter {
            desc: writer_descriptor(LinkKind::Video),
            tx,
            last: None,
            stats: Arc::clone(&stats),
        },
        VideoReader {
            desc: reader_descriptor(LinkKind::Video),
            rx,
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
            stats: Arc::clone(&stats),
        },
        PacketReader {
            desc: reader_descriptor(LinkKind::Packets),
            rx,
            pending: None,
            stats: Arc::clone(&stats),
        },
        stats,
    )
}

/// Sends the newest video frame of its input into a queue. Runs in the producer's
/// partition.
pub struct VideoWriter {
    desc: NodeDescriptor,
    tx: SyncSender<VideoFrame>,
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
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let Some(frame) = ctx.inputs().video(0, 0) else {
            return Ok(());
        };
        let key = (frame.timestamp(), frame.sequence());
        if self.last == Some(key) {
            return Ok(());
        }
        self.last = Some(key);
        let Some(copy) = frame.share() else {
            self.stats.unshareable.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        };
        match self.tx.try_send(copy) {
            Ok(()) => {
                self.stats.sent.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Full(_)) => {
                self.stats.dropped_full.fetch_add(1, Ordering::Relaxed);
            }
            // The reader is gone (its partition dropped it); nothing to deliver.
            Err(TrySendError::Disconnected(_)) => {}
        }
        Ok(())
    }
}

/// Passes on the newest frame waiting in a video queue. Runs in the consumer's
/// partition; its output slot keeps the last frame like any video producer.
pub struct VideoReader {
    desc: NodeDescriptor,
    rx: Receiver<VideoFrame>,
    stats: Arc<QueueStats>,
}

impl<X: Runtime> Node<X> for VideoReader {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {
        // Frames still waiting were for a schedule that no longer runs this node.
        while let Ok(frame) = self.rx.try_recv() {
            drop(frame);
            self.stats.skipped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let mut newest = None;
        while let Ok(frame) = self.rx.try_recv() {
            if newest.replace(frame).is_some() {
                self.stats.skipped.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some(frame) = newest {
            self.stats.received.fetch_add(1, Ordering::Relaxed);
            // The replaced frame is dropped here, on this worker.
            let _previous = ctx.outputs().set_video(0, Some(frame))?;
        }
        Ok(())
    }
}

/// Sends every packet of its input into a queue and rings the consumer's doorbell.
pub struct PacketWriter {
    desc: NodeDescriptor,
    tx: SyncSender<EncodedPacket>,
    doorbell: Arc<Doorbell>,
    stats: Arc<QueueStats>,
}

impl<X: Runtime> Node<X> for PacketWriter {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }

    fn prepare(&mut self, _: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        Ok(())
    }

    fn release(&mut self, _: &mut PrepareCtx<'_, X>) {}

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let mut any = false;
        for packet in ctx.inputs().packets(0) {
            match self.tx.try_send(packet.clone()) {
                Ok(()) => {
                    self.stats.sent.fetch_add(1, Ordering::Relaxed);
                    any = true;
                }
                Err(TrySendError::Full(_)) => {
                    self.stats.overflows.fetch_add(1, Ordering::Relaxed);
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
/// stay queued for the next tick.
pub struct PacketReader {
    desc: NodeDescriptor,
    rx: Receiver<EncodedPacket>,
    /// A packet taken from the queue that did not fit into this tick's output.
    pending: Option<EncodedPacket>,
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
        while self.rx.try_recv().is_ok() {}
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
