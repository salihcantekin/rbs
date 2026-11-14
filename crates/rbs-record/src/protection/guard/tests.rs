#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

//! Synthetic samples on explicit timestamps: no disk, no clock, no sleeping (ADR-0032).

use super::*;

const VOL: VolumeKey = VolumeKey(1);

fn at(secs: u64) -> Timestamp {
    Timestamp::from_nanos(secs * 1_000_000_000)
}

fn rec(id: u64, lost: u64) -> OutputSample {
    OutputSample {
        output: OutputId::test(id),
        stream: false,
        volume: Some(VOL),
        counters: Progress {
            encoder_skipped: lost,
            ..Progress::default()
        },
    }
}

fn stream(id: u64, dropped: u64) -> OutputSample {
    OutputSample {
        output: OutputId::test(id),
        stream: true,
        volume: None,
        counters: Progress {
            transport_drops: dropped,
            ..Progress::default()
        },
    }
}

fn disk(free: u64) -> Option<ProbeStep> {
    Some(ProbeStep::Reading {
        volumes: vec![VolumeSample {
            volume: VOL,
            free_bytes: Probed::Value(free),
        }],
        memory: Probed::Unsupported,
    })
}

fn sample(outputs: Vec<OutputSample>, probe: Option<ProbeStep>) -> Sample {
    Sample {
        outputs,
        probe,
        ..Sample::default()
    }
}

fn disk_level(g: &Guard) -> Level {
    g.status().disks.first().map_or(Level::Ok, |d| d.level)
}

/// Feeds a volume that falls by `per_5s` bytes every five seconds from `start`,
/// from `from` to `to` seconds; returns the stops asked for.
fn fill(g: &mut Guard, from: u64, to: u64, start: u64, per_5s: u64) -> Vec<StopRequest> {
    let mut stops = Vec::new();
    for t in (from..=to).step_by(5) {
        let free = start.saturating_sub((t - from) / 5 * per_5s);
        stops.extend(g.step(at(t), &sample(vec![rec(1, 0)], disk(free))));
    }
    stops
}

#[test]
fn no_rate_is_guessed_from_fewer_than_three_readings() {
    let mut g = Guard::new(ProtectionSettings::default());
    g.step(at(0), &sample(vec![rec(1, 0)], disk(10 * GIB)));
    g.step(at(5), &sample(vec![rec(1, 0)], disk(9 * GIB)));
    assert_eq!(g.status().disks[0].seconds_left, None);
    assert_eq!(disk_level(&g), Level::Ok);
    g.step(at(10), &sample(vec![rec(1, 0)], disk(8 * GIB)));
    // 2 GiB fell in 10 s; 7 GiB above the 1 GiB reserve: 35 s.
    assert_eq!(g.status().disks[0].seconds_left, Some(35));
}

#[test]
fn a_disk_that_does_not_fall_has_no_time_limit() {
    let mut g = Guard::new(ProtectionSettings::default());
    fill(&mut g, 0, 60, 50 * GIB, 0);
    assert_eq!(g.status().disks[0].seconds_left, None);
    assert_eq!(disk_level(&g), Level::Ok);
    assert!(g.status().alerts.is_empty());
    assert_eq!(g.status().level(), Level::Ok);
}

#[test]
fn time_left_raises_notice_then_warning_then_stops_cleanly_at_the_reserve() {
    let mut g = Guard::new(ProtectionSettings::default());
    // 100 MiB per 5 s = 20 MiB/s. 41 GiB free: 40 GiB above the reserve, about 34 min.
    let mib = 1024 * 1024;
    let stops = fill(&mut g, 0, 20, 41 * GIB, 100 * mib);
    assert!(stops.is_empty());
    assert_eq!(disk_level(&g), Level::Ok);
    // Below 30 minutes: notice.
    let free = 30 * GIB;
    let stops = fill(&mut g, 25, 85, free, 100 * mib);
    assert!(stops.is_empty());
    assert_eq!(disk_level(&g), Level::Notice);
    let a = &g.status().alerts[0];
    assert_eq!(a.kind, AlertKind::Disk);
    assert!(a.text.starts_with("About "), "{}", a.text);
    // Below 3 minutes: warning, with the stop announced.
    let stops = fill(&mut g, 90, 150, 3 * GIB, 100 * mib);
    assert!(stops.is_empty());
    assert_eq!(disk_level(&g), Level::Warning);
    assert!(g.status().alerts[0].text.contains("will stop"));
    assert_eq!(g.status().level(), Level::Warning);
    // At the reserve: critical, and the recording is asked to stop once.
    let stops = g.step(at(155), &sample(vec![rec(1, 0)], disk(GIB)));
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].output, OutputId::test(1));
    assert!(stops[0].reason.contains("1.0 GiB"), "{}", stops[0].reason);
    assert_eq!(disk_level(&g), Level::Critical);
    let again = g.step(at(160), &sample(vec![rec(1, 0)], disk(GIB - 1)));
    assert!(again.is_empty(), "asked once");
}

