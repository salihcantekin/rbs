//! Runs the capture port conformance suite against the PipeWire adapter.
//!
//! The test publishes its own `Video/Source` node (a moving test pattern in BGRx or
//! RGBA, in shared memory) and runs the suite against it: enumeration, sessions, timestamps,
//! latest wins, allocations, resize (the source renegotiates its size), target loss
//! (the source goes away) and resource release. It needs a running PipeWire daemon
//! with a session manager (WirePlumber) for the stream to be linked; without a daemon
//! socket it only checks probing and reports the rest skipped, unless
//! `RBS_REQUIRE_PIPEWIRE` is set (CI starts both and sets it).
//!
//! Run it with `--test-threads=1` so the process thread count check is exact; the file
//! holds a single test for that reason.

/// Allocations plus frees made by `f` on this thread.
fn measure(f: &mut dyn FnMut()) -> u64 {
    let info = allocation_counter::measure(f);
    info.count_total + info.count_current.unsigned_abs()
}

#[cfg(target_os = "linux")]
mod source {
    //! A PipeWire `Video/Source` node driven by a timer, on its own thread.

    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use std::time::Duration;

    use pipewire as pw;
    use pw::spa;
    use pw::spa::pod::serialize::PodSerializer;
    use pw::spa::pod::{ChoiceValue, Object, Pod, Property, Value};
    use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

    pub const NODE_NAME: &str = "rbs-test-source";
    const RATE: u32 = 60;

    enum Command {
        Resize(u32, u32),
        Quit,
    }

    /// The running source. Dropping it removes the node.
    pub struct TestSource {
        commands: Mutex<Option<pw::channel::Sender<Command>>>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    fn serialize(value: Value) -> Vec<u8> {
        match PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value) {
            Ok((cursor, _)) => cursor.into_inner(),
            Err(e) => panic!("serialize: {e:?}"),
        }
    }

