//! The output node: packets in, delivered through an optional muxer and a transport, on
//! its own partition (spec `docs/specs/io-nodes.md`, "Output node").

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rbs_core::media::{ContainerId, EncodedPacket, MuxChunk, StreamDescriptor, StreamIndex};
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
    /// Items still undelivered when `release` gave up.
    pub finish_lost: AtomicU64,
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
    transport: Option<Box<dyn Transport>>,
    muxer: Option<Box<dyn Muxer>>,
    backlog: VecDeque<Item>,
    chunks: Vec<MuxChunk>,
    events: Vec<TransportEvent>,
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
        Self {
            descriptor: output_descriptor(streams.len()),
            streams,
            transport_factory: factory,
            transport_config: config,
            muxer_setup: None,
            backlog_limit: DEFAULT_BACKLOG,
            finish_timeout: DEFAULT_FINISH_TIMEOUT,
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

    /// The node's counters.
    pub fn stats(&self) -> Arc<OutputNodeStats> {
        Arc::clone(&self.stats)
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
                Err(_) => return Err(NodeError::Permanent("transport failed")),
            }
        }
        if self.backlog.len() >= self.backlog_limit {
            return Err(NodeError::Permanent(
                "output backlog full: the destination is too slow",
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
                Err(_) => return Err(NodeError::Permanent("transport failed")),
            }
        }
        Ok(true)
    }

    fn count(&self, outcome: SendOutcome) {
        count(&self.stats, outcome);
    }

    /// Muxes or forwards one packet of stream `stream`.
    fn write(&mut self, stream: usize, packet: &EncodedPacket) -> Result<(), NodeError> {
        add(&self.stats.packets, 1);
        let index = StreamIndex(stream as u16);
        let Some(muxer) = self.muxer.as_mut() else {
            // Shares the payload; no copy.
            return self.send(Item::Packet(index, packet.clone()));
        };
        match muxer.write(index, packet, &mut self.chunks) {
            Ok(()) => {}
            Err(MuxError::NonMonotonic { .. }) => {
                add(&self.stats.mux_drops, 1);
                return Ok(());
            }
            Err(_) => return Err(NodeError::Permanent("muxer failed")),
        }
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
    fn drain_until(&mut self, deadline: Instant) -> bool {
        loop {
            match self.drain_backlog() {
                Ok(true) => return true,
                Ok(false) if Instant::now() < deadline => std::thread::sleep(FINISH_RETRY),
                _ => return false,
            }
        }
    }

    /// Sends `chunks` from `prepare` or `release` (control thread), waiting for room.
    fn send_blocking(&mut self, chunks: Vec<MuxChunk>, deadline: Instant) -> bool {
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
                Some(
                    setup
                        .factory
                        .create(&config)
                        .map_err(|e| NodeError::Failed(format!("muxer: {e}")))?,
                )
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
            .map_err(|e| NodeError::Failed(format!("transport: {e}")))?;
        transport
            .start(&self.streams)
            .map_err(|e| NodeError::Failed(format!("transport start: {e}")))?;
        self.transport = Some(transport);
        self.backlog = VecDeque::with_capacity(self.backlog_limit);
        self.chunks = Vec::with_capacity(16);
        self.events = Vec::with_capacity(16);
        if let Some(mut muxer) = muxer {
            let mut header = Vec::new();
            let begun = muxer.begin(&mut header);
            self.muxer = Some(muxer);
            if let Err(e) = begun {
                return Err(NodeError::Failed(format!("muxer begin: {e}")));
            }
            let deadline = Instant::now() + self.finish_timeout;
            if !self.send_blocking(header, deadline) {
                return Err(NodeError::Failed("could not write the header".into()));
            }
        }
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let inputs = ctx.inputs();
        self.drain_backlog()?;
        for stream in 0..self.streams.len() {
            for packet in inputs.packets(stream) {
                self.write(stream, packet)?;
            }
        }
        let transport = self.transport.as_mut().ok_or(NodeError::NotReady)?;
        self.events.clear();
        transport.poll_events(&mut self.events);
        let mut disconnected = false;
        for event in &self.events {
            match event {
                TransportEvent::KeyframeRequested => add(&self.stats.keyframe_requests, 1),
                TransportEvent::Disconnected { .. } => disconnected = true,
                _ => {}
            }
        }
        if disconnected {
            return Err(NodeError::Permanent("transport disconnected"));
        }
        Ok(())
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        let deadline = Instant::now() + self.finish_timeout;
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
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = transport.finish(left);
        }
        self.chunks = Vec::new();
        self.events = Vec::new();
    }
}
