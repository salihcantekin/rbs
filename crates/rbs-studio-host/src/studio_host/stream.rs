//! Go live: streams, their destinations and encoders, stream holds and link reports (ADR-0159 decision 8).

use super::*;

impl Host {
    /// Puts canvas `c`'s stream on hold (`Some(true)`) with `picture`, takes it back
    /// to the program (`Some(false)`), or toggles (`None`) (ADR-0124 decisions 12 to
    /// 14). A stream sharing its encoder with the recording pauses that recording
    /// first and holds once the pause has begun; Resume stream resumes only a
    /// recording its hold paused. Returns whether the stream is on hold now.
    pub(super) fn hold_stream(
        &mut self,
        c: OutputCanvas,
        hold: Option<bool>,
        asked: rbs_api::request::StreamHold,
    ) -> Result<bool, ApiError> {
        let conflict = |m: String| ApiError::new(ErrorKind::Conflict, m);
        let Some(id) = self.outs[slot(c)].streaming.as_ref().map(|s| s.2) else {
            return Err(conflict("nothing streams".into()));
        };
        let hold = hold.unwrap_or_else(|| !self.stream_held(c));
        if hold {
            let ask = self.hold_ask(c, asked)?;
            match self.hold_engine(id, ask) {
                Ok(_) => {
                    self.outs[slot(c)].hold.pending = None;
                    self.hold_own_encodings(c, Some(ask));
                }
                Err(rbs_record::HoldRefused::SharedWithRecording) => {
                    if self.outs[slot(c)].hold.pending == Some(ask) {
                        return Err(conflict("the stream is already on hold".into()));
                    }
                    let paused = self.outs[slot(c)]
                        .recording
                        .as_ref()
                        .and_then(|r| self.studio.output_pause(r.1))
                        .is_some_and(|p| p.paused);
                    if !paused {
                        self.pause_recording(c, Some(true))?;
                        self.outs[slot(c)].hold.paused_recording = true;
                    }
                    self.outs[slot(c)].hold.pending = Some(ask);
                }
                Err(e) => return Err(hold_error(e)),
            }
        } else {
            if !self.stream_held(c) {
                return Err(conflict("the stream is not on hold".into()));
            }
            if self.outs[slot(c)].hold.pending.take().is_none() {
                self.studio
                    .release_hold_scene(id, &self.adapters.sources)
                    .map_err(|e| conflict(hold_refusal(e)))?;
                self.hold_own_encodings(c, None);
            }
            if std::mem::take(&mut self.outs[slot(c)].hold.paused_recording) {
                // Ended meanwhile, or already resumed: nothing to resume.
                let _ = self.pause_recording(c, Some(false));
            }
        }
        self.refresh_stream_hold(c);
        Ok(hold)
    }

    /// The hold `asked` names, with the stream settings' "While on hold" for what it
    /// leaves out (protocols 1.65 and 1.67). A scene or a sound with a picture that
    /// is not a scene is `invalid_params`; a scene hold with no scene named or set is
    /// `conflict`.
    pub(super) fn hold_ask(
        &self,
        c: OutputCanvas,
        asked: rbs_api::request::StreamHold,
    ) -> Result<HoldAsk, ApiError> {
        use rbs_api::request::HoldPicture as P;
        let profile = self.stream_profile_of(c);
        let picture = asked
            .picture
            .unwrap_or_else(|| profile.map(|p| p.hold_picture).unwrap_or_default());
        if picture != P::Scene {
            if asked.scene.is_some() || asked.sound.is_some() {
                return Err(ApiError::new(
                    ErrorKind::InvalidParams,
                    "scene and sound go only with picture \"scene\"",
                ));
            }
            return Ok(HoldAsk {
                picture: hold_picture(picture),
                scene: None,
                sound: rbs_record::HoldSound::Silence,
            });
        }
        let scene = asked
            .scene
            .or_else(|| profile.and_then(|p| p.hold_scene))
            .ok_or_else(|| {
                ApiError::new(
                    ErrorKind::Conflict,
                    "no hold scene is set: choose one in Settings, Stream",
                )
            })?;
        let sound = asked
            .sound
            .or_else(|| profile.map(|p| p.hold_sound))
            .unwrap_or_default();
        Ok(HoldAsk {
            picture: HoldPicture::Scene,
            scene: Some(SceneId::from_raw(scene)),
            sound: hold_sound(sound),
        })
    }

    /// Asks the studio for the hold `ask`: a scene's (bound and opened first) or a
    /// picture's. A scene left by a change to another picture leaves at once.
    pub(super) fn hold_engine(
        &mut self,
        id: OutputId,
        ask: HoldAsk,
    ) -> Result<Timestamp, rbs_record::HoldRefused> {
        match ask.scene {
            Some(scene) => {
                self.studio
                    .hold_output_scene(id, scene, ask.sound, &self.adapters.sources)
            }
            None => {
                let at = self.studio.hold_output(id, ask.picture)?;
                self.studio.settle_hold_scenes(&self.adapters.sources);
                Ok(at)
            }
        }
    }

    /// Canvas `c`'s stream is on hold, or a hold waits for its recording's pause.
    pub(super) fn stream_held(&self, c: OutputCanvas) -> bool {
        let o = &self.outs[slot(c)];
        o.hold.pending.is_some()
            || o.streaming
                .as_ref()
                .and_then(|s| self.studio.output_hold(s.2))
                .is_some_and(|h| h.picture.is_some())
    }

