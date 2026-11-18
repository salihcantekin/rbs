//! The real CEF runner against a downloaded engine (add-ons spec "Browser engine
//! consumer"). Ignored: it needs a built `rbs-browser-cef` and an installed engine.
//!
//! ```text
//! RBS_TEST_CEF_RUNNER=<path to rbs-browser-cef> \
//! RBS_TEST_CEF_RUNTIME=<add-ons root>/org.rbs.browser-engine/<version> \
//! cargo test -p rbs-browser-runner --test real_cef -- --ignored
//! ```
//!
//! On macOS the runner path is the executable inside its bundle. Waits are for
//! conditions with a hang guard (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rbs_browser_ipc::Configure;
use rbs_browser_ipc::exit::Cause;
use rbs_browser_runner::{HelperChild, runtime_env, spawn_helper_with_env};
use rbs_browser_shm::SlotLayout;

/// A cold engine start may take the whole startup limit.
const GUARD: Duration = Duration::from_secs(30);

fn paths() -> Option<(PathBuf, PathBuf)> {
    let runner = std::env::var_os("RBS_TEST_CEF_RUNNER")?;
    let runtime = std::env::var_os("RBS_TEST_CEF_RUNTIME")?;
    Some((runner.into(), runtime.into()))
}

fn spawn(runner: &Path, runtime: &Path) -> HelperChild {
    let library = rbs_browser_ipc::runtime::read(runtime)
        .map_or_else(|_| runtime.join("Release"), |rt| rt.library_dir);
    let config = Configure {
        url: "data:text/html,<body style='background:%23f00'></body>".into(),
        width: 64,
        height: 64,
        fps: 30,
        transparent: false,
        frames: String::new(),
        sound: String::new(),
        sample_rate: 0,
    };
    spawn_helper_with_env(
        runner,
        &[],
        &runtime_env(runtime, &library),
        &config,
        SlotLayout::Rgba8Straight,
    )
    .unwrap()
}

fn wait(child: &mut HelperChild, mut done: impl FnMut(&mut HelperChild) -> bool) {
    let start = Instant::now();
    while !done(child) {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "slow: starts the real CEF runner; needs RBS_TEST_CEF_RUNNER and RBS_TEST_CEF_RUNTIME"]
fn slow_the_runner_paints_with_the_downloaded_engine() {
    let Some((runner, runtime)) = paths() else {
        return;
    };
    let mut child = spawn(&runner, &runtime);
    wait(&mut child, |c| c.refresh() || !c.process_alive());
    assert!(
        child.process_alive(),
        "exited with {:?} ({:?})",
        child.exit_code,
        child.exit_cause()
    );
    // A red page, straight Rgba8.
    assert_eq!(&child.frame[..4], &[255, 0, 0, 255]);
    child.close(rbs_browser_runner::STOP_GRACE);
}

#[test]
#[ignore = "slow: starts the real CEF runner; needs RBS_TEST_CEF_RUNNER"]
fn slow_the_runner_without_an_engine_exits_as_runtime_missing() {
    let Some((runner, _)) = paths() else {
        return;
    };
    let empty = std::env::temp_dir().join(format!("rbs-no-engine-{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let mut child = spawn(&runner, &empty);
    wait(&mut child, |c| !c.process_alive());
    assert_eq!(
        child.exit_cause(),
        Cause::RuntimeMissing,
        "{:?}",
        child.exit_code
    );
    let _ = std::fs::remove_dir_all(empty);
}

#[test]
#[ignore = "slow: starts the real CEF runner; needs RBS_TEST_CEF_RUNNER and RBS_TEST_CEF_RUNTIME"]
fn slow_the_runner_with_another_engine_version_exits_as_a_mismatch() {
    let Some((runner, runtime)) = paths() else {
        return;
    };
    // The same engine under a marker that names another version.
    let rt = rbs_browser_ipc::runtime::read(&runtime).unwrap();
    let other = std::env::temp_dir().join(format!("rbs-other-engine-{}", std::process::id()));
    std::fs::create_dir_all(&other).unwrap();
    let rel = |dir: &Path| {
        dir.strip_prefix(&runtime)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    };
    std::fs::write(
        other.join(".complete"),
        format!(
            "version = \"1.0.0\"\nsha256 = \"00\"\nlibrary_dir = \"{}\"\nresources_dir = \"{}\"\n",
            rel(&rt.library_dir),
            rel(&rt.resources_dir)
        ),
    )
    .unwrap();
    for part in [&rt.library_dir, &rt.resources_dir] {
        let first = rel(part).split('/').next().unwrap().to_owned();
        let _ = link_dir(&runtime.join(&first), &other.join(&first));
    }
    let mut child = spawn(&runner, &other);
    wait(&mut child, |c| !c.process_alive());
    assert_eq!(
        child.exit_cause(),
        Cause::RuntimeMismatch,
        "{:?}",
        child.exit_code
    );
    let _ = std::fs::remove_dir_all(other);
}

#[cfg(unix)]
fn link_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(from, to)
}

#[cfg(windows)]
fn link_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(from, to)
}
