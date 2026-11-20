//! What a composition root hands the studio host instead of the host naming an adapter
//! (ADR-0159 decision 4): each is a [`super::HostConfig`] field.

use std::fmt::Debug;
use std::fs::File;
use std::sync::Arc;

use rbs_core::media::ContainerId;
use rbs_sources::SourceTypes;

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
