//! The output node's `process` per tick (hot path, P15): one video and two audio
//! packets a tick, delivered without a muxer to a transport that only counts them, so
//! the node's own work is what is measured. `plain` is the default output; `tracked`
//! keeps the span of what it delivers (`OutputNode::track_span`, ADR-0089).
//!
//! Run: `cargo bench -p rbs-nodes --bench output`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::media::{
    AudioFormat, BitstreamFormat, Bytes, CodecId, EncodedPacket, StreamDescriptor, StreamParams,
};
use rbs_core::time::Timestamp;
use rbs_core::timing::FrameRate;
use rbs_graph::node::{LinkKind, NodeDescriptor, NodeKind, NodeTypeId, Pin};
use rbs_graph::runtime::{Node, NodeError, PrepareCtx, ProcessCtx};
use rbs_nodes::OutputNode;
use rbs_transport::{TransportConfig, TransportInput};
use support::{FakeTransportFactory, Rig, TransportState, nv12};

/// Pushes one H.264 packet on pin 0 and two AAC packets on pin 1 each tick, with
/// times moving on as a 60 fps recording's do.
struct Feed {
    desc: NodeDescriptor,
}

impl Node<()> for Feed {
    fn descriptor(&self) -> &NodeDescriptor {
        &self.desc
    }
    fn prepare(&mut self, _: &mut PrepareCtx<'_, ()>) -> Result<(), NodeError> {
        Ok(())
    }
    fn process(&mut self, ctx: &mut ProcessCtx<'_, ()>) -> Result<(), NodeError> {
        let i = ctx.tick().index;
        let t = Timestamp::from_nanos(1_000_000_000 + i * 16_666_667);
        let video = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            t,
            t,
            i % 120 == 0,
            Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        )
        .unwrap();
        ctx.outputs().push_packet(0, video).unwrap();
        for k in 0..2u64 {
            let a = t + Duration::from_nanos(k * 8_000_000);
            let audio = EncodedPacket::new(
                CodecId::Aac,
                BitstreamFormat::Raw,
                a,
                a,
                true,
                Bytes::from_static(&[1]),
            )
            .unwrap()
            .with_duration(Duration::from_nanos(21_333_333));
            ctx.outputs().push_packet(1, audio).unwrap();
        }
        Ok(())
    }
    fn release(&mut self, _: &mut PrepareCtx<'_, ()>) {}
}

fn streams() -> Vec<StreamDescriptor> {
    vec![
        StreamDescriptor::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Bytes::new(),
            StreamParams::video(nv12(), FrameRate::FPS_60),
        ),
        StreamDescriptor::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            Bytes::new(),
            StreamParams::audio(AudioFormat::STEREO_48K, 1024),
        ),
    ]
}

fn rig(track: bool) -> Rig {
    let mut rig = Rig::new();
    let feed = rig.add(Box::new(Feed {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.bench.feed"),
            NodeKind::Source,
            Vec::new(),
            vec![
                Pin::named("p0", LinkKind::Packets, false),
                Pin::named("p1", LinkKind::Packets, false),
            ],
        ),
    }));
    // Counts only: no log, so the transport costs the same in both variants.
    let tx = Arc::new(TransportState::default());
    let output = OutputNode::new(
        streams(),
        Arc::new(FakeTransportFactory(tx)),
        TransportConfig::new("rtmp://never.invalid/x", TransportInput::Packets),
    );
    let output = if track {
        rig.add(Box::new(output.track_span()))
    } else {
        rig.add(Box::new(output))
    };
    for i in 0..2 {
        rig.graph
            .connect(feed, &format!("p{i}"), output, &format!("s{i}"))
            .unwrap();
    }
    assert!(rig.apply().failed.is_empty());
    // Past the aligned start, so every tick delivers.
    rig.ticks(4);
    rig
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("output_tick");
    for (name, track) in [("plain", false), ("tracked", true)] {
        let mut r = rig(track);
        group.bench_function(name, |b| {
            b.iter(|| {
                r.tick();
                black_box(&r);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
