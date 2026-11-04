//! FFmpeg adapter for the RBS encoder port (`docs/specs/output-ports.md`): AAC-LC on
//! Linux through libavcodec's native AAC encoder (`aac`), loaded at run time
//! (ADR-0007: FFmpeg only as an optional, dynamically linked codec adapter).
//!
//! - Loading: `libavcodec.so.<major>` and the matching `libavutil.so.<major>` of FFmpeg
//!   6, 7 or 8 (libavcodec 60, 61, 62) through the dynamic loader, once per process.
//!   A missing library, symbol or encoder, or a major version this adapter has no
//!   layout for, makes the probe empty; it never panics.
//! - Input: `f32` blocks of any length, planar or interleaved, mono or stereo at
//!   44.1 or 48 kHz. Samples go into a pre-allocated planar 1024-frame staging area
//!   (the encoder's own sample format, `AV_SAMPLE_FMT_FLTP`, so nothing is converted);
//!   each full frame is copied into the session's one `AVFrame` and encoded
//!   synchronously on the calling thread (`threads = 1`: no threads, no callbacks).
//! - Output: raw AAC-LC access units (`BitstreamFormat::Raw`) with a two-byte
//!   `AudioSpecificConfig` as extradata. Each packet is 1024 samples; its timestamp is
//!   the first block's timestamp plus 1024 samples per packet (invariant 8). The
//!   encoder's delay (libavcodec's `initial_padding`, one frame) is not signalled, as
//!   the spec's open question on encoder delay describes.
//! - Bitrates: any rate from [`MIN_BITRATE`] to [`MAX_BITRATE_PER_CHANNEL`] per
//!   channel; fixed while running.
//! - A block whose timestamp is a codec frame or more away from the running sample
//!   timeline is a discontinuity: the partial frame is padded with silence, the encoder
//!   is drained, and the timeline restarts at the block (on a new encoder context,
//!   opened with its first frame, since libavcodec's AAC encoder cannot continue after
//!   end of stream).
//!
//! The timeline, staging and validation live in a platform-neutral session over an
//! encoder engine; the `testing` feature adds a fake engine so the port suite runs on
//! every platform. On other platforms the real factory compiles and probes to an empty
//! set.
//!
//! Licence: FFmpeg's libraries are LGPL-2.1-or-later (GPL when the distribution built
//! them with GPL parts); both are compatible with RBS (GPL-3.0-or-later). Nothing of
//! FFmpeg is built or shipped with RBS: the system's libraries are used.

#[cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    feature = "testing"
))]
mod session;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux;

#[cfg(feature = "testing")]
pub mod testing;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.codec.ffmpeg-aac";

/// Sample frames per AAC-LC packet.
pub const FRAME_SIZE: u32 = 1024;

/// Sample rates the adapter declares.
pub const SAMPLE_RATES: [u32; 2] = [44_100, 48_000];

/// Lowest bitrate a session accepts, in bit/s (for any channel count).
pub const MIN_BITRATE: u64 = 32_000;

/// Highest bitrate per channel a session accepts, in bit/s: 256 kbit/s mono,
/// 512 kbit/s stereo. AAC-LC allows 6144 bits per channel and frame (288 kbit/s per
/// channel at 48 kHz, 264.6 at 44.1 kHz); libavcodec would lower a higher rate
/// silently.
pub const MAX_BITRATE_PER_CHANNEL: u64 = 256_000;

/// Highest bitrate a stereo session accepts, in bit/s.
pub const MAX_BITRATE: u64 = 2 * MAX_BITRATE_PER_CHANNEL;

pub use platform::FfmpegAacFactory;

/// The concrete session type.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub use linux::FfmpegAacSession;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod platform {
    pub use crate::linux::FfmpegAacFactory;
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod platform {
    use rbs_codec::{AudioEncoder, AudioEncoderConfig, AudioEncoderFactory, CreateError};
    use rbs_core::capability::CapabilitySet;
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};

    /// Factory of the FFmpeg AAC adapter. Unavailable on this platform.
    #[derive(Debug, Default)]
    pub struct FfmpegAacFactory;

    impl FfmpegAacFactory {
        /// Creates the factory.
        pub fn new() -> Self {
            Self
        }
    }

    impl AdapterFactory for FfmpegAacFactory {
        fn id(&self) -> AdapterId {
            AdapterId::new(crate::ADAPTER_ID)
        }

        fn display_name(&self) -> &str {
            "FFmpeg AAC (unavailable on this platform)"
        }

        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            Ok(CapabilitySet::new())
        }
    }

    impl AudioEncoderFactory for FfmpegAacFactory {
        fn create(&self, _: &AudioEncoderConfig) -> Result<Box<dyn AudioEncoder>, CreateError> {
            Err(CreateError::Unsupported {
                what: "the FFmpeg AAC adapter is only built for x86_64 and aarch64 Linux".into(),
            })
        }
    }
}
