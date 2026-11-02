//! Audio formats and blocks.
//!
//! Inside the engine audio is 32-bit float. The audio graph runs on fixed blocks of
//! [`DEFAULT_BLOCK_FRAMES`] sample frames at [`SampleRate::HZ_48000`] (10 ms).
//! Device sample formats (16-bit, 24-bit, ...) are converted at the edge by audio
//! adapters. See `docs/specs/media-types.md#audio`.

use std::fmt;

use super::MediaError;
use crate::time::Timestamp;
use crate::timing::SampleRate;

/// Sample frames per audio graph block by default (10 ms at 48 kHz).
pub const DEFAULT_BLOCK_FRAMES: u32 = 480;

/// The most channels a block can carry.
pub const MAX_CHANNELS: usize = 8;

/// Channel layout. Channel order follows the usual WAVE / SMPTE order
/// (L, R, C, LFE, Ls, Rs, Lb, Rb).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelLayout {
    /// One channel.
    Mono,
    /// Left, right.
    Stereo,
    /// L, R, C, LFE.
    Surround3_1,
    /// L, R, C, LFE, Ls, Rs.
    Surround5_1,
    /// L, R, C, LFE, Ls, Rs, Lb, Rb.
    Surround7_1,
    /// Channels with no speaker positions (1 to [`MAX_CHANNELS`]).
    Discrete(u8),
}

impl ChannelLayout {
    /// The number of channels.
    pub const fn channels(self) -> usize {
        match self {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
            ChannelLayout::Surround3_1 => 4,
            ChannelLayout::Surround5_1 => 6,
            ChannelLayout::Surround7_1 => 8,
            ChannelLayout::Discrete(n) => n as usize,
        }
    }
}

/// The format of an audio stream or link: rate and channels. Samples are `f32`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioFormat {
    /// Sample frames per second.
    pub sample_rate: SampleRate,
    /// Channel layout.
    pub channels: ChannelLayout,
}

impl AudioFormat {
    /// 48 kHz stereo, the default graph format.
    pub const STEREO_48K: Self = Self {
        sample_rate: SampleRate::HZ_48000,
        channels: ChannelLayout::Stereo,
    };

    /// Creates a format, checking the channel count.
    pub fn new(sample_rate: SampleRate, channels: ChannelLayout) -> Result<Self, MediaError> {
        let n = channels.channels();
        if n == 0 || n > MAX_CHANNELS {
            return Err(MediaError::ChannelCount {
                max: MAX_CHANNELS,
                actual: n,
            });
        }
        Ok(Self {
            sample_rate,
            channels,
        })
    }
}

/// Borrowed per-channel slices of a planar block, all the same length.
#[derive(Clone, Copy)]
pub struct PlanarChannels<'a> {
    channels: [&'a [f32]; MAX_CHANNELS],
    count: usize,
}

impl<'a> PlanarChannels<'a> {
    /// Number of channels.
    pub fn len(&self) -> usize {
        self.count
    }

    /// True if there are no channels (never the case for a valid block).
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Channel `index`, or `None` if out of range.
    pub fn get(&self, index: usize) -> Option<&'a [f32]> {
        self.channels[..self.count].get(index).copied()
    }

    /// The channels in order.
    pub fn iter(&self) -> impl Iterator<Item = &'a [f32]> + '_ {
        self.channels[..self.count].iter().copied()
    }
}

impl fmt::Debug for PlanarChannels<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} channels", self.count)
    }
}

/// The samples of a block, borrowed for the duration of a call.
#[non_exhaustive]
#[derive(Debug, Clone, Copy)]
pub enum AudioSamples<'a> {
    /// One slice per channel, each `frames` long. The graph's native layout.
    Planar(PlanarChannels<'a>),
    /// Frames of interleaved channels, `frames * channels` long.
    Interleaved(&'a [f32]),
}

/// A block of audio lent to a call: format, master-clock time of its first sample,
/// length in sample frames and the samples.
///
/// Building one never allocates. To keep samples beyond the call, copy them into a
/// buffer you pre-allocated.
#[derive(Debug, Clone, Copy)]
pub struct AudioBlock<'a> {
    format: AudioFormat,
    timestamp: Timestamp,
    frames: usize,
    samples: AudioSamples<'a>,
}

