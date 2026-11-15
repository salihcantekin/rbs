//! ADR-0087 step D2: the export part of *Test this computer*, on a virtual clock with
//! a scripted rig (ADR-0032: no sleeping). The real rig on a GPU is at the end and
//! skips without an adapter (`RBS_REQUIRE_GPU` fails instead).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_core::media::ColorSpace;
use rbs_core::time::{Clock, Timestamp};
use rbs_core::timing::FrameRate;
use rbs_gpu::YuvFormat;
use rbs_record::calibration::{
    CalibrationEvent, EXPORT_LIMIT, ExportClass, ExportCounters, ExportResult, ExportRig,
    ExportSkipCode, ExportsEnd, GPU_FRAMES, GpuExportRig, Pacer, RigError, class_estimate,
    export_rounds, export_warmup, run_exports,
};
use rbs_record::estimate::SizeClass;

#[derive(Clone, Default)]
struct VClock(Arc<AtomicU64>);

impl VClock {
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
    fn yield_now(&self) {}
    fn wait_until(&self, t: Timestamp) {
        let now = self.0.now();
        if now < t {
            self.0.advance(t.saturating_since(now));
        }
    }
}

/// What the scripted rig does.
#[derive(Clone)]
struct Script {
    /// Render-thread time of one tick.
    tick: Duration,
    /// GPU time of one conversion; the ring holds four.
    gpu: Duration,
    /// Every Nth paced tick finds the ring full (`busy`); 0 never.
    busy_every: u64,
    /// A stale frame every Nth tick; 0 never.
    stale_every: u64,
    /// The waits never finish.
    hang: bool,
    /// `begin` fails.
    begin_fails: bool,
    /// The device is lost on this tick.
    lost_at: Option<u64>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            tick: Duration::from_micros(400),
            gpu: Duration::from_micros(300),
            busy_every: 0,
            stale_every: 0,
            hang: false,
            begin_fails: false,
            lost_at: None,
        }
    }
}

struct FakeRig {
    clock: VClock,
    script: Script,
    ticks: u64,
    counters: ExportCounters,
    /// Conversions not yet read back, and when the GPU is free again.
    in_flight: u64,
    gpu_free: Timestamp,
    begun: Arc<Mutex<Vec<(u32, u32)>>>,
}

impl ExportRig for FakeRig {
    fn begin(&mut self, class: &ExportClass) -> Result<(), RigError> {
        self.begun.lock().unwrap().push((class.width, class.height));
        if self.script.begin_fails {
            return Err(RigError::Failed);
        }
        Ok(())
    }

    fn tick(&mut self) -> Result<(), RigError> {
        self.ticks += 1;
        if self.script.lost_at == Some(self.ticks) {
            return Err(RigError::DeviceLost);
        }
        let now = self.clock.now();
        // Readbacks done by now are delivered.
        if now >= self.gpu_free {
            self.counters.frames += self.in_flight;
            self.in_flight = 0;
        }
        let forced_busy =
            self.script.busy_every > 0 && self.ticks.is_multiple_of(self.script.busy_every);
        if self.in_flight >= 4 || forced_busy {
            self.counters.busy += 1;
        } else {
            self.in_flight += 1;
            self.gpu_free = self.gpu_free.max(now) + self.script.gpu;
            if self.script.stale_every > 0 && self.ticks.is_multiple_of(self.script.stale_every) {
                self.counters.lost += 1;
                self.counters.frames = self.counters.frames.saturating_sub(1);
            }
        }
        self.clock.advance(self.script.tick);
        Ok(())
    }

    fn wait(&mut self, timeout: Duration) -> Result<bool, RigError> {
        let now = self.clock.now();
        if self.script.hang {
            self.clock.advance(timeout);
            return Ok(false);
        }
        let left = self.gpu_free.saturating_since(now);
        if left > timeout {
            self.clock.advance(timeout);
            return Ok(false);
        }
        self.clock.advance(left);
        self.counters.frames += self.in_flight;
        self.in_flight = 0;
        Ok(true)
    }

    fn counters(&self) -> ExportCounters {
        self.counters
    }

    fn end(&mut self) {}
}

fn class(size: SizeClass) -> ExportClass {
    let (width, height) = ExportClass::bound(size, (0, 0));
    ExportClass {
        size,
        width,
        height,
        format: YuvFormat::Nv12,
        color: ColorSpace::BT709_LIMITED,
        open_shadows: false,
    }
}

struct Run {
    events: Vec<CalibrationEvent>,
    end: ExportsEnd,
    begun: Vec<(u32, u32)>,
}

