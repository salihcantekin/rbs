//! Live H.264 decoder for the RBS video decoder port (`docs/specs/backend-ports.md`,
//! "Live video decode", ADR-0139 phase 1c) over Microsoft's H.264 decoder MFT.
//!
//! - Input: the live case only: Annex B, one access unit per packet, SPS and PPS in
//!   band. Every SPS is checked with `rbs_codec::h264` before the MFT sees the bytes:
//!   malformed or above `max_size` (cropped size) is `InvalidInput`; outside the allow
//!   list (Constrained Baseline, Main, High, 4:2:0, 8-bit, level 5.2), interlaced, or
//!   above the declared 4096 x 2304 is `Unsupported`. NAL types 0, 13, 14, 15 and 20
//!   are `InvalidInput`. A refused packet that carries an SPS or a slice gates on the
//!   next keyframe. `LengthPrefixed` is refused at `create` (`Unsupported`).
//! - A packet without a slice (SEI, AUD) is dropped, or fed when it carries parameter
//!   sets; only packets with a slice wait for a picture.
//! - Low latency: `CODECAPI_AVLowLatencyMode` (no reordering delay); the pictures of a
//!   packet come out on the `submit` or `drain` that follows it.
//! - Stamps: Media Foundation counts 100 ns, so sample times carry only the submission
//!   index; the session keeps the submitted stamps in a bounded FIFO and gives each
//!   picture its packet's `pts` bit-exactly. A full FIFO (16 packets) is `Busy`.
//! - Output: with `DecodeOutput` accepting `d3d11` on a known `adapter_luid`, the MFT
//!   decodes with DXVA on a D3D11 device of that adapter and each picture is copied on
//!   the GPU into one of `min_surfaces` (at most 8) shared NV12 NT-handle textures,
//!   handed out `SurfaceSync::Ready` with a lease (see `pool`). Otherwise, or when the
//!   adapter or the MFT refuses DXVA, the MFT decodes in software and pictures are
//!   copied to system memory (NV12, a new buffer per picture, counted in `cpu_copies`;
//!   `max_surfaces()` is then 0). Both copies apply the SPS crop; colour comes from the
//!   VUI ([`color_of`]), BT.709 limited when absent.
//! - An in-band SPS of another size within `max_size` is followed (the MFT's stream
//!   change sets a new output type, pool slots are remade at the new size);
//!   `output_format` changes when the packet with the new SPS is submitted. `submit`
//!   never returns `FormatChanged`. A removed, reset or hung device is `DeviceLost`;
//!   an MFT refusal of the stream (`MF_E_UNSUPPORTED_FORMAT`, `MF_E_INVALIDMEDIATYPE`,
//!   `E_NOTIMPL` and the like from a decode call) is `Unsupported`.
//!
//! The factory does nothing until `create`; `probe` creates the MFT once, without a
//! device, and caches the answer.

#[cfg(windows)]
pub(crate) mod mft;
#[cfg(windows)]
mod pool;
#[cfg(windows)]
mod session;

#[cfg(windows)]
pub use session::MfH264Decoder;

use rbs_codec::caps;
use rbs_codec::h264::{Sps, SpsColor};
use rbs_codec::{CreateError, VideoDecoder, VideoDecoderConfig, VideoDecoderFactory};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{
    ColorPrimaries, ColorRange, ColorSpace, MatrixCoefficients, Size, TransferFunction,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::Timestamp;

/// Largest picture the decoder declares (`codec.video.decode.width`/`.height`).
pub const MAX_DECODE_SIZE: Size = Size::new(4096, 2304);

/// The output colour of a stream: the VUI's matrix, primaries and range (8-bit SDR,
/// so the transfer is BT.709), BT.709 limited when the SPS states none.
pub fn color_of(color: Option<SpsColor>) -> ColorSpace {
    color.map_or(ColorSpace::BT709_LIMITED, |c| {
        color_from(c.primaries, c.matrix, c.full_range)
    })
}

/// [`color_of`] from the VUI code points (`colour_primaries`, `matrix_coefficients`,
/// `video_full_range_flag`).
pub fn color_from(primaries: u8, matrix: u8, full_range: bool) -> ColorSpace {
    let matrix = match matrix {
        5 | 6 => MatrixCoefficients::Bt601,
        9 => MatrixCoefficients::Bt2020Ncl,
        _ => MatrixCoefficients::Bt709,
    };
    let primaries = match primaries {
        5 => ColorPrimaries::Bt601Pal,
        6 | 7 => ColorPrimaries::Bt601Ntsc,
        9 => ColorPrimaries::Bt2020,
        _ => ColorPrimaries::Bt709,
    };
    let range = if full_range {
        ColorRange::Full
    } else {
        ColorRange::Limited
    };
    ColorSpace::new(primaries, TransferFunction::Bt709, matrix, range)
}

/// Where one picture lies in the decoded frame, and what it is stamped with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) pts: Timestamp,
    /// Cropped size.
    pub(crate) size: Size,
    /// Crop origin in the coded frame.
    pub(crate) left: u32,
    pub(crate) top: u32,
    pub(crate) color: ColorSpace,
}

