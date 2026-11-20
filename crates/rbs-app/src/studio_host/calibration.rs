//! *Test this computer* on the control thread (ADR-0087 step D1, spec
//! `docs/specs/output-estimate.md` "Calibration run", control API 1.55).
//!
//! `calibration.run` builds the plan `record.estimate` would build, snapshots the
//! candidate encoders and starts one worker thread for the run, plus three lane
//! threads (the take's layout) while the encoder part runs. The worker reports over
//! the control thread's own queue, so the host never polls; while a run is active the
//! host checks its deadline in every round. With no run there is no thread and no
//! check (P13). Output starts are refused while a run is active, and a run is refused
//! while any output runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use rbs_api::event::{EventPayload, StateKey, Topic};
use rbs_api::types as api;
use rbs_codec::{CreateError, EncodeError, VideoEncoderConfig, VideoEncoderFactory};
use rbs_core::media::SystemFrame;
use rbs_core::time::{Clock, MonotonicClock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_gpu::GpuContext;
use rbs_record::calibration::{
    CALIBRATION_LIMIT, CALIBRATION_WRITE_BYTES, CalibrationEvent, CalibrationOutcome,
    CalibrationPlan, EncoderResult, ExportResult, ExportRig, ExportSkip, ExportSkipCode,
    ExportsEnd, FileSink, FolderResult, GpuExportRig, Lanes, LanesFailure, Pacer, RigError,
    RoundRequest, SessionReport, SessionRunner, SkipCode, Skipped, WriteSink, measure_write_speed,
    run_encoders, run_exports,
};
use rbs_record::estimate::StopReason;

use super::{Host, Msg, OutputCanvas, slot};
use crate::calibration_file::{EncoderEntry, ExportEntry, FolderEntry, stop_reason_name};

/// Makes the export part's rig from the live GPU context (step D2).
pub type ExportRigMaker =
    Arc<dyn Fn(&Arc<GpuContext>, FrameRate) -> Result<Box<dyn ExportRig>, RigError> + Send + Sync>;

/// After a lost throwaway device, how long the live one is watched before it is taken
/// as intact: several live render ticks.
const LIVE_LOSS_WATCH: Duration = Duration::from_millis(100);

/// The refusal of a start while a run is active (glossary: *Test this computer*).
pub(super) const RUNNING: &str = "Test this computer is running";

/// Past the run's limit, the host waits this long for the worker before it ends the
/// run itself; the same after a cancel.
const GRACE: Duration = Duration::from_secs(5);

/// The run's injected parts: the monotonic clock, real waits and the file system in
/// the app; a test clock, a pacer that advances it and a fake sink in tests.
#[derive(Clone)]
pub struct CalibrationSetup {
    /// The run's clock (the host's deadline uses it too).
    pub clock: Arc<dyn Clock>,
    /// Waits between rounds and `Busy` retries.
    pub pacer: Arc<dyn Pacer>,
    /// Makes the write test's file system.
    pub sink: Arc<dyn Fn() -> Box<dyn WriteSink + Send> + Send + Sync>,
    /// Bytes the write test writes.
    pub write_bytes: u64,
    /// The wall time in seconds since the Unix epoch.
    pub now_unix: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Makes the export part's rig: in the app a throwaway device on the live GPU's
    /// adapter ([`GpuContext::sibling`]); a fake in tests.
    pub export_rig: ExportRigMaker,
}

impl std::fmt::Debug for CalibrationSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalibrationSetup")
            .field("write_bytes", &self.write_bytes)
            .finish_non_exhaustive()
    }
}

/// Real waits on the monotonic clock.
struct RealPacer {
    clock: Arc<dyn Clock>,
}

impl Pacer for RealPacer {
    fn yield_now(&self) {
        std::thread::yield_now();
    }

    fn wait_until(&self, t: Timestamp) {
        let left = t.saturating_since(self.clock.now());
        if !left.is_zero() {
            // A calibration worker, not a real-time or UI thread: pacing one session
            // for `frame_time` is a real wait.
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(left);
        }
    }
}

