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
    CaptureError, CaptureFactory, CaptureRequest, CaptureSession, GpuDeviceId, GpuImportTarget,
    Permission, SyncKind, TargetId, TargetKind,
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
    /// How much older than the first post-pause call a second, immediate call may
    /// return a frame. Frames carry their capture time, not their delivery time: a
    /// frame captured just before the first call can legitimately be delivered just
    /// after it. Zero for adapters that stamp frames on delivery; asynchronous sources
    /// set a few milliseconds. It must stay far below one frame interval, or a queue
    /// one frame deep would pass.
    pub delivery_slack: Duration,
    /// Runs the closure and returns the number of heap allocations plus frees it made
    /// on the calling thread. When set, the suite checks that `latest_frame` (and
    /// dropping its frame) neither allocates nor frees. Adapter tests provide it, for
    /// example with the `allocation-counter` crate.
    pub measure_allocations: Option<MeasureAllocations>,
    /// Hooks to drive targets and count resources.
    pub hooks: Option<&'a dyn TestHooks>,
    /// Treat every skipped check as a failure (hardware runners).
    pub strict: bool,
    /// The import target to request GPU memory kinds with: the device the test
    /// harness will import frames into. Without it, declared GPU memory kinds are
    /// skipped (a failure in strict mode).
    pub gpu: Option<&'a GpuImportTarget>,
    /// Lists the thread ids of the whole process, independently of what the adapter
    /// reports (see [`process_thread_ids`]). Only meaningful when the test binary runs
    /// one test at a time (`--test-threads=1`).
    pub process_threads: Option<fn() -> Option<Vec<u64>>>,
}

/// The ids of this process's threads, where the platform makes that cheap to read
/// without extra dependencies (Linux: `/proc/self/task`). `None` elsewhere.
pub fn process_thread_ids() -> Option<Vec<u64>> {
    if cfg!(target_os = "linux") {
        let entries = std::fs::read_dir("/proc/self/task").ok()?;
        Some(
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect(),
        )
    } else {
        None
    }
}

/// Whether the thread `id` of this process is exiting (Linux `PF_EXITING` in its
/// `stat` flags) or already gone. `false` where it cannot be told.
fn thread_exiting(id: u64) -> bool {
    /// `PF_EXITING`: set when the task enters `do_exit`, before a joiner wakes.
    const PF_EXITING: u64 = 0x4;
    if !cfg!(target_os = "linux") {
        return false;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/self/task/{id}/stat")) else {
        return true;
    };
    // Fields after the parenthesised name: state, ppid, pgrp, session, tty_nr, tpgid,
    // flags.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(6))
        .and_then(|flags| flags.parse::<u64>().ok())
        .is_some_and(|flags| flags & PF_EXITING != 0)
}

