//! The control thread's loop: one message, one API request and every driver per round (ADR-0041 decision 7, ADR-0159 decision 8).

use super::*;

/// What the control thread is handed besides the studio configuration.
pub(super) struct ControlIo {
    pub(super) rx: Receiver<Msg>,
    pub(super) inbox: std::sync::Weak<ControlOut>,
    pub(super) levels: Arc<crate::levels::LevelFeed>,
}

pub(super) fn run(
    config: HostConfig,
    adapters: impl FnOnce() -> (RecordAdapters, Vec<String>),
    gpu: impl FnOnce() -> Result<Arc<GpuContext>, String>,
    engine: impl Fn() -> Engine + 'static,
    io: ControlIo,
    notify: &dyn Fn(Update),
) {
    let ControlIo { rx, inbox, levels } = io;
    // A control thread, not a real-time one: its panics get crash reports (ADR-0055).
    rbs_crash::report_panics_on_this_thread();
    let started = rbs_crash::contain(|| start(config, adapters, gpu, engine, levels))
        .unwrap_or_else(|_| Err(rbs_crash::CAUGHT_TEXT.to_owned()));
    let mut host = match started {
        Ok((mut host, update)) => {
            notify(update);
            host.inbox = inbox;
            host.studio.bind_control_thread();
            if let Some(out) = host.inbox.upgrade() {
                out.bind(host.studio.control_wake());
            }
            host
        }
        Err(e) => {
            rbs_core::error!(
                rbs_core::log::Area::Studio,
                "the studio could not start: {e}"
            );
            notify(Update::Failed(e.clone()));
            // Keep answering until the UI goes away, so it never waits on a dead thread;
            // control API requests get the reason.
            let mut api: Option<Api> = None;
            while let Ok(m) = rx.recv() {
                match m {
                    Msg::AttachApi(a) => api = Some(*a),
                    Msg::DetachApi => api = None,
                    Msg::Ui(_)
                    | Msg::ApiWake
                    | Msg::PickFailed(_)
                    | Msg::Picked { .. }
                    | Msg::ReplayWritten(_)
                    | Msg::Thumbnail { .. }
                    | Msg::RegionPreview { .. }
                    | Msg::Calibration { .. }
                    | Msg::PermissionAnswered(_)
                    | Msg::StingerLook { .. }
                    | Msg::TextMeasured { .. }
                    | Msg::AddOn { .. } => {}
                }
                if let Some(api) = &api {
                    let mut events = EventPublisher::new();
                    while api.serve_one(&mut Refuse(e.clone()), &mut events, notify) {}
                }
            }
            return;
        }
    };
    host.publish(notify);
    let mut failed: Option<String> = None;
    // The first round runs before any park, so a bypass during start is collected.
    let mut wait = Some(Duration::ZERO);
    let mut caught_in_row = 0;
    loop {
        let msg = match rx.try_recv() {
            Ok(m) => Some(m),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => break,
        };
        if msg.is_none() {
            match wait {
                Some(d) if d.is_zero() => {}
                due => {
                    if host.studio.bypass_pending() {
                        wait = Some(Duration::ZERO);
                        continue;
                    }
                    // The load above is the last read before this park. Nothing in
                    // between may park, or it would consume the bypass token.
                    match due {
                        Some(d) => std::thread::park_timeout(d),
                        None => std::thread::park(),
                    }
                    wait = Some(Duration::ZERO);
                    continue;
                }
            }
        }
        // Each round is a panic boundary (ADR-0055 decision 3): a bug in one command or
        // one round fails that round, and the studio and its outputs keep running.
        let kind = failures::Kind::of(msg.as_ref());
        let logged = |u: Update| {
            failures::log_update(kind, &u);
            notify(u);
        };
        match rbs_crash::contain(|| round(&mut host, &mut failed, msg, &logged)) {
            Ok(next) => {
                caught_in_row = 0;
                wait = next;
                // An API request panicked: its client got an `internal` error.
                if std::mem::take(&mut host.caught) {
                    let _ = rbs_crash::contain(|| host.recover(&mut failed, 1, notify));
                }
            }
            Err(_) => {
                caught_in_row += 1;
                rbs_core::error!(rbs_core::log::Area::Studio, "a studio round panicked");
                wait = Some(MAX_WAIT);
                notify(Update::Error(rbs_crash::CAUGHT_TEXT.into()));
                let _ = rbs_crash::contain(|| host.recover(&mut failed, caught_in_row, notify));
            }
        }
    }
    // A calibration run stops; the app does not wait for its worker.
    host.calibration_run.stop();
    // Closing while recording: keep the tails too, as Stop does.
    let ending: Vec<OutputId> = host
        .outs
        .iter()
        .filter_map(|o| o.recording.as_ref().map(|(_, id)| *id))
        .filter(|id| host.studio.end_output(*id))
        .collect();
    let started = std::time::Instant::now();
    while !ending.iter().all(|id| host.studio.output_tail_done(*id))
        && started.elapsed() < CLOSE_TAIL_GUARD
    {
        host.studio.service();
        // The control thread at shutdown, not the UI thread (ADR-0055 decision 6).
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(ENDING_WAIT);
    }
    for c in OutputCanvas::ALL {
        host.end_stream(c, None, notify);
        host.stop_recording(c, None, notify);
    }
    host.save_text_timers();
    let _ = host.studio.shutdown();
}

