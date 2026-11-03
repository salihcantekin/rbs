//! Input streams and the factory trait adapters implement.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rbs_core::media::AudioFormat;
use rbs_core::registry::AdapterFactory;
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::SampleRate;

use crate::endpoint::{AudioEndpoint, EndpointId, EndpointKind, Permission};
use crate::error::AudioIoError;

/// Ring capacity when the request does not name one.
pub const DEFAULT_BUFFER: Duration = Duration::from_millis(200);
/// Smallest ring capacity an adapter accepts; shorter requests are raised to it.
pub const MIN_BUFFER: Duration = Duration::from_millis(50);
/// Largest ring capacity an adapter accepts; longer requests are lowered to it.
pub const MAX_BUFFER: Duration = Duration::from_secs(2);

/// What the consumer asks of an input stream.
#[non_exhaustive]
#[derive(Clone)]
pub struct InputRequest {
    /// Which endpoint to read, from `enumerate`.
    pub endpoint: EndpointId,
    /// Sample rates the consumer prefers, best first. The stream delivers the first
    /// one in the endpoint's `sample_rates`, else the endpoint's native rate; a rate
    /// mismatch is never an error, because the audio graph resamples.
    pub sample_rates: Vec<SampleRate>,
    /// How much audio the stream keeps for a consumer that falls behind, clamped to
    /// [`MIN_BUFFER`]..=[`MAX_BUFFER`]. Audio beyond it is dropped and counted.
    pub buffer: Duration,
    /// The master clock samples are stamped with (ADR-0004).
    pub clock: Arc<dyn Clock>,
}

impl InputRequest {
    /// A request for `endpoint` at its native rate with the default buffer.
    pub fn new(endpoint: EndpointId, clock: Arc<dyn Clock>) -> Self {
        Self {
            endpoint,
            sample_rates: Vec::new(),
            buffer: DEFAULT_BUFFER,
            clock,
        }
    }

    /// Sets the preferred sample rates, best first.
    pub fn with_sample_rates(mut self, rates: &[SampleRate]) -> Self {
        self.sample_rates = rates.to_vec();
        self
    }

    /// Sets the buffer length.
    pub fn with_buffer(mut self, buffer: Duration) -> Self {
        self.buffer = buffer;
        self
    }

    /// The format a stream delivers for this request on `endpoint`: the first
    /// requested rate the endpoint lists, else its native rate, with its channels.
    pub fn format_for(&self, endpoint: &AudioEndpoint) -> AudioFormat {
        let mut format = endpoint.format;
        if let Some(rate) = self
            .sample_rates
            .iter()
            .find(|r| endpoint.sample_rates.contains(r))
        {
            format.sample_rate = *rate;
        }
        format
    }

    /// The buffer length clamped to the accepted range.
    pub fn clamped_buffer(&self) -> Duration {
        self.buffer.clamp(MIN_BUFFER, MAX_BUFFER)
    }
}

impl fmt::Debug for InputRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InputRequest")
            .field("endpoint", &self.endpoint)
            .field("sample_rates", &self.sample_rates)
            .field("buffer", &self.buffer)
            .finish_non_exhaustive()
    }
}

/// The result of one [`AudioInputStream::read`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioRead {
    /// Sample frames written to the start of every output channel. Zero when nothing
    /// was available.
    pub frames: usize,
    /// Master-clock time of the first frame read. Meaningless when `frames` is zero.
    pub timestamp: Timestamp,
    /// True when these frames do not directly follow the frames of the previous
    /// non-empty read: the first read after `start`, and the first after audio was
    /// dropped or the OS reported a gap. Within a run, the next read's timestamp is
    /// this one plus `frames` at the stream's rate, up to the device's drift.
    pub discontinuity: bool,
}

impl AudioRead {
    /// A read that returned nothing.
    pub const EMPTY: AudioRead = AudioRead {
        frames: 0,
        timestamp: Timestamp::ZERO,
        discontinuity: false,
    };

    /// A read of `frames` frames starting at `timestamp`.
    pub const fn new(frames: usize, timestamp: Timestamp, discontinuity: bool) -> Self {
        Self {
            frames,
            timestamp,
            discontinuity,
        }
    }
}

/// Counters of an input stream, for diagnostics and the conformance suite.
///
/// Non-exhaustive: adapters start from `InputStats::default()` and set fields.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputStats {
    /// Sample frames received from the OS.
    pub captured: u64,
    /// Sample frames returned by `read`.
    pub delivered: u64,
    /// Sample frames received but never delivered: the buffer was full, or `stop`
    /// discarded them.
    pub dropped: u64,
    /// Runs started: the first audio after `start`, after a drop, or after a gap the
    /// OS reported.
    pub discontinuities: u64,
}

impl InputStats {
    /// Counters with the given values.
    pub const fn new(captured: u64, delivered: u64, dropped: u64, discontinuities: u64) -> Self {
        Self {
            captured,
            delivered,
            dropped,
            discontinuities,
        }
    }

    /// The sum of two sets of counters, for a stream that keeps totals across rings.
    pub const fn plus(self, other: InputStats) -> Self {
        Self::new(
            self.captured + other.captured,
            self.delivered + other.delivered,
            self.dropped + other.dropped,
            self.discontinuities + other.discontinuities,
        )
    }
}