    /// Holds a stream whose hold waits for its recording's pause, once that pause
    /// has begun. Returns how soon to look again while it still waits.
    pub(super) fn drive_hold(&mut self, c: OutputCanvas) -> Option<Duration> {
        let ask = self.outs[slot(c)].hold.pending?;
        let id = self.outs[slot(c)].streaming.as_ref()?.2;
        match self.hold_engine(id, ask) {
            Err(rbs_record::HoldRefused::SharedWithRecording) => {
                let paused = self.outs[slot(c)]
                    .recording
                    .as_ref()
                    .and_then(|r| self.studio.output_pause(r.1))
                    .is_some_and(|p| p.paused);
                if paused {
                    // The pause begins within two render periods.
                    return Some(ENDING_WAIT);
                }
                // The recording was resumed meanwhile: the hold is given up.
                self.outs[slot(c)].hold = HoldPlan::default();
            }
            Ok(_) => {
                self.outs[slot(c)].hold.pending = None;
                self.hold_own_encodings(c, Some(ask));
            }
            Err(_) => self.outs[slot(c)].hold.pending = None,
        }
        self.refresh_stream_hold(c);
        None
    }

    /// Holds (`Some`) or releases (`None`) every destination of `c` with its own
    /// encoder chain besides the anchor's, so the whole stream holds (ADR-0127 decision
    /// 13). The anchor's chain, and every destination sharing it, is held already. A
    /// refusal leaves that destination as it is: it is only one of the stream's.
    pub(super) fn hold_own_encodings(&mut self, c: OutputCanvas, ask: Option<HoldAsk>) {
        let anchor = self.outs[slot(c)].streaming.as_ref().map(|s| s.2);
        let own: Vec<OutputId> = self.outs[slot(c)]
            .destinations
            .iter()
            .filter(|d| d.own)
            .filter_map(|d| d.output)
            .filter(|id| Some(*id) != anchor)
            .collect();
        for id in own {
            match ask {
                Some(ask) => {
                    let _ = self.hold_engine(id, ask);
                }
                None => {
                    let _ = self.studio.release_hold_scene(id, &self.adapters.sources);
                }
            }
        }
    }

    /// Puts the hold fields of `c`'s live stream into its stream state, as of now
    /// (published as a change, never per frame).
    pub(super) fn refresh_stream_hold(&mut self, c: OutputCanvas) {
        let Some(id) = self.outs[slot(c)].streaming.as_ref().map(|s| s.2) else {
            return;
        };
        let pending = self.outs[slot(c)].hold.pending;
        let hold = self.studio.output_hold(id).unwrap_or_default();
        let mut state = self.outs[slot(c)].stream_state.clone();
        if let api::StreamState::Live {
            held,
            held_ms,
            hold_picture,
            hold_scene,
            hold_sound,
            hold_fallback,
            hold_sound_fallback,
            hold_fallback_reason,
            ..
        } = &mut state
        {
            *held = pending.is_some() || hold.picture.is_some();
            *held_ms = hold.held_ns / 1_000_000;
            // A hold waiting for the recording's pause says what it will send.
            let (picture, scene, sound) = match (hold.picture, pending) {
                (Some(p), _) => (Some(p), hold.scene, hold.sound),
                (None, Some(ask)) => (Some(ask.picture), ask.scene, ask.scene.map(|_| ask.sound)),
                (None, None) => (None, None, None),
            };
            *hold_picture = picture.map(api_hold_picture);
            *hold_scene = scene.map(SceneId::get);
            *hold_sound = sound.map(api_hold_sound);
            *hold_fallback = hold.fallback.is_some();
            *hold_sound_fallback = hold.sound_fallback.is_some();
            *hold_fallback_reason = hold
                .fallback
                .map(|f| format!("{f}: showing the last frame"))
                .or_else(|| {
                    hold.sound_fallback
                        .map(|f| format!("{f}: its sound is silent"))
                });
            self.set_stream_state(c, state);
        }
    }

    pub(super) fn set_stream_state(&mut self, c: OutputCanvas, state: api::StreamState) {
        // Every state says whether streaming is available on the canvas (control API
        // 1.93, ADR-0148 decision 10).
        let state = state.with_available(self.stream_available(c));
        if state == self.outs[slot(c)].stream_state {
            return;
        }
        self.outs[slot(c)].stream_state = state.clone();
        match c {
            OutputCanvas::Main => self
                .events
                .changed(StateKey::topic(Topic::StreamState), || {
                    EventPayload::StreamState(state)
                }),
            OutputCanvas::Vertical => self
                .events
                .changed(canvas_key(Topic::StreamCanvasState, c), || {
                    EventPayload::StreamCanvasState(api::CanvasStreamState { canvas: c, state })
                }),
        };
    }

    /// A stream runs on either canvas.
    pub(super) fn any_streaming(&self) -> bool {
        self.outs.iter().any(|o| o.streaming.is_some())
    }

