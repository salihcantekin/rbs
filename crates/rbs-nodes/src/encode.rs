//! The video encoder node: frames in, packets out, on its own partition
//! (spec `docs/specs/io-nodes.md`, "Video encoder node").

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_codec::{
    CreateError, EncodeError, EncoderChange, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory,
};
use rbs_core::media::{EncodedPacket, StreamDescriptor, VideoFrame};
use rbs_core::time::Timestamp;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx, Runtime};

/// Node type id of the video encoder.
pub const VIDEO_ENCODER_NODE_TYPE: &str = "org.rbs.video-encoder";
/// Index of the "video" input pin.
pub const VIDEO_PIN: usize = 0;
/// Index of the "packets" output pin.
pub const PACKETS_PIN: usize = 0;
/// Packets kept for the next tick when the output slot is full (`PER_TICK_EVENTS`).
pub const CARRY_PACKETS: usize = 64;
/// Room reserved for one `drain`.
const DRAIN_CAPACITY: usize = 64;

/// The video encoder node's descriptor: kind `Encoder`, input "video", output "packets".
pub fn video_encoder_descriptor() -> NodeDescriptor {
    NodeDescriptor::new(
        NodeTypeId::new_static(VIDEO_ENCODER_NODE_TYPE),
        NodeKind::Encoder,
        vec![Pin::one("video", LinkKind::Video)],
        vec![Pin::one("packets", LinkKind::Packets)],
    )
}

/// Counters of a video encoder node, readable from any thread.
#[derive(Debug, Default)]
pub struct EncoderNodeStats {
    /// Frames the session accepted.
    pub submitted: AtomicU64,
    /// Packets passed on.
    pub packets: AtomicU64,
    /// Frames dropped because the session stayed busy after a drain.
    pub busy_drops: AtomicU64,
    /// Frames the session refused as invalid (wrong format, size or time).
    pub refused: AtomicU64,
    /// Packets dropped because the carry-over buffer was full, and the packets after
    /// them up to the next keyframe.
    pub overflow_drops: AtomicU64,
    /// Packets lost at `release`: the node left the schedule without a final tick (a
    /// live edit, or a drain that timed out), so the session's flush and the carried
    /// packets had no tick left to pass them on.
    pub flush_lost: AtomicU64,
    /// Final ticks on which the session was flushed into the graph (ADR-0043).
    pub flushes: AtomicU64,
}

fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Ordering::Relaxed);
}

/// Owns one video encoder session and runs it on the encoder's partition.
pub struct VideoEncoderNode {
    descriptor: NodeDescriptor,
    factory: Arc<dyn VideoEncoderFactory>,
    config: VideoEncoderConfig,
    stream: StreamDescriptor,
    session: Option<Box<dyn VideoEncoder>>,
    /// (timestamp, sequence) of the last frame given to the session.
    last: Option<(Timestamp, u64)>,
    drained: Vec<EncodedPacket>,
    carry: VecDeque<EncodedPacket>,
    /// After a carry overflow: packets are dropped until the next keyframe.
    resync: bool,
    /// The session was flushed on a final tick: it takes no more frames.
    ended: bool,
    stats: Arc<EncoderNodeStats>,
}

impl VideoEncoderNode {
    /// Creates an encoder session once (control thread) to learn [`Self::stream`] for the
    /// outputs' muxers and checks the configuration, then drops it: an installed node
    /// holds no encoder (P13). `prepare` creates the session that runs.
    pub fn open(
        factory: Arc<dyn VideoEncoderFactory>,
        config: VideoEncoderConfig,
    ) -> Result<Self, CreateError> {
        let stream = factory.create(&config)?.stream().clone();
        Ok(Self {
            descriptor: video_encoder_descriptor(),
            factory,
            config,
            stream,
            session: None,
            last: None,
            drained: Vec::new(),
            carry: VecDeque::new(),
            resync: false,
            ended: false,
            stats: Arc::default(),
        })
    }

