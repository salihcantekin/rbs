//! The helper's standard error reaches the studio's diagnostics log (ADR-0158): its own
//! lines at Info, the engine's at Debug, in `Area::Browser`. Its own test binary: the
//! log sink is installed once per process.
//!
//! Waits are for the lines with a hang guard, never for timing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rbs_browser_ipc::Configure;
use rbs_browser_runner::spawn_helper;
use rbs_browser_shm::SlotLayout;
use rbs_core::log::{self, Area, Filter, Level, Record, Sink};

const GUARD: Duration = Duration::from_secs(20);

struct Capture(Mutex<Vec<(Level, Area, String)>>);

impl Sink for Capture {
    fn write(&self, r: &Record<'_>) {
        self.0
            .lock()
            .unwrap()
            .push((r.level, r.area, r.args.to_string()));
    }
}

static SEEN: Capture = Capture(Mutex::new(Vec::new()));

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

#[test]
fn helper_lines_reach_the_log_with_their_levels() {
    log::install(&SEEN).unwrap();
    log::set_all(Filter::Trace);
    let config = Configure {
        url: "https://example.com/".into(),
        width: 4,
        height: 2,
        fps: 60,
        transparent: false,
        frames: String::new(),
        sound: String::new(),
        sample_rate: 0,
        page: rbs_browser_ipc::PageOptions::default(),
        gpu: None,
    };
    let _child = spawn_helper(
        program().as_path(),
        &["--say"],
        &config,
        SlotLayout::Rgba8Straight,
    )
    .unwrap();
    let start = Instant::now();
    while SEEN.0.lock().unwrap().len() < 2 {
        assert!(start.elapsed() < GUARD, "hang guard");
        std::thread::yield_now();
    }
    let seen = SEEN.0.lock().unwrap().clone();
    assert_eq!(
        seen[..2],
        [
            (
                Level::Info,
                Area::Browser,
                "rbs-browser-helper: started".to_owned()
            ),
            (
                Level::Debug,
                Area::Browser,
                "helper: engine noise".to_owned()
            ),
        ]
    );
}
