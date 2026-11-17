//! Browser picture hand-off (ADR-0109 decision 8): the render thread's
//! `BrowserFrameSink::take_due` (hot path, per tick) and the supervisor's
//! `BrowserFrameSink::publish_at` with `BrowserPictureEdge::stamp`, 1920x1080 RGBA.
//!
//! `cargo bench -p rbs-sources --bench browser_picture`

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_sources::{BrowserFrameSink, BrowserPictureEdge};

const W: u32 = 1920;
const H: u32 = 1080;

fn bench(c: &mut Criterion) {
    let rgba = vec![128u8; (W * H * 4) as usize];
    let mut group = c.benchmark_group("browser_picture");
    // A tick with no new picture: what most render ticks of a still page cost.
    group.bench_function("take_due_nothing", |b| {
        let sink = BrowserFrameSink::new();
        b.iter(|| std::hint::black_box(sink.take_due(Timestamp::from_nanos(1))));
    });
    // A tick that takes a due picture and drops its reference (the upload is timed by
    // the GPU upload benches).
    group.bench_function("take_due_picture_1080p", |b| {
        let sink = BrowserFrameSink::new();
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            for _ in 0..iters {
                sink.publish_at(&rgba, W, H, Some(Timestamp::ZERO));
                let start = std::time::Instant::now();
                std::hint::black_box(sink.take_due(Timestamp::ZERO));
                spent += start.elapsed();
            }
            spent
        });
    });
    // The supervisor: stamp and publish one picture (one copy into reused bytes).
    group.bench_function("stamp_publish_1080p", |b| {
        let clock = TestClock::new();
        let mut edge = BrowserPictureEdge::new(Arc::new(clock.clone()));
        let sink = BrowserFrameSink::new();
        b.iter(|| {
            clock.advance(Duration::from_millis(16));
            let os = 1_000_000_000_000 + clock.now().as_nanos();
            let stamp = edge.stamp(os - 3_000_000, os);
            sink.publish_at(&rgba, W, H, Some(stamp));
            drop(sink.take_due(stamp));
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
