//! The port's conformance suite against the real Apple Vision backend. Runs on macOS
//! only; the one-line test below checks the other platforms.
//!
//! Vision works on its own thread, so the test wraps the backend: a decorated session
//! answers `latest` only once the worker has finished every frame submitted so far,
//! by polling the real `latest` for a condition (no sleeping, ADR-0032) with a hang
//! guard on iterations.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use rbs_segment::{ModelRef, PersonMaskBackend, SegmentHost};
use rbs_segment_vision::{TYPE_ID, VisionBackend};

struct NoModels;
impl SegmentHost for NoModels {
    fn model(&self, _name: &str) -> Option<ModelRef> {
        None
    }
}

#[test]
fn the_type_id_is_stable() {
    assert_eq!(VisionBackend::new().type_id(), TYPE_ID);
    assert_eq!(TYPE_ID, "org.rbs.segment.apple-vision");
}

#[cfg(not(target_os = "macos"))]
#[test]
fn it_is_skipped_off_macos() {
    let backend = VisionBackend::new();
    assert!(!backend.probe(&NoModels).available);
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use rbs_core::media::{
        Bytes, ColorSpace, PixelFormat, PlaneLayout, Size, SystemFrame, VideoFormat, VideoFrame,
    };
    use rbs_core::time::Timestamp;
    use rbs_segment::conformance::{Harness, run_all};
    use rbs_segment::{
        BackendInfo, Matte, PersonMaskSession, SegmentError, SessionRequest, SubmitOutcome,
    };

    /// Polls the real session until the matte for the last submitted frame arrives.
    struct Waiting {
        inner: Box<dyn PersonMaskSession>,
        outstanding: u32,
    }

    impl PersonMaskSession for Waiting {
        fn warm(&mut self) -> Result<(), SegmentError> {
            self.inner.warm()
        }
        fn submit(&mut self, frame: &VideoFrame) -> SubmitOutcome {
            let outcome = self.inner.submit(frame);
            if outcome == SubmitOutcome::Accepted {
                self.outstanding = 1;
            }
            outcome
        }
        fn latest(&mut self) -> Result<Option<Matte>, SegmentError> {
            // Hang guard: about 30 s of spinning at the worst, never a timing assertion.
            for _ in 0..30_000_000u32 {
                match self.inner.latest()? {
                    Some(matte) => {
                        self.outstanding = 0;
                        return Ok(Some(matte));
                    }
                    None if self.outstanding == 0 => return Ok(None),
                    None => std::thread::yield_now(),
                }
            }
            panic!("the Vision worker did not finish a frame: hang guard");
        }
        fn reset(&mut self) {
            self.inner.reset();
        }
        fn close(self: Box<Self>) {
            self.inner.close();
        }
    }

    struct WaitingBackend(VisionBackend);

    impl PersonMaskBackend for WaitingBackend {
        fn type_id(&self) -> &'static str {
            self.0.type_id()
        }
        fn probe(&self, host: &dyn SegmentHost) -> BackendInfo {
            self.0.probe(host)
        }
        fn open(
            &self,
            request: SessionRequest,
        ) -> Result<Box<dyn PersonMaskSession>, SegmentError> {
            let inner = self.0.open(request)?;
            Ok(Box::new(Waiting {
                inner,
                outstanding: 0,
            }))
        }
    }

    fn frame(n: u64) -> VideoFrame {
        let size = Size::new(256, 256);
        let mut data = vec![0u8; 256 * 256 * 4];
        for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
            let x = (index % 256) as u8;
            pixel.copy_from_slice(&[x, 255 - x, 128, 255]);
        }
        let format = VideoFormat::new(size, PixelFormat::Bgra8, ColorSpace::SRGB);
        let system = SystemFrame::new(
            format,
            Bytes::from(data),
            &[PlaneLayout {
                offset: 0,
                stride: 256 * 4,
            }],
        )
        .unwrap();
        VideoFrame::system(Timestamp::from_nanos(n * 1_000_000), system, None)
    }

    #[test]
    fn apple_vision_passes_the_conformance_suite() {
        let backend = WaitingBackend(VisionBackend::new());
        let settle = || {};
        run_all(&Harness {
            backend: &backend,
            host: &NoModels,
            make_frame: &frame,
            settle: &settle,
        });
    }
}
