//! Several outputs per canvas (spec `recording.md`, "Several outputs", ADR-0047): a
//! recording and a live stream at once, on their own encoders when their settings
//! differ and sharing them when they match; each stops without touching the other.
//! Reference adapters on a `TestClock`, as in `studio.rs`; tests wait for counts.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_codec::RateControl;
use rbs_core::media::{ContainerId, Size};
use rbs_core::time::{Clock, TestClock, Timestamp};
use rbs_record::{
    AUDIO_ENCODER, AUDIO_ENCODER_2, END_TAIL_LIMIT, MAX_OUTPUTS_PER_CANVAS, OUTPUT, OUTPUT_2,
    OutputSettings, RecordError, Studio, StudioSettings, VIDEO_ENCODER, VIDEO_ENCODER_2,
};
use support::*;

/// Workers of the slots: video encoder, audio encoder, output; slot 0 then 1 (the third
/// and fourth slots are in `more_slots`).
fn slots(studio: &Studio) -> [bool; 6] {
    [
        VIDEO_ENCODER,
        AUDIO_ENCODER,
        OUTPUT,
        VIDEO_ENCODER_2,
        AUDIO_ENCODER_2,
        OUTPUT_2,
    ]
    .map(|p| studio.engine().has_worker(p))
}

fn more_slots(studio: &Studio) -> [bool; 6] {
    [
        rbs_record::VIDEO_ENCODER_3,
        rbs_record::AUDIO_ENCODER_3,
        rbs_record::OUTPUT_3,
        rbs_record::VIDEO_ENCODER_4,
        rbs_record::AUDIO_ENCODER_4,
        rbs_record::OUTPUT_4,
    ]
    .map(|p| studio.engine().has_worker(p))
}

fn start(r: &Rig) -> (Studio, TestClock) {
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let studio = Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    (studio, clock)
}

fn recording() -> OutputSettings {
    OutputSettings::new("mem://rec", ContainerId::Matroska)
}

fn stream(bitrate_bps: u64) -> OutputSettings {
    let mut out = OutputSettings::packets("stream://live/app");
    out.video.rate_control = RateControl::Cbr { bitrate_bps };
    out
}

