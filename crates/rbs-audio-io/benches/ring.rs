//! Cost of the real-time sample hand-off: one 10 ms stereo packet pushed by the OS
//! callback and read by the audio thread.
//!
//! `cargo bench -p rbs-audio-io --bench ring`

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_audio_io::ring::{self, Push};
use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;

fn hand_off(c: &mut Criterion) {
    let (mut producer, mut consumer) = ring::channel(AudioFormat::STEREO_48K, 9_600, 256);
    let f32_packet = vec![0.25_f32; 960];
    let i16_packet = vec![1_000_i16; 960];
    let mut left = vec![0.0_f32; 480];
    let mut right = vec![0.0_f32; 480];
    let mut t = 0_u64;

    c.bench_function("ring push f32 + read 480 stereo frames", |b| {
        b.iter(|| {
            t += 10_000_000;
            let push = producer.push(Timestamp::from_nanos(t), black_box(&f32_packet));
            assert_eq!(push, Push::Written);
            let read = consumer.read(&mut [&mut left, &mut right]);
            black_box(read.map(|r| r.frames).unwrap_or(0))
        })
    });
    c.bench_function("ring push i16 + read 480 stereo frames", |b| {
        b.iter(|| {
            t += 10_000_000;
            let push = producer.push(Timestamp::from_nanos(t), black_box(&i16_packet));
            assert_eq!(push, Push::Written);
            let read = consumer.read(&mut [&mut left, &mut right]);
            black_box(read.map(|r| r.frames).unwrap_or(0))
        })
    });
    c.bench_function("ring read with nothing available", |b| {
        b.iter(|| {
            let read = consumer.read(&mut [&mut left, &mut right]);
            black_box(read.map(|r| r.frames).unwrap_or(0))
        })
    });
}

criterion_group!(benches, hand_off);
criterion_main!(benches);
