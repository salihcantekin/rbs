//! The recording manifest (spec `docs/specs/recording.md`, "Recording manifest";
//! ADR-0086 decision 6, ADR-0089 decisions 2, 5 and 6).
//!
//! A recording writes `recording.rbs.toml` (a recording folder) or
//! `<file stem>.rbs.toml` (a plain recording) next to its media: when the time
//! origin `t0` was, which files belong to it, and the markers. The stop rewrite adds
//! each file's timing and format and sets `complete`, which the editor project writer
//! (`rbs-project`) needs. The text has `format` and `kind` like every RBS document, no
//! directory and no secret.
//!
//! This crate is data only (no engine, port, adapter or UI crate). `rbs-record` builds
//! and writes manifests; `rbs-project` reads them.

mod markers;
mod read;

use std::fmt;
use std::path::Path;

use rbs_core::timing::FrameRate;

pub use markers::{
    MAX_LABEL_CHARS, MAX_MARKER_LEAD, MAX_MARKERS, Marker, MarkerColor, MarkerError, MarkerKind,
    MarkerLog, checked_label,
};
pub use read::ReadError;

/// The `kind` of a recording manifest.
pub const KIND: &str = "recording-manifest";
/// The format this build writes and reads.
pub const FORMAT: u32 = 1;
/// The manifest's name inside a recording folder.
pub const FOLDER_MANIFEST: &str = "recording.rbs.toml";
/// What a plain recording appends to its file stem.
pub const FILE_SUFFIX: &str = ".rbs.toml";
/// Largest manifest read, in bytes (10 000 markers of 200 characters fit).
pub const MAX_SIZE: u64 = 8 * 1024 * 1024;

/// A scene source's id as the manifest stores it (the scene model's `SourceId`
/// number).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceRef(u32);

impl SourceRef {
    /// The source with this number.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    /// The number.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// The picture of a file, written at stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Frames per second; the manifest stores the frame duration (`"1001/60000"`).
    pub frame_rate: FrameRate,
}

/// The sound of a file, written at stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sound {
    /// Samples per second.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
}

/// What the stop rewrite knows about one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTiming {
    /// The file's first sample, from `t0`, in nanoseconds.
    pub start_offset_ns: u64,
    /// The file's length in nanoseconds.
    pub duration_ns: u64,
    /// Its picture, when it has one.
    pub picture: Option<Picture>,
    /// Its sound, when it has one.
    pub sound: Option<Sound>,
}

/// One file in the recording manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// `"master"`, `"video"` or `"audio"`.
    pub role: String,
    /// Name inside the recording folder (with its extension once the file is open).
    pub file_name: String,
    /// Set for a separate track.
    pub source: Option<SourceRef>,
    /// The source's display name, for a separate track.
    pub source_name: Option<String>,
    /// Timing and format, written at stop.
    pub timing: Option<FileTiming>,
}

impl ManifestFile {
    /// A file as listed at start: no timing yet.
    pub fn listed(role: &str, file_name: &str) -> Self {
        Self {
            role: role.to_owned(),
            file_name: file_name.to_owned(),
            source: None,
            source_name: None,
            timing: None,
        }
    }
}

/// What the stop rewrite knows about the whole recording. Its presence is
/// `complete = true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    /// The recording's length from `t0`, in nanoseconds.
    pub duration_ns: u64,
    /// The canvas size and frame rate of the master, absent for a recording without
    /// a picture.
    pub picture: Option<Picture>,
}

/// The recording manifest.
///
/// It has no directory and no secret. `completion` and each file's `timing` are
/// absent until the recording stops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingManifest {
    /// Wall-clock UTC of `t0`, as the caller formatted it.
    pub t0_utc: String,
    /// `t0` on the master clock, in nanoseconds.
    pub t0_ns: u64,
    /// The master file, then each separate track.
    pub files: Vec<ManifestFile>,
    /// The markers, in the order added.
    pub markers: Vec<Marker>,
    /// Set by the stop rewrite.
    pub completion: Option<Completion>,
}

