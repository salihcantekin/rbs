//! ADR-0056 decision 6: on macOS the app adds `applicationShouldTerminate:` (PR #148)
//! and `applicationShouldHandleReopen:hasVisibleWindows:` to winit's application
//! delegate class, which works only while winit does not define them. With winit
//! 0.30.13 it does not. A winit upgrade must be checked again (the Dock click and Cmd+Q
//! on a Mac) before this pin is moved: if the delegate answers either selector itself,
//! RBS falls back to quitting on Close, silently for the user.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

#[test]
fn winit_is_the_checked_version() {
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
        .expect("the workspace lockfile");
    // Line by line: a Windows checkout may have CRLF line ends.
    let lines: Vec<&str> = lock.lines().map(str::trim_end).collect();
    let versions: Vec<&str> = lines
        .windows(2)
        .filter(|w| w[0] == "name = \"winit\"")
        .filter_map(|w| w[1].strip_prefix("version = \""))
        .map(|v| v.trim_end_matches('"'))
        .collect();
    assert_eq!(
        versions,
        ["0.30.13"],
        "winit changed: re-check ADR-0056 decision 6 on a Mac"
    );
}
