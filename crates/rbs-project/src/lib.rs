//! Editor projects from a recording manifest (spec `docs/specs/recording.md`,
//! "Editor project"; ADR-0089 decisions 7 to 10).
//!
//! [`build`] is pure: a complete [`RecordingManifest`], the folder the files are in
//! and a [`Format`] give the project text. It never opens a media file; times, sizes
//! and rates come from the manifest. [`export`] does the I/O around it: it reads the
//! manifest, leaves out missing separate files, and writes the project next to the
//! manifest under a fresh name.

mod fcpxml;
mod time;
mod url;

use std::fmt;
use std::path::{Path, PathBuf};

use rbs_manifest::RecordingManifest;

pub use url::MediaFolder;

/// A project format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// FCPXML 1.9, one `.fcpxml` file (Final Cut Pro, DaVinci Resolve).
    Fcpxml,
}

impl Format {
    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Fcpxml => "fcpxml",
        }
    }

    /// Parses a control API or command line name (`fcpxml`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fcpxml" => Some(Self::Fcpxml),
            _ => None,
        }
    }
}

/// Why no project was made. Messages have no path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProjectError {
    /// The manifest has no stop rewrite (`complete = true`), for example after a crash.
    Incomplete,
    /// A file the project needs has no timing or format in the manifest.
    MissingTiming(String),
    /// The manifest has no master file.
    NoMaster,
    /// The master file is not in the folder.
    MissingMaster,
    /// The master has no picture; a sound-only recording has no project format yet.
    NoPicture,
    /// A file has a role this build does not place.
    UnknownRole(String),
    /// The media folder is not an absolute path.
    NotAbsolute,
    /// `--strict` and a separate file is missing.
    MissingFile(String),
    /// The manifest could not be read.
    Manifest(rbs_manifest::ReadError),
    /// Reading the manifest or writing the project failed.
    Io(std::io::ErrorKind),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Incomplete => {
                f.write_str("the recording did not finish cleanly, so its manifest has no timing")
            }
            Self::MissingTiming(name) => write!(f, "the manifest has no timing for {name}"),
            Self::NoMaster => f.write_str("the manifest names no master file"),
            Self::MissingMaster => f.write_str("the master file is not in the folder"),
            Self::NoPicture => f.write_str("a recording without a picture has no project yet"),
            Self::UnknownRole(role) => write!(f, "a file has the unknown role {role}"),
            Self::NotAbsolute => f.write_str("the recording folder is not an absolute path"),
            Self::MissingFile(name) => write!(f, "{name} is not in the folder"),
            Self::Manifest(e) => e.fmt(f),
            Self::Io(kind) => write!(f, "the project could not be written ({kind})"),
        }
    }
}

impl std::error::Error for ProjectError {}

/// The project text for `manifest`, whose files are in `folder`. `title` names the
/// editor's event and project. Every file the manifest lists is used.
pub fn build(
    manifest: &RecordingManifest,
    folder: &MediaFolder,
    format: Format,
    title: &str,
) -> Result<String, ProjectError> {
    match format {
        Format::Fcpxml => fcpxml::write(manifest, folder, title),
    }
}

/// What [`export`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exported {
    /// The project's path.
    pub path: PathBuf,
    /// Separate files the manifest names that were not in the folder (left out).
    pub missing: Vec<String>,
}

/// Reads `folder/manifest_name`, builds the project and writes it next to the
/// manifest as `<stem>.<ext>`, or `<stem> 2.<ext>`, ... when that exists; nothing is
/// ever replaced. A missing separate file is left out and listed, or with `strict`
/// is [`ProjectError::MissingFile`]. A missing master is always an error.
pub fn export(
    folder: &Path,
    manifest_name: &str,
    format: Format,
    strict: bool,
) -> Result<Exported, ProjectError> {
    let media = MediaFolder::from_path(folder)?;
    let path = folder.join(manifest_name);
    let size = std::fs::metadata(&path)
        .map_err(|e| ProjectError::Io(e.kind()))?
        .len();
    if size > rbs_manifest::MAX_SIZE {
        return Err(ProjectError::Io(std::io::ErrorKind::InvalidData));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| ProjectError::Io(e.kind()))?;
    let mut manifest = RecordingManifest::from_toml(&text).map_err(ProjectError::Manifest)?;
    if !manifest.is_complete() {
        return Err(ProjectError::Incomplete);
    }
    let mut missing = Vec::new();
    let mut kept = Vec::with_capacity(manifest.files.len());
    for file in manifest.files {
        if folder.join(&file.file_name).is_file() {
            kept.push(file);
        } else if file.role == "master" {
            return Err(ProjectError::MissingMaster);
        } else if strict {
            return Err(ProjectError::MissingFile(file.file_name));
        } else {
            missing.push(file.file_name);
        }
    }
    manifest.files = kept;
    let stem = manifest_name
        .strip_suffix(rbs_manifest::FILE_SUFFIX)
        .unwrap_or(manifest_name);
    let stem = if stem == "recording" {
        folder.file_name().and_then(|n| n.to_str()).unwrap_or(stem)
    } else {
        stem
    };
    let project = build(&manifest, &media, format, stem)?;
    let path = fresh_path(folder, stem, format.extension());
    write_new(&path, &project)?;
    Ok(Exported { path, missing })
}

fn fresh_path(folder: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut n = 1u32;
    loop {
        let name = if n == 1 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem} {n}.{ext}")
        };
        let path = folder.join(name);
        if !path.exists() || n == u32::MAX {
            return path;
        }
        n += 1;
    }
}

/// Creates `path`, failing when something appeared there meanwhile.
fn write_new(path: &Path, text: &str) -> Result<(), ProjectError> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| ProjectError::Io(e.kind()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| ProjectError::Io(e.kind()))
}
