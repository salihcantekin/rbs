//! The studio (spec `recording.md`, "Studio", ADR-0041): the program scene runs, the
//! mixer is metered while nothing records, recordings start and stop on the running
//! engine, and scene edits open and close sources while it runs. Reference adapters on
//! a `TestClock`, as in `record.rs`; tests wait for counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_core::media::{CodecId, ContainerId};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::Cadence;
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    AUDIO, AUDIO_ENCODER, END_TAIL_LIMIT, OUTPUT, OutputSettings, RENDER, RecordError, Studio,
    StudioSettings, VIDEO_ENCODER,
};
use rbs_scene::model::{ItemContent, Layout, Settings};
use rbs_sources::types;
use support::*;

fn workers(studio: &Studio) -> [bool; 5] {
    [RENDER, AUDIO, VIDEO_ENCODER, AUDIO_ENCODER, OUTPUT].map(|p| studio.engine().has_worker(p))
}

#[test]
fn recordings_start_and_stop_while_the_studio_keeps_running() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let log = &r.log;
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    // Without a preview nothing consumes the canvas: no render worker and no capture
    // open. The mixer runs for its meters (meter sink): audio sources are open.
    assert_eq!(workers(&studio), [false, true, false, false, false]);
    assert_eq!(r.capture.live_buffers(), 0, "no display or camera open");
    assert_eq!(r.audio.active_streams(), 2);
    let names: Vec<String> = studio.sources().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["Microphone", "System audio"]);
    let reached = run_until(&mut studio, &clock, |s| {
        s.progress().master_peak_db.is_some_and(f32::is_finite)
    });
    assert!(reached, "the master meter moves while nothing records");

    for (round, url) in ["mem://first", "mem://second"].into_iter().enumerate() {
        let out = OutputSettings::new(url, ContainerId::Matroska);
        assert!(studio.outputs().is_empty(), "no output");
        let id = studio.start_output(&r.adapters, &out).unwrap();
        let started = studio.output(id).unwrap().clone();
        assert!(!started.video_shared && !started.audio_shared);
        assert_eq!(started.sources.len(), 4, "{started:?}");
        assert!(started.sources.iter().all(|s| s.failed.is_none()));
        assert_eq!(studio.outputs(), [id]);
        assert!(studio.link(id).is_some());
        let before = (log.count(0), log.count(1));
        let reached = run_until(&mut studio, &clock, |_| {
            log.count(0) >= before.0 + 8 && log.count(1) >= before.1 + 12
        });
        let w = studio.engine().worker_stats(RENDER).unwrap();
        assert!(
            reached,
            "round {round}: {:?} render ticks {} late {} starts {} workers {:?}",
            studio.progress(),
            w.ticks(),
            w.late(),
            w.starts(),
            workers(&studio)
        );

        // The second round ends first (ADR-0045): every canvas up to the end reaches
        // the file, although the render partition keeps running and the export leaves
        // it without a final tick.
        let end = (round == 1).then(|| {
            assert!(!studio.output_tail_done(id), "not ended yet");
            let end = clock.now();
            assert!(studio.end_output(id));
            assert!(run_until(&mut studio, &clock, |s| s.output_tail_done(id)));
            assert!(clock.now() < end + END_TAIL_LIMIT, "ended by itself");
            end
        });
        let report = studio.stop_output(id).unwrap();
        if let Some(end) = end {
            let frames = Cadence::frames(Timestamp::ZERO, FPS);
            let last_tick = frames.time_of(due(&frames, Duration::ZERO, end) - 1);
            assert_eq!(pts_of(log, 0).last(), Some(&last_tick), "{report:?}");
            let p = &report.progress;
            assert_eq!(p.frames_exported, p.frames_encoded, "{report:?}");
        }
        assert!(report.shutdown_errors.is_empty(), "{report:?}");
        assert_eq!(report.failure, None);
        assert!(report.progress.packets_muxed() > 0);
        assert_eq!(report.progress.mux_drops, 0);
        let n = round as u64 + 1;
        assert_eq!(log.headers.load(Ordering::Relaxed), n);
        assert_eq!(log.trailers.load(Ordering::Relaxed), n, "trailer written");
        assert_eq!(
            log.finished.load(Ordering::Relaxed),
            n,
            "transport finished"
        );
        assert!(studio.outputs().is_empty());
        assert!(studio.stop_output(id).is_none());
        assert_eq!(studio.link(id), None);

        // The recording's partitions stopped; the audio kept running; video sources
        // are closed again (nothing shows them).
        assert_eq!(workers(&studio), [false, true, false, false, false]);
        assert_eq!(r.capture.live_buffers(), 0);
        assert_eq!(r.audio.active_streams(), 2);
        assert_eq!(studio.mixer_anomalies(), 0);
    }
    // Streams: video then audio.
    let streams = log.streams.lock().unwrap().clone();
    assert_eq!(streams[0].codec, CodecId::H264);
    assert_eq!(streams[1].codec, CodecId::Aac);

    let (report, errors) = studio.shutdown();
    assert!(report.is_empty());
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(studio.engine().workers(), 0);
    assert_eq!(r.audio.active_streams(), 0);
}

