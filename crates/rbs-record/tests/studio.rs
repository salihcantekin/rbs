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
    AUDIO, AUDIO_ENCODER, END_TAIL_LIMIT, ExtraCanvas, OUTPUT, OutputSettings, RENDER, RecordError,
    Studio, StudioSettings, VIDEO_ENCODER,
};
use rbs_scene::SceneLevel;
#[cfg(any(windows, target_vendor = "apple"))]
use rbs_scene::model::CanvasId;
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
fn a_scene_switch_sends_the_program_scenes_own_levels() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let system = col.sources().find(|s| s.name == "System audio").unwrap().id;
    let live = col.canvas(canvas).unwrap().program.unwrap();
    // "Intermission": the same sources, the microphone muted by the scene (ADR-0091).
    let (quiet, _) = col.duplicate_scene(live, "Intermission").unwrap();
    col.set_scene_own_levels(quiet, col.heard_levels(canvas, quiet))
        .unwrap();
    col.set_scene_level(quiet, mic, Some(SceneLevel::new(None, Some(true))))
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
    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let peak = |s: &Studio, id| s.meter(id).unwrap().take().peak(0);
    let settle = |studio: &mut Studio| {
        let t = blocks(studio);
        assert!(run_until(studio, &clock, |s| blocks(s) >= t + 5));
        let _ = (peak(studio, mic), peak(studio, system));
        let t = blocks(studio);
        assert!(run_until(studio, &clock, |s| blocks(s) >= t + 10));
    };
    settle(&mut studio);
    assert!(peak(&studio, mic) > 0.0, "Live uses the source's own level");

    // The switch goes through the ordinary update and brings the scene's levels.
    col.set_program(canvas, Some(quiet)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    settle(&mut studio);
    assert_eq!(peak(&studio, mic), 0.0, "muted by the scene");
    assert!(peak(&studio, system) > 0.0, "no entry: its own level");

    // A live edit of the program scene's entry takes the light path.
    col.set_scene_level(quiet, mic, None).unwrap();
    assert!(studio.set_mix(&col));
    settle(&mut studio);
    assert!(peak(&studio, mic) > 0.0, "entry reset");

    // Back to a scene in Shared levels: the source's own values again.
    col.set_scene_level(quiet, mic, Some(SceneLevel::new(None, Some(true))))
        .unwrap();
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    settle(&mut studio);
    assert!(peak(&studio, mic) > 0.0);
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
fn a_ducker_lowers_one_source_by_the_signal_of_another() {
    use rbs_scene::model::SettingValue;
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let music = col.sources().find(|s| s.name == "System audio").unwrap().id;
    let mut settings = Settings::new();
    settings.insert(
        "key_source".to_owned(),
        SettingValue::Int(i64::from(mic.get())),
    );
    // Any key louder than -80 dB ducks.
    settings.insert("threshold".to_owned(), SettingValue::Float(-80.0));
    settings.insert("amount".to_owned(), SettingValue::Float(12.0));
    settings.insert("attack".to_owned(), SettingValue::Float(1.0));
    let (duck, _) = col
        .add_filter(
            music,
            NodeTypeId::new_static(rbs_audio_filters::ducker::DUCKER),
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
    assert!(studio.filter_problems().is_empty());
    assert!(studio.problems().is_empty(), "{:?}", studio.problems());
    let meter = std::sync::Arc::clone(studio.audio_filter_meter(duck).unwrap());
    // The microphone's tone reaches the ducker's key and lowers the music.
    assert!(run_until(&mut studio, &clock, |_| {
        meter.input_peak_db() > -80.0 && meter.gain_reduction_db() > 11.0
    }));
    // Without a key setting the ducker hears silence and lets the music return.
    let mut none = Settings::new();
    none.insert("amount".to_owned(), SettingValue::Float(12.0));
    col.set_filter_settings(music, duck, none).unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    assert!(run_until(&mut studio, &clock, |_| {
        meter.gain_reduction_db() < 0.5
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

#[cfg(any(windows, target_vendor = "apple"))]
/// Starts a studio on the shared test scene plus, optionally, a vertical canvas showing
/// the same program scene; returns the studio and the buffers the sources hold.
fn two_canvas_studio(
    vertical: bool,
) -> Option<(Studio, rbs_core::time::TestClock, usize, CanvasId)> {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_gpu::share::PreviewDesc;

    let gpu = gpu()?;
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    col.set_program(tall, Some(program)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let desc = PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 4,
    };
    settings.preview = Some(desc);
    if vertical {
        settings.extra_canvases = vec![ExtraCanvas::new(tall).with_preview(desc)];
    }
    let studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let buffers = r.capture.live_buffers();
    Some((studio, clock, buffers, tall))
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn a_vertical_canvas_shares_the_sources() {
    let Some((mut alone, _, one_canvas, absent)) = two_canvas_studio(false) else {
        return;
    };
    assert!(alone.canvas_preview_stats(absent).is_none());
    let (_, errors) = alone.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    let Some((mut studio, _, two_canvases, tall)) = two_canvas_studio(true) else {
        return;
    };
    // Every source is opened once, whatever the number of canvases (ADR-0093), and the
    // further canvas's picture keeps the render worker alive.
    assert_eq!(one_canvas, two_canvases);
    assert!(studio.canvas_preview_stats(tall).is_some());
    assert!(studio.engine().has_worker(RENDER));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn a_ui_subscribed_to_a_vertical_canvas_gets_its_frames() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Instant;

    use rbs_gpu::share::PreviewSubscriber;

    let Some((mut studio, clock, _, tall)) = two_canvas_studio(true) else {
        return;
    };
    let endpoint = studio
        .canvas_preview(tall)
        .expect("the picture is prepared");
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
    assert!(reached, "{:?}", studio.canvas_preview_stats(tall));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn a_vertical_canvas_gets_and_loses_its_picture_while_it_runs() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_gpu::share::PreviewDesc;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    col.set_program(tall, Some(program)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    settings.extra_canvases = vec![ExtraCanvas::new(tall)];
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    // No panel: no picture (P13).
    assert!(studio.canvas_preview_stats(tall).is_none());
    let desc = PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 4,
    };
    studio
        .set_canvas_picture(&col, &r.adapters.sources, tall, Some(desc))
        .unwrap();
    // Asking again changes nothing.
    studio
        .set_canvas_picture(&col, &r.adapters.sources, tall, Some(desc))
        .unwrap();
    let reached = run_until(&mut studio, &clock, |s| s.canvas_preview(tall).is_some());
    assert!(reached, "the picture is prepared");
    studio
        .set_canvas_picture(&col, &r.adapters.sources, tall, None)
        .unwrap();
    assert!(studio.canvas_preview_stats(tall).is_none());
    assert!(studio.canvas_preview(tall).is_none());
    // A canvas that does not run has no picture to change.
    let absent = CanvasId::from_raw(999);
    assert!(matches!(
        studio.set_canvas_picture(&col, &r.adapters.sources, absent, Some(desc)),
        Err(RecordError::UnknownCanvas(c)) if c == absent
    ));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn pictures_are_added_and_removed_on_the_main_canvas_while_it_runs() {
    use rbs_gpu::share::PreviewDesc;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, main) = scene();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    // No projector: no picture (ADR-0125 decision 4, P13).
    assert!(studio.pictures().is_empty());
    let desc = PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 4,
    };
    // Any number of pictures may show one canvas, each with its own endpoint.
    let (one, _) = studio
        .add_picture(&col, &r.adapters.sources, main, desc)
        .unwrap();
    let (two, _) = studio
        .add_picture(&col, &r.adapters.sources, main, desc)
        .unwrap();
    assert_ne!(one, two);
    assert_eq!(studio.pictures(), vec![one, two]);
    let reached = run_until(&mut studio, &clock, |s| {
        s.picture(one).is_some() && s.picture(two).is_some()
    });
    assert!(reached, "both pictures are prepared");
    assert_ne!(studio.picture(one), studio.picture(two));
    assert!(studio.picture_failure(one).is_none());
    studio
        .remove_picture(&col, &r.adapters.sources, one)
        .unwrap();
    assert!(studio.picture(one).is_none());
    assert!(studio.picture_stats(one).is_none());
    assert_eq!(studio.pictures(), vec![two]);
    // Removing it again changes nothing.
    studio
        .remove_picture(&col, &r.adapters.sources, one)
        .unwrap();
    studio
        .remove_picture(&col, &r.adapters.sources, two)
        .unwrap();
    assert!(studio.pictures().is_empty());
    // A canvas that does not run cannot get a picture.
    let absent = CanvasId::from_raw(999);
    assert!(matches!(
        studio.add_picture(&col, &r.adapters.sources, absent, desc),
        Err(RecordError::UnknownCanvas(c)) if c == absent
    ));
    assert!(studio.pictures().is_empty());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn a_source_picture_runs_a_source_no_scene_shows_until_it_is_removed() {
    use rbs_gpu::share::PreviewDesc;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, main) = scene();
    // A camera in no scene: nothing runs it (P13).
    let (cam, _) = col
        .add_source(
            "Loose camera",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let buffers = r.capture.live_buffers();
    assert_eq!(studio.picture_size(cam), None);
    let desc = PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 4,
    };
    // Its own picture (an Interact window, ADR-0132 decision 6) holds it at Shown
    // (ADR-0152 decision 2): it opens and its picture is prepared.
    let (pid, _) = studio
        .add_source_picture(&col, &r.adapters.sources, cam, desc)
        .unwrap();
    assert_eq!(studio.pictures(), vec![pid]);
    assert!(r.capture.live_buffers() > buffers, "the camera is open");
    let reached = run_until(&mut studio, &clock, |s| {
        s.picture(pid).is_some() && s.picture_size(cam).is_some()
    });
    assert!(reached, "the picture is prepared and the camera runs");
    // Removed: the camera closes again.
    studio
        .remove_picture(&col, &r.adapters.sources, pid)
        .unwrap();
    assert!(studio.pictures().is_empty());
    assert_eq!(studio.picture_size(cam), None);
    assert_eq!(r.capture.live_buffers(), buffers);
    // A source that is gone cannot get a picture.
    col.delete_source(cam).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(
        studio
            .add_source_picture(&col, &r.adapters.sources, cam, desc)
            .is_err()
    );
    assert!(studio.pictures().is_empty());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn pictures_of_a_further_canvas_leave_with_it() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_gpu::share::PreviewDesc;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    col.set_program(tall, Some(program)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    settings.extra_canvases = vec![ExtraCanvas::new(tall)];
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let desc = PreviewDesc {
        width: 18,
        height: 32,
        surfaces: 4,
    };
    // A projector's picture without the panel's: the canvas composes for it alone.
    let (id, _) = studio
        .add_picture(&col, &r.adapters.sources, tall, desc)
        .unwrap();
    assert!(studio.canvas_preview_stats(tall).is_none());
    let reached = run_until(&mut studio, &clock, |s| s.picture(id).is_some());
    assert!(reached, "the picture is prepared");
    studio
        .deactivate_canvas(&col, &r.adapters.sources, tall)
        .unwrap();
    assert!(studio.pictures().is_empty());
    assert!(studio.picture(id).is_none());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_further_canvas_must_run_at_the_main_rate_and_be_new() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, main) = scene();
    let program = col.canvas(main).unwrap().program.unwrap();
    let (slow, _) = col
        .add_canvas("Slow", Size::new(18, 32), FrameRate::FPS_60)
        .unwrap();
    col.set_program(slow, Some(program)).unwrap();
    let start = |extra: Vec<ExtraCanvas>| {
        let (engine, _) = engine_at_start();
        let mut settings = StudioSettings::new(main);
        settings.audio = false;
        settings.extra_canvases = extra;
        Studio::start(engine, gpu.clone(), &col, &r.adapters.sources, &settings).err()
    };
    assert!(matches!(
        start(vec![ExtraCanvas::new(slow)]),
        Some(RecordError::CanvasRate(c)) if c == slow
    ));
    assert!(matches!(
        start(vec![ExtraCanvas::new(main)]),
        Some(RecordError::DuplicateCanvas(c)) if c == main
    ));
}

/// Names of the sources scheduled while an output takes the vertical canvas, which has a
/// scene of its own (with a source only that scene shows); `follow` makes the vertical
/// canvas show the main program scene instead (ADR-0093 decision 3).
fn scheduled_for_vertical(follow: bool) -> Option<Vec<String>> {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;
    use rbs_scene::model::Fallback;

    let gpu = gpu()?;
    let r = rig();
    let (mut col, main) = scene();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    let (own, _) = col
        .add_scene("Vertical only", rbs_scene::model::SceneScope::Canvas(tall))
        .unwrap();
    let (extra, _) = col
        .add_source(
            "Vertical camera",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.add_item(
        own,
        ItemContent::Source(extra),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    col.set_program(tall, Some(own)).unwrap();
    if follow {
        col.set_canvas_follow(tall, Some(main), Fallback::KeepLast)
            .unwrap();
    }
    let (engine, _) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    settings.extra_canvases = vec![ExtraCanvas::new(tall)];
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let mut out = OutputSettings::new("mem://vertical", ContainerId::Matroska);
    out.audio = None;
    out.canvas = Some(tall);
    studio.start_output(&r.adapters, &out).unwrap();
    let names = studio.sources().into_iter().map(|s| s.name).collect();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    Some(names)
}

#[test]
fn a_following_canvas_shows_the_main_program_scene() {
    let Some(own) = scheduled_for_vertical(false) else {
        return;
    };
    assert!(own.iter().any(|n| n == "Vertical camera"), "{own:?}");
    let following = scheduled_for_vertical(true).unwrap();
    assert!(
        !following.iter().any(|n| n == "Vertical camera"),
        "{following:?}"
    );
    assert!(following.iter().any(|n| n == "Display"), "{following:?}");
}

#[test]
fn a_canvas_that_does_not_run_costs_nothing_and_its_sources_close_when_it_stops() {
    use rbs_core::media::video::Size;
    use rbs_core::timing::FrameRate;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, main) = scene();
    let (tall, _) = col
        .add_canvas("Vertical", Size::new(H / 2, W / 2), FrameRate::FPS_30)
        .unwrap();
    let (own, _) = col
        .add_scene("Vertical only", rbs_scene::model::SceneScope::Canvas(tall))
        .unwrap();
    let (extra, _) = col
        .add_source(
            "Vertical camera",
            NodeTypeId::new_static(types::CAMERA_SOURCE),
            Settings::new(),
        )
        .unwrap();
    col.add_item(
        own,
        ItemContent::Source(extra),
        Layout::new([0.0, 0.0], [1.0, 1.0]),
        None,
    )
    .unwrap();
    col.set_program(tall, Some(own)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let mut rec = OutputSettings::new("mem://main", ContainerId::Matroska);
    rec.audio = None;
    studio.start_output(&r.adapters, &rec).unwrap();
    let names = |s: &Studio| -> Vec<String> { s.sources().into_iter().map(|x| x.name).collect() };
    assert!(!names(&studio).iter().any(|n| n == "Vertical camera"));
    let buffers = r.capture.live_buffers();

    // Running the canvas opens its own source ...
    studio
        .activate_canvas(&col, &r.adapters.sources, ExtraCanvas::new(tall))
        .unwrap();
    let mut out = OutputSettings::new("mem://vertical", ContainerId::Matroska);
    out.audio = None;
    out.canvas = Some(tall);
    let id = studio.start_output(&r.adapters, &out).unwrap();
    assert!(names(&studio).iter().any(|n| n == "Vertical camera"));
    let reached = run_until(&mut studio, &clock, |_| r.capture.live_buffers() > buffers);
    assert!(reached);
    // ... and stopping it releases that source again.
    studio.stop_output(id).unwrap();
    studio
        .deactivate_canvas(&col, &r.adapters.sources, tall)
        .unwrap();
    assert!(!names(&studio).iter().any(|n| n == "Vertical camera"));
    let reached = run_until(&mut studio, &clock, |_| r.capture.live_buffers() <= buffers);
    assert!(reached, "the vertical camera is still open");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// A media file and a browser page are adopted on the render partition (ADR-0108,
/// ADR-0109). The fake decoder is registered here only. No FFmpeg and no helper process.
#[test]
fn media_and_browser_sources_are_adopted_on_the_render_partition() {
    use std::sync::Arc;

    use rbs_core::media::video::Size;
    use rbs_media_decode_test::{ScriptClip, ScriptFactory};
    use rbs_scene::SceneCollection;
    use rbs_scene::model::{SceneScope, SettingValue};
    use rbs_sources::types::keys;

    let Some(gpu) = gpu() else {
        return;
    };
    let mut r = rig();
    r.adapters
        .sources
        .register_media(Arc::new(ScriptFactory::new(ScriptClip::silent_frames())))
        .unwrap();
    r.adapters.probe();

    let dir = std::env::temp_dir().join(format!(
        "rbs-record-media-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("clip.mp4");
    std::fs::write(&file, b"not-decoded").unwrap();
    struct RemoveDir(std::path::PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveDir(dir);

    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let mut media_settings = Settings::new();
    media_settings.insert(
        keys::PATH.to_owned(),
        SettingValue::Text(file.to_string_lossy().into_owned()),
    );
    let (clip, _) = col
        .add_source(
            "Clip",
            NodeTypeId::new_static(types::MEDIA_SOURCE),
            media_settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(clip),
        Layout::new([0.0, 0.0], [W as f32, H as f32]),
        None,
    )
    .unwrap();
    let mut page_settings = Settings::new();
    page_settings.insert(
        keys::URL.to_owned(),
        SettingValue::Text("https://example.com/page".to_owned()),
    );
    page_settings.insert(keys::WIDTH.to_owned(), SettingValue::Int(64));
    page_settings.insert(keys::HEIGHT.to_owned(), SettingValue::Int(36));
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            page_settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(page),
        Layout::new([0.0, 0.0], [W as f32, H as f32]),
        None,
    )
    .unwrap();
    col.set_program(canvas, Some(scene)).unwrap();

    use rbs_browser_ipc::Configure;
    use rbs_sources::{BrowserFrameSink, BrowserSession, BrowserTree};

    struct TestBrowserTree;

    struct TestSession;

    impl BrowserSession for TestSession {
        fn poll(&mut self) -> Option<i32> {
            None
        }
        fn stop(&mut self) {}
    }

    impl BrowserTree for TestBrowserTree {
        fn start(
            &self,
            config: &Configure,
            sink: std::sync::Arc<BrowserFrameSink>,
            _sound: Option<std::sync::Arc<rbs_sources::BrowserSound>>,
        ) -> Result<Box<dyn BrowserSession>, String> {
            let pixels = usize::try_from(config.width)
                .unwrap_or(0)
                .saturating_mul(usize::try_from(config.height).unwrap_or(0))
                .saturating_mul(4);
            let mut rgba = vec![8u8; pixels];
            for chunk in rgba.chunks_exact_mut(4) {
                chunk[3] = 255;
            }
            sink.publish(&rgba, config.width, config.height);
            Ok(Box::new(TestSession))
        }
    }

    let (engine, _clock) = engine_at_start();
    let tree: std::sync::Arc<dyn BrowserTree> = std::sync::Arc::new(TestBrowserTree);
    let mut studio = Studio::start_with_tree(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
        Some(tree),
    )
    .unwrap();
    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://page", ContainerId::Matroska),
        )
        .unwrap();
    // The picture node is drawn by the program compositor; its sound node (in the
    // audio partition) is the mixer's producer (ADR-0106).
    let media = studio
        .sources()
        .into_iter()
        .find(|s| s.source == clip)
        .expect("media source scheduled");
    assert_eq!(media.adapter.as_str(), "org.rbs.media-decode.test");
    assert!(media.failed.is_none(), "{media:?}");
    let browser = studio
        .sources()
        .into_iter()
        .find(|s| s.source == page)
        .expect("browser source scheduled");
    assert_ne!(
        browser.failed.as_deref(),
        Some("unsupported instance kind"),
        "{browser:?}"
    );
    assert!(browser.failed.is_none(), "{browser:?}");
    assert_eq!(studio.picture_size(page), Some((64, 36)));
    // On air: the program scene draws it, so it plays (ADR-0107 decision 6).
    let control = studio.media_control(clip).expect("media handle").clone();
    assert_eq!(control.state(), rbs_nodes::MediaPlayState::Playing);
    // Studio mode with the clip only in the preview scene: not on air, so it does
    // not play. Headless, nothing draws the preview, so the cued source sleeps.
    let (empty, _) = col.add_scene("Empty", SceneScope::Shared).unwrap();
    col.set_program(canvas, Some(empty)).unwrap();
    col.set_preview(canvas, Some(scene)).unwrap();
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    // The Move keeps the leaving clip playing while it fades out (ADR-0128); end it.
    studio.end_transitions();
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(control.state(), rbs_nodes::MediaPlayState::Standby);
    // The take: on air again, it plays.
    col.set_program(canvas, Some(scene)).unwrap();
    col.set_preview(canvas, Some(empty)).unwrap();
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    assert_eq!(control.state(), rbs_nodes::MediaPlayState::Playing);
    let _ = studio.stop_output(id);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// Adds a scene "Camera only" with the camera source and returns it.
fn camera_scene(col: &mut rbs_scene::SceneCollection) -> rbs_scene::model::SceneId {
    let camera = col.sources().find(|s| s.name == "Camera").unwrap().id;
    let (scene, _) = col
        .add_scene("Camera only", rbs_scene::model::SceneScope::Shared)
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(camera),
        Layout::new([0.0, 0.0], [W as f32, H as f32]),
        None,
    )
    .unwrap();
    scene
}

#[test]
fn a_fade_waits_for_the_incoming_picture_blends_and_settles() {
    use rbs_record::{READY_WAIT, TransitionState};
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    let mut fade = CollectionTransition {
        kind: TransitionKind::Fade,
        ..CollectionTransition::default()
    };
    fade.fade.duration_ms = 100;
    col.set_transition(fade).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    // An output, so the program canvas is drawn.
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition", ContainerId::Matroska),
        )
        .unwrap();
    let ticks = |s: &Studio| s.engine().worker_stats(RENDER).unwrap().ticks();
    let t0 = ticks(&studio);
    assert!(run_until(&mut studio, &clock, |s| ticks(s) >= t0 + 3));
    assert_eq!(
        studio.transition_state(canvas),
        None,
        "nothing before a change"
    );
    assert_eq!(
        studio.transition_wait(clock.now()),
        None,
        "no timer armed (P13)"
    );

    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let changed = clock.now();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );
    assert!(studio.transition_wait(clock.now()).is_some());

    // The incoming compositor draws within a few frames, well before READY_WAIT; then
    // the blend runs, then ends.
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));
    assert!(
        clock.now().saturating_since(changed) < READY_WAIT,
        "started on the ready counter, not the timeout"
    );
    let started = clock.now();
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Ending)
    }));
    assert!(clock.now().saturating_since(started) >= Duration::from_millis(100));
    assert!(run_until(&mut studio, &clock, |s| {
        // `service` reports the end once; the host settles.
        s.transition_state(canvas) == Some(TransitionState::Ending)
            && s.transition_wait(clock.now()) == Some(Duration::ZERO)
    }));
    assert!(studio.take_transition_done());
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(studio.transition_state(canvas), None, "settled");
    assert_eq!(studio.transition_wait(clock.now()), None);

    // Back with a Cut: no transition at all.
    let mut t = col.transition();
    t.kind = TransitionKind::Cut;
    col.set_transition(t).unwrap();
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.transition_state(canvas), None);
    let t1 = ticks(&studio);
    assert!(run_until(&mut studio, &clock, |s| ticks(s) >= t1 + 3));
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_change_during_a_fade_starts_a_new_one_and_ending_drops_it() {
    use rbs_record::TransitionState;
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    let mut fade = CollectionTransition {
        kind: TransitionKind::Fade,
        ..CollectionTransition::default()
    };
    fade.fade.duration_ms = 1000;
    col.set_transition(fade).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    // An output, so the program canvas is drawn.
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition", ContainerId::Matroska),
        )
        .unwrap();
    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));

    // Decision 9: a change while one runs starts a new one from what is on air.
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );

    // Leaving studio mode or switching the collection ends it at once.
    studio.end_transitions();
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(studio.transition_state(canvas), None);
    let ticks = |s: &Studio| s.engine().worker_stats(RENDER).unwrap().ticks();
    let t1 = ticks(&studio);
    assert!(run_until(&mut studio, &clock, |s| ticks(s) >= t1 + 3));
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn noise_suppression_latency_is_compensated_without_touching_the_stored_offset() {
    // ADR-0130 decision 4: the input gets `stored - L`; the stored offset stays the
    // device's value; disabling the filter takes the compensation away.
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    const MS: i64 = 1_000_000;
    let (ns, _) = col
        .add_filter(
            mic,
            NodeTypeId::new_static(rbs_audio_filters::noise::NOISE_SUPPRESSION),
            Settings::new(),
        )
        .unwrap();
    let mut settings = StudioSettings::new(canvas);
    settings.audio_offsets.insert(mic, 20 * MS);
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    assert!(
        studio.filter_problems().is_empty(),
        "{:?}",
        studio.filter_problems()
    );
    assert!(run_until(&mut studio, &clock, |s| {
        s.progress().master_peak_db.is_some_and(f32::is_finite)
    }));
    let latency = studio.audio_latency(mic).unwrap();
    assert_eq!(latency.filter_latency_ns, 10 * MS);
    assert_eq!(latency.heard_offset_ns, 20 * MS);
    assert_eq!(latency.uncompensated_ns, 0);
    assert_eq!(studio.settings().audio_offsets.get(&mic), Some(&(20 * MS)));
    // A source without filters is unchanged.
    assert_eq!(
        studio.audio_latency(display).map(|l| l.filter_latency_ns),
        Some(0)
    );

    // A new stored offset keeps the compensation.
    assert!(studio.set_audio_offset(mic, -30 * MS));
    let latency = studio.audio_latency(mic).unwrap();
    // Default graph: 35 ms of advance; -30 - 10 is cut to -35, so 5 ms are left.
    assert_eq!(latency.heard_offset_ns, -25 * MS);
    assert_eq!(latency.uncompensated_ns, 5 * MS);
    assert_eq!(studio.settings().audio_offsets.get(&mic), Some(&(-30 * MS)));

    // Disabled: Standby, no latency, the stored offset alone.
    col.set_filter_enabled(mic, ns, false).unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    let latency = studio.audio_latency(mic).unwrap();
    assert_eq!(latency.filter_latency_ns, 0);
    assert_eq!(latency.heard_offset_ns, -30 * MS);
    assert_eq!(studio.settings().audio_offsets.get(&mic), Some(&(-30 * MS)));
    assert!(run_until(&mut studio, &clock, |s| {
        s.progress().master_peak_db.is_some_and(f32::is_finite)
    }));
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn during_a_fade_the_sound_follows_the_picture() {
    // ADR-0128 decision 10: a source only in the outgoing scene ramps to silence over
    // the transition, not in the mixer's default 10 ms.
    use rbs_record::TransitionState;
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let other = camera_scene(&mut col);
    let mut fade = CollectionTransition {
        kind: TransitionKind::Fade,
        ..CollectionTransition::default()
    };
    fade.fade.duration_ms = 1000;
    col.set_transition(fade).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition-sound", ContainerId::Matroska),
        )
        .unwrap();
    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let peak = |s: &Studio| s.meter(mic).unwrap().take().peak(0);
    let window = |studio: &mut Studio| {
        let _ = peak(studio);
        let t = blocks(studio);
        assert!(run_until(studio, &clock, |s| blocks(s) >= t + 5));
        peak(studio)
    };
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    let full = window(&mut studio);
    assert!(full > 0.0);

    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    // Waiting: the outgoing level holds.
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));
    let start = clock.now();
    // A quarter of the way in, the microphone is between full and silent.
    assert!(run_until(&mut studio, &clock, |_| {
        clock.now().saturating_since(start) >= Duration::from_millis(250)
    }));
    let middle = window(&mut studio);
    assert!(middle > 0.0 && middle < full * 0.9, "{middle} of {full}");
    // After the transition it is silent and leaves at the settle.
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Ending)
    }));
    // Audio blocks trail the clock by the audio latency; look past it.
    assert!(run_until(&mut studio, &clock, |_| {
        clock.now().saturating_since(start) >= Duration::from_millis(1100)
    }));
    assert_eq!(window(&mut studio), 0.0);
    assert!(run_until(&mut studio, &clock, |s| s
        .transition_wait(clock.now())
        == Some(Duration::ZERO)));
    assert!(studio.take_transition_done());
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(studio.transition_state(canvas), None);
    assert_eq!(studio.mixer_anomalies(), 0);
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn transitions_report_their_start_and_end_and_take_a_one_off_kind() {
    // ADR-0128 decision 13.
    use rbs_record::{TransitionOnce, TransitionPhase, TransitionState};
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Cut,
        ..CollectionTransition::default()
    })
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
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition-events", ContainerId::Matroska),
        )
        .unwrap();
    let phases = |s: &mut Studio| -> Vec<(TransitionPhase, TransitionKind)> {
        s.take_transition_events()
            .into_iter()
            .map(|e| (e.phase, e.kind))
            .collect()
    };

    // A Cut starts and ends at once.
    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let events = studio.take_transition_events();
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[0].phase, TransitionPhase::Started);
    assert_eq!(events[1].phase, TransitionPhase::Ended);
    assert_eq!(events[0].from, Some(live));
    assert_eq!(events[0].to, Some(other));
    assert_eq!(studio.transition_state(canvas), None);

    // A one-off Fade of 100 ms over the collection's Cut: started when it runs, ended
    // after its duration; the next change is a Cut again.
    studio.set_transition_once(TransitionOnce::new(Some(TransitionKind::Fade), Some(100)));
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );
    assert!(phases(&mut studio).is_empty(), "nothing while it waits");
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));
    assert_eq!(
        phases(&mut studio),
        [(TransitionPhase::Started, TransitionKind::Fade)]
    );
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Ending)
    }));
    assert_eq!(
        phases(&mut studio),
        [(TransitionPhase::Ended, TransitionKind::Fade)]
    );
    assert!(run_until(&mut studio, &clock, |s| s
        .transition_wait(clock.now())
        == Some(Duration::ZERO)));
    assert!(studio.take_transition_done());
    studio.settle_transitions(&r.adapters.sources);
    assert!(phases(&mut studio).is_empty());

    // A one-off Move of 200 ms ends on the service after its glide, not at once.
    studio.set_transition_once(TransitionOnce::new(Some(TransitionKind::Move), Some(200)));
    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let started = clock.now();
    assert_eq!(
        phases(&mut studio),
        [(TransitionPhase::Started, TransitionKind::Move)]
    );
    assert_eq!(
        studio.transition_wait(clock.now()),
        Some(Duration::from_millis(200))
    );
    let mut ended = Vec::new();
    assert!(run_until(&mut studio, &clock, |_| {
        clock.now().saturating_since(started) >= Duration::from_millis(250)
    }));
    ended.extend(phases(&mut studio));
    assert_eq!(ended, [(TransitionPhase::Ended, TransitionKind::Move)]);
    // The display leaves, so its fade-out kept the outgoing scene: settle it.
    assert!(run_until(&mut studio, &clock, |s| s
        .transition_wait(clock.now())
        == Some(Duration::ZERO)));
    assert!(studio.take_transition_done());
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(studio.transition_wait(clock.now()), None, "nothing armed");

    // The one-off was used once: back to the collection's Cut.
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(
        phases(&mut studio),
        [
            (TransitionPhase::Started, TransitionKind::Cut),
            (TransitionPhase::Ended, TransitionKind::Cut)
        ]
    );
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_move_fades_out_what_leaves_and_needs_nothing_when_nothing_leaves() {
    // ADR-0128 decision 5: a Move runs at once, without an incoming compositor, only
    // when the outgoing scene draws a source the incoming one does not.
    use rbs_record::{TransitionPhase, TransitionState};
    use rbs_scene::{CollectionGlide, CollectionTransition, GlideEasing, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Move,
        ..CollectionTransition::default()
    })
    .unwrap();
    col.set_glide(CollectionGlide {
        duration_ms: 200,
        easing: GlideEasing::EaseInOut,
    })
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
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition-move", ContainerId::Matroska),
        )
        .unwrap();
    let ticks = |s: &Studio| s.engine().worker_stats(RENDER).unwrap().ticks();
    let t0 = ticks(&studio);
    assert!(run_until(&mut studio, &clock, |s| ticks(s) >= t0 + 3));

    // The display, the microphone and the rest leave: it runs at once.
    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Running),
        "no incoming picture to wait for"
    );
    let events = studio.take_transition_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].phase, TransitionPhase::Started);
    assert_eq!(events[0].kind, TransitionKind::Move);
    assert_eq!(events[0].duration, Duration::from_millis(200));
    assert_eq!(
        studio.transition_wait(clock.now()),
        Some(Duration::from_millis(200))
    );
    let t1 = ticks(&studio);
    assert!(run_until(&mut studio, &clock, |s| ticks(s) >= t1 + 3));
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Running)
    );
    assert!(run_until(&mut studio, &clock, |s| s
        .transition_wait(clock.now())
        == Some(Duration::ZERO)));
    assert!(studio.take_transition_done());
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(studio.transition_state(canvas), None, "settled");

    // Back: everything of the camera scene is in the live scene, nothing leaves.
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.transition_state(canvas), None);
    let t2 = ticks(&studio);
    assert!(run_until(&mut studio, &clock, |s| ticks(s) >= t2 + 3));
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_fade_through_a_colour_a_swipe_and_a_slide_run_and_settle() {
    // ADR-0128 decision 5: each draws for its duration and settles like a Fade.
    use rbs_record::{TransitionPhase, TransitionState};
    use rbs_scene::{CollectionTransition, SwipeDirection, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition-kinds", ContainerId::Matroska),
        )
        .unwrap();
    let mut via = CollectionTransition {
        kind: TransitionKind::Fade,
        ..CollectionTransition::default()
    };
    via.fade.duration_ms = 100;
    via.fade.via = Some([0, 0, 0]);
    let mut swipe = CollectionTransition {
        kind: TransitionKind::Swipe,
        ..CollectionTransition::default()
    };
    swipe.swipe.duration_ms = 100;
    swipe.swipe.direction = SwipeDirection::Up;
    let mut slide = CollectionTransition {
        kind: TransitionKind::Slide,
        ..CollectionTransition::default()
    };
    slide.slide.duration_ms = 100;
    slide.slide.direction = SwipeDirection::Right;
    let mut to = other;
    for (transition, kind) in [
        (via, TransitionKind::Fade),
        (swipe, TransitionKind::Swipe),
        (slide, TransitionKind::Slide),
    ] {
        col.set_transition(transition).unwrap();
        col.set_program(canvas, Some(to)).unwrap();
        studio.update(&col, &r.adapters.sources).unwrap();
        assert!(run_until(&mut studio, &clock, |s| {
            s.transition_state(canvas) == Some(TransitionState::Running)
        }));
        let started = clock.now();
        assert!(run_until(&mut studio, &clock, |s| s
            .transition_wait(clock.now())
            == Some(Duration::ZERO)
            && s.transition_state(canvas) == Some(TransitionState::Ending)));
        assert!(clock.now().saturating_since(started) >= Duration::from_millis(100));
        let events = studio.take_transition_events();
        assert_eq!(
            events.iter().map(|e| (e.phase, e.kind)).collect::<Vec<_>>(),
            [
                (TransitionPhase::Started, kind),
                (TransitionPhase::Ended, kind)
            ]
        );
        assert!(studio.take_transition_done());
        studio.settle_transitions(&r.adapters.sources);
        assert_eq!(studio.transition_state(canvas), None);
        to = if to == other { live } else { other };
    }
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn the_incoming_scenes_override_comes_between_a_one_off_and_the_collection() {
    // ADR-0135 decision 8.
    use rbs_record::{TransitionOnce, TransitionPhase};
    use rbs_scene::{CollectionTransition, SceneTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Cut,
        ..CollectionTransition::default()
    })
    .unwrap();
    col.set_scene_transition(
        other,
        Some(SceneTransition {
            kind: Some(TransitionKind::Fade),
            duration_ms: Some(120),
        }),
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
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://transition-override", ContainerId::Matroska),
        )
        .unwrap();
    let run = |studio: &mut Studio, col: &mut rbs_scene::SceneCollection, to| {
        col.set_program(canvas, Some(to)).unwrap();
        studio.update(col, &r.adapters.sources).unwrap();
        let mut events = Vec::new();
        assert!(run_until(studio, &clock, |s| {
            s.transition_state(canvas).is_none()
                || s.transition_wait(clock.now()) == Some(Duration::ZERO)
        }));
        if studio.take_transition_done() {
            studio.settle_transitions(&r.adapters.sources);
        }
        events.extend(studio.take_transition_events());
        events
            .into_iter()
            .filter(|e| e.phase == TransitionPhase::Ended)
            .map(|e| (e.kind, e.duration))
            .collect::<Vec<_>>()
    };
    // Into the scene: its override.
    assert_eq!(
        run(&mut studio, &mut col, other),
        [(TransitionKind::Fade, Duration::from_millis(120))]
    );
    // Out of it: the collection's Cut.
    assert_eq!(
        run(&mut studio, &mut col, live),
        [(TransitionKind::Cut, Duration::ZERO)]
    );
    // A one-off kind wins, with that kind's collection duration (Swipe, 500 ms).
    studio.set_transition_once(TransitionOnce::new(Some(TransitionKind::Swipe), None));
    assert_eq!(
        run(&mut studio, &mut col, other),
        [(TransitionKind::Swipe, Duration::from_millis(500))]
    );
    // A one-off duration alone keeps the override's kind.
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    let _ = studio.take_transition_events();
    studio.set_transition_once(TransitionOnce::new(None, Some(60)));
    assert_eq!(
        run(&mut studio, &mut col, other),
        [(TransitionKind::Fade, Duration::from_millis(60))]
    );
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_browser_page_is_heard_in_the_program_mix() {
    // ADR-0106, ADR-0109 decision 6: the page's sound reaches the sound node through
    // the feed, and the sound node is the producer of the page's strip.
    use std::sync::{Arc, Mutex};

    use rbs_browser_ipc::Configure;
    use rbs_scene::model::SettingValue;
    use rbs_sources::types::keys;
    use rbs_sources::{
        BrowserFrameSink, BrowserSession, BrowserSound, BrowserSoundFeed, BrowserTree,
    };

    let Some(gpu) = gpu() else {
        return;
    };
    let r = rig();
    let (mut col, canvas) = scene();
    let scene = col.scenes().next().unwrap().id;
    // The page is the only sound: what the master meter shows is the page.
    for name in ["Microphone", "System audio"] {
        let id = col.sources().find(|s| s.name == name).unwrap().id;
        col.delete_source(id).unwrap();
    }
    let mut page_settings = Settings::new();
    page_settings.insert(
        keys::URL.to_owned(),
        SettingValue::Text("https://example.com/tone".to_owned()),
    );
    page_settings.insert(keys::WIDTH.to_owned(), SettingValue::Int(64));
    page_settings.insert(keys::HEIGHT.to_owned(), SettingValue::Int(36));
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            page_settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(page),
        Layout::new([0.0, 0.0], [W as f32, H as f32]),
        None,
    )
    .unwrap();

    /// The OS clock of the test page runs this far ahead of master time.
    const OS: u64 = 5_000_000_000_000;

    /// A page that plays a 0.5 square wave, written on each control tick for the
    /// master time since the last one, in 10 ms packets.
    struct TonePage {
        clock: rbs_core::time::TestClock,
        feed: Option<BrowserSoundFeed>,
        made_until: u64,
    }

    impl BrowserSession for TonePage {
        fn poll(&mut self) -> Option<i32> {
            let now = self.clock.now().as_nanos();
            let feed = self.feed.as_mut()?;
            feed.observe(OS + now);
            if self.made_until == 0 {
                self.made_until = now;
            }
            while self.made_until + 10_000_000 <= now {
                let samples: Vec<f32> = (0..480)
                    .flat_map(|f| {
                        let v = if (f / 24) % 2 == 0 { 0.5 } else { -0.5 };
                        [v, v]
                    })
                    .collect();
                feed.packet(OS + self.made_until, 48_000, 2, &samples);
                self.made_until += 10_000_000;
            }
            feed.pump();
            None
        }
        fn stop(&mut self) {}
    }

    struct ToneTree(rbs_core::time::TestClock, Arc<Mutex<Option<u32>>>);

    impl BrowserTree for ToneTree {
        fn start(
            &self,
            config: &Configure,
            _sink: Arc<BrowserFrameSink>,
            sound: Option<Arc<BrowserSound>>,
        ) -> Result<Box<dyn BrowserSession>, String> {
            *self.1.lock().unwrap() = Some(config.sample_rate);
            Ok(Box::new(TonePage {
                clock: self.0.clone(),
                feed: sound.map(BrowserSoundFeed::new),
                made_until: 0,
            }))
        }
    }

    let (engine, clock) = engine_at_start();
    let asked = Arc::new(Mutex::new(None));
    let tree: Arc<dyn BrowserTree> = Arc::new(ToneTree(clock.clone(), Arc::clone(&asked)));
    let mut studio = Studio::start_with_tree(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
        Some(tree),
    )
    .unwrap();
    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://tone", ContainerId::Matroska),
        )
        .unwrap();
    // No level meter on picture-and-sound strips yet; the master hears only the page.
    assert!(studio.meter(page).is_none());
    let master = Arc::clone(studio.master_meter().expect("a program mixer"));
    let heard = run_until(&mut studio, &clock, |_| master.observe().peak(0) > 0.4);
    assert!(heard, "{:?}", studio.progress());
    // The helper was asked for the graph's rate.
    assert_eq!(*asked.lock().unwrap(), Some(48_000));
    assert_eq!(studio.mixer_anomalies(), 0);
    let _ = studio.stop_output(id);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_t_bar_holds_the_blend_then_takes_or_cancels() {
    use rbs_record::{ManualStep, TransitionPhase, TransitionState};
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Fade,
        ..CollectionTransition::default()
    })
    .unwrap();
    // Studio mode: the camera scene in preview.
    col.set_preview(canvas, Some(other)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://t-bar", ContainerId::Matroska),
        )
        .unwrap();
    let src = &r.adapters.sources;
    // 0 with nothing held does nothing.
    assert_eq!(
        studio.transition_progress(canvas, 0.0, src).unwrap(),
        ManualStep::Idle
    );
    assert_eq!(studio.transition_state(canvas), None);

    // The first value above 0 starts it; it waits for the picture, then holds.
    assert_eq!(
        studio.transition_progress(canvas, 0.4, src).unwrap(),
        ManualStep::Holding
    );
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );
    assert!(studio.manual_transition(canvas));
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));
    let events = studio.take_transition_events();
    let phases: Vec<_> = events
        .iter()
        .map(|e| (e.phase, e.manual, e.progress))
        .collect();
    assert_eq!(
        phases,
        [
            (TransitionPhase::Started, true, None),
            (TransitionPhase::Holding, true, Some(0.4)),
        ]
    );
    assert_eq!(events[0].duration, Duration::ZERO);
    assert_eq!((events[0].from, events[0].to), (Some(live), Some(other)));
    // Held, nothing is armed (P13) and the program has not changed.
    assert_eq!(studio.transition_wait(clock.now()), None);
    assert_eq!(
        studio.transition_progress(canvas, 0.6, src).unwrap(),
        ManualStep::Holding
    );
    assert_eq!(
        studio.transition_progress(canvas, 0.7, src).unwrap(),
        ManualStep::Holding
    );
    let held: Vec<_> = studio
        .take_transition_events()
        .iter()
        .map(|e| (e.phase, e.progress))
        .collect();
    assert_eq!(
        held,
        [(TransitionPhase::Holding, Some(0.7))],
        "the latest value only"
    );
    let t0 = studio.engine().worker_stats(RENDER).unwrap().ticks();
    assert!(run_until(&mut studio, &clock, |s| {
        s.engine().worker_stats(RENDER).unwrap().ticks() >= t0 + 3
    }));

    // 1 takes: the host changes the program, which ends it without a timed one.
    assert_eq!(
        studio.transition_progress(canvas, 1.0, src).unwrap(),
        ManualStep::Take
    );
    col.set_program(canvas, Some(other)).unwrap();
    col.set_preview(canvas, Some(live)).unwrap();
    studio.update(&col, src).unwrap();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Ending)
    );
    let ended = studio.take_transition_events();
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].phase, TransitionPhase::Ended);
    assert!(ended[0].manual);
    assert_eq!(ended[0].to, Some(other));
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_wait(clock.now()) == Some(Duration::ZERO)
    }));
    assert!(studio.take_transition_done());
    studio.settle_transitions(src);
    assert_eq!(studio.transition_state(canvas), None);

    // Back to 0 cancels: `ended` with `to` equal to `from`, and the program stays.
    assert_eq!(
        studio.transition_progress(canvas, 0.5, src).unwrap(),
        ManualStep::Holding
    );
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));
    studio.take_transition_events();
    assert_eq!(
        studio.transition_progress(canvas, 0.0, src).unwrap(),
        ManualStep::Idle
    );
    assert!(!studio.manual_transition(canvas));
    let cancelled = studio.take_transition_events();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(
        (cancelled[0].from, cancelled[0].to),
        (Some(other), Some(other))
    );
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_wait(clock.now()) == Some(Duration::ZERO)
    }));
    assert!(studio.take_transition_done());
    studio.settle_transitions(src);
    assert_eq!(studio.transition_state(canvas), None);
    assert_eq!(col.canvas(canvas).unwrap().program, Some(other));
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// Rehearse (ADR-0135 decisions 11 to 13): the transition plays on the Preview picture
/// only, the program never changes, and it ends after its time, when stopped, or when
/// the preview scene changes; Cut has nothing to rehearse. The preview picture runs on
/// Windows and macOS only.
#[cfg(any(windows, target_vendor = "apple"))]
#[test]
fn a_rehearsal_plays_on_the_preview_picture_only_and_ends() {
    use rbs_gpu::share::PreviewDesc;
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let src = &r.adapters.sources;
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    let fade = |ms: u32| {
        let mut t = CollectionTransition {
            kind: TransitionKind::Fade,
            ..CollectionTransition::default()
        };
        t.fade.duration_ms = ms;
        t
    };
    col.set_transition(fade(5000)).unwrap();
    col.set_preview(canvas, Some(other)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    settings.preview = Some(PreviewDesc {
        width: 32,
        height: 18,
        surfaces: 3,
    });
    settings.scene_preview = true;
    let mut studio = Studio::start(engine, gpu, &col, src, &settings).unwrap();
    assert!(studio.studio_mode());
    // Render ticks: `run_until` fails on any node failure, so a picture whose input
    // was relinked wrong shows here.
    let more = |studio: &Studio, n: u64| {
        let at = studio.engine().worker_stats(RENDER).unwrap().ticks();
        move |s: &Studio| s.engine().worker_stats(RENDER).unwrap().ticks() >= at + n
    };

    // It plays on the Preview picture, which keeps running; nothing else moves.
    assert!(studio.rehearse(src).unwrap());
    assert!(studio.rehearsing());
    assert!(!studio.rehearse(src).unwrap(), "one at a time");
    let ticks = more(&studio, 10);
    assert!(run_until(&mut studio, &clock, ticks));
    assert!(studio.rehearsing());
    assert_eq!(studio.transition_state(canvas), None);
    assert!(studio.take_transition_events().is_empty());
    assert_eq!(col.canvas(canvas).unwrap().program, Some(live));

    // Stopped: the picture shows the preview scene again.
    studio.end_rehearsal(src).unwrap();
    assert!(!studio.rehearsing());
    let ticks = more(&studio, 10);
    assert!(run_until(&mut studio, &clock, ticks));

    // A short one ends after its time.
    col.set_transition(fade(300)).unwrap();
    studio.update(&col, src).unwrap();
    assert!(studio.rehearse(src).unwrap());
    let wait = studio.rehearsal_wait(clock.now()).unwrap();
    assert!(wait <= Duration::from_millis(300), "{wait:?}");
    assert!(run_until(&mut studio, &clock, Studio::rehearsal_done));
    studio.end_rehearsal(src).unwrap();
    assert!(!studio.rehearsing());
    assert_eq!(studio.rehearsal_wait(clock.now()), None);

    // A change of the preview scene ends it at once.
    assert!(studio.rehearse(src).unwrap());
    col.set_preview(canvas, Some(live)).unwrap();
    studio.update(&col, src).unwrap();
    assert!(!studio.rehearsing());
    let ticks = more(&studio, 10);
    assert!(run_until(&mut studio, &clock, ticks));

    // Cut has nothing to rehearse.
    col.set_preview(canvas, Some(other)).unwrap();
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Cut,
        ..CollectionTransition::default()
    })
    .unwrap();
    studio.update(&col, src).unwrap();
    assert!(!studio.rehearse(src).unwrap());

    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// Without a Preview picture (no UI, or outside studio mode) there is nothing to
