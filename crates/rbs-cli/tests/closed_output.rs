//! `rbs-cli` with standard output and error closed: the program runs to its normal
//! exit code instead of panicking (crash-resilience spec, "Standard output and error").
//! Each pipe's reader is dropped before the child starts, so every write fails, on
//! Windows as on Unix, without a race.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::process::{Command, Stdio};

/// Runs `rbs-cli args` with stdout and stderr writing into a pipe nobody reads.
fn run_closed(args: &[&str]) -> Option<i32> {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    Command::new(env!("CARGO_BIN_EXE_rbs-cli"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .status()
        .unwrap()
        .code()
}

#[test]
fn help_with_closed_output_exits_normally() {
    // `println!` would panic here (exit code 101).
    assert_eq!(run_closed(&["help"]), Some(0));
}

#[test]
fn a_usage_error_with_closed_output_exits_normally() {
    assert_eq!(run_closed(&["no-such-command"]), Some(2));
}
