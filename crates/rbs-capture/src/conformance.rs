//! The conformance suite every capture adapter runs in its own tests.
//!
//! ```ignore
//! let options = Options::default();
//! let report = rbs_capture::conformance::run(&factory, &mut TestPacer::new(), &options)?;
//! ```
//!
//! Each check maps to an invariant in `docs/specs/capture-port.md`. Checks that need a
//! target the machine does not have (no camera, no display on a CI runner) or a hook
//! the adapter does not provide are skipped and listed in [`Report::skipped`]. With
//! [`Options::strict`] (hardware runners), any skip is a failure.

use std::fmt;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rbs_core::capability::{CapabilityKey, CapabilitySet, CapabilityValue};
use rbs_core::media::{MemoryKind, PixelFormat, VideoFrame};
use rbs_core::time::{Clock, MonotonicClock, TestClock, Timestamp};

use crate::caps;
use crate::{
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, CaptureStats, Permission,
    TargetId, TargetKind,
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
#[derive(Debug, Clone)]
pub struct TestPacer {
    clock: TestClock,
}

impl Default for TestPacer {
    fn default() -> Self {
        Self::new()
    }
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

/// Adapter-provided hooks that let the suite drive targets and observe resources.
///
/// Software adapters implement them directly. Hardware test setups implement them with
/// a helper window the test owns (animate, resize and close it).
pub trait TestHooks {
    /// Changes the content of `target`, for APIs that only deliver frames when content
    /// changes (Windows Graphics Capture, ScreenCaptureKit). Called before every poll.
    fn stimulate(&self, target: &TargetId) {
        let _ = target;
    }

    /// Resizes `target` to `width` x `height`. Returns `false` if not possible.
    fn resize(&self, target: &TargetId, width: u32, height: u32) -> bool {
        let _ = (target, width, height);
        false
    }

    /// Makes `target` disappear (close the window, unplug the device). Returns
    /// `false` if not possible.
    fn remove(&self, target: &TargetId) -> bool {
        let _ = target;
        false
    }

    /// The number of threads, OS handles and GPU allocations the adapter currently
    /// owns, or `None` if it cannot tell.
    fn resources(&self) -> Option<u64> {
        None
    }
}

/// Runs a closure and returns the heap allocations plus frees it made on the calling
/// thread.
pub type MeasureAllocations = fn(&mut dyn FnMut()) -> u64;

/// Suite settings.
#[derive(Clone, Copy)]
pub struct Options<'a> {
    /// Time between polls.
    pub poll_interval: Duration,
    /// How long to wait for a frame before failing.
    pub frame_timeout: Duration,
    /// Frames to collect when checking timestamps and sequence numbers.
    pub frames_to_check: u32,
    /// Pause used to check that only the newest frame is returned.
    pub pause: Duration,
    /// Largest allowed age (`clock.now() - timestamp`) of a frame returned right after
    /// a pause.
    pub max_latency: Duration,
    /// Runs the closure and returns the number of heap allocations plus frees it made
    /// on the calling thread. When set, the suite checks that `latest_frame` (and
    /// dropping its frame) neither allocates nor frees. Adapter tests provide it, for
    /// example with the `allocation-counter` crate.
    pub measure_allocations: Option<MeasureAllocations>,
    /// Hooks to drive targets and count resources.
    pub hooks: Option<&'a dyn TestHooks>,
    /// Treat every skipped check as a failure (hardware runners).
    pub strict: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(5),
            frame_timeout: Duration::from_secs(2),
            frames_to_check: 8,
            pause: Duration::from_millis(500),
            max_latency: Duration::from_millis(100),
            measure_allocations: None,
            hooks: None,
            strict: false,
        }
    }
}

impl fmt::Debug for Options<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("poll_interval", &self.poll_interval)
            .field("frame_timeout", &self.frame_timeout)
            .field("frames_to_check", &self.frames_to_check)
            .field("pause", &self.pause)
            .field("max_latency", &self.max_latency)
            .field("measure_allocations", &self.measure_allocations.is_some())
            .field("hooks", &self.hooks.is_some())
            .field("strict", &self.strict)
            .finish()
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

