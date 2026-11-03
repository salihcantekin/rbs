//! The whole audio path on the reference adapter: two inputs with a drifting device
//! clock (one at 44.1 kHz) -> mixer with meters -> audio queue -> encoder node on its
//! own partition. Timing, continuity and the allocation guard (ADR-0037).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_audio::{
    AudioGraphConfig, EncoderNode, InputNode, LevelMeter, MixerNode, MixerParams, Strip,
};
use rbs_audio_io::{AudioBackend, EndpointId, InputRequest};
use rbs_audio_io_test::{LOOPBACK, MONO_INPUT, TestAudioBackend};
use rbs_codec::{AudioEncoder, EncodeError, EncoderChange};
use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{
    AudioBlock, AudioFormat, BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamDescriptor,
    StreamParams,
};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_graph::description::{GraphDescription, NodeId, PartitionId};
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::partition::Lowering;
use rbs_graph::runtime::{
    Controller, Executor, Node, NodeError, PrepareCtx, ProcessCtx, Tick, channel,
};

const AUDIO: PartitionId = PartitionId(10);
const ENCODER: PartitionId = PartitionId(11);

/// Emits one packet per 1024 frames, pts on its own sample count from the first
/// block, like the AAC adapters; never allocates (static payload). Records the
/// timestamps of the blocks it gets.
struct FakeAac {
    stream: StreamDescriptor,
    origin: Option<Timestamp>,
    frames_in: u64,
    frames_out: u64,
    blocks: Arc<Mutex<Vec<(Timestamp, usize)>>>,
}

impl AudioEncoder for FakeAac {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn frame_size(&self) -> u32 {
        1024
    }
    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError> {
        self.origin.get_or_insert(block.timestamp());
        self.frames_in += block.frames() as u64;
        if let Ok(mut b) = self.blocks.try_lock() {
            if b.len() < b.capacity() {
                b.push((block.timestamp(), block.frames()));
            }
        }
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        while self.frames_in - self.frames_out >= 1024 && out.len() < out.capacity() {
            let pts = self.origin.unwrap_or(Timestamp::ZERO)
                + Duration::from_nanos(self.frames_out * 1_000_000_000 / 48_000);
            self.frames_out += 1024;
            out.push(
                EncodedPacket::new(
                    CodecId::Aac,
                    BitstreamFormat::Raw,
                    pts,
                    pts,
                    true,
                    Bytes::from_static(b"aac"),
                )
                .unwrap(),
            );
        }
        Ok(())
    }
    fn reconfigure(&mut self, _: &EncoderChange) -> Result<(), EncodeError> {
        Err(EncodeError::Unsupported)
    }
    fn flush(&mut self, _: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        Ok(())
    }
}

/// Counts packets and checks their pts increase.
struct PacketSink {
    desc: NodeDescriptor,
    count: Arc<AtomicU64>,
    last: Option<Timestamp>,
}

impl Node<()> for PacketSink {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        for p in ctx.inputs().packets(0) {
            if self.last.is_some_and(|l| p.pts() <= l) {
                return Err(NodeError::InvalidInput("pts went back"));
            }
            self.last = Some(p.pts());
            self.count.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

fn active(g: &mut GraphDescription, d: &NodeDescriptor, p: PartitionId) -> NodeId {
    let id = g.add_node(Arc::new(d.clone())).unwrap();
    g.set_state(id, Lifecycle::Standby).unwrap();
    g.set_state(id, Lifecycle::Active).unwrap();
    g.set_partition(id, p).unwrap();
    id
}

struct Rig {
    clock: TestClock,
    audio: (Controller<()>, Executor<()>),
    encoder: (Controller<()>, Executor<()>),
    blocks: Arc<Mutex<Vec<(Timestamp, usize)>>>,
    packets: Arc<AtomicU64>,
    meters: [Arc<LevelMeter>; 3],
    inputs: [Arc<rbs_audio::InputStats>; 2],
    queue: Arc<rbs_graph::runtime::queue::QueueStats>,
    config: AudioGraphConfig,
    tick: u64,
}

impl Rig {
    fn new() -> Self {
        let config = AudioGraphConfig::default();
        let clock = TestClock::new();
        let backend = TestAudioBackend::new();
        let open = |id: &str| {
            backend
                .open_input(&InputRequest::new(
                    EndpointId::new(id),
                    Arc::new(clock.clone()),
                ))
                .unwrap()
        };
        let (mic, _, mic_stats) = InputNode::new(open(MONO_INPUT), config, Duration::ZERO);
        let (lb, _, lb_stats) = InputNode::new(open(LOOPBACK), config, Duration::ZERO);
        let meters = [
            Arc::new(LevelMeter::new(2)),
            Arc::new(LevelMeter::new(2)),
            Arc::new(LevelMeter::new(2)),
        ];
        let mut g = GraphDescription::new();
        let mic_id = active(&mut g, Node::<()>::descriptor(&mic), AUDIO);
        let lb_id = active(&mut g, Node::<()>::descriptor(&lb), AUDIO);
        let params = MixerParams::new(vec![
            Strip::new(mic_id, 1.0).with_meter(Arc::clone(&meters[0])),
            Strip::new(lb_id, 0.5).with_meter(Arc::clone(&meters[1])),
        ])
        .with_master_meter(Arc::clone(&meters[2]));
        let (mixer, _mixer_params, _) = MixerNode::new(config, 8, params);
        let mix_id = active(&mut g, Node::<()>::descriptor(&mixer), AUDIO);
        let blocks = Arc::new(Mutex::new(Vec::with_capacity(10_000)));
        let enc = EncoderNode::new(
            Box::new(FakeAac {
                stream: StreamDescriptor::new(
                    CodecId::Aac,
                    BitstreamFormat::Raw,
                    Bytes::new(),
                    StreamParams::audio(AudioFormat::STEREO_48K, 1024),
                ),
                origin: None,
                frames_in: 0,
                frames_out: 0,
                blocks: Arc::clone(&blocks),
            }),
            64,
        );
        let enc_id = active(&mut g, Node::<()>::descriptor(&enc), ENCODER);
        let packets = Arc::new(AtomicU64::new(0));
        let sink = PacketSink {
            desc: NodeDescriptor::new(
                NodeTypeId::new("org.rbs.test.packet-sink"),
                NodeKind::Output,
                vec![Pin::one("in", LinkKind::Packets)],
                vec![],
            ),
            count: Arc::clone(&packets),
            last: None,
        };
        let sink_id = active(&mut g, &sink.desc, ENCODER);
        g.connect(mic_id, "out", mix_id, "in").unwrap();
        g.connect(lb_id, "out", mix_id, "in").unwrap();
        g.connect(mix_id, "main", enc_id, "in").unwrap();
        g.connect(enc_id, "packets", sink_id, "in").unwrap();

        let mut lowering = Lowering::new();
        lowering.set_audio_queue(50, config.block_frames);
        let mut lowered = lowering.lower(&g);
        assert!(lowered.problems.is_empty());
        let mut audio = channel::<()>(Arc::new(()), ());
        let mut encoder = channel::<()>(Arc::new(()), ());
        audio.0.set_audio_block_frames(config.block_frames);
        encoder.0.set_audio_block_frames(config.block_frames);
        audio.0.adopt(mic_id, Box::new(mic)).unwrap();
        audio.0.adopt(lb_id, Box::new(lb)).unwrap();
        audio.0.adopt(mix_id, Box::new(mixer)).unwrap();
        encoder.0.adopt(enc_id, Box::new(enc)).unwrap();
        encoder.0.adopt(sink_id, Box::new(sink)).unwrap();
        let new = lowered.created.pop().unwrap();
        let queue = Arc::clone(&new.crossing.stats);
        let (w, r) = new.ends.writer::<()>();
        audio.0.adopt(new.crossing.writer, w).unwrap();
        encoder
            .0
            .adopt(new.crossing.reader, r.into_node::<()>())
            .unwrap();
        for (p, part) in &lowered.parts {
            let (ctl, exec) = if *p == AUDIO {
                &mut audio
            } else {
                &mut encoder
            };
            let report = ctl.apply(part).unwrap();
            assert!(report.failed.is_empty(), "{:?}", report.failed);
            exec.swap_pending();
            ctl.collect().unwrap().unwrap();
        }
        Self {
            clock,
            audio,
            encoder,
            blocks,
            packets,
            meters,
            inputs: [mic_stats, lb_stats],
            queue,
            config,
            tick: 0,
        }
    }

