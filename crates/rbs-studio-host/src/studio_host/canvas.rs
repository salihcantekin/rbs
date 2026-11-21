//! The canvas and the settings: size and rate, the restart that changes them, the
//! control API's view of them, item layouts as shown and stored, and recovering from a
//! failed round.

use super::*;

impl Host {
    /// Restarts the studio with a new canvas and tells the UI what runs now (always,
    /// also when refused).
    pub(super) fn change_canvas(
        &mut self,
        size: Size,
        fps: FrameRate,
        notify: &dyn Fn(Update),
    ) -> Result<(), Refusal> {
        let before = self.api_canvas();
        let result = self.set_canvas(size, fps);
        self.canvas_changed(before, notify);
        result
    }

    /// After a restart: tells the UI the canvas (connect to the preview again), shows
    /// the rows anew and publishes a canvas change to control API clients.
    pub(super) fn canvas_changed(&mut self, before: api::Canvas, notify: &dyn Fn(Update)) {
        let (size, fps) = self
            .collection
            .canvas(self.canvas)
            .map_or((Size::new(0, 0), FrameRate::FPS_30), |c| {
                (c.size, c.frame_rate)
            });
        notify(Update::Canvas {
            preview: self.studio.preview(),
            size,
            fps,
        });
        self.rows.clear();
        self.heard.clear();
        // The rows were cleared: build them again even if nothing else changed.
        self.list_stamp = None;
        self.scene_preview_sent = None;
        self.verticals.vertical_preview_sent = None;
        self.publish(notify);
        let now = self.api_canvas();
        if now != before {
            self.events
                .changed(StateKey::topic(Topic::CanvasChanged), || {
                    EventPayload::CanvasChanged(now)
                });
            self.settings_changed();
        }
    }

    /// The main canvas in pixels, for an output size fitted to it (ADR-0102).
    pub(super) fn main_canvas_size(&self) -> (u32, u32) {
        let canvas = self.api_canvas();
        (canvas.width, canvas.height)
    }

    /// The canvas as the control API shows it.
    pub(super) fn api_canvas(&self) -> api::Canvas {
        let (size, rate) = self
            .collection
            .canvas(self.canvas)
            .map_or((Size::new(0, 0), FrameRate::FPS_30), |c| {
                (c.size, c.frame_rate)
            });
        let r = rate.per_second();
        api::Canvas {
            width: size.width,
            height: size.height,
            fps_num: r.num(),
            fps_den: r.den(),
        }
    }

    /// The settings the control thread owns, as the control API shows them.
    pub(super) fn api_settings(&self) -> api::Settings {
        let mut s = api::Settings {
            profiles: vec![self.recording.profile.to_api()],
            canvas: self.api_canvas(),
            stream: self
                .streaming
                .stream_profile
                .as_ref()
                .map(StreamProfile::to_api),
            vertical: Some(self.verticals.vertical_flags.to_api()),
            loudness: Some(api::LoudnessSettings {
                target: self.loudness_state.loudness_settings.target,
                true_peak_limit: self.loudness_state.loudness_settings.true_peak_limit,
            }),
            swap_after_transition: None,
            separate_files: None,
            replay: None,
            vertical_profile: None,
            vertical_stream: None,
        };
        self.api_output_settings(&mut s);
        s
    }

    pub(super) fn settings_changed(&mut self) {
        let settings = self.api_settings();
        self.events
            .changed(StateKey::topic(Topic::SettingsChanged), || {
                EventPayload::SettingsChanged(settings)
            });
    }

    /// Puts meters on the mixer while the Mixer is shown or a client samples levels.
    pub(super) fn sync_meters(&mut self) {
        let on = self.meters_ui || self.events.subscribed(rbs_api::Topic::AudioLevels);
        if on == self.meters_applied {
            return;
        }
        self.studio.set_meters_on(on);
        self.meters_applied = on;
    }

    /// Serves one waiting control API request; true when one was served.
    pub(super) fn serve_api(&mut self, notify: &dyn Fn(Update)) -> bool {
        let Some(api) = self.api.take() else {
            return false;
        };
        // The endpoint answers `events.subscribe` from the publisher it is given, and
        // the target publishes through `self.events`: the publisher moves out for the
        // call and `ApiTarget` swaps it back in while a command runs.
        let mut events = std::mem::take(&mut self.events);
        let served = api.serve_one(&mut ApiTarget { host: self, notify }, &mut events, notify);
        self.events = events;
        self.api = Some(api);
        served
    }

    /// `item`'s box as the studio's canvas draws it now (ADR-0110).
    pub(super) fn shown_layout(&self, item: &SceneItem) -> Layout {
        self.collection
            .placed_layout(self.canvas, item.layout_on(self.canvas))
    }

    /// `live`, which is in the studio canvas's current pixels, in the layout space
    /// the document stores (ADR-0110).
    pub(super) fn stored_layout(&self, live: &Layout) -> Layout {
        self.collection.stored_layout(self.canvas, live)
    }

