//! FCPXML golden files (ADR-0089 decisions 8 and 9).
//!
//! Each case builds a manifest by hand and compares the project with a file under
//! `tests/golden`. After an intended change, regenerate them with
//! `RBS_BLESS=1 cargo test -p rbs-project` and review the diff.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_core::timing::FrameRate;
use rbs_manifest::{
    Completion, FileTiming, ManifestFile, MarkerColor, MarkerKind, MarkerLog, Picture,
    RecordingManifest, Sound, SourceRef,
};
use rbs_project::{Format, MediaFolder, ProjectError, build, export};

const MS: u64 = 1_000_000;
const S: u64 = 1_000_000_000;

fn golden(name: &str, text: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var("RBS_BLESS").is_ok_and(|v| v == "1") {
        std::fs::write(&path, text).unwrap();
    }
    let want = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e} (run with RBS_BLESS=1)", path.display()));
    assert_eq!(text, want, "{name} differs from the golden file");
}

fn picture(width: u32, height: u32, frame_rate: FrameRate) -> Picture {
    Picture {
        width,
        height,
        frame_rate,
    }
}

fn sound(sample_rate: u32, channels: u16) -> Sound {
    Sound {
        sample_rate,
        channels,
    }
}

fn file(
    role: &str,
    name: &str,
    source: Option<(u32, &str)>,
    start_offset_ns: u64,
    duration_ns: u64,
    picture: Option<Picture>,
    sound: Option<Sound>,
) -> ManifestFile {
    let mut f = ManifestFile::listed(role, name);
    if let Some((id, source_name)) = source {
        f.source = Some(SourceRef::new(id));
        f.source_name = Some(source_name.to_owned());
    }
    f.timing = Some(FileTiming {
        start_offset_ns,
        duration_ns,
        picture,
        sound,
    });
    f
}

/// 59.94 fps with 48 kHz separate sound: a master that starts within half a frame of
/// `t0` (no gap), a screen track, two microphones at sub-frame offsets, markers of
/// every kind and one after the end.
fn interview() -> RecordingManifest {
    let canvas = picture(1920, 1080, FrameRate::FPS_59_94);
    let t0 = 1_000 * S;
    let mut m = RecordingManifest::new("2026-11-14T01:00:00.000Z", t0);
    m.files = vec![
        file(
            "master",
            "master.mkv",
            None,
            2 * MS,
            60 * S,
            Some(canvas),
            Some(sound(48_000, 2)),
        ),
        file(
            "video",
            "Screen.mkv",
            Some((3, "Screen")),
            17 * MS,
            59_983 * MS,
            Some(picture(2560, 1440, FrameRate::FPS_59_94)),
            None,
        ),
        file(
            "audio",
            "Mic \"Host\".wav",
            Some((5, "Mic \"Host\"")),
            2_500_000,
            59_997 * MS,
            None,
            Some(sound(48_000, 1)),
        ),
        file(
            "audio",
            "Guest & Co.wav",
            Some((6, "Guest & Co")),
            900 * MS,
            50 * S,
            None,
            Some(sound(48_000, 1)),
        ),
    ];
    let mut log = MarkerLog::new(t0, Duration::from_secs(2)).unwrap();
    log.add_auto(
        t0 + 10 * S,
        MarkerKind::Scene,
        "Interview",
        MarkerColor::Blue,
    )
    .unwrap();
    log.add_manual(t0 + 15 * S, None, Some(MarkerColor::Red))
        .unwrap();
    log.add_manual(t0 + 30 * S, Some("Great <answer>"), None)
        .unwrap();
    log.add_auto(
        t0 + 40 * S,
        MarkerKind::StreamEnd,
        "Stream ended",
        MarkerColor::Yellow,
    )
    .unwrap();
    log.add_manual(t0 + 75 * S, Some("After the end"), None)
        .unwrap();
    m.markers = log.markers().to_vec();
    m.completion = Some(Completion {
        duration_ns: 60 * S,
        picture: Some(canvas),
    });
    m
}

/// 30 fps, a master that starts 100 ms after `t0` (a three-frame gap), a microphone
/// that starts before it (on the gap) and a marker inside the gap.
fn late_master() -> RecordingManifest {
    let canvas = picture(1080, 1920, FrameRate::FPS_30);
    let mut m = RecordingManifest::new("2026-11-14T02:00:00.000Z", 0);
    m.files = vec![
        file(
            "master",
            "Konuşma 1.mp4",
            None,
            100 * MS,
            10 * S,
            Some(canvas),
            Some(sound(44_100, 2)),
        ),
        file(
            "audio",
            "Mikrofon.wav",
            Some((2, "Mikrofon")),
            10 * MS,
            10_090 * MS,
            None,
            Some(sound(44_100, 2)),
        ),
    ];
    let mut log = MarkerLog::new(0, Duration::ZERO).unwrap();
    log.add_manual(50 * MS, Some("Before the picture"), None)
        .unwrap();
    log.add_auto(5 * S, MarkerKind::Scene, "Outro", MarkerColor::Blue)
        .unwrap();
    m.markers = log.markers().to_vec();
    m.completion = Some(Completion {
        duration_ns: 10_100 * MS,
        picture: Some(canvas),
    });
    m
}