    /// `n` audio ticks at their times, each followed by an encoder tick.
    fn run(&mut self, n: u64) {
        let cadence = self.config.cadence(Timestamp::ZERO);
        for _ in 0..n {
            let t = cadence.time_of(self.tick);
            let now = self.clock.now();
            if t > now {
                self.clock.advance(t.saturating_since(now));
            }
            self.audio.1.run_tick(Tick::of(&cadence, self.tick));
            self.encoder.1.run_tick(Tick::new(self.tick, t));
            self.tick += 1;
        }
    }
}

#[test]
fn mixed_audio_reaches_the_encoder_continuous_and_on_the_block_grid() {
    let mut r = Rig::new();
    r.run(1_000); // 10 s
    let blocks = r.blocks.lock().unwrap().clone();
    assert!(blocks.len() >= 990, "{}", blocks.len());
    for (i, w) in blocks.windows(2).enumerate() {
        assert_eq!(
            w[1].0.saturating_since(w[0].0),
            Duration::from_millis(10),
            "block {i}"
        );
        assert_eq!(w[0].1, 480);
    }
    // First block: the first tick at or after the latency.
    assert_eq!(blocks[0].0, Timestamp::ZERO);
    for s in &r.inputs {
        assert_eq!(s.read().resyncs, 0);
    }
    assert_eq!(r.queue.overflows(), 0);
    assert_eq!(r.queue.filled(), 0);
    assert!(r.packets.load(Ordering::Relaxed) >= 460);
    // The test adapter's tone peaks at 0.5; the loopback strip is at half gain.
    let mic = r.meters[0].take();
    let lb = r.meters[1].take();
    assert!((mic.peak(0) - 0.5).abs() < 0.02, "{}", mic.peak(0));
    assert!((lb.peak(0) - 0.25).abs() < 0.02, "{}", lb.peak(0));
    assert!(r.meters[2].take().peak(0) > 0.5);
}

#[test]
fn the_audio_partition_does_not_allocate_per_block() {
    let mut r = Rig::new();
    r.run(200);
    let cadence = r.config.cadence(Timestamp::ZERO);
    let mut tick = r.tick;
    let clock = r.clock.clone();
    let audio = &mut r.audio.1;
    let counts = allocation_counter::measure(|| {
        for _ in 0..500 {
            let t = cadence.time_of(tick);
            clock.advance(t.saturating_since(clock.now()));
            audio.run_tick(Tick::of(&cadence, tick));
            tick += 1;
        }
    });
    assert_eq!(
        counts.count_total, 0,
        "input nodes, mixer or queue writer allocated"
    );
    r.tick = tick;
    // The encoder side catches up without allocating either (static payloads).
    let encoder = &mut r.encoder.1;
    let counts = allocation_counter::measure(|| {
        for k in 0..600 {
            encoder.run_tick(Tick::new(10_000 + k, Timestamp::ZERO));
        }
    });
    assert_eq!(
        counts.count_total, 0,
        "queue reader or encoder node allocated"
    );
}
