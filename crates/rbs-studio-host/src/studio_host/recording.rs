//! Recordings: start, pause, markers, stop and the output profile's recording part (ADR-0159 decision 8).

use super::*;

impl Host {
    /// Pauses (`Some(true)`), resumes (`Some(false)`) or toggles (`None`) the
    /// recording of `c` (ADR-0124, control API 1.61) and publishes its new state.
    /// Answers whether it is paused now; `conflict` when nothing records or the
    /// studio refuses.
    pub(super) fn pause_recording(
        &mut self,
        c: OutputCanvas,
        pause: Option<bool>,
    ) -> Result<bool, ApiError> {
        let o = &self.outs[slot(c)];
        let Some(id) = o.recording.as_ref().filter(|_| !o.ending).map(|r| r.1) else {
            return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
        };
        let pause =
            pause.unwrap_or_else(|| !self.studio.output_pause(id).is_some_and(|p| p.paused));
        if !pause && self.outs[slot(c)].hold.pending.is_some() {
            // The stream's hold waits for this pause (ADR-0124 decision 14), as the
            // studio refuses while the hold is on.
            return Err(ApiError::new(
                ErrorKind::Conflict,
                pause_refusal(rbs_record::PauseRefused::Held),
            ));
        }
        let done = if pause {
            self.studio.pause_output(id)
        } else {
            self.studio.resume_output(id)
        };
        let at = done
            .map_err(|e| ApiError::new(ErrorKind::Conflict, pause_refusal(e)))?
            .as_nanos();
        // Markers and the manifest are in file time (ADR-0124 decision 10).
        let join_error = self.studio.output_pause(id).and_then(|p| p.join_error);
        if let Some(markers) = self.outs[slot(c)].markers.as_mut() {
            markers.paused(pause, at, join_error);
        }
        self.refresh_record_pause(c);
        Ok(pause)
    }

    /// Puts the pause fields of `c`'s running recording into its record state, as of
    /// now (published as a change, never per frame).
    pub(super) fn refresh_record_pause(&mut self, c: OutputCanvas) {
        let Some(id) = self.outs[slot(c)].recording.as_ref().map(|r| r.1) else {
            return;
        };
        let Some(progress) = self.studio.output_counters(id) else {
            return;
        };
        let mut state = self.outs[slot(c)].record_state.clone();
        if let api::RecordState::Recording {
            paused,
            paused_ms,
            recorded_ms,
            ..
        } = &mut state
        {
            *paused = progress.paused;
            *paused_ms = progress.paused_ns / 1_000_000;
            *recorded_ms = progress.recorded_ns / 1_000_000;
            self.set_record_state(c, state);
        }
    }

    pub(super) fn set_record_state(&mut self, c: OutputCanvas, state: api::RecordState) {
        self.outs[slot(c)].record_state = state.clone();
        match c {
            OutputCanvas::Main => self
                .events
                .changed(StateKey::topic(Topic::RecordState), || {
                    EventPayload::RecordState(state)
                }),
            OutputCanvas::Vertical => self
                .events
                .changed(canvas_key(Topic::RecordCanvasState, c), || {
                    EventPayload::RecordCanvasState(api::CanvasRecordState { canvas: c, state })
                }),
        };
    }

    /// Starts recording to `r.path`; the file's folder is made first. `gate` is how
    /// a control API start asked for its separate video files (protocol 1.54).
    pub(super) fn start_recording(
        &mut self,
        c: OutputCanvas,
        r: &RecordRequest,
        profile: Option<String>,
        gate: StartGate,
    ) -> Result<RecordingUpdate, StartError> {
        if let Some(why) = self.calibration_blocks() {
            return Err(StartError::Message(format!("{why}.")));
        }
        if self.outs[slot(c)].recording.is_some() {
            return Err(StartError::Message(
                "A recording is already running.".into(),
            ));
        }
        let canvas = self.output_canvas(c)?;
        if !r.path.is_absolute() {
            return Err(format!("{} is not a full path.", r.path.display()).into());
        }
        if c == OutputCanvas::Main && r.iso && self.marks_separate() {
            return self.start_recording_with_tracks(c, r, profile, gate);
        }
        self.start_master(c, canvas, r, &r.path, profile, None)
    }

    /// The start gate of a main recording with separate video files (ADR-0087
    /// decisions 5 and 7, control API 1.54): the plan of the whole start, judged once
    /// before anything is on disk.
    pub(super) fn gate_start(
        &mut self,
        r: &RecordRequest,
        video: VideoSettings,
        marked: &[SourceId],
        sound: bool,
        gate: StartGate,
    ) -> rbs_record::estimate::GateDecision {
        use rbs_record::estimate as est;
        let out = self.master_settings(None, r, &r.path);
        let separate = rbs_record::IsoVideoStart::new(String::new(), r.container, video);
        let mut request = rbs_record::StartPlanRequest::default();
        request.outputs = vec![rbs_record::PlannedOutput::new(&out, r.encoder.as_deref())];
        request.separate = Some(&separate);
        request.sound_files = sound;
        let (plan, declared) = self
            .studio
            .start_plan(&self.adapters, &self.collection, &request);
        // The profile's recording folder: the take folder is new on every start.
        let folder = r.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let limits = self.estimate_limits(declared, &folder);
        let request = match gate {
            StartGate::Older => est::GateRequest::Older,
            StartGate::Asked {
                acknowledge_heavy,
                policy,
            } => est::GateRequest::Asked {
                acknowledge_heavy,
                policy: match policy {
                    LimitPolicy::Ask => est::GatePolicy::Ask,
                    LimitPolicy::Skip => est::GatePolicy::Skip,
                },
            },
        };
        est::gate(&plan, &limits, marked, request)
    }

    /// The `[[skipped]]` entries of the take manifest for `skipped`.
    pub(super) fn manifest_skipped(
        &self,
        skipped: &[api::SkippedTrack],
    ) -> Vec<rbs_manifest::SkippedEntry> {
        skipped
            .iter()
            .map(|t| rbs_manifest::SkippedEntry {
                source: rbs_manifest::SourceRef::new(t.source),
                source_name: self
                    .collection
                    .source(SourceId::from_raw(t.source))
                    .map(|s| s.name.clone()),
                role: wire_name(&t.role),
                reason: wire_name(&t.reason),
            })
            .collect()
    }

    /// A source of the collection is marked to record its picture or its sound
    /// separately.
    pub(super) fn marks_separate(&self) -> bool {
        self.collection
            .sources()
            .any(|s| !s.record_separately.is_none())
    }

