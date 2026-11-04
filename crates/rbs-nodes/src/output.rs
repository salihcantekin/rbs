//! The output node: packets in, delivered through an optional muxer and a transport, on
//! its own partition (spec `docs/specs/io-nodes.md`, "Output node").

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rbs_core::media::{
    ContainerId, EncodedPacket, MuxChunk, StreamDescriptor, StreamIndex, StreamParams,
};
use rbs_core::time::{Clock, MonotonicClock, Timestamp};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};
use rbs_mux::{MuxError, Muxer, MuxerConfig, MuxerFactory};
use rbs_transport::{
    SendOutcome, Transport, TransportConfig, TransportError, TransportEvent, TransportFactory,
    TransportInput, TransportItem,
};

/// Node type id of the output.
pub const OUTPUT_NODE_TYPE: &str = "org.rbs.output";
/// Items kept by default when the transport's queue is full (`QueueFull`), before the
/// output stops with an error.
pub const DEFAULT_BACKLOG: usize = 4096;
/// Default time `release` waits to deliver the tail and close the transport.
pub const DEFAULT_FINISH_TIMEOUT: Duration = Duration::from_secs(5);
/// Packets of the other streams an output with video keeps while it waits for the
/// first video packet (aligned start); past it the oldest are dropped (`start_drops`).
/// About 5 s of AAC at 48 kHz.
pub const START_HOLD: usize = 256;
/// How often `release` retries a full queue while it delivers the tail.
const FINISH_RETRY: Duration = Duration::from_millis(5);

/// The output node's descriptor for `streams` packet inputs: kind `Output`, inputs
/// `"s0"`, `"s1"`, ... (one single-link packet pin per stream, in stream order).
pub fn output_descriptor(streams: usize) -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(OUTPUT_NODE_TYPE),
        NodeKind::Output,
        (0..streams)
            .map(|i| Pin::named(format!("s{i}"), LinkKind::Packets, false))
            .collect(),
        Vec::new(),
    )
}

/// Counters of an output node, readable from any thread.
#[derive(Debug, Default)]
pub struct OutputNodeStats {
    /// Packets taken from the inputs.
    pub packets: AtomicU64,
    /// Items (packets or chunks) the transport queued.
    pub sent: AtomicU64,
    /// Items the transport dropped by its drop policy.
    pub transport_drops: AtomicU64,
    /// Packets the muxer dropped as out of order (`MuxError::NonMonotonic`).
    pub mux_drops: AtomicU64,
    /// Largest backlog seen.
    pub backlog_peak: AtomicU64,
    /// `KeyframeRequested` events from the transport (not forwarded yet).
    pub keyframe_requests: AtomicU64,
    /// `Connected` events: the transport reached its destination (a file opened, a
    /// stream published), counting every reconnection.
    pub connects: AtomicU64,
    /// `Reconnecting` events: connection attempts after a failure or a lost connection.
    pub reconnect_attempts: AtomicU64,
    /// Connected now: set by `Connected`, cleared by `Reconnecting` and `Disconnected`.
    pub connected: AtomicBool,
    /// Items still undelivered when `release` gave up.
    pub finish_lost: AtomicU64,
    /// Packets of other streams dropped because they end before the first video
    /// packet (aligned start), or because the hold before it was full.
    pub start_drops: AtomicU64,
    /// Why the output stopped for good, set once at the first permanent failure.
    pub failure: OnceLock<String>,
}

/// The time source of an output's blocking waits in `prepare` and `release` (control
/// thread): when to give up, and how to wait before trying again. Injected so tests
/// never sleep (ADR-0032).
pub trait WaitClock: Send + Sync {
    /// The current time.
    fn now(&self) -> Timestamp;
    /// Waits about `duration` before the next try.
    fn pause(&self, duration: Duration);
}

/// The production [`WaitClock`]: the monotonic clock and a thread sleep.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemWaitClock(MonotonicClock);

impl WaitClock for SystemWaitClock {
    fn now(&self) -> Timestamp {
        self.0.now()
    }

    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Something waiting for room in the transport's queue.
enum Item {
    Packet(StreamIndex, EncodedPacket),
    Chunk(MuxChunk),
}

impl Item {
    fn as_transport(&self) -> TransportItem<'_> {
        match self {
            Item::Packet(stream, packet) => TransportItem::Packet {
                stream: *stream,
                packet,
            },
            Item::Chunk(chunk) => TransportItem::Chunk(chunk),
        }
    }
}

/// Where the output's media starts (aligned start, spec `io-nodes.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// No video stream, or video only: every packet goes out.
    Open,
    /// Waiting for the first packet of the video stream; others are held.
    Waiting,
    /// From the first video packet's pts: earlier packets of other streams are dropped.
    From(Timestamp),
}

