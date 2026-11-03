//! Fakes of the encoder, muxer and transport ports and a frame source, for the node tests.
//! They record what they see in plain counters and logs; nothing sleeps or spawns.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Not every test uses every helper.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rbs_codec::{
    CreateError, EncodeError, EncoderChange, RateControl, VideoEncoder, VideoEncoderConfig,
    VideoEncoderFactory,
};
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{
    BitstreamFormat, Bytes, ChunkKind, CodecId, ColorSpace, ContainerId, EncodedPacket,
    FramePayload, MemoryKind, MuxChunk, PixelFormat, PlaneLayout, Size, StreamDescriptor,
    StreamIndex, StreamParams, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::description::{GraphDescription, NodeId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};
use rbs_mux::{Muxer, MuxerConfig, MuxerFactory};
use rbs_transport::{
    SendOutcome, Transport, TransportConfig, TransportError, TransportEvent, TransportFactory,
    TransportInput, TransportItem, TransportStats,
};

pub const W: u32 = 16;
pub const H: u32 = 8;

pub fn nv12() -> VideoFormat {
    VideoFormat::new(
        Size::new(W, H),
        PixelFormat::Nv12,
        ColorSpace::BT709_LIMITED,
    )
}

pub fn encoder_config() -> VideoEncoderConfig {
    VideoEncoderConfig::new(
        CodecId::H264,
        nv12(),
        FrameRate::FPS_60,
        MemoryKind::System,
        RateControl::Cbr {
            bitrate_bps: 1_000_000,
        },
    )
}

macro_rules! factory_basics {
    ($id:literal) => {
        fn id(&self) -> AdapterId {
            AdapterId::new($id)
        }
        fn display_name(&self) -> &str {
            $id
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    };
}

// ---- Frame source -------------------------------------------------------------------

/// A source that publishes a new system NV12 frame every `every` ticks (timestamp =
/// tick time, sequence = tick index); the frame stays on its slot in between.
pub struct FrameSource {
    desc: NodeDescriptor,
    every: u64,
    data: Bytes,
}

impl FrameSource {
    pub fn new(every: u64) -> Self {
        Self {
            desc: NodeDescriptor::new(
                NodeTypeId::new_static("org.rbs.test.frames"),
                NodeKind::Source,
                vec![],
                vec![Pin::one("video", LinkKind::Video)],
            ),
            every: every.max(1),
            data: Bytes::from(vec![0u8; (W * H * 3 / 2) as usize]),
        }
    }
}

impl Node<()> for FrameSource {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let tick = ctx.tick();
        if tick.index % self.every != 0 {
            return Ok(());
        }
        let planes = [
            PlaneLayout {
                offset: 0,
                stride: W as usize,
            },
            PlaneLayout {
                offset: (W * H) as usize,
                stride: W as usize,
            },
        ];
        let frame = SystemFrame::new(nv12(), self.data.clone(), &planes).unwrap();
        let frame = VideoFrame::system(tick.time, frame, None).with_sequence(tick.index);
        drop(ctx.outputs().set_video(0, Some(frame))?);
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
}

// ---- Encoder -------------------------------------------------------------------------

/// Shared knobs and counters of the fake encoder.
#[derive(Default)]
pub struct EncoderState {
    pub submitted: AtomicU64,
    pub keyframe_requests: AtomicU64,
    /// Packets produced per frame (the first of each frame carries the frame's pts).
    pub packets_per_frame: AtomicU64,
    /// Extradata of sessions created from now on.
    pub extradata: Mutex<Bytes>,
    pub sessions: AtomicU64,
}

pub struct FakeEncoderFactory(pub Arc<EncoderState>);

impl AdapterFactory for FakeEncoderFactory {
    factory_basics!("org.rbs.test.encoder");
}

impl VideoEncoderFactory for FakeEncoderFactory {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        self.0.sessions.fetch_add(1, Ordering::Relaxed);
        let extradata = self.0.extradata.lock().unwrap().clone();
        Ok(Box::new(FakeEncoder {
            state: Arc::clone(&self.0),
            stream: StreamDescriptor::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                extradata,
                StreamParams::video(config.format, config.frame_rate),
            ),
            pending: Vec::with_capacity(256),
            count: 0,
            payload: Bytes::from_static(b"frame"),
        }))
    }
}

pub struct FakeEncoder {
    state: Arc<EncoderState>,
    stream: StreamDescriptor,
    pending: Vec<EncodedPacket>,
    count: u64,
    payload: Bytes,
}

impl VideoEncoder for FakeEncoder {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        if !matches!(frame.payload(), FramePayload::System(_)) || *frame.format() != nv12() {
            return Err(EncodeError::InvalidInput("format".into()));
        }
        let n = self.state.packets_per_frame.load(Ordering::Relaxed).max(1);
        for i in 0..n {
            let pts = Timestamp::from_nanos(frame.timestamp().as_nanos() + i);
            let key = self.count.is_multiple_of(4) && i == 0;
            let packet = EncodedPacket::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                pts,
                pts,
                key,
                self.payload.clone(),
            )
            .unwrap();
            self.pending.push(packet);
        }
        self.count += 1;
        self.state.submitted.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        out.append(&mut self.pending);
        Ok(())
    }
    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError> {
        if matches!(change, EncoderChange::RequestKeyframe) {
            self.state.keyframe_requests.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        self.drain(out)
    }
}

// ---- Muxer ---------------------------------------------------------------------------

pub struct FakeMuxerFactory;

impl AdapterFactory for FakeMuxerFactory {
    factory_basics!("org.rbs.test.muxer");
}

impl MuxerFactory for FakeMuxerFactory {
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, rbs_mux::CreateError> {
        Ok(Box::new(FakeMuxer {
            streams: config.streams.len(),
        }))
    }
}