    /// The main recording with its separate files (ADR-0086 decisions 5 to 7): one
    /// folder named like the file would be, holding `master.<ext>`, one file per track
    /// and `recording.rbs.toml`. The start gate (ADR-0087 decisions 5 and 7) decides
    /// first, before the folder is made, so its refusal leaves nothing on disk; when it
    /// skips every video file and nothing else is marked, the take is one file. The
    /// folder must not exist, so nothing outside it is touched. The sound files open
    /// first; a failure stops what was opened, removes the files this start may have
    /// made and refuses the recording (a request with `on_hardware_limit: "skip"`
    /// skips them instead). A marked source the program mix does not hear is left out
    /// and listed in `skipped` (keeping it running is decision 8, a later step).
    pub(super) fn start_recording_with_tracks(
        &mut self,
        c: OutputCanvas,
        r: &RecordRequest,
        profile: Option<String>,
        gate: StartGate,
    ) -> Result<RecordingUpdate, StartError> {
        let canvas = self.output_canvas(c)?;
        let folder = r.path.with_extension("");
        let iso_plan = rbs_record::plan(&self.collection).ok();
        let marked = |role: rbs_record::IsoRole| -> Vec<SourceId> {
            let mut v: Vec<SourceId> = iso_plan
                .iter()
                .flat_map(|p| p.tracks.iter())
                .filter(|t| t.role == role)
                .map(|t| t.source)
                .collect();
            v.sort();
            v
        };
        let marked_video = marked(rbs_record::IsoRole::Video);
        let has_images = !marked(rbs_record::IsoRole::Image).is_empty();
        let sound = marked(rbs_record::IsoRole::Audio);
        let skip_audio_on_failure = matches!(
            gate,
            StartGate::Asked {
                policy: LimitPolicy::Skip,
                ..
            }
        );
        let video_skip = |sources: &[SourceId], reason| -> Vec<api::SkippedTrack> {
            sources
                .iter()
                .map(|s| api::SkippedTrack {
                    source: s.get(),
                    role: api::TrackRole::Video,
                    reason,
                })
                .collect()
        };
        // The gate: once, before the folder is made and before any file opens.
        let mut pictures = None;
        let mut decided: Vec<api::SkippedTrack> = Vec::new();
        if let Some(video) = r.video.clone()
            && !marked_video.is_empty()
        {
            let decision = self.gate_start(r, video, &marked_video, !sound.is_empty(), gate);
            if let Some((why, judged, tracks)) = decision.refusal {
                return Err(gate_refusal(api_refusal(why), &judged, &tracks, &[]));
            }
            let no_encoder = !decision.no_encoder.is_empty();
            let keep = match gate {
                StartGate::Older if !no_encoder => None,
                _ => Some(decision.keep.clone()),
            };
            if no_encoder {
                decided = video_skip(&marked_video, api::SkipReason::OpenFailed);
            } else if keep.is_some() {
                decided = video_skip(&decision.limit, api::SkipReason::Limit);
            }
            let nothing_left = match &keep {
                Some(k) => k.is_empty(),
                None => decision.count == 0,
            };
            if nothing_left && !has_images && sound.is_empty() {
                // One file, as 1.46 writes (ADR-0087 decision 7): nothing separate is
                // left. Its skipped files are only in the answer and `record.status`.
                let skipped = if decided.is_empty() {
                    video_skip(&marked_video, api::SkipReason::Limit)
                } else {
                    decided
                };
                let started = self.start_master(c, canvas, r, &r.path, profile, None)?;
                if let api::RecordState::Recording { skipped: s, .. } =
                    &mut self.outs[slot(c)].record_state
                {
                    *s = skipped;
                    let state = self.outs[slot(c)].record_state.clone();
                    self.set_record_state(c, state);
                }
                return Ok(started);
            }
            pictures = Some(PictureGate {
                keep,
                count: decision.count,
                no_encoder,
                ask: matches!(
                    gate,
                    StartGate::Asked {
                        policy: LimitPolicy::Ask,
                        ..
                    }
                ),
                judged: decision.estimate,
            });
        }
        make_recording_folder(&folder)?;
        let master_name = format!("{}.{}", rbs_record::MASTER_STEM, container_ext(r.container));
        let planned = planned_names(iso_plan.as_ref(), r.container, &master_name);
        let t0_ns = self.clock.now().as_nanos();
        let t0_utc = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        // The sound files open before the master, so they follow a window made now
        // that the master is then started with (ADR-0124).
        let pause = rbs_record::SharedPause::new();
        let start = iso_audio_start(&folder, &master_name, &t0_utc, t0_ns, &pause);
        let (mut manifest, mut skipped) = match self.studio.start_iso_audio(&self.adapters, &start)
        {
            Ok(started) => {
                let skipped: Vec<api::SkippedTrack> = started
                    .not_heard
                    .iter()
                    .map(|(source, _)| api::SkippedTrack {
                        source: source.get(),
                        role: api::TrackRole::Audio,
                        reason: api::SkipReason::NotHeard,
                    })
                    .collect();
                (started.manifest, skipped)
            }
            Err(e) => {
                // `on_hardware_limit: "skip"` (ADR-0087 decision 5): the take goes on
                // without its sound files.
                let without_sound = iso_plan
                    .as_ref()
                    .filter(|_| skip_audio_on_failure)
                    .and_then(|plan| {
                        rbs_record::manifest_at_start(plan, &master_name, &t0_utc, t0_ns).ok()
                    });
                let Some(mut manifest) = without_sound else {
                    remove_recording_folder(&folder, &planned);
                    return Err(format!("The separate files could not start: {e}").into());
                };
                manifest.files.retain(|f| f.role != "audio");
                let skipped = sound
                    .iter()
                    .map(|s| api::SkippedTrack {
                        source: s.get(),
                        role: api::TrackRole::Audio,
                        reason: api::SkipReason::OpenFailed,
                    })
                    .collect();
                (manifest, skipped)
            }
        };
        skipped.extend(decided);
        skipped.sort_by_key(|s| s.source);
        // Picture tracks open once the master runs (sources draw only then); until
        // then the manifest lists only the files that are open.
        manifest
            .files
            .retain(|f| f.role != "video" && f.role != "image");
        manifest.skipped = self.manifest_skipped(&skipped);
        if let Err(e) = rbs_record::write_manifest(&folder, &manifest) {
            self.studio.stop_iso_audio();
            remove_recording_folder(&folder, &planned);
            return Err(format!("{e}.").into());
        }
        let iso = IsoFiles {
            tracks: audio_tracks(&manifest),
            skipped,
            manifest: manifest.clone(),
            folder: folder.clone(),
            images: Vec::new(),
            gate: pictures,
            software_stopped: Vec::new(),
            pause,
        };
        let master = folder.join(&master_name);
        match self.start_master(c, canvas, r, &master, profile, Some(iso)) {
            Ok(started) => Ok(started),
            Err(e) => {
                self.studio.stop_iso_audio();
                let left = remove_recording_folder(&folder, &planned);
                Err(match e {
                    StartError::Refused(m, why) if !left.is_empty() => StartError::Refused(
                        format!("{m} These files could not be removed: {}.", left.join(", ")),
                        why,
                    ),
                    e => e,
                })
            }
        }
    }

