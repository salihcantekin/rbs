//! The per-packet work of a capture stream that does not call WASAPI: place a packet
//! in master time, push it into the sample ring, and fill silence while a loopback
//! stream gets no packets.
//!
//! Runs on the stream's capture thread for every WASAPI packet (a hot path,
//! `docs/architecture/hot-paths.md`): no allocation, locks, I/O or logging after
//! [`Packets::new`]. Kept free of Windows types so it is tested and benchmarked on
//! every platform.
//!
//! # Timestamps
//!
//! Each packet carries the QPC time at which the audio engine recorded the device
//! position of its first frame (`u64QPCPosition`); the port's clock edge maps it to
//! master time. Consecutive packets of a run are expected to follow each other: a
//! packet whose mapped time is more than [`MAX_STEP`] away from where the previous one
//! ended starts a new run, and a new run never starts before the previous packet
//! ended. Packets without a valid time (`AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR`) follow
//! the previous packet.
//!
//! # Silence while nothing plays
//!
//! WASAPI loopback and process loopback deliver no packets while nothing plays. For
//! those kinds the stream is created with a fill lag: when no audio has been stored
//! for that long, [`Packets::fill`] stores timed silence from where the last audio
//! ended up to `now - lag`, continuing the run. When packets come back, the first one
//! continues the run if it starts where the silence ended (within [`MAX_STEP`]) and
//! otherwise starts a new run after it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_audio_io::edge::EdgeClock;
use rbs_audio_io::ring::{I24, Push, RingProducer, Sample};
use rbs_core::time::Timestamp;
use rbs_core::timing::SampleRate;

/// Largest distance between a packet's time and the end of the previous one that
/// still continues the run. Half the conformance suite's default tolerance (2 ms).
pub(crate) const MAX_STEP: Duration = Duration::from_millis(1);

/// Longest silence filled at once. A capture thread that did not run for longer
/// (a suspended machine) starts a new run instead of pushing seconds of silence.
const MAX_FILL: Duration = Duration::from_millis(500);

/// Frames converted per step for devices with more channels than the stream
/// delivers.
const SCRATCH_FRAMES: usize = 1_024;

