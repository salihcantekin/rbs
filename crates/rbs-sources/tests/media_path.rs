//! Local-only media paths are refused before a decoder reads the file (ADR-0108).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

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
    assert!(!media.node.queues_live());
    assert!(!media.node.decoder_held());
    assert_eq!(media.node.phase(), rbs_nodes::MediaPhase::Standby);
    assert_eq!(factory.counts.alive.load(Ordering::Relaxed), 0);
    assert_eq!(factory.counts.file_bytes.load(Ordering::Relaxed), 0);
    assert!(factory.counts.opens.load(Ordering::Relaxed) >= 1);
    assert_eq!(fs::read(&file).unwrap(), b"not-decoded");
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
