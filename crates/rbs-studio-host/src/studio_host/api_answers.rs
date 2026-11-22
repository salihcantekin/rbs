//! Control API answers outside the main dispatch: canvases, profiles, outputs and the refusing target (ADR-0159 decision 8).

use super::*;

impl ApiTarget<'_> {
    /// `record.start` or `stream.start` with `canvas: "all"` (ADR-0111 decision 3): the
    /// main canvas first, whose refusal fails the call; then the vertical one, whose
    /// refusal is only its entry. One step on the control thread.
    pub(super) fn start_all(
        &mut self,
        kind: api::OutputKind,
        iso: bool,
        gate: StartGate,
    ) -> Result<Response, ApiError> {
        let mut outputs = Vec::new();
        for c in self.host.output_canvases() {
            let out = &self.host.outs[slot(c)];
            let (running, ending) = match kind {
                api::OutputKind::Record => (out.recording.is_some(), out.ending),
                api::OutputKind::Stream => (out.streaming.is_some(), false),
            };
            let entry = |outcome| api::OutputEntry {
                kind: None,
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error: None,
                estimate: None,
                skipped: Vec::new(),
            };
            if ending {
                let e = ApiError::new(ErrorKind::Conflict, "the recording is still ending");
                if c == OutputCanvas::Main {
                    return Err(e);
                }
                outputs.push(api::OutputEntry {
                    kind: None,
                    error: Some(api::OutputError::from(&e)),
                    ..entry(api::OutputOutcome::Refused)
                });
                continue;
            }
            if running {
                outputs.push(entry(api::OutputOutcome::Running));
                continue;
            }
            // `canvas: "all"` starts the canvases where streaming is available
            // (ADR-0148 decision 10).
            if kind == api::OutputKind::Stream && !self.host.stream_available(c) {
                outputs.push(entry(api::OutputOutcome::NotConfigured));
                continue;
            }
            let started = match kind {
                api::OutputKind::Record => {
                    let path = self.host.recording_file(c);
                    self.record(c, path, false, iso, gate)
                }
                api::OutputKind::Stream => self.stream(c),
            };
            match started {
                Ok(Response::RecordStarted(r)) => outputs.push(api::OutputEntry {
                    kind: None,
                    video_encoder: Some(r.video_encoder),
                    audio_encoder: r.audio_encoder,
                    estimate: r.estimate,
                    skipped: r.skipped,
                    ..entry(api::OutputOutcome::Started)
                }),
                Ok(Response::StreamStarted(r)) => outputs.push(api::OutputEntry {
                    kind: None,
                    video_encoder: Some(r.video_encoder),
                    audio_encoder: r.audio_encoder,
                    ..entry(api::OutputOutcome::Started)
                }),
                Ok(_) => outputs.push(entry(api::OutputOutcome::Started)),
                Err(e) if c == OutputCanvas::Main => return Err(e),
                Err(e) => outputs.push(api::OutputEntry {
                    kind: None,
                    error: Some(api::OutputError::from(&e)),
                    ..entry(api::OutputOutcome::Refused)
                }),
            }
        }
        // Refused only when no canvas streams or can (ADR-0148 decision 10).
        if kind == api::OutputKind::Stream
            && outputs
                .iter()
                .all(|o| o.outcome == api::OutputOutcome::NotConfigured)
        {
            return Err(failed(STREAMING_OFF));
        }
        if !outputs
            .iter()
            .any(|o| o.outcome == api::OutputOutcome::Started)
        {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                match kind {
                    api::OutputKind::Record => "a recording is already running",
                    api::OutputKind::Stream => "a stream is already running",
                },
            ));
        }
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// `record.stop` or `stream.stop` with `canvas: "all"`: stops every running output
    /// of that kind; `conflict` only when none ran.
    /// `record.pause` (`Some(true)`), `record.resume` (`Some(false)`) or
    /// `record.pause_toggle` (`None`) on the canvases `p` names (control API 1.61).
    pub(super) fn pause_asked(
        &mut self,
        p: rbs_api::request::OnCanvases,
        pause: Option<bool>,
    ) -> Result<Response, ApiError> {
        let Some(one) = p.target().single() else {
            return self.pause_all(pause);
        };
        let notify = self.notify;
        let host = &mut *self.host;
        let c = host.api_canvas_of(Some(one))?;
        host.pause_recording(c, pause)?;
        notify(host.pause_update());
        host.publish(notify);
        Ok(Response::Done(Empty {}))
    }

    /// `stream.hold` (`Some(true)`), `stream.resume` (`Some(false)`) or
    /// `stream.hold_toggle` (`None`) on the canvases `target` names (control API
    /// 1.64). With `all`: every live stream, main first; a toggle holds them all when
    /// any is not held, else resumes them all.
    pub(super) fn hold_asked(
        &mut self,
        target: api::OutputTarget,
        hold: Option<bool>,
        asked: rbs_api::request::StreamHold,
    ) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        if let Some(one) = target.single() {
            let c = host.api_canvas_of(Some(one))?;
            host.hold_stream(c, hold, asked)?;
            notify(host.pause_update());
            notify(host.hold_update());
            host.publish(notify);
            return Ok(Response::Done(Empty {}));
        }
        let canvases = host.output_canvases();
        let live = |host: &Host, c: OutputCanvas| host.outs[slot(c)].streaming.is_some();
        let hold = hold.unwrap_or_else(|| {
            canvases
                .iter()
                .any(|&c| live(host, c) && !host.stream_held(c))
        });
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        for c in canvases {
            let (outcome, error) = if !live(host, c) {
                (api::OutputOutcome::NotRunning, None)
            } else {
                match host.hold_stream(c, Some(hold), asked) {
                    Ok(true) => (api::OutputOutcome::Held, None),
                    Ok(false) => (api::OutputOutcome::Resumed, None),
                    Err(e) => {
                        let error = api::OutputError::from(&e);
                        refusals.push(e);
                        (api::OutputOutcome::Refused, Some(error))
                    }
                }
            };
            outputs.push(api::OutputEntry {
                canvas: c,
                kind: None,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error,
                estimate: None,
                skipped: Vec::new(),
            });
        }
        changed_or_first_refusal(&outputs, refusals, "nothing streams")?;
        notify(host.pause_update());
        notify(host.hold_update());
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// `output.break_toggle` (control API 1.64, ADR-0124 decision 15): when every
    /// live stream is on hold and every running recording is paused, resumes them
    /// all (streams first, so a recording a hold paused resumes with its stream);
    /// otherwise holds the streams that are not held (with the picture their "While on
    /// hold" setting names) and pauses the recordings that are not paused. The answer lists the outputs it changed or
    /// that refused, main first, each with its `kind`.
    pub(super) fn break_toggle(&mut self) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        let canvases = host.output_canvases();
        let streams: Vec<_> = canvases
            .iter()
            .copied()
            .filter(|&c| host.outs[slot(c)].streaming.is_some())
            .collect();
        let recording = |host: &Host, c: OutputCanvas| {
            let o = &host.outs[slot(c)];
            o.recording.as_ref().filter(|_| !o.ending).map(|r| r.1)
        };
        let paused = |host: &Host, c: OutputCanvas| {
            recording(host, c)
                .and_then(|id| host.studio.output_pause(id))
                .is_some_and(|p| p.paused)
        };
        let records: Vec<_> = canvases
            .iter()
            .copied()
            .filter(|&c| recording(host, c).is_some())
            .collect();
        if streams.is_empty() && records.is_empty() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "nothing records or streams",
            ));
        }
        let resume = streams.iter().all(|&c| host.stream_held(c))
            && records.iter().all(|&c| paused(host, c));
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        let mut entry = |c, kind, done: Result<api::OutputOutcome, ApiError>| {
            let (outcome, error) = match done {
                Ok(o) => (o, None),
                Err(e) => {
                    let error = api::OutputError::from(&e);
                    refusals.push(e);
                    (api::OutputOutcome::Refused, Some(error))
                }
            };
            outputs.push(api::OutputEntry {
                canvas: c,
                kind: Some(kind),
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error,
                estimate: None,
                skipped: Vec::new(),
            });
        };
        let before: Vec<bool> = records.iter().map(|&c| paused(host, c)).collect();
        for &c in &streams {
            if resume || !host.stream_held(c) {
                let done = host
                    .hold_stream(c, Some(!resume), rbs_api::request::StreamHold::default())
                    .map(|held| {
                        if held {
                            api::OutputOutcome::Held
                        } else {
                            api::OutputOutcome::Resumed
                        }
                    });
                entry(c, api::OutputKind::Stream, done);
            }
        }
        // After the streams: a hold may have paused, or its end resumed, a recording
        // sharing its encoder (decision 14); that recording is listed as changed.
        for (&c, was) in records.iter().zip(before) {
            let now = paused(host, c);
            if was != now {
                let outcome = if now {
                    api::OutputOutcome::Paused
                } else {
                    api::OutputOutcome::Resumed
                };
                entry(c, api::OutputKind::Record, Ok(outcome));
            } else if now == resume {
                let done = host.pause_recording(c, Some(!resume)).map(|p| {
                    if p {
                        api::OutputOutcome::Paused
                    } else {
                        api::OutputOutcome::Resumed
                    }
                });
                entry(c, api::OutputKind::Record, done);
            }
        }
        changed_or_first_refusal(&outputs, refusals, "nothing changed")?;
        notify(host.pause_update());
        notify(host.hold_update());
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// `record.pause`, `record.resume` or `record.pause_toggle` with `canvas: "all"`
    /// (control API 1.61): every running recording, main first. A toggle pauses them
    /// all when any runs unpaused, else resumes them all. `conflict` when none
    /// changed: the one refusal when there is one, else "nothing records".
    pub(super) fn pause_all(&mut self, pause: Option<bool>) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        let canvases = host.output_canvases();
        let running = |host: &Host, c: OutputCanvas| {
            let o = &host.outs[slot(c)];
            o.recording.as_ref().filter(|_| !o.ending).map(|r| r.1)
        };
        let pause = pause.unwrap_or_else(|| {
            canvases.iter().any(|&c| {
                running(host, c)
                    .is_some_and(|id| !host.studio.output_pause(id).is_some_and(|p| p.paused))
            })
        });
        let mut outputs = Vec::new();
        let mut refusals = Vec::new();
        for c in canvases {
            let (outcome, error) = if running(host, c).is_none() {
                (api::OutputOutcome::NotRunning, None)
            } else {
                match host.pause_recording(c, Some(pause)) {
                    Ok(true) => (api::OutputOutcome::Paused, None),
                    Ok(false) => (api::OutputOutcome::Resumed, None),
                    Err(e) => {
                        let error = api::OutputError::from(&e);
                        refusals.push(e);
                        (api::OutputOutcome::Refused, Some(error))
                    }
                }
            };
            outputs.push(api::OutputEntry {
                kind: None,
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error,
                estimate: None,
                skipped: Vec::new(),
            });
        }
        if !outputs.iter().any(|o| {
            matches!(
                o.outcome,
                api::OutputOutcome::Paused | api::OutputOutcome::Resumed
            )
        }) {
            return Err(refusals
                .into_iter()
                .next()
                .unwrap_or_else(|| ApiError::new(ErrorKind::Conflict, "nothing records")));
        }
        notify(host.pause_update());
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    pub(super) fn stop_all(&mut self, kind: api::OutputKind) -> Result<Response, ApiError> {
        let notify = self.notify;
        let host = &mut *self.host;
        let mut outputs = Vec::new();
        for c in host.output_canvases() {
            let out = &host.outs[slot(c)];
            let running = match kind {
                api::OutputKind::Record => out.recording.is_some() && !out.ending,
                api::OutputKind::Stream => out.streaming.is_some(),
            };
            let outcome = if running {
                match kind {
                    api::OutputKind::Record => host.end_recording(c, notify),
                    api::OutputKind::Stream => host.end_stream_asked(c, notify),
                }
                api::OutputOutcome::Stopped
            } else {
                api::OutputOutcome::NotRunning
            };
            outputs.push(api::OutputEntry {
                kind: None,
                canvas: c,
                outcome,
                video_encoder: None,
                audio_encoder: None,
                error: None,
                estimate: None,
                skipped: Vec::new(),
            });
        }
        if !outputs
            .iter()
            .any(|o| o.outcome == api::OutputOutcome::Stopped)
        {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                match kind {
                    api::OutputKind::Record => "nothing records",
                    api::OutputKind::Stream => "nothing streams",
                },
            ));
        }
        host.publish(notify);
        Ok(Response::Outputs(api::OutputsResult { outputs }))
    }

    /// Goes live on canvas `c` with its saved stream settings and the key in the key
    /// store.
    pub(super) fn stream(&mut self, c: OutputCanvas) -> Result<Response, ApiError> {
        self.stream_to(c, None)
    }

    /// The running canvas of destination `id`, or `not_found`.
    pub(super) fn destination_of(&self, id: u32) -> Result<OutputCanvas, ApiError> {
        let c = self.host.destination_canvas(id).ok_or_else(|| {
            ApiError::new(ErrorKind::NotFound, format!("there is no destination {id}"))
        })?;
        self.host.api_canvas_of(Some(c))
    }

    /// `stream.destination.start` (protocol 1.77, ADR-0127 decision 16).
    pub(super) fn destination_start(&mut self, id: u32) -> Result<Response, ApiError> {
        let c = self.destination_of(id)?;
        let notify = self.notify;
        let conflict = |why: &str| Err(ApiError::new(ErrorKind::Conflict, why));
        let Some(d) = self
            .host
            .resolved_destinations(c)
            .into_iter()
            .find(|d| d.destination.id == id)
        else {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                format!("there is no destination {id}"),
            ));
        };
        // Turning one on stays a separate, visible step.
        if !d.destination.enabled {
            return conflict("the destination is turned off: turn it on first");
        }
        if d.parsed().is_none() {
            return conflict("the destination is not set up: enter its server in Settings, Stream");
        }
        if self.host.outs[slot(c)].streaming.is_none() {
            return self.stream_to(c, Some(id));
        }
        if self.host.outs[slot(c)]
            .destinations
            .iter()
            .any(|d| d.id == id && d.output.is_some())
        {
            return conflict("the destination is already on air");
        }
        let host = &mut *self.host;
        let (request, hidden) = host.api_stream_request(c, Some(id))?;
        let result = host.start_destination(c, &request, id);
        host.report_link(c, notify);
        host.publish(notify);
        result.map_err(|why| {
            failed(hidden.iter().fold(why.into_text(), |w, url| {
                w.replace(url.as_str(), "the saved server")
            }))
        })?;
        Ok(Response::Done(Empty {}))
    }

    /// `stream.destination.stop` (protocol 1.77): the last running destination ends
    /// the stream, as `stream.stop` does.
    pub(super) fn destination_stop(&mut self, id: u32) -> Result<Response, ApiError> {
        let c = self.destination_of(id)?;
        let notify = self.notify;
        let host = &mut *self.host;
        let o = &host.outs[slot(c)];
        let running = o.destinations.iter().filter(|d| d.output.is_some()).count();
        let this = o
            .destinations
            .iter()
            .any(|d| d.id == id && d.output.is_some());
        if o.streaming.is_none() || !this {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "the destination is not on air",
            ));
        }
        if running == 1 {
            host.end_stream_asked(c, notify);
        } else {
            host.stop_destination(c, id);
            host.report_link(c, notify);
        }
        host.publish(notify);
        Ok(Response::Done(Empty {}))
    }

    /// `stream.start` of `c`, or `stream.destination.start` of `only` while `c` does
    /// not stream: that canvas's stream with this destination alone (protocol 1.77).
    pub(super) fn stream_to(
        &mut self,
        c: OutputCanvas,
        only: Option<u32>,
    ) -> Result<Response, ApiError> {
        let notify = self.notify;
        // The window's Go live is told as its own start was: the texts name the
        // server (the window is the local user's), and a missing key is `NoKey`.
        let window = self.window && only.is_none();
        let tell = |u: StreamUpdate| {
            if window {
                notify(stream_update(c, u));
            }
        };
        let host = &mut *self.host;
        host.note_encoders(notify);
        if let Some(why) = &host.recording.cannot_record {
            tell(StreamUpdate::NotStarted(why.clone()));
            return Err(failed_because("recording_blocked", why.clone()));
        }
        if host.outs[slot(c)].streaming.is_some() {
            let why = "a stream is already running";
            tell(StreamUpdate::NotStarted(why.into()));
            return Err(ApiError::new(ErrorKind::Conflict, why));
        }
        let (request, hidden) = host.api_stream_request(c, only).inspect_err(|e| {
            tell(StreamUpdate::NotStarted(e.message.clone()));
        })?;
        let hide = |why: String| {
            failed(
                hidden
                    .iter()
                    .fold(why, |w, url| w.replace(url.as_str(), "the saved server")),
            )
        };
        let result = host.start_stream_to(c, &request, only);
        // The texts name the stream, or the destination, whose key is missing.
        let which = match (only, c) {
            (Some(id), _) if id != MAIN_FIRST && id != VERTICAL_FIRST => " for this destination",
            (_, OutputCanvas::Main) => "",
            (_, OutputCanvas::Vertical) => " for the vertical stream",
        };
        host.publish(notify);
        match result {
            Ok(StreamUpdate::Started {
                url,
                video_encoder,
                audio_encoder,
            }) => {
                let started = StreamStarted {
                    video_encoder: video_encoder.clone(),
                    audio_encoder: audio_encoder.clone(),
                };
                // The UI shows the stream as if its own button started it.
                notify(stream_update(
                    c,
                    StreamUpdate::Started {
                        url,
                        video_encoder,
                        audio_encoder,
                    },
                ));
                if window {
                    // Each destination that did not start, by name (ADR-0127
                    // decision 7), as the window's own start said.
                    for d in &host.outs[slot(c)].destinations {
                        if let (DestinationState::Failed, Some(why)) = (d.state, &d.error) {
                            notify(Update::Error(format!("{}: {why}", d.label())));
                        }
                    }
                }
                // The stream runs: a panic while its recording starts must not turn
                // the answer into an error (the round recovers the host).
                if rbs_crash::contain(|| host.record_with_stream(c, notify)).is_err() {
                    host.caught = true;
                }
                host.publish(notify);
                Ok(Response::StreamStarted(started))
            }
            Ok(StreamUpdate::NoKey) => {
                tell(StreamUpdate::NoKey);
                Err(failed(format!(
                    "no stream key is saved{which}: enter it in Settings, Stream"
                )))
            }
            Ok(_) => Err(failed("the stream did not start")),
            Err(why) => {
                let text = why.into_text();
                tell(StreamUpdate::NotStarted(text.clone()));
                Err(hide(text))
            }
        }
    }
}

