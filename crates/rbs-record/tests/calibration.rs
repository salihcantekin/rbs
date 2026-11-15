//! ADR-0087 step D1: the calibration run's encoder measurement and folder write, on a
//! virtual clock with inline lanes and scripted fake encoders (ADR-0032: no sleeping).

#![allow(clippy::unwrap_used)] // Tests fail by panicking.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_codec::{
    CreateError, EncodeError, EncoderChange, RateControl, VideoEncoder, VideoEncoderConfig,
    VideoEncoderFactory,
};
use rbs_core::capability::CapabilitySet;
use rbs_core::media::{
    BitstreamFormat, Bytes, CodecId, ColorSpace, EncodedPacket, MemoryKind, PixelFormat, Size,
    StreamDescriptor, StreamParams, SystemFrame, VideoFormat, VideoFrame,
};
use rbs_core::registry::{AdapterFactory, AdapterId, ProbeError};
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_record::calibration::{
    CalibrationEvent, CalibrationOutcome, CalibrationPlan, Candidate, ClassKey, ClassPlan,
    FolderResult, Lanes, LanesFailure, Pacer, RoundRequest, SESSION_CEILING, SessionReport,
    SessionRunner, SkipCode, WriteSink, measure_write_speed, run_encoders, temp_name,
};
use rbs_record::estimate::{FpsClass, RateClass, SizeClass, StopReason};

// ---------------------------------------------------------------------------------
// A clock the inline lanes may set back (each lane starts the round at its start).

#[derive(Clone, Default)]
struct VClock(Arc<AtomicU64>);

impl VClock {
    fn set(&self, t: Timestamp) {
        self.0.store(t.as_nanos(), Ordering::Relaxed);
    }
    fn advance(&self, d: Duration) {
        self.0
            .fetch_add(u64::try_from(d.as_nanos()).unwrap(), Ordering::Relaxed);
    }
}

impl Clock for VClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(self.0.load(Ordering::Relaxed))
    }
}

struct TestPacer(VClock);

impl Pacer for TestPacer {
    fn yield_now(&self) {
        self.0.advance(Duration::from_micros(50));
    }
    fn wait_until(&self, t: Timestamp) {
        if self.0.now() < t {
            self.0.set(t);
        }
    }
}

// ---------------------------------------------------------------------------------
// Inline lanes: lanes run one after another, each from the round's start time; the
// round ends at the slowest lane's end, as three threads would.

struct InlineLanes {
    clock: VClock,
    pacer: TestPacer,
    stop: Arc<AtomicBool>,
    lanes: [Vec<SessionRunner>; 3],
    order: Vec<(usize, usize)>,
}

impl InlineLanes {
    fn new(clock: &VClock, stop: &Arc<AtomicBool>) -> Self {
        Self {
            clock: clock.clone(),
            pacer: TestPacer(clock.clone()),
            stop: Arc::clone(stop),
            lanes: [Vec::new(), Vec::new(), Vec::new()],
            order: Vec::new(),
        }
    }
}

impl Lanes for InlineLanes {
    fn open(
        &mut self,
        lane: usize,
        factory: &Arc<dyn VideoEncoderFactory>,
        config: &VideoEncoderConfig,
        ring: &Arc<[SystemFrame]>,
    ) -> Result<Result<(), CreateError>, LanesFailure> {
        match factory.create(config) {
            Ok(e) => {
                let period = config.frame_rate.frame_duration();
                self.lanes[lane].push(SessionRunner::new(e, Arc::clone(ring), period));
                self.order.push((lane, self.lanes[lane].len() - 1));
                Ok(Ok(()))
            }
            Err(e) => Ok(Err(e)),
        }
    }

    fn round(
        &mut self,
        request: RoundRequest,
        out: &mut Vec<SessionReport>,
    ) -> Result<(), LanesFailure> {
        let t0 = self.clock.now();
        let mut end = t0;
        let mut reports: [Vec<SessionReport>; 3] = Default::default();
        for (lane, runners) in self.lanes.iter_mut().enumerate() {
            self.clock.set(t0);
            for r in runners.iter_mut() {
                reports[lane].push(r.encode(
                    &self.clock,
                    &self.pacer,
                    request.deadline,
                    &self.stop,
                ));
            }
            end = end.max(self.clock.now());
        }
        self.clock.set(end);
        out.clear();
        for (lane, i) in &self.order {
            out.push(reports[*lane][*i].clone());
        }
        Ok(())
    }

