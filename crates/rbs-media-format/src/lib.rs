//! Bitstream and framing helpers shared by muxer and transport adapters (ADR-0019).
//!
//! Pure functions over byte slices. They depend only on `rbs-core`, so any adapter may
//! use them without depending on another adapter.
//!
//! - [`nal`]: H.264 / HEVC NAL unit iteration and Annex B <-> length-prefixed
//!   conversion.
//! - [`avc`]: `AVCDecoderConfigurationRecord` (avcC) parsing and building.
//! - [`aac`]: `AudioSpecificConfig` parsing and ADTS headers.
//! - [`amf0`]: AMF0 values for FLV script tags and RTMP commands.
//! - [`flv`]: FLV tags and H.264/AAC tag bodies, shared by the FLV muxer and RTMP.
//!
//! Functions that run per packet write into a caller-provided `Vec<u8>` and do not
//! allocate once it has capacity (hot paths, P6 and P15).

pub mod aac;
pub mod amf0;
pub mod avc;
pub mod flv;
pub mod nal;

use std::fmt;

/// Why a bitstream could not be read.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// The data ends before a structure is complete.
    Truncated(&'static str),
    /// A field has a value the format does not allow.
    Invalid(&'static str),
    /// Valid, but not supported by these helpers.
    Unsupported(&'static str),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Truncated(what) => write!(f, "truncated {what}"),
            FormatError::Invalid(what) => write!(f, "invalid {what}"),
            FormatError::Unsupported(what) => write!(f, "unsupported {what}"),
        }
    }
}

impl std::error::Error for FormatError {}
