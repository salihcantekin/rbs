//! The lock-free sample ring between an adapter's OS callback and the audio thread.
//!
//! Adapter-support module: versioned with the port crate but outside the port
//! contract (it may change in a minor version). Contract: "Sample ring" in
//! `docs/specs/audio-io-port.md`.
//!
//! One producer (the OS callback or the adapter's thread) pushes packets of
//! interleaved or planar device samples, each with the master time of its first
//! frame. One consumer (the stream's `read` on the audio thread) moves them out as
//! planar `f32`. Samples are stored as `f32` bits in atomics, so the ring needs no
//! `unsafe`; on every supported CPU a relaxed atomic load or store of 32 bits is a
//! plain load or store.
//!
//! Each packet leaves a *mark* (frame position, timestamp, whether it starts a new
//! run) in a second fixed ring. A read extrapolates the timestamp of its first frame
//! from the newest mark at or before it, crosses marks of the same run, and stops
//! before a mark that starts a new run. When the mark ring is full, a packet that
//! continues the run is stored without a mark (its time is extrapolated) and a packet
//! that starts a new run is dropped.
//!
//! When the sample ring is full, the incoming packet is dropped whole and counted,
//! and the next stored packet starts a new run: the producer never waits and never
//! overwrites audio the consumer has not read.
//!
//! Every method except [`channel`] is lock-free and allocation-free.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::AudioFormat;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, SampleRate};

use crate::error::AudioIoError;
use crate::stream::{AudioRead, InputStats};

/// Marks a ring keeps by default: one per packet for 256 packets, enough for the
/// longest buffer at 10 ms packets and for 50 ms of buffer at 0.2 ms packets.
pub const DEFAULT_MARKS: usize = 256;

/// A device sample type the ring converts to `f32` when it is pushed.
pub trait Sample: Copy {
    /// The sample as `f32`, full scale at `-1.0..=1.0`.
    fn to_f32(self) -> f32;
}

impl Sample for f32 {
    fn to_f32(self) -> f32 {
        self
    }
}

impl Sample for f64 {
    fn to_f32(self) -> f32 {
        self as f32
    }
}

impl Sample for i16 {
    fn to_f32(self) -> f32 {
        f32::from(self) / 32_768.0
    }
}

impl Sample for i32 {
    fn to_f32(self) -> f32 {
        (f64::from(self) / 2_147_483_648.0) as f32
    }
}

impl Sample for u8 {
    fn to_f32(self) -> f32 {
        (f32::from(self) - 128.0) / 128.0
    }
}

/// A packed little-endian signed 24-bit sample, as many USB devices deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct I24(pub [u8; 3]);

impl Sample for I24 {
    fn to_f32(self) -> f32 {
        let [a, b, c] = self.0;
        // Place the 24 bits in the top of an i32 so the sign is kept.
        let value = i32::from_le_bytes([0, a, b, c]) >> 8;
        value as f32 / 8_388_608.0
    }
}

/// Sample frames that last `duration` at `rate`, rounded up.
pub fn frames_for(rate: SampleRate, duration: Duration) -> usize {
    let frames = (u128::from(rate.hz()) * duration.as_nanos()).div_ceil(1_000_000_000);
    usize::try_from(frames).unwrap_or(usize::MAX)
}

/// Ring capacity for a stream: `buffer` of audio, but at least two of the largest
/// packets the OS may deliver (`max_packet_frames`, for example a PipeWire quantum
/// of 4096 frames), so every packet fits once the consumer has read the previous one.
pub fn capacity_frames(rate: SampleRate, buffer: Duration, max_packet_frames: usize) -> usize {
    frames_for(rate, buffer).max(max_packet_frames.saturating_mul(2))
}

/// Whether a pushed packet was stored.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// Stored; the consumer will read it.
    Written,
    /// Dropped and counted, because the ring was full. The next stored packet starts
    /// a new run.
    Dropped,
}

struct Mark {
    position: AtomicU64,
    timestamp: AtomicU64,
    discontinuity: AtomicBool,
}

struct Shared {
    channels: usize,
    /// Capacity in sample frames.
    capacity: u64,
    /// `capacity * channels` interleaved `f32` bits.
    samples: Box<[AtomicU32]>,
    /// Frames written in total; only the producer stores it.
    write: AtomicU64,
    /// Frames read in total; only the consumer stores it.
    read: AtomicU64,
    marks: Box<[Mark]>,
    mark_write: AtomicU64,
    mark_read: AtomicU64,
    captured: AtomicU64,
    dropped: AtomicU64,
    discontinuities: AtomicU64,
}