/// One input stream from one endpoint.
///
/// Created in Standby by [`AudioBackend::open_input`]; [`start`](Self::start) makes it
/// Active and [`stop`](Self::stop) returns it to Standby (ADR-0013). Dropping the
/// stream releases everything it holds. Driven from one thread at a time.
pub trait AudioInputStream: Send {
    /// The delivered format. Never changes; a device format change ends the stream
    /// with [`AudioIoError::FormatChanged`].
    fn format(&self) -> AudioFormat;

    /// Standby -> Active: opens the OS stream and starts capturing.
    ///
    /// May allocate, start threads and block briefly (at most 500 ms). Called during
    /// warm-up, never from the audio thread. Calling it on an Active stream is a no-op.
    fn start(&mut self) -> Result<(), AudioIoError>;

    /// Active -> Standby: stops the OS stream and releases what `start` acquired.
    /// Audio not read yet is discarded (counted as dropped). Idempotent.
    fn stop(&mut self);

    /// Moves the oldest available audio into `out`, one slice per channel.
    ///
    /// `out` has exactly `format().channels.channels()` slices of equal length;
    /// otherwise nothing is read and [`AudioIoError::BufferMismatch`] is returned.
    /// Reads at most that length. The frames returned are always one contiguous run:
    /// a read stops before a discontinuity, which the next read reports. Returns
    /// [`AudioRead::EMPTY`] in Standby and when nothing is available.
    ///
    /// **Real-time**: called from the audio thread every block. Must not allocate,
    /// free, block, do I/O, log or take any lock; the hand-off from the OS callback is
    /// the lock-free [`ring`](crate::ring).
    fn read(&mut self, out: &mut [&mut [f32]]) -> Result<AudioRead, AudioIoError>;

    /// Sample frames a `read` could return now, up to the next discontinuity or
    /// beyond it. **Real-time**, same rules as `read`.
    fn available(&self) -> usize;

    /// Stream counters. Cheap enough for every block.
    fn stats(&self) -> InputStats;

    /// How long after its timestamps the stream's audio is presented (heard) at the
    /// endpoint, as measured by the adapter at the last `start`: for `OutputLoopback`
    /// and `Application` streams, the render endpoint's output latency (the OS stamps
    /// loopback audio when it mixes it, before the device plays it). `None` for input
    /// streams, in Standby, and when the adapter cannot measure it. The audio graph adds
    /// it to the source's sync offset so loopback audio lines up with what was seen
    /// (ADR-0042). Not real-time: read it after `start`, not per block.
    fn presentation_latency(&self) -> Option<Duration> {
        None
    }
}

/// The factory every audio I/O adapter implements.
///
/// Methods added in later minor versions come with a default implementation that
/// returns [`AudioIoError::Unsupported`], so existing adapters keep compiling. Audio
/// output (monitoring) will be added the same way.
pub trait AudioBackend: AdapterFactory {
    /// Lists the endpoints of `kind` available now.
    ///
    /// Not real-time; may take up to 100 ms. Never shows a permission prompt and keeps
    /// no resources afterwards. Returns [`AudioIoError::Unsupported`] for kinds the
    /// adapter cannot list.
    fn enumerate(&self, kind: EndpointKind) -> Result<Vec<AudioEndpoint>, AudioIoError>;

    /// Opens an input stream on one endpoint, in Standby.
    ///
    /// Never prompts and never activates the device (a microphone indicator stays
    /// off): the stream owns no threads, OS streams or device handles until
    /// [`AudioInputStream::start`]. The format is decided here from what `enumerate`
    /// reported ([`InputRequest::format_for`]).
    fn open_input(&self, request: &InputRequest)
    -> Result<Box<dyn AudioInputStream>, AudioIoError>;

    /// Whether capturing `kind` is currently allowed. Fast; never prompts.
    fn permission(&self, kind: EndpointKind) -> Permission {
        let _ = kind;
        Permission::Granted
    }

    /// Asks the user for permission to capture `kind` if it is not determined yet.
    /// Blocks while the OS prompt is shown; call it from a control thread.
    fn request_permission(&self, kind: EndpointKind) -> Result<Permission, AudioIoError> {
        Ok(self.permission(kind))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_core::time::TestClock;

    #[test]
    fn format_follows_the_request_rules() {
        let endpoint = AudioEndpoint::new(
            EndpointId::new("a"),
            EndpointKind::Input,
            "A",
            AudioFormat::STEREO_48K,
        )
        .with_sample_rates(&[SampleRate::HZ_44100]);
        let clock: Arc<dyn Clock> = Arc::new(TestClock::new());
        let request = InputRequest::new(EndpointId::new("a"), Arc::clone(&clock));
        assert_eq!(request.format_for(&endpoint), AudioFormat::STEREO_48K);

        let request = request.with_sample_rates(&[SampleRate::HZ_96000, SampleRate::HZ_44100]);
        assert_eq!(
            request.format_for(&endpoint).sample_rate,
            SampleRate::HZ_44100
        );
        let request = request.with_sample_rates(&[SampleRate::HZ_96000]);
        assert_eq!(
            request.format_for(&endpoint).sample_rate,
            SampleRate::HZ_48000
        );

        assert_eq!(
            request.clone().with_buffer(Duration::ZERO).clamped_buffer(),
            MIN_BUFFER
        );
        assert_eq!(
            request
                .with_buffer(Duration::from_secs(60))
                .clamped_buffer(),
            MAX_BUFFER
        );
    }
}