impl RecordingManifest {
    /// A manifest with no file, marker or completion.
    pub fn new(t0_utc: &str, t0_ns: u64) -> Self {
        Self {
            t0_utc: t0_utc.to_owned(),
            t0_ns,
            files: Vec::new(),
            markers: Vec::new(),
            completion: None,
        }
    }

    /// Whether the stop rewrite happened.
    pub fn is_complete(&self) -> bool {
        self.completion.is_some()
    }

    /// TOML with `format` and `kind`. Names are file names only. Fields that are not
    /// known yet are left out.
    pub fn to_toml(&self) -> String {
        let mut text = format!(
            "format = {FORMAT}\nkind = \"{KIND}\"\nt0_utc = {}\nt0_ns = {}\n",
            toml_string(&self.t0_utc),
            self.t0_ns
        );
        if let Some(done) = &self.completion {
            text.push_str("complete = true\n");
            text.push_str(&format!("duration_ns = {}\n", done.duration_ns));
            if let Some(p) = &done.picture {
                push_picture(&mut text, p);
            }
        }
        for file in &self.files {
            text.push_str("\n[[files]]\n");
            text.push_str(&format!("role = {}\n", toml_string(&file.role)));
            text.push_str(&format!("name = {}\n", toml_string(&file.file_name)));
            if let Some(source) = file.source {
                text.push_str(&format!("source = {}\n", source.get()));
            }
            if let Some(name) = &file.source_name {
                text.push_str(&format!("source_name = {}\n", toml_string(name)));
            }
            if let Some(t) = &file.timing {
                text.push_str(&format!("start_offset_ns = {}\n", t.start_offset_ns));
                text.push_str(&format!("duration_ns = {}\n", t.duration_ns));
                if let Some(p) = &t.picture {
                    push_picture(&mut text, p);
                }
                if let Some(s) = &t.sound {
                    text.push_str(&format!("sample_rate = {}\n", s.sample_rate));
                    text.push_str(&format!("channels = {}\n", s.channels));
                }
            }
        }
        for m in &self.markers {
            text.push_str("\n[[markers]]\n");
            text.push_str(&format!("index = {}\n", m.index));
            text.push_str(&format!("t_ns = {}\n", m.t_ns));
            if let Some(p) = m.pressed_ns {
                text.push_str(&format!("pressed_ns = {p}\n"));
            }
            text.push_str(&format!("kind = \"{}\"\n", m.kind.as_str()));
            text.push_str(&format!("label = {}\n", toml_string(&m.label)));
            text.push_str(&format!("color = \"{}\"\n", m.color.as_str()));
        }
        text
    }

    /// Reads a manifest. Unknown keys are ignored (a later minor may add some); a
    /// higher `format`, another `kind`, a missing or mistyped field are errors.
    pub fn from_toml(text: &str) -> Result<Self, ReadError> {
        read::parse(text)
    }
}

/// The manifest name of a plain recording: `<stem>.rbs.toml` for `<stem>.<ext>`.
/// `None` for a name that is not a single file name.
pub fn manifest_name_for(media_file: &str) -> Option<String> {
    if !single_file_name(media_file) {
        return None;
    }
    let stem = match media_file.rfind('.') {
        Some(dot) if dot > 0 => &media_file[..dot],
        _ => media_file,
    };
    Some(format!("{stem}{FILE_SUFFIX}"))
}

/// Why a manifest was not written. The message has no path.
#[derive(Debug)]
pub struct WriteError(std::io::Error);

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the recording manifest could not be written ({})",
            self.0.kind()
        )
    }
}

impl std::error::Error for WriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Writes `manifest` as `dir/name` under a temporary name, then renames it, so a
/// reader sees the old or the new file, never half of one.
pub fn write(dir: &Path, name: &str, manifest: &RecordingManifest) -> Result<(), WriteError> {
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, manifest.to_toml()).map_err(WriteError)?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        WriteError(e)
    })
}

/// Whether `name` is one file name: not empty, not `.` or `..`, no separator, drive
/// colon or NUL.
pub fn single_file_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', ':', '\0'])
}

