//! `rbs-mcp` with standard error closed: a usage error ends with its exit code instead
//! of a panic (crash-resilience spec, "Standard output and error"). The pipe's reader
//! is dropped before the child starts, so every write fails without a race.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::process::{Command, Stdio};

#[test]
fn a_usage_error_with_closed_output_exits_normally() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let status = Command::new(env!("CARGO_BIN_EXE_rbs-mcp"))
        .arg("--no-such-option")
        .stdin(Stdio::null())
        .stdout(writer.try_clone().unwrap())
        .stderr(writer)
        .status()
        .unwrap();
    // `eprintln!` would panic here (exit code 101).
    assert_eq!(status.code(), Some(2));
}
