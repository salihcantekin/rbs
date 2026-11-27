//! Encoder feedback from an output's transport (ADR-0139 decisions 8 and 9, phase 1a):
//! a peer's keyframe request (`TransportEvent::KeyframeRequested`, a WebRTC PLI) and its
//! congestion estimate (`TransportEvent::BitrateHint`) reach the encoder.
//!
//! Only an output that owns its encoder chain gets feedback (ADR-0047 decision 6,
//! ADR-0127 decision 4: a shared encoder never follows one output). The studio builds
//! the output with [`OutputNode::with_encoder_feedback`](crate::OutputNode) and the
//! encoder with [`VideoEncoderNode::with_bitrate_changes`](crate::VideoEncoderNode);
//! an output or encoder built without them runs no feedback code (P13).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rbs_codec::RateControl;
use rbs_core::time::Timestamp;

use crate::encode::KeyframeAt;

/// Shortest time between two keyframes forwarded from one output (ADR-0139 decision 9).
pub const KEYFRAME_FORWARD_INTERVAL: Duration = Duration::from_secs(1);
/// Shortest time between two bitrate changes (ADR-0139 decision 9).
pub const BITRATE_CHANGE_INTERVAL: Duration = Duration::from_millis(500);
/// Largest raise of one bitrate step, in percent of the current bitrate.
pub const BITRATE_STEP_UP_PERCENT: u64 = 10;

/// A bitrate the control side (the output's limiter) wants the encoder to use. The
/// encoder node takes it before its next frame. Of two values not yet taken, the
/// later wins.
#[derive(Debug, Default)]
pub struct BitrateTarget {
    /// Bits per second not yet taken; 0 for none.
    bps: AtomicU64,
}

impl BitrateTarget {
    /// Asks for `bps` (at least 1) from the next frame on.
    pub fn request(&self, bps: u64) {
        self.bps.store(bps.max(1), Ordering::Release);
    }

    /// The value not yet taken.
    pub fn pending(&self) -> Option<u64> {
        match self.bps.load(Ordering::Acquire) {
            0 => None,
            bps => Some(bps),
        }
    }

    /// Takes the value not yet taken. One relaxed load when there is none.
    pub(crate) fn take(&self) -> Option<u64> {
        if self.bps.load(Ordering::Relaxed) == 0 {
            return None;
        }
        match self.bps.swap(0, Ordering::AcqRel) {
            0 => None,
            bps => Some(bps),
        }
    }
}

/// `rate` with its bitrate replaced by `bps`; `None` for a mode without one
/// (constant quality), which never follows the network.
pub fn with_bitrate(rate: RateControl, bps: u64) -> Option<RateControl> {
    match rate {
        RateControl::Cbr { .. } => Some(RateControl::Cbr { bitrate_bps: bps }),
        RateControl::CbrPadded { .. } => Some(RateControl::CbrPadded { bitrate_bps: bps }),
        RateControl::Vbr {
            target_bps,
            max_bps,
        } => {
            // Keep the configured peak-to-average ratio.
            let max = if target_bps == 0 {
                bps
            } else {
                u64::try_from(u128::from(bps) * u128::from(max_bps) / u128::from(target_bps))
                    .unwrap_or(u64::MAX)
                    .max(bps)
            };
            Some(RateControl::Vbr {
                target_bps: bps,
                max_bps: max,
            })
        }
        _ => None,
    }
}

/// The share's own bitrate limiter (ADR-0139 decision 9): follows the transport's
/// congestion estimate between a floor and a maximum, at most one change per
/// [`BITRATE_CHANGE_INTERVAL`], down in one step and up by at most
/// [`BITRATE_STEP_UP_PERCENT`] per step. Pure: time comes in with each call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitrateLimiter {
    floor_bps: u64,
    max_bps: u64,
    current_bps: u64,
    last_change: Option<Timestamp>,
}

