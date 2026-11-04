//! A whole recording on the reference adapters (spec `recording.md`, "Conformance
//! tests"): synthetic display, camera, microphone and loopback, a fake H.264 encoder,
//! the Media Foundation AAC session over its fake engine, and a muxer and transport
//! that log what they get. The engine runs on a `TestClock`; the test moves it and
//! wakes the workers, and waits for packet counts, never for a duration (ADR-0032).
//! Needs a GPU adapter (software is fine: lavapipe, WARP); skipped without one unless
//! `RBS_REQUIRE_GPU` is set.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_codec::{EncoderPreset, KeyframeInterval, RateControl};
use rbs_core::media::{CodecId, ContainerId, Size};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::{Cadence, SampleRate};
use rbs_graph::node::NodeTypeId;
use rbs_record::{AUDIO, Port, RecordError, RecordSettings, Recording};
use rbs_scene::SceneCollection;
use rbs_scene::model::{ItemContent, Layout, SceneScope, SettingValue, Settings};
use rbs_sources::types::keys;
use rbs_transport::{ReconnectPolicy, Secret, TransportInput};
use support::*;

#[test]
fn a_scene_records_video_and_audio_in_sync_on_the_master_clock() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let log = &r.log;
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://recording", ContainerId::Matroska);
    let mut recording = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap();

    let started = recording.started().clone();
    assert_eq!(started.video_encoder.adapter.as_str(), "org.rbs.test.h264");
    assert_eq!(
        started.audio_encoder.as_ref().unwrap().adapter.as_str(),
        "org.rbs.codec.mf-aac.fake"
    );
    let names: Vec<&str> = started.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names.len(), 4, "{names:?}");
    assert!(
        started.sources.iter().all(|s| s.failed.is_none()),
        "{started:?}"
    );
    assert!(started.problems.is_empty(), "{started:?}");

    let reached = run_until(&mut recording, &clock, |_| {
        log.count(0) >= 12 && log.count(1) >= 20
    });
    assert!(reached, "packets: {:?}", recording.progress());
    let end = clock.now();
    let report = recording.finish();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.failure, None);
    let p = &report.progress;
    assert_eq!(p.mux_drops, 0);
    assert_eq!(p.audio_queue_drops, 0);
    assert_eq!(p.audio_packet_drops, 0);
    assert_eq!(p.late_audio_ticks, 0);
    assert_eq!(p.late_render_ticks, 0);
    for s in &p.sources {
        assert!(s.frames > 0, "{} delivered nothing: {report:?}", s.name);
    }
    // Every exported frame is accounted for: encoded, skipped on the way to the
    // encoder, or dropped there (#110). The queue's counters survive the stop.
    assert!(p.frames_encoded > 0, "{report:?}");
    assert_eq!(
        p.frames_exported,
        p.frames_encoded + p.encoder_skipped + p.encoder_drops,
        "{report:?}"
    );

    // Streams: video then audio, as the output declared them.
    let streams = log.streams.lock().unwrap().clone();
    assert_eq!(streams.len(), 2);
    assert_eq!(streams[0].codec, CodecId::H264);
    assert_eq!(streams[1].codec, CodecId::Aac);
    assert_eq!(log.headers.load(Ordering::Relaxed), 1);
    assert_eq!(log.trailers.load(Ordering::Relaxed), 1);
    assert_eq!(log.finished.load(Ordering::Relaxed), 1);
    assert!(
        log.packets
            .lock()
            .unwrap()
            .iter()
            .all(|p| (p.stream == 0) == (p.codec == CodecId::H264))
    );
    let video = pts_of(log, 0);
    let audio = pts_of(log, 1);

    // Video: master time on the canvas cadence (origin 0), strictly increasing, from
    // the first render tick after the start on (the first frame needs a capture and a
    // readback, a few ticks).
    let frames = Cadence::frames(Timestamp::ZERO, FPS);
    for pair in video.windows(2) {
        assert!(pair[0] < pair[1], "video pts not increasing: {pair:?}");
    }
    for t in &video {
        let i = frames.index_at_or_before(*t).unwrap();
        assert_eq!(frames.time_of(i), *t, "video pts off the canvas cadence");
    }
    let first_tick = frames.time_of(due(&frames, Duration::ZERO, START) - 1);
    assert!(
        video[0] >= first_tick && video[0] <= first_tick + FPS.frame_duration() * 4,
        "first video pts {:?}, first render tick {first_tick:?}",
        video[0]
    );

    // Audio: the output starts at the first video packet (aligned start, io-nodes
    // spec), so the first audio packet is the one that spans the first video pts.
    let aac = SampleRate::HZ_48000.duration_of(1024);
    assert!(
        audio[0].saturating_since(video[0]) < aac && video[0].saturating_since(audio[0]) < aac,
        "first audio pts {:?} does not span the first video pts {:?}",
        audio[0],
        video[0]
    );
    for pair in audio.windows(2) {
        let step = pair[1].saturating_since(pair[0]);
        assert!(
            step.abs_diff(aac) <= Duration::from_micros(1),
            "audio gap or overlap: {pair:?}"
        );
    }
    assert!(*video.last().unwrap() <= end && *audio.last().unwrap() <= end);
    assert!(r.submitted.load(Ordering::Relaxed) >= 12);
}

