//! How a source's row names its kind and its faults (moved from `studio_ui.rs`).

use rbs_app::studio_host::SourceKind;
use rbs_core::fault::SourceFault;

/// The reason a source is unavailable, in plain words for its kind (ADR-0077).
pub(super) fn unavailable_text(fault: SourceFault, kind: Option<SourceKind>) -> &'static str {
    use SourceKind as K;
    match (fault, kind) {
        (SourceFault::TargetGone, Some(K::Window)) => "Window closed",
        (SourceFault::TargetGone, Some(K::Screen)) => "Display disconnected",
        (SourceFault::TargetGone, Some(K::Camera)) => "Camera unplugged",
        (SourceFault::TargetGone, Some(K::AudioInput)) => "Microphone not found",
        (SourceFault::TargetGone, Some(K::SystemAudio)) => "Audio output not found",
        (SourceFault::TargetGone, Some(K::ApplicationAudio)) => "Application not running",
        (SourceFault::TargetGone, _) => "Source not found",
        (SourceFault::PermissionDenied, Some(K::Camera)) => "Camera access denied",
        (SourceFault::PermissionDenied, Some(K::AudioInput)) => "Microphone access denied",
        (SourceFault::PermissionDenied, _) => "Access denied",
        (SourceFault::Busy, Some(K::Camera)) => "Camera in use by another app",
        (SourceFault::Busy, _) => "In use by another app",
        _ => "Not available",
    }
}

/// The icon a source's row shows for its kind (design system icon set).
pub(super) fn source_icon(kind: Option<SourceKind>) -> &'static str {
    match kind {
        Some(SourceKind::Screen) => "monitor",
        Some(SourceKind::Window) => "window",
        Some(SourceKind::Camera) => "camera",
        Some(SourceKind::AudioInput) => "mic",
        Some(SourceKind::SystemAudio | SourceKind::ApplicationAudio) => "speaker",
        Some(SourceKind::Image | SourceKind::Slideshow) => "image",
        Some(SourceKind::Color) => "palette",
        Some(SourceKind::Media) => "film",
        Some(SourceKind::Browser) => "globe",
        // Until the text UI (ADR-0129 phase (f)) brings its own icon.
        Some(SourceKind::Text) => "text",
        None => "window",
    }
}