fn run_full(
    classes: &[ExportClass],
    period: Duration,
    script: Option<Script>,
    live_lost: bool,
    stop: &AtomicBool,
) -> Run {
    let clock = VClock::default();
    let pacer = TestPacer(clock.clone());
    let begun = Arc::new(Mutex::new(Vec::new()));
    let mut open = || -> Result<Box<dyn ExportRig>, RigError> {
        match &script {
            Some(s) => Ok(Box::new(FakeRig {
                clock: clock.clone(),
                script: s.clone(),
                ticks: 0,
                counters: ExportCounters::default(),
                in_flight: 0,
                gpu_free: Timestamp::ZERO,
                begun: Arc::clone(&begun),
            })),
            None => Err(RigError::Failed),
        }
    };
    let mut events = Vec::new();
    let (_rig, end) = run_exports(
        classes,
        period,
        &mut open,
        &mut || live_lost,
        &clock,
        &pacer,
        stop,
        Timestamp::ZERO,
        &mut |e| events.push(e),
    );
    let begun = begun.lock().unwrap().clone();
    Run { events, end, begun }
}

fn run(classes: &[ExportClass], period: Duration, script: Script) -> Run {
    run_full(
        classes,
        period,
        Some(script),
        false,
        &AtomicBool::new(false),
    )
}

fn results(r: &Run) -> Vec<ExportResult> {
    r.events
        .iter()
        .filter_map(|e| match e {
            CalibrationEvent::Export(x) => Some(x.clone()),
            _ => None,
        })
        .collect()
}

fn skips(r: &Run) -> Vec<(SizeClass, ExportSkipCode)> {
    r.events
        .iter()
        .filter_map(|e| match e {
            CalibrationEvent::ExportSkipped(s) => Some((s.class.size, s.code)),
            _ => None,
        })
        .collect()
}

const P60: Duration = Duration::from_nanos(16_666_667);

#[test]
fn the_time_is_the_render_threads_mean_tick_and_the_gpu_time_is_its_throughput() {
    let r = run(&[class(SizeClass::L1080)], P60, Script::default());
    assert_eq!(r.end, ExportsEnd::Clean);
    let x = &results(&r)[0];
    assert_eq!(x.time, Duration::from_micros(400));
    assert!(x.kept_up);
    // Back to back, the GPU sets the pace: 300 µs per conversion, not the 400 µs a
    // tick costs the thread, nor a wait's latency.
    let gpu = x.gpu.unwrap();
    assert!(
        gpu >= Duration::from_micros(300) && gpu <= Duration::from_micros(420),
        "{gpu:?}"
    );
    assert_eq!(r.begun, vec![(1920, 1080)]);
}

#[test]
fn rounds_follow_time_at_any_frame_rate() {
    for (fps, warm, rounds) in [(24u32, 8, 16), (30, 8, 16), (60, 15, 30), (120, 30, 60)] {
        let period = FrameRate::new(fps, 1).unwrap().frame_duration();
        assert_eq!(
            (export_warmup(period), export_rounds(period)),
            (warm, rounds),
            "{fps}"
        );
    }
    // In the export limit after the device's reserve: 2 classes at 24 fps, 3 at 30,
    // 60 and 120 fps.
    let budget = EXPORT_LIMIT - Duration::from_secs(1);
    for (fps, fit) in [(24u32, 2u32), (30, 3), (60, 3), (120, 3)] {
        let period = FrameRate::new(fps, 1).unwrap().frame_duration();
        let n = (budget.as_nanos() / class_estimate(period).as_nanos()) as u32;
        assert_eq!(n, fit, "{fps}");
    }
}

#[test]
fn a_full_ring_or_too_few_frames_is_a_whole_period() {
    let r = run(
        &[class(SizeClass::L2160)],
        P60,
        Script {
            busy_every: 7,
            ..Script::default()
        },
    );
    let x = &results(&r)[0];
    assert!(!x.kept_up);
    assert_eq!(x.time, P60);
}

#[test]
fn a_stale_frame_alone_is_not_falling_behind() {
    let r = run(
        &[class(SizeClass::L1080)],
        P60,
        Script {
            stale_every: 9,
            ..Script::default()
        },
    );
    assert!(results(&r)[0].kept_up);
}

#[test]
fn a_gpu_that_never_answers_is_skipped_for_time_and_reaped() {
    let r = run(
        &[class(SizeClass::L1080), class(SizeClass::L720)],
        P60,
        Script {
            hang: true,
            ..Script::default()
        },
    );
    assert_eq!(r.end, ExportsEnd::Reap);
    assert!(results(&r).is_empty());
    assert_eq!(
        skips(&r),
        vec![
            (SizeClass::L1080, ExportSkipCode::TimeLimit),
            (SizeClass::L720, ExportSkipCode::TimeLimit)
        ]
    );
}

#[test]
fn no_throwaway_device_skips_every_class() {
    let r = run_full(
        &[class(SizeClass::L1080), class(SizeClass::L720)],
        P60,
        None,
        false,
        &AtomicBool::new(false),
    );
    assert_eq!(r.end, ExportsEnd::Clean);
    assert_eq!(
        skips(&r),
        vec![
            (SizeClass::L1080, ExportSkipCode::GpuUnavailable),
            (SizeClass::L720, ExportSkipCode::GpuUnavailable)
        ]
    );
}

