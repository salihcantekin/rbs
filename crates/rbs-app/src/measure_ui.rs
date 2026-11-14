//! The UI measure mode (ADR-0120 decision 13): `RBS_MEASURE_UI=<seconds>` measures the
//! main window as the user sees it and quits.
//!
//! After 3 s of warm-up it runs three phases of `<seconds>` each and prints one line per
//! phase:
//!
//! - **idle**: nothing is sent; frames counted here are redraws nobody asked for;
//! - **hover**: the pointer sweeps across the toolbar row and back, one step per tick, so
//!   every button's hover fill changes;
//! - **resize**: the window grows and shrinks by up to 200 logical pixels, one step per
//!   tick.
//!
//! For each phase: frames drawn, and the render time (Slint's before- to after-rendering
//! notification, the work a theme's effects add) and the frame interval as p50, p95, p99
//! and max in milliseconds. `RBS_EFFECTS=<calm|vivid|tactile>` picks the effects of both
//! slots for the run (`look`), so the same build measures each setting. Process CPU and
//! GPU memory are read from the OS tools while it runs (Activity Monitor or `footprint`
//! on macOS, Task Manager on Windows).
//!
//! Zero cost when unused (P13): without the variable nothing here is installed.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::ComponentHandle;
use slint::platform::WindowEvent;

const WARM_UP: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(16);
/// Pointer steps across the toolbar, and the y of the sweep (logical pixels from the
/// top: the middle of the toolbar row).
const HOVER_STEPS: u32 = 120;
const HOVER_Y: f32 = 28.0;
const RESIZE_REACH: f32 = 200.0;
const RESIZE_STEPS: u32 = 60;

#[derive(Default)]
struct Phase {
    name: &'static str,
    renders: Vec<Duration>,
    intervals: Vec<Duration>,
}

#[derive(Default)]
struct State {
    phases: Vec<Phase>,
    started: Option<Instant>,
    last_frame: Option<Instant>,
    render_start: Option<Instant>,
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
    {
        let state = state.clone();
        let installed = crate::entry::set_rendering_notifier(main.window(), move |s, _| {
            let now = Instant::now();
            let mut st = state.borrow_mut();
            match s {
                slint::RenderingState::BeforeRendering => st.render_start = Some(now),
                slint::RenderingState::AfterRendering => {
                    let render = st.render_start.take().map(|t| now - t);
                    let interval = st.last_frame.replace(now).map(|t| now - t);
                    if let Some(p) = st.phases.last_mut() {
                        if let Some(r) = render {
                            p.renders.push(r);
                        }
                        if let Some(i) = interval {
                            p.intervals.push(i);
                        }
                    }
                }
                _ => {}
            }
        });
        if let Err(e) = installed {
            eprintln!("rbs measure: this renderer gives no frame notifications: {e:?}");
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
        let names = ["idle", "hover", "resize"];
        if index >= names.len() {
            if let Some(size) = base_size {
                window.set_size(size);
            }
            report(&st.phases, scale);
            let _ = slint::quit_event_loop();
            return;
        }
        if st.phases.len() <= index {
            st.phases.push(Phase {
                name: names[index],
                ..Phase::default()
            });
            st.last_frame = None;
            tick = 0;
        }
        tick += 1;
        let size = window.size().to_logical(scale);
        match names[index] {
            "hover" => {
                let step = tick % (2 * HOVER_STEPS);
                let step = if step < HOVER_STEPS {
                    step
                } else {
                    2 * HOVER_STEPS - step
                };
                let x = size.width * step as f32 / HOVER_STEPS as f32;
                window.dispatch_event(WindowEvent::PointerMoved {
                    position: slint::LogicalPosition::new(x, HOVER_Y),
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
                window.set_size(slint::LogicalSize::new(
                    base.width + grow,
                    base.height + grow / 2.0,
                ));
            }
            _ => {}
        }
    });
    // The timer lives as long as the event loop.
    std::mem::forget(timer);
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

fn report(phases: &[Phase], scale: f32) {
    let effects = std::env::var("RBS_EFFECTS").unwrap_or_else(|_| "theme".into());
    for p in phases {
        let r = spread(&p.renders);
        let i = spread(&p.intervals);
        println!(
            "rbs measure: effects={effects} scale={scale} phase={} frames={} render_ms p50={:.2} p95={:.2} p99={:.2} max={:.2} interval_ms p50={:.2} p95={:.2} p99={:.2} max={:.2}",
            p.name,
            p.renders.len(),
            r[0],
            r[1],
            r[2],
            r[3],
            i[0],
            i[1],
            i[2],
            i[3]
        );
    }
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
}
