//! The control API on the control thread: the target that answers one request at a time, and its dispatch (ADR-0159 decision 8).

use super::*;

mod api_add;
mod api_canvas;
mod api_collection;
mod api_item;
mod api_item2;
mod api_item3;
mod api_record;
mod api_scene;
mod api_settings;
mod api_source;
mod api_source2;
mod api_source3;
mod api_studio;
mod api_vcam;

/// The control API's view of the host: one request at a time on the control thread.
pub(super) struct ApiTarget<'a> {
    pub(super) host: &'a mut Host,
    pub(super) notify: &'a dyn Fn(Update),
}

/// Clears [`WINDOW_GESTURE`] from a gesture any caller but the window sent, so a
/// client's drag never joins a window step; the value is masked, never refused
/// (ADR-0164 decision 2).
fn clear_window_gesture(command: &mut rbs_api::Command) {
    use rbs_api::Command as C;
    let gesture = match command {
        C::ItemSetBox(p) => &mut p.gesture,
        C::ItemSetCrop(p) => &mut p.gesture,
        C::ItemSetRotation(p) => &mut p.gesture,
        C::ItemSetStyle(p) => &mut p.gesture,
        C::ItemSetScroll(p) => &mut p.gesture,
        C::ItemFlip(p) => &mut p.gesture,
        C::ItemRotate(p) => &mut p.gesture,
        C::ItemFit(p) => &mut p.gesture,
        C::ItemAlign(p) => &mut p.gesture,
        C::ItemResetTransform(p) => &mut p.gesture,
        C::SourceSetSettings(p) => &mut p.gesture,
        _ => return,
    };
    *gesture &= !WINDOW_GESTURE;
}

impl ControlTarget for ApiTarget<'_> {
    fn handle_as(
        &mut self,
        caller: &rbs_api::Caller,
        mut command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        clear_window_gesture(&mut command);
        self.host.check_on_air(caller.irreversible, &command)?;
        self.host.check_network(caller.irreversible, &command)?;
        self.host.check_text_file(&command)?;
        self.host
            .check_local_only(caller.token.is_some(), &command)?;
        self.handle(command, events)
    }

    fn handle(
        &mut self,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        // The real publisher comes back into the host for the command (see
        // `Host::serve_api`), so its changes publish with the right `seq`.
        std::mem::swap(&mut self.host.events, events);
        // A panic fails this request only (ADR-0055 decision 3); the control loop
        // reconciles the host after the round.
        let method = command.method();
        let result = rbs_crash::contain(|| self.run(command));
        std::mem::swap(&mut self.host.events, events);
        let result = result.unwrap_or_else(|_| {
            self.host.caught = true;
            Err(ApiError::new(ErrorKind::Internal, rbs_crash::CAUGHT_TEXT))
        });
        if let Err(e) = &result {
            failures::log_api_refusal(method, e);
        }
        result
    }
}