    /// Opens the separate video files and image tracks of the main recording, once its
    /// master runs (ADR-0086 step 3.5, ADR-0117), as the start gate decided (ADR-0087
    /// decisions 5 and 7). A source with no picture, a file the gate skipped, a
    /// recording with no picture or files that cannot open are listed in `skipped`, and
    /// the manifest is written again with what is open. Only a request with
    /// `on_hardware_limit: "ask"` is refused here, when a file cannot open; the video
    /// files that did open are still open and the caller stops them.
    pub(super) fn start_separate_pictures(
        &mut self,
        files: &mut IsoFiles,
        r: &RecordRequest,
        master: rbs_record::OutputId,
    ) -> Result<(), StartError> {
        let gate = files.gate.take();
        let Ok(plan) = rbs_record::plan(&self.collection) else {
            return Ok(());
        };
        let pictures: Vec<_> = plan
            .tracks
            .iter()
            .filter(|t| {
                matches!(
                    t.role,
                    rbs_record::IsoRole::Video | rbs_record::IsoRole::Image
                )
            })
            .collect();
        if pictures.is_empty() {
            return Ok(());
        }
        let already = |files: &IsoFiles, source: u32, role: api::TrackRole| {
            files
                .skipped
                .iter()
                .any(|s| s.source == source && s.role == role)
        };
        let skip_all = |files: &mut IsoFiles, reason: api::SkipReason| {
            for t in &pictures {
                let role = api_track_role(t.role);
                if !already(files, t.source.get(), role) {
                    files.skipped.push(api::SkippedTrack {
                        source: t.source.get(),
                        role,
                        reason,
                    });
                }
            }
        };
        // A WAV or FLAC take draws no picture and has no encoder settings to match.
        let Some(video) = r.video.clone() else {
            skip_all(files, api::SkipReason::AudioOnly);
            files.skipped.sort_by_key(|s| s.source);
            files.manifest.skipped = self.manifest_skipped(&files.skipped);
            let _ = rbs_record::write_manifest(&files.folder, &files.manifest);
            return Ok(());
        };
        let mut start = rbs_record::IsoVideoStart::new(
            rbs_transport::file_url(&files.folder),
            r.container,
            video,
        );
        start.image_dir = Some(files.folder.clone());
        start.t0 = Timestamp::from_nanos(files.manifest.t0_ns);
        // The video files pause with their master (ADR-0124).
        start.pause_with = Some(master);
        start.skip_no_picture = true;
        // A file whose hardware encoder refuses a session is skipped, never moved to a
        // software encoder (ADR-0087 decision 5).
        start.stop_before_software = true;
        start.skip_unopened = true;
        let ask = gate.as_ref().is_some_and(|g| g.ask);
        let no_encoder = gate.as_ref().is_some_and(|g| g.no_encoder);
        if let Some(g) = &gate {
            start.keep.clone_from(&g.keep);
            start.max_video = g.count;
        }
        let kept_video = || -> Vec<rbs_record::estimate::TrackRef> {
            gate.as_ref()
                .and_then(|g| g.keep.clone())
                .unwrap_or_default()
                .into_iter()
                .map(|source| rbs_record::estimate::TrackRef {
                    source,
                    role: rbs_record::IsoRole::Video,
                })
                .collect()
        };
        let started = match self.studio.start_iso_video(&self.adapters, &start) {
            Ok(started) => started,
            Err(e) => {
                if let Some(g) = gate.as_ref().filter(|g| g.ask) {
                    let mut refusal = gate_refusal(
                        api::RefusalReason::OpenFailed,
                        &g.judged,
                        &kept_video(),
                        &[],
                    );
                    if let StartError::Refused(m, _) = &mut refusal {
                        *m = format!("The separate video files could not open: {e}. {m}");
                    }
                    return Err(refusal);
                }
                skip_all(files, api::SkipReason::OpenFailed);
                files.skipped.sort_by_key(|s| s.source);
                files.manifest.skipped = self.manifest_skipped(&files.skipped);
                let _ = rbs_record::write_manifest(&files.folder, &files.manifest);
                return Ok(());
            }
        };
        let unopened: Vec<&rbs_record::IsoSkipped> = started
            .skipped
            .iter()
            .filter(|s| {
                matches!(
                    s.reason,
                    rbs_record::IsoSkip::EncoderRefused | rbs_record::IsoSkip::SoftwareStopped
                )
            })
            .collect();
        let software: Vec<SourceId> = unopened
            .iter()
            .filter(|s| s.reason == rbs_record::IsoSkip::SoftwareStopped)
            .map(|s| s.source)
            .collect();
        if ask
            && !unopened.is_empty()
            && let Some(g) = &gate
        {
            let tracks: Vec<_> = unopened
                .iter()
                .map(|s| rbs_record::estimate::TrackRef {
                    source: s.source,
                    role: s.role,
                })
                .collect();
            return Err(gate_refusal(
                api::RefusalReason::HardwareLimit,
                &g.judged,
                &tracks,
                &software,
            ));
        }
        files.software_stopped = software;
        for s in &started.skipped {
            let role = api_track_role(s.role);
            if already(files, s.source.get(), role) {
                continue;
            }
            files.skipped.push(api::SkippedTrack {
                source: s.source.get(),
                role,
                reason: match s.reason {
                    rbs_record::IsoSkip::Limit if no_encoder => api::SkipReason::OpenFailed,
                    rbs_record::IsoSkip::Limit => api::SkipReason::Limit,
                    rbs_record::IsoSkip::NoPicture => api::SkipReason::NoPicture,
                    // An encoder that refused, a stop before a software encoder, or a
                    // reason added later to the engine, not yet named here.
                    _ => api::SkipReason::OpenFailed,
                },
            });
        }
        let opened = started
            .tracks
            .iter()
            .map(|t| (t.source, rbs_record::IsoRole::Video, t.file_name.clone()))
            .chain(
                started
                    .images
                    .iter()
                    .map(|t| (t.source, rbs_record::IsoRole::Image, t.file_name.clone())),
            );
        for (source, role, file) in opened {
            files.tracks.push(api::RecordTrack {
                file: file.clone(),
                role: api_track_role(role),
                source: source.get(),
            });
            let mut entry = rbs_manifest::ManifestFile::listed(role.as_str(), &file);
            entry.source = Some(rbs_manifest::SourceRef::new(source.get()));
            entry.source_name = self.collection.source(source).map(|s| s.name.clone());
            files.manifest.files.push(entry);
        }
        files.images = started.images;
        files.tracks.sort_by_key(|t| t.source);
        files.skipped.sort_by_key(|s| s.source);
        files.manifest.skipped = self.manifest_skipped(&files.skipped);
        // The start manifest named only the files open before the master. The same
        // folder took that write a moment ago; if this one fails, the files are still
        // valid and the stop write lists the image tracks.
        let _ = rbs_record::write_manifest(&files.folder, &files.manifest);
        Ok(())
    }

