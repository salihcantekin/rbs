//! Outputs on a bus (ADR-0170 decision 3, ADR-0097 F2): an output names a bus of the
//! main canvas and encodes that bus's mix; outputs share an audio encoder only on the
//! same bus; a name without a bus refuses the start. Reference adapters on a
//! `TestClock`; tests wait for block counts, never for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_core::media::ContainerId;
use rbs_record::{AUDIO, OutputSettings, RecordError, Studio, StudioSettings};
use rbs_scene::model::BusId;
use support::*;

fn audio_only(url: &str, bus: Option<&str>) -> OutputSettings {
    let mut out = OutputSettings::new(url, ContainerId::Matroska);
    out.video = None;
    out.audio_bus = bus.map(str::to_owned);
    out
}

#[test]
fn an_output_on_a_bus_hears_only_what_is_sent_to_it() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let mic = col.sources().find(|s| s.name == "Microphone").unwrap().id;
    let (music, _) = col.add_bus(canvas, "Music").unwrap();
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
    let wait = |s: &mut Studio, n| {
        let t = blocks(s);
        assert!(run_until(s, &clock, |s| blocks(s) >= t + n));
    };

    // A name without a bus is refused; it never falls back to `main`.
    assert!(matches!(
        studio.start_output(&r.adapters, &audio_only("mem://nope", Some("Nope"))),
        Err(RecordError::UnknownBus(n)) if n == "Nope"
    ));
    assert!(studio.outputs().is_empty());

    // `main` and Music take encoders of their own; a second Music output shares.
    studio
        .start_output(&r.adapters, &audio_only("mem://main", None))
        .unwrap();
    let rec = studio
        .start_output(&r.adapters, &audio_only("mem://music", Some("Music")))
        .unwrap();
    assert!(!studio.output(rec).unwrap().audio_shared, "another bus");
    let again = studio
        .start_output(&r.adapters, &audio_only("mem://music2", Some("Music")))
        .unwrap();
    assert!(studio.output(again).unwrap().audio_shared, "same bus");

    // Nothing is sent to Music yet: its meter stays silent while `main` hears the
    // microphone.
    let meter = std::sync::Arc::clone(studio.bus_meter("Music").unwrap());
    assert!(studio.bus_meter("Nope").is_none());
    wait(&mut studio, 10);
    let _ = meter.take();
    let _ = studio.master_meter().unwrap().take();
    wait(&mut studio, 10);
    assert_eq!(meter.take().peak(0), 0.0, "no sends, no sound");
    assert!(studio.master_meter().unwrap().take().peak(0) > 0.0);

    // The microphone sent to Music and taken off `main`: heard on Music only.
    col.set_send(canvas, mic, music, 1.0).unwrap();
    col.set_send(canvas, mic, BusId::MAIN, 0.0).unwrap();
    for other in col.sources().map(|s| s.id).collect::<Vec<_>>() {
        if other != mic {
            col.set_source_muted(other, true).unwrap();
        }
    }
    assert!(studio.set_mix(&col));
    wait(&mut studio, 10);
    let _ = meter.take();
    let _ = studio.master_meter().unwrap().take();
    wait(&mut studio, 10);
    assert!(meter.take().peak(0) > 0.0, "the send is heard on Music");
    assert_eq!(studio.master_meter().unwrap().take().peak(0), 0.0);

    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
