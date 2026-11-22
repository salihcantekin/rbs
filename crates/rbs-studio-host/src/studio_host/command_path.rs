//! Which path each window command takes to the studio (ADR-0163 decision 4): every
//! [`Command`] variant is classified here, so a new variant does not build until it is.
//! The rows' notes are the known differences of
//! `docs/research/command-path-inventory-2026-11.md`; each namespace step of the ADR
//! checks its rows again, and this table is the record.

use super::*;

/// How a window command reaches the studio (ADR-0163 decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandPath {
    /// [`Command::Api`]: a control API command, run as a remote call would be.
    Api,
    /// The handler only builds the listed API methods from control-thread state and
    /// runs them as [`Command::Api`] would.
    Translates(&'static [&'static str]),
    /// Window only, for the reason given.
    WindowOnly(WindowReason),
    /// The listed API methods do what the handler does; the variant has not moved yet.
    Pending(&'static [&'static str]),
    /// The listed API methods need a protocol change first (a `gesture`, a request
    /// parameter) to do what the window does.
    Contract(&'static [&'static str]),
    /// No API method, or no `settings.set` field, yet.
    Missing,
}

/// Why a window command stays off the control API (ADR-0163 decisions 4 and 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowReason {
    /// Opens, closes or feeds a view of this window; changes no collection, history or
    /// published state.
    View,
    /// Needs the person or the operating system at this computer: stream keys, the
    /// virtual camera's setup, permission prompts.
    LocalOnly,
    /// A value shown on the running studio while a slider is dragged, and the stored
    /// value put back; never saved, never on the history.
    Preview,
    /// The window saved a settings file it owns, or loaded a collection, and tells the
    /// host; the API changes the same setting through `settings.set` or
    /// `collection.switch`.
    Mirror,
}

/// One row per variant: the exhaustive `match` of [`Command::path`], the variant's
/// name and the table the U20 test counts.
macro_rules! paths {
    ($($variant:ident => $path:expr,)*) => {
        impl Command {
            /// How this command reaches the studio (ADR-0163).
            pub fn path(&self) -> CommandPath {
                use CommandPath::*;
                use WindowReason::*;
                match self {
                    $(Command::$variant { .. } => $path,)*
                }
            }

            /// The variant's name, as in [`Command::paths`].
            pub fn name(&self) -> &'static str {
                match self {
                    $(Command::$variant { .. } => stringify!($variant),)*
                }
            }

            /// Every variant's name and path, in declaration order.
            pub fn paths() -> &'static [(&'static str, CommandPath)] {
                use CommandPath::*;
                use WindowReason::*;
                const PATHS: &[(&str, CommandPath)] = &[$((stringify!($variant), $path),)*];
                PATHS
            }
        }
    };
}