#[test]
fn warn_only_never_stops() {
    let mut g = Guard::new(ProtectionSettings {
        at_critical: AtCritical::WarnOnly,
        ..ProtectionSettings::default()
    });
    let stops = g.step(at(0), &sample(vec![rec(1, 0)], disk(GIB / 2)));
    assert!(stops.is_empty());
    assert_eq!(disk_level(&g), Level::Critical);
    assert!(g.status().alerts[0].text.contains("almost full"));
}

#[test]
fn every_recording_on_the_volume_stops_but_not_a_stream() {
    let mut g = Guard::new(ProtectionSettings::default());
    let mut other = rec(2, 0);
    other.volume = Some(VolumeKey(9));
    let outputs = vec![rec(1, 0), rec(3, 0), stream(4, 0), other];
    let stops = g.step(at(0), &sample(outputs, disk(0)));
    let ids: Vec<OutputId> = stops.iter().map(|s| s.output).collect();
    assert_eq!(ids, [OutputId::test(1), OutputId::test(3)]);
}

#[test]
fn a_recording_started_on_a_full_volume_is_asked_to_stop_too() {
    let mut g = Guard::new(ProtectionSettings::default());
    assert_eq!(g.step(at(0), &sample(vec![rec(1, 0)], disk(0))).len(), 1);
    // The first one is still ending when a second starts on the same volume; the
    // reading is the last one (no new probe answer this step).
    let stops = g.step(at(1), &sample(vec![rec(1, 0), rec(2, 0)], None));
    assert_eq!(stops.len(), 1);
    assert_eq!(stops[0].output, OutputId::test(2));
}

#[test]
fn critical_falls_back_only_with_a_margin_above_the_reserve() {
    let mut g = Guard::new(ProtectionSettings {
        at_critical: AtCritical::WarnOnly,
        ..ProtectionSettings::default()
    });
    g.step(at(0), &sample(vec![rec(1, 0)], disk(GIB)));
    assert_eq!(disk_level(&g), Level::Critical);
    // 5 % above the reserve: still critical.
    g.step(at(5), &sample(vec![rec(1, 0)], disk(GIB + GIB / 20)));
    assert_eq!(disk_level(&g), Level::Critical);
    // 20 % above (someone freed space): no longer critical.
    g.step(at(10), &sample(vec![rec(1, 0)], disk(GIB + GIB / 5)));
    assert_ne!(disk_level(&g), Level::Critical);
}

#[test]
fn a_timed_out_probe_makes_disk_and_memory_unavailable_until_it_answers() {
    let mut g = Guard::new(ProtectionSettings::default());
    fill(&mut g, 0, 10, 10 * GIB, 0);
    g.step(at(20), &sample(vec![rec(1, 0)], Some(ProbeStep::TimedOut)));
    let st = g.status();
    assert_eq!(st.disks[0].level, Level::Unavailable);
    assert_eq!(st.disks[0].free_bytes, None);
    assert_eq!(st.memory, Some(Level::Unavailable));
    assert_eq!(st.alerts[0].text, "Free disk space could not be read.");
    // Unavailable does not raise the overall level.
    assert_eq!(st.level(), Level::Ok);
    g.step(at(25), &sample(vec![rec(1, 0)], disk(10 * GIB)));
    assert_eq!(disk_level(&g), Level::Ok);
}

#[test]
fn a_volume_no_recording_uses_is_forgotten() {
    let mut g = Guard::new(ProtectionSettings::default());
    fill(&mut g, 0, 10, 10 * GIB, 0);
    g.step(at(15), &sample(vec![stream(4, 0)], None));
    assert!(g.status().disks.is_empty());
}

#[test]
fn lag_needs_the_limit_exceeded_for_lag_seconds_in_a_row_and_clears_the_same_way() {
    let mut g = Guard::new(ProtectionSettings::default());
    let lag = |g: &Guard| g.status().outputs[0].level;
    // Baseline, then 2 lost frames per second: over the limit of 1.
    g.step(at(0), &sample(vec![rec(1, 0)], None));
    g.step(at(1), &sample(vec![rec(1, 2)], None));
    g.step(at(2), &sample(vec![rec(1, 4)], None));
    assert_eq!(lag(&g), Level::Ok, "two seconds are not enough");
    g.step(at(3), &sample(vec![rec(1, 6)], None));
    assert_eq!(lag(&g), Level::Warning);
    let a = &g.status().alerts[0];
    assert_eq!(
        (a.kind, a.output),
        (AlertKind::Lag, Some(OutputId::test(1)))
    );
    assert!(a.text.starts_with("The recording is losing frames"));
    // Exactly at the limit is not over it; three such seconds clear the warning.
    g.step(at(4), &sample(vec![rec(1, 7)], None));
    g.step(at(5), &sample(vec![rec(1, 8)], None));
    assert_eq!(lag(&g), Level::Warning);
    g.step(at(6), &sample(vec![rec(1, 9)], None));
    assert_eq!(lag(&g), Level::Ok);
    assert!(g.status().alerts.is_empty());
}

