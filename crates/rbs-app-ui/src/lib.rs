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
        pub use $types::{FocusReveal, SettingsFind, IconName, Icons, SizeBand, Experience, Sections, OverlayCover, PickerRow, Overlays, LiveDestinationRow, TabData, StackData, RailData, HandleData, PanelEntry, WorkspaceEntry, ToolbarData, EdgeTabData, Cover, MenuChoice, Menus, Shell, AppMenuRow, AppMenuChip, AppMenus, ThemeOption, ThemeTokenRow, Appearance, ContextChoice, ContextItem, ContextMenus, ObsSceneChoice, ObsReportRow, ReplayBuffer, SettingsHit, SettingsScroll, ShortcutChip, ShortcutRow, Shortcuts, FilterFieldData, FilterLevels, FilterData, SourceData, SceneData, LookData, SampleData, KindData, MarkData, ModeData, SyncMeasureData, AttributeGroupRow, AttributeTargetRow, AttributesData, TextData, SourceSettingsData, SourceTargetData, MeterRow, ScaleMark, LoudnessData, DestinationRow, EncoderData, TokenRow, AiActionRow, StatRow, EntryRow, Studio, VirtualCamera, Theme, Tokens, ThemeSlot};
        pub use $main::{MainWindow, FloatWindow};
        pub use $settings::{SettingsWindow};
        pub use $dialogs::{SourceSettingsWindow, SyncMeasureWindow, ObsImportWindow, AttributesWindow, ProjectorWindow};
    };
}

/// Re-exports the test-only probe windows (`probes` feature, decision 1).
#[cfg(feature = "probes")]
macro_rules! reexport_probes {
    ($probes:ident) => {
        pub use $probes::{OverlayProbe, FocusRevealProbe, SettingRowProbe, NumberFieldProbe, ExperienceProbe, EffectsProbe, SegmentProbe, PropertiesProbe, ReplayProbe};
    };
}

#[cfg(not(rbs_ui_live))]
reexport!(rbs_app_ui_base, rbs_app_ui_main, rbs_app_ui_settings, rbs_app_ui_dialogs);
#[cfg(all(not(rbs_ui_live), feature = "probes"))]
reexport_probes!(rbs_app_ui_probes);

#[cfg(rbs_ui_live)]
reexport!(rbs_app_ui_single, rbs_app_ui_single, rbs_app_ui_single, rbs_app_ui_single);
#[cfg(all(rbs_ui_live, feature = "probes"))]
reexport_probes!(rbs_app_ui_single);

/// Copies a token file's values into a window's `Theme` and `Tokens` globals
/// (`rbs-app` `look`, themes spec "Applying").
pub mod tokens_apply {
    use super::{Theme, ThemeSlot, Tokens};

    include!(concat!(env!("OUT_DIR"), "/tokens_apply.rs"));
}
