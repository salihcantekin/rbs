//! Live H.264 decoder for the RBS video decoder port (`docs/specs/backend-ports.md`,
//! "Live video decode", ADR-0139 phase 1c) over Microsoft's H.264 decoder MFT.
//!
//! - Input: the live case only: Annex B, one access unit per packet, SPS and PPS in
//!   band. Every SPS is checked with `rbs_codec::h264` (allow list, cropped size against
//!   `max_size`) before the MFT sees the bytes. `LengthPrefixed` is refused at `create`
//!   (`Unsupported`), so the engine picks another adapter.
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
//!   copied to system memory (NV12, counted in `cpu_copies`). Colour is BT.709 limited
//!   (the shared SPS parser does not read the VUI yet).
//! - An in-band SPS of another size within `max_size` is followed (the MFT's stream
//!   change sets a new output type, pool slots are remade at the new size). `submit`
//!   never returns `FormatChanged`.
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
use rbs_codec::{CreateError, VideoDecoder, VideoDecoderConfig, VideoDecoderFactory};
use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

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
            CapabilityValue::IntRange { min: 16, max: 4096 },
        )
        .with(
            caps::VIDEO_DECODE_HEIGHT,
            CapabilityValue::IntRange { min: 16, max: 2304 },
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