/// Creates a ring for `format` holding `capacity_frames` sample frames (at least
/// one) and `marks` packet marks (at least one).
///
/// Allocates; call it from `start`, never from the audio thread.
pub fn channel(
    format: AudioFormat,
    capacity_frames: usize,
    marks: usize,
) -> (RingProducer, RingConsumer) {
    let channels = format.channels.channels().max(1);
    let capacity = capacity_frames.max(1);
    let shared = Arc::new(Shared {
        channels,
        capacity: capacity as u64,
        samples: (0..capacity * channels)
            .map(|_| AtomicU32::new(0))
            .collect(),
        write: AtomicU64::new(0),
        read: AtomicU64::new(0),
        marks: (0..marks.max(1))
            .map(|_| Mark {
                position: AtomicU64::new(0),
                timestamp: AtomicU64::new(0),
                discontinuity: AtomicBool::new(false),
            })
            .collect(),
        mark_write: AtomicU64::new(0),
        mark_read: AtomicU64::new(0),
        captured: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        discontinuities: AtomicU64::new(0),
    });
    (
        RingProducer {
            shared: Arc::clone(&shared),
            gap: true,
        },
        RingConsumer {
            shared,
            rate: format.sample_rate,
            base: None,
            pending: false,
            last_start: None,
            delivered: 0,
        },
    )
}

/// The producer half: owned by the OS callback or the adapter's thread.
pub struct RingProducer {
    shared: Arc<Shared>,
    /// The next stored packet starts a new run.
    gap: bool,
}

impl RingProducer {
    /// Channels per frame.
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Sample frames that fit now.
    pub fn free(&self) -> usize {
        let s = &*self.shared;
        let used = s.write.load(Ordering::Relaxed) - s.read.load(Ordering::Acquire);
        (s.capacity - used) as usize
    }

    /// Pushes interleaved samples whose first frame was captured at `timestamp`
    /// (master time). A trailing partial frame is ignored.
    pub fn push<S: Sample>(&mut self, timestamp: Timestamp, interleaved: &[S]) -> Push {
        let channels = self.shared.channels;
        let frames = interleaved.len() / channels;
        self.push_with(timestamp, frames, |frame, channel| {
            interleaved[frame * channels + channel].to_f32()
        })
    }

    /// Pushes planar samples, one slice per channel, whose first frame was captured at
    /// `timestamp`. The packet is as long as the shortest slice; missing channels are
    /// silent and extra slices are ignored.
    pub fn push_planar<S: Sample>(&mut self, timestamp: Timestamp, channels: &[&[S]]) -> Push {
        let count = channels.len().min(self.shared.channels);
        let frames = channels[..count].iter().map(|c| c.len()).min().unwrap_or(0);
        self.push_with(timestamp, frames, |frame, channel| {
            if channel < count {
                channels[channel][frame].to_f32()
            } else {
                0.0
            }
        })
    }

    /// Pushes `frames` frames of silence captured at `timestamp`, for packets the OS
    /// flags as silent.
    pub fn push_silence(&mut self, timestamp: Timestamp, frames: usize) -> Push {
        self.push_with(timestamp, frames, |_, _| 0.0)
    }

    /// Counts `frames` frames the adapter received (or knows the OS captured) but
    /// will not push, and starts a new run.
    pub fn lost(&mut self, frames: u64) {
        let s = &*self.shared;
        s.captured.fetch_add(frames, Ordering::Relaxed);
        s.dropped.fetch_add(frames, Ordering::Relaxed);
        self.gap = true;
    }

    /// Makes the next stored packet start a new run, for gaps the OS reports
    /// (a WASAPI data discontinuity, a PipeWire xrun).
    pub fn gap(&mut self) {
        self.gap = true;
    }

