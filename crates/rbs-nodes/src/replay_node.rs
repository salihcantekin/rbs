//! The replay ring node: packets in, kept in a [`PacketRing`], on an output partition
//! (ADR-0088 decision 2 and 6; spec `docs/specs/recording.md`, "Replay buffer").
//!
//! The node is an `Output`-kind node with one packet input per stream (`s0` video,
//! `s1` audio, ...), like [`crate::OutputNode`], so it takes packets from the same
//! encoder chains. It exists only while the buffer is armed (Installed / Standby /
//! Active, ADR-0013): a disarmed buffer has no node, thread or memory.
//!
//! Real-time contract of `process`: no allocation, I/O or contended lock. Evicted
//! packets are handed to a reaper thread in batches through a bounded channel (`try_send`
//! and `try_recv` never wait and never allocate); a save request arrives as an atomic and
//! the cut runs into storage reserved at arm time, whose result goes back the same way.
//! No lock is taken on the executor thread.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_core::media::StreamIndex;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

use crate::replay::{Clip, ClipBounds, ClipError, HoldLimit, PacketRing, ReplayLimits, RingEntry};

/// Node type id of the replay ring.
pub const REPLAY_NODE_TYPE: &str = "org.rbs.replay-ring";
/// Evicted packets collected before the reaper is woken.
const REAPER_BATCH: usize = 32;
/// Room reserved for evicted packets between two hand-offs.
const EVICTED_CAPACITY: usize = 128;
/// Batches circulating between the node and the reaper.
const BATCHES: usize = 4;

/// The replay ring node's descriptor for `streams` packet inputs: kind `Output`, inputs
/// `"s0"`, `"s1"`, ... in stream order.
pub fn replay_ring_descriptor(streams: usize) -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(REPLAY_NODE_TYPE),
        NodeKind::Output,
        (0..streams)
            .map(|i| Pin::named(format!("s{i}"), LinkKind::Packets, false))
            .collect(),
        Vec::new(),
    )
}

const IDLE: u8 = 0;
const REQUESTED: u8 = 1;
/// `request` value meaning "the whole ring".
const WHOLE: u64 = u64::MAX;

/// Why a clip request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    /// An earlier request has not been served or taken yet.
    Busy,
}

/// What the ring holds, readable from any thread (the `replay.status` data).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplayStatus {
    /// Usable past in milliseconds.
    pub held_ms: u64,
    /// Payload bytes held.
    pub bytes: u64,
    /// Packets held.
    pub packets: u64,
    /// True when memory, not the length setting, is why the past is short.
    pub memory_limited: bool,
    /// Packets freed inside `process` because the reaper had no room (should stay 0).
    pub inline_frees: u64,
    /// Clips cut so far.
    pub clips_cut: u64,
}

type CutResult = (Result<ClipBounds, ClipError>, Vec<RingEntry>);

/// The control-thread side of a ring node: status, clip requests, results.
#[derive(Debug)]
pub struct ReplayHandle {
    request: AtomicU64,
    state: AtomicU8,
    results: Mutex<Receiver<CutResult>>,
    spare: SyncSender<Vec<RingEntry>>,
    capacity: usize,
    held_ms: AtomicU64,
    bytes: AtomicU64,
    packets: AtomicU64,
    memory_limited: AtomicU8,
    inline_frees: AtomicU64,
    clips_cut: AtomicU64,
}

impl ReplayHandle {
    /// What the ring holds, as of the last tick.
    pub fn status(&self) -> ReplayStatus {
        ReplayStatus {
            held_ms: self.held_ms.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            packets: self.packets.load(Ordering::Relaxed),
            memory_limited: self.memory_limited.load(Ordering::Relaxed) != 0,
            inline_frees: self.inline_frees.load(Ordering::Relaxed),
            clips_cut: self.clips_cut.load(Ordering::Relaxed),
        }
    }