    /// The stream settings of `c`, `None` until the UI sent them (the vertical
    /// canvas's: until they are saved).
    pub(super) fn stream_profile_of(&self, c: OutputCanvas) -> Option<&StreamProfile> {
        match c {
            OutputCanvas::Main => self.stream_profile.as_ref(),
            OutputCanvas::Vertical => self.vertical_stream_profile.as_ref(),
        }
    }

    /// The key store account of `c`'s stream key.
    pub(super) fn key_account_of(&self, c: OutputCanvas) -> String {
        match c {
            OutputCanvas::Main => self.key_account.clone(),
            OutputCanvas::Vertical => stream_key::vertical_account(&self.key_account),
        }
    }

    /// The UI's Go live for `c`: starts the stream, says how it went, and records
    /// with it when that is on.
    pub(super) fn stream_asked(
        &mut self,
        c: OutputCanvas,
        r: &StreamRequest,
        notify: &dyn Fn(Update),
    ) {
        self.note_encoders(notify);
        if let Some(why) = &self.cannot_record {
            notify(stream_update(c, StreamUpdate::NotStarted(why.clone())));
            return;
        }
        let update = match self.start_stream(c, r) {
            Ok(update) => update,
            Err(why) => StreamUpdate::NotStarted(why),
        };
        let started = matches!(update, StreamUpdate::Started { .. });
        notify(stream_update(c, update));
        if started {
            // Each destination that did not start, by name (ADR-0127 decision 7).
            for d in &self.outs[slot(c)].destinations {
                if let (DestinationState::Failed, Some(why)) = (d.state, &d.error) {
                    notify(Update::Error(format!("{}: {why}", d.label())));
                }
            }
            self.record_with_stream(c, notify);
        }
        self.publish(notify);
    }

    /// Saves (`Some`) or removes `c`'s stream key; `Ok(true)` when one is saved now.
    pub(super) fn save_key(&self, c: OutputCanvas, key: Option<&Secret>) -> Result<bool, String> {
        let account = self.key_account_of(c);
        let result = match key {
            Some(k) => self.keys.write(&account, k).map(|()| true),
            None => self.keys.remove(&account).map(|()| false),
        };
        result.map_err(|e| format!("{} refused the stream key: {e}", self.keys.place()))
    }

    /// Ends every output of both canvases because of `why` (the studio failed).
    pub(super) fn end_outputs(&mut self, why: &str, notify: &dyn Fn(Update)) {
        for c in OutputCanvas::ALL {
            self.stop_recording(c, Some(why.to_owned()), notify);
            self.end_stream(c, Some(why.to_owned()), notify);
        }
    }

    /// The user (End stream) or a control API client (`stream.stop`) ends the stream:
    /// a recording that started with it stops too.
    pub(super) fn end_stream_asked(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        let with_stream = self.outs[slot(c)].auto_recording;
        // A stream that failed before it was asked to end keeps its recording, as one
        // the round finds failed does.
        let failure = self.end_stream(c, None, notify);
        if with_stream && failure.is_none() && self.outs[slot(c)].recording.is_some() {
            self.end_recording(c, notify);
        }
    }

    /// Goes live: reads each destination's key (RTMP: required) from the key store,
    /// then attaches the stream's encoders and one output per enabled destination to
    /// the running studio (ADR-0127): destinations sharing the stream's encoding in
    /// one apply, so they start on the same keyframe, then each with its own
    /// encoding. It goes live when at least one destination starts; the others are
    /// kept as failed with their reason. The connections are made in the background;
    /// [`Host::report_link`] tells the UI how they go.
    pub(super) fn start_stream(
        &mut self,
        c: OutputCanvas,
        r: &StreamRequest,
    ) -> Result<StreamUpdate, String> {
        self.start_stream_to(c, r, None)
    }

