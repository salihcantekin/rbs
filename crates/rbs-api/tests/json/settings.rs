//! `settings.*` with the output fields of protocol 1.100 (ADR-0164 decision 5).

use super::*;

/// Settings with only the older fields.
pub(super) fn plain(
    profiles: Vec<RecordingProfile>,
    stream: Option<StreamSettings>,
    loudness: Option<LoudnessSettings>,
) -> Settings {
    Settings {
        profiles,
        canvas: canvas(),
        stream: stream.map(Box::new),
        vertical: None,
        loudness,
        swap_after_transition: None,
        separate_files: None,
        replay: None,
        vertical_profile: None,
        vertical_stream: None,
    }
}

/// The rest of `every_request`'s patch: the stream, the loudness and the 1.100 fields.
pub(super) fn patch_rest() -> SettingsPatch {
    SettingsPatch {
        stream: Some(Box::new(StreamSettingsPatch {
            reconnect: false,
            srt_latency_ms: Some(500),
            encoding: encoding(),
            auto_record: Some(true),
            output_size: None,
            hold_picture: Some(HoldPicture::Black),
            hold_scene: None,
            hold_sound: None,
            audio_bus: Some("vod".into()),
        })),
        loudness: Some(LoudnessSettings {
            target: -16.0,
            true_peak_limit: -1.0,
        }),
        swap_after_transition: Some(false),
        separate_files: Some(SeparateFilesSettings {
            enabled: true,
            on_hardware_limit: HardwareLimitPolicy::Skip,
        }),
        replay: Some(Box::new(replay(Some("/clips")))),
        vertical_profile: Some(Some(Box::new(profile()))),
        vertical_stream: None,
        ..SettingsPatch::default()
    }
}

fn replay(folder: Option<&str>) -> ReplaySettings {
    ReplaySettings {
        arm_on_start: true,
        length_secs: 60,
        memory_mib: 512,
        audio: false,
        folder: folder.map(Into::into),
    }
}

#[test]
fn the_output_settings_round_trip_and_null_is_kept_apart_from_absent() {
    let mut s = plain(vec![profile()], None, None);
    s.swap_after_transition = Some(true);
    s.separate_files = Some(SeparateFilesSettings {
        enabled: false,
        on_hardware_limit: HardwareLimitPolicy::Ask,
    });
    s.replay = Some(replay(None));
    s.vertical_profile = Some(None);
    s.vertical_stream = Some(Some(Box::new(stream_settings())));
    round_trip(&s);
    let text = serde_json::to_value(&s).unwrap();
    assert!(text["vertical_profile"].is_null(), "{text}");
    assert!(text.get("vertical_profile").is_some(), "{text}");
    // A 1.99 server's answer has none of them.
    let old = serde_json::to_value(plain(vec![], None, None)).unwrap();
    assert!(old.get("vertical_profile").is_none(), "{old}");
    let back: Settings = serde_json::from_value(old).unwrap();
    assert_eq!(back.vertical_stream, None);

    // In a patch, `null` is present (`Some(None)`), absent is `None`.
    let p: SettingsPatch = serde_json::from_value(json!({"vertical_stream": null})).unwrap();
    assert_eq!(p.vertical_stream, Some(None));
    let p: SettingsPatch = serde_json::from_value(json!({})).unwrap();
    assert_eq!(p.vertical_stream, None);
}

#[test]
fn output_folders_that_are_not_local_reach_the_network() {
    let set = |p: SettingsPatch| Command::SettingsSet(p).reaches_network();
    let far = "\\\\server\\share";
    assert!(!set(SettingsPatch {
        replay: Some(Box::new(replay(Some("/clips")))),
        ..SettingsPatch::default()
    }));
    assert!(set(SettingsPatch {
        replay: Some(Box::new(replay(Some(far)))),
        ..SettingsPatch::default()
    }));
    let mut vertical = profile();
    vertical.folder = far.into();
    assert!(set(SettingsPatch {
        vertical_profile: Some(Some(Box::new(vertical))),
        ..SettingsPatch::default()
    }));
    assert!(!set(SettingsPatch {
        vertical_profile: Some(None),
        ..SettingsPatch::default()
    }));
}