    /// The output settings of a recording of `r` into `path` on `canvas`.
    pub(super) fn master_settings(
        &self,
        canvas: Option<CanvasId>,
        r: &RecordRequest,
        path: &Path,
    ) -> OutputSettings {
        let mut out = OutputSettings::new(rbs_transport::file_url(path), r.container);
        out.canvas = canvas;
        out.video = r.video.clone();
        out.audio = self.adapters.audio_encoders.ids().next().map(|_| {
            let mut a = AudioSettings::default();
            if r.video.is_none() {
                a.codec = if r.container == ContainerId::Flac {
                    CodecId::Flac
                } else {
                    CodecId::Pcm
                };
                a.bitrate_bps = 0;
                a.bit_depth = r.bit_depth;
            } else {
                a.bitrate_bps = r.audio_bitrate_bps;
            }
            a
        });
        // The size this recording starts with. A later settings change waits for the
        // next start (ADR-0102). `None` stays the canvas, including a side above 7680.
        // An audio-only file has no picture size.
        out.size = if r.video.is_none() { None } else { r.size };
        out
    }

    /// The estimate of a start of the saved recording profile on `c` (or on every
    /// canvas with `all`), next to what runs (`record.estimate`, ADR-0087 decision 7).
    /// A recording that runs on a canvas the start covers is left out, as if it had
    /// stopped. Opens nothing and never waits.
    pub(super) fn record_estimate(
        &mut self,
        c: OutputCanvas,
        all: bool,
        iso: Option<bool>,
    ) -> Result<api::EstimateAnswer, String> {
        self.ensure_encoders();
        let canvases: Vec<OutputCanvas> = if all { self.output_canvases() } else { vec![c] };
        let mut settings = Vec::new();
        let mut encoders = Vec::new();
        let mut replacing = Vec::new();
        let mut separate = None;
        let mut sound_files = false;
        let mut folder = None;
        let mut replacing_separate = false;
        for one in canvases {
            let path = self.recording_file(one);
            let r = self.profile_request(one, path.clone())?;
            let canvas = self.output_canvas(one)?;
            settings.push(self.master_settings(canvas, &r, &path));
            encoders.push(r.encoder.clone());
            if let Some((_, id)) = self.outs[slot(one)].recording {
                replacing.push(id);
            }
            if one == OutputCanvas::Main {
                folder = path.parent().map(Path::to_path_buf);
                replacing_separate = self.outs[slot(one)].recording.is_some();
                if iso.unwrap_or(self.separate_files) && self.marks_separate() {
                    sound_files = true;
                    separate = r.video.clone().map(|video| {
                        rbs_record::IsoVideoStart::new(String::new(), r.container, video)
                    });
                }
            }
        }
        let mut request = rbs_record::StartPlanRequest::default();
        request.outputs = settings
            .iter()
            .zip(&encoders)
            .map(|(s, e)| rbs_record::PlannedOutput::new(s, e.as_deref()))
            .collect();
        request.separate = separate.as_ref();
        request.sound_files = sound_files;
        request.replacing = replacing;
        request.replacing_separate = replacing_separate;
        let (plan, declared) = self
            .studio
            .start_plan(&self.adapters, &self.collection, &request);
        let folder = folder.unwrap_or_default();
        let limits = self.estimate_limits(declared, &folder);
        let estimate = rbs_record::estimate::estimate(&plan, &limits);
        let at_most_now = plan
            .video_template
            .is_some()
            .then(|| rbs_record::estimate::separate_video_limit(&estimate));
        Ok(api_estimate(&estimate, at_most_now))
    }

    /// The estimate of what runs now, for `record.start`'s answer (protocol 1.53): the
    /// plan of what the start decided to open, which now runs.
    pub(super) fn running_estimate(&mut self, c: OutputCanvas) -> Option<api::EstimateSummary> {
        let folder = self.outs[slot(c)].recording.as_ref().and_then(|(p, _)| {
            // A take with separate files is a folder inside the profile's folder.
            let parent = p.parent()?;
            Some(if self.iso.is_some() && c == OutputCanvas::Main {
                parent.parent().unwrap_or(parent).to_path_buf()
            } else {
                parent.to_path_buf()
            })
        })?;
        let request = rbs_record::StartPlanRequest::default();
        let (plan, declared) = self
            .studio
            .start_plan(&self.adapters, &self.collection, &request);
        let limits = self.estimate_limits(declared, &folder);
        let e = rbs_record::estimate::estimate(&plan, &limits);
        let mut a = api_estimate(&e, None);
        if c == OutputCanvas::Main
            && let Some(files) = &self.iso
        {
            add_software_fallback(&mut a.reasons, &files.software_stopped);
        }
        Some(api::EstimateSummary {
            level: a.level,
            unknown: a.unknown,
            reasons: a.reasons,
        })
    }

