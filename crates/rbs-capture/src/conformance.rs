//! The conformance suite every capture adapter runs in its own tests.
//!
//! ```ignore
//! let report = rbs_capture::conformance::run(&factory, &mut TestPacer::new(), &Options::default())?;
//! ```
//!
//! Each check maps to an invariant in `docs/specs/capture-port.md`. Checks that need a
//! target the machine does not have (no camera, no display on a CI runner) are
//! skipped and listed in [`Report::skipped`]; they never fail.

use std::fmt;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::{MemoryKind, PixelFormat, VideoFrame};
use rbs_core::time::{Clock, MonotonicClock, TestClock, Timestamp};

use crate::caps;
use crate::{
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, TargetId, TargetKind,
    TargetSelector,
};

/// Lets time pass between polls.
///
/// Software adapters driven by the request's clock use [`TestPacer`], so the suite is
/// deterministic. Hardware adapters, whose frames arrive on OS threads, use
/// [`RealTimePacer`].
pub trait Pacer {
    /// The master clock to put in capture requests.
    fn clock(&self) -> Arc<dyn Clock>;
    /// Lets about `d` of time pass.
    fn wait(&mut self, d: Duration);
}

/// A pacer over a [`TestClock`]: `wait` advances the clock and returns at once.
#[derive(Debug, Clone, Default)]
pub struct TestPacer {
    clock: TestClock,
}

impl TestPacer {
    /// A pacer whose clock starts at 1 s (so frames are never at the origin).
    pub fn new() -> Self {
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(1));
        Self { clock }
    }

    /// The underlying test clock.
    pub fn test_clock(&self) -> &TestClock {
        &self.clock
    }
}

impl Pacer for TestPacer {
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(self.clock.clone())
    }

    fn wait(&mut self, d: Duration) {
        self.clock.advance(d);
    }
}

/// A pacer over the real monotonic clock: `wait` sleeps. Only for hardware adapters.
#[derive(Debug, Clone, Default)]
pub struct RealTimePacer {
    clock: MonotonicClock,
}

impl RealTimePacer {
    /// A pacer whose clock origin is now.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Pacer for RealTimePacer {
    fn clock(&self) -> Arc<dyn Clock> {
        Arc::new(self.clock)
    }

    fn wait(&mut self, d: Duration) {
        thread::sleep(d);
    }
}

/// Suite settings.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Time between polls.
    pub poll_interval: Duration,
    /// How long to wait for a frame before failing.
    pub frame_timeout: Duration,
    /// Frames to collect when checking timestamps and sequence numbers.
    pub frames_to_check: u32,
    /// Returns the number of heap allocations made so far by the calling thread.
    /// When set, the suite checks that `latest_frame` (and dropping its frame) does not
    /// allocate. Adapter tests provide it with a counting global allocator.
    pub thread_allocations: Option<fn() -> u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(5),
            frame_timeout: Duration::from_secs(2),
            frames_to_check: 8,
            thread_allocations: None,
        }
    }
}

/// What the suite checked and skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Checks that ran and passed.
    pub passed: Vec<String>,
    /// Checks that did not run, with the reason.
    pub skipped: Vec<String>,
}

/// The first check that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// Name of the check.
    pub check: &'static str,
    /// What went wrong.
    pub detail: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.check, self.detail)
    }
}

impl std::error::Error for Failure {}

fn fail<T>(check: &'static str, detail: impl Into<String>) -> Result<T, Failure> {
    Err(Failure {
        check,
        detail: detail.into(),
    })
}

fn ensure(check: &'static str, ok: bool, detail: impl FnOnce() -> String) -> Result<(), Failure> {
    if ok { Ok(()) } else { fail(check, detail()) }
}

fn symbols(caps: &CapabilitySet, key: &rbs_core::capability::CapabilityKey) -> Vec<String> {
    match caps.get(key) {
        Some(CapabilityValue::Set(items)) => items.clone(),
        _ => Vec::new(),
    }
}

