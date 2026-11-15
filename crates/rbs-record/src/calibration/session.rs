//! One measured encoder session: submits the ring's frames with strictly increasing
//! timestamps, retries a `Busy` within the round, drains, and keeps the counts the
//! late rules read. Allocation-free per frame once the packet vector has grown.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rbs_codec::{EncodeError, VideoEncoder};
use rbs_core::media::{Bytes, EncodedPacket, PlaneLayout, SystemFrame, VideoFormat, VideoFrame};
use rbs_core::time::{Clock, Timestamp};

use super::LOST_WINDOW;
use super::driver::Pacer;

/// Bits of the lost-frame window: `2 × 240` judged frames plus the tail.
const SEEN_BITS: u64 = 512;

/// `frames` frames of a moving picture in `format` (planar layouts with tight rows),
/// each different: a diagonal gradient that shifts each frame and a bright block that
/// moves across it, which costs an encoder about what a screen or a camera does (the
/// owner's choice of 2026-11-15, ADR-0087 Review: noise cost VideoToolbox about 40 %
/// more than any real picture). `None` when the format has no CPU layout.
pub fn moving_ring(format: VideoFormat, frames: usize) -> Option<Arc<[SystemFrame]>> {
    let pf = format.pixel_format;
    let (width, height) = (format.size.width, format.size.height);
    let mut ring = Vec::with_capacity(frames);
    for f in 0..frames {
        let mut data = Vec::new();
        let mut planes = Vec::new();
        for plane in 0..pf.plane_count() {
            let stride = usize::try_from(pf.min_row_bytes(plane, width)?).ok()?;
            let rows = pf.plane_rows(plane, height)? as usize;
            planes.push(PlaneLayout {
                offset: data.len(),
                stride,
            });
            data.reserve(stride * rows);
            let shift = f * 24;
            for y in 0..rows {
                for x in 0..stride {
                    let v = if plane > 0 {
                        // Chroma: a gentle gradient around the middle.
                        (112 + (x + y + shift) % 32) as u8
                    } else if (x + shift) % stride < stride / 6 && y > rows / 3 && y < rows / 2 {
                        235
                    } else {
                        ((x + y + shift) % 220 + 16) as u8
                    };
                    data.push(v);
                }
            }
        }
        ring.push(SystemFrame::new(format, Bytes::from(data), &planes).ok()?);
    }
    Some(ring.into())
}

/// What one frame of a round did.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum FrameOutcome {
    /// Accepted.
    #[default]
    Accepted,
    /// `Busy` until the round's deadline: the round is late.
    Late,
    /// The stop flag was set during a retry.
    Stopped,
    /// The encoder failed.
    Error(EncodeError),
}

/// One session's answer for one round.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionReport {
    /// What the frame did.
    pub outcome: FrameOutcome,
    /// Frames submitted and not yet drained as a packet.
    pub backlog: u64,
    /// A frame older than the lost window has no packet.
    pub lost: bool,
    /// `submit` plus `drain` time of the frame, without `Busy` retries.
    pub call_time: Duration,
}

/// One measured session.
pub struct SessionRunner {
    encoder: Box<dyn VideoEncoder>,
    ring: Arc<[SystemFrame]>,
    period_ns: u64,
    /// Frames submitted (the next frame's index).
    submitted: u64,
    /// Packets drained.
    drained: u64,
    packets: Vec<EncodedPacket>,
    /// Bit `i % SEEN_BITS` set when frame `i` has a packet.
    seen: [u64; (SEEN_BITS / 64) as usize],
    /// Frames below this index are checked.
    checked: u64,
    /// The newest frame index with a packet, plus one.
    newest: u64,
    lost: bool,
}

impl SessionRunner {
    /// A session over `encoder` that feeds `ring` at `period`.
    pub fn new(encoder: Box<dyn VideoEncoder>, ring: Arc<[SystemFrame]>, period: Duration) -> Self {
        Self {
            encoder,
            ring,
            period_ns: u64::try_from(period.as_nanos()).unwrap_or(u64::MAX).max(1),
            submitted: 0,
            drained: 0,
            packets: Vec::with_capacity(8),
            seen: [0; (SEEN_BITS / 64) as usize],
            checked: 0,
            newest: 0,
            lost: false,
        }
    }

