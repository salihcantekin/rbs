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
        stream,
        vertical: None,
        loudness,
        swap_after_transition: None,
        separate_files: None,
        replay: None,
        vertical_profile: None,
        vertical_stream: None,
    }
}

/// The rest of `every_request`'s patch: the loudness and the 1.100 fields.
pub(super) fn patch_rest() -> SettingsPatch {
    SettingsPatch {
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
