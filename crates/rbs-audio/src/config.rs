//! The graph format and block timing.

use std::fmt;
use std::time::Duration;

use rbs_core::media::{AudioFormat, DEFAULT_BLOCK_FRAMES, MAX_CHANNELS};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, Rational};
use rbs_graph::runtime::{MAX_BLOCK_FRAMES, Tick};

/// Default delay of the audio graph behind real time.
pub const DEFAULT_LATENCY: Duration = Duration::from_millis(50);
/// Shortest graph latency accepted.
pub const MIN_LATENCY: Duration = Duration::from_millis(10);
/// Longest graph latency accepted.
pub const MAX_LATENCY: Duration = Duration::from_secs(1);

/// Why a configuration was refused.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioGraphError {
    /// The format has no channels or more than `MAX_CHANNELS`.
    InvalidFormat,
    /// The block size is 0 or larger than `MAX_BLOCK_FRAMES`.
    BlockFrames(u32),
    /// The latency is outside `MIN_LATENCY..=MAX_LATENCY`.
    Latency(Duration),
}

impl fmt::Display for AudioGraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AudioGraphError::InvalidFormat => f.write_str("invalid audio graph format"),
            AudioGraphError::BlockFrames(n) => write!(f, "invalid block size {n}"),
            AudioGraphError::Latency(d) => write!(f, "invalid graph latency {d:?}"),
        }
    }
}

impl std::error::Error for AudioGraphError {}

/// The format, block size and latency of one audio partition. Every audio link in the
/// partition carries blocks of exactly this format and size.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioGraphConfig {
    /// Rate and channels of every block.
    pub format: AudioFormat,
    /// Frames per block.
    pub block_frames: u32,
    /// How far behind real time the graph runs, so every input has delivered a
    /// block's audio by the time it is mixed.
    pub latency: Duration,
}

impl Default for AudioGraphConfig {
    fn default() -> Self {
        Self {
            format: AudioFormat::STEREO_48K,
            block_frames: DEFAULT_BLOCK_FRAMES,
            latency: DEFAULT_LATENCY,
        }
    }
}

impl AudioGraphConfig {
    /// A checked configuration.
    pub fn new(
        format: AudioFormat,
        block_frames: u32,
        latency: Duration,
    ) -> Result<Self, AudioGraphError> {
        let n = format.channels.channels();
        if n == 0 || n > MAX_CHANNELS {
            return Err(AudioGraphError::InvalidFormat);
        }
        if block_frames == 0 || block_frames > MAX_BLOCK_FRAMES {
            return Err(AudioGraphError::BlockFrames(block_frames));
        }
        if !(MIN_LATENCY..=MAX_LATENCY).contains(&latency) {
            return Err(AudioGraphError::Latency(latency));
        }
        Ok(Self {
            format,
            block_frames,
            latency,
        })
    }

    /// The block cadence: `sample_rate / block_frames` ticks per second from `origin`.
    pub fn cadence(&self, origin: Timestamp) -> Cadence {
        // Both parts are non-zero unless a caller zeroed `block_frames` by hand; then
        // 100 blocks per second.
        let rate =
            Rational::new(self.format.sample_rate.hz(), self.block_frames).unwrap_or(HUNDRED);
        Cadence::new(origin, rate)
    }

    /// The length of one block.
    pub fn period(&self) -> Duration {
        self.format
            .sample_rate
            .duration_of(u64::from(self.block_frames))
    }

    /// The master time of the first frame of the block that `tick` produces: the tick's
    /// time minus the latency. `None` for ticks earlier than the latency after the clock
    /// origin, which produce no block (their time would repeat 0).
    pub fn block_time(&self, tick: Tick) -> Option<Timestamp> {
        let latency = u64::try_from(self.latency.as_nanos()).unwrap_or(u64::MAX);
        tick.time
            .as_nanos()
            .checked_sub(latency)
            .map(Timestamp::from_nanos)
    }
}

const HUNDRED: Rational = match Rational::new(100, 1) {
    Some(r) => r,
    None => panic!("100/1 is a valid rational"),
};

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use rbs_core::media::ChannelLayout;
    use rbs_core::timing::SampleRate;

    #[test]
    fn block_times_follow_the_cadence_minus_latency_and_never_repeat() {
        let config = AudioGraphConfig::default();
        let cadence = config.cadence(Timestamp::ZERO);
        assert_eq!(config.period(), Duration::from_millis(10));
        // The first five ticks are earlier than the latency: no block.
        for i in 0..5 {
            assert_eq!(config.block_time(Tick::of(&cadence, i)), None);
        }
        let mut last = None;
        for i in 5..100_000u64 {
            let t = config.block_time(Tick::of(&cadence, i)).unwrap();
            // Exactly `block_frames` frames apart at 48 kHz: 10 ms.
            assert_eq!(t.as_nanos(), (i - 5) * 10_000_000);
            assert!(last.is_none_or(|l| t > l));
            last = Some(t);
        }
    }

    #[test]
    fn at_44_1_khz_blocks_stay_on_the_sample_grid() {
        let format = AudioFormat::new(SampleRate::HZ_44100, ChannelLayout::Stereo).unwrap();
        let config = AudioGraphConfig::new(format, 441, Duration::from_millis(50)).unwrap();
        let cadence = config.cadence(Timestamp::ZERO);
        let t = |i| cadence.time_of(i).as_nanos();
        // 441 frames at 44.1 kHz is exactly 10 ms; after a day still exact.
        assert_eq!(t(8_640_000), 86_400_000_000_000);
    }

    #[test]
    fn invalid_configurations_are_refused() {
        let f = AudioFormat::STEREO_48K;
        assert_eq!(
            AudioGraphConfig::new(f, 0, DEFAULT_LATENCY),
            Err(AudioGraphError::BlockFrames(0))
        );
        assert_eq!(
            AudioGraphConfig::new(f, 480, Duration::from_millis(1)),
            Err(AudioGraphError::Latency(Duration::from_millis(1)))
        );
        assert!(AudioGraphConfig::new(f, 480, DEFAULT_LATENCY).is_ok());
    }
}