/// Threads in `now` that are not in `before`.
fn new_threads(before: &[u64], now: &[u64]) -> Vec<u64> {
    now.iter()
        .copied()
        .filter(|t| !before.contains(t))
        .collect()
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(5),
            frame_timeout: Duration::from_secs(2),
            frames_to_check: 8,
            pause: Duration::from_millis(500),
            max_latency: Duration::from_millis(100),
            delivery_slack: Duration::ZERO,
            measure_allocations: None,
            hooks: None,
            strict: false,
            gpu: None,
            process_threads: None,
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
            .field("delivery_slack", &self.delivery_slack)
            .field("measure_allocations", &self.measure_allocations.is_some())
            .field("hooks", &self.hooks.is_some())
            .field("strict", &self.strict)
            .field("gpu", &self.gpu)
            .field("process_threads", &self.process_threads.is_some())
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

/// Longest a probe may take before the suite calls it hung (capture-port.md,
/// Conformance tests). A hang guard, not the 50 ms performance budget: a pass/fail
/// test must not depend on how fast the runner is (ADR-0032); the budget is measured
/// with benchmarks.
const PROBE_GUARD: Duration = Duration::from_secs(1);

/// Probe calls the suite times; the fastest must be within [`PROBE_GUARD`].
const PROBE_ATTEMPTS: u32 = 3;

/// Longest a `latest_frame` may take while every frame is held before the suite calls
/// it blocking: a hang guard, far above a non-blocking call, so scheduling delays on a
/// loaded runner never fail the suite (ADR-0032).
const LATEST_FRAME_GUARD: Duration = Duration::from_millis(500);

/// Runs the capture conformance suite against `factory`.
pub fn run(
    factory: &dyn CaptureFactory,
    pacer: &mut dyn Pacer,
    options: &Options<'_>,
) -> Result<Report, Failure> {
    // The fastest of a few calls, so one-time process costs (loading system frameworks
    // on the first call) do not count. A probe that hangs every time still fails.
    let mut probe_time = Duration::MAX;
    let mut probed = None;
    for _ in 0..PROBE_ATTEMPTS {
        let started = Instant::now();
        let result = factory.probe();
        probe_time = probe_time.min(started.elapsed());
        probed = Some(result);
        if probe_time < PROBE_GUARD {
            break;
        }
    }
    ensure("probe", probe_time < PROBE_GUARD, || {
        format!(
            "probe took at least {probe_time:?} in {PROBE_ATTEMPTS} calls, limit is {PROBE_GUARD:?}"
        )
    })?;
    let Some(probed) = probed else {
        return fail("probe", "probe was not called".to_owned());
    };
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

    fn resources(&self) -> (Option<u64>, Option<Vec<u64>>) {
        (
            self.options.hooks.and_then(|h| h.resources()),
            self.options.process_threads.and_then(|ids| ids()),
        )
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
        let request = CaptureRequest::new(target.clone(), self.pacer.clock())
            .with_pixel_formats(pf)
            .with_memory(mem);
        match self.options.gpu {
            Some(gpu) => request.with_gpu(gpu.clone()),
            None => request,
        }
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

        if self.options.gpu.is_none() && !self.memory.contains(&MemoryKind::System) {
            // A GPU-only adapter cannot open a session without an import target. The
            // refusal itself is still checked.
            for mem in self.memory.clone() {
                self.check_gpu_import(&testable, mem)?;
            }
            self.skip(
                "sessions",
                "only GPU memory kinds are declared and no GPU import target was given",
            );
            self.skip("target lost", "no session can be opened");
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
            if mem != MemoryKind::System {
                self.check_gpu_import(testable, mem)?;
                if self.options.gpu.is_none() {
                    self.skip(CHECK, &format!("{mem}: no GPU import target given"));
                    continue;
                }
            }
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

    /// Zero-copy negotiation: a GPU memory kind is refused without an import target and
    /// for a device the adapter cannot share with.
    fn check_gpu_import(
        &mut self,
        testable: &[(TargetKind, TargetId)],
        mem: MemoryKind,
    ) -> Result<(), Failure> {
        const CHECK: &str = "gpu import";
        let foreign = GpuImportTarget::new(GpuDeviceId::Uuid([0xA5; 16]), &[SyncKind::Ready]);
        for (_, id) in testable {
            let bare = CaptureRequest::new(id.clone(), self.pacer.clock())
                .with_pixel_formats(&self.pixel_formats)
                .with_memory(&[mem]);
            for (what, request) in [
                ("without an import target", bare.clone()),
                ("for a foreign device", bare.with_gpu(foreign.clone())),
            ] {
                let result = self.factory.open(&request);
                ensure(
                    CHECK,
                    matches!(result, Err(CaptureError::FormatUnsupported)),
                    || {
                        format!(
                            "{id}: {mem} {what} must be FormatUnsupported, got {:?}",
                            result.err()
                        )
                    },
                )?;
            }
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
        self.check_resources("standby", &baseline, "after open")?;
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

        // An id the adapter asks the host to store must open.
        if let Some(stored) = session.target_to_store() {
            let reopened = self.open(&stored, &[], &[]);
            ensure("target to store", reopened.is_ok(), || {
                format!(
                    "{id}: target_to_store {stored} does not open: {:?}",
                    reopened.err()
                )
            })?;
            self.pass("target to store");
        }

        let clock = self.pacer.clock();
        let stats_at_start = session.stats();
        let mut last: Option<Last> = None;
        // Largest gap between consecutive frames: the target's frame interval as seen
        // by the consumer, used to judge frame age below.
        let mut interval = Duration::ZERO;
        let mut first_sequence: Option<u64> = None;
        for _ in 0..self.options.frames_to_check {
            let frame = self.next_frame("timestamps", id, session.as_mut())?;
            first_sequence.get_or_insert(frame.sequence());
            if let Some(prev) = last {
                interval = interval.max(frame.timestamp().saturating_since(prev.timestamp));
            }
            self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
        }
        self.pass("timestamps");
        self.pass("sequence");

        // Invariant 5: after a long pause only the newest frame is returned.
        self.pacer.wait(self.options.pause);
        if let Some(hooks) = self.options.hooks {
            hooks.stimulate(id);
        }
        let called_at = clock.now();
        match session.latest_frame() {
            Ok(Some(frame)) => {
                self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
                let age = called_at.saturating_since(frame.timestamp());
                let limit = interval + self.options.max_latency;
                ensure("latest wins", age <= limit, || {
                    format!(
                        "{id}: after a {:?} pause the frame returned is {age:?} old (limit {limit:?})",
                        self.options.pause
                    )
                })?;
                // A second call right away may only return a frame captured after the
                // first call, never an older queued one (no slack).
                match session.latest_frame() {
                    Ok(Some(next)) => {
                        let stale_by = called_at.saturating_since(next.timestamp());
                        ensure(
                            "latest wins",
                            stale_by <= self.options.delivery_slack,
                            || {
                                format!(
                                    "{id}: a second call returned a frame {stale_by:?} older than the first call (slack {:?})",
                                    self.options.delivery_slack
                                )
                            },
                        )?;
                        self.check_frame(session.as_ref(), &next, clock.as_ref(), &mut last)?;
                    }
                    Ok(None) => {}
                    Err(e) => return fail("latest wins", format!("{id}: {e}")),
                }
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
        self.check_resources("stop", &baseline, "after stop")?;
        self.pass("stop");

        // Invariant 3, accounted once over the whole run while the adapter is quiet:
        // every sequence number from the first to the last frame was delivered or
        // counted as dropped. Checking per pair of frames would race with the OS
        // thread counting drops between `latest_frame` and `stats`.
        if let (Some(first), Some(prev)) = (first_sequence, last) {
            let stats = session.stats();
            let span = prev.sequence - first + 1;
            let delivered = stats.delivered - stats_at_start.delivered;
            let dropped = stats.dropped - stats_at_start.dropped;
            ensure("sequence", span <= delivered + dropped, || {
                format!(
                    "{id}: sequence numbers {first}..={} span {span} frames but only {delivered} were delivered and {dropped} counted as dropped",
                    prev.sequence
                )
            })?;
        }
        session.start().map_err(|e| Failure {
            check: "restart",
            detail: format!("{id}: {e}"),
        })?;
        let frame = self.next_frame("restart", id, session.as_mut())?;
        self.check_frame(session.as_ref(), &frame, clock.as_ref(), &mut last)?;
        self.pass("restart");

        // The last frame held across `stop` is dropped by the render thread too, so
        // that drop must not free (reclaiming happens on the session's own drop).
        session.stop();
        match self.options.measure_allocations {
            Some(measure) => {
                let mut held = Some(frame);
                let used = measure(&mut || drop(held.take()));
                ensure("no allocation", used == 0, || {
                    format!(
                        "{id}: dropping a frame held across stop allocated or freed {used} times"
                    )
                })?;
            }
            None => drop(frame),
        }

        drop(session);
        self.check_resources("drop", &baseline, "after drop")?;
        self.pass("drop");
        Ok(())
    }

    fn check_resources(
        &mut self,
        check: &'static str,
        baseline: &(Option<u64>, Option<Vec<u64>>),
        when: &str,
    ) -> Result<(), Failure> {
        let now = self.resources();
        match (baseline.0, now.0) {
            (Some(before), Some(now)) => ensure(check, now == before, || {
                format!("adapter reports {now} resources {when}, {before} before open")
            })?,
            _ => self.skip(check, "adapter does not report resources"),
        }
        if self.options.process_threads.is_some() {
            // Threads are compared by id: a thread that existed before open may end
            // meanwhile (an earlier session's thread still exiting), which is fine;
            // a thread that did not exist before is the adapter's, unless it is
            // already exiting. A joined thread can still be listed for a moment,
            // because the kernel wakes the joiner before it removes the exiting task;
            // such a task is marked exiting, so it is told apart without waiting.
            match (&baseline.1, &now.1) {
                (Some(before), Some(now)) => {
                    let new: Vec<u64> = new_threads(before, now)
                        .into_iter()
                        .filter(|id| !thread_exiting(*id))
                        .collect();
                    ensure(check, new.is_empty(), || {
                        format!(
                            "process has {} new threads {when} ({new:?}; {} before open, {} now)",
                            new.len(),
                            before.len(),
                            now.len()
                        )
                    })?;
                }
                _ => self.skip(check, "process thread count unavailable"),
            }
        }
        Ok(())
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
            ensure(CHECK, took < LATEST_FRAME_GUARD, || {
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
        }
        *last = Some(Last {
            timestamp: frame.timestamp(),
            sequence: frame.sequence(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_threads_ignore_ended_ones() {
        assert_eq!(new_threads(&[1, 2, 3], &[1, 3]), Vec::<u64>::new());
        assert_eq!(new_threads(&[1, 2], &[1, 2, 7]), vec![7]);
    }
}
