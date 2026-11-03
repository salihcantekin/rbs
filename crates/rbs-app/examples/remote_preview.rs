//! UI side of the cross-process preview demo (ADR-0028, ADR-0022 acceptance gate).
//!
//! Shows the engine's preview in a Slint window without copying pixels: the engine
//! process renders into IOSurfaces, this process aliases the same IOSurfaces as `wgpu`
//! textures on its own device (which it hands to Slint) and displays them.
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
//! This is a gate demo, not the product's Program/Preview panel: wiring the panel to the
//! engine is follow-up work in the shell.

#[cfg(target_vendor = "apple")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    demo::run()
}

#[cfg(not(target_vendor = "apple"))]
fn main() {
    eprintln!("remote_preview: cross-process preview is implemented on macOS only (ADR-0028)");
}

#[cfg(target_vendor = "apple")]
mod demo {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use rbs_gpu::share::{PreviewEndpoint, PreviewFrame, PreviewSubscriber};
    use rbs_gpu::{GpuContext, GpuRequest};
    use slint::ComponentHandle;
    use slint::wgpu_30::{WGPUConfiguration, wgpu};

    slint::slint! {
        export component PreviewWindow inherits Window {
            title: "RBS remote preview";
            preferred-width: 1280px;
            preferred-height: 760px;
            background: #0b0d12;
            in property <image> frame;
            in property <string> status;
            VerticalLayout {
                Image {
                    source: root.frame;
                    image-fit: contain;
                    accessible-role: image;
                    accessible-label: "Preview";
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

    /// Per-window state, touched only on the Slint event loop thread.
    struct State {
        images: Vec<slint::Image>,
        /// The frame on screen.
        shown: Option<PreviewFrame>,
        /// Replaced frames whose last draw has not been submitted yet.
        replaced: Vec<PreviewFrame>,
        /// Replaced frames and the submission after their last draw.
        retiring: VecDeque<(PreviewFrame, wgpu::SubmissionIndex)>,
        /// Polls `retiring` while it is not empty, so frames go back even when nothing
        /// redraws (no new frame can arrive while every surface is held).
        timer: slint::Timer,
        device: wgpu::Device,
        frames: u64,
        since: Instant,
        fps: f64,
        ids: String,
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

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let (engine, endpoint) = match std::env::args().nth(1) {
            Some(arg) => (None, arg.parse::<PreviewEndpoint>()?),
            None => {
                let (child, endpoint) = spawn_engine()?;
                (Some(child), endpoint)
            }
        };

        // One device for this process: rbs-gpu creates it, Slint renders with it.
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
        let textures = sub.textures(&ctx)?;
        let images = textures
            .into_iter()
            .map(slint::Image::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let ids = (0..sub.surfaces())
            .filter_map(|s| sub.surface_id(s))
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("remote_preview: imported IOSurfaces {ids} (no copies)");

        let window = PreviewWindow::new()?;
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                images,
                shown: None,
                replaced: Vec::new(),
                retiring: VecDeque::new(),
                timer: slint::Timer::default(),
                device: ctx.device().clone(),
                frames: 0,
                since: Instant::now(),
                fps: 0.0,
                ids,
            });
        });

        // After each render, the frames replaced before it can go back once its GPU work
        // is done: an empty submission marks the end of that render on the queue.
        let queue = ctx.queue().clone();
        window.window().set_rendering_notifier(move |state, _| {
            if !matches!(state, slint::RenderingState::AfterRendering) {
                return;
            }
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut()
                    && !st.replaced.is_empty()
                {
                    let index = queue.submit(std::iter::empty());
                    for f in st.replaced.drain(..) {
                        st.retiring.push_back((f, index.clone()));
                    }
                }
            });
            retire();
        })?;

        // Receives frames off the UI thread; only the latest is shown.
        let weak = window.as_weak();
        let receiver = std::thread::spawn(move || {
            loop {
                match sub.recv(Duration::from_millis(250)) {
                    Ok(Some(frame)) => {
                        let weak = weak.clone();
                        let ok = slint::invoke_from_event_loop(move || {
                            show(&weak, frame);
                            retire();
                        });
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
        // releases them.
        STATE.with(|s| s.borrow_mut().take());
        drop(engine); // stdin closes, the engine stops
        drop(receiver); // detached; it ends with the process
        Ok(())
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
            let Some(image) = st.images.get(frame.slot()) else {
                return;
            };
            window.set_frame(image.clone());
            if let Some(old) = st.shown.replace(frame) {
                st.replaced.push(old);
            }
            st.frames += 1;
            let dt = st.since.elapsed().as_secs_f64();
            if dt >= 1.0 {
                st.fps = st.frames as f64 / dt;
                st.frames = 0;
                st.since = Instant::now();
                let seq = st.shown.as_ref().map_or(0, PreviewFrame::sequence);
                window.set_status(
                    format!(
                        "Preview from engine process: {:.0} fps, frame {seq}, IOSurfaces {} shared, no copies",
                        st.fps, st.ids
                    )
                    .into(),
                );
            }
        });
    }

    /// Releases replaced frames whose last draw has completed on the GPU. Never waits.
    fn retire() {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            let Some(st) = s.as_mut() else {
                return;
            };
            while let Some((_, index)) = st.retiring.front() {
                let done = st
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
                st.retiring.pop_front();
            }
            if st.retiring.is_empty() {
                st.timer.stop();
            } else if !st.timer.running() {
                st.timer
                    .start(slint::TimerMode::Repeated, Duration::from_millis(2), retire);
            }
        });
    }
}