impl BitrateLimiter {
    /// A limiter that starts at `start_bps`, clamped into `floor_bps..=max_bps`.
    /// `None` when the floor is 0 or above the maximum.
    pub fn new(start_bps: u64, floor_bps: u64, max_bps: u64) -> Option<Self> {
        (floor_bps > 0 && floor_bps <= max_bps).then(|| Self {
            floor_bps,
            max_bps,
            current_bps: start_bps.clamp(floor_bps, max_bps),
            last_change: None,
        })
    }

    /// The bitrate in use.
    pub fn current(&self) -> u64 {
        self.current_bps
    }

    /// Takes an estimate at `now`; the new bitrate when it changes.
    pub fn offer(&mut self, estimate_bps: u64, now: Timestamp) -> Option<u64> {
        let target = estimate_bps.clamp(self.floor_bps, self.max_bps);
        if target == self.current_bps {
            return None;
        }
        if let Some(last) = self.last_change
            && now < last + BITRATE_CHANGE_INTERVAL
        {
            return None;
        }
        let next = if target < self.current_bps {
            target
        } else {
            let step = (self.current_bps * BITRATE_STEP_UP_PERCENT / 100).max(1);
            target.min(self.current_bps.saturating_add(step))
        };
        self.current_bps = next;
        self.last_change = Some(now);
        Some(next)
    }
}

/// What an output that owns its encoder chain sends back to that encoder.
#[derive(Debug, Default)]
pub struct EncoderFeedback {
    keyframes: Option<Arc<KeyframeAt>>,
    bitrate: Option<(Arc<BitrateTarget>, BitrateLimiter)>,
    last_keyframe: Option<Timestamp>,
}

/// Counters of [`EncoderFeedback`], read by the output node's stats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Forwarded {
    pub keyframe: bool,
    pub bitrate: Option<u64>,
}

impl EncoderFeedback {
    /// Feedback that forwards nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Forwards keyframe requests to the encoder's handle
    /// ([`VideoEncoderNode::with_keyframe_requests`](crate::VideoEncoderNode)),
    /// at most one per [`KEYFRAME_FORWARD_INTERVAL`].
    pub fn with_keyframes(mut self, handle: Arc<KeyframeAt>) -> Self {
        self.keyframes = Some(handle);
        self
    }

    /// Follows the transport's bitrate hints through `limiter` into the encoder's
    /// target ([`VideoEncoderNode::with_bitrate_changes`](crate::VideoEncoderNode)).
    pub fn with_bitrate(mut self, target: Arc<BitrateTarget>, limiter: BitrateLimiter) -> Self {
        self.bitrate = Some((target, limiter));
        self
    }

    /// The limiter's current bitrate, when the bitrate is followed.
    pub fn bitrate(&self) -> Option<u64> {
        self.bitrate.as_ref().map(|(_, l)| l.current())
    }

    /// A keyframe request at master time `now`. Whether it was forwarded.
    pub(crate) fn keyframe_requested(&mut self, now: Timestamp) -> bool {
        let Some(handle) = &self.keyframes else {
            return false;
        };
        if self
            .last_keyframe
            .is_some_and(|last| now < last + KEYFRAME_FORWARD_INTERVAL)
        {
            return false;
        }
        handle.request(now);
        self.last_keyframe = Some(now);
        true
    }

    /// A bitrate hint at master time `now`. The new bitrate when it changed.
    pub(crate) fn bitrate_hint(&mut self, bps: u64, now: Timestamp) -> Option<u64> {
        let (target, limiter) = self.bitrate.as_mut()?;
        let next = limiter.offer(bps, now)?;
        target.request(next);
        Some(next)
    }