impl<'a> AudioBlock<'a> {
    /// A planar block. Every slice in `channels` must have the same length, and their
    /// number must match `format`.
    pub fn planar(
        format: AudioFormat,
        timestamp: Timestamp,
        channels: &[&'a [f32]],
    ) -> Result<Self, MediaError> {
        let expected = format.channels.channels();
        if channels.len() != expected || expected > MAX_CHANNELS {
            return Err(MediaError::ChannelCount {
                max: expected,
                actual: channels.len(),
            });
        }
        let frames = channels.first().map_or(0, |c| c.len());
        let mut stored: [&'a [f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (index, channel) in channels.iter().enumerate() {
            if channel.len() != frames {
                return Err(MediaError::ChannelLength {
                    channel: index,
                    expected: frames,
                    actual: channel.len(),
                });
            }
            stored[index] = channel;
        }
        Ok(Self {
            format,
            timestamp,
            frames,
            samples: AudioSamples::Planar(PlanarChannels {
                channels: stored,
                count: expected,
            }),
        })
    }

    /// An interleaved block. `samples.len()` must be a multiple of the channel count.
    pub fn interleaved(
        format: AudioFormat,
        timestamp: Timestamp,
        samples: &'a [f32],
    ) -> Result<Self, MediaError> {
        let channels = format.channels.channels();
        if channels == 0 || samples.len() % channels != 0 {
            return Err(MediaError::ChannelLength {
                channel: 0,
                expected: samples.len().next_multiple_of(channels.max(1)),
                actual: samples.len(),
            });
        }
        Ok(Self {
            format,
            timestamp,
            frames: samples.len() / channels,
            samples: AudioSamples::Interleaved(samples),
        })
    }

    /// The format.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Master-clock time of the first sample frame.
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Length in sample frames.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The samples.
    pub fn samples(&self) -> AudioSamples<'a> {
        self.samples
    }

    /// Channel `index` of a planar block, or `None` for interleaved blocks and
    /// out-of-range channels.
    pub fn channel(&self, index: usize) -> Option<&'a [f32]> {
        match self.samples {
            AudioSamples::Planar(channels) => channels.get(index),
            AudioSamples::Interleaved(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_and_formats() {
        assert_eq!(ChannelLayout::Surround5_1.channels(), 6);
        assert!(AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Discrete(0)).is_err());
        assert!(AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Discrete(9)).is_err());
        assert_eq!(
            AudioFormat::new(SampleRate::HZ_48000, ChannelLayout::Stereo),
            Ok(AudioFormat::STEREO_48K)
        );
    }

    #[test]
    fn planar_blocks_check_channels() {
        let left = [0.0_f32; 480];
        let right = [1.0_f32; 480];
        let t = Timestamp::from_nanos(10);
        let Ok(block) = AudioBlock::planar(AudioFormat::STEREO_48K, t, &[&left, &right]) else {
            panic!("valid stereo block");
        };
        assert_eq!(block.frames(), 480);
        assert_eq!(block.timestamp(), t);
        assert_eq!(block.channel(1).map(|c| c[0]), Some(1.0));
        assert!(block.channel(2).is_none());

        assert!(matches!(
            AudioBlock::planar(AudioFormat::STEREO_48K, t, &[&left]),
            Err(MediaError::ChannelCount { .. })
        ));
        assert!(matches!(
            AudioBlock::planar(AudioFormat::STEREO_48K, t, &[&left, &right[..479]]),
            Err(MediaError::ChannelLength { channel: 1, .. })
        ));
    }

    #[test]
    fn interleaved_blocks_check_length() {
        let samples = [0.5_f32; 960];
        let t = Timestamp::ZERO;
        let Ok(block) = AudioBlock::interleaved(AudioFormat::STEREO_48K, t, &samples) else {
            panic!("valid interleaved block");
        };
        assert_eq!(block.frames(), 480);
        assert!(block.channel(0).is_none());
        assert!(matches!(block.samples(), AudioSamples::Interleaved(s) if s.len() == 960));
        assert!(AudioBlock::interleaved(AudioFormat::STEREO_48K, t, &samples[..959]).is_err());
    }
}
