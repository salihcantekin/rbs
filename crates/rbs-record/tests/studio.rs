//! The studio (spec `recording.md`, "Studio", ADR-0041): the program scene runs, the
//! mixer is metered while nothing records, recordings start and stop on the running
//! engine, and scene edits open and close sources while it runs. Reference adapters on
//! a `TestClock`, as in `record.rs`; tests wait for counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_core::media::{CodecId, ContainerId};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    AUDIO, AUDIO_ENCODER, OUTPUT, OutputSettings, RENDER, RecordError, Studio, StudioSettings,
    VIDEO_ENCODER,
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
        let started = studio.start_recording(&r.adapters, &out).unwrap().clone();
        assert_eq!(started.sources.len(), 4, "{started:?}");
        assert!(started.sources.iter().all(|s| s.failed.is_none()));
        assert!(studio.is_recording());
        assert!(matches!(
            studio.start_recording(&r.adapters, &out),
            Err(RecordError::AlreadyRecording)
        ));
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

        let report = studio.stop_recording().unwrap();
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
        assert!(!studio.is_recording());
        assert!(studio.stop_recording().is_none());

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
    assert!(report.is_none());
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
        .start_recording(
            &r.adapters,
            &OutputSettings::new("mem://x", ContainerId::Mp4Fragmented),
        )
        .unwrap_err();
    assert!(matches!(err, RecordError::NoAdapter { .. }), "{err}");
    assert_eq!(r.capture.live_buffers(), 0);

    // The destination cannot be opened: the output fails in `prepare`, the recording's
    // nodes are removed again and the studio runs on.
    let err = studio
        .start_recording(
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
    assert!(!studio.is_recording());
    assert_eq!(workers(&studio), [false, true, false, false, false]);
    assert_eq!(r.log.headers.load(Ordering::Relaxed), 0);

    // The next recording works.
    studio
        .start_recording(
            &r.adapters,
            &OutputSettings::new("mem://ok", ContainerId::Matroska),
        )
        .unwrap();
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= 4);
    assert!(reached, "{:?}", studio.progress());
    let report = studio.stop_recording().unwrap();
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
    studio
        .start_recording(
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

    // Removed again: closed, and the recording goes on.
    col.delete_source(mic).unwrap();
    col.delete_source(cam).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(studio.sources().len(), 4);
    assert_eq!(r.audio.active_streams(), 2);
    assert_eq!(r.capture.live_buffers(), buffers);
    assert!(studio.meter(mic).is_none());
    let before = r.log.count(0);
    let reached = run_until(&mut studio, &clock, |_| r.log.count(0) >= before + 3);
    assert!(reached);
    assert_eq!(studio.mixer_anomalies(), 0);
    let report = studio.stop_recording().unwrap();
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
