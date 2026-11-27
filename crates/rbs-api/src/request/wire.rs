//! The flat wire form of [`Request`] (`{"method": ..., "params": ...}`) and the
//! conversions both ways.

use serde::{Deserialize, Serialize};

use super::*;

impl Request {
    /// Its method name.
    pub fn method(&self) -> &'static str {
        match self {
            Request::Hello(_) => "session.hello",
            Request::Authenticate(_) => "session.authenticate",
            Request::Describe(_) => "api.describe",
            Request::Unsubscribe(_) => "events.unsubscribe",
            Request::Command(c) => c.method(),
            Request::Ui(ui) => ui.method(),
        }
    }

    /// The scope it needs, or `None` for the handshake.
    pub fn scope(&self) -> Option<Scope> {
        match self {
            Self::Ui(_) => Some(Scope::Control),
            _ => method_scope(self.method()).flatten(),
        }
    }
}

/// The flat wire form of [`Request`]: `{"method": ..., "params": ...}`.
#[derive(Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub(crate) enum WireRequest {
    #[serde(rename = "ui.command.list")]
    UiList(crate::ui::UiList),
    #[serde(rename = "ui.command.run")]
    UiRun(crate::ui::UiRun),
    #[serde(rename = "session.hello")]
    Hello(Hello),
    #[serde(rename = "session.authenticate")]
    Authenticate(Authenticate),
    #[serde(rename = "api.describe")]
    Describe(DescribeParams),
    #[serde(rename = "studio.get")]
    StudioGet(Empty),
    #[serde(rename = "source.list")]
    SourceList(Empty),
    #[serde(rename = "source.targets")]
    SourceTargets(TargetsParams),
    #[serde(rename = "source.add")]
    SourceAdd(AddSource),
    #[serde(rename = "source.delete")]
    SourceDelete(SourceRef),
    #[serde(rename = "item.set_style")]
    ItemSetStyle(SetItemStyle),
    #[serde(rename = "item.set_crop")]
    ItemSetCrop(SetItemCrop),
    #[serde(rename = "item.flip")]
    ItemFlip(FlipItem),
    #[serde(rename = "item.rotate")]
    ItemRotate(RotateItem),
    #[serde(rename = "item.fit")]
    ItemFit(FitItem),
    #[serde(rename = "item.align")]
    ItemAlign(AlignItem),
    #[serde(rename = "item.reset_transform")]
    ItemResetTransform(ResetItemTransform),
    #[serde(rename = "item.set_rotation")]
    ItemSetRotation(SetItemRotation),
    #[serde(rename = "item.align_items")]
    ItemAlignItems(AlignItems),
    #[serde(rename = "item.distribute")]
    ItemDistribute(DistributeItems),
    #[serde(rename = "item.fit_text_box")]
    ItemFitTextBox(FitTextBox),
    #[serde(rename = "item.set_filled")]
    ItemSetFilled(SetItemFilled),
    #[serde(rename = "item.set_shown_region")]
    ItemSetShownRegion(SetItemShownRegion),
    #[serde(rename = "canvas.auto_layout")]
    CanvasAutoLayout(CanvasAutoLayout),
    #[serde(rename = "canvas.set_vertical_size")]
    CanvasSetVerticalSize(SetVerticalSize),
    #[serde(rename = "scene.list")]
    SceneList(Empty),
    #[serde(rename = "scene.add")]
    SceneAdd(AddScene),
    #[serde(rename = "scene.remove")]
    SceneRemove(SceneRef),
    #[serde(rename = "scene.rename")]
    SceneRename(RenameScene),
    #[serde(rename = "scene.set_transition")]
    SceneSetTransition(SetSceneTransition),
    #[serde(rename = "scene.duplicate")]
    SceneDuplicate(DuplicateScene),
    #[serde(rename = "source.duplicate")]
    SourceDuplicate(SourceInScene),
    #[serde(rename = "source.reset")]
    SourceReset(ResetSource),
    #[serde(rename = "source.copy_attributes")]
    SourceCopyAttributes(SourceInScene),
    #[serde(rename = "source.attributes_clipboard")]
    SourceAttributesClipboard(Empty),
    #[serde(rename = "source.set_text")]
    SourceSetText(SetText),
    #[serde(rename = "text.fonts")]
    TextFonts(Empty),
    #[serde(rename = "source.paste_attributes")]
    SourcePasteAttributes(PasteAttributes),
    #[serde(rename = "scene.switch")]
    SceneSwitch(SwitchScene),
    #[serde(rename = "source.rename")]
    SourceRename(RenameSource),
    #[serde(rename = "source.set_volume")]
    SourceSetVolume(SetSourceVolume),
    #[serde(rename = "source.set_muted")]
    SourceSetMuted(SetSourceMuted),
    #[serde(rename = "source.set_mixer_hidden")]
    SourceSetMixerHidden(SetSourceMixerHidden),
    #[serde(rename = "canvas.set_master_volume")]
    CanvasSetMasterVolume(SetMasterVolume),
    #[serde(rename = "canvas.set_master_muted")]
    CanvasSetMasterMuted(SetMasterMuted),
    #[serde(rename = "item.add")]
    ItemAdd(AddItem),
    #[serde(rename = "filter.types")]
    FilterTypes(FilterTypesParams),
    #[serde(rename = "filter.add")]
    FilterAdd(AddFilter),
    #[serde(rename = "filter.add_voice_chain")]
    FilterAddVoiceChain(SourceRef),
    #[serde(rename = "filter.remove")]
    FilterRemove(FilterRef),
    #[serde(rename = "filter.move")]
    FilterMove(MoveFilter),
    #[serde(rename = "filter.set_enabled")]
    FilterSetEnabled(SetFilterEnabled),
    #[serde(rename = "filter.set_settings")]
    FilterSetSettings(SetFilterSettings),
    #[serde(rename = "source.set_sync_offset")]
    SourceSetSyncOffset(SetSourceSyncOffset),
    #[serde(rename = "sync.measure_start")]
    SyncMeasureStart(SyncMeasureStart),
    #[serde(rename = "sync.measure_cancel")]
    SyncMeasureCancel(SyncMeasureCancel),
    #[serde(rename = "sync.devices")]
    SyncDevices(Empty),
    #[serde(rename = "sync.forget_device")]
    SyncForgetDevice(SyncForgetDevice),
    #[serde(rename = "sync.apply_to_device_sources")]
    SyncApplyToDeviceSources(SyncApplyToDeviceSources),
    #[serde(rename = "source.retarget")]
    SourceRetarget(RetargetSource),
    #[serde(rename = "source.reload")]
    SourceReload(ReloadSource),
    #[serde(rename = "browser.reload")]
    BrowserReload(BrowserReload),
    #[serde(rename = "browser.run_script")]
    BrowserRunScript(BrowserRunScript),
    #[serde(rename = "browser.forget_sign_in")]
    BrowserForgetSignIn(BrowserForgetSignIn),
    #[serde(rename = "browser.input")]
    BrowserInput(BrowserInput),
    #[serde(rename = "source.get_settings")]
    SourceGetSettings(GetSourceSettings),
    #[serde(rename = "media.play")]
    MediaPlay(MediaSource),
    #[serde(rename = "media.pause")]
    MediaPause(MediaSource),
    #[serde(rename = "media.seek")]
    MediaSeek(MediaSeek),
    #[serde(rename = "media.restart")]
    MediaRestart(MediaSource),
    #[serde(rename = "media.state")]
    MediaState(MediaSource),
    #[serde(rename = "media.next")]
    MediaNext(MediaSource),
    #[serde(rename = "media.previous")]
    MediaPrevious(MediaSource),
    #[serde(rename = "source.set_settings")]
    SourceSetSettings(SetSourceSettings),
    #[serde(rename = "source.set_companion_audio")]
    SourceSetCompanionAudio(SetCompanionAudio),
    #[serde(rename = "canvas.add_vertical")]
    CanvasAddVertical(AddVerticalCanvas),
    #[serde(rename = "canvas.remove_vertical")]
    CanvasRemoveVertical(Empty),
    #[serde(rename = "settings.recent_targets")]
    SettingsRecentTargets(TargetsParams),
    #[serde(rename = "settings.clear_recent_targets")]
    SettingsClearRecentTargets(TargetsParams),
    #[serde(rename = "item.set_visible")]
    ItemSetVisible(SetItemVisible),
    #[serde(rename = "item.set_locked")]
    ItemSetLocked(SetItemLocked),
    #[serde(rename = "item.set_show_for")]
    ItemSetShowFor(SetItemShowFor),
    #[serde(rename = "item.show")]
    ItemShow(ShowItem),
    #[serde(rename = "item.set_scroll")]
    ItemSetScroll(SetItemScroll),
    #[serde(rename = "item.scroll_pause")]
    ItemScrollPause(ScrollItem),
    #[serde(rename = "item.scroll_resume")]
    ItemScrollResume(ScrollItem),
    #[serde(rename = "item.scroll_restart")]
    ItemScrollRestart(ScrollItem),
    #[serde(rename = "item.scroll_speed")]
    ItemScrollSpeed(SetScrollSpeed),
    #[serde(rename = "scene.sources")]
    SceneSources(SceneRef),
    #[serde(rename = "group.create")]
    GroupCreate(CreateGroup),
    #[serde(rename = "group.remove")]
    GroupRemove(GroupRef),
    #[serde(rename = "group.rename")]
    GroupRename(RenameGroup),
    #[serde(rename = "group.list")]
    GroupList(SceneRef),
    #[serde(rename = "canvas.set_preview")]
    CanvasSetPreview(SetPreview),
    #[serde(rename = "canvas.transition")]
    CanvasTransition(TransitionOnce),
    #[serde(rename = "canvas.rehearse")]
    CanvasRehearse(Rehearse),
    #[serde(rename = "canvas.transition_progress")]
    CanvasTransitionProgress(TransitionProgress),
    #[serde(rename = "item.remove")]
    ItemRemove(ItemRef),
    #[serde(rename = "item.move")]
    ItemMove(MoveItem),
    #[serde(rename = "scene.move")]
    SceneMove(MoveScene),
    #[serde(rename = "source.set_active")]
    SourceSetActive(SetSourceActive),
    #[serde(rename = "look.list")]
    LookList(Empty),
    #[serde(rename = "look.apply")]
    LookApply(ApplyLook),
    #[serde(rename = "source.add_sample")]
    SourceAddSample(AddSample),
    #[serde(rename = "item.set_box")]
    ItemSetBox(SetItemBox),
    #[serde(rename = "global.list")]
    GlobalList(Empty),
    #[serde(rename = "global.add")]
    GlobalAdd(AddGlobal),
    #[serde(rename = "global.remove")]
    GlobalRemove(RemoveGlobal),
    #[serde(rename = "edit.undo")]
    EditUndo(Empty),
    #[serde(rename = "edit.redo")]
    EditRedo(Empty),
    #[serde(rename = "edit.undo_delete")]
    EditUndoDelete(Empty),
    #[serde(rename = "output.profiles")]
    OutputProfiles(Empty),
    #[serde(rename = "record.start")]
    RecordStart(RecordStart),
    #[serde(rename = "record.estimate")]
    RecordEstimate(RecordStart),
    #[serde(rename = "record.start_to_file")]
    RecordStartToFile(RecordStartToFile),
    #[serde(rename = "record.stop")]
    RecordStop(OnCanvases),
    #[serde(rename = "record.pause")]
    RecordPause(OnCanvases),
    #[serde(rename = "record.resume")]
    RecordResume(OnCanvases),
    #[serde(rename = "record.pause_toggle")]
    RecordPauseToggle(OnCanvases),
    #[serde(rename = "record.status")]
    RecordStatus(OnCanvas),
    #[serde(rename = "stream.start")]
    StreamStart(OnCanvases),
    #[serde(rename = "stream.stop")]
    StreamStop(OnCanvases),
    #[serde(rename = "stream.hold")]
    StreamHold(StreamHold),
    #[serde(rename = "stream.resume")]
    StreamResume(OnCanvases),
    #[serde(rename = "stream.hold_toggle")]
    StreamHoldToggle(StreamHold),
    #[serde(rename = "output.break_toggle")]
    OutputBreakToggle(Empty),
    #[serde(rename = "stream.status")]
    StreamStatus(OnCanvas),
    #[serde(rename = "stream.destinations")]
    StreamDestinations(DestinationsOf),
    #[serde(rename = "stream.destination.start")]
    StreamDestinationStart(DestinationRef),
    #[serde(rename = "stream.destination.stop")]
    StreamDestinationStop(DestinationRef),
    #[serde(rename = "stream.destination.set_enabled")]
    StreamDestinationSetEnabled(SetDestinationEnabled),
    #[serde(rename = "output.set_with_vertical")]
    OutputSetWithVertical(SetWithVertical),
    #[serde(rename = "replay.status")]
    ReplayStatus(Empty),
    #[serde(rename = "protection.status")]
    ProtectionStatus(Empty),
    #[serde(rename = "calibration.run")]
    CalibrationRun(CalibrationRun),
    #[serde(rename = "calibration.cancel")]
    CalibrationCancel(Empty),
    #[serde(rename = "calibration.status")]
    CalibrationStatus(Empty),
    #[serde(rename = "source.set_monitor")]
    SourceSetMonitor(SetSourceMonitor),
    #[serde(rename = "monitor.status")]
    MonitorStatus(Empty),
    #[serde(rename = "monitor.devices")]
    MonitorDevices(Empty),
    #[serde(rename = "monitor.set_device")]
    MonitorSetDevice(SetMonitorDevice),
    #[serde(rename = "monitor.set_volume")]
    MonitorSetVolume(SetMonitorVolume),
    #[serde(rename = "monitor.set_solo")]
    MonitorSetSolo(SetMonitorSolo),
    #[serde(rename = "loudness.status")]
    LoudnessStatus(Empty),
    #[serde(rename = "loudness.set_enabled")]
    LoudnessSetEnabled(SetLoudnessEnabled),
    #[serde(rename = "loudness.reset")]
    LoudnessReset(Empty),
    #[serde(rename = "replay.arm")]
    ReplayArm(Empty),
    #[serde(rename = "replay.disarm")]
    ReplayDisarm(Empty),
    #[serde(rename = "replay.save")]
    ReplaySave(ReplaySave),
    #[serde(rename = "vcam.status")]
    VcamStatus(Empty),
    #[serde(rename = "vcam.start")]
    VcamStart(Empty),
    #[serde(rename = "vcam.stop")]
    VcamStop(Empty),
    #[serde(rename = "settings.get")]
    SettingsGet(Empty),
    #[serde(rename = "settings.set")]
    SettingsSet(SettingsPatch),
    #[serde(rename = "profile.list")]
    ProfileList(Empty),
    #[serde(rename = "profile.switch")]
    ProfileSwitch(EntryRef),
    #[serde(rename = "collection.list")]
    CollectionList(Empty),
    #[serde(rename = "collection.switch")]
    CollectionSwitch(EntryRef),
    #[serde(rename = "collection.set_glide")]
    CollectionSetGlide(SetCollectionGlide),
    #[serde(rename = "collection.set_stinger_file")]
    CollectionSetStingerFile(SetStingerFile),
    #[serde(rename = "collection.set_transition")]
    CollectionSetTransition(SetCollectionTransition),
    #[serde(rename = "source.set_glide")]
    SourceSetGlide(SetSourceGlide),
    #[serde(rename = "source.set_activity")]
    SourceSetActivity(SetSourceActivity),
    #[serde(rename = "source.get_activity")]
    SourceGetActivity(SourceRef),
    #[serde(rename = "source.set_region")]
    SourceSetRegion(SetSourceRegion),
    #[serde(rename = "source.set_record_separately")]
    SourceSetRecordSeparately(SetRecordSeparately),
    #[serde(rename = "marker.add")]
    MarkerAdd(MarkerAdd),
    #[serde(rename = "marker.remove")]
    MarkerRemove(MarkerRemove),
    #[serde(rename = "project.export")]
    ProjectExport(ProjectExport),
    #[serde(rename = "source.get_region")]
    SourceGetRegion(SourceRef),
    #[serde(rename = "source.set_chrome")]
    SourceSetChrome(SetSourceChrome),
    #[serde(rename = "source.get_chrome")]
    SourceGetChrome(SourceRef),
    #[serde(rename = "source.fit_chrome")]
    SourceFitChrome(SourceRef),
    #[serde(rename = "source.reset_chrome")]
    SourceResetChrome(SourceRef),
    #[serde(rename = "scene.audio.get")]
    SceneAudioGet(SceneRef),
    #[serde(rename = "scene.audio.set")]
    SceneAudioSet(SetSceneLevel),
    #[serde(rename = "scene.audio.own")]
    SceneAudioOwn(SceneRef),
    #[serde(rename = "scene.audio.shared")]
    SceneAudioShared(SceneRef),
    #[serde(rename = "scene.audio.reset_source")]
    SceneAudioResetSource(SceneSourceRef),
    #[serde(rename = "events.subscribe")]
    EventsSubscribe(Subscribe),
    #[serde(rename = "events.unsubscribe")]
    EventsUnsubscribe(Unsubscribe),
    #[serde(rename = "tokens.list")]
    TokensList(Empty),
    #[serde(rename = "tokens.create")]
    TokensCreate(CreateToken),
    #[serde(rename = "tokens.revoke")]
    TokensRevoke(TokenRef),
    #[serde(rename = "import.obs.scan")]
    ImportObsScan(Empty),
    #[serde(rename = "import.obs.check")]
    ImportObsCheck(ObsImport),
    #[serde(rename = "import.obs.run")]
    ImportObsRun(ObsImport),
    #[serde(rename = "text.timer")]
    TextTimer(TextTimer),
    #[serde(rename = "addon.list")]
    AddonList(Empty),
    #[serde(rename = "addon.install")]
    AddonInstall(AddOnRef),
    #[serde(rename = "addon.update")]
    AddonUpdate(AddOnRef),
    #[serde(rename = "addon.cancel")]
    AddonCancel(AddOnRef),
    #[serde(rename = "addon.clean")]
    AddonClean(AddOnRef),
    #[serde(rename = "addon.remove")]
    AddonRemove(AddOnRef),
}

