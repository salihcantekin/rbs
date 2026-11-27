//! The video encoder node's `process` per tick (hot path, P15), with a fake encoder so
//! the node's own work is what is measured: a new frame each tick into an encoder that
//! makes one packet, passed to a counting output. `plain` is the default node;
//! `keyframe_requests` reads a pending-free keyframe request each frame (ADR-0124
//! decision 7); `bitrate_changes` also reads an empty bitrate target (ADR-0139
//! decision 8). Compare `plain` on main and on the change, and the three variants.
//!
//! Run: `cargo bench -p rbs-nodes --bench encoder`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_nodes::{OutputNode, VideoEncoderNode};
use rbs_transport::{TransportConfig, TransportInput};
use support::{
    EncoderState, FakeEncoderFactory, FakeTransportFactory, FrameSource, Rig, TransportState,
    encoder_config,
};

#[derive(Clone, Copy)]
enum Variant {
    Plain,
    Keyframes,
    Bitrate,
}

fn rig(variant: Variant) -> Rig {
    let mut rig = Rig::new();
    let encoder = VideoEncoderNode::open(
        Arc::new(FakeEncoderFactory(Arc::new(EncoderState::default()))),
        encoder_config(),
    )
    .unwrap();
    // Nothing is requested: the node holds its own reference to each handle.
    let encoder = match variant {
        Variant::Plain => encoder,
        Variant::Keyframes => encoder.with_keyframe_requests().0,
        Variant::Bitrate => encoder.with_bitrate_changes().0,
    };
    let stream = encoder.stream().clone();
    let output = OutputNode::new(
        vec![stream],
        Arc::new(FakeTransportFactory(Arc::new(TransportState::default()))),
        TransportConfig::new("rtmp://never.invalid/x", TransportInput::Packets),
    );
    let source = rig.add(Box::new(FrameSource::new(1)));
    let encoder = rig.add(Box::new(encoder));
    let output = rig.add(Box::new(output));
    rig.graph
        .connect(source, "video", encoder, "video")
        .unwrap();
    rig.graph.connect(encoder, "packets", output, "s0").unwrap();
    assert!(rig.apply().failed.is_empty());
    rig.ticks(4);
    rig
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("encoder_tick");
    for (name, variant) in [
        ("plain", Variant::Plain),
        ("keyframe_requests", Variant::Keyframes),
        ("bitrate_changes", Variant::Bitrate),
    ] {
        let mut r = rig(variant);
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
