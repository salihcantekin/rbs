//! The output fields of `settings.set` and `settings.get` (protocol 1.100, ADR-0164
//! decision 5): each change goes back to the window, which saves the file it owns.

use super::*;

fn kind(answer: &serde_json::Value) -> &serde_json::Value {
    &answer["error"]["data"]["kind"]
}

#[test]
fn output_settings_are_read_and_changed_through_settings() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &TestClock::new());
    add_display(&host, &rx);
    let (api, mut c) = api_client(&host);
    let none = serde_json::json!({});
    c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["settings.changed"]}),
    );

    let got = c.call("settings.get", none.clone());
    let data = &got["result"]["data"];
    assert_eq!(data["swap_after_transition"], false, "{got}");
    assert_eq!(data["separate_files"]["enabled"], true, "{got}");
    assert_eq!(data["separate_files"]["on_hardware_limit"], "ask", "{got}");
    assert_eq!(data["replay"]["length_secs"], 120, "{got}");
    assert!(data["replay"]["folder"].is_null(), "{got}");
    assert!(data["vertical_profile"].is_null(), "{got}");
    assert!(data["vertical_stream"].is_null(), "{got}");
    let main_profile = data["profiles"][0].clone();

    // Swap after transition.
    let set = c.call(
        "settings.set",
        serde_json::json!({"swap_after_transition": true}),
    );
    assert_eq!(
        set["result"]["data"]["swap_after_transition"], true,
        "{set}"
    );
    let on = next(&rx, |u| match u {
        Update::SwapAfterTransition(on) => Some(on),
        _ => None,
    });
    assert!(on);
    c.event("settings.changed", |d| d["swap_after_transition"] == true);

    // Separate files; `cpu` waits for its encoder.
    let set = c.call(
        "settings.set",
        serde_json::json!({"separate_files": {"enabled": false, "on_hardware_limit": "skip"}}),
    );
    assert_eq!(
        set["result"]["data"]["separate_files"]["enabled"], false,
        "{set}"
    );
    let files = next(&rx, |u| match u {
        Update::SeparateFiles(f) => Some(f),
        _ => None,
    });
    assert!(!files.enabled);
    assert_eq!(files.on_hardware_limit, "skip");
    let refused = c.call(
        "settings.set",
        serde_json::json!({"separate_files": {"enabled": true, "on_hardware_limit": "cpu"}}),
    );
    assert_eq!(kind(&refused), "invalid_params", "{refused}");

    // The replay buffer, with its bounds and an absolute folder.
    let replay = serde_json::json!({
        "arm_on_start": true, "length_secs": 60, "memory_mib": 512, "audio": false,
        "folder": "/clips"
    });
    let set = c.call("settings.set", serde_json::json!({"replay": replay}));
    assert_eq!(set["result"]["data"]["replay"]["folder"], "/clips", "{set}");
    let setup = next(&rx, |u| match u {
        Update::ReplaySetup(s) => Some(s),
        _ => None,
    });
    assert_eq!(setup.length_secs, 60);
    assert_eq!(
        setup.folder.as_deref(),
        Some(std::path::Path::new("/clips"))
    );
    for (field, value) in [("length_secs", 5.into()), ("folder", "clips".into())] {
        let mut bad = replay.clone();
        bad[field] = value;
        let refused = c.call("settings.set", serde_json::json!({"replay": bad}));
        assert_eq!(kind(&refused), "invalid_params", "{field}: {refused}");
    }

    // A patch is checked whole: a bad field leaves the good one unapplied.
    let mut bad = replay.clone();
    bad["memory_mib"] = 1.into();
    let refused = c.call(
        "settings.set",
        serde_json::json!({"swap_after_transition": false, "replay": bad}),
    );
    assert_eq!(kind(&refused), "invalid_params", "{refused}");
    let got = c.call("settings.get", none.clone());
    assert_eq!(
        got["result"]["data"]["swap_after_transition"], true,
        "{got}"
    );

    // The vertical recording's own settings: set, then removed with `null`.
    let mut vertical = main_profile.clone();
    vertical["folder"] = "/vertical".into();
    let set = c.call(
        "settings.set",
        serde_json::json!({"vertical_profile": vertical}),
    );
    assert_eq!(
        set["result"]["data"]["vertical_profile"]["folder"], "/vertical",
        "{set}"
    );
    let saved = next(&rx, |u| match u {
        Update::VerticalProfile(p) => Some(p),
        _ => None,
    });
    assert_eq!(
        saved.map(|p| p.folder),
        Some(std::path::PathBuf::from("/vertical"))
    );
    let set = c.call(
        "settings.set",
        serde_json::json!({"vertical_profile": null}),
    );
    assert!(set["result"]["data"]["vertical_profile"].is_null(), "{set}");
    let removed = next(&rx, |u| match u {
        Update::VerticalProfile(p) => Some(p),
        _ => None,
    });
    assert_eq!(removed, None);
    vertical["folder"] = "vertical".into();
    let refused = c.call(
        "settings.set",
        serde_json::json!({"vertical_profile": vertical}),
    );
    assert_eq!(kind(&refused), "invalid_params", "{refused}");

    // The vertical stream: never `null`, and only once the window set it up; the
    // server and the key stay the local user's.
    let patch = serde_json::json!({"vertical_stream": {
        "reconnect": false,
        "srt_latency_ms": null,
        "encoding": {
            "encoder": null, "rate_control": "vbr", "video_kbps": 3000,
            "max_video_kbps": 4500, "quality": 23, "keyframe_seconds": 2,
            "profile": null, "preset": "quality", "b_frames": 2,
            "cbr_padding": false, "audio_kbps": 160
        }
    }});
    let refused = c.call("settings.set", serde_json::json!({"vertical_stream": null}));
    assert_eq!(kind(&refused), "invalid_params", "{refused}");
    let refused = c.call("settings.set", patch.clone());
    assert_eq!(kind(&refused), "conflict", "{refused}");
    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    profile.key_saved = true;
    host.send(Command::SetVerticalStreamProfile(Some(profile)));
    let set = c.call("settings.set", patch);
    let data = &set["result"]["data"]["vertical_stream"];
    assert_eq!(data["encoding"]["video_kbps"], 3000, "{set}");
    assert_eq!(data["key_saved"], true, "{set}");
    assert!(!set.to_string().contains("live.example.com"), "{set}");
    let saved = next(&rx, |u| match u {
        Update::VerticalStreamProfile(p) => Some(p),
        _ => None,
    });
    assert_eq!(saved.server, URL);
    assert!(saved.key_saved);
    assert!(!saved.reconnect);
    drop(api);
}

