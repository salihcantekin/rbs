//! `Session::end_clean_at_exit`, in child processes of this test binary: a process
//! that returns from `main` or (not on Windows, where `std::process::exit` calls
//! `ExitProcess` and runs no C runtime exit handler) calls `exit` leaves no marker; one
//! whose main thread panicked uncaught keeps it. The child runs one test of this file, picked by `RBS_CRASH_CHILD`.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::{Path, PathBuf};
use std::process::Command;

const CHILD: &str = "RBS_CRASH_CHILD";
const DIR: &str = "RBS_CRASH_DIR";

/// Runs `test` of this binary in a child with `mode`; returns the child's marker files.
fn run_child(mode: &str) -> (PathBuf, Vec<String>) {
    let dir = std::env::temp_dir().join(format!("rbs-crash-exit-{mode}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CHILD, mode)
        .env(DIR, &dir)
        // The panicking child's failure report is expected; keep it out of this output.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    // The child that panics fails its test; the others do not.
    assert_eq!(status.success(), mode != "panic", "{status:?}");
    (dir.clone(), markers(&dir))
}

fn markers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("running-"))
        .collect()
}

/// The child: only does something when started by `run_child`.
#[test]
fn child() {
    let Ok(mode) = std::env::var(CHILD) else {
        return;
    };
    let dir = PathBuf::from(std::env::var(DIR).unwrap());
    let session = rbs_crash::install(&dir).unwrap();
    assert!(session.end_clean_at_exit());
    match mode.as_str() {
        // As Cmd+Q on macOS: `exit` without returning from the event loop.
        "exit" => std::process::exit(0),
        // The test passes and the harness returns from `main`.
        "return" => {}
        "panic" => panic!("uncaught on the thread that installed"),
        other => panic!("unknown mode {other}"),
    }
}

#[test]
fn returning_from_main_ends_the_session_cleanly() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let (dir, left) = run_child("return");
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(not(windows))]
#[test]
fn exit_ends_the_session_cleanly() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let (dir, left) = run_child("exit");
    assert!(left.is_empty(), "{left:?}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_uncaught_panic_keeps_the_marker() {
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    let (dir, left) = run_child("panic");
    assert_eq!(left.len(), 1, "{left:?}");
    std::fs::remove_dir_all(dir).unwrap();
}