    /// Frames submitted and not yet drained.
    pub fn backlog(&self) -> u64 {
        self.submitted.saturating_sub(self.drained)
    }

    fn take_packets(&mut self) {
        for p in self.packets.drain(..) {
            self.drained += 1;
            let index = p.pts().as_nanos() / self.period_ns;
            if index < self.checked {
                continue;
            }
            if index >= self.checked + SEEN_BITS {
                // Further ahead than the window: the frames in between cannot all
                // have been seen.
                self.lost = true;
                continue;
            }
            self.seen[((index % SEEN_BITS) / 64) as usize] |= 1 << (index % 64);
            self.newest = self.newest.max(index + 1);
        }
        // Every frame older than the window must have its packet.
        let horizon = self.newest.saturating_sub(LOST_WINDOW);
        while self.checked < horizon {
            let i = self.checked;
            let word = ((i % SEEN_BITS) / 64) as usize;
            let bit = 1 << (i % 64);
            if self.seen[word] & bit == 0 {
                self.lost = true;
            }
            self.seen[word] &= !bit;
            self.checked += 1;
        }
        // A backlog wider than the window is a loss the counts cannot place.
        if self.submitted.saturating_sub(self.checked) > SEEN_BITS {
            self.lost = true;
        }
    }

    fn drain(&mut self) -> Result<(), EncodeError> {
        let r = self.encoder.drain(&mut self.packets);
        self.take_packets();
        r
    }

    /// Submits the next frame and drains. A `Busy` is retried (drain, yield) until
    /// the frame is accepted or `deadline` passes (late); the stop flag ends a retry.
    pub fn encode(
        &mut self,
        clock: &dyn Clock,
        pacer: &dyn Pacer,
        deadline: Timestamp,
        stop: &AtomicBool,
    ) -> SessionReport {
        let index = self.submitted;
        let frame = VideoFrame::system(
            Timestamp::from_nanos(index.saturating_mul(self.period_ns)),
            self.ring[(index % self.ring.len() as u64) as usize].clone(),
            None,
        );
        let mut call = Duration::ZERO;
        let outcome = loop {
            let t = clock.now();
            let r = self.encoder.submit(&frame);
            call += clock.now().saturating_since(t);
            match r {
                Ok(()) => {
                    self.submitted += 1;
                    let t = clock.now();
                    let d = self.drain();
                    call += clock.now().saturating_since(t);
                    break match d {
                        Ok(()) => FrameOutcome::Accepted,
                        Err(e) => FrameOutcome::Error(e),
                    };
                }
                Err(EncodeError::Busy) => {
                    // Room comes only from a drain on some adapters: drain, then try
                    // again while the round can still be on time.
                    if let Err(e) = self.drain() {
                        break FrameOutcome::Error(e);
                    }
                    if stop.load(Ordering::Relaxed) {
                        break FrameOutcome::Stopped;
                    }
                    if clock.now() > deadline {
                        break FrameOutcome::Late;
                    }
                    pacer.yield_now();
                }
                Err(e) => break FrameOutcome::Error(e),
            }
        };
        SessionReport {
            outcome,
            backlog: self.backlog(),
            lost: self.lost,
            call_time: call,
        }
    }

    /// Encodes every pending frame and checks that each submitted frame has its packet.
    /// Returns whether a frame was lost.
    pub fn flush(&mut self) -> Result<bool, EncodeError> {
        let r = self.encoder.flush(&mut self.packets);
        self.take_packets();
        r?;
        // After a flush every submitted frame has produced one packet.
        let lost = self.lost || self.drained < self.submitted || self.unseen_tail();
        Ok(lost)
    }

    fn unseen_tail(&self) -> bool {
        (self.checked..self.submitted)
            .any(|i| self.seen[((i % SEEN_BITS) / 64) as usize] & (1 << (i % 64)) == 0)
    }
}