    fn format(width: u32, height: u32) -> Vec<u8> {
        let id = |v: u32| Value::Id(Id(v));
        serialize(Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: vec![
                Property::new(
                    spa::param::format::FormatProperties::MediaType.as_raw(),
                    id(spa::param::format::MediaType::Video.as_raw()),
                ),
                Property::new(
                    spa::param::format::FormatProperties::MediaSubtype.as_raw(),
                    id(spa::param::format::MediaSubtype::Raw.as_raw()),
                ),
                Property::new(
                    spa::param::format::FormatProperties::VideoFormat.as_raw(),
                    Value::Choice(ChoiceValue::Id(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Enum {
                            default: Id(spa::param::video::VideoFormat::BGRx.as_raw()),
                            alternatives: vec![
                                Id(spa::param::video::VideoFormat::BGRx.as_raw()),
                                Id(spa::param::video::VideoFormat::RGBA.as_raw()),
                            ],
                        },
                    ))),
                ),
                Property::new(
                    spa::param::format::FormatProperties::VideoSize.as_raw(),
                    Value::Rectangle(Rectangle { width, height }),
                ),
                Property::new(
                    spa::param::format::FormatProperties::VideoFramerate.as_raw(),
                    Value::Choice(ChoiceValue::Fraction(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Range {
                            default: Fraction {
                                num: RATE,
                                denom: 1,
                            },
                            min: Fraction { num: 0, denom: 1 },
                            max: Fraction { num: 240, denom: 1 },
                        },
                    ))),
                ),
            ],
        }))
    }

    fn buffers(width: u32, height: u32) -> Vec<u8> {
        let stride = width as i32 * 4;
        serialize(Value::Object(Object {
            type_: SpaTypes::ObjectParamBuffers.as_raw(),
            id: spa::param::ParamType::Buffers.as_raw(),
            properties: vec![
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_buffers,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Range {
                            default: 8,
                            min: 2,
                            max: 16,
                        },
                    ))),
                ),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_size,
                    Value::Int(stride * height as i32),
                ),
                Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
                Property::new(
                    spa::sys::SPA_PARAM_BUFFERS_dataType,
                    Value::Choice(ChoiceValue::Int(Choice(
                        ChoiceFlags::empty(),
                        ChoiceEnum::Flags {
                            default: 1 << spa::sys::SPA_DATA_MemFd,
                            flags: vec![1 << spa::sys::SPA_DATA_MemFd],
                        },
                    ))),
                ),
            ],
        }))
    }

    fn pods(bytes: &[Vec<u8>]) -> Vec<&Pod> {
        bytes.iter().filter_map(|b| Pod::from_bytes(b)).collect()
    }

    /// Size and frame counter of the stream.
    struct State {
        width: Cell<u32>,
        height: Cell<u32>,
        frame: Cell<u32>,
    }

    impl TestSource {
        /// Publishes the node and waits until the stream is connected.
        pub fn start(width: u32, height: u32) -> Self {
            let (commands, receiver) = pw::channel::channel();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let thread = std::thread::Builder::new()
                .name("test-source".into())
                .spawn(move || run(width, height, receiver, ready_tx));
            let thread = match thread {
                Ok(t) => t,
                Err(e) => panic!("spawn: {e}"),
            };
            if let Err(e) = ready_rx.recv_timeout(Duration::from_secs(5)) {
                panic!("test source did not start: {e}");
            }
            Self {
                commands: Mutex::new(Some(commands)),
                thread: Mutex::new(Some(thread)),
            }
        }

        fn send(&self, command: Command) -> bool {
            match self.commands.lock() {
                Ok(guard) => guard.as_ref().is_some_and(|s| s.send(command).is_ok()),
                Err(_) => false,
            }
        }

        pub fn resize(&self, width: u32, height: u32) -> bool {
            self.send(Command::Resize(width, height))
        }

        /// Removes the node and waits for its thread.
        pub fn remove(&self) -> bool {
            let sent = self.send(Command::Quit);
            if let Ok(mut guard) = self.commands.lock() {
                guard.take();
            }
            let thread = self.thread.lock().ok().and_then(|mut t| t.take());
            if let Some(thread) = thread {
                let _ = thread.join();
            }
            sent
        }
    }

    impl Drop for TestSource {
        fn drop(&mut self) {
            self.remove();
        }
    }

    fn run(
        width: u32,
        height: u32,
        commands: pw::channel::Receiver<Command>,
        ready: std::sync::mpsc::Sender<()>,
    ) {
        pw::init();
        let fail = |what: &str| -> ! { panic!("test source: {what}") };
        let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
            fail("main loop")
        };
        let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
            fail("context")
        };
        let Ok(core) = context.connect_rc(None) else {
            fail("connect")
        };
        let props = pw::properties::properties! {
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Source",
            *pw::keys::NODE_NAME => NODE_NAME,
            *pw::keys::NODE_DESCRIPTION => "RBS test source",
        };
        let Ok(stream) = pw::stream::StreamRc::new(core.clone(), NODE_NAME, props) else {
            fail("stream")
        };
        let state = Rc::new(State {
            width: Cell::new(width),
            height: Cell::new(height),
            frame: Cell::new(0),
        });
        let listener = stream
            .add_local_listener_with_user_data(Rc::clone(&state))
            .param_changed(|stream, state, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = spa::param::video::VideoInfoRaw::new();
                if info.parse(param).is_err() {
                    return;
                }
                let size = info.size();
                state.width.set(size.width);
                state.height.set(size.height);
                let params = [buffers(size.width, size.height)];
                let _ = stream.update_params(&mut pods(&params));
            })
            .process(|stream, state| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let (w, h) = (state.width.get() as usize, state.height.get() as usize);
                let frame = state.frame.get().wrapping_add(1);
                state.frame.set(frame);
                let Some(data) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let stride = w * 4;
                let size = stride * h;
                if let Some(bytes) = data.data() {
                    let size = size.min(bytes.len());
                    // A moving gradient, opaque in both offered formats.
                    for (i, px) in bytes[..size].chunks_exact_mut(4).enumerate() {
                        let x = (i % w.max(1)) as u32;
                        px[0] = (x.wrapping_add(frame) & 0xff) as u8;
                        px[1] = ((i / w.max(1)) & 0xff) as u8;
                        px[2] = (frame & 0xff) as u8;
                        px[3] = 0xff;
                    }
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = stride as i32;
                    *chunk.size_mut() = size as u32;
                }
            })
            .register();
        let Ok(_listener) = listener else {
            fail("listener")
        };
        let params = [format(width, height)];
        if stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut pods(&params),
            )
            .is_err()
        {
            fail("stream connect");
        }

        let timer = mainloop.loop_().add_timer({
            let stream = stream.clone();
            move |_| {
                let _ = stream.trigger_process();
            }
        });
        let period = Duration::from_secs(1) / RATE;
        let _ = timer.update_timer(Some(period), Some(period));

        let _commands = commands.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            let stream = stream.clone();
            move |command| match command {
                Command::Resize(w, h) => {
                    let params = [format(w, h)];
                    let _ = stream.update_params(&mut pods(&params));
                }
                Command::Quit => mainloop.quit(),
            }
        });
        let _ = ready.send(());
        mainloop.run();
        let _ = stream.disconnect();
    }
}