    /// [`Host::start_stream`] to destination `only` alone when it is `Some`
    /// (`stream.destination.start` while nothing streams, ADR-0127 decision 16).
    pub(super) fn start_stream_to(
        &mut self,
        c: OutputCanvas,
        r: &StreamRequest,
        only: Option<u32>,
    ) -> Result<StreamUpdate, String> {
        if let Some(why) = self.calibration_blocks() {
            return Err(format!("{why}."));
        }
        if self.outs[slot(c)].streaming.is_some() {
            return Err("A stream is already running.".into());
        }
        let canvas = self.output_canvas(c)?;
        if self.cannot_record.is_some() {
            return Err(format!(
                "Streaming is not available on {} yet: this system has no video encoder RBS can use.",
                os_name()
            ));
        }
        let (plans, mut failed) = self.destination_plans(c, r, only);
        if plans.is_empty() {
            return Err(failed
                .first()
                .and_then(|d| d.error.clone())
                .unwrap_or_else(|| {
                    "No destination is turned on: turn one on in Settings, Stream.".into()
                }));
        }
        let alone = plans.len() == 1 && failed.is_empty();
        let mut ready: Vec<(Planned, OutputSettings)> = Vec::new();
        for p in plans {
            let key = match self.keys.read(&p.account) {
                Ok(key) => key,
                Err(e) => {
                    let which = match (p.live.id, c) {
                        (_, _) if !p.first => format!(" of {}", p.live.label()),
                        (_, OutputCanvas::Main) => String::new(),
                        (_, OutputCanvas::Vertical) => " of the vertical stream".into(),
                    };
                    let why = format!(
                        "The stream key{which} could not be read from {}: {e}",
                        self.keys.place()
                    );
                    if alone {
                        return Err(why);
                    }
                    failed.push(p.failed(why));
                    continue;
                }
            };
            if p.request.needs_key && key.is_none() {
                if alone {
                    return Ok(StreamUpdate::NoKey);
                }
                failed.push(p.failed("No stream key is saved for it.".into()));
                continue;
            }
            let out = stream_output(&p.request, key, canvas, &self.adapters);
            ready.push((p, out));
        }
        // Destinations with the stream's encoding start together; each other one
        // after, on its own encoder.
        let (shared, own): (Vec<_>, Vec<_>) = ready.into_iter().partition(|(p, _)| !p.live.own);
        let mut started: Vec<LiveDestination> = Vec::new();
        if !shared.is_empty() {
            let outs: Vec<OutputSettings> = shared.iter().map(|(_, o)| o.clone()).collect();
            let results = self.start_outputs(&outs, r.encoder.as_deref());
            for ((p, _), result) in shared.into_iter().zip(results) {
                match result {
                    Ok(id) => started.push(p.started(id)),
                    Err(why) if alone => return Err(why),
                    Err(why) => failed.push(p.failed(why)),
                }
            }
        }
        for (p, out) in own {
            match self.start_output(&out, p.request.encoder.as_deref()) {
                Ok((id, _, _)) => started.push(p.started(id)),
                Err(why) if alone => return Err(why),
                Err(why) => failed.push(p.failed(why)),
            }
        }
        let Some(anchor) = started.first().cloned() else {
            return Err(failed
                .first()
                .and_then(|d| d.error.clone())
                .unwrap_or_else(|| "No destination started.".into()));
        };
        let id = anchor.output.ok_or("the stream did not start")?;
        let (video_encoder, audio_encoder) = match self.studio.output(id) {
            Some(report) => (
                report
                    .video_encoder
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_default(),
                report.audio_encoder.as_ref().map(|c| c.name.clone()),
            ),
            None => (String::new(), None),
        };
        // Before the host tracks it: recovery stops an output a panicked start left
        // running (ADR-0055 decision 3).
        inject!(self, Fault::AfterStreamStart);
        self.outs[slot(c)].streaming = Some((anchor.url.clone(), Link::default(), id));
        self.outs[slot(c)].destinations = started.into_iter().chain(failed).collect();
        let now = self.clock.now();
        let destinations = self.outs[slot(c)]
            .destinations
            .iter()
            .map(|d| d.api(now))
            .collect();
        let protocol = match parse_server(&anchor.url).map(|s| s.protocol) {
            Ok(Protocol::Srt) => api::StreamProtocol::Srt,
            // Only RTMP and SRT servers pass the settings' check.
            _ => api::StreamProtocol::Rtmp,
        };
        self.set_stream_state(
            c,
            api::StreamState::Live {
                // `set_stream_state` sets it from the destinations.
                available: true,
                protocol,
                connected: false,
                connects: 0,
                reconnect_attempts: 0,
                video_encoder: video_encoder.clone(),
                audio_encoder: audio_encoder.clone(),
                held: false,
                held_ms: 0,
                hold_picture: None,
                hold_scene: None,
                hold_sound: None,
                hold_fallback: false,
                hold_sound_fallback: false,
                hold_fallback_reason: None,
                destinations,
            },
        );
        self.mark_stream(c, true);
        Ok(StreamUpdate::Started {
            url: anchor.url,
            video_encoder,
            audio_encoder,
        })
    }

    /// The destinations Go live of `c` sends to, with their requests: the first one
    /// with `r` (what the UI or the API built from the stream settings), every other
    /// enabled one from the saved destinations. Those enabled but not configured come
    /// back failed with the reason.
    pub(super) fn destination_plans(
        &self,
        c: OutputCanvas,
        r: &StreamRequest,
        only: Option<u32>,
    ) -> (Vec<Planned>, Vec<LiveDestination>) {
        let role = match c {
            OutputCanvas::Main => DestinationCanvas::Main,
            OutputCanvas::Vertical => DestinationCanvas::Vertical,
        };
        let mut profile = self.stream_profile_of(c).cloned().unwrap_or_default();
        // A request that names a server is the first destination's (the UI or the API
        // built it from the stream settings), also before those settings reached here.
        if profile.server.is_empty() && !r.url.is_empty() {
            profile.server = r.url.clone();
        }
        let mut plans = Vec::new();
        let mut skipped = Vec::new();
        for d in self.destinations.of(role, &profile, &self.key_account) {
            if !d.destination.enabled || only.is_some_and(|id| id != d.destination.id) {
                continue;
            }
            let first = d.destination.is_first();
            let base = if first {
                r.clone()
            } else {
                let Some(server) = d.parsed() else {
                    skipped.push(
                        Planned::of(&d, r.clone(), first)
                            .failed("Not set up: enter its server in Settings, Stream.".into()),
                    );
                    continue;
                };
                StreamRequest {
                    url: d.destination.server.clone(),
                    input: server.protocol.input(),
                    needs_key: server.protocol.needs_key(),
                    reconnect: d.reconnect_policy(),
                    transport_options: d.transport_options(),
                    ..r.clone()
                }
            };
            let request = match &d.destination.encoding {
                DestinationEncoding::Shared => base,
                DestinationEncoding::Own {
                    encoding,
                    output_size,
                } => StreamRequest {
                    encoder: encoding.encoder.clone(),
                    video: encoding.video_settings(),
                    audio_bitrate_bps: encoding.audio_bps(),
                    size: output_size.map(|s| s.pair()),
                    ..base
                },
            };
            plans.push(Planned::of(&d, request, first));
        }
        (plans, skipped)
    }

