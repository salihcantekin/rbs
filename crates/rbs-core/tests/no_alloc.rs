//! Allocation guard: hot-path functions in `rbs_core::timing` and `rbs_core::media`
//! must not allocate (docs/specs/performance-budgets.md, hard rules).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::{
    AudioBlock, AudioFormat, Bytes, ColorSpace, EncodedPacket, Lease, LeaseOwner, PixelFormat,
    PlaneLayout, Size, SystemFrame, TextureId, VideoFormat, VideoFrame,
};
use rbs_core::media::{BitstreamFormat, CodecId};
use rbs_core::time::{ClockMapping, Timestamp};
use rbs_core::timing::{Cadence, DriftEstimator, FrameRate, SampleRate, Timebase};

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

fn assert_no_alloc(name: &str, f: impl FnOnce()) {
    let info = allocation_counter::measure(f);
    assert_eq!(info.count_total, 0, "{name} allocated");
}

#[test]
fn timing_does_not_allocate() {
    let mut est = DriftEstimator::default();
    assert_no_alloc("timing", || {
        let cadence = Cadence::frames(Timestamp::from_nanos(5), FrameRate::FPS_59_94);
        let t = cadence.time_of(1_000_000);
        let _ = cadence.index_at_or_before(t);
        let _ = cadence.next_after(t);
        let _ = Cadence::blocks(Timestamp::ZERO, SampleRate::HZ_48000, 480);
        let _ = Timebase::MPEG_90K.ticks_since(Timestamp::ZERO, t);
        let _ = Timebase::MILLIS.duration_of(1_234);
        let _ = ClockMapping::anchored(0, t).to_master(99);
        for i in 0..500_u64 {
            est.observe(i * 10_000_000, Timestamp::from_nanos(i * 10_000_250));
        }
        let _ = est.mapping();
        let _ = SampleRate::HZ_48000.duration_of(480);
        let _ = FrameRate::FPS_60.frame_duration() + Duration::ZERO;
    });
}

#[test]
fn media_values_do_not_allocate() {
    let pool: Arc<dyn LeaseOwner> = Arc::new(Pool(AtomicU64::new(1)));
    let left = [0.0_f32; 480];
    let right = [0.0_f32; 480];
    let interleaved = [0.0_f32; 960];
    let format = VideoFormat::new(Size::new(64, 64), PixelFormat::Bgra8, ColorSpace::SRGB);
    // The first clone of a `Bytes` made from a `Vec` allocates its shared header once,
    // so producers share a buffer when they create it, off the fan-out path
    // (docs/specs/media-types.md#packets).
    let buffer = Bytes::from(vec![0_u8; 64 * 64 * 4]);
    let payload = Bytes::from(vec![0_u8; 1_000]);
    drop((buffer.clone(), payload.clone()));
    assert_no_alloc("media", || {
        pool.retain(0);
        // SAFETY: one reference was counted for token 0 just above.
        let lease = unsafe { Lease::adopt(Arc::clone(&pool), 0) };
        let frame = VideoFrame::texture(
            format,
            Timestamp::from_nanos(1),
            TextureId::new(0, 0),
            Some(lease),
        )
        .with_sequence(1);
        let queued = frame.share();
        drop(frame);
        drop(queued);

        let _ = AudioBlock::planar(AudioFormat::STEREO_48K, Timestamp::ZERO, &[&left, &right]);
        let _ = AudioBlock::interleaved(AudioFormat::STEREO_48K, Timestamp::ZERO, &interleaved);

        let cpu = SystemFrame::new(
            format,
            buffer.clone(),
            &[PlaneLayout {
                offset: 0,
                stride: 256,
            }],
        );
        drop(cpu);

        let packet = EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Timestamp::ZERO,
            Timestamp::ZERO,
            true,
            payload.clone(),
        );
        let fan_out = (packet.clone(), packet.clone(), packet.data.slice(4..));
        drop(fan_out);
    });
}
