//! What a composition root hands the studio host instead of the host naming an adapter
//! (ADR-0159 decision 4): each is a [`super::HostConfig`] field.

use std::fmt::Debug;
use std::fs::File;

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