    /// The request `stream.start` of `c` sends with (from the saved stream settings),
    /// and the server URLs no answer may repeat. With `only` naming a destination
    /// other than the first, the first one's server may be empty: the request then
    /// carries that destination's server, and only the stream's encoding is used.
    pub(super) fn api_stream_request(
        &self,
        c: OutputCanvas,
        only: Option<u32>,
    ) -> Result<(StreamRequest, Vec<String>), ApiError> {
        // The vertical stream's texts name it, so a client sees which one is missing.
        let which = match c {
            OutputCanvas::Main => "",
            OutputCanvas::Vertical => " for the vertical stream",
        };
        if self.stream_profile.is_none() {
            return Err(failed("the studio is still starting; try again"));
        }
        let no_server = || {
            failed(format!(
                "no server is saved{which}: enter it in Settings, Stream"
            ))
        };
        let other = only.filter(|id| *id != MAIN_FIRST && *id != VERTICAL_FIRST);
        let mut profile = match (self.stream_profile_of(c), other) {
            (Some(p), _) => p.clone(),
            (None, Some(_)) => self.stream_profile.clone().unwrap_or_default(),
            (None, None) => return Err(no_server()),
        };
        // Every destination off: refused before any key is read (ADR-0148 decision 10).
        if only.is_none() && !self.stream_available(c) {
            return Err(failed(STREAMING_OFF));
        }
        // The first slot removed: the other destinations go live on the stream's
        // shared settings (ADR-0148 decision 6), or streaming is off.
        if other.is_none() && !crate::stream_destinations::first_exists(&profile) {
            if !self.stream_available(c) {
                return Err(failed(STREAMING_OFF));
            }
            let encoder = profile
                .encoding
                .encoder
                .clone()
                .filter(|id| self.encoders.iter().any(|e| e.id == *id));
            let request = StreamRequest::from_settings(&profile, encoder).map_err(|_| {
                failed(format!(
                    "the saved stream settings{which} cannot be used: see Settings, Stream"
                ))
            })?;
            return Ok((request, Vec::new()));
        }
        // The saved servers are settings (`admin`): a `control` caller never gets one
        // back, also not inside an error text.
        let mut hidden: Vec<String> = Vec::new();
        if !profile.server.is_empty() {
            hidden.push(profile.server.clone());
        }
        if let Some(id) = other {
            let server = self
                .resolved_destinations(c)
                .into_iter()
                .find(|d| d.destination.id == id)
                .map(|d| d.destination.server)
                .unwrap_or_default();
            if !server.is_empty() {
                hidden.push(server.clone());
            }
            if profile.server.is_empty() {
                profile.server = server;
            }
        }
        if profile.server.is_empty() {
            return Err(no_server());
        }
        // A saved encoder this system does not have falls back to the best one, as the
        // UI's automatic choice does.
        let encoder = profile
            .encoding
            .encoder
            .clone()
            .filter(|id| self.encoders.iter().any(|e| e.id == *id));
        let request = StreamRequest::from_profile(&profile, encoder).map_err(|_| {
            failed(format!(
                "the saved stream settings{which} cannot be used: see Settings, Stream"
            ))
        })?;
        Ok((request, hidden))
    }

    /// The canvas destination `id` belongs to; `None` for an id the profile does not
    /// have.
    pub(super) fn destination_canvas(&self, id: u32) -> Option<OutputCanvas> {
        let canvas = match id {
            MAIN_FIRST => DestinationCanvas::Main,
            VERTICAL_FIRST => DestinationCanvas::Vertical,
            _ => self.destinations.get(id)?.canvas,
        };
        let c = match canvas {
            DestinationCanvas::Main => OutputCanvas::Main,
            DestinationCanvas::Vertical => OutputCanvas::Vertical,
        };
        // A first slot that was removed is no destination (ADR-0148 decisions 2, 10).
        if (id == MAIN_FIRST || id == VERTICAL_FIRST)
            && !self
                .stream_profile_of(c)
                .is_some_and(crate::stream_destinations::first_exists)
        {
            return None;
        }
        Some(c)
    }

    /// The stream settings or destinations changed: each canvas's stream state says
    /// again whether streaming is available, and publishes when that changed (control
    /// API 1.93, ADR-0148 decision 10). A removed vertical canvas publishes nothing.
    pub(super) fn availability_changed(&mut self) {
        for c in self.output_canvases() {
            let state = self.outs[slot(c)].stream_state.clone();
            self.set_stream_state(c, state);
        }
    }

    /// Streaming is available on `c` (ADR-0148 decision 1): one of its existing
    /// destinations is on, or the destinations section did not load.
    pub(super) fn stream_available(&self, c: OutputCanvas) -> bool {
        if self.destinations_unloaded {
            return true;
        }
        let role = match c {
            OutputCanvas::Main => DestinationCanvas::Main,
            OutputCanvas::Vertical => DestinationCanvas::Vertical,
        };
        let profile = self.stream_profile_of(c).cloned().unwrap_or_default();
        self.destinations.available(role, &profile)
    }

