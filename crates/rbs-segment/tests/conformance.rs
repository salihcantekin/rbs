//! A test backend run through the conformance suite, and checks that the suite catches
//! a broken backend. No sleeping, no threads: the "worker" finishes inside `settle`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use rbs_core::media::{
    Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::time::Timestamp;
use rbs_segment::conformance::{Harness, run_all};
use rbs_segment::*;

const SIZE: Size = Size::new(4, 4);

#[derive(Default)]
struct Shared {
    pending: Option<Timestamp>,
    done: Option<Timestamp>,
}

struct TestSession {
    shared: Arc<Mutex<Shared>>,
}

impl PersonMaskSession for TestSession {
    fn warm(&mut self) -> Result<(), SegmentError> {
        Ok(())
    }
    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
        self.shared.lock().unwrap().pending = Some(frame.timestamp());
        SubmitOutcome::Accepted
    }
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        let done = self.shared.lock().unwrap().done.take();
        Ok(done.map(|timestamp| {
            Matte::new(
                timestamp,
                Size::new(2, 2),
                MattePlane::Cpu(Arc::from(vec![255u8; 4])),
            )
        }))
    }
    fn reset(&mut self) {}
    fn close(self: Box<Self>) {}
}

struct TestBackend {
    available: bool,
    /// A broken backend that returns every matte twice.
    sticky: bool,
    shared: Arc<Mutex<Shared>>,
}

struct StickySession(TestSession, Option<Matte>);

impl PersonMaskSession for StickySession {
    fn warm(&mut self) -> Result<(), SegmentError> {
        self.0.warm()
    }
    fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
        self.0.submit(frame)
    }
    fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
        if let Some(matte) = self.0.latest()? {
            self.1 = Some(matte);
        }
        Ok(self.1.clone())
    }
    fn reset(&mut self) {}
    fn close(self: Box<Self>) {}
}

impl PersonMaskBackend for TestBackend {
    fn type_id(&self) -> &'static str {
        "org.rbs.segment.test"
    }
    fn probe(&self, _host: &dyn SegmentHost) -> BackendInfo {
        if !self.available {
            return BackendInfo::unavailable();
        }
        let mut info = BackendInfo::unavailable();
        info.available = true;
        info.levels = vec![
            QualityLevel::new(LevelId::Low, CostHint::new(1.0, ComputeUnit::Cpu, "test")),
            QualityLevel::new(LevelId::High, CostHint::new(9.0, ComputeUnit::Cpu, "test")),
        ];
        info
    }
    fn open(&self, request: SessionRequest) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
        if !self.available {
            return Err(SegmentError::Unavailable);
        }
        if request.level == LevelId::Medium {
            return Err(SegmentError::Unsupported {
                level: request.level,
            });
        }
        let session = TestSession {
            shared: Arc::clone(&self.shared),
        };
        if self.sticky {
            Ok(Box::new(StickySession(session, None)))
        } else {
            Ok(Box::new(session))
        }
    }
}

struct NoModels;
impl SegmentHost for NoModels {
    fn model(&self, _name: &str) -> Option<ModelRef> {
        None
    }
}

fn frame(n: u64) -> VideoFrame {
    let format = VideoFormat::new(SIZE, PixelFormat::Rgba8, ColorSpace::SRGB);
    let system = SystemFrame::new(
        format,
        Bytes::from(vec![0u8; 64]),
        &[PlaneLayout {
            offset: 0,
            stride: 16,
        }],
    )
    .unwrap();
    VideoFrame::system(Timestamp::from_nanos(n * 1_000_000), system, None)
}

fn harness_for(backend: &TestBackend, run: impl FnOnce(&Harness<'_>)) {
    let shared = Arc::clone(&backend.shared);
    let settle = move || {
        let mut shared = shared.lock().unwrap();
        shared.done = shared.pending.take();
    };
    run(&Harness {
        backend,
        host: &NoModels,
        make_frame: &frame,
        settle: &settle,
    });
}

#[test]
fn a_correct_backend_passes_the_suite() {
    let backend = TestBackend {
        available: true,
        sticky: false,
        shared: Arc::default(),
    };
    harness_for(&backend, run_all);
}

#[test]
fn an_unavailable_backend_is_skipped_not_failed() {
    let backend = TestBackend {
        available: false,
        sticky: false,
        shared: Arc::default(),
    };
    harness_for(&backend, run_all);
}

#[test]
#[should_panic(expected = "each matte is returned once")]
fn a_backend_that_repeats_a_matte_fails_the_suite() {
    let backend = TestBackend {
        available: true,
        sticky: true,
        shared: Arc::default(),
    };
    harness_for(&backend, run_all);
}

#[test]
fn levels_are_ordered_and_errors_display() {
    assert!(LevelId::Low < LevelId::Medium && LevelId::Medium < LevelId::High);
    let text = SegmentError::Unsupported {
        level: LevelId::High,
    }
    .to_string();
    assert!(text.contains("High"));
}

#[test]
fn a_gpu_matte_release_is_seen_by_the_session() {
    let release = MatteRelease::new();
    let matte = GpuMatte::new(7, rbs_core::media::SurfaceSync::Ready, release.clone());
    assert!(!matte.release.is_released());
    release.release();
    assert!(matte.release.is_released());
}
