//! Separate files next to a master recording (ADR-0086).
//!
//! [`plan`] names each track from the sources marked in the collection and opens
//! nothing. [`manifest_at_start`] is the manifest written when a recording
//! starts. Audio files are opened by [`crate::Studio::start_iso_audio`] in the
//! caller's container and codec. PCM and FLAC pass `bit_depth` when it is set;
//! absent is 16-bit. Video files are named and not opened yet. Offsets and drop
//! counters are filled when the recording stops.

use std::collections::BTreeSet;
use std::fmt;

use rbs_manifest::{ManifestFile, RecordingManifest, SourceRef};
use rbs_scene::SceneCollection;
use rbs_scene::model::{RecordSeparately, SourceId};
use rbs_sources::types::{SourceClass, class_of};

/// Most separate video files in one recording (ADR-0086).
pub const MAX_ISO_VIDEO: usize = 16;
/// Most separate audio files in one recording (ADR-0086).
pub const MAX_ISO_AUDIO: usize = 32;

/// Which feed a separate file carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsoRole {
    /// The source's picture.
    Video,
    /// The source's sound.
    Audio,
}

impl IsoRole {
    /// The manifest role: `"video"` or `"audio"`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// One separate file the start rule would open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoTrack {
    /// The source.
    pub source: SourceId,
    /// The source's display name.
    pub source_name: String,
    /// Picture or sound.
    pub role: IsoRole,
    /// File name inside the recording folder. No directory.
    pub file_name: String,
}

/// The separate files of one recording, in source id order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsoPlan {
    /// Empty when no source is marked.
    pub tracks: Vec<IsoTrack>,
}

/// Why a separate-file plan cannot start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IsoError {
    /// More video tracks than [`MAX_ISO_VIDEO`].
    TooManyVideo {
        /// How many were marked.
        count: usize,
    },
    /// More audio tracks than [`MAX_ISO_AUDIO`].
    TooManyAudio {
        /// How many were marked.
        count: usize,
    },
    /// `video` was set on a built-in audio source.
    VideoOnAudioOnly {
        /// The source.
        source: SourceId,
        /// Its display name.
        name: String,
    },
    /// `audio` was set on a built-in picture source that has no sound of its own.
    AudioOnPictureOnly {
        /// The source.
        source: SourceId,
        /// Its display name.
        name: String,
    },
    /// The master file name is empty, or it contains a directory.
    MasterName,
    /// `t0_utc` is empty.
    Clock,
    /// An audio track's source is not in the program mix, so there is no pin to tap.
    /// Keeping off-air sources Active is a later step (ADR-0086 decision 8).
    NotHeard {
        /// The source.
        source: SourceId,
        /// Its display name.
        name: String,
    },
    /// `recording.rbs.toml` could not be written. The message has no path.
    Manifest,
}

impl fmt::Display for IsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyVideo { count } => {
                write!(f, "{count} separate video files; at most {MAX_ISO_VIDEO}")
            }
            Self::TooManyAudio { count } => {
                write!(f, "{count} separate audio files; at most {MAX_ISO_AUDIO}")
            }
            Self::VideoOnAudioOnly { name, .. } => {
                write!(f, "{name} has no picture to record separately")
            }
            Self::AudioOnPictureOnly { name, .. } => {
                write!(f, "{name} has no sound of its own to record separately")
            }
            Self::MasterName => f.write_str("the master file name must be a single name"),
            Self::Clock => f.write_str("the recording start time is empty"),
            Self::NotHeard { name, .. } => {
                write!(f, "{name} is not in the program mix")
            }
            Self::Manifest => f.write_str("the recording manifest could not be written"),
        }
    }
}

/// Writes `recording.rbs.toml` into `dir` (a temporary name, then a rename). The
/// manifest text has no directory. An I/O error is [`IsoError::Manifest`].
pub fn write_manifest(dir: &std::path::Path, manifest: &RecordingManifest) -> Result<(), IsoError> {
    rbs_manifest::write(dir, rbs_manifest::FOLDER_MANIFEST, manifest)
        .map_err(|_| IsoError::Manifest)
}