    /// The destinations of `c` with their first one's settings filled in.
    pub(super) fn resolved_destinations(&self, c: OutputCanvas) -> Vec<Resolved> {
        let role = match c {
            OutputCanvas::Main => DestinationCanvas::Main,
            OutputCanvas::Vertical => DestinationCanvas::Vertical,
        };
        let profile = self.stream_profile_of(c).cloned().unwrap_or_default();
        self.destinations.of(role, &profile, &self.key_account)
    }

    /// `stream.destinations` rows of `c` (control API 1.77): never a server or a key.
    pub(super) fn api_destinations(&self, c: OutputCanvas) -> Vec<api::StreamDestination> {
        self.resolved_destinations(c)
            .into_iter()
            .map(|d| api::StreamDestination {
                id: d.destination.id,
                configured: d.configured(),
                name: d.destination.name,
                service: d.destination.service,
                canvas: c,
                enabled: d.destination.enabled,
                encoding: if d.destination.encoding.is_own() {
                    api::DestinationEncodingKind::Own
                } else {
                    api::DestinationEncodingKind::Shared
                },
            })
            .collect()
    }

    /// Starts destination `id` of `c`'s running stream (`stream.destination.start`,
    /// ADR-0127 decision 16): it joins the shared encoder at its next keyframe, or
    /// opens its own chain. `r` is the request the stream's first destination would
    /// start with.
    pub(super) fn start_destination(
        &mut self,
        c: OutputCanvas,
        r: &StreamRequest,
        id: u32,
    ) -> Result<(), String> {
        let (plans, failed) = self.destination_plans(c, r, Some(id));
        if let Some(why) = failed.into_iter().find_map(|d| d.error) {
            return Err(why);
        }
        let p = plans
            .into_iter()
            .next()
            .ok_or_else(|| format!("there is no destination {id}"))?;
        let key = self.keys.read(&p.account).map_err(|e| {
            format!(
                "The stream key of {} could not be read from {}: {e}",
                p.live.label(),
                self.keys.place()
            )
        })?;
        if p.request.needs_key && key.is_none() {
            return Err(format!("No stream key is saved for {}.", p.live.label()));
        }
        let canvas = self.output_canvas(c)?;
        let out = stream_output(&p.request, key, canvas, &self.adapters);
        let output = if p.live.own {
            self.start_output(&out, p.request.encoder.as_deref())?.0
        } else {
            self.start_outputs(std::slice::from_ref(&out), r.encoder.as_deref())
                .pop()
                .unwrap_or_else(|| Err("the destination did not start".into()))?
        };
        let live = p.started(output);
        let dests = &mut self.outs[slot(c)].destinations;
        match dests.iter_mut().find(|d| d.id == id) {
            Some(d) => *d = live,
            None => dests.push(live),
        }
        Ok(())
    }

    /// Stops destination `id` of `c`'s running stream while others run on
    /// (`stream.destination.stop`): its output drains and its transport finishes; the
    /// shared encoder runs on. When it was the anchor, the next running destination
    /// becomes the anchor. False when it does not run.
    pub(super) fn stop_destination(&mut self, c: OutputCanvas, id: u32) -> bool {
        let Some(anchor) = self.outs[slot(c)].streaming.as_ref().map(|s| s.2) else {
            return false;
        };
        let Some(d) = self.outs[slot(c)]
            .destinations
            .iter_mut()
            .find(|d| d.id == id && d.output.is_some())
        else {
            return false;
        };
        let Some(output) = d.output.take() else {
            return false;
        };
        d.state = DestinationState::Stopped;
        d.link.connected = false;
        let _ = self.studio.stop_output(output);
        if output == anchor
            && let Some(next) = self.outs[slot(c)]
                .destinations
                .iter()
                .find(|d| d.output.is_some())
                .cloned()
            && let Some(next_id) = next.output
        {
            self.outs[slot(c)].streaming = Some((next.url, Link::default(), next_id));
            self.refresh_stream_hold(c);
        }
        true
    }

    /// Attaches `out` to the studio with the picked encoder only, or the best one (a
    /// running one when the settings match, ADR-0047); returns the output and the
    /// encoders' names.
    /// Probes video and audio encoders once. Returns whether this call did the probe.
    pub(super) fn ensure_encoders(&mut self) -> bool {
        if self.encoders_probed {
            return false;
        }
        self.adapters.probe_encoders();
        let (encoders, cannot_record) = encoder_catalog(&self.adapters);
        self.encoders = encoders;
        self.cannot_record = cannot_record;
        self.encoders_probed = true;
        true
    }

    pub(super) fn publish_encoders(&self, notify: &dyn Fn(Update)) {
        notify(Update::Encoders {
            encoders: self.encoders.clone(),
            cannot_record: self.cannot_record.clone(),
        });
    }

    /// Probes when needed and tells the UI. A later call publishes nothing.
    pub(super) fn note_encoders(&mut self, notify: &dyn Fn(Update)) {
        if self.ensure_encoders() {
            self.publish_encoders(notify);
        }
    }

