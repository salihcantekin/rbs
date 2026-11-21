//! Wires the browser runner into the studio (ADR-0109, ADR-0145 decisions 7 and 8).
//!
//! The desktop app does not link CEF. It starts the runner `rbs-browser-cef`, which
//! ships next to the app and loads the browser engine add-on from the directory the
//! studio names in its environment (add-ons spec "Browser engine consumer"). Tests and
//! developers point `RBS_BROWSER_HELPER` at another runner; the test double
//! `rbs-browser-helper` is found through `CARGO_BIN_EXE_rbs-browser-helper` only.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::studio_host::{BrowserTrees, HostBrowserTree};
use rbs_browser_ipc::Configure;
use rbs_browser_ipc::control::Control;
use rbs_browser_ipc::exit::Cause;
use rbs_browser_runner::{PagePicture, PageSound, Supervisor, runtime_env, spawn_helper_prepared};
use rbs_browser_shm::OwnedSoundPacket;
use rbs_browser_shm::SlotLayout;
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

pub use crate::studio_host::{EngineRuntime, EngineSlot, GPU_FALLBACK, page_reason};

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
    /// Render adapter. `None`: every page stays on the shared-memory path.
    adapter: Option<u64>,
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
            adapter: None,
        }
    }

    /// Keeps signed-in profiles under `dir` (`browser-profiles/` in the local data
    /// directory).
    pub fn with_profiles(mut self, dir: PathBuf) -> Self {
        self.profiles = Some(dir);
        self
    }

    /// Pages on this adapter copy into studio textures (ADR-0109 decision 8,
    /// ADR-0157). [`RunnerTrees`] names the studio's adapter.
    pub fn with_adapter(mut self, luid: u64) -> Self {
        self.adapter = Some(luid);
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

impl HostBrowserTree for RunnerBrowserTree {
    fn in_use(&self) -> bool {
        RunnerBrowserTree::in_use(self)
    }

    fn needs_engine(&self) -> bool {
        RunnerBrowserTree::needs_engine(self)
    }
}

/// The adapter a tree's GPU pages copy on: the D3D LUID on Windows, the `MTLDevice`
/// registry id on macOS when the system has exactly one Metal device (ADR-0167
/// decision 2). `None`: shared memory.
fn gpu_adapter(gpu: &rbs_gpu::GpuContext) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        if rbs_gpu::browser_surfaces::metal_device_count() != 1 {
            return None;
        }
        rbs_gpu::browser_surfaces::metal_registry_id(gpu)
    }
    #[cfg(not(target_os = "macos"))]
    gpu.adapter_luid()
}

/// The studio host's browser trees in this root: a [`RunnerBrowserTree`] with the
/// runner of [`resolve_runner`], or the one given (ADR-0159 decision 4).
#[derive(Debug, Clone, Default)]
pub struct RunnerTrees {
    /// `None`: every start is `helper_missing`.
    pub runner: Option<Runner>,
}

impl RunnerTrees {
    /// The runner this process starts ([`resolve_runner`]).
    pub fn resolved() -> Self {
        Self {
            runner: resolve_runner(),
        }
    }
}

impl BrowserTrees for RunnerTrees {
    fn create(
        &self,
        engine: Arc<EngineSlot>,
        profiles: Option<PathBuf>,
        gpu: &Arc<rbs_gpu::GpuContext>,
    ) -> Arc<dyn HostBrowserTree> {
        let mut tree = RunnerBrowserTree::new(self.runner.clone(), engine);
        if let Some(dir) = profiles {
            tree = tree.with_profiles(dir);
        }
        // Pages copy into textures on the studio's adapter (ADR-0157, accepted
        // 2026-11-21) where it has a LUID, which is on Windows only. On macOS they
        // copy into IOSurfaces when the system has one Metal device (ADR-0167).
        // `RBS_BROWSER_SHARED_MEMORY` keeps every page on shared memory, for a
        // device check or a driver that misbehaves.
        if std::env::var_os("RBS_BROWSER_SHARED_MEMORY").is_none_or(|v| v.is_empty())
            && let Some(luid) = gpu_adapter(gpu)
        {
            tree = tree.with_adapter(luid);
        }
        Arc::new(tree)
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
        self.start_on(config, sink, sound, self.adapter)
    }

    fn start_on_shared_memory(
        &self,
        config: &Configure,
        sink: Arc<BrowserFrameSink>,
        sound: Option<Arc<BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        self.start_on(config, sink, sound, None)
    }
}

