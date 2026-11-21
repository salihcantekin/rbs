//! A source's settings, media and browser controls, and its capture choices (ADR-0159 decision 8).

use super::*;

impl Host {
    /// What the Source settings window shows for a source.
    pub(super) fn source_settings_view(&self, id: SourceId) -> Result<SourceSettingsView, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let kind = SourceKind::of(&source.type_id)
            .ok_or_else(|| Refusal::Invalid("This source has no settings window.".to_owned()))?;
        let int = |key: &str| match source.settings.get(key) {
            Some(SettingValue::Int(n)) => u32::try_from(*n).ok(),
            _ => None,
        };
        let text = |key: &str| match source.settings.get(key) {
            Some(SettingValue::Text(t)) => Some(t.clone()),
            _ => None,
        };
        let modes = match (kind, text(keys::TARGET)) {
            (SourceKind::Camera, Some(target)) => self
                .adapters
                .sources
                .targets(&kind.type_id())
                .unwrap_or_default()
                .into_iter()
                .find(|t| t.target == target)
                .map(|t| t.modes)
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let (cap_has, cap_l, cap_t, cap_r, cap_b) = match source.region {
            Some(r) => (true, r.left, r.top, r.right, r.bottom),
            None => (false, 0, 0, 0, 0),
        };
        let (chrome_on, chrome_t, chrome_r, chrome_b, chrome_l, chrome_fill) = match source.chrome {
            Some(c) => (
                true,
                c.top,
                c.right,
                c.bottom,
                c.left,
                match c.fill {
                    ChromeFill::Cover => 0,
                    ChromeFill::Fit => 1,
                    _ => 0,
                },
            ),
            None => (false, 0, 0, 0, 0, 0),
        };
        let has_work_rect = kind == SourceKind::Screen && self.display_work(id).is_some();
        Ok(SourceSettingsView {
            id,
            name: source.name.clone(),
            kind,
            modes,
            size: int(keys::WIDTH).zip(int(keys::HEIGHT)),
            fps: int(keys::FPS),
            cursor: !matches!(
                source.settings.get(keys::CURSOR),
                Some(SettingValue::Bool(false))
            ),
            cap_has,
            cap_l,
            cap_t,
            cap_r,
            cap_b,
            chrome_on,
            chrome_t,
            chrome_r,
            chrome_b,
            chrome_l,
            chrome_fill,
            has_work_rect,
            pic_w: self.sizes.get(&id).map(|(w, _)| *w).unwrap_or(0),
            pic_h: self.sizes.get(&id).map(|(_, h)| *h).unwrap_or(0),
        })
    }

    /// Capture, chrome and timed-show fields for a sources list row.
    pub(super) fn source_row_extra(&self, source: &Source, item: ItemId) -> RowExtra {
        let (cap_has, cap_l, cap_t, cap_r, cap_b) = match source.region {
            Some(r) => (true, r.left, r.top, r.right, r.bottom),
            None => (false, 0, 0, 0, 0),
        };
        let (chrome_on, chrome_t, chrome_r, chrome_b, chrome_l, chrome_fill) = match source.chrome {
            Some(c) => (
                true,
                c.top,
                c.right,
                c.bottom,
                c.left,
                match c.fill {
                    ChromeFill::Cover => 0,
                    ChromeFill::Fit => 1,
                    _ => 0,
                },
            ),
            None => (false, 0, 0, 0, 0, 0),
        };
        let has_work_rect = source.type_id.as_str() == rbs_scene::DISPLAY_SOURCE_TYPE
            && self.display_work(source.id).is_some();
        let show_for_ms = self
            .collection
            .item(item)
            .and_then(|(_, i)| i.show_for_ms)
            .unwrap_or(0);
        (
            cap_has,
            cap_l,
            cap_t,
            cap_r,
            cap_b,
            chrome_on,
            chrome_t,
            chrome_r,
            chrome_b,
            chrome_l,
            chrome_fill,
            has_work_rect,
            show_for_ms,
        )
    }