    pub(super) fn start_output(
        &mut self,
        out: &OutputSettings,
        encoder: Option<&str>,
    ) -> Result<(OutputId, String, Option<String>), String> {
        // A path that did not publish still probes before an encoder is chosen.
        self.ensure_encoders();
        // A picked encoder is the only one tried: if it cannot open, the output fails
        // rather than quietly using another one.
        let all = match encoder {
            None => None,
            Some(id) => {
                let only = only_encoder(&self.adapters.video_encoders, id)?;
                Some(std::mem::replace(&mut self.adapters.video_encoders, only))
            }
        };
        let started = self
            .studio
            .start_output(&self.adapters, out)
            .map_err(|e| e.to_string())
            .and_then(|id| {
                let report = self.studio.output(id).ok_or("the output did not start")?;
                Ok((
                    id,
                    report
                        .video_encoder
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                    report.audio_encoder.as_ref().map(|c| c.name.clone()),
                ))
            });
        if let Some(all) = all {
            self.adapters.video_encoders = all;
        }
        started
    }

    /// [`Host::start_output`] for several outputs in one apply (ADR-0127 decision 3):
    /// one result per output, in order; one that fails does not stop the others.
    pub(super) fn start_outputs(
        &mut self,
        outs: &[OutputSettings],
        encoder: Option<&str>,
    ) -> Vec<Result<OutputId, String>> {
        let all = match encoder {
            None => None,
            Some(id) => match only_encoder(&self.adapters.video_encoders, id) {
                Ok(only) => Some(std::mem::replace(&mut self.adapters.video_encoders, only)),
                Err(why) => return outs.iter().map(|_| Err(why.clone())).collect(),
            },
        };
        let results = self
            .studio
            .start_outputs(&self.adapters, outs)
            .into_iter()
            .map(|r| r.map_err(|e| e.to_string()))
            .collect();
        if let Some(all) = all {
            self.adapters.video_encoders = all;
        }
        results
    }

    /// Sends the running outputs' counters to the Stats panel once every
    /// [`STATS_INTERVAL`], the first at once, and empty counters once when the last
    /// output stopped; nothing while no output runs (no timer: ADR-0013). Returns the
    /// time until the next report.
    pub(super) fn report_stats(
        &mut self,
        now: Timestamp,
        notify: &dyn Fn(Update),
    ) -> Option<Duration> {
        let recording = self.outs[0].recording.as_ref().map(|(_, id)| *id);
        let stream = self.outs[0].streaming.as_ref().map(|(_, _, id)| *id);
        if recording.is_none() && stream.is_none() {
            // The last output stopped: the panel empties once.
            if self.stats_due.take().is_some() {
                notify(Update::Stats(Stats::default()));
            }
            return None;
        }
        let due = *self.stats_due.get_or_insert(now);
        if now < due {
            return Some(due.saturating_since(now));
        }
        self.stats_due = Some(now + STATS_INTERVAL);
        let counters = |id: Option<OutputId>| id.and_then(|id| self.studio.output_counters(id));
        notify(Update::Stats(Stats::of(
            counters(recording).as_ref(),
            counters(stream).as_ref(),
        )));
        Some(STATS_INTERVAL)
    }

    /// Reads each running destination of `c`'s stream (ADR-0127 decisions 5 and 6):
    /// its connection and state, and its failure. A destination whose output failed is
    /// removed without a drain and named in a notice; the others run on. When the
    /// anchor fails and another destination runs, that one becomes the anchor. Returns
    /// the stream's failure when no destination is left running.
    pub(super) fn check_destinations(
        &mut self,
        c: OutputCanvas,
        notify: &dyn Fn(Update),
    ) -> Option<String> {
        let now = self.clock.now();
        let mut lost: Option<String> = None;
        let anchor = self.outs[slot(c)].streaming.as_ref().map(|s| s.2);
        let mut dests = std::mem::take(&mut self.outs[slot(c)].destinations);
        if dests.is_empty()
            && let Some(id) = anchor
        {
            // A stream the host started before it tracked destinations: one.
            if let Some(why) = self.studio.output_failure(id) {
                return Some(why);
            }
            return None;
        }
        for d in &mut dests {
            let Some(id) = d.output else {
                continue;
            };
            if let Some(why) = self.studio.output_failure(id) {
                let _ = self.studio.abort_output(id);
                d.output = None;
                d.state = DestinationState::Failed;
                d.link.connected = false;
                notify(Update::Error(format!("{}: {why}", d.label())));
                lost = Some(why.clone());
                d.error = Some(why);
                continue;
            }
            if let Some(link) = self.studio.link(id) {
                d.state = if link.connected {
                    DestinationState::Live
                } else if link.connects > 0 {
                    DestinationState::Reconnecting
                } else {
                    DestinationState::Connecting
                };
                if link.connected && d.on_air_since.is_none() {
                    d.on_air_since = Some(now);
                }
                d.link = link;
            }
        }
        let running = dests.iter().find(|d| d.output.is_some()).cloned();
        self.outs[slot(c)].destinations = dests;
        match (running, anchor) {
            (None, _) => Some(lost.unwrap_or_else(|| "every destination failed".into())),
            (Some(next), Some(id))
                if next.output != Some(id)
                    && !self.outs[slot(c)]
                        .destinations
                        .iter()
                        .any(|d| d.output == Some(id)) =>
            {
                // The anchor failed: the stream goes on with the next destination.
                let next_id = next.output?;
                self.outs[slot(c)].streaming = Some((next.url, Link::default(), next_id));
                self.refresh_stream_hold(c);
                None
            }
            _ => None,
        }
    }