#[test]
fn separate_files_wait_for_the_recording_to_stop() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    add_display(&host, &rx);
    host.send(Command::SetProfile(record_profile("separate-files")));
    let (api, mut c) = api_client(&host);
    let got = c.call("settings.get", serde_json::json!({}));
    let id = got["result"]["data"]["profiles"][0]["id"].clone();
    let started = c.call("record.start", serde_json::json!({"profile": id}));
    assert!(started.get("result").is_some(), "{started}");
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));

    let off = serde_json::json!({"separate_files": {"enabled": false, "on_hardware_limit": "ask"}});
    let refused = c.call("settings.set", off.clone());
    assert_eq!(kind(&refused), "conflict", "{refused}");
    // The same settings are no change, so they are not refused.
    let same = serde_json::json!({"separate_files": {"enabled": true, "on_hardware_limit": "ask"}});
    let set = c.call("settings.set", same);
    assert!(set.get("result").is_some(), "{set}");

    c.call("record.stop", serde_json::json!({}));
    let stopped = next_while(
        &rx,
        || clock.advance(END_TAIL_LIMIT),
        |u| match u {
            Update::Recording(r @ RecordingUpdate::Stopped { .. }) => Some(r),
            _ => None,
        },
    );
    assert!(matches!(stopped, RecordingUpdate::Stopped { .. }));
    let set = c.call("settings.set", off);
    assert_eq!(
        set["result"]["data"]["separate_files"]["enabled"], false,
        "{set}"
    );
    drop(api);
}
