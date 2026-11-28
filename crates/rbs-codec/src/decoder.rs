//! Live video decoder port: packets in, pictures out (ADR-0139 decision 2).
//!
//! Unlike `rbs-media-decode`, which opens a file and owns demuxing and seeking, a live
//! decoder owns neither: the caller depacketizes the network stream, validates it, and
//! submits one access unit at a time. Contract: `docs/specs/backend-ports.md`, section
//! "Live video decode".

use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, MemoryKind, Size, StreamDescriptor, VideoFrame,
};
use rbs_core::registry::AdapterFactory;

use crate::error::{CreateError, DecodeError};

/// Where the decoder should put its pictures.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeTarget {
    /// LUID of the GPU adapter the studio renders on (Direct3D). A decoder whose
    /// surfaces must live on that adapter decodes there, or hands out system memory;
    /// it never hands out a native surface on another adapter.
    pub adapter_luid: Option<u64>,
    /// Memory kinds the caller can import, most wanted first. Empty means system
    /// memory only. An adapter hands out the first kind it can produce.
    pub accept: [Option<MemoryKind>; 3],
}

impl DecodeTarget {
    /// System memory only.
    pub const fn system() -> Self {
        Self {
            adapter_luid: None,
            accept: [None; 3],
        }
    }

    /// Accepts `kind` first (builder).
    pub const fn with_native(mut self, kind: MemoryKind) -> Self {
        self.accept[0] = Some(kind);
        self
    }

    /// The GPU adapter to decode on (builder).
    pub const fn on_adapter(mut self, luid: u64) -> Self {
        self.adapter_luid = Some(luid);
        self
    }

    /// True when `kind` is acceptable.
    pub fn accepts(&self, kind: MemoryKind) -> bool {
        kind == MemoryKind::System || self.accept.contains(&Some(kind))
    }
}

/// Default of [`VideoDecoderConfig::min_surfaces`].
pub const DEFAULT_MIN_SURFACES: usize = 3;

/// Counters of one decoder session, read on a cold thread (ADR-0139). Monotonic since
/// `create`; `reset` does not clear them.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecoderStats {
    /// Pictures produced.
    pub pictures: u64,
    /// Pictures dropped because every native surface was leased.
    pub dropped_no_surface: u64,
    /// Pictures copied to system memory (P4) instead of handed out natively.
    pub cpu_copies: u64,
    /// True while the decoder waits for a keyframe.
    pub need_keyframe: bool,
}

/// Configuration of one live video decoder session.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct VideoDecoderConfig {
    /// The codec. Phase 1 of ADR-0139 needs `H264` only.
    pub codec: CodecId,
    /// Framing of every packet submitted. The live case (the guest source) is `AnnexB`
    /// with empty `extradata`: one access unit per packet, SPS and PPS in band.
    /// `LengthPrefixed` needs `extradata`.
    pub bitstream: BitstreamFormat,
    /// Parameter sets for `LengthPrefixed` input (an `avcC` record). `None` for
    /// `AnnexB`, where SPS and PPS arrive in band.
    pub extradata: Option<Bytes>,
    /// Largest picture the caller allows; an SPS above it is refused with
    /// [`DecodeError::InvalidInput`] before the platform decoder sees it. The caller
    /// has already validated the SPS (ADR-0139 decision 11); this is the adapter's
    /// second line, not the first.
    pub max_size: Size,
    /// Where pictures should live.
    pub target: DecodeTarget,
    /// Surfaces the caller needs in flight at once (unreleased leases plus the picture
    /// being decoded). `create` gives at least this many native surfaces, or fails with
    /// [`CreateError::Unsupported`] or [`CreateError::ResourceExhausted`]. Ignored for
    /// system-memory output. Default 3.
    pub min_surfaces: usize,
}

impl VideoDecoderConfig {
    /// An H.264 Annex B session of at most `max_size` pictures.
    pub fn h264_annex_b(max_size: Size) -> Self {
        Self {
            codec: CodecId::H264,
            bitstream: BitstreamFormat::AnnexB,
            extradata: None,
            max_size,
            target: DecodeTarget::system(),
            min_surfaces: DEFAULT_MIN_SURFACES,
        }
    }

    /// Sets the surfaces needed in flight (builder).
    pub fn with_min_surfaces(mut self, n: usize) -> Self {
        self.min_surfaces = n;
        self
    }

    /// Sets the target (builder).
    pub fn with_target(mut self, target: DecodeTarget) -> Self {
        self.target = target;
        self
    }
}

/// Creates live video decoder sessions. One per adapter, registered in a
/// `Registry<dyn VideoDecoderFactory>`. Capabilities: `codec.video.decode`,
/// `decode.output_memory`, `codec.sessions.max` (see [`crate::caps`]).
pub trait VideoDecoderFactory: AdapterFactory {
    /// Creates a session. May allocate and open the device; must not leave threads or
    /// handles behind when the session is dropped, and starts no thread before this
    /// call (P13).
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError>;
}

/// One live decoder session, driven by one worker thread at a time.
///
/// Pictures come out in presentation order with the `pts` of the packet that produced
/// them; the adapter keeps no clock and no jitter buffer.
pub trait VideoDecoder: Send {
    /// What the session decodes. `StreamParams::Video` carries the size once known;
    /// before the first parameter sets it holds `config.max_size`.
    fn stream(&self) -> &StreamDescriptor;

    /// Submits one access unit. Never blocks longer than one frame interval. A packet
    /// of another codec or framing is [`DecodeError::InvalidInput`]. Until a keyframe
    /// with parameter sets has been decoded, a non-key packet is dropped and the call
    /// returns [`DecodeError::NeedKeyframe`] (the caller sends a PLI).
    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), DecodeError>;

    /// Moves pictures decoded so far to `out`, in presentation order. Does not
    /// allocate when `out` has spare capacity. A native picture carries its lease:
    /// the surface goes back to the adapter's pool when the lease is released, and
    /// only then (ADR-0109 decision 8).
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError>;

    /// Drops every queued packet and picture and waits for the next keyframe. Used
    /// after packet loss the caller cannot repair, and when the source stops being
    /// drawn (ADR-0139 decision 12). Outstanding leases stay valid.
    fn reset(&mut self) -> Result<(), DecodeError>;

    /// Decodes everything submitted and moves the remaining pictures to `out`. A
    /// second flush emits nothing.
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError>;

    /// Native surfaces this session can have outstanding at once (pool size). The
    /// caller keeps at most this many pictures unreleased; a decoder with every
    /// surface leased drops the picture (counted) rather than allocate.
    fn max_surfaces(&self) -> usize {
        0
    }

    /// Counters, read on a cold thread. Cheap and lock-free for the worker (atomics).
    fn stats(&self) -> DecoderStats;
}
