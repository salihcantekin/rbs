//! The pause cut of a recording that shares an encoder (ADR-0124 decision 4).
//!
//! When a recording takes its video or audio packets from an encoder another output
//! (a live stream) also uses, nothing before the encoder can be cut. The recording's
//! output node cuts the shared streams' packets instead: an [`OutputCut`] names the
//! window and the streams cut at the output, and the node's [`CutGate`] drops their
//! packets inside it and stamps later packets earlier by the time left out.
//!
//! - Video, in decode order: from the first packet whose pts is at or after
//!   `pause_at` nothing passes until a keyframe whose pts is at or after `resume_at`
//!   (the keyframe the control thread asked the encoder for). Pictures that would
//!   show before that keyframe are dropped. Each dts is kept above the last one
//!   written while it stays at or below its pts.
//! - Audio: packets with pts in `[pause_at, resume_at)` are dropped; a packet that
//!   runs over `pause_at` is trimmed to end there. The join may be off by less than
//!   one audio packet, always later.
//!
//! [`NoCut`], the default, compiles to nothing. [`CutGate`] pays one relaxed load per
//! tick (has the window changed?) and, per packet, a compare and a re-stamp only once
//! a stream has time left out.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_core::media::{EncodedPacket, StreamDescriptor, StreamParams};
use rbs_core::pause::{PauseError, PauseWindow, Window};
use rbs_core::time::Timestamp;

/// A recording's cut at its output node, written by the control thread.
#[derive(Debug, Default)]
pub struct OutputCut {
    window: PauseWindow,
    /// Bit `i` set: stream `i` is cut at the output in the current window.
    streams: AtomicU64,
}

impl OutputCut {
    /// Opens a pause at `at` for the streams in `mask`. Control thread only.
    pub fn pause(&self, at: u64, mask: u64) -> Result<Window, PauseError> {
        // Written before the window: the window's release publishes it.
        self.streams.store(mask, Ordering::Relaxed);
        self.window.pause(at)
    }

    /// Ends the open pause at `at`; at or before its start it is cancelled.
    pub fn resume(&self, at: u64) -> Result<Window, PauseError> {
        self.window.resume(at)
    }

    /// The window as the gate reads it.
    pub fn window(&self) -> &PauseWindow {
        &self.window
    }
}

/// What a gate does with one packet.
#[derive(Debug)]
pub enum Gated {
    /// Written as it is.
    Pass,
    /// Dropped (inside a pause window).
    Drop,
    /// Written with new times.
    Restamped(EncodedPacket),
}

/// Decides which packets an output node writes. Chosen when the node is built.
pub trait PacketGate: Send + 'static {
    /// Once per tick, before the tick's packets.
    fn tick(&mut self);
    /// One packet of stream `stream`.
    fn pass(&mut self, stream: usize, packet: &EncodedPacket) -> Gated;
}

/// Writes every packet. No code runs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoCut;

impl PacketGate for NoCut {
    #[inline(always)]
    fn tick(&mut self) {}
    #[inline(always)]
    fn pass(&mut self, _: usize, _: &EncodedPacket) -> Gated {
        Gated::Pass
    }
}

/// Per-stream state of a [`CutGate`].
#[derive(Debug, Clone, Copy)]
struct Lane {
    video: bool,
    /// Time left out of this stream by the windows behind it.
    shift: u64,
    /// The window whose cut this stream has finished (its length is in `shift`).
    done: u64,
    /// Video only: dropping until the join keyframe, since this window's pause.
    cutting: Option<Window>,
    /// The last dts written, for the per-packet clamp.
    last_dts: Option<u64>,
}

/// Cuts the streams an [`OutputCut`] names.
#[derive(Debug)]
pub struct CutGate {
    cut: Arc<OutputCut>,
    window: Window,
    mask: u64,
    lanes: Vec<Lane>,
    /// Packets dropped inside windows.
    pub(crate) dropped: Arc<AtomicU64>,
}

impl CutGate {
    /// A gate over `streams` following `cut`.
    pub fn new(cut: Arc<OutputCut>, streams: &[StreamDescriptor]) -> Self {
        Self {
            cut,
            window: Window::NONE,
            mask: 0,
            lanes: streams
                .iter()
                .map(|s| Lane {
                    video: matches!(s.params, StreamParams::Video { .. }),
                    shift: 0,
                    done: 0,
                    cutting: None,
                    last_dts: None,
                })
                .collect(),
            dropped: Arc::default(),
        }
    }