paths! {
    // with_audio: true; split as AddSource
    // split: the OS permission prompt and system picker stay in the window (LocalOnly)
    // must still send Update::Deleted for the undo toast
    // global: true
    // quiet when there is nothing to undo
    // as Undo
    SetSourceVolume => Translates(&["source.set_volume", "scene.audio.set"]),
    SetSourceMuted => Translates(&["source.set_muted", "scene.audio.set"]),
    SetMetersUi => WindowOnly(View),
    EnsureEncoders => WindowOnly(View),
    SetSceneOwnLevels => Translates(&["scene.audio.own", "scene.audio.shared"]),
    ResetSceneLevel => Translates(&["scene.audio.reset_source"]),
    AutoLayoutVertical => Translates(&["canvas.auto_layout"]),
    SetVerticalSize => Translates(&["canvas.set_vertical_size"]),
    SetMasterVolume => Translates(&["canvas.set_master_volume"]),
    SetMasterMuted => Translates(&["canvas.set_master_muted"]),
    SetLoudnessShown => Translates(&["loudness.set_enabled"]),
    SetLoudnessSettings => Translates(&["settings.set"]),
    SetSourceMixerHidden => Translates(&["source.set_mixer_hidden"]),
    ApplyLook => Translates(&["look.apply"]),
    AddSample => Translates(&["source.add_sample"]),
    PreviewFilter => WindowOnly(Preview),
    RestoreFilter => WindowOnly(Preview),
    OpenSyncMeasure => WindowOnly(View),
    // cancels the measurement of the view's source, then drops the view (ADR-0168)
    CloseSyncMeasure => Translates(&["sync.measure_cancel"]),
    GrabThumbnails => WindowOnly(View),
    StopThumbnails => WindowOnly(View),
    OpenSourceSettings => WindowOnly(View),
    CloseSourceSettings => WindowOnly(View),
    // the page's state picks the call (ADR-0168)
    BrowserControl => Translates(&["browser.reload", "source.reload", "browser.run_script"]),
    // window stops the page first, API does not
    // window forces the next media-state publish
    OpenAttributes => WindowOnly(View),
    // becomes Translates (branches on the preview)
    SwitchScene => Translates(&["scene.switch", "canvas.set_preview"]),
    SetStudioMode => Translates(&["canvas.set_preview"]),
    SetSwapAfterTransition => WindowOnly(Mirror),
    SetVerticalProfile => WindowOnly(Mirror),
    SetVerticalStreamProfile => WindowOnly(Mirror),
    // servers and key flags are the local user's; on/off is stream.destination.set_enabled
    SetStreamDestinations => WindowOnly(LocalOnly),
    SetSeparateFiles => WindowOnly(Mirror),
    SetVerticalPicture => WindowOnly(View),
    OpenProjector => WindowOnly(View),
    CloseProjector => WindowOnly(View),
    OpenInteract => WindowOnly(View),
    CloseInteract => WindowOnly(View),
    // quiet answer (input to a closed page is ignored today)
    SetProfile => WindowOnly(Mirror),
    SetVirtualCamera => WindowOnly(LocalOnly),
    SetReplay => WindowOnly(Mirror),
    AddMarker => Translates(&["marker.add"]),
    RemoveMarkers => Translates(&["marker.remove"]),
    // own error sentence around the refusal
    PauseRecording => Translates(&["record.pause_toggle"]),
    // own error sentence around the refusal
    HoldStream => Translates(&["stream.hold_toggle"]),
    // own error sentence around the refusal
    Break => Translates(&["output.break_toggle"]),
    SetStreamProfile => WindowOnly(Mirror),
    SaveStreamKey => WindowOnly(LocalOnly),
    SetKeyAccount => WindowOnly(LocalOnly),
    SaveVerticalStreamKey => WindowOnly(LocalOnly),
    SaveDestinationKey => WindowOnly(LocalOnly),
    StartDestination => Translates(&["stream.destination.start"]),
    StopDestination => Translates(&["stream.destination.stop"]),
    RemoveStreamKey => WindowOnly(LocalOnly),
    SaveImportedKey => WindowOnly(LocalOnly),
    SwitchCollection => WindowOnly(Mirror),
    // no settings.set field
    SetLibrary => Missing,
    ReadPermissions => WindowOnly(LocalOnly),
    PickSource => WindowOnly(LocalOnly),
    RequestPermission => WindowOnly(LocalOnly),
    Api => Api,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_api::request::METHODS;

    /// ADR-0163 decision 5: these counts may only fall. A step that moves a handler
    /// which must pick its method from host state may raise `TRANSLATES` by as many
    /// as `PENDING` falls; any other rise needs the owner's written approval.
    /// `LOCAL_ONLY` 9 to 10 and `MIRROR` 3 to 8 are approved by ADR-0164 decision 9
    /// (owner, 2026-11-21). `TRANSLATES` 22 to 24 and `LOCAL_ONLY` 10 to 11 are
    /// approved by ADR-0168 decision 4 (owner, 2026-11-22).
    const TRANSLATES: usize = 24;
    const PENDING: usize = 0;
    const CONTRACT: usize = 0;
    const MISSING: usize = 1;
    const VIEW: usize = 13;
    const LOCAL_ONLY: usize = 11;
    const PREVIEW: usize = 2;
    const MIRROR: usize = 8;

    fn count(f: impl Fn(&CommandPath) -> bool) -> usize {
        Command::paths().iter().filter(|(_, p)| f(p)).count()
    }

    #[test]
    fn the_window_only_commands_do_not_grow() {
        let counts = [
            (
                "Translates",
                count(|p| matches!(p, CommandPath::Translates(_))),
                TRANSLATES,
            ),
            (
                "Pending",
                count(|p| matches!(p, CommandPath::Pending(_))),
                PENDING,
            ),
            (
                "Contract",
                count(|p| matches!(p, CommandPath::Contract(_))),
                CONTRACT,
            ),
            (
                "Missing",
                count(|p| matches!(p, CommandPath::Missing)),
                MISSING,
            ),
            (
                "View",
                count(|p| *p == CommandPath::WindowOnly(WindowReason::View)),
                VIEW,
            ),
            (
                "LocalOnly",
                count(|p| *p == CommandPath::WindowOnly(WindowReason::LocalOnly)),
                LOCAL_ONLY,
            ),
            (
                "Preview",
                count(|p| *p == CommandPath::WindowOnly(WindowReason::Preview)),
                PREVIEW,
            ),
            (
                "Mirror",
                count(|p| *p == CommandPath::WindowOnly(WindowReason::Mirror)),
                MIRROR,
            ),
        ];
        for (what, now, most) in counts {
            assert!(
                now <= most,
                "{now} {what} commands, at most {most} (ADR-0163)"
            );
        }
        assert_eq!(count(|p| *p == CommandPath::Api), 1);
    }

    #[test]
    fn named_methods_are_control_api_methods() {
        for (name, path) in Command::paths() {
            let methods = match path {
                CommandPath::Translates(m) | CommandPath::Pending(m) | CommandPath::Contract(m) => {
                    *m
                }
                _ => continue,
            };
            assert!(!methods.is_empty(), "{name} names no method");
            for m in methods {
                assert!(
                    METHODS.iter().any(|i| i.name == *m),
                    "{name} names {m}, which is not a control API method"
                );
            }
        }
    }

    #[test]
    fn every_variant_has_one_row() {
        let names: Vec<_> = Command::paths().iter().map(|(n, _)| *n).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        assert_eq!(Command::Break.name(), "Break");
        assert_eq!(
            Command::Break.path(),
            CommandPath::Translates(&["output.break_toggle"])
        );
    }
}