/// One `Media` chunk per packet whose data is the packet's data; header and trailer
/// chunks say what they are.
pub struct FakeMuxer {
    streams: usize,
}

impl Muxer for FakeMuxer {
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), rbs_mux::MuxError> {
        out.push(MuxChunk::new(
            ChunkKind::Header,
            Bytes::from_static(b"header"),
        ));
        Ok(())
    }
    fn write(
        &mut self,
        stream: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), rbs_mux::MuxError> {
        if stream.get() >= self.streams {
            return Err(rbs_mux::MuxError::UnknownStream(stream.0));
        }
        out.push(MuxChunk::new(
            ChunkKind::Media {
                keyframe: packet.keyframe(),
                timestamp: packet.dts(),
            },
            packet.data().clone(),
        ));
        Ok(())
    }
    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), rbs_mux::MuxError> {
        out.push(MuxChunk::new(
            ChunkKind::Trailer,
            Bytes::from_static(b"trailer"),
        ));
        Ok(())
    }
}

// ---- Transport -----------------------------------------------------------------------

/// What the fake transport received, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Got {
    Header,
    Media { timestamp: Timestamp, key: bool },
    Trailer,
    Packet { stream: u16, pts: Timestamp },
}

#[derive(Default)]
pub struct TransportState {
    /// While set, `send` returns `QueueFull`.
    pub full: AtomicBool,
    /// When false, nothing is logged (allocation tests); `sent` still counts.
    pub log_enabled: AtomicBool,
    pub log: Mutex<Vec<Got>>,
    pub sent: AtomicU64,
    pub started: AtomicU64,
    pub finished: AtomicU64,
}

impl TransportState {
    pub fn logging() -> Arc<Self> {
        let s = Self::default();
        s.log_enabled.store(true, Ordering::Relaxed);
        Arc::new(s)
    }
    pub fn got(&self) -> Vec<Got> {
        self.log.lock().unwrap().clone()
    }
}

pub struct FakeTransportFactory(pub Arc<TransportState>);

impl AdapterFactory for FakeTransportFactory {
    factory_basics!("org.rbs.test.transport");
}

impl TransportFactory for FakeTransportFactory {
    fn create(
        &self,
        _config: &TransportConfig,
    ) -> Result<Box<dyn Transport>, rbs_transport::CreateError> {
        Ok(Box::new(FakeTransport(Arc::clone(&self.0))))
    }
}

pub struct FakeTransport(Arc<TransportState>);

impl Transport for FakeTransport {
    fn start(&mut self, _streams: &[StreamDescriptor]) -> Result<(), TransportError> {
        self.0.started.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        if self.0.full.load(Ordering::Relaxed) {
            return Err(TransportError::QueueFull);
        }
        self.0.sent.fetch_add(1, Ordering::Relaxed);
        if self.0.log_enabled.load(Ordering::Relaxed) {
            let got = match item {
                TransportItem::Packet { stream, packet } => Got::Packet {
                    stream: stream.0,
                    pts: packet.pts(),
                },
                TransportItem::Chunk(chunk) => match chunk.kind {
                    ChunkKind::Header => Got::Header,
                    ChunkKind::Media {
                        timestamp,
                        keyframe,
                    } => Got::Media {
                        timestamp,
                        key: keyframe,
                    },
                    _ => Got::Trailer,
                },
                _ => unreachable!("new transport item kind"),
            };
            self.0.log.lock().unwrap().push(got);
        }
        Ok(SendOutcome::Queued)
    }
    fn poll_events(&mut self, _out: &mut Vec<TransportEvent>) {}
    fn stats(&self) -> TransportStats {
        TransportStats::default()
    }
    fn finish(&mut self, _timeout: std::time::Duration) -> Result<(), TransportError> {
        self.0.finished.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

pub fn file_config() -> TransportConfig {
    TransportConfig::new(
        "file:///tmp/never-written.mkv",
        TransportInput::Container(ContainerId::Matroska),
    )
}

// ---- Rig -----------------------------------------------------------------------------

/// One `()` partition with a controller, an executor and a graph.
pub struct Rig {
    pub graph: GraphDescription,
    pub ctl: Controller<()>,
    pub exec: Executor<()>,
    pub tick: u64,
}

impl Rig {
    pub fn new() -> Self {
        let (ctl, exec) = channel::<()>(Arc::new(()), ());
        Self {
            graph: GraphDescription::new(),
            ctl,
            exec,
            tick: 0,
        }
    }

    pub fn add(&mut self, node: Box<dyn Node<()>>) -> NodeId {
        use rbs_core::lifecycle::Lifecycle;
        let id = self
            .graph
            .add_node(Arc::new(node.descriptor().clone()))
            .unwrap();
        self.graph.set_state(id, Lifecycle::Standby).unwrap();
        self.graph.set_state(id, Lifecycle::Active).unwrap();
        self.ctl.adopt(id, node).unwrap();
        id
    }

    /// Applies the graph and swaps it in at once, so tick 0 already runs it.
    pub fn apply(&mut self) -> rbs_graph::runtime::ApplyReport {
        let report = self.ctl.apply(&self.graph).unwrap();
        self.exec.swap_pending();
        let _ = self.ctl.collect();
        report
    }

    pub fn tick(&mut self) {
        let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
        self.exec.run_tick(Tick::of(&cadence, self.tick));
        self.tick += 1;
        // Take back a pending swap, if any, so the next apply is not `Busy`.
        let _ = self.ctl.collect();
    }

    pub fn ticks(&mut self, n: u64) {
        for _ in 0..n {
            self.tick();
        }
    }

    /// Drains: one tick on the empty schedule, then collect (releases every node).
    pub fn drain(&mut self) {
        self.ctl.drain().unwrap();
        self.tick();
    }
}
