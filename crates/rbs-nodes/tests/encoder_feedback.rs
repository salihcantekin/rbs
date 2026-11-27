//! Encoder feedback from an output that owns its encoder chain (ADR-0139 decisions 8
//! and 9, phase 1a): the transport's keyframe requests and bitrate hints reach the
//! encoder session. Ticks are driven by hand on the 60 fps cadence; nothing waits.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rbs_codec::RateControl;
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::runtime::Tick;
use rbs_nodes::encode::EncoderNodeStats;
use rbs_nodes::output::OutputNodeStats;
use rbs_nodes::{BitrateLimiter, EncoderFeedback, OutputNode, VideoEncoderNode};
use rbs_transport::{TransportConfig, TransportEvent, TransportInput};
use support::{
    EncoderState, FakeEncoderFactory, FakeTransportFactory, FrameSource, Rig, TransportState,
    encoder_config,
};

/// Ticks in one second at 60 fps.
const SECOND: u64 = 60;

struct Chain {
    rig: Rig,
    enc: Arc<EncoderState>,
    tx: Arc<TransportState>,
    enc_stats: Arc<EncoderNodeStats>,
    out_stats: Arc<OutputNodeStats>,
}

/// source (a new frame every tick) -> encoder -> output, with feedback when `wired`.
fn chain(wired: bool) -> Chain {
    chain_on(wired, TransportState::logging())
}

fn chain_on(wired: bool, tx: Arc<TransportState>) -> Chain {
    let mut rig = Rig::new();
    let enc = Arc::new(EncoderState::default());
    enc.packets_per_frame.store(1, Ordering::Relaxed);
    let encoder = VideoEncoderNode::open(
        Arc::new(FakeEncoderFactory(Arc::clone(&enc))),
        encoder_config(),
    )
    .unwrap();
    let output_for = |stream| {
        OutputNode::new(
            vec![stream],
            Arc::new(FakeTransportFactory(Arc::clone(&tx))),
            TransportConfig::new("webrtc://peer.invalid/x", TransportInput::Packets),
        )
    };
    let (encoder, output) = if wired {
        let (encoder, keyframes) = encoder.with_keyframe_requests();
        let (encoder, bitrate) = encoder.with_bitrate_changes();
        let limiter = BitrateLimiter::new(1_000_000, 300_000, 8_000_000).unwrap();
        let feedback = EncoderFeedback::new()
            .with_keyframes(keyframes)
            .with_bitrate(bitrate, limiter);
        let output = output_for(encoder.stream().clone()).with_encoder_feedback(feedback);
        (encoder, output)
    } else {
        let output = output_for(encoder.stream().clone());
        (encoder, output)
    };
    let enc_stats = encoder.stats();
    let out_stats = output.stats();
    let source = rig.add(Box::new(FrameSource::new(1)));
    let encoder = rig.add(Box::new(encoder));
    let output = rig.add(Box::new(output));
    rig.graph
        .connect(source, "video", encoder, "video")
        .unwrap();
    rig.graph.connect(encoder, "packets", output, "s0").unwrap();
    let report = rig.apply();
    assert!(report.failed.is_empty(), "{report:?}");
    rig.ticks(2);
    Chain {
        rig,
        enc,
        tx,
        enc_stats,
        out_stats,
    }
}

impl Chain {
    fn event(&mut self, event: TransportEvent) {
        self.tx.events.lock().unwrap().push(event);
        // The output sees it on the next tick; the encoder applies it on the frame after.
        self.rig.ticks(3);
    }

    fn keyframes(&self) -> u64 {
        self.enc.keyframe_requests.load(Ordering::Relaxed)
    }
}

#[test]
fn a_keyframe_request_reaches_the_encoder_at_most_once_per_second() {
    let mut c = chain(true);
    c.event(TransportEvent::KeyframeRequested);
    assert_eq!(c.keyframes(), 1);
    assert_eq!(c.enc_stats.keyframes_requested.load(Ordering::Relaxed), 1);
    // The frame right after the request is the keyframe (a PLI gives a keyframe on the
    // next encoded frame, ADR-0139 decision 16).
    assert_eq!(c.enc.forced.lock().unwrap().len(), 1);

    c.event(TransportEvent::KeyframeRequested);
    assert_eq!(
        c.keyframes(),
        1,
        "a second request within a second is not forwarded"
    );
    c.rig.ticks(SECOND);
    c.event(TransportEvent::KeyframeRequested);
    assert_eq!(c.keyframes(), 2);
    assert_eq!(c.out_stats.keyframe_requests.load(Ordering::Relaxed), 3);
    assert_eq!(c.out_stats.keyframes_forwarded.load(Ordering::Relaxed), 2);
}

