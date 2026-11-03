//! The conformance suite every audio input adapter runs in its own tests.
//!
//! ```ignore
//! let report = rbs_audio_io::conformance::run(&backend, &mut TestPacer::new(), &Options::default());
//! report.assert_ran(&["probe", "enumerate", "timestamps"]);
//! ```
//!
//! Each check maps to an invariant in `docs/specs/audio-io-port.md`. Checks that need
//! an endpoint the machine does not have (no microphone on a CI runner) or a hook the
//! adapter does not provide are skipped. With [`Options::strict`] (hardware runners
//! and the reference adapter), any skip is a failure.
//!
//! Test support: allocates freely and is never used on a runtime path.

use std::collections::HashSet;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::conformance::{Outcome, guarded, probe_case, set_values};
use rbs_core::time::{Clock, MonotonicClock, TestClock, Timestamp};

pub use rbs_core::conformance::Report;

use crate::caps;
use crate::{
    AudioBackend, AudioEndpoint, AudioInputStream, AudioIoError, AudioRead, EndpointId,
    EndpointKind, InputRequest,
};

/// Lets time pass between reads.
///
/// Software adapters driven by the request's clock use [`TestPacer`], so the suite is
/// deterministic. Hardware adapters, whose audio arrives on OS threads, use
/// [`RealTimePacer`].
pub trait Pacer {
    /// The master clock to put in requests. The suite also measures its time bounds
    /// with it.
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
    /// A pacer whose clock starts at 1 s (so audio is never at the origin).
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

/// A pacer over the real monotonic clock: `wait` sleeps. Only for hardware adapters,
/// whose tests are gated (ADR-0032).
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

/// Adapter-provided hooks that let the suite drive endpoints and observe resources.
pub trait TestHooks {
    /// Makes `endpoint` disappear (unplug the device, end the application). Returns
    /// `false` if not possible.
    fn remove(&self, endpoint: &EndpointId) -> bool {
        let _ = endpoint;
        false
    }

    /// The number of threads, OS streams and device handles the adapter currently
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
    /// Time between reads.
    pub poll_interval: Duration,
    /// How long to wait for the first audio after `start`, and for `EndpointLost`.
    pub audio_timeout: Duration,
    /// How much audio to collect for the timestamp checks.
    pub run_length: Duration,
    /// Largest error allowed between a read's timestamp and the one its run predicts,
    /// and for a timestamp ahead of the clock.
    pub timestamp_tolerance: Duration,
    /// Endpoints of each kind to exercise (machines can have many).
    pub max_endpoints: usize,
    /// Measures allocations of `read`; without it that check is skipped.
    pub measure_allocations: Option<MeasureAllocations>,
    /// Hooks for endpoint removal and resource counting.
    pub hooks: Option<&'a dyn TestHooks>,
    /// Every skip is a failure.
    pub strict: bool,
}

impl Default for Options<'_> {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(10),
            audio_timeout: Duration::from_secs(2),
            run_length: Duration::from_millis(300),
            timestamp_tolerance: Duration::from_millis(2),
            max_endpoints: 2,
            measure_allocations: None,
            hooks: None,
            strict: false,
        }
    }
}

/// An endpoint id no adapter uses.
const MISSING: &str = "org.rbs.conformance.missing-endpoint";

/// Frames per read buffer.
const READ_FRAMES: usize = 2_048;

/// Most reads between two waits (2 s of 48 kHz audio at `READ_FRAMES`).
const READS_PER_POLL: usize = 64;

/// Runs the suite. Never panics on adapter misbehaviour: failures are cases.
pub fn run<B: AudioBackend + ?Sized>(
    backend: &B,
    pacer: &mut dyn Pacer,
    options: &Options<'_>,
) -> Report {
    let mut report = Report::default();
    if let Some(caps) = probe_case(&mut report, backend) {
        let mut suite = Suite {
            backend,
            pacer,
            options,
            report: &mut report,
        };
        suite.run(&caps);
    }
    if options.strict {
        for case in &mut report.cases {
            if let Outcome::Skipped(reason) = &case.outcome {
                case.outcome = Outcome::Failed(format!("skipped in strict mode: {reason}"));
            }
        }
    }
    report
}