fn symbols(caps: &CapabilitySet, key: &CapabilityKey) -> Vec<String> {
    match caps.get(key) {
        Some(CapabilityValue::Set(items)) => items.clone(),
        _ => Vec::new(),
    }
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

const ALL_MEMORY_KINDS: [MemoryKind; 7] = [
    MemoryKind::System,
    MemoryKind::Gpu,
    MemoryKind::D3d11,
    MemoryKind::D3d12,
    MemoryKind::Vulkan,
    MemoryKind::Metal,
    MemoryKind::DmaBuf,
];

/// Last delivered frame of a session, for monotonicity and drop accounting.
#[derive(Debug, Clone, Copy)]
struct Last {
    timestamp: Timestamp,
    sequence: u64,
    dropped: u64,
    delivered: u64,
}

struct Suite<'a> {
    factory: &'a dyn CaptureFactory,
    pacer: &'a mut dyn Pacer,
    options: Options<'a>,
    report: Report,
    pixel_formats: Vec<PixelFormat>,
    memory: Vec<MemoryKind>,
    in_flight: u64,
}

/// Runs the capture conformance suite against `factory`.
pub fn run(
    factory: &dyn CaptureFactory,
    pacer: &mut dyn Pacer,
    options: &Options<'_>,
) -> Result<Report, Failure> {
    let started = Instant::now();
    let probed = factory.probe();
    let probe_time = started.elapsed();
    ensure("probe", probe_time < Duration::from_millis(50), || {
        format!("probe took {probe_time:?}, limit is 50 ms")
    })?;
    let caps = match probed {
        Ok(caps) => caps,
        Err(e) if options.strict => return fail("probe", format!("probe failed: {e}")),
        Err(e) => {
            return Ok(Report {
                passed: vec!["probe".to_owned()],
                skipped: vec![format!("all: probe failed ({e})")],
            });
        }
    };
    let names = symbols(&caps, &caps::PIXEL_FORMATS);
    let pixel_formats: Vec<PixelFormat> = ALL_PIXEL_FORMATS
        .into_iter()
        .filter(|pf| names.iter().any(|n| n == pf.as_str()))
        .collect();
    let names = symbols(&caps, &caps::MEMORY);
    let memory: Vec<MemoryKind> = ALL_MEMORY_KINDS
        .into_iter()
        .filter(|m| names.iter().any(|n| n == m.as_str()))
        .collect();
    let in_flight = match caps.get(&caps::FRAMES_IN_FLIGHT) {
        Some(CapabilityValue::Int(n)) if *n >= 1 => *n as u64,
        _ => 1,
    };

    let mut suite = Suite {
        factory,
        pacer,
        options: *options,
        report: Report::default(),
        pixel_formats,
        memory,
        in_flight,
    };
    suite.pass("probe");
    if caps.is_empty() {
        suite.skip("all", "probe declared no capabilities");
        return suite.finish();
    }
    suite.run_checks(&caps)?;
    suite.finish()
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

    fn finish(self) -> Result<Report, Failure> {
        if self.options.strict && !self.report.skipped.is_empty() {
            return fail(
                "strict",
                format!("checks were skipped: {:?}", self.report.skipped),
            );
        }
        Ok(self.report)
    }

    fn resources(&self) -> Option<u64> {
        self.options.hooks.and_then(|h| h.resources())
    }

    fn request(
        &self,
        target: &TargetId,
        pixel_formats: &[PixelFormat],
        memory: &[MemoryKind],
    ) -> CaptureRequest {
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
        CaptureRequest::new(target.clone(), self.pacer.clock())
            .with_pixel_formats(pf)
            .with_memory(mem)
    }

    fn open(
        &self,
        target: &TargetId,
        pixel_formats: &[PixelFormat],
        memory: &[MemoryKind],
    ) -> Result<Box<dyn CaptureSession>, CaptureError> {
        self.factory
            .open(&self.request(target, pixel_formats, memory))
    }

    /// Polls until a frame arrives or the timeout passes.
    fn next_frame(
        &mut self,
        check: &'static str,
        id: &TargetId,
        session: &mut dyn CaptureSession,
    ) -> Result<VideoFrame, Failure> {
        let deadline = self.options.frame_timeout;
        let mut waited = Duration::ZERO;
        loop {
            if let Some(hooks) = self.options.hooks {
                hooks.stimulate(id);
            }
            match session.latest_frame() {
                Ok(Some(frame)) => return Ok(frame),
                Ok(None) => {}
                Err(e) => return fail(check, format!("{id}: latest_frame failed: {e}")),
            }
            if waited >= deadline {
                return fail(check, format!("{id}: no frame within {deadline:?}"));
            }
            self.pacer.wait(self.options.poll_interval);
            waited += self.options.poll_interval;
        }
    }

    fn run_checks(&mut self, caps: &CapabilitySet) -> Result<(), Failure> {
        ensure("caps", !self.pixel_formats.is_empty(), || {
            format!("{} declares no known pixel format", caps::PIXEL_FORMATS)
        })?;
        ensure("caps", !self.memory.is_empty(), || {
            format!("{} declares no known memory kind", caps::MEMORY)
        })?;
        self.pass("caps");

        let kinds = symbols(caps, &caps::TARGET_KINDS);
        let enumerable = symbols(caps, &caps::TARGET_ENUMERATE);
        let picker = symbols(caps, &caps::TARGET_PICKER);

        let mut testable: Vec<(TargetKind, TargetId)> = Vec::new();
        for kind in TargetKind::ALL {
            let name = kind.as_str();
            let declared = kinds.iter().any(|k| k == name);
            if declared && self.factory.permission(kind) != Permission::Granted {
                self.skip("sessions", &format!("{name}: permission not granted"));
                continue;
            }
            let listed = self.factory.enumerate(kind);
            if enumerable.iter().any(|k| k == name) {
                ensure("enumerate", declared, || {
                    format!("{name} is enumerable but not in {}", caps::TARGET_KINDS)
                })?;
                let targets = match listed {
                    Ok(targets) => targets,
                    Err(e) => {
                        return fail("enumerate", format!("{name}: declared but failed: {e}"));
                    }
                };
                if let Some(bad) = targets.iter().find(|t| t.kind != kind) {
                    return fail(
                        "enumerate",
                        format!("asked for {name}, got {} target {}", bad.kind, bad.id),
                    );
                }
                match targets.first() {
                    Some(t) => testable.push((kind, t.id.clone())),
                    None => self.skip("sessions", &format!("no {name} target on this machine")),
                }
            } else {
                ensure(
                    "enumerate",
                    matches!(listed, Err(CaptureError::Unsupported(_))),
                    || {
                        format!(
                            "{name} is not declared enumerable but enumerate returned {listed:?}"
                        )
                    },
                )?;
            }
            if picker.iter().any(|k| k == name) {
                // Calling pick would show a dialog; adapters test it manually.
                self.skip("pick", &format!("{name}: picker needs a user"));
            } else {
                let picked = self.factory.pick(kind);
                ensure(
                    "pick",
                    matches!(picked, Err(CaptureError::Unsupported(_))),
                    || format!("pick for {name} is not declared but returned {picked:?}"),
                )?;
            }
        }
        self.pass("enumerate");
        self.pass("pick");

        let unknown = self.open(
            &TargetId::new("org.rbs.conformance.no-such-target"),
            &[],
            &[],
        );
        ensure(
            "unknown target",
            matches!(unknown, Err(CaptureError::TargetNotFound)),
            || format!("open of an unknown id returned {:?}", unknown.err()),
        )?;
        self.pass("unknown target");

        if testable.is_empty() {
            self.skip("sessions", "no enumerable target to open");
            return Ok(());
        }

        self.check_formats(&testable)?;
        for (kind, id) in &testable {
            self.check_session(*kind, id)?;
        }
        // Target loss last: it removes the targets.
        match self.options.hooks {
            Some(_) => {
                for (_, id) in &testable {
                    self.check_target_lost(id)?;
                }
            }
            None => self.skip("target lost", "no test hooks given"),
        }
        Ok(())
    }

    /// Invariants 4 and 10: every declared format and memory kind can be delivered,
    /// and negotiation follows the request order.
    fn check_formats(&mut self, testable: &[(TargetKind, TargetId)]) -> Result<(), Failure> {
        const CHECK: &str = "formats";
        for pf in self.pixel_formats.clone() {
            let mut delivered = false;
            for (_, id) in testable {
                match self.open(id, &[pf], &[]) {
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
                        let frame = self.next_frame(CHECK, id, session.as_mut())?;
                        ensure(CHECK, frame.format().pixel_format == pf, || {
                            format!("asked for {pf}, frame is {}", frame.format().pixel_format)
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
                match self.open(id, &[], &[mem]) {
                    Ok(mut session) => {
                        session.start().map_err(|e| Failure {
                            check: CHECK,
                            detail: format!("start with {mem} failed: {e}"),
                        })?;
                        let frame = self.next_frame(CHECK, id, session.as_mut())?;
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

        // Order: with every declared format and memory kind requested in reverse
        // order, the adapter must pick the first one it supports for that target.
        for (_, id) in testable {
            let mut formats = self.pixel_formats.clone();
            formats.reverse();
            let mut memory = self.memory.clone();
            memory.reverse();
            let supported_pf: Vec<PixelFormat> = formats
                .iter()
                .copied()
                .filter(|pf| self.open(id, &[*pf], &[]).is_ok())
                .collect();
            let supported_mem: Vec<MemoryKind> = memory
                .iter()
                .copied()
                .filter(|m| self.open(id, &[], &[*m]).is_ok())
                .collect();
            let (Some(want_pf), Some(want_mem)) = (supported_pf.first(), supported_mem.first())
            else {
                continue;
            };
            let mut session = match self.open(id, &formats, &memory) {
                Ok(session) => session,
                // The first supported format and memory kind may not combine.
                Err(CaptureError::FormatUnsupported) => continue,
                Err(e) => return fail(CHECK, format!("{id}: open with all formats failed: {e}")),
            };
            ensure(CHECK, session.format().pixel_format == *want_pf, || {
                format!(
                    "{id}: request order {formats:?} should give {want_pf}, got {}",
                    session.format().pixel_format
                )
            })?;
            session.start().map_err(|e| Failure {
                check: CHECK,
                detail: format!("{id}: start failed: {e}"),
            })?;
            let frame = self.next_frame(CHECK, id, session.as_mut())?;
            ensure(CHECK, frame.memory_kind() == *want_mem, || {
                format!(
                    "{id}: memory order {memory:?} should give {want_mem}, got {}",
                    frame.memory_kind()
                )
            })?;
        }
        self.pass(CHECK);
        Ok(())
    }

    /// Invariants 1, 2, 3, 5, 6, 8 and 9 on one target, plus the allocation check.
    fn check_session(&mut self, kind: TargetKind, id: &TargetId) -> Result<(), Failure> {
        let baseline = self.resources();
        let mut session = self.open(id, &[], &[]).map_err(|e| Failure {
            check: "open",
            detail: format!("{kind} {id}: {e}"),
        })?;
        self.pass("open");

        // Invariant 1: nothing before start, no resources while in Standby.
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
        self.check_resources("standby", baseline, "after open")?;
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
        let mut last: Option<Last> = None;
        for _ in 0..self.options.frames_to_check {
            let frame = self.next_frame("timestamps", id, session.as_mut())?;
            self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
        }
        self.pass("timestamps");
        self.pass("sequence");

        // Invariant 5: after a long pause only the newest frame is returned.
        self.pacer.wait(self.options.pause);
        if let Some(hooks) = self.options.hooks {
            hooks.stimulate(id);
        }
        match session.latest_frame() {
            Ok(Some(frame)) => {
                self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
                let age = clock.now().saturating_since(frame.timestamp());
                ensure("latest wins", age <= self.options.max_latency, || {
                    format!(
                        "{id}: after a {:?} pause the frame returned is {age:?} old",
                        self.options.pause
                    )
                })?;
                self.pass("latest wins");
            }
            Ok(None) => self.skip("latest wins", &format!("{id}: no frame after the pause")),
            Err(e) => return fail("latest wins", format!("{id}: {e}")),
        }

        self.check_allocations(id, session.as_mut())?;
        self.check_in_flight(id, session.as_mut(), clock.as_ref(), &mut last)?;
        self.check_resize(kind, id, session.as_mut(), clock.as_ref(), &mut last)?;

        // Invariant 8: stop releases, is idempotent, and start works again.
        session.stop();
        session.stop();
        self.pacer.wait(self.options.poll_interval * 4);
        match session.latest_frame() {
            Ok(None) => {}
            other => {
                return fail(
                    "stop",
                    format!("{id}: latest_frame after stop returned {other:?}"),
                );
            }
        }
        self.check_resources("stop", baseline, "after stop")?;
        self.pass("stop");
        session.start().map_err(|e| Failure {
            check: "restart",
            detail: format!("{id}: {e}"),
        })?;
        let frame = self.next_frame("restart", id, session.as_mut())?;
        self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
        drop(frame);
        self.pass("restart");

        drop(session);
        self.check_resources("drop", baseline, "after drop")?;
        self.pass("drop");
        Ok(())
    }

    fn check_resources(
        &mut self,
        check: &'static str,
        baseline: Option<u64>,
        when: &str,
    ) -> Result<(), Failure> {
        match (baseline, self.resources()) {
            (Some(before), Some(now)) => ensure(check, now == before, || {
                format!("adapter owns {now} resources {when}, {before} before open")
            }),
            _ => {
                self.skip(check, "adapter does not report resources");
                Ok(())
            }
        }
    }

    fn check_allocations(
        &mut self,
        id: &TargetId,
        session: &mut dyn CaptureSession,
    ) -> Result<(), Failure> {
        let Some(measure) = self.options.measure_allocations else {
            self.skip("no allocation", "no allocation counter given");
            return Ok(());
        };
        for _ in 0..self.options.frames_to_check {
            self.pacer.wait(self.options.poll_interval);
            if let Some(hooks) = self.options.hooks {
                hooks.stimulate(id);
            }
            let mut failed = None;
            let used = measure(&mut || {
                if let Err(e) = session.latest_frame() {
                    failed = Some(e);
                }
            });
            if let Some(e) = failed {
                return fail("no allocation", format!("{id}: {e}"));
            }
            ensure("no allocation", used == 0, || {
                format!("{id}: latest_frame and dropping its frame allocated or freed {used} times")
            })?;
        }
        self.pass("no allocation");
        Ok(())
    }

    /// Invariant 9: holding fewer than `in_flight` frames never starves the consumer,
    /// and holding all of them never blocks.
    fn check_in_flight(
        &mut self,
        id: &TargetId,
        session: &mut dyn CaptureSession,
        clock: &dyn Clock,
        last: &mut Option<Last>,
    ) -> Result<(), Failure> {
        const CHECK: &str = "in flight";
        let mut held = Vec::new();
        while (held.len() as u64) < self.in_flight {
            let frame = self.next_frame(CHECK, id, session)?;
            self.check_frame(session, &frame, clock, last)?;
            held.push(frame);
        }
        for _ in 0..4 {
            self.pacer.wait(self.options.poll_interval);
            if let Some(hooks) = self.options.hooks {
                hooks.stimulate(id);
            }
            let started = Instant::now();
            let result = session.latest_frame();
            let took = started.elapsed();
            ensure(CHECK, took < Duration::from_millis(20), || {
                format!("{id}: latest_frame took {took:?} while every frame was held")
            })?;
            match result {
                Ok(Some(frame)) => self.check_frame(session, &frame, clock, last)?,
                Ok(None) => {}
                Err(e) => return fail(CHECK, format!("{id}: {e}")),
            }
        }
        held.clear();
        let frame = self.next_frame(CHECK, id, session)?;
        self.check_frame(session, &frame, clock, last)?;
        self.pass(CHECK);
        Ok(())
    }

    /// Invariant 6: a resized target delivers frames of the new size.
    fn check_resize(
        &mut self,
        kind: TargetKind,
        id: &TargetId,
        session: &mut dyn CaptureSession,
        clock: &dyn Clock,
        last: &mut Option<Last>,
    ) -> Result<(), Failure> {
        const CHECK: &str = "resize";
        if kind == TargetKind::Camera {
            return Ok(());
        }
        let size = session.format().size;
        let (w, h) = ((size.width / 2).max(2) & !1, (size.height / 2).max(2) & !1);
        let resized = self
            .options
            .hooks
            .is_some_and(|hooks| hooks.resize(id, w, h));
        if !resized {
            self.skip(CHECK, &format!("{id}: cannot resize"));
            return Ok(());
        }
        let mut waited = Duration::ZERO;
        loop {
            let frame = self.next_frame(CHECK, id, session)?;
            if frame.format().size.width == w && frame.format().size.height == h {
                self.check_frame(session, &frame, clock, last)?;
                break;
            }
            ensure(CHECK, waited < self.options.frame_timeout, || {
                format!(
                    "{id}: resized to {w}x{h}, frames still {}",
                    frame.format().size
                )
            })?;
            self.check_frame(session, &frame, clock, last)?;
            self.pacer.wait(self.options.poll_interval);
            waited += self.options.poll_interval;
        }
        let format = session.format();
        ensure(
            CHECK,
            (format.size.width, format.size.height) == (w, h),
            || format!("{id}: frames are {w}x{h} but format() says {}", format.size),
        )?;
        self.pass(CHECK);
        Ok(())
    }

    /// Invariant 7: a target that goes away gives `TargetLost`, not silence.
    fn check_target_lost(&mut self, id: &TargetId) -> Result<(), Failure> {
        const CHECK: &str = "target lost";
        let Some(hooks) = self.options.hooks else {
            return Ok(());
        };
        let mut session = self.open(id, &[], &[]).map_err(|e| Failure {
            check: CHECK,
            detail: format!("{id}: open failed: {e}"),
        })?;
        session.start().map_err(|e| Failure {
            check: CHECK,
            detail: format!("{id}: start failed: {e}"),
        })?;
        drop(self.next_frame(CHECK, id, session.as_mut())?);
        if !hooks.remove(id) {
            self.skip(CHECK, &format!("{id}: cannot remove"));
            return Ok(());
        }
        let mut waited = Duration::ZERO;
        loop {
            match session.latest_frame() {
                Err(CaptureError::TargetLost) => break,
                Err(e) => return fail(CHECK, format!("{id}: expected TargetLost, got {e}")),
                Ok(_) => {}
            }
            ensure(CHECK, waited < self.options.frame_timeout, || {
                format!(
                    "{id}: removed but no TargetLost within {:?}",
                    self.options.frame_timeout
                )
            })?;
            self.pacer.wait(self.options.poll_interval);
            waited += self.options.poll_interval;
        }
        self.pass(CHECK);
        Ok(())
    }

    /// Invariants 2, 3 and 4 for one frame.
    fn check_frame(
        &self,
        session: &dyn CaptureSession,
        frame: &VideoFrame,
        clock: &dyn Clock,
        last: &mut Option<Last>,
    ) -> Result<(), Failure> {
        let now = clock.now();
        let stats: CaptureStats = session.stats();
        ensure("timestamps", frame.timestamp() <= now, || {
            format!(
                "frame at {} ns is ahead of the clock ({} ns)",
                frame.timestamp().as_nanos(),
                now.as_nanos()
            )
        })?;
        if let Some(prev) = *last {
            ensure("timestamps", frame.timestamp() > prev.timestamp, || {
                format!(
                    "timestamp {} ns does not increase (previous {} ns)",
                    frame.timestamp().as_nanos(),
                    prev.timestamp.as_nanos()
                )
            })?;
            ensure("sequence", frame.sequence() > prev.sequence, || {
                format!(
                    "sequence {} does not increase (previous {})",
                    frame.sequence(),
                    prev.sequence
                )
            })?;
            // Frames between the two were either delivered (to calls the suite did not
            // inspect) or counted as dropped.
            let gap = frame.sequence() - prev.sequence - 1;
            let dropped = stats.dropped.saturating_sub(prev.dropped);
            let unseen = stats.delivered.saturating_sub(prev.delivered + 1);
            ensure("sequence", gap == dropped + unseen, || {
                format!(
                    "sequence jumped from {} to {} but {dropped} frames were counted as dropped and {unseen} delivered in between",
                    prev.sequence,
                    frame.sequence()
                )
            })?;
        }
        *last = Some(Last {
            timestamp: frame.timestamp(),
            sequence: frame.sequence(),
            dropped: stats.dropped,
            delivered: stats.delivered,
        });

        let format = session.format();
        ensure("format", *frame.format() == format, || {
            format!(
                "frame format {:?} differs from session format {format:?}",
                frame.format()
            )
        })?;
        ensure("format", self.memory.contains(&frame.memory_kind()), || {
            format!("frame memory {} was not requested", frame.memory_kind())
        })?;
        Ok(())
    }
}