#[test]
fn one_bad_second_between_good_ones_is_not_lag() {
    let mut g = Guard::new(ProtectionSettings::default());
    let mut lost = 0;
    for t in 0..20 {
        // A burst of 5 every fourth second.
        if t % 4 == 3 {
            lost += 5;
        }
        g.step(at(t), &sample(vec![rec(1, lost)], None));
        assert_eq!(g.status().outputs[0].level, Level::Ok, "t={t}");
    }
}

#[test]
fn a_stream_counts_transport_drops_and_says_connection() {
    let mut g = Guard::new(ProtectionSettings::default());
    for t in 0..=3 {
        g.step(at(t), &sample(vec![stream(4, t * 30)], None));
    }
    assert_eq!(g.status().outputs[0].level, Level::Warning);
    assert!(g.status().alerts[0].text.contains("connection"));
    // A recording ignores transport drops (its transport is a file).
    assert_eq!(frames_lost(&stream(4, 9).counters, false), 0);
}

#[test]
fn late_render_and_audio_ticks_are_studio_wide_alerts() {
    let mut g = Guard::new(ProtectionSettings::default());
    for t in 0..=3 {
        let s = Sample {
            outputs: vec![rec(1, 0)],
            late_render_ticks: t * 10,
            late_audio_ticks: t * 10,
            probe: None,
        };
        g.step(at(t), &s);
    }
    let st = g.status();
    assert_eq!(st.render, Some(Level::Warning));
    assert_eq!(st.audio, Some(Level::Warning));
    assert_eq!(st.outputs[0].level, Level::Ok);
    let kinds: Vec<AlertKind> = st.alerts.iter().map(|a| a.kind).collect();
    assert_eq!(kinds, [AlertKind::Render, AlertKind::Audio]);
}

#[test]
fn memory_warns_below_the_minimum_and_clears_with_a_margin() {
    let mut g = Guard::new(ProtectionSettings::default());
    let mem = |avail: u64| {
        Some(ProbeStep::Reading {
            volumes: Vec::new(),
            memory: Probed::Value(avail),
        })
    };
    g.step(at(0), &sample(vec![stream(4, 0)], mem(GIB / 2)));
    assert_eq!(g.status().memory, Some(Level::Warning));
    assert_eq!(g.status().available_memory, Some(GIB / 2));
    assert_eq!(
        g.status().alerts[0].text,
        "Memory is low: 0.5 GiB available."
    );
    g.step(at(5), &sample(vec![stream(4, 0)], mem(GIB + GIB / 20)));
    assert_eq!(g.status().memory, Some(Level::Warning), "inside the margin");
    g.step(at(10), &sample(vec![stream(4, 0)], mem(2 * GIB)));
    assert_eq!(g.status().memory, Some(Level::Ok));
    // A platform without a reading reports it as unavailable, never a guess.
    g.step(
        at(15),
        &sample(
            vec![stream(4, 0)],
            Some(ProbeStep::Reading {
                volumes: Vec::new(),
                memory: Probed::Unsupported,
            }),
        ),
    );
    assert_eq!(g.status().memory, Some(Level::Unavailable));
}

#[test]
fn thresholds_changed_during_a_take_apply_on_the_next_step() {
    let mut g = Guard::new(ProtectionSettings::default());
    fill(&mut g, 0, 10, 5 * GIB, 0);
    assert_eq!(disk_level(&g), Level::Ok);
    g.set_settings(ProtectionSettings {
        reserve_bytes: 6 * GIB,
        at_critical: AtCritical::WarnOnly,
        ..ProtectionSettings::default()
    });
    g.step(at(15), &sample(vec![rec(1, 0)], disk(5 * GIB)));
    assert_eq!(disk_level(&g), Level::Critical);
}

#[test]
fn the_probe_step_maps_folders_to_volumes_once() {
    use crate::protection::{ProbeReading, VolumeSpace};
    let space = |v, f| {
        Probed::Value(VolumeSpace {
            volume: VolumeKey(v),
            free_bytes: f,
        })
    };
    let step = ProbeStep::of(ProbeOutcome::Reading(ProbeReading {
        folders: vec![
            ("/a".into(), space(1, 10)),
            ("/b".into(), space(1, 10)),
            ("/c".into(), Probed::Unknown),
            ("/d".into(), space(2, 20)),
        ],
        memory: Probed::Value(3),
    }));
    let ProbeStep::Reading { volumes, memory } = step else {
        panic!("a reading");
    };
    assert_eq!(volumes.len(), 2);
    assert_eq!(memory, Probed::Value(3));
    assert_eq!(ProbeStep::of(ProbeOutcome::TimedOut), ProbeStep::TimedOut);
}

#[test]
fn ended_outputs_are_forgotten() {
    let mut g = Guard::new(ProtectionSettings::default());
    g.step(at(0), &sample(vec![rec(1, 0), stream(4, 0)], None));
    assert_eq!(g.status().outputs.len(), 2);
    g.step(at(1), &sample(vec![stream(4, 0)], None));
    assert_eq!(g.status().outputs.len(), 1);
    assert_eq!(g.status().outputs[0].output, OutputId::test(4));
}
