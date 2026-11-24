//! The studio runs background removal through the filter type's own node (ADR-0179;
//! spec `video-filters.md`, "Background removal"): a scheduled filter opens a session
//! and lends it small pictures; disabling the filter closes the session (P13). Fake
//! backend, no model. Skips without a GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rbs_core::media::{ContainerId, MemoryKind, Size, VideoFrame};
use rbs_filters_segment::{BACKGROUND_REMOVAL, TYPE_ID, install_backends};
use rbs_graph::node::NodeTypeId;
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::model::Settings;
use rbs_segment::{
    BackendInfo, ComputeUnit, CostHint, LevelId, Matte, MattePlane, PersonMaskBackend,
    PersonMaskSession, QualityLevel, SegmentError, SegmentHost, SessionRequest, SubmitOutcome,
};
use rbs_sources::types;
use support::*;

static EXTRA: [&dyn rbs_filters::FilterType; 1] = [&BACKGROUND_REMOVAL];

#[derive(Default)]
struct Counts {
    opened: AtomicU32,
    closed: AtomicU32,
    submits: AtomicU32,
}

struct Session {
    counts: Arc<Counts>,
    pending: Option<rbs_core::time::Timestamp>,
}

impl PersonMaskSession for Session {
    fn warm(&mut self) -> Result<(), SegmentError> {
        Ok(())
    }
    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
        self.counts.submits.fetch_add(1, Ordering::Relaxed);
        self.pending = Some(frame.timestamp());
        SubmitOutcome::Accepted
    }
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        Ok(self.pending.take().map(|time| {
            Matte::new(
                time,
                Size::new(4, 4),
                MattePlane::Cpu(vec![255u8; 16].into()),
            )
        }))
    }
    fn reset(&mut self) {}
    fn close(self: Box<Self>) {
        self.counts.closed.fetch_add(1, Ordering::Relaxed);
    }
}

struct Backend(Arc<Counts>);

impl PersonMaskBackend for Backend {
    fn type_id(&self) -> &'static str {
        "org.rbs.segment.test"
    }
    fn probe(&self, _: &dyn SegmentHost) -> BackendInfo {
        let mut info = BackendInfo::unavailable();
        info.available = true;
        info.input = MemoryKind::System;
        info.levels = vec![QualityLevel::new(
            LevelId::Medium,
            CostHint::new(1.0, ComputeUnit::Cpu, "test"),
        )];
        info
    }
    fn open(&self, _: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
        self.0.opened.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(Session {
            counts: Arc::clone(&self.0),
            pending: None,
        }))
    }
}

#[test]
fn an_enabled_filter_runs_a_session_and_a_disabled_one_holds_none() {
    let Some(gpu) = gpu() else { return };
    let counts = Arc::new(Counts::default());
    assert!(install_backends(vec![Arc::new(Backend(Arc::clone(
        &counts
    )))]));
    assert!(rbs_filters::install_extra(&EXTRA));
    let r = rig();
    let (mut col, canvas) = scene();
    let display = col
        .sources()
        .find(|s| s.type_id.as_str() == types::DISPLAY_SOURCE)
        .map(|s| s.id)
        .unwrap();
    let (filter, _) = col
        .add_filter(display, NodeTypeId::new_static(TYPE_ID), Settings::new())
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://background-removal", ContainerId::Matroska),
        )
        .unwrap();
    assert!(run_until(&mut studio, &clock, |_| {
        counts.submits.load(Ordering::Relaxed) >= 2
    }));
    assert_eq!(counts.opened.load(Ordering::Relaxed), 1);
    assert_eq!(counts.closed.load(Ordering::Relaxed), 0);

    col.set_filter_enabled(display, filter, false).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(run_until(&mut studio, &clock, |_| {
        counts.closed.load(Ordering::Relaxed) == 1
    }));

    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
