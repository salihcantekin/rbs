//! Outputs beside the main program: the virtual camera and the vertical canvas with its picture, size and layout (ADR-0159 decision 8).

use super::*;

impl Host {
    /// The canvases a start or stop with `canvas: "all"` acts on, main first
    /// (ADR-0111 decision 3).
    pub(super) fn output_canvases(&self) -> Vec<OutputCanvas> {
        let mut all = vec![OutputCanvas::Main];
        if self.verticals.vertical.is_some() {
            all.push(OutputCanvas::Vertical);
        }
        all
    }

    /// The studio canvas `c`'s outputs use (`None`: the main one); refused in words
    /// when there is no vertical canvas.
    pub(super) fn output_canvas(&self, c: OutputCanvas) -> Result<Option<CanvasId>, Refusal> {
        match c {
            OutputCanvas::Main => Ok(None),
            OutputCanvas::Vertical => self
                .verticals
                .vertical
                .map(|v| Some(v.canvas))
                .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".to_owned())),
        }
    }

    /// The vertical canvas is gone: its outputs' states go with it (no event), and a
    /// new one starts idle (protocol 1.29). The `seq` counters are kept, so a client
    /// never takes a new canvas's events for older ones.
    pub(super) fn forget_vertical_outputs(&mut self) {
        self.outs[slot(OutputCanvas::Vertical)] = CanvasOutputs::default();
    }

