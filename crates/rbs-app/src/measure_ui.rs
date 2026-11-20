//! The UI measure mode (ADR-0120 decision 13): `RBS_MEASURE_UI=<seconds>` measures the
//! main window as the user sees it and quits.
//!
//! After 3 s of warm-up it runs five phases of `<seconds>` each and prints one line per
//! phase:
//!
//! - **idle**: nothing is sent; frames counted here are redraws nobody asked for;
//! - **hover**: the pointer traces a Lissajous path over the whole window, one step per
//!   tick (x and y bounce between the edges with different periods), so the hover fills
//!   change whichever page is showing, the first-run page included;
//! - **resize**: the window grows and shrinks by up to 200 logical pixels, one step per
//!   tick;
//! - **sweep**: the pointer runs along the frame bar's row of buttons and back, so the
//!   gradient faces change their hover state one after another (ADR-0122 decision 9);
//! - **redraw**: a redraw of the whole window is asked for every tick, as a moving meter
//!   causes where the GPU path redraws everything (ADR-0122 decision 9).
//!
//! For each phase: frames drawn, and the render time (Slint's before- to after-rendering
//! notification, the work a theme's effects add) and the frame interval as p50, p95, p99
//! and max in milliseconds, and the meter rows the Mixer wrote (`meter_writes`).
//!
//! Responsiveness (performance-budgets spec, "Desktop UI"), one more line per phase:
//!
//! - **input to frame** (`input_ms`, hover, resize and sweep): from a pointer move or a
//!   size change handed to the window to the end of the next frame drawn, p50, p95, p99
//!   and max. A move that changes nothing draws no frame; the next move replaces it, so
//!   each value starts at the latest input before its frame. It covers the app's share only: the OS's input
//!   queue before and the compositor's presentation after are not in it;
//! - **long tasks** (`long_tasks`): UI entries that ran `ui_thread::LONG_TASK` (50 ms)
//!   or longer, so input waited at least that long; their count, the longest, and the
//!   three places with the most time, as `file:line=count/max_ms`. The warm-up is a
//!   phase of its own here (`warmup`), since start-up work lands there.
//!
//! At the end, `close_ms` is the time Quit took on the UI thread until every window was
//! hidden and the shutdown handed to its thread (ADR-0156); the process exit after it is
//! timed by `cargo xtask device-check app`.
//! `RBS_EFFECTS=<calm|glow|glass|tactile>` picks the effects of both slots for the run
//! (`look`), so the same build measures each setting. Process CPU and GPU memory are
//! read from the OS tools while it runs (Activity Monitor or `footprint` on macOS, Task
//! Manager on Windows).
//!
//! Frame notifications: Slint keeps one rendering notifier per window, and a second
//! `set_rendering_notifier` replaces the first. On macOS and Windows the app already has
//! one (the preview layer follows the window, `main.rs`), so there that notifier calls
//! [`observe`] and this module installs none. Elsewhere it installs its own.
//!
//! Zero cost when unused (P13): without the variable nothing here is installed, and
//! [`observe`] returns after one thread-local read.

use std::cell::RefCell;
use std::panic::Location;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::platform::WindowEvent;

const WARM_UP: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(16);
/// Pointer steps from edge to edge: across (x) and down (y). Different, coprime periods
/// make the path cover the window within a few seconds.
const HOVER_STEPS: u32 = 120;
const HOVER_STEPS_Y: u32 = 43;
const RESIZE_REACH: f32 = 200.0;
const RESIZE_STEPS: u32 = 60;
/// Where the sweep runs, in logical pixels from the window's top: inside the frame bar's
/// buttons in every effects setting (the bar is taller in Glow, its buttons centred).
const SWEEP_Y: f32 = 24.0;

#[derive(Default)]
struct Phase {
    name: &'static str,
    renders: Vec<Duration>,
    intervals: Vec<Duration>,
    /// Meter rows written in this phase ([`count_meter_write`]).
    meter_writes: u64,
    /// Input to the end of the next frame (hover, resize and sweep).
    inputs: Vec<Duration>,
    /// Long tasks ([`rbs_app::ui_thread::LONG_TASK`]) and where their entries were
    /// installed.
    long: Vec<(Duration, &'static Location<'static>)>,
}

#[derive(Default)]
struct State {
    phases: Vec<Phase>,
    started: Option<Instant>,
    last_frame: Option<Instant>,
    render_start: Option<Instant>,
    /// The latest input (pointer move or size change) not yet answered by a frame.
    input_at: Option<Instant>,
    /// Long tasks before the first phase ([`WARM_UP`]).
    warm_up_long: Vec<(Duration, &'static Location<'static>)>,
    reported: bool,
}

thread_local! {
    /// The running measurement, when the app's own notifier forwards frames here.
    static ACTIVE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
}

/// Called by the app's rendering notifier for every state; does nothing unless a
/// measurement is running.
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
pub fn observe(state: &slint::RenderingState) {
    ACTIVE.with(|a| {
        if let Some(st) = a.borrow().as_ref() {
            record(st, state);
        }
    });
}

