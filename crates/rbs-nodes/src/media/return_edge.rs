//! What a media source does when it goes on air again (ADR-0152 decision 6,
//! ADR-0108 decision 10): `resume` where it stopped, `restart` from the range start,
//! or `continue` as if it had kept playing while away.
//!
//! `continue` stores an anchor (master time and position) when a playing film leaves
//! air; nothing decodes while it is away. On air again it seeks to the anchored
//! position plus the time since, clamped to the play range: wrapped when `loop` is
//! true, the end hold past the end when false. A paused or ended film has no anchor
//! and follows the `resume` rows. A source never on air starts at the range start.
//! The anchor lives in memory only: after an app restart `continue` is `resume`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use rbs_core::time::Timestamp;

use super::Shared;
use super::control::MediaControl;
use super::player::Player;

/// The return edge of a media source, written by the host from the source's
/// `on_return` (ADR-0152 decision 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReturnEdge {
    /// Where it stopped (`restart_when_shown` false).
    Resume,
    /// From the start of the play range (`restart_when_shown` true).
    Restart,
    /// Where it would be had it kept playing while away.
    Continue,
}

/// Where a `continue` source picks up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resumed {
    /// Plays from this position.
    At(Duration),
    /// Past the end of the range without `loop`: the end hold at this position.
    Ended(Duration),
}

/// The anchor of a film that left air playing: the instant of its pause record. The
/// position is the cursor, which the worker stops at that instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Anchor {
    pub(crate) at: Timestamp,
}

impl Player {
    /// The return edge now.
    pub(crate) fn return_edge(&self) -> ReturnEdge {
        if self.restart_when_shown {
            ReturnEdge::Restart
        } else if self.continues {
            ReturnEdge::Continue
        } else {
            ReturnEdge::Resume
        }
    }

    pub(crate) fn set_return_edge(&mut self, edge: ReturnEdge) {
        self.restart_when_shown = edge == ReturnEdge::Restart;
        self.continues = edge == ReturnEdge::Continue;
        if edge != ReturnEdge::Continue {
            self.anchor = None;
        }
    }

    /// Stores the anchor of a film that leaves air playing, with `continue`: after
    /// the pause record the take wrote.
    pub(crate) fn anchor_leaving(&mut self, shared: &Shared) {
        if self.continues {
            self.anchor = Some(Anchor {
                at: shared.controls.newest().instant,
            });
        }
    }

    /// Where a `continue` source picks up when it plays again from `instant`; takes
    /// the anchor and marks the source as having been on air. `None` without
    /// `continue`, or without an anchor once it has been on air: the `resume` rows
    /// apply.
    pub(crate) fn continue_at(&mut self, shared: &Shared, instant: Timestamp) -> Option<Resumed> {
        let aired = std::mem::replace(&mut self.aired, true);
        let anchor = self.anchor.take();
        if !self.continues {
            return None;
        }
        let start = shared.range_start();
        let Some(anchor) = anchor else {
            return (!aired).then_some(Resumed::At(start));
        };
        let position = shared.cursor() + instant.saturating_since(anchor.at);
        let duration = match shared.duration.load(Ordering::Acquire) {
            0 => None,
            n => Some(Duration::from_nanos(n - 1)),
        };
        let end = match (shared.range_end(), duration) {
            (Some(end), Some(duration)) => Some(end.min(duration)),
            (end, duration) => end.or(duration),
        };
        Some(continued(
            position,
            start,
            end,
            self.loop_flag.load(Ordering::Acquire),
        ))
    }
}

/// `position` in the range from `start` to `end` (`None`: not known, never clamped):
/// wrapped with `looped`, the end hold past the end without it.
pub(crate) fn continued(
    position: Duration,
    start: Duration,
    end: Option<Duration>,
    looped: bool,
) -> Resumed {
    let position = position.max(start);
    let Some(end) = end.filter(|end| *end > start) else {
        return Resumed::At(position);
    };
    if position < end {
        return Resumed::At(position);
    }
    if looped {
        let length = (end - start).as_nanos();
        let into = (position - start).as_nanos() % length;
        // `into` is below `length`, itself a `Duration`'s nanoseconds.
        return Resumed::At(start + Duration::from_nanos(u64::try_from(into).unwrap_or(0)));
    }
    Resumed::Ended(end.saturating_sub(Duration::from_nanos(1)))
}

impl MediaControl {
    /// The live return edge (ADR-0152 decisions 6 and 7): read by the next on-air
    /// transition and the next wake. Armed as the stinger, it is stored and comes
    /// back when the transition ends.
    pub fn set_on_return(&self, edge: ReturnEdge) {
        let mut player = self.shared.player();
        match player.armed.as_mut() {
            Some(armed) => armed.1 = edge,
            None => player.set_return_edge(edge),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_continued_position_is_clamped_wrapped_or_ended() {
        assert_eq!(continued(s(5), s(2), Some(s(10)), false), Resumed::At(s(5)));
        assert_eq!(continued(s(1), s(2), Some(s(10)), false), Resumed::At(s(2)));
        assert_eq!(continued(s(13), s(2), Some(s(10)), true), Resumed::At(s(5)));
        assert_eq!(continued(s(10), s(2), Some(s(10)), true), Resumed::At(s(2)));
        assert_eq!(
            continued(s(13), s(2), Some(s(10)), false),
            Resumed::Ended(s(10) - Duration::from_nanos(1))
        );
        assert_eq!(continued(s(99), s(0), None, false), Resumed::At(s(99)));
    }
}
