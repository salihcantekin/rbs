//! AudioToolbox adapter for the RBS encoder port (`docs/specs/output-ports.md`):
//! AAC-LC on macOS through Apple's `AudioConverter`.
//!
//! - Input: `f32` blocks of any length, planar or interleaved, mono or stereo at
//!   44.1 or 48 kHz. Samples are buffered into a pre-allocated 1024-frame staging
//!   area; each full frame is handed to the converter synchronously on the calling
//!   thread (no threads of our own, no callbacks on other threads).
//! - Output: raw AAC-LC access units (`BitstreamFormat::Raw`) with a two-byte
//!   `AudioSpecificConfig` as extradata. Each packet is 1024 samples; its timestamp is
//!   the first block's timestamp plus 1024 samples per packet (invariant 8). The
//!   encoder's priming delay (`AudioConverterPrimeInfo::leadingFrames`, 2112 samples
//!   for Apple's encoder) is not signalled yet: the audio is late by that delay, as
//!   the spec's open question on encoder delay describes. `AacSession::priming`
//!   reports it.
//! - A block whose timestamp is a codec frame or more away from the running sample
//!   timeline is a discontinuity: the partial frame is padded with silence, the
//!   converter is drained and reset, and the timeline restarts at the block.
//!
//! On other platforms the factory compiles and probes to an empty set.
//!
//! Licence: AudioToolbox is part of macOS; the bindings are MIT OR Apache-2.0 OR Zlib.

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.codec.audiotoolbox";

/// Sample frames per AAC-LC packet.
pub const FRAME_SIZE: u32 = 1024;

/// Sample rates the adapter declares.
pub const SAMPLE_RATES: [u32; 2] = [44_100, 48_000];

#[cfg(target_os = "macos")]
mod mac;

pub use platform::AacFactory;

/// The concrete session type.
#[cfg(target_os = "macos")]
pub use mac::AacSession;

#[cfg(target_os = "macos")]
mod platform {
    pub use crate::mac::AacFactory;
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the AudioToolbox AAC adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct AacFactory;

    impl AacFactory {
        /// Creates the factory.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for AacFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "AudioToolbox AAC (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl AudioEncoderFactory for AacFactory {
        fn create(&self, _: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "AudioToolbox is only available on macOS".into(),
            })
        }
    }
}