/// rehearse on, and nothing is added (ADR-0135 decision 12).
#[test]
fn without_a_preview_picture_nothing_is_rehearsed() {
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let src = &r.adapters.sources;
    let (mut col, canvas) = scene();
    let other = camera_scene(&mut col);
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Fade,
        ..CollectionTransition::default()
    })
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let mut settings = StudioSettings::new(canvas);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, src, &settings).unwrap();
    assert!(!studio.rehearse(src).unwrap(), "outside studio mode");
    col.set_preview(canvas, Some(other)).unwrap();
    studio.update(&col, src).unwrap();
    assert!(!studio.rehearse(src).unwrap(), "no picture for a UI");
    assert!(!studio.rehearsing());
    assert_eq!(studio.rehearsal_wait(Timestamp::ZERO), None);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// A Stinger change (ADR-0140 decisions 4 to 8): the stinger is armed before the
/// apply that makes it Active, the change waits for its first picture and the
/// incoming picture, plays it for the play range's length, and gives the source its
/// keys back at the end. Without what the host knows of the file it is a Cut.
#[test]
fn a_stinger_waits_for_its_first_picture_plays_its_length_and_disarms() {
    use std::sync::Arc;

    use rbs_media_decode_test::{ScriptClip, ScriptFactory};
    use rbs_record::{StingerFacts, TransitionState};
    use rbs_scene::model::SettingValue;
    use rbs_scene::{StingerSettings, TransitionKind};
    use rbs_sources::types::keys;

    let Some(gpu) = gpu() else { return };
    let mut r = rig();
    r.adapters
        .sources
        .register_media(Arc::new(
            ScriptFactory::new(ScriptClip::silent_frames()).with_alpha(true),
        ))
        .unwrap();
    r.adapters.probe();
    let dir = std::env::temp_dir().join(format!(
        "rbs-record-stinger-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("stinger.mov");
    std::fs::write(&file, b"not-decoded").unwrap();
    struct RemoveDir(std::path::PathBuf);
    impl Drop for RemoveDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = RemoveDir(dir);

    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    let mut settings = Settings::new();
    settings.insert(
        keys::PATH.to_owned(),
        SettingValue::Text(file.to_string_lossy().into_owned()),
    );
    let (sting, _) = col
        .add_source(
            "Stinger",
            NodeTypeId::new_static(types::MEDIA_SOURCE),
            settings,
        )
        .unwrap();
    let mut t = col.transition();
    t.kind = TransitionKind::Stinger;
    t.stinger = StingerSettings {
        source: Some(sting),
        cut_ms: None,
    };
    col.set_transition(t).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://stinger", ContainerId::Matroska),
        )
        .unwrap();
    assert!(
        studio.media_control(sting).is_none(),
        "Standby while unused (P13)"
    );

    // Nothing known of the file yet: the change is a Cut.
    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.transition_state(canvas), None);
    assert!(!studio.stinger_armed(sting));

    let length = Duration::from_millis(300);
    studio.set_stinger(Some(StingerFacts {
        source: sting,
        length: Some(length),
        found_cut: Some(Duration::from_millis(100)),
        size: Some((2, 2)),
        alpha_unsupported: false,
    }));
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(
        studio.transition_state(canvas),
        Some(TransitionState::Waiting)
    );
    assert!(studio.stinger_armed(sting), "armed before the apply");
    let control = studio.media_control(sting).cloned().unwrap();
    assert!(control.is_armed());
    let waited = clock.now();
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Running)
    }));
    assert!(
        clock.now().saturating_since(waited) < rbs_record::STINGER_WAIT,
        "started on both pictures, not the timeout"
    );
    assert_eq!(studio.stinger_late_cuts(), 0);
    let started = clock.now();
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Ending)
    }));
    assert!(clock.now().saturating_since(started) >= length);
    assert!(run_until(&mut studio, &clock, |s| {
        s.transition_state(canvas) == Some(TransitionState::Ending)
            && s.transition_wait(clock.now()) == Some(Duration::ZERO)
    }));
    assert!(studio.take_transition_done());
    studio.settle_transitions(&r.adapters.sources);
    assert_eq!(studio.transition_state(canvas), None, "settled");
    assert!(!studio.stinger_armed(sting));
    assert!(!control.is_armed(), "its own keys are back");

    // A file whose alpha is not kept here runs as a Cut.
    studio.set_stinger(Some(StingerFacts {
        source: sting,
        length: Some(length),
        found_cut: None,
        size: None,
        alpha_unsupported: true,
    }));
    col.set_program(canvas, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.transition_state(canvas), None);
    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// A browser source without its engine (add-ons spec "Browser engine consumer"):