/// The files a recording will write, named from the collection.
///
/// A built-in audio source may set `audio` only, and a built-in picture source
/// without its own sound may set `video` only. A media file, and a type this build
/// does not know, may set either. No source is opened.
pub fn plan(collection: &SceneCollection) -> Result<IsoPlan, IsoError> {
    let mut tracks = Vec::new();
    // Names are compared case-folded, as Windows and macOS file systems do, and the
    // master's name is reserved.
    let mut used = BTreeSet::from([MASTER_STEM.to_owned()]);
    for source in collection.sources() {
        let flag = source.record_separately;
        if flag.is_none() {
            continue;
        }
        check_class(source.id, &source.name, &source.type_id, flag)?;
        let stem = safe_stem(&source.name, source.id);
        if flag.video && flag.audio {
            tracks.push(track(
                source.id,
                &source.name,
                IsoRole::Video,
                &take_name(&mut used, &format!("{stem}-video"), source.id),
            ));
            tracks.push(track(
                source.id,
                &source.name,
                IsoRole::Audio,
                &take_name(&mut used, &format!("{stem}-audio"), source.id),
            ));
        } else if flag.video {
            tracks.push(track(
                source.id,
                &source.name,
                IsoRole::Video,
                &take_name(&mut used, &stem, source.id),
            ));
        } else {
            tracks.push(track(
                source.id,
                &source.name,
                IsoRole::Audio,
                &take_name(&mut used, &stem, source.id),
            ));
        }
    }
    let video = tracks.iter().filter(|t| t.role == IsoRole::Video).count();
    let audio = tracks.iter().filter(|t| t.role == IsoRole::Audio).count();
    if video > MAX_ISO_VIDEO {
        return Err(IsoError::TooManyVideo { count: video });
    }
    if audio > MAX_ISO_AUDIO {
        return Err(IsoError::TooManyAudio { count: audio });
    }
    Ok(IsoPlan { tracks })
}

fn track(source: SourceId, source_name: &str, role: IsoRole, file_name: &str) -> IsoTrack {
    IsoTrack {
        source,
        source_name: source_name.to_owned(),
        role,
        file_name: file_name.to_owned(),
    }
}

fn check_class(
    source: SourceId,
    name: &str,
    type_id: &rbs_graph::node::NodeTypeId,
    flag: RecordSeparately,
) -> Result<(), IsoError> {
    match class_of(type_id) {
        Some(SourceClass::Audio(_)) if flag.video => Err(IsoError::VideoOnAudioOnly {
            source,
            name: name.to_owned(),
        }),
        Some(SourceClass::Capture(_) | SourceClass::Still | SourceClass::Browser) if flag.audio => {
            Err(IsoError::AudioOnPictureOnly {
                source,
                name: name.to_owned(),
            })
        }
        _ => Ok(()),
    }
}

/// The manifest at the moment recording starts (ADR-0086 decision 6). `master_file`
/// is a single name, never a path. Offsets, formats and markers are absent until the
/// recording stops.
pub fn manifest_at_start(
    plan: &IsoPlan,
    master_file: &str,
    t0_utc: &str,
    t0_ns: u64,
) -> Result<RecordingManifest, IsoError> {
    if !rbs_manifest::single_file_name(master_file) {
        return Err(IsoError::MasterName);
    }
    if t0_utc.is_empty() {
        return Err(IsoError::Clock);
    }
    let mut manifest = RecordingManifest::new(t0_utc, t0_ns);
    manifest.files.reserve(plan.tracks.len() + 1);
    manifest
        .files
        .push(ManifestFile::listed("master", master_file));
    for track in &plan.tracks {
        let mut file = ManifestFile::listed(track.role.as_str(), &track.file_name);
        file.source = Some(SourceRef::new(track.source.get()));
        file.source_name = Some(track.source_name.clone());
        manifest.files.push(file);
    }
    Ok(manifest)
}

fn safe_stem(name: &str, id: SourceId) -> String {
    let mut stem = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') {
            stem.push(c);
        } else if !c.is_control() {
            stem.push('_');
        }
    }
    while stem.len() > 80 {
        stem.pop();
    }
    let stem = stem.trim().trim_matches('.').trim().to_owned();
    if stem.is_empty() || is_reserved(&stem) {
        format!("source-{}", id.get())
    } else {
        stem
    }
}

