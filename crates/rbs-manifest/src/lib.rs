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

/// One picture of an image track (ADR-0117 decision 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEntry {
    /// Its file name inside the recording folder (`<name>.png`, `<name>-2.png`, ...).
    pub file_name: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The time from `t0` of the render tick on which the picture was taken.
    pub at_ns: u64,
}

/// What an image track counted, written at stop (ADR-0117 decisions 3 and 6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageCounts {
    /// Changes taken later than they happened (the interval, a busy readback or a
    /// long render tick); each image shows the picture as it was when taken.
    pub coalesced: u64,
    /// Changes not written because the track reached its image cap.
    pub over_cap: u64,
}

/// One file in the recording manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// `"master"`, `"video"`, `"audio"` or `"image"`.
    pub role: String,
    /// Name inside the recording folder (with its extension once the file is open).
    pub file_name: String,
    /// Set for a separate track.
    pub source: Option<SourceRef>,
    /// The source's display name, for a separate track.
    pub source_name: Option<String>,
    /// Timing and format, written at stop.
    pub timing: Option<FileTiming>,
    /// The pictures of an image track, in the order they were taken. Empty for any
    /// other role.
    pub images: Vec<ImageEntry>,
    /// An image track's counters, written at stop.
    pub image_counts: Option<ImageCounts>,
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
            images: Vec::new(),
            image_counts: None,
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
    /// Marked sources the take did not write, in source order (`[[skipped]]`,
    /// control API 1.54). Never in `files`.
    pub skipped: Vec<SkippedEntry>,
}

