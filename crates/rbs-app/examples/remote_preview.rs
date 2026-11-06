//! UI side of the cross-process preview demo (ADR-0028, ADR-0034, ADR-0036).
//!
//! Shows the engine's preview in a Slint window without copying pixels: the engine
//! process renders into IOSurfaces (macOS) or shared D3D12 textures (Windows) and this
//! process displays the same memory.
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
//!   Animation layer (macOS, ADR-0034) or the buffers of a composition swapchain in a
//!   DirectComposition visual (Windows, ADR-0036) over the preview area. A new frame
//!   changes one layer property or presents one buffer; Slint does not redraw. Where the
//!   layer cannot be made (Windows without a composition swapchain, for example over
//!   remote desktop) the demo says so and falls back to `slint`.
//! - `RBS_PREVIEW_MODE=slint`: the surfaces are aliased as `wgpu` textures and drawn by
//!   Slint's renderer as an `Image` (ADR-0028's first path). Every frame redraws the
//!   window.
//! - `RBS_DEMO_METER_COUNT=<n>`: meters in the strip (default 16).
//! - `RBS_DEMO_METERS=1`: adds a strip of audio meters (scale, peak hold, clip
//!   indicator per `rbs_ui::meter`) updated at 30 Hz, with their accessible values at
//!   1 Hz (the budget scenario of `docs/specs/performance-budgets.md`, "Desktop UI").
//! - `RBS_METER_MODE=layer` (default with the layer preview): the meters' moving parts
//!   are Core Animation layers (ADR-0035); Slint draws only their static parts.
//!   `RBS_METER_MODE=slint` (default with `RBS_PREVIEW_MODE=slint`): Slint draws them, so
//!   every update redraws the window.
//! - `RBS_DEMO_SECONDS=<n>`: after 3 s of warm-up, measures `n` seconds, prints one
//!   `MEASURE` line (shown frames per second, late frames, process CPU, resident memory)
//!   and exits.
//!
//! This is a demo, not the product's Program/Preview panel: wiring the panel to the
//! engine is follow-up work in the shell.
#![allow(clippy::disallowed_methods)] // A demo, not the app shell: ADR-0054 rules the shell.

#[cfg(any(target_os = "macos", windows))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    demo::run()
}

#[cfg(not(any(target_os = "macos", windows)))]
fn main() {
    eprintln!(
        "remote_preview: cross-process preview is implemented on macOS and Windows only (ADR-0028, ADR-0036)"
    );
}

#[cfg(any(target_os = "macos", windows))]
mod demo {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    use no_meter_bridge::{MeterBridgeLayer, MeterGeometry};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    #[cfg(target_os = "macos")]
    use rbs_app::native::{MeterBridgeLayer, MeterColors, MeterGeometry};
    use rbs_core::time::{Clock, MonotonicClock};
    use rbs_gpu::share::{
        LayerRect, PreviewEndpoint, PreviewFrame, PreviewLayer, PreviewSubscriber,
    };
    use rbs_gpu::{GpuContext, GpuRequest};
    use rbs_ui::layout::Rect;
    use rbs_ui::meter::{Meter, MeterView, Orientation};
    use slint::wgpu_30::{WGPUConfiguration, wgpu};
    use slint::{ComponentHandle, Model, VecModel};

    /// Meter strip layout, shared by the Slint markup and the layer geometry.
    const DEFAULT_METERS: usize = 16;
    const STRIP_HEIGHT: f32 = 56.0;
    const STRIP_PAD: f32 = 8.0;
    const METER_GAP: f32 = 4.0;
    const CLIP_HEIGHT: f32 = 6.0;
    const CLIP_GAP: f32 = 2.0;

