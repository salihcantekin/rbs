//! Runs the capture port conformance suite against the PipeWire adapter.
//!
//! The test publishes its own `Video/Source` node (a moving test pattern in BGRx or
//! RGBA, in shared memory) and runs the suite against it: enumeration, sessions, timestamps,
//! latest wins, allocations, resize (the source renegotiates its size), target loss
//! (the source goes away) and resource release. It needs a running PipeWire daemon
//! with a session manager (WirePlumber) for the stream to be linked; without a daemon
//! socket it only checks probing and reports the rest skipped, unless
//! `RBS_REQUIRE_PIPEWIRE` is set (CI starts both and sets it).
//!
//! The source runs in a child process (this binary, `test_source_process`), so the
//! adapter's threads and fds are counted alone. The file holds a single test that runs
//! by default, so the process thread count check is exact.

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[cfg(target_os = "linux")]
#[path = "common/source.rs"]
mod source;

/// The test source process started by `source::TestSource`; does nothing on its own.
#[test]
#[ignore = "started by passes_conformance as the test source process"]
fn test_source_process() {
    #[cfg(target_os = "linux")]
    if let Some(size) = std::env::var_os(source::SERVE) {
        source::serve(&size.to_string_lossy());
    }
}

#[test]
fn passes_conformance() {
    use rbs_capture::conformance::{self, Options, RealTimePacer};
    use rbs_capture_pipewire::PipeWireCaptureFactory;

    let factory = PipeWireCaptureFactory::new();

    #[cfg(target_os = "linux")]
    let _source;
    #[cfg(target_os = "linux")]
    let hooks;
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut options = Options {
        measure_allocations: Some(measure),
        ..Options::default()
    };
    #[cfg(target_os = "linux")]
    {
        if rbs_capture_pipewire::daemon_present() {
            let source = std::sync::Arc::new(source::TestSource::start(320, 240));
            hooks = Hooks(std::sync::Arc::clone(&source));
            _source = source;
            options.hooks = Some(&hooks);
            options.process_threads = Some(thread_count);
        } else if std::env::var_os("RBS_REQUIRE_PIPEWIRE").is_some_and(|v| !v.is_empty()) {
            panic!("RBS_REQUIRE_PIPEWIRE is set but no PipeWire daemon socket was found");
        } else {
            eprintln!("no PipeWire daemon: only probing is checked");
        }
    }
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            eprintln!("{report:?}");
            if options.hooks.is_some() {
                for check in [
                    "enumerate",
                    "formats",
                    "standby",
                    "timestamps",
                    "latest wins",
                    "no allocation",
                    "stop",
                    "target lost",
                ] {
                    assert!(
                        report.passed.iter().any(|c| c == check),
                        "{check} did not run: {report:?}"
                    );
                }
            }
        }
        Err(failure) => panic!("conformance failed: {failure}"),
    }
    // The suite does not resize cameras, but PipeWire sources may renegotiate their
    // size at any time; check that the adapter follows.
    #[cfg(target_os = "linux")]
    if options.hooks.is_some() {
        follows_renegotiated_size(&factory);
    }
}

/// A source that changes its size mid-stream delivers frames, and a format, of the
/// new size.
#[cfg(target_os = "linux")]
fn follows_renegotiated_size(factory: &rbs_capture_pipewire::PipeWireCaptureFactory) {
    use std::time::{Duration, Instant};

    use rbs_capture::{CaptureFactory, CaptureRequest, TargetId};
    use rbs_core::media::{MemoryKind, PixelFormat};
    use rbs_core::time::MonotonicClock;

    let source = source::TestSource::start(320, 240);
    let target = TargetId::new(format!("node:{}", source::NODE_NAME));
    let request = CaptureRequest::new(target, std::sync::Arc::new(MonotonicClock::new()))
        .with_pixel_formats(&[PixelFormat::Bgra8])
        .with_memory(&[MemoryKind::System]);
    let mut session = match factory.open(&request) {
        Ok(s) => s,
        Err(e) => panic!("open: {e}"),
    };
    if let Err(e) = session.start() {
        panic!("start: {e}");
    }
    let wait_for = |session: &mut Box<dyn rbs_capture::CaptureSession>, w: u32, h: u32| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match session.latest_frame() {
                Ok(Some(frame)) if frame.format().size.width == w => {
                    assert_eq!(frame.format().size.height, h);
                    return;
                }
                Ok(_) if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Ok(_) => panic!("no {w}x{h} frame within 3 s"),
                Err(e) => panic!("latest_frame: {e}"),
            }
        }
    };
    wait_for(&mut session, 320, 240);
    assert!(source.resize(160, 120), "resize command not sent");
    wait_for(&mut session, 160, 120);
    let size = session.format().size;
    assert_eq!(
        (size.width, size.height),
        (160, 120),
        "format() did not follow"
    );
    session.stop();
}

/// The process thread count, logging the thread names so a mismatch shows which
/// thread is left.
#[cfg(target_os = "linux")]
fn thread_count() -> Option<u64> {
    let names: Vec<String> = std::fs::read_dir("/proc/self/task")
        .ok()?
        .filter_map(Result::ok)
        .map(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .map_or_else(|_| "?".to_owned(), |name| name.trim().to_owned())
        })
        .collect();
    eprintln!("threads: {names:?}");
    Some(names.len() as u64)
}

#[cfg(target_os = "linux")]
struct Hooks(std::sync::Arc<source::TestSource>);

#[cfg(target_os = "linux")]
impl rbs_capture::conformance::TestHooks for Hooks {
    fn resize(&self, target: &rbs_capture::TargetId, width: u32, height: u32) -> bool {
        is_test_source(target) && self.0.resize(width, height)
    }

    fn remove(&self, target: &rbs_capture::TargetId) -> bool {
        is_test_source(target) && self.0.remove()
    }

    /// Open file descriptors of the process other than shared-memory buffers: daemon
    /// connections, event and timer fds, DMA-BUFs. The test source in this process
    /// reallocates its memfd buffers on every link, so those are not counted. Threads
    /// are counted separately by the suite.
    fn resources(&self) -> Option<u64> {
        let entries = std::fs::read_dir("/proc/self/fd").ok()?;
        let count = entries
            .filter_map(Result::ok)
            .filter(|entry| {
                std::fs::read_link(entry.path())
                    .is_ok_and(|target| !target.to_string_lossy().starts_with("/memfd:"))
            })
            .count();
        Some(count as u64)
    }
}

#[cfg(target_os = "linux")]
fn is_test_source(target: &rbs_capture::TargetId) -> bool {
    target.as_str() == format!("node:{}", source::NODE_NAME)
}