impl RunnerBrowserTree {
    /// Starts a tree; with `adapter`, its pages copy into studio textures there.
    fn start_on(
        &self,
        config: &Configure,
        sink: Arc<BrowserFrameSink>,
        sound: Option<Arc<BrowserSound>>,
        adapter: Option<u64>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = adapter;
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| "no browser runner was found next to the app".to_owned())?;
        let args: Vec<&str> = runner.args.iter().map(String::as_str).collect();
        #[cfg(windows)]
        let textures = adapter.and_then(|luid| {
            crate::browser_gpu::sweep();
            rbs_gpu::browser_pages::PageTextures::create(
                luid,
                config.width,
                config.height,
                rbs_browser_shm::DEFAULT_GPU_SLOTS as usize,
            )
            .ok()
        });
        // macOS: the slot surfaces, offered to the helper on a Mach service once its
        // pid is known (ADR-0167 decision 3).
        #[cfg(target_os = "macos")]
        let surfaces = adapter.and_then(|_| {
            rbs_gpu::browser_surfaces::PageSurfaces::create(
                config.width,
                config.height,
                rbs_browser_shm::DEFAULT_GPU_SLOTS as usize,
            )
            .ok()
        });
        #[cfg(target_os = "macos")]
        let mut offer: Option<crate::browser_metal::Offer> = None;
        #[allow(unused_mut)] // Only Windows and macOS set a gate.
        let mut child = spawn_helper_prepared(
            &runner.program,
            &args,
            &self.env(),
            config,
            SlotLayout::Rgba8Straight,
            |pid| {
                #[cfg(windows)]
                {
                    textures.as_ref().and_then(|textures| {
                        let (handles, fence) = textures.duplicate_to(pid)?;
                        let mut share = rbs_browser_ipc::D3dShare {
                            adapter_luid: textures.luid(),
                            fence,
                            textures: [0; rbs_browser_ipc::MAX_GPU_TEXTURES],
                            count: 0,
                        };
                        for (to, from) in share.textures.iter_mut().zip(&handles) {
                            *to = *from;
                            share.count += 1;
                        }
                        Some(rbs_browser_ipc::GpuShare::D3d(share))
                    })
                }
                #[cfg(target_os = "macos")]
                {
                    let made = surfaces
                        .as_ref()
                        .zip(adapter)
                        .and_then(|(s, id)| Some((crate::browser_metal::Offer::new(s, pid)?, id)));
                    made.map(|(made, registry_id)| {
                        let share = made.share(registry_id);
                        offer = Some(made);
                        share
                    })
                }
                #[cfg(not(any(windows, target_os = "macos")))]
                {
                    let _ = pid;
                    None
                }
            },
        )?;
        #[cfg(windows)]
        let slots = textures.and_then(|textures| {
            crate::browser_gpu::PageSlots::open(
                child.frame_name(),
                config.width,
                config.height,
                textures,
            )
            .ok()
        });
        // The fence gate hands a picture on only once its copy has finished
        // (ADR-0157 decision 2). Only a page with textures gets one.
        #[cfg(windows)]
        if child.gpu_requested() {
            let Some(slots) = &slots else {
                // The helper copies into textures the studio cannot show: start
                // again without them.
                child.close(std::time::Duration::ZERO);
                return self.start_on(config, sink, sound, None);
            };
            // A picture of the previous tree still being imported can add one stale
            // failure, never ten.
            sink.clear_failed_imports();
            child.set_gpu_gate(
                Box::new(Fence(slots.fence_read(), Arc::clone(&sink))),
                config.fps,
            );
        }
        #[cfg(target_os = "macos")]
        let slots = surfaces.and_then(|surfaces| {
            crate::browser_metal::PageSlots::open(
                child.frame_name(),
                config.width,
                config.height,
                surfaces,
            )
            .ok()
        });
        // The helper publishes only finished copies, so the gate has no fence
        // (ADR-0167 decision 6); the surfaces stay on offer until it takes them.
        #[cfg(target_os = "macos")]
        if child.gpu_requested() {
            let (Some(_), Some(offer)) = (&slots, offer.take()) else {
                child.close(std::time::Duration::ZERO);
                return self.start_on(config, sink, sound, None);
            };
            sink.clear_failed_imports();
            child.set_fenceless_gate(Box::new(Imports(Arc::clone(&sink))), config.fps);
            child.set_handover(Box::new(offer));
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        let slots = None::<()>;
        // The supervisor thread exists only while the source is Active (P13). It
        // stamps each picture on the master clock at the edge, by the rule the page's
        // sound follows, so the render thread shows it with that sound (ADR-0109
        // decision 8). Without a sound node there is no clock: pictures are due at once.
        let mut edge = sound
            .as_ref()
            .map(|sound| BrowserPictureEdge::new(sound.clock()));
        #[cfg(any(windows, target_os = "macos"))]
        let kept = slots.clone();
        #[cfg(not(any(windows, target_os = "macos")))]
        let _ = slots;
        let publish = move |rgba: &mut Vec<u8>, width, height, painted, picture: PagePicture| {
            let stamp = edge
                .as_mut()
                .map(|edge| edge.stamp(painted, rbs_browser_shm::os_monotonic_nanos()));
            match picture {
                PagePicture::Bytes { fresh: true } => {
                    sink.publish_frame(rgba, width, height, stamp);
                }
                PagePicture::Bytes { fresh: false } => sink.republish(),
                PagePicture::Texture { slot, .. } => {
                    #[cfg(any(windows, target_os = "macos"))]
                    if let Some(frame) = kept.as_ref().and_then(|s| s.picture(slot, stamp)) {
                        sink.publish_native(frame, stamp);
                    }
                    #[cfg(not(any(windows, target_os = "macos")))]
                    {
                        let _ = (slot, stamp);
                    }
                }
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
            #[cfg(any(windows, target_os = "macos"))]
            slots,
        }))
    }
}

