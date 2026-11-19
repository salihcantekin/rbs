//! Wires the browser runner into the studio (ADR-0109, ADR-0145 decisions 7 and 8).
//!
//! The desktop app does not link CEF. It starts the runner `rbs-browser-cef`, which
//! ships next to the app and loads the browser engine add-on from the directory the
//! studio names in its environment (add-ons spec "Browser engine consumer"). Tests and
//! developers point `RBS_BROWSER_HELPER` at another runner; the test double
//! `rbs-browser-helper` is found through `CARGO_BIN_EXE_rbs-browser-helper` only.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rbs_browser_ipc::Configure;
use rbs_browser_ipc::control::Control;
use rbs_browser_ipc::exit::Cause;
use rbs_browser_runner::{PageSound, Supervisor, runtime_env, spawn_helper_with_env};
use rbs_browser_shm::SlotLayout;
use rbs_browser_shm::SoundPacket;
use rbs_sources::{
    BROWSER_RUNTIME_MISMATCH, BROWSER_RUNTIME_MISSING, BrowserFrameSink, BrowserPictureEdge,
    BrowserSession, BrowserSound, BrowserSoundFeed, BrowserTree, HELPER_MISSING,
};

/// The runner that ships next to the app.
const RUNNER: &str = "rbs-browser-cef";

/// Which runner this process starts, and whether it needs the engine add-on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    /// The executable.
    pub program: PathBuf,
    /// Its arguments (the test double's `--hold`).
    pub args: Vec<String>,
    /// True for the shipped runner: it does not start without an installed engine.
    /// An override or the test double may carry its own engine.
    pub needs_engine: bool,
}

/// Locates the runner for this process: `RBS_BROWSER_HELPER` overrides, then the
/// test double `CARGO_BIN_EXE_rbs-browser-helper` (tests), otherwise `rbs-browser-cef`
/// next to the app (inside the bundle on macOS). `rbs-browser-helper` next to the app
/// is not picked up.
pub fn resolve_runner() -> Option<Runner> {
    let from_env = |key: &str| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| path.is_file())
    };
    if let Some(program) = from_env("RBS_BROWSER_HELPER") {
        return Some(Runner {
            program,
            args: vec!["--hold".into()],
            needs_engine: false,
        });
    }
    if let Some(program) = from_env("CARGO_BIN_EXE_rbs-browser-helper") {
        return Some(Runner {
            program,
            args: vec!["--hold".into()],
            needs_engine: false,
        });
    }
    let mut dir = std::env::current_exe().ok()?;
    dir.pop();
    shipped_runner(&dir).map(|program| Runner {
        program,
        args: Vec::new(),
        needs_engine: true,
    })
}

/// `rbs-browser-cef` in `dir` (the app's directory), or inside its bundle on macOS.
fn shipped_runner(dir: &Path) -> Option<PathBuf> {
    let name = format!("{RUNNER}{}", std::env::consts::EXE_SUFFIX);
    let flat = dir.join(&name);
    if flat.is_file() {
        return Some(flat);
    }
    // macOS: the runner is an app bundle with its helpers (ADR-0132 decision 1).
    let bundled = dir
        .join(format!("{RUNNER}.app"))
        .join("Contents")
        .join("MacOS")
        .join(&name);
    (cfg!(target_os = "macos") && bundled.is_file()).then_some(bundled)
}

/// The installed browser engine as the runner needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRuntime {
    /// The installed version's directory (it holds the marker).
    pub root: PathBuf,
    /// The directory that holds the library.
    pub library_dir: PathBuf,
}

/// The engine slot the host sets after resolving the add-on, and the tree reads at
/// each start. Shared by one host and its tree.
#[derive(Debug, Default)]
pub struct EngineSlot {
    runtime: Mutex<Option<EngineRuntime>>,
}

impl EngineSlot {
    /// Sets or clears the installed engine.
    pub fn set(&self, runtime: Option<EngineRuntime>) {
        if let Ok(mut slot) = self.runtime.lock() {
            *slot = runtime;
        }
    }

    /// The installed engine, if any.
    pub fn get(&self) -> Option<EngineRuntime> {
        self.runtime.lock().ok().and_then(|slot| slot.clone())
    }
}

/// Starts the runner for each browser source.
#[derive(Debug)]
pub struct RunnerBrowserTree {
    runner: Option<Runner>,
    engine: Arc<EngineSlot>,
    /// Sessions alive now; the add-on is in use while it is above 0.
    live: Arc<AtomicUsize>,
    /// `browser-profiles/` in the local data directory (ADR-0132 decision 10);
    /// `None`: every page gets a fresh profile.
    profiles: Option<PathBuf>,
}

impl RunnerBrowserTree {
    /// A tree with `runner` (none: every start is `helper_missing`) that reads the
    /// engine from `engine`.
    pub fn new(runner: Option<Runner>, engine: Arc<EngineSlot>) -> Self {
        Self {
            runner,
            engine,
            live: Arc::new(AtomicUsize::new(0)),
            profiles: None,
        }
    }

    /// Keeps signed-in profiles under `dir` (`browser-profiles/` in the local data
    /// directory).
    pub fn with_profiles(mut self, dir: PathBuf) -> Self {
        self.profiles = Some(dir);
        self
    }

    /// True while a runner this tree started is alive (or still stopping).
    pub fn in_use(&self) -> bool {
        self.live.load(Ordering::Acquire) > 0
    }

    /// True when a start would be refused only for want of the engine add-on.
    pub fn needs_engine(&self) -> bool {
        self.runner.as_ref().is_some_and(|r| r.needs_engine) && self.engine.get().is_none()
    }