impl CalibrationSetup {
    /// The app's parts.
    pub fn real() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
        Self {
            pacer: Arc::new(RealPacer {
                clock: Arc::clone(&clock),
            }),
            clock,
            sink: Arc::new(|| Box::new(FileSink::default())),
            write_bytes: CALIBRATION_WRITE_BYTES,
            now_unix: Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
            }),
            export_rig: Arc::new(|live: &Arc<GpuContext>, rate: FrameRate| {
                let sibling = live.sibling().map_err(|_| RigError::Failed)?;
                Ok(Box::new(GpuExportRig::new(Arc::new(sibling), rate)) as Box<dyn ExportRig>)
            }),
        }
    }
}

/// What the worker tells the control thread.
pub(super) enum WorkerMsg {
    Event(CalibrationEvent),
    SessionsClosed,
    Folder(FolderResult),
    Done { cancelled: bool },
    Failed(api::CalibrationCode, &'static str),
}

/// The active run.
struct ActiveRun {
    id: u64,
    stop: Arc<AtomicBool>,
    deadline: Timestamp,
    cancel_at: Option<Timestamp>,
    folder: PathBuf,
    step: u32,
    steps: u32,
    part: api::CalibrationPart,
    adapter: Option<String>,
    encoders: Vec<EncoderResult>,
    skipped: Vec<Skipped>,
    write: Option<FolderResult>,
    /// Steps the export part used, before the encoder steps.
    export_steps: u32,
    exports: Vec<ExportResult>,
    exports_skipped: Vec<ExportSkip>,
}

/// The host's calibration state.
pub(super) struct CalibrationHost {
    setup: CalibrationSetup,
    run: Option<ActiveRun>,
    state: api::CalibrationState,
    next_id: u64,
    /// The last run could not measure exports on this computer (no throwaway device,
    /// or the export failed); kept for the app run (`calibrate`, step D2).
    pub(super) exports_unmeasurable: bool,
}

impl CalibrationHost {
    pub(super) fn new(setup: Option<CalibrationSetup>) -> Self {
        Self {
            setup: setup.unwrap_or_else(CalibrationSetup::real),
            run: None,
            state: api::CalibrationState::Idle,
            next_id: 1,
            exports_unmeasurable: false,
        }
    }

    /// Why a start is refused now, if it is.
    pub(super) fn blocks(&self) -> Option<&'static str> {
        self.run.is_some().then_some(RUNNING)
    }

    /// Sets the stop flag of an active run (at app shutdown).
    pub(super) fn stop(&self) {
        if let Some(r) = &self.run {
            r.stop.store(true, Ordering::Relaxed);
        }
    }
}

// ---------------------------------------------------------------------------------
// The lanes on three threads, talking over channels: one request and one answer per
// lane per round. A lane whose thread panicked or ended answers with a disconnect,
// which ends the run `failed`, code `panicked`.

enum LaneReq {
    Open(
        Arc<dyn VideoEncoderFactory>,
        Box<VideoEncoderConfig>,
        Arc<[SystemFrame]>,
    ),
    Round(RoundRequest, Vec<SessionReport>),
    Close,
}

enum LaneAns {
    Opened(Result<(), CreateError>),
    Round(Vec<SessionReport>),
    Closed(Vec<Result<bool, EncodeError>>),
}

struct Lane {
    tx: Sender<LaneReq>,
    rx: Receiver<LaneAns>,
    buf: Vec<SessionReport>,
    sessions: usize,
}

struct ThreadLanes {
    lanes: Vec<Lane>,
    order: Vec<(usize, usize)>,
}

fn lane_body(
    rx: Receiver<LaneReq>,
    tx: Sender<LaneAns>,
    clock: Arc<dyn Clock>,
    pacer: Arc<dyn Pacer>,
    stop: Arc<AtomicBool>,
) {
    let mut runners: Vec<SessionRunner> = Vec::new();
    while let Ok(req) = rx.recv() {
        let ans = match req {
            LaneReq::Open(factory, config, ring) => {
                LaneAns::Opened(factory.create(&config).map(|e| {
                    let period = config.frame_rate.frame_duration();
                    runners.push(SessionRunner::new(e, ring, period));
                }))
            }
            LaneReq::Round(request, mut out) => {
                out.clear();
                for r in &mut runners {
                    out.push(r.encode(&*clock, &*pacer, request.deadline, &stop));
                }
                LaneAns::Round(out)
            }
            LaneReq::Close => {
                let closed = runners.iter_mut().map(SessionRunner::flush).collect();
                runners.clear();
                LaneAns::Closed(closed)
            }
        };
        if tx.send(ans).is_err() {
            return;
        }
    }
}