    /// Whether a live pause holds `item`'s scroll on the main and the vertical canvas.
    pub(super) fn scroll_paused_of(&self, item: &SceneItem) -> [bool; 2] {
        let paused = |canvas: Option<CanvasId>| {
            canvas.is_some_and(|c| {
                self.collection
                    .placed_layout(c, item.layout_on(c))
                    .scroll
                    .is_some()
                    && self.studio.scroll_paused(item.id, c)
            })
        };
        [paused(Some(self.canvas)), paused(self.vertical_edits())]
    }

    /// A source's stored settings as JSON values (`source.get_settings`).
    pub(super) fn source_settings_json(
        &self,
        id: SourceId,
    ) -> Result<BTreeMap<String, serde_json::Value>, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        Ok(source
            .settings
            .iter()
            .map(|(k, v)| {
                let v = match v {
                    SettingValue::Bool(b) => serde_json::Value::Bool(*b),
                    SettingValue::Int(n) => serde_json::Value::from(*n),
                    SettingValue::Float(f) => serde_json::Value::from(*f),
                    SettingValue::Text(t) => serde_json::Value::String(t.clone()),
                    _ => serde_json::Value::Null,
                };
                (k.clone(), v)
            })
            .chain(
                // The deprecated spelling of a film's `on_return`, computed from the
                // policy until the next major protocol (ADR-0152 decision 10).
                (source.type_id.as_str() == types::MEDIA_SOURCE).then(|| {
                    (
                        keys::RESTART_WHEN_SHOWN.to_owned(),
                        serde_json::Value::Bool(restarts_when_shown(source)),
                    )
                }),
            )
            .chain(
                // The deprecated spelling of a page's `run: placed` (ADR-0152 decision
                // 10, ADR-0149 decision 5).
                (SourceKind::of(&source.type_id) == Some(SourceKind::Browser)).then(|| {
                    (
                        keys::KEEP_RUNNING.to_owned(),
                        serde_json::Value::Bool(keeps_running(source)),
                    )
                }),
            )
            .collect())
    }

    /// Changes a source's stored settings by a patch (`source.set_settings`, ADR-0077): a
    /// `null` removes a key; the result is validated for the source's type, nothing is
    /// written when it is refused, and a change is one undo step. The source opens again
    /// with the new settings (filters, layout and scenes stay). `Ok(false)` when nothing
    /// changes.
    pub(super) fn patch_source_settings(
        &mut self,
        id: SourceId,
        patch: &BTreeMap<String, serde_json::Value>,
    ) -> Result<bool, Refusal> {
        self.patch_source_settings_in(id, patch, 0)
    }

    /// [`Self::patch_source_settings`] as part of `gesture`: patches that share a
    /// non-zero gesture are one undo step (a dragged field, design system NumberField).
    pub(super) fn patch_source_settings_in(
        &mut self,
        id: SourceId,
        patch: &BTreeMap<String, serde_json::Value>,
        gesture: u64,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let (name, type_id) = (source.name.clone(), source.type_id.clone());
        let old_device = device_sync::device_key(source);
        let mut settings = source.settings.clone();
        let mut activity = source.activity;
        let browser = SourceKind::of(&type_id) == Some(SourceKind::Browser);
        // A kept profile's id is RBS's to set, never a client's (ADR-0132 decision 10).
        if browser && patch.contains_key(keys::PROFILE_ID) {
            return Err(Refusal::Invalid(
                "\"profile_id\" is set by RBS when \"keep_signed_in\" turns on.".to_owned(),
            ));
        }
        for (key, value) in patch {
            match value {
                serde_json::Value::Null => {
                    settings.remove(key);
                }
                serde_json::Value::Bool(b) => {
                    settings.insert(key.clone(), SettingValue::Bool(*b));
                }
                serde_json::Value::Number(n) => {
                    let v = match n.as_i64() {
                        Some(i) => SettingValue::Int(i),
                        None => SettingValue::Float(n.as_f64().unwrap_or(f64::NAN)),
                    };
                    if matches!(v, SettingValue::Float(f) if !f.is_finite()) {
                        return Err(Refusal::Invalid(format!(
                            "{key:?} must be a finite number."
                        )));
                    }
                    settings.insert(key.clone(), v);
                }
                serde_json::Value::String(t) => {
                    settings.insert(key.clone(), SettingValue::Text(t.clone()));
                }
                _ => {
                    return Err(Refusal::Invalid(format!(
                        "{key:?} must be a flag, a number, text or null."
                    )));
                }
            }
        }
        // A film's `restart_when_shown` is the deprecated spelling of its activity
        // `on_return` (ADR-0152 decision 10): written to the policy, never stored.
        if type_id.as_str() == types::MEDIA_SOURCE {
            match settings.remove(keys::RESTART_WHEN_SHOWN) {
                Some(SettingValue::Bool(restart)) => {
                    activity.on_return = Some(if restart {
                        rbs_scene::activity::OnReturn::Restart
                    } else {
                        rbs_scene::activity::OnReturn::Resume
                    });
                }
                Some(_) => {
                    return Err(Refusal::Invalid(
                        "\"restart_when_shown\" must be a flag.".to_owned(),
                    ));
                }
                None if patch.get(keys::RESTART_WHEN_SHOWN) == Some(&serde_json::Value::Null) => {
                    activity.on_return = None;
                }
                None => {}
            }
        }
        // A page's `keep_running` is the deprecated spelling of `run: placed`
        // (ADR-0152 decision 10): true sets it; false or null takes it back to the
        // type's default, and leaves any other `run` alone.
        if browser {
            match settings.remove(keys::KEEP_RUNNING) {
                Some(SettingValue::Bool(true)) => {
                    activity.run = Some(rbs_scene::activity::Run::Placed);
                }
                Some(SettingValue::Bool(false)) => {
                    if activity.run == Some(rbs_scene::activity::Run::Placed) {
                        activity.run = None;
                    }
                }
                Some(_) => {
                    return Err(Refusal::Invalid(
                        "\"keep_running\" must be a flag.".to_owned(),
                    ));
                }
                None if patch.get(keys::KEEP_RUNNING) == Some(&serde_json::Value::Null)
                    && activity.run == Some(rbs_scene::activity::Run::Placed) =>
                {
                    activity.run = None;
                }
                None => {}
            }
        }
        if browser
            && settings.get(keys::KEEP_SIGNED_IN) == Some(&SettingValue::Bool(true))
            && !settings.contains_key(keys::PROFILE_ID)
        {
            settings.insert(
                keys::PROFILE_ID.to_owned(),
                SettingValue::Text(new_profile_id()?),
            );
        }
        rbs_sources::validate_settings(&type_id, &settings)
            .map_err(|e| Refusal::Invalid(e.to_string()))?;
        let (settings_changed, activity_changed) =
            self.collection.source(id).map_or((false, false), |s| {
                (s.settings != settings, s.activity != activity)
            });
        if !settings_changed && !activity_changed {
            return Ok(false);
        }
        // A separate picture track keeps its size for the whole take (ADR-0129
        // decision 4): a text's box does not change while one is written.
        if rbs_sources::types::class_of(&type_id) == Some(rbs_sources::types::SourceClass::Text)
            && self.studio.iso_video_records(id)
            && self.collection.source(id).is_some_and(|s| {
                [
                    rbs_sources::types::keys::WIDTH,
                    rbs_sources::types::keys::HEIGHT,
                ]
                .iter()
                .any(|k| s.settings.get(*k) != settings.get(*k))
            })
        {
            return Err(Refusal::Invalid(format!("{name}: {STOP_TO_CHANGE_BOX}")));
        }
        let mut undos = Vec::new();
        if activity_changed {
            undos.push(
                self.collection
                    .set_source_activity(id, activity)
                    .map_err(|e| Refusal::of(&e))?,
            );
        }
        if !settings_changed {
            self.history
                .record(EditKind::SourceSettings, &name, gesture, undos);
            return Ok(true);
        }
        undos.push(
            self.collection
                .set_source_settings(id, settings)
                .map_err(|e| Refusal::of(&e))?,
        );
        // Marks follow stillness (ADR-0129 decision 1): a text that starts to move
        // loses its `video_as: image` in the same undo step.
        if let Some(mark) = self.collection.source(id).and_then(|s| {
            rbs_sources::mark_after_edit(&s.type_id, &s.settings, s.record_separately)
        }) {
            undos.push(
                self.collection
                    .set_source_record_separately(id, mark)
                    .map_err(|e| Refusal::of(&e))?,
            );
        }
        self.history
            .record(EditKind::SourceSettings, &name, gesture, undos);
        self.fill_device_offset(id, old_device.as_ref(), &name, EditKind::SourceSettings);
        Ok(true)
    }

    /// Runs one playback command on a media-file source and reports its state
    /// (protocol 1.74, ADR-0108 decision 10). Live actions: the document does not
    /// change and nothing is an undo step. A source of another type is refused.
    pub(super) fn media_command(
        &mut self,
        raw: u32,
        op: MediaOp,
    ) -> Result<rbs_api::request::MediaStateInfo, Refusal> {
        use rbs_api::request::{MediaPlayState as Wire, MediaStateInfo};
        use rbs_record::MediaPlayState as State;
        let id = SourceId::from_raw(raw);
        let Some(source) = self.collection.source(id) else {
            return Err(Refusal::NotFound(format!("There is no source {raw}.")));
        };
        if source.type_id.as_str() != types::MEDIA_SOURCE {
            return Err(Refusal::Invalid(format!(
                "Source {raw} is not a media file."
            )));
        }
        let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        let Some(control) = self.studio.media_control(id) else {
            // No instance yet: the source is in no scene the studio runs.
            if matches!(op, MediaOp::State) {
                return Ok(MediaStateInfo {
                    source: raw,
                    state: Wire::Standby,
                    cursor_ms: 0,
                    duration_ms: None,
                });
            }
            return Err(Refusal::Conflict(format!(
                "Media source {raw} is not in a scene that is shown."
            )));
        };
        if !matches!(op, MediaOp::State) && self.studio.stinger_armed(id) {
            // Played by a transition until it ends (ADR-0140 decision 5).
            return Err(Refusal::Conflict(format!(
                "Media source {raw} is playing as the stinger."
            )));
        }
        let failed = |e: rbs_record::MediaNodeError| Refusal::Failed(e.to_string());
        match op {
            MediaOp::Play => control.play().map_err(failed)?,
            MediaOp::Pause => control.pause().map_err(failed)?,
            MediaOp::Seek(ms) => control.seek(Duration::from_millis(ms)).map_err(failed)?,
            MediaOp::Restart => control.restart().map_err(failed)?,
            MediaOp::State => {}
        }
        let state = match control.state() {
            State::Playing => Wire::Playing,
            State::Paused => Wire::Paused,
            State::Cued => Wire::Cued,
            State::Ended => Wire::Ended,
            State::Standby => Wire::Standby,
            State::Unavailable => Wire::Unavailable,
            _ => Wire::Unknown,
        };
        Ok(MediaStateInfo {
            source: raw,
            state,
            cursor_ms: millis(control.cursor()),
            duration_ms: control.duration().map(millis),
        })
    }

    /// Sends the play state of every media source when it changed; while one plays,
    /// asks to be called again in [`MEDIA_TICK`]. Nothing runs without media (P13).
    pub(super) fn report_media(&mut self, notify: &dyn Fn(Update)) -> Option<Duration> {
        use rbs_record::MediaPlayState as State;
        let mut playing = false;
        let ticks: Vec<MediaTick> = self
            .studio
            .media_controls()
            .map(|(id, control)| {
                let state = control.state();
                playing |= state == State::Playing;
                let step = MEDIA_TICK.as_millis() as u64;
                let cursor = u64::try_from(control.cursor().as_millis()).unwrap_or(u64::MAX);
                MediaTick {
                    id,
                    state: match state {
                        State::Playing => 0,
                        State::Paused => 1,
                        State::Cued => 2,
                        State::Ended => 3,
                        State::Unavailable => 5,
                        _ => 4,
                    },
                    cursor_ms: u32::try_from(cursor / step * step).unwrap_or(u32::MAX),
                    duration_ms: control
                        .duration()
                        .map_or(0, |d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX)),
                }
            })
            .collect();
        if self.media_sent.as_ref() != Some(&ticks) {
            self.media_sent = Some(ticks.clone());
            notify(Update::Media(ticks));
        }
        playing.then_some(MEDIA_TICK)
    }

    /// `browser.reload` and `browser.run_script`: a message to the source's running
    /// page. No running page is `conflict` with reason `browser_not_running`; an
    /// agent cannot wake a hidden page (ADR-0132 decision 5, ADR-0149 decision 7).
    pub(super) fn browser_message(
        &mut self,
        source: u32,
        message: &rbs_sources::BrowserMessage,
    ) -> Result<(), ApiError> {
        if self
            .studio
            .browser_control(SourceId::from_raw(source), message)
        {
            return Ok(());
        }
        Err(self.browser_refusal(source))
    }

    /// Why a message to `source`'s page was not sent: `not_found` when it is not a
    /// browser source, else `conflict` with reason `browser_not_running`.
    pub(super) fn browser_refusal(&self, source: u32) -> ApiError {
        let browser = self
            .collection
            .source(SourceId::from_raw(source))
            .is_some_and(|s| s.type_id.as_str() == types::BROWSER_SOURCE);
        if !browser {
            return ApiError::new(
                ErrorKind::NotFound,
                format!("there is no browser source {source}"),
            );
        }
        let mut data = serde_json::Map::new();
        data.insert("reason".into(), "browser_not_running".into());
        ApiError::with_data(
            ErrorKind::Conflict,
            "the page is not running: it runs while a scene on Program or Preview shows it",
            data,
        )
    }

    /// `browser.input` (ADR-0132 decisions 6 and 9): the events, in order, to the
    /// source's running page. Checked first, so a bad event sends none of them. No
    /// running page is `conflict` with reason `browser_not_running`. Text is never
    /// logged.
    pub(super) fn browser_input(
        &mut self,
        source: u32,
        events: &[rbs_api::request::BrowserInputEvent],
    ) -> Result<(), ApiError> {
        if events.len() > rbs_api::request::BrowserInput::MAX_EVENTS {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                format!(
                    "at most {} events per call",
                    rbs_api::request::BrowserInput::MAX_EVENTS
                ),
            ));
        }
        let wire = events
            .iter()
            .map(browser_input_event)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|why| ApiError::new(ErrorKind::InvalidParams, why))?;
        let id = SourceId::from_raw(source);
        if !self.studio.browser_running(id) {
            // Not a browser source (`not_found`) or no running page (`conflict`).
            return Err(self.browser_refusal(source));
        }
        for event in wire {
            if !self
                .studio
                .browser_control(id, &rbs_sources::BrowserMessage::Input(event))
            {
                break;
            }
        }
        Ok(())
    }

    /// Forget sign-in (ADR-0132 decision 10): the source gets a new kept-profile id,
    /// so its page starts again signed out at once, and the old profile's directory is
    /// deleted once the page that used it has closed. One undo step, which brings back
    /// only the id: the profile itself is gone. `stop_first` is the desktop app's
    /// button, which stops the running page itself; a client is refused with `busy`
    /// while the page runs (decision 9). The caller applies the collection.
    pub(super) fn forget_sign_in(
        &mut self,
        id: SourceId,
        stop_first: bool,
    ) -> Result<(), ApiError> {
        let Some(source) = self
            .collection
            .source(id)
            .filter(|s| s.type_id.as_str() == types::BROWSER_SOURCE)
        else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("there is no browser source {}", id.get()),
            ));
        };
        let (name, mut settings) = (source.name.clone(), source.settings.clone());
        let Some(old) = crate::browser_profiles::id_of(source).map(str::to_owned) else {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                format!("{name} keeps no sign-in"),
            ));
        };
        if !stop_first && self.studio.browser_running(id) {
            return Err(ApiError::new(
                ErrorKind::Busy,
                format!("{name} is running: its sign-in is forgotten once the page stops"),
            ));
        }
        let new = crate::browser_profiles::new_id()
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
        settings.insert(keys::PROFILE_ID.to_owned(), SettingValue::Text(new));
        let undo = self
            .collection
            .set_source_settings(id, settings)
            .map_err(|e| ApiError::new(ErrorKind::Internal, e.to_string()))?;
        self.history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
        if let Some(dir) = &self.profiles_dir {
            crate::browser_profiles::remove_soon(dir, &old);
        }
        Ok(())
    }

    /// Tries a source again with its stored settings. Only when a closed and reopened
    /// window is found by its hints is something edited: the source is re-pointed (one
    /// "retarget" undo step) and the caller applies the collection, which replaces the
    /// node; otherwise it is no edit and no undo step.
    pub(super) fn reload_source(&mut self, id: SourceId) -> Result<Reload, Refusal> {
        if self.collection.source(id).is_none() {
            return Err(Refusal::NotFound(format!(
                "There is no source {}.",
                id.get()
            )));
        }
        // A window that was closed and opened again has a new id: when the stored one
        // is gone, exactly one window of the same application with the same title is
        // taken instead (ADR-0077 decision 3b); otherwise nothing is guessed.
        if let Some(target) = self.window_by_hints(id)
            && self.retarget_source(id, &target)?
        {
            return Ok(Reload::Retargeted);
        }
        Ok(Reload::Restarted(self.studio.reload_source(id)))
    }

    /// The window a window source's hints name, when its stored target is no longer
    /// listed and exactly one listed window has the same application and title.
    pub(super) fn window_by_hints(&self, id: SourceId) -> Option<Target> {
        let source = self.collection.source(id)?;
        if SourceKind::of(&source.type_id) != Some(SourceKind::Window) {
            return None;
        }
        let text = |key: &str| match source.settings.get(key) {
            Some(SettingValue::Text(t)) => Some(t.as_str()),
            _ => None,
        };
        let (adapter, stored) = (text(keys::ADAPTER)?, text(keys::TARGET)?);
        let (app, title) = (source.hints.get(HINT_APP)?, source.hints.get(HINT_TITLE)?);
        match_by_hints(
            self.targets(SourceKind::Window),
            adapter,
            stored,
            app,
            title,
        )
    }

    /// Properties' choice of `run` and `on_return` for a source (ADR-0152): the
    /// type's default is stored as absent, so a document stays at the oldest format
    /// that holds it, and the source's linger is kept.
    pub(super) fn set_source_choice(
        &mut self,
        id: SourceId,
        run: rbs_scene::activity::Run,
        on_return: Option<rbs_scene::activity::OnReturn>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let traits = rbs_sources::activity_traits(&source.type_id);
        let activity = rbs_scene::activity::SourceActivity::new(
            (run != traits.default_run).then_some(run),
            on_return.filter(|r| Some(*r) != traits.default_on_return),
            source.activity.linger_s,
        );
        self.set_source_activity(id, activity)
    }

    /// Replaces a source's activity policy (ADR-0152). One undo step (recorded as a
    /// settings change). A value the source's type does not allow is refused as
    /// invalid; the same policy again changes nothing.
    pub(super) fn set_source_activity(
        &mut self,
        id: SourceId,
        activity: rbs_scene::activity::SourceActivity,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let traits = rbs_sources::activity_traits(&source.type_id);
        if !traits.allows(&activity) {
            return Err(Refusal::Invalid(format!(
                "Source {} does not allow that activity.",
                id.get()
            )));
        }
        if source.activity == activity {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_activity(id, activity)
            .map_err(|e| Refusal::of(&e))?;
        self.history
            .record(EditKind::SourceSettings, &name, 0, vec![undo]);
        Ok(true)
    }

    /// Sets or clears a source region (ADR-0103). One undo step, not an item crop.
    /// A sound-only source is refused. When the picture size is known, a region
    /// that leaves no pixel is refused. Setting a region also places the item box
    /// with Cover so a canvas-sized region fills the canvas (settings Apply).
    pub(super) fn set_capture_region(
        &mut self,
        id: SourceId,
        region: Option<SourceRegion>,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        if SourceKind::of(&source.type_id).is_some_and(|kind| kind.is_audio()) {
            return Err(Refusal::Invalid("A sound source has no picture.".into()));
        }
        if let Some(region) = region
            && let Some(picture) = self.sizes.get(&id).copied()
            && region.size(picture).is_none()
        {
            return Err(Refusal::Invalid("The region leaves no picture.".into()));
        }
        if source.region == region {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_region(id, region)
            .map_err(|e| Refusal::of(&e))?;
        let mut steps = vec![undo];
        if region.is_some()
            && let Ok(Some(fit)) = self.fit_item_to_visible(id, FitMode::Fill)
        {
            steps.push(fit);
        }
        self.history.record(EditKind::Region, &name, 0, steps);
        Ok(true)
    }

    /// The window commands of this concern, routed by `Host::command`.
    pub(super) fn source_edit_command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::AddFilter { source, type_id } => {
                let done = self
                    .add_filter(source, &type_id, &std::collections::BTreeMap::new())
                    .map(|_| true);
                self.answer(done, notify);
            }
            Command::AddVoiceChain(source) => {
                let done = self.add_voice_chain(source);
                self.answer(done, notify);
            }
            Command::AddSample { sample } => {
                self.run_ui_api(
                    rbs_api::Command::SourceAddSample(rbs_api::request::AddSample { sample }),
                    notify,
                );
            }
            Command::AddStamp { kind } => match self.add_stamp(kind) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::AddImage { path } => match self.add_image_file(&path) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::AddMedia { path } => match self.add_media_file(&path) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::SetStingerFile(path) => match self.set_stinger_file(&path) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::FitTextBox(id) => {
                let done = self.fit_text_box(id);
                self.answer(done, notify);
            }
            Command::ListTextFonts => self.list_text_fonts(notify),
            Command::TextTimer { source, action } => match self.studio.text_timer(source, action) {
                Ok(_) => self.save_text_timers(),
                Err(rbs_record::TimerRefused::NotATimer) => notify(Update::Error(
                    "Only a countdown for a length or a stopwatch has a timer.".into(),
                )),
                Err(_) => {}
            },
            Command::AddText { text } => match self.add_text(None, &text, false) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            // Without its engine Browser leads to Settings › Add-ons and adds nothing
            // (ADR-0145 decision 8).
            Command::AddBrowser { .. } if self.browser_needs_engine() => {
                notify(Update::OpenAddOn(crate::add_ons::BROWSER_ENGINE.to_owned()));
            }
            Command::AddBrowser { url } => match self.add_browser_page(&url) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::SetSourceFill { id, color } => match self.set_source_fill(id, &color) {
                Ok(_) => self.apply(notify),
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::SetSourceFilled { id, on } => match self.set_source_filled(id, on) {
                Ok(true) => self.apply(notify),
                Ok(false) => {}
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::ApplyLook { source, look } => {
                self.run_ui_api(
                    rbs_api::Command::LookApply(rbs_api::request::ApplyLook {
                        source: source.get(),
                        look,
                    }),
                    notify,
                );
            }
            Command::RemoveFilter(filter) => {
                let done = self.remove_filter(filter).map(|()| true);
                self.answer(done, notify);
            }
            Command::MoveFilter { filter, index } => {
                let done = self.move_filter(filter, index);
                self.answer(done, notify);
            }
            Command::SetFilterEnabled { filter, enabled } => {
                let done = self.set_filter_enabled(filter, enabled);
                self.answer(done, notify);
            }
            Command::SetFilterValues { filter, values } => {
                let done = self.set_filter_values(filter, values);
                // An unchanged release still puts the collection's values back on air.
                if matches!(done, Ok(false)) {
                    self.studio.restore_filter(filter);
                }
                self.answer(done, notify);
            }
            Command::PreviewFilter { filter, values } => {
                self.studio.preview_filter(filter, values);
            }
            Command::RestoreFilter(filter) => {
                self.studio.restore_filter(filter);
            }
            Command::OpenSyncMeasure(id) => self.open_sync_measure_window(id, notify),
            Command::StartSyncMeasure(reference) => {
                self.start_sync_measure_window(reference, notify);
            }
            Command::CloseSyncMeasure => self.close_sync_measure_window(notify),
            Command::ApplySyncToDeviceSources(id) => {
                let done = self.apply_to_device_sources(id).map(|n| n > 0);
                self.answer_live(done, notify);
            }
            Command::SetSourceSyncOffset { id, offset_ms } => {
                let done = self.set_source_sync_offset(id, offset_ms);
                self.answer_live(done, notify);
            }
            Command::RetargetSource { id, target } => {
                let done = self.retarget_source(id, &target);
                self.answer(done, notify);
            }
            Command::GrabThumbnails { kind, rows, max } => self.grab_thumbnails(kind, rows, max),
            Command::StopThumbnails => {
                self.thumbs.fetch_add(1, AtomicOrdering::AcqRel);
            }
            Command::OpenSourceSettings(id) => match self.source_settings_view(id) {
                Ok(view) => {
                    notify(Update::SourceSettings(view));
                    self.grab_region_preview(id);
                }
                Err(e) => notify(Update::Error(e.text().to_owned())),
            },
            Command::CloseSourceSettings => {
                self.region_grabs.fetch_add(1, AtomicOrdering::AcqRel);
            }
            Command::SetSourceSettings { id, patch, gesture } => {
                let done = self.patch_source_settings_in(id, &patch, gesture);
                self.answer(done, notify);
            }
            Command::Media { id, op } => {
                if let Err(e) = self.media_command(id.get(), op) {
                    notify(Update::Error(e.text().to_owned()));
                }
                self.media_sent = None;
            }
            Command::ReloadSource(id) => match self.reload_source(id) {
                Ok(Reload::Retargeted) => self.answer(Ok(true), notify),
                Ok(Reload::Restarted(done)) => self.answer_live(Ok(done), notify),
                Err(e) => self.answer_live(Err(e), notify),
            },
            Command::ForgetBrowserSignIn(id) => {
                let done = self
                    .forget_sign_in(id, true)
                    .map(|()| true)
                    .map_err(|e| Refusal::Conflict(e.message));
                self.answer(done, notify);
            }
            Command::BrowserControl { id, message } => {
                let browser = self
                    .collection
                    .source(id)
                    .is_some_and(|s| s.type_id.as_str() == types::BROWSER_SOURCE);
                if !browser {
                    let e = Refusal::NotFound(format!("There is no browser source {}.", id.get()));
                    self.answer_live(Err(e), notify);
                } else if self.studio.browser_control(id, &message) {
                    self.answer_live(Ok(true), notify);
                } else if matches!(message, rbs_browser_ipc::control::Control::Reload { .. }) {
                    match self.reload_source(id) {
                        Ok(Reload::Retargeted) => self.answer(Ok(true), notify),
                        Ok(Reload::Restarted(done)) => self.answer_live(Ok(done), notify),
                        Err(e) => self.answer_live(Err(e), notify),
                    }
                } else {
                    // No page runs: nothing to run the script in.
                    self.answer_live(Ok(false), notify);
                }
            }
            Command::SetCompanionAudio { source, on } => {
                let has = self
                    .collection
                    .sources()
                    .any(|s| s.companion_of() == Some(source));
                let done = match (on, has) {
                    (true, false) => self.add_companion(source, false).map(|_| true),
                    (false, true) => self.remove_companion(source).map(|()| true),
                    _ => Ok(false),
                };
                self.answer(done, notify);
            }
            // Routed elsewhere by `Host::command`.
            _ => {}
        }
    }
}