    /// Stores `live` (current canvas pixels) as the item's layout. One undo record.
    pub(super) fn save_live_layout(
        &mut self,
        id: ItemId,
        canvas: Option<CanvasId>,
        live: Layout,
    ) -> Result<Undo, SceneError> {
        // An own layout is stored in its canvas's space, the default in the studio's.
        let space = canvas.unwrap_or(self.canvas);
        let stored = self.collection.stored_layout(space, &live);
        self.collection.set_item_layout(id, canvas, stored)
    }

    /// Restarts the studio with the canvas at `size` and `fps`. Stored item boxes stay
    /// in the canvas's layout space and are fitted when drawn (ADR-0110). When the new
    /// canvas does not start, the old one starts again and the error says why; when
    /// that fails too, `dead` says why.
    pub(super) fn set_canvas(&mut self, size: Size, fps: FrameRate) -> Result<(), Refusal> {
        if self.any_recording() {
            return Err(Refusal::Conflict(
                "Stop the recording before changing the canvas.".to_owned(),
            ));
        }
        if self.any_streaming() {
            return Err(Refusal::Conflict(
                "End the stream before changing the canvas.".to_owned(),
            ));
        }
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::Failed("the canvas is gone".to_owned()))?;
        let (old_size, old_fps) = (canvas.size, canvas.frame_rate);
        if (old_size, old_fps) == (size, fps) {
            return Ok(());
        }
        let before = self.collection.clone();
        self.collection
            .set_canvas_format(self.canvas, size, fps)
            .map_err(|e| Refusal::Failed(format!("canvas: {e:?}")))?;
        // Layouts stay in the canvas's layout space (ADR-0110). Flattening fits them
        // into `size`; returning to the layout size shows the stored boxes again.
        self.restarting = true;
        let _ = self.studio.shutdown();
        let restarted = self.restart();
        // Still restarting: a panic here leaves the studio dead (ADR-0055 decision 3).
        inject!(self, Fault::DuringCanvasRestart);
        self.restarting = false;
        match restarted {
            Ok(()) => {
                // A canvas change is not an undo step (control API). The layout space is
                // unchanged, so earlier box edits stay valid; they are dropped with the
                // restart so undo cannot cross it.
                self.edits.history.clear();
                Ok(())
            }
            Err(why) => {
                self.note_collection();
                self.collection = before;
                self.restarting = true;
                let again = self.restart();
                self.restarting = false;
                match again {
                    Ok(()) => Err(Refusal::Failed(format!(
                        "The canvas {} x {} cannot be used: {why}",
                        size.width, size.height
                    ))),
                    Err(again) => {
                        self.dead = Some(again.into_text());
                        Err(why)
                    }
                }
            }
        }
    }

    /// Brings the host's bookkeeping back in line with the studio after a caught panic
    /// (ADR-0055 decision 3), `in_row` being how many rounds in a row panicked:
    /// - the command that panicked gets the answer the UI waits for, if any;
    /// - a panic during a canvas restart leaves the studio stopped: it is dead;
    /// - an output the studio runs but the host does not track (a start that panicked
    ///   half-way) is stopped, so no file grows that Stop cannot end;
    /// - a tracked recording or stream that no longer runs (a stop that panicked
    ///   half-way) is ended again;
    /// - after [`CAUGHT_LIMIT`] panics in a row the studio counts as failed, and its
    ///   outputs end, each step under its own boundary.
    ///
    /// Every step runs under its own boundary, so one that panics again does not skip
    /// the others.
    pub(super) fn recover(
        &mut self,
        failed: &mut Option<String>,
        in_row: u32,
        notify: &dyn Fn(Update),
    ) {
        const WHY: &str = "An internal error ended it (a report was saved).";
        if let Some(answer) = self.unanswered.take() {
            notify(answer);
        }
        if std::mem::take(&mut self.restarting) {
            let why = self
                .dead
                .get_or_insert_with(|| {
                    "An internal error stopped the studio while it restarted.".into()
                })
                .clone();
            // The round that panicked does not reach its own dead check, and the next
            // round may not run until something wakes the thread. Say it failed now.
            if failed.is_none() {
                *failed = Some(why.clone());
                notify(Update::Failed(why));
            }
        }
        let running = self.studio.outputs();
        let tracked = |o: &CanvasOutputs| {
            (
                o.recording.as_ref().map(|(_, id)| *id),
                o.streaming.as_ref().map(|(_, _, id)| *id),
            )
        };
        let mut stopped = 0;
        for id in &running {
            let known = self.outs.iter().any(|o| {
                let (r, s) = tracked(o);
                r == Some(*id)
                    || s == Some(*id)
                    || o.destinations.iter().any(|d| d.output == Some(*id))
            });
            if !known {
                let _ = rbs_crash::contain(|| self.studio.stop_output(*id));
                stopped += 1;
            }
        }
        // Separate files without their master: a start that panicked half-way, or a
        // master that is no longer tracked.
        if self.outs[slot(OutputCanvas::Main)].recording.is_none() {
            self.recording.iso = None;
        }
        if self.recording.iso.is_none() && self.studio.iso_video_running() {
            let _ = rbs_crash::contain(|| self.studio.stop_iso_video());
        }
        if self.recording.iso.is_none() && self.studio.iso_audio_running() {
            let _ = rbs_crash::contain(|| self.studio.stop_iso_audio());
        }
        #[cfg(feature = "fault-injection")]
        notify(Update::Recovered { stopped });
        let _ = stopped;
        for c in OutputCanvas::ALL {
            let (recording, streaming) = tracked(&self.outs[slot(c)]);
            if recording.is_some_and(|id| !running.contains(&id)) {
                let _ = rbs_crash::contain(|| self.stop_recording(c, Some(WHY.into()), notify));
            }
            if streaming.is_some_and(|id| !running.contains(&id)) {
                let _ = rbs_crash::contain(|| self.end_stream(c, Some(WHY.into()), notify));
            }
        }
        if self.vcam.camera.is_some() != self.studio.virtual_camera().is_some() {
            self.studio.stop_virtual_camera();
            self.vcam.camera = None;
            notify(Update::VirtualCamera(None));
            self.camera_changed();
        }
        if in_row >= CAUGHT_LIMIT && failed.is_none() {
            let why = String::from(
                "RBS stopped the studio after repeated internal errors (reports were saved).",
            );
            *failed = Some(why.clone());
            for c in OutputCanvas::ALL {
                let _ = rbs_crash::contain(|| self.stop_recording(c, Some(why.clone()), notify));
                let _ = rbs_crash::contain(|| self.end_stream(c, Some(why.clone()), notify));
            }
            for id in self.studio.outputs() {
                let _ = rbs_crash::contain(|| self.studio.stop_output(id));
            }
            for o in &mut self.outs {
                o.recording = None;
                o.streaming = None;
                o.destinations.clear();
            }
            notify(Update::Failed(why));
        }
        let _ = rbs_crash::contain(|| self.publish(notify));
    }

    /// Starts a new studio for the collection as it is.
    pub(super) fn restart(&mut self) -> Result<(), Refusal> {
        let canvas = self
            .collection
            .canvas(self.canvas)
            .ok_or_else(|| Refusal::Failed("the canvas is gone".to_owned()))?;
        let mut settings = StudioSettings::new(self.canvas);
        let preview = self
            .preview_surfaces
            .map(|_| preview_desc(canvas.size, canvas.frame_rate));
        self.preview_surfaces = preview.map(|desc| desc.surfaces);
        settings.preview = preview;
        settings.scene_preview = SCENE_PICTURE;
        settings.audio_offsets = filters::audio_offsets(&self.collection);
        settings.media_pause_release = self.media_pause_release;
        if let Some(v) = self
            .verticals
            .vertical
            .filter(|v| self.collection.canvas(v.canvas).is_some())
        {
            settings.extra_canvases = vec![self.vertical_extra(v.canvas)];
        } else if self.verticals.vertical.take().is_some() {
            self.forget_vertical_outputs();
        }
        let run = self.run_collection();
        // Tickers go on where they were (ADR-0150 decision 5).
        settings.scroll_runs = Some(self.studio.scroll_handover());
        // Timers and elapsed times run on across the restart (ADR-0129 decision 11).
        let timers = self.studio.text_timers();
        let old_now = self.clock.now();
        let engine = (self.engine)();
        let clock = engine.clock();
        let studio = Studio::start_with_tree(
            engine,
            Arc::clone(&self.gpu),
            run.as_ref().unwrap_or(&self.collection),
            &self.adapters.sources,
            &settings,
            self.browser_tree.clone(),
        )
        .map_err(|e| Refusal::Failed(e.to_string()))?;
        self.studio = studio;
        self.clock = clock;
        self.studio.set_text_timers(&timers);
        self.texts.text_elapsed = self.texts.text_elapsed.moved(old_now, self.clock.now());
        self.waiting = self.studio.waiting();
        // Pictures belonged to the old studio; the next publish adds them again with
        // the canvas's size now (ADR-0125 decision 3).
        for p in &mut self.projectors {
            p.picture = None;
        }
        // The camera keeps sending across a canvas change; when it cannot, the next
        // round says so.
        if let Some(settings) = self.vcam.camera.take()
            && self.start_camera(settings).is_err()
        {
            self.vcam.camera_due = Some(self.clock.now());
        }
        self.camera_changed();
        // A collection opened with a saved vertical canvas runs it again (ADR-0093
        // decision 9).
        self.restore_vertical();
        // The replay buffer keeps its place: it is armed again in the new studio.
        self.rearm_replay(&|_| {});
        // The loudness meter measures again, from scratch, in the new studio.
        self.restore_loudness();
        rbs_core::info!(rbs_core::log::Area::Studio, "the studio restarted");
        Ok(())
    }
}