    /// The canvas a control API call asks for (protocol 1.29): the main one by
    /// default; `not_found` for the vertical one while there is none.
    pub(super) fn api_canvas_of(
        &self,
        asked: Option<OutputCanvas>,
    ) -> Result<OutputCanvas, ApiError> {
        let c = asked.unwrap_or_default();
        if c == OutputCanvas::Vertical && self.verticals.vertical.is_none() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "there is no vertical canvas",
            ));
        }
        Ok(c)
    }

    /// Starts sending to the virtual camera with `settings` for the user or an API
    /// client, and tells the UI and `vcam.changed`; the UI gets the error when
    /// `show_error` (an API client gets it in its answer instead).
    pub(super) fn start_camera_asked(
        &mut self,
        settings: VirtualCameraSettings,
        notify: &dyn Fn(Update),
        show_error: bool,
    ) -> Result<(), Refusal> {
        let started = self.start_camera(settings);
        match &started {
            Ok(()) => self.vcam.camera_due = None,
            Err(why) if show_error => notify(Update::Error(why.text().to_owned())),
            Err(_) => {}
        }
        notify(Update::VirtualCamera(self.camera_status()));
        self.camera_changed();
        started
    }

    /// Stops sending to the virtual camera, and tells the UI and `vcam.changed`.
    pub(super) fn stop_camera(&mut self, notify: &dyn Fn(Update)) {
        self.studio.stop_virtual_camera();
        self.vcam.camera = None;
        self.vcam.camera_due = None;
        notify(Update::VirtualCamera(None));
        self.camera_changed();
    }

    /// The camera for `vcam.status` (with its counters) and `vcam.changed` (without).
    pub(super) fn api_camera(&self, frames: bool) -> api::VcamStatus {
        let sending = self.camera_status();
        let (state, reason) = match self.vcam.camera_setup.as_ref().map(|s| &s.state) {
            _ if self.vcam.camera_factory.is_none() => (
                Some(api::VcamInstall::Unavailable),
                Some("The virtual camera is not available on this computer".to_owned()),
            ),
            None | Some(None) => (None, None),
            Some(Some(InstallState::Installed)) => (Some(api::VcamInstall::Added), None),
            Some(Some(InstallState::NotInstalled)) => (Some(api::VcamInstall::NotAdded), None),
            Some(Some(InstallState::NeedsUpdate { reason })) => {
                (Some(api::VcamInstall::NeedsUpdate), Some(reason.clone()))
            }
            Some(Some(InstallState::Unsupported { reason })) => {
                (Some(api::VcamInstall::Unavailable), Some(reason.clone()))
            }
            // A state a later port version adds: not known to this version.
            Some(Some(_)) => (None, None),
        };
        let (width, height, rate) = match (&sending, &self.vcam.camera_setup) {
            (Some(s), _) => (s.width, s.height, s.rate),
            (None, Some(setup)) => (setup.width, setup.height, rounded_rate(setup.fps)),
            (None, None) => (0, 0, 0),
        };
        api::VcamStatus {
            state,
            reason,
            sending: sending.is_some(),
            width,
            height,
            rate,
            frames: sending
                .as_ref()
                .filter(|_| frames)
                .and_then(|s| s.counters)
                .map(|c| api::VcamFrames {
                    sent: c.sent(),
                    replaced: c.replaced(),
                    refused: c.refused(),
                    consumer_busy: c.consumer_busy(),
                    consumer_absent: c.consumer_absent(),
                    format_pending: c.format_pending(),
                    os_errors: c.os_errors(),
                }),
            failure: sending.and_then(|s| s.failure),
        }
    }

    /// Publishes `vcam.changed` when the camera's state differs from the last one.
    pub(super) fn camera_changed(&mut self) {
        let now = self.api_camera(false);
        if self.vcam.camera_published.as_ref() == Some(&now) {
            return;
        }
        self.vcam.camera_published = Some(now.clone());
        self.events
            .changed(StateKey::topic(Topic::VcamChanged), || {
                EventPayload::VcamChanged(now)
            });
    }

    /// Starts sending to the virtual camera with `settings`.
    pub(super) fn start_camera(&mut self, settings: VirtualCameraSettings) -> Result<(), Refusal> {
        if let Some(why) = self.calibration_blocks() {
            return Err(Refusal::Conflict(format!("{why}.")));
        }
        let factory = self.vcam.camera_factory.clone().ok_or_else(|| {
            Refusal::Failed("The virtual camera is not available on this computer.".to_owned())
        })?;
        self.studio
            .start_virtual_camera(&*factory, settings)
            .map_err(|e| Refusal::Failed(format!("The virtual camera did not start: {e}")))?;
        self.vcam.camera = Some(settings);
        Ok(())
    }

    /// The virtual camera while it sends.
    pub(super) fn camera_status(&self) -> Option<CameraStatus> {
        let state = self.studio.virtual_camera()?;
        let format = state.config.format;
        Some(CameraStatus {
            width: format.width,
            height: format.height,
            rate: rounded_rate(format.rate),
            counters: state.session.map(|s| s.snapshot()),
            failure: state.failure,
        })
    }

    /// Sends the virtual camera's counters once every [`STATS_INTERVAL`] while it
    /// sends (no timer otherwise: ADR-0013), and says once when it stopped by itself
    /// (it could not start again after a restart). Returns the time until the next.
    pub(super) fn report_camera(
        &mut self,
        now: Timestamp,
        notify: &dyn Fn(Update),
    ) -> Option<Duration> {
        if self.vcam.camera.is_none() {
            if self.vcam.camera_due.take().is_some() {
                notify(Update::Error(
                    "The virtual camera stopped: it could not start again with the new canvas."
                        .into(),
                ));
                notify(Update::VirtualCamera(None));
                self.camera_changed();
            }
            return None;
        }
        let due = *self.vcam.camera_due.get_or_insert(now);
        if now < due {
            return Some(due.saturating_since(now));
        }
        self.vcam.camera_due = Some(now + STATS_INTERVAL);
        notify(Update::VirtualCamera(self.camera_status()));
        // A failure of the camera's node, or its end, is a change of state.
        self.camera_changed();
        Some(STATS_INTERVAL)
    }

    /// Makes the second canvas and runs it (ADR-0093 decision 8). Everything is tried
    /// on a copy of the collection; the collection and the studio stay as they were when
    /// any step is refused.
    pub(super) fn add_vertical(&mut self, spec: VerticalCanvas) -> Result<bool, Refusal> {
        if self.verticals.vertical.is_some() {
            return Err(Refusal::Conflict(
                "There is a vertical canvas already.".to_owned(),
            ));
        }
        let main = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::NotFound("There is no canvas.".to_owned()))?;
        let fps = main.frame_rate;
        let mut draft = self.collection.clone();
        let name = ["Vertical", "Vertical output", "Vertical canvas"]
            .into_iter()
            .chain((2..).map(|_| "Vertical 9:16"))
            .find(|n| !draft.canvases().any(|c| c.name == *n))
            .unwrap_or("Vertical");
        let (canvas, _) = draft
            .add_canvas(name, spec.size, fps)
            .map_err(|e| Refusal::of(&e))?;
        // Following needs scenes the new canvas may show (ADR-0093 decision 3): the main
        // canvas's own scenes become shared (U15); the global scene stays where it is.
        let mut shared = Vec::new();
        if spec.follow {
            let own: Vec<_> = draft
                .scenes_for(self.canvas)
                .filter(|s| s.scope == SceneScope::Canvas(self.canvas))
                .filter(|s| !draft.is_global_scene(s.id))
                .map(|s| s.id)
                .collect();
            for id in &own {
                draft
                    .set_scene_scope(*id, SceneScope::Shared)
                    .map_err(|e| Refusal::of(&e))?;
            }
            shared = own;
        }
        // A scene of its own to start from when it does not follow, else the one on air.
        let scene = if draft.scenes_for(canvas).any(|s| s.id == self.scene) {
            self.scene
        } else {
            draft
                .showable_scenes(canvas)
                .into_iter()
                .next()
                .map(|s| s.id)
                .ok_or_else(|| {
                    Refusal::Conflict(
                        "No scene can be shown on a vertical canvas: the scenes belong to the main canvas.".to_owned(),
                    )
                })?
        };
        draft
            .set_program(canvas, Some(scene))
            .map_err(|e| Refusal::of(&e))?;
        if spec.follow {
            draft
                .set_canvas_follow(canvas, Some(self.canvas), Default::default())
                .map_err(|e| Refusal::of(&e))?;
        }
        if let Some(mode) = spec.layout {
            let sizes = &self.sizes;
            draft
                .auto_layout(
                    canvas,
                    self.scene,
                    mode,
                    &|source| sizes.get(&source).copied(),
                    &BTreeMap::new(),
                )
                .map_err(|e| Refusal::of(&e))?;
        }
        self.note_collection();
        let before = std::mem::replace(&mut self.collection, draft);
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        match self.studio.activate_canvas(
            running,
            &self.adapters.sources,
            self.vertical_extra(canvas),
        ) {
            Ok(_) => {
                self.verticals.vertical_shared = shared;
                self.verticals.vertical = Some(VerticalState {
                    canvas,
                    size: spec.size,
                    follow: spec.follow,
                });
                self.waiting = self.studio.waiting();
                Ok(true)
            }
            Err(e) => {
                self.note_collection();
                self.collection = before;
                Err(Refusal::Failed(format!(
                    "The vertical canvas did not start: {e}"
                )))
            }
        }
    }

    /// The vertical canvas as the studio runs it: with its picture while its panel
    /// shows (ADR-0093 decision 7).
    /// Runs the vertical canvas the collection saved, when none runs yet: the canvas
    /// stays in the document across a restart of RBS or a collection switch, and
    /// without this its outputs, panel picture and Remove would be gone while a new
    /// Add made a second one (ADR-0093 decision 9). A canvas that cannot start is told
    /// once and stays in the document.
    pub(super) fn restore_vertical(&mut self) {
        if self.verticals.vertical.is_some() {
            return;
        }
        let Some(state) = saved_vertical(&self.collection, self.canvas) else {
            return;
        };
        let extra = self.vertical_extra(state.canvas);
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        match self
            .studio
            .activate_canvas(running, &self.adapters.sources, extra)
        {
            Ok(_) => {
                self.verticals.vertical = Some(state);
                self.waiting = self.studio.waiting();
            }
            Err(e) => {
                self.verticals.vertical_restore_error =
                    Some(format!("The vertical canvas did not start: {e}"));
            }
        }
    }

    pub(super) fn vertical_extra(&self, canvas: CanvasId) -> ExtraCanvas {
        let extra = ExtraCanvas::new(canvas);
        let picture = self.collection.canvas(canvas);
        match (
            self.verticals.vertical_picture,
            self.preview_surfaces,
            picture,
        ) {
            (true, Some(_), Some(info)) => {
                extra.with_preview(preview_desc(info.size, info.frame_rate))
            }
            _ => extra,
        }
    }

    /// The vertical canvas's panel opened or closed: its picture starts or stops, also
    /// while its outputs run.
    pub(super) fn set_vertical_picture(&mut self, on: bool) -> Result<(), Refusal> {
        self.verticals.vertical_picture = on;
        let Some(v) = self.verticals.vertical else {
            return Ok(());
        };
        let desc = self.vertical_extra(v.canvas).preview;
        let run = self.run_collection();
        let running = run.as_ref().unwrap_or(&self.collection);
        self.studio
            .set_canvas_picture(running, &self.adapters.sources, v.canvas, desc)
            .map(|_| ())
            .map_err(|e| Refusal::Failed(format!("The vertical preview did not start: {e}")))?;
        self.waiting = self.studio.waiting();
        Ok(())
    }

    /// Stops the second canvas and removes it from the collection.
    pub(super) fn remove_vertical(&mut self) -> Result<bool, Refusal> {
        let Some(state) = self.verticals.vertical else {
            return Ok(false);
        };
        if self.outs[slot(OutputCanvas::Vertical)].busy() {
            return Err(Refusal::Conflict(
                "Stop the recording or stream of the vertical canvas first.".to_owned(),
            ));
        }
        let mut draft = self.collection.clone();
        draft
            .remove_canvas(state.canvas)
            .map_err(|e| Refusal::of(&e))?;
        // The canvas leaves the engine while it is still in the collection.
        let run = run_copy_or(&self.collection, self.canvas, self.program.held.as_ref());
        self.studio
            .deactivate_canvas(
                run.as_ref().unwrap_or(&self.collection),
                &self.adapters.sources,
                state.canvas,
            )
            .map_err(|e| match e {
                RecordError::CanvasBusy(_) => Refusal::Conflict(
                    "Stop the recording or stream of the vertical canvas first.".to_owned(),
                ),
                e => Refusal::Failed(e.to_string()),
            })?;
        // The scenes the add shared go back to the main canvas (the removal is one step).
        for id in std::mem::take(&mut self.verticals.vertical_shared) {
            if draft
                .scenes_for(self.canvas)
                .any(|s| s.id == id && s.scope == SceneScope::Shared)
            {
                let _ = draft.set_scene_scope(id, SceneScope::Canvas(self.canvas));
            }
        }
        self.note_collection();
        self.collection = draft;
        self.verticals.vertical = None;
        // Steps that wrote vertical layouts would name a canvas that is gone (scene
        // model invariant 4), so the history ends here, as after a canvas change.
        self.edits.history.clear();
        self.forget_vertical_outputs();
        self.waiting = self.studio.waiting();
        Ok(true)
    }

    /// The vertical canvas, when its boxes can be edited: it shows the scene being
    /// edited, which is the scene on air outside studio mode (it follows the main
    /// program and shows no fallback).
    pub(super) fn vertical_edits(&self) -> Option<CanvasId> {
        let v = self.verticals.vertical?;
        let shown = self.vertical_shown()?;
        (self.preview().is_none() && shown == self.edited()).then_some(v.canvas)
    }

    /// The scene the vertical canvas shows now. A following canvas shows its leader's
    /// program scene (or its fallback), not its own `program`, which is only what it
    /// showed before it followed.
    pub(super) fn vertical_shown(&self) -> Option<SceneId> {
        let v = self.verticals.vertical?;
        let canvas = self.collection.canvas(v.canvas)?;
        if canvas.follow.is_some() {
            self.collection
                .followed_programs(&BTreeMap::new())
                .get(&v.canvas)
                .copied()
        } else {
            canvas.program
        }
    }

    /// Arranges `scene` (default: the one the vertical canvas shows) for the vertical
    /// canvas, one undo step (`canvas.auto_layout`, ADR-0093 decision 4). The number of
    /// layouts written.
    pub(super) fn auto_layout_vertical(
        &mut self,
        mode: AutoLayout,
        scene: Option<SceneId>,
    ) -> Result<u32, Refusal> {
        let v = self
            .verticals
            .vertical
            .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".into()))?;
        let scene = match scene {
            Some(s) => s,
            None => self
                .vertical_shown()
                .ok_or_else(|| Refusal::NotFound("The vertical canvas shows no scene.".into()))?,
        };
        let shown = self
            .collection
            .scene(scene)
            .ok_or_else(|| Refusal::NotFound(format!("There is no scene {}.", scene.get())))?;
        if self.collection.is_global_scene(scene)
            || !self.collection.scenes_for(v.canvas).any(|s| s.id == scene)
        {
            return Err(Refusal::Conflict(
                "This scene cannot be shown on the vertical canvas.".into(),
            ));
        }
        let name = shown.name.clone();
        let count = shown
            .items
            .iter()
            .filter(|i| i.visible)
            .filter(|i| match i.content {
                ItemContent::Source(id) => self.sizes.contains_key(&id),
                _ => false,
            })
            .count() as u32;
        let sizes = &self.sizes;
        let undo = self
            .collection
            .auto_layout(
                v.canvas,
                scene,
                mode,
                &|source| sizes.get(&source).copied(),
                &BTreeMap::new(),
            )
            .map_err(|e| Refusal::of(&e))?;
        self.edits
            .history
            .record(EditKind::Transform, &name, 0, vec![undo]);
        Ok(count)
    }

    /// Changes the vertical canvas's size (`canvas.set_vertical_size`, ADR-0093
    /// decision 8): its compositor restarts, layouts stay in its layout space
    /// (ADR-0110), and the undo history stays. `Ok(false)` when it has that size.
    pub(super) fn set_vertical_size(&mut self, size: Size) -> Result<bool, Refusal> {
        let v = self
            .verticals
            .vertical
            .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".into()))?;
        if v.size == size {
            return Ok(false);
        }
        if self.outs[slot(OutputCanvas::Vertical)].busy() {
            return Err(Refusal::Conflict(
                "Stop the recording or stream of the vertical canvas first.".to_owned(),
            ));
        }
        let fps = self
            .collection
            .canvas(v.canvas)
            .map(|c| c.frame_rate)
            .ok_or_else(|| Refusal::NotFound("There is no vertical canvas.".into()))?;
        let mut draft = self.collection.clone();
        draft
            .set_canvas_format(v.canvas, size, fps)
            .map_err(|e| Refusal::of(&e))?;
        let run = run_copy_or(&self.collection, self.canvas, self.program.held.as_ref());
        self.studio
            .deactivate_canvas(
                run.as_ref().unwrap_or(&self.collection),
                &self.adapters.sources,
                v.canvas,
            )
            .map_err(|e| match e {
                RecordError::CanvasBusy(_) => Refusal::Conflict(
                    "Stop the recording or stream of the vertical canvas first.".to_owned(),
                ),
                e => Refusal::Failed(e.to_string()),
            })?;
        self.note_collection();
        let before = std::mem::replace(&mut self.collection, draft);
        let started = {
            let extra = self.vertical_extra(v.canvas);
            let run = self.run_collection();
            let running = run.as_ref().unwrap_or(&self.collection);
            self.studio
                .activate_canvas(running, &self.adapters.sources, extra)
        };
        match started {
            Ok(_) => {
                self.verticals.vertical = Some(VerticalState { size, ..v });
                self.waiting = self.studio.waiting();
                Ok(true)
            }
            Err(e) => {
                // The canvas runs again at the size it had.
                self.note_collection();
                self.collection = before;
                let extra = self.vertical_extra(v.canvas);
                let run = self.run_collection();
                let running = run.as_ref().unwrap_or(&self.collection);
                if self
                    .studio
                    .activate_canvas(running, &self.adapters.sources, extra)
                    .is_err()
                {
                    self.verticals.vertical = None;
                }
                self.waiting = self.studio.waiting();
                Err(Refusal::Failed(format!(
                    "The vertical canvas did not start at that size: {e}"
                )))
            }
        }
    }

    /// Places a video source's picture on the vertical canvas (ADR-0093 decision 4):
    /// the box, in that canvas's pixels, becomes the item's own layout there, so the
    /// main canvas does not change (U15). Starts from what the vertical canvas shows.
    /// `Ok(false)` when the box is already there.
    pub(super) fn set_vertical_box(
        &mut self,
        id: SourceId,
        position: [f32; 2],
        size: [f32; 2],
        gesture: u64,
    ) -> Result<bool, Refusal> {
        let valid = position.iter().chain(size.iter()).all(|v| v.is_finite())
            && size.iter().all(|v| *v != 0.0);
        if !valid {
            return Err(Refusal::Invalid(
                "Position and size must be numbers, and the size cannot be 0.".into(),
            ));
        }
        if self.verticals.vertical.is_none() {
            return Err(Refusal::NotFound("There is no vertical canvas.".into()));
        }
        let canvas = self.vertical_edits().ok_or_else(|| {
            Refusal::Conflict(
                "The vertical canvas shows another scene: its boxes are edited while it shows the scene on air outside studio mode."
                    .into(),
            )
        })?;
        let (_, item) = self.item_of(id)?;
        if self
            .globals()
            .is_some_and(|g| self.scene_of(item.id) == Some(g))
        {
            return Err(Refusal::Conflict(
                "Global sources are not on the vertical canvas yet.".into(),
            ));
        }
        let mut layout = self
            .collection
            .placed_layout(canvas, item.layout_on(canvas));
        if !item.overrides.contains_key(&canvas)
            && layout.position == position
            && layout.size == size
        {
            return Ok(false);
        }
        layout.position = position;
        layout.size = size;
        // Compared in layout space: a box mapped out and back is not always the same
        // float, and the same box must not make a second undo step (ADR-0110).
        let stored = self.collection.stored_layout(canvas, &layout);
        if item.overrides.get(&canvas) == Some(&stored) {
            return Ok(false);
        }
        let item = item.id;
        let undo = self
            .collection
            .set_item_layout(item, Some(canvas), stored)
            .map_err(|e| Refusal::of(&e))?;
        let name = self.name_of(id);
        self.edits
            .history
            .record(EditKind::Place, &name, gesture, vec![undo]);
        Ok(true)
    }

    /// The window commands of this concern, routed by `Host::command`.
    pub(super) fn canvas_and_mix_command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::AutoLayoutVertical(mode) => {
                self.run_ui_api(
                    rbs_api::Command::CanvasAutoLayout(rbs_api::request::CanvasAutoLayout {
                        canvas: OutputCanvas::Vertical,
                        mode,
                        scene: None,
                    }),
                    notify,
                );
            }
            Command::SetVerticalSize(size) => {
                self.run_ui_api(
                    rbs_api::Command::CanvasSetVerticalSize(rbs_api::request::SetVerticalSize {
                        width: size.width,
                        height: size.height,
                    }),
                    notify,
                );
            }
            Command::SetMasterVolume { volume } => {
                self.run_ui_api(
                    rbs_api::Command::CanvasSetMasterVolume(rbs_api::request::SetMasterVolume {
                        volume,
                    }),
                    notify,
                );
            }
            Command::SetMasterMuted { muted } => {
                self.run_ui_api(
                    rbs_api::Command::CanvasSetMasterMuted(rbs_api::request::SetMasterMuted {
                        muted,
                    }),
                    notify,
                );
            }
            Command::SetLoudnessShown(on) => {
                self.run_ui_api(
                    rbs_api::Command::LoudnessSetEnabled(rbs_api::request::SetLoudnessEnabled {
                        on,
                    }),
                    notify,
                );
            }
            Command::SetLoudnessSettings {
                target,
                true_peak_limit,
            } => {
                self.run_ui_api(
                    rbs_api::Command::SettingsSet(rbs_api::request::SettingsPatch {
                        loudness: Some(api::LoudnessSettings {
                            target,
                            true_peak_limit,
                        }),
                        ..Default::default()
                    }),
                    notify,
                );
            }
            // Routed elsewhere by `Host::command`.
            _ => {}
        }
    }

    /// The window commands of this concern, routed by `Host::command`.
    pub(super) fn output_command(&mut self, c: Command, notify: &dyn Fn(Update)) {
        match c {
            Command::StartRecording(r) => self.record_asked(OutputCanvas::Main, &r, notify),
            Command::StartVerticalRecording(r) => {
                self.record_asked(OutputCanvas::Vertical, &r, notify);
            }
            Command::SetVerticalProfile(profile) => self.recording.vertical_profile = profile,
            Command::SetVerticalStreamProfile(profile) => {
                self.streaming.vertical_stream_profile = profile;
                self.availability_changed();
            }
            Command::SetStreamDestinations(destinations, unloaded) => {
                self.streaming.destinations = destinations;
                self.streaming.destinations_unloaded = unloaded;
                self.availability_changed();
            }
            Command::SetSeparateFiles(files) => {
                self.recording.separate_files = files.enabled;
                self.recording.limit_policy = files.limit_policy();
            }
            Command::SetVerticalPicture(on) => {
                // Turning the picture off cannot leave anything to report: the node
                // goes with the next update at the latest.
                if let Err(why) = self.set_vertical_picture(on)
                    && on
                {
                    notify(Update::VerticalPictureFailed);
                    notify(Update::Error(why.into_text()));
                }
                self.publish(notify);
            }
            Command::OpenProjector { id, canvas } => {
                self.open_projector(id, canvas, notify);
                self.publish(notify);
            }
            Command::OpenInteract(source) => self.open_interact(source, notify),
            Command::CloseInteract(source) => {
                self.close_interact(source);
                self.publish(notify);
            }
            Command::InteractInput { source, event } => self.interact_input(source, event),
            Command::CloseProjector(id) => {
                self.close_projector(id);
                self.publish(notify);
            }
            Command::SetCanvas { size, fps } => {
                if let Err(why) = self.change_canvas(size, fps, notify) {
                    notify(Update::Error(why.into_text()));
                }
            }
            Command::SetProfile(profile) => {
                if profile != self.recording.profile {
                    self.recording.profile = profile;
                    self.settings_changed();
                }
            }
            Command::StartStream(r) => self.stream_asked(OutputCanvas::Main, &r, notify),
            Command::StartVerticalStream(r) => {
                self.stream_asked(OutputCanvas::Vertical, &r, notify);
            }
            Command::StartVirtualCamera { width, height, fps } => {
                // An API client started it first: show the camera that runs, no error.
                if self.vcam.camera.is_some() {
                    notify(Update::VirtualCamera(self.camera_status()));
                } else {
                    let settings = VirtualCameraSettings::new(width, height, fps);
                    let _ = self.start_camera_asked(settings, notify, true);
                }
            }
            Command::StopVirtualCamera => self.stop_camera(notify),
            Command::SetReplay(setup) => {
                self.replay.setup = setup;
                self.replay_changed(notify);
            }
            Command::ArmReplay => {
                if let Err(why) = self.arm_replay(notify) {
                    notify(Update::Error(why.into_text()));
                }
            }
            Command::DisarmReplay => self.disarm_replay(notify),
            Command::AddMarker => {
                self.run_ui_api(
                    rbs_api::Command::MarkerAdd(rbs_api::request::MarkerAdd::default()),
                    notify,
                );
            }
            Command::RemoveMarkers(placed) => {
                for (canvas, index) in placed {
                    self.run_ui_api(
                        rbs_api::Command::MarkerRemove(rbs_api::request::MarkerRemove {
                            canvas,
                            index,
                        }),
                        notify,
                    );
                }
            }
            // Routed elsewhere by `Host::command`.
            _ => {}
        }
    }
}