/// `invalid_params` when an explicit size is not the canvas-aspect fit (ADR-0102
/// decisions 4 and 7). Absent stays the canvas. Even sides and the 2..=7680 range
/// are already refused by the profile's own check.
pub(super) fn output_size_for_canvas(
    size: Option<crate::record_profile::OutputSize>,
    canvas: (u32, u32),
) -> Result<(), ApiError> {
    let Some(size) = size else {
        return Ok(());
    };
    if output_size_matches(size.pair(), canvas) {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorKind::InvalidParams,
            format!(
                "Output size {} x {} does not keep this canvas's shape.",
                size.width, size.height
            ),
        ))
    }
}

pub(super) fn check_profile(id: &str) -> Result<(), ApiError> {
    if id == API_PROFILE_ID {
        Ok(())
    } else {
        Err(ApiError::new(
            ErrorKind::NotFound,
            format!("no recording profile {id:?}"),
        ))
    }
}

pub(super) fn canvas_profile(c: &api::Canvas) -> Result<CanvasProfile, ApiError> {
    if c.fps_den != 1 {
        return Err(ApiError::new(
            ErrorKind::InvalidParams,
            "the canvas rate must be a whole number of frames per second",
        ));
    }
    let profile = CanvasProfile {
        width: c.width,
        height: c.height,
        fps: c.fps_num,
        ..CanvasProfile::new()
    };
    match profile.problem() {
        Some(why) => Err(ApiError::new(ErrorKind::InvalidParams, why)),
        None => Ok(profile),
    }
}

/// Answers every control API request with why the studio cannot run.
pub(super) struct Refuse(pub(super) String);

impl ControlTarget for Refuse {
    fn handle(
        &mut self,
        _command: rbs_api::Command,
        _events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        Err(failed(format!("the studio stopped: {}", self.0)))
    }
}