    slint::slint! {
        // Moving parts in strip coordinates (logical pixels), from MeterView::frames.
        export struct MeterData {
            lo-y: float, lo-h: float, mi-y: float, mi-h: float, hi-y: float, hi-h: float,
            pk-y: float, pk-h: float, pk-zone: int, clipped: bool,
        }
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
            // Slint draws the meters' moving parts (meter mode `slint`).
            in property <bool> draw-meters;
            in property <[MeterData]> levels;
            in property <[string]> meter-values;
            in property <int> meter-count: 16;
            // Where the preview layer goes, in window coordinates.
            out property <length> area-x: area.absolute-position.x;
            out property <length> area-y: area.absolute-position.y;
            out property <length> area-width: area.width;
            out property <length> area-height: area.height;
            out property <length> strip-x: strip.absolute-position.x;
            out property <length> strip-y: strip.absolute-position.y;
            out property <length> strip-width: strip.width;
            out property <length> strip-height: strip.height;
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
                strip := Rectangle {
                    height: root.meters ? 56px : 0px;
                    visible: root.meters;
                    property <length> pad: 8px;
                    property <length> gap: 4px;
                    property <length> clip-h: 6px;
                    property <length> clip-gap: 2px;
                    // Whole points, so the layer's snapped edges and Slint's agree.
                    property <length> meter-w: floor((self.width - 2 * pad - (root.meter-count - 1) * gap) / root.meter-count / 1px) * 1px;
                    property <length> bar-y: pad + clip-h + clip-gap;
                    property <length> bar-h: self.height - bar-y - pad;
                    // Static parts, drawn once: clip indicator outline and bar background.
                    for value[i] in root.meter-values: Rectangle {
                        x: pad + i * (meter-w + gap);
                        y: 0;
                        width: meter-w;
                        height: parent.height;
                        accessible-role: progress-indicator;
                        accessible-label: "Meter " + (i + 1);
                        accessible-value: value;
                        Rectangle {
                            y: pad;
                            height: clip-h;
                            border-width: 1px;
                            border-color: #c38bf0;
                        }
                        Rectangle {
                            y: bar-y;
                            height: bar-h;
                            background: #161a22;
                        }
                    }
                    // Moving parts when Slint draws them: the rectangles of MeterView::frames
                    // (the demo writes the dark theme's token values literally).
                    if root.draw-meters: Rectangle {
                        for m[i] in root.levels: Rectangle {
                            x: pad + i * (meter-w + gap);
                            width: meter-w;
                            Rectangle {
                                y: pad;
                                height: clip-h;
                                visible: m.clipped;
                                background: #c38bf0;
                            }
                            Rectangle { y: m.lo-y * 1px; height: m.lo-h * 1px; background: #3fb950; }
                            Rectangle { y: m.mi-y * 1px; height: m.mi-h * 1px; background: #e3b341; }
                            Rectangle { y: m.hi-y * 1px; height: m.hi-h * 1px; background: #f0883e; }
                            Rectangle {
                                visible: m.pk-h > 0;
                                y: m.pk-y * 1px;
                                height: m.pk-h * 1px;
                                background: m.pk-zone == 2 ? #f0883e : m.pk-zone == 1 ? #e3b341 : #3fb950;
                            }
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
        /// Retires at the oldest waiting frame's due time (layer) or polls GPU completion
        /// (slint).
        timer: slint::Timer,
        clock: MonotonicClock,
        /// Meter mode `layer`: the meters' moving parts, attached at the first redraw.
        bridge: Option<MeterBridgeLayer>,
        bridge_attached: bool,
        /// The bridge's meter geometry, one per meter, reused on every redraw.
        meter_geometry: Vec<MeterGeometry>,
    }

    /// Width of one meter in whole points (the markup floors the same way).
    fn meter_width(strip_width: f32, count: usize) -> f32 {
        let n = count as f32;
        ((strip_width - 2.0 * STRIP_PAD - (n - 1.0) * METER_GAP) / n).floor()
    }

    /// A meter's bar in strip coordinates, `x` from the left (width filled in later).
    fn bar_rect(x: f32) -> Rect {
        let y = STRIP_PAD + CLIP_HEIGHT + CLIP_GAP;
        Rect {
            x,
            y,
            w: 1.0,
            h: STRIP_HEIGHT - y - STRIP_PAD,
        }
    }

    /// The moving parts for Slint, from the same frames the layer draws.
    fn meter_data(v: &MeterView, bar: &Rect) -> MeterData {
        let f = v.frames(*bar, Orientation::Vertical);
        let peak = f.peak.unwrap_or_default();
        MeterData {
            lo_y: f.zones[0].y,
            lo_h: f.zones[0].h,
            mi_y: f.zones[1].y,
            mi_h: f.zones[1].h,
            hi_y: f.zones[2].y,
            hi_h: f.zones[2].h,
            pk_y: peak.y,
            pk_h: peak.h,
            pk_zone: i32::try_from(f.peak_zone).unwrap_or(0),
            clipped: v.clipped,
        }
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
            .map(|d| d.join(format!("preview_engine{}", std::env::consts::EXE_SUFFIX)))
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
    #[cfg(windows)]
    fn cpu_time() -> Duration {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let mut t = [FILETIME::default(); 4];
        // SAFETY: GetProcessTimes fills the four FILETIMEs it is given for our own process.
        let ok = unsafe {
            let [create, exit, kernel, user] = &mut t;
            GetProcessTimes(GetCurrentProcess(), create, exit, kernel, user).is_ok()
        };
        if !ok {
            return Duration::ZERO;
        }
        // 100 ns units.
        let ft = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
        Duration::from_nanos((ft(t[2]) + ft(t[3])) * 100)
    }

    /// Peak working set in MiB.
    #[cfg(windows)]
    fn peak_rss_mib() -> f64 {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        let mut c = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        // SAFETY: fills the counters struct of the given size for our own process.
        let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb).is_ok() };
        if ok {
            c.PeakWorkingSetSize as f64 / (1024.0 * 1024.0)
        } else {
            0.0
        }
    }

    /// Process CPU time (user + system) so far.
    #[cfg(target_os = "macos")]
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
    #[cfg(target_os = "macos")]
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
        let meter_count = std::env::var("RBS_DEMO_METER_COUNT")
            .ok()
            .map(|v| v.parse::<usize>())
            .transpose()?
            .unwrap_or(DEFAULT_METERS)
            .max(1);
        let meter_mode = match std::env::var("RBS_METER_MODE").as_deref() {
            Ok("slint") => Mode::Slint,
            Ok("layer") => Mode::Layer,
            Err(_) => mode,
            Ok(other) => return Err(format!("unknown RBS_METER_MODE {other}").into()),
        };
        // The meter bridge is macOS only (ADR-0035); Slint draws the meters elsewhere.
        let meter_mode = if cfg!(windows) {
            Mode::Slint
        } else {
            meter_mode
        };
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
        #[cfg(target_os = "macos")]
        let ids = format!(
            "IOSurfaces {}",
            (0..sub.surfaces())
                .filter_map(|s| sub.surface_id(s))
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        #[cfg(windows)]
        let ids = format!("{} D3D12 textures", sub.surfaces());

        // The layer needs the subscriber's surfaces; the receiver thread needs the
        // subscriber. A second subscriber is not allowed, so the layer is made here and
        // attached at the first redraw.
        let mut mode = mode;
        let layer = match mode {
            Mode::Layer => {
                #[cfg(target_os = "macos")]
                let made: Result<PreviewLayer, rbs_gpu::GpuError> = Ok(PreviewLayer::new(&sub));
                #[cfg(windows)]
                let made = PreviewLayer::new(&sub);
                match made {
                    Ok(layer) => {
                        let needed = layer.surfaces_needed(Duration::from_nanos(16_666_667));
                        if layer.surfaces() < needed {
                            eprintln!(
                                "remote_preview: the engine's ring has {} surfaces, the layer needs {needed} at 60 fps; frames will be dropped",
                                layer.surfaces()
                            );
                        }
                        Some(layer)
                    }
                    Err(e) => {
                        eprintln!(
                            "remote_preview: no preview layer ({e}); Slint draws the preview"
                        );
                        mode = Mode::Slint;
                        None
                    }
                }
            }
            Mode::Slint => None,
        };
        eprintln!(
            "remote_preview: shared {ids} (no copies), mode {}",
            match mode {
                Mode::Layer => "layer",
                Mode::Slint => "slint",
            }
        );

        let window = PreviewWindow::new()?;
        window.set_draw_frame(mode == Mode::Slint);
        window.set_meters(meters);
        window.set_draw_meters(meters && meter_mode == Mode::Slint);
        #[cfg(windows)]
        let bridge: Option<MeterBridgeLayer> = None;
        #[cfg(target_os = "macos")]
        let bridge = if meters && meter_mode == Mode::Layer {
            let tokens = rbs_ui::tokens::Tokens::built_in()?;
            let colors =
                MeterColors::from_tokens(&tokens, "dark").ok_or("no meter colours in tokens")?;
            Some(MeterBridgeLayer::new(&colors))
        } else {
            None
        };
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
                bridge,
                bridge_attached: false,
                meter_geometry: vec![
                    MeterGeometry {
                        bar: Rect::default(),
                        clip: Rect::default(),
                        orientation: Orientation::Vertical,
                    };
                    meter_count
                ],
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
                slint::RenderingState::BeforeRendering => {
                    place_layer(&window, &attached);
                    place_meters(&window);
                }
                slint::RenderingState::AfterRendering => after_rendering(&queue),
                _ => {}
            }
        })?;

        // Meters updated at 30 Hz, accessible values (the period's peak) at 1 Hz, only
        // when asked for. Levels are a made-up signal from -48 to +2 dBFS (the clip latches).
        let count = meter_count;
        window.set_meter_count(i32::try_from(count)?);
        let silent = meter_data(&MeterView::SILENT, &bar_rect(0.0));
        let levels = Rc::new(VecModel::from(vec![silent; count]));
        let values = Rc::new(VecModel::from(vec![
            slint::SharedString::from("silent");
            if meters { count } else { 0 }
        ]));
        window.set_levels(levels.clone().into());
        window.set_meter_values(values.clone().into());
        let meter_timer = slint::Timer::default();
        if meters {
            let mut tick = 0u32;
            let start = Instant::now();
            let mut ballistics = vec![Meter::new(); count];
            let mut views = vec![MeterView::SILENT; count];
            let weak = window.as_weak();
            meter_timer.start(
                slint::TimerMode::Repeated,
                Duration::from_millis(33),
                move || {
                    tick = tick.wrapping_add(1);
                    let now = start.elapsed();
                    for (i, (m, v)) in ballistics.iter_mut().zip(&mut views).enumerate() {
                        let phase = (tick as f32) * 0.21 + (i as f32) * 0.7;
                        m.update(-23.0 + 25.0 * phase.sin(), now);
                        *v = m.view(now);
                    }
                    let drawn = STATE.with(|s| {
                        s.borrow_mut()
                            .as_mut()
                            .and_then(|st| st.bridge.as_mut())
                            .map(|b| b.show(&views))
                            .is_some()
                    });
                    if !drawn && let Some(w) = weak.upgrade() {
                        let meter_w = meter_width(w.get_strip_width(), count);
                        for (i, v) in views.iter().enumerate() {
                            let x = STRIP_PAD + i as f32 * (meter_w + METER_GAP);
                            levels.set_row_data(i, meter_data(v, &bar_rect(x)));
                        }
                    }
                    if tick.is_multiple_of(30) {
                        for (i, m) in ballistics.iter_mut().enumerate() {
                            values.set_row_data(i, m.take_report().english().into());
                        }
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
                            "MEASURE mode={} meters={} meter_mode={} seconds={wall:.1} fps={:.1} late={late} cpu_pct={:.1} peak_rss_mib={:.0}",
                            match mode {
                                Mode::Layer => "layer",
                                Mode::Slint => "slint",
                            },
                            u8::from(meters),
                            match meter_mode {
                                Mode::Layer => "layer",
                                Mode::Slint => "slint",
                            },
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
                #[cfg(target_os = "macos")]
                let attach = {
                    let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                        return;
                    };
                    // SAFETY: the handle is the live NSView of this window, which the Metal
                    // renderer made layer-backed; rendering notifiers run on the main thread.
                    unsafe { layer.attach_to_ns_view(appkit.ns_view) }
                };
                #[cfg(windows)]
                let attach = {
                    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
                        return;
                    };
                    // SAFETY: the handle is the live top-level window of this process;
                    // rendering notifiers run on the thread that owns it.
                    unsafe { layer.attach_to_hwnd(win32.hwnd) }
                };
                if let Err(e) = attach {
                    eprintln!("remote_preview: cannot attach the preview layer: {e}");
                    return;
                }
                attached.set(true);
            }
            // Core Animation places in points (Slint's logical pixels); DirectComposition
            // in physical pixels.
            #[cfg(target_os = "macos")]
            let scale = 1.0;
            #[cfg(windows)]
            let scale = f64::from(window.window().scale_factor());
            layer.set_rect(LayerRect {
                x: f64::from(window.get_area_x()) * scale,
                y: f64::from(window.get_area_y()) * scale,
                width: f64::from(window.get_area_width()) * scale,
                height: f64::from(window.get_area_height()) * scale,
            });
        });
    }

    /// Meter mode `layer`: attaches the meter bridge once the view exists and keeps it
    /// over the strip, with each meter where the Slint markup draws its static parts.
    fn place_meters(window: &PreviewWindow) {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(st) = s.as_mut() else {
                return;
            };
            let Some(bridge) = st.bridge.as_mut() else {
                return;
            };
            if !st.bridge_attached {
                // Without a view the meters fall back to Slint for good.
                let fallback = |st: &mut State| {
                    st.bridge = None;
                    window.set_draw_meters(true);
                };
                let handle = window.window().window_handle();
                let Ok(handle) = handle.window_handle() else {
                    return;
                };
                let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                    fallback(st);
                    return;
                };
                // SAFETY: as in `place_layer`.
                if let Err(e) = unsafe { bridge.attach_to_ns_view(appkit.ns_view) } {
                    eprintln!(
                        "remote_preview: cannot attach the meter layer, Slint draws the meters: {e}"
                    );
                    fallback(st);
                    return;
                }
                st.bridge_attached = true;
            }
            let width = window.get_strip_width();
            bridge.set_rect(Rect {
                x: window.get_strip_x(),
                y: window.get_strip_y(),
                w: width,
                h: window.get_strip_height(),
            });
            // Same arithmetic as the markup; set_meters does nothing when unchanged.
            let count = st.meter_geometry.len();
            let meter_w = meter_width(width, count);
            for (i, g) in st.meter_geometry.iter_mut().enumerate() {
                let x = STRIP_PAD + i as f32 * (meter_w + METER_GAP);
                g.bar = Rect {
                    w: meter_w,
                    ..bar_rect(x)
                };
                g.clip = Rect {
                    x,
                    y: STRIP_PAD,
                    w: meter_w,
                    h: CLIP_HEIGHT,
                };
            }
            bridge.set_meters(&st.meter_geometry);
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
                        "Preview from engine process: {fps:.0} fps, {} shared, no copies",
                        st.ids
                    )
                    .into(),
                );
            }
        });
        retire();
    }

    /// Releases replaced frames that nothing reads any more. Never waits. While frames
    /// wait, a one-shot timer calls it again at the oldest one's due time (layer) or in
    /// 2 ms (slint).
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

    /// Windows has no meter bridge yet (ADR-0035 is macOS only): these types keep the
    /// demo's shared code compiling, and the bridge is never made.
    #[cfg(windows)]
    mod no_meter_bridge {
        use rbs_ui::layout::Rect;
        use rbs_ui::meter::{MeterView, Orientation};

        #[derive(Clone)]
        #[allow(
            dead_code,
            reason = "mirrors the macOS type; nothing reads it on Windows"
        )]
        pub struct MeterGeometry {
            pub bar: Rect,
            pub clip: Rect,
            pub orientation: Orientation,
        }

        /// Uninhabited: never constructed on Windows.
        pub enum MeterBridgeLayer {}

        impl MeterBridgeLayer {
            pub fn show(&mut self, _: &[MeterView]) {
                match *self {}
            }
            pub fn set_rect(&mut self, _: Rect) {
                match *self {}
            }
            pub fn set_meters(&mut self, _: &[MeterGeometry]) {
                match *self {}
            }
            /// # Safety
            /// Never callable: the type has no values.
            pub unsafe fn attach_to_ns_view(
                &mut self,
                _: std::ptr::NonNull<std::ffi::c_void>,
            ) -> Result<(), String> {
                match *self {}
            }
        }
    }
}