impl ApiTarget<'_> {
    pub(super) fn run(&mut self, command: rbs_api::Command) -> Result<Response, ApiError> {
        use rbs_api::Command as C;
        if let Some(e) = output_refusal(self.host, &command) {
            return Err(e);
        }
        match command {
            C::StudioGet
            | C::SourceList
            | C::SourceTargets(..)
            | C::SourceAdd(..)
            | C::SourceSetCompanionAudio(..)
            | C::SourceDelete(..)
            | C::SceneList
            | C::SceneAdd(..) => self.answer_studio(command),
            C::SceneRemove(..)
            | C::SceneRename(..)
            | C::SceneDuplicate(..)
            | C::SourceDuplicate(..)
            | C::SourceReset(..)
            | C::SourceCopyAttributes(..)
            | C::SourceAttributesClipboard
            | C::SourcePasteAttributes(..)
            | C::SceneAudioGet(..)
            | C::SceneAudioSet(..)
            | C::SceneAudioOwn(..)
            | C::SceneAudioShared(..)
            | C::SceneAudioResetSource(..)
            | C::SceneSources(..)
            | C::CanvasSetPreview(..)
            | C::CanvasTransitionProgress(..)
            | C::CanvasTransition(..)
            | C::SceneSwitch(..) => self.answer_scene(command),
            C::SourceRename(..)
            | C::SourceSetVolume(..)
            | C::SourceSetMuted(..)
            | C::SourceSetMonitor(..)
            | C::MonitorStatus
            | C::MonitorDevices
            | C::MonitorSetDevice(..)
            | C::MonitorSetVolume(..)
            | C::MonitorSetSolo(..)
            | C::LoudnessStatus
            | C::LoudnessSetEnabled(..)
            | C::LoudnessReset
            | C::SourceSetMixerHidden(..)
            | C::CanvasAddVertical(..)
            | C::CanvasAutoLayout(..)
            | C::CanvasSetVerticalSize(..)
            | C::CanvasRemoveVertical(..) => self.answer_source(command),
            C::CanvasSetMasterVolume(..)
            | C::CanvasSetMasterMuted(..)
            | C::FilterTypes(..)
            | C::FilterAdd(..)
            | C::FilterRemove(..)
            | C::FilterMove(..)
            | C::FilterSetEnabled(..)
            | C::FilterSetSettings(..)
            | C::SourceSetSyncOffset(..)
            | C::SyncMeasureStart(..)
            | C::SyncMeasureCancel(..)
            | C::SyncDevices
            | C::SyncForgetDevice(..)
            | C::SyncApplyToDeviceSources(..)
            | C::SourceRetarget(..)
            | C::SettingsRecentTargets(..)
            | C::SettingsClearRecentTargets(..) => self.answer_canvas(command),
            C::SourceGetSettings(..)
            | C::SourceSetSettings(..)
            | C::SourceSetText(..)
            | C::TextTimer(..)
            | C::TextFonts
            | C::MediaPlay(..)
            | C::MediaPause(..)
            | C::MediaSeek(..)
            | C::MediaRestart(..)
            | C::MediaState(..)
            | C::BrowserReload(..)
            | C::BrowserRunScript(..)
            | C::BrowserForgetSignIn(..)
            | C::BrowserInput(..)
            | C::SourceReload(..)
            | C::ItemRemove(..)
            | C::SourceSetActive(..)
            | C::LookList
            | C::LookApply(..)
            | C::SourceAddSample(..) => self.answer_source2(command),
            C::ItemMove(..)
            | C::SceneMove(..)
            | C::ItemFitTextBox(..)
            | C::ItemSetFilled(..)
            | C::ItemSetShownRegion(..) => self.answer_item3(command),
            C::ItemSetBox(..)
            | C::GlobalList
            | C::GlobalAdd(..)
            | C::GlobalRemove(..)
            | C::ItemAdd(..)
            | C::ItemSetVisible(..)
            | C::ItemSetLocked(..)
            | C::ItemSetShowFor(..)
            | C::ItemShow(..)
            | C::ItemFlip(..)
            | C::ItemRotate(..)
            | C::ItemFit(..)
            | C::ItemAlign(..)
            | C::ItemResetTransform(..)
            | C::ItemSetRotation(..)
            | C::ItemSetCrop(..) => self.answer_item(command),
            C::ItemSetStyle(..)
            | C::ItemSetScroll(..)
            | C::ItemScrollPause(..)
            | C::ItemScrollResume(..)
            | C::ItemScrollRestart(..)
            | C::ItemScrollSpeed(..)
            | C::EditUndo
            | C::EditRedo
            | C::OutputSetWithVertical(..)
            | C::OutputProfiles
            | C::RecordStart(..)
            | C::RecordEstimate(..)
            | C::RecordStartToFile(..) => self.answer_item2(command),
            C::RecordStop(..)
            | C::RecordPause(..)
            | C::RecordResume(..)
            | C::RecordPauseToggle(..)
            | C::StreamHold(..)
            | C::StreamResume(..)
            | C::StreamHoldToggle(..)
            | C::OutputBreakToggle
            | C::MarkerAdd(..)
            | C::MarkerRemove(..)
            | C::ProjectExport(..)
            | C::RecordStatus(..)
            | C::StreamStart(..)
            | C::StreamStop(..)
            | C::StreamDestinations(..)
            | C::StreamDestinationStart(..)
            | C::StreamDestinationStop(..)
            | C::StreamDestinationSetEnabled(..)
            | C::StreamStatus(..)
            | C::VcamStatus => self.answer_record(command),
            C::VcamStart
            | C::VcamStop
            | C::CalibrationRun(..)
            | C::CalibrationCancel
            | C::CalibrationStatus
            | C::ProtectionStatus
            | C::ReplayStatus
            | C::ReplayArm
            | C::ReplayDisarm
            | C::ReplaySave(..)
            | C::SettingsGet => self.answer_vcam(command),
            C::SettingsSet(..)
            | C::ProfileList
            | C::CollectionList
            | C::ProfileSwitch(..)
            | C::CollectionSwitch(..)
            | C::ImportObsScan
            | C::ImportObsCheck(..)
            | C::ImportObsRun(..)
            | C::AddonList
            | C::AddonInstall(..)
            | C::AddonCancel(..)
            | C::AddonClean(..)
            | C::AddonRemove(..)
            | C::CollectionSetGlide(..)
            | C::SceneSetTransition(..) => self.answer_settings(command),
            C::FilterAddVoiceChain(..) | C::CollectionSetStingerFile(..) => {
                self.answer_add(command)
            }
            C::CollectionSetTransition(..)
            | C::SourceSetGlide(..)
            | C::SourceSetRegion(..)
            | C::SourceSetRecordSeparately(..)
            | C::SourceGetRegion(..)
            | C::SourceSetChrome(..)
            | C::SourceSetActivity(..)
            | C::SourceGetActivity(..)
            | C::SourceGetChrome(..)
            | C::SourceFitChrome(..) => self.answer_collection(command),
            C::SourceResetChrome(..)
            | C::EventsSubscribe(..)
            | C::TokensList
            | C::TokensCreate(..)
            | C::TokensRevoke(..) => self.answer_source3(command),
        }
    }

    /// Starts recording canvas `c` with its saved settings into `path`.
    pub(super) fn record(
        &mut self,
        c: OutputCanvas,
        path: PathBuf,
        explicit: bool,
        iso: bool,
        gate: StartGate,
    ) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        if explicit && !path.is_absolute() {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                format!("{} is not a full path", path.display()),
            ));
        }
        if explicit && path.exists() {
            return Err(ApiError::new(
                ErrorKind::FileExists,
                format!("{} already exists", path.display()),
            ));
        }
        host.note_encoders(notify);
        if !host.record_profile(c).format.is_audio_only()
            && let Some(why) = &host.recording.cannot_record
        {
            return Err(failed(why.clone()));
        }
        if host.outs[slot(c)].recording.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a recording is already running",
            ));
        }
        let mut request = host.profile_request(c, path).map_err(|why| match c {
            OutputCanvas::Main => failed(why.into_text()),
            OutputCanvas::Vertical => failed(format!("the vertical recording: {why}")),
        })?;
        request.iso = iso;
        let profile = (!explicit).then(|| API_PROFILE_ID.to_owned());
        let result = host.start_recording(c, &request, profile, gate);
        host.publish(notify);
        match result {
            Ok(update) => {
                let started = match &update {
                    RecordingUpdate::Started {
                        video_encoder,
                        audio_encoder,
                        ..
                    } => RecordStarted {
                        video_encoder: video_encoder.clone(),
                        audio_encoder: audio_encoder.clone(),
                        estimate: host.running_estimate(c),
                        // A one-file take (every video file skipped, ADR-0087
                        // decision 7) has its skipped files only in its state.
                        skipped: match &host.outs[slot(c)].record_state {
                            api::RecordState::Recording { skipped, .. } => skipped.clone(),
                            _ => Vec::new(),
                        },
                    },
                    _ => return Err(failed("the recording did not start")),
                };
                // The UI shows the recording as if its own button started it.
                notify(recording_update(c, update));
                Ok(Response::RecordStarted(started))
            }
            Err(why) => Err(why.into_api()),
        }
    }
}

