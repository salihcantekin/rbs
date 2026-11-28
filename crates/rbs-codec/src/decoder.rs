//! Live video decoder port: packets in, pictures out (ADR-0139 decision 2).
//!
//! Unlike `rbs-media-decode`, which opens a file and owns demuxing and seeking, a live
//! decoder owns neither: the caller depacketizes the network stream, validates it, and
//! submits one access unit at a time. Contract: `docs/specs/backend-ports.md`, section
//! "Live video decode".

use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, EncodedPacket, MemoryKind, Size, VideoFormat, VideoFrame,
};
use rbs_core::registry::AdapterFactory;

use crate::error::{CreateError, DecodeError};

/// Where the decoder should put its pictures. (Not `rbs_media_decode::DecodeTarget`, the
/// file port's type; the two have different fields.)
///
/// `adapter_luid: None` on Windows means system memory only. Native kinds are named per
/// platform by the spec: `MemoryKind::D3d11` means `NativeSurface::D3dSharedHandle` with
/// `nt: true` (never a raw texture pointer), `MemoryKind::Metal` means `IoSurface`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeOutput {
    /// LUID of the GPU adapter the studio renders on (Direct3D). A decoder whose
    /// surfaces must live on that adapter decodes there, or hands out system memory;
    /// it never hands out a native surface on another adapter.
    pub adapter_luid: Option<u64>,
    /// Memory kinds the caller can import, most wanted first. Empty means system
    /// memory only. An adapter hands out the first kind it can produce.
    pub accept: [Option<MemoryKind>; 3],
}

impl DecodeOutput {
    /// System memory only.
    pub const fn system() -> Self {
        Self {
            adapter_luid: None,
            accept: [None; 3],
        }
    }

    /// Accepts `kind` as the next preference (builder); at most three are kept.
    pub fn accepting(mut self, kind: MemoryKind) -> Self {
        if let Some(slot) = self.accept.iter_mut().find(|s| s.is_none()) {
            *slot = Some(kind);
        }
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
    /// Largest picture the caller allows, compared with the *cropped* (display) size
    /// (a 1080p stream is coded at 1088 lines). A parameter set above it is refused with
    /// [`DecodeError::InvalidInput`] before the platform decoder sees it, using the
    /// shared parser in [`crate::h264`]. The caller has validated the SPS too (ADR-0139
    /// decision 11); this is the adapter's second line.
    pub max_size: Size,
    /// Where pictures should live.
    pub target: DecodeOutput,
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
            target: DecodeOutput::system(),
            min_surfaces: DEFAULT_MIN_SURFACES,
        }
    }

    /// Uses length-prefixed input with the `avcC` record in `extradata` (builder).
    pub fn length_prefixed(mut self, length_size: u8, extradata: Bytes) -> Self {
        self.bitstream = BitstreamFormat::LengthPrefixed { length_size };
        self.extradata = Some(extradata);
        self
    }

    /// Sets the surfaces needed in flight (builder).
    pub fn with_min_surfaces(mut self, n: usize) -> Self {
        self.min_surfaces = n;
        self
    }

    /// Sets the target (builder).
    pub fn with_target(mut self, target: DecodeOutput) -> Self {
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
    /// The picture format once known (cropped size, NV12 or the native layout, colour
    /// from the VUI, BT.709 limited when absent); `None` before the first parameter
    /// sets. Updated when the adapter follows an in-band SPS change.
    fn output_format(&self) -> Option<VideoFormat>;

    /// Submits one access unit. Not blocking beyond the platform call itself, and
    /// never more than 2 ms of adapter work. A packet of another codec or framing is
    /// [`DecodeError::InvalidInput`]. Until a keyframe with parameter sets has been
    /// decoded (and again after `reset`, `flush` or a platform decode error), a non-key
    /// packet is dropped and the call returns [`DecodeError::NeedKeyframe`] (the caller
    /// sends a PLI). [`DecodeError::Busy`] means the adapter's bounded input queue is
    /// full: `drain`, then submit the same packet again.
    ///
    /// The picture keeps the packet's `pts` bit-exactly (not rounded to the platform's
    /// time unit; the adapter keeps its own FIFO keyed by submission order if needed).
    fn submit(&mut self, packet: &EncodedPacket) -> Result<(), DecodeError>;

    /// Moves pictures decoded so far to `out`, in presentation order. Does not
    /// allocate when `out` has spare capacity. A native picture carries its lease:
    /// the surface goes back to the adapter's pool when the lease is released, and
    /// only then (ADR-0109 decision 8).
    fn drain(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError>;

    /// Drops every queued packet and picture and waits for the next keyframe. Keeps the
    /// session and its pool. Outstanding leases stay valid. (A guest source that is not
    /// drawn drops the whole decoder instead, so Standby holds no hardware session.)
    fn reset(&mut self) -> Result<(), DecodeError>;

    /// Decodes everything submitted and moves the remaining pictures to `out`. Afterwards
    /// the session is usable again and gated on a keyframe, like after `reset`. A
    /// second flush emits nothing.
    fn flush(&mut self, out: &mut Vec<VideoFrame>) -> Result<(), DecodeError>;

    /// Most native surfaces this session has leased out at once; the decoder enforces
    /// it (a new picture with every surface leased is released and counted in
    /// `dropped_no_surface`; it never allocates beyond it and never waits). 0 if and
    /// only if the session hands out system memory only. Adapters with native output
    /// must override it; at most 8, so four guests stay under the importer's limit.
    fn max_surfaces(&self) -> usize {
        0
    }

    /// Counters, read on a cold thread. Cheap and lock-free for the worker (atomics).
    fn stats(&self) -> DecoderStats;
}