/// Why a text's box cannot change during a take that records its picture separately
/// (ADR-0129 decision 5).
pub(super) const STOP_TO_CHANGE_BOX: &str =
    "Stop recording to change the box: the separate picture track keeps one size.";

/// How often the control loop looks at a program switch held back for its texts.
pub(super) const HELD_SWITCH_POLL: Duration = Duration::from_millis(4);

/// Caught panics in a row after which the studio counts as failed: its recording and
/// stream end cleanly instead of running on with a host that keeps panicking.
pub(super) const CAUGHT_LIMIT: u32 = 3;

/// One round of the control loop: the message that woke it (`None`: the wait ended),
/// one API request, then the studio's service. Returns how long to wait for the next.
pub(super) fn round(
    host: &mut Host,
    failed: &mut Option<String>,
    msg: Option<Msg>,
    notify: &dyn Fn(Update),
) -> Option<Duration> {
    take_msg(host, failed, msg, notify);
    // One API request per round: a UI command that arrived meanwhile is taken by the
    // next `recv_timeout` first, so the local user always goes first (decision 5).
    let served_api = match &*failed {
        None => host.serve_api(notify),
        Some(why) => host
            .api
            .as_ref()
            .is_some_and(|api| api.serve_one(&mut Refuse(why.clone()), &mut host.events, notify)),
    };
    if failed.is_none() {
        host.sync_meters();
    }
    if failed.is_none()
        && let Some(why) = host.dead.take()
    {
        // Failed first: a panic while the outputs end must not undo it.
        *failed = Some(why.clone());
        host.end_outputs(&why, notify);
        notify(Update::Failed(why));
    }
    if failed.is_some() {
        // A run still meets its deadline while the studio is failed.
        let calibration = host.drive_calibration();
        return if served_api {
            Some(Duration::ZERO)
        } else {
            Some(calibration.unwrap_or(Duration::from_secs(3600)))
        };
    }
    host.feed_text_live();
    host.studio.service();
    let busy = host.waiting || host.program.held_switch.is_some();
    let cap = round_cap(&host.outs, busy);
    // An output or a switch is checked every round and has no deadline of its own.
    // The engine's wait is taken again after the last settle, not from this service.
    let poll = busy
        || host
            .outs
            .iter()
            .any(|o| o.recording.is_some() || o.streaming.is_some() || o.ending);
    let mut armed = poll;
    let mut wait = if poll { cap } else { Duration::MAX };
    if let Some(left) = host.studio.control_extra() {
        arm(&mut wait, &mut armed, left);
    }
    if served_api {
        // More may wait; look again right after any UI command.
        wait = Duration::ZERO;
        armed = true;
    }
    if let Some(why) = host.studio.failure() {
        // Clients see the recording end with the failure instead of a stale state.
        // Failed first: a panic while the outputs end must not undo it.
        *failed = Some(why.clone());
        host.end_outputs(&why, notify);
        notify(Update::Failed(why));
        return finish_wait(wait, armed);
    }
    // A replacement that waited for its predecessor opens now.
    let waiting = host.studio.waiting();
    if host.waiting && !waiting {
        host.apply(notify);
    }
    host.waiting = waiting;
    if let Some(left) = host.drive_crisp(notify) {
        arm(&mut wait, &mut armed, left);
    }
    if host.texts.fonts_wanted {
        host.list_text_fonts(notify);
        if host.texts.fonts_wanted {
            arm(&mut wait, &mut armed, text_resize::FONTS_POLL);
        }
    }
    if host.program.held_switch.is_some() {
        host.drive_held_switch(notify);
        if host.program.held_switch.is_some() {
            // Look again within a render tick.
            arm(&mut wait, &mut armed, HELD_SWITCH_POLL);
        }
    }
    // The latest T-bar value (ADR-0135 decision 1), once per round.
    if let Some(t) = host.program.pending_progress.take() {
        host.drive_progress(t, notify);
    }
    // A transition ended (ADR-0128 decision 8): one update without the outgoing scene
    // removes the incoming compositor and unlinks what only the outgoing scene used.
    if host.studio.take_transition_done() {
        host.studio.settle_transitions(&host.adapters.sources);
    }
    host.publish_transitions();
    // A rehearsal's time is over (ADR-0135 decision 12): the Preview picture shows the
    // preview scene again.
    if host.studio.rehearsal_done()
        && let Err(e) = host.studio.end_rehearsal(&host.adapters.sources)
    {
        notify(Update::Error(format!("The rehearsal could not end: {e}")));
    }
    // A held T-bar that ended other than by going to 0 (its take, a transition, a
    // switch or a preview change): the rail's T-bar goes back to 0.
    let manual = host.studio.manual_transition(host.canvas);
    if host.program.tbar_manual && !manual && !host.program.tbar_zeroed {
        notify(Update::TBarReset);
    }
    host.program.tbar_manual = manual;
    host.program.tbar_zeroed = false;
    // A hold scene, or its sound, fell back (ADR-0124 phase 2b): its parts leave and
    // the stream states say so.
    if host.studio.take_hold_fallback() {
        host.studio.settle_hold_scenes(&host.adapters.sources);
        for c in OutputCanvas::ALL {
            host.refresh_stream_hold(c);
        }
        notify(host.hold_update());
        host.publish(notify);
    }
    drive_outputs(host, notify, &mut wait, &mut armed);
    let now = host.clock.now();
    drive_timed(host, now, notify, &mut wait, &mut armed);
    host.publish(notify);
    finish_wait(wait, armed)
}

