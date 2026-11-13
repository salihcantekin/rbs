//! Separate audio files (ADR-0086 step 2): one AAC file per marked source, tapped
//! before the fader, on partitions that are not output slots. Reference adapters on
//! a `TestClock`, as in `studio.rs`. Waits are for packet counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::atomic::Ordering;

use rbs_core::media::{CodecId, ContainerId};
use rbs_record::{
    ISO_AUDIO_ENCODER, ISO_OUTPUT, IsoAudioStart, OutputSettings, RecordError, Studio,
    StudioSettings,
};
use rbs_scene::model::RecordSeparately;
use support::*;

fn aac(log: &Log) -> usize {
    log.packets
        .lock()
        .unwrap()
        .iter()
        .filter(|packet| packet.codec == CodecId::Aac)
        .count()
}

fn start_request(url: &str) -> IsoAudioStart {
    IsoAudioStart::new(
        url,
        ContainerId::Matroska,
        "master.mkv",
        "2026-11-13T19:00:00Z",
        1_000,
    )
}

#[test]
fn separate_audio_follows_a_muted_source_and_is_not_an_output() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let display = col.sources().find(|s| s.name == "Display").unwrap().id;
    col.set_source_muted(mic, true).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    let idle = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap();
    assert_eq!(idle.audio_tracks, 0);
    assert!(!studio.iso_audio_running());
    assert!(!studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);

    col.set_source_record_separately(mic, RecordSeparately::new(false, true))
        .unwrap();
    col.set_source_record_separately(display, RecordSeparately::new(true, false))
        .unwrap();
    let updated = studio.update(&col, &r.adapters.sources).unwrap();
    assert!(updated.failed.is_empty(), "{updated:?}");
    let started = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap();
    assert_eq!(started.audio_tracks, 1);
    assert_eq!(started.video_pending, 1);
    assert!(studio.iso_audio_running());
    assert!(studio.outputs().is_empty());
    assert_eq!(r.video_sessions.load(Ordering::Relaxed), 0);
    let text = started.manifest.to_toml();
    assert!(text.contains("name = \"Microphone.mkv\"\n"));
    assert!(text.contains("name = \"Display\"\n"));
    assert!(!text.contains("start_offset_ns"));
    let err = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap_err();
    assert!(matches!(err, RecordError::IsoRunning));

    let mic_node = studio
        .sources()
        .into_iter()
        .find(|s| s.name == "Microphone")
        .unwrap()
        .node;
    assert!(
        studio.engine().crossings().iter().any(|crossing| {
            crossing.link.from.node == mic_node && crossing.to == ISO_AUDIO_ENCODER
        }),
        "the file is tapped from the microphone, before the mixer"
    );
    let reached = run_until(&mut studio, &clock, |_| {
        aac(&r.log) >= 4 && r.log.headers.load(Ordering::Relaxed) >= 1
    });
    assert!(
        reached,
        "aac {} headers {}",
        aac(&r.log),
        r.log.headers.load(Ordering::Relaxed)
    );
    assert!(studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(studio.engine().has_worker(ISO_OUTPUT));

    let id = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://master", ContainerId::Matroska),
        )
        .unwrap();
    assert_eq!(studio.outputs(), [id]);
    assert!(studio.iso_audio_running());
    let report = studio.stop_output(id).unwrap();
    assert!(report.shutdown_errors.is_empty(), "{report:?}");

    assert!(studio.stop_iso_audio());
    assert!(!studio.iso_audio_running());
    assert!(!studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert!(r.log.trailers.load(Ordering::Relaxed) >= 1);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_separate_audio_file_that_cannot_open_leaves_the_studio_running() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    col.set_source_record_separately(mic, RecordSeparately::new(false, true))
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

    let err = studio
        .start_iso_audio(&r.adapters, &start_request("mem://fail"))
        .unwrap_err();
    assert!(matches!(err, RecordError::Start(_)), "{err}");
    assert!(!studio.iso_audio_running());
    assert!(!studio.engine().has_worker(ISO_AUDIO_ENCODER));
    assert!(!studio.engine().has_worker(ISO_OUTPUT));
    assert!(studio.outputs().is_empty());

    let started = studio
        .start_iso_audio(&r.adapters, &start_request("mem://iso"))
        .unwrap();
    assert_eq!(started.audio_tracks, 1);
    assert!(studio.stop_iso_audio());
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
