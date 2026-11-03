//! UI side of the cross-process preview demo (ADR-0028, ADR-0034).
//!
//! Shows the engine's preview in a Slint window without copying pixels: the engine
//! process renders into IOSurfaces and this process displays the same IOSurfaces.
//!
//! ```sh
//! cargo build -p rbs-gpu --release --example preview_engine
//! cargo run -p rbs-app --release --example remote_preview
//! ```
//!
//! Without arguments it starts `preview_engine` from the same target directory and
//! reads the endpoint from its first line. With an argument `service#token` it connects
//! to an engine that is already running.
//!
//! Environment:
//!
//! - `RBS_PREVIEW_MODE=layer` (default): the surfaces are the contents of a Core
//!   Animation layer over the preview area (ADR-0034). A new frame changes one layer
//!   property; Slint does not redraw.
//! - `RBS_PREVIEW_MODE=slint`: the surfaces are aliased as `wgpu` textures and drawn by
//!   Slint's renderer as an `Image` (ADR-0028's first path). Every frame redraws the
//!   window.
//! - `RBS_DEMO_METERS=1`: adds a strip of 16 audio-style meters that Slint animates at
//!   30 Hz (the budget scenario of `docs/specs/performance-budgets.md`, "Desktop UI").
//! - `RBS_DEMO_SECONDS=<n>`: after 3 s of warm-up, measures `n` seconds, prints one
//!   `MEASURE` line (shown frames per second, late frames, process CPU, resident memory)
//!   and exits.
//!
//! This is a demo, not the product's Program/Preview panel: wiring the panel to the
//! engine is follow-up work in the shell.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    demo::run()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("remote_preview: cross-process preview is implemented on macOS only (ADR-0028)");
}

#[cfg(target_os = "macos")]
mod demo {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use rbs_core::time::{Clock, MonotonicClock};
    use rbs_gpu::share::{
        LayerRect, PreviewEndpoint, PreviewFrame, PreviewLayer, PreviewSubscriber,
    };
    use rbs_gpu::{GpuContext, GpuRequest};
    use slint::wgpu_30::{WGPUConfiguration, wgpu};
    use slint::{ComponentHandle, Model, VecModel};