    /// Starts the recording of `c` into `path` with `r`'s settings.
    pub(super) fn start_master(
        &mut self,
        c: OutputCanvas,
        canvas: Option<CanvasId>,
        r: &RecordRequest,
        path: &Path,
        profile: Option<String>,
        mut iso: Option<IsoFiles>,
    ) -> Result<RecordingUpdate, StartError> {
        if let Some(folder) = path.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|e| format!("Cannot make the folder {}: {e}", folder.display()))?;
        }
        if path.exists() {
            return Err(format!("{} already exists.", path.display()).into());
        }
        let mut out = self.master_settings(canvas, r, path);
        // The master keeps its timing only for a take that can become an editor
        // project: one with separate files or with the project setting on (P13).
        out.track_span = r.editor_project.is_some() || iso.is_some();
        // Every recording can be paused (ADR-0124): its export and audio encoder
        // follow a window, and its separate files follow the same one.
        out.pausable = true;
        out.shared_pause = iso.as_ref().map(|f| f.pause.clone());
        let started = self.start_output(&out, r.encoder.as_deref());
        inject!(self, Fault::AfterOutputStart);
        if let Ok((id, _, _)) = &started
            && let Some(files) = iso.as_mut()
        {
            let id = *id;
            if let Err(refusal) = self.start_separate_pictures(files, r, id) {
                // A refusal after the master runs (`on_hardware_limit: "ask"`): stop the
                // video files that opened and the master; the caller stops the sound
                // files and removes the folder. An encoder the master shares with a
                // running stream keeps running for it (ADR-0047).
                self.studio.stop_iso_video();
                let _ = self.studio.stop_output(id);
                return Err(refusal);
            }
        }
        let separate: Vec<String> = iso
            .as_ref()
            .map(|f| {
                f.tracks
                    .iter()
                    .filter_map(|t| {
                        let what = match t.role {
                            api::TrackRole::Video => "video",
                            api::TrackRole::Image => "images",
                            _ => "sound",
                        };
                        self.collection
                            .source(SourceId::from_raw(t.source))
                            .map(|s| format!("{} ({what})", s.name))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let started = started.map(|(id, video_encoder, audio_encoder)| {
            (
                id,
                RecordingUpdate::Started {
                    path: path.to_path_buf(),
                    video_encoder,
                    audio_encoder,
                    separate,
                },
            )
        });
        if let Ok((
            id,
            RecordingUpdate::Started {
                video_encoder,
                audio_encoder,
                ..
            },
        )) = &started
        {
            self.outs[slot(c)].recording = Some((path.to_path_buf(), *id));
            // Markers count from the separate files' `t0`, or from now for one file.
            let (t0_ns, t0_utc) = iso.as_ref().map_or_else(
                || {
                    (
                        self.clock.now().as_nanos(),
                        chrono::Utc::now()
                            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
                            .to_string(),
                    )
                },
                |f| (f.manifest.t0_ns, f.manifest.t0_utc.clone()),
            );
            let scene = self.program_for(c);
            self.outs[slot(c)].markers = RecordingMarkers::new(
                path,
                t0_ns,
                t0_utc,
                iso.as_ref(),
                r.editor_project,
                r.markers,
                scene,
            )
            .map(|mut m| {
                m.chapters = self.seams.chapters_for(r.container);
                m
            });
            // Tracked together with the master, after the last panic point of the
            // start, so recovery never finds files without their master (ADR-0055).
            let (tracks, skipped) = iso
                .as_ref()
                .map(|f| (f.tracks.clone(), f.skipped.clone()))
                .unwrap_or_default();
            if c == OutputCanvas::Main {
                self.iso = iso;
            }
            self.set_record_state(
                c,
                api::RecordState::Recording {
                    profile,
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                    tracks,
                    skipped,
                    paused: false,
                    paused_ms: 0,
                    recorded_ms: 0,
                },
            );
        } else if std::fs::metadata(path).is_ok_and(|m| m.len() == 0) {
            // The output may have made the file before a later node failed.
            let _ = std::fs::remove_file(path);
        }
        started.map(|(_, update)| update).map_err(StartError::from)
    }

    /// Scene markers (ADR-0089 decision 3): a recording whose canvas now shows
    /// another program scene than it last marked gets a `scene` marker named after
    /// it. Runs after every change that reaches the studio, so a switch, a transition
    /// and an undo each mark once; nested scenes are not program scenes.
    pub(super) fn mark_scene_changes(&mut self) {
        let now = self.clock.now().as_nanos();
        for c in OutputCanvas::ALL {
            let program = self.program_for(c);
            let name = program
                .and_then(|id| self.collection.scene(id))
                .map(|s| s.name.clone());
            let Some(state) = self.outs[slot(c)].markers.as_mut() else {
                continue;
            };
            if program == state.scene {
                continue;
            }
            state.scene = program;
            let Some(name) = name.filter(|_| state.settings.scene_changes) else {
                continue;
            };
            // A full log refuses it; the recording goes on.
            let _ = state.log.add_auto(
                now,
                rbs_manifest::MarkerKind::Scene,
                &name,
                rbs_manifest::MarkerColor::Purple,
            );
        }
    }

    /// Stream markers (ADR-0089 decision 3): the recording on the stream's canvas
    /// gets `stream_start` or `stream_end` when its settings ask for them.
    pub(super) fn mark_stream(&mut self, c: OutputCanvas, started: bool) {
        let now = self.clock.now().as_nanos();
        let Some(state) = self.outs[slot(c)].markers.as_mut() else {
            return;
        };
        if !state.settings.stream_events {
            return;
        }
        let (kind, label, color) = if started {
            (
                rbs_manifest::MarkerKind::StreamStart,
                "Stream started",
                rbs_manifest::MarkerColor::Green,
            )
        } else {
            (
                rbs_manifest::MarkerKind::StreamEnd,
                "Stream ended",
                rbs_manifest::MarkerColor::Red,
            )
        };
        let _ = state.log.add_auto(now, kind, label, color);
    }

    /// `marker.add` (ADR-0089 decisions 1 and 11): a manual marker in every running
    /// recording `p.canvas` names (absent: all). Checked on every target before any
    /// marker is added, so a refusal adds none.
    pub(super) fn add_marker(
        &mut self,
        p: &rbs_api::request::MarkerAdd,
    ) -> Result<rbs_api::request::MarkersAdded, ApiError> {
        let label = rbs_manifest::checked_label(p.label.as_deref())
            .map_err(|e| ApiError::new(ErrorKind::InvalidParams, e.to_string()))?;
        let targets: Vec<OutputCanvas> = match p.canvas.unwrap_or(api::OutputTarget::All).single() {
            Some(one) => vec![self.api_canvas_of(Some(one))?],
            None => OutputCanvas::ALL.to_vec(),
        };
        let running: Vec<OutputCanvas> = targets
            .into_iter()
            .filter(|c| self.outs[slot(*c)].markers.is_some())
            .collect();
        if running.is_empty() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "no recording is running, so there is nothing to mark",
            ));
        }
        if running.iter().any(|c| {
            self.outs[slot(*c)]
                .markers
                .as_ref()
                .is_some_and(|m| m.log.markers().len() >= rbs_manifest::MAX_MARKERS)
        }) {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                rbs_manifest::MarkerError::Full.to_string(),
            ));
        }
        let now = self.clock.now().as_nanos();
        let color = marker_color(p.color);
        let mut markers = Vec::with_capacity(running.len());
        for c in running {
            let Some(state) = self.outs[slot(c)].markers.as_mut() else {
                continue;
            };
            let m = state
                .log
                .add_manual(now, label.as_deref(), color)
                .map_err(|e| ApiError::new(ErrorKind::Conflict, e.to_string()))?;
            markers.push(rbs_api::request::MarkerPlaced {
                canvas: c,
                index: m.index,
                t_ns: m.t_ns,
                while_paused: m.while_paused,
            });
        }
        Ok(rbs_api::request::MarkersAdded { markers })
    }

    /// `marker.remove` (ADR-0089 decision 11).
    pub(super) fn remove_marker(&mut self, c: OutputCanvas, index: u32) -> Result<(), ApiError> {
        let c = self.api_canvas_of(Some(c))?;
        let Some(state) = self.outs[slot(c)].markers.as_mut() else {
            return Err(ApiError::new(ErrorKind::Conflict, "nothing records"));
        };
        if state.log.remove(index) {
            Ok(())
        } else {
            Err(ApiError::new(
                ErrorKind::NotFound,
                format!("the recording has no marker {index}"),
            ))
        }
    }

    /// The marker flush timers (ADR-0089 decision 5): a recording with a marker not
    /// yet on disk arms a one-shot 5-second timer; when it is due the manifest is
    /// rewritten. Returns the time to the next one. No marker, no timer (P13).
    pub(super) fn drive_marker_flush(
        &mut self,
        now: rbs_core::time::Timestamp,
        notify: &dyn Fn(Update),
    ) -> Option<Duration> {
        let now_ns = now.as_nanos();
        let mut next: Option<u64> = None;
        for c in OutputCanvas::ALL {
            let Some(m) = self.outs[slot(c)].markers.as_mut() else {
                continue;
            };
            if !m.log.has_pending() {
                continue;
            }
            let due = *m
                .flush_at
                .get_or_insert(now_ns.saturating_add(MARKER_FLUSH_NS));
            if now_ns < due {
                next = Some(next.map_or(due, |n| n.min(due)));
                continue;
            }
            if let Err(e) = m.flush(now_ns) {
                notify(Update::Error(format!(
                    "The markers could not be saved yet: {e}"
                )));
            }
        }
        next.map(|due| Duration::from_nanos(due.saturating_sub(now_ns)))
    }

    /// [`Update::StreamHold`]: each live stream's hold state now.
    pub(super) fn hold_update(&self) -> Update {
        let state = |c: OutputCanvas| {
            self.outs[slot(c)].streaming.as_ref()?;
            let hold = self.outs[slot(c)]
                .streaming
                .as_ref()
                .and_then(|s| self.studio.output_hold(s.2))
                .unwrap_or_default();
            let note = if hold.fallback.is_some() {
                Some(HoldNote::Picture)
            } else if hold.sound_fallback.is_some() {
                Some(HoldNote::Sound)
            } else {
                None
            };
            Some(StreamHold {
                held: self.stream_held(c),
                note,
            })
        };
        Update::StreamHold([state(OutputCanvas::Main), state(OutputCanvas::Vertical)])
    }

    /// [`Update::RecordPause`]: each running recording's pause state now.
    pub(super) fn pause_update(&self) -> Update {
        let state = |c: OutputCanvas| {
            let o = &self.outs[slot(c)];
            let id = o.recording.as_ref().filter(|_| !o.ending)?.1;
            let progress = self.studio.output_counters(id)?;
            Some(RecordPause {
                paused: progress.paused,
                recorded_ns: progress.recorded_ns,
            })
        };
        Update::RecordPause([state(OutputCanvas::Main), state(OutputCanvas::Vertical)])
    }

    /// [`Update::Markers`]: each running recording's manual marker count and, after
    /// an add, the marker just placed.
    pub(super) fn markers_update(&self, added: Option<&rbs_api::request::MarkersAdded>) -> Update {
        let manual = |c: OutputCanvas| {
            self.outs[slot(c)].markers.as_ref().map(|m| {
                let n = m
                    .log
                    .markers()
                    .iter()
                    .filter(|k| k.kind == rbs_manifest::MarkerKind::Manual)
                    .count();
                u32::try_from(n).unwrap_or(u32::MAX)
            })
        };
        let placed = added.and_then(|a| {
            let first = a.markers.first()?;
            let label = self.outs[slot(first.canvas)]
                .markers
                .as_ref()?
                .log
                .markers()
                .iter()
                .find(|m| m.index == first.index)?
                .label
                .clone();
            Some(MarkerNote {
                label,
                t_ns: first.t_ns,
                placed: a.markers.iter().map(|m| (m.canvas, m.index)).collect(),
            })
        });
        Update::Markers {
            counts: [manual(OutputCanvas::Main), manual(OutputCanvas::Vertical)],
            placed,
        }
    }

    /// `project.export` (ADR-0089 decisions 8 and 11): an editor project from a
    /// finished recording in the profile's recordings folder. `recording` is one path
    /// segment: a recording folder's name, a plain recording's file name or its
    /// manifest's name. Runs on the control thread; writing takes milliseconds.
    pub(super) fn export_project(
        &self,
        recording: &str,
        format: api::ProjectFormat,
    ) -> Result<rbs_api::request::ProjectExported, ApiError> {
        if !rbs_manifest::single_file_name(recording) {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "recording is one name inside the recordings folder",
            ));
        }
        let folder = &self.profile.folder;
        let named = folder.join(recording);
        let (dir, manifest) = if named.is_dir() {
            (named, rbs_manifest::FOLDER_MANIFEST.to_owned())
        } else if recording.ends_with(rbs_manifest::FILE_SUFFIX) {
            (folder.clone(), recording.to_owned())
        } else {
            let Some(name) = rbs_manifest::manifest_name_for(recording) else {
                return Err(ApiError::new(ErrorKind::NotFound, "no such recording"));
            };
            (folder.clone(), name)
        };
        if !dir.join(&manifest).is_file() {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "the recording has no manifest: it was recorded without separate files, \
                 markers or the editor project setting",
            ));
        }
        let format = match format {
            api::ProjectFormat::Fcpxml => rbs_project::Format::Fcpxml,
            api::ProjectFormat::Xmeml => rbs_project::Format::Xmeml,
        };
        match rbs_project::export(&dir, &manifest, format, false) {
            Ok(done) => Ok(rbs_api::request::ProjectExported {
                file: done
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                missing: done.missing,
            }),
            Err(rbs_project::ProjectError::Incomplete) => Err(failed(
                "the recording has no timing for an editor project: it was recorded without \
                 separate files or the editor project setting, or it did not stop cleanly",
            )),
            Err(e) => Err(failed(e.to_string())),
        }
    }

    /// The recording settings of `c`: the vertical canvas uses the main ones until
    /// its own are saved.
    pub(super) fn record_profile(&self, c: OutputCanvas) -> &RecordProfile {
        match c {
            OutputCanvas::Main => &self.profile,
            OutputCanvas::Vertical => self.vertical_profile.as_ref().unwrap_or(&self.profile),
        }
    }

    /// A new file for a recording of `c` with its saved settings, named for now.
    /// What the estimate knows about this computer for a start that records into
    /// `folder` (ADR-0087 decision 1): the adapters' declared capabilities, the
    /// calibration entries that still hold and the folder's cached free space. Never
    /// waits: a folder with no fresh reading is unknown now and read for the next call.
    pub(super) fn estimate_limits(
        &mut self,
        adapters: Vec<rbs_record::estimate::DeclaredAdapter>,
        folder: &Path,
    ) -> rbs_record::estimate::Limits {
        let this = self.this_computer();
        let mut calibration = self.calibration.for_estimate(&this, folder);
        calibration.exports_unmeasurable = self.calibration_run.exports_unmeasurable;
        let now = self.clock.now();
        let free_space = self
            .disk_probe
            .as_mut()
            .and_then(|p| p.free_space(folder, now).bytes());
        rbs_record::estimate::Limits {
            adapters,
            calibration: Some(calibration),
            free_space,
            previous_tick: None,
        }
    }

    /// This computer's adapters and RBS version, for the calibration file.
    pub(super) fn this_computer(&self) -> crate::calibration_file::ThisComputer {
        crate::calibration_file::ThisComputer {
            // No adapter reports `adapter.version` yet (ADR-0087 decision 4).
            adapters: self
                .adapters
                .video_encoders
                .ids()
                .map(|id| (id.as_str().to_owned(), None))
                .collect(),
            rbs_version: env!("CARGO_PKG_VERSION").to_owned(),
            // The GPU, its driver and the OS (read once by the root): export
            // measurements hold only on the same (step D2).
            gpu: Some(self.seams.computer_key(&self.gpu.adapter_key())),
        }
    }

    pub(super) fn recording_file(&self, c: OutputCanvas) -> PathBuf {
        let now = chrono::Local::now().naive_local();
        let p = self.record_profile(c);
        match c {
            OutputCanvas::Main => p.file_for(now),
            OutputCanvas::Vertical => p.vertical_file_for(now),
        }
    }

    /// The UI's Record for `c`: starts the recording and says how it went.
    pub(super) fn record_asked(
        &mut self,
        c: OutputCanvas,
        r: &RecordRequest,
        notify: &dyn Fn(Update),
    ) {
        self.note_encoders(notify);
        if !self.record_profile(c).format.is_audio_only()
            && let Some(why) = &self.cannot_record
        {
            notify(recording_update(
                c,
                RecordingUpdate::NotStarted(why.clone()),
            ));
            return;
        }
        let update = match self.start_recording(c, r, Some(API_PROFILE_ID.into()), StartGate::Older)
        {
            Ok(update) => update,
            Err(why) => RecordingUpdate::NotStarted(why.into_message()),
        };
        notify(recording_update(c, update));
        self.publish(notify);
    }

    /// Stops `c`'s recording, if one runs, and says so.
    pub(super) fn stop_recording(
        &mut self,
        c: OutputCanvas,
        failure: Option<String>,
        notify: &dyn Fn(Update),
    ) {
        let o = &mut self.outs[slot(c)];
        o.ending = false;
        o.auto_recording = false;
        o.hold.paused_recording = false;
        let protected = o.protection_stop.take();
        // Tracked until the end: when a stop panics half-way, `Host::recover` finds the
        // recording still tracked and ends it again (ADR-0055 decision 3).
        let Some((path, id)) = o.recording.clone() else {
            return;
        };
        let report = self.studio.stop_output(id);
        // The separate files end with their master (ADR-0086 decision 5).
        let mut separate = Vec::new();
        if c == OutputCanvas::Main
            && let Some(files) = self.iso.take()
        {
            self.studio.stop_iso_video();
            self.studio.stop_iso_audio();
            separate = self.studio.take_iso_media();
            if let Some(markers) = self.outs[slot(c)].markers.as_mut() {
                markers.set_images(&files.images);
            }
        }
        let now = self.clock.now().as_nanos();
        if let Some(markers) = self.outs[slot(c)].markers.as_mut() {
            markers.stopped_ns = now.saturating_sub(markers.log.t0_ns());
            markers.set_ended(report.as_ref().and_then(|r| r.media), separate);
        }
        inject!(self, Fault::InRecordingStop);
        let (frames, dropped, flush_lost, failed) = report.map_or((0, 0, 0, None), |r| {
            (
                r.progress.frames_encoded,
                r.progress.export_drops + r.progress.encoder_skipped + r.progress.encoder_drops,
                r.flush_lost,
                r.failure,
            )
        });
        let mut failure = failure.or(failed).or(protected);
        // The markers go into the manifest once the media is closed (ADR-0089
        // decision 5); a failure there does not touch the media.
        if let Some(mut markers) = self.outs[slot(c)].markers.take() {
            let why = match markers.write() {
                Err(e) => Some(format!("The markers could not be saved: {e}.")),
                // The project only after the manifest is on disk (ADR-0089 decision 10).
                Ok(()) => {
                    let project = markers
                        .write_project()
                        .err()
                        .map(|e| format!("The editor project was not written: {e}."));
                    // Inside the media only after a clean stop (ADR-0089 decision 14).
                    let chapters = if failure.is_none() {
                        markers.write_chapters().err().map(|e| {
                            format!("The markers could not be added to the file as chapters: {e}.")
                        })
                    } else {
                        None
                    };
                    match (project, chapters) {
                        (Some(a), Some(b)) => Some(format!("{a} {b}")),
                        (a, b) => a.or(b),
                    }
                }
            };
            if let Some(why) = why {
                failure = Some(match failure {
                    Some(f) => format!("{f} {why}"),
                    None => why,
                });
            }
        }
        self.set_record_state(
            c,
            api::RecordState::Idle {
                last: Some(api::RecordingSummary {
                    failure: failure.clone(),
                    frames,
                    dropped,
                }),
            },
        );
        self.outs[slot(c)].recording = None;
        notify(recording_update(
            c,
            RecordingUpdate::Stopped {
                path,
                failure,
                frames,
                dropped,
                flush_lost,
            },
        ));
    }

    /// A recording of `c` to `path` with its saved recording settings, as
    /// `record.start` and "Record when the stream starts" make it; refused in words
    /// when the settings cannot be used.
    pub(super) fn profile_request(
        &self,
        c: OutputCanvas,
        path: PathBuf,
    ) -> Result<RecordRequest, String> {
        let p = self.record_profile(c);
        if let Some(why) = p.problem() {
            return Err(why);
        }
        // A saved encoder this system does not have falls back to the best one, as the
        // UI's automatic choice does.
        let encoder = p
            .encoder
            .clone()
            .filter(|id| self.encoders.iter().any(|e| e.id == *id));
        // A vertical recording with no section of its own keeps the main encoding
        // and encodes at the vertical canvas: the main size is not copied.
        let size = match c {
            OutputCanvas::Main => p.output_size.map(|s| s.pair()),
            OutputCanvas::Vertical => self
                .vertical_profile
                .as_ref()
                .and_then(|v| v.output_size)
                .map(|s| s.pair()),
        };
        let start = p.recording_start(encoder, size);
        Ok(RecordRequest {
            path,
            container: start.container,
            encoder: start.encoder,
            video: start.video,
            audio_bitrate_bps: start.audio_bitrate_bps,
            bit_depth: start.bit_depth,
            size: start.size,
            iso: self.separate_files,
            editor_project: p.editor_project,
            markers: p.markers,
        })
    }

    /// Asks the running recording to stop: its video ends now and the loop stops it
    /// once the frames still read back are in (ADR-0045).
    pub(super) fn end_recording(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        if let Some((_, id)) = &self.outs[slot(c)].recording
            && self.studio.end_output(*id)
        {
            self.outs[slot(c)].ending = true;
        } else {
            self.stop_recording(c, None, notify);
        }
    }

    /// A stream just started: with "Record when the stream starts" on and no
    /// recording running, records the program with the recording settings, as Record
    /// would. The UI is told either way; a recording that cannot start leaves the
    /// stream running.
    pub(super) fn record_with_stream(&mut self, c: OutputCanvas, notify: &dyn Fn(Update)) {
        let on = self.stream_profile_of(c).is_some_and(|p| p.auto_record);
        let o = &self.outs[slot(c)];
        if !on || o.recording.is_some() || o.streaming.is_none() {
            return;
        }
        let started = if !self.record_profile(c).format.is_audio_only()
            && let Some(why) = &self.cannot_record
        {
            Err(why.clone())
        } else {
            self.profile_request(c, self.recording_file(c))
                .and_then(|r| {
                    self.start_recording(c, &r, Some(API_PROFILE_ID.into()), StartGate::Older)
                        .map_err(StartError::into_message)
                })
        };
        let update = match started {
            Ok(update) => {
                self.outs[slot(c)].auto_recording = true;
                update
            }
            Err(why) => RecordingUpdate::NotStarted(why),
        };
        notify(recording_update(c, update));
    }

    /// Changes which of `id`'s feeds record separately from the next recording on
    /// (ADR-0086 decision 2, ADR-0117 decision 1; protocol 1.46 and 1.52). One undo
    /// step; `Ok(false)` when nothing changed.
    pub(super) fn set_record_separately(
        &mut self,
        id: SourceId,
        patch: RecordPatch,
    ) -> Result<bool, Refusal> {
        let source = self
            .collection
            .source(id)
            .ok_or_else(|| Refusal::NotFound(format!("There is no source {}.", id.get())))?;
        let class = types::class_of(&source.type_id);
        let picture_only = matches!(
            class,
            Some(
                types::SourceClass::Capture(_)
                    | types::SourceClass::Still
                    | types::SourceClass::Browser
                    | types::SourceClass::Text
            )
        );
        let sound_only = matches!(class, Some(types::SourceClass::Audio(_)));
        let mut flag = source.record_separately;
        // `enabled` is the main feed: the picture of a picture source, the sound of
        // anything else (a media file records its sound, as in 1.46). Naming that feed
        // twice in one call is refused, so no order between them is needed.
        let aliased = if picture_only {
            patch.video
        } else {
            patch.audio
        };
        if patch.enabled.is_some() && aliased.is_some() {
            return Err(Refusal::Invalid(
                "`enabled` and the feed it stands for cannot both be given.".into(),
            ));
        }
        if let Some(on) = patch.enabled {
            if picture_only {
                flag.video = on;
            } else {
                flag.audio = on;
            }
        }
        if let Some(on) = patch.video {
            if on && sound_only {
                return Err(Refusal::Invalid(format!(
                    "{} has no picture to record separately.",
                    source.name
                )));
            }
            flag.video = on;
        }
        if let Some(on) = patch.audio {
            if on && picture_only {
                return Err(Refusal::Invalid(format!(
                    "{} has no sound of its own to record separately.",
                    source.name
                )));
            }
            flag.audio = on;
        }
        if let Some(form) = patch.video_as {
            if form == VideoAs::Image && !rbs_sources::is_still(&source.type_id, &source.settings) {
                return Err(Refusal::Invalid(format!(
                    "{} is not a still picture, so it cannot be recorded as images.",
                    source.name
                )));
            }
            flag.video_as = form;
        }
        // The form is kept only while the picture is marked (scene model): a form for
        // an unmarked picture would not be saved, so it is refused rather than lost.
        if !flag.video {
            if patch.video_as.is_some_and(|form| form != VideoAs::Auto) {
                return Err(Refusal::Invalid(format!(
                    "{}'s picture is not marked, so it has no form to set.",
                    source.name
                )));
            }
            flag.video_as = VideoAs::Auto;
        }
        if source.record_separately == flag {
            return Ok(false);
        }
        let name = source.name.clone();
        let undo = self
            .collection
            .set_source_record_separately(id, flag)
            .map_err(|e| Refusal::of(&e))?;
        // A mark the start plan would refuse (more files than it takes) is refused
        // here, so a recording is never refused because of marks set through the
        // app or the API.
        if let Err(e) = rbs_record::plan(&self.collection) {
            let _ = self.collection.undo(undo);
            return Err(Refusal::Invalid(format!("{e}.")));
        }
        self.history
            .record(EditKind::RecordSeparately, &name, 0, vec![undo]);
        Ok(true)
    }
}

