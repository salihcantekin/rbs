//! The video encoder node's `process` per tick (hot path, P15), with a fake encoder so
//! the node's own work is what is measured: a new frame each tick into an encoder that
//! makes one packet, passed to a counting output. `plain` is the default node;
//! `keyframe_requests` reads a pending-free keyframe request each frame (ADR-0124
//! decision 7). Compare `plain` on main and on the change, and the two variants.
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

fn rig(keyframes: bool) -> Rig {
    let mut rig = Rig::new();
    let encoder = VideoEncoderNode::open(
        Arc::new(FakeEncoderFactory(Arc::new(EncoderState::default()))),
        encoder_config(),
    )
    .unwrap();
    let encoder = if keyframes {
        // Nothing is requested: the node holds its own reference to the request.
        encoder.with_keyframe_requests().0
    } else {
        encoder
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
    for (name, keyframes) in [("plain", false), ("keyframe_requests", true)] {
        let mut r = rig(keyframes);
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