    slint::slint! {
        export component PreviewWindow inherits Window {
            title: "RBS remote preview";
            preferred-width: 1280px;
            preferred-height: 760px;
            background: #0b0d12;
            in property <image> frame;
            // Slint draws the frame (mode `slint`); otherwise a layer covers the area.
            in property <bool> draw-frame;
            in property <string> status;
            in property <bool> meters;
            in property <[float]> levels;
            // Where the preview layer goes, in window coordinates.
            out property <length> area-x: area.absolute-position.x;
            out property <length> area-y: area.absolute-position.y;
            out property <length> area-width: area.width;
            out property <length> area-height: area.height;
            VerticalLayout {
                area := Rectangle {
                    background: #000000;
                    accessible-role: image;
                    accessible-label: "Preview";
                    if root.draw-frame: Image {
                        width: parent.width;
                        height: parent.height;
                        source: root.frame;
                        image-fit: contain;
                    }
                }
                if root.meters: HorizontalLayout {
                    height: 56px;
                    padding: 8px;
                    spacing: 4px;
                    for level in root.levels: Rectangle {
                        background: #1c2028;
                        Rectangle {
                            y: parent.height * (1 - level);
                            height: parent.height * level;
                            background: level > 0.85 ? #e5534b : #3fb950;
                        }
                    }
                }
                Text {
                    text: root.status;
                    color: #c8ccd4;
                    height: 40px;
                    vertical-alignment: center;
                    horizontal-alignment: center;
                }
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Layer,
        Slint,
    }

    /// Frame counters for the status line and the measurement.
    struct Counters {
        frames: u64,
        since: Instant,
        last: Option<Instant>,
        /// Since the measurement started: frames and intervals over 25 ms.
        measured_frames: u64,
        late: u64,
    }

    impl Counters {
        fn frame(&mut self) {
            let now = Instant::now();
            if let Some(last) = self.last.replace(now)
                && now.duration_since(last) > Duration::from_millis(25)
            {
                self.late += 1;
            }
            self.frames += 1;
            self.measured_frames += 1;
        }
    }

    /// Per-window state, touched only on the Slint event loop thread.
    struct State {
        mode: Mode,
        counters: Counters,
        ids: String,
        /// Mode `layer`, created at the first redraw (the window's view exists then).
        layer: Option<PreviewLayer>,
        /// Mode `slint`.
        slint: Option<SlintPath>,
        /// Polls retirement while it cannot wait for the next frame.
        timer: slint::Timer,
        clock: MonotonicClock,
    }

    /// Mode `slint`: frames are wgpu textures that Slint draws.
    struct SlintPath {
        images: Vec<slint::Image>,
        /// The frame on screen.
        shown: Option<PreviewFrame>,
        /// Replaced frames whose last draw has not been submitted yet.
        replaced: Vec<PreviewFrame>,
        /// Replaced frames and the submission after their last draw.
        retiring: VecDeque<(PreviewFrame, wgpu::SubmissionIndex)>,
        device: wgpu::Device,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    /// Starts `preview_engine` next to this example and reads its endpoint.
    fn spawn_engine() -> Result<(Child, PreviewEndpoint), Box<dyn std::error::Error>> {
        let exe = std::env::current_exe()?;
        let engine = exe
            .parent()
            .map(|d| d.join("preview_engine"))
            .ok_or("no example directory")?;
        let mut child = Command::new(&engine)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "cannot start {}: {e} (build it with `cargo build -p rbs-gpu --example preview_engine` in the same profile)",
                    engine.display()
                )
            })?;
        let stdout = child.stdout.take().ok_or("no engine stdout")?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line)?;
        let endpoint = line.trim().parse::<PreviewEndpoint>()?;
        Ok((child, endpoint))
    }

    /// Process CPU time (user + system) so far.
    fn cpu_time() -> Duration {
        // SAFETY: getrusage fills the zeroed struct it is given; RUSAGE_SELF is valid.
        let usage = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            usage
        };
        let tv = |t: libc::timeval| {
            Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
        };
        tv(usage.ru_utime) + tv(usage.ru_stime)
    }

    /// Peak resident memory in MiB (macOS reports bytes).
    fn peak_rss_mib() -> f64 {
        // SAFETY: as in `cpu_time`.
        let usage = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            usage
        };
        usage.ru_maxrss as f64 / (1024.0 * 1024.0)
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let mode = match std::env::var("RBS_PREVIEW_MODE").as_deref() {
            Ok("slint") => Mode::Slint,
            Ok("layer") | Err(_) => Mode::Layer,
            Ok(other) => return Err(format!("unknown RBS_PREVIEW_MODE {other}").into()),
        };
        let meters = std::env::var("RBS_DEMO_METERS").is_ok_and(|v| v == "1");
        let measure = std::env::var("RBS_DEMO_SECONDS")
            .ok()
            .map(|v| v.parse::<u64>())
            .transpose()?;

        let (engine, endpoint) = match std::env::args().nth(1) {
            Some(arg) => (None, arg.parse::<PreviewEndpoint>()?),
            None => {
                let (child, endpoint) = spawn_engine()?;
                (Some(child), endpoint)
            }
        };

        // One device for this process: rbs-gpu creates it, Slint renders with it (the
        // `slint` mode draws textures of this device).
        let ctx = GpuContext::new(&GpuRequest::default())?;
        slint::BackendSelector::new()
            .require_wgpu_30(WGPUConfiguration::Manual {
                instance: ctx.instance().clone(),
                adapter: ctx.adapter().clone(),
                device: ctx.device().clone(),
                queue: ctx.queue().clone(),
            })
            .select()?;

        let mut sub = PreviewSubscriber::connect(&endpoint, Duration::from_secs(5))?;
        let ids = (0..sub.surfaces())
            .filter_map(|s| sub.surface_id(s))
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!(
            "remote_preview: shared IOSurfaces {ids} (no copies), mode {}",
            match mode {
                Mode::Layer => "layer",
                Mode::Slint => "slint",
            }
        );

        let window = PreviewWindow::new()?;
        window.set_draw_frame(mode == Mode::Slint);
        window.set_meters(meters);
        let slint_path = match mode {
            Mode::Slint => {
                let images = sub
                    .textures(&ctx)?
                    .into_iter()
                    .map(slint::Image::try_from)
                    .collect::<Result<Vec<_>, _>>()?;
                Some(SlintPath {
                    images,
                    shown: None,
                    replaced: Vec::new(),
                    retiring: VecDeque::new(),
                    device: ctx.device().clone(),
                })
            }
            Mode::Layer => None,
        };
        // The layer needs the subscriber's surfaces; the receiver thread needs the
        // subscriber. A second subscriber is not allowed, so the layer is made here and
        // attached at the first redraw.
        let layer = match mode {
            Mode::Layer => {
                let layer = PreviewLayer::new(&sub);
                let needed = layer.surfaces_needed(Duration::from_nanos(16_666_667));
                if layer.surfaces() < needed {
                    eprintln!(
                        "remote_preview: the engine's ring has {} surfaces, the layer needs {needed} at 60 fps; frames will be dropped",
                        layer.surfaces()
                    );
                }
                Some(layer)
            }
            Mode::Slint => None,
        };
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                mode,
                counters: Counters {
                    frames: 0,
                    since: Instant::now(),
                    last: None,
                    measured_frames: 0,
                    late: 0,
                },
                ids,
                layer,
                slint: slint_path,
                timer: slint::Timer::default(),
                clock: MonotonicClock::new(),
            });
        });

        let queue = ctx.queue().clone();
        let weak = window.as_weak();
        let attached = Rc::new(std::cell::Cell::new(false));
        window.window().set_rendering_notifier(move |state, _| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            match state {
                slint::RenderingState::BeforeRendering => place_layer(&window, &attached),
                slint::RenderingState::AfterRendering => after_rendering(&queue),
                _ => {}
            }
        })?;

        // Meters: 16 bars that move at 30 Hz, only when asked for.
        let levels = Rc::new(VecModel::from(vec![0.0f32; 16]));
        window.set_levels(levels.clone().into());
        let meter_timer = slint::Timer::default();
        if meters {
            let mut tick = 0u32;
            meter_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(33),
                move || {
                    tick = tick.wrapping_add(1);
                    for i in 0..levels.row_count() {
                        let phase = (tick as f32) * 0.21 + (i as f32) * 0.7;
                        levels.set_row_data(i, 0.5 + 0.45 * phase.sin());
                    }
                },
            );
        }

        // Measurement: warm up, then read CPU time over the window and quit.
        let measure_timer = slint::Timer::default();
        if let Some(secs) = measure {
            let weak = window.as_weak();
            let mut start: Option<(Instant, Duration)> = None;
            measure_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_secs(3),
                move || match start {
                    None => {
                        STATE.with(|s| {
                            if let Some(st) = s.borrow_mut().as_mut() {
                                st.counters.measured_frames = 0;
                                st.counters.late = 0;
                            }
                        });
                        start = Some((Instant::now(), cpu_time()));
                    }
                    Some((t0, c0)) if t0.elapsed() >= Duration::from_secs(secs) => {
                        let wall = t0.elapsed().as_secs_f64();
                        let cpu = (cpu_time() - c0).as_secs_f64();
                        let (frames, late) = STATE.with(|s| {
                            s.borrow()
                                .as_ref()
                                .map_or((0, 0), |st| (st.counters.measured_frames, st.counters.late))
                        });
                        println!(
                            "MEASURE mode={} meters={} seconds={wall:.1} fps={:.1} late={late} cpu_pct={:.1} peak_rss_mib={:.0}",
                            match mode {
                                Mode::Layer => "layer",
                                Mode::Slint => "slint",
                            },
                            u8::from(meters),
                            frames as f64 / wall,
                            100.0 * cpu / wall,
                            peak_rss_mib(),
                        );
                        if let Some(w) = weak.upgrade() {
                            let _ = w.hide();
                        }
                        let _ = slint::quit_event_loop();
                    }
                    Some(_) => {}
                },
            );
        }

        // Receives frames off the UI thread; only the latest is shown.
        let weak = window.as_weak();
        let receiver = std::thread::spawn(move || {
            loop {
                match sub.recv(Duration::from_millis(250)) {
                    Ok(Some(frame)) => {
                        let weak = weak.clone();
                        let ok = slint::invoke_from_event_loop(move || show(&weak, frame));
                        if ok.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let weak = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = weak.upgrade() {
                                w.set_status(format!("Engine disconnected: {e}").into());
                            }
                        });
                        break;
                    }
                }
            }
        });

        window.run()?;
        // The last draws must be done before the frames go back.
        ctx.wait_idle()?;
        // Frames must go back before the subscriber's thread ends; dropping the state
        // releases them (the layer leaves the window first).
        STATE.with(|s| s.borrow_mut().take());
        drop(engine); // stdin closes, the engine stops
        drop(receiver); // detached; it ends with the process
        Ok(())
    }

    /// Mode `layer`: attaches the layer once the view exists and keeps it over the
    /// preview area. Runs before every Slint redraw, which every layout change causes.
    fn place_layer(window: &PreviewWindow, attached: &std::cell::Cell<bool>) {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(st) = s.as_mut() else {
                return;
            };
            let Some(layer) = st.layer.as_mut() else {
                return;
            };
            if !attached.get() {
                let handle = window.window().window_handle();
                let Ok(handle) = handle.window_handle() else {
                    return;
                };
                let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                    return;
                };
                // SAFETY: the handle is the live NSView of this window, which the Metal
                // renderer made layer-backed; rendering notifiers run on the main thread.
                if let Err(e) = unsafe { layer.attach_to_ns_view(appkit.ns_view) } {
                    eprintln!("remote_preview: cannot attach the preview layer: {e}");
                    return;
                }
                attached.set(true);
            }
            layer.set_rect(LayerRect {
                x: f64::from(window.get_area_x()),
                y: f64::from(window.get_area_y()),
                width: f64::from(window.get_area_width()),
                height: f64::from(window.get_area_height()),
            });
        });
    }

    /// Mode `slint`: after each render, the frames replaced before it can go back once
    /// its GPU work is done: an empty submission marks the end of that render.
    fn after_rendering(queue: &wgpu::Queue) {
        STATE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut()
                && let Some(p) = st.slint.as_mut()
                && !p.replaced.is_empty()
            {
                let index = queue.submit(std::iter::empty());
                for f in p.replaced.drain(..) {
                    p.retiring.push_back((f, index.clone()));
                }
            }
        });
        retire();
    }

    fn show(weak: &slint::Weak<PreviewWindow>, frame: PreviewFrame) {
        let Some(window) = weak.upgrade() else {
            return;
        };
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(st) = s.as_mut() else {
                return;
            };
            match st.mode {
                Mode::Layer => {
                    if let Some(layer) = st.layer.as_mut() {
                        layer.show(frame, st.clock.now());
                    }
                }
                Mode::Slint => {
                    if let Some(p) = st.slint.as_mut()
                        && let Some(image) = p.images.get(frame.slot())
                    {
                        window.set_frame(image.clone());
                        if let Some(old) = p.shown.replace(frame) {
                            p.replaced.push(old);
                        }
                    }
                }
            }
            st.counters.frame();
            let dt = st.counters.since.elapsed().as_secs_f64();
            if dt >= 1.0 {
                let fps = st.counters.frames as f64 / dt;
                st.counters.frames = 0;
                st.counters.since = Instant::now();
                window.set_status(
                    format!(
                        "Preview from engine process: {fps:.0} fps, IOSurfaces {} shared, no copies",
                        st.ids
                    )
                    .into(),
                );
            }
        });
        retire();
    }

    /// Releases replaced frames that nothing reads any more. Never waits. Polls on a
    /// timer only while the engine may have no free surface (so no new frame would come
    /// to trigger it).
    fn retire() {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(st) = s.as_mut() else {
                return;
            };
            let now = st.clock.now();
            let poll = match (st.layer.as_mut(), st.slint.as_mut()) {
                (Some(layer), _) => {
                    layer.retire(now);
                    // At the oldest frame's retire time: waiting for the next frame would
                    // hold surfaces up to a frame longer than the rule asks.
                    layer
                        .next_retire()
                        .map(|due| due.saturating_since(now).max(Duration::from_millis(1)))
                }
                (None, Some(p)) => {
                    while let Some((_, index)) = p.retiring.front() {
                        let done = p
                            .device
                            .poll(wgpu::PollType::Wait {
                                submission_index: Some(index.clone()),
                                timeout: Some(Duration::ZERO),
                            })
                            .is_ok();
                        if !done {
                            break;
                        }
                        // Dropping the frame sends its release to the engine.
                        p.retiring.pop_front();
                    }
                    (!p.retiring.is_empty()).then_some(Duration::from_millis(2))
                }
                (None, None) => None,
            };
            match poll {
                None => st.timer.stop(),
                Some(after) => {
                    if !st.timer.running() {
                        st.timer.start(slint::TimerMode::SingleShot, after, retire);
                    }
                }
            }
        });
    }
}