    /// Applies one transport event's feedback.
    pub(crate) fn on_event(
        &mut self,
        event: &rbs_transport::TransportEvent,
        now: Timestamp,
    ) -> Forwarded {
        match event {
            rbs_transport::TransportEvent::KeyframeRequested => Forwarded {
                keyframe: self.keyframe_requested(now),
                bitrate: None,
            },
            rbs_transport::TransportEvent::BitrateHint { bps } => Forwarded {
                keyframe: false,
                bitrate: self.bitrate_hint(*bps, now),
            },
            _ => Forwarded::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn the_limiter_stays_within_its_bounds() {
        let mut l = BitrateLimiter::new(2_500_000, 300_000, 8_000_000).unwrap();
        assert_eq!(
            l.offer(10, at(0)),
            Some(300_000),
            "down at once, to the floor"
        );
        assert_eq!(l.offer(u64::MAX, at(10_000)), Some(330_000), "up by 10 %");
        let mut t = 10_000;
        let mut last = 330_000;
        while let Some(next) = l.offer(u64::MAX, at(t + 500)) {
            assert!(next <= 8_000_000 && next > last && next <= last + last / 10 + 1);
            last = next;
            t += 500;
        }
        assert_eq!(l.current(), 8_000_000);
        assert!(BitrateLimiter::new(1, 0, 10).is_none());
        assert!(BitrateLimiter::new(1, 11, 10).is_none());
        assert_eq!(BitrateLimiter::new(1, 5, 10).unwrap().current(), 5);
    }

    #[test]
    fn the_limiter_changes_at_most_once_per_half_second() {
        let mut l = BitrateLimiter::new(2_000_000, 300_000, 8_000_000).unwrap();
        assert_eq!(l.offer(1_000_000, at(1000)), Some(1_000_000));
        assert_eq!(l.offer(500_000, at(1499)), None, "too soon, even downward");
        assert_eq!(l.offer(500_000, at(1500)), Some(500_000));
        assert_eq!(l.offer(500_000, at(9000)), None, "no change, no step");
    }

    #[test]
    fn rate_controls_keep_their_mode() {
        assert_eq!(
            with_bitrate(RateControl::Cbr { bitrate_bps: 1 }, 7),
            Some(RateControl::Cbr { bitrate_bps: 7 })
        );
        assert_eq!(
            with_bitrate(RateControl::CbrPadded { bitrate_bps: 1 }, 7),
            Some(RateControl::CbrPadded { bitrate_bps: 7 })
        );
        assert_eq!(
            with_bitrate(
                RateControl::Vbr {
                    target_bps: 2_000,
                    max_bps: 3_000
                },
                1_000
            ),
            Some(RateControl::Vbr {
                target_bps: 1_000,
                max_bps: 1_500
            })
        );
        assert_eq!(
            with_bitrate(RateControl::ConstantQuality { level: 20 }, 7),
            None
        );
    }

    #[test]
    fn the_target_keeps_the_latest_value_until_taken() {
        let t = BitrateTarget::default();
        assert_eq!(t.take(), None);
        t.request(5);
        t.request(6);
        assert_eq!(t.pending(), Some(6));
        assert_eq!(t.take(), Some(6));
        assert_eq!(t.take(), None);
        t.request(0);
        assert_eq!(
            t.take(),
            Some(1),
            "0 means none, so a request is at least 1"
        );
    }

    #[test]
    fn keyframe_requests_are_forwarded_at_most_once_per_second() {
        let handle = Arc::new(KeyframeAt::default());
        let mut f = EncoderFeedback::new().with_keyframes(Arc::clone(&handle));
        assert!(f.keyframe_requested(at(5000)));
        assert_eq!(handle.pending(), Some(at(5000)));
        assert!(!f.keyframe_requested(at(5999)));
        assert!(f.keyframe_requested(at(6000)));
        assert!(
            !EncoderFeedback::new().keyframe_requested(at(0)),
            "no handle: nothing"
        );
    }

    #[test]
    fn bitrate_hints_reach_the_target_through_the_limiter() {
        let target = Arc::new(BitrateTarget::default());
        let limiter = BitrateLimiter::new(2_500_000, 300_000, 8_000_000).unwrap();
        let mut f = EncoderFeedback::new().with_bitrate(Arc::clone(&target), limiter);
        assert_eq!(f.bitrate_hint(1_000_000, at(0)), Some(1_000_000));
        assert_eq!(target.pending(), Some(1_000_000));
        assert_eq!(f.bitrate_hint(900_000, at(100)), None);
        assert_eq!(f.bitrate(), Some(1_000_000));
        assert_eq!(EncoderFeedback::new().bitrate_hint(1, at(0)), None);
    }
}
