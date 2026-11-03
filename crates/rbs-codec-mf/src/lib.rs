//! Media Foundation adapter for the RBS encoder port (`docs/specs/output-ports.md`):
//! AAC-LC on Windows through Microsoft's AAC encoder MFT (`CLSID_AACMFTEncoder`).
//!
//! - Input: `f32` blocks of any length, planar or interleaved, mono or stereo at
//!   44.1 or 48 kHz. Samples are converted to 16-bit PCM (the only input the
//!   Microsoft encoder takes) into a pre-allocated interleaved 1024-frame staging
//!   area; each full frame is handed to the MFT synchronously on the calling thread
//!   (no threads of our own, no callbacks).
//! - Output: raw AAC-LC access units (`BitstreamFormat::Raw`) with a two-byte
//!   `AudioSpecificConfig` as extradata. Each packet is 1024 samples; its timestamp is
//!   the first block's timestamp plus 1024 samples per packet (invariant 8). The
//!   encoder's delay is not signalled, as the spec's open question on encoder delay
//!   describes.
//! - Bitrates: the Microsoft encoder accepts exactly 96, 128, 160 and 192 kbit/s, so
//!   the session refuses every other rate and cannot change it while running.
//! - A block whose timestamp is a codec frame or more away from the running sample
//!   timeline is a discontinuity: the partial frame is padded with silence, the MFT is
//!   drained, and the timeline restarts at the block.
//!
//! The timeline, staging and validation live in a platform-neutral session over an
//! encoder engine; the `testing` feature adds a fake engine so the port suite runs on
//! every platform. On other platforms the real factory compiles and probes to an empty
//! set.
//!
//! Licence: Media Foundation is part of Windows; the bindings are MIT OR Apache-2.0.

#[cfg(any(windows, feature = "testing"))]
mod session;

#[cfg(windows)]
mod win;

#[cfg(feature = "testing")]
pub mod testing;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.codec.mf-aac";

/// Sample frames per AAC-LC packet.
pub const FRAME_SIZE: u32 = 1024;

/// Sample rates the adapter declares.
pub const SAMPLE_RATES: [u32; 2] = [44_100, 48_000];

/// Bitrates the Microsoft AAC encoder accepts (`MF_MT_AUDIO_AVG_BYTES_PER_SECOND` of
/// 12000, 16000, 20000 or 24000), for every channel count.
pub const BITRATES: [u64; 4] = [96_000, 128_000, 160_000, 192_000];

pub use platform::MfAacFactory;

/// The concrete session type.
#[cfg(windows)]
pub use win::MfAacSession;

#[cfg(windows)]
mod platform {
    pub use crate::win::MfAacFactory;
}

#[cfg(not(windows))]
mod platform {
    use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the Media Foundation AAC adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct MfAacFactory;

    impl MfAacFactory {
        /// Creates the factory.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for MfAacFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "Media Foundation AAC (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl AudioEncoderFactory for MfAacFactory {
        fn create(&self, _: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "Media Foundation is only available on Windows".into(),
            })
        }
    }
}
