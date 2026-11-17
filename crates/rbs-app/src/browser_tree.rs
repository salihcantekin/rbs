//! Wires the test-double browser helper executable into the studio (ADR-0109).
//!
//! The desktop app does not link CEF. It starts `rbs-browser-helper` next to the main
//! executable, or from `RBS_BROWSER_HELPER` / `CARGO_BIN_EXE_rbs-browser-helper`.

use std::path::PathBuf;
use std::sync::Arc;

use rbs_browser_ipc::Configure;
use rbs_browser_runner::{PageSound, Supervisor, spawn_helper};
use rbs_browser_shm::SlotLayout;
use rbs_browser_shm::SoundPacket;
use rbs_sources::{
    BrowserFrameSink, BrowserPictureEdge, BrowserSession, BrowserSound, BrowserSoundFeed,
    BrowserTree,
};

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
        sound: Option<Arc<BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        let child = spawn_helper(
            self.program.as_path(),
            &["--hold"],
            config,
            SlotLayout::Rgba8Straight,
        )?;
        // The supervisor thread exists only while the source is Active (P13). It
        // stamps each picture on the master clock at the edge, by the rule the page's
        // sound follows, so the render thread shows it with that sound (ADR-0109
        // decision 8). Without a sound node there is no clock: pictures are due at once.
        let mut edge = sound
            .as_ref()
            .map(|sound| BrowserPictureEdge::new(sound.clock()));
        let publish = move |rgba: &[u8], width, height, painted| {
            let stamp = edge
                .as_mut()
                .map(|edge| edge.stamp(painted, rbs_browser_shm::os_monotonic_nanos()));
            sink.publish_at(rgba, width, height, stamp);
        };
        let supervisor = match sound {
            // The page's sound reaches the sound node through the feed, on the
            // supervisor thread (ADR-0109 decision 6).
            Some(sound) => Supervisor::start_with_sound(
                child,
                config.fps,
                publish,
                Box::new(FeedSound(BrowserSoundFeed::new(sound))),
            )?,
            None => Supervisor::start(child, config.fps, publish)?,
        };
        Ok(Box::new(HelperSession { supervisor }))
    }
}

/// Hands the supervisor's packets to the studio's feed.
struct FeedSound(BrowserSoundFeed);

impl PageSound for FeedSound {
    fn observe(&mut self, os_now: u64) {
        self.0.observe(os_now);
    }

    fn packet(&mut self, packet: SoundPacket<'_>) {
        self.0.packet(
            packet.os_nanos,
            packet.rate,
            packet.channels,
            packet.samples,
        );
    }

    fn pump(&mut self) {
        self.0.pump();
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
