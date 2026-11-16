//! One tick of each media node (hot path, P15; ADR-0106, ADR-0108 decision 5):
//! `picture_tick` is the picture node's dequeue at a render tick (the CPU part; the
//! upload is the existing `SourceUpload` path), `sound_block` is the sound node
//! writing one 10 ms block. The decode worker runs on its own thread with a
//! `TestClock`; only the node call is timed.
//!
//! Run: `cargo bench -p rbs-nodes --bench media`.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)] // Benchmark harness.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio::AudioGraphConfig;
use rbs_core::media::AudioFormat;
use rbs_core::time::{TestClock, Timestamp};
use rbs_media_decode::InterleavedPcm;
use rbs_media_decode_test::{ScriptClip, ScriptFactory};
use rbs_nodes::{MediaPair, media_pair};

const BLOCK: usize = 480;
const LATENCY: Duration = Duration::from_millis(50);

fn pair(clock: &Arc<TestClock>) -> MediaPair {
    let clip = ScriptClip {
        audio: Some(InterleavedPcm {
            sample_rate: 44_100,
            channels: 2,
            samples: vec![0.5; 13_230 * 2],
        }),
        ..ScriptClip::silent_frames()
    };
    let config = AudioGraphConfig::new(AudioFormat::STEREO_48K, BLOCK as u32, LATENCY).unwrap();
    media_pair(
        Arc::new(ScriptFactory::new(clip)),
        std::path::PathBuf::from("clip.mp4"),
        true,
        true,
        clock.clone(),
        config,
    )
}

fn settle(pair: &MediaPair, clock: &TestClock, t: Timestamp) {
    clock.set(t);
    while !(pair.picture.worker_ended() || pair.picture.worker_settled_at().is_some_and(|s| s >= t))
    {
        std::thread::yield_now();
    }
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("media");
    group.bench_function("picture_tick", |b| {
        b.iter_custom(|iters| {
            let clock = Arc::new(TestClock::new());
            let mut pair = pair(&clock);
            pair.picture.wake(Timestamp::ZERO).unwrap();
            let mut spent = Duration::ZERO;
            for i in 0..iters {
                let t = Timestamp::from_nanos(i * 16_666_667);
                settle(&pair, &clock, t);
                let start = Instant::now();
                black_box(pair.picture.dequeue(t));
                spent += start.elapsed();
            }
            pair.picture.standby();
            spent
        });
    });
    group.bench_function("sound_block", |b| {
        b.iter_custom(|iters| {
            let clock = Arc::new(TestClock::new());
            let mut pair = pair(&clock);
            pair.sound.attach().unwrap();
            let mut l = vec![0.0f32; BLOCK];
            let mut r = vec![0.0f32; BLOCK];
            pair.sound
                .fill(Timestamp::ZERO, &mut [&mut l, &mut r], BLOCK);
            pair.picture.wake(Timestamp::ZERO).unwrap();
            let mut spent = Duration::ZERO;
            for i in 0..iters {
                let t = Timestamp::from_nanos(i * 10_000_000) + LATENCY;
                settle(&pair, &clock, t);
                let start = Instant::now();
                black_box(pair.sound.fill(t - LATENCY, &mut [&mut l, &mut r], BLOCK));
                spent += start.elapsed();
            }
            pair.picture.standby();
            spent
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