/// The samples of one OS packet, interleaved with the device's channel count.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Samples<'a> {
    F32(&'a [f32]),
    F64(&'a [f64]),
    U8(&'a [u8]),
    I16(&'a [i16]),
    I24(&'a [I24]),
    I32(&'a [i32]),
    /// The OS flagged the packet as silent (`AUDCLNT_BUFFERFLAGS_SILENT`).
    Silent,
}

/// One packet as WASAPI hands it over.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Packet<'a> {
    pub samples: Samples<'a>,
    /// Frames in the packet.
    pub frames: usize,
    /// QPC time of the first frame in nanoseconds, `None` when WASAPI flags it as
    /// invalid.
    pub device_time: Option<u64>,
    /// WASAPI reports a gap before this packet (`AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY`).
    pub discontinuity: bool,
}

/// Stamps and stores the packets of one Active stream.
pub(crate) struct Packets {
    producer: RingProducer,
    edge: EdgeClock,
    rate: SampleRate,
    /// Channels per frame in the OS buffers.
    source_channels: usize,
    /// Planar scratch for devices with more channels than the stream delivers.
    scratch: Box<[f32]>,
    /// Where the last stored audio ended (or, before any, the floor of a restarted
    /// stream).
    end: Timestamp,
    /// A run is open: the next packet may continue it.
    open: bool,
    /// The last audio stored was filler silence.
    filled: bool,
    /// Fill silence when no audio was stored for this long (loopback kinds).
    fill_lag: Option<Duration>,
    /// Largest silence packet: one WASAPI period.
    fill_frames: usize,
    /// Master time at which the stream started.
    started: Timestamp,
    /// Master time at which the last stored audio ended, in nanoseconds, kept by the
    /// stream so a restarted stream starts later.
    last_end: Arc<AtomicU64>,
}

impl Packets {
    /// Packets at `rate` with `source_channels` channels into `producer`, starting no
    /// earlier than `last_end`. `fill_lag` enables silence filling; `fill_frames` is
    /// the largest silence packet. Allocates the scratch buffer.
    pub(crate) fn new(
        producer: RingProducer,
        rate: SampleRate,
        source_channels: usize,
        fill: Option<(Duration, usize)>,
        started: Timestamp,
        last_end: Arc<AtomicU64>,
    ) -> Self {
        let channels = producer.channels();
        let scratch = if source_channels > channels {
            vec![0.0; channels * SCRATCH_FRAMES].into_boxed_slice()
        } else {
            Box::default()
        };
        let floor = Timestamp::from_nanos(last_end.load(Ordering::Relaxed));
        Self {
            producer,
            edge: EdgeClock::default(),
            rate,
            source_channels,
            scratch,
            end: floor,
            open: false,
            filled: false,
            fill_lag: fill.map(|(lag, _)| lag),
            fill_frames: fill.map_or(0, |(_, frames)| frames.max(1)),
            started,
            last_end,
        }
    }

    /// Records that `os_nanos` (QPC) and `now` (master) were read back to back.
    pub(crate) fn observe(&mut self, os_nanos: u64, now: Timestamp) {
        self.edge.observe(os_nanos, now);
    }

    /// Stores one packet handled at master time `now`.
    pub(crate) fn push(&mut self, packet: Packet<'_>, now: Timestamp) {
        let frames = packet.frames;
        if frames == 0 {
            return;
        }
        let duration = self.rate.duration_of(frames as u64);
        let latest = now - duration;
        let measured = match packet.device_time {
            Some(time) => self.edge.to_master(time, duration, now),
            None if self.open => self.end,
            None => latest,
        };
        let mut start = measured;
        let mut new_run = !self.open || packet.discontinuity;
        if self.open && distance(measured, self.end) > MAX_STEP {
            new_run = true;
        }
        if new_run && start < self.end {
            // A new run never overlaps the audio before it.
            start = self.end;
        }
        if start > latest + MAX_STEP {
            // Cannot follow the previous audio without ending in the future (the
            // device clock runs ahead, or silence was filled up to here): lost,
            // counted.
            self.producer.lost(frames as u64);
            self.open = false;
            return;
        }
        if new_run {
            self.producer.gap();
        }
        let written = self.store(packet.samples, frames, start);
        self.stored(start + duration, written);
        self.filled = false;
    }

    /// Fills silence up to `now - lag` when no audio was stored for `lag` (loopback
    /// kinds; does nothing for others).
    pub(crate) fn fill(&mut self, now: Timestamp) {
        let Some(lag) = self.fill_lag else {
            return;
        };
        let target = now - lag;
        let mut base = self.end.max(self.started);
        if target <= base {
            return;
        }
        if target.saturating_since(base) > MAX_FILL {
            // The thread did not run for a long time: start over close to now.
            base = target - MAX_FILL;
            self.open = false;
        }
        let nanos = u128::from(target.saturating_since(base).as_nanos() as u64);
        let mut frames = usize::try_from(nanos * u128::from(self.rate.hz()) / 1_000_000_000)
            .unwrap_or(usize::MAX);
        if base > self.end || !self.open {
            self.producer.gap();
            self.open = false;
        }
        while frames > 0 {
            let n = frames.min(self.fill_frames);
            let written = self.producer.push_silence(base, n) == Push::Written;
            base = base + self.rate.duration_of(n as u64);
            self.stored(base, written);
            if !written {
                break;
            }
            frames -= n;
        }
        self.filled = true;
    }

    /// The last stored audio was silence filled by [`fill`](Self::fill).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn filled(&self) -> bool {
        self.filled
    }

    /// A packet of `frames` frames that could not be read (an unexpected buffer):
    /// counted as lost, and the next packet starts a new run.
    pub(crate) fn lose(&mut self, frames: u64) {
        self.producer.lost(frames);
        self.open = false;
    }

    fn stored(&mut self, end: Timestamp, written: bool) {
        self.end = self.end.max(end);
        // A dropped packet makes the ring start a new run with the next one.
        self.open = written;
        self.last_end.store(self.end.as_nanos(), Ordering::Relaxed);
    }

    fn store(&mut self, samples: Samples<'_>, frames: usize, start: Timestamp) -> bool {
        match samples {
            Samples::Silent => self.producer.push_silence(start, frames) == Push::Written,
            Samples::F32(s) => self.store_as(s, frames, start),
            Samples::F64(s) => self.store_as(s, frames, start),
            Samples::U8(s) => self.store_as(s, frames, start),
            Samples::I16(s) => self.store_as(s, frames, start),
            Samples::I24(s) => self.store_as(s, frames, start),
            Samples::I32(s) => self.store_as(s, frames, start),
        }
    }

    fn store_as<S: Sample>(&mut self, interleaved: &[S], frames: usize, start: Timestamp) -> bool {
        let source = self.source_channels;
        let frames = frames.min(interleaved.len() / source.max(1));
        let channels = self.producer.channels();
        if source == channels {
            return self.producer.push(start, &interleaved[..frames * source]) == Push::Written;
        }
        // More channels than delivered: the first `channels`, in steps through the
        // planar scratch.
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(SCRATCH_FRAMES);
            for (c, plane) in self.scratch.chunks_exact_mut(SCRATCH_FRAMES).enumerate() {
                for (f, sample) in plane[..n].iter_mut().enumerate() {
                    *sample = interleaved[(done + f) * source + c].to_f32();
                }
            }
            let mut planes: [&[f32]; rbs_core::media::MAX_CHANNELS] =
                [&[]; rbs_core::media::MAX_CHANNELS];
            for (c, plane) in self.scratch.chunks_exact(SCRATCH_FRAMES).enumerate() {
                planes[c] = &plane[..n];
            }
            let at = start + self.rate.duration_of(done as u64);
            if self.producer.push_planar(at, &planes[..channels]) == Push::Dropped {
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

/// Benchmark access to the per-packet work (`benches/packet.rs`). Not part of the
/// adapter's API.
#[doc(hidden)]
pub mod bench {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use rbs_audio_io::ring::{self, RingConsumer};
    use rbs_core::media::AudioFormat;
    use rbs_core::time::Timestamp;

    use super::{Packet, Samples};

    /// One stream's packet path and the ring reader behind it.
    pub struct Path {
        packets: super::Packets,
        /// The reader of the ring.
        pub consumer: RingConsumer,
    }

    impl Path {
        /// A path for `format` from a device with `source_channels` channels, with a
        /// ring of `capacity` frames.
        pub fn new(format: AudioFormat, source_channels: usize, capacity: usize) -> Self {
            let (producer, consumer) = ring::channel(format, capacity, ring::DEFAULT_MARKS);
            Self {
                packets: super::Packets::new(
                    producer,
                    format.sample_rate,
                    source_channels,
                    None,
                    Timestamp::ZERO,
                    Arc::new(AtomicU64::new(0)),
                ),
                consumer,
            }
        }

        /// What the capture thread does with one `f32` packet.
        pub fn push_f32(&mut self, interleaved: &[f32], frames: usize, qpc: u64, now: Timestamp) {
            self.packets.observe(now.as_nanos(), now);
            self.packets.push(
                Packet {
                    samples: Samples::F32(interleaved),
                    frames,
                    device_time: Some(qpc),
                    discontinuity: false,
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
    use rbs_audio_io::ring::{self, RingConsumer};
    use rbs_core::media::{AudioFormat, ChannelLayout};

    const PERIOD_FRAMES: usize = 480;
    const PERIOD: u64 = 10_000_000;

    fn path(source_channels: usize, fill: bool) -> (Packets, RingConsumer, Arc<AtomicU64>) {
        let format = AudioFormat::STEREO_48K;
        let (producer, consumer) = ring::channel(format, 48_000, 1_024);
        let last_end = Arc::new(AtomicU64::new(0));
        let packets = Packets::new(
            producer,
            format.sample_rate,
            source_channels,
            fill.then_some((Duration::from_millis(30), PERIOD_FRAMES)),
            Timestamp::from_nanos(1_000_000_000),
            Arc::clone(&last_end),
        );
        (packets, consumer, last_end)
    }

    fn packet(samples: &[f32], qpc: u64) -> Packet<'_> {
        Packet {
            samples: Samples::F32(samples),
            frames: PERIOD_FRAMES,
            device_time: Some(qpc),
            discontinuity: false,
        }
    }

    fn drain(consumer: &mut RingConsumer) -> Vec<rbs_audio_io::AudioRead> {
        let mut left = vec![0.0_f32; 4_096];
        let mut right = vec![0.0_f32; 4_096];
        let mut reads = Vec::new();
        while let Ok(read) = consumer.read(&mut [&mut left, &mut right]) {
            if read.frames == 0 {
                break;
            }
            reads.push(read);
        }
        reads
    }

    /// QPC and master time are the same clock here, offset by 3 s.
    const OFFSET: u64 = 3_000_000_000;

    #[test]
    fn steady_packets_are_one_run_and_do_not_allocate() {
        let (mut packets, mut consumer, last_end) = path(2, false);
        let samples = vec![0.25_f32; PERIOD_FRAMES * 2];
        for k in 1..=50_u64 {
            let captured = OFFSET + k * PERIOD;
            let now = Timestamp::from_nanos(k * PERIOD + PERIOD + 200_000);
            let allocations = allocation_counter::measure(|| {
                packets.observe(now.as_nanos() + OFFSET, now);
                packets.push(packet(&samples, captured), now);
            });
            assert_eq!(allocations.count_total, 0, "push allocated");
        }
        let reads = drain(&mut consumer);
        assert!(reads[0].discontinuity);
        assert!(reads[1..].iter().all(|r| !r.discontinuity), "{reads:?}");
        assert_eq!(
            reads.iter().map(|r| r.frames).sum::<usize>(),
            50 * PERIOD_FRAMES
        );
        assert_eq!(reads[0].timestamp.as_nanos(), PERIOD);
        assert_eq!(last_end.load(Ordering::Relaxed), 51 * PERIOD);
    }

    #[test]
    fn a_jump_starts_a_new_run_that_never_overlaps() {
        let (mut packets, mut consumer, _) = path(2, false);
        let samples = vec![0.0_f32; PERIOD_FRAMES * 2];
        let now = |k: u64| Timestamp::from_nanos(k * PERIOD + PERIOD + 100_000);
        for k in 1..=3_u64 {
            packets.observe(now(k).as_nanos() + OFFSET, now(k));
            packets.push(packet(&samples, OFFSET + k * PERIOD), now(k));
        }
        // 2 ms late: a gap.
        packets.push(packet(&samples, OFFSET + 4 * PERIOD + 2_000_000), now(5));
        // 3 ms early: overlaps the previous packet, so it starts where that one ended.
        packets.push(packet(&samples, OFFSET + 5 * PERIOD - 1_000_000), now(6));
        let reads = drain(&mut consumer);
        let flags: Vec<bool> = reads.iter().map(|r| r.discontinuity).collect();
        assert_eq!(flags, [true, true, true]);
        assert_eq!(reads[1].timestamp.as_nanos(), 4 * PERIOD + 2_000_000);
        assert_eq!(reads[2].timestamp.as_nanos(), 5 * PERIOD + 2_000_000);
    }

    #[test]
    fn os_gaps_and_invalid_times_are_honoured() {
        let (mut packets, mut consumer, _) = path(2, false);
        let samples = vec![0.0_f32; PERIOD_FRAMES * 2];
        let now = Timestamp::from_nanos(10 * PERIOD);
        packets.observe(OFFSET + 10 * PERIOD, now);
        packets.push(packet(&samples, OFFSET + PERIOD), now);
        // No valid time: follows the previous packet.
        packets.push(
            Packet {
                device_time: None,
                ..packet(&samples, 0)
            },
            now,
        );
        // The OS reports a gap, although the time follows on.
        packets.push(
            Packet {
                discontinuity: true,
                samples: Samples::Silent,
                ..packet(&samples, OFFSET + 3 * PERIOD)
            },
            now,
        );
        let reads = drain(&mut consumer);
        assert_eq!(reads.len(), 2, "{reads:?}");
        assert_eq!(reads[0].frames, 2 * PERIOD_FRAMES);
        assert_eq!(reads[0].timestamp.as_nanos(), PERIOD);
        assert!(reads[1].discontinuity);
        assert_eq!(reads[1].timestamp.as_nanos(), 3 * PERIOD);
    }

    #[test]
    fn silence_fills_quiet_loopback_and_hands_over_to_audio() {
        let (mut packets, mut consumer, _) = path(2, true);
        let start = 1_000_000_000_u64;
        // Nothing plays for 100 ms: silence up to now - 30 ms.
        for ms in (10..=100_u64).step_by(10) {
            packets.fill(Timestamp::from_nanos(start + ms * 1_000_000));
        }
        assert!(packets.filled());
        let reads = drain(&mut consumer);
        assert_eq!(reads[0].timestamp.as_nanos(), start);
        assert!(reads[0].discontinuity);
        assert!(reads[1..].iter().all(|r| !r.discontinuity));
        let frames: usize = reads.iter().map(|r| r.frames).sum();
        assert_eq!(frames, 70 * 48);

        // Audio starts again: its first packet was captured 5 ms after the silence
        // ended, so it starts a new run there.
        let samples = vec![0.5_f32; PERIOD_FRAMES * 2];
        let now = Timestamp::from_nanos(start + 90_000_000);
        packets.observe(now.as_nanos() + OFFSET, now);
        let captured = start + 75_000_000;
        packets.push(packet(&samples, captured + OFFSET), now);
        assert!(!packets.filled());
        let reads = drain(&mut consumer);
        assert!(reads[0].discontinuity);
        assert_eq!(reads[0].timestamp.as_nanos(), captured);
        // While audio flows (the next packet is due), nothing is filled.
        packets.fill(Timestamp::from_nanos(start + 110_000_000));
        assert!(drain(&mut consumer).is_empty());
    }

    #[test]
    fn audio_continuing_the_silence_keeps_the_run() {
        let (mut packets, mut consumer, _) = path(2, true);
        let start = 1_000_000_000_u64;
        packets.fill(Timestamp::from_nanos(start + 50_000_000));
        let samples = vec![0.5_f32; PERIOD_FRAMES * 2];
        let now = Timestamp::from_nanos(start + 60_000_000);
        packets.observe(now.as_nanos() + OFFSET, now);
        // Captured 0.5 ms after the 20 ms of silence ended.
        packets.push(packet(&samples, start + 20_500_000 + OFFSET), now);
        let reads = drain(&mut consumer);
        assert_eq!(reads.iter().filter(|r| r.discontinuity).count(), 1);
    }

    #[test]
    fn a_long_stall_restarts_the_silence_near_now() {
        let (mut packets, mut consumer, _) = path(2, true);
        let now = Timestamp::from_nanos(5_000_000_000);
        packets.fill(now);
        let reads = drain(&mut consumer);
        let frames: usize = reads.iter().map(|r| r.frames).sum();
        assert_eq!(frames, 500 * 48);
        assert_eq!(
            reads[0].timestamp.as_nanos(),
            5_000_000_000 - 30_000_000 - 500_000_000
        );
    }

    #[test]
    fn wide_devices_deliver_their_first_channels() {
        let format = AudioFormat::new(
            rbs_core::timing::SampleRate::HZ_48000,
            ChannelLayout::Stereo,
        )
        .unwrap();
        let (producer, mut consumer) = ring::channel(format, 48_000, 64);
        let mut packets = Packets::new(
            producer,
            format.sample_rate,
            4,
            None,
            Timestamp::ZERO,
            Arc::new(AtomicU64::new(0)),
        );
        let frames = 1_500;
        let interleaved: Vec<i16> = (0..frames * 4).map(|i| (i % 4) as i16 * 1_000).collect();
        let allocations = allocation_counter::measure(|| {
            packets.push(
                Packet {
                    samples: Samples::I16(&interleaved),
                    frames,
                    device_time: None,
                    discontinuity: false,
                },
                Timestamp::from_nanos(1_000_000_000),
            );
        });
        assert_eq!(allocations.count_total, 0);
        let mut left = vec![0.0_f32; 2_048];
        let mut right = vec![0.0_f32; 2_048];
        let read = consumer.read(&mut [&mut left, &mut right]).unwrap();
        assert_eq!(read.frames, frames);
        assert_eq!(left[7], 0.0);
        assert_eq!(right[7], 1_000.0 / 32_768.0);
    }

    #[test]
    fn restarted_streams_start_after_the_previous_one() {
        let (mut packets, mut consumer, last_end) = path(2, false);
        let samples = vec![0.0_f32; PERIOD_FRAMES * 2];
        let now = Timestamp::from_nanos(2 * PERIOD);
        packets.push(packet(&samples, 0), now);
        let first = drain(&mut consumer);
        // A new Active stream whose device times map earlier.
        let format = AudioFormat::STEREO_48K;
        let (producer, mut consumer) = ring::channel(format, 48_000, 64);
        let mut packets = Packets::new(
            producer,
            format.sample_rate,
            2,
            None,
            Timestamp::ZERO,
            Arc::clone(&last_end),
        );
        packets.observe(OFFSET, Timestamp::ZERO);
        packets.push(packet(&samples, OFFSET), Timestamp::from_nanos(4 * PERIOD));
        let second = drain(&mut consumer);
        assert!(second[0].timestamp >= first[0].timestamp + Duration::from_millis(10));
    }
}