#[test]
fn a_bitrate_hint_reaches_the_encoder_through_the_limiter() {
    let mut c = chain(true);
    c.event(TransportEvent::BitrateHint { bps: 500_000 });
    assert_eq!(
        *c.enc.bitrates.lock().unwrap(),
        vec![RateControl::Cbr {
            bitrate_bps: 500_000
        }]
    );
    assert_eq!(c.enc_stats.bitrate_changes.load(Ordering::Relaxed), 1);
    assert_eq!(c.enc_stats.bitrate_bps.load(Ordering::Relaxed), 500_000);

    // Under the floor: clamped to it, but not within 500 ms of the last change.
    c.event(TransportEvent::BitrateHint { bps: 1 });
    assert_eq!(c.enc.bitrates.lock().unwrap().len(), 1);
    c.rig.ticks(SECOND / 2);
    c.event(TransportEvent::BitrateHint { bps: 1 });
    assert_eq!(
        c.enc.bitrates.lock().unwrap().last(),
        Some(&RateControl::Cbr {
            bitrate_bps: 300_000
        })
    );
    // Up by at most 10 % per step.
    c.rig.ticks(SECOND / 2);
    c.event(TransportEvent::BitrateHint { bps: u64::MAX });
    assert_eq!(
        c.enc.bitrates.lock().unwrap().last(),
        Some(&RateControl::Cbr {
            bitrate_bps: 330_000
        })
    );
    assert_eq!(c.out_stats.bitrate_hints.load(Ordering::Relaxed), 4);
    assert_eq!(c.out_stats.bitrate_steps.load(Ordering::Relaxed), 3);
}

#[test]
fn a_refused_bitrate_change_keeps_the_stream_going() {
    let mut c = chain(true);
    c.enc.refuse_reconfigure.store(true, Ordering::Relaxed);
    let before = c.enc.submitted.load(Ordering::Relaxed);
    c.event(TransportEvent::BitrateHint { bps: 500_000 });
    assert_eq!(c.enc_stats.bitrate_refused.load(Ordering::Relaxed), 1);
    assert_eq!(c.enc_stats.bitrate_changes.load(Ordering::Relaxed), 0);
    assert!(
        c.enc.submitted.load(Ordering::Relaxed) > before,
        "frames still encode"
    );
}

#[test]
fn an_output_without_feedback_forwards_nothing() {
    let mut c = chain(false);
    c.event(TransportEvent::KeyframeRequested);
    c.event(TransportEvent::BitrateHint { bps: 500_000 });
    assert_eq!(c.keyframes(), 0);
    assert!(c.enc.bitrates.lock().unwrap().is_empty());
    assert_eq!(c.out_stats.keyframe_requests.load(Ordering::Relaxed), 1);
    assert_eq!(c.out_stats.bitrate_hints.load(Ordering::Relaxed), 1);
    assert_eq!(c.out_stats.keyframes_forwarded.load(Ordering::Relaxed), 0);
    assert_eq!(c.out_stats.bitrate_steps.load(Ordering::Relaxed), 0);
}

#[test]
fn feedback_adds_no_allocation_per_tick() {
    let tx = Arc::new(TransportState::default()); // no logging
    let mut c = chain_on(true, tx);
    // Warm the event buffers and the fake's records once (the test's, not the node's).
    c.event(TransportEvent::KeyframeRequested);
    c.event(TransportEvent::BitrateHint { bps: 500_000 });
    c.enc.bitrates.lock().unwrap().reserve(64);
    c.enc.forced.lock().unwrap().reserve(64);
    c.tx.events.lock().unwrap().reserve(8);
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let start = c.rig.tick;
    let steady = allocation_counter::measure(|| {
        for i in start..start + 240 {
            if i % 40 == 0 {
                let mut events = c.tx.events.lock().unwrap();
                events.push(TransportEvent::KeyframeRequested);
                events.push(TransportEvent::BitrateHint { bps: 400_000 + i });
            }
            c.rig.exec.run_tick(Tick::of(&cadence, i));
        }
    });
    assert_eq!(steady.count_total, 0, "ticks with feedback allocated");
    assert!(c.keyframes() >= 3);
    assert!(c.enc_stats.bitrate_changes.load(Ordering::Relaxed) >= 3);
}
