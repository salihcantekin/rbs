//! Live H.264 decoder for the RBS video decoder port (`docs/specs/backend-ports.md`,
//! "Live video decode", ADR-0139 phase 1c) over `VTDecompressionSession`.
//!
//! - Input: the live case only: Annex B, one access unit per packet, SPS and PPS in
//!   band. Every SPS is checked with `rbs_codec::h264` before VideoToolbox sees the
//!   bytes: malformed or above `max_size` (cropped size) is `InvalidInput`; outside the
//!   allow list (Constrained Baseline, Main, High, 4:2:0, 8-bit, level 5.2), interlaced,
//!   or above the declared 4096 x 2304 is `Unsupported`. NAL types 0, 13, 14, 15 and 20
//!   are `InvalidInput`. A refused packet that carries an SPS or a slice gates on the
//!   next keyframe. `LengthPrefixed` is refused at `create` (`Unsupported`).
//! - Each packet with a slice becomes one length-prefixed (AVCC) sample of its slices
//!   and SEI; SPS and PPS go into a `CMVideoFormatDescription`
//!   (`CMVideoFormatDescriptionCreateFromH264ParameterSets`). The session is created at
//!   the first IDR and recreated at the IDR that follows a changed SPS or PPS, so an
//!   in-band size change within `max_size` is followed; `output_format` changes when
//!   the packet with the new SPS is submitted. A packet without a slice (parameter
//!   sets, SEI, AUD) produces no picture.
//! - Low latency: synchronous decode without temporal processing (no reordering),
//!   `kVTDecompressionPropertyKey_RealTime`; the picture of a packet is ready when
//!   `submit` returns (lookahead 0).
//! - Stamps: each sample carries its submission index as `sourceFrameRefCon`; the
//!   session keeps the submitted stamps in a FIFO keyed by that index and gives each
//!   picture its packet's `pts` bit-exactly. More than 16 undrained pictures is `Busy`.
//! - Output: with `DecodeOutput` accepting `metal`, pictures are handed out as
//!   `NativeSurface::IoSurface` (NV12, `'420v'`; `'420f'` for a full-range stream)
//!   with `SurfaceSync::Ready` and a lease on one of `min_surfaces` (at most 8) slots
//!   (see `slots`). Otherwise, or when a picture cannot be handed out as a display-size
//!   IOSurface, pictures are copied to system memory (NV12, a new buffer per picture,
//!   counted in `cpu_copies`; `max_surfaces()` is then 0). The copy applies the SPS
//!   crop; colour comes from the VUI, BT.709 limited when absent.
//! - Errors: a refusal of the stream (`kVTCouldNotFindVideoDecoderErr`,
//!   `kVTVideoDecoderUnsupportedDataFormatErr`, `kVTCouldNotCreateInstanceErr`) is
//!   `Unsupported`; an invalid session, a removed decoder or a session malfunction
//!   (sleep, GPU change) is `DeviceLost`; a decoder malfunction, an allocation failure
//!   or a busy hardware decoder is a recoverable `Failed` that drops the platform
//!   session (the next IDR makes a new one); any other decode failure is a recoverable
//!   `Failed`. Every failure gates on the next keyframe, and a refused SPS or slice
//!   also forgets the staged parameter sets. An HDR transfer (PQ, HLG) is
//!   `Unsupported`. A failure to recreate the session for a changed stream is
//!   `FormatChanged`. Fallbacks to software decode or system memory are logged once at
//!   Warn in `Area::Encode`.
//!
//! The factory does nothing until `create`; `probe` asks VideoToolbox only whether
//! H.264 decodes in hardware (macOS always has the software decoder). No thread is
//! started by the adapter; the pool's janitor starts on the first dropped pool.

mod input;
mod picture;
mod session;
mod slots;
mod vt;

pub use session::VtH264Decoder;

use objc2_core_media::kCMVideoCodecType_H264;
use objc2_video_toolbox::VTIsHardwareDecodeSupported;
use rbs_codec::{CreateError, VideoDecoder, VideoDecoderConfig, VideoDecoderFactory, caps};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::Size;
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

/// Largest picture the decoder declares (`codec.video.decode.width`/`.height`).
pub const MAX_DECODE_SIZE: Size = Size::new(4096, 2304);

/// Factory of VideoToolbox live H.264 decoder sessions
/// (`org.rbs.codec.videotoolbox-h264-decode`).
#[derive(Debug, Default)]
pub struct VideoToolboxDecoderFactory;

impl VideoToolboxDecoderFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }

    /// Creates a session with its concrete type.
    pub fn create_session(
        &self,
        config: &VideoDecoderConfig,
    ) -> Result<VtH264Decoder, CreateError> {
        VtH264Decoder::open(config)
    }
}

pub(crate) fn capabilities(hardware: bool) -> CapabilitySet {
    let set = |v: &[&str]| CapabilityValue::Set(v.iter().map(|s| (*s).to_string()).collect());
    let side = |max: u32| CapabilityValue::IntRange {
        min: 16,
        max: i64::from(max),
    };
    CapabilitySet::new()
        .with(caps::HARDWARE, CapabilityValue::Flag(hardware))
        .with(caps::VIDEO_DECODE, set(&["h264"]))
        .with(
            caps::VIDEO_DECODE_PROFILES,
            set(&["h264.baseline", "h264.main", "h264.high"]),
        )
        .with(caps::VIDEO_DECODE_WIDTH, side(MAX_DECODE_SIZE.width))
        .with(caps::VIDEO_DECODE_HEIGHT, side(MAX_DECODE_SIZE.height))
        .with(caps::VIDEO_DECODE_FPS_MAX, CapabilityValue::Int(60))
        .with(caps::DECODE_OUTPUT_MEMORY, set(&["system", "metal"]))
}

impl AdapterFactory for VideoToolboxDecoderFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(crate::DECODER_ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "VideoToolbox H.264 decoder"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        // SAFETY: a plain query with a codec constant.
        let hardware = unsafe { VTIsHardwareDecodeSupported(kCMVideoCodecType_H264) };
        Ok(capabilities(hardware))
    }
}

impl VideoDecoderFactory for VideoToolboxDecoderFactory {
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError> {
        Ok(Box::new(self.create_session(config)?))
    }
}