/// How a `record.start` asks for its separate video files (protocol 1.54): with
/// neither new parameter it is an older request; `cpu` is refused until the CPU
/// encoder for separate files exists (ADR-0119 step (c), owner record on ADR-0087).
pub(super) fn start_gate(
    p: &rbs_api::request::RecordStart,
    profile: (LimitPolicy, bool),
) -> Result<StartGate, ApiError> {
    use rbs_api::request::HardwareLimitPolicy as P;
    if p.acknowledge_heavy.is_none() && p.on_hardware_limit.is_none() {
        return Ok(StartGate::Older);
    }
    let policy = match p.on_hardware_limit {
        Some(P::Ask) => LimitPolicy::Ask,
        Some(P::Skip) => LimitPolicy::Skip,
        Some(P::Cpu) => {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "the CPU encoder for separate files comes with a later version",
            ));
        }
        None => {
            if !profile.1 {
                let what = "recording.iso.on_hardware_limit: unknown value; \"ask\"";
                rbs_core::warn!(rbs_core::log::Area::Record, "{what}");
            }
            profile.0
        }
    };
    Ok(StartGate::Asked {
        acknowledge_heavy: p.acknowledge_heavy.unwrap_or(false),
        policy,
    })
}

/// Why no output may start for `command` now: a profile or collection switch is
/// still being made, or *Test this computer* runs (ADR-0087 step D1).
fn output_refusal(host: &Host, command: &rbs_api::Command) -> Option<ApiError> {
    use rbs_api::Command as C;
    // An output started now would run with what the switch replaces.
    if host.switch_pending.is_some()
        && matches!(
            command,
            C::RecordStart(_) | C::RecordStartToFile(_) | C::StreamStart(_)
        )
    {
        return Some(ApiError::new(
            ErrorKind::Conflict,
            "a switch of output profiles or scene collections is still being made; try again",
        ));
    }
    // No output starts while *Test this computer* runs (ADR-0087 step D1).
    if matches!(
        command,
        C::RecordStart(_)
            | C::RecordStartToFile(_)
            | C::StreamStart(_)
            | C::VcamStart
            | C::ReplayArm
    ) && let Some(why) = host.calibration_blocks()
    {
        return Some(ApiError::new(ErrorKind::Conflict, why));
    }
    None
}
