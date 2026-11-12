//! Capability keys of the media-file decode port (ADR-0108 decision 4).

use rbs_core::capability::CapabilityKey;

/// Pixel path: a set containing `zero-copy`, `cpu-upload`, or both.
/// The host prefers `zero-copy`.
pub const PIXEL: CapabilityKey = CapabilityKey::new_static("media.decode.pixel");
