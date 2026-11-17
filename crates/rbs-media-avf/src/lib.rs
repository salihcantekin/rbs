//! AVFoundation adapter for the RBS media-decode port
//! (`docs/specs/media-decode-port.md`, ADR-0108): local video files on macOS.
//!
//! # How a file is read
//!
//! - `open` creates an `AVURLAsset` and an `AVAssetReader` with one track output for
//!   the first video track, asking for `32BGRA`; VideoToolbox decodes in hardware.
//!   The stated length is the asset's duration.
//! - Sound has its own `AVAssetReader` with one track output for the first audio track
//!   (interleaved 32-bit float PCM at the track's own rate), created on the first
//!   `next_sound`. The streams are read separately (spec invariant 2), and a source
//!   that is never heard starts no second reader (P13).
//! - A reader cannot move once it reads, so `seek` replaces the readers with ones whose
//!   time range starts at the cursor.
//! - Each picture is copied once from the pixel buffer into a new `Rgba8` buffer.
//!
//! The capability is `cpu-upload`: the measured fallback of ADR-0108 decision 4. The
//! zero-copy path (the decoder's `IOSurface` as a native picture) is a later step that
//! needs the picture node to import native surfaces first.
//!
//! On other platforms the factory compiles and probes as unavailable, so a composition
//! root can register it unconditionally.

// The platform-neutral half is only called by the macOS backend and the tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::Path;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_media_decode::{DecodeError, MediaDecoder, MediaDecoderFactory, caps, pixel};

#[cfg(target_os = "macos")]
mod mac;

/// Adapter id.
pub const ADAPTER_ID: &str = "org.rbs.media-decode.avf";

/// The factory. Holds nothing.
#[derive(Debug, Default)]
pub struct AvfMediaFactory;

impl AvfMediaFactory {
    /// Creates the factory. Does nothing until used.
    pub fn new() -> Self {
        Self
    }
}

impl AdapterFactory for AvfMediaFactory {
    fn id(&self) -> AdapterId {
        AdapterId::new(ADAPTER_ID)
    }

    fn display_name(&self) -> &str {
        "AVFoundation files"
    }

    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        if cfg!(target_os = "macos") {
            Ok(CapabilitySet::new().with(
                caps::PIXEL,
                CapabilityValue::Set(vec![pixel::CPU_UPLOAD.to_owned()]),
            ))
        } else {
            Err(ProbeError::new("AVFoundation needs macOS"))
        }
    }
}

impl MediaDecoderFactory for AvfMediaFactory {
    #[cfg(target_os = "macos")]
    fn open(&self, path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        Ok(Box::new(mac::AvfDecoder::open(path)?))
    }

    #[cfg(not(target_os = "macos"))]
    fn open(&self, _path: &Path) -> Result<Box<dyn MediaDecoder>, DecodeError> {
        Err(DecodeError::Unsupported("AVFoundation needs macOS".into()))
    }
}

/// A `CMTime` (`value` / `timescale`) as a duration; `None` when invalid or negative.
fn cm_duration(valid: bool, value: i64, timescale: i32) -> Option<Duration> {
    if !valid || timescale <= 0 {
        return None;
    }
    let value = u64::try_from(value).ok()?;
    let nanos = u128::from(value) * 1_000_000_000 / timescale as u128;
    Some(Duration::from_nanos(u64::try_from(nanos).ok()?))
}

/// The nominal frame duration of a track's stated frame rate.
fn nominal_duration(rate: f32) -> Option<Duration> {
    (rate.is_finite() && rate > 0.0).then(|| Duration::from_secs_f64(1.0 / f64::from(rate)))
}

/// Copies a `width` x `height` `B, G, R, A` picture with `stride` bytes per row into
/// `out` as `R, G, B, A`. Returns `None` when a row lies outside `src`.
fn bgra_to_rgba(
    src: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) -> Option<()> {
    out.clear();
    out.reserve(width * height * 4);
    for row in 0..height {
        let start = row.checked_mul(stride)?;
        let bytes = src.get(start..start.checked_add(width * 4)?)?;
        for px in bytes.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Some(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cm_time_converts_and_refuses_invalid_values() {
        assert_eq!(cm_duration(true, 3, 30), Some(Duration::from_millis(100)));
        assert_eq!(cm_duration(false, 3, 30), None);
        assert_eq!(cm_duration(true, -1, 30), None);
        assert_eq!(cm_duration(true, 1, 0), None);
    }

    #[test]
    fn the_nominal_duration_comes_from_the_rate() {
        assert_eq!(nominal_duration(10.0), Some(Duration::from_millis(100)));
        assert_eq!(nominal_duration(0.0), None);
        assert_eq!(nominal_duration(f32::NAN), None);
    }

    #[test]
    fn rows_with_padding_are_swizzled() {
        // 1x2 picture with an 8-byte stride.
        let src = [1, 2, 3, 4, 0, 0, 0, 0, 10, 20, 30, 40];
        let mut out = Vec::new();
        bgra_to_rgba(&src, 8, 1, 2, &mut out).unwrap();
        assert_eq!(out, vec![3, 2, 1, 4, 30, 20, 10, 40]);
        assert!(bgra_to_rgba(&src, 8, 2, 2, &mut out).is_none());
    }

    #[test]
    fn the_factory_probes_by_platform() {
        assert_eq!(
            AvfMediaFactory::new().probe().is_ok(),
            cfg!(target_os = "macos")
        );
    }
}
