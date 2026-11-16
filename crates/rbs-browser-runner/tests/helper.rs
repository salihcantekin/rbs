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
    let mut sup = Supervisor::start(spawn(&["--hold"]), 60, move |px, w, h| {
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

    let sup = Supervisor::start(spawn(&["--exit-immediately"]), 60, |_, _, _| {}).unwrap();
    let start = Instant::now();
    while !sup.exited() {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
}
