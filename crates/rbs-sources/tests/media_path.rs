//! Local-only media paths are refused before a decoder reads the file (ADR-0108).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use rbs_media_decode::pixel::CPU_UPLOAD;
use rbs_media_decode_test::{ScriptClip, ScriptFactory};
use rbs_scene::SceneCollection;
use rbs_scene::model::{SettingValue, Settings};
use rbs_sources::types::{self, keys};
use rbs_sources::{InstanceContext, SourceError, SourceInstance, SourceTypes};

#[test]
fn a_url_unc_or_device_path_is_refused_before_any_content_read() {
    let factory = Arc::new(ScriptFactory::new(ScriptClip::silent_frames()));
    let mut reg = SourceTypes::new();
    reg.register_media(factory.clone()).unwrap();
    reg.probe();
    let ctx = InstanceContext::new(
        Arc::new(rbs_core::time::TestClock::new()),
        rbs_audio::AudioGraphConfig::default(),
    );
    for path in [
        "https://example.com/a.mp4",
        r"\\server\share\a.mp4",
        r"\\.\pipe\rbs",
        "clips/relative.mp4",
    ] {
        let err = instantiate(&reg, &ctx, path).unwrap_err();
        assert!(
            matches!(err, SourceError::InvalidSetting { key: "path", .. }),
            "{path} -> {err}"
        );
    }
    assert_eq!(factory.counts.opens.load(Ordering::Relaxed), 0);
    assert_eq!(factory.counts.pulls.load(Ordering::Relaxed), 0);
    assert_eq!(factory.counts.file_bytes.load(Ordering::Relaxed), 0);
}