#[test]
fn passes_conformance() {
    use rbs_capture::conformance::{self, Options, RealTimePacer};
    use rbs_capture_pipewire::PipeWireCaptureFactory;

    let factory = PipeWireCaptureFactory::new();

    #[cfg(target_os = "linux")]
    let _source;
    #[cfg(target_os = "linux")]
    let hooks;
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut options = Options {
        measure_allocations: Some(measure),
        ..Options::default()
    };
    #[cfg(target_os = "linux")]
    {
        if rbs_capture_pipewire::daemon_present() {
            let source = std::sync::Arc::new(source::TestSource::start(320, 240));
            hooks = Hooks(std::sync::Arc::clone(&source));
            _source = source;
            options.hooks = Some(&hooks);
            options.process_threads = Some(conformance::process_thread_count);
        } else if std::env::var_os("RBS_REQUIRE_PIPEWIRE").is_some_and(|v| !v.is_empty()) {
            panic!("RBS_REQUIRE_PIPEWIRE is set but no PipeWire daemon socket was found");
        } else {
            eprintln!("no PipeWire daemon: only probing is checked");
        }
    }
    match conformance::run(&factory, &mut RealTimePacer::new(), &options) {
        Ok(report) => {
            eprintln!("{report:?}");
            if options.hooks.is_some() {
                for check in [
                    "enumerate",
                    "formats",
                    "standby",
                    "timestamps",
                    "latest wins",
                    "no allocation",
                    "stop",
                    "target lost",
                ] {
                    assert!(
                        report.passed.iter().any(|c| c == check),
                        "{check} did not run: {report:?}"
                    );
                }
            }
        }
        Err(failure) => panic!("conformance failed: {failure}"),
    }
    // The suite does not resize cameras, but PipeWire sources may renegotiate their
    // size at any time; check that the adapter follows.
    #[cfg(target_os = "linux")]
    if options.hooks.is_some() {
        follows_renegotiated_size(&factory);
    }
}

/// A source that changes its size mid-stream delivers frames, and a format, of the
/// new size.
#[cfg(target_os = "linux")]
fn follows_renegotiated_size(factory: &rbs_capture_pipewire::PipeWireCaptureFactory) {
    use std::time::{Duration, Instant};

    use rbs_capture::{CaptureFactory, CaptureRequest, TargetId};
    use rbs_core::media::{MemoryKind, PixelFormat};
    use rbs_core::time::MonotonicClock;

    let source = source::TestSource::start(320, 240);
    let target = TargetId::new(format!("node:{}", source::NODE_NAME));
    let request = CaptureRequest::new(target, std::sync::Arc::new(MonotonicClock::new()))
        .with_pixel_formats(&[PixelFormat::Bgra8])
        .with_memory(&[MemoryKind::System]);
    let mut session = match factory.open(&request) {
        Ok(s) => s,
        Err(e) => panic!("open: {e}"),
    };
    if let Err(e) = session.start() {
        panic!("start: {e}");
    }
    let wait_for = |session: &mut Box<dyn rbs_capture::CaptureSession>, w: u32, h: u32| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match session.latest_frame() {
                Ok(Some(frame)) if frame.format().size.width == w => {
                    assert_eq!(frame.format().size.height, h);
                    return;
                }
                Ok(_) if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Ok(_) => panic!("no {w}x{h} frame within 3 s"),
                Err(e) => panic!("latest_frame: {e}"),
            }
        }
    };
    wait_for(&mut session, 320, 240);
    assert!(source.resize(160, 120), "resize command not sent");
    wait_for(&mut session, 160, 120);
    let size = session.format().size;
    assert_eq!(
        (size.width, size.height),
        (160, 120),
        "format() did not follow"
    );
    session.stop();
}

#[cfg(target_os = "linux")]
struct Hooks(std::sync::Arc<source::TestSource>);

#[cfg(target_os = "linux")]
impl rbs_capture::conformance::TestHooks for Hooks {
    fn resize(&self, target: &rbs_capture::TargetId, width: u32, height: u32) -> bool {
        is_test_source(target) && self.0.resize(width, height)
    }

    fn remove(&self, target: &rbs_capture::TargetId) -> bool {
        is_test_source(target) && self.0.remove()
    }
}

#[cfg(target_os = "linux")]
fn is_test_source(target: &rbs_capture::TargetId) -> bool {
    target.as_str() == format!("node:{}", source::NODE_NAME)
}