struct Suite<'s, 'o, B: AudioBackend + ?Sized> {
    backend: &'s B,
    pacer: &'s mut dyn Pacer,
    options: &'s Options<'o>,
    report: &'s mut Report,
}

/// Planar read buffers for one stream.
struct Buffers {
    channels: Vec<Vec<f32>>,
}

impl Buffers {
    fn new(channels: usize, frames: usize) -> Self {
        Self {
            channels: vec![vec![0.0; frames]; channels],
        }
    }

    fn read(&mut self, stream: &mut dyn AudioInputStream) -> Result<AudioRead, AudioIoError> {
        let mut slices: Vec<&mut [f32]> =
            self.channels.iter_mut().map(|c| c.as_mut_slice()).collect();
        stream.read(&mut slices)
    }
}

/// Bookkeeping of a stream's reads, checked against the spec's invariants.
#[derive(Default)]
struct Reads {
    delivered: u64,
    last: Option<AudioRead>,
    errors: Vec<String>,
}

impl Reads {
    fn record(&mut self, read: AudioRead, now: Timestamp, rate: u32, tolerance: Duration) {
        if read.frames == 0 {
            return;
        }
        self.delivered += read.frames as u64;
        let duration = duration_of(read.frames as u64, rate);
        if read.timestamp + duration > now + tolerance {
            self.errors.push(format!(
                "read ending at {:?} is ahead of the clock ({now:?})",
                read.timestamp + duration
            ));
        }
        match self.last {
            None if !read.discontinuity => self
                .errors
                .push("the first read after start is not a discontinuity".into()),
            Some(last) => {
                if read.timestamp <= last.timestamp {
                    self.errors.push(format!(
                        "timestamp {:?} does not increase (previous {:?})",
                        read.timestamp, last.timestamp
                    ));
                }
                if !read.discontinuity {
                    let expected = last.timestamp + duration_of(last.frames as u64, rate);
                    let error = if read.timestamp > expected {
                        read.timestamp.saturating_since(expected)
                    } else {
                        expected.saturating_since(read.timestamp)
                    };
                    if error > tolerance {
                        self.errors.push(format!(
                            "read at {:?} continues a run expected at {expected:?} (error {error:?})",
                            read.timestamp
                        ));
                    }
                }
            }
            None => {}
        }
        self.last = Some(read);
    }
}

fn duration_of(frames: u64, rate: u32) -> Duration {
    Duration::from_nanos(
        u64::try_from(u128::from(frames) * 1_000_000_000 / u128::from(rate.max(1)))
            .unwrap_or(u64::MAX),
    )
}

impl<B: AudioBackend + ?Sized> Suite<'_, '_, B> {
    fn skip(&mut self, name: &str, reason: impl Into<String>) {
        self.report.record(name, Outcome::Skipped(reason.into()));
    }

    fn resources(&self) -> Option<u64> {
        self.options.hooks.and_then(|h| h.resources())
    }

    fn request(&self, endpoint: &AudioEndpoint) -> InputRequest {
        InputRequest::new(endpoint.id.clone(), self.pacer.clock())
    }

    fn open(&self, request: &InputRequest) -> Result<Box<dyn AudioInputStream>, String> {
        guarded(|| {
            self.backend
                .open_input(request)
                .map_err(|e| format!("open {}: {e}", request.endpoint))
        })
    }