#[test]
fn a_recording_and_a_stream_with_different_settings_run_on_their_own_encoders() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    let one = r.video_sessions.load(Ordering::Relaxed);
    let live = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    let report = studio.output(live).unwrap();
    assert!(!report.video_shared, "other bitrate: an encoder of its own");
    assert!(
        report.audio_shared,
        "same audio settings: the AAC encoder is shared"
    );
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 2 * one);
    assert_eq!(studio.outputs(), [rec, live]);
    assert_eq!(slots(&studio), [true, true, true, true, false, true]);

    // A third output is refused before anything is chosen.
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    assert!(matches!(
        studio.start_output(&r.adapters, &stream(3_000_000)),
        Err(RecordError::TooManyOutputs)
    ));
    assert_eq!(studio.outputs().len(), MAX_OUTPUTS_PER_CANVAS);
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);

    let reached = run_until(&mut studio, &clock, |_| {
        r.log.count(0) >= 8 && r.log.count(1) >= 8 && r.stream_log.count(0) >= 8
    });
    assert!(reached, "{:?}", studio.output_progress(live));

    // The recording ends (its own export takes no later canvas, ADR-0045) and
    // stops: its file is complete, the stream goes on.
    let end = clock.now();
    assert!(studio.end_output(rec));
    assert!(run_until(&mut studio, &clock, |s| s.output_tail_done(rec)));
    assert!(clock.now() < end + END_TAIL_LIMIT, "ended by itself");
    assert!(!studio.output_tail_done(live), "the stream was not ended");
    let report = studio.stop_output(rec).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.failure, None);
    assert_eq!(r.log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(r.log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 0);
    // Its video encoder and output stopped; the AAC encoder stays for the stream.
    assert_eq!(slots(&studio), [false, true, false, true, false, true]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached, "the stream keeps delivering");
    let p = studio.output_progress(live).unwrap();
    assert_eq!((p.encoder_drops, p.mux_drops), (0, 0), "{p:?}");
    // The counters without sources or meter peaks match (nothing ticks in between).
    let c = studio.output_counters(live).unwrap();
    assert!(c.sources.is_empty() && c.master_peak_db.is_none(), "{c:?}");
    let p = studio.output_progress(live).unwrap();
    assert_eq!(
        (
            c.frames_encoded,
            c.packets_taken,
            c.connects,
            c.late_render_ticks
        ),
        (
            p.frames_encoded,
            p.packets_taken,
            p.connects,
            p.late_render_ticks
        )
    );
    assert_eq!(studio.output_counters(rec), None, "stopped");

    let report = studio.stop_output(live).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(slots(&studio), [false; 6]);
    let (reports, errors) = studio.shutdown();
    assert!(reports.is_empty());
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn outputs_with_the_same_settings_share_the_encoders() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let live = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 16);
    assert!(reached);

    // A recording with the stream's settings joins the running encoders.
    let mut out = recording();
    out.video = stream(6_000_000).video;
    let joined = clock.now();
    let rec = studio.start_output(&r.adapters, &out).unwrap();
    let report = studio.output(rec).unwrap();
    assert!(report.video_shared && report.audio_shared, "{report:?}");
    assert_eq!(
        r.video_sessions.load(Ordering::Relaxed),
        sessions,
        "no second session"
    );
    // One encoder chain (slot 0), two outputs.
    assert_eq!(slots(&studio), [true, true, true, false, false, true]);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= 16);
    assert!(reached, "{:?}", studio.output_progress(rec));

    // The joining output starts at a keyframe, at or after the join (less what the
    // encoder held then), and counts only what it took.
    let first = r
        .log
        .packets
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.stream == 0)
        .copied()
        .unwrap();
    assert!(first.keyframe, "{first:?}");
    let held = Timestamp::from_nanos(joined.as_nanos() - 100_000_000);
    assert!(first.pts >= held, "{first:?} joined at {joined:?}");
    let p_rec = studio.output_progress(rec).unwrap();
    let p_live = studio.output_progress(live).unwrap();
    assert!(
        p_rec.frames_encoded < p_live.frames_encoded,
        "{p_rec:?} {p_live:?}"
    );
    assert!(p_rec.frames_encoded > 0);

    // The stream ends first: the shared export keeps running, and the tail is in
    // once the encoder took a frame past the end. The recording keeps its encoders.
    let end = clock.now();
    assert!(studio.end_output(live));
    assert!(run_until(&mut studio, &clock, |s| s.output_tail_done(live)));
    assert!(clock.now() < end + END_TAIL_LIMIT, "ended by itself");
    let report = studio.stop_output(live).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.flush_lost, 0, "a shared encoder is not flushed");
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(slots(&studio), [true, true, false, false, false, true]);
    let before = r.log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 8);
    assert!(reached, "the recording keeps delivering");

    // A new stream takes the free output partition and shares the recording's chain.
    let again = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    assert!(studio.output(again).unwrap().video_shared);
    assert_eq!(slots(&studio), [true, true, true, false, false, true]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached);

    let (reports, errors) = studio.shutdown();
    assert_eq!(reports.len(), 2);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(reports.iter().all(|(_, r)| r.shutdown_errors.is_empty()));
    assert_eq!(r.log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 2);
    assert_eq!(studio.engine().workers(), 0);
}

