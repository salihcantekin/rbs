//! Encoder factory and session traits.

use rbs_core::media::{AudioBlock, EncodedPacket, StreamDescriptor, VideoFrame};
use rbs_core::registry::AdapterFactory;

use crate::config::{AudioEncoderConfig, EncoderChange, VideoEncoderConfig};
use crate::error::{CreateError, EncodeError};

/// Creates video encoder sessions. One per adapter, registered in a
/// `Registry<dyn VideoEncoderFactory>`.
pub trait VideoEncoderFactory: AdapterFactory {
    /// Creates a session. May allocate and open the device; must not leave threads or
    /// handles behind when the returned session is dropped.
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError>;
}

/// One video encoder session, driven by one encoder worker thread at a time.
pub trait VideoEncoder: Send {
    /// Codec parameters and extradata. Valid from creation on, so muxers and
    /// transports can write headers before the first packet.
    fn stream(&self) -> &StreamDescriptor;

    /// **Hot.** Submits one frame. Never blocks longer than one frame interval. A frame
    /// that does not match the session's format, size or memory kind is refused with
    /// [`EncodeError::InvalidInput`]. To keep the frame after returning, `share()` it (the
    /// clone holds its own lease).
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError>;

    /// **Hot.** Moves packets produced so far to `out`. Does not allocate when `out`
    /// has spare capacity.
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;

    /// Applies a runtime change. Unsupported changes return
    /// [`EncodeError::Unsupported`]; they never silently do nothing.
    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError>;

    /// Encodes every pending frame and moves the remaining packets to `out`. After a
    /// flush, every submitted frame has produced exactly one packet.
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
}

/// Creates audio encoder sessions.
pub trait AudioEncoderFactory: AdapterFactory {
    /// Creates a session.
    fn create(&self, config: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError>;
}

/// One audio encoder session.
pub trait AudioEncoder: Send {
    /// Codec parameters and extradata, valid from creation on.
    fn stream(&self) -> &StreamDescriptor;

    /// Sample frames per channel the codec consumes per packet (1024 for AAC-LC, 960
    /// for 20 ms Opus at 48 kHz).
    fn frame_size(&self) -> u32;

    /// **Hot.** Submits any number of sample frames; the adapter buffers to its frame
    /// size in pre-allocated storage. The first packet's `pts` is the first block's
    /// timestamp and each packet advances by exactly `frame_size` samples (encoder
    /// priming is hidden by the adapter).
    fn submit(&mut self, block: &AudioBlock<'_>) -> Result<(), EncodeError>;

    /// **Hot.** Moves packets produced so far to `out`.
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;

    /// Applies a runtime change ([`EncoderChange::Bitrate`] only; keyframe requests
    /// return `Unsupported` or succeed trivially for codecs where every packet is a
    /// keyframe).
    fn reconfigure(&mut self, change: &EncoderChange) -> Result<(), EncodeError>;

    /// Pads the last partial frame with silence, encodes it and moves the remaining
    /// packets to `out`.
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError>;
}