/// The virtual camera: its factory, settings, setup and what was published.
pub(super) struct CameraState {
    /// The platform's virtual camera.
    pub(super) camera_factory: Option<Arc<dyn VirtualCameraFactory>>,
    /// What the virtual camera sends while it sends: started again after a restart.
    pub(super) camera: Option<VirtualCameraSettings>,
    /// When the camera's counters go out next; `None` while it does not send.
    pub(super) camera_due: Option<Timestamp>,
    /// The camera as the app's settings know it; `None` until the app sent it.
    pub(super) camera_setup: Option<CameraSetup>,
    /// The `vcam.changed` state last published.
    pub(super) camera_published: Option<api::VcamStatus>,
}

impl CameraState {
    pub(super) fn new(camera_factory: Option<Arc<dyn VirtualCameraFactory>>) -> Self {
        Self {
            camera_factory,
            camera: None,
            camera_due: None,
            camera_setup: None,
            camera_published: None,
        }
    }
}

/// The vertical canvas: its state, flags, shared scenes and picture.
#[derive(Default)]
pub(super) struct VerticalHost {
    /// The second canvas that runs, and what was last sent about it (ADR-0093).
    pub(super) vertical: Option<VerticalState>,
    /// The saved vertical canvas could not start again; told once at the next publish.
    pub(super) vertical_restore_error: Option<String>,
    /// Whether Record and Go live also act on the vertical outputs (ADR-0111), as the
    /// UI last sent them.
    pub(super) vertical_flags: VerticalFlags,
    /// Scenes `add_vertical` made shared, so removing the canvas can scope them back.
    pub(super) vertical_shared: Vec<SceneId>,
    pub(super) vertical_sent: Option<Option<VerticalState>>,
    /// The vertical canvas's panel shows: its picture runs (ADR-0093 decision 7).
    pub(super) vertical_picture: bool,
    pub(super) vertical_preview_sent: Option<PreviewEndpoint>,
}
