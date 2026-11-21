//! What a composition root hands the studio host instead of the host naming an adapter
//! (ADR-0159 decision 4): each is a [`super::HostConfig`] field.

use std::fmt::Debug;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rbs_core::media::ContainerId;
use rbs_gpu::GpuContext;
use rbs_sources::{
    BrowserFrameSink, BrowserSession, BrowserSound, BrowserTree, HELPER_MISSING, SourceTypes,
};

use super::{MainDisplay, SourceKind};

/// One marker as a chapter of a finished file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chapter<'a> {
    /// From the file's first presentation time, in nanoseconds.
    pub start_ns: u64,
    /// The marker's label.
    pub title: &'a str,
}

/// What [`ChapterWriter::append`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The chapters are in the file.
    Written,
    /// The file takes no chapters (another muxer wrote it, or it could not seek); the
    /// manifest keeps the markers.
    NotSupported,
}

/// Writes markers into a finished recording as chapters (ADR-0089 decision 14).
pub trait ChapterWriter: Send + Sync + Debug {
    /// Appends `chapters`, sorted by start, to `file`. An error is a failed read or
    /// write, in words.
    fn append(&self, file: &mut File, chapters: &[Chapter<'_>]) -> Result<Applied, String>;
}

/// Reads a recording folder's free space for the estimate
/// ([`super::HostConfig::read_free_space`]).
#[derive(Clone)]
pub struct FreeSpaceReader(pub rbs_record::estimate::ReadFreeSpace);

impl Debug for FreeSpaceReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FreeSpaceReader")
    }
}

/// The host's copy of the seams in its [`super::HostConfig`].
pub(super) struct HostSeams {
    /// [`super::HostConfig::chapters`].
    pub chapters: Option<Arc<dyn ChapterWriter>>,
    /// [`super::HostConfig::os_version`], empty when unknown.
    pub os_version: String,
}

impl HostSeams {
    pub(super) fn new(chapters: Option<Arc<dyn ChapterWriter>>, os: Option<String>) -> Self {
        Self {
            chapters,
            os_version: os.unwrap_or_default(),
        }
    }

    /// The chapter writer for a master in `container`: only Matroska takes chapters.
    pub(super) fn chapters_for(&self, container: ContainerId) -> Option<Arc<dyn ChapterWriter>> {
        self.chapters
            .clone()
            .filter(|_| container == ContainerId::Matroska)
    }

    /// The calibration key's computer part: the GPU adapter's key and the OS version.
    pub(super) fn computer_key(&self, adapter: &str) -> String {
        format!("{adapter}|{}", self.os_version)
    }
}

/// Reads the main display for the Video settings row at start (ADR-0102 decision 8,
/// [`super::HostConfig::main_display`]).
pub type MainDisplayProbe = fn(&SourceTypes) -> Option<MainDisplay>;

/// The default screen target's pixel size and scale (the first screen without a
/// default). Not for macOS, where listing screens opens ScreenCaptureKit shareable
/// content: the root passes the Core Graphics framebuffer size there.
pub fn display_from_default_screen(sources: &SourceTypes) -> Option<MainDisplay> {
    let screens = sources
        .targets(&SourceKind::Screen.type_id())
        .unwrap_or_default();
    screens
        .iter()
        .find(|t| t.is_default)
        .or_else(|| screens.first())
        .and_then(|t| {
            t.size.map(|size| MainDisplay {
                size,
                scale: t.scale,
            })
        })
}

/// The main display at start, from the root's probe.
pub(super) fn probe_display(
    probe: Option<MainDisplayProbe>,
    sources: &SourceTypes,
) -> Option<MainDisplay> {
    probe.and_then(|probe| probe(sources))
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

/// A browser tree as the host keeps it: the sources' tree, and the two questions the
/// add-ons code asks.
pub trait HostBrowserTree: BrowserTree {
    /// True while a helper the tree started is alive (or still stopping).
    fn in_use(&self) -> bool;
    /// True when a start would be refused only for want of the engine add-on.
    fn needs_engine(&self) -> bool;
}

/// Makes the browser tree for a studio start ([`super::HostConfig::browser`]), after
/// the GPU context exists, so a root can read the adapter from it.
pub trait BrowserTrees: Send + Sync + Debug {
    /// The tree that reads the installed engine from `engine` and keeps signed-in
    /// profiles under `profiles`.
    fn create(
        &self,
        engine: Arc<EngineSlot>,
        profiles: Option<PathBuf>,
        gpu: &Arc<GpuContext>,
    ) -> Arc<dyn HostBrowserTree>;
}

/// The tree without a browser helper: every start is `helper_missing`, as a runner
/// tree without a runner answers.
#[derive(Debug, Default)]
pub(super) struct NoRunnerTree {
    profiles: Option<PathBuf>,
}

impl BrowserTree for NoRunnerTree {
    fn start(
        &self,
        _config: &rbs_browser_ipc::Configure,
        _sink: Arc<BrowserFrameSink>,
        _sound: Option<Arc<BrowserSound>>,
    ) -> Result<Box<dyn BrowserSession>, String> {
        Err(HELPER_MISSING.to_owned())
    }

    fn unavailable(&self) -> Option<&'static str> {
        Some(HELPER_MISSING)
    }

    fn profile_dir(&self, profile_id: &str) -> Option<String> {
        if !rbs_sources::is_profile_id(profile_id) {
            return None;
        }
        let dir = self.profiles.as_ref()?.join(profile_id);
        Some(dir.to_string_lossy().into_owned())
    }
}

impl HostBrowserTree for NoRunnerTree {
    fn in_use(&self) -> bool {
        false
    }

    fn needs_engine(&self) -> bool {
        false
    }
}

/// The tree for a start: the root's, or [`NoRunnerTree`] without one.
pub(super) fn browser_tree(
    trees: Option<&Arc<dyn BrowserTrees>>,
    engine: Arc<EngineSlot>,
    profiles: Option<PathBuf>,
    gpu: &Arc<GpuContext>,
) -> Arc<dyn HostBrowserTree> {
    match trees {
        Some(trees) => trees.create(engine, profiles, gpu),
        None => Arc::new(NoRunnerTree { profiles }),
    }
}