/// A marked source a take left without its file (`[[skipped]]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    /// The source.
    pub source: SourceRef,
    /// The source's name when the take started.
    pub source_name: Option<String>,
    /// The feed that was not written: `video`, `image` or `audio`.
    pub role: String,
    /// Why, as `record.status` names it (`limit`, `no_picture`, `open_failed`, ...).
    pub reason: String,
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
            skipped: Vec::new(),
        }
    }

    /// Whether the stop rewrite happened.
    pub fn is_complete(&self) -> bool {
        self.completion.is_some()
    }

    /// Moves `t0` earlier to `t0_ns` (a plain recording whose first sample came before
    /// the moment its markers count from): every time from `t0` (markers, file
    /// offsets, image times) grows by the difference, so nothing moves on the master
    /// clock. A later `t0_ns` changes nothing. `t0_utc` is kept: it names the start to
    /// the millisecond and is not a timeline position.
    pub fn move_origin(&mut self, t0_ns: u64) {
        let Some(shift) = self.t0_ns.checked_sub(t0_ns).filter(|s| *s > 0) else {
            return;
        };
        self.t0_ns = t0_ns;
        for m in &mut self.markers {
            m.t_ns = m.t_ns.saturating_add(shift);
            m.pressed_ns = m.pressed_ns.map(|p| p.saturating_add(shift));
        }
        for f in &mut self.files {
            if let Some(t) = &mut f.timing {
                t.start_offset_ns = t.start_offset_ns.saturating_add(shift);
            }
            for image in &mut f.images {
                image.at_ns = image.at_ns.saturating_add(shift);
            }
        }
    }

    /// The stop rewrite's top level (ADR-0089 decision 6): `complete = true`, the
    /// recording's length (the latest end of a file with timing) and the master's
    /// picture. Call after each file's `timing` is set.
    pub fn complete(&mut self) {
        let duration_ns = self
            .files
            .iter()
            .filter_map(|f| f.timing.as_ref())
            .map(|t| t.start_offset_ns.saturating_add(t.duration_ns))
            .max()
            .unwrap_or(0);
        let picture = self
            .files
            .iter()
            .find(|f| f.role == "master")
            .and_then(|f| f.timing.as_ref())
            .and_then(|t| t.picture);
        self.completion = Some(Completion {
            duration_ns,
            picture,
        });
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
            if let Some(c) = &file.image_counts {
                text.push_str(&format!("coalesced = {}\n", c.coalesced));
                text.push_str(&format!("over_cap = {}\n", c.over_cap));
            }
            for image in &file.images {
                text.push_str("\n[[files.images]]\n");
                text.push_str(&format!("name = {}\n", toml_string(&image.file_name)));
                text.push_str(&format!("width = {}\n", image.width));
                text.push_str(&format!("height = {}\n", image.height));
                text.push_str(&format!("at_ns = {}\n", image.at_ns));
            }
        }
        for s in &self.skipped {
            text.push_str("\n[[skipped]]\n");
            text.push_str(&format!("source = {}\n", s.source.get()));
            if let Some(name) = &s.source_name {
                text.push_str(&format!("source_name = {}\n", toml_string(name)));
            }
            text.push_str(&format!("role = {}\n", toml_string(&s.role)));
            text.push_str(&format!("reason = {}\n", toml_string(&s.reason)));
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
        let mut logo = ManifestFile::listed("image", "Logo.png");
        logo.source = Some(SourceRef::new(9));
        logo.source_name = Some("Logo".to_owned());
        logo.images = vec![
            ImageEntry {
                file_name: "Logo.png".to_owned(),
                width: 320,
                height: 180,
                at_ns: 0,
            },
            ImageEntry {
                file_name: "Logo-2.png".to_owned(),
                width: 640,
                height: 360,
                at_ns: 4_200_000_000,
            },
        ];
        logo.image_counts = Some(ImageCounts {
            coalesced: 3,
            over_cap: 0,
        });
        m.files = vec![master, logo, mic];
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
        assert!(text.contains("\n[[files.images]]\nname = \"Logo-2.png\"\n"));
        assert_eq!(RecordingManifest::from_toml(&text).unwrap(), m);
    }

    #[test]
    fn skipped_sources_are_written_apart_from_files_and_read_back() {
        let mut m = complete();
        m.skipped = vec![
            SkippedEntry {
                source: SourceRef::new(4),
                source_name: Some("Cam \"4\"".into()),
                role: "video".into(),
                reason: "limit".into(),
            },
            SkippedEntry {
                source: SourceRef::new(7),
                source_name: None,
                role: "audio".into(),
                reason: "open_failed".into(),
            },
        ];
        let text = m.to_toml();
        assert!(text.contains("\n[[skipped]]\nsource = 4\nsource_name = \"Cam \\\"4\\\"\"\nrole = \"video\"\nreason = \"limit\"\n"), "{text}");
        let back = RecordingManifest::from_toml(&text).unwrap();
        assert_eq!(back, m);
        assert!(
            back.files
                .iter()
                .all(|f| f.source.map(SourceRef::get) != Some(4))
        );
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
    fn moving_the_origin_earlier_keeps_every_time_on_the_master_clock() {
        let mut m = complete();
        let before = m.clone();
        m.move_origin(6_000);
        assert_eq!(m, before, "a later origin changes nothing");
        m.move_origin(4_000);
        assert_eq!(m.t0_ns, 4_000);
        assert_eq!(m.markers[0].t_ns, before.markers[0].t_ns + 1_000);
        assert_eq!(
            m.markers[0].pressed_ns,
            before.markers[0].pressed_ns.map(|p| p + 1_000)
        );
        let offset = |m: &RecordingManifest, i: usize| m.files[i].timing.unwrap().start_offset_ns;
        assert_eq!(offset(&m, 2), offset(&before, 2) + 1_000);
        assert_eq!(m.files[1].images[1].at_ns, 4_200_001_000);
    }

    #[test]
    fn completing_takes_the_latest_end_and_the_master_picture() {
        let mut m = complete();
        let expected = m.completion;
        m.completion = None;
        m.complete();
        assert_eq!(m.completion, expected);
        // The microphone ends after the master: the recording is as long as it.
        m.files[2].timing.as_mut().unwrap().duration_ns = 10_000_000_000;
        m.complete();
        assert_eq!(m.completion.unwrap().duration_ns, 10_002_500_000);
        let mut empty = RecordingManifest::new("t", 0);
        empty.complete();
        assert_eq!(
            empty.completion,
            Some(Completion {
                duration_ns: 0,
                picture: None
            })
        );
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