    fn push_with(
        &mut self,
        timestamp: Timestamp,
        frames: usize,
        mut sample: impl FnMut(usize, usize) -> f32,
    ) -> Push {
        let s = &*self.shared;
        let frames64 = frames as u64;
        s.captured.fetch_add(frames64, Ordering::Relaxed);
        if frames == 0 {
            return Push::Written;
        }
        let write = s.write.load(Ordering::Relaxed);
        let read = s.read.load(Ordering::Acquire);
        if frames64 > s.capacity - (write - read) {
            s.dropped.fetch_add(frames64, Ordering::Relaxed);
            self.gap = true;
            return Push::Dropped;
        }
        let mark_write = s.mark_write.load(Ordering::Relaxed);
        let mark_read = s.mark_read.load(Ordering::Acquire);
        let mark_free = mark_write - mark_read < s.marks.len() as u64;
        if !mark_free && self.gap {
            s.dropped.fetch_add(frames64, Ordering::Relaxed);
            return Push::Dropped;
        }
        if mark_free {
            // The mark is published before the samples, so a consumer that sees the
            // samples also sees the mark that times them.
            let mark = &s.marks[(mark_write % s.marks.len() as u64) as usize];
            mark.position.store(write, Ordering::Relaxed);
            mark.timestamp
                .store(timestamp.as_nanos(), Ordering::Relaxed);
            mark.discontinuity.store(self.gap, Ordering::Relaxed);
            s.mark_write.store(mark_write + 1, Ordering::Release);
            if self.gap {
                s.discontinuities.fetch_add(1, Ordering::Relaxed);
                self.gap = false;
            }
        }
        let channels = s.channels;
        let mut slot = (write % s.capacity) as usize;
        let capacity = s.capacity as usize;
        for frame in 0..frames {
            let base = slot * channels;
            for channel in 0..channels {
                s.samples[base + channel]
                    .store(sample(frame, channel).to_bits(), Ordering::Relaxed);
            }
            slot += 1;
            if slot == capacity {
                slot = 0;
            }
        }
        s.write.store(write + frames64, Ordering::Release);
        Push::Written
    }
}

/// The consumer half: owned by the input stream, read on the audio thread.
pub struct RingConsumer {
    shared: Arc<Shared>,
    rate: SampleRate,
    /// Position and time of the newest mark at or before the read position.
    base: Option<(u64, Timestamp)>,
    /// A mark that starts a new run was passed; the next non-empty read reports it.
    pending: bool,
    /// Timestamp of the last non-empty read: the next one is later.
    last_start: Option<Timestamp>,
    delivered: u64,
}

impl RingConsumer {
    /// Channels per frame.
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Sample frames waiting, across discontinuities.
    pub fn available(&self) -> usize {
        let s = &*self.shared;
        (s.write.load(Ordering::Acquire) - s.read.load(Ordering::Relaxed)) as usize
    }

    /// Counters since the ring was created.
    pub fn stats(&self) -> InputStats {
        let s = &*self.shared;
        InputStats::new(
            s.captured.load(Ordering::Relaxed),
            self.delivered,
            s.dropped.load(Ordering::Relaxed),
            s.discontinuities.load(Ordering::Relaxed),
        )
    }

    /// Final counters once the producer has stopped: frames not read yet count as
    /// dropped. Call after the OS stream is stopped, off the audio thread.
    pub fn close(self) -> InputStats {
        let mut stats = self.stats();
        stats.dropped += self.available() as u64;
        stats
    }

