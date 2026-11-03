//! The per-cycle work of a capture stream that does not call CoreAudio: place one IO
//! cycle's input in master time and push it into the sample ring.
//!
//! Runs on the HAL's real-time IO thread for every IO cycle (a hot path,
//! `docs/architecture/hot-paths.md`): no allocation, locks, I/O or logging after
//! [`Packets::new`]. Kept free of CoreAudio types so it is tested and benchmarked on
//! every platform.
//!
//! # Timestamps and runs
//!
//! Each cycle carries the host time and the device sample time of its first input
//! frame (`inInputTime`). The host time is mapped to master time through the port's
//! clock edge. The sample time tells whether audio was lost: a cycle whose sample time
//! does not continue the previous one (an overload, a device hiccup) starts a new
//! run. A cycle whose mapped time is more than [`MAX_STEP`] away from where the previous
//! one ended starts a new run too, and a new run never starts before the previous
//! cycle ended.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio_io::edge::EdgeClock;
use rbs_audio_io::ring::{Push, RingProducer};
use rbs_core::media::MAX_CHANNELS;
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;

/// Largest distance between a cycle's time and the end of the previous one that still
/// continues the run. Half the conformance suite's default tolerance (2 ms).
pub(crate) const MAX_STEP: Duration = Duration::from_millis(1);

/// Frames converted per step when the input is not one interleaved buffer with
/// exactly the delivered channels.
const SCRATCH_FRAMES: usize = 1_024;

/// One input buffer of a cycle: interleaved `f32` with `channels` channels.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Buffer<'a> {
    pub samples: &'a [f32],
    pub channels: usize,
}

/// One IO cycle's input.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cycle<'a> {
    /// The buffers holding the delivered channels, in order (at most `MAX_CHANNELS`).
    pub buffers: &'a [Buffer<'a>],
    /// Frames in the cycle.
    pub frames: usize,
    /// Host time of the first frame in nanoseconds, if valid.
    pub host_nanos: Option<u64>,
    /// Device sample time of the first frame, if valid.
    pub sample_time: Option<f64>,
}

/// Stamps and stores the cycles of one Active stream.
pub(crate) struct Packets {
    producer: RingProducer,
    edge: EdgeClock,
    rate: SampleRate,
    /// Planar scratch for input that is not one matching interleaved buffer.
    scratch: Box<[f32]>,
    /// Where the last stored audio ended (or, before any, the floor of a restarted
    /// stream).
    end: Timestamp,
    /// A run is open: the next cycle may continue it.
    open: bool,
    /// The sample time the next cycle continues from.
    next_sample: Option<f64>,
    /// Master time at which the last stored audio ended, in nanoseconds, kept by the
    /// stream so a restarted stream starts later.
    last_end: Arc<AtomicU64>,
}

impl Packets {
    /// Cycles at `rate` into `producer`, starting no earlier than `last_end`.
    /// Allocates the scratch buffer.
    pub(crate) fn new(producer: RingProducer, rate: SampleRate, last_end: Arc<AtomicU64>) -> Self {
        let channels = producer.channels();
        let floor = Timestamp::from_nanos(last_end.load(Ordering::Relaxed));
        Self {
            producer,
            edge: EdgeClock::default(),
            rate,
            scratch: vec![0.0; channels * SCRATCH_FRAMES].into_boxed_slice(),
            end: floor,
            open: false,
            next_sample: None,
            last_end,
        }
    }

    /// Records that `os_nanos` (host time) and `now` (master) were read back to back.
    pub(crate) fn observe(&mut self, os_nanos: u64, now: Timestamp) {
        self.edge.observe(os_nanos, now);
    }

    /// Stores one cycle handled at master time `now`.
    pub(crate) fn push(&mut self, cycle: Cycle<'_>, now: Timestamp) {
        let frames = cycle.frames;
        if frames == 0 {
            return;
        }
        let duration = self.rate.duration_of(frames as u64);
        let latest = now - duration;
        let measured = match cycle.host_nanos {
            Some(time) => self.edge.to_master(time, duration, now),
            None if self.open => self.end,
            None => latest,
        };
        let mut new_run = !self.open;
        if let (Some(time), Some(expected)) = (cycle.sample_time, self.next_sample)
            && (time - expected).abs() >= 0.5
        {
            // The device skipped or repeated frames.
            new_run = true;
        }
        self.next_sample = cycle.sample_time.map(|t| t + frames as f64);
        if self.open && distance(measured, self.end) > MAX_STEP {
            new_run = true;
        }
        let mut start = measured;
        if new_run && start < self.end {
            start = self.end;
        }
        if start > latest + MAX_STEP {
            // Cannot follow the previous audio without ending in the future: lost,
            // counted.
            self.producer.lost(frames as u64);
            self.open = false;
            return;
        }
        if new_run {
            self.producer.gap();
        }
        let written = self.store(cycle.buffers, frames, start);
        self.end = self.end.max(start + duration);
        self.open = written;
        self.last_end.store(self.end.as_nanos(), Ordering::Relaxed);
    }