impl Placement {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn of(sps: &Sps, pts: Timestamp) -> Self {
        Self {
            pts,
            size: sps.size,
            left: sps.crop_left,
            top: sps.crop_top,
            color: color_of(sps.color),
        }
    }

    /// True when the picture is NV12-aligned (even origin and size) and lies inside a
    /// decoded frame of `width` x `height`.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn fits(&self, width: u32, height: u32) -> bool {
        let even = |v: u32| v.is_multiple_of(2);
        even(self.left)
            && even(self.top)
            && even(self.size.width)
            && even(self.size.height)
            && u64::from(self.left) + u64::from(self.size.width) <= u64::from(width)
            && u64::from(self.top) + u64::from(self.size.height) <= u64::from(height)
    }
}

/// Adapter id of the decoder.
pub const DECODER_ADAPTER_ID: &str = "org.rbs.codec.mf-h264-decode";

/// Factory of Media Foundation live H.264 decoder sessions.
#[derive(Debug, Default)]
pub struct MfH264DecoderFactory;

impl MfH264DecoderFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }

    /// Creates a session with its concrete type.
    #[cfg(windows)]
    pub fn create_session(
        &self,
        config: &VideoDecoderConfig,
    ) -> Result<MfH264Decoder, CreateError> {
        MfH264Decoder::open(config)
    }
}

fn capabilities() -> CapabilitySet {
    let set = |v: &[&str]| CapabilityValue::Set(v.iter().map(|s| (*s).to_string()).collect());
    CapabilitySet::new()
        .with(caps::VIDEO_DECODE, set(&["h264"]))
        .with(
            caps::VIDEO_DECODE_PROFILES,
            set(&["h264.baseline", "h264.main", "h264.high"]),
        )
        .with(
            caps::VIDEO_DECODE_WIDTH,
            CapabilityValue::IntRange {
                min: 16,
                max: i64::from(MAX_DECODE_SIZE.width),
            },
        )
        .with(
            caps::VIDEO_DECODE_HEIGHT,
            CapabilityValue::IntRange {
                min: 16,
                max: i64::from(MAX_DECODE_SIZE.height),
            },
        )
        .with(caps::VIDEO_DECODE_FPS_MAX, CapabilityValue::Int(60))
        .with(caps::DECODE_OUTPUT_MEMORY, set(&["system", "d3d11"]))
}

impl AdapterFactory for MfH264DecoderFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(DECODER_ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "Media Foundation H.264 decoder"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        #[cfg(windows)]
        {
            // N editions without the Media Feature Pack have no decoder. The answer
            // cannot change while the process runs.
            static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            let available = *AVAILABLE.get_or_init(|| mft::create().is_ok());
            Ok(if available {
                capabilities()
            } else {
                CapabilitySet::new()
            })
        }
        #[cfg(not(windows))]
        {
            let _ = capabilities;
            Ok(CapabilitySet::new())
        }
    }
}

impl VideoDecoderFactory for MfH264DecoderFactory {
    fn create(&self, config: &VideoDecoderConfig) -> Result<Box<dyn VideoDecoder>, CreateError> {
        #[cfg(windows)]
        {
            Ok(Box::new(self.create_session(config)?))
        }
        #[cfg(not(windows))]
        {
            let _ = config;
            Err(CreateError::Unsupported {
                what: "Media Foundation is only available on Windows".into(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_comes_from_the_vui_with_a_bt709_limited_default() {
        assert_eq!(color_of(None), ColorSpace::BT709_LIMITED);
        assert_eq!(color_from(1, 1, false), ColorSpace::BT709_LIMITED);
        let c = color_from(6, 6, true);
        assert_eq!(c.matrix, MatrixCoefficients::Bt601);
        assert_eq!(c.primaries, ColorPrimaries::Bt601Ntsc);
        assert_eq!(c.range, ColorRange::Full);
        assert_eq!(
            color_from(9, 9, false).matrix,
            MatrixCoefficients::Bt2020Ncl
        );
        assert_eq!(color_from(2, 2, false), ColorSpace::BT709_LIMITED); // unspecified
    }

    #[test]
    fn a_placement_fits_only_inside_the_frame_and_nv12_aligned() {
        let at = |left, top, w, h| Placement {
            pts: Timestamp::ZERO,
            size: Size::new(w, h),
            left,
            top,
            color: ColorSpace::BT709_LIMITED,
        };
        assert!(at(0, 0, 1920, 1080).fits(1920, 1088));
        assert!(at(8, 4, 1904, 1080).fits(1920, 1088));
        assert!(!at(8, 4, 1920, 1080).fits(1920, 1088));
        assert!(!at(1, 0, 16, 16).fits(64, 64));
        assert!(!at(0, 0, 15, 16).fits(64, 64));
        assert!(!at(u32::MAX - 1, 0, 2, 2).fits(64, 64));
    }
}
