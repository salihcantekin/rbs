//! The conformance suite every person mask backend passes (spec "Conformance suite").
//!
//! The suite never sleeps (ADR-0032): the backend's test harness provides `settle`,
//! which returns once the worker has finished every frame submitted so far (a wait on
//! a condition, bounded by a hang guard only). Each check panics with a message that
//! names the broken rule.

#![allow(clippy::expect_used)] // a conformance check panics with a message by design

use rbs_core::media::VideoFrame;

use crate::error::SegmentError;
use crate::info::{BackendInfo, LevelId};
use crate::session::{
    PersonMaskBackend, PersonMaskSession, SegmentHost, SessionRequest, SubmitOutcome,
};

/// What a backend's test supplies to the suite.
pub struct Harness<'a> {
    /// The backend under test.
    pub backend: &'a dyn PersonMaskBackend,
    /// The host that lends models.
    pub host: &'a dyn SegmentHost,
    /// Builds the n-th test frame (any picture of the size the backend was opened with;
    /// frame `n` has timestamp `n` milliseconds).
    pub make_frame: &'a dyn Fn(u64) -> VideoFrame,
    /// Returns once the worker has finished every frame submitted so far.
    pub settle: &'a dyn Fn(),
}

fn level_order_ok(info: &BackendInfo) -> bool {
    (1..=3).contains(&info.levels.len())
        && info.levels.windows(2).all(|pair| pair[0].id < pair[1].id)
}

/// `probe` is cheap and consistent: one to three levels in increasing order, or none
/// when the backend is not available. Returns the info for the other checks.
pub fn check_probe(harness: &Harness<'_>) -> BackendInfo {
    let info = harness.backend.probe(harness.host);
    if info.available {
        assert!(
            level_order_ok(&info),
            "probe: levels must be one to three and ordered low to high"
        );
        for level in &info.levels {
            assert!(
                level.cost.ms_per_frame.is_finite() && level.cost.ms_per_frame >= 0.0,
                "probe: a level's cost must be a measured, finite number"
            );
        }
    } else {
        assert!(
            info.levels.is_empty(),
            "probe: an unavailable backend offers no levels"
        );
    }
    info
}

/// Opens a session at the first frame's size and warms it.
fn open_warm(harness: &Harness<'_>, level: LevelId) -> Box<dyn PersonMaskSession> {
    let first = (harness.make_frame)(0);
    let request = SessionRequest::new(level, first.format().size);
    let mut session = harness
        .backend
        .open(request)
        .expect("open: an available level must open");
    session.warm().expect("warm: must succeed after open");
    session
}

/// A level the backend does not offer is refused with `Unsupported`.
pub fn check_unsupported_level(harness: &Harness<'_>, info: &BackendInfo) {
    let missing = [LevelId::Low, LevelId::Medium, LevelId::High]
        .into_iter()
        .find(|id| info.level(*id).is_none());
    if let Some(level) = missing {
        let first = (harness.make_frame)(0);
        let result = harness
            .backend
            .open(SessionRequest::new(level, first.format().size));
        assert!(
            matches!(result, Err(SegmentError::Unsupported { level: asked }) if asked == level),
            "open: a level the backend does not offer must give Unsupported"
        );
    }
}

/// A submitted frame produces one matte, returned once, with the frame's timestamp and
/// a size that stays the same for the whole session.
pub fn check_matte_flow(harness: &Harness<'_>, level: LevelId) {
    let mut session = open_warm(harness, level);
    assert!(
        matches!(session.latest(), Ok(None)),
        "latest: before any frame there is no matte"
    );
    let frame = (harness.make_frame)(1);
    assert_eq!(
        session.submit(&frame),
        SubmitOutcome::Accepted,
        "submit: an idle session accepts a frame"
    );
    (harness.settle)();
    let matte = session
        .latest()
        .expect("latest: no error expected")
        .expect("latest: the finished matte must be returned");
    assert_eq!(
        matte.timestamp,
        frame.timestamp(),
        "matte: carries the frame's timestamp"
    );
    assert!(!matte.size.is_empty(), "matte: has a size");
    assert!(
        matches!(session.latest(), Ok(None)),
        "latest: each matte is returned once"
    );
    let second = (harness.make_frame)(2);
    session.submit(&second);
    (harness.settle)();
    let next = session.latest().expect("no error").expect("second matte");
    assert_eq!(
        next.size, matte.size,
        "matte: size is fixed for the session"
    );
    session.close();
}

/// `reset` is accepted at any time and the session keeps working after it.
pub fn check_reset(harness: &Harness<'_>, level: LevelId) {
    let mut session = open_warm(harness, level);
    session.reset();
    session.submit(&(harness.make_frame)(1));
    (harness.settle)();
    assert!(
        matches!(session.latest(), Ok(Some(_))),
        "reset: the session produces mattes after a reset"
    );
    session.close();
}

/// Runs every check for every level the backend offers. Backends that are not
/// available here (an OS-only backend on another platform) are skipped, not failed.
pub fn run_all(harness: &Harness<'_>) {
    let info = check_probe(harness);
    if !info.available {
        return;
    }
    check_unsupported_level(harness, &info);
    for level in &info.levels {
        check_matte_flow(harness, level.id);
        check_reset(harness, level.id);
    }
}