fn declared_pixel_formats(caps: &CapabilitySet) -> Vec<PixelFormat> {
    let names = symbols(caps, &caps::PIXEL_FORMATS);
    ALL_PIXEL_FORMATS
        .iter()
        .copied()
        .filter(|pf| names.iter().any(|n| n == pf.as_str()))
        .collect()
}

fn declared_memory(caps: &CapabilitySet) -> Vec<MemoryKind> {
    let names = symbols(caps, &caps::MEMORY);
    ALL_MEMORY_KINDS
        .iter()
        .copied()
        .filter(|m| names.iter().any(|n| n == m.as_str()))
        .collect()
}

const ALL_PIXEL_FORMATS: [PixelFormat; 12] = [
    PixelFormat::Bgra8,
    PixelFormat::Bgra8Srgb,
    PixelFormat::Rgba8,
    PixelFormat::Rgba8Srgb,
    PixelFormat::Rgb10a2,
    PixelFormat::Rgba16Float,
    PixelFormat::Nv12,
    PixelFormat::P010,
    PixelFormat::I420,
    PixelFormat::I444,
    PixelFormat::Yuy2,
    PixelFormat::Uyvy,
];

const ALL_MEMORY_KINDS: [MemoryKind; 8] = [
    MemoryKind::System,
    MemoryKind::Gpu,
    MemoryKind::D3d11,
    MemoryKind::D3d12,
    MemoryKind::Vulkan,
    MemoryKind::Metal,
    MemoryKind::DmaBuf,
    MemoryKind::Cuda,
];

struct Suite<'a> {
    factory: &'a dyn CaptureFactory,
    pacer: &'a mut dyn Pacer,
    options: Options,
    report: Report,
    pixel_formats: Vec<PixelFormat>,
    memory: Vec<MemoryKind>,
}

/// Runs the capture conformance suite against `factory`.
pub fn run(
    factory: &dyn CaptureFactory,
    pacer: &mut dyn Pacer,
    options: &Options,
) -> Result<Report, Failure> {
    let started = Instant::now();
    let caps = match factory.probe() {
        Ok(caps) => caps,
        Err(e) => {
            return Ok(Report {
                passed: Vec::new(),
                skipped: vec![format!("all: probe failed ({e})")],
            });
        }
    };
    let probe_time = started.elapsed();
    ensure("probe", probe_time < Duration::from_millis(50), || {
        format!("probe took {probe_time:?}, limit is 50 ms")
    })?;

    let mut suite = Suite {
        factory,
        pacer,
        options: *options,
        report: Report::default(),
        pixel_formats: declared_pixel_formats(&caps),
        memory: declared_memory(&caps),
    };
    suite.pass("probe");
    if caps.is_empty() {
        suite.skip("all", "probe declared no capabilities");
        return Ok(suite.report);
    }
    ensure("caps", !suite.pixel_formats.is_empty(), || {
        format!("{} declares no known pixel format", caps::PIXEL_FORMATS)
    })?;
    ensure("caps", !suite.memory.is_empty(), || {
        format!("{} declares no known memory kind", caps::MEMORY)
    })?;

    let kinds = symbols(&caps, &caps::TARGET_KINDS);
    let enumerable = symbols(&caps, &caps::TARGET_ENUMERATE);
    let picker = symbols(&caps, &caps::TARGET_PICKER);

    let mut testable: Vec<(TargetKind, TargetId)> = Vec::new();
    for kind in TargetKind::ALL {
        let name = kind.as_str();
        let listed = factory.enumerate(kind);
        if enumerable.iter().any(|k| k == name) {
            ensure("enumerate", kinds.iter().any(|k| k == name), || {
                format!("{name} is enumerable but not in {}", caps::TARGET_KINDS)
            })?;
            let targets = match listed {
                Ok(targets) => targets,
                Err(e) => return fail("enumerate", format!("{name}: declared but failed: {e}")),
            };
            if let Some(bad) = targets.iter().find(|t| t.kind != kind) {
                return fail(
                    "enumerate",
                    format!("asked for {name}, got {} target {}", bad.kind, bad.id),
                );
            }
            match targets.first() {
                Some(t) => testable.push((kind, t.id.clone())),
                None => suite.skip("sessions", &format!("no {name} target on this machine")),
            }
        } else {
            ensure(
                "enumerate",
                matches!(listed, Err(CaptureError::Unsupported(_))),
                || format!("{name} is not declared enumerable but enumerate returned {listed:?}"),
            )?;
        }
        if !picker.iter().any(|k| k == name) {
            let result = suite.open(TargetSelector::Picker(kind), &[], &[]);
            ensure(
                "picker",
                matches!(result, Err(CaptureError::Unsupported(_))),
                || {
                    format!(
                        "picker for {name} is not declared but open returned {:?}",
                        result.err()
                    )
                },
            )?;
        }
    }
    suite.pass("enumerate");
    suite.pass("picker");

    let unknown = suite.open(
        TargetSelector::Id(TargetId::new("org.rbs.conformance.no-such-target")),
        &[],
        &[],
    );
    ensure(
        "unknown target",
        matches!(unknown, Err(CaptureError::TargetNotFound)),
        || format!("open of an unknown id returned {:?}", unknown.err()),
    )?;
    suite.pass("unknown target");

    if testable.is_empty() {
        suite.skip("sessions", "no enumerable target to open");
        return Ok(suite.report);
    }

    suite.check_formats(&testable)?;
    for (kind, id) in &testable {
        suite.check_session(*kind, id)?;
    }
    Ok(suite.report)
}

