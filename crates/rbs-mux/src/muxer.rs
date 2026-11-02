//! Muxer factory and session traits.

use std::time::Duration;

use rbs_core::media::{ContainerId, EncodedPacket, MuxChunk, StreamDescriptor, StreamIndex};
use rbs_core::registry::AdapterFactory;

use crate::error::{CreateError, MuxError};

/// Everything needed to create a muxer session. Build with [`MuxerConfig::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxerConfig {
    /// Container to write.
    pub container: ContainerId,
    /// The streams, in [`StreamIndex`] order. Usually taken from the encoders'
    /// `stream()` descriptors.
    pub streams: Vec<StreamDescriptor>,
    /// Target fragment (fMP4) or cluster (Matroska) duration; also the most media a
    /// muxer may hold back for interleaving. `None` means 1 second.
    pub fragment_duration: Option<Duration>,
}

impl MuxerConfig {
    /// A configuration with the default 1 s interleaving bound.
    pub fn new(container: ContainerId, streams: Vec<StreamDescriptor>) -> Self {
        Self {
            container,
            streams,
            fragment_duration: None,
        }
    }

    /// Sets the fragment (fMP4) or cluster (Matroska) duration.
    pub fn with_fragment_duration(mut self, duration: Duration) -> Self {
        self.fragment_duration = Some(duration);
        self
    }

    /// The interleaving bound: `fragment_duration`, or 1 second when unset.
    pub fn max_buffered(&self) -> Duration {
        self.fragment_duration.unwrap_or(Duration::from_secs(1))
    }
}

/// Creates muxer sessions. One per adapter, registered in a `Registry<dyn MuxerFactory>`.
pub trait MuxerFactory: AdapterFactory {
    /// Creates a session. Streams the container cannot carry are refused here with
    /// [`CreateError::Unsupported`].
    fn create(&self, config: &MuxerConfig) -> Result<Box<dyn Muxer>, CreateError>;
}

/// One muxer session, driven by one output worker at a time.
///
/// Call order: `begin`, any number of `write`, then `finish`.
pub trait Muxer: Send {
    /// Writes the container header as one or more `Header` chunks.
    fn begin(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError>;

    /// **Hot.** Adds one packet of `stream`. Emits `Media` chunks when a fragment,
    /// cluster or packet is complete. Holds back at most
    /// [`MuxerConfig::max_buffered`] of media. Does not allocate per call once its
    /// buffers have grown to the stream's working size, and never copies payloads
    /// that it can pass through as `Bytes` slices.
    fn write(
        &mut self,
        stream: StreamIndex,
        packet: &EncodedPacket,
        out: &mut Vec<MuxChunk>,
    ) -> Result<(), MuxError>;

    /// Emits held-back media, then `Trailer` chunks, then `Patch` chunks for seekable
    /// transports. The session cannot be used afterwards.
    fn finish(&mut self, out: &mut Vec<MuxChunk>) -> Result<(), MuxError>;
}