    fn store(&mut self, buffers: &[Buffer<'_>], frames: usize, start: Timestamp) -> bool {
        let channels = self.producer.channels();
        if let [only] = buffers
            && only.channels == channels
        {
            let n = frames.min(only.samples.len() / channels.max(1));
            return self.producer.push(start, &only.samples[..n * channels]) == Push::Written;
        }
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(SCRATCH_FRAMES);
            let mut planes = self.scratch.chunks_exact_mut(SCRATCH_FRAMES);
            'fill: for buffer in buffers {
                for c in 0..buffer.channels {
                    let Some(plane) = planes.next() else {
                        break 'fill;
                    };
                    for (f, sample) in plane[..n].iter_mut().enumerate() {
                        *sample = buffer
                            .samples
                            .get((done + f) * buffer.channels + c)
                            .copied()
                            .unwrap_or(0.0);
                    }
                }
            }
            // Channels no buffer holds stay silent.
            for plane in planes {
                plane[..n].fill(0.0);
            }
            let mut slices: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
            for (slice, plane) in slices
                .iter_mut()
                .zip(self.scratch.chunks_exact(SCRATCH_FRAMES))
            {
                *slice = &plane[..n];
            }
            let at = start + self.rate.duration_of(done as u64);
            if self.producer.push_planar(at, &slices[..channels]) == Push::Dropped {
                return false;
            }
            done += n;
        }
        true
    }
}