    /// Asks the node to cut a clip of the last `seconds` (the whole ring for `None`) on
    /// its next tick. Fetch the result with [`take_clip`](Self::take_clip).
    pub fn request_clip(&self, seconds: Option<Duration>) -> Result<(), RequestError> {
        self.state
            .compare_exchange(IDLE, REQUESTED, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RequestError::Busy)?;
        let value = seconds.map_or(WHOLE, |d| {
            u64::try_from(d.as_millis())
                .unwrap_or(u64::MAX - 1)
                .min(u64::MAX - 1)
                + 1
        });
        self.request.store(value, Ordering::Release);
        Ok(())
    }

    /// The clip the node cut for the last request, once it is ready. Control thread.
    pub fn take_clip(&self) -> Option<Result<Clip, ClipError>> {
        let (result, entries) = self.results.lock().ok()?.try_recv().ok()?;
        self.state.store(IDLE, Ordering::Release);
        // The node needs storage for the next cut: a fresh reserved vector, allocated
        // here on the control thread. A full spare slot means one is waiting already.
        let _ = self.spare.try_send(Vec::with_capacity(self.capacity));
        Some(result.map(|bounds| Clip { entries, bounds }))
    }
}

/// Packets evicted from the ring, on their way to the reaper thread.
type Batch = Vec<RingEntry>;

/// The replay ring node.
pub struct ReplayRingNode {
    descriptor: NodeDescriptor,
    streams: usize,
    ring: PacketRing,
    handle: Arc<ReplayHandle>,
    /// Evicted packets collected since the last hand-off.
    evicted: Batch,
    /// To the reaper: full batches.
    to_reaper: Option<SyncSender<Batch>>,
    /// From the reaper: emptied batches with their capacity.
    from_reaper: Receiver<Batch>,
    reaper_tx_back: Option<SyncSender<Batch>>,
    reaper_rx: Option<Receiver<Batch>>,
    reaper: Option<JoinHandle<()>>,
    /// Storage for the next cut, taken for a cut and returned through the results.
    cut_buf: Option<Vec<RingEntry>>,
    spare: Receiver<Vec<RingEntry>>,
    results: SyncSender<CutResult>,
    limit_ms: u64,
}

impl ReplayRingNode {
    /// A node of `streams` inputs; `video` is the index of the video stream. Reserves
    /// the ring's storage now (arming is a control-thread action).
    pub fn new(streams: usize, video: StreamIndex, limits: ReplayLimits) -> Self {
        let (results, results_rx) = sync_channel(1);
        let (spare_tx, spare) = sync_channel(1);
        let handle = Arc::new(ReplayHandle {
            request: AtomicU64::new(0),
            state: AtomicU8::new(IDLE),
            results: Mutex::new(results_rx),
            spare: spare_tx,
            capacity: limits.capacity,
            held_ms: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            packets: AtomicU64::new(0),
            memory_limited: AtomicU8::new(0),
            inline_frees: AtomicU64::new(0),
            clips_cut: AtomicU64::new(0),
        });
        // Batches circulate: node -> reaper (to_reaper) -> node (back); a few spare ones
        // so a slow reaper delays a hand-off instead of freeing in `process`.
        let (to_reaper, reaper_rx) = sync_channel(BATCHES);
        let (back_tx, from_reaper) = sync_channel(BATCHES);
        for _ in 0..BATCHES - 1 {
            let _ = back_tx.try_send(Vec::with_capacity(EVICTED_CAPACITY));
        }
        Self {
            descriptor: replay_ring_descriptor(streams),
            streams,
            ring: PacketRing::new(limits, video),
            handle,
            evicted: Vec::with_capacity(EVICTED_CAPACITY),
            to_reaper: Some(to_reaper),
            from_reaper,
            reaper_tx_back: Some(back_tx),
            reaper_rx: Some(reaper_rx),
            reaper: None,
            cut_buf: Some(Vec::with_capacity(limits.capacity)),
            spare,
            results,
            limit_ms: u64::try_from(limits.length.as_millis()).unwrap_or(u64::MAX),
        }
    }

