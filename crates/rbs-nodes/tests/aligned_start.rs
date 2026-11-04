//! The output node starts at the first video packet (spec `io-nodes.md`, "Output
//! node", aligned start): packets of other streams that end before it are dropped, and
//! those that come first are held until it arrives. Ticks are driven by hand.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamDescriptor, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};
use rbs_nodes::OutputNode;
use rbs_nodes::output::{OutputNodeStats, START_HOLD};
use rbs_transport::{TransportConfig, TransportInput};
use support::{FakeTransportFactory, Got, Rig, TransportState, nv12};

const AAC_FRAME: Duration = Duration::from_nanos(21_333_333);

fn h264_stream() -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        Bytes::new(),
        StreamParams::video(nv12(), FrameRate::FPS_60),
    )
}

fn aac_stream() -> StreamDescriptor {
    StreamDescriptor::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        Bytes::new(),
        StreamParams::audio(AudioFormat::STEREO_48K, 1024),
    )
}

fn ms(v: u64) -> Timestamp {
    Timestamp::from_nanos(v * 1_000_000)
}

/// Pushes scripted packets: `(tick, pin, packet)`, pin 0 video, pin 1 audio.
struct Script {
    desc: NodeDescriptor,
    packets: Vec<(u64, usize, EncodedPacket)>,
}

impl Node<()> for Script {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let index = ctx.tick().index;
        for (tick, pin, packet) in &self.packets {
            if *tick == index {
                ctx.outputs().push_packet(*pin, packet.clone()).unwrap();
            }
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
}

fn video(pts: u64) -> EncodedPacket {
    EncodedPacket::new(
        CodecId::H264,
        BitstreamFormat::AnnexB,
        ms(pts),
        ms(pts),
        true,
        Bytes::from_static(&[0, 0, 0, 1, 0x65]),
    )
    .unwrap()
}

fn audio(pts_ns: u64) -> EncodedPacket {
    let t = Timestamp::from_nanos(pts_ns);
    EncodedPacket::new(
        CodecId::Aac,
        BitstreamFormat::Raw,
        t,
        t,
        true,
        Bytes::from_static(&[1]),
    )
    .unwrap()
    .with_duration(AAC_FRAME)
}

/// The n-th AAC frame from 1 s on.
fn aac(n: u64) -> u64 {
    1_000_000_000 + n * 21_333_333
}

fn run(
    streams: Vec<StreamDescriptor>,
    packets: Vec<(u64, usize, EncodedPacket)>,
    ticks: u64,
) -> (Vec<Got>, Arc<OutputNodeStats>) {
    let pins = streams.len();
    let tx = TransportState::logging();
    let mut rig = Rig::new();
    let outputs = (0..pins)
        .map(|i| Pin::named(format!("p{i}"), LinkKind::Packets, false))
        .collect();
    let script = rig.add(Box::new(Script {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.script"),
            NodeKind::Source,
            Vec::new(),
            outputs,
        ),
        packets,
    }));
    let output = OutputNode::new(
        streams,
        Arc::new(FakeTransportFactory(Arc::clone(&tx))),
        TransportConfig::new("rtmp://never.invalid/x", TransportInput::Packets),
    );
    let stats = output.stats();
    let output = rig.add(Box::new(output));
    for i in 0..pins {
        rig.graph
            .connect(script, &format!("p{i}"), output, &format!("s{i}"))
            .unwrap();
    }
    let report = rig.apply();
    assert!(report.failed.is_empty(), "{report:?}");
    rig.ticks(ticks);
    rig.drain();
    (tx.got(), stats)
}

fn packets(got: &[Got]) -> Vec<(u16, u64)> {
    got.iter()
        .filter_map(|g| match g {
            Got::Packet { stream, pts } => Some((*stream, pts.as_nanos())),
            _ => None,
        })
        .collect()
}

#[test]
fn audio_before_the_first_video_packet_is_dropped() {
    // Audio from 1 s; the first video frame is at 1.143 s and comes three ticks later
    // (encoder latency). Audio that ends by then is dropped, the frame that spans the
    // start is kept.
    let mut script: Vec<_> = (0..10).map(|n| (n / 3, 1, audio(aac(n)))).collect();
    script.push((3, 0, video(1143)));
    script.push((4, 0, video(1160)));
    script.push((4, 1, audio(aac(10))));
    let (got, stats) = run(vec![h264_stream(), aac_stream()], script, 6);

    let got = packets(&got);
    // AAC frame 6 spans 1.128..1.149 s: kept. Frames 0..=5 end at or before 1.143 s.
    assert_eq!(
        got,
        vec![
            (1, aac(6)),
            (1, aac(7)),
            (1, aac(8)),
            // Frame 9 arrives on the video packet's tick, after it (stream order).
            (0, 1_143_000_000),
            (1, aac(9)),
            (0, 1_160_000_000),
            (1, aac(10)),
        ]
    );
    assert_eq!(stats.start_drops.load(Ordering::Relaxed), 6);
}

#[test]
fn late_audio_before_the_start_is_dropped_too() {
    // Video first, then an audio packet that ended before it (queued late).
    let script = vec![
        (0, 0, video(1100)),
        (1, 1, audio(aac(0))),
        (1, 1, audio(aac(5))),
    ];
    let (got, stats) = run(vec![h264_stream(), aac_stream()], script, 3);
    assert_eq!(packets(&got), vec![(0, 1_100_000_000), (1, aac(5))]);
    assert_eq!(stats.start_drops.load(Ordering::Relaxed), 1);
}

#[test]
fn an_output_without_video_or_with_video_only_passes_everything() {
    let (got, stats) = run(
        vec![aac_stream()],
        vec![(0, 0, audio(aac(0))), (1, 0, audio(aac(1)))],
        3,
    );
    assert_eq!(packets(&got).len(), 2);
    assert_eq!(stats.start_drops.load(Ordering::Relaxed), 0);
    let (got, _) = run(vec![h264_stream()], vec![(0, 0, video(5))], 2);
    assert_eq!(packets(&got).len(), 1);
}

#[test]
fn held_packets_are_bounded_and_written_when_no_video_ever_comes() {
    let n = START_HOLD as u64 + 10;
    let script = (0..n).map(|i| (i / 16, 1, audio(aac(i)))).collect();
    let (got, stats) = run(vec![h264_stream(), aac_stream()], script, n / 16 + 2);
    // The oldest ten were dropped from the full hold; the rest go out at release.
    let got = packets(&got);
    assert_eq!(got.len(), START_HOLD);
    assert_eq!(got[0], (1, aac(10)));
    assert_eq!(stats.start_drops.load(Ordering::Relaxed), 10);
}
