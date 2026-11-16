//! Wires the test-double browser helper executable into the studio (ADR-0109).
//!
//! The desktop app does not link CEF. It starts `rbs-browser-helper` next to the main
//! executable, or from `RBS_BROWSER_HELPER` / `CARGO_BIN_EXE_rbs-browser-helper`.

use std::path::PathBuf;
use std::sync::Arc;

use rbs_browser_ipc::Configure;
use rbs_browser_runner::{Supervisor, spawn_helper};
use rbs_browser_shm::SlotLayout;
use rbs_sources::{BrowserFrameSink, BrowserSession, BrowserTree};

/// Locates `rbs-browser-helper` for this process.
pub fn resolve_browser_helper() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RBS_BROWSER_HELPER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_rbs-browser-helper") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut exe = std::env::current_exe().ok()?;
    exe.pop();
    if exe.file_name().is_some_and(|name| name == "deps") {
        exe.pop();
    }
    let name = if cfg!(windows) {
        "rbs-browser-helper.exe"
    } else {
        "rbs-browser-helper"
    };
    exe.push(name);
    exe.is_file().then_some(exe)
}

/// Starts the synthetic helper shipped with `rbs-browser-runner`.
#[derive(Debug)]
pub struct HelperBrowserTree {
    program: PathBuf,
}

impl HelperBrowserTree {
    /// Fails when [`resolve_browser_helper`] finds no executable.
    pub fn new() -> Result<Self, String> {
        let program = resolve_browser_helper()
            .ok_or_else(|| "rbs-browser-helper was not found next to the app".to_owned())?;
        Ok(Self { program })
    }
}

impl BrowserTree for HelperBrowserTree {
    fn start(
        &self,
        config: &Configure,
        sink: Arc<BrowserFrameSink>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        let child = spawn_helper(
            self.program.as_path(),
            &["--hold"],
            config,
            SlotLayout::Rgba8Straight,
        )?;
        // The supervisor thread exists only while the source is Active (P13).
        let supervisor = Supervisor::start(child, config.fps, move |rgba, width, height| {
            sink.publish(rgba, width, height);
        })?;
        Ok(Box::new(HelperSession { supervisor }))
    }
}

struct HelperSession {
    supervisor: Supervisor,
}

impl BrowserSession for HelperSession {
    fn poll(&mut self) -> Option<i32> {
        // An exit or a lost heartbeat (ADR-0109 decision 8), seen by the supervisor.
        self.supervisor.exited().then_some(1)
    }

    fn stop(&mut self) {
        // Never waits: the supervisor thread closes the helper (ADR-0109 decision 8).
        self.supervisor.stop_detached();
    }
}
