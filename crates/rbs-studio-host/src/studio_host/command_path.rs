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
    ListTargets => Pending(&["source.targets"]),
    // with_audio: true; split as AddSource
    AddSourceWithAudio => Pending(&["source.add"]),
    // split: the OS permission prompt and system picker stay in the window (LocalOnly)
    AddSource => Pending(&["source.add"]),
    // must still send Update::Deleted for the undo toast
    RemoveFromScene => Pending(&["item.remove"]),
    // global: true
    AddGlobalSource => Pending(&["source.add"]),
    // edit.undo has no only-if-a-delete condition
    UndoDelete => Missing,
    // quiet when there is nothing to undo
    Undo => Pending(&["edit.undo"]),
    // as Undo
    Redo => Pending(&["edit.redo"]),
    // scene.move has no target scene
    MoveSceneTo => Missing,
    // item.move has no target item
    MoveSourceTo => Missing,
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
    // no voice chain method
    AddVoiceChain => Missing,
    ApplyLook => Translates(&["look.apply"]),
    AddSample => Translates(&["source.add_sample"]),
    // no shape source in source.add
    AddStamp => Missing,
    // source.add has no file path, and the box is sized from the image
    AddImage => Missing,
    // source.add has no file path
    AddMedia => Missing,
    // no stinger file method
    SetStingerFile => Missing,
    // source.add has no URL
    AddBrowser => Missing,
    AddText => Pending(&["source.add"]),
    // no fit-box method
    FitTextBox => Missing,
    ListTextFonts => Pending(&["text.fonts"]),
    TextTimer => Pending(&["text.timer"]),
    SetSourceFill => Pending(&["source.set_settings"]),
    // also adds a border to the item style in one undo step; no method
    SetSourceFilled => Missing,
    // window then restores stored values on an unchanged release
    SetFilterValues => Pending(&["filter.set_settings"]),
    PreviewFilter => WindowOnly(Preview),
    RestoreFilter => WindowOnly(Preview),
    OpenSyncMeasure => WindowOnly(View),
    // refusals go to the Measure view, needs a ticketed answer
    StartSyncMeasure => Pending(&["sync.measure_start"]),
    // as StartSyncMeasure
    CloseSyncMeasure => Pending(&["sync.measure_cancel"]),
    ApplySyncToDeviceSources => Pending(&["sync.apply_to_device_sources"]),
    RetargetSource => Pending(&["source.retarget"]),
    GrabThumbnails => WindowOnly(View),
    StopThumbnails => WindowOnly(View),
    OpenSourceSettings => WindowOnly(View),
    CloseSourceSettings => WindowOnly(View),
    // source.set_settings needs gesture; its text-file refusal moves to the caller checks
    SetSourceSettings => Contract(&["source.set_settings"]),
    BrowserControl => Pending(&["browser.reload", "browser.run_script"]),
    // window stops the page first, API does not
    ForgetBrowserSignIn => Pending(&["browser.forget_sign_in"]),
    // window forces the next media-state publish
    Media => Pending(&["media.play", "media.pause", "media.seek", "media.restart"]),
    SetTransition => Pending(&["collection.set_transition"]),
    SetSourceActivity => Pending(&["source.set_activity"]),
    OpenAttributes => WindowOnly(View),
    // becomes Translates (branches on the preview)
    SwitchScene => Translates(&["scene.switch", "canvas.set_preview"]),
    SetStudioMode => Translates(&["canvas.set_preview"]),
    // starts a rehearsal on the engine; no method (ui.transition.rehearse is the registry entry)
    Rehearse => Missing,
    // no settings.set field
    SetSwapAfterTransition => Missing,
    SetSourceBox => Pending(&["item.set_box"]),
    // item.set_crop needs gesture
    SetSourceCrop => Contract(&["item.set_crop"]),
    // item.flip/rotate/fit/align/reset_transform need gesture
    TransformSource => Contract(&["item.flip", "item.rotate", "item.fit", "item.align", "item.reset_transform"]),
    // no method for the shown region (ADR-0058 Show buttons); source.set_region is the capture region
    SetSourceRegion => Missing,
    SetRecordSeparately => Pending(&["source.set_record_separately"]),
    // item.set_style needs gesture
    SetSourceStyle => Contract(&["item.set_style"]),
    // item.set_scroll needs gesture
    SetSourceScroll => Contract(&["item.set_scroll"]),
    ScrollLive => Pending(&["item.scroll_pause", "item.scroll_resume", "item.scroll_restart", "item.scroll_speed"]),
    // record.start builds its request from host state; the window sends its own RecordRequest
    StartRecording => Contract(&["record.start"]),
    StopRecording => Pending(&["record.stop"]),
    // as StartRecording
    StartVerticalRecording => Contract(&["record.start"]),
    StopVerticalRecording => Pending(&["record.stop"]),
    // as StartStream
    StartVerticalStream => Contract(&["stream.start"]),
    EndVerticalStream => Pending(&["stream.stop"]),
    // no settings.set field
    SetVerticalProfile => Missing,
    // no settings.set field
    SetVerticalStreamProfile => Missing,
    // no settings.set field
    SetStreamDestinations => Missing,
    SetVerticalFlags => Pending(&["output.set_with_vertical"]),
    // no settings.set field
    SetSeparateFiles => Missing,
    SetVerticalPicture => WindowOnly(View),
    OpenProjector => WindowOnly(View),
    CloseProjector => WindowOnly(View),
    OpenInteract => WindowOnly(View),
    CloseInteract => WindowOnly(View),
    // quiet answer (input to a closed page is ignored today)
    InteractInput => Pending(&["browser.input"]),
    // canvas
    SetCanvas => Pending(&["settings.set"]),
    SetProfile => WindowOnly(Mirror),
    // stream.start takes no request; the window sends its own StreamRequest
    StartStream => Contract(&["stream.start"]),
    EndStream => Pending(&["stream.stop"]),
    // vcam.start takes no size or rate
    StartVirtualCamera => Contract(&["vcam.start"]),
    StopVirtualCamera => Pending(&["vcam.stop"]),
    SetVirtualCamera => WindowOnly(LocalOnly),
    // no settings.set field
    SetReplay => Missing,
    ArmReplay => Pending(&["replay.arm"]),
    DisarmReplay => Pending(&["replay.disarm"]),
    SaveReplay => Pending(&["replay.save"]),
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
    RequestPermission => WindowOnly(LocalOnly),
    ListAddOns => Pending(&["addon.list"]),
    AddOn => Pending(&["addon.install", "addon.cancel", "addon.clean", "addon.remove"]),
    Api => Api,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_api::request::METHODS;

    /// ADR-0163 decision 5: these counts may only fall. A step that moves a handler
    /// which must pick its method from host state may raise `TRANSLATES` by as many
    /// as `PENDING` falls; any other rise needs the owner's written approval.
    const TRANSLATES: usize = 22;
    const PENDING: usize = 37;
    const CONTRACT: usize = 10;
    const MISSING: usize = 20;
    const VIEW: usize = 13;
    const LOCAL_ONLY: usize = 9;
    const PREVIEW: usize = 2;
    const MIRROR: usize = 3;

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
        assert_eq!(Command::UndoDelete.name(), "UndoDelete");
        assert_eq!(Command::UndoDelete.path(), CommandPath::Missing);
    }
}
