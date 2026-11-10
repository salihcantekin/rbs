//! Audio output: playing audio on a playback device (monitoring, ADR-0097).
//!
//! The engine pushes interleaved `f32` frames into an [`OutputProducer`]; the
//! adapter's OS render callback owns the [`OutputConsumer`] and pulls from it. The two
//! halves are a lock-free single-producer single-consumer ring, so neither side
//! allocates, locks or waits (principle P6). An empty ring plays silence and is
//! counted; a full ring drops the newest audio and is counted. Samples are stored as
//! `f32` bits in atomics, so the ring needs no `unsafe`.
//!
//! Contract: "Audio output" in `docs/specs/audio-io-port.md`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use rbs_core::media::AudioFormat;

use crate::endpoint::EndpointId;
use crate::error::AudioIoError;

/// Largest ring an output accepts, in frames (about 1.4 s at 48 kHz).
pub const MAX_RING_FRAMES: usize = 65_536;
/// Smallest ring an output accepts, in frames; smaller requests are raised to it.
pub const MIN_RING_FRAMES: usize = 256;
/// Ring size when the request does not name one (about 85 ms at 48 kHz).
pub const DEFAULT_RING_FRAMES: usize = 4_096;

/// What the engine asks of an output.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputRequest {
    /// Which playback endpoint to open, from `enumerate(EndpointKind::Output)`.
    pub endpoint: EndpointId,
    /// Capacity of the ring between the engine and the OS callback, in frames,
    /// clamped to [`MIN_RING_FRAMES`]..=[`MAX_RING_FRAMES`].
    pub ring_frames: usize,
}

impl OutputRequest {
    /// A request for `endpoint` with the default ring.
    pub fn new(endpoint: EndpointId) -> Self {
        Self {
            endpoint,
            ring_frames: DEFAULT_RING_FRAMES,
        }
    }

    /// Sets the ring capacity in frames.
    pub fn with_ring_frames(mut self, frames: usize) -> Self {
        self.ring_frames = frames;
        self
    }

    /// The ring capacity clamped to the accepted range.
    pub fn clamped_ring_frames(&self) -> usize {
        self.ring_frames.clamp(MIN_RING_FRAMES, MAX_RING_FRAMES)
    }
}

/// Counters of an output, read at any time.
///
/// Invariant: `written == played + dropped + queued`, and `silence` counts the frames
/// the OS asked for that the ring could not supply (they are not in `played`).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputStats {
    /// Frames the engine offered to the ring.
    pub written: u64,
    /// Frames the OS callback took from the ring.
    pub played: u64,
    /// Frames of silence the callback played because the ring was empty (underrun).
    pub silence: u64,
    /// Offered frames that did not fit and were dropped (overrun).
    pub dropped: u64,
}

/// An output opened on a playback endpoint, in Standby.
///
/// Like an input stream it owns no threads, OS streams or device handles until
/// [`start`](AudioOutput::start): opening does not activate the device.
pub trait AudioOutput: Send {
    /// The format the device plays (its shared-mode mix format; the adapter never
    /// resamples in the OS, so the engine converts to this).
    fn format(&self) -> AudioFormat;

    /// The producer half of the ring the engine writes into. Can be taken once.
    fn producer(&mut self) -> Option<OutputProducer>;

    /// Starts playing. Errors are the usual open-time ones; a device lost later is
    /// reported by [`status`](AudioOutput::status).
    fn start(&mut self) -> Result<(), AudioIoError>;

    /// Stops playing and releases the OS stream and its threads. The output can be
    /// started again. Dropping an output stops it.
    fn stop(&mut self);

    /// The adapter's best estimate of the device latency in nanoseconds.
    fn latency_ns(&self) -> u64;

    /// The counters of this output.
    fn stats(&self) -> OutputStats;

    /// `Ok` while the output plays or is stopped on purpose; the error that ended it
    /// otherwise ([`AudioIoError::EndpointLost`], [`AudioIoError::DeviceBusy`],
    /// [`AudioIoError::FormatChanged`]), recoverable as for input (ADR-0024).
    fn status(&self) -> Result<(), AudioIoError>;
}

struct Shared {
    /// Samples, `capacity * channels` of them, as `f32` bits.
    samples: Box<[AtomicU32]>,
    capacity: usize,
    channels: usize,
    /// Frames ever pushed into the ring (producer-owned, read by the consumer).
    head: AtomicU64,
    /// Frames ever taken out of the ring (consumer-owned, read by the producer).
    tail: AtomicU64,
    offered: AtomicU64,
    dropped: AtomicU64,
    silence: AtomicU64,
}

/// The engine's half of the output ring.
pub struct OutputProducer {
    shared: Arc<Shared>,
}

/// The adapter's half of the output ring, owned by the OS render callback.
pub struct OutputConsumer {
    shared: Arc<Shared>,
}

/// Creates a ring of `frames` frames of `channels` channels. Allocates; call it when
/// the output is opened, never on a real-time thread.
pub fn channel(channels: usize, frames: usize) -> (OutputProducer, OutputConsumer) {
    let channels = channels.max(1);
    let capacity = frames.clamp(MIN_RING_FRAMES, MAX_RING_FRAMES);
    let samples = (0..capacity * channels)
        .map(|_| AtomicU32::new(0))
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let shared = Arc::new(Shared {
        samples,
        capacity,
        channels,
        head: AtomicU64::new(0),
        tail: AtomicU64::new(0),
        offered: AtomicU64::new(0),
        dropped: AtomicU64::new(0),
        silence: AtomicU64::new(0),
    });
    (
        OutputProducer {
            shared: Arc::clone(&shared),
        },
        OutputConsumer { shared },
    )
}

