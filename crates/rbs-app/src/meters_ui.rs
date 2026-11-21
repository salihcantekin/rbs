//! Whether the UI reads the mixer's meters, told to the control thread (F-15 of the
//! 2026-11 resource audit; spec `docs/specs/audio-graph.md`, "Mixer").
//!
//! The UI reads them while a Mixer is shown in a window that is not hidden. With
//! every window hidden (Keep RBS running, ADR-0056) nothing reads them, so the mixer
//! drops them unless a control client samples `audio.levels`.

use crate::studio_ui::StudioUi;
use rbs_app::studio_host::Command;

/// Sends `shown` when it differs from `sent`, the value last sent.
pub(crate) fn send(studio: &StudioUi, sent: &mut bool, shown: bool) {
    if shown != *sent {
        studio.send(Command::SetMetersUi(shown));
        *sent = shown;
    }
}