/// The message this round took, if any (`round`).
pub(super) fn take_msg(
    host: &mut Host,
    failed: &Option<String>,
    msg: Option<Msg>,
    notify: &dyn Fn(Update),
) {
    match msg {
        Some(Msg::Ui(c)) => {
            if failed.is_none() {
                host.unanswered = answer_on_panic(&c);
                host.command(c, notify);
                host.unanswered = None;
            }
        }
        Some(Msg::AttachApi(api)) => {
            host.events.attach(api.endpoint.event_link());
            host.api = Some(*api);
        }
        Some(Msg::DetachApi) => {
            host.events.detach();
            host.api = None;
        }
        Some(Msg::PickFailed(why)) => notify(Update::Error(why)),
        Some(Msg::Picked { kind, target }) => host.add_picked(kind, &target, notify),
        Some(Msg::PermissionAnswered(kind)) => host.permission_answered(kind, notify),
        Some(Msg::ReplayWritten(result)) => host.replay_written(result, notify),
        Some(Msg::Thumbnail {
            generation,
            adapter,
            target,
            picture,
        }) => {
            // A picture of a list that was closed or replaced meanwhile is dropped.
            if generation == host.pickers.thumbs.load(AtomicOrdering::Acquire) {
                notify(Update::Thumbnail {
                    adapter,
                    target,
                    picture,
                });
            }
        }
        Some(Msg::RegionPreview {
            generation,
            id,
            picture,
        }) => {
            if generation == host.pickers.region_grabs.load(AtomicOrdering::Acquire) {
                notify(Update::RegionPreview {
                    id,
                    generation,
                    picture,
                });
            }
        }
        Some(Msg::Calibration { id, msg }) => host.calibration_msg(id, msg),
        Some(Msg::AddOn { index, msg }) => host.addon_msg(index, msg, notify),
        Some(Msg::StingerLook { key, result }) => {
            if host.stinger_looked(key, result) {
                host.publish(notify);
            }
        }
        Some(Msg::TextMeasured { id, text, size }) => {
            if text_measure::apply(host, id, &text, size) {
                host.publish(notify);
            }
        }
        Some(Msg::ApiWake) | None => {}
    }
}

