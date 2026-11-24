//! The filter type: the conformance checks, the backend registry and the node the type
//! builds for the studio. Own test binary: the registry is process-wide.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use rbs_core::media::{MemoryKind, VideoFrame};
use rbs_filters::conformance::{check_type, compile};
use rbs_filters::{FilterType, Needs, Values};
use rbs_filters_segment::{BACKGROUND_REMOVAL, TYPE_ID, available, install_backends};
use rbs_segment::{
    BackendInfo, ComputeUnit, CostHint, LevelId, Matte, PersonMaskBackend, PersonMaskSession,
    QualityLevel, SegmentError, SegmentHost, SessionRequest, SubmitOutcome,
};

#[test]
fn the_type_passes_the_conformance_checks() {
    assert_eq!(check_type(&BACKGROUND_REMOVAL), Vec::<String>::new());
    compile(BACKGROUND_REMOVAL.passes()[0].shader).unwrap();
    assert_eq!(BACKGROUND_REMOVAL.type_id().as_str(), TYPE_ID);
    assert!(BACKGROUND_REMOVAL.needs().contains(Needs::PERSON_MASK));
}

struct Idle;
impl PersonMaskSession for Idle {
    fn warm(&mut self) -> Result<(), SegmentError> {
        Ok(())
    }
    fn submit(&mut self, _: &VideoFrame) -> SubmitOutcome {
        SubmitOutcome::DroppedBusy
    }
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        Ok(None)
    }
    fn reset(&mut self) {}
    fn close(self: Box<Self>) {}
}

struct Backend;
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
            CostHint::new(9.0, ComputeUnit::Npu, "test"),
        )];
        info
    }
    fn open(&self, _: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
        Ok(Box::new(Idle))
    }
}

#[test]
fn the_type_is_offered_only_with_a_backend_and_builds_its_own_node() {
    let ty: &dyn FilterType = &BACKGROUND_REMOVAL;
    assert!(!available(), "no backend installed yet");
    assert!(ty.custom_node(Values::defaults(ty), None).is_some());
    assert!(install_backends(vec![Arc::new(Backend)]));
    assert!(!install_backends(vec![]), "installed once");
    assert!(available());
    // A level the backend lacks maps to the nearest one it has: High asks for Medium.
    let values = Values::defaults(ty)
        .with(ty, [("quality", rbs_filters::Value::Choice(2))])
        .unwrap();
    assert!(ty.custom_node(values, Some([1920, 1080])).is_some());
}