/// Makes a separate-files recording's folder and its parents. `create_dir`, not
/// `create_dir_all`, for the folder itself: it fails when the folder exists, so a
/// folder RBS did not make is never written into or cleaned up.
fn make_recording_folder(folder: &Path) -> Result<(), String> {
    if let Some(parent) = folder.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot make the folder {}: {e}", parent.display()))?;
    }
    std::fs::create_dir(folder).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => format!("{} already exists.", folder.display()),
        _ => format!("Cannot make the folder {}: {e}", folder.display()),
    })
}

/// Every name a separate-files start may write, for the cleanup of a refused start.
fn planned_names(
    iso_plan: Option<&rbs_record::IsoPlan>,
    container: ContainerId,
    master_name: &str,
) -> Vec<String> {
    let mut planned: Vec<String> = iso_plan
        .map(|p| {
            p.tracks
                .iter()
                .map(|t| match t.role {
                    rbs_record::IsoRole::Audio => {
                        format!("{}.{}", t.file_name, container_ext(ContainerId::Wav))
                    }
                    rbs_record::IsoRole::Image => {
                        rbs_record::PngWriterNode::file_name(&t.file_name, 1)
                    }
                    _ => format!("{}.{}", t.file_name, container_ext(container)),
                })
                .collect()
        })
        .unwrap_or_default();
    planned.push(master_name.to_owned());
    planned
}

/// The sound files' start: WAV, 24-bit PCM, an editor's format with no generation
/// loss. A profile section for it comes with the Separate files card (ADR-0086
/// decision 4).
fn iso_audio_start(
    folder: &Path,
    master_name: &str,
    t0_utc: &str,
    t0_ns: u64,
    pause: &rbs_record::SharedPause,
) -> IsoAudioStart {
    let mut start = IsoAudioStart::new(
        rbs_transport::file_url(folder),
        ContainerId::Wav,
        master_name.to_owned(),
        t0_utc.to_owned(),
        t0_ns,
    );
    start.audio.codec = CodecId::Pcm;
    start.audio.bitrate_bps = 0;
    start.audio.bit_depth = Some(24);
    start.skip_unheard = true;
    start.pause = Some(pause.clone());
    start
}

/// The sound files of a manifest as the control API's tracks.
fn audio_tracks(manifest: &rbs_manifest::RecordingManifest) -> Vec<api::RecordTrack> {
    manifest
        .files
        .iter()
        .filter(|f| f.role == "audio")
        .filter_map(|f| {
            f.source.map(|source| api::RecordTrack {
                file: f.file_name.clone(),
                role: api::TrackRole::Audio,
                source: source.get(),
            })
        })
        .collect()
}