#[test]
fn stream_settings_are_a_minor_addition() {
    let old: SettingsPatch = serde_json::from_value(json!({"canvas": null})).unwrap();
    assert_eq!(old.stream, None);
    let text = serde_json::to_value(settings::plain(vec![], None, None)).unwrap();
    assert!(text.get("stream").is_none(), "{text}");
    let got = serde_json::to_value(stream_settings()).unwrap();
    assert!(got.get("server").is_none(), "{got}");
    let back: StreamSettingsPatch = serde_json::from_value(got).unwrap();
    assert_eq!(
        back,
        StreamSettingsPatch {
            reconnect: true,
            srt_latency_ms: None,
            encoding: encoding(),
            auto_record: Some(true),
            output_size: None,
            hold_picture: Some(HoldPicture::Black),
            hold_scene: Some(Some(4)),
            hold_sound: Some(HoldSound::Scene),
            audio_bus: Some("vod".into()),
        }
    );
    // Protocol 1.67: `null` clears the hold scene, absent leaves it.
    let cleared: StreamSettingsPatch = serde_json::from_value({
        let mut v = serde_json::to_value(stream_settings()).unwrap();
        v["hold_scene"] = serde_json::Value::Null;
        v
    })
    .unwrap();
    assert_eq!(cleared.hold_scene, Some(None));
    // Protocol 1.65 added `hold_picture`: a 1.64 server's settings read as `freeze`,
    // and a 1.64 client's patch leaves it as it is.
    let mut older = serde_json::to_value(stream_settings()).unwrap();
    assert_eq!(older["hold_picture"], "black");
    older.as_object_mut().unwrap().remove("hold_picture");
    let read: StreamSettings = serde_json::from_value(older.clone()).unwrap();
    assert_eq!(read.hold_picture, HoldPicture::Freeze);
    let patch: StreamSettingsPatch = serde_json::from_value(older).unwrap();
    assert_eq!(patch.hold_picture, None);
    // Protocol 1.67 added `hold_scene` and `hold_sound`: a 1.66 server's settings
    // read as none and silence, and a 1.66 client's patch leaves them.
    let mut older = serde_json::to_value(stream_settings()).unwrap();
    for field in ["hold_scene", "hold_sound"] {
        older.as_object_mut().unwrap().remove(field);
    }
    let read: StreamSettings = serde_json::from_value(older.clone()).unwrap();
    assert_eq!(
        (read.hold_scene, read.hold_sound),
        (None, HoldSound::Silence)
    );
    let patch: StreamSettingsPatch = serde_json::from_value(older).unwrap();
    assert_eq!((patch.hold_scene, patch.hold_sound), (None, None));
    // Protocol 1.11 added `auto_record`: a 1.2 server's settings read as off, and a
    // 1.2 client's patch leaves it as it is.
    let mut old = serde_json::to_value(stream_settings()).unwrap();
    old.as_object_mut().unwrap().remove("auto_record");
    let read: StreamSettings = serde_json::from_value(old.clone()).unwrap();
    assert!(!read.auto_record);
    let patch: StreamSettingsPatch = serde_json::from_value(old).unwrap();
    assert_eq!(patch.auto_record, None);
    let text = serde_json::to_value(&patch).unwrap();
    assert!(text.get("auto_record").is_none(), "{text}");
    // Protocol 1.101 added `audio_bus`: a 1.100 server's settings read as `main`
    // (absent), and a 1.100 client's patch leaves it.
    let mut older = serde_json::to_value(stream_settings()).unwrap();
    assert_eq!(older["audio_bus"], "vod");
    older.as_object_mut().unwrap().remove("audio_bus");
    let read: StreamSettings = serde_json::from_value(older.clone()).unwrap();
    assert_eq!(read.audio_bus, None);
    let patch: StreamSettingsPatch = serde_json::from_value(older).unwrap();
    assert_eq!(patch.audio_bus, None);
}

#[test]
fn a_recording_profile_names_its_audio_tracks() {
    // Protocol 1.101: omitted when `["main"]`, so a 1.100 client reads the object.
    let text = serde_json::to_value(profile()).unwrap();
    assert!(text.get("audio_tracks").is_none(), "{text}");
    let mut p = profile();
    p.audio_tracks = Some(vec!["vod".into()]);
    let text = serde_json::to_value(&p).unwrap();
    assert_eq!(text["audio_tracks"], json!(["vod"]));
    let back: RecordingProfile = serde_json::from_value(text).unwrap();
    assert_eq!(back, p);
}
