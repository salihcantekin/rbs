//! Browser sound node block path (`BrowserSoundNode::fill`, hot path) and the feed's
//! work per block on the supervisor thread (`BrowserSoundFeed::pump`), 48 kHz stereo.
//!
//! `cargo bench -p rbs-sources --bench browser_sound`

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio::AudioGraphConfig;
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_sources::{BrowserSound, BrowserSoundFeed, BrowserSoundNode};

const OS: u64 = 1_000_000_000_000;

/// One 10 ms block of the page: master time `t`, then the feed renders what is due.
fn feed_block(clock: &TestClock, feed: &mut BrowserSoundFeed, block: u64, samples: &[f32]) {
    clock.advance(Duration::from_millis(10));
    let now = clock.now().as_nanos();
    feed.observe(OS + now);
    feed.packet(OS + block * 10_000_000, 48_000, 2, samples);
    feed.pump();
}

fn bench(c: &mut Criterion) {
    let samples: Vec<f32> = (0..960).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
    let mut group = c.benchmark_group("browser_sound_block");

    // A rig: the page and the feed advance one block, the audio thread takes one.
    struct Rig {
        clock: TestClock,
        node: BrowserSoundNode,
        feed: BrowserSoundFeed,
        block: u64,
        played: u64,
        l: Vec<f32>,
        r: Vec<f32>,
    }
    let rig = || {
        let clock = TestClock::new();
        let shared = BrowserSound::new(AudioGraphConfig::default(), Arc::new(clock.clone()));
        let mut node = BrowserSoundNode::new(Arc::clone(&shared));
        node.attach();
        let (mut l, mut r) = (vec![0.0f32; 480], vec![0.0f32; 480]);
        node.fill(Timestamp::ZERO, &mut [&mut l, &mut r], 480);
        Rig {
            clock,
            node,
            feed: BrowserSoundFeed::new(shared),
            block: 0,
            played: 0,
            l,
            r,
        }
    };
    let take = |rig: &mut Rig| {
        let t = Timestamp::from_nanos(rig.played * 10_000_000);
        let wrote = rig.node.fill(t, &mut [&mut rig.l, &mut rig.r], 480);
        rig.played += 1;
        wrote
    };
    // The node alone (audio thread): only `fill` is timed.
    group.bench_function("node_fill_48k_stereo", |b| {
        let mut rig = rig();
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            for _ in 0..iters {
                feed_block(&rig.clock, &mut rig.feed, rig.block, &samples);
                rig.block += 1;
                while rig.played * 10_000_000 + 50_000_000 <= rig.clock.now().as_nanos() {
                    let start = std::time::Instant::now();
                    std::hint::black_box(take(&mut rig));
                    spent += start.elapsed();
                }
            }
            spent
        });
    });
    // The feed (supervisor thread): packet, aligner and push for one block.
    group.bench_function("feed_block_48k_stereo", |b| {
        let mut rig = rig();
        b.iter_custom(|iters| {
            let mut spent = Duration::ZERO;
            for _ in 0..iters {
                let start = std::time::Instant::now();
                feed_block(&rig.clock, &mut rig.feed, rig.block, &samples);
                spent += start.elapsed();
                rig.block += 1;
                while rig.played * 10_000_000 + 50_000_000 <= rig.clock.now().as_nanos() {
                    take(&mut rig);
                }
            }
            spent
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
