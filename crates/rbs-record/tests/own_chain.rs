//! Outputs that own their video encoder chain and reserved slots (spec
//! `recording.md`, "Outputs that own their chain", ADR-0139 decisions 8 and 9).
//! Reference adapters on a `TestClock`, as in `outputs.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_codec::RateControl;
use rbs_record::{
    MAX_CHAINS_PER_CANVAS, OutputSettings, OwnChain, RecordError, Studio, StudioSettings,
};
use support::*;

fn start(r: &Rig) -> Studio {
    let (col, canvas) = scene();
    let (engine, _clock) = engine_at_start();
    Studio::start(
        engine,
        gpu().unwrap(),
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap()
}

fn stream(own: bool) -> OutputSettings {
    let mut out = OutputSettings::packets("stream://live/app");
    out.video.as_mut().unwrap().rate_control = RateControl::Cbr {
        bitrate_bps: 2_500_000,
    };
    out.own_chain = own.then(|| OwnChain::new(300_000, 8_000_000));
    out
}

#[test]
fn an_output_that_owns_its_chain_shares_no_encoder_and_none_shares_its() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let mut studio = start(&r);
    let plain = studio.start_output(&r.adapters, &stream(false)).unwrap();
    let own = studio.start_output(&r.adapters, &stream(true)).unwrap();
    assert!(
        !studio.output(own).unwrap().video_shared,
        "same settings, own chain"
    );
    let later = studio.start_output(&r.adapters, &stream(false)).unwrap();
    assert!(studio.output(later).unwrap().video_shared);
    // The later output joined the plain chain: stopping the owner leaves it running.
    studio.stop_output(own).unwrap();
    assert_eq!(studio.outputs(), [plain, later]);
}

#[test]
fn reserved_slots_count_as_used_until_released_or_taken() {
    if gpu().is_none() {
        return;
    }
    let r = rig();
    let mut studio = start(&r);
    let ids: Vec<_> = (0..MAX_CHAINS_PER_CANVAS)
        .map(|_| studio.reserve_slot(None).unwrap())
        .collect();
    assert!(matches!(
        studio.reserve_slot(None),
        Err(RecordError::NoEncoderSlot)
    ));
    assert!(matches!(
        studio.start_output(&r.adapters, &stream(false)),
        Err(RecordError::NoEncoderSlot)
    ));
    assert!(studio.release_slot(ids[0]));
    assert!(!studio.release_slot(ids[0]), "released once");

    // A start that names its reservation takes it; a failed one gives it back.
    let mut own = stream(true);
    own.reserved = Some(ids[1]);
    own.size = Some((3, 3));
    assert!(matches!(
        studio.start_output(&r.adapters, &own),
        Err(RecordError::OutputSize { .. })
    ));
    assert_eq!(studio.reserved_slots(), MAX_CHAINS_PER_CANVAS - 1);
    own.size = None;
    studio.start_output(&r.adapters, &own).unwrap();
    assert_eq!(studio.reserved_slots(), MAX_CHAINS_PER_CANVAS - 2);
    assert!(!studio.release_slot(ids[1]), "taken by the start");
    // Two chains left reserved, one running: one free slot remains.
    studio.start_output(&r.adapters, &stream(false)).unwrap();
    assert!(matches!(
        studio.reserve_slot(None),
        Err(RecordError::NoEncoderSlot)
    ));
}
