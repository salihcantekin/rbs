//! The last-frame rule of a native preview layer (ADR-0161): the newest frame whose
//! present failed is kept and presented again, instead of going back unseen. A
//! still page (an Interact window after the mouse stops, a projector of a paused
//! scene) otherwise keeps showing an older picture until something changes.
//!
//! Pure bookkeeping over any frame type, so the rule is tested on every platform;
//! the Windows layer owns the presents.

// Only the Windows layer presents; other platforms build this for its tests.
#![cfg_attr(not(all(windows, feature = "preview-layer")), allow(dead_code))]

use std::time::Duration;

use rbs_core::time::Timestamp;

/// How long a frame whose present keeps failing is tried again before it goes back.
/// A busy compositor clears within a refresh or two; a present that still fails
/// after this is not going to succeed for this frame.
pub(crate) const RETRY_FOR: Duration = Duration::from_secs(1);

/// What to do with the kept frame now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Retry<F> {
    /// No frame waits.
    Nothing,
    /// Present it again; on failure hand it back with [`PresentRetry::hold`].
    Again(F),
    /// It failed for [`RETRY_FOR`]: give it back unseen (counted missed).
    GiveUp(F),
}

/// At most one frame whose present failed, and counts of what happened.
#[derive(Debug)]
pub(crate) struct PresentRetry<F> {
    frame: Option<F>,
    /// The first failed present of the kept frame.
    since: Option<Timestamp>,
    failures: u64,
    missed: u64,
}

impl<F> PresentRetry<F> {
    pub(crate) const fn new() -> Self {
        Self {
            frame: None,
            since: None,
            failures: 0,
            missed: 0,
        }
    }

    /// The present of `frame` failed at `now`: keep it. A frame tried again keeps
    /// the time of its first failure.
    pub(crate) fn hold(&mut self, frame: F, now: Timestamp) {
        self.failures += 1;
        self.since.get_or_insert(now);
        self.frame = Some(frame);
    }

    /// A present succeeded.
    pub(crate) fn presented(&mut self) {
        self.since = None;
    }

    /// A newer frame arrived: the kept one, if any, is replaced (counted missed;
    /// the caller retires it, its buffer may be the surface's current one).
    pub(crate) fn newer(&mut self) -> Option<F> {
        self.since = None;
        let old = self.frame.take();
        self.missed += u64::from(old.is_some());
        old
    }

    /// The kept frame, to present again or to give up at `now`. Nothing at the
    /// instant of its first failure: the callers retire right after `show`, and the
    /// compositor has not had a chance to clear yet.
    pub(crate) fn take_due(&mut self, now: Timestamp) -> Retry<F> {
        let since = self.since.unwrap_or(now);
        if self.frame.is_none() || (self.since.is_some() && now == since) {
            return Retry::Nothing;
        }
        let Some(frame) = self.frame.take() else {
            return Retry::Nothing;
        };
        if now.saturating_since(since) >= RETRY_FOR {
            self.since = None;
            self.missed += 1;
            return Retry::GiveUp(frame);
        }
        Retry::Again(frame)
    }

    /// The kept frame without a retry (detach, a lost device), counted missed.
    pub(crate) fn clear(&mut self) -> Option<F> {
        self.newer()
    }

    /// True while a frame waits for another present.
    pub(crate) fn waiting(&self) -> bool {
        self.frame.is_some()
    }

    /// Failed presents so far.
    pub(crate) fn failures(&self) -> u64 {
        self.failures
    }

    /// Frames whose present failed and that were never shown: replaced by a newer
    /// frame, given up, or dropped with the layer's window.
    pub(crate) fn missed(&self) -> u64 {
        self.missed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp::ZERO + Duration::from_millis(ms)
    }

    /// The newest frame of a still page survives a failed present and is tried
    /// again until it shows; nothing is counted missed.
    #[test]
    fn a_failed_present_keeps_the_frame_until_a_retry_shows_it() {
        let mut r = PresentRetry::new();
        r.hold(7, at(0));
        assert!(r.waiting());
        for t in [4, 8, 12] {
            match r.take_due(at(t)) {
                Retry::Again(f) => r.hold(f, at(t)),
                other => panic!("{other:?} at {t} ms"),
            }
        }
        assert_eq!(r.take_due(at(16)), Retry::Again(7));
        r.presented();
        assert!(!r.waiting());
        assert_eq!(r.take_due(at(20)), Retry::Nothing);
        assert_eq!((r.failures(), r.missed()), (4, 0));
    }

    /// A newer frame replaces the kept one, which counts as missed; the newer
    /// frame's own failure starts a new retry window.
    #[test]
    fn a_newer_frame_replaces_the_kept_one_and_restarts_the_window() {
        let mut r = PresentRetry::new();
        r.hold(1, at(0));
        assert_eq!(r.newer(), Some(1));
        assert_eq!(r.newer(), None, "nothing kept now");
        r.hold(2, at(900));
        assert_eq!(r.take_due(at(1_100)), Retry::Again(2), "window from 900 ms");
        r.hold(2, at(1_100));
        assert_eq!(r.missed(), 1);
    }

    /// A present that keeps failing gives the frame back after [`RETRY_FOR`] from its
    /// first failure, so the layer stops polling (P13) and the engine gets the
    /// surface back.
    #[test]
    fn a_frame_that_never_presents_goes_back_after_the_retry_window() {
        let mut r = PresentRetry::new();
        r.hold(3, at(10));
        let mut t = 10;
        loop {
            t += 4;
            match r.take_due(at(t)) {
                Retry::Again(f) => r.hold(f, at(t)),
                Retry::GiveUp(f) => {
                    assert_eq!(f, 3);
                    break;
                }
                Retry::Nothing => panic!("lost the frame"),
            }
        }
        assert_eq!(t, 10 + 1_000, "given up at one second, not before");
        assert!(!r.waiting());
        assert_eq!(r.missed(), 1);
    }

    /// A retire at the instant of the failure does not retry; one later does.
    #[test]
    fn no_retry_at_the_instant_of_the_failure() {
        let mut r = PresentRetry::new();
        r.hold(4, at(20));
        assert_eq!(r.take_due(at(20)), Retry::Nothing);
        assert!(r.waiting());
        assert_eq!(r.take_due(at(24)), Retry::Again(4));
        assert_eq!(r.failures(), 1);
    }

    /// A success ends the window: a later failure gets a full second again.
    #[test]
    fn a_success_resets_the_retry_window() {
        let mut r = PresentRetry::new();
        r.hold(1, at(0));
        assert_eq!(r.take_due(at(900)), Retry::Again(1));
        r.presented();
        r.hold(2, at(1_500));
        assert_eq!(r.take_due(at(2_400)), Retry::Again(2), "not given up");
        r.hold(2, at(2_400));
        assert_eq!(r.take_due(at(2_500)), Retry::GiveUp(2));
        assert_eq!(r.missed(), 1);
    }

    #[test]
    fn clearing_counts_a_kept_frame_as_missed() {
        let mut r = PresentRetry::new();
        assert_eq!(r.clear(), None);
        r.hold(5, at(0));
        assert_eq!(r.clear(), Some(5));
        assert_eq!((r.failures(), r.missed()), (1, 1));
    }
}