    /// Moves up to one output length of the oldest frames into `out`, one slice per
    /// channel, as [`AudioInputStream::read`](crate::AudioInputStream::read) specifies.
    pub fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError> {
        let s = &*self.shared;
        let channels = s.channels;
        if out.len() != channels {
            return Err(AudioIoError::BufferMismatch);
        }
        let len = out[0].len();
        if out.iter().any(|c| c.len() != len) {
            return Err(AudioIoError::BufferMismatch);
        }
        let read = s.read.load(Ordering::Relaxed);
        // Samples first, then marks: every mark of a visible sample is visible too.
        let write = s.write.load(Ordering::Acquire);
        let mark_write = s.mark_write.load(Ordering::Acquire);
        let mut mark_read = s.mark_read.load(Ordering::Relaxed);
        let mark_count = s.marks.len() as u64;
        let mark_at = |index: u64| &s.marks[(index % mark_count) as usize];

        // Take the marks at or before the read position.
        while mark_read < mark_write {
            let mark = mark_at(mark_read);
            let position = mark.position.load(Ordering::Relaxed);
            if position > read {
                break;
            }
            self.base = Some((
                position,
                Timestamp::from_nanos(mark.timestamp.load(Ordering::Relaxed)),
            ));
            self.pending |= mark.discontinuity.load(Ordering::Relaxed);
            mark_read += 1;
        }

        // Stop before the next mark that starts a new run.
        let mut frames = (write - read).min(len as u64);
        let mut index = mark_read;
        while index < mark_write {
            let mark = mark_at(index);
            let position = mark.position.load(Ordering::Relaxed);
            if position >= read + frames {
                break;
            }
            if mark.discontinuity.load(Ordering::Relaxed) {
                frames = position - read;
                break;
            }
            index += 1;
        }
        if frames == 0 {
            s.mark_read.store(mark_read, Ordering::Release);
            return Ok(AudioRead::EMPTY);
        }

        let capacity = s.capacity as usize;
        let mut slot = (read % s.capacity) as usize;
        for frame in 0..frames as usize {
            let base = slot * channels;
            for (channel, out) in out.iter_mut().enumerate() {
                out[frame] = f32::from_bits(s.samples[base + channel].load(Ordering::Relaxed));
            }
            slot += 1;
            if slot == capacity {
                slot = 0;
            }
        }
        let extrapolated = match self.base {
            Some((position, time)) => {
                Cadence::samples(time, self.rate).time_of(read.saturating_sub(position))
            }
            None => Timestamp::ZERO,
        };
        // A packet stamped early (OS timestamp jitter, a mapping refresh) must not
        // move a read back in time: timestamps strictly increase. Only the previous
        // start is the floor, not its end, so a backward step of the mapping is
        // corrected at the next mark instead of being carried forward.
        let timestamp = match self.last_start {
            Some(last) if extrapolated <= last => last + Duration::from_nanos(1),
            _ => extrapolated,
        };
        self.last_start = Some(timestamp);
        let end = read + frames;
        s.read.store(end, Ordering::Release);

        // Take the marks inside the frames just read, so the base stays current.
        while mark_read < mark_write {
            let mark = mark_at(mark_read);
            let position = mark.position.load(Ordering::Relaxed);
            if position >= end {
                break;
            }
            self.base = Some((
                position,
                Timestamp::from_nanos(mark.timestamp.load(Ordering::Relaxed)),
            ));
            mark_read += 1;
        }
        s.mark_read.store(mark_read, Ordering::Release);

        let discontinuity = std::mem::take(&mut self.pending);
        self.delivered += frames;
        Ok(AudioRead::new(frames as usize, timestamp, discontinuity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::media::ChannelLayout;

    const MS: u64 = 1_000_000;

    fn stereo() -> AudioFormat {
        AudioFormat::STEREO_48K
    }

    fn packet(start: usize, frames: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|i| [i as f32, -(i as f32)])
            .collect()
    }

    fn read(consumer: &mut RingConsumer, len: usize) -> (AudioRead, Vec<f32>, Vec<f32>) {
        let mut left = vec![0.0; len];
        let mut right = vec![0.0; len];
        let result = consumer.read(&mut [&mut left, &mut right]);
        let Ok(result) = result else {
            panic!("read failed: {result:?}");
        };
        left.truncate(result.frames);
        right.truncate(result.frames);
        (result, left, right)
    }

    #[test]
    fn samples_keep_order_and_timestamps_extrapolate() {
        let (mut producer, mut consumer) = channel(stereo(), 1_000, 8);
        assert_eq!(
            producer.push(Timestamp::from_nanos(10 * MS), &packet(0, 480)),
            Push::Written
        );
        assert_eq!(
            producer.push(Timestamp::from_nanos(20 * MS), &packet(480, 480)),
            Push::Written
        );
        assert_eq!(consumer.available(), 960);

        let (first, left, right) = read(&mut consumer, 600);
        assert_eq!(
            first,
            AudioRead::new(600, Timestamp::from_nanos(10 * MS), true)
        );
        assert!(left.iter().enumerate().all(|(i, s)| *s == i as f32));
        assert!(right.iter().enumerate().all(|(i, s)| *s == -(i as f32)));

        // 600 frames into a packet stamped at 10 ms: 120 frames after the 20 ms mark.
        let (second, left, _) = read(&mut consumer, 600);
        assert_eq!(
            second,
            AudioRead::new(360, Timestamp::from_nanos(22_500_000), false)
        );
        assert_eq!(left.first(), Some(&600.0));
        assert_eq!(read(&mut consumer, 600).0, AudioRead::EMPTY);
        let stats = consumer.stats();
        assert_eq!(
            (stats.captured, stats.delivered, stats.dropped),
            (960, 960, 0)
        );
        assert_eq!(stats.discontinuities, 1);
    }

    #[test]
    fn early_packets_never_move_reads_backwards() {
        let (mut producer, mut consumer) = channel(stereo(), 4_000, 8);
        // The second packet is stamped before the first (a broken OS time), the
        // third 3 ms late.
        let _ = producer.push(Timestamp::from_nanos(10 * MS), &packet(0, 480));
        let _ = producer.push(Timestamp::from_nanos(9 * MS), &packet(480, 480));
        let _ = producer.push(Timestamp::from_nanos(33 * MS), &packet(960, 480));
        let times: Vec<_> = (0..3)
            .map(|_| read(&mut consumer, 480).0.timestamp)
            .collect();
        assert_eq!(
            times,
            [
                Timestamp::from_nanos(10 * MS),
                Timestamp::from_nanos(10 * MS + 1),
                Timestamp::from_nanos(33 * MS),
            ]
        );
    }

    #[test]
    fn a_full_ring_drops_whole_packets_and_starts_a_new_run() {
        let (mut producer, mut consumer) = channel(stereo(), 1_000, 8);
        assert_eq!(
            producer.push(Timestamp::from_nanos(MS), &packet(0, 600)),
            Push::Written
        );
        assert_eq!(
            producer.push(Timestamp::from_nanos(2 * MS), &packet(600, 600)),
            Push::Dropped
        );
        assert_eq!(
            producer.push(Timestamp::from_nanos(30 * MS), &packet(1_200, 400)),
            Push::Written
        );
        // The read stops at the new run, which the next read reports.
        let (first, _, _) = read(&mut consumer, 2_000);
        assert_eq!(first, AudioRead::new(600, Timestamp::from_nanos(MS), true));
        let (second, left, _) = read(&mut consumer, 2_000);
        assert_eq!(
            second,
            AudioRead::new(400, Timestamp::from_nanos(30 * MS), true)
        );
        assert_eq!(left.first(), Some(&1_200.0));
        let stats = consumer.stats();
        assert_eq!(
            (stats.captured, stats.delivered, stats.dropped),
            (1_600, 1_000, 600)
        );
        assert_eq!(stats.discontinuities, 2);
    }

    #[test]
    fn gaps_and_lost_frames_start_new_runs() {
        let (mut producer, mut consumer) = channel(stereo(), 4_000, 8);
        let _ = producer.push(Timestamp::from_nanos(MS), &packet(0, 100));
        producer.gap();
        let _ = producer.push_silence(Timestamp::from_nanos(5 * MS), 100);
        producer.lost(50);
        let _ = producer.push(Timestamp::from_nanos(9 * MS), &packet(0, 100));
        let runs: Vec<_> = (0..4).map(|_| read(&mut consumer, 1_000).0).collect();
        assert_eq!(
            runs,
            [
                AudioRead::new(100, Timestamp::from_nanos(MS), true),
                AudioRead::new(100, Timestamp::from_nanos(5 * MS), true),
                AudioRead::new(100, Timestamp::from_nanos(9 * MS), true),
                AudioRead::EMPTY,
            ]
        );
        let stats = consumer.close();
        assert_eq!((stats.captured, stats.dropped), (350, 50));
    }

    #[test]
    fn full_mark_ring_extrapolates_within_a_run_and_drops_new_runs() {
        let (mut producer, mut consumer) = channel(stereo(), 10_000, 2);
        for k in 0..4_u64 {
            // Packets 2 and 3 find the mark ring full and are stored without marks.
            let push = producer.push(Timestamp::from_nanos(k * 11 * MS), &packet(0, 480));
            assert_eq!(push, Push::Written);
        }
        producer.gap();
        assert_eq!(
            producer.push(Timestamp::from_nanos(50 * MS), &packet(0, 480)),
            Push::Dropped
        );
        let (all, _, _) = read(&mut consumer, 10_000);
        assert_eq!(all, AudioRead::new(1_920, Timestamp::ZERO, true));
        // After reading, marks are free again and the new run is stored.
        assert_eq!(
            producer.push(Timestamp::from_nanos(60 * MS), &packet(0, 480)),
            Push::Written
        );
        assert_eq!(
            read(&mut consumer, 10_000).0,
            AudioRead::new(480, Timestamp::from_nanos(60 * MS), true)
        );
    }

    #[test]
    fn wraps_around_many_times() {
        let (mut producer, mut consumer) = channel(stereo(), 1_001, 4);
        let mut next = 0_usize;
        for k in 0..1_000_u64 {
            let _ = producer.push(Timestamp::from_nanos(k * 10 * MS), &packet(next, 480));
            next += 480;
            let (result, left, right) = read(&mut consumer, 480);
            assert_eq!(result.frames, 480);
            assert_eq!(result.timestamp, Timestamp::from_nanos(k * 10 * MS));
            assert_eq!(result.discontinuity, k == 0);
            assert_eq!(left[0], (next - 480) as f32);
            assert_eq!(right[479], -((next - 1) as f32));
        }
    }

    #[test]
    fn planar_and_sample_conversion() {
        let format = AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Surround3_1)
            .unwrap_or(AudioFormat::STEREO_48K);
        let (mut producer, mut consumer) = channel(format, 100, 4);
        let a: [i16; 3] = [i16::MIN, 0, 16_384];
        let b: [i16; 2] = [1, 2];
        assert_eq!(
            producer.push_planar(Timestamp::ZERO, &[&a, &b]),
            Push::Written
        );
        let mut out = [[9.0_f32; 4]; 4];
        let [o0, o1, o2, o3] = &mut out;
        let Ok(result) = consumer.read(&mut [&mut o0[..], &mut o1[..], &mut o2[..], &mut o3[..]])
        else {
            panic!("read failed");
        };
        assert_eq!(result.frames, 2);
        assert_eq!(&out[0][..2], &[-1.0, 0.0]);
        assert_eq!(out[2][..2], [0.0, 0.0]);

        assert_eq!(I24([0xff, 0xff, 0x7f]).to_f32(), 8_388_607.0 / 8_388_608.0);
        assert_eq!(I24([0x00, 0x00, 0x80]).to_f32(), -1.0);
        assert_eq!(i32::MIN.to_f32(), -1.0);
        assert_eq!(128_u8.to_f32(), 0.0);
        assert_eq!(
            frames_for(SampleRate::HZ_44100, Duration::from_millis(10)),
            441
        );
        assert_eq!(frames_for(SampleRate::HZ_48000, Duration::from_nanos(1)), 1);
        let rate = SampleRate::HZ_48000;
        assert_eq!(
            capacity_frames(rate, Duration::from_millis(50), 4_096),
            8_192
        );
        assert_eq!(
            capacity_frames(rate, Duration::from_millis(200), 480),
            9_600
        );
    }

    #[test]
    fn mismatched_buffers_are_refused() {
        let (_producer, mut consumer) = channel(stereo(), 100, 4);
        let mut a = [0.0_f32; 4];
        let mut b = [0.0_f32; 3];
        assert_eq!(
            consumer.read(&mut [&mut a[..]]),
            Err(AudioIoError::BufferMismatch)
        );
        assert_eq!(
            consumer.read(&mut [&mut a[..], &mut b[..]]),
            Err(AudioIoError::BufferMismatch)
        );
    }

    #[test]
    fn producer_and_consumer_on_two_threads() {
        let (mut producer, mut consumer) = channel(stereo(), 2_048, 16);
        const PACKETS: usize = 2_000;
        let writer = std::thread::spawn(move || {
            let mut next = 0;
            let mut k = 0_u64;
            while (next / 64) < PACKETS {
                let samples = packet(next, 64);
                if producer.push(Timestamp::from_nanos(k), &samples) == Push::Written {
                    next += 64;
                    k += 1;
                } else {
                    // Full: yield until the reader catches up. The drop was counted;
                    // the reader checks only ordering, not completeness.
                    std::thread::yield_now();
                }
            }
        });
        let mut expected_left = 0.0_f32;
        let mut seen = 0;
        let mut left = vec![0.0_f32; 100];
        let mut right = vec![0.0_f32; 100];
        while seen < PACKETS * 64 {
            let Ok(result) = consumer.read(&mut [&mut left, &mut right]) else {
                panic!("read failed");
            };
            for i in 0..result.frames {
                assert_eq!(left[i], expected_left, "frame {seen}");
                assert_eq!(right[i], -expected_left);
                expected_left += 1.0;
                seen += 1;
            }
            if result.frames == 0 {
                if writer.is_finished() && consumer.available() == 0 {
                    break;
                }
                std::thread::yield_now();
            }
        }
        assert!(writer.join().is_ok());
        assert_eq!(seen, PACKETS * 64);
    }
}
