//! The video encoder and output nodes on a CPU-only partition (spec `io-nodes.md`,
//! "Video encoder node" and "Output node"), with fake ports. Ticks are driven by hand;
//! nothing depends on timing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use rbs_core::lifecycle::Lifecycle;
use rbs_core::media::{Bytes, ContainerId};
use rbs_core::time::Timestamp;
use rbs_core::timing::{Cadence, FrameRate};
use rbs_graph::runtime::{PER_TICK_EVENTS, Tick};
use rbs_nodes::encode::CARRY_PACKETS;
use rbs_nodes::{OutputNode, VideoEncoderNode};
use support::{
    EncoderState, FakeEncoderFactory, FakeMuxerFactory, FakeTransportFactory, FrameSource, Got,
    Rig, TransportState, encoder_config, file_config,
};

struct Chain {
    rig: Rig,
    encoder: rbs_graph::description::NodeId,
    output: rbs_graph::description::NodeId,
    enc_state: Arc<EncoderState>,
    tx_state: Arc<TransportState>,
    enc_stats: Arc<rbs_nodes::encode::EncoderNodeStats>,
    out_stats: Arc<rbs_nodes::output::OutputNodeStats>,
}

/// source (new frame every `every` ticks) -> encoder -> output (fake muxer + transport).
fn chain(every: u64, packets_per_frame: u64, backlog: usize, tx: Arc<TransportState>) -> Chain {
    let mut rig = Rig::new();
    let enc_state = Arc::new(EncoderState::default());
    enc_state
        .packets_per_frame
        .store(packets_per_frame, Ordering::Relaxed);
    let encoder = VideoEncoderNode::open(
        Arc::new(FakeEncoderFactory(Arc::clone(&enc_state))),
        encoder_config(),
    )
    .unwrap();
    let stream = encoder.stream().clone();
    let enc_stats = encoder.stats();
    let output = OutputNode::new(
        vec![stream],
        Arc::new(FakeTransportFactory(Arc::clone(&tx))),
        file_config(),
    )
    .with_muxer(Arc::new(FakeMuxerFactory), ContainerId::Matroska, None)
    .with_backlog(backlog);
    let out_stats = output.stats();

    let source = rig.add(Box::new(FrameSource::new(every)));
    let encoder = rig.add(Box::new(encoder));
    let output = rig.add(Box::new(output));
    rig.graph
        .connect(source, "video", encoder, "video")
        .unwrap();
    rig.graph.connect(encoder, "packets", output, "s0").unwrap();
    let report = rig.apply();
    assert!(report.failed.is_empty(), "{report:?}");
    Chain {
        rig,
        encoder,
        output,
        enc_state,
        tx_state: tx,
        enc_stats,
        out_stats,
    }
}

fn media_times(got: &[Got]) -> Vec<u64> {
    got.iter()
        .filter_map(|g| match g {
            Got::Media { timestamp, .. } => Some(timestamp.as_nanos()),
            _ => None,
        })
        .collect()
}

fn frame_time(index: u64) -> u64 {
    Tick::of(&Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60), index)
        .time
        .as_nanos()
}

#[test]
fn each_new_frame_is_encoded_once_and_the_file_gets_header_media_trailer() {
    let mut c = chain(2, 1, 64, TransportState::logging());
    // The header is written when the output joins, before any tick.
    assert_eq!(c.tx_state.got(), vec![Got::Header]);
    c.rig.ticks(10);
    // Draining runs one final tick (10) on the old schedule, then releases.
    c.rig.drain();

    // Frames 0, 2, ..., 10: each submitted once although it stays on its slot.
    assert_eq!(c.enc_state.submitted.load(Ordering::Relaxed), 6);
    let got = c.tx_state.got();
    assert_eq!(got.first(), Some(&Got::Header));
    assert_eq!(got.last(), Some(&Got::Trailer));
    assert_eq!(
        media_times(&got),
        [0, 2, 4, 6, 8, 10].map(frame_time).to_vec(),
        "{got:?}"
    );
    assert_eq!(c.tx_state.finished.load(Ordering::Relaxed), 1);
    assert_eq!(c.enc_stats.refused.load(Ordering::Relaxed), 0);
}

#[test]
fn a_full_transport_queue_is_backlogged_and_delivered_in_order() {
    let mut c = chain(1, 1, 64, TransportState::logging());
    c.rig.ticks(3);
    c.tx_state.full.store(true, Ordering::Relaxed);
    c.rig.ticks(5);
    c.tx_state.full.store(false, Ordering::Relaxed);
    c.rig.ticks(2);
    c.rig.drain();

    assert_eq!(c.out_stats.backlog_peak.load(Ordering::Relaxed), 5);
    let got = c.tx_state.got();
    assert_eq!(
        media_times(&got),
        (0..=10).map(frame_time).collect::<Vec<_>>()
    );
    assert_eq!(got.last(), Some(&Got::Trailer));
    let health = c.rig.exec.health(c.output);
    assert!(health.is_none_or(|h| h.errors == 0), "{health:?}");
}