/// Whether `packet` ends at or before `start` (its pts when it has no duration).
fn ends_before(packet: &EncodedPacket, start: Timestamp) -> bool {
    match packet.duration() {
        Some(d) => packet.pts() + d <= start,
        None => packet.pts() < start,
    }
}

struct MuxerSetup {
    factory: Arc<dyn MuxerFactory>,
    container: ContainerId,
    fragment_duration: Option<Duration>,
}

/// Delivers the packets of its inputs through a muxer (for container transports such
/// as files) and a transport.
pub struct OutputNode {
    descriptor: NodeDescriptor,
    streams: Vec<StreamDescriptor>,
    transport_factory: Arc<dyn TransportFactory>,
    transport_config: TransportConfig,
    muxer_setup: Option<MuxerSetup>,
    backlog_limit: usize,
    finish_timeout: Duration,
    clock: Arc<dyn WaitClock>,
    /// Media reached the transport: a new session would truncate the destination.
    wrote_media: bool,
    transport: Option<Box<dyn Transport>>,
    muxer: Option<Box<dyn Muxer>>,
    backlog: VecDeque<Item>,
    chunks: Vec<MuxChunk>,
    events: Vec<TransportEvent>,
    /// The video stream's index, if the output has one and other streams too.
    video: Option<usize>,
    start: Start,
    held: VecDeque<(usize, EncodedPacket)>,
    stats: Arc<OutputNodeStats>,
}

impl OutputNode {
    /// An output of `streams` (stream `i` arrives on input pin `"s{i}"`) delivered by a
    /// transport from `factory`. Nothing is created until `prepare`.
    pub fn new(
        streams: Vec<StreamDescriptor>,
        factory: Arc<dyn TransportFactory>,
        config: TransportConfig,
    ) -> Self {
        let video = streams
            .iter()
            .position(|s| matches!(s.params, StreamParams::Video { .. }))
            .filter(|_| streams.len() > 1);
        Self {
            descriptor: output_descriptor(streams.len()),
            video,
            start: Start::Open,
            held: VecDeque::new(),
            streams,
            transport_factory: factory,
            transport_config: config,
            muxer_setup: None,
            backlog_limit: DEFAULT_BACKLOG,
            finish_timeout: DEFAULT_FINISH_TIMEOUT,
            clock: Arc::new(SystemWaitClock::default()),
            wrote_media: false,
            transport: None,
            muxer: None,
            backlog: VecDeque::new(),
            chunks: Vec::new(),
            events: Vec::new(),
            stats: Arc::default(),
        }
    }

    /// Muxes into `container` before the transport. Required when the transport's input
    /// is `TransportInput::Container`, and the container must match.
    pub fn with_muxer(
        mut self,
        factory: Arc<dyn MuxerFactory>,
        container: ContainerId,
        fragment_duration: Option<Duration>,
    ) -> Self {
        self.muxer_setup = Some(MuxerSetup {
            factory,
            container,
            fragment_duration,
        });
        self
    }

    /// Items kept while the transport's queue is full; past it the output stops with an
    /// error rather than write a file with holes (output ports spec).
    pub fn with_backlog(mut self, items: usize) -> Self {
        self.backlog_limit = items.max(1);
        self
    }

    /// How long `release` may block delivering the tail and closing the transport.
    pub fn with_finish_timeout(mut self, timeout: Duration) -> Self {
        self.finish_timeout = timeout;
        self
    }