/// Unavailable with the engine's words, no slot, no back-off; once the engine is
/// installed `browser_engine_changed` starts it at the next apply, with no restart.
#[test]
fn a_browser_source_waits_for_its_engine_and_starts_after_the_install() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    use rbs_browser_ipc::Configure;
    use rbs_core::media::video::Size;
    use rbs_scene::SceneCollection;
    use rbs_scene::model::{SceneScope, SettingValue};
    use rbs_sources::types::keys;
    use rbs_sources::{BrowserFrameSink, BrowserSession, BrowserTree};

    let Some(gpu) = gpu() else {
        return;
    };
    let r = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let mut settings = Settings::new();
    settings.insert(
        keys::URL.to_owned(),
        SettingValue::Text("https://example.com/".to_owned()),
    );
    settings.insert(keys::WIDTH.to_owned(), SettingValue::Int(64));
    settings.insert(keys::HEIGHT.to_owned(), SettingValue::Int(36));
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(page),
        Layout::new([0.0, 0.0], [W as f32, H as f32]),
        None,
    )
    .unwrap();
    col.set_program(canvas, Some(scene)).unwrap();

    struct Session;
    impl BrowserSession for Session {
        fn poll(&mut self) -> Option<i32> {
            None
        }
        fn stop(&mut self) {}
    }
    struct Tree {
        installed: AtomicBool,
        starts: AtomicU32,
    }
    impl BrowserTree for Tree {
        fn start(
            &self,
            _config: &Configure,
            _sink: Arc<BrowserFrameSink>,
            _sound: Option<Arc<rbs_sources::BrowserSound>>,
        ) -> Result<Box<dyn BrowserSession>, String> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(Session))
        }
        fn unavailable(&self) -> Option<&'static str> {
            (!self.installed.load(Ordering::Relaxed))
                .then_some(rbs_sources::BROWSER_RUNTIME_MISSING)
        }
    }
    let tree = Arc::new(Tree {
        installed: AtomicBool::new(false),
        starts: AtomicU32::new(0),
    });

    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start_with_tree(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
        Some(Arc::clone(&tree) as Arc<dyn BrowserTree>),
    )
    .unwrap();
    studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://page", ContainerId::Matroska),
        )
        .unwrap();
    let page_state = |s: &Studio| s.sources().into_iter().find(|st| st.source == page);
    let missing = run_until(&mut studio, &clock, |s| {
        page_state(s)
            .and_then(|st| st.failed)
            .is_some_and(|why| why.contains("browser engine is not installed"))
    });
    assert!(
        missing,
        "{:?} starts={}",
        page_state(&studio),
        tree.starts.load(Ordering::Relaxed)
    );
    assert_eq!(tree.starts.load(Ordering::Relaxed), 0, "nothing started");
    // No back-off: the node waits for the studio, not for a timer.
    assert!(run_until(&mut studio, &clock, |_| true));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 0);

    tree.installed.store(true, Ordering::Relaxed);
    assert_eq!(studio.browser_engine_changed(), 1);
    let up = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(up.failed.is_empty(), "{:?}", up.failed);
    let running = run_until(&mut studio, &clock, |s| {
        page_state(s).is_some_and(|st| st.failed.is_none())
    });
    assert!(running, "{:?}", page_state(&studio));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