    /// The stream every session of this node produces. A session created again after a
    /// restart must produce the same descriptor, or `prepare` fails permanently.
    pub fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }

    /// The encoder configuration.
    pub fn config(&self) -> &VideoEncoderConfig {
        &self.config
    }

    /// The node's counters.
    pub fn stats(&self) -> Arc<EncoderNodeStats> {
        Arc::clone(&self.stats)
    }

    /// Submits `frame`, draining once and retrying when the session is busy.
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), NodeError> {
        let session = self.session.as_mut().ok_or(NodeError::NotReady)?;
        let mut result = session.submit(frame);
        if matches!(result, Err(EncodeError::Busy)) {
            session.drain(&mut self.drained).map_err(encode_error)?;
            result = session.submit(frame);
        }
        match result {
            Ok(()) => {
                add(&self.stats.submitted, 1);
                Ok(())
            }
            Err(EncodeError::Busy) => {
                add(&self.stats.busy_drops, 1);
                Ok(())
            }
            // Not an error of the tick: packets already pushed this tick would be
            // discarded with it and break the stream (spec, invariant 4).
            Err(EncodeError::InvalidInput(_)) => {
                add(&self.stats.refused, 1);
                Ok(())
            }
            Err(e) => Err(encode_error(e)),
        }
    }

    /// Passes `packet` on, or keeps it for the next tick.
    fn pass(
        &mut self,
        outputs: &mut rbs_graph::runtime::Outputs<'_>,
        packet: EncodedPacket,
    ) -> Result<(), NodeError> {
        if self.resync {
            if !packet.keyframe() {
                add(&self.stats.overflow_drops, 1);
                return Ok(());
            }
            self.resync = false;
        }
        if !self.carry.is_empty() {
            return self.keep(packet);
        }
        match outputs.push_packet(PACKETS_PIN, packet) {
            Ok(()) => {
                add(&self.stats.packets, 1);
                Ok(())
            }
            Err(rejected) => self.keep(rejected.value),
        }
    }

    /// Keeps `packet` for the next tick; when the carry-over is full, drops it and
    /// resynchronises at the next keyframe, so the output never gets a broken stream.
    fn keep(&mut self, packet: EncodedPacket) -> Result<(), NodeError> {
        if self.carry.len() < CARRY_PACKETS {
            self.carry.push_back(packet);
            return Ok(());
        }
        add(&self.stats.overflow_drops, 1);
        self.resync = true;
        if let Some(session) = self.session.as_mut() {
            // Best effort: an encoder that cannot do it resyncs at its next interval.
            let _ = session.reconfigure(&EncoderChange::RequestKeyframe);
        }
        Ok(())
    }
}

fn encode_error(e: EncodeError) -> NodeError {
    match e {
        EncodeError::DeviceLost => NodeError::Permanent("encoder device lost"),
        e if e.is_recoverable() => NodeError::Backend("encoder error"),
        _ => NodeError::Permanent("encoder failed"),
    }
}

impl<X: Runtime> Node<X> for VideoEncoderNode {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _ctx: &mut PrepareCtx<'_, X>) -> Result<(), NodeError> {
        if self.session.is_none() {
            let session = self.factory.create(&self.config).map_err(|e| match e {
                e if e.is_recoverable() => NodeError::Failed(format!("encoder: {e}")),
                _ => NodeError::Permanent("the encoder cannot be created"),
            })?;
            if *session.stream() != self.stream {
                // The outputs wrote headers for the first stream; a different one needs a
                // new output chain (output ports spec, `VideoEncoder::stream`).
                return Err(NodeError::Permanent("encoder stream changed after restart"));
            }
            self.session = Some(session);
        }
        self.last = None;
        self.resync = false;
        self.ended = false;
        self.drained = Vec::with_capacity(DRAIN_CAPACITY);
        self.carry = VecDeque::with_capacity(CARRY_PACKETS);
        Ok(())
    }

    fn process(&mut self, ctx: &mut ProcessCtx<'_, X>) -> Result<(), NodeError> {
        let draining = ctx.tick().draining;
        let inputs = ctx.inputs();
        let outputs = ctx.outputs();

        // Packets left over from earlier ticks go first, in order.
        while let Some(packet) = self.carry.pop_front() {
            if let Err(rejected) = outputs.push_packet(PACKETS_PIN, packet) {
                self.carry.push_front(rejected.value);
                break;
            }
            add(&self.stats.packets, 1);
        }

        let mut result = Ok(());
        if self.ended {
            return result;
        }
        if let Some(frame) = inputs.video(VIDEO_PIN, 0) {
            let key = (frame.timestamp(), frame.sequence());
            if self.last != Some(key) {
                self.last = Some(key);
                result = self.submit(frame);
            }
        }

        let session = self.session.as_mut().ok_or(NodeError::NotReady)?;
        if draining {
            // End of stream (ADR-0043): the frame above was the last one. A flush may
            // wait for the encoder's in-flight frames and may grow `drained` past its
            // reserve; final ticks are bounded by the drain timeout. A cadence partition
            // runs one final tick, so packets past the pin's per-tick limit are carried
            // and counted as `flush_lost` at `release` (hardware sessions hold at most
            // their slots, a few frames, well below it).
            session.flush(&mut self.drained).map_err(encode_error)?;
            self.ended = true;
            add(&self.stats.flushes, 1);
        } else {
            session.drain(&mut self.drained).map_err(encode_error)?;
        }
        // `drained` keeps its capacity; `pass` moves each packet out.
        let mut drained = std::mem::take(&mut self.drained);
        for packet in drained.drain(..) {
            self.pass(outputs, packet)?;
        }
        self.drained = drained;
        result
    }

    fn release(&mut self, _ctx: &mut PrepareCtx<'_, X>) {
        if let Some(mut session) = self.session.take() {
            let mut out = Vec::new();
            if session.flush(&mut out).is_ok() {
                add(&self.stats.flush_lost, out.len() as u64);
            }
        }
        add(&self.stats.flush_lost, self.carry.len() as u64);
        self.carry = VecDeque::new();
        self.drained = Vec::new();
    }
}