#[test]
fn a_backlog_overflow_stops_the_output_instead_of_writing_holes() {
    let mut c = chain(1, 1, 2, TransportState::logging());
    c.tx_state.full.store(true, Ordering::Relaxed);
    c.rig.ticks(4);
    let health = c.rig.exec.health(c.output).unwrap();
    assert!(health.permanent && health.bypassed, "{health:?}");
    assert_eq!(c.out_stats.backlog_peak.load(Ordering::Relaxed), 2);
}

#[test]
fn packets_past_the_per_tick_limit_are_carried_over_in_order() {
    // 40 packets per frame against 16 per tick: the carry-over fills and empties.
    let per_frame = 40;
    assert!(per_frame as usize > PER_TICK_EVENTS && (per_frame as usize) < CARRY_PACKETS);
    let mut c = chain(8, per_frame, 4096, TransportState::logging());
    // The final drain tick (23) brings no new frame.
    c.rig.ticks(23);
    c.rig.drain();

    let times = media_times(&c.tx_state.got());
    assert_eq!(times.len(), 3 * per_frame as usize, "frames 0, 8 and 16");
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "out of order: {times:?}"
    );
    assert_eq!(c.enc_stats.overflow_drops.load(Ordering::Relaxed), 0);
    assert_eq!(c.enc_stats.flush_lost.load(Ordering::Relaxed), 0);
}

#[test]
fn a_carry_overflow_drops_to_the_next_keyframe_and_asks_for_one() {
    // 200 packets per frame on every tick overflow the carry-over.
    let mut c = chain(1, 200, 1 << 16, TransportState::logging());
    c.rig.ticks(6);
    assert!(c.enc_stats.overflow_drops.load(Ordering::Relaxed) > 0);
    assert!(c.enc_state.keyframe_requests.load(Ordering::Relaxed) > 0);
    // Every media chunk after the first gap starts a run that begins with a keyframe.
    let got = c.tx_state.got();
    let media: Vec<(u64, bool)> = got
        .iter()
        .filter_map(|g| match g {
            Got::Media { timestamp, key } => Some((timestamp.as_nanos(), *key)),
            _ => None,
        })
        .collect();
    assert!(media[0].1, "the stream starts at a keyframe");
    // (frame index, packet index within the frame) of a media time.
    let place = |t: u64| {
        let f = (0..64).find(|&f| (frame_time(f)..frame_time(f) + 200).contains(&t));
        let f = f.expect("a packet of a known frame");
        (f, t - frame_time(f))
    };
    let mut gaps = 0;
    for pair in media.windows(2) {
        let ((f0, i0), ((f1, i1), key)) = (place(pair[0].0), (place(pair[1].0), pair[1].1));
        let contiguous = (f1 == f0 && i1 == i0 + 1) || (f1 == f0 + 1 && i0 == 199 && i1 == 0);
        if !contiguous {
            gaps += 1;
            assert!(key, "resumed without a keyframe at frame {f1} packet {i1}");
        }
    }
    assert!(gaps > 0, "the test expects at least one gap");
}

#[test]
fn a_restarted_encoder_with_another_stream_fails_permanently() {
    let mut c = chain(1, 1, 64, TransportState::logging());
    c.rig.ticks(2);
    // Standby releases the session; Active again prepares a new one.
    *c.enc_state.extradata.lock().unwrap() = Bytes::from_static(b"other sps");
    c.rig
        .graph
        .set_state(c.encoder, Lifecycle::Standby)
        .unwrap();
    c.rig.apply();
    c.rig.graph.set_state(c.encoder, Lifecycle::Active).unwrap();
    let report = c.rig.apply();
    let failed = report.failed.iter().find(|f| f.0 == c.encoder);
    assert!(failed.is_some(), "{report:?}");
    assert_eq!(c.enc_state.sessions.load(Ordering::Relaxed), 2);
}

#[test]
fn steady_state_ticks_do_not_allocate() {
    let tx = Arc::new(TransportState::default()); // no logging
    let mut c = chain(1, 1, 64, tx);
    c.rig.ticks(4);
    let cadence = Cadence::frames(Timestamp::ZERO, FrameRate::FPS_60);
    let start = c.rig.tick;
    let steady = allocation_counter::measure(|| {
        for i in start..start + 200 {
            c.rig.exec.run_tick(Tick::of(&cadence, i));
        }
    });
    assert_eq!(steady.count_total, 0, "steady-state ticks allocated");
    assert_eq!(c.enc_state.submitted.load(Ordering::Relaxed), 204);
    // Header + one chunk per frame.
    assert_eq!(c.tx_state.sent.load(Ordering::Relaxed), 1 + 204);
}