#[test]
fn an_output_that_cannot_start_leaves_the_running_one_alone() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);
    let live = studio
        .start_output(&r.adapters, &stream(6_000_000))
        .unwrap();
    assert!(run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= 8));

    // Same settings (it would share the encoders), but the file cannot be opened.
    let mut out = OutputSettings::new("mem://fail", ContainerId::Matroska);
    out.video = stream(6_000_000).video;
    match studio.start_output(&r.adapters, &out) {
        Err(RecordError::Start(failed)) => {
            assert!(failed.iter().any(|f| f.node == "output"), "{failed:?}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(studio.outputs(), [live]);
    assert_eq!(slots(&studio), [true, true, true, false, false, false]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached, "the stream goes on");
    assert_eq!(studio.output_failure(live), None);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn an_output_of_a_further_canvas_has_its_own_export_and_encoder() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_record::ExtraCanvas;

    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    col.set_program(tall, Some(program)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.extra_canvases = vec![ExtraCanvas::new(tall)];
    let mut studio = Studio::start(engine, gpu().unwrap(), &col, &r.adapters.sources, &settings)
        .expect("a canvas without a picture starts");
    // Nothing consumes either canvas yet (P13).
    assert!(!studio.engine().has_worker(rbs_record::RENDER));

    let horizontal = studio.start_output(&r.adapters, &recording()).unwrap();
    let one = r.video_sessions.load(Ordering::Relaxed);
    // The same settings on another canvas never share the encoder.
    let mut vertical_out = stream(6_000_000);
    vertical_out.video = recording().video;
    vertical_out.canvas = Some(tall);
    let vertical = studio.start_output(&r.adapters, &vertical_out).unwrap();
    assert!(!studio.output(vertical).unwrap().video_shared);
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 2 * one);

    let reached = run_until(&mut studio, &clock, |_| {
        r.log.count(0) >= 8 && r.stream_log.count(0) >= 8
    });
    assert!(reached, "{:?}", studio.output_progress(vertical));

    for id in [horizontal, vertical] {
        let report = studio.stop_output(id).unwrap();
        assert!(report.shutdown_errors.is_empty(), "{report:?}");
    }
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn each_canvas_runs_two_outputs_and_four_run_in_all() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_record::{ExtraCanvas, MAX_OUTPUTS};

    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    col.set_program(tall, Some(program)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.extra_canvases = vec![ExtraCanvas::new(tall)];
    let mut studio = Studio::start(engine, gpu().unwrap(), &col, &r.adapters.sources, &settings)
        .expect("a canvas without a picture starts");

    let on = |canvas, bitrate: u64| {
        let mut o = stream(bitrate);
        o.canvas = canvas;
        o
    };
    // The slots of the second canvas cost nothing until used (P13).
    assert_eq!(more_slots(&studio), [false; 6]);
    // A recording and a stream on the main canvas, and the same on the vertical one.
    let mut ids = vec![
        studio.start_output(&r.adapters, &recording()).unwrap(),
        studio
            .start_output(&r.adapters, &on(None, 6_000_000))
            .unwrap(),
    ];
    let mut tall_rec = recording();
    tall_rec.canvas = Some(tall);
    ids.push(studio.start_output(&r.adapters, &tall_rec).unwrap());
    ids.push(
        studio
            .start_output(&r.adapters, &on(Some(tall), 4_000_000))
            .unwrap(),
    );
    assert_eq!(ids.len(), MAX_OUTPUTS);
    assert_eq!(studio.outputs().len(), MAX_OUTPUTS);
    // Every slot has its own video encoder and output, the third and fourth included;
    // all four share the one AAC encoder (the same audio settings, one mix).
    assert_eq!(slots(&studio), [true, true, true, true, false, true]);
    assert_eq!(more_slots(&studio), [true, false, true, true, false, true]);
    // A third output of either canvas is refused before anything is chosen.
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    for canvas in [None, Some(tall)] {
        assert!(matches!(
            studio.start_output(&r.adapters, &on(canvas, 3_000_000)),
            Err(RecordError::TooManyOutputs)
        ));
    }
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);

    // Every output encodes frames, the vertical ones too.
    let reached = run_until(&mut studio, &clock, |s| {
        ids.iter().all(|id| {
            s.output_progress(*id)
                .is_some_and(|p| p.frames_encoded >= 4)
        })
    });
    assert!(
        reached,
        "{:?}",
        ids.iter()
            .map(|id| studio.output_progress(*id))
            .collect::<Vec<_>>()
    );
    // An output that ended but is not stopped yet still counts.
    assert!(studio.end_output(ids[2]));
    assert!(matches!(
        studio.start_output(&r.adapters, &on(Some(tall), 3_000_000)),
        Err(RecordError::TooManyOutputs)
    ));
    for id in ids {
        let report = studio.stop_output(id).unwrap();
        assert!(report.shutdown_errors.is_empty(), "{report:?}");
    }
    assert_eq!(slots(&studio), [false; 6]);
    assert_eq!(more_slots(&studio), [false; 6]);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_further_canvas_starts_and_stops_while_the_studio_runs() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_record::ExtraCanvas;

    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    col.set_program(tall, Some(program)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(main),
    )
    .unwrap();
    let mut vertical = recording();
    vertical.canvas = Some(tall);
    // Not running yet: refused before anything is chosen.
    assert!(matches!(
        studio.start_output(&r.adapters, &vertical),
        Err(RecordError::UnknownCanvas(c)) if c == tall
    ));

    studio
        .activate_canvas(&col, &r.adapters.sources, ExtraCanvas::new(tall))
        .unwrap();
    assert!(matches!(
        studio.activate_canvas(&col, &r.adapters.sources, ExtraCanvas::new(tall)),
        Err(RecordError::DuplicateCanvas(c)) if c == tall
    ));
    let id = studio.start_output(&r.adapters, &vertical).unwrap();
    assert!(matches!(
        studio.deactivate_canvas(&col, &r.adapters.sources, tall),
        Err(RecordError::CanvasBusy(c)) if c == tall
    ));
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= 8);
    assert!(reached, "{:?}", studio.output_progress(id));
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");

    studio
        .deactivate_canvas(&col, &r.adapters.sources, tall)
        .unwrap();
    assert!(matches!(
        studio.start_output(&r.adapters, &vertical),
        Err(RecordError::UnknownCanvas(c)) if c == tall
    ));
    // It can run again.
    studio
        .activate_canvas(&col, &r.adapters.sources, ExtraCanvas::new(tall))
        .unwrap();
    let id = studio.start_output(&r.adapters, &vertical).unwrap();
    let before = r.log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 8);
    assert!(reached, "{:?}", studio.output_progress(id));
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

fn live() -> OutputSettings {
    OutputSettings::packets("stream://live/app")
}

fn picture(r: &Rig) -> Size {
    r.video_config.lock().unwrap().as_ref().unwrap().format.size
}

#[test]
fn an_unset_output_size_is_the_canvas_and_the_same_size_shares() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    assert_eq!(picture(&r), Size::new(W, H));
    let sessions = r.video_sessions.load(Ordering::Relaxed);

    // Odd, below 2, or above 7680: nothing starts, and the running output stays.
    for (width, height) in [(33u32, 18), (1, 2), (0, 2), (7690, 2), (7682, 4), (3, 4)] {
        let mut bad = recording();
        bad.size = Some((width, height));
        let err = studio.start_output(&r.adapters, &bad).unwrap_err();
        assert!(
            matches!(
                err,
                RecordError::OutputSize {
                    width: w,
                    height: h
                } if w == width && h == height
            ),
            "{err}"
        );
    }
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);
    assert_eq!(studio.outputs(), [rec]);

    // The same resolved size (explicit canvas size, same video settings) shares.
    let mut same = live();
    same.size = Some((W, H));
    let joined = studio.start_output(&r.adapters, &same).unwrap();
    let report = studio.output(joined).unwrap();
    assert!(report.video_shared && report.audio_shared, "{report:?}");
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), sessions);
    assert_eq!(slots(&studio), [true, true, true, false, false, true]);

    let reached = run_until(&mut studio, &clock, |_| {
        r.log.count(0) >= 8 && r.stream_log.count(0) >= 8
    });
    assert!(reached);

    let end = clock.now();
    assert!(studio.end_output(joined));
    assert!(run_until(&mut studio, &clock, |s| {
        s.output_tail_done(joined)
    }));
    assert!(clock.now() < end + END_TAIL_LIMIT, "ended by itself");
    let report = studio.stop_output(joined).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.flush_lost, 0, "a shared encoder is not flushed");
    let before = r.log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 8);
    assert!(reached, "the recording keeps delivering");
    assert_eq!(slots(&studio), [true, true, true, false, false, false]);
    assert!(studio.output(rec).is_some());

    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_different_output_size_uses_its_own_encoder() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let (mut studio, clock) = start(&r);

    let rec = studio.start_output(&r.adapters, &recording()).unwrap();
    let sessions = r.video_sessions.load(Ordering::Relaxed);
    assert_eq!(picture(&r), Size::new(W, H));

    let mut half = live();
    half.size = Some((W / 2, H / 2));
    let small = studio.start_output(&r.adapters, &half).unwrap();
    let report = studio.output(small).unwrap();
    assert!(!report.video_shared, "other size: an encoder of its own");
    assert!(
        report.audio_shared,
        "same audio settings: the AAC encoder is shared"
    );
    assert_eq!(picture(&r), Size::new(W / 2, H / 2));
    // open probes a session and prepare opens another, as the first output did.
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 2 * sessions);
    assert_eq!(slots(&studio), [true, true, true, true, false, true]);

    let reached = run_until(&mut studio, &clock, |_| {
        r.log.count(0) >= 8 && r.stream_log.count(0) >= 8
    });
    assert!(reached, "{:?}", studio.output_progress(small));

    // The canvas-sized output stops. The scaled one keeps delivering.
    let end = clock.now();
    assert!(studio.end_output(rec));
    assert!(run_until(&mut studio, &clock, |s| s.output_tail_done(rec)));
    assert!(clock.now() < end + END_TAIL_LIMIT, "ended by itself");
    let report = studio.stop_output(rec).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(r.log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(r.stream_log.finished.load(Ordering::Relaxed), 0);
    assert_eq!(slots(&studio), [false, true, false, true, false, true]);
    let before = r.stream_log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.stream_log.count(0) >= before + 8);
    assert!(reached, "the scaled output keeps delivering");

    let report = studio.stop_output(small).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
