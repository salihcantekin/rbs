//! A diagnostic of *Test this computer*'s encoder part (ADR-0087 step D1): runs the
//! `rbs_record::calibration` step driver on the platform's encoders with a tracing
//! lane executor, once per frame content, and prints per step what the late rules
//! read (round times, backlog, `Busy`, lost frames, call times). Nothing is written.
//!
//! ```sh
//! cargo run --release -p rbs-app --example calibration_trace -- [kbps]
//! ```
//!
//! Contents: `moving` (the run's own ring: a gradient with a moving block, close to a
//! screen or a camera), `noise` (high-residual noise, the worst case) and `flat` (one grey frame).
//! Only the plan's likely encoder of the master's class is measured. `kbps` sets the
//! master's video bit rate (the default output settings' when absent).

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::expect_used)] // A demo: setup errors end it.
#![allow(clippy::disallowed_methods)] // A demo, not the app shell: ADR-0055 rules the shell.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use rbs_codec::{CreateError, EncodeError, VideoEncoderConfig, VideoEncoderFactory};
use rbs_core::media::{Bytes, ContainerId, PlaneLayout, Size, SystemFrame, VideoFormat};
use rbs_core::time::{Clock, MonotonicClock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_record::calibration::{
    CalibrationEvent, CalibrationPlan, FrameOutcome, Lanes, LanesFailure, Pacer, RoundRequest,
    SessionReport, SessionRunner, run_encoders,
};
use rbs_record::{OutputSettings, PlannedOutput, StartPlanRequest, Studio, StudioSettings};
use rbs_scene::SceneCollection;
use rbs_scene::model::SceneScope;

#[derive(Clone, Copy, Debug)]
enum Content {
    Noise,
    Moving,
    Flat,
}

struct RealPacer(Arc<dyn Clock>);

impl Pacer for RealPacer {
    fn yield_now(&self) {
        std::thread::yield_now();
    }

    fn wait_until(&self, t: Timestamp) {
        let left = t.saturating_since(self.0.now());
        if !left.is_zero() {
            std::thread::sleep(left);
        }
    }
}

/// Eight frames of `content` (the run's ring is kept for `Moving`).
fn ring(format: VideoFormat, content: Content, run: &Arc<[SystemFrame]>) -> Arc<[SystemFrame]> {
    let (w, h) = (format.size.width, format.size.height);
    let pf = format.pixel_format;
    let frames = match content {
        Content::Moving => return Arc::clone(run),
        Content::Flat => 1,
        Content::Noise => 8,
    };
    let mut out = Vec::new();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..frames {
        let mut data = Vec::new();
        let mut planes = Vec::new();
        for plane in 0..pf.plane_count() {
            let stride = pf.min_row_bytes(plane, w).expect("a CPU layout") as usize;
            let rows = pf.plane_rows(plane, h).expect("a CPU layout") as usize;
            planes.push(PlaneLayout {
                offset: data.len(),
                stride,
            });
            for _ in 0..rows * stride {
                let v = if matches!(content, Content::Flat) {
                    128u8
                } else {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                };
                data.push(v);
            }
        }
        out.push(SystemFrame::new(format, Bytes::from(data), &planes).expect("a frame"));
    }
    if matches!(content, Content::Flat) {
        let one = out[0].clone();
        out = vec![one; 8];
    }
    out.into()
}

enum Req {
    Open(
        Arc<dyn VideoEncoderFactory>,
        Box<VideoEncoderConfig>,
        Arc<[SystemFrame]>,
    ),
    Round(RoundRequest),
    Close,
}

enum Ans {
    Opened(Result<(), CreateError>),
    Round(Vec<SessionReport>),
    Closed(Vec<Result<bool, EncodeError>>),
}

struct Lane {
    tx: Sender<Req>,
    rx: Receiver<Ans>,
    reports: Vec<SessionReport>,
}

/// One step's numbers.
#[derive(Default)]
struct StepTrace {
    sessions: usize,
    rounds: Vec<Duration>,
    backlog_max: Vec<u64>,
    last: Vec<(Duration, Vec<u64>, Vec<String>)>,
    busy_late: u32,
    lost: u32,
    call: Vec<Duration>,
}

struct TraceLanes {
    lanes: Vec<Lane>,
    order: Vec<(usize, usize)>,
    content: Content,
    period: Duration,
    step: StepTrace,
}

fn lane_body(rx: Receiver<Req>, tx: Sender<Ans>, clock: Arc<dyn Clock>, pacer: Arc<dyn Pacer>) {
    let stop = AtomicBool::new(false);
    let mut runners: Vec<SessionRunner> = Vec::new();
    while let Ok(req) = rx.recv() {
        let ans = match req {
            Req::Open(factory, config, ring) => Ans::Opened(factory.create(&config).map(|e| {
                runners.push(SessionRunner::new(
                    e,
                    ring,
                    config.frame_rate.frame_duration(),
                ));
            })),
            Req::Round(r) => Ans::Round(
                runners
                    .iter_mut()
                    .map(|s| s.encode(&*clock, &*pacer, r.deadline, &stop))
                    .collect(),
            ),
            Req::Close => {
                let c = runners.iter_mut().map(SessionRunner::flush).collect();
                runners.clear();
                Ans::Closed(c)
            }
        };
        if tx.send(ans).is_err() {
            return;
        }
    }
}

impl TraceLanes {
    fn new(
        clock: &Arc<dyn Clock>,
        pacer: &Arc<dyn Pacer>,
        content: Content,
        period: Duration,
    ) -> Self {
        let lanes = (0..3)
            .map(|_| {
                let (tx, rx_req) = mpsc::channel();
                let (tx_ans, rx) = mpsc::channel();
                let (c, p) = (Arc::clone(clock), Arc::clone(pacer));
                std::thread::spawn(move || lane_body(rx_req, tx_ans, c, p));
                Lane {
                    tx,
                    rx,
                    reports: Vec::new(),
                }
            })
            .collect();
        Self {
            lanes,
            order: Vec::new(),
            content,
            period,
            step: StepTrace::default(),
        }
    }

    fn print_step(&mut self) {
        let s = std::mem::take(&mut self.step);
        if s.sessions == 0 {
            return;
        }
        let p = self.period.as_secs_f64() * 1e3;
        let ms = |d: &Duration| d.as_secs_f64() * 1e3;
        let n = s.rounds.len();
        let tail = &s.rounds[n.saturating_sub(120)..];
        let mean = tail.iter().map(ms).sum::<f64>() / tail.len().max(1) as f64;
        let max = s.rounds.iter().map(ms).fold(0.0, f64::max);
        let call = s.call.iter().map(ms).sum::<f64>() / s.call.len().max(1) as f64;
        println!(
            "  step {:2}: {:4} rounds, last-120 mean {:6.2} ms (P {:.2}), max {:7.2} ms, \
             backlog max {:?}, busy-late {}, lost {}, mean call {:.2} ms",
            s.sessions, n, mean, p, max, s.backlog_max, s.busy_late, s.lost, call
        );
        for (d, b, o) in &s.last {
            println!("      last: {:6.2} ms backlog {b:?} {o:?}", ms(d));
        }
    }
}

impl Lanes for TraceLanes {
    fn open(
        &mut self,
        lane: usize,
        factory: &Arc<dyn VideoEncoderFactory>,
        config: &VideoEncoderConfig,
        run_ring: &Arc<[SystemFrame]>,
    ) -> Result<Result<(), CreateError>, LanesFailure> {
        self.print_step();
        let ring = ring(config.format, self.content, run_ring);
        let l = self.lanes.get_mut(lane).ok_or(LanesFailure)?;
        l.tx.send(Req::Open(
            Arc::clone(factory),
            Box::new(config.clone()),
            ring,
        ))
        .map_err(|_| LanesFailure)?;
        match l.rx.recv() {
            Ok(Ans::Opened(Ok(()))) => {
                let i = self.order.iter().filter(|(x, _)| *x == lane).count();
                self.order.push((lane, i));
                self.step.sessions = self.order.len();
                self.step.backlog_max = vec![0; self.order.len()];
                Ok(Ok(()))
            }
            Ok(Ans::Opened(Err(e))) => {
                println!("  open {} refused: {e:?}", self.order.len() + 1);
                Ok(Err(e))
            }
            _ => Err(LanesFailure),
        }
    }

    fn round(
        &mut self,
        request: RoundRequest,
        out: &mut Vec<SessionReport>,
    ) -> Result<(), LanesFailure> {
        let start = Instant::now();
        for l in &self.lanes {
            l.tx.send(Req::Round(request)).map_err(|_| LanesFailure)?;
        }
        for l in &mut self.lanes {
            match l.rx.recv() {
                Ok(Ans::Round(r)) => l.reports = r,
                _ => return Err(LanesFailure),
            }
        }
        let d = start.elapsed();
        out.clear();
        for (lane, i) in &self.order {
            out.push(
                self.lanes[*lane]
                    .reports
                    .get(*i)
                    .cloned()
                    .ok_or(LanesFailure)?,
            );
        }
        let s = &mut self.step;
        s.rounds.push(d);
        for (m, r) in s.backlog_max.iter_mut().zip(out.iter()) {
            *m = (*m).max(r.backlog);
        }
        for r in out.iter() {
            s.call.push(r.call_time);
            if r.outcome == FrameOutcome::Late {
                s.busy_late += 1;
            }
            if r.lost {
                s.lost += 1;
            }
        }
        s.last.push((
            d,
            out.iter().map(|r| r.backlog).collect(),
            out.iter().map(|r| format!("{:?}", r.outcome)).collect(),
        ));
        if s.last.len() > 3 {
            s.last.remove(0);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<Vec<Result<bool, EncodeError>>, LanesFailure> {
        self.print_step();
        let mut out = Vec::new();
        for l in &self.lanes {
            l.tx.send(Req::Close).map_err(|_| LanesFailure)?;
        }
        for l in &self.lanes {
            match l.rx.recv() {
                Ok(Ans::Closed(c)) => out.extend(c),
                _ => return Err(LanesFailure),
            }
        }
        self.order.clear();
        println!("  closed: {out:?}");
        Ok(out)
    }
}

fn main() {
    let kbps: Option<u64> = std::env::args().nth(1).and_then(|s| s.parse().ok());
    let mut col = SceneCollection::new();
    let (main, _) = col
        .add_canvas("Main", Size::new(1920, 1080), FrameRate::FPS_60)
        .expect("canvas");
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).expect("scene");
    col.set_program(main, Some(scene)).expect("program");
    let (mut adapters, _) = rbs_app::platform::adapters();
    adapters.probe();
    let gpu = Arc::new(rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest::default()).expect("a GPU"));
    let mut engine = rbs_engine::Engine::new();
    engine.set_thread_policy(Some(Arc::new(rbs_thread_os::OsThreadPolicy::new())));
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let mut studio =
        Studio::start(engine, gpu, &col, &adapters.sources, &settings).expect("the studio starts");
    let mut out = OutputSettings::new("file:///tmp/unused.mkv", ContainerId::Matroska);
    out.audio = None;
    out.canvas = Some(main);
    if let (Some(k), Some(v)) = (kbps, out.video.as_mut()) {
        v.rate_control = rbs_codec::RateControl::Cbr {
            bitrate_bps: k * 1000,
        };
    }
    let mut request = StartPlanRequest::default();
    request.outputs = vec![PlannedOutput::new(&out, None)];
    let full = studio.calibration_plan(&adapters, &col, &request, std::env::temp_dir());
    let Some(class) = full.classes.first() else {
        eprintln!("no class to measure");
        return;
    };
    let mut class = class.clone();
    class.candidates.retain(|c| c.likely);
    println!(
        "class {:?}; candidate {:?}; config {:?}",
        class.key,
        class
            .candidates
            .iter()
            .map(|c| c.adapter.as_str())
            .collect::<Vec<_>>(),
        class.config
    );
    let plan = CalibrationPlan {
        classes: vec![class],
        folder: full.folder,
        exports: Vec::new(),
        frame_rate: full.frame_rate,
    };
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let pacer: Arc<dyn Pacer> = Arc::new(RealPacer(Arc::clone(&clock)));
    let period = FrameRate::FPS_60.frame_duration();
    for content in [Content::Moving, Content::Noise, Content::Flat] {
        println!("== {content:?}");
        let mut lanes = TraceLanes::new(&clock, &pacer, content, period);
        let stop = AtomicBool::new(false);
        let begun = Instant::now();
        let outcome = run_encoders(
            &plan,
            &mut lanes,
            &*clock,
            &*pacer,
            &stop,
            clock.now(),
            &mut |e| match e {
                CalibrationEvent::Series(r) => println!(
                    "  RESULT sessions {} stopped_by {:?} frame_time {:?}",
                    r.sessions, r.stopped_by, r.frame_time
                ),
                CalibrationEvent::Skipped(s) => println!("  SKIPPED {s:?}"),
                _ => {}
            },
        );
        println!(
            "  outcome {outcome:?} after {:.1} s",
            begun.elapsed().as_secs_f64()
        );
    }
    let _ = studio.shutdown();
}
