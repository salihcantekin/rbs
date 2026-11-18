//! The video encoder node: frames in, packets out, on its own partition
//! (spec `docs/specs/io-nodes.md`, "Video encoder node").

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_codec::{
    CreateError, EncodeError, EncoderChange, VideoEncoder, VideoEncoderConfig, VideoEncoderFactory,
};
use rbs_core::media::{EncodedPacket, MemoryKind, StreamDescriptor, VideoFrame};
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
    /// Timestamp in nanoseconds plus one of the last frame the session accepted; 0
    /// before the first (ADR-0045).
    pub last_frame: AtomicU64,
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
    /// Keyframes asked of the session through [`KeyframeAt`] (ADR-0124 decision 7).
    pub keyframes_requested: AtomicU64,
    /// Requests through [`KeyframeAt`] the session refused; the stream goes on.
    pub keyframes_refused: AtomicU64,
}

/// A request from the control thread for a keyframe at the first frame whose time is at
/// or after a given time (ADR-0124 decision 7). The node reads it once per new frame,
/// before that frame is submitted, and only when it was built with
/// [`VideoEncoderNode::with_keyframe_requests`]: a node without it never reads it.
#[derive(Debug)]
pub struct KeyframeAt {
    /// Nanoseconds of the requested time; `u64::MAX` for none.
    at: AtomicU64,
}

impl Default for KeyframeAt {
    fn default() -> Self {
        Self {
            at: AtomicU64::new(u64::MAX),
        }
    }
}

impl KeyframeAt {
    /// Asks for a keyframe at the first frame at or after `at`. Of two requests not yet
    /// served, the earlier time wins; a frame at or after it serves both.
    pub fn request(&self, at: Timestamp) {
        self.at.fetch_min(at.as_nanos(), Ordering::AcqRel);
    }

    /// The time of a request not yet served.
    pub fn pending(&self) -> Option<Timestamp> {
        match self.at.load(Ordering::Acquire) {
            u64::MAX => None,
            at => Some(Timestamp::from_nanos(at)),
        }
    }

    /// Takes the request when `frame_ns` is at or after it. A request made meanwhile for
    /// a later time stays, unless this frame serves it too.
    fn take_due(&self, frame_ns: u64) -> bool {
        let at = self.at.load(Ordering::Acquire);
        at != u64::MAX
            && frame_ns >= at
            && self
                .at
                .compare_exchange(at, u64::MAX, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }
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
    /// Keyframe requests from the control thread; `None` when the adapter does not
    /// declare `codec.reconfigure: keyframe`, so nothing is read per frame.
    keyframe_at: Option<Arc<KeyframeAt>>,
    stats: Arc<EncoderNodeStats>,
    /// Frames the probed session said it keeps after `submit` (ADR-0147 decision 4).
    frames_held: usize,
    /// The input is a native GPU memory kind (ADR-0147): every failure is permanent,
    /// so the studio rebuilds the chain with system memory (decision 9).
    native: bool,
    /// A native frame was refused: the next tick fails permanently, before it submits
    /// anything, so the packets of the refusing tick are kept (decision 9).
    fail_next: bool,
}

impl VideoEncoderNode {
    /// Creates an encoder session once (control thread) to learn [`Self::stream`] for the
    /// outputs' muxers and checks the configuration, then drops it: an installed node
    /// holds no encoder (P13). `prepare` creates the session that runs.
    pub fn open(
        factory: Arc<dyn VideoEncoderFactory>,
        config: VideoEncoderConfig,
    ) -> Result<Self, CreateError> {
        let probe = factory.create(&config)?;
        let stream = probe.stream().clone();
        let frames_held = probe.max_frames_held();
        drop(probe);
        let native = !matches!(config.memory, MemoryKind::System | MemoryKind::Gpu);
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
            keyframe_at: None,
            stats: Arc::default(),
            frames_held,
            native,
            fail_next: false,
        })
    }

    /// Frames the session keeps after `submit`, as the probe reported
    /// (`VideoEncoder::max_frames_held`): a native export sizes its ring with it.
    pub fn frames_held(&self) -> usize {
        self.frames_held
    }

    /// Lets the control thread ask for a keyframe at a time through the returned
    /// handle (ADR-0124 decision 7). Call it only for an adapter that declares
    /// `codec.reconfigure: keyframe`; the node then reads the request once per new
    /// frame. A node built without it reads nothing.
    pub fn with_keyframe_requests(mut self) -> (Self, Arc<KeyframeAt>) {
        let handle = Arc::new(KeyframeAt::default());
        self.keyframe_at = Some(Arc::clone(&handle));
        (self, handle)
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
        let due = self
            .keyframe_at
            .as_ref()
            .is_some_and(|k| k.take_due(frame.timestamp().as_nanos()));
        if due {
            // Before the frame, so this frame is the keyframe. A refusal leaves the
            // stream valid; the caller sees it in the counter.
            match session.reconfigure(&EncoderChange::RequestKeyframe) {
                Ok(()) => add(&self.stats.keyframes_requested, 1),
                Err(_) => add(&self.stats.keyframes_refused, 1),
            }
        }
        let mut result = session.submit(frame);
        if matches!(result, Err(EncodeError::Busy)) {
            session.drain(&mut self.drained).map_err(encode_error)?;
            result = session.submit(frame);
        }
        match result {
            Ok(()) => {
                add(&self.stats.submitted, 1);
                let at = frame.timestamp().as_nanos().saturating_add(1);
                self.stats.last_frame.store(at, Ordering::Relaxed);
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
                // A native frame refused: permanent from the next tick (ADR-0147).
                self.fail_next |= self.native;
                Ok(())
            }
            Err(_) if self.native => Err(NodeError::Permanent("native encoder input failed")),
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
            let native = self.native;
            let session = self.factory.create(&self.config).map_err(|e| match e {
                e if e.is_recoverable() && !native => NodeError::Failed(format!("encoder: {e}")),
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
        self.fail_next = false;
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

        if self.fail_next {
            return Err(NodeError::Permanent("encoder refused native input"));
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
