//! The helper executable, without CEF (ADR-0109 decision 12).
//!
//! Waits are for conditions (a frame, an exit) with a hang guard, never for timing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use rbs_browser_ipc::{Configure, HEARTBEAT_LIMIT};
use rbs_browser_runner::{HelperChild, Supervisor, spawn_helper};
use rbs_browser_shm::SlotLayout;

const GUARD: Duration = Duration::from_secs(20);

fn program() -> std::path::PathBuf {
    let mut path = std::env::current_exe().expect("test executable");
    path.pop();
    if path.file_name().is_some_and(|name| name == "deps") {
        path.pop();
    }
    path.push(format!(
        "rbs-browser-helper{}",
        std::env::consts::EXE_SUFFIX
    ));
    path
}

fn config() -> Configure {
    Configure {
        url: "https://example.com/".into(),
        width: 4,
        height: 2,
        fps: 60,
        transparent: false,
        frames: String::new(),
        sound: String::new(),
        sample_rate: 0,
        page: rbs_browser_ipc::PageOptions::default(),
    }
}

fn spawn(args: &[&str]) -> HelperChild {
    spawn_helper(
        program().as_path(),
        args,
        &config(),
        SlotLayout::Rgba8Straight,
    )
    .unwrap()
}

/// Polls until `done` holds; panics after the hang guard.
fn wait_until(child: &mut HelperChild, mut done: impl FnMut(&mut HelperChild) -> bool) {
    let start = Instant::now();
    while !done(child) {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
}

#[test]
fn helper_exit_leaves_the_host_running() {
    let mut child = spawn(&["--exit-immediately"]);
    wait_until(&mut child, |c| !c.process_alive());
    assert!(child.unavailable);
    assert_eq!(child.attempts, 1);
    assert_eq!(child.retry_after, Duration::from_millis(500));
    assert!(!child.refresh(), "no frame from a helper that exited first");
}

#[test]
fn frames_arrive_through_the_studio_section_and_keep_coming() {
    let mut child = spawn(&["--hold"]);
    wait_until(&mut child, |c| c.refresh());
    assert_eq!((child.width, child.height), (4, 2));
    assert_eq!(child.frame.len(), 4 * 2 * 4);
    assert_eq!(child.frame[3], 255, "the helper paints opaque");
    let first = child.frames_written();
    wait_until(&mut child, |c| c.frames_written() > first + 2);
    assert!(child.process_alive());
    child.stop();
    assert!(!child.process_alive());
}

#[test]
fn dropping_the_studio_copy_does_not_join_a_live_helper() {
    let mut child = spawn(&["--hold"]);
    wait_until(&mut child, |c| c.refresh());
    drop(child.take_frame());
    assert!(child.process_alive(), "dropping the copy joined the child");
    child.stop();
}

#[test]
fn a_lost_heartbeat_stops_the_helper_and_counts_as_an_exit() {
    let mut child = spawn(&["--hang"]);
    wait_until(&mut child, |c| c.refresh());
    let now = Instant::now();
    assert!(child.check_heartbeat(now), "the first beat is seen");
    assert!(child.process_alive(), "still running before the limit");
    assert!(!child.check_heartbeat(now + HEARTBEAT_LIMIT));
    assert!(child.unavailable);
    assert_eq!(child.attempts, 1);
    assert!(!child.process_alive());
}

#[cfg(unix)]
#[test]
fn a_helper_gets_the_startup_limit_before_its_first_beat() {
    // `sh` reading stdin stands in for an engine still starting: it never beats.
    let mut child = spawn_helper(
        std::path::Path::new("sh"),
        &["-c", "cat > /dev/null"],
        &config(),
        SlotLayout::Rgba8Straight,
    )
    .expect("spawn sh");
    let spawned = Instant::now();
    assert!(
        child.check_heartbeat(spawned + HEARTBEAT_LIMIT),
        "not hung yet"
    );
    assert!(child.process_alive());
    assert!(!child.check_heartbeat(spawned + rbs_browser_ipc::STARTUP_LIMIT));
    assert!(child.unavailable);
    assert_eq!(child.attempts, 1);
}

#[test]
fn a_one_shot_helper_leaves_its_frame_after_it_exits() {
    let mut child = spawn(&[]);
    wait_until(&mut child, |c| !c.process_alive());
    assert!(child.refresh(), "the frame stays in the studio's section");
    assert_eq!(&child.frame[..4], &[40, 180, 255, 255]);
}

#[test]
fn the_supervisor_publishes_frames_and_reports_an_exit() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    let published = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&published);
    let mut sup = Supervisor::start(spawn(&["--hold"]), 60, move |px, w, h, painted| {
        assert!(painted > 0, "the helper stamps its frames");
        assert_eq!(px.len(), (w * h * 4) as usize);
        seen.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let start = Instant::now();
    while published.load(Ordering::Relaxed) < 3 {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
    assert!(!sup.exited());
    let child = sup.stop().unwrap();
    assert!(child.unavailable);

    let sup = Supervisor::start(spawn(&["--exit-immediately"]), 60, |_, _, _, _| {}).unwrap();
    let start = Instant::now();
    while !sup.exited() {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
}

#[test]
fn control_messages_reach_the_running_helper_in_order() {
    use rbs_browser_ipc::control::Control;
    use rbs_browser_runner::control_mark;
    use std::sync::{Arc, Mutex};
    let first = Arc::new(Mutex::new([0u8; 2]));
    let seen = Arc::clone(&first);
    let mut sup = Supervisor::start(spawn(&["--hold"]), 60, move |px, _, _, _| {
        *seen.lock().unwrap() = [px[0], px[1]];
    })
    .unwrap();
    let wait_for = |want: [u8; 2]| {
        let start = Instant::now();
        while *first.lock().unwrap() != want {
            assert!(start.elapsed() < GUARD, "hang guard");
            std::thread::yield_now();
        }
    };
    let messages = [
        Control::Reload { bypass_cache: true },
        Control::RunScript(Some("document.title = 'x'".into())),
        Control::RunScript(None),
        // Live page options and input keep the same process (ADR-0149 decision 9).
        Control::Page(Box::new(rbs_browser_ipc::PageOptions {
            zoom_percent: 150,
            css: "body{margin:0}".into(),
            ..rbs_browser_ipc::PageOptions::default()
        })),
        Control::Input(rbs_browser_ipc::input::InputEvent::Text("a".into())),
    ];
    for (n, message) in messages.iter().enumerate() {
        assert!(sup.send(message));
        wait_for([u8::try_from(n + 1).unwrap(), control_mark(message)]);
    }
    // A background page is hidden and shown again (ADR-0149 decision 5); asking for
    // the state it is in sends nothing.
    let n = u8::try_from(messages.len()).unwrap();
    assert!(sup.shown());
    assert!(sup.set_shown(false));
    assert!(!sup.shown());
    wait_for([n + 1, control_mark(&Control::Shown(false))]);
    assert!(sup.set_shown(false));
    assert!(sup.set_shown(true));
    wait_for([n + 2, control_mark(&Control::Shown(true))]);
    let child = sup.stop().unwrap();
    assert!(child.unavailable);
    assert!(
        !sup.send(&Control::Reload {
            bypass_cache: false
        }),
        "a stopped helper takes no message"
    );
}

#[test]
fn page_sound_crosses_the_sound_section_in_order() {
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Heard {
        frames: usize,
        packets: Vec<(u64, usize)>,
        loud: bool,
        pumps: usize,
        format: Option<(u32, u16)>,
    }
    struct Sink(Arc<Mutex<Heard>>);
    impl rbs_browser_runner::PageSound for Sink {
        fn packet(&mut self, packet: rbs_browser_shm::SoundPacket<'_>) {
            let mut heard = self.0.lock().unwrap();
            let frames = packet.samples.len() / usize::from(packet.channels);
            heard.frames += frames;
            heard.packets.push((packet.os_nanos, frames));
            heard.loud |= packet.samples.iter().any(|s| s.abs() > 0.1);
            heard.format = Some((packet.rate, packet.channels));
        }
        fn observe(&mut self, _os_now: u64) {}
        fn pump(&mut self) {
            self.0.lock().unwrap().pumps += 1;
        }
    }

    let mut config = config();
    config.sample_rate = 48_000;
    let child = spawn_helper(
        program().as_path(),
        &["--hold", "--tone"],
        &config,
        SlotLayout::Rgba8Straight,
    )
    .unwrap();
    assert!(child.has_sound());
    let heard = Arc::new(Mutex::new(Heard::default()));
    let mut sup = Supervisor::start_with_sound(
        child,
        60,
        |_, _, _, _| {},
        Box::new(Sink(Arc::clone(&heard))),
    )
    .unwrap();
    let start = Instant::now();
    // A tenth of a second of sound, whatever the scheduling.
    while heard.lock().unwrap().frames < 4_800 {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
    // Hidden, the sound is dropped; shown again, it comes back from the present
    // (ADR-0149 decision 5), still in order.
    assert!(sup.set_shown(false));
    assert!(sup.set_shown(true));
    let resumed = heard.lock().unwrap().frames + 4_800;
    while heard.lock().unwrap().frames < resumed {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
    sup.stop().unwrap();
    let heard = heard.lock().unwrap();
    assert_eq!(heard.format, Some((48_000, 2)));
    assert!(heard.loud);
    assert!(heard.pumps > 0);
    // Each packet starts where the one before ended, on the OS clock, except once:
    // across the dropped sound of the hidden page, which only moves forward.
    let mut gaps = 0;
    for pair in heard.packets.windows(2) {
        let ((t0, n0), (t1, _)) = (pair[0], pair[1]);
        let end = t0 + (n0 as u64 * 1_000_000_000).div_ceil(48_000);
        if t1.abs_diff(end) > 1 {
            assert!(t1 > end, "{t0} {n0} {t1}");
            gaps += 1;
        }
    }
    assert!(gaps <= 1, "{gaps} gaps");
}

#[test]
fn no_sound_section_without_a_sample_rate() {
    let child = spawn(&["--once"]);
    assert!(!child.has_sound());
}

/// A runtime directory with a marker for `version` and an empty library directory.
fn fake_runtime(name: &str, version: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("rbs-runner-rt-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("Release")).unwrap();
    std::fs::write(
        root.join(".complete"),
        format!(
            "version = \"{version}\"\nsha256 = \"00\"\nlibrary_dir = \"Release\"\nresources_dir = \"Resources\"\n"
        ),
    )
    .unwrap();
    root
}

fn spawn_with_runtime(runtime: Option<&std::path::Path>) -> HelperChild {
    use rbs_browser_ipc::runtime::RUNTIME_ENV;
    let env: Vec<(std::ffi::OsString, std::ffi::OsString)> = runtime
        .map(|rt| (RUNTIME_ENV.into(), rt.as_os_str().to_owned()))
        .into_iter()
        .collect();
    rbs_browser_runner::spawn_helper_with_env(
        program().as_path(),
        &["--needs-runtime", "--hold"],
        &env,
        &config(),
        SlotLayout::Rgba8Straight,
    )
    .unwrap()
}

#[test]
fn a_runner_without_its_engine_exits_as_runtime_missing() {
    use rbs_browser_ipc::exit::Cause;
    // A runtime path with no marker: the install never finished.
    let missing = std::env::temp_dir().join("rbs-runner-rt-none");
    let mut child = spawn_with_runtime(Some(&missing));
    wait_until(&mut child, |c| !c.process_alive());
    assert_eq!(
        child.exit_code,
        Some(rbs_browser_ipc::exit::RUNTIME_MISSING)
    );
    assert!(!child.has_beaten());
    assert_eq!(child.exit_cause(), Cause::RuntimeMissing);
}

#[test]
fn a_runner_with_another_engine_version_exits_as_a_mismatch() {
    use rbs_browser_ipc::exit::Cause;
    let rt = fake_runtime("old", "155.0.1");
    let sup = Supervisor::start(spawn_with_runtime(Some(&rt)), 60, |_, _, _, _| {}).unwrap();
    let start = Instant::now();
    while !sup.exited() {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
    assert_eq!(sup.exit_cause(), Cause::RuntimeMismatch);
    let _ = std::fs::remove_dir_all(rt);
}

#[test]
fn a_runner_with_its_engine_runs_and_a_later_exit_is_not_the_engine() {
    use rbs_browser_ipc::exit::Cause;
    let rt = fake_runtime("ok", "156.0.30");
    let mut child = spawn_with_runtime(Some(&rt));
    wait_until(&mut child, |c| c.refresh());
    assert!(child.process_alive());
    assert!(child.has_beaten());
    child.stop();
    assert_eq!(child.exit_cause(), Cause::Other, "the studio's kill");
    let _ = std::fs::remove_dir_all(rt);
}