/// Each canvas's recording and stream this round: a failed or finished recording
/// stops, a stream's destinations and link are checked (`round`).
pub(super) fn drive_outputs(
    host: &mut Host,
    notify: &dyn Fn(Update),
    wait: &mut Duration,
    armed: &mut bool,
) {
    for c in OutputCanvas::ALL {
        let o = &host.outs[slot(c)];
        if let Some((_, id)) = &o.recording
            && let Some(why) = host.studio.output_failure(*id)
        {
            host.stop_recording(c, Some(why), notify);
        }
        let o = &host.outs[slot(c)];
        if o.ending
            && let Some((_, id)) = &o.recording
        {
            if host.studio.output_tail_done(*id) {
                host.stop_recording(c, None, notify);
            } else {
                // The tail takes a few frames; look again soon.
                arm(wait, armed, ENDING_WAIT);
            }
        }
        if host.outs[slot(c)].streaming.is_some() {
            if let Some(why) = host.check_destinations(c, notify) {
                host.end_stream(c, Some(why), notify);
            } else {
                host.report_link(c, notify);
                if let Some(left) = host.drive_hold(c) {
                    arm(wait, armed, left);
                }
            }
        }
    }
}

/// The drivers with deadlines of their own, in `round`'s order: stats, protection,
/// camera, monitor, calibration, markers, media, replay, cues, lingers, transitions and the
/// engine's collection.
pub(super) fn drive_timed(
    host: &mut Host,
    now: rbs_core::time::Timestamp,
    notify: &dyn Fn(Update),
    wait: &mut Duration,
    armed: &mut bool,
) {
    if let Some(left) = host.report_stats(now, notify) {
        arm(wait, armed, left);
    }
    host.drive_log_summary(now, wait, armed);
    if let Some(left) = host.drive_protection(now, notify) {
        arm(wait, armed, left);
    }
    if let Some(left) = host.report_camera(now, notify) {
        arm(wait, armed, left);
    }
    if let Some(left) = host.drive_monitor(now) {
        arm(wait, armed, left);
    }
    if let Some(left) = host.drive_audio_rest(now) {
        arm(wait, armed, left);
    }
    if let Some(left) = host.drive_calibration() {
        arm(wait, armed, left);
    }
    if let Some(left) = host.drive_sync_measure(notify) {
        arm(wait, armed, left);
    }
    if let Some(left) = host.drive_marker_flush(now, notify) {
        arm(wait, armed, left);
    }
    if let Some(left) = host.report_media(notify) {
        arm(wait, armed, left);
    }
    host.report_monitor(notify);
    if let Some(left) = host.report_replay(now, notify) {
        arm(wait, armed, left);
    }
    // One wake for the soonest timed-show deadline. An empty cue map adds nothing (ADR-0105, P13).
    if host.studio.expire_cues(now) {
        host.apply(notify);
    }
    if let Some(left) = host.studio.cue_wait(now) {
        arm(wait, armed, left);
    }
    // A source leaves its linger on the master clock; none lingering arms nothing (ADR-0152).
    if host.studio.linger_due(now) {
        host.apply(notify);
    }
    if let Some(left) = host.studio.linger_wait(now) {
        arm(wait, armed, left);
    }
    // A running transition steps on the master clock; none arms nothing (P13).
    if let Some(left) = host.studio.transition_wait(now) {
        arm(wait, armed, left);
    }
    // A rehearsal ends on the master clock; none arms nothing (P13).
    if let Some(left) = host.studio.rehearsal_wait(now) {
        arm(wait, armed, left);
    }
    // After every settle in this round, including one that timed out and left a swap
    // in flight. A bypass during that settle is in this wait, not in the earlier one.
    if let Some(left) = host.studio.collect_engine() {
        arm(wait, armed, left);
    }
}

/// Keeps `left` as the soonest deadline. `armed` stays false only while no driver and
/// no engine asked for a wake, so the control thread can block on the next message.
pub(super) fn arm(wait: &mut Duration, armed: &mut bool, left: Duration) {
    *wait = (*wait).min(left);
    *armed = true;
}

pub(super) fn finish_wait(wait: Duration, armed: bool) -> Option<Duration> {
    armed.then_some(wait)
}

/// The longest sleep after a round: [`MAX_WAIT`] while an output runs or ends on any
/// canvas, or a replacement or switch waits (`busy`), since those are checked each
/// round without a deadline of their own (failures, tails, links, holds); otherwise
/// [`IDLE_WAIT`].
pub(super) fn round_cap(outs: &[CanvasOutputs], busy: bool) -> Duration {
    let running = outs
        .iter()
        .any(|o| o.recording.is_some() || o.streaming.is_some() || o.ending);
    if running || busy { MAX_WAIT } else { IDLE_WAIT }
}
