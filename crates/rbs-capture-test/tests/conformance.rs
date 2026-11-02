//! Runs the capture port conformance suite against the synthetic adapter.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rbs_capture::conformance::{self, Options, TestPacer};
use rbs_capture_test::TestCaptureFactory;

/// Counts allocations per thread so the suite can check that `latest_frame` does not
/// allocate, unaffected by other test threads.
struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every method forwards to `System` with the same arguments, so the
// `GlobalAlloc` contract holds exactly as for `System`. The thread-local counter is a
// `const`-initialised `Cell`, which never allocates, so counting cannot recurse.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: the caller upholds `alloc`'s contract; forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was returned by `System.alloc` above with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
        // SAFETY: `ptr` and `layout` come from this allocator, which is `System`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

fn thread_allocations() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

#[test]
fn synthetic_adapter_passes_conformance() {
    let factory = TestCaptureFactory::new();
    let options = Options {
        thread_allocations: Some(thread_allocations),
        ..Options::default()
    };
    let report = conformance::run(&factory, &mut TestPacer::new(), &options);
    let report = match report {
        Ok(report) => report,
        Err(failure) => panic!("conformance failed: {failure}"),
    };
    assert!(report.skipped.is_empty(), "skipped: {:?}", report.skipped);
    for check in [
        "probe",
        "enumerate",
        "formats",
        "standby",
        "timestamps",
        "sequence",
        "latest wins",
        "no allocation",
        "stop",
        "restart",
    ] {
        assert!(
            report.passed.iter().any(|c| c == check),
            "{check} did not run: {report:?}"
        );
    }
}

/// Wrappers that break one invariant each, to show the suite catches it.
mod broken {
    use rbs_capture::{
        CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats, CaptureTarget,
        TargetKind,
    };
    use rbs_core::capability::CapabilitySet;
    use rbs_core::media::{VideoFormat, VideoFrame};
    use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
    use rbs_core::time::Timestamp;
    use std::time::Duration;

    #[derive(Clone, Copy)]
    pub enum Fault {
        FutureTimestamps,
        AllocatesPerFrame,
        FramesBeforeStart,
        StopKeepsDelivering,
    }

    pub struct Factory {
        pub inner: super::TestCaptureFactory,
        pub fault: Fault,
    }

    impl AdapterFactory for Factory {
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

    impl CaptureFactory for Factory {
        fn enumerate(&self, kind: TargetKind) -> Result<Vec<CaptureTarget>, CaptureError> {
            self.inner.enumerate(kind)
        }
        fn open(&self, request: &CaptureRequest) -> Result<Box<dyn CaptureSession>, CaptureError> {
            let mut inner = self.inner.open(request)?;
            if matches!(self.fault, Fault::FramesBeforeStart) {
                inner.start()?;
            }
            Ok(Box::new(Session {
                inner,
                fault: self.fault,
                stopped: false,
            }))
        }
    }

    struct Session {
        inner: Box<dyn CaptureSession>,
        fault: Fault,
        stopped: bool,
    }

    impl CaptureSession for Session {
        fn format(&self) -> VideoFormat {
            self.inner.format()
        }
        fn start(&mut self) -> Result<(), CaptureError> {
            self.stopped = false;
            self.inner.start()
        }
        fn stop(&mut self) {
            self.stopped = true;
            if !matches!(self.fault, Fault::StopKeepsDelivering) {
                self.inner.stop();
            }
        }
        fn latest_frame(&mut self) -> Result<Option<VideoFrame>, CaptureError> {
            let frame = self.inner.latest_frame()?;
            Ok(frame.map(|mut f| {
                match self.fault {
                    Fault::FutureTimestamps => {
                        f.timestamp = f.timestamp + Duration::from_secs(1);
                    }
                    Fault::AllocatesPerFrame => {
                        std::hint::black_box(Box::new(0u64));
                    }
                    Fault::FramesBeforeStart | Fault::StopKeepsDelivering => {}
                }
                let _ = Timestamp::ZERO;
                f
            }))
        }
        fn stats(&self) -> CaptureStats {
            self.inner.stats()
        }
    }
}

#[test]
fn suite_catches_broken_adapters() {
    use broken::Fault;
    for (fault, check) in [
        (Fault::FutureTimestamps, "timestamps"),
        (Fault::AllocatesPerFrame, "no allocation"),
        (Fault::FramesBeforeStart, "standby"),
        (Fault::StopKeepsDelivering, "stop"),
    ] {
        let factory = broken::Factory {
            inner: TestCaptureFactory::new(),
            fault,
        };
        let options = Options {
            thread_allocations: Some(thread_allocations),
            ..Options::default()
        };
        match conformance::run(&factory, &mut TestPacer::new(), &options) {
            Ok(report) => panic!("broken adapter passed ({check} expected to fail): {report:?}"),
            Err(failure) => assert_eq!(failure.check, check, "{failure}"),
        }
    }
}