/// A browser tree whose pages record the control messages they get (ADR-0149,
/// ADR-0152 tests).
mod fake_browser {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use rbs_browser_ipc::Configure;
    use rbs_browser_ipc::control::Control;
    use rbs_sources::{BrowserFrameSink, BrowserSession, BrowserTree};

    type Log = Arc<Mutex<Vec<Control>>>;

    struct Session {
        sent: Log,
        stops: Arc<AtomicU32>,
    }

    impl BrowserSession for Session {
        fn poll(&mut self) -> Option<i32> {
            None
        }
        fn stop(&mut self) {
            self.stops.fetch_add(1, Ordering::Relaxed);
        }
        fn send(&mut self, message: &Control) -> bool {
            self.sent.lock().unwrap().push(message.clone());
            true
        }
    }

    pub struct FakeTree {
        pub starts: AtomicU32,
        pub stops: Arc<AtomicU32>,
        pub sent: Log,
    }

    impl FakeTree {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: AtomicU32::new(0),
                stops: Arc::new(AtomicU32::new(0)),
                sent: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    impl BrowserTree for FakeTree {
        fn start(
            &self,
            _config: &Configure,
            _sink: Arc<BrowserFrameSink>,
            _sound: Option<Arc<rbs_sources::BrowserSound>>,
        ) -> Result<Box<dyn BrowserSession>, String> {
            self.starts.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(Session {
                sent: Arc::clone(&self.sent),
                stops: Arc::clone(&self.stops),
            }))
        }
    }
}

