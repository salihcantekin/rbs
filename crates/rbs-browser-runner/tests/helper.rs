//! The helper executable, without CEF (ADR-0109 decision 12).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use rbs_browser_ipc::Configure;
use rbs_browser_runner::spawn_helper;

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

fn config(transparent: bool) -> Configure {
    Configure {
        url: "https://example.com/".into(),
        width: 2,
        height: 2,
        fps: 30,
        transparent,
    }
}

#[test]
fn helper_exit_leaves_the_host_running() {
    let result = std::panic::catch_unwind(|| {
        spawn_helper(program().as_path(), &["--exit-immediately"], &config(false))
    });
    let mut child = result
        .expect("the host panicked when the helper exited")
        .unwrap();
    assert!(child.unavailable);
    assert!(child.attempts >= 1);
    assert_eq!(child.retry_after, Duration::from_millis(500));
    assert!(!child.process_alive());
    assert!(child.frame.iter().all(|byte| *byte == 0));
}

#[test]
fn dropping_the_studio_buffer_does_not_join_a_live_helper() {
    let mut child = spawn_helper(program().as_path(), &["--hold"], &config(false)).unwrap();
    assert_eq!(child.width, 2);
    assert_eq!(child.frame[3], 255, "transparent false forces opaque alpha");
    let frame = child.take_frame();
    drop(frame);
    assert!(
        child.process_alive(),
        "dropping the studio buffer joined or reaped the child"
    );
    child.stop();
    assert!(!child.process_alive());
}

#[test]
fn a_frame_is_copied_then_exit_is_unavailable() {
    let mut child = spawn_helper(program().as_path(), &[], &config(true)).unwrap();
    // 2×2 fits in one checker cell: bright cyan from the stand-in helper.
    assert_eq!(child.frame[0], 40);
    assert_eq!(child.frame[1], 180);
    assert_eq!(child.frame[2], 255);
    assert_eq!(
        child.frame[3], 255,
        "helper paints opaque; seal_alpha keeps it"
    );
    assert!(child.unavailable);
    assert!(!child.process_alive());
}