impl ThreadLanes {
    fn new(
        clock: &Arc<dyn Clock>,
        pacer: &Arc<dyn Pacer>,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self, std::io::Error> {
        let mut lanes = Vec::new();
        for i in 0..3 {
            let (req_tx, req_rx) = mpsc::channel();
            let (ans_tx, ans_rx) = mpsc::channel();
            let (clock, pacer, stop) = (Arc::clone(clock), Arc::clone(pacer), Arc::clone(stop));
            std::thread::Builder::new()
                .name(format!("rbs-calibration-lane-{i}"))
                .spawn(move || {
                    // Not a real-time thread: its panics get crash reports (ADR-0055).
                    rbs_crash::report_panics_on_this_thread();
                    // A panic drops `ans_tx`: the worker sees a disconnect.
                    let _ = rbs_crash::contain(|| lane_body(req_rx, ans_tx, clock, pacer, stop));
                })?;
            lanes.push(Lane {
                tx: req_tx,
                rx: ans_rx,
                buf: Vec::with_capacity(16),
                sessions: 0,
            });
        }
        Ok(Self {
            lanes,
            order: Vec::new(),
        })
    }
}

impl Lanes for ThreadLanes {
    fn open(
        &mut self,
        lane: usize,
        factory: &Arc<dyn VideoEncoderFactory>,
        config: &VideoEncoderConfig,
        ring: &Arc<[SystemFrame]>,
    ) -> Result<Result<(), CreateError>, LanesFailure> {
        crate::ui_thread::assert_off_ui("a calibration lane");
        let l = self.lanes.get_mut(lane).ok_or(LanesFailure)?;
        l.tx.send(LaneReq::Open(
            Arc::clone(factory),
            Box::new(config.clone()),
            Arc::clone(ring),
        ))
        .map_err(|_| LanesFailure)?;
        match l.rx.recv() {
            Ok(LaneAns::Opened(Ok(()))) => {
                self.order.push((lane, l.sessions));
                l.sessions += 1;
                Ok(Ok(()))
            }
            Ok(LaneAns::Opened(Err(e))) => Ok(Err(e)),
            _ => Err(LanesFailure),
        }
    }

    fn round(
        &mut self,
        request: RoundRequest,
        out: &mut Vec<SessionReport>,
    ) -> Result<(), LanesFailure> {
        crate::ui_thread::assert_off_ui("a calibration lane");
        for l in &mut self.lanes {
            let buf = std::mem::take(&mut l.buf);
            l.tx.send(LaneReq::Round(request, buf))
                .map_err(|_| LanesFailure)?;
        }
        for l in &mut self.lanes {
            match l.rx.recv() {
                Ok(LaneAns::Round(b)) => l.buf = b,
                _ => return Err(LanesFailure),
            }
        }
        out.clear();
        for (lane, i) in &self.order {
            let r = self.lanes[*lane].buf.get(*i).ok_or(LanesFailure)?;
            out.push(r.clone());
        }
        Ok(())
    }