    fn close(&mut self) -> Result<Vec<Result<bool, EncodeError>>, LanesFailure> {
        let mut out = Vec::new();
        for (lane, i) in self.order.drain(..) {
            out.push(self.lanes[lane][i].flush());
        }
        for l in &mut self.lanes {
            l.clear();
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------
// A scripted encoder: CPU time per call on the caller's lane, a shared device that
// works frames one after another, a ring of in-flight frames, a session limit.

#[derive(Clone)]
struct Script {
    /// CPU time of one submit.
    call: Duration,
    /// Device time of one frame (shared by all sessions of the factory).
    device: Duration,
    /// In-flight frames per session.
    ring: usize,
    /// `drain` waits for room on a full ring (`rbs-codec-hw`); else `submit` answers
    /// `Busy` (VideoToolbox).
    blocking_drain: bool,
    /// Open refused from this session on.
    limit: Option<(u32, CreateError)>,
    /// Lose the frame with this index of session 1.
    lose: Option<u64>,
    /// Fail submits from this session's frame index on.
    fail_at: Option<(u32, u64, EncodeError)>,
    /// Session 1's submit of this frame index takes this long instead.
    hiccup: Option<(u64, Duration)>,
    /// Set this flag when session `n` opens.
    stop_at_open: Option<(u64, Arc<AtomicBool>)>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            call: Duration::from_micros(100),
            device: Duration::ZERO,
            ring: 4,
            blocking_drain: true,
            limit: None,
            lose: None,
            fail_at: None,
            hiccup: None,
            stop_at_open: None,
        }
    }
}

struct Fake {
    id: &'static str,
    script: Script,
    clock: VClock,
    device_free: Arc<Mutex<Timestamp>>,
    opened: Arc<AtomicU64>,
}

impl Fake {
    fn new(id: &'static str, script: Script, clock: &VClock) -> Arc<Self> {
        Arc::new(Self {
            id,
            script,
            clock: clock.clone(),
            device_free: Arc::new(Mutex::new(Timestamp::ZERO)),
            opened: Arc::new(AtomicU64::new(0)),
        })
    }
}

impl AdapterFactory for Fake {
    fn id(&self) -> AdapterId {
        AdapterId::new(self.id)
    }
    fn display_name(&self) -> &str {
        self.id
    }
    fn probe(&self) -> Result<CapabilitySet, ProbeError> {
        Ok(CapabilitySet::new())
    }
}

impl VideoEncoderFactory for Fake {
    fn create(&self, config: &VideoEncoderConfig) -> Result<Box<dyn VideoEncoder>, CreateError> {
        let n = self.opened.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some((at, stop)) = &self.script.stop_at_open
            && n == *at
        {
            stop.store(true, Ordering::Relaxed);
        }
        if let Some((from, e)) = &self.script.limit
            && n >= u64::from(*from)
        {
            return Err(e.clone());
        }
        Ok(Box::new(FakeSession {
            number: u32::try_from(n).unwrap(),
            script: self.script.clone(),
            clock: self.clock.clone(),
            device_free: Arc::clone(&self.device_free),
            stream: StreamDescriptor::new(
                CodecId::H264,
                BitstreamFormat::AnnexB,
                Bytes::new(),
                StreamParams::video(config.format, config.frame_rate),
            ),
            in_flight: VecDeque::new(),
            last_pts: None,
            submitted: 0,
        }))
    }
}

struct FakeSession {
    number: u32,
    script: Script,
    clock: VClock,
    device_free: Arc<Mutex<Timestamp>>,
    stream: StreamDescriptor,
    /// (pts, done at)
    in_flight: VecDeque<(Timestamp, Timestamp)>,
    last_pts: Option<Timestamp>,
    submitted: u64,
}

impl FakeSession {
    fn packet(pts: Timestamp) -> EncodedPacket {
        EncodedPacket::new(
            CodecId::H264,
            BitstreamFormat::AnnexB,
            pts,
            pts,
            true,
            Bytes::new(),
        )
        .unwrap()
    }
    fn emit(&mut self, out: &mut Vec<EncodedPacket>, now: Timestamp) {
        while let Some(&(pts, done)) = self.in_flight.front() {
            if done > now {
                break;
            }
            self.in_flight.pop_front();
            let lost = self.number == 1
                && self.script.lose.is_some_and(|i| {
                    pts.as_nanos() / FrameRate::FPS_60.frame_duration().as_nanos() as u64 == i
                });
            if !lost {
                out.push(Self::packet(pts));
            }
        }
    }
}

impl VideoEncoder for FakeSession {
    fn stream(&self) -> &StreamDescriptor {
        &self.stream
    }
    fn submit(&mut self, frame: &VideoFrame) -> Result<(), EncodeError> {
        if let Some((n, at, e)) = &self.script.fail_at
            && *n == self.number
            && self.submitted >= *at
        {
            return Err(e.clone());
        }
        if self.last_pts.is_some_and(|p| frame.timestamp() <= p) {
            return Err(EncodeError::InvalidInput("pts not after the last".into()));
        }
        if !self.script.blocking_drain && self.in_flight.len() >= self.script.ring {
            return Err(EncodeError::Busy);
        }
        let call = match self.script.hiccup {
            Some((i, d)) if self.number == 1 && self.submitted == i => d,
            _ => self.script.call,
        };
        self.clock.advance(call);
        let now = self.clock.now();
        let done = {
            let mut free = self.device_free.lock().unwrap();
            let done = (*free).max(now) + self.script.device;
            *free = done;
            done
        };
        self.in_flight.push_back((frame.timestamp(), done));
        self.last_pts = Some(frame.timestamp());
        self.submitted += 1;
        Ok(())
    }
    fn drain(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        if self.script.blocking_drain && self.in_flight.len() >= self.script.ring {
            let (_, done) = self.in_flight[0];
            if done > self.clock.now() {
                self.clock.set(done);
            }
        }
        let now = self.clock.now();
        self.emit(out, now);
        Ok(())
    }
    fn reconfigure(&mut self, _: &EncoderChange) -> Result<(), EncodeError> {
        Err(EncodeError::Unsupported)
    }
    fn flush(&mut self, out: &mut Vec<EncodedPacket>) -> Result<(), EncodeError> {
        if let Some(&(_, done)) = self.in_flight.back()
            && done > self.clock.now()
        {
            self.clock.set(done);
        }
        let now = self.clock.now();
        self.emit(out, now);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------

fn config() -> VideoEncoderConfig {
    VideoEncoderConfig::new(
        CodecId::H264,
        VideoFormat::new(
            Size::new(64, 36),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        ),
        FrameRate::FPS_60,
        MemoryKind::System,
        RateControl::Cbr {
            bitrate_bps: 1_000_000,
        },
    )
}

fn key() -> ClassKey {
    ClassKey {
        codec: "h264".to_owned(),
        size: SizeClass::L720,
        fps: FpsClass::F60,
        rate: RateClass::Cbr,
    }
}

fn candidate(f: &Arc<Fake>, likely: bool) -> Candidate {
    Candidate {
        adapter: AdapterId::new(f.id),
        version: None,
        hardware: true,
        likely,
        factory: Arc::clone(f) as Arc<dyn VideoEncoderFactory>,
    }
}

fn plan(candidates: Vec<Candidate>) -> CalibrationPlan {
    CalibrationPlan {
        classes: vec![ClassPlan {
            key: key(),
            config: config(),
            candidates,
        }],
        folder: PathBuf::from("/rec"),
    }
}

struct Run {
    events: Vec<CalibrationEvent>,
    outcome: CalibrationOutcome,
}

fn run_with(plan: &CalibrationPlan, clock: &VClock, stop: &Arc<AtomicBool>) -> Run {
    let mut lanes = InlineLanes::new(clock, stop);
    let pacer = TestPacer(clock.clone());
    let mut events = Vec::new();
    let start = clock.now();
    let outcome = run_encoders(plan, &mut lanes, clock, &pacer, stop, start, &mut |e| {
        events.push(e)
    });
    Run { events, outcome }
}

fn run(fakes: &[(&Arc<Fake>, bool)], clock: &VClock) -> Run {
    let p = plan(fakes.iter().map(|(f, l)| candidate(f, *l)).collect());
    run_with(&p, clock, &Arc::new(AtomicBool::new(false)))
}

fn results(r: &Run) -> Vec<(String, u32, StopReason)> {
    r.events
        .iter()
        .filter_map(|e| match e {
            CalibrationEvent::Series(s) => {
                Some((s.key.adapter.as_str().to_owned(), s.sessions, s.stopped_by))
            }
            _ => None,
        })
        .collect()
}

fn skips(r: &Run) -> Vec<SkipCode> {
    r.events
        .iter()
        .filter_map(|e| match e {
            CalibrationEvent::Skipped(s) => Some(s.code),
            _ => None,
        })
        .collect()
}

const P: Duration = Duration::from_nanos(16_666_666);

/// A device that does `k` frames per period (shared by every session).
fn device_for(k: f64) -> Duration {
    P.div_f64(k)
}

// ---------------------------------------------------------------------------------

#[test]
fn the_count_is_the_device_capacity_on_a_blocking_drain() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            device: device_for(4.5),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(r.outcome, CalibrationOutcome::Done { cancelled: false });
    assert_eq!(results(&r), vec![("hw".to_owned(), 4, StopReason::Late)]);
}

#[test]
fn a_drain_that_never_blocks_and_a_short_ring_pass_at_the_capacity_not_zero() {
    let clock = VClock::default();
    let f = Fake::new(
        "vt",
        Script {
            device: device_for(4.5),
            ring: 16,
            blocking_drain: false,
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("vt".to_owned(), 4, StopReason::Late)]);
}

#[test]
fn the_take_layout_runs_files_on_two_threads_beside_the_master() {
    // Software: 7 ms of CPU per frame and no device. Serial would keep 2; the master
    // on its own lane and two lanes of files keep 1 + 2 × 2 = 5.
    let clock = VClock::default();
    let f = Fake::new(
        "sw",
        Script {
            call: Duration::from_millis(7),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("sw".to_owned(), 5, StopReason::Late)]);
}

#[test]
fn a_one_percent_shortfall_fails_the_sustained_rate() {
    // One session on a device 1 % too slow: the jitter depth alone would pass it.
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            device: P.mul_f64(1.01),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("hw".to_owned(), 0, StopReason::Late)]);
}