fn distance(a: Timestamp, b: Timestamp) -> Duration {
    Duration::from_nanos(a.as_nanos().abs_diff(b.as_nanos()))
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

    use super::{Buffer, Cycle};

    /// One stream's cycle path and the ring reader behind it.
    pub struct Path {
        packets: super::Packets,
        /// The reader of the ring.
        pub consumer: RingConsumer,
    }

    impl Path {
        /// A path for `format` with a ring of `capacity` frames.
        pub fn new(format: AudioFormat, capacity: usize) -> Self {
            let (producer, consumer) = ring::channel(format, capacity, 1_024);
            Self {
                packets: super::Packets::new(
                    producer,
                    format.sample_rate,
                    Arc::new(AtomicU64::new(0)),
                ),
                consumer,
            }
        }

        /// What the IO proc does with one cycle of buffers, each `(samples, channels)`.
        pub fn push(
            &mut self,
            buffers: &[(&[f32], usize)],
            frames: usize,
            host_nanos: u64,
            sample_time: f64,
            now: Timestamp,
        ) {
            let mut list = [Buffer::default(); 8];
            for (slot, (samples, channels)) in list.iter_mut().zip(buffers) {
                *slot = Buffer {
                    samples,
                    channels: *channels,
                };
            }
            self.packets.observe(now.as_nanos(), now);
            self.packets.push(
                Cycle {
                    buffers: &list[..buffers.len().min(8)],
                    frames,
                    host_nanos: Some(host_nanos),
                    sample_time: Some(sample_time),
                },
                now,
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_audio_io::AudioRead;
    use rbs_audio_io::ring::{self, RingConsumer};
    use rbs_core::media::{AudioFormat, ChannelLayout};

    const FRAMES: usize = 512;
    /// 512 frames at 48 kHz.
    const PERIOD: u64 = 10_666_666;
    /// Host time is master time plus 2 s.
    const OFFSET: u64 = 2_000_000_000;

    fn path(format: AudioFormat) -> (Packets, RingConsumer, Arc<AtomicU64>) {
        let (producer, consumer) = ring::channel(format, 48_000, 1_024);
        let last_end = Arc::new(AtomicU64::new(0));
        let packets = Packets::new(producer, format.sample_rate, Arc::clone(&last_end));
        (packets, consumer, last_end)
    }

    fn drain(consumer: &mut RingConsumer) -> Vec<AudioRead> {
        let channels = consumer.channels();
        let mut planes: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0; 8_192]).collect();
        let mut reads = Vec::new();
        loop {
            let mut out: Vec<&mut [f32]> = planes.iter_mut().map(|p| p.as_mut_slice()).collect();
            match consumer.read(&mut out) {
                Ok(read) if read.frames > 0 => reads.push(read),
                _ => break,
            }
        }
        reads
    }

    fn cycle<'a>(buffers: &'a [Buffer<'a>], k: u64, sample: f64) -> Cycle<'a> {
        Cycle {
            buffers,
            frames: FRAMES,
            host_nanos: Some(OFFSET + k * PERIOD),
            sample_time: Some(sample),
        }
    }

    #[test]
    fn steady_cycles_are_one_run_and_do_not_allocate() {
        let (mut packets, mut consumer, _) = path(AudioFormat::STEREO_48K);
        let samples = vec![0.5_f32; FRAMES * 2];
        let buffers = [Buffer {
            samples: &samples,
            channels: 2,
        }];
        for k in 1..=40_u64 {
            let now = Timestamp::from_nanos((k + 1) * PERIOD + 300_000);
            let allocations = allocation_counter::measure(|| {
                packets.observe(OFFSET + now.as_nanos(), now);
                packets.push(cycle(&buffers, k, (k * FRAMES as u64) as f64), now);
            });
            assert_eq!(allocations.count_total, 0, "push allocated");
        }
        let reads = drain(&mut consumer);
        assert!(reads[0].discontinuity);
        assert!(reads[1..].iter().all(|r| !r.discontinuity), "{reads:?}");
        assert_eq!(reads.iter().map(|r| r.frames).sum::<usize>(), 40 * FRAMES);
    }

    #[test]
    fn a_sample_time_jump_starts_a_new_run() {
        let (mut packets, mut consumer, _) = path(AudioFormat::STEREO_48K);
        let samples = vec![0.0_f32; FRAMES * 2];
        let buffers = [Buffer {
            samples: &samples,
            channels: 2,
        }];
        let now = Timestamp::from_nanos(10 * PERIOD);
        packets.observe(OFFSET + now.as_nanos(), now);
        packets.push(cycle(&buffers, 1, 512.0), now);
        packets.push(cycle(&buffers, 2, 1_024.0), now);
        // One cycle's frames were lost, although the host time follows on.
        packets.push(cycle(&buffers, 3, 2_048.0), now);
        let reads = drain(&mut consumer);
        let flags: Vec<bool> = reads.iter().map(|r| r.discontinuity).collect();
        assert_eq!(flags, [true, true]);
        assert_eq!(reads[0].frames, 2 * FRAMES);
    }

    #[test]
    fn a_time_jump_starts_a_new_run_that_never_overlaps() {
        let (mut packets, mut consumer, last_end) = path(AudioFormat::STEREO_48K);
        let samples = vec![0.0_f32; FRAMES * 2];
        let buffers = [Buffer {
            samples: &samples,
            channels: 2,
        }];
        let now = Timestamp::from_nanos(10 * PERIOD);
        packets.observe(OFFSET + now.as_nanos(), now);
        packets.push(cycle(&buffers, 1, 512.0), now);
        // 3 ms early, with no sample time to say why: placed after the previous cycle.
        packets.push(
            Cycle {
                host_nanos: Some(OFFSET + 2 * PERIOD - 3_000_000),
                sample_time: None,
                ..cycle(&buffers, 2, 0.0)
            },
            now,
        );
        let reads = drain(&mut consumer);
        assert_eq!(reads.len(), 2);
        assert!(reads[1].discontinuity);
        assert_eq!(reads[1].timestamp.as_nanos(), 2 * PERIOD);
        assert_eq!(last_end.load(Ordering::Relaxed), 3 * PERIOD);
    }

    #[test]
    fn split_and_wide_input_delivers_the_first_channels() {
        // A device with a stereo buffer and a 4-channel buffer, delivered as 4
        // discrete channels: both of the first buffer, two of the second.
        let format = AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Discrete(4)).unwrap();
        let (mut packets, mut consumer, _) = path(format);
        let frames = 1_500;
        let first: Vec<f32> = (0..frames * 2).map(|i| (i % 2) as f32 * 0.1).collect();
        let second: Vec<f32> = (0..frames * 4)
            .map(|i| 0.2 + (i % 4) as f32 * 0.1)
            .collect();
        let buffers = [
            Buffer {
                samples: &first,
                channels: 2,
            },
            Buffer {
                samples: &second,
                channels: 4,
            },
        ];
        let allocations = allocation_counter::measure(|| {
            packets.push(
                Cycle {
                    buffers: &buffers,
                    frames,
                    host_nanos: None,
                    sample_time: None,
                },
                Timestamp::from_nanos(1_000_000_000),
            );
        });
        assert_eq!(allocations.count_total, 0);
        let mut planes = vec![vec![0.0_f32; 2_048]; 4];
        let mut out: Vec<&mut [f32]> = planes.iter_mut().map(|p| p.as_mut_slice()).collect();
        let read = consumer.read(&mut out).unwrap();
        assert_eq!(read.frames, frames);
        let firsts: Vec<f32> = planes.iter().map(|p| p[9]).collect();
        assert_eq!(firsts, [0.0, 0.1, 0.2, 0.3]);
    }

    #[test]
    fn restarted_streams_start_after_the_previous_one() {
        let (mut packets, mut consumer, last_end) = path(AudioFormat::STEREO_48K);
        let samples = vec![0.0_f32; FRAMES * 2];
        let buffers = [Buffer {
            samples: &samples,
            channels: 2,
        }];
        packets.push(cycle(&buffers, 1, 0.0), Timestamp::from_nanos(5 * PERIOD));
        let first = drain(&mut consumer);
        let (producer, mut consumer) = ring::channel(AudioFormat::STEREO_48K, 48_000, 64);
        let mut packets = Packets::new(producer, SampleRate::HZ_48000, last_end);
        packets.observe(OFFSET, Timestamp::ZERO);
        packets.push(cycle(&buffers, 0, 0.0), Timestamp::from_nanos(10 * PERIOD));
        let second = drain(&mut consumer);
        assert!(second[0].timestamp >= first[0].timestamp + Duration::from_millis(10));
    }
}
