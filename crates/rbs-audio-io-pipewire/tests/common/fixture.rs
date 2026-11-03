//! PipeWire endpoints for the integration tests, each in its own child process (this
//! test binary started with an environment variable), so the adapter's threads and fds
//! are counted alone and each endpoint can be removed on its own:
//!
//! - `mic`: an `Audio/Source` node playing a mono sine wave (an `Input` endpoint);
//! - `sink`: a null `Audio/Sink` node (its monitor is an `OutputLoopback` endpoint);
//! - `app`: an application playing a stereo sine wave into an unlinked
//!   `Stream/Output/Audio` stream (an `Application` endpoint).

#![allow(dead_code)]

use std::cell::Cell;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::Mutex;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Object, Pod, Value};
use pw::spa::utils::SpaTypes;

/// Environment variable that turns the test binary into a fixture process.
pub const SERVE: &str = "RBS_PIPEWIRE_AUDIO_FIXTURE";

pub const MIC: &str = "rbs-test-mic";
pub const SINK: &str = "rbs-test-sink";
pub const APP: &str = "rbs-test-app";

const RATE: u32 = 48_000;

/// A fixture process. Dropping it removes its endpoint.
pub struct Fixture {
    role: &'static str,
    child: Mutex<Option<Child>>,
    /// The child's stdout, kept open so its later output never hits a closed pipe.
    _stdout: Mutex<Option<std::io::Lines<BufReader<std::process::ChildStdout>>>>,
}

impl Fixture {
    /// Starts the fixture `role` (`mic`, `sink` or `app`) and waits until its node and
    /// ports are published.
    pub fn start(role: &'static str, test_name: &str) -> Self {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => panic!("current_exe: {e}"),
        };
        let child = Command::new(exe)
            .args(["--exact", test_name, "--ignored", "--nocapture"])
            .arg("--test-threads=1")
            .env(SERVE, role)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(e) => panic!("spawn the {role} fixture: {e}"),
        };
        let Some(stdout) = child.stdout.take() else {
            panic!("no stdout")
        };
        // The child prints `ready` once published (test harness lines come first).
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
            panic!("the {role} fixture did not start");
        }
        Self {
            role,
            child: Mutex::new(Some(child)),
            _stdout: Mutex::new(Some(lines)),
        }
    }

    pub fn role(&self) -> &'static str {
        self.role
    }

    /// Ends the process (its node and client disappear) and waits for it.
    pub fn remove(&self) -> bool {
        let child = self.child.lock().ok().and_then(|mut c| c.take());
        let Some(mut child) = child else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = writeln!(stdin, "quit").and_then(|()| stdin.flush());
        }
        let _ = child.wait();
        true
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.remove();
    }
}

fn audio_format(channels: u32, position: &[u32]) -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(channels);
    let mut positions = [0_u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    positions[..position.len()].copy_from_slice(position);
    info.set_flags(spa::param::audio::AudioInfoRawFlags::empty());
    info.set_position(positions);
    let object = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    });
    match PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &object) {
        Ok((cursor, _)) => cursor.into_inner(),
        Err(e) => panic!("serialize: {e:?}"),
    }
}