#[test]
fn a_refused_open_is_the_cap() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            limit: Some((4, CreateError::ResourceExhausted)),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("hw".to_owned(), 3, StopReason::Refused)]);
}

#[test]
fn a_busy_first_open_writes_no_entry() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            limit: Some((1, CreateError::ResourceExhausted)),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert!(results(&r).is_empty());
    assert_eq!(skips(&r), vec![SkipCode::Busy]);
}

#[test]
fn an_unsupported_class_is_skipped() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            limit: Some((
                1,
                CreateError::Unsupported {
                    what: "size".into(),
                },
            )),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert!(results(&r).is_empty());
    assert_eq!(skips(&r), vec![SkipCode::Unsupported]);
}

#[test]
fn a_device_error_after_step_one_keeps_a_lower_bound() {
    let clock = VClock::default();
    let f = Fake::new(
        "vt",
        Script {
            limit: Some((4, CreateError::Device("gone".into()))),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("vt".to_owned(), 3, StopReason::Error)]);
    assert_eq!(skips(&r), vec![SkipCode::OpenFailed]);
}

#[test]
fn a_failed_encode_at_step_one_writes_no_entry() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            fail_at: Some((
                1,
                3,
                EncodeError::Failed {
                    reason: "x".into(),
                    recoverable: false,
                },
            )),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert!(results(&r).is_empty());
    assert_eq!(skips(&r), vec![SkipCode::EncodeFailed]);
}