#[test]
fn a_background_page_runs_hidden_while_placed_and_stops_when_it_no_longer_is_one() {
    // ADR-0149 decision 5, ADR-0152 decisions 4 and 7: `run: placed` keeps the page's
    // helper running, hidden, while its picture node is not scheduled; shown, the same
    // helper paints at full rate; back to the type's default, it stops.
    use std::sync::Arc;

    use fake_browser::FakeTree;
    use rbs_browser_ipc::control::Control;
    use rbs_core::media::video::Size;
    use rbs_scene::SceneCollection;
    use rbs_scene::activity::{Run, SourceActivity};
    use rbs_scene::model::{SceneScope, SettingValue};
    use rbs_sources::BrowserTree;
    use rbs_sources::types::keys;

    let Some(gpu) = gpu() else {
        return;
    };
    let r = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (live, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shelf, _) = col.add_scene("Shelf", SceneScope::Shared).unwrap();
    let mut settings = Settings::new();
    settings.insert(
        keys::URL.to_owned(),
        SettingValue::Text("https://example.com/".to_owned()),
    );
    settings.insert(keys::WIDTH.to_owned(), SettingValue::Int(64));
    settings.insert(keys::HEIGHT.to_owned(), SettingValue::Int(36));
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            settings,
        )
        .unwrap();
    let full = Layout::new([0.0, 0.0], [W as f32, H as f32]);
    col.add_item(shelf, ItemContent::Source(page), full, None)
        .unwrap();
    col.set_source_activity(page, SourceActivity::new(Some(Run::Placed), None, None))
        .unwrap();
    col.set_program(canvas, Some(live)).unwrap();

    let tree = FakeTree::new();
    let sent = || tree.sent.lock().unwrap().clone();

    let (engine, _clock) = engine_at_start();
    let mut studio = Studio::start_with_tree(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
        Some(Arc::clone(&tree) as Arc<dyn BrowserTree>),
    )
    .unwrap();
    // Placed on a scene nothing shows: started by the studio, hidden.
    assert!(studio.browser_running(page));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    assert_eq!(sent(), vec![Control::Shown(false)]);

    // On the program: the same helper, shown.
    let item = col
        .add_item(live, ItemContent::Source(page), full, None)
        .unwrap()
        .0;
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.browser_running(page));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    assert_eq!(sent(), vec![Control::Shown(false), Control::Shown(true)]);

    // Off the program again: the node leaves the schedule, the helper stays, hidden.
    col.remove_item(item).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.browser_running(page));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    assert_eq!(tree.stops.load(Ordering::Relaxed), 0);
    assert_eq!(sent().last(), Some(&Control::Shown(false)));

    // Back to the type's default (`run: shown`): nothing shows it, so it stops.
    col.set_source_activity(page, SourceActivity::default())
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(!studio.browser_running(page));
    assert_eq!(tree.stops.load(Ordering::Relaxed), 1);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_page_with_restart_reloads_on_its_on_air_edge_and_a_waking_page_loads_once() {
    // ADR-0152 decisions 6 and 9: `on_return: restart` reloads a running page when
    // it goes on air from below, once per edge; a page that wakes on that edge is
    // started fresh and gets no extra reload.
    use std::sync::Arc;

    use fake_browser::FakeTree;
    use rbs_browser_ipc::control::Control;
    use rbs_core::media::video::Size;
    use rbs_scene::SceneCollection;
    use rbs_scene::activity::{OnReturn, Run, SourceActivity};
    use rbs_scene::model::{SceneScope, SettingValue};
    use rbs_sources::BrowserTree;
    use rbs_sources::types::keys;

    let Some(gpu) = gpu() else {
        return;
    };
    let r = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (live, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shelf, _) = col.add_scene("Shelf", SceneScope::Shared).unwrap();
    let page_settings = || {
        let mut settings = Settings::new();
        settings.insert(
            keys::URL.to_owned(),
            SettingValue::Text("https://example.com/".to_owned()),
        );
        settings.insert(keys::WIDTH.to_owned(), SettingValue::Int(64));
        settings.insert(keys::HEIGHT.to_owned(), SettingValue::Int(36));
        settings
    };
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            page_settings(),
        )
        .unwrap();
    let full = Layout::new([0.0, 0.0], [W as f32, H as f32]);
    col.add_item(shelf, ItemContent::Source(page), full, None)
        .unwrap();
    // A background page, so it runs before it goes on air.
    col.set_source_activity(
        page,
        SourceActivity::new(Some(Run::Placed), Some(OnReturn::Restart), None),
    )
    .unwrap();
    col.set_program(canvas, Some(live)).unwrap();

    let tree = FakeTree::new();
    let reloads = || {
        tree.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, Control::Reload { .. }))
            .count()
    };
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start_with_tree(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
        Some(Arc::clone(&tree) as Arc<dyn BrowserTree>),
    )
    .unwrap();
    assert!(studio.browser_running(page));
    assert_eq!(reloads(), 0);

    // On air: the running page reloads once.
    let item = col
        .add_item(live, ItemContent::Source(page), full, None)
        .unwrap()
        .0;
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(reloads(), 1);
    assert_eq!(
        tree.starts.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the same helper"
    );

    // A sync that keeps it on air is no edge.
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(reloads(), 1);

    // Off air and back: one more reload.
    col.remove_item(item).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(reloads(), 1);
    col.add_item(live, ItemContent::Source(page), full, None)
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(reloads(), 2);

    // A page at the type's `run: shown` that wakes as it goes on air loads once.
    let (fresh, _) = col
        .add_source(
            "Fresh",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            page_settings(),
        )
        .unwrap();
    col.set_source_activity(
        fresh,
        SourceActivity::new(None, Some(OnReturn::Restart), None),
    )
    .unwrap();
    col.add_item(live, ItemContent::Source(fresh), full, None)
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    // The engine prepares the woken node once an output draws the program.
    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://pages", ContainerId::Matroska),
        )
        .unwrap();
    let woke = run_until(&mut studio, &clock, |s| s.browser_running(fresh));
    assert!(woke, "{:?}", studio.progress());
    assert_eq!(reloads(), 2, "a waking page is not reloaded");
    let _ = studio.stop_output(id);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn the_same_picture_under_another_scene_id_starts_no_transition() {
    // ADR-0151 follow-up: the desktop app's held program copy taken or given back.
    use rbs_scene::{CollectionTransition, TransitionKind};

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let other = camera_scene(&mut col);
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Cut,
        ..CollectionTransition::default()
    })
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    col.set_program(canvas, Some(other)).unwrap();
    studio
        .update_same_picture(&col, &r.adapters.sources)
        .unwrap();
    assert!(studio.take_transition_events().is_empty());
    assert_eq!(studio.transition_state(canvas), None);

    // The next update notes changes again.
    col.set_program(canvas, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.take_transition_events().len(), 2, "a Cut again");
}