    /// Packets dropped inside pause windows.
    pub fn dropped(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

/// When the pause `cut` ended, judged from a later window `w`: its own `resume_at`
/// when `w` is the same pause, else from the time `w` says was left out before it.
fn resume_of(cut: &Window, w: &Window) -> u64 {
    if w.pause_at == cut.pause_at {
        w.resume_at
    } else if w.before > cut.before {
        cut.pause_at.saturating_add(w.before - cut.before)
    } else {
        u64::MAX
    }
}

impl CutGate {
    fn restamp(lane: &mut Lane, packet: &EncodedPacket, end: Option<u64>) -> Gated {
        if lane.shift == 0 && end.is_none() {
            lane.last_dts = Some(packet.dts().as_nanos());
            return Gated::Pass;
        }
        let pts = packet.pts().as_nanos().saturating_sub(lane.shift);
        let mut dts = packet.dts().as_nanos().saturating_sub(lane.shift);
        if let Some(last) = lane.last_dts
            && dts <= last
            && last < pts
        {
            dts = last + 1;
        }
        lane.last_dts = Some(dts);
        let Ok(mut out) = EncodedPacket::new(
            packet.codec(),
            packet.bitstream(),
            Timestamp::from_nanos(pts),
            Timestamp::from_nanos(dts.min(pts)),
            packet.keyframe(),
            packet.data().clone(),
        ) else {
            return Gated::Drop;
        };
        let duration = match end {
            Some(end) => Some(Duration::from_nanos(
                end.saturating_sub(packet.pts().as_nanos()),
            )),
            None => packet.duration(),
        };
        if let Some(d) = duration {
            out = out.with_duration(d);
        }
        Gated::Restamped(out)
    }
}

impl PacketGate for CutGate {
    fn tick(&mut self) {
        let pause = self.cut.window();
        if pause.version() != self.window.epoch.wrapping_mul(2)
            && let Some(w) = pause.read()
        {
            self.window = w;
            self.mask = self.cut.streams.load(Ordering::Relaxed);
        }
    }

    fn pass(&mut self, stream: usize, packet: &EncodedPacket) -> Gated {
        let w = self.window;
        let cut = self.mask & (1 << (stream & 63)) != 0;
        let Some(lane) = self.lanes.get_mut(stream) else {
            return Gated::Pass;
        };
        let pts = packet.pts().as_nanos();
        if lane.video {
            if let Some(c) = lane.cutting {
                let resume = resume_of(&c, &w);
                if resume != u64::MAX && pts >= resume && packet.keyframe() {
                    lane.shift += resume - c.pause_at;
                    lane.done = w.epoch;
                    lane.cutting = None;
                    return Self::restamp(lane, packet, None);
                }
                if w.pause_at == u64::MAX && c.epoch < w.epoch && w.before == c.before {
                    // The pause was cancelled: nothing was left out.
                    lane.cutting = None;
                    return Self::restamp(lane, packet, None);
                }
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return Gated::Drop;
            }
            if cut && lane.done != w.epoch && w.pause_at != u64::MAX && pts >= w.pause_at {
                if w.resume_at != u64::MAX && pts >= w.resume_at && packet.keyframe() {
                    lane.shift += w.resume_at - w.pause_at;
                    lane.done = w.epoch;
                    return Self::restamp(lane, packet, None);
                }
                lane.cutting = Some(w);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return Gated::Drop;
            }
            return Self::restamp(lane, packet, None);
        }
        if !cut || lane.done == w.epoch || w.pause_at == u64::MAX {
            return Self::restamp(lane, packet, None);
        }
        if pts < w.pause_at {
            let runs_over = packet
                .duration()
                .is_some_and(|d| pts.saturating_add(d.as_nanos() as u64) > w.pause_at);
            return Self::restamp(lane, packet, runs_over.then_some(w.pause_at));
        }
        if pts < w.resume_at {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Gated::Drop;
        }
        lane.shift += w.resume_at - w.pause_at;
        lane.done = w.epoch;
        Self::restamp(lane, packet, None)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use super::*;
    use rbs_core::media::{AudioFormat, BitstreamFormat, Bytes, CodecId, PixelFormat, Size};
    use rbs_core::media::{ColorSpace, VideoFormat};
    use rbs_core::timing::FrameRate;

    const MS: u64 = 1_000_000;

    fn streams() -> Vec<StreamDescriptor> {
        vec![
            StreamDescriptor::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                Bytes::new(),
                StreamParams::video(
                    VideoFormat::new(
                        Size::new(16, 16),
                        PixelFormat::Nv12,
                        ColorSpace::BT709_LIMITED,
                    ),
                    FrameRate::FPS_30,
                ),
            ),
            StreamDescriptor::new(
                CodecId::Aac,
                BitstreamFormat::Raw,
                Bytes::new(),
                StreamParams::audio(AudioFormat::STEREO_48K, 1024),
            ),
        ]
    }

    fn video(pts_ms: u64, dts_ms: u64, key: bool) -> EncodedPacket {
        EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            Timestamp::from_nanos(pts_ms * MS),
            Timestamp::from_nanos(dts_ms * MS),
            key,
            Bytes::from_static(b"v"),
        )
        .unwrap()
    }

    fn audio(pts_ms: u64) -> EncodedPacket {
        EncodedPacket::new(
            CodecId::Aac,
            BitstreamFormat::Raw,
            Timestamp::from_nanos(pts_ms * MS),
            Timestamp::from_nanos(pts_ms * MS),
            true,
            Bytes::from_static(b"a"),
        )
        .unwrap()
        .with_duration(Duration::from_millis(20))
    }

    /// (pts, dts, duration) in ms of what the gate writes, or `None` for a drop.
    fn run(gate: &mut CutGate, stream: usize, p: &EncodedPacket) -> Option<(u64, u64, u64)> {
        let ms = |t: Timestamp| t.as_nanos() / MS;
        let dur = |p: &EncodedPacket| p.duration().map_or(0, |d| d.as_millis() as u64);
        match gate.pass(stream, p) {
            Gated::Pass => Some((ms(p.pts()), ms(p.dts()), dur(p))),
            Gated::Drop => None,
            Gated::Restamped(q) => Some((ms(q.pts()), ms(q.dts()), dur(&q))),
        }
    }

    #[test]
    fn a_shared_video_stream_resumes_at_the_join_keyframe_and_audio_is_trimmed() {
        let cut = Arc::new(OutputCut::default());
        let mut gate = CutGate::new(Arc::clone(&cut), &streams());
        gate.tick();
        assert_eq!(run(&mut gate, 0, &video(0, 0, true)), Some((0, 0, 0)));
        cut.pause(100 * MS, 0b11).unwrap();
        gate.tick();
        assert_eq!(run(&mut gate, 0, &video(66, 66, false)), Some((66, 66, 0)));
        // Audio running over the pause is trimmed to end at it.
        assert_eq!(run(&mut gate, 1, &audio(90)), Some((90, 90, 10)));
        assert_eq!(run(&mut gate, 0, &video(100, 100, false)), None);
        assert_eq!(run(&mut gate, 1, &audio(110)), None);
        cut.resume(400 * MS).unwrap();
        gate.tick();
        // Not a keyframe yet, then the join keyframe: 300 ms left out.
        assert_eq!(run(&mut gate, 0, &video(400, 400, false)), None);
        assert_eq!(run(&mut gate, 1, &audio(390)), None);
        assert_eq!(
            run(&mut gate, 0, &video(433, 433, true)),
            Some((133, 133, 0))
        );
        assert_eq!(run(&mut gate, 1, &audio(410)), Some((110, 110, 20)));
        assert_eq!(
            run(&mut gate, 0, &video(466, 466, false)),
            Some((166, 166, 0))
        );
    }

    #[test]
    fn streams_not_named_pass_and_a_cancelled_pause_leaves_out_nothing() {
        let cut = Arc::new(OutputCut::default());
        let mut gate = CutGate::new(Arc::clone(&cut), &streams());
        // Only video is cut at the output; audio was cut before its own encoder.
        cut.pause(100 * MS, 0b01).unwrap();
        gate.tick();
        assert_eq!(run(&mut gate, 1, &audio(150)), Some((150, 150, 20)));
        assert_eq!(run(&mut gate, 0, &video(50, 50, true)), Some((50, 50, 0)));
        cut.resume(80 * MS).unwrap(); // Before the pause: cancelled.
        gate.tick();
        assert_eq!(
            run(&mut gate, 0, &video(100, 100, false)),
            Some((100, 100, 0))
        );
    }

    #[test]
    fn dts_stays_above_the_last_one_across_a_join() {
        let cut = Arc::new(OutputCut::default());
        let mut gate = CutGate::new(Arc::clone(&cut), &streams());
        cut.pause(100 * MS, 0b01).unwrap();
        gate.tick();
        // A B-frame stream: pts ahead of dts.
        assert_eq!(run(&mut gate, 0, &video(90, 60, false)), Some((90, 60, 0)));
        assert_eq!(run(&mut gate, 0, &video(120, 90, false)), None);
        cut.resume(200 * MS).unwrap();
        gate.tick();
        // The join keyframe's dts would go back to 0 after the shift: clamped.
        let (pts, dts, _) = run(&mut gate, 0, &video(200, 100, true)).unwrap();
        assert_eq!(pts, 100);
        // One nanosecond above the last dts (60 ms), shown here in whole ms.
        assert!(dts >= 60 && dts <= pts, "{dts}");
    }

    #[test]
    fn gating_does_not_allocate_beyond_the_packet() {
        let cut = Arc::new(OutputCut::default());
        let mut gate = CutGate::new(Arc::clone(&cut), &streams());
        let packets: Vec<_> = (0..50).map(|i| audio(i * 20)).collect();
        let counted = allocation_counter::measure(|| {
            gate.tick();
            for p in &packets {
                std::hint::black_box(gate.pass(1, p));
            }
        });
        assert_eq!(counted.count_total, 0);
    }
}