fn is_reserved(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// The master file's name without its extension (ADR-0086 decision 7). No track takes it.
pub const MASTER_STEM: &str = "master";

fn take_name(used: &mut BTreeSet<String>, stem: &str, id: SourceId) -> String {
    let mut n = 1u32;
    loop {
        let candidate = if n == 1 {
            stem.to_owned()
        } else {
            format!("{stem} {n}")
        };
        if used.insert(candidate.to_lowercase()) {
            return candidate;
        }
        match n.checked_add(1) {
            Some(next) => n = next,
            None => {
                let fallback = format!("source-{}", id.get());
                if used.insert(fallback.clone()) {
                    return fallback;
                }
                let unique = format!("source-{}-{}", id.get(), n);
                used.insert(unique.clone());
                return unique;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::model::{RecordSeparately, Settings, SourceId};
    use rbs_sources::types::{AUDIO_INPUT_SOURCE, DISPLAY_SOURCE, MEDIA_SOURCE};

    fn source(col: &mut SceneCollection, name: &str, type_id: &str) -> SourceId {
        col.add_source(name, NodeTypeId::new(type_id), Settings::new())
            .unwrap()
            .0
    }

    fn mark(col: &mut SceneCollection, id: SourceId, video: bool, audio: bool) {
        col.set_source_record_separately(id, RecordSeparately::new(video, audio))
            .unwrap();
    }

    #[test]
    fn track_names_never_take_the_master_name_or_differ_only_by_case() {
        let mut col = SceneCollection::new();
        let master = source(&mut col, "Master", AUDIO_INPUT_SOURCE);
        let upper = source(&mut col, "Mic", AUDIO_INPUT_SOURCE);
        let lower = source(&mut col, "mic", AUDIO_INPUT_SOURCE);
        for id in [master, upper, lower] {
            mark(&mut col, id, false, true);
        }
        let names: Vec<_> = plan(&col)
            .unwrap()
            .tracks
            .into_iter()
            .map(|t| t.file_name)
            .collect();
        assert_eq!(names, ["Master 2", "Mic", "mic 2"]);
    }

    #[test]
    fn a_plan_names_separate_files_and_the_start_manifest_has_no_path() {
        let mut col = SceneCollection::new();
        let display = source(&mut col, "Screen", DISPLAY_SOURCE);
        let mic = source(&mut col, "Mic", AUDIO_INPUT_SOURCE);
        let other = source(&mut col, "Mic.", AUDIO_INPUT_SOURCE);
        mark(&mut col, display, true, false);
        mark(&mut col, mic, false, true);
        mark(&mut col, other, false, true);
        let plan = plan(&col).unwrap();
        assert_eq!(plan.tracks.len(), 3);
        assert_eq!(plan.tracks[0].role, IsoRole::Video);
        assert_eq!(plan.tracks[0].file_name, "Screen");
        assert_eq!(plan.tracks[1].file_name, "Mic");
        assert_eq!(plan.tracks[2].file_name, "Mic 2");

        let manifest =
            manifest_at_start(&plan, "master.mkv", "2026-11-13T19:00:00Z", 1_000).unwrap();
        let text = manifest.to_toml();
        assert!(text.starts_with("format = 1\nkind = \"recording-manifest\"\n"));
        assert!(text.contains("role = \"master\"\nname = \"master.mkv\"\n"));
        assert!(text.contains("role = \"audio\"\nname = \"Mic 2\"\n"));
        assert!(!text.contains('\\'));
        assert!(!text.contains("start_offset_ns"));
        assert!(manifest_at_start(&plan, "C:/out.mkv", "t", 0).is_err());
        assert!(manifest_at_start(&plan, "master.mkv", "", 0).is_err());
    }

    #[test]
    fn a_picture_flag_on_a_microphone_and_too_many_video_tracks_are_refused() {
        let mut col = SceneCollection::new();
        let mic = source(&mut col, "Mic", AUDIO_INPUT_SOURCE);
        mark(&mut col, mic, true, false);
        assert!(matches!(plan(&col), Err(IsoError::VideoOnAudioOnly { .. })));

        let mut col = SceneCollection::new();
        let display = source(&mut col, "Screen", DISPLAY_SOURCE);
        mark(&mut col, display, true, true);
        assert!(matches!(
            plan(&col),
            Err(IsoError::AudioOnPictureOnly { .. })
        ));

        let mut col = SceneCollection::new();
        let media = source(&mut col, "Clip", MEDIA_SOURCE);
        mark(&mut col, media, true, true);
        let separate = plan(&col).unwrap();
        assert_eq!(separate.tracks[0].file_name, "Clip-video");
        assert_eq!(separate.tracks[1].file_name, "Clip-audio");

        let mut col = SceneCollection::new();
        for i in 0..17 {
            let id = source(&mut col, &format!("Cam {i}"), DISPLAY_SOURCE);
            mark(&mut col, id, true, false);
        }
        assert!(matches!(
            plan(&col),
            Err(IsoError::TooManyVideo { count: 17 })
        ));
    }

    #[test]
    fn a_reserved_file_name_uses_the_source_id() {
        let mut col = SceneCollection::new();
        let id = source(&mut col, "CON", AUDIO_INPUT_SOURCE);
        mark(&mut col, id, false, true);
        let plan = plan(&col).unwrap();
        assert_eq!(plan.tracks[0].file_name, format!("source-{}", id.get()));
    }

    #[test]
    fn the_start_manifest_is_written_without_a_directory_in_the_text() {
        let mut manifest = RecordingManifest::new("2026-11-13T19:00:00Z", 1);
        manifest
            .files
            .push(ManifestFile::listed("master", "master.mkv"));
        let dir = std::env::temp_dir().join(format!("rbs-iso-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        struct Guard(std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(dir.clone());
        write_manifest(&dir, &manifest).unwrap();
        let text = std::fs::read_to_string(dir.join("recording.rbs.toml")).unwrap();
        assert_eq!(text, manifest.to_toml());
        assert!(!text.contains("rbs-iso-manifest"));
        assert!(write_manifest(dir.join("missing").as_path(), &manifest).is_err());
    }
}