    fn env(&self) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        self.engine
            .get()
            .map(|rt| runtime_env(&rt.root, &rt.library_dir))
            .unwrap_or_default()
    }
}

impl BrowserTree for RunnerBrowserTree {
    fn unavailable(&self) -> Option<&'static str> {
        if self.runner.is_none() {
            return Some(HELPER_MISSING);
        }
        self.needs_engine().then_some(BROWSER_RUNTIME_MISSING)
    }

    fn profile_dir(&self, profile_id: &str) -> Option<String> {
        // Only a server-made id names a directory, so no setting reaches another path.
        if !rbs_sources::is_profile_id(profile_id) {
            return None;
        }
        let dir = self.profiles.as_ref()?.join(profile_id);
        Some(dir.to_string_lossy().into_owned())
    }

    fn start(
        &self,
        config: &Configure,
        sink: Arc<BrowserFrameSink>,
        sound: Option<Arc<BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| "no browser runner was found next to the app".to_owned())?;
        let args: Vec<&str> = runner.args.iter().map(String::as_str).collect();
        let child = spawn_helper_with_env(
            &runner.program,
            &args,
            &self.env(),
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
        let publish = move |rgba: &mut Vec<u8>, width, height, painted, fresh| {
            let stamp = edge
                .as_mut()
                .map(|edge| edge.stamp(painted, rbs_browser_shm::os_monotonic_nanos()));
            if fresh {
                sink.publish_frame(rgba, width, height, stamp);
            } else {
                sink.republish();
            }
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
        Ok(Box::new(RunnerSession {
            supervisor,
            _live: Live::new(&self.live),
        }))
    }
}

/// Counts one live session; the count drops when the session does.
#[derive(Debug)]
struct Live(Arc<AtomicUsize>);

impl Live {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(count))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
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

struct RunnerSession {
    supervisor: Supervisor,
    _live: Live,
}

impl BrowserSession for RunnerSession {
    fn poll(&mut self) -> Option<i32> {
        // An exit or a lost heartbeat (ADR-0109 decision 8), seen by the supervisor.
        self.supervisor.exited().then_some(1)
    }

    fn stop(&mut self) {
        // Never waits: the supervisor thread closes the helper (ADR-0109 decision 8).
        self.supervisor.stop_detached();
    }

    fn send(&mut self, message: &Control) -> bool {
        self.supervisor.send(message)
    }

    fn set_shown(&mut self, shown: bool) {
        // The supervisor tells the helper and changes its own read rates (ADR-0149
        // decision 5).
        let _ = self.supervisor.set_shown(shown);
    }

    fn exit_reason(&self) -> Option<&'static str> {
        match self.supervisor.exit_cause() {
            Cause::RuntimeMissing => Some(BROWSER_RUNTIME_MISSING),
            Cause::RuntimeMismatch => Some(BROWSER_RUNTIME_MISMATCH),
            Cause::Other => None,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
    use super::*;

    fn tree(needs_engine: bool) -> RunnerBrowserTree {
        RunnerBrowserTree::new(
            Some(Runner {
                program: PathBuf::from("runner"),
                args: Vec::new(),
                needs_engine,
            }),
            Arc::new(EngineSlot::default()),
        )
    }

    #[test]
    fn the_shipped_runner_needs_the_engine_and_an_override_does_not() {
        let shipped = tree(true);
        assert_eq!(shipped.unavailable(), Some(BROWSER_RUNTIME_MISSING));
        assert!(shipped.env().is_empty());
        shipped.engine.set(Some(EngineRuntime {
            root: PathBuf::from("/rt"),
            library_dir: PathBuf::from("/rt/Release"),
        }));
        assert_eq!(shipped.unavailable(), None);
        let env = shipped.env();
        assert_eq!(env[0].0, rbs_browser_ipc::runtime::RUNTIME_ENV);
        assert_eq!(env[0].1, "/rt");
        if cfg!(target_os = "linux") {
            assert_eq!(env[1], ("LD_LIBRARY_PATH".into(), "/rt/Release".into()));
        }
        assert_eq!(tree(false).unavailable(), None);
        let none = RunnerBrowserTree::new(None, Arc::new(EngineSlot::default()));
        assert_eq!(none.unavailable(), Some(HELPER_MISSING));
    }

    #[test]
    fn a_kept_profile_lives_under_the_profiles_directory() {
        let id = "0123456789abcdef0123456789abcdef";
        assert_eq!(tree(false).profile_dir(id), None);
        let kept = tree(false).with_profiles(PathBuf::from("/data/browser-profiles"));
        assert_eq!(
            kept.profile_dir(id).map(PathBuf::from),
            Some(PathBuf::from("/data/browser-profiles").join(id))
        );
        assert_eq!(kept.profile_dir("../escape"), None);
    }

    #[test]
    fn the_test_double_next_to_the_app_is_not_the_runner() {
        let dir = std::env::temp_dir().join(format!("rbs-runner-pick-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = std::env::consts::EXE_SUFFIX;
        std::fs::write(dir.join(format!("rbs-browser-helper{exe}")), b"").unwrap();
        assert_eq!(shipped_runner(&dir), None);
        std::fs::write(dir.join(format!("rbs-browser-cef{exe}")), b"").unwrap();
        assert_eq!(
            shipped_runner(&dir),
            Some(dir.join(format!("rbs-browser-cef{exe}")))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_live_count_follows_the_sessions() {
        let count = Arc::new(AtomicUsize::new(0));
        let a = Live::new(&count);
        let b = Live::new(&count);
        assert_eq!(count.load(Ordering::Acquire), 2);
        drop(a);
        drop(b);
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