#[test]
fn without_audio_only_video_is_recorded_and_audio_sources_stay_closed() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut settings = RecordSettings::new(canvas, "mem://video", ContainerId::Matroska);
    settings.audio = None;
    let mut recording = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap();
    let started = recording.started().clone();
    let names: Vec<&str> = started.sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["Display", "Camera"]);
    assert_eq!(started.audio_encoder, None);
    assert_eq!(r.audio.active_streams(), 0, "no audio stream opened");
    assert!(!recording.engine().has_worker(AUDIO));
    let reached = run_until(&mut recording, &clock, |_| r.log.count(0) >= 5);
    assert!(reached, "{:?}", recording.progress());
    let report = recording.finish();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(r.log.streams.lock().unwrap().len(), 1);
    assert_eq!(r.log.count(1), 0);
}

#[test]
fn a_stream_sends_packets_without_a_muxer_with_its_secret_and_reconnect_policy() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut settings = RecordSettings::packets(canvas, "stream://live/app");
    settings.secret = Some(Secret::new("key-123"));
    let reconnect = ReconnectPolicy::Backoff {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(8),
        attempts: None,
    };
    settings.reconnect = reconnect;
    settings.transport_options = vec![("latency_ms".into(), "200".into())];
    let mut recording = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap();
    let started = recording.started().clone();
    assert_eq!(started.muxer, None, "no muxer for packet input");
    assert_eq!(
        started.transport.adapter.as_str(),
        "org.rbs.test.packet-transport"
    );
    assert!(
        !format!("{started:?}{settings:?}").contains("key-123"),
        "the secret is never in Debug output"
    );
    let config = r.log.packet_config.lock().unwrap().clone().unwrap();
    assert_eq!(config.url, "stream://live/app");
    assert_eq!(config.input, TransportInput::Packets);
    assert_eq!(config.secret.as_ref().map(Secret::expose), Some("key-123"));
    assert_eq!(config.reconnect, reconnect);
    assert_eq!(
        config.adapter_options,
        [("latency_ms".to_owned(), "200".to_owned())]
    );

    let reached = run_until(&mut recording, &clock, |_| {
        r.log.count(0) >= 6 && r.log.count(1) >= 10
    });
    assert!(reached, "packets: {:?}", recording.progress());
    let report = recording.finish();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    assert_eq!(report.failure, None);
    // The transport got the encoders' streams, video then audio, and no chunk.
    let codecs: Vec<CodecId> = r
        .log
        .streams
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.codec)
        .collect();
    assert_eq!(codecs, [CodecId::H264, CodecId::Aac]);
    assert_eq!(r.log.headers.load(Ordering::Relaxed), 0);
    assert_eq!(r.log.finished.load(Ordering::Relaxed), 1);
    assert_eq!(report.progress.mux_drops, 0);
}

#[test]
fn a_transport_that_does_not_take_the_input_is_not_chosen() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    // `mem` transports take Matroska chunks only, `stream` ones packets only.
    for settings in [
        RecordSettings::packets(canvas, "mem://x"),
        RecordSettings::new(canvas, "stream://x", ContainerId::Matroska),
    ] {
        let (engine, _clock) = engine_at_start();
        let Err(err) = Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings)
        else {
            panic!("a transport was chosen")
        };
        assert!(
            matches!(
                err,
                RecordError::NoAdapter {
                    port: Port::Transport,
                    ..
                }
            ),
            "{err}"
        );
    }
    assert_eq!(r.capture.live_buffers(), 0);
}

#[test]
fn problems_outside_the_recorded_scene_do_not_stop_it() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    // An unknown plugin source in a scene that is not on air.
    let (idle, _) = col.add_scene("Idle", SceneScope::Shared).unwrap();
    let (plugin, _) = col
        .add_source(
            "Plugin",
            NodeTypeId::new_static("com.example.plugin"),
            Settings::new(),
        )
        .unwrap();
    col.add_item(
        idle,
        ItemContent::Source(plugin),
        Layout::new([0.0, 0.0], [8.0, 8.0]),
        None,
    )
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://x", ContainerId::Matroska);
    let recording =
        Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings).unwrap();
    assert_eq!(recording.started().problems.len(), 1);
    assert_eq!(recording.started().sources.len(), 4);
    let _ = recording.finish();

    // The same source on air stops the start before anything opens.
    let r = rig();
    let program = col.canvas(canvas).unwrap().program.unwrap();
    col.add_item(
        program,
        ItemContent::Source(plugin),
        Layout::new([0.0, 0.0], [8.0, 8.0]),
        None,
    )
    .unwrap();
    let (engine, _clock) = engine_at_start();
    let err = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap_err();
    assert!(matches!(err, RecordError::Problems(_)), "{err}");
    assert_eq!(r.log.headers.load(Ordering::Relaxed), 0);
    assert_eq!(r.capture.live_buffers(), 0);
    assert_eq!(r.audio.active_streams(), 0);
}