#[test]
fn a_lost_device_fails_the_whole_run() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            fail_at: Some((2, 3, EncodeError::DeviceLost)),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(r.outcome, CalibrationOutcome::DeviceLost);
}

#[test]
fn a_lost_frame_fails_the_step() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            lose: Some(30),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("hw".to_owned(), 0, StopReason::Late)]);
}

#[test]
fn a_fast_encoder_reaches_the_ceiling() {
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            call: Duration::from_micros(10),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(
        results(&r),
        vec![("hw".to_owned(), SESSION_CEILING, StopReason::Ceiling)]
    );
}

#[test]
fn the_likely_adapter_is_measured_first() {
    let clock = VClock::default();
    let a = Fake::new("a", Script::default(), &clock);
    let b = Fake::new(
        "b",
        Script {
            limit: Some((3, CreateError::ResourceExhausted)),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&a, false), (&b, true)], &clock);
    let order: Vec<String> = results(&r).into_iter().map(|(a, _, _)| a).collect();
    assert_eq!(order, vec!["b".to_owned(), "a".to_owned()]);
}

#[test]
fn a_cancel_before_any_series_skips_them_all() {
    let clock = VClock::default();
    let f = Fake::new("hw", Script::default(), &clock);
    let p = plan(vec![candidate(&f, true)]);
    let stop = Arc::new(AtomicBool::new(true));
    let r = run_with(&p, &clock, &stop);
    assert_eq!(r.outcome, CalibrationOutcome::Done { cancelled: true });
    assert_eq!(skips(&r), vec![SkipCode::Cancelled]);
}

