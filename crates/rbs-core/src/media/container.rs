//! Container output shared by the mux and transport ports: container ids, stream
//! indices and the chunks a muxer emits.
//!
//! These live in the foundation because both `rbs-mux` (producer) and
//! `rbs-transport` (consumer) use them and ports depend only on `rbs-core`
//! (principle P1). See `docs/specs/output-ports.md` and ADR-0019.

use std::fmt;

use bytes::Bytes;

use crate::time::Timestamp;

/// A container format.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContainerId {
    /// Fragmented ISO BMFF (fMP4): `ftyp` + `moov`, then `moof` + `mdat` fragments.
    Mp4Fragmented,
    /// Matroska / WebM.
    Matroska,
    /// MPEG-2 transport stream.
    MpegTs,
    /// Flash Video (FLV), including Enhanced RTMP codec extensions.
    Flv,
}

impl ContainerId {
    /// Every container id, in declaration order.
    pub const ALL: [ContainerId; 4] = [
        ContainerId::Mp4Fragmented,
        ContainerId::Matroska,
        ContainerId::MpegTs,
        ContainerId::Flv,
    ];

    /// Stable lowercase name, used in capability values (`mux.container`).
    pub const fn as_str(self) -> &'static str {
        match self {
            ContainerId::Mp4Fragmented => "mp4.fragmented",
            ContainerId::Matroska => "matroska",
            ContainerId::MpegTs => "mpegts",
            ContainerId::Flv => "flv",
        }
    }

    /// Parses a name produced by [`ContainerId::as_str`].
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// The `transport.input` capability value for a transport that takes this
    /// container, for example `container.mpegts`.
    pub const fn transport_input(self) -> &'static str {
        match self {
            ContainerId::Mp4Fragmented => "container.mp4.fragmented",
            ContainerId::Matroska => "container.matroska",
            ContainerId::MpegTs => "container.mpegts",
            ContainerId::Flv => "container.flv",
        }
    }
}

impl fmt::Display for ContainerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Index of a stream within one output, in the order of the stream descriptors given
/// to the muxer or transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamIndex(pub u16);

impl StreamIndex {
    /// The index as a `usize`, for slice access.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

/// What a [`MuxChunk`] contains. New kinds (for example segment boundaries for HLS)
/// may be added in minor versions.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// Container header. Always emitted before any media.
    Header,
    /// Media data.
    Media {
        /// True if a reader can start decoding at the start of this chunk.
        keyframe: bool,
        /// Earliest decode time of the media in the chunk, in master-clock time.
        timestamp: Timestamp,
    },
    /// Data written once at the end (indexes, final sizes).
    Trailer,
    /// Overwrite bytes at an absolute offset from the start of the byte stream. Only
    /// seekable transports apply it; the output stays playable when it is ignored.
    Patch {
        /// Offset of the first byte to overwrite.
        offset: u64,
    },
}

/// A piece of container output. Build with [`MuxChunk::new`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuxChunk {
    /// What the chunk contains.
    pub kind: ChunkKind,
    /// The bytes. Shared and reference counted, so fan-out never copies.
    pub data: Bytes,
}

impl MuxChunk {
    /// Creates a chunk.
    pub fn new(kind: ChunkKind, data: Bytes) -> Self {
        Self { kind, data }
    }

    /// True for chunks that belong to the sequential byte stream (everything except
    /// patches).
    pub const fn is_sequential(&self) -> bool {
        !matches!(self.kind, ChunkKind::Patch { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_round_trip() {
        for c in ContainerId::ALL {
            assert_eq!(ContainerId::from_name(c.as_str()), Some(c));
            assert_eq!(c.transport_input(), format!("container.{c}"));
        }
        assert_eq!(ContainerId::from_name("avi"), None);
    }

    #[test]
    fn patches_are_not_sequential() {
        let patch = MuxChunk::new(
            ChunkKind::Patch { offset: 8 },
            Bytes::from_static(&[0, 0, 0, 1]),
        );
        let header = MuxChunk::new(ChunkKind::Header, Bytes::new());
        assert!(!patch.is_sequential());
        assert!(header.is_sequential());
        assert_eq!(StreamIndex(3).get(), 3);
    }
}