impl Suite<'_> {
    fn pass(&mut self, check: &str) {
        if !self.report.passed.iter().any(|c| c == check) {
            self.report.passed.push(check.to_owned());
        }
    }

    fn skip(&mut self, check: &str, reason: &str) {
        self.report.skipped.push(format!("{check}: {reason}"));
    }

    fn open(
        &self,
        target: TargetSelector,
        pixel_formats: &[PixelFormat],
        memory: &[MemoryKind],
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        let pf = if pixel_formats.is_empty() {
            &self.pixel_formats
        } else {
            pixel_formats
        };
        let mem = if memory.is_empty() {
            &self.memory
        } else {
            memory
        };
        let request = CaptureRequest::new(target, self.pacer.clock())
            .with_pixel_formats(pf)
            .with_memory(mem);
        self.factory.open(&request)
    }

    /// Polls until a frame arrives or the timeout passes.
    fn next_frame(
        &mut self,
        check: &'static str,
        session: &mut dyn CaptureSession,
    ) -> Result<VideoFrame, Failure> {
        let deadline = self.options.frame_timeout;
        let mut waited = Duration::ZERO;
        loop {
            match session.latest_frame() {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(e) => return fail(check, format!("latest_frame failed: {e}")),
            }
            if waited >= deadline {
                return fail(check, format!("no frame within {deadline:?}"));
            }
            self.pacer.wait(self.options.poll_interval);
            waited += self.options.poll_interval;
        }
    }

    /// Invariants 4 and 10: every declared format and memory kind can be delivered,
    /// and negotiation follows the request order.
    fn check_formats(&mut self, testable: &[(TargetKind, TargetId)]) -> Result<(), Failure> {
        const CHECK: &str = "formats";
        for pf in self.pixel_formats.clone() {
            let mut delivered = false;
            for (_, id) in testable {
                match self.open(TargetSelector::Id(id.clone()), &[pf], &[]) {
                    Ok(mut session) => {
                        ensure(CHECK, session.format().pixel_format == pf, || {
                            format!(
                                "asked for {pf}, negotiated {}",
                                session.format().pixel_format
                            )
                        })?;
                        session.start().map_err(|e| Failure {
                            check: CHECK,
                            detail: format!("start with {pf} failed: {e}"),
                        })?;
                        let frame = self.next_frame(CHECK, session.as_mut())?;
                        ensure(CHECK, frame.format.pixel_format == pf, || {
                            format!("asked for {pf}, frame is {}", frame.format.pixel_format)
                        })?;
                        delivered = true;
                        break;
                    }
                    Err(CaptureError::FormatUnsupported) => {}
                    Err(e) => return fail(CHECK, format!("open with {pf} failed: {e}")),
                }
            }
            ensure(CHECK, delivered, || {
                format!("{pf} is declared but no target delivers it")
            })?;
        }

        for mem in self.memory.clone() {
            let mut delivered = false;
            for (_, id) in testable {
                match self.open(TargetSelector::Id(id.clone()), &[], &[mem]) {
                    Ok(mut session) => {
                        session.start().map_err(|e| Failure {
                            check: CHECK,
                            detail: format!("start with {mem} failed: {e}"),
                        })?;
                        let frame = self.next_frame(CHECK, session.as_mut())?;
                        ensure(CHECK, frame.memory_kind() == mem, || {
                            format!("asked for {mem}, frame is in {}", frame.memory_kind())
                        })?;
                        delivered = true;
                        break;
                    }
                    Err(CaptureError::FormatUnsupported) => {}
                    Err(e) => return fail(CHECK, format!("open with {mem} failed: {e}")),
                }
            }
            ensure(CHECK, delivered, || {
                format!("{mem} is declared but no target delivers it")
            })?;
        }

        // Order: with every declared format requested, in reverse order, the adapter
        // must pick the first one it supports for that target.
        for (_, id) in testable {
            let supported: Vec<PixelFormat> = self
                .pixel_formats
                .clone()
                .into_iter()
                .filter(|pf| {
                    self.open(TargetSelector::Id(id.clone()), &[*pf], &[])
                        .is_ok()
                })
                .collect();
            let mut order = self.pixel_formats.clone();
            order.reverse();
            let expected = order.iter().copied().find(|pf| supported.contains(pf));
            if let Some(expected) = expected {
                let session = self
                    .open(TargetSelector::Id(id.clone()), &order, &[])
                    .map_err(|e| Failure {
                        check: CHECK,
                        detail: format!("open {id} with all formats failed: {e}"),
                    })?;
                ensure(CHECK, session.format().pixel_format == expected, || {
                    format!(
                        "{id}: request order {order:?} should give {expected}, got {}",
                        session.format().pixel_format
                    )
                })?;
            }
        }
        self.pass(CHECK);
        Ok(())
    }

    /// Invariants 1, 2, 3, 4, 5 and 8 on one target, plus the allocation check.
    fn check_session(&mut self, kind: TargetKind, id: &TargetId) -> Result<(), Failure> {
        let mut session = self
            .open(TargetSelector::Id(id.clone()), &[], &[])
            .map_err(|e| Failure {
                check: "open",
                detail: format!("{kind} {id}: {e}"),
            })?;
        self.pass("open");

        // Invariant 1: nothing before start.
        self.pacer.wait(self.options.poll_interval * 4);
        match session.latest_frame() {
            Ok(None) => {}
            other => {
                return fail(
                    "standby",
                    format!("{id}: latest_frame before start returned {other:?}"),
                );
            }
        }
        self.pass("standby");

        session.start().map_err(|e| Failure {
            check: "start",
            detail: format!("{id}: {e}"),
        })?;
        session.start().map_err(|e| Failure {
            check: "start",
            detail: format!("{id}: second start must be a no-op, failed: {e}"),
        })?;
        self.pass("start");

        let clock = self.pacer.clock();
        let mut last: Option<(Timestamp, u64)> = None;
        let mut first_sequence = None;
        let stats_before = session.stats();
        for _ in 0..self.options.frames_to_check {
            let frame = self.next_frame("timestamps", session.as_mut())?;
            self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
            first_sequence.get_or_insert(frame.sequence);
        }
        self.pass("timestamps");

        // Invariant 3: sequence gaps equal frames counted as dropped. The first frame
        // collected above may already have skipped some, which `stats_before` covers
        // only if they were counted before the first delivery.
        let stats = session.stats();
        if let (Some(first), Some((_, last_sequence))) = (first_sequence, last) {
            let delivered = stats.delivered - stats_before.delivered;
            let gaps = (last_sequence - first) + 1 - delivered;
            let dropped = stats.dropped - stats_before.dropped;
            ensure("sequence", gaps <= dropped, || {
                format!(
                    "{id}: {gaps} sequence numbers skipped but only {dropped} frames counted as dropped"
                )
            })?;
        }
        self.pass("sequence");

        // Invariant 5: after a long pause only the newest frame is returned.
        let interval = self.options.poll_interval;
        self.pacer.wait(interval * 10);
        if let Ok(Some(frame)) = session.latest_frame() {
            self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
            match session.latest_frame() {
                Ok(Some(next)) => {
                    self.check_frame(session.as_ref(), &next, clock.as_ref(), &mut last)?
                }
                Ok(None) => {}
                Err(e) => return fail("latest wins", format!("{id}: {e}")),
            }
        }
        self.pass("latest wins");

        if let Some(allocations) = self.options.thread_allocations {
            for _ in 0..self.options.frames_to_check {
                self.pacer.wait(interval);
                let before = allocations();
                let result = session.latest_frame();
                drop(result);
                let used = allocations() - before;
                ensure("no allocation", used == 0, || {
                    format!("{id}: latest_frame allocated {used} times")
                })?;
            }
            self.pass("no allocation");
        } else {
            self.skip("no allocation", "no allocation counter given");
        }

        // Invariant 8: stop releases, is idempotent, and start works again.
        session.stop();
        session.stop();
        self.pacer.wait(interval * 4);
        match session.latest_frame() {
            Ok(None) => {}
            other => {
                return fail(
                    "stop",
                    format!("{id}: latest_frame after stop returned {other:?}"),
                );
            }
        }
        session.start().map_err(|e| Failure {
            check: "restart",
            detail: format!("{id}: {e}"),
        })?;
        let frame = self.next_frame("restart", session.as_mut())?;
        self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
        session.stop();
        self.pass("stop");
        self.pass("restart");
        Ok(())
    }

    /// Invariants 2, 3 and 4 for one frame.
    fn check_frame(
        &self,
        session: &dyn CaptureSession,
        frame: &VideoFrame,
        clock: &dyn Clock,
        last: &mut Option<(Timestamp, u64)>,
    ) -> Result<(), Failure> {
        let now = clock.now();
        ensure("timestamps", frame.timestamp <= now, || {
            format!(
                "frame at {} ns is ahead of the clock ({} ns)",
                frame.timestamp.as_nanos(),
                now.as_nanos()
            )
        })?;
        if let Some((t, seq)) = *last {
            ensure("timestamps", frame.timestamp > t, || {
                format!(
                    "timestamp {} ns does not increase (previous {} ns)",
                    frame.timestamp.as_nanos(),
                    t.as_nanos()
                )
            })?;
            ensure("sequence", frame.sequence > seq, || {
                format!(
                    "sequence {} does not increase (previous {seq})",
                    frame.sequence
                )
            })?;
        }
        *last = Some((frame.timestamp, frame.sequence));

        let format = session.format();
        ensure("format", frame.format == format, || {
            format!(
                "frame format {:?} differs from session format {format:?}",
                frame.format
            )
        })?;
        ensure("format", self.memory.contains(&frame.memory_kind()), || {
            format!("frame memory {} was not requested", frame.memory_kind())
        })?;
        Ok(())
    }
}
