//! A PipeWire `Video/Source` node driven by a timer, shared by the integration tests.
//! Each test binary runs it in a child process (the binary itself, started with an
//! environment variable), so the adapter's threads and fds are counted alone.

#![allow(dead_code)]

use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Mutex;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{ChoiceValue, Object, Pod, Property, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};

pub const NODE_NAME: &str = "rbs-test-source";
const RATE: u32 = 60;

pub enum Message {
    Resize(u32, u32),
    Quit,
}

/// The running source. Dropping it removes the node.
pub struct TestSource {
    child: Mutex<Option<Child>>,
    /// The child's stdout, kept open so its later output never hits a closed pipe.
    _stdout: Mutex<Option<std::io::Lines<BufReader<std::process::ChildStdout>>>>,
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

/// Environment variable that turns the test binary into the source process.
pub const SERVE: &str = "RBS_PIPEWIRE_TEST_SOURCE";

impl TestSource {
    /// Publishes the node from a child process (this test binary running
    /// [`serve`]) and waits until its stream is connected. A separate process keeps
    /// the source's threads and fds out of the adapter's resource counts.
    pub fn start(width: u32, height: u32) -> Self {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => panic!("current_exe: {e}"),
        };
        let child = Command::new(exe)
            .args(["--exact", "test_source_process", "--ignored", "--nocapture"])
            .arg("--test-threads=1")
            .env(SERVE, format!("{width}x{height}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => panic!("spawn the test source: {e}"),
        };
        let Some(stdout) = child.stdout.take() else {
            panic!("no stdout")
        };
        // The child prints `ready` once connected (test harness lines come first).
        let mut ready = false;
        let mut lines = BufReader::new(stdout).lines();
        for line in lines.by_ref() {
            match line {
                Ok(line) if line.trim_end().ends_with("ready") => {
                    ready = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if !ready {
            let _ = child.kill();
            panic!("test source did not start");
        }
        Self {
            _stdout: Mutex::new(Some(lines)),
            child: Mutex::new(Some(child)),
        }
    }

    fn send(&self, line: &str) -> bool {
        let Ok(mut guard) = self.child.lock() else {
            return false;
        };
        let Some(stdin) = guard.as_mut().and_then(|c| c.stdin.as_mut()) else {
            return false;
        };
        writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .is_ok()
    }

    pub fn resize(&self, width: u32, height: u32) -> bool {
        self.send(&format!("resize {width} {height}"))
    }

    /// Removes the node and waits for the source process to exit.
    pub fn remove(&self) -> bool {
        let sent = self.send("quit");
        let child = self.child.lock().ok().and_then(|mut c| c.take());
        if let Some(mut child) = child {
            drop(child.stdin.take());
            let _ = child.wait();
        }
        sent
    }
}

impl Drop for TestSource {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The source process: runs the node until `quit` or the end of stdin.
pub fn serve(size: &str) {
    let (width, height) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((320, 240));
    let (commands, receiver) = pw::channel::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let words: Vec<&str> = line.split_whitespace().collect();
            let command = match words.as_slice() {
                ["resize", w, h] => match (w.parse(), h.parse()) {
                    (Ok(w), Ok(h)) => Message::Resize(w, h),
                    _ => continue,
                },
                ["quit"] => break,
                _ => continue,
            };
            if commands.send(command).is_err() {
                return;
            }
        }
        let _ = commands.send(Message::Quit);
    });
    run(width, height, receiver, |_| {
        println!("ready");
        let _ = std::io::stdout().flush();
    });
}

pub fn run(
    width: u32,
    height: u32,
    commands: pw::channel::Receiver<Message>,
    ready: impl FnOnce(u32) + 'static,
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
        .state_changed({
            // Ready once the node is exported (Paused), so consumers can find it.
            let ready = std::cell::RefCell::new(Some(ready));
            move |stream, _, _, new| {
                if matches!(new, pw::stream::StreamState::Paused) {
                    if let Some(ready) = ready.borrow_mut().take() {
                        ready(stream.node_id());
                    }
                }
            }
        })
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
            Message::Resize(w, h) => {
                let params = [format(w, h)];
                let _ = stream.update_params(&mut pods(&params));
            }
            Message::Quit => mainloop.quit(),
        }
    });
    mainloop.run();
    let _ = stream.disconnect();
}
