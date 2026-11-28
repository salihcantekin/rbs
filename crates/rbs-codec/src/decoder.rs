//! Audio decoder port: packets in, interleaved `f32` samples out.
//!
//! A live decoder (a remote guest's sound, ADR-0139 decision 2). It is not the file
//! port: it owns no demuxing and no seeking, and the caller does all timing.

use std::fmt;

use rbs_core::media::{AudioFormat, Bytes, CodecId, EncodedPacket};
use rbs_core::registry::AdapterFactory;

use crate::error::CreateError;

/// Everything needed to create an audio decoder session. Build with
/// [`AudioDecoderConfig::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDecoderConfig {
    /// Codec of the incoming packets.
    pub codec: CodecId,
    /// Sample rate and channel layout of the decoded output.
    pub format: AudioFormat,
    /// Codec setup data (`OpusHead`, `AudioSpecificConfig`); empty if the codec needs none.
    pub extradata: Bytes,
    /// Opaque adapter-specific options. The engine never reads them (principle P3).
    pub adapter_options: Vec<(String, String)>,
}

impl AudioDecoderConfig {
    /// A configuration with no extradata and no adapter options.
    pub fn new(codec: CodecId, format: AudioFormat) -> Self {
        Self {
            codec,
            format,
            extradata: Bytes::new(),
            adapter_options: Vec::new(),
        }
    }

    /// Sets the codec setup data.
    pub fn with_extradata(mut self, extradata: Bytes) -> Self {
        self.extradata = extradata;
        self
    }

    /// Adds an opaque adapter-specific option.
    pub fn with_adapter_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.adapter_options.push((key.into(), value.into()));
        self
    }
}

/// Creates audio decoder sessions. One per adapter, registered in a
/// `Registry<dyn AudioDecoderFactory>`.
pub trait AudioDecoderFactory: AdapterFactory {
    /// Creates a session. May allocate; must not leave threads or handles behind when the
    /// returned session is dropped.
    fn create(&self, config: &AudioDecoderConfig) -> Result<Box<dyn AudioDecoder>, CreateError>;
}

/// One audio decoder session, driven by one thread at a time.
pub trait AudioDecoder: Send {
    /// Format of the decoded samples.
    fn format(&self) -> AudioFormat;

    /// Most sample frames (per channel) one `decode` or `conceal` call can produce. A
    /// caller that reserves `max_frames() * channels` samples in `out` never makes the
    /// session allocate.
    fn max_frames(&self) -> usize;

    /// **Hot.** Decodes one packet. `out` is cleared first and receives the interleaved
    /// samples; the return value is the number of sample frames (per channel). Does not
    /// allocate when `out` has the capacity [`Self::max_frames`] implies. A packet that
    /// does not parse is [`DecodeError::Corrupt`] and leaves the session usable.
    fn decode(&mut self, packet: &EncodedPacket, out: &mut Vec<f32>) -> Result<usize, DecodeError>;

    /// **Hot.** Produces `frames` sample frames of concealment for a packet that never
    /// arrived, continuing the stream smoothly. `frames` must be a length the codec can
    /// conceal (for Opus a multiple of 2.5 ms, at most 120 ms), otherwise
    /// [`DecodeError::InvalidInput`]. Same buffer rules as [`Self::decode`].
    fn conceal(&mut self, frames: usize, out: &mut Vec<f32>) -> Result<usize, DecodeError>;

    /// Forgets all state, as for a new session, without releasing buffers. Call it after a
    /// gap too long to conceal.
    fn reset(&mut self);
}

/// Why a decoder call failed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The input does not fit the session (wrong codec, empty packet, bad length).
    InvalidInput(String),
    /// The packet is not valid for the codec. The session stays usable.
    Corrupt(String),
    /// Any other failure inside the adapter.
    Failed {
        /// Description for diagnostics.
        reason: String,
        /// Whether the session can continue.
        recoverable: bool,
    },
}

impl DecodeError {
    /// True when the session can continue.
    pub const fn is_recoverable(&self) -> bool {
        match self {
            DecodeError::InvalidInput(_) | DecodeError::Corrupt(_) => true,
            DecodeError::Failed { recoverable, .. } => *recoverable,
        }
    }
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::InvalidInput(e) => write!(f, "invalid input: {e}"),
            DecodeError::Corrupt(e) => write!(f, "corrupt packet: {e}"),
            DecodeError::Failed { reason, .. } => write!(f, "decode failed: {reason}"),
        }
    }
}

impl std::error::Error for DecodeError {}