/// Called for every meter row the Mixer writes; does nothing unless a measurement is
/// running. The count tells whether idle frames come from meters (audit U-02).
pub fn count_meter_write() {
    ACTIVE.with(|a| {
        if let Some(st) = a.borrow().as_ref()
            && let Some(p) = st.borrow_mut().phases.last_mut()
        {
            p.meter_writes += 1;
        }
    });
}

/// The UI thread's long-task hook while measuring ([`rbs_app::ui_thread`]).
fn long_task(took: Duration, at: &'static Location<'static>) {
    ACTIVE.with(|a| {
        if let Some(st) = a.borrow().as_ref()
            && let Ok(mut st) = st.try_borrow_mut()
        {
            match st.phases.last_mut() {
                Some(p) => p.long.push((took, at)),
                None => st.warm_up_long.push((took, at)),
            }
        }
    });
}

fn record(state: &Rc<RefCell<State>>, s: &slint::RenderingState) {
    let now = Instant::now();
    let mut st = state.borrow_mut();
    match s {
        slint::RenderingState::BeforeRendering => st.render_start = Some(now),
        slint::RenderingState::AfterRendering => {
            let render = st.render_start.take().map(|t| now - t);
            let interval = st.last_frame.replace(now).map(|t| now - t);
            let input = st.input_at.take().map(|t| now - t);
            if let Some(p) = st.phases.last_mut() {
                if let Some(r) = render {
                    p.renders.push(r);
                }
                if let Some(i) = interval {
                    p.intervals.push(i);
                }
                if let Some(i) = input {
                    p.inputs.push(i);
                }
            }
        }
        _ => {}
    }
}

/// The seconds `RBS_MEASURE_UI` asks for, if set and valid.
pub fn requested() -> Option<u64> {
    std::env::var("RBS_MEASURE_UI")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
}

/// Installs the measure mode on `main`: the rendering notifier and the sweep timer.
pub fn install(main: &crate::ui::MainWindow, seconds: u64) {
    let state = Rc::new(RefCell::new(State::default()));
    // Frames (where the app's notifier forwards to `observe`) and meter writes.
    ACTIVE.with(|a| *a.borrow_mut() = Some(state.clone()));
    rbs_app::ui_thread::set_long_task_hook(Some(long_task));
    if !cfg!(any(target_os = "macos", windows)) {
        let st = state.clone();
        let installed =
            crate::entry::set_rendering_notifier(main.window(), move |s, _| record(&st, &s));
        if let Err(e) = installed {
            rbs_core::warn!(
                rbs_core::log::Area::Ui,
                "measure: this renderer gives no frame notifications: {e:?}"
            );
        }
    }
    let weak = main.as_weak();
    let phase_len = Duration::from_secs(seconds);
    let timer = slint::Timer::default();
    let mut tick: u32 = 0;
    let mut base_size: Option<slint::LogicalSize> = None;
    crate::entry::start(&timer, slint::TimerMode::Repeated, TICK, move || {
        let Some(main) = weak.upgrade() else { return };
        let now = Instant::now();
        let mut st = state.borrow_mut();
        let started = *st.started.get_or_insert(now);
        let elapsed = now - started;
        let window = main.window();
        let scale = window.scale_factor();
        if elapsed < WARM_UP {
            return;
        }
        let at = elapsed - WARM_UP;
        let index = (at.as_secs_f64() / phase_len.as_secs_f64()) as usize;
        let names = ["idle", "hover", "resize", "sweep", "redraw"];
        if index >= names.len() {
            // The quit takes effect after this tick returns; report once.
            if !st.reported {
                st.reported = true;
                if let Some(size) = base_size {
                    window.set_size(size);
                }
                report(&st, scale);
                rbs_app::ui_thread::set_long_task_hook(None);
                ACTIVE.with(|a| *a.borrow_mut() = None);
                // Quits as the user's Quit does, so a run also measures the shutdown
                // (ADR-0156); queued, since this tick holds the window.
                crate::quit::quit_after_measure();
            }
            return;
        }
        if st.phases.len() <= index {
            st.phases.push(Phase {
                name: names[index],
                ..Phase::default()
            });
            st.last_frame = None;
            st.input_at = None;
            tick = 0;
        }
        tick += 1;
        let size = window.size().to_logical(scale);
        match names[index] {
            "hover" => {
                let x = size.width * bounce(tick, HOVER_STEPS);
                let y = size.height * bounce(tick, HOVER_STEPS_Y);
                st.input_at = Some(Instant::now());
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(x, y),
                });
            }
            "resize" => {
                let base = *base_size.get_or_insert(size);
                let step = tick % (2 * RESIZE_STEPS);
                let step = if step < RESIZE_STEPS {
                    step
                } else {
                    2 * RESIZE_STEPS - step
                };
                let grow = RESIZE_REACH * step as f32 / RESIZE_STEPS as f32;
                st.input_at = Some(Instant::now());
                window.set_size(slint::LogicalSize::new(
                    base.width + grow,
                    base.height + grow / 2.0,
                ));
            }
            "sweep" => {
                if let Some(size) = base_size.take() {
                    window.set_size(size);
                }
                let x = size.width * bounce(tick, HOVER_STEPS);
                st.input_at = Some(Instant::now());
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(x, SWEEP_Y),
                });
            }
            "redraw" => window.request_redraw(),
            _ => {}
        }
    });
    // The timer lives as long as the event loop.
    std::mem::forget(timer);
}