impl OutputProducer {
    /// Channels per frame.
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Frames the ring can take now.
    pub fn space(&self) -> usize {
        let used =
            self.shared.head.load(Ordering::Relaxed) - self.shared.tail.load(Ordering::Acquire);
        self.shared.capacity - used as usize
    }

    /// Pushes interleaved frames (a trailing partial frame is ignored). Frames that do not fit are
    /// dropped whole and counted. Returns the frames stored. Lock-free and
    /// allocation-free.
    pub fn push(&mut self, interleaved: &[f32]) -> usize {
        let shared = &*self.shared;
        let frames = interleaved.len() / shared.channels;
        let head = shared.head.load(Ordering::Relaxed);
        let used = (head - shared.tail.load(Ordering::Acquire)) as usize;
        let take = frames.min(shared.capacity - used);
        for f in 0..take {
            let slot = ((head as usize + f) % shared.capacity) * shared.channels;
            for c in 0..shared.channels {
                shared.samples[slot + c].store(
                    interleaved[f * shared.channels + c].to_bits(),
                    Ordering::Relaxed,
                );
            }
        }
        shared.head.store(head + take as u64, Ordering::Release);
        shared.offered.fetch_add(frames as u64, Ordering::Relaxed);
        shared
            .dropped
            .fetch_add((frames - take) as u64, Ordering::Relaxed);
        take
    }

    /// The counters of the ring.
    pub fn stats(&self) -> OutputStats {
        self.shared.stats()
    }
}

/// Reads the counters of a ring from any thread, while the consumer lives in an OS
/// callback.
#[derive(Clone)]
pub struct OutputStatsReader {
    shared: Arc<Shared>,
}

impl OutputStatsReader {
    /// The counters of the ring.
    pub fn stats(&self) -> OutputStats {
        self.shared.stats()
    }
}

impl OutputConsumer {
    /// A reader of this ring's counters that stays with the caller.
    pub fn reader(&self) -> OutputStatsReader {
        OutputStatsReader {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Channels per frame.
    pub fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Fills `out` (interleaved, a multiple of the channel count) from the ring and
    /// plays silence for what the ring cannot supply (counted). Returns the frames
    /// taken from the ring. Lock-free and allocation-free: safe in a render callback.
    pub fn pull(&mut self, out: &mut [f32]) -> usize {
        let shared = &*self.shared;
        let frames = out.len() / shared.channels;
        let tail = shared.tail.load(Ordering::Relaxed);
        let ready = (shared.head.load(Ordering::Acquire) - tail) as usize;
        let take = frames.min(ready);
        for f in 0..take {
            let slot = ((tail as usize + f) % shared.capacity) * shared.channels;
            for c in 0..shared.channels {
                out[f * shared.channels + c] =
                    f32::from_bits(shared.samples[slot + c].load(Ordering::Relaxed));
            }
        }
        for v in &mut out[take * shared.channels..] {
            *v = 0.0;
        }
        shared.tail.store(tail + take as u64, Ordering::Release);
        shared
            .silence
            .fetch_add((frames - take) as u64, Ordering::Relaxed);
        take
    }

    /// The counters of the ring.
    pub fn stats(&self) -> OutputStats {
        self.shared.stats()
    }
}

impl Shared {
    fn stats(&self) -> OutputStats {
        OutputStats {
            written: self.offered.load(Ordering::Relaxed),
            played: self.tail.load(Ordering::Relaxed),
            silence: self.silence.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_come_out_in_order_and_the_counters_add_up() {
        let (mut p, mut c) = channel(2, 256);
        let input: Vec<f32> = (0..20).map(|i| i as f32).collect();
        assert_eq!(p.push(&input), 10);
        let mut out = [9.0f32; 24];
        assert_eq!(c.pull(&mut out), 10);
        assert_eq!(&out[..20], &input[..]);
        // The rest of the request is silence, counted as underrun.
        assert!(out[20..].iter().all(|v| *v == 0.0));
        let s = c.stats();
        assert_eq!((s.written, s.played, s.silence, s.dropped), (10, 10, 2, 0));
    }

    #[test]
    fn a_full_ring_drops_the_newest_frames_whole_and_counts_them() {
        let (mut p, mut c) = channel(1, 256);
        let big = vec![0.5f32; 300];
        assert_eq!(p.push(&big), 256);
        assert_eq!(p.space(), 0);
        let s = p.stats();
        assert_eq!((s.written, s.dropped), (300, 44));
        let mut out = vec![0.0f32; 256];
        assert_eq!(c.pull(&mut out), 256);
        assert_eq!(p.space(), 256);
        let s = c.stats();
        assert_eq!(s.written, s.played + s.dropped);
    }

    #[test]
    fn the_ring_wraps_without_losing_order() {
        let (mut p, mut c) = channel(1, 256);
        let mut next = 0.0f32;
        let mut expect = 0.0f32;
        let mut out = vec![0.0f32; 100];
        for _ in 0..20 {
            let batch: Vec<f32> = (0..100)
                .map(|_| {
                    next += 1.0;
                    next
                })
                .collect();
            assert_eq!(p.push(&batch), 100);
            assert_eq!(c.pull(&mut out), 100);
            for v in &out {
                expect += 1.0;
                assert_eq!(*v, expect);
            }
        }
    }

    #[test]
    fn requests_clamp_the_ring() {
        let id = EndpointId::new("x");
        assert_eq!(
            OutputRequest::new(id.clone())
                .with_ring_frames(1)
                .clamped_ring_frames(),
            MIN_RING_FRAMES
        );
        assert_eq!(
            OutputRequest::new(id)
                .with_ring_frames(usize::MAX)
                .clamped_ring_frames(),
            MAX_RING_FRAMES
        );
    }
}
