//! Media data shared by every port, node and adapter: video frames, audio blocks,
//! encoded packets, and the leases that keep their memory alive.
//!
//! These are plain data. Core never touches pixels or GPU APIs (principle P1);
//! `rbs-gpu` resolves texture ids and imports native surfaces. Contract:
//! `docs/specs/media-types.md`. Decision: `docs/adr/0018-media-frame-representation.md`.

pub mod audio;
pub mod lease;
pub mod packet;
pub mod video;

use std::fmt;

pub use audio::{
    AudioBlock, AudioFormat, AudioSamples, ChannelLayout, DEFAULT_BLOCK_FRAMES, MAX_CHANNELS,
    PlanarChannels,
};
/// Reference-counted, sliceable byte buffer used for packet payloads and CPU frames.
pub use bytes::Bytes;
pub use lease::{Lease, LeaseOwner};
pub use packet::{
    BitstreamFormat, CodecId, EncodedPacket, MediaKind, StreamDescriptor, StreamParams,
};
pub use video::{
    ColorPrimaries, ColorRange, ColorSpace, D3dApi, DmaBufPlane, DmaBufSurface, FramePayload,
    MatrixCoefficients, MemoryKind, NativeSurface, PixelFormat, PlaneLayout, Size, SurfaceSync,
    SystemFrame, TextureId, TransferFunction, VideoFormat, VideoFrame,
};

/// Why a media value could not be built. Never allocates.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaError {
    /// A width or height is zero.
    EmptySize,
    /// Wrong number of planes for the pixel format (or too many for the container).
    PlaneCount {
        /// Planes expected (or the maximum).
        expected: usize,
        /// Planes given.
        actual: usize,
    },
    /// A row stride is shorter than one row of the plane.
    StrideTooSmall {
        /// Plane index.
        plane: usize,
        /// Stride given.
        stride: usize,
        /// Minimum row length in bytes.
        min: u64,
    },
    /// The buffer ends before the last row of a plane.
    BufferTooSmall {
        /// Plane index.
        plane: usize,
        /// Bytes needed.
        needed: u64,
        /// Buffer length.
        actual: usize,
    },
    /// Wrong number of audio channels.
    ChannelCount {
        /// Channels expected (or the maximum).
        max: usize,
        /// Channels given.
        actual: usize,
    },
    /// A packet's decode time is after its presentation time.
    DtsAfterPts,
    /// An audio channel or interleaved buffer has the wrong length.
    ChannelLength {
        /// Channel index (0 for interleaved).
        channel: usize,
        /// Samples expected.
        expected: usize,
        /// Samples given.
        actual: usize,
    },
}

impl fmt::Display for MediaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MediaError::EmptySize => f.write_str("width or height is zero"),
            MediaError::PlaneCount { expected, actual } => {
                write!(f, "expected {expected} planes, got {actual}")
            }
            MediaError::StrideTooSmall { plane, stride, min } => {
                write!(f, "plane {plane}: stride {stride} is below row size {min}")
            }
            MediaError::BufferTooSmall {
                plane,
                needed,
                actual,
            } => write!(
                f,
                "plane {plane}: needs {needed} bytes, buffer has {actual}"
            ),
            MediaError::DtsAfterPts => f.write_str("decode time is after presentation time"),
            MediaError::ChannelCount { max, actual } => {
                write!(f, "expected {max} channels, got {actual}")
            }
            MediaError::ChannelLength {
                channel,
                expected,
                actual,
            } => write!(
                f,
                "channel {channel}: expected {expected} samples, got {actual}"
            ),
        }
    }
}

impl std::error::Error for MediaError {}
