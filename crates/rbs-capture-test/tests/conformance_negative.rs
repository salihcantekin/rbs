//! Runs the capture port conformance suite against wrappers of the synthetic adapter
//! that each break one invariant, to show the suite catches it.

use rbs_capture::conformance::{self, Options, TestPacer};
use rbs_capture_test::TestCaptureFactory;

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

/// Wrappers that break one invariant each.
mod broken {
    use rbs_capture::conformance::TestHooks;
    use rbs_capture::{
        CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats, CaptureTarget,
        TargetId, TargetKind,
    };
    use rbs_core::capability::CapabilitySet;
    use rbs_core::media::{VideoFormat, VideoFrame};
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
    use std::collections::VecDeque;

    #[derive(Clone, Copy, Debug)]
    pub enum Fault {
        /// Allocates on every `latest_frame`.
        AllocatesPerFrame,
        /// Delivers frames before `start`.
        FramesBeforeStart,
        /// Keeps delivering after `stop`.
        StopKeepsDelivering,
        /// Reports no drops.
        HidesDrops,
        /// Returns the oldest queued frame instead of the newest.
        Fifo,
        /// Keeps a frame forever after the session is dropped.
        LeaksOnDrop,
        /// Turns `TargetLost` into silence.
        IgnoresTargetLoss,
        /// Keeps reporting the size from `open` after a resize.
        StaleFormat,
    }

    pub struct Factory<'a> {
        pub inner: &'a super::TestCaptureFactory,
        pub fault: Fault,
    }

    impl AdapterFactory for Factory<'static> {
        fn id(&self) -> AdapterId {
            self.inner.id()
        }
        fn display_name(&self) -> &str {
            "broken"
        }
        fn probe(&self) -> Result<CapabilitySet, ProbeError> {
            self.inner.probe()
        }
    }

    impl CaptureFactory for Factory<'static> {
        fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            self.inner.enumerate(kind)
        }
        fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            let mut inner = self.inner.open(request)?;
            if matches!(self.fault, Fault::FramesBeforeStart) {
                inner.start()?;
            }
            let format = inner.format();
            Ok(Box::new(Session {
                inner,
                fault: self.fault,
                format,
                queue: VecDeque::with_capacity(64),
                stopped_once: false,
            }))
        }
    }

    impl TestHooks for Factory<'static> {
        fn resize(&self, target: &TargetId, width: u32, height: u32) -> bool {
            self.inner.resize(target, width, height)
        }
        fn remove(&self, target: &TargetId) -> bool {
            self.inner.remove(target)
        }
        fn resources(&self) -> Option<u64> {
            self.inner.resources()
        }
    }

    struct Session {
        inner: Box<dyn CaptureSession>,
        fault: Fault,
        format: VideoFormat,
        queue: VecDeque<VideoFrame>,
        stopped_once: bool,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            if matches!(self.fault, Fault::LeaksOnDrop) {
                if let Some(frame) = self.queue.pop_front() {
                    std::mem::forget(frame);
                }
            }
        }
    }

    impl CaptureSession for Session {
        fn format(&self) -> VideoFormat {
            match self.fault {
                Fault::StaleFormat => self.format,
                _ => self.inner.format(),
            }
        }
        fn start(&mut self) -> Result<(), CaptureError> {
            self.inner.start()
        }
        fn stop(&mut self) {
            self.stopped_once = true;
            match self.fault {
                Fault::StopKeepsDelivering => {}
                Fault::LeaksOnDrop => self.inner.stop(),
                _ => {
                    self.queue.clear();
                    self.inner.stop();
                }
            }
        }
        fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError> {
            let frame = match self.inner.latest_frame() {
                Err(CaptureError::TargetLost) if matches!(self.fault, Fault::IgnoresTargetLoss) => {
                    return Ok(None);
                }
                other => other?,
            };
            match self.fault {
                Fault::AllocatesPerFrame => {
                    std::hint::black_box(Box::new(0u64));
                    Ok(frame)
                }
                Fault::Fifo => {
                    // Hand out frames one behind the newest, as an adapter with a
                    // queue would. Every frame is delivered once and drops are counted
                    // by the inner adapter; only the "newest" rule is broken.
                    if let Some(f) = frame {
                        self.queue.push_back(f);
                    }
                    if self.queue.len() > 1 {
                        Ok(self.queue.pop_front())
                    } else {
                        Ok(None)
                    }
                }
                Fault::LeaksOnDrop => {
                    if let Some(shared) = frame.as_ref().and_then(VideoFrame::share) {
                        if self.stopped_once && self.queue.is_empty() {
                            self.queue.push_back(shared);
                        }
                    }
                    Ok(frame)
                }
                _ => Ok(frame),
            }
        }
        fn stats(&self) -> CaptureStats {
            let mut stats = self.inner.stats();
            if matches!(self.fault, Fault::HidesDrops) {
                stats.dropped = 0;
            }
            stats
        }
    }
}

#[test]
fn suite_catches_broken_adapters() {
    use broken::Fault;
    for (fault, check) in [
        (Fault::AllocatesPerFrame, "no allocation"),
        (Fault::FramesBeforeStart, "standby"),
        (Fault::StopKeepsDelivering, "stop"),
        (Fault::HidesDrops, "sequence"),
        (Fault::Fifo, "latest wins"),
        (Fault::LeaksOnDrop, "drop"),
        (Fault::IgnoresTargetLoss, "target lost"),
        (Fault::StaleFormat, "format"),
    ] {
        let inner: &'static TestCaptureFactory = Box::leak(Box::new(TestCaptureFactory::new()));
        let factory = broken::Factory { inner, fault };
        let options = Options {
            measure_allocations: Some(measure),
            hooks: Some(&factory),
            strict: true,
            ..Options::default()
        };
        match conformance::run(&factory, &mut TestPacer::new(), &options) {
            Ok(report) => panic!("{fault:?} passed ({check} expected to fail): {report:?}"),
            Err(failure) => assert_eq!(failure.check, check, "{fault:?}: {failure}"),
        }
    }
}