    fn close(&mut self) -> Result<Vec<Result<bool, EncodeError>>, LanesFailure> {
        crate::ui_thread::assert_off_ui("a calibration lane");
        let mut out = Vec::new();
        for l in &mut self.lanes {
            l.tx.send(LaneReq::Close).map_err(|_| LanesFailure)?;
        }
        for l in &mut self.lanes {
            match l.rx.recv() {
                Ok(LaneAns::Closed(c)) => out.extend(c),
                _ => return Err(LanesFailure),
            }
            l.sessions = 0;
        }
        self.order.clear();
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------
// The worker.

struct WorkerInput {
    id: u64,
    plan: CalibrationPlan,
    setup: CalibrationSetup,
    stop: Arc<AtomicBool>,
    start: Timestamp,
    free_space: Option<u64>,
    inbox: std::sync::Weak<crate::studio_host::ControlOut>,
    live: Arc<GpuContext>,
}

fn send(inbox: &std::sync::Weak<crate::studio_host::ControlOut>, id: u64, msg: WorkerMsg) {
    if let Some(tx) = inbox.upgrade() {
        let _ = tx.send(Msg::Calibration { id, msg });
    }
}

/// Drops a rig on a thread of its own that may never end: dropping a device whose
/// queue still has work waits for it without a bound (a hung driver), so the worker
/// never does it itself.
fn reap(rig: Box<dyn ExportRig>) {
    let spawned = std::thread::Builder::new()
        .name("rbs-calibration-reaper".into())
        .spawn(move || {
            rbs_crash::report_panics_on_this_thread();
            let _ = rbs_crash::contain(move || drop(rig));
        });
    // No thread: leaking the device is better than blocking the worker on it.
    if let Err(e) = spawned {
        drop(e);
    }
}

/// The export part (step D2): on a throwaway device, before the encoders.
fn export_part(w: &WorkerInput) -> Result<(), ()> {
    let send = |m| send(&w.inbox, w.id, m);
    if w.plan.exports.is_empty() {
        return Ok(());
    }
    let period = w.plan.frame_rate.frame_duration();
    let maker = Arc::clone(&w.setup.export_rig);
    let live = Arc::clone(&w.live);
    let rate = w.plan.frame_rate;
    let mut open = || maker(&live, rate);
    let clock = Arc::clone(&w.setup.clock);
    let pacer = Arc::clone(&w.setup.pacer);
    let mut live_lost = || {
        if w.live.metrics().device_lost() {
            return true;
        }
        // Let the live render loop run a few ticks, then look again.
        pacer.wait_until(clock.now() + LIVE_LOSS_WATCH);
        w.live.metrics().device_lost()
    };
    let (rig, end) = run_exports(
        &w.plan.exports,
        period,
        &mut open,
        &mut live_lost,
        &*w.setup.clock,
        &*w.setup.pacer,
        &w.stop,
        w.start,
        &mut |e| send(WorkerMsg::Event(e)),
    );
    match (rig, end) {
        (Some(rig), ExportsEnd::Clean) => drop(rig),
        (Some(rig), _) => reap(rig),
        (None, _) => {}
    }
    if end == ExportsEnd::LiveLost {
        send(WorkerMsg::Failed(
            api::CalibrationCode::DeviceLost,
            "the graphics device was lost",
        ));
        return Err(());
    }
    Ok(())
}

fn worker(w: &WorkerInput) {
    let send = |m| send(&w.inbox, w.id, m);
    if export_part(w).is_err() {
        return;
    }
    if !w.plan.classes.is_empty() {
        let mut lanes = match ThreadLanes::new(&w.setup.clock, &w.setup.pacer, &w.stop) {
            Ok(l) => l,
            Err(_) => {
                send(WorkerMsg::Failed(
                    api::CalibrationCode::Ended,
                    "the test threads could not start",
                ));
                return;
            }
        };
        let outcome = run_encoders(
            &w.plan,
            &mut lanes,
            &*w.setup.clock,
            &*w.setup.pacer,
            &w.stop,
            w.start,
            &mut |e| send(WorkerMsg::Event(e)),
        );
        // Dropping the lanes ends their threads once they finish what they hold.
        drop(lanes);
        match outcome {
            CalibrationOutcome::Done { .. } => {}
            CalibrationOutcome::DeviceLost => {
                send(WorkerMsg::Failed(
                    api::CalibrationCode::DeviceLost,
                    "an encoder device was lost",
                ));
                return;
            }
            CalibrationOutcome::Panicked => {
                send(WorkerMsg::Failed(
                    api::CalibrationCode::Panicked,
                    "the test stopped on an error",
                ));
                return;
            }
        }
    }
    send(WorkerMsg::SessionsClosed);
    let mut sink = (w.setup.sink)();
    let folder = measure_write_speed(
        &w.plan.folder,
        &mut *sink,
        &*w.setup.clock,
        &w.stop,
        w.start + CALIBRATION_LIMIT,
        w.setup.write_bytes,
        w.free_space,
        (w.setup.now_unix)(),
        std::process::id(),
    );
    send(WorkerMsg::Folder(folder));
    send(WorkerMsg::Done {
        cancelled: w.stop.load(Ordering::Relaxed),
    });
}

// ---------------------------------------------------------------------------------
// The host side.

fn api_class(k: &rbs_record::calibration::ClassKey) -> api::CalibrationClass {
    api::CalibrationClass {
        codec: k.codec.clone(),
        size: k.size.as_str().to_owned(),
        fps: k.fps.as_str().to_owned(),
        rate: k.rate.as_str().to_owned(),
    }
}

fn api_code(c: SkipCode) -> api::CalibrationCode {
    match c {
        SkipCode::Unsupported => api::CalibrationCode::Unsupported,
        SkipCode::Busy => api::CalibrationCode::Busy,
        SkipCode::OpenFailed => api::CalibrationCode::OpenFailed,
        SkipCode::EncodeFailed => api::CalibrationCode::EncodeFailed,
        SkipCode::NoRoom => api::CalibrationCode::NoRoom,
        SkipCode::WriteFailed => api::CalibrationCode::WriteFailed,
        SkipCode::TimeLimit => api::CalibrationCode::TimeLimit,
        SkipCode::Cancelled => api::CalibrationCode::Cancelled,
    }
}

/// Whole microseconds, rounded up.
fn micros_up(d: Duration) -> u64 {
    u64::try_from(d.as_nanos().div_ceil(1000)).unwrap_or(u64::MAX)
}

fn api_format(f: rbs_record::estimate::ExportFormat) -> api::CalibrationExportFormat {
    match f {
        rbs_record::estimate::ExportFormat::Nv12 => api::CalibrationExportFormat::Nv12,
        rbs_record::estimate::ExportFormat::P010 => api::CalibrationExportFormat::P010,
    }
}

fn api_export_code(c: ExportSkipCode) -> api::CalibrationExportCode {
    match c {
        ExportSkipCode::GpuUnavailable => api::CalibrationExportCode::GpuUnavailable,
        ExportSkipCode::ExportFailed => api::CalibrationExportCode::ExportFailed,
        ExportSkipCode::DeviceLost => api::CalibrationExportCode::DeviceLost,
        ExportSkipCode::TimeLimit => api::CalibrationExportCode::TimeLimit,
        ExportSkipCode::Cancelled => api::CalibrationExportCode::Cancelled,
    }
}

fn api_stop(s: StopReason) -> api::CalibrationStop {
    match s {
        StopReason::Refused => api::CalibrationStop::Refused,
        StopReason::Late => api::CalibrationStop::Late,
        StopReason::Ceiling => api::CalibrationStop::Ceiling,
        StopReason::Limit => api::CalibrationStop::Limit,
        StopReason::Cancelled => api::CalibrationStop::Cancelled,
        StopReason::Error => api::CalibrationStop::Error,
    }
}

/// Why `calibration.run` is refused.
pub(super) enum RunRefusal {
    Conflict(&'static str),
    NotFound(String),
    Failed(String),
}

impl Host {
    /// Why an output start is refused now, if it is (ADR-0087 step D1).
    pub(super) fn calibration_blocks(&self) -> Option<&'static str> {
        self.calibration_run.blocks()
    }

    /// Something runs that a run must not share the encoders with.
    fn outputs_running(&self) -> bool {
        OutputCanvas::ALL.iter().any(|c| {
            let o = &self.outs[slot(*c)];
            o.recording.is_some() || o.streaming.is_some()
        }) || self.camera.is_some()
            || self.studio.replay_armed()
            || self.restarting
    }

    /// `calibration.run`: measures the plan of the saved recording profile on `c`.
    pub(super) fn run_calibration(&mut self, c: OutputCanvas) -> Result<(), RunRefusal> {
        if self.calibration_run.run.is_some() {
            return Err(RunRefusal::Conflict(RUNNING));
        }
        if self.outputs_running() {
            return Err(RunRefusal::Conflict(
                "an output runs; stop it before testing this computer",
            ));
        }
        // The test encodes as a start would, so it needs the encoder list too.
        self.ensure_encoders();
        let path = self.recording_file(c);
        let r = self
            .profile_request(c, path.clone())
            .map_err(RunRefusal::NotFound)?;
        let canvas = self.output_canvas(c).map_err(RunRefusal::NotFound)?;
        let out = self.master_settings(canvas, &r, &path);
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let separate = (c == OutputCanvas::Main && self.separate_files && self.marks_separate())
            .then(|| r.video.clone())
            .flatten()
            .map(|video| rbs_record::IsoVideoStart::new(String::new(), r.container, video));
        let mut request = rbs_record::StartPlanRequest::default();
        request.outputs = vec![rbs_record::PlannedOutput::new(&out, r.encoder.as_deref())];
        request.separate = separate.as_ref();
        let plan = self.studio.calibration_plan(
            &self.adapters,
            &self.collection,
            &request,
            folder.clone(),
        );
        // The folder is made as a recording makes it.
        let _ = std::fs::create_dir_all(&folder);
        let now = self.clock.now();
        let free_space = self
            .disk_probe
            .as_mut()
            .and_then(|p| p.free_space(&folder, now).bytes());

        let id = self.calibration_run.next_id;
        self.calibration_run.next_id += 1;
        let setup = self.calibration_run.setup.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let start = setup.clock.now();
        let steps = plan.steps();
        let first_part = if plan.exports.is_empty() {
            api::CalibrationPart::Encoder
        } else {
            api::CalibrationPart::Export
        };
        let input = WorkerInput {
            id,
            plan,
            setup: setup.clone(),
            stop: Arc::clone(&stop),
            start,
            free_space,
            inbox: self.inbox.clone(),
            live: Arc::clone(&self.gpu),
        };
        std::thread::Builder::new()
            .name("rbs-calibration".into())
            .spawn(move || {
                rbs_crash::report_panics_on_this_thread();
                if rbs_crash::contain(|| worker(&input)).is_err() {
                    send(
                        &input.inbox,
                        input.id,
                        WorkerMsg::Failed(
                            api::CalibrationCode::Panicked,
                            "the test stopped on an error",
                        ),
                    );
                }
            })
            .map_err(|_| RunRefusal::Failed("the test could not start".into()))?;
        // The thread is detached: the host never joins a worker.
        self.calibration_run.run = Some(ActiveRun {
            id,
            stop,
            deadline: start + CALIBRATION_LIMIT + GRACE,
            cancel_at: None,
            folder,
            step: 0,
            steps,
            part: first_part,
            adapter: None,
            encoders: Vec::new(),
            skipped: Vec::new(),
            write: None,
            export_steps: 0,
            exports: Vec::new(),
            exports_skipped: Vec::new(),
        });
        self.publish_calibration_running();
        Ok(())
    }

    /// `calibration.cancel`.
    pub(super) fn cancel_calibration(&mut self) {
        let now = self.calibration_run.setup.clock.now();
        if let Some(r) = &mut self.calibration_run.run
            && r.cancel_at.is_none()
        {
            r.stop.store(true, Ordering::Relaxed);
            r.cancel_at = Some(now + GRACE);
        }
        self.publish_calibration_running();
    }

    /// `calibration.status`.
    pub(super) fn api_calibration(&self) -> api::CalibrationState {
        self.calibration_run.state.clone()
    }

    /// A message from the worker of run `id`; a late one of an ended run is dropped.
    pub(super) fn calibration_msg(&mut self, id: u64, msg: WorkerMsg) {
        let Some(r) = &mut self.calibration_run.run else {
            return;
        };
        if r.id != id {
            return;
        }
        match msg {
            WorkerMsg::Event(CalibrationEvent::Step { step, adapter }) => {
                r.part = api::CalibrationPart::Encoder;
                r.step = r.export_steps + step;
                r.adapter = Some(adapter.as_str().to_owned());
                self.publish_calibration_running();
            }
            WorkerMsg::Event(CalibrationEvent::ExportStep { step }) => {
                r.part = api::CalibrationPart::Export;
                r.step = step;
                r.export_steps = step;
                r.adapter = None;
                self.publish_calibration_running();
            }
            WorkerMsg::Event(CalibrationEvent::Export(x)) => {
                r.exports.push(x);
                self.publish_calibration_running();
            }
            WorkerMsg::Event(CalibrationEvent::ExportSkipped(x)) => r.exports_skipped.push(x),
            WorkerMsg::Event(CalibrationEvent::Series(s)) => {
                r.encoders.push(s);
                self.publish_calibration_running();
            }
            WorkerMsg::Event(CalibrationEvent::Skipped(s)) => r.skipped.push(s),
            WorkerMsg::SessionsClosed => {
                r.part = api::CalibrationPart::Write;
                r.adapter = None;
                r.step = r.steps.saturating_sub(1).max(r.step);
                self.publish_calibration_running();
            }
            WorkerMsg::Folder(f) => r.write = Some(f),
            WorkerMsg::Done { cancelled } => self.end_calibration(cancelled),
            WorkerMsg::Failed(code, message) => self.fail_calibration(code, message),
        }
    }

    /// The deadline check of an active run, in every round; how long until it is due.
    pub(super) fn drive_calibration(&mut self) -> Option<Duration> {
        let r = self.calibration_run.run.as_ref()?;
        let now = self.calibration_run.setup.clock.now();
        let due = r.cancel_at.map_or(r.deadline, |c| c.min(r.deadline));
        if now < due {
            return Some(super::MAX_WAIT.min(due.saturating_since(now)));
        }
        // The worker did not answer in time: stop it, keep what arrived, detach.
        r.stop.store(true, Ordering::Relaxed);
        let cancelled = r.cancel_at.is_some();
        if r.encoders.is_empty() && r.exports.is_empty() && r.write.is_none() && !cancelled {
            self.fail_calibration(
                api::CalibrationCode::Hung,
                "the test did not answer; encoder sessions it held may stay open until RBS restarts",
            );
        } else {
            self.end_calibration(cancelled);
        }
        None
    }

    fn end_calibration(&mut self, cancelled: bool) {
        let Some(r) = self.calibration_run.run.take() else {
            return;
        };
        let now_unix = (self.calibration_run.setup.now_unix)();
        let entries: Vec<EncoderEntry> = r
            .encoders
            .iter()
            .map(|e| EncoderEntry {
                adapter: e.key.adapter.as_str().to_owned(),
                version: e.key.version.clone(),
                codec: e.key.class.codec.clone(),
                size: e.key.class.size.as_str().to_owned(),
                fps: e.key.class.fps.as_str().to_owned(),
                rate: e.key.class.rate.as_str().to_owned(),
                sessions: e.sessions,
                pixels_per_second: e.pixels_per_second,
                frame_time_us: u64::try_from(e.frame_time.as_micros()).unwrap_or(u64::MAX),
                measured_at: now_unix,
                partial: e.stopped_by.lower_bound(),
                stopped_by: Some(stop_reason_name(e.stopped_by).to_owned()),
            })
            .collect();
        let (write_speed, write_partial, write_skip) = match &r.write {
            Some(FolderResult::Speed {
                bytes_per_second,
                partial,
            }) => (Some(*bytes_per_second), *partial, None),
            Some(FolderResult::Skipped(code)) => (None, false, Some(*code)),
            None => (None, true, Some(SkipCode::TimeLimit)),
        };
        let folder = write_speed.map(|speed| FolderEntry {
            path: r.folder.clone(),
            write_speed: speed,
            measured_at: now_unix,
            partial: write_partial,
        });
        let this = self.this_computer();
        self.calibration
            .merge_run(&this, entries, folder, |p: &Path| p.exists());
        let gpu = this.gpu.clone();
        self.calibration.merge_exports(
            r.exports
                .iter()
                .map(|x| ExportEntry {
                    size: x.class.size.as_str().to_owned(),
                    format: Some(x.class.export_format().as_str().to_owned()),
                    time_us: micros_up(x.time),
                    gpu_us: x.gpu.map(micros_up),
                    kept_up: x.kept_up,
                    pixels: Some(u64::from(x.class.width) * u64::from(x.class.height)),
                    gpu: gpu.clone(),
                    measured_at: now_unix,
                    partial: false,
                })
                .collect(),
        );
        // Whether a test can still help a missing export (`calibrate`).
        if !r.exports.is_empty() {
            self.calibration_run.exports_unmeasurable = false;
        } else if r.exports_skipped.iter().any(|x| {
            matches!(
                x.code,
                ExportSkipCode::GpuUnavailable | ExportSkipCode::ExportFailed
            )
        }) {
            self.calibration_run.exports_unmeasurable = true;
        }
        let saved = match &self.config_dir {
            Some(dir) => crate::calibration_file::save(dir, &self.calibration).is_ok(),
            None => false,
        };
        let mut skipped: Vec<api::CalibrationSkip> = r
            .skipped
            .iter()
            .map(|s| api::CalibrationSkip {
                code: api_code(s.code),
                adapter: s.series.as_ref().map(|k| k.adapter.as_str().to_owned()),
                class: s.series.as_ref().map(|k| api_class(&k.class)),
            })
            .collect();
        if let Some(code) = write_skip {
            skipped.push(api::CalibrationSkip {
                code: api_code(code),
                adapter: None,
                class: None,
            });
        }
        let partial = write_partial
            || r.encoders.iter().any(|e| e.stopped_by.lower_bound())
            || r.skipped
                .iter()
                .any(|s| matches!(s.code, SkipCode::TimeLimit | SkipCode::Cancelled))
            || matches!(write_skip, Some(SkipCode::TimeLimit | SkipCode::Cancelled))
            || r.exports_skipped.iter().any(|x| {
                matches!(
                    x.code,
                    ExportSkipCode::TimeLimit | ExportSkipCode::Cancelled
                )
            });
        self.set_calibration_state(api::CalibrationState::Done {
            partial,
            cancelled,
            saved,
            encoders: r
                .encoders
                .iter()
                .map(|e| api::CalibrationEncoder {
                    adapter: e.key.adapter.as_str().to_owned(),
                    class: api_class(&e.key.class),
                    sessions: e.sessions,
                    pixels_per_second: e.pixels_per_second,
                    stopped_by: api_stop(e.stopped_by),
                })
                .collect(),
            write_speed,
            write_partial,
            skipped,
            exports: r
                .exports
                .iter()
                .map(|x| api::CalibrationExport {
                    size: x.class.size.as_str().to_owned(),
                    format: api_format(x.class.export_format()),
                    time_us: micros_up(x.time),
                    gpu_us: x.gpu.map(micros_up),
                    kept_up: x.kept_up,
                })
                .collect(),
            exports_skipped: r
                .exports_skipped
                .iter()
                .map(|x| api::CalibrationExportSkip {
                    size: x.class.size.as_str().to_owned(),
                    format: api_format(x.class.export_format()),
                    code: api_export_code(x.code),
                })
                .collect(),
        });
    }

    fn fail_calibration(&mut self, code: api::CalibrationCode, message: &str) {
        if let Some(r) = self.calibration_run.run.take() {
            r.stop.store(true, Ordering::Relaxed);
        }
        self.set_calibration_state(api::CalibrationState::Failed {
            code,
            message: message.to_owned(),
        });
    }

    fn publish_calibration_running(&mut self) {
        let Some(r) = &self.calibration_run.run else {
            return;
        };
        let state = api::CalibrationState::Running {
            step: r.step,
            steps: r.steps,
            part: r.part,
            adapter: r.adapter.clone(),
            cancelling: r.cancel_at.is_some(),
        };
        self.set_calibration_state(state);
    }

    fn set_calibration_state(&mut self, state: api::CalibrationState) {
        if self.calibration_run.state == state {
            return;
        }
        let payload = state.clone();
        self.events
            .changed(StateKey::topic(Topic::CalibrationState), || {
                EventPayload::CalibrationState(payload)
            });
        self.calibration_run.state = state;
    }
}