/// `From<WireRequest> for Request` and back: the arms that differ, then the methods
/// whose wire and command variants share a name and carry their parameters unchanged.
macro_rules! wire_conversions {
    (
        from_wire { $($from:tt)* }
        to_wire { $($to:tt)* }
        command_to_wire { $($command:tt)* }
        same [$($same:ident),* $(,)?]
    ) => {
        impl From<WireRequest> for Request {
            fn from(w: WireRequest) -> Self {
                use Command as C;
                match w {
                    $($from)*
                    $(WireRequest::$same(p) => Request::Command(C::$same(p)),)*
                }
            }
        }

        impl From<Request> for WireRequest {
            fn from(r: Request) -> Self {
                use Command as C;
                match r {
                    $($to)*
                    Request::Command(c) => match c {
                        $($command)*
                        $(C::$same(p) => WireRequest::$same(p),)*
                    },
                }
            }
        }
    };
}

wire_conversions! {
    from_wire {
        WireRequest::UiList(p) => Request::Ui(crate::ui::UiRequest::List { client: p.client }),
        WireRequest::UiRun(p) => Request::Ui(crate::ui::UiRequest::Run { id: p.id, client: p.client }),
        WireRequest::Hello(p) => Request::Hello(p),
        WireRequest::Authenticate(p) => Request::Authenticate(p),
        WireRequest::Describe(p) => Request::Describe(p),
        WireRequest::EventsUnsubscribe(p) => Request::Unsubscribe(p),
        WireRequest::StudioGet(_) => Request::Command(C::StudioGet),
        WireRequest::SourceList(_) => Request::Command(C::SourceList),
        WireRequest::SceneList(_) => Request::Command(C::SceneList),
        WireRequest::SourceAttributesClipboard(_) => {
            Request::Command(C::SourceAttributesClipboard)
        }
        WireRequest::TextFonts(_) => Request::Command(C::TextFonts),
        WireRequest::SyncDevices(_) => Request::Command(C::SyncDevices),
        WireRequest::LookList(_) => Request::Command(C::LookList),
        WireRequest::GlobalList(_) => Request::Command(C::GlobalList),
        WireRequest::EditUndo(_) => Request::Command(C::EditUndo),
        WireRequest::EditRedo(_) => Request::Command(C::EditRedo),
        WireRequest::EditUndoDelete(_) => Request::Command(C::EditUndoDelete),
        WireRequest::OutputProfiles(_) => Request::Command(C::OutputProfiles),
        WireRequest::OutputBreakToggle(_) => Request::Command(C::OutputBreakToggle),
        WireRequest::ReplayStatus(_) => Request::Command(C::ReplayStatus),
        WireRequest::ProtectionStatus(_) => Request::Command(C::ProtectionStatus),
        WireRequest::CalibrationCancel(_) => Request::Command(C::CalibrationCancel),
        WireRequest::CalibrationStatus(_) => Request::Command(C::CalibrationStatus),
        WireRequest::MonitorStatus(_) => Request::Command(C::MonitorStatus),
        WireRequest::MonitorDevices(_) => Request::Command(C::MonitorDevices),
        WireRequest::LoudnessStatus(_) => Request::Command(C::LoudnessStatus),
        WireRequest::LoudnessReset(_) => Request::Command(C::LoudnessReset),
        WireRequest::ReplayArm(_) => Request::Command(C::ReplayArm),
        WireRequest::ReplayDisarm(_) => Request::Command(C::ReplayDisarm),
        WireRequest::VcamStatus(_) => Request::Command(C::VcamStatus),
        WireRequest::VcamStart(_) => Request::Command(C::VcamStart),
        WireRequest::VcamStop(_) => Request::Command(C::VcamStop),
        WireRequest::SettingsGet(_) => Request::Command(C::SettingsGet),
        WireRequest::ProfileList(_) => Request::Command(C::ProfileList),
        WireRequest::CollectionList(_) => Request::Command(C::CollectionList),
        WireRequest::TokensList(_) => Request::Command(C::TokensList),
        WireRequest::ImportObsScan(_) => Request::Command(C::ImportObsScan),
        WireRequest::AddonList(_) => Request::Command(C::AddonList),
    }
    to_wire {
        Request::Ui(crate::ui::UiRequest::List { client }) => WireRequest::UiList(crate::ui::UiList { client }),
        Request::Ui(crate::ui::UiRequest::Run { id, client }) => WireRequest::UiRun(crate::ui::UiRun { id, client }),
        Request::Hello(p) => WireRequest::Hello(p),
        Request::Authenticate(p) => WireRequest::Authenticate(p),
        Request::Describe(p) => WireRequest::Describe(p),
        Request::Unsubscribe(p) => WireRequest::EventsUnsubscribe(p),
    }
    command_to_wire {
        C::StudioGet => WireRequest::StudioGet(Empty {}),
        C::SourceList => WireRequest::SourceList(Empty {}),
        C::SceneList => WireRequest::SceneList(Empty {}),
        C::SourceAttributesClipboard => WireRequest::SourceAttributesClipboard(Empty {}),
        C::TextFonts => WireRequest::TextFonts(Empty {}),
        C::SyncDevices => WireRequest::SyncDevices(Empty {}),
        C::LookList => WireRequest::LookList(Empty {}),
        C::GlobalList => WireRequest::GlobalList(Empty {}),
        C::EditUndo => WireRequest::EditUndo(Empty {}),
        C::EditRedo => WireRequest::EditRedo(Empty {}),
        C::EditUndoDelete => WireRequest::EditUndoDelete(Empty {}),
        C::OutputProfiles => WireRequest::OutputProfiles(Empty {}),
        C::OutputBreakToggle => WireRequest::OutputBreakToggle(Empty {}),
        C::ReplayStatus => WireRequest::ReplayStatus(Empty {}),
        C::ProtectionStatus => WireRequest::ProtectionStatus(Empty {}),
        C::CalibrationCancel => WireRequest::CalibrationCancel(Empty {}),
        C::CalibrationStatus => WireRequest::CalibrationStatus(Empty {}),
        C::MonitorStatus => WireRequest::MonitorStatus(Empty {}),
        C::MonitorDevices => WireRequest::MonitorDevices(Empty {}),
        C::LoudnessStatus => WireRequest::LoudnessStatus(Empty {}),
        C::LoudnessReset => WireRequest::LoudnessReset(Empty {}),
        C::ReplayArm => WireRequest::ReplayArm(Empty {}),
        C::ReplayDisarm => WireRequest::ReplayDisarm(Empty {}),
        C::VcamStatus => WireRequest::VcamStatus(Empty {}),
        C::VcamStart => WireRequest::VcamStart(Empty {}),
        C::VcamStop => WireRequest::VcamStop(Empty {}),
        C::SettingsGet => WireRequest::SettingsGet(Empty {}),
        C::ProfileList => WireRequest::ProfileList(Empty {}),
        C::CollectionList => WireRequest::CollectionList(Empty {}),
        C::TokensList => WireRequest::TokensList(Empty {}),
        C::ImportObsScan => WireRequest::ImportObsScan(Empty {}),
        C::AddonList => WireRequest::AddonList(Empty {}),
    }
    same [
    SourceTargets, SourceAdd, SourceDelete, ItemSetStyle, ItemSetCrop, ItemFlip, ItemRotate,
    ItemFit, ItemAlign, ItemResetTransform, ItemFitTextBox, ItemSetFilled, ItemSetShownRegion,
    SceneAdd, SceneRemove, SceneRename, SceneSetTransition, SceneDuplicate, SourceDuplicate,
    SourceReset, SourceCopyAttributes, SourceSetText, SourcePasteAttributes, SceneSwitch,
    SourceRename, SourceSetVolume, SourceSetMuted, SourceSetMixerHidden, CanvasSetMasterVolume,
    CanvasSetMasterMuted, ItemAdd, FilterTypes, FilterAdd, FilterRemove, FilterMove,
    FilterSetEnabled, FilterSetSettings, SourceSetSyncOffset, SyncMeasureStart,
    SyncMeasureCancel, SyncForgetDevice, SourceRetarget, SourceReload, BrowserReload,
    BrowserRunScript, BrowserForgetSignIn, BrowserInput, SourceGetSettings, MediaPlay,
    MediaPause, MediaSeek, MediaRestart, MediaState, MediaNext, MediaPrevious, SourceSetSettings, CanvasAddVertical,
    CanvasRemoveVertical, CanvasAutoLayout, CanvasSetVerticalSize, SettingsRecentTargets,
    SceneSources, CanvasSetPreview, CanvasTransition, ItemRemove, ItemMove, SceneMove,
    SourceSetActive, LookApply, SourceAddSample, ItemSetBox, GlobalAdd, GlobalRemove,
    ItemSetVisible, ItemSetLocked, ItemSetShowFor, ItemShow, ItemSetScroll, ItemScrollPause,
    ItemScrollResume, ItemScrollRestart, ItemScrollSpeed, RecordStart, RecordEstimate,
    RecordStartToFile, RecordStop, RecordPause, RecordResume, RecordPauseToggle, RecordStatus,
    StreamStart, StreamStop, StreamHold, StreamResume, StreamHoldToggle, StreamStatus,
    StreamDestinations, StreamDestinationStop, OutputSetWithVertical, CalibrationRun,
    SourceSetMonitor, MonitorSetDevice, MonitorSetVolume, MonitorSetSolo, LoudnessSetEnabled,
    ReplaySave, SettingsSet, ProfileSwitch, CollectionSwitch, CollectionSetGlide,
    SourceSetGlide, SourceSetActivity, SourceGetActivity, SourceSetRegion, MarkerAdd,
    MarkerRemove, ProjectExport, SourceGetRegion, SourceSetChrome, SourceGetChrome,
    SourceFitChrome, SourceResetChrome, SceneAudioGet, SceneAudioSet, SceneAudioOwn,
    SceneAudioShared, SceneAudioResetSource, EventsSubscribe, TokensCreate, TokensRevoke,
    ImportObsCheck, ImportObsRun, TextTimer, AddonInstall, AddonCancel, AddonClean,
    AddonRemove, FilterAddVoiceChain, CollectionSetStingerFile, SyncApplyToDeviceSources,
    SourceSetCompanionAudio, SettingsClearRecentTargets, CanvasTransitionProgress,
    StreamDestinationStart, StreamDestinationSetEnabled, CollectionSetTransition,
    SourceSetRecordSeparately,
    ItemSetRotation, ItemAlignItems, ItemDistribute,
    GroupCreate, GroupRemove, GroupRename, GroupList,
    CanvasRehearse,
    AddonUpdate,
    ]
}
