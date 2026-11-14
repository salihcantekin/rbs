//! Separate video files (ADR-0086 step 3.2, ADR-0119): one file per source marked for
//! video, from the source's own picture (or its chain tail when filters are asked
//! for), each encoded on a partition of its own and written on the ISO output
//! partition. Reference adapters on a `TestClock`, as in
//! `studio.rs`. Waits are for packet counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_core::media::{CodecId, ContainerId};
use rbs_graph::node::NodeTypeId;
use rbs_record::{
    ISO_OUTPUT, ISO_VIDEO_ENCODERS, IsoError, IsoVideoStart, OutputSettings, RecordError, Studio,
    StudioSettings, VideoSettings,
};
use rbs_scene::model::{ItemContent, Layout, RecordSeparately, SettingValue, Settings};
use rbs_sources::types::{self, keys};
use support::*;

fn h264(log: &Log) -> usize {
    log.packets
        .lock()
        .unwrap()
        .iter()
        .filter(|packet| packet.codec == CodecId::H264)
        .count()
}

fn start_request() -> IsoVideoStart {
    IsoVideoStart::new("mem://iso", ContainerId::Matroska, VideoSettings::default())
}

#[test]
fn separate_video_files_follow_each_marked_source_and_are_not_outputs() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    // Nothing marked: nothing is added and no worker starts.
    let idle = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap();
    assert!(idle.tracks.is_empty());
    assert!(!studio.iso_video_running());
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part));
    }
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);

    // The master runs first: the sources draw only while something takes the canvas.
    let master = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    let master_sessions = r.video_sessions.load(Ordering::Relaxed);

    // A still picture: its chain gives one frame, and its file still gets one per tick.
    let scene = col.scenes().next().unwrap().id;
    let settings: Settings = [
        (
            keys::COLOR.to_owned(),
            SettingValue::Text("#30a050".to_owned()),
        ),
        (keys::WIDTH.to_owned(), SettingValue::Int(32)),
        (keys::HEIGHT.to_owned(), SettingValue::Int(18)),
    ]
    .into_iter()
    .collect();
    let (card, _) = col
        .add_source(
            "Card",
            NodeTypeId::new_static(types::COLOR_SOURCE),
            settings,
        )
        .unwrap();
    col.add_item(
        scene,
        ItemContent::Source(card),
        Layout::new([0.0, 0.0], [8.0, 4.5]),
        None,
    )
    .unwrap();
    for name in ["Display", "Camera", "Card"] {
        let id = col.sources().find(|s| s.name == name).unwrap().id;
        col.set_source_record_separately(id, RecordSeparately::new(true, false))
            .unwrap();
    }
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    // A file takes its size from the source's picture, known once capture opened.
    let marked: Vec<_> = col
        .sources()
        .filter(|s| s.record_separately.video)
        .map(|s| s.id)
        .collect();
    let reached = run_until(&mut studio, &clock, |s| {
        marked.iter().all(|id| s.picture_size(*id).is_some())
    });
    let sizes: Vec<_> = marked.iter().map(|id| studio.picture_size(*id)).collect();
    assert!(reached, "{sizes:?} {:?}", studio.progress());
    let started = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap();
    assert_eq!(started.tracks.len(), 3);
    for track in &started.tracks {
        assert!(track.file_name.ends_with(".mkv"), "{}", track.file_name);
        assert_eq!(track.size.0 % 2, 0);
        assert_eq!(track.size.1 % 2, 0);
    }
    assert!(studio.iso_video_running());
    assert_eq!(studio.outputs(), [master], "the files are not outputs");
    assert_eq!(
        r.video_sessions.load(Ordering::Relaxed),
        master_sessions * 4,
        "each file opens its encoder as the master did"
    );
    let err = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap_err();
    assert!(matches!(err, RecordError::IsoRunning));

    let reached = run_until(&mut studio, &clock, |_| {
        started
            .tracks
            .iter()
            .all(|t| t.stats.frames.load(Ordering::Relaxed) >= 10)
            && r.log.headers.load(Ordering::Relaxed) >= 4
    });
    assert!(
        reached,
        "h264 {} headers {}",
        h264(&r.log),
        r.log.headers.load(Ordering::Relaxed)
    );
    // Files on the master's engine take turns on two encoder partitions, and none beyond.
    for (i, part) in ISO_VIDEO_ENCODERS.into_iter().enumerate() {
        assert_eq!(studio.engine().has_worker(part), i < 2, "{part:?}");
    }
    assert!(studio.engine().has_worker(ISO_OUTPUT));

    for track in &started.tracks {
        assert_eq!(track.stats.lost.load(Ordering::Relaxed), 0, "{track:?}");
        assert!(track.queue.is_some(), "{track:?}");
    }

    assert!(studio.stop_iso_video());
    assert!(!studio.iso_video_running());
    assert!(!studio.stop_iso_video());
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part), "{part:?}");
    }
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert!(r.log.trailers.load(Ordering::Relaxed) >= 3);
    let report = studio.stop_output(master).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_marked_source_with_no_picture_yet_adds_nothing() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    col.set_source_record_separately(display, RecordSeparately::new(true, false))
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
    // No tick ran, so the capture has not reported a picture.
    let err = studio
        .start_iso_video(&r.adapters, &start_request())
        .unwrap_err();
    assert!(
        matches!(err, RecordError::Iso(IsoError::NoPicture { source, .. }) if source == display),
        "{err:?}"
    );
    assert!(!studio.iso_video_running());
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);
    for part in ISO_VIDEO_ENCODERS {
        assert!(!studio.engine().has_worker(part));
    }
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_filtered_source_is_recorded_without_its_filters_unless_asked() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    col.add_filter(
        display,
        NodeTypeId::new_static(rbs_filters::catalogue::BLUR),
        Settings::new(),
    )
    .unwrap();
    col.set_source_record_separately(display, RecordSeparately::new(true, false))
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
    let master = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |s| s
        .picture_size(display)
        .is_some()));

    // The default reads the source's own picture; `include_filters` reads its chain
    // tail. Both give a file at the canvas frame rate.
    for include_filters in [false, true] {
        let mut request = start_request();
        request.include_filters = include_filters;
        let started = studio.start_iso_video(&r.adapters, &request).unwrap();
        assert_eq!(started.tracks.len(), 1);
        let track = &started.tracks[0];
        let reached = run_until(&mut studio, &clock, |_| {
            track.stats.frames.load(Ordering::Relaxed) >= 5
        });
        assert!(reached, "include_filters {include_filters}: {track:?}");
        assert_eq!(track.stats.lost.load(Ordering::Relaxed), 0, "{track:?}");
        assert!(studio.stop_iso_video());
    }

    let report = studio.stop_output(master).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