    /// Tells the UI when the stream's connection changed.
    pub(super) fn report_link(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        if self.outs[slot(c)].streaming.is_none() {
            return;
        }
        // Only what the UI shows: a change of transport drops alone sends nothing.
        let shown = |l: &Link| (l.connected, l.connects, l.reconnect_attempts);
        // The stream's link sums its running destinations' (ADR-0127 decision 16):
        // connected while one is.
        let mut sum: Option<Link> = None;
        for d in &self.outs[slot(c)].destinations {
            if d.output.is_some() {
                let s = sum.get_or_insert_with(Link::default);
                s.connected |= d.link.connected;
                s.connects += d.link.connects;
                s.reconnect_attempts += d.link.reconnect_attempts;
                s.transport_drops += d.link.transport_drops;
            }
        }
        // Each destination's row (control API 1.77): a change of state, counters or
        // error is published; the time on air alone is not (never per round).
        let now = self.clock.now();
        let rows: Vec<api::LiveDestination> = self.outs[slot(c)]
            .destinations
            .iter()
            .map(|d| d.api(now))
            .collect();
        let without_time = |rows: &[api::LiveDestination]| -> Vec<api::LiveDestination> {
            rows.iter()
                .cloned()
                .map(|r| api::LiveDestination { on_air_ms: 0, ..r })
                .collect()
        };
        let rows_shown = without_time(&rows);
        if rows_shown != self.outs[slot(c)].destinations_shown {
            self.outs[slot(c)].destinations_shown = rows_shown;
            notify(Update::LiveDestinations(c, rows.clone()));
        }
        let rows_changed = match &self.outs[slot(c)].stream_state {
            api::StreamState::Live { destinations, .. } => {
                without_time(destinations) != without_time(&rows)
            }
            api::StreamState::Idle { .. } => false,
        };
        let Some((_, last, _)) = &mut self.outs[slot(c)].streaming else {
            return;
        };
        let link = sum.filter(|l| shown(l) != shown(last));
        if let Some(link) = link {
            *last = link;
            notify(stream_update(c, StreamUpdate::Link(link)));
        }
        if link.is_none() && !rows_changed {
            return;
        }
        let mut state = self.outs[slot(c)].stream_state.clone();
        if let api::StreamState::Live {
            connected,
            connects,
            reconnect_attempts,
            destinations,
            ..
        } = &mut state
        {
            if let Some(link) = link {
                *connected = link.connected;
                *connects = link.connects;
                *reconnect_attempts = link.reconnect_attempts;
            }
            *destinations = rows;
            self.set_stream_state(c, state);
        }
    }

    /// Ends the stream, if one runs, and says so; returns why it failed, if it did.
    pub(super) fn end_stream(
        &mut self,
        c: OutputCanvas,
        failure: Option<String>,
        notify: &dyn Fn(Update),
    ) -> Option<String> {
        // Tracked until the end, as in `stop_recording`.
        let (_, _, id) = self.outs[slot(c)].streaming.clone()?;
        // Every destination in one draining apply (ADR-0127 decision 7); the anchor's
        // report is the stream's.
        let ids: Vec<OutputId> = std::iter::once(id)
            .chain(
                self.outs[slot(c)]
                    .destinations
                    .iter()
                    .filter_map(|d| d.output)
                    .filter(|o| *o != id),
            )
            .collect();
        let report = self
            .studio
            .stop_outputs(&ids)
            .into_iter()
            .find(|(o, _)| *o == id)
            .map(|(_, r)| r);
        // Still tracked: recovery ends a stream whose stop panicked half-way.
        inject!(self, Fault::InStreamEnd);
        let (frames, dropped, connects, failed) = report.map_or((0, 0, 0, None), |r| {
            let p = &r.progress;
            (
                p.frames_encoded,
                p.export_drops + p.encoder_skipped + p.encoder_drops + p.transport_drops,
                p.connects,
                r.failure,
            )
        });
        let failure = failure.or(failed);
        self.outs[slot(c)].streaming = None;
        self.outs[slot(c)].destinations.clear();
        if !self.outs[slot(c)].destinations_shown.is_empty() {
            self.outs[slot(c)].destinations_shown.clear();
            notify(Update::LiveDestinations(c, Vec::new()));
        }
        self.outs[slot(c)].hold = HoldPlan::default();
        self.mark_stream(c, false);
        // A recording started with the stream outlives a stream that failed: it is the
        // copy of what the viewers missed. From now on it is the user's to stop.
        self.outs[slot(c)].auto_recording = false;
        self.set_stream_state(
            c,
            api::StreamState::Idle {
                // `set_stream_state` sets it from the destinations.
                available: true,
                last: Some(api::StreamSummary {
                    failure: failure.clone(),
                    frames,
                    dropped,
                    connects,
                }),
            },
        );
        notify(stream_update(
            c,
            StreamUpdate::Ended {
                failure: failure.clone(),
                frames,
                dropped,
                connects,
            },
        ));
        failure
    }
}