#[test]
fn a_failed_graph_skips_its_class() {
    let r = run(
        &[class(SizeClass::L1080)],
        P60,
        Script {
            begin_fails: true,
            ..Script::default()
        },
    );
    assert_eq!(
        skips(&r),
        vec![(SizeClass::L1080, ExportSkipCode::ExportFailed)]
    );
    assert_eq!(r.end, ExportsEnd::Clean);
}

#[test]
fn a_lost_throwaway_device_goes_on_unless_the_live_one_is_lost_too() {
    let lost = Script {
        lost_at: Some(3),
        ..Script::default()
    };
    let classes = [class(SizeClass::L1080), class(SizeClass::L720)];
    let r = run_full(
        &classes,
        P60,
        Some(lost.clone()),
        false,
        &AtomicBool::new(false),
    );
    assert_eq!(r.end, ExportsEnd::Reap);
    assert_eq!(
        skips(&r),
        vec![
            (SizeClass::L1080, ExportSkipCode::DeviceLost),
            (SizeClass::L720, ExportSkipCode::DeviceLost)
        ]
    );
    let r = run_full(&classes, P60, Some(lost), true, &AtomicBool::new(false));
    assert_eq!(r.end, ExportsEnd::LiveLost);
}

#[test]
fn a_cancel_skips_what_is_left() {
    let r = run_full(
        &[class(SizeClass::L1080)],
        P60,
        Some(Script::default()),
        false,
        &AtomicBool::new(true),
    );
    assert_eq!(
        skips(&r),
        vec![(SizeClass::L1080, ExportSkipCode::Cancelled)]
    );
    assert!(r.begun.is_empty());
}

#[test]
fn classes_past_the_limit_are_skipped_for_time() {
    // A slow export at 24 fps: each class uses its whole GPU part, and a class
    // starts only while its worst case fits in what is left; the rest is left for a
    // later run.
    let period = FrameRate::FPS_24.frame_duration();
    let classes = [
        class(SizeClass::L2160),
        class(SizeClass::L1440),
        class(SizeClass::L1080),
        class(SizeClass::L720),
    ];
    let r = run(
        &classes,
        period,
        Script {
            tick: Duration::from_millis(10),
            gpu: Duration::from_millis(10),
            ..Script::default()
        },
    );
    assert_eq!(results(&r).len(), 3);
    assert_eq!(
        skips(&r),
        vec![(SizeClass::L720, ExportSkipCode::TimeLimit)]
    );
}

#[test]
fn a_class_is_measured_at_its_bound_or_a_larger_planned_size() {
    assert_eq!(
        ExportClass::bound(SizeClass::L1080, (1440, 1080)),
        (1920, 1080)
    );
    assert_eq!(
        ExportClass::bound(SizeClass::L2160, (1080, 1920)),
        (3840, 2160)
    );
    assert_eq!(
        ExportClass::bound(SizeClass::Above2160, (5121, 2881)),
        (5122, 2882)
    );
    let _ = GPU_FRAMES;
}

// ---------------------------------------------------------------------------------
// The real rig on a GPU.

#[test]
fn the_real_rig_measures_a_tiny_class_on_a_throwaway_device() {
    let ctx = match rbs_gpu::GpuContext::new(&rbs_gpu::GpuRequest {
        allow_software: true,
        ..rbs_gpu::GpuRequest::default()
    }) {
        Ok(c) => Arc::new(c),
        Err(rbs_gpu::GpuError::NoAdapter) => {
            assert!(
                std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                "RBS_REQUIRE_GPU is set but no adapter was found"
            );
            eprintln!("SKIPPED: no GPU adapter");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    // The run's own device, as the app makes it; the real clock bounds the waits.
    let sibling = Arc::new(ctx.sibling().unwrap());
    let clock = rbs_core::time::MonotonicClock::new();
    struct NoWait;
    impl Pacer for NoWait {
        fn yield_now(&self) {}
        fn wait_until(&self, _: Timestamp) {}
    }
    let tiny = ExportClass {
        size: SizeClass::L720,
        width: 64,
        height: 36,
        format: YuvFormat::Nv12,
        color: ColorSpace::BT709_LIMITED,
        open_shadows: false,
    };
    let mut open = || -> Result<Box<dyn ExportRig>, RigError> {
        Ok(Box::new(GpuExportRig::new(
            Arc::clone(&sibling),
            FrameRate::FPS_60,
        )))
    };
    let mut events = Vec::new();
    let (rig, end) = run_exports(
        &[tiny],
        P60,
        &mut open,
        &mut || false,
        &clock,
        &NoWait,
        &AtomicBool::new(false),
        clock.now(),
        &mut |e| events.push(e),
    );
    drop(rig);
    assert_eq!(end, ExportsEnd::Clean, "{events:?}");
    let measured: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            CalibrationEvent::Export(x) => Some(x.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(measured.len(), 1, "{events:?}");
    eprintln!("measured on {}: {:?}", ctx.adapter_name(), measured[0]);
}