    /// The time source of the waits in `prepare` and `release` (tests).
    pub fn with_wait_clock(mut self, clock: Arc<dyn WaitClock>) -> Self {
        self.clock = clock;
        self
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<OutputNodeStats> {
        Arc::clone(&self.stats)
    }

    /// Records why the output stopped (first cause only) and returns the node error.
    /// Runs once per output at most: the string is built only on this path.
    fn fail(&self, error: NodeError, cause: impl FnOnce() -> String) -> NodeError {
        let _ = self.stats.failure.set(cause());
        error
    }

    /// Hands `item` to the transport, or appends it to the backlog when the queue is
    /// full. Real-time: never waits; the backlog is preallocated.
    fn send(&mut self, item: Item) -> Result<(), NodeError> {
        if self.backlog.is_empty() {
            let transport = self.transport.as_mut().ok_or(NodeError::NotReady)?;
            match transport.send(item.as_transport()) {
                Ok(outcome) => {
                    self.count(outcome);
                    return Ok(());
                }
                Err(TransportError::QueueFull) => {}
                Err(e) => {
                    return Err(self.fail(NodeError::Permanent("transport failed"), || {
                        format!("transport: {e}")
                    }));
                }
            }
        }
        if self.backlog.len() >= self.backlog_limit {
            return Err(self.fail(
                NodeError::Permanent("output backlog full: the destination is too slow"),
                || "output backlog full: the destination is too slow".into(),
            ));
        }
        self.backlog.push_back(item);
        let len = self.backlog.len() as u64;
        self.stats.backlog_peak.fetch_max(len, Ordering::Relaxed);
        Ok(())
    }

    /// Sends backlog items while the transport takes them. Returns whether it is empty.
    fn drain_backlog(&mut self) -> Result<bool, NodeError> {
        let transport = self.transport.as_mut().ok_or(NodeError::NotReady)?;
        while let Some(item) = self.backlog.front() {
            match transport.send(item.as_transport()) {
                Ok(outcome) => {
                    count(&self.stats, outcome);
                    self.backlog.pop_front();
                }
                Err(TransportError::QueueFull) => return Ok(false),
                Err(e) => {
                    return Err(self.fail(NodeError::Permanent("transport failed"), || {
                        format!("transport: {e}")
                    }));
                }
            }
        }
        Ok(true)
    }

    /// A start error: retried with back-off when the transport says it may
    /// recover (a network connection), permanent otherwise (a denied path).
    fn transport_error(&self, what: &'static str, e: TransportError) -> NodeError {
        if e.is_recoverable() {
            NodeError::Failed(format!("{what}: {e}"))
        } else {
            self.fail(NodeError::Permanent("the transport cannot start"), || {
                format!("{what}: {e}")
            })
        }
    }

    fn count(&self, outcome: SendOutcome) {
        count(&self.stats, outcome);
    }

    /// Takes one packet of stream `stream` in: the output starts at the first video
    /// packet, so packets of other streams that end before it are dropped and packets
    /// that come first are held until it arrives. Real-time: the hold is preallocated
    /// and a packet clone shares its payload.
    fn accept(&mut self, stream: usize, packet: &EncodedPacket) -> Result<(), NodeError> {
        match self.start {
            Start::Open => self.write(stream, packet),
            Start::From(start) => {
                if Some(stream) != self.video && ends_before(packet, start) {
                    add(&self.stats.start_drops, 1);
                    return Ok(());
                }
                self.write(stream, packet)
            }
            Start::Waiting if Some(stream) == self.video => {
                let start = packet.pts();
                self.start = Start::From(start);
                while let Some((s, held)) = self.held.pop_front() {
                    if ends_before(&held, start) {
                        add(&self.stats.start_drops, 1);
                    } else {
                        self.write(s, &held)?;
                    }
                }
                self.write(stream, packet)
            }
            Start::Waiting => {
                if self.held.len() >= START_HOLD {
                    self.held.pop_front();
                    add(&self.stats.start_drops, 1);
                }
                self.held.push_back((stream, packet.clone()));
                Ok(())
            }
        }
    }

    /// Muxes or forwards one packet of stream `stream`.
    fn write(&mut self, stream: usize, packet: &EncodedPacket) -> Result<(), NodeError> {
        add(&self.stats.packets, 1);
        let index = StreamIndex(stream as u16);
        let Some(muxer) = self.muxer.as_mut() else {
            // Shares the payload; no copy.
            self.wrote_media = true;
            return self.send(Item::Packet(index, packet.clone()));
        };
        match muxer.write(index, packet, &mut self.chunks) {
            Ok(()) => {}
            Err(MuxError::NonMonotonic { .. }) => {
                add(&self.stats.mux_drops, 1);
                return Ok(());
            }
            Err(e) => {
                return Err(self.fail(NodeError::Permanent("muxer failed"), || {
                    format!("muxer: {e}")
                }));
            }
        }
        self.wrote_media = true;
        let mut chunks = std::mem::take(&mut self.chunks);
        let mut result = Ok(());
        for chunk in chunks.drain(..) {
            if result.is_ok() {
                result = self.send(Item::Chunk(chunk));
            }
        }
        self.chunks = chunks;
        result
    }

    /// Blocks up to `deadline` until the backlog is delivered (control thread only).
    fn drain_until(&mut self, deadline: Timestamp) -> bool {
        loop {
            match self.drain_backlog() {
                Ok(true) => return true,
                Ok(false) if self.clock.now() < deadline => self.clock.pause(FINISH_RETRY),
                _ => return false,
            }
        }
    }

    /// Sends `chunks` from `prepare` or `release` (control thread), waiting for room.
    fn send_blocking(&mut self, chunks: Vec<MuxChunk>, deadline: Timestamp) -> bool {
        for chunk in chunks {
            if self.send(Item::Chunk(chunk)).is_err() || !self.drain_until(deadline) {
                return false;
            }
        }
        true
    }
}

fn count(stats: &OutputNodeStats, outcome: SendOutcome) {
    match outcome {
        SendOutcome::Queued => add(&stats.sent, 1),
        SendOutcome::QueuedAfterDrops { dropped, .. } => {
            add(&stats.sent, 1);
            add(&stats.transport_drops, u64::from(dropped));
        }
        SendOutcome::Dropped { .. } => add(&stats.transport_drops, 1),
        _ => add(&stats.sent, 1),
    }
}

impl<X: Runtime> Node<X> for OutputNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        if self.wrote_media {
            // A restart (after a panic in an adapter, ADR-0024) would open the destination
            // again and truncate a file; an output is single-use once media went out.
            return Err(self.fail(
                NodeError::Permanent("an output that wrote media cannot restart"),
                || "an output that wrote media cannot restart".into(),
            ));
        }
        let wants_muxer = match &self.transport_config.input {
            TransportInput::Container(c) => Some(*c),
            _ => None,
        };
        let muxer = match (&self.muxer_setup, wants_muxer) {
            (None, None) => None,
            (Some(setup), Some(container)) if setup.container == container => {
                let mut config = MuxerConfig::new(container, self.streams.clone());
                if let Some(d) = setup.fragment_duration {
                    config = config.with_fragment_duration(d);
                }
                Some(setup.factory.create(&config).map_err(|e| {
                    self.fail(NodeError::Permanent("the muxer cannot be created"), || {
                        format!("muxer: {e}")
                    })
                })?)
            }
            _ => {
                return Err(NodeError::Permanent(
                    "the muxer does not match the transport's input",
                ));
            }
        };
        let mut transport = self
            .transport_factory
            .create(&self.transport_config)
            .map_err(|e| {
                // Never fixed by retrying (transport port).
                self.fail(
                    NodeError::Permanent("the transport cannot be created"),
                    || format!("transport: {e}"),
                )
            })?;
        transport
            .start(&self.streams)
            .map_err(|e| self.transport_error("transport start", e))?;
        self.transport = Some(transport);
        self.backlog = VecDeque::with_capacity(self.backlog_limit);
        if self.video.is_some() {
            self.start = Start::Waiting;
            self.held = VecDeque::with_capacity(START_HOLD);
        }
        self.chunks = Vec::with_capacity(16);
        self.events = Vec::with_capacity(16);
        if let Some(mut muxer) = muxer {
            let mut header = Vec::new();
            let begun = muxer.begin(&mut header);
            self.muxer = Some(muxer);
            if let Err(e) = begun {
                return Err(self.fail(NodeError::Permanent("muxer begin failed"), || {
                    format!("muxer begin: {e}")
                }));
            }
            let deadline = self.clock.now() + self.finish_timeout;
            if !self.send_blocking(header, deadline) {
                return Err(NodeError::Failed(
                    "could not write the header in time".into(),
                ));
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let inputs = ctx.inputs();
        self.drain_backlog()?;
        for stream in 0..self.streams.len() {
            for packet in inputs.packets(stream) {
                self.accept(stream, packet)?;
            }
        }
        let transport = self.transport.as_mut().ok_or(NodeError::NotReady)?;
        self.events.clear();
        transport.poll_events(&mut self.events);
        let mut disconnected = false;
        for event in &self.events {
            match event {
                TransportEvent::KeyframeRequested => add(&self.stats.keyframe_requests, 1),
                TransportEvent::Connected => {
                    add(&self.stats.connects, 1);
                    self.stats.connected.store(true, Ordering::Relaxed);
                }
                TransportEvent::Reconnecting { .. } => {
                    add(&self.stats.reconnect_attempts, 1);
                    self.stats.connected.store(false, Ordering::Relaxed);
                }
                TransportEvent::Disconnected { .. } => {
                    self.stats.connected.store(false, Ordering::Relaxed);
                    disconnected = true;
                }
                _ => {}
            }
        }
        if disconnected {
            return Err(
                self.fail(NodeError::Permanent("transport disconnected"), || {
                    "transport disconnected".into()
                }),
            );
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        // No video packet ever came: the held packets are all the output has.
        self.start = Start::Open;
        let held = std::mem::take(&mut self.held);
        for (stream, packet) in held {
            if self.transport.is_some() && self.write(stream, &packet).is_err() {
                break;
            }
        }
        let deadline = self.clock.now() + self.finish_timeout;
        let mut delivered = self.transport.is_some() && self.drain_until(deadline);
        if let Some(mut muxer) = self.muxer.take() {
            let mut tail = Vec::new();
            if muxer.finish(&mut tail).is_ok() && delivered {
                delivered = self.send_blocking(tail, deadline);
            }
        }
        if !delivered {
            add(&self.stats.finish_lost, self.backlog.len() as u64);
        }
        self.backlog = VecDeque::new();
        if let Some(mut transport) = self.transport.take() {
            let left = deadline.saturating_since(self.clock.now());
            let _ = transport.finish(left);
        }
        self.stats.connected.store(false, Ordering::Relaxed);
        self.chunks = Vec::new();
        self.events = Vec::new();
    }
}