#[test]
fn the_run_keeps_within_its_encoder_time() {
    // Four adapters that reach the ceiling slowly; the run ends by 45 s on the clock.
    let clock = VClock::default();
    let script = Script {
        call: Duration::from_micros(500),
        ..Script::default()
    };
    let fakes: Vec<Arc<Fake>> = ["a", "b", "c", "d"]
        .into_iter()
        .map(|id| Fake::new(id, script.clone(), &clock))
        .collect();
    let refs: Vec<(&Arc<Fake>, bool)> =
        fakes.iter().enumerate().map(|(i, f)| (f, i == 0)).collect();
    let r = run(&refs, &clock);
    assert!(clock.now().saturating_since(Timestamp::ZERO) <= Duration::from_secs(47));
    assert!(!results(&r).is_empty());
}

// ---------------------------------------------------------------------------------
// The folder write.

#[derive(Default)]
struct MemSink {
    clock: VClock,
    per_chunk: Duration,
    files: Vec<(String, u64)>,
    removed: Vec<PathBuf>,
    written: u64,
    fail_write: bool,
    stop_after: Option<(u64, Arc<AtomicBool>)>,
}

impl WriteSink for MemSink {
    fn list(&mut self, _: &Path) -> std::io::Result<Vec<(String, u64)>> {
        Ok(self.files.clone())
    }
    fn remove(&mut self, path: &Path) -> std::io::Result<()> {
        self.removed.push(path.to_owned());
        Ok(())
    }
    fn create(&mut self, _: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        if self.fail_write {
            return Err(std::io::Error::other("full"));
        }
        self.clock.advance(self.per_chunk);
        self.written += chunk.len() as u64;
        if let Some((at, stop)) = &self.stop_after
            && self.written >= *at
        {
            stop.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.clock.advance(self.per_chunk);
        Ok(())
    }
}

const MIB: u64 = 1024 * 1024;

fn write(sink: &mut MemSink, stop: &AtomicBool, free: Option<u64>) -> FolderResult {
    let clock = sink.clock.clone();
    measure_write_speed(
        Path::new("/rec"),
        sink,
        &clock,
        stop,
        Timestamp::ZERO + Duration::from_secs(60),
        256 * MIB,
        free,
        10_000,
        7,
    )
}

#[test]
fn the_write_speed_counts_to_the_end_of_the_sync() {
    let mut sink = MemSink {
        per_chunk: Duration::from_millis(100),
        ..MemSink::default()
    };
    let r = write(&mut sink, &AtomicBool::new(false), None);
    // 32 chunks and the sync: 3.3 s for 256 MiB.
    let expected = (256 * MIB) as f64 / 3.3;
    match r {
        FolderResult::Speed {
            bytes_per_second,
            partial,
        } => {
            assert!(!partial);
            assert!(
                (bytes_per_second as f64 - expected).abs() < 1.0,
                "{bytes_per_second}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(sink.removed, vec![PathBuf::from("/rec").join(temp_name(7))]);
}

#[test]
fn only_old_leftovers_of_other_processes_are_removed() {
    let mut sink = MemSink {
        per_chunk: Duration::from_millis(1),
        files: vec![
            (temp_name(1), 10_000 - 601),
            (temp_name(2), 10_000 - 10),
            (temp_name(7), 0),
        ],
        ..MemSink::default()
    };
    write(&mut sink, &AtomicBool::new(false), None);
    assert_eq!(
        sink.removed,
        vec![
            PathBuf::from("/rec").join(temp_name(1)),
            PathBuf::from("/rec").join(temp_name(7)),
        ]
    );
}

#[test]
fn no_room_skips_the_write() {
    let mut sink = MemSink::default();
    let r = write(&mut sink, &AtomicBool::new(false), Some(400 * MIB));
    assert_eq!(r, FolderResult::Skipped(SkipCode::NoRoom));
}

#[test]
fn a_failing_write_has_no_entry_and_removes_its_file() {
    let mut sink = MemSink {
        fail_write: true,
        ..MemSink::default()
    };
    let r = write(&mut sink, &AtomicBool::new(false), None);
    assert_eq!(r, FolderResult::Skipped(SkipCode::WriteFailed));
    assert_eq!(sink.removed.len(), 1);
}

#[test]
fn a_zero_elapsed_write_has_no_entry() {
    let mut sink = MemSink::default();
    let r = write(&mut sink, &AtomicBool::new(false), None);
    assert_eq!(r, FolderResult::Skipped(SkipCode::WriteFailed));
}

#[test]
fn a_cancel_after_enough_bytes_records_a_synced_partial_rate() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut sink = MemSink {
        per_chunk: Duration::from_millis(100),
        stop_after: Some((40 * MIB, Arc::clone(&stop))),
        ..MemSink::default()
    };
    match write(&mut sink, &stop, None) {
        FolderResult::Speed { partial, .. } => assert!(partial),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_cancel_before_enough_bytes_has_no_entry() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut sink = MemSink {
        per_chunk: Duration::from_millis(100),
        stop_after: Some((8 * MIB, Arc::clone(&stop))),
        ..MemSink::default()
    };
    assert_eq!(
        write(&mut sink, &stop, None),
        FolderResult::Skipped(SkipCode::Cancelled)
    );
}

fn hiccup_run(extra: Duration) -> Run {
    // Session 1 alone: one judged round of step 1 (frame 40, after warm-up) takes
    // `extra`; the device is fast, so only the schedule decides.
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            hiccup: Some((40, extra)),
            limit: Some((2, CreateError::ResourceExhausted)),
            ..Script::default()
        },
        &clock,
    );
    run(&[(&f, true)], &clock)
}

#[test]
fn a_single_round_of_three_periods_passes_and_longer_fails() {
    // The round's slack starts at Q periods; the hiccup round is the whole round.
    let r = hiccup_run(P * 3 - Duration::from_micros(200));
    assert_eq!(results(&r), vec![("hw".to_owned(), 1, StopReason::Refused)]);
    let r = hiccup_run(P * 3 + Duration::from_micros(1));
    assert_eq!(results(&r), vec![("hw".to_owned(), 0, StopReason::Late)]);
}

#[test]
fn a_cancel_during_a_series_keeps_a_lower_bound() {
    let clock = VClock::default();
    let stop = Arc::new(AtomicBool::new(false));
    let f = Fake::new(
        "hw",
        Script {
            stop_at_open: Some((4, Arc::clone(&stop))),
            ..Script::default()
        },
        &clock,
    );
    let p = plan(vec![candidate(&f, true)]);
    let r = run_with(&p, &clock, &stop);
    assert_eq!(r.outcome, CalibrationOutcome::Done { cancelled: true });
    assert_eq!(
        results(&r),
        vec![("hw".to_owned(), 3, StopReason::Cancelled)]
    );
}

#[test]
fn timestamps_increase_across_steps() {
    // The fake refuses a pts that is not after the last: a full series passes.
    let clock = VClock::default();
    let f = Fake::new(
        "hw",
        Script {
            limit: Some((6, CreateError::ResourceExhausted)),
            ..Script::default()
        },
        &clock,
    );
    let r = run(&[(&f, true)], &clock);
    assert_eq!(results(&r), vec![("hw".to_owned(), 5, StopReason::Refused)]);
}

/// The ring is a moving picture (the owner's choice of 2026-11-15): every frame
/// differs from the others, and it is not noise (neighbouring pixels are close).
#[test]
fn the_ring_is_a_moving_picture_with_different_frames() {
    let ring = rbs_record::calibration::moving_ring(config().format, 8).unwrap();
    assert_eq!(ring.len(), 8);
    for i in 0..ring.len() {
        for j in i + 1..ring.len() {
            assert_ne!(ring[i].data(), ring[j].data(), "frames {i} and {j}");
        }
    }
    let y = &ring[0].data()[..1920];
    let steps = y.windows(2).filter(|w| w[0].abs_diff(w[1]) > 8).count();
    assert!(steps < 64, "{steps} jumps in one row");
}