/// The supervisor's view of a macOS page: no fence, only the render thread's failed
/// imports of its pictures (ADR-0167 decision 6).
#[cfg(target_os = "macos")]
struct Imports(Arc<BrowserFrameSink>);

#[cfg(target_os = "macos")]
impl rbs_browser_runner::FenceView for Imports {
    fn completed(&self) -> u64 {
        0
    }

    fn failed_imports(&self) -> u32 {
        self.0.failed_imports()
    }
}

/// The supervisor's view of a page's shared fence (ADR-0157 decision 2) and of the
/// render thread's failed imports of its pictures (decision 9).
#[cfg(windows)]
struct Fence(rbs_gpu::browser_pages::FenceRead, Arc<BrowserFrameSink>);

#[cfg(windows)]
impl rbs_browser_runner::FenceView for Fence {
    fn completed(&self) -> u64 {
        self.0.completed()
    }

    fn failed_imports(&self) -> u32 {
        self.1.failed_imports()
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

    fn packet(&mut self, packet: OwnedSoundPacket) -> Vec<f32> {
        self.0.take_packet(
            packet.os_nanos,
            packet.rate,
            packet.channels,
            packet.samples,
        )
    }

    fn pump(&mut self) {
        self.0.pump();
    }
}

struct RunnerSession {
    supervisor: Supervisor,
    _live: Live,
    #[cfg(windows)]
    slots: Option<std::sync::Arc<crate::browser_gpu::PageSlots>>,
    /// macOS: the surfaces, freed when the last frame lease goes (the importer's
    /// cached texture retains each surface, so no handle hold is needed, decision 8).
    #[cfg(target_os = "macos")]
    #[allow(dead_code)] // Held, not read: it keeps the surfaces for the session's life.
    slots: Option<std::sync::Arc<crate::browser_metal::PageSlots>>,
}

/// A session dropped without [`BrowserSession::stop`] still buries its slots, so a
/// page's textures and handles are never freed while a frame may hold them (ADR-0157
/// decision 13, round-5 review).
#[cfg(windows)]
impl Drop for RunnerSession {
    fn drop(&mut self) {
        if let Some(slots) = self.slots.take() {
            crate::browser_gpu::bury(slots);
        }
    }
}

impl BrowserSession for RunnerSession {
    fn poll(&mut self) -> Option<i32> {
        // An exit or a lost heartbeat (ADR-0109 decision 8), seen by the supervisor.
        self.supervisor.exited().then_some(1)
    }

    fn stop(&mut self) {
        // Never waits: the supervisor thread closes the helper (ADR-0109 decision 8).
        self.supervisor.stop_detached();
        #[cfg(windows)]
        if let Some(slots) = self.slots.take() {
            crate::browser_gpu::bury(slots);
        }
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
            Cause::GpuFallback | Cause::Other => None,
        }
    }

    fn gpu_fallback(&self) -> bool {
        self.supervisor.exit_cause() == Cause::GpuFallback
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
