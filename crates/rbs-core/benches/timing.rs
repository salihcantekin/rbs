//! Micro-benchmarks for hot-path timing and media value code.
//! Run with `cargo bench -p rbs-core --bench timing`.

#![allow(missing_docs)]

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use rbs_core::media::{
    AudioBlock, AudioFormat, ColorSpace, Lease, LeaseOwner, PixelFormat, Size, TextureId,
    VideoFormat, VideoFrame,
};
use rbs_core::time::{ClockMapping, Timestamp};
use rbs_core::timing::{Cadence, DriftEstimator, FrameRate, Timebase};

struct Pool(AtomicU64);

// SAFETY: the pool owns no memory; counts are exact atomics.
unsafe impl LeaseOwner for Pool {
    fn retain(&self, _token: u64) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn release(&self, _token: u64) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn timing(c: &mut Criterion) {
    let cadence = Cadence::frames(Timestamp::from_nanos(123_456), FrameRate::FPS_59_94);
    c.bench_function("cadence_time_of", |b| {
        let mut i = 1_000_000_u64;
        b.iter(|| {
            i += 1;
            black_box(cadence.time_of(black_box(i)))
        })
    });
    c.bench_function("cadence_index_at_or_before", |b| {
        let mut t = 10_000_000_000_u64;
        b.iter(|| {
            t += 16_683_333;
            black_box(cadence.index_at_or_before(black_box(Timestamp::from_nanos(t))))
        })
    });
    let origin = Timestamp::from_nanos(1_000);
    c.bench_function("timebase_ticks_since_90k", |b| {
        let mut t = 5_000_000_000_u64;
        b.iter(|| {
            t += 16_683_333;
            black_box(Timebase::MPEG_90K.ticks_since(origin, black_box(Timestamp::from_nanos(t))))
        })
    });
    let mapping = ClockMapping::anchored(1_000, Timestamp::from_nanos(50_000)).with_rate(1.000_25);
    c.bench_function("clock_mapping_to_master", |b| {
        let mut s = 1_000_u64;
        b.iter(|| {
            s += 10_000_000;
            black_box(mapping.to_master(black_box(s)))
        })
    });
    c.bench_function("drift_observe", |b| {
        let mut est = DriftEstimator::default();
        let mut s = 0_u64;
        b.iter(|| {
            s += 10_000_000;
            let _ = est.observe(black_box(s), Timestamp::from_nanos(s + 77));
        })
    });
    c.bench_function("drift_mapping_full_window", |b| {
        let mut est = DriftEstimator::new(Duration::from_millis(10));
        for i in 0..200_u64 {
            let _ = est.observe(i * 10_000_000, Timestamp::from_nanos(i * 10_000_100));
        }
        b.iter(|| black_box(est.mapping()))
    });
}

fn media(c: &mut Criterion) {
    let pool = Arc::new(Pool(AtomicU64::new(1)));
    // SAFETY: the pool starts with one counted reference for token 1.
    let lease = unsafe { Lease::adopt(pool.clone(), 1) };
    let frame = VideoFrame::texture(
        VideoFormat::new(
            Size::new(1920, 1080),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        Timestamp::from_nanos(1),
        TextureId::new(1, 1),
        Some(lease),
    );
    c.bench_function("video_frame_clone_drop", |b| {
        b.iter(|| drop(black_box(frame.share())))
    });
    let left = [0.0_f32; 480];
    let right = [0.0_f32; 480];
    c.bench_function("audio_block_planar", |b| {
        b.iter(|| {
            black_box(AudioBlock::planar(
                AudioFormat::STEREO_48K,
                Timestamp::ZERO,
                &[black_box(&left[..]), black_box(&right[..])],
            ))
        })
    });
}

criterion_group!(benches, timing, media);
criterion_main!(benches);