    fn run(&mut self, caps: &CapabilitySet) {
        let endpoints = self.enumerate(caps);
        let missing = guarded(|| {
            let request = InputRequest::new(EndpointId::new(MISSING), self.pacer.clock());
            match self.backend.open_input(&request) {
                Err(AudioIoError::EndpointNotFound) => Ok(()),
                Err(e) => Err(format!("expected EndpointNotFound, got {e}")),
                Ok(_) => Err("opened an endpoint that does not exist".into()),
            }
        });
        self.report.check("unknown endpoint", missing);

        if endpoints.is_empty() {
            self.skip("endpoints", "no endpoints on this machine");
            return;
        }
        for endpoint in &endpoints {
            self.endpoint(endpoint);
        }
    }

    fn enumerate(&mut self, caps: &CapabilitySet) -> Vec<AudioEndpoint> {
        let kinds = set_values(caps, &caps::INPUT_KINDS);
        let listed = set_values(caps, &caps::INPUT_ENUMERATE);
        let max_channels = match caps.get(&caps::INPUT_CHANNELS_MAX) {
            Some(CapabilityValue::Int(n)) => usize::try_from(*n).unwrap_or(0),
            _ => rbs_core::media::MAX_CHANNELS,
        };
        let before = self.resources();
        let mut endpoints = Vec::new();
        let mut errors = Vec::new();
        for kind in EndpointKind::ALL {
            let result = guarded(|| self.backend.enumerate(kind).map_err(|e| e.to_string()));
            if !listed.iter().any(|k| k == kind.as_str()) {
                if !matches!(&result, Err(e) if e.starts_with("unsupported")) {
                    errors.push(format!(
                        "{kind}: not in {} but enumerate did not return Unsupported",
                        caps::INPUT_ENUMERATE.as_str()
                    ));
                }
                continue;
            }
            let list = match result {
                Ok(list) => list,
                Err(e) => {
                    errors.push(format!("{kind}: {e}"));
                    continue;
                }
            };
            let mut ids = HashSet::new();
            for endpoint in &list {
                if endpoint.kind != kind {
                    errors.push(format!(
                        "{}: listed for {kind}, is {}",
                        endpoint.id, endpoint.kind
                    ));
                }
                if !ids.insert(endpoint.id.clone()) {
                    errors.push(format!("{}: listed twice", endpoint.id));
                }
                if !endpoint.sample_rates.contains(&endpoint.format.sample_rate) {
                    errors.push(format!("{}: native rate not in sample_rates", endpoint.id));
                }
                if endpoint.format.channels.channels() > max_channels {
                    errors.push(format!("{}: more channels than declared", endpoint.id));
                }
            }
            if !kinds.iter().any(|k| k == kind.as_str()) {
                errors.push(format!(
                    "{kind}: listed but not in {}",
                    caps::INPUT_KINDS.as_str()
                ));
            }
            endpoints.extend(list.into_iter().take(self.options.max_endpoints));
        }
        if before != self.resources() {
            errors.push(format!(
                "enumerate kept resources: {before:?} before, {:?} after",
                self.resources()
            ));
        }
        self.report.check(
            "enumerate",
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            },
        );
        endpoints
    }

    fn endpoint(&mut self, endpoint: &AudioEndpoint) {
        let id = endpoint.id.clone();
        self.format(endpoint);

        let baseline = self.resources();
        let request = self.request(endpoint);
        let mut stream = match self.open(&request) {
            Ok(stream) => stream,
            Err(e) => {
                self.report.check("open", Err(e));
                return;
            }
        };
        let format = stream.format();
        let rate = format.sample_rate.hz();
        let channels = format.channels.channels();
        let mut buffers = Buffers::new(channels, READ_FRAMES);
        let tolerance = self.options.timestamp_tolerance;

        // Standby: nothing owned, nothing delivered.
        self.pacer.wait(self.options.poll_interval * 5);
        let standby = (|| {
            if self.resources() != baseline {
                return Err(format!(
                    "{id}: open acquired resources ({baseline:?} -> {:?})",
                    self.resources()
                ));
            }
            match buffers.read(stream.as_mut()) {
                Ok(read) if read.frames == 0 => {}
                other => return Err(format!("{id}: read in Standby returned {other:?}")),
            }
            if stream.available() != 0 || stream.stats() != Default::default() {
                return Err(format!(
                    "{id}: audio counted in Standby: {:?}",
                    stream.stats()
                ));
            }
            Ok(())
        })();
        self.report.check("standby", standby);

        let mut wrong = Buffers::new(channels + 1, 16);
        let mismatch = match wrong.read(stream.as_mut()) {
            Err(AudioIoError::BufferMismatch) => Ok(()),
            other => Err(format!("{id}: wrong channel count returned {other:?}")),
        };
        self.report.check("buffer mismatch", mismatch);

        match stream.start() {
            Ok(()) => {}
            Err(e @ (AudioIoError::PermissionDenied | AudioIoError::PermissionNotDetermined)) => {
                self.skip("start", format!("{id}: {e}"));
                return;
            }
            Err(e) => {
                self.report.check("start", Err(format!("{id}: {e}")));
                return;
            }
        }
        let clock = self.pacer.clock();

        // Delivery and timestamps over one run.
        let mut reads = Reads::default();
        let started = clock.now();
        let wanted = duration_of(1, rate).as_nanos().max(1);
        let wanted = (self.options.run_length.as_nanos() / wanted) as u64;
        let mut lost = None;
        while reads.delivered < wanted {
            if reads.delivered == 0
                && clock.now().saturating_since(started) > self.options.audio_timeout
            {
                break;
            }
            if reads.delivered > 0
                && clock.now().saturating_since(started)
                    > self.options.audio_timeout + self.options.run_length * 4
            {
                break;
            }
            self.pacer.wait(self.options.poll_interval);
            // Bounded, so an adapter that returns audio forever fails instead of
            // hanging the suite (ADR-0032).
            for _ in 0..READS_PER_POLL {
                match buffers.read(stream.as_mut()) {
                    Ok(read) if read.frames > 0 => reads.record(read, clock.now(), rate, tolerance),
                    Ok(_) => break,
                    Err(e) => {
                        lost = Some(e);
                        break;
                    }
                }
            }
            if lost.is_some() {
                break;
            }
        }
        let delivery = match lost {
            Some(e) => Err(format!("{id}: read failed: {e}")),
            None if reads.delivered == 0 => Err(format!(
                "{id}: no audio within {:?}",
                self.options.audio_timeout
            )),
            None if reads.delivered < wanted => Err(format!(
                "{id}: only {} of {wanted} frames arrived",
                reads.delivered
            )),
            None => Ok(()),
        };
        let delivered_ok = delivery.is_ok();
        self.report.check("delivery", delivery);
        if !delivered_ok {
            stream.stop();
            return;
        }
        let timestamps = if reads.errors.is_empty() {
            Ok(())
        } else {
            Err(format!("{id}: {}", reads.errors.join("; ")))
        };
        self.report.check("timestamps", timestamps);

        // No allocation or free in `read`, `available` and `stats`.
        match self.options.measure_allocations {
            Some(measure) => {
                // Measured reads are repeated until one returns audio: OS packets can
                // be longer than the poll interval (a PipeWire quantum of 1024 frames
                // is 21 ms), so a fixed wait may find nothing new. Every attempt,
                // empty or not, must neither allocate nor free.
                let deadline = clock.now() + self.options.audio_timeout;
                let mut allocations = 0;
                let mut result = Ok(AudioRead::EMPTY);
                loop {
                    self.pacer.wait(self.options.poll_interval);
                    let mut slices: Vec<&mut [f32]> = buffers
                        .channels
                        .iter_mut()
                        .map(|c| c.as_mut_slice())
                        .collect();
                    allocations += measure(&mut || {
                        result = stream.read(&mut slices);
                        let _ = stream.available();
                        let _ = stream.stats();
                    });
                    drop(slices);
                    let empty = matches!(result, Ok(read) if read.frames == 0);
                    if !empty || clock.now() > deadline {
                        break;
                    }
                }
                if let Ok(read) = result {
                    reads.record(read, clock.now(), rate, tolerance);
                }
                self.report.check(
                    "no allocation",
                    match (allocations, result) {
                        (0, Ok(read)) if read.frames > 0 => Ok(()),
                        (0, other) => Err(format!("{id}: read returned {other:?}")),
                        (n, _) => Err(format!("{id}: read allocated or freed {n} times")),
                    },
                );
            }
            None => self.skip("no allocation", "no allocation counter"),
        }

        // Overflow: a consumer that stops reading loses audio, counted, and the
        // audio after the loss starts a new run.
        let before = stream.stats();
        self.pacer
            .wait(request.clamped_buffer() * 2 + self.options.poll_interval);
        let mut new_run = false;
        let deadline = clock.now() + self.options.audio_timeout;
        let mut reads_since_wait = 0;
        while !new_run && clock.now() <= deadline {
            if reads_since_wait == READS_PER_POLL {
                self.pacer.wait(self.options.poll_interval);
                reads_since_wait = 0;
            }
            reads_since_wait += 1;
            match buffers.read(stream.as_mut()) {
                Ok(read) if read.frames > 0 => {
                    new_run |= read.discontinuity;
                    reads.record(read, clock.now(), rate, tolerance);
                }
                Ok(_) => self.pacer.wait(self.options.poll_interval),
                Err(e) => {
                    reads
                        .errors
                        .push(format!("read failed after overflow: {e}"));
                    break;
                }
            }
        }
        let after = stream.stats();
        let overflow = if after.dropped <= before.dropped {
            Err(format!(
                "{id}: no audio counted as dropped after not reading: {after:?}"
            ))
        } else if !new_run {
            Err(format!(
                "{id}: no discontinuity reported after dropped audio"
            ))
        } else if after.discontinuities <= before.discontinuities {
            Err(format!("{id}: discontinuities not counted: {after:?}"))
        } else {
            Ok(())
        };
        self.report.check("overflow", overflow);

        // Stop: nothing read afterwards, every captured frame accounted for.
        let last = reads.last;
        stream.stop();
        let stats = stream.stats();
        let accounting = if stats.delivered != reads.delivered {
            Err(format!(
                "{id}: delivered {} but reads returned {}",
                stats.delivered, reads.delivered
            ))
        } else if stats.captured != stats.delivered + stats.dropped {
            Err(format!(
                "{id}: captured != delivered + dropped after stop: {stats:?}"
            ))
        } else {
            Ok(())
        };
        self.report.check("accounting", accounting);
        self.pacer.wait(self.options.poll_interval * 3);
        let stop = match buffers.read(stream.as_mut()) {
            Ok(read) if read.frames == 0 && stream.available() == 0 => {
                if self.resources() == baseline {
                    Ok(())
                } else {
                    Err(format!(
                        "{id}: resources after stop {:?}, before open {baseline:?}",
                        self.resources()
                    ))
                }
            }
            other => Err(format!("{id}: read after stop returned {other:?}")),
        };
        self.report.check("stop", stop);
        self.report.check(
            "reads",
            if reads.errors.is_empty() {
                Ok(())
            } else {
                Err(format!("{id}: {}", reads.errors.join("; ")))
            },
        );

        // Restart: a new run, later than everything before.
        let restart = (|| {
            stream.start().map_err(|e| format!("{id}: restart: {e}"))?;
            let deadline = clock.now() + self.options.audio_timeout;
            loop {
                if clock.now() > deadline {
                    return Err(format!("{id}: no audio after restart"));
                }
                self.pacer.wait(self.options.poll_interval);
                let read = buffers
                    .read(stream.as_mut())
                    .map_err(|e| format!("{id}: read after restart: {e}"))?;
                if read.frames == 0 {
                    continue;
                }
                if !read.discontinuity {
                    return Err(format!(
                        "{id}: first read after restart is not a discontinuity"
                    ));
                }
                if let Some(last) = last
                    && read.timestamp <= last.timestamp
                {
                    return Err(format!(
                        "{id}: timestamp {:?} after restart is not after {:?}",
                        read.timestamp, last.timestamp
                    ));
                }
                return Ok(());
            }
        })();
        self.report.check("restart", restart);
        stream.stop();
        drop(stream);
        self.report.check(
            "drop",
            if self.resources() == baseline {
                Ok(())
            } else {
                Err(format!(
                    "{id}: resources after drop {:?}, before open {baseline:?}",
                    self.resources()
                ))
            },
        );

        self.endpoint_lost(endpoint);
    }

    /// Requests pick the first listed rate, else the native one.
    fn format(&mut self, endpoint: &AudioEndpoint) {
        let id = &endpoint.id;
        let native = endpoint.format;
        let unlisted = [
            8_000, 11_025, 12_000, 22_050, 24_000, 32_000, 88_200, 176_400, 192_000,
        ]
        .into_iter()
        .filter_map(rbs_core::timing::SampleRate::new)
        .find(|r| !endpoint.sample_rates.contains(r));
        let alternative = endpoint
            .sample_rates
            .iter()
            .copied()
            .find(|r| *r != native.sample_rate)
            .unwrap_or(native.sample_rate);
        let mut cases = vec![(Vec::new(), native.sample_rate)];
        if let Some(unlisted) = unlisted {
            cases.push((vec![unlisted, alternative], alternative));
            cases.push((vec![unlisted], native.sample_rate));
        }
        let mut errors = Vec::new();
        for (rates, expected) in cases {
            let request = self.request(endpoint).with_sample_rates(&rates);
            match self.open(&request) {
                Ok(stream) => {
                    let format = stream.format();
                    if format.sample_rate != expected || format.channels != native.channels {
                        errors.push(format!(
                            "{id}: request {rates:?} gave {format:?}, expected {expected} Hz with {:?}",
                            native.channels
                        ));
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        self.report.check(
            "format",
            if errors.is_empty() {
                Ok(())
            } else {
                Err(errors.join("; "))
            },
        );
    }

    /// A removed endpoint makes `read` fail with `EndpointLost`, never silence.
    fn endpoint_lost(&mut self, endpoint: &AudioEndpoint) {
        let Some(hooks) = self.options.hooks else {
            self.skip("endpoint lost", "no hooks");
            return;
        };
        let request = self.request(endpoint);
        let mut stream = match self.open(&request) {
            Ok(stream) => stream,
            Err(e) => {
                self.report.check("endpoint lost", Err(e));
                return;
            }
        };
        if let Err(e) = stream.start() {
            self.report
                .check("endpoint lost", Err(format!("{}: start: {e}", endpoint.id)));
            return;
        }
        let channels = stream.format().channels.channels();
        let mut buffers = Buffers::new(channels, READ_FRAMES);
        self.pacer.wait(self.options.poll_interval);
        let _ = buffers.read(stream.as_mut());
        if !hooks.remove(&endpoint.id) {
            self.skip(
                "endpoint lost",
                format!("{}: cannot be removed", endpoint.id),
            );
            return;
        }
        let clock = self.pacer.clock();
        let deadline = clock.now() + self.options.audio_timeout;
        let lost = loop {
            if clock.now() > deadline {
                break Err(format!(
                    "{}: no EndpointLost within {:?}",
                    endpoint.id, self.options.audio_timeout
                ));
            }
            self.pacer.wait(self.options.poll_interval);
            match buffers.read(stream.as_mut()) {
                Err(AudioIoError::EndpointLost) => break Ok(()),
                Err(e) => break Err(format!("{}: expected EndpointLost, got {e}", endpoint.id)),
                Ok(_) => {}
            }
        };
        self.report.check("endpoint lost", lost);
    }
}