#[test]
fn a_regular_file_is_probed_and_then_the_decoder_is_dropped() {
    let dir = std::env::temp_dir().join(format!(
        "rbs-media-src-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("clip.mp4");
    {
        let mut out = fs::File::create(&file).unwrap();
        out.write_all(b"not-decoded").unwrap();
    }
    let factory = Arc::new(ScriptFactory::new(ScriptClip::silent_frames()));
    let mut reg = SourceTypes::new();
    reg.register_media(factory.clone()).unwrap();
    reg.probe();
    let ctx = InstanceContext::new(
        Arc::new(rbs_core::time::TestClock::new()),
        rbs_audio::AudioGraphConfig::default(),
    );
    let instance = instantiate(&reg, &ctx, &file.to_string_lossy()).unwrap();
    let SourceInstance::Media(media) = instance else {
        panic!("media");
    };
    assert!(!media.picture.queues_live());
    assert!(!media.picture.decoder_held());
    assert_eq!(media.picture.phase(), rbs_nodes::MediaPhase::Standby);
    assert_eq!(factory.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(factory.counts.file_bytes.load(Ordering::Relaxed), 0);
    assert!(factory.counts.opens.load(Ordering::Relaxed) >= 1);
    assert_eq!(fs::read(&file).unwrap(), b"not-decoded");
    let _ = fs::remove_dir_all(&dir);
}

fn temp_file(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "rbs-media-{tag}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("clip.mp4");
    fs::write(&file, b"not-decoded").unwrap();
    (dir, file)
}

#[test]
fn the_probe_falls_past_an_adapter_that_refuses_the_file() {
    let (dir, file) = temp_file("fallback");
    let mut refusing = ScriptClip::silent_frames();
    refusing.fail_open = Some("codec not installed".to_owned());
    let first = Arc::new(ScriptFactory::with_pixel(
        "org.rbs.media-decode.refuses",
        refusing,
        CPU_UPLOAD,
    ));
    let second = Arc::new(ScriptFactory::with_pixel(
        "org.rbs.media-decode.opens",
        ScriptClip::silent_frames(),
        CPU_UPLOAD,
    ));
    let mut reg = SourceTypes::new();
    reg.register_media(first.clone()).unwrap();
    reg.register_media(second.clone()).unwrap();
    reg.probe();
    let ctx = InstanceContext::new(
        Arc::new(rbs_core::time::TestClock::new()),
        rbs_audio::AudioGraphConfig::default(),
    );
    let instance = instantiate(&reg, &ctx, &file.to_string_lossy()).unwrap();
    let SourceInstance::Media(media) = instance else {
        panic!("media");
    };
    assert_eq!(media.adapter.as_str(), "org.rbs.media-decode.opens");
    assert_eq!(first.counts.opens.load(Ordering::Relaxed), 1);
    assert_eq!(first.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(second.counts.alive.load(Ordering::Relaxed), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn when_every_adapter_refuses_the_most_specific_error_is_reported() {
    let (dir, file) = temp_file("refuse");
    let mut failing = ScriptClip::silent_frames();
    failing.fail_open = Some("broken file".to_owned());
    let no_picture = ScriptClip {
        frames: Vec::new(),
        ..ScriptClip::silent_frames()
    };
    let mut reg = SourceTypes::new();
    reg.register_media(Arc::new(ScriptFactory::with_pixel(
        "org.rbs.media-decode.fails",
        failing,
        CPU_UPLOAD,
    )))
    .unwrap();
    reg.register_media(Arc::new(ScriptFactory::with_pixel(
        "org.rbs.media-decode.no-picture",
        no_picture,
        CPU_UPLOAD,
    )))
    .unwrap();
    reg.probe();
    let ctx = InstanceContext::new(
        Arc::new(rbs_core::time::TestClock::new()),
        rbs_audio::AudioGraphConfig::default(),
    );
    let err = instantiate(&reg, &ctx, &file.to_string_lossy()).unwrap_err();
    assert!(
        matches!(err, SourceError::InvalidSetting { key: "path", .. }),
        "{err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

fn instantiate(
    reg: &SourceTypes,
    ctx: &InstanceContext,
    path: &str,
) -> Result<SourceInstance, SourceError> {
    let mut settings = Settings::new();
    settings.insert(keys::PATH.to_owned(), SettingValue::Text(path.to_owned()));
    let mut collection = SceneCollection::new();
    let (id, _) = collection
        .add_source(
            "Clip",
            rbs_graph::node::NodeTypeId::new(types::MEDIA_SOURCE),
            settings,
        )
        .unwrap();
    reg.instantiate(collection.source(id).unwrap(), ctx)
}

#[test]
fn a_missing_codec_is_reported_over_a_plain_failure() {
    let (dir, file) = temp_file("codec");
    let mut failing = ScriptClip::silent_frames();
    failing.fail_open = Some("broken".to_owned());
    let mut missing = ScriptClip::silent_frames();
    missing.unsupported = Some("HEVC".to_owned());
    let mut reg = SourceTypes::new();
    // The less specific failure comes first, then the missing codec.
    reg.register_media(Arc::new(ScriptFactory::with_pixel(
        "org.rbs.media-decode.fails",
        failing,
        CPU_UPLOAD,
    )))
    .unwrap();
    reg.register_media(Arc::new(ScriptFactory::with_pixel(
        "org.rbs.media-decode.no-hevc",
        missing,
        CPU_UPLOAD,
    )))
    .unwrap();
    reg.probe();
    let ctx = InstanceContext::new(
        Arc::new(rbs_core::time::TestClock::new()),
        rbs_audio::AudioGraphConfig::default(),
    );
    let err = instantiate(&reg, &ctx, &file.to_string_lossy()).unwrap_err();
    assert!(
        matches!(&err, SourceError::Media(text) if text.contains("HEVC")),
        "{err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_play_range_with_end_not_after_start_is_refused() {
    let id = rbs_graph::node::NodeTypeId::new(types::MEDIA_SOURCE);
    let with = |start: Option<i64>, end: Option<SettingValue>| {
        let mut settings = Settings::new();
        settings.insert(
            keys::PATH.to_owned(),
            SettingValue::Text(r"C:\clips\clip.mp4".to_owned()),
        );
        if let Some(start) = start {
            settings.insert(keys::START_MS.to_owned(), SettingValue::Int(start));
        }
        if let Some(end) = end {
            settings.insert(keys::END_MS.to_owned(), end);
        }
        rbs_sources::validate_settings(&id, &settings)
    };
    assert!(with(None, None).is_ok());
    assert!(with(Some(1_000), None).is_ok());
    assert!(with(Some(1_000), Some(SettingValue::Int(2_000))).is_ok());
    for (start, end, key) in [
        (Some(2_000), Some(SettingValue::Int(2_000)), "end_ms"),
        (Some(2_000), Some(SettingValue::Int(1_000)), "end_ms"),
        (None, Some(SettingValue::Int(0)), "end_ms"),
        (Some(-1), None, "start_ms"),
        (None, Some(SettingValue::Text("1s".to_owned())), "end_ms"),
    ] {
        let err = with(start, end.clone()).unwrap_err();
        assert!(
            matches!(err, SourceError::InvalidSetting { key: k, .. } if k == key),
            "{start:?} {end:?} -> {err}"
        );
    }
    assert!(rbs_sources::option_keys(&id).contains(&keys::START_MS));
    assert!(rbs_sources::option_keys(&id).contains(&keys::END_MS));
}
