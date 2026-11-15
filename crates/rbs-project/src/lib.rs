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
mod xmeml;

use std::fmt;
use std::path::{Path, PathBuf};

use rbs_manifest::RecordingManifest;

pub use url::MediaFolder;

/// What a project writes besides the files and the markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// One marker per pause, on its join ("Paused 2:10", the time left out), from
    /// the recording setting "Pauses become markers in the editor project" (ADR-0124
    /// decision 10). On by default.
    pub pause_markers: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            pause_markers: true,
        }
    }
}

/// A marker of the project, in file time from `t0`.
pub(crate) struct ProjectMarker {
    pub(crate) t_ns: u64,
    pub(crate) label: String,
    /// A chapter marker (a scene change).
    pub(crate) chapter: bool,
}

/// The manifest's markers and, with `pause_markers`, one per pause on its join; in
/// time order, a pause before the markers on its join.
pub(crate) fn project_markers(
    manifest: &RecordingManifest,
    options: Options,
) -> Vec<ProjectMarker> {
    let pauses = manifest
        .pauses
        .iter()
        .filter(|_| options.pause_markers)
        .map(|p| {
            (
                (p.at_file_ns, 0u8, 0u32),
                ProjectMarker {
                    t_ns: p.at_file_ns,
                    label: format!("Paused {}", clock_text(p.length_ns())),
                    chapter: false,
                },
            )
        });
    let markers = manifest.markers.iter().map(|m| {
        (
            (m.t_ns, 1u8, m.index),
            ProjectMarker {
                t_ns: m.t_ns,
                label: m.label.clone(),
                chapter: m.kind == rbs_manifest::MarkerKind::Scene,
            },
        )
    });
    let mut all: Vec<_> = pauses.chain(markers).collect();
    all.sort_by_key(|(key, _)| *key);
    all.into_iter().map(|(_, m)| m).collect()
}

/// `m:ss`, or `h:mm:ss` from an hour, whole seconds rounded down.
fn clock_text(ns: u64) -> String {
    let s = ns / 1_000_000_000;
    let (h, m, s) = (s / 3600, s / 60 % 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A project format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// FCPXML 1.9, one `.fcpxml` file (Final Cut Pro). DaVinci Resolve 21 imports
    /// its clips but drops its markers.
    Fcpxml,
    /// FCP7 XML (`xmeml` version 5), one `.xml` file (DaVinci Resolve with markers,
    /// Premiere Pro).
    Xmeml,
}

impl Format {
    /// The file extension, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Fcpxml => "fcpxml",
            Self::Xmeml => "xml",
        }
    }

    /// Parses a control API or command line name (`fcpxml`, `xmeml`).
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "fcpxml" => Some(Self::Fcpxml),
            "xmeml" => Some(Self::Xmeml),
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
    /// The format cannot express the recording's frame rate (xmeml takes whole and
    /// NTSC rates only), as `num/den`.
    UnsupportedRate(String),
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
            Self::UnsupportedRate(rate) => {
                write!(f, "this project format cannot hold the frame rate {rate}")
            }
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
    build_with(manifest, folder, format, title, Options::default())
}

/// [`build`] with `options`.
pub fn build_with(
    manifest: &RecordingManifest,
    folder: &MediaFolder,
    format: Format,
    title: &str,
    options: Options,
) -> Result<String, ProjectError> {
    match format {
        Format::Fcpxml => fcpxml::write(manifest, folder, title, options),
        Format::Xmeml => xmeml::write(manifest, folder, title, options),
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
    export_with(folder, manifest_name, format, strict, Options::default())
}

/// [`export`] with `options`.
pub fn export_with(
    folder: &Path,
    manifest_name: &str,
    format: Format,
    strict: bool,
    options: Options,
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
    let project = build_with(&manifest, &media, format, stem, options)?;
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
