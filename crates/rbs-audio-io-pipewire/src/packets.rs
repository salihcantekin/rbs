//! The per-cycle work of a capture stream that does not call PipeWire: place one
//! packet in master time and push it into the sample ring.
//!
//! Runs on PipeWire's real-time data thread for every graph cycle (a hot path,
//! `docs/architecture/hot-paths.md`): no allocation, locks, I/O or logging. Kept free
//! of PipeWire types so it is tested and benchmarked on every platform.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rbs_audio_io::edge::EdgeClock;
use rbs_audio_io::ring::{Push, RingProducer};
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;

use crate::pacing::Pacing;

/// Stamps and stores the packets of one Active stream.
pub(crate) struct Packets {
    producer: RingProducer,
    edge: EdgeClock,
    pacing: Pacing,
    rate: SampleRate,
    /// Master time of the last packet in nanoseconds, kept by the stream so a restart
    /// stamps later.
    last_stamp: Arc<AtomicU64>,
}

impl Packets {
    /// Packets at `rate` into `producer`, stamped later than `last_stamp`.
    pub(crate) fn new(
        producer: RingProducer,
        rate: SampleRate,
        last_stamp: Arc<AtomicU64>,
    ) -> Self {
        let floor = Timestamp::from_nanos(last_stamp.load(Ordering::Relaxed));
        Self {
            producer,
            edge: EdgeClock::default(),
            pacing: Pacing::new(rate).with_floor(floor),
            rate,
            last_stamp,
        }
    }

    /// Pushes one packet of planar samples (one equally long slice per channel) whose
    /// last frame was captured at `captured_end` on `CLOCK_MONOTONIC`. `os_now` and
    /// `now` are the monotonic and master clocks read back to back in this cycle.
    pub(crate) fn push(
        &mut self,
        planes: &[&[f32]],
        captured_end: u64,
        os_now: u64,
        now: Timestamp,
    ) {
        self.edge.observe(os_now, now);
        let frames = planes.first().map_or(0, |p| p.len());
        if frames == 0 {
            return;
        }
        let duration = self.rate.duration_of(frames as u64);
        let duration_nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let measured =
            self.edge
                .to_master(captured_end.saturating_sub(duration_nanos), duration, now);
        let stamp = self.pacing.stamp(measured, frames, now);
        if stamp.new_run {
            self.producer.gap();
        }
        if self.producer.push_planar(stamp.timestamp, planes) == Push::Dropped {
            // Counted by the ring, whose next stored packet starts a new run.
            self.pacing.break_run();
        }
        self.last_stamp
            .store(stamp.timestamp.as_nanos(), Ordering::Relaxed);
    }
}

/// Benchmark access to the per-cycle work (`benches/cycle.rs`). Not part of the
/// adapter's API.
#[doc(hidden)]
pub mod bench {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use rbs_audio_io::ring::{self, RingConsumer};
    use rbs_core::media::AudioFormat;
    use rbs_core::time::Timestamp;

    /// One stream's packet path and the ring reader behind it.
    pub struct Cycle {
        packets: super::Packets,
        /// The reader of the ring.
        pub consumer: RingConsumer,
    }

    impl Cycle {
        /// A path for `format` with a ring of `capacity` frames.
        pub fn new(format: AudioFormat, capacity: usize) -> Self {
            let (producer, consumer) = ring::channel(format, capacity, ring::DEFAULT_MARKS);
            Self {
                packets: super::Packets::new(
                    producer,
                    format.sample_rate,
                    Arc::new(AtomicU64::new(0)),
                ),
                consumer,
            }
        }

        /// What the process callback does with one buffer.
        pub fn push(&mut self, planes: &[&[f32]], captured_end: u64, os_now: u64, now: Timestamp) {
            self.packets.push(planes, captured_end, os_now, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bench::Cycle;
    use rbs_core::media::AudioFormat;
    use rbs_core::time::Timestamp;

    const QUANTUM: u64 = 1_024;

    /// Master time 5 s behind the monotonic clock.
    const OFFSET: u64 = 5_000_000_000;

    #[test]
    fn packets_become_one_run_and_never_allocate() {
        let mut cycle = Cycle::new(AudioFormat::STEREO_48K, 16_384);
        let left = vec![0.5_f32; QUANTUM as usize];
        let right = vec![-0.5_f32; QUANTUM as usize];
        let period = QUANTUM * 1_000_000_000 / 48_000;
        let mut out = [vec![0.0_f32; 4_096], vec![0.0_f32; 4_096]];
        let mut reads = Vec::new();
        for k in 1..=40_u64 {
            // The cycle wakes 0.2 ms late at most, as on a busy machine.
            let os_now = 10_000_000_000 + k * period + (k * 37 % 5) * 40_000;
            let captured_end = 10_000_000_000 + k * period;
            let now = Timestamp::from_nanos(os_now - OFFSET);
            let allocations = allocation_counter::measure(|| {
                cycle.push(&[&left, &right], captured_end, os_now, now);
            });
            assert_eq!(allocations.count_total, 0, "push allocated");
            let [l, r] = &mut out;
            if let Ok(read) = cycle.consumer.read(&mut [l, r])
                && read.frames > 0
            {
                assert_eq!(out[0][0], 0.5);
                assert_eq!(out[1][0], -0.5);
                reads.push(read);
            }
        }
        assert_eq!(reads.len(), 40);
        assert!(reads[0].discontinuity);
        for pair in reads.windows(2) {
            assert!(!pair[1].discontinuity, "a run broke: {pair:?}");
            let expected = pair[0].timestamp.as_nanos() + period;
            let error = pair[1].timestamp.as_nanos().abs_diff(expected);
            assert!(error < 100_000, "step error {error} ns");
        }
        // The edge maps monotonic to master time: packets end about when captured.
        let last = reads.last().map_or(0, |r| r.timestamp.as_nanos());
        let expected = 10_000_000_000 + 40 * period - period - OFFSET;
        assert!(last.abs_diff(expected) < 300_000, "{last} vs {expected}");
    }
}