fn push_picture(text: &mut String, p: &Picture) {
    let rate = p.frame_rate.per_second();
    text.push_str(&format!("width = {}\n", p.width));
    text.push_str(&format!("height = {}\n", p.height));
    // A frame lasts den/num seconds when the rate is num/den.
    text.push_str(&format!(
        "frame_duration = \"{}/{}\"\n",
        rate.den(),
        rate.num()
    ));
}

/// A TOML basic string.
pub fn toml_string(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' | '"' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn complete() -> RecordingManifest {
        let mut m = RecordingManifest::new("2026-11-14T01:00:00.000Z", 5_000);
        let mut master = ManifestFile::listed("master", "master.mkv");
        let picture = Picture {
            width: 1920,
            height: 1080,
            frame_rate: FrameRate::FPS_59_94,
        };
        master.timing = Some(FileTiming {
            start_offset_ns: 0,
            duration_ns: 10_000_000_000,
            picture: Some(picture),
            sound: Some(Sound {
                sample_rate: 48_000,
                channels: 2,
            }),
        });
        let mut mic = ManifestFile::listed("audio", "Mic \"A\".wav");
        mic.source = Some(SourceRef::new(7));
        mic.source_name = Some("Mic \"A\"".to_owned());
        mic.timing = Some(FileTiming {
            start_offset_ns: 2_500_000,
            duration_ns: 9_990_000_000,
            picture: None,
            sound: Some(Sound {
                sample_rate: 48_000,
                channels: 1,
            }),
        });
        m.files = vec![master, mic];
        let mut log = MarkerLog::new(5_000, std::time::Duration::from_secs(1)).unwrap();
        log.add_manual(3_000_005_000, Some("Good"), Some(MarkerColor::Green))
            .unwrap();
        log.add_auto(4_000_005_000, MarkerKind::Scene, "Talk", MarkerColor::Blue)
            .unwrap();
        m.markers = log.markers().to_vec();
        m.completion = Some(Completion {
            duration_ns: 10_000_000_000,
            picture: Some(picture),
        });
        m
    }

    #[test]
    fn a_complete_manifest_reads_back_the_same() {
        let m = complete();
        let text = m.to_toml();
        assert!(text.starts_with("format = 1\nkind = \"recording-manifest\"\n"));
        assert!(text.contains("frame_duration = \"1001/60000\"\n"));
        assert!(text.contains("complete = true\n"));
        assert_eq!(RecordingManifest::from_toml(&text).unwrap(), m);
    }

    #[test]
    fn a_start_manifest_has_no_stop_fields_and_reads_back_incomplete() {
        let mut m = RecordingManifest::new("t", 1);
        m.files.push(ManifestFile::listed("master", "master.mkv"));
        let text = m.to_toml();
        for key in ["complete", "start_offset_ns", "duration_ns", "[[markers]]"] {
            assert!(!text.contains(key), "{key} in {text}");
        }
        let back = RecordingManifest::from_toml(&text).unwrap();
        assert!(!back.is_complete());
        assert_eq!(back, m);
    }

    #[test]
    fn plain_recordings_name_their_manifest_after_the_file() {
        assert_eq!(
            manifest_name_for("2026-11-14 01-00-00.mkv").as_deref(),
            Some("2026-11-14 01-00-00.rbs.toml")
        );
        assert_eq!(manifest_name_for("take").as_deref(), Some("take.rbs.toml"));
        assert_eq!(manifest_name_for(".mkv").as_deref(), Some(".mkv.rbs.toml"));
        assert_eq!(manifest_name_for("a/b.mkv"), None);
        assert_eq!(manifest_name_for("C:x.mkv"), None);
    }

    #[test]
    fn the_manifest_is_written_atomically_without_a_directory_in_the_text() {
        let m = complete();
        let dir = std::env::temp_dir().join(format!("rbs-manifest-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(dir.clone());
        write(&dir, FOLDER_MANIFEST, &m).unwrap();
        let text = std::fs::read_to_string(dir.join(FOLDER_MANIFEST)).unwrap();
        assert_eq!(text, m.to_toml());
        assert!(!text.contains("rbs-manifest-write"));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary left"
        );
        let err = write(&dir.join("missing"), FOLDER_MANIFEST, &m).unwrap_err();
        assert!(!err.to_string().contains("rbs-manifest-write"));
    }
}
