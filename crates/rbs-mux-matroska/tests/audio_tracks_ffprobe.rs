//! Several named audio tracks (ADR-0170 decision 6), read back with `ffprobe`: each
//! track's name, and only track 1 played by default. Ignored by default because CI
//! has no FFmpeg tools; run with `-- --ignored` where `ffprobe` is installed. The tool
//! only reads the test file; the muxer does not use FFmpeg.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::process::Command;

use rbs_core::media::{ContainerId, StreamDescriptor};
use rbs_mux::MuxerConfig;
use rbs_mux::conformance::{aac_stream, h264_stream, mux_file};

/// `(default disposition, name tag)` of every audio stream `ffprobe` reads in `file`.
fn audio_tracks(file: &std::path::Path, tag: &str) -> Vec<(String, String)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a", "-show_entries"])
        .arg(format!("stream_disposition=default:stream_tags={tag}"))
        .args(["-of", "csv=p=0"])
        .arg(file)
        .output()
        .expect("ffprobe is installed");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let (default, name) = l.split_once(',').unwrap_or((l, ""));
            (default.to_owned(), name.to_owned())
        })
        .collect()
}

fn three_tracks() -> Vec<StreamDescriptor> {
    let mut streams = vec![h264_stream()];
    streams.extend(
        ["main", "VOD", "Chat"]
            .iter()
            .enumerate()
            .map(|(i, n)| aac_stream().with_name(*n).with_default(i == 0)),
    );
    streams
}

fn written(
    factory: &dyn rbs_mux::MuxerFactory,
    container: ContainerId,
    ext: &str,
) -> std::path::PathBuf {
    let config = MuxerConfig::new(container, three_tracks());
    let file = mux_file(factory, &config).unwrap();
    let path = std::env::temp_dir().join(format!(
        "rbs-audio-tracks-{}-{}.{ext}",
        std::process::id(),
        container
    ));
    std::fs::write(&path, file).unwrap();
    path
}

#[test]
#[ignore = "needs the ffprobe command-line tool"]
fn three_named_tracks_and_only_the_first_default() {
    let path = written(
        &rbs_mux_matroska::MatroskaMuxerFactory,
        ContainerId::Matroska,
        "mkv",
    );
    let tracks = audio_tracks(&path, "title");
    let _ = std::fs::remove_file(&path);
    let expected = [("1", "main"), ("0", "VOD"), ("0", "Chat")];
    assert_eq!(
        tracks,
        expected.map(|(d, n)| (d.to_owned(), n.to_owned())),
        "{tracks:?}"
    );
}
