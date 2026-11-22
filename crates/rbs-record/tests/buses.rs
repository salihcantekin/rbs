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

#[test]
fn a_recording_writes_one_named_track_per_bus_on_the_same_packet_grid() {
    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    col.add_bus(canvas, "Music").unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let tracks = |names: &[&str]| {
        let mut out = audio_only("mem://tracks", None);
        out.more_tracks = names.iter().map(|n| (*n).to_owned()).collect();
        out.pausable = true;
        out
    };

    // Refused before anything opens: a bus named twice, a missing bus, and more
    // tracks than a packet transport carries.
    assert!(matches!(
        studio.start_output(&r.adapters, &tracks(&["Music", "Music"])),
        Err(RecordError::RepeatedTrack(n)) if n == "Music"
    ));
    assert!(matches!(
        studio.start_output(&r.adapters, &tracks(&["Nope"])),
        Err(RecordError::UnknownBus(n)) if n == "Nope"
    ));
    let mut packets = OutputSettings::packets("stream://live");
    packets.video = None;
    packets.more_tracks = vec!["Music".to_owned()];
    assert!(matches!(
        studio.start_output(&r.adapters, &packets),
        Err(RecordError::TracksUnsupported { allowed: 1 })
    ));
    assert!(studio.outputs().is_empty());

    let id = studio
        .start_output(&r.adapters, &tracks(&["Music"]))
        .unwrap();
    let report = studio.output(id).unwrap();
    assert!(!report.audio_shared);
    let buses: Vec<&str> = report.tracks.iter().map(|t| t.bus.as_str()).collect();
    assert_eq!(buses, ["main", "Music"]);
    let streams = r.log.streams.lock().unwrap().clone();
    let named: Vec<(Option<&str>, bool)> = streams
        .iter()
        .map(|s| (s.name.as_deref(), s.default))
        .collect();
    assert_eq!(named, [(Some("main"), true), (Some("Music"), false)]);

    let blocks = |s: &Studio| s.engine().worker_stats(AUDIO).unwrap().ticks();
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 20
        && r.log.count(1) >= 5));
    let progress = studio.output_progress(id).unwrap();
    assert_eq!(progress.tracks.len(), 2);
    assert_eq!(progress.tracks[1].bus, "Music");

    // Paused and resumed: both tracks' encoders follow one window, cut on the same
    // block.
    studio.pause_output(id).unwrap();
    let t = blocks(&studio);
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 10));
    studio.resume_output(id).unwrap();
    let (before, t) = (r.log.count(1), blocks(&studio));
    assert!(run_until(&mut studio, &clock, |s| blocks(s) >= t + 20
        && r.log.count(1) >= before + 5));
    assert!(!studio.output_progress(id).unwrap().paused);

    // Both tracks' encoders cut the same blocks: the same packet times, in order.
    let pts = |stream: u16| -> Vec<_> {
        let all = r.log.packets.lock().unwrap();
        all.iter()
            .filter(|p| p.stream == stream)
            .map(|p| p.pts)
            .collect()
    };
    let (one, two) = (pts(0), pts(1));
    let n = one.len().min(two.len());
    assert!(
        n >= 5 && one.len().abs_diff(two.len()) <= 1,
        "{} {}",
        one.len(),
        two.len()
    );
    assert_eq!(one[..n], two[..n]);

    // A one-track output of `main` never joins the multi-track chain.
    let main = studio
        .start_output(&r.adapters, &audio_only("mem://main", None))
        .unwrap();
    assert!(!studio.output(main).unwrap().audio_shared);
    assert!(studio.output(main).unwrap().tracks.is_empty());

    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
