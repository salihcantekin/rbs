//! Media-file decode port (ADR-0108).
//!
//! A [`MediaDecoderFactory`] opens a path the host has already accepted and returns
//! pictures, interleaved PCM, and container timestamps. It does not own the master
//! clock, `loop`, `restart_when_shown`, the playback cursor, or the queues. Those
//! belong to the `org.rbs.media` node.
//!
//! FFmpeg adapter not linked in this tree yet (ADR-0007). The real adapter will be
//! `rbs-media-ffmpeg`, dynamically linked and LGPL. Conformance uses
//! `rbs-media-decode-test`. This crate does not link `ffmpeg-sys` or download FFmpeg.
//!
//! Capability `media.decode.pixel` is `zero-copy` or `cpu-upload`. The host prefers
//! `zero-copy`, then registration order, unless the source's `adapter` setting names
//! one id.

mod path;

pub mod caps;

use std::path::Path;
use std::time::Duration;

use rbs_core::registry::AdapterFactory;

pub use path::{PathError, accept_local_media_path, canonical_is_local};

/// Largest picture side, in pixels (ADR-0108 decision 6, same ceiling as a still).
pub const MAX_SIDE: u32 = 8192;
/// Largest one-frame payload, in bytes (128 MiB, the still decoder-buffer ceiling).
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
/// Video lookahead slot cap. The byte cap can stop the queue earlier.
pub const VIDEO_LOOKAHEAD: usize = 8;
/// Audio blocks kept ahead of the graph. Each block is the graph's `block_frames`.
pub const AUDIO_LOOKAHEAD: usize = 4;

/// `media.decode.pixel` values.
pub mod pixel {
    /// GPU handle the existing YUV decoder can sample.
    pub const ZERO_COPY: &str = "zero-copy";
    /// System memory the worker uploads through `SourceUpload` before enqueue.
    pub const CPU_UPLOAD: &str = "cpu-upload";
}

/// Why a picture must not be queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureRefuse {
    /// A side is 0 or over [`MAX_SIDE`].
    Side,
    /// The payload is over [`MAX_FRAME_BYTES`].
    FrameBytes,
}

impl PictureRefuse {
    /// What `InvalidSetting` should say.
    pub const fn expected(self) -> &'static str {
        match self {
            PictureRefuse::Side => "at most 8192 pixels on a side",
            PictureRefuse::FrameBytes => "at most 128 MiB in one frame",
        }
    }
}

/// Refuses a picture that must not allocate a queue (ADR-0108 decision 6).
pub fn refuse_picture(width: u32, height: u32, payload_bytes: usize) -> Option<PictureRefuse> {
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        return Some(PictureRefuse::Side);
    }
    if payload_bytes > MAX_FRAME_BYTES {
        Some(PictureRefuse::FrameBytes)
    } else {
        None
    }
}

/// Pixel layout the node will enqueue. Anything else is converted by the adapter or
/// `open` fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PictureLayout {
    /// Straight-alpha sRGB `Rgba8`, the still-source rule (GIF, APNG, animated WebP).
    Rgba8,
    /// 8-bit 4:2:0. The node uses the existing `YuvDecoder` (`check_color`).
    Nv12,
    /// 10-bit 4:2:0. The node uses the existing `YuvDecoder` (`check_color`).
    P010,
}

/// Colour the adapter already converted to something `check_color` can accept, or sRGB
/// for a full-frame picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleColor {
    /// Straight-alpha sRGB.
    Srgb,
    /// BT.709 limited-range Y'CbCr.
    Bt709Limited,
    /// BT.709 full-range Y'CbCr.
    Bt709Full,
}

/// One picture. `bytes` is the payload the worker enqueues after the size check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Layout after the adapter's conversion.
    pub layout: PictureLayout,
    /// Colour space of `bytes`.
    pub color: SampleColor,
    /// Plane bytes. Not copied by the port.
    pub bytes: Vec<u8>,
}

/// Interleaved `f32` PCM from the first audio stream. Further streams are ignored by
/// the node.
#[derive(Debug, Clone, PartialEq)]
pub struct InterleavedPcm {
    /// Source sample rate.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Interleaved samples.
    pub samples: Vec<f32>,
}

/// One pull from the container. A missing timestamp continues from the previous stamp
/// by `frame_duration`. Neither stamp nor duration fails the open (`Media`).
#[derive(Debug, Clone, PartialEq)]
pub struct MediaSample {
    /// Container presentation time, when the container states one.
    pub pts: Option<Duration>,
    /// Container frame duration, when the container states one.
    pub frame_duration: Option<Duration>,
    /// The picture, when this sample has one.
    pub picture: Option<Picture>,
    /// PCM from the first audio stream, when this sample has some.
    pub pcm: Option<InterleavedPcm>,
}

/// Why decode failed. The host maps this to `SourceError::Media`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The file, codec, or a mid-stream fault. The text is for logs.
    Failed(String),
    /// A sample had neither a timestamp nor a frame duration to continue from.
    NoTimestamp,
    /// The adapter will not convert this layout.
    Unsupported(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Failed(text) => f.write_str(text),
            DecodeError::NoTimestamp => {
                f.write_str("the container has neither a timestamp nor a frame duration")
            }
            DecodeError::Unsupported(what) => write!(f, "unsupported {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// An open file. The node seeks and pulls. Dropping it releases the file.
pub trait MediaDecoder: Send {
    /// Seeks to `cursor` from the start of the file. Called on wake, not from `process`.
    fn seek(&mut self, cursor: Duration) -> Result<(), DecodeError>;

    /// The next sample, or `Ok(None)` at the end of the file.
    fn pull(&mut self) -> Result<Option<MediaSample>, DecodeError>;

    /// Whether the container has an audio stream. The node still has an audio pin when
    /// this is false and emits silence (ADR-0108 decision 2).
    fn audio_stream(&self) -> bool;
}

/// Factory registered by a composition root. `open` is only called with a path
/// [`accept_local_media_path`] already accepted.
pub trait MediaDecoderFactory: AdapterFactory {
    /// Opens `path`. The returned decoder is dropped by the caller when the probe or
    /// the worker ends. A probe must not keep it.
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError>;
}
