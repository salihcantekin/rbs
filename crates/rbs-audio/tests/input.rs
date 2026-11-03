//! Input node: alignment, drift correction, offsets, gaps and late audio (audio graph
//! spec "Input node", ADR-0037 decisions 4 to 6).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_audio::{AudioGraphConfig, InputParams};
use rbs_core::media::ChannelLayout;
use rbs_core::timing::SampleRate;
use support::{Rig, Script};

const SETTLE: Duration = Duration::from_secs(4);

fn rig(script: Script) -> Rig {
    Rig::new(script, AudioGraphConfig::default(), InputParams::NONE)
}

#[test]
fn a_perfect_device_is_aligned_from_the_first_block() {
    let mut r = rig(Script::default());
    r.run(300);
    let c = r.stats.read();
    assert!(r.max_error_after(Duration::from_millis(100)) < 50e-6);
    assert_eq!(c.resyncs, 0);
    assert_eq!(r.silent_blocks_after(Duration::from_millis(100)), 0);
}

#[test]
fn drift_is_corrected_without_resyncs() {
    for drift in [1e-4, -1e-4, 1e-3, -1e-3] {
        let mut r = rig(Script {
            drift,
            ..Script::default()
        });
        r.run(2_000); // 20 s
        let c = r.stats.read();
        assert_eq!(c.resyncs, 0, "drift {drift}");
        let e = r.max_error_after(SETTLE);
        // Steady state is drift / gain (0.2 ms at 100 ppm, 2 ms at 1000 ppm).
        let bound = (drift.abs() / rbs_audio::STEER_GAIN) * 1.2 + 100e-6;
        assert!(e < bound, "drift {drift}: error {e} s");
        assert_eq!(r.silent_blocks_after(SETTLE), 0, "drift {drift}");
    }
}

#[test]
fn timestamp_jitter_does_not_resync_or_misalign() {
    let mut r = rig(Script {
        drift: 1e-4,
        jitter: Duration::from_millis(1),
        ..Script::default()
    });
    r.run(2_000);
    let c = r.stats.read();
    assert_eq!(c.resyncs, 0);
    assert_eq!(c.gaps_filled + c.overlaps_dropped, 0);
    assert!(r.max_error_after(SETTLE) < 1e-3);
}

#[test]
fn a_44_1_khz_stereo_device_is_converted_and_aligned() {
    let mut r = rig(Script {
        rate: SampleRate::HZ_44100,
        channels: ChannelLayout::Stereo,
        packet: 441,
        drift: -1e-4,
        ..Script::default()
    });
    r.run(1_000);
    assert_eq!(r.stats.read().resyncs, 0);
    assert!(r.max_error_after(SETTLE) < 0.5e-3);
    // The level survives conversion and the stereo matrix.
    let rms = r.seen.lock().unwrap().last().unwrap().rms;
    assert!(
        (rms - support::AMPLITUDE / 2f64.sqrt()).abs() < 0.01,
        "{rms}"
    );
}

#[test]
fn a_delay_offset_plays_the_source_later() {
    let mut r = Rig::new(
        Script::default(),
        AudioGraphConfig::default(),
        InputParams::delayed(Duration::from_millis(30)),
    );
    r.run(300);
    // The probe measures against `block time - offset`.
    assert!(r.max_error_after(Duration::from_millis(200)) < 50e-6);
}

#[test]
fn an_advance_offset_plays_the_source_earlier() {
    let mut r = Rig::new(
        Script::default(),
        AudioGraphConfig::default(),
        InputParams::advanced(Duration::from_millis(30)),
    );
    r.run(300);
    assert!(r.max_error_after(Duration::from_millis(200)) < 50e-6);
    assert_eq!(r.silent_blocks_after(Duration::from_millis(200)), 0);
}

#[test]
fn an_offset_change_while_running_moves_the_source() {
    let mut r = rig(Script::default());
    r.run(200);
    r.params
        .send(InputParams::delayed(Duration::from_millis(3)));
    r.run(1_000);
    // A 3 ms change is below the resync threshold: it slews, then holds. The probe
    // measures against offset 0, so the error settles at -3 ms.
    let last = r.seen.lock().unwrap().last().unwrap().error;
    assert!((last + 3e-3).abs() < 100e-6, "{last}");
    assert_eq!(r.stats.read().resyncs, 0);
}

#[test]
fn a_gap_inside_a_run_is_filled_and_keeps_alignment() {
    let mut r = rig(Script {
        // 30 ms missing after 2 s.
        gaps: vec![(96_000, 1_440)],
        ..Script::default()
    });
    r.run(500);
    let c = r.stats.read();
    assert_eq!(c.gaps_filled, 1_440);
    assert_eq!(c.resyncs, 0);
    assert!(r.max_error_after(Duration::from_millis(2_100)) < 50e-6);
}

#[test]
fn late_audio_is_silence_and_sync_is_kept() {
    let mut r = rig(Script {
        // From 2 s on the device delivers 40 ms later: 1 ms + 40 ms + 10 ms packet
        // exceeds the 50 ms latency.
        late_from: Some((96_000, Duration::from_millis(40))),
        ..Script::default()
    });
    r.run(500);
    let c = r.stats.read();
    assert!(c.late_frames > 0);
    // Whatever audio is heard is still in sync.
    assert!(r.max_error_after(Duration::from_millis(100)) < 50e-6);
}

#[test]
fn a_restarted_stream_resyncs() {
    let mut r = rig(Script {
        // 2 s missing: larger than the FIFO, so the map restarts.
        gaps: vec![(96_000, 96_000)],
        ..Script::default()
    });
    r.run(600);
    assert!(r.max_error_after(Duration::from_millis(4_200)) < 50e-6);
    assert!(r.silent_blocks_after(Duration::from_millis(4_200)) == 0);
}