/// The fixture process: publishes the role's endpoint until `quit` or the end of stdin.
pub fn serve(role: &str) {
    pw::init();
    let fail = |what: &str| -> ! { panic!("{role} fixture: {what}") };
    let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
        fail("main loop")
    };
    let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
        fail("context")
    };
    let Ok(core) = context.connect_rc(None) else {
        fail("connect")
    };
    let (quit_tx, quit_rx) = pw::channel::channel::<()>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) if line.trim() == "quit" => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = quit_tx.send(());
    });
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let ready = || {
        println!("ready");
        let _ = std::io::stdout().flush();
    };

    if role == "sink" {
        let props = pw::properties::properties! {
            "factory.name" => "support.null-audio-sink",
            *pw::keys::NODE_NAME => SINK,
            *pw::keys::NODE_DESCRIPTION => "RBS test sink",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            "audio.position" => "FL,FR",
            "object.linger" => "false",
        };
        let Ok(_node) = core.create_object::<pw::node::Node>("adapter", &props) else {
            fail("create the null sink")
        };
        wait_for_ports(&mainloop, &core, SINK, 4);
        ready();
        mainloop.run();
        return;
    }

    let (name, channels, position, props) = match role {
        "mic" => (
            MIC,
            1_u32,
            vec![spa::sys::SPA_AUDIO_CHANNEL_MONO],
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CLASS => "Audio/Source",
                *pw::keys::NODE_NAME => MIC,
                *pw::keys::NODE_DESCRIPTION => "RBS test microphone",
            },
        ),
        "app" => (
            "rbs-test-app-stream",
            2,
            vec![
                spa::sys::SPA_AUDIO_CHANNEL_FL,
                spa::sys::SPA_AUDIO_CHANNEL_FR,
            ],
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Playback",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::NODE_NAME => "rbs-test-app-stream",
                *pw::keys::APP_NAME => APP,
            },
        ),
        other => fail(&format!("unknown role {other}")),
    };
    let Ok(stream) = pw::stream::StreamRc::new(core.clone(), name, props) else {
        fail("stream")
    };
    let phase = Rc::new(Cell::new(0_u64));
    let listener = stream
        .add_local_listener_with_user_data(phase)
        .process(move |stream, phase| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let stride = 4 * channels as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            // One default graph cycle; PipeWire asks again when it needs more.
            let frames = (bytes.len() / stride).min(1_024);
            for frame in bytes[..frames * stride].chunks_exact_mut(stride) {
                let n = phase.get();
                phase.set(n + 1);
                let value = (n as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.25;
                for sample in frame.chunks_exact_mut(4) {
                    sample.copy_from_slice(&value.to_le_bytes());
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (frames * stride) as u32;
        })
        .register();
    let Ok(_listener) = listener else {
        fail("listener")
    };
    let param = audio_format(channels, &position);
    let Some(pod) = Pod::from_bytes(&param) else {
        fail("format pod")
    };
    // Neither node links itself anywhere: the adapter links to them.
    if stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )
        .is_err()
    {
        fail("stream connect");
    }
    wait_for_ports(&mainloop, &core, name, channels as usize);
    ready();
    mainloop.run();
    let _ = stream.disconnect();
}

/// Runs the loop until the node `name` has `count` ports in the registry.
fn wait_for_ports(
    mainloop: &pw::main_loop::MainLoopRc,
    core: &pw::core::CoreRc,
    name: &str,
    count: usize,
) {
    let Ok(registry) = core.get_registry_rc() else {
        panic!("registry")
    };
    let node = Rc::new(Cell::new(None::<u32>));
    let ports = Rc::new(std::cell::RefCell::new(Vec::<u32>::new()));
    let found = Rc::new(Cell::new(false));
    let check = {
        let node = Rc::clone(&node);
        let ports = Rc::clone(&ports);
        let found = Rc::clone(&found);
        let mainloop = mainloop.clone();
        move || {
            if let Some(node) = node.get()
                && ports.borrow().iter().filter(|p| **p == node).count() >= count
                && !found.replace(true)
            {
                mainloop.quit();
            }
        }
    };
    let wanted = name.to_owned();
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else { return };
            match global.type_ {
                pw::types::ObjectType::Node if props.get("node.name") == Some(wanted.as_str()) => {
                    node.set(Some(global.id));
                }
                pw::types::ObjectType::Port => {
                    if let Some(id) = props.get("node.id").and_then(|v| v.parse().ok()) {
                        ports.borrow_mut().push(id);
                    }
                }
                _ => return,
            }
            check();
        })
        .register();
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let _ = timer.update_timer(Some(std::time::Duration::from_secs(5)), None);
    mainloop.run();
    if !found.get() {
        panic!("{name} ports did not appear");
    }
}