/// A triangle wave over `steps`: 0 at tick 0, 1 at `steps`, back to 0 at `2 * steps`.
fn bounce(tick: u32, steps: u32) -> f32 {
    let step = tick % (2 * steps);
    let step = if step < steps { step } else { 2 * steps - step };
    step as f32 / steps as f32
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// p50, p95, p99 and max in milliseconds; zeros for none.
fn spread(values: &[Duration]) -> [f64; 4] {
    if values.is_empty() {
        return [0.0; 4];
    }
    let mut v: Vec<Duration> = values.to_vec();
    v.sort();
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    [ms(at(0.5)), ms(at(0.95)), ms(at(0.99)), ms(v[v.len() - 1])]
}

fn report(st: &State, scale: f32) {
    let effects = std::env::var("RBS_EFFECTS").unwrap_or_else(|_| "theme".into());
    rbs_crash::outln!("rbs measure: phase=warmup {}", long_tasks(&st.warm_up_long));
    for p in &st.phases {
        let r = spread(&p.renders);
        let i = spread(&p.intervals);
        rbs_crash::outln!(
            "rbs measure: effects={effects} scale={scale} phase={} frames={} meter_writes={} render_ms p50={:.2} p95={:.2} p99={:.2} max={:.2} interval_ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
            p.name,
            p.renders.len(),
            p.meter_writes,
            r[0],
            r[1],
            r[2],
            r[3],
            i[0],
            i[1],
            i[2],
            i[3]
        );
        let input = spread(&p.inputs);
        rbs_crash::outln!(
            "rbs measure: phase={} inputs={} input_ms p50={:.2} p95={:.2} p99={:.2} max={:.2} {}",
            p.name,
            p.inputs.len(),
            input[0],
            input[1],
            input[2],
            input[3],
            long_tasks(&p.long)
        );
    }
}

/// `long_tasks=N long_max_ms=M top=place=count/max_ms,...`: the three places with the
/// most time in long tasks.
fn long_tasks(long: &[(Duration, &'static Location<'static>)]) -> String {
    let max = long.iter().map(|l| l.0).max().unwrap_or_default();
    let mut places: Vec<(&'static Location<'static>, u32, Duration, Duration)> = Vec::new();
    for &(took, at) in long {
        match places.iter_mut().find(|p| p.0 == at) {
            Some(p) => {
                p.1 += 1;
                p.2 += took;
                p.3 = p.3.max(took);
            }
            None => places.push((at, 1, took, took)),
        }
    }
    places.sort_by_key(|p| std::cmp::Reverse(p.2));
    let top: Vec<String> = places
        .iter()
        .take(3)
        .map(|(at, n, _, max)| format!("{}:{}={n}/{:.0}", at.file(), at.line(), ms(*max)))
        .collect();
    format!(
        "long_tasks={} long_max_ms={:.0} top={}",
        long.len(),
        ms(max),
        if top.is_empty() {
            "-".to_owned()
        } else {
            top.join(",")
        }
    )
}

/// Prints how long Quit took on the UI thread (`close_ms`), from the call to its
/// return: every window hidden and the shutdown on its own thread.
pub fn report_close(took: Duration) {
    rbs_crash::outln!("rbs measure: close_ms={:.1}", ms(took));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_takes_quantiles_of_sorted_values() {
        let v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let s = spread(&v);
        assert_eq!(s[3], 100.0);
        assert!((s[0] - 51.0).abs() < 1.0 && (s[1] - 95.0).abs() < 1.0);
        assert_eq!(spread(&[]), [0.0; 4]);
    }

    #[test]
    fn long_tasks_list_the_places_with_the_most_time() {
        let a = Location::caller();
        let b = Location::caller();
        let none = long_tasks(&[]);
        assert_eq!(none, "long_tasks=0 long_max_ms=0 top=-");
        let line = long_tasks(&[
            (Duration::from_millis(60), a),
            (Duration::from_millis(70), a),
            (Duration::from_millis(200), b),
        ]);
        assert!(
            line.starts_with("long_tasks=3 long_max_ms=200 top="),
            "{line}"
        );
        let top = line.split("top=").nth(1).unwrap_or_default();
        let first = format!("{}:{}=1/200", b.file(), b.line());
        let second = format!("{}:{}=2/70", a.file(), a.line());
        assert_eq!(top, format!("{first},{second}"));
    }

    #[test]
    fn bounce_goes_edge_to_edge_and_back() {
        assert_eq!(bounce(0, 4), 0.0);
        assert_eq!(bounce(2, 4), 0.5);
        assert_eq!(bounce(4, 4), 1.0);
        assert_eq!(bounce(6, 4), 0.5);
        assert_eq!(bounce(8, 4), 0.0);
    }
}