#[test]
fn a_59_94_recording_with_48k_separate_sound() {
    let folder = MediaFolder::from_text(r"C:\Users\Salih\Videos\RBS\Interview 2026-11-14").unwrap();
    let text = build(
        &interview(),
        &folder,
        Format::Fcpxml,
        "Interview 2026-11-14",
    )
    .unwrap();
    golden("interview-5994.fcpxml", &text);
}

#[test]
fn a_master_that_starts_late_puts_earlier_items_on_a_gap() {
    let folder = MediaFolder::from_text("/Users/salih/Movies/RBS/Konuşma").unwrap();
    let text = build(&late_master(), &folder, Format::Fcpxml, "Konuşma").unwrap();
    golden("late-master-30.fcpxml", &text);
}

#[test]
fn incomplete_or_unplaceable_manifests_are_refused() {
    let folder = MediaFolder::from_text("/tmp/x").unwrap();
    let mut m = interview();
    m.completion = None;
    assert_eq!(
        build(&m, &folder, Format::Fcpxml, "x"),
        Err(ProjectError::Incomplete)
    );

    let mut m = interview();
    if let Some(done) = &mut m.completion {
        done.picture = None;
    }
    assert_eq!(
        build(&m, &folder, Format::Fcpxml, "x"),
        Err(ProjectError::NoPicture)
    );

    let mut m = interview();
    m.files[1].timing = None;
    assert_eq!(
        build(&m, &folder, Format::Fcpxml, "x"),
        Err(ProjectError::MissingTiming("Screen.mkv".to_owned()))
    );

    let mut m = interview();
    m.files[2].role = "midi".to_owned();
    assert_eq!(
        build(&m, &folder, Format::Fcpxml, "x"),
        Err(ProjectError::UnknownRole("midi".to_owned()))
    );

    let mut m = interview();
    m.files.remove(0);
    assert_eq!(
        build(&m, &folder, Format::Fcpxml, "x"),
        Err(ProjectError::NoMaster)
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("rbs-project-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn export_leaves_out_missing_tracks_and_never_replaces_a_project() {
    let dir = TempDir::new("export");
    let folder = dir.0.join("Interview");
    std::fs::create_dir_all(&folder).unwrap();
    let manifest = interview();
    rbs_manifest::write(&folder, rbs_manifest::FOLDER_MANIFEST, &manifest).unwrap();

    // No master yet.
    assert_eq!(
        export(
            &folder,
            rbs_manifest::FOLDER_MANIFEST,
            Format::Fcpxml,
            false
        ),
        Err(ProjectError::MissingMaster)
    );
    for name in ["master.mkv", "Screen.mkv", "Mic \"Host\".wav"] {
        std::fs::write(folder.join(name), b"").unwrap();
    }
    assert_eq!(
        export(&folder, rbs_manifest::FOLDER_MANIFEST, Format::Fcpxml, true),
        Err(ProjectError::MissingFile("Guest & Co.wav".to_owned()))
    );
    let first = export(
        &folder,
        rbs_manifest::FOLDER_MANIFEST,
        Format::Fcpxml,
        false,
    )
    .unwrap();
    assert_eq!(first.path, folder.join("Interview.fcpxml"));
    assert_eq!(first.missing, vec!["Guest & Co.wav".to_owned()]);
    let text = std::fs::read_to_string(&first.path).unwrap();
    assert!(text.contains("Mic%20%22Host%22.wav"));
    assert!(!text.contains("Guest"));

    let second = export(
        &folder,
        rbs_manifest::FOLDER_MANIFEST,
        Format::Fcpxml,
        false,
    )
    .unwrap();
    assert_eq!(second.path, folder.join("Interview 2.fcpxml"));
    assert_eq!(std::fs::read_to_string(&first.path).unwrap(), text);

    // A plain recording's manifest names the project after its file.
    let plain = rbs_manifest::manifest_name_for("Take 3.mkv").unwrap();
    let mut m = interview();
    m.files.truncate(1);
    m.files[0].file_name = "Take 3.mkv".to_owned();
    rbs_manifest::write(&dir.0, &plain, &m).unwrap();
    std::fs::write(dir.0.join("Take 3.mkv"), b"").unwrap();
    let out = export(&dir.0, &plain, Format::Fcpxml, true).unwrap();
    assert_eq!(out.path, dir.0.join("Take 3.fcpxml"));

    // A recording that did not stop cleanly.
    let mut m = interview();
    m.completion = None;
    rbs_manifest::write(&dir.0, "crashed.rbs.toml", &m).unwrap();
    assert_eq!(
        export(&dir.0, "crashed.rbs.toml", Format::Fcpxml, false),
        Err(ProjectError::Incomplete)
    );
}