#[test]
fn every_video_quality_setting_reaches_the_encoder() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, _) = engine_at_start();
    let mut settings = RecordSettings::new(canvas, "mem://quality", ContainerId::Matroska);
    settings.video.rate_control = RateControl::Vbr {
        target_bps: 6_000_000,
        max_bps: 9_000_000,
    };
    settings.video.keyframe_interval = KeyframeInterval::Frames(90);
    settings.video.max_b_frames = 2;
    settings.video.profile = Some("main".into());
    settings.video.preset = EncoderPreset::Quality;
    let recording = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap();
    let config = r.video_config.lock().unwrap().clone().unwrap();
    assert_eq!(config.rate_control, settings.video.rate_control);
    assert_eq!(config.keyframe_interval, KeyframeInterval::Frames(90));
    assert_eq!(config.max_b_frames, 2);
    assert_eq!(config.profile.as_deref(), Some("main"));
    assert_eq!(config.tuning.preset, EncoderPreset::Quality);
    let report = recording.finish();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
}

#[test]
fn a_rate_control_no_encoder_takes_fails_with_each_reason() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (col, canvas) = scene();
    let (engine, _) = engine_at_start();
    let mut settings = RecordSettings::new(canvas, "mem://padded", ContainerId::Matroska);
    settings.video.rate_control = RateControl::CbrPadded {
        bitrate_bps: 8_000_000,
    };
    let err = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap_err();
    match &err {
        RecordError::NoAdapter {
            port: Port::VideoEncoder,
            tried,
        } => {
            assert_eq!(tried.len(), 2, "{err}");
            assert!(tried[1].1.contains("cbr_padded"), "{err}");
        }
        other => panic!("{other}"),
    }
    assert!(
        r.video_config.lock().unwrap().is_none(),
        "no session created"
    );
}

#[test]
fn an_unknown_canvas_or_a_missing_encoder_fails_before_anything_runs() {
    let Some(gpu) = gpu() else { return };
    let (col, canvas) = scene();

    // An empty video encoder registry.
    let mut r = rig();
    r.adapters.video_encoders = rbs_core::registry::Registry::new();
    let (engine, _) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://x", ContainerId::Matroska);
    let err = Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings).unwrap_err();
    assert!(
        matches!(
            err,
            RecordError::NoAdapter {
                port: Port::VideoEncoder,
                ..
            }
        ),
        "{err}"
    );

    // A container no muxer lists.
    let r = rig();
    let (engine, _) = engine_at_start();
    let settings = RecordSettings::new(canvas, "mem://x", ContainerId::Mp4Fragmented);
    let err = Recording::start(engine, Arc::clone(&gpu), &col, &r.adapters, &settings).unwrap_err();
    assert!(
        matches!(
            err,
            RecordError::NoAdapter {
                port: Port::Muxer,
                ..
            }
        ),
        "{err}"
    );

    // A canvas that is not in the collection.
    let (other, _) = {
        let mut c = SceneCollection::new();
        c.add_canvas("A", Size::new(W, H), FPS).unwrap();
        c.add_canvas("B", Size::new(W, H), FPS).unwrap()
    };
    let (engine, _) = engine_at_start();
    let settings = RecordSettings::new(other, "mem://x", ContainerId::Matroska);
    let err = Recording::start(engine, gpu, &col, &r.adapters, &settings).unwrap_err();
    assert!(matches!(err, RecordError::UnknownCanvas(_)), "{err}");
    assert_eq!(
        r.log.headers.load(Ordering::Relaxed),
        0,
        "nothing was opened"
    );
    assert_eq!(r.capture.live_buffers(), 0);
    assert_eq!(r.audio.active_streams(), 0);
}

#[test]
fn a_video_source_that_cannot_be_instantiated_fails_the_start_before_any_device_opens() {
    let Some(gpu) = gpu() else { return };
    let (mut col, canvas) = scene();
    // A camera asked for 0 frames per second is refused when it is instantiated.
    let camera = col.sources().find(|s| s.name == "Camera").unwrap().id;
    let mut settings = Settings::new();
    settings.insert(keys::FPS.into(), SettingValue::Int(0));
    col.set_source_settings(camera, settings).unwrap();
    let r = rig();
    let (engine, _) = engine_at_start();
    let record = RecordSettings::new(canvas, "mem://x", ContainerId::Matroska);
    let err = Recording::start(engine, gpu, &col, &r.adapters, &record).unwrap_err();
    assert!(
        matches!(&err, RecordError::Source { name, .. } if name == "Camera"),
        "{err}"
    );
    // Nothing was opened: no audio stream ran, no frame was captured.
    assert_eq!(r.audio.active_streams(), 0);
    assert_eq!(r.capture.live_buffers(), 0);
    assert!(r.log.packets.lock().unwrap().is_empty());
}
