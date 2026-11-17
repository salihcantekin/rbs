//! The UI types `rbs-app` uses, from the crates of the generated UI code (ADR-0138
//! decision 1), and the token setters generated from the design token file (rule U6).
//!
//! Normally the shared globals, structs and enums come from `rbs-app-ui-base` and the
//! windows from one crate per window group, so a change rebuilds only its group and the
//! groups compile in parallel. With the UI live preview (`live-preview` feature and
//! `SLINT_LIVE_PREVIEW`, `build.rs` sets `rbs_ui_live`) everything comes from
//! `rbs-app-ui-single` instead. The names are listed, not glob re-exported, so a name
//! exported twice fails to compile (decision 4).
#![allow(missing_docs, clippy::all, clippy::unwrap_used, clippy::expect_used)]

/// Re-exports the shared types and the windows from `$types` and the window crates.
macro_rules! reexport {
    ($types:ident, $main:ident, $settings:ident, $dialogs:ident) => {
        pub use $dialogs::{
            AttributesWindow, ObsImportWindow, ProjectorWindow, SourceSettingsWindow,
            SyncMeasureWindow,
        };
        pub use $main::{FloatWindow, MainWindow};
        pub use $settings::SettingsWindow;
        pub use $types::{
            AiActionRow, AppMenuChip, AppMenuRow, AppMenus, Appearance, AttributeGroupRow,
            AttributeTargetRow, AttributesData, ContextChoice, ContextItem, ContextMenus, Cover,
            DestinationRow, EdgeTabData, EncoderData, EntryRow, Experience, FilterData,
            FilterFieldData, FilterLevels, FocusReveal, HandleData, IconName, Icons, KindData,
            LiveDestinationRow, LookData, LoudnessData, MarkData, MenuChoice, Menus, MeterRow,
            ModeData, ObsReportRow, ObsSceneChoice, OverlayCover, Overlays, PanelEntry, PickerRow,
            RailData, ReplayBuffer, SampleData, ScaleMark, SceneData, Sections, SettingsFind,
            SettingsHit, SettingsScroll, Shell, ShortcutChip, ShortcutRow, Shortcuts, SizeBand,
            SourceData, SourceSettingsData, SourceTargetData, StackData, StatRow, Studio,
            SyncMeasureData, TabData, TextData, Theme, ThemeOption, ThemeSlot, ThemeTokenRow,
            TokenRow, Tokens, ToolbarData, VirtualCamera, WorkspaceEntry,
        };
    };
}

/// Re-exports the test-only probe windows (`probes` feature, decision 1).
#[cfg(feature = "probes")]
macro_rules! reexport_probes {
    ($probes:ident) => {
        pub use $probes::{
            EffectsProbe, ExperienceProbe, FocusRevealProbe, NumberFieldProbe, OverlayProbe,
            PropertiesProbe, ReplayProbe, SegmentProbe, SettingRowProbe,
        };
    };
}

#[cfg(not(rbs_ui_live))]
reexport!(
    rbs_app_ui_base,
    rbs_app_ui_main,
    rbs_app_ui_settings,
    rbs_app_ui_dialogs
);
#[cfg(all(not(rbs_ui_live), feature = "probes"))]
reexport_probes!(rbs_app_ui_probes);

#[cfg(rbs_ui_live)]
reexport!(
    rbs_app_ui_single,
    rbs_app_ui_single,
    rbs_app_ui_single,
    rbs_app_ui_single
);
#[cfg(all(rbs_ui_live, feature = "probes"))]
reexport_probes!(rbs_app_ui_single);

/// Copies a token file's values into a window's `Theme` and `Tokens` globals
/// (`rbs-app` `look`, themes spec "Applying").
pub mod tokens_apply {
    use super::{Theme, ThemeSlot, Tokens};

    include!(concat!(env!("OUT_DIR"), "/tokens_apply.rs"));
}