    /// The handle the control thread keeps.
    pub fn handle(&self) -> Arc<ReplayHandle> {
        Arc::clone(&self.handle)
    }

    fn publish_status(&self) {
        let held = self.ring.held();
        let h = &self.handle;
        h.held_ms.store(
            u64::try_from(held.duration.as_millis())
                .unwrap_or(u64::MAX)
                .min(self.limit_ms),
            Ordering::Relaxed,
        );
        h.bytes.store(held.bytes as u64, Ordering::Relaxed);
        h.packets.store(held.packets as u64, Ordering::Relaxed);
        h.memory_limited.store(
            u8::from(held.limited_by == Some(HoldLimit::Memory)),
            Ordering::Relaxed,
        );
        h.inline_frees
            .store(self.ring.inline_frees(), Ordering::Relaxed);
    }

    /// Hands the evicted packets to the reaper when enough collected. Never waits: with
    /// no emptied batch to swap in, or a full channel, the packets stay for the next tick.
    fn hand_off(&mut self) {
        if self.evicted.len() < REAPER_BATCH {
            return;
        }
        let Some(tx) = &self.to_reaper else { return };
        let Ok(fresh) = self.from_reaper.try_recv() else {
            return;
        };
        let full = std::mem::replace(&mut self.evicted, fresh);
        if let Err(e) = tx.try_send(full) {
            // Cannot happen while the reaper lives (the batches in flight are fixed, so
            // the channel is never full); if it died, keep the packets here.
            let (TrySendError::Full(b) | TrySendError::Disconnected(b)) = e;
            self.evicted = b;
        }
    }

    fn serve_request(&mut self) {
        let h = &self.handle;
        if h.state.load(Ordering::Acquire) != REQUESTED {
            return;
        }
        let value = h.request.load(Ordering::Acquire);
        if value == 0 {
            return; // already served, waiting to be taken
        }
        let buf = match self.cut_buf.take() {
            Some(b) => b,
            None => match self.spare.try_recv() {
                Ok(b) => b,
                Err(_) => return, // control has not supplied storage yet; next tick
            },
        };
        let mut buf = buf;
        let seconds = (value != WHOLE).then(|| Duration::from_millis(value - 1));
        let result = self.ring.cut(seconds, &mut buf);
        h.request.store(0, Ordering::Relaxed);
        h.clips_cut.fetch_add(1, Ordering::Relaxed);
        // One request at a time, so the single result slot is free.
        let _ = self.results.try_send((result, buf));
    }
}

impl<X: Runtime> Node<X> for ReplayRingNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        let (Some(rx), Some(back)) = (self.reaper_rx.take(), self.reaper_tx_back.take()) else {
            return Err(NodeError::Permanent("a replay ring node starts once"));
        };
        let thread = std::thread::Builder::new()
            .name("rbs-replay-reaper".into())
            .spawn(move || reaper_loop(&rx, &back))
            .map_err(|_| NodeError::Permanent("the replay reaper thread cannot start"))?;
        self.reaper = Some(thread);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let inputs = ctx.inputs();
        for stream in 0..self.streams {
            for packet in inputs.packets(stream) {
                let index = StreamIndex(stream as u16);
                self.ring.push(index, packet.clone(), &mut self.evicted);
            }
        }
        self.serve_request();
        self.hand_off();
        self.publish_status();
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        // Control thread: freeing the buffered past here is allowed.
        self.ring.clear(&mut self.evicted);
        self.evicted = Vec::new();
        // Closing the channel ends the reaper after it frees what is queued.
        self.to_reaper = None;
        if let Some(t) = self.reaper.take() {
            let _ = t.join();
        }
    }
}

/// Frees what the node handed over, off the real-time path. Blocks in `recv` while idle
/// (no polling) and returns each emptied batch for reuse; ends when the node drops its
/// sender.
fn reaper_loop(rx: &Receiver<Batch>, back: &SyncSender<Batch>) {
    while let Ok(mut batch) = rx.recv() {
        batch.clear();
        let _ = back.try_send(batch);
    }
}