#[test]
fn a_recording_that_cannot_start_leaves_the_studio_running() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    // No muxer for the container: refused before anything is added.
    let err = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://x", ContainerId::Mp4Fragmented),
        )
        .unwrap_err();
    assert!(matches!(err, RecordError::NoAdapter { .. }), "{err}");
    assert_eq!(r.capture.live_buffers(), 0);

    // The destination cannot be opened: the output fails in `prepare`, the recording's
    // nodes are removed again and the studio runs on.
    let err = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://fail", ContainerId::Matroska),
        )
        .unwrap_err();
    match &err {
        RecordError::Start(failed) => {
            assert!(failed.iter().any(|f| f.node == "output"), "{failed:?}");
        }
        other => panic!("{other}"),
    }
    assert!(studio.outputs().is_empty());
    assert_eq!(workers(&studio), [false, true, false, false, false]);
    assert_eq!(r.log.headers.load(Ordering::Relaxed), 0);

    // The next recording works.
    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://ok", ContainerId::Matroska),
        )
        .unwrap();
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= 4);
    assert!(reached, "{:?}", studio.progress());
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(r.log.trailers.load(Ordering::Relaxed), 1);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn scene_edits_open_and_close_sources_while_it_runs() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let program = col.canvas(canvas).unwrap().program.unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://edit", ContainerId::Matroska),
        )
        .unwrap();
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= 3);
    assert!(reached);
    let buffers = r.capture.live_buffers();
    assert!(buffers > 0);

    // A second microphone and a second camera added while recording.
    let (mic, _) = col
        .add_source(
            "Microphone 2",
            NodeTypeId::new_static(types::AUDIO_INPUT_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.add_item(
        program,
        ItemContent::Source(mic),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    let (cam, _) = col
        .add_source(
            "Camera 2",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.add_item(
        program,
        ItemContent::Source(cam),
        Layout::new([0.0, 0.0], [16.0, 9.0]),
        None,
    )
    .unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert!(updated.problems.is_empty(), "{updated:?}");
    assert_eq!(studio.sources().len(), 6);
    assert_eq!(r.audio.active_streams(), 3);
    assert!(
        r.capture.live_buffers() > buffers,
        "the second camera is open"
    );
    assert!(
        studio.meter(mic).is_some(),
        "the new input has a strip meter"
    );
    let frames = |s: &Studio, name: &str| {
        s.progress()
            .sources
            .iter()
            .find(|p| p.name == name)
            .map_or(0, |p| p.frames)
    };
    let reached = run_until(&mut studio, &clock, |s| {
        frames(s, "Camera 2") > 0 && frames(s, "Microphone 2") > 0
    });
    assert!(reached, "{:?}", studio.progress());
    assert_eq!(studio.mixer_anomalies(), 0);
    // The camera's capture reports its picture size (ADR-0058); sound has none.
    let size = studio.picture_size(cam).expect("the camera's picture size");
    assert!(size.0 > 0 && size.1 > 0, "{size:?}");
    assert_eq!(studio.picture_size(mic), None);

    // Removed again: closed, and the recording goes on.
    col.delete_source(mic).unwrap();
    col.delete_source(cam).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.sources().len(), 4);
    assert_eq!(r.audio.active_streams(), 2);
    assert_eq!(r.capture.live_buffers(), buffers);
    assert!(studio.meter(mic).is_none());
    assert_eq!(studio.picture_size(cam), None);
    let before = r.log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 3);
    assert!(reached);
    assert_eq!(studio.mixer_anomalies(), 0);
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(not(any(windows, target_vendor = "apple")))]
#[test]
fn a_preview_where_the_platform_has_none_fails_the_start() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, _clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.preview = Some(rbs_gpu::share::PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 3,
    });
    let err = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap_err();
    match err {
        RecordError::Start(failed) => {
            assert!(failed.iter().any(|f| f.node == "preview"), "{failed:?}");
        }
        other => panic!("{other}"),
    }
    assert_eq!(r.capture.live_buffers(), 0);
    assert_eq!(r.audio.active_streams(), 0);
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn a_ui_subscribed_to_the_studio_preview_gets_canvas_frames() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::{Duration, Instant};

    use rbs_gpu::share::{PreviewDesc, PreviewSubscriber};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    settings.preview = Some(PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 4,
    });
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    // The preview keeps the canvas scheduled: render runs and the display is open
    // without a recording.
    assert!(studio.engine().has_worker(RENDER));
    assert!(r.capture.live_buffers() > 0);
    let endpoint = studio.preview().expect("the preview node is prepared");

    // The UI side on its own thread: connect, then count frames (real-time limits
    // are hang guards only; the test ends on the count).
    let frames = Arc::new(AtomicU64::new(0));
    let ui = {
        let frames = Arc::clone(&frames);
        std::thread::spawn(move || {
            let mut sub = PreviewSubscriber::connect(&endpoint, LIMIT).unwrap();
            let wall = Instant::now();
            while frames.load(Ordering::Relaxed) < 3 && wall.elapsed() < LIMIT {
                if let Some(frame) = sub.recv(Duration::from_millis(250)).unwrap() {
                    assert!(frame.sequence() > 0);
                    frames.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let reached = run_until(&mut studio, &clock, |_| frames.load(Ordering::Relaxed) >= 3);
    ui.join().unwrap();
    assert!(reached, "{:?}", studio.preview_stats());
    let stats = studio.preview_stats().unwrap();
    assert!(stats.published.load(Ordering::Relaxed) >= 3);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

fn resyncs(studio: &Studio, name: &str) -> u64 {
    let p = studio.progress();
    let s = p.sources.iter().find(|s| s.name == name);
    s.map_or(0, |s| s.resyncs)
}

#[test]
fn audio_sync_offsets_apply_from_the_start_and_while_it_runs() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let system = col.sources().find(|s| s.name == "System audio").unwrap().id;
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    const MS_40: i64 = 40_000_000;
    let mut settings = StudioSettings::new(canvas);
    settings.audio_offsets.insert(mic, MS_40);
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let reached = run_until(&mut studio, &clock, |s| {
        s.progress().master_peak_db.is_some_and(f32::is_finite)
    });
    assert!(reached);
    let base = (
        resyncs(&studio, "Microphone"),
        resyncs(&studio, "System audio"),
    );

    // The microphone has had its offset since its first block: the same value again
    // changes nothing, so its input does not resync.
    assert!(studio.set_audio_offset(mic, MS_40));
    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 20));
    assert_eq!(
        resyncs(&studio, "Microphone"),
        base.0,
        "no change, no resync"
    );

    // A change above 5 ms resyncs the running input once; the other source keeps its
    // timeline.
    assert!(studio.set_audio_offset(system, -MS_40 / 4));
    let reached = run_until(&mut studio, &clock, |s| resyncs(s, "System audio") > base.1);
    assert!(reached, "{:?}", studio.progress());
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 20));
    assert_eq!(resyncs(&studio, "System audio"), base.1 + 1);
    assert_eq!(resyncs(&studio, "Microphone"), base.0);
    assert_eq!(
        studio.settings().audio_offsets.get(&system),
        Some(&(-MS_40 / 4))
    );

    // 0 removes it; a source without an audio input only keeps the setting.
    assert!(studio.set_audio_offset(mic, 0));
    assert!(!studio.settings().audio_offsets.contains_key(&mic));
    assert!(!studio.set_audio_offset(display, MS_40));
    assert_eq!(studio.settings().audio_offsets.get(&display), Some(&MS_40));

    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn volume_and_mute_reach_the_mixer_without_a_rebind() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let system = col.sources().find(|s| s.name == "System audio").unwrap().id;
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let peak = |s: &Studio, id| s.meter(id).unwrap().take().peak(0);
    // The strip meters are post-fader: both inputs are heard.
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    assert!(peak(&studio, mic) > 0.0);
    assert!(peak(&studio, system) > 0.0);
    let sources = studio.sources();

    // Mute the microphone on the light path: after the ramp it is silent, the other
    // input is not, and no source was opened or closed again.
    col.set_source_muted(mic, true).unwrap();
    assert!(studio.set_mix(&col));
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 5));
    let _ = (peak(&studio, mic), peak(&studio, system));
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    assert_eq!(peak(&studio, mic), 0.0, "muted after the ramp");
    assert!(peak(&studio, system) > 0.0);
    assert_eq!(studio.sources(), sources);
    assert_eq!(r.audio.active_streams(), 2);

    // A scene edit afterwards keeps the levels (the update reads them too), and a
    // volume of 0 is silent like a mute.
    col.set_source_muted(mic, false).unwrap();
    col.set_source_volume(system, 0.0).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 5));
    let _ = (peak(&studio, mic), peak(&studio, system));
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    assert!(peak(&studio, mic) > 0.0, "unmuted");
    assert_eq!(peak(&studio, system), 0.0, "volume 0");
    assert_eq!(studio.mixer_anomalies(), 0);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn the_master_gain_reaches_the_mixer_at_start_and_on_the_light_path() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    // A saved, muted master is applied from the first block (ADR-0081).
    col.set_canvas_master_muted(canvas, true).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let master = |s: &Studio| s.master_meter().unwrap().take().peak(0);
    let strip = |s: &Studio| s.meter(mic).unwrap().take().peak(0);
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 15));
    let _ = (master(&studio), strip(&studio));
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    assert_eq!(master(&studio), 0.0, "a muted master is silent");
    assert!(strip(&studio) > 0.0, "the strip is post-strip-fader only");

    // Unmute on the light path: the master hears the mix again, with no rebind.
    col.set_canvas_master_muted(canvas, false).unwrap();
    assert!(studio.set_mix(&col));
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 5));
    let _ = master(&studio);
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    let full = master(&studio);
    assert!(full > 0.0, "unmuted");

    // Half volume lowers the peak.
    col.set_canvas_master_volume(canvas, 0.5).unwrap();
    assert!(studio.set_mix(&col));
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 5));
    let _ = master(&studio);
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    let half = master(&studio);
    assert!(half > 0.0 && half < full, "{half} < {full}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn filters_run_on_a_source_and_follow_edits_without_a_new_node() {
    use rbs_scene::model::SettingValue;
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let display = col
        .sources()
        .find(|s| s.type_id.as_str() == types::DISPLAY_SOURCE)
        .map(|s| s.id)
        .unwrap();
    // A stored radius out of range: run clamped and reported, the document untouched.
    let mut settings = Settings::new();
    settings.insert("radius".to_owned(), SettingValue::Float(500.0));
    let (blur, _) = col
        .add_filter(
            display,
            NodeTypeId::new_static(rbs_filters::catalogue::BLUR),
            settings,
        )
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    assert_eq!(studio.filter_problems().len(), 1);
    assert_eq!(studio.filter_problems()[0].key, "radius");
    let frames = |s: &Studio| s.filter_stats(blur).map_or(0, |st| st.frames());
    // Pictures run only while something uses the canvas.
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://filters", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| frames(s) >= 3));

    // A settings edit reaches the running node; no node is made for it.
    let stats = std::sync::Arc::clone(studio.filter_stats(blur).unwrap());
    let mut fixed = Settings::new();
    fixed.insert("radius".to_owned(), SettingValue::Float(4.0));
    col.set_filter_settings(display, blur, fixed).unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert!(studio.filter_problems().is_empty());
    assert!(std::sync::Arc::ptr_eq(
        &stats,
        studio.filter_stats(blur).unwrap()
    ));
    let before = frames(&studio);
    assert!(run_until(&mut studio, &clock, |s| frames(s) > before));

    // Removed: its instance is taken back.
    col.remove_filter(display, blur).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.filter_stats(blur).is_none());
    let report = studio.stop_output(out).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn audio_filters_run_on_a_microphone_and_follow_edits_without_a_new_node() {
    use rbs_scene::model::SettingValue;
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let (gate, _) = col
        .add_filter(
            mic,
            NodeTypeId::new_static(rbs_audio_filters::gate::NOISE_GATE),
            Settings::new(),
        )
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    assert!(studio.filter_problems().is_empty());
    // The filter runs in the audio partition and the mix keeps playing through it.
    let meter = std::sync::Arc::clone(studio.audio_filter_meter(gate).unwrap());
    assert!(run_until(&mut studio, &clock, |s| {
        s.progress().master_peak_db.is_some_and(f32::is_finite)
    }));
    assert!(meter.input_peak_db() <= 0.0);

    // An edit reaches the running node; no node is made for it.
    let mut edited = Settings::new();
    edited.insert("hold".to_owned(), SettingValue::Float(20.0));
    col.set_filter_settings(mic, gate, edited).unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert!(std::sync::Arc::ptr_eq(
        &meter,
        studio.audio_filter_meter(gate).unwrap()
    ));

    // Removed: its instance is taken back and the microphone plays on unfiltered.
    col.remove_filter(mic, gate).unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert!(studio.audio_filter_meter(gate).is_none());
    assert!(run_until(&mut studio, &clock, |s| {
        s.progress().master_peak_db.is_some_and(f32::is_finite)
    }));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn studio_mode_without_a_preview_description_runs_nothing_of_it() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let (empty, _) = col
        .add_scene("Empty", rbs_scene::model::SceneScope::Shared)
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    assert!(!studio.studio_mode(), "nothing of it outside studio mode");

    // Studio mode without a preview description (headless): nothing of it runs.
    col.set_preview(canvas, Some(empty)).unwrap();
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    assert!(!studio.studio_mode());
    assert!(studio.scene_preview().is_none());
    let t = studio.engine().worker_stats(AUDIO).unwrap().ticks();
    assert!(run_until(&mut studio, &clock, |s| {
        s.engine().worker_stats(AUDIO).unwrap().ticks() >= t + 5
    }));

    // A cut that swaps the scenes: the program follows, the studio runs on.
    col.set_program(canvas, Some(empty)).unwrap();
    col.set_preview(canvas, Some(live)).unwrap();
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    assert!(studio.failure().is_none());

    // Leaving studio mode retires it; the studio runs on.
    col.set_preview(canvas, None).unwrap();
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    assert!(!studio.studio_mode());
    let t = studio.engine().worker_stats(AUDIO).unwrap().ticks();
    assert!(run_until(&mut studio, &clock, |s| {
        s.engine().worker_stats(AUDIO).unwrap().ticks() >= t + 5
    }));
    assert!(studio.failure().is_none());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn in_studio_mode_a_ui_gets_the_preview_scene_on_its_own_picture() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    use rbs_gpu::share::{PreviewDesc, PreviewSubscriber};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    settings.preview = Some(PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 3,
    });
    settings.scene_preview = true;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    assert!(studio.scene_preview().is_none());
    col.set_preview(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let endpoint = studio.scene_preview().expect("the preview scene's picture");
    assert_ne!(
        Some(&endpoint),
        studio.preview().as_ref(),
        "a picture of its own"
    );

    let frames = Arc::new(AtomicU64::new(0));
    let ui = {
        let frames = Arc::clone(&frames);
        std::thread::spawn(move || {
            let mut sub = PreviewSubscriber::connect(&endpoint, LIMIT).unwrap();
            let wall = Instant::now();
            while frames.load(Ordering::Relaxed) < 3 && wall.elapsed() < LIMIT {
                if sub.recv(Duration::from_millis(250)).unwrap().is_some() {
                    frames.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let reached = run_until(&mut studio, &clock, |_| frames.load(Ordering::Relaxed) >= 3);
    ui.join().unwrap();
    assert!(reached);
    // A cut that swaps the scenes keeps both pictures and opens or closes no source.
    let (empty, _) = col
        .add_scene("Empty", rbs_scene::model::SceneScope::Shared)
        .unwrap();
    col.set_preview(canvas, Some(empty)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let sources = studio.sources();
    let before = studio.scene_preview();
    col.set_program(canvas, Some(empty)).unwrap();
    col.set_preview(canvas, Some(live)).unwrap();
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    assert_eq!(studio.scene_preview(), before, "the same picture");
    assert_eq!(studio.sources(), sources);
    col.set_preview(canvas, None).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.scene_preview().is_none());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
