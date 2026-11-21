//! The control API on the control thread: the target that answers one request at a time, and its dispatch (ADR-0159 decision 8).

use super::*;

/// The control API's view of the host: one request at a time on the control thread.
pub(super) struct ApiTarget<'a> {
    pub(super) host: &'a mut Host,
    pub(super) notify: &'a dyn Fn(Update),
}

impl ControlTarget for ApiTarget<'_> {
    fn handle_as(
        &mut self,
        caller: &rbs_api::Caller,
        command: rbs_api::Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        self.host.check_on_air(caller.irreversible, &command)?;
        self.host.check_network(caller.irreversible, &command)?;
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
        let notify = self.notify;
        let host = &mut *self.host;
        // An output started now would run with what the switch replaces.
        if host.switch_pending.is_some()
            && matches!(
                command,
                C::RecordStart(_) | C::RecordStartToFile(_) | C::StreamStart(_)
            )
        {
            return Err(ApiError::new(
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
            return Err(ApiError::new(ErrorKind::Conflict, why));
        }
        match command {
            C::StudioGet => {
                host.note_encoders(notify);
                Ok(Response::Studio(api::Snapshot {
                    seq: host.events.seq_map([Topic::CanvasChanged]),
                    data: api::StudioInfo {
                        canvas: host.api_canvas(),
                        kinds: host.kinds.iter().filter_map(|k| api_kind(*k)).collect(),
                        encoders: host
                            .encoders
                            .iter()
                            .map(|e| api::EncoderInfo {
                                id: e.id.clone(),
                                name: e.name.clone(),
                                rate_controls: e.options.rate_controls.clone(),
                                h264_profiles: e.options.h264_profiles.clone(),
                                b_frames_max: e.options.b_frames_max,
                            })
                            .collect(),
                        cannot_record: host.cannot_record.clone(),
                    },
                }))
            }
            C::SourceList => Ok(Response::Sources(api::Snapshot {
                seq: host.events.seq_map([Topic::SourceChanged]),
                data: host
                    .heard
                    .iter()
                    .map(|row| with_input_counters(&host.studio, api_source(row)))
                    .collect(),
            })),
            C::SourceTargets(p) => {
                let kind = app_kind(p.kind);
                Ok(Response::Targets(TargetList {
                    kind: p.kind,
                    targets: host
                        .targets(kind)
                        .into_iter()
                        .map(|t| api::CaptureTarget {
                            name: t.name,
                            adapter: t.adapter,
                            target: t.target,
                            is_default: t.is_default,
                            size: t
                                .size
                                .map(|(width, height)| api::Dimensions { width, height }),
                        })
                        .collect(),
                }))
            }
            C::SourceAdd(p) if p.kind == api::SourceKind::Text => {
                // A text has no target (ADR-0129 decision 8).
                let text = p.text.as_deref().unwrap_or("Text");
                let id = if p.global {
                    if p.scene.is_some() {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "a global source is in every scene: leave out scene",
                        ));
                    }
                    host.add_text(p.name.as_deref(), text, true)
                        .map_err(|e| refused(&e))?
                } else {
                    host.placing_into(p.scene, |h| {
                        h.add_text(p.name.as_deref(), text, false)
                            .map_err(|e| refused(&e))
                    })?
                };
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceAdd(p) => {
                if p.text.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "text is only for a text source",
                    ));
                }
                let kind = app_kind(p.kind);
                let Some(mut target) = host
                    .targets(kind)
                    .into_iter()
                    .find(|t| t.adapter == p.adapter && t.target == p.target)
                else {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("{} has no target {:?}", p.adapter, p.target),
                    ));
                };
                if let Some(name) = p.name.filter(|n| !n.trim().is_empty()) {
                    target.name = name;
                }
                let id = if p.global {
                    // A global source (ADR-0065): in every scene, so no scene is named.
                    if p.scene.is_some() {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "a global source is in every scene: leave out scene",
                        ));
                    }
                    host.add_global(kind, &target).map_err(|e| refused(&e))?
                } else {
                    host.placing_into(p.scene, |h| {
                        let id = h.add(kind, &target).map_err(failed)?;
                        if p.with_audio {
                            // One undo step for the picture and its sound; a refused
                            // companion takes the picture back out.
                            if let Err(e) = h.add_companion(id, true) {
                                let _ = h.delete(id);
                                return Err(refused(&e));
                            }
                        }
                        Ok(id)
                    })?
                };
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::SourceSetCompanionAudio(p) => {
                let picture = SourceId::from_raw(p.source);
                if host.collection.source(picture).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no source {}", p.source),
                    ));
                }
                let has = host
                    .collection
                    .sources()
                    .any(|s| s.companion_of() == Some(picture));
                if p.enabled != has {
                    if p.enabled {
                        host.add_companion(picture, false)
                            .map_err(|e| refused(&e))?;
                    } else {
                        host.remove_companion(picture).map_err(|e| refused(&e))?;
                    }
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceDelete(p) => {
                let id = SourceId::from_raw(p.source);
                let Some(name) = host.collection.source(id).map(|s| s.name.clone()) else {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no source {}", p.source),
                    ));
                };
                host.delete(id).map_err(failed)?;
                host.apply(notify);
                notify(Update::Deleted {
                    name,
                    from_scene: false,
                });
                Ok(Response::Done(Empty {}))
            }
            C::SceneList => Ok(Response::Scenes(api::Snapshot {
                seq: host.events.seq_map([Topic::SceneChanged]),
                data: scene_cache::answer(host),
            })),
            C::SceneAdd(p) => {
                let name = p.name.as_deref();
                let id = host.add_scene(name).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SceneAdded(SceneAdded { scene: id.get() }))
            }
            C::SceneRemove(p) => {
                let done = host.remove_scene(SceneId::from_raw(p.scene));
                // A switch made before a refusal still reaches the studio.
                host.apply(notify);
                done.map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::SceneRename(p) => {
                let changed = host
                    .rename_scene(SceneId::from_raw(p.scene), &p.name)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneDuplicate(p) => host.api_scene_duplicate(p, notify),
            C::SourceDuplicate(p) => host.api_source_duplicate(p, notify),
            C::SourceReset(p) => host.api_source_reset(p, notify),
            C::SourceCopyAttributes(p) => host.api_copy_attributes(p, notify),
            C::SourceAttributesClipboard => Ok(host.api_attributes_clipboard()),
            C::SourcePasteAttributes(p) => host.api_paste_attributes(p, notify),
            C::SceneAudioGet(p) => {
                let id = SceneId::from_raw(p.scene);
                host.levels_scene(id).map_err(|e| refused(&e))?;
                let audio = host.collection.scene(id).and_then(|s| s.audio.as_ref());
                Ok(Response::SceneLevels(rbs_api::request::SceneLevels {
                    scene: p.scene,
                    own: audio.is_some(),
                    levels: audio
                        .map(|a| {
                            a.levels
                                .iter()
                                .map(|(source, l)| rbs_api::request::SceneLevelInfo {
                                    source: source.get(),
                                    volume: l.volume,
                                    muted: l.muted,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }))
            }
            C::SceneAudioSet(p) => {
                let (turned_own, changed) = host
                    .scene_level_set(
                        SceneId::from_raw(p.scene),
                        SourceId::from_raw(p.source),
                        p.volume,
                        p.muted,
                    )
                    .map_err(|e| refused(&e))?;
                if turned_own {
                    // An undo step: saved and on the history like any edit.
                    host.apply(notify);
                } else if changed {
                    host.publish(notify);
                }
                Ok(Response::SceneLevelSet(rbs_api::request::SceneLevelSet {
                    turned_own,
                }))
            }
            C::SceneAudioOwn(p) => {
                if host
                    .scene_levels_own(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneAudioShared(p) => {
                if host
                    .scene_levels_shared(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneAudioResetSource(p) => {
                if host
                    .scene_level_reset(SceneId::from_raw(p.scene), SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSources(p) => {
                let id = SceneId::from_raw(p.scene);
                if !host.listed(id) {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("no scene {}", p.scene),
                    ));
                }
                Ok(Response::SceneSources(SceneSourceList {
                    scene: p.scene,
                    sources: host.rows_of(id).iter().map(api_source).collect(),
                }))
            }
            C::CanvasSetPreview(p) => {
                let changed = match p.scene {
                    None => host.set_studio_mode(false),
                    Some(raw) => {
                        let id = SceneId::from_raw(raw);
                        if host.preview().is_none() {
                            if !host.listed(id) {
                                return Err(refused(&Refusal::NotFound(format!(
                                    "There is no scene {raw}."
                                ))));
                            }
                            host.set_studio_mode(true)
                                .and_then(|_| host.set_preview_scene(id))
                                .map(|_| true)
                        } else {
                            host.set_preview_scene(id)
                        }
                    }
                }
                .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasTransitionProgress(p) => {
                if !p.t.is_finite() || !(0.0..=1.0).contains(&p.t) {
                    return Err(refused(&Refusal::Invalid(
                        "t must be a number from 0 to 1.".to_owned(),
                    )));
                }
                if p.canvas == Some(OutputCanvas::Vertical) {
                    return Err(refused(&Refusal::Conflict(
                        "Only the main canvas has a T-bar.".to_owned(),
                    )));
                }
                if host.preview().is_none() {
                    return Err(refused(&Refusal::Conflict(STUDIO_MODE_OFF.to_owned())));
                }
                host.pending_progress = Some(p.t);
                Ok(Response::Done(Empty {}))
            }
            C::CanvasTransition(p) => {
                let once = transition_once(p).map_err(|e| refused(&e))?;
                let changed = host.transition().map_err(|e| refused(&e))?;
                if changed {
                    host.studio.set_transition_once(once);
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSwitch(p) => {
                let once = transition_once(p.once()).map_err(|e| refused(&e))?;
                let changed = host
                    .switch_scene(SceneId::from_raw(p.scene))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.studio.set_transition_once(once);
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceRename(p) => {
                let changed = host
                    .rename_source(SourceId::from_raw(p.source), &p.name)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetVolume(p) => {
                let changed = host
                    .set_source_audio(SourceId::from_raw(p.source), Some(p.volume), None)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMuted(p) => {
                let changed = host
                    .set_source_audio(SourceId::from_raw(p.source), None, Some(p.muted))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMonitor(p) => {
                let changed = host
                    .set_source_monitor(
                        SourceId::from_raw(p.source),
                        monitor::scene_mode(p.monitor),
                    )
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::MonitorStatus => Ok(Response::MonitorStatus(api::Snapshot {
                seq: {
                    host.monitor_api_changed();
                    host.events.seq_map([Topic::MonitorChanged])
                },
                data: host.api_monitor(),
            })),
            C::MonitorDevices => Ok(Response::MonitorDevices(host.api_monitor_devices())),
            C::MonitorSetDevice(p) => {
                host.set_monitor_device(p.device).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::MonitorSetVolume(p) => {
                host.set_monitor_volume(p.volume).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::MonitorSetSolo(p) => {
                host.set_monitor_solo(&p.sources).map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::LoudnessStatus => Ok(Response::LoudnessStatus(host.api_loudness())),
            C::LoudnessSetEnabled(p) => {
                let done = host.set_loudness_enabled(p.on).map_err(|e| refused(&e));
                host.publish(notify);
                done?;
                Ok(Response::Done(Empty {}))
            }
            C::LoudnessReset => {
                host.reset_loudness().map_err(|e| refused(&e))?;
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetMixerHidden(p) => {
                let changed = host
                    .set_source_mixer_hidden(SourceId::from_raw(p.source), p.hidden)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasAddVertical(p) => {
                use rbs_api::request::VerticalLayout as L;
                let (width, height) = (p.width.unwrap_or(1080), p.height.unwrap_or(1920));
                let fits = |v: u32| {
                    (VERTICAL_MIN_SIDE..=VERTICAL_MAX_SIDE).contains(&v) && v.is_multiple_of(2)
                };
                if !fits(width) || !fits(height) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "width and height must be even and from {VERTICAL_MIN_SIDE} to {VERTICAL_MAX_SIDE}"
                        ),
                    ));
                }
                let spec = VerticalCanvas {
                    size: Size::new(width, height),
                    follow: p.follow.unwrap_or(true),
                    layout: p.layout.map(|l| match l {
                        L::Fit => AutoLayout::Fit,
                        L::Fill => AutoLayout::Fill,
                        L::Stack => AutoLayout::Stack,
                    }),
                };
                let done = host.add_vertical(spec).map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasAutoLayout(p) => {
                use rbs_api::request::VerticalLayout as L;
                if p.canvas != OutputCanvas::Vertical {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "canvas must be \"vertical\" (arranging the main canvas is not in this version)",
                    ));
                }
                let mode = match p.mode {
                    L::Fit => AutoLayout::Fit,
                    L::Fill => AutoLayout::Fill,
                    L::Stack => AutoLayout::Stack,
                };
                let items = host
                    .auto_layout_vertical(mode, p.scene.map(SceneId::from_raw))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::AutoLayoutDone(rbs_api::request::AutoLayoutDone {
                    items,
                }))
            }
            C::CanvasSetVerticalSize(p) => {
                let fits = |v: u32| {
                    (VERTICAL_MIN_SIDE..=VERTICAL_MAX_SIDE).contains(&v) && v.is_multiple_of(2)
                };
                if !fits(p.width) || !fits(p.height) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "width and height must be even and from {VERTICAL_MIN_SIDE} to {VERTICAL_MAX_SIDE}"
                        ),
                    ));
                }
                let done = host
                    .set_vertical_size(Size::new(p.width, p.height))
                    .map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasRemoveVertical(_) => {
                if host.vertical.is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        "there is no vertical canvas",
                    ));
                }
                let done = host.remove_vertical().map_err(|e| refused(&e))?;
                if done {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasSetMasterVolume(p) => {
                let changed = host
                    .set_master(Some(p.volume), None)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CanvasSetMasterMuted(p) => {
                let changed = host
                    .set_master(None, Some(p.muted))
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterTypes(p) => {
                let graph = &host.studio.settings().audio_graph;
                let setup = rbs_audio_filters::DspSetup::new(graph.format, graph.block_frames);
                Ok(Response::FilterTypes(filters::api_types(p.domain, setup)))
            }
            C::FilterAdd(p) => {
                let settings = p.settings.unwrap_or_default();
                let id = host
                    .add_filter(SourceId::from_raw(p.source), &p.type_id, &settings)
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::FilterAdded(rbs_api::request::FilterAdded {
                    filter: id.get(),
                }))
            }
            C::FilterRemove(p) => {
                host.remove_filter(FilterId::from_raw(p.filter))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::FilterMove(p) => {
                if host
                    .move_filter(FilterId::from_raw(p.filter), p.index as usize)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterSetEnabled(p) => {
                if host
                    .set_filter_enabled(FilterId::from_raw(p.filter), p.enabled)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::FilterSetSettings(p) => {
                if host
                    .patch_filter(FilterId::from_raw(p.filter), &p.settings)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetSyncOffset(p) => {
                if host
                    .set_source_sync_offset(SourceId::from_raw(p.source), p.offset_ms)
                    .map_err(|e| refused(&e))?
                {
                    host.publish(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SyncMeasureStart(p) => {
                let (measurement, reference) = host.start_sync_measure(
                    SourceId::from_raw(p.source),
                    p.reference.map(SourceId::from_raw),
                )?;
                Ok(Response::SyncMeasureStarted(
                    rbs_api::request::SyncMeasureStarted {
                        measurement,
                        reference: reference.get(),
                    },
                ))
            }
            C::SyncMeasureCancel(p) => {
                host.cancel_sync_measure(&p.measurement)?;
                Ok(Response::Done(Empty {}))
            }
            C::SyncDevices => Ok(Response::SyncDevices(rbs_api::request::SyncDeviceList {
                devices: host.api_sync_devices(),
            })),
            C::SyncForgetDevice(p) => {
                if host.forget_device(&sync_measure::file_key(p.key)) {
                    Ok(Response::Done(Empty {}))
                } else {
                    Err(ApiError::new(
                        ErrorKind::NotFound,
                        "No offset is remembered for this device.",
                    ))
                }
            }
            C::SyncApplyToDeviceSources(p) => {
                let source = SourceId::from_raw(p.source);
                let peers = host.device_peers(source);
                host.apply_to_device_sources(source)
                    .map_err(|e| refused(&e))?;
                if !peers.is_empty() {
                    host.publish(notify);
                }
                Ok(Response::SyncAppliedToDeviceSources(
                    rbs_api::request::SyncAppliedToDeviceSources {
                        changed: peers.iter().map(|(id, _)| id.get()).collect(),
                    },
                ))
            }
            C::SourceRetarget(p) => {
                let target = Target {
                    name: String::new(),
                    adapter: p.adapter,
                    target: p.target,
                    is_default: false,
                    size: None,
                    app: None,
                    recent: None,
                    picker: false,
                    mic: None,
                    sound: false,
                };
                if host
                    .retarget_source(SourceId::from_raw(p.source), &target)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SettingsRecentTargets(p) => {
                let kind = app_kind(p.kind);
                let targets = host
                    .recent_targets(kind)
                    .into_iter()
                    .map(|t| api::CaptureTarget {
                        name: t.name,
                        adapter: t.adapter,
                        target: t.target,
                        is_default: t.is_default,
                        size: t
                            .size
                            .map(|(width, height)| api::Dimensions { width, height }),
                    })
                    .collect();
                Ok(Response::Targets(TargetList {
                    kind: p.kind,
                    targets,
                }))
            }
            C::SettingsClearRecentTargets(p) => {
                host.clear_recent_targets(app_kind(p.kind));
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetSettings(p) => {
                let settings = host
                    .source_settings_json(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?;
                Ok(Response::SourceSettings(SourceSettingsInfo {
                    source: p.source,
                    settings,
                }))
            }
            C::SourceSetSettings(p) => {
                // A text path would put any readable file on air: the API cannot write
                // it until token scopes exist (ADR-0129 decision 8).
                if p.settings.contains_key(keys::FILE)
                    && host
                        .collection
                        .source(SourceId::from_raw(p.source))
                        .is_some_and(|s| SourceKind::of(&s.type_id) == Some(SourceKind::Text))
                {
                    return Err(ApiError::new(
                        ErrorKind::Forbidden,
                        "the control API cannot set a text source's file",
                    ));
                }
                if host
                    .patch_source_settings(SourceId::from_raw(p.source), &p.settings)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetText(p) => {
                if host
                    .set_source_text(SourceId::from_raw(p.source), &p.text)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::TextTimer(p) => host.text_timer(p),
            C::TextFonts => match host.studio.text_fonts(&host.adapters.sources) {
                rbs_record::TextFonts::Ready(families) => {
                    Ok(Response::TextFonts(rbs_api::types::TextFonts {
                        families: families.as_ref().clone(),
                    }))
                }
                rbs_record::TextFonts::NotReady => Err(ApiError::new(
                    ErrorKind::NotReady,
                    "the font list is being read; ask again shortly",
                )),
                rbs_record::TextFonts::Unavailable => Err(ApiError::new(
                    ErrorKind::Failed,
                    "no text renderer is installed",
                )),
            },
            C::MediaPlay(p) => host
                .media_command(p.source, MediaOp::Play)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaPause(p) => host
                .media_command(p.source, MediaOp::Pause)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaSeek(p) => host
                .media_command(p.source, MediaOp::Seek(p.position_ms))
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaRestart(p) => host
                .media_command(p.source, MediaOp::Restart)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::MediaState(p) => host
                .media_command(p.source, MediaOp::State)
                .map(Response::MediaState)
                .map_err(|e| refused(&e)),
            C::BrowserReload(p) => host
                .browser_message(
                    p.source,
                    &rbs_sources::BrowserMessage::Reload {
                        bypass_cache: p.bypass_cache,
                    },
                )
                .map(|()| Response::Done(Empty {})),
            C::BrowserRunScript(p) => host
                .browser_message(p.source, &rbs_sources::BrowserMessage::RunScript(p.script))
                .map(|()| Response::Done(Empty {})),
            C::BrowserForgetSignIn(p) => {
                host.forget_sign_in(SourceId::from_raw(p.source), false)?;
                host.apply(notify);
                Ok(Response::Done(Empty {}))
            }
            C::BrowserInput(p) => host
                .browser_input(p.source, &p.events)
                .map(|()| Response::Done(Empty {})),
            C::SourceReload(p) => {
                if host
                    .reload_source(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                    == Reload::Retargeted
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemRemove(p) => {
                let (_, removed) = host
                    .remove_from_scene(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemRemoved(item_removed(&removed)))
            }
            C::SceneMove(p) => match host.move_scene(SceneId::from_raw(p.scene), p.direction) {
                Ok(true) => {
                    host.apply(notify);
                    Ok(Response::Done(Empty {}))
                }
                Ok(false) => Ok(Response::Done(Empty {})),
                Err(e) => Err(ApiError::new(ErrorKind::Conflict, e)),
            },
            C::SourceSetActive(p) => {
                match host.set_source_active(SourceId::from_raw(p.source), p.active) {
                    Ok(true) => {
                        host.apply(notify);
                        Ok(Response::Done(Empty {}))
                    }
                    Ok(false) => Ok(Response::Done(Empty {})),
                    Err(e) => Err(ApiError::new(ErrorKind::NotFound, e)),
                }
            }
            C::LookList => Ok(Response::Looks(LookList {
                looks: rbs_scene::look::built_in()
                    .into_iter()
                    .map(|l| LookInfo {
                        id: l.id.to_owned(),
                        name: l.name.to_owned(),
                        group: api_look_group(l.group),
                    })
                    .collect(),
            })),
            C::LookApply(p) => {
                let changed = host
                    .apply_look(SourceId::from_raw(p.source), &p.look)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceAddSample(p) => {
                let id = host.add_sample(&p.sample).map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::SourceAdded(SourceAdded { source: id.get() }))
            }
            C::ItemSetBox(p) => {
                let source = SourceId::from_raw(p.source);
                let changed = match p.canvas {
                    OutputCanvas::Main => {
                        host.set_source_box(source, p.position, p.size, p.gesture)
                    }
                    OutputCanvas::Vertical => {
                        host.set_vertical_box(source, p.position, p.size, p.gesture)
                    }
                }
                .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::BoxPlaced(BoxPlaced {
                    seq: p.seq,
                    ok: true,
                }))
            }
            C::ItemMove(p) => {
                let source = host
                    .collection
                    .item(ItemId::from_raw(p.item))
                    .and_then(|(_, it)| match it.content {
                        ItemContent::Source(id) => Some(id),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            "This source is not in the scene any more.",
                        )
                    })?;
                match host.move_source(source, p.direction) {
                    Ok(true) => {
                        host.apply(notify);
                        Ok(Response::Done(Empty {}))
                    }
                    Ok(false) => Ok(Response::Done(Empty {})),
                    Err(e) => Err(ApiError::new(ErrorKind::Conflict, e)),
                }
            }
            C::GlobalList => Ok(Response::GlobalSources(GlobalSourceList {
                sources: host.global_rows().iter().map(api_source).collect(),
            })),
            C::GlobalAdd(p) => {
                let item = host
                    .make_global(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(Response::ItemAdded(ItemAdded { item: item.get() }))
            }
            C::GlobalRemove(p) => {
                let done = host
                    .unmake_global(SourceId::from_raw(p.source), p.keep)
                    .map_err(|e| refused(&e))?;
                host.apply(notify);
                Ok(match done {
                    Ok(item) => Response::ItemAdded(ItemAdded { item: item.get() }),
                    Err(removed) => Response::ItemRemoved(item_removed(&removed)),
                })
            }
            C::ItemAdd(p) => {
                let item = host.placing_into(p.scene, |h| {
                    h.add_existing(SourceId::from_raw(p.source))
                        .map_err(|e| refused(&e))
                })?;
                host.apply(notify);
                Ok(Response::ItemAdded(ItemAdded { item: item.get() }))
            }
            C::ItemSetVisible(p) => {
                let changed = host
                    .set_item_visible(ItemId::from_raw(p.item), p.visible)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetLocked(p) => {
                let changed = host
                    .set_item_locked(ItemId::from_raw(p.item), p.locked)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetShowFor(p) => {
                let changed = host
                    .set_item_show_for(ItemId::from_raw(p.item), p.show_for_ms)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemShow(p) => {
                let started = host
                    .show_item(ItemId::from_raw(p.item))
                    .map_err(|e| refused(&e))?;
                if started {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemFlip(p) => {
                let op = match p.axis {
                    Axis::Horizontal => TransformOp::FlipHorizontal,
                    Axis::Vertical => TransformOp::FlipVertical,
                };
                host.api_transform(p.item, op, p.canvas, notify)
            }
            C::ItemRotate(p) => {
                if p.degrees % 90 != 0 || p.degrees == 0 || p.degrees.abs() > 360 {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "degrees must be a non-zero multiple of 90 from -360 to 360",
                    ));
                }
                host.api_transform(
                    p.item,
                    TransformOp::Turn(p.degrees as f32),
                    p.canvas,
                    notify,
                )
            }
            C::ItemFit(p) => {
                let mode = match p.mode {
                    FitKind::Fit => FitMode::Fit,
                    FitKind::Fill => FitMode::Fill,
                    FitKind::Stretch => FitMode::Stretch,
                };
                host.api_transform(p.item, TransformOp::Fit(mode), p.canvas, notify)
            }
            C::ItemAlign(p) => {
                let axis = match p.axis {
                    None => CenterAxis::Both,
                    Some(AlignAxis::Both) => CenterAxis::Both,
                    Some(AlignAxis::Horizontal) => CenterAxis::Horizontal,
                    Some(AlignAxis::Vertical) => CenterAxis::Vertical,
                };
                host.api_transform(p.item, TransformOp::Center(axis), p.canvas, notify)
            }
            C::ItemResetTransform(p) => {
                host.api_transform(p.item, TransformOp::Reset, p.canvas, notify)
            }
            C::ItemSetCrop(p) => {
                let [l, t, r, b] = p.crop;
                let item = ItemId::from_raw(p.item);
                match host.set_item_crop(item, Crop::new(l, t, r, b), p.keep_scale, 0, p.canvas) {
                    Ok(changed) => {
                        if changed {
                            host.apply(notify);
                        }
                        Ok(Response::Done(Empty {}))
                    }
                    Err(e) => Err(ApiError::new(
                        match e {
                            CropRefusal::NotFound(_) => ErrorKind::NotFound,
                            CropRefusal::Invalid(_) => ErrorKind::InvalidParams,
                            CropRefusal::Conflict(_) => ErrorKind::Conflict,
                            CropRefusal::NoPicture | CropRefusal::Failed(_) => ErrorKind::Failed,
                        },
                        e.text(),
                    )),
                }
            }
            C::ItemSetStyle(p) => {
                let style = scene_style(&p.style)
                    .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
                // The canvas comes first in the error order (protocol 1.44).
                host.edit_canvas(p.canvas).map_err(|e| refused(&e))?;
                let item = ItemId::from_raw(p.item);
                let found = host
                    .collection
                    .scenes()
                    .flat_map(|s| s.items.iter())
                    .find(|i| i.id == item)
                    .map(|i| i.content);
                match found {
                    None => {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no item {}", p.item),
                        ));
                    }
                    Some(ItemContent::Scene(_)) => {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "shape and border are for source items, not nested scenes",
                        ));
                    }
                    Some(_) => {}
                }
                if p.canvas == OutputCanvas::Vertical
                    && host.collection.item(item).map(|(scene, _)| scene) == host.globals()
                {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "Global sources are not on the vertical canvas yet.",
                    ));
                }
                if host
                    .set_item_style(item, style, 0, p.canvas)
                    .map_err(failed)?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemSetScroll(p) => {
                let scroll = match &p.scroll {
                    Some(s) => Some(
                        scene_scroll(s)
                            .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?,
                    ),
                    None => None,
                };
                // The canvas comes first in the error order (protocol 1.44).
                host.edit_canvas(p.canvas).map_err(|e| refused(&e))?;
                let item = ItemId::from_raw(p.item);
                match host.collection.item(item).map(|(_, i)| i.content) {
                    None => {
                        return Err(ApiError::new(
                            ErrorKind::NotFound,
                            format!("no item {}", p.item),
                        ));
                    }
                    Some(ItemContent::Scene(_)) => {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "scroll is for source items, not nested scenes",
                        ));
                    }
                    Some(_) => {}
                }
                if p.canvas == OutputCanvas::Vertical
                    && host.collection.item(item).map(|(scene, _)| scene) == host.globals()
                {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "Global sources are not on the vertical canvas yet.",
                    ));
                }
                if host
                    .set_item_scroll(item, scroll, 0, p.canvas)
                    .map_err(failed)?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::ItemScrollPause(p) => {
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Pause)
            }
            C::ItemScrollResume(p) => {
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Resume)
            }
            C::ItemScrollRestart(p) => {
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Restart)
            }
            C::ItemScrollSpeed(p) => {
                if !(MIN_SCROLL_SPEED..=MAX_SCROLL_SPEED).contains(&p.speed) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "speed must be 1 to 4000; pause is item.scroll_pause",
                    ));
                }
                host.api_scroll_control(p.item, p.canvas, ScrollControl::Speed(f64::from(p.speed)))
            }
            C::EditUndo | C::EditRedo => {
                let done = host.undo_or_redo(matches!(command, C::EditUndo));
                host.apply(notify);
                done.map_err(|(empty, why)| {
                    let kind = if empty {
                        ErrorKind::Conflict
                    } else {
                        ErrorKind::UndoConflict
                    };
                    ApiError::new(kind, why)
                })?;
                Ok(Response::Done(Empty {}))
            }
            C::OutputSetWithVertical(p) => {
                let flags = host.vertical_flags.set(p.kind, p.on);
                if flags != host.vertical_flags {
                    host.vertical_flags = flags;
                    notify(Update::VerticalFlags(flags));
                    host.settings_changed();
                }
                Ok(Response::Done(Empty {}))
            }
            C::OutputProfiles => Ok(Response::Profiles(ProfileList {
                profiles: vec![host.profile.to_api_listing()],
            })),
            C::RecordStart(p) => {
                check_profile(&p.profile)?;
                let gate = start_gate(&p, host.limit_policy)?;
                match p.canvas.unwrap_or_default().single() {
                    Some(one) => {
                        let c = host.api_canvas_of(Some(one))?;
                        let path = host.recording_file(c);
                        let iso = p.iso.unwrap_or(host.separate_files);
                        self.record(c, path, false, iso, gate)
                    }
                    None => {
                        let iso = p.iso.unwrap_or(host.separate_files);
                        self.start_all(api::OutputKind::Record, iso, gate)
                    }
                }
            }
            C::RecordEstimate(p) => {
                check_profile(&p.profile)?;
                let target = p.canvas.unwrap_or_default();
                let (c, all) = match target.single() {
                    Some(one) => (host.api_canvas_of(Some(one))?, false),
                    None => (OutputCanvas::Main, true),
                };
                host.record_estimate(c, all, p.iso)
                    .map(Response::RecordEstimate)
                    .map_err(failed)
            }
            C::RecordStartToFile(p) => {
                check_profile(&p.profile)?;
                let c = host.api_canvas_of(p.canvas)?;
                let path = PathBuf::from(&p.path);
                if !path.is_absolute() {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!("{} is not a full path", p.path),
                    ));
                }
                self.record(c, path, true, false, StartGate::Older)
            }
            C::RecordStop(p) => {
                let Some(one) = p.target().single() else {
                    return self.stop_all(api::OutputKind::Record);
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.outs[slot(c)].recording.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
                }
                host.stop_recording(c, None, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::RecordPause(p) => self.pause_asked(p, Some(true)),
            C::RecordResume(p) => self.pause_asked(p, Some(false)),
            C::RecordPauseToggle(p) => self.pause_asked(p, None),
            C::StreamHold(p) => self.hold_asked(p.target(), Some(true), p),
            C::StreamResume(p) => self.hold_asked(
                p.target(),
                Some(false),
                rbs_api::request::StreamHold::default(),
            ),
            C::StreamHoldToggle(p) => self.hold_asked(p.target(), None, p),
            C::OutputBreakToggle => self.break_toggle(),
            C::MarkerAdd(p) => {
                let added = host.add_marker(&p)?;
                notify(host.markers_update(Some(&added)));
                Ok(Response::MarkersAdded(added))
            }
            C::MarkerRemove(p) => {
                host.remove_marker(p.canvas, p.index)?;
                notify(host.markers_update(None));
                Ok(Response::Done(Empty {}))
            }
            C::ProjectExport(p) => host
                .export_project(&p.recording, p.format)
                .map(Response::ProjectExported),
            C::RecordStatus(p) => {
                let c = host.api_canvas_of(p.canvas)?;
                let data = host.outs[slot(c)].record_state.clone();
                Ok(match c {
                    OutputCanvas::Main => Response::RecordStatus(api::Snapshot {
                        seq: host.events.seq_map([Topic::RecordState]),
                        data,
                    }),
                    OutputCanvas::Vertical => Response::RecordCanvasStatus(api::Snapshot {
                        seq: host.canvas_seq(Topic::RecordCanvasState, c),
                        data: api::CanvasRecordState {
                            canvas: c,
                            state: data,
                        },
                    }),
                })
            }
            C::StreamStart(p) => {
                let Some(one) = p.target().single() else {
                    return self.start_all(api::OutputKind::Stream, false, StartGate::Older);
                };
                let c = host.api_canvas_of(Some(one))?;
                self.stream(c)
            }
            C::StreamStop(p) => {
                let Some(one) = p.target().single() else {
                    return self.stop_all(api::OutputKind::Stream);
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.outs[slot(c)].streaming.is_none() {
                    return Err(ApiError::new(ErrorKind::Conflict, "nothing streams"));
                }
                host.end_stream_asked(c, notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::StreamDestinations(p) => {
                let canvases = match p.canvas {
                    Some(c) => vec![host.api_canvas_of(Some(c))?],
                    None if host.vertical.is_some() => {
                        vec![OutputCanvas::Main, OutputCanvas::Vertical]
                    }
                    None => vec![OutputCanvas::Main],
                };
                Ok(Response::StreamDestinations(api::StreamDestinations {
                    destinations: canvases
                        .into_iter()
                        .flat_map(|c| host.api_destinations(c))
                        .collect(),
                }))
            }
            C::StreamDestinationStart(p) => self.destination_start(p.id),
            C::StreamDestinationStop(p) => self.destination_stop(p.id),
            C::StreamDestinationSetEnabled(p) => {
                if host.destination_canvas(p.id).is_none() {
                    return Err(ApiError::new(
                        ErrorKind::NotFound,
                        format!("there is no destination {}", p.id),
                    ));
                }
                if host.destinations.set_enabled(p.id, p.enabled) {
                    notify(Update::StreamDestinations(host.destinations.clone()));
                }
                Ok(Response::Done(Empty {}))
            }
            C::StreamStatus(p) => {
                let c = host.api_canvas_of(p.canvas)?;
                let data = host.outs[slot(c)].stream_state.clone();
                Ok(match c {
                    OutputCanvas::Main => Response::StreamStatus(api::Snapshot {
                        seq: host.events.seq_map([Topic::StreamState]),
                        data,
                    }),
                    OutputCanvas::Vertical => Response::StreamCanvasStatus(api::Snapshot {
                        seq: host.canvas_seq(Topic::StreamCanvasState, c),
                        data: api::CanvasStreamState {
                            canvas: c,
                            state: data,
                        },
                    }),
                })
            }
            C::VcamStatus => Ok(Response::VcamStatus(api::Snapshot {
                // The state as it is now, so the data and its `seq` match.
                seq: {
                    host.camera_changed();
                    host.events.seq_map([Topic::VcamChanged])
                },
                data: host.api_camera(true),
            })),
            C::VcamStart => {
                if host.camera.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera already sends",
                    ));
                }
                if host.camera_factory.is_none() {
                    return Err(failed(
                        "the virtual camera is not available on this computer",
                    ));
                }
                let Some(setup) = &host.camera_setup else {
                    return Err(failed("the studio is still starting; try again"));
                };
                if setup.busy {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera is being changed in Settings; try again",
                    ));
                }
                match &setup.state {
                    Some(InstallState::NotInstalled) => {
                        return Err(failed(
                            "the virtual camera is not added: add it in Settings, Virtual camera",
                        ));
                    }
                    Some(InstallState::NeedsUpdate { .. }) => {
                        return Err(failed(
                            "the virtual camera needs an update: add it again in Settings, Virtual camera",
                        ));
                    }
                    Some(InstallState::Unsupported { reason }) => {
                        return Err(failed(format!(
                            "the virtual camera is not available on this computer: {reason}"
                        )));
                    }
                    // Not read yet: starting says whether it is added.
                    _ => {}
                }
                let settings = VirtualCameraSettings::new(setup.width, setup.height, setup.fps);
                host.start_camera_asked(settings, notify, false)
                    .map_err(failed)?;
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::VcamStop => {
                if host.camera.is_none() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the virtual camera does not send",
                    ));
                }
                host.stop_camera(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationRun(p) => {
                check_profile(&p.profile)?;
                let Some(one) = p.canvas.unwrap_or_default().single() else {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "one canvas per run: main or vertical",
                    ));
                };
                let c = host.api_canvas_of(Some(one))?;
                if host.switch_pending.is_some() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the studio is restarting",
                    ));
                }
                host.run_calibration(c).map_err(|e| match e {
                    calibration::RunRefusal::Conflict(why) => {
                        ApiError::new(ErrorKind::Conflict, why)
                    }
                    calibration::RunRefusal::NotFound(why) => {
                        ApiError::new(ErrorKind::NotFound, why)
                    }
                    calibration::RunRefusal::Failed(why) => failed(why),
                })?;
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationCancel => {
                host.cancel_calibration();
                Ok(Response::Done(Empty {}))
            }
            C::CalibrationStatus => Ok(Response::CalibrationStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::CalibrationState]),
                data: host.api_calibration(),
            })),
            C::ProtectionStatus => Ok(Response::ProtectionStatus(api::Snapshot {
                seq: host.events.seq_map([Topic::ProtectionChanged]),
                data: host.api_protection(),
            })),
            C::ReplayStatus => Ok(Response::ReplayStatus(api::Snapshot {
                // The state as it is now, so the data and its `seq` match.
                seq: {
                    host.replay_api_changed();
                    host.events.seq_map([Topic::ReplayChanged])
                },
                data: host.api_replay(),
            })),
            C::ReplayArm => {
                if host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is already armed",
                    ));
                }
                host.arm_replay(notify).map_err(failed)?;
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplayDisarm => {
                if !host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is not armed",
                    ));
                }
                host.disarm_replay(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::ReplaySave(p) => {
                if let Some(seconds) = p.seconds
                    && !(1..=host.api_replay().length_secs).contains(&seconds)
                {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!(
                            "seconds must be 1 to {} (the buffer's length)",
                            host.api_replay().length_secs
                        ),
                    ));
                }
                if !host.studio.replay_armed() {
                    return Err(ApiError::new(
                        ErrorKind::Conflict,
                        "the replay buffer is not armed",
                    ));
                }
                // The file is written on a worker: the answer says the clip was
                // requested, and `replay.changed` carries where it went.
                host.save_replay(p.seconds)
                    .map_err(|why| ApiError::new(ErrorKind::Conflict, why))?;
                host.replay_changed(notify);
                host.publish(notify);
                Ok(Response::Done(Empty {}))
            }
            C::SettingsGet => Ok(Response::Settings(api::Snapshot {
                seq: host.events.seq_map([Topic::SettingsChanged]),
                data: host.api_settings(),
            })),
            C::SettingsSet(patch) => {
                let profile = if let Some(profiles) = &patch.profiles {
                    let [p] = profiles.as_slice() else {
                        return Err(ApiError::new(
                            ErrorKind::InvalidParams,
                            "this server has exactly one recording profile",
                        ));
                    };
                    check_profile(&p.id)?;
                    let profile = RecordProfile::from_api(p);
                    if let Some(why) = profile.problem() {
                        return Err(ApiError::new(ErrorKind::InvalidParams, why));
                    }
                    output_size_for_canvas(profile.output_size, host.main_canvas_size())?;
                    // A vertical recording without settings of its own runs on these.
                    let uses = host.outs[0].recording.is_some()
                        || (host.vertical_profile.is_none()
                            && host.outs[slot(OutputCanvas::Vertical)].recording.is_some());
                    if uses && profile != host.profile {
                        return Err(ApiError::new(
                            ErrorKind::Conflict,
                            "stop the recording before changing its settings",
                        ));
                    }
                    Some(profile)
                } else {
                    None
                };
                let stream = match &patch.stream {
                    Some(patch) => {
                        let Some(current) = &host.stream_profile else {
                            return Err(failed("the studio is still starting; try again"));
                        };
                        let stream = current
                            .with_api(patch)
                            .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
                        output_size_for_canvas(stream.output_size, host.main_canvas_size())?;
                        if host.outs[0].streaming.is_some() && stream != *current {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing its settings",
                            ));
                        }
                        Some(stream)
                    }
                    None => None,
                };
                if let Some(l) = &patch.loudness
                    && let Some(why) =
                        crate::loudness_settings::problem(l.target, l.true_peak_limit)
                {
                    return Err(ApiError::new(ErrorKind::InvalidParams, why));
                }
                // A canvas change is checked here; applying it is the part that can
                // still fail, and then nothing of the patch has been applied.
                let canvas = match &patch.canvas {
                    Some(canvas) => {
                        let c = canvas_profile(canvas)?;
                        let same = host
                            .collection
                            .canvas(host.canvas)
                            .is_some_and(|k| k.size == c.size() && k.frame_rate == c.frame_rate());
                        if same {
                            None
                        } else if host.any_recording() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "stop the recording before changing the canvas",
                            ));
                        } else if host.any_streaming() {
                            return Err(ApiError::new(
                                ErrorKind::Conflict,
                                "end the stream before changing the canvas",
                            ));
                        } else {
                            Some(c)
                        }
                    }
                    None => None,
                };
                // The loudness file is written before anything is applied (a failed
                // save changes nothing); a canvas change that then fails puts it back.
                let loudness = match &patch.loudness {
                    Some(l) => host
                        .save_loudness_settings(l.target, l.true_peak_limit)
                        .map_err(|e| refused(&e))?,
                    None => None,
                };
                if let Some(c) = canvas
                    && let Err(e) = host.change_canvas(c.size(), c.frame_rate(), notify)
                {
                    if loudness.is_some() {
                        host.undo_loudness_save();
                    }
                    return Err(failed(e));
                }
                if let Some(profile) = profile
                    && profile != host.profile
                {
                    host.profile = profile.clone();
                    notify(Update::Profile(profile));
                    host.settings_changed();
                }
                if let Some(stream) = stream
                    && host.stream_profile.as_ref() != Some(&stream)
                {
                    host.stream_profile = Some(stream.clone());
                    notify(Update::StreamProfile(stream));
                    host.settings_changed();
                }
                if let Some(saved) = loudness {
                    host.apply_loudness_settings(saved);
                    host.publish(notify);
                }
                Ok(Response::Settings(api::Snapshot {
                    seq: host.events.seq_map([Topic::SettingsChanged]),
                    data: host.api_settings(),
                }))
            }
            C::ProfileList => host.library_list(false),
            C::CollectionList => host.library_list(true),
            C::ProfileSwitch(r) => host.library_switch(false, r.id, notify),
            C::CollectionSwitch(r) => host.library_switch(true, r.id, notify),
            C::ImportObsScan => obs_import_api::scan(),
            C::ImportObsCheck(p) => obs_import_api::check(&p),
            C::ImportObsRun(p) => host.import_obs(&p, notify),
            C::AddonList => Ok(Response::AddOnList(host.addon_list())),
            C::AddonInstall(r) => host
                .addon_install(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::AddonCancel(r) => host.addon_cancel(&r.id).map(|()| Response::Done(Empty {})),
            C::AddonClean(r) => host
                .addon_clean(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::AddonRemove(r) => host
                .addon_remove(&r.id, notify)
                .map(|()| Response::Done(Empty {})),
            C::CollectionSetGlide(p) => {
                let easing = match p.easing {
                    api::GlideEasing::Linear => 0,
                    api::GlideEasing::EaseInOut => 1,
                };
                let changed = host
                    .set_glide(p.duration_ms, easing)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SceneSetTransition(p) => {
                let t = p.transition.map(|t| rbs_scene::SceneTransition {
                    kind: t.kind.map(scene_transition_kind),
                    duration_ms: t.duration_ms,
                });
                let changed = host
                    .set_scene_transition(SceneId::from_raw(p.scene), t)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::CollectionSetTransition(p) => {
                let changed = host.set_transition(&p).map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::TransitionSet(rbs_api::request::TransitionSet {
                    alpha_unsupported: host.stinger_alpha_unsupported(),
                }))
            }
            C::SourceSetGlide(p) => {
                let (custom, duration_ms, easing) = match p.glide {
                    None => (false, 0, 0),
                    Some(g) => (
                        true,
                        g.duration_ms,
                        match g.easing {
                            api::GlideEasing::Linear => 0,
                            api::GlideEasing::EaseInOut => 1,
                        },
                    ),
                };
                let changed = host
                    .set_source_glide(SourceId::from_raw(p.source), custom, duration_ms, easing)
                    .map_err(|e| refused(&e))?;
                if changed {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetRegion(p) => {
                let region = p.region.map(SourceRegion::from_array);
                if host
                    .set_capture_region(SourceId::from_raw(p.source), region)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetRecordSeparately(p) => {
                if host
                    .set_record_separately(SourceId::from_raw(p.source), RecordPatch::from_api(&p))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetRegion(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                if SourceKind::of(&source.type_id).is_some_and(|kind| kind.is_audio()) {
                    return Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        "A sound source has no picture.",
                    ));
                }
                Ok(Response::SourceRegion(SourceRegionInfo {
                    source: p.source,
                    region: source.region.map(SourceRegion::to_array),
                }))
            }
            C::SourceSetChrome(p) => {
                let chrome = p.chrome.map(|spec| {
                    let fill = match spec.fill {
                        rbs_api::request::ChromeFill::Cover => ChromeFill::Cover,
                        rbs_api::request::ChromeFill::Fit => ChromeFill::Fit,
                    };
                    DisplayChrome::new(spec.top, spec.right, spec.bottom, spec.left).with_fill(fill)
                });
                if host
                    .set_display_chrome(SourceId::from_raw(p.source), chrome)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceSetActivity(p) => {
                let activity = rbs_scene::activity::SourceActivity::new(
                    p.run.map(activity_run),
                    p.on_return.map(activity_return),
                    p.linger_s,
                );
                if host
                    .set_source_activity(SourceId::from_raw(p.source), activity)
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceGetActivity(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                let traits = rbs_sources::activity_traits(&source.type_id);
                let saved = source.activity;
                let effective = traits.effective(Some(&saved));
                Ok(Response::SourceActivity(
                    rbs_api::request::SourceActivityInfo {
                        source: p.source,
                        run: saved.run.map(api_run),
                        on_return: saved.on_return.map(api_return),
                        linger_s: saved.linger_s,
                        effective: rbs_api::request::EffectiveActivity {
                            run: api_run(effective.run),
                            on_return: effective.on_return.map(api_return),
                            linger_s: effective.linger_s,
                        },
                        supported: saved.is_default() || traits.allows(&saved),
                        runs: traits.runs.iter().copied().map(api_run).collect(),
                        on_returns: traits.on_returns.iter().copied().map(api_return).collect(),
                    },
                ))
            }
            C::SourceGetChrome(p) => {
                let source = host
                    .collection
                    .source(SourceId::from_raw(p.source))
                    .ok_or_else(|| {
                        ApiError::new(
                            ErrorKind::NotFound,
                            format!("There is no source {}.", p.source),
                        )
                    })?;
                Ok(Response::SourceChrome(SourceChromeInfo {
                    source: p.source,
                    chrome: source.chrome.map(|chrome| ChromeSpec {
                        top: chrome.top,
                        right: chrome.right,
                        bottom: chrome.bottom,
                        left: chrome.left,
                        fill: match chrome.fill {
                            ChromeFill::Cover => rbs_api::request::ChromeFill::Cover,
                            ChromeFill::Fit => rbs_api::request::ChromeFill::Fit,
                            _ => rbs_api::request::ChromeFill::Cover,
                        },
                    }),
                }))
            }
            C::SourceFitChrome(p) => {
                if host
                    .fit_display_chrome(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            C::SourceResetChrome(p) => {
                if host
                    .reset_display_chrome(SourceId::from_raw(p.source))
                    .map_err(|e| refused(&e))?
                {
                    host.apply(notify);
                }
                Ok(Response::Done(Empty {}))
            }
            // Answered by the endpoint; never reaches a target.
            // `tokens.*` are answered before (`WithTokens`) when the API was attached
            // with the settings.
            C::EventsSubscribe(_) | C::TokensList | C::TokensCreate(_) | C::TokensRevoke(_) => {
                Err(rbs_api::control::unsupported(&command))
            }
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
            && let Some(why) = &host.cannot_record
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
            OutputCanvas::Main => failed(why),
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
