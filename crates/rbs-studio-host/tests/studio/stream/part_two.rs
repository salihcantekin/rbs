//! Stream tests, continued (stream keys onwards); split from stream.rs by size.

use super::*;

#[test]
pub(super) fn each_output_profile_has_its_own_stream_key() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &clock);
    add_display(&host, &rx);
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "default_key",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));

    // Another profile: the default profile's key is not its key.
    let b = rbs_studio_host::stream_key::account("studio-b");
    host.send(Command::SetKeyAccount(b.clone()));
    host.send(Command::StartStream(request()));
    assert_eq!(stream(&rx), StreamUpdate::NoKey);
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "b_key",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    assert_eq!(keys.read(&b).unwrap().unwrap().expose(), "b_key");
    assert_eq!(keys.read(ACCOUNT).unwrap().unwrap().expose(), "default_key");

    // Deleting a profile removes its key only.
    host.send(Command::SetKeyAccount(ACCOUNT.to_owned()));
    host.send(Command::RemoveStreamKey(b.clone()));
    host.send(Command::SaveStreamKey(Some(rbs_transport::Secret::new(
        "default_key",
    ))));
    assert_eq!(key_saved(&rx), Ok(true));
    assert!(keys.read(&b).unwrap().is_none());
    assert_eq!(keys.read(ACCOUNT).unwrap().unwrap().expose(), "default_key");
    assert!(server.sessions.lock().unwrap().is_empty());
    drop(host);
    while rx.recv_timeout(LIMIT).is_ok() {}
}

pub(super) const VERTICAL_URL: &str = "rtmp://shorts.example.com/app";

#[test]
pub(super) fn the_vertical_canvas_records_and_streams_beside_the_main_one() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("main_key"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &clock);
    let (api, mut c) = api_client(&host);
    let vertical = serde_json::json!({"canvas": "vertical"});
    let sub = c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["stream.canvas_state", "record.canvas_state"]}),
    );
    assert!(sub["result"].is_object(), "{sub}");
    let mut main = StreamProfile::new();
    main.server = URL.into();
    main.reconnect = false;
    host.send(Command::SetStreamProfile(main));
    host.send(Command::SetProfile(record_profile("vertical")));
    add_display(&host, &rx);

    // No vertical canvas yet, and an unknown canvas is not a canvas.
    for method in [
        "record.status",
        "stream.status",
        "stream.start",
        "record.stop",
    ] {
        let r = c.call(method, vertical.clone());
        assert_eq!(r["error"]["data"]["kind"], "not_found", "{method}: {r}");
    }
    let square = c.call("stream.start", serde_json::json!({"canvas": "square"}));
    assert_eq!(
        square["error"]["data"]["kind"], "invalid_params",
        "{square}"
    );
    let added = c.call(
        "canvas.add_vertical",
        serde_json::json!({"width": 720, "height": 1280}),
    );
    assert!(added["result"].is_object(), "{added}");
    next(&rx, |u| {
        matches!(u, Update::Vertical(Some(_))).then_some(())
    });

    // No vertical stream settings saved: refused, naming the vertical stream.
    let refused = c.call("stream.start", vertical.clone());
    assert_eq!(refused["error"]["data"]["kind"], "failed", "{refused}");
    assert!(refused.to_string().contains("vertical stream"), "{refused}");
    let mut shorts = StreamProfile::new();
    shorts.server = VERTICAL_URL.into();
    shorts.reconnect = false;
    // "Record when the stream starts" is per canvas.
    shorts.auto_record = true;
    host.send(Command::SetVerticalStreamProfile(Some(shorts)));
    host.send(Command::ListTargets(SourceKind::Screen));
    next(&rx, |u| matches!(u, Update::Targets(..)).then_some(()));
    // Its key is its own: the main key is not used for it.
    let no_key = c.call("stream.start", vertical.clone());
    assert!(no_key.to_string().contains("vertical stream"), "{no_key}");
    let account = rbs_studio_host::stream_key::vertical_account(ACCOUNT);
    assert_eq!(account, "stream key/vertical");
    keys.write(&account, &rbs_transport::Secret::new("shorts_key"))
        .unwrap();

    // Each canvas records and streams: four outputs at once.
    let none = serde_json::json!({});
    let started = c.call("stream.start", none.clone());
    assert!(started["result"].is_object(), "{started}");
    let started = c.call("stream.start", vertical.clone());
    assert!(started["result"].is_object(), "{started}");
    next(&rx, |u| match u {
        Update::VerticalStream(StreamUpdate::Started { url, .. }) => {
            assert_eq!(url, VERTICAL_URL);
            Some(())
        }
        _ => None,
    });
    let profile = serde_json::json!({"profile": "default"});
    let rec = c.call("record.start", profile.clone());
    assert!(rec["result"].is_object(), "{rec}");
    // The vertical stream started the vertical recording, not a main one.
    next(&rx, |u| match u {
        Update::VerticalRecording(RecordingUpdate::Started { .. }) => Some(()),
        Update::VerticalRecording(other) => panic!("{other:?}"),
        _ => None,
    });
    let again = c.call(
        "record.start",
        serde_json::json!({"profile": "default", "canvas": "vertical"}),
    );
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");
    let sessions = server.sessions.lock().unwrap().clone();
    assert!(
        sessions.contains(&(URL.to_owned(), Some("main_key".to_owned()))),
        "{sessions:?}"
    );
    assert!(
        sessions.contains(&(VERTICAL_URL.to_owned(), Some("shorts_key".to_owned()))),
        "{sessions:?}"
    );

    // The vertical states have their own topic and key; the main ones are untouched.
    let live = c.event("stream.canvas_state", |d| {
        d["canvas"] == "vertical" && d["state"] == "live"
    });
    assert_eq!(live["params"]["object"], 1, "{live}");
    let status = c.call("stream.status", vertical.clone());
    assert_eq!(status["result"]["data"]["canvas"], "vertical", "{status}");
    assert_eq!(status["result"]["data"]["state"], "live", "{status}");
    assert!(
        status["result"]["seq"]["stream.canvas_state:1"].is_u64(),
        "{status}"
    );
    let main_status = c.call("stream.status", none.clone());
    assert_eq!(
        main_status["result"]["data"]["state"], "live",
        "{main_status}"
    );
    assert!(
        main_status["result"]["data"]["canvas"].is_null(),
        "{main_status}"
    );
    c.event("record.canvas_state", |d| d["state"] == "recording");

    // The canvas cannot go while its outputs run.
    let busy = c.call("canvas.remove_vertical", none.clone());
    assert_eq!(busy["error"]["data"]["kind"], "conflict", "{busy}");
    // Ending the vertical stream stops the recording it started (after its tail).
    assert!(c.call("stream.stop", vertical.clone())["result"].is_object());
    let idle = c.event("stream.canvas_state", |d| d["state"] == "idle");
    let last_seq = idle["params"]["seq"].as_u64().unwrap();
    let path = next_while(
        &rx,
        || clock.advance(END_TAIL_LIMIT * 2),
        |u| match u {
            Update::VerticalRecording(RecordingUpdate::Stopped { path, failure, .. }) => {
                assert_eq!(failure, None);
                Some(path)
            }
            _ => None,
        },
    );
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.ends_with(" vertical.mkv"), "{name}");
    c.event("record.canvas_state", |d| d["state"] == "idle");
    // The main canvas's outputs ran on.
    let main_status = c.call("record.status", none.clone());
    assert_eq!(
        main_status["result"]["data"]["state"], "recording",
        "{main_status}"
    );
    let main_status = c.call("stream.status", none.clone());
    assert_eq!(
        main_status["result"]["data"]["state"], "live",
        "{main_status}"
    );

    assert!(c.call("canvas.remove_vertical", none.clone())["result"].is_object());
    next(&rx, |u| matches!(u, Update::Vertical(None)).then_some(()));
    let gone = c.call("record.status", vertical.clone());
    assert_eq!(gone["error"]["data"]["kind"], "not_found", "{gone}");
    // A new vertical canvas starts idle, and its events count on from the old ones.
    let added = c.call(
        "canvas.add_vertical",
        serde_json::json!({"width": 720, "height": 1280}),
    );
    assert!(added["result"].is_object(), "{added}");
    next(&rx, |u| {
        matches!(u, Update::Vertical(Some(_))).then_some(())
    });
    let status = c.call("stream.status", vertical.clone());
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");
    assert!(status["result"]["data"]["last"].is_null(), "{status}");
    assert!(c.call("stream.start", vertical.clone())["result"].is_object());
    let live = c.event("stream.canvas_state", |d| d["state"] == "live");
    assert!(live["params"]["seq"].as_u64().unwrap() > last_seq, "{live}");
    assert!(c.call("stream.stop", vertical)["result"].is_object());
    assert!(c.call("record.stop", none.clone())["result"].is_object());
    assert!(c.call("stream.stop", none)["result"].is_object());
    drop(host);
    drop(api);
}

/// ADR-0111: `canvas: "all"` starts and stops the main output, then the vertical one,
/// with one entry per canvas; an unconfigured vertical stream is skipped, and the
/// with-main flags are read through `settings.get` and set by `output.set_with_vertical`.
#[test]
pub(super) fn all_canvases_start_and_stop_together_and_report_each_output() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("main_key"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    let (api, mut c) = api_client(&host);
    let mut main = StreamProfile::new();
    main.server = URL.into();
    main.reconnect = false;
    host.send(Command::SetStreamProfile(main));
    host.send(Command::SetProfile(record_profile("all")));
    add_display(&host, &rx);
    let all = serde_json::json!({"profile": "default", "canvas": "all"});
    let outcomes = |r: &serde_json::Value| -> Vec<(String, String)> {
        r["result"]["outputs"]
            .as_array()
            .unwrap_or_else(|| panic!("{r}"))
            .iter()
            .map(|o| {
                (
                    o["canvas"].as_str().unwrap().to_owned(),
                    o["outcome"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let pair = |c: &str, o: &str| (c.to_owned(), o.to_owned());

    // The flags read as on by default, also with no vertical canvas.
    let settings = c.call("settings.get", serde_json::json!({}));
    assert_eq!(
        settings["result"]["data"]["vertical"],
        serde_json::json!({"record_with_main": true, "stream_with_main": true}),
        "{settings}"
    );
    // "all" is not a value of the methods that act on one canvas.
    for method in ["record.status", "stream.status"] {
        let r = c.call(method, serde_json::json!({"canvas": "all"}));
        assert_eq!(
            r["error"]["data"]["kind"], "invalid_params",
            "{method}: {r}"
        );
    }
    let to_file = c.call(
        "record.start_to_file",
        serde_json::json!({"profile": "default", "path": "/tmp/x.mkv", "canvas": "all"}),
    );
    assert_eq!(
        to_file["error"]["data"]["kind"], "invalid_params",
        "{to_file}"
    );

    // Without a vertical canvas, "all" is the main canvas alone. (A stream, since
    // recording file names count seconds and the clock stands still.)
    let all_stream = serde_json::json!({"canvas": "all"});
    let live = c.call("stream.start", all_stream.clone());
    assert_eq!(outcomes(&live), [pair("main", "started")], "{live}");
    assert!(
        live["result"]["outputs"][0]["video_encoder"].is_string(),
        "{live}"
    );
    let again = c.call("stream.start", all_stream.clone());
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");
    let ended = c.call("stream.stop", all_stream);
    assert_eq!(outcomes(&ended), [pair("main", "stopped")], "{ended}");

    let added = c.call(
        "canvas.add_vertical",
        serde_json::json!({"width": 720, "height": 1280}),
    );
    assert!(added["result"].is_object(), "{added}");
    next(&rx, |u| {
        matches!(u, Update::Vertical(Some(_))).then_some(())
    });

    // Both recordings start, main first; a second start finds both running.
    let rec = c.call("record.start", all.clone());
    assert_eq!(
        outcomes(&rec),
        [pair("main", "started"), pair("vertical", "started")],
        "{rec}"
    );
    // Each started entry carries the estimate of what now runs (protocol 1.53).
    for entry in 0..2 {
        let level = &rec["result"]["outputs"][entry]["estimate"]["level"];
        assert!(level == "fits" || level == "heavy", "{rec}");
    }
    let again = c.call("record.start", all.clone());
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");
    let status = c.call("record.status", serde_json::json!({"canvas": "vertical"}));
    assert_eq!(status["result"]["data"]["state"], "recording", "{status}");

    // No vertical server is saved: the vertical stream is skipped, not an error.
    let live = c.call("stream.start", serde_json::json!({"canvas": "all"}));
    assert_eq!(
        outcomes(&live),
        [pair("main", "started"), pair("vertical", "not_configured")],
        "{live}"
    );
    let ended = c.call("stream.stop", serde_json::json!({"canvas": "all"}));
    assert_eq!(
        outcomes(&ended),
        [pair("main", "stopped"), pair("vertical", "not_running")],
        "{ended}"
    );
    let none = c.call("stream.stop", serde_json::json!({"canvas": "all"}));
    assert_eq!(none["error"]["data"]["kind"], "conflict", "{none}");

    let stop = c.call("record.stop", serde_json::json!({"canvas": "all"}));
    assert_eq!(
        outcomes(&stop),
        [pair("main", "stopped"), pair("vertical", "stopped")],
        "{stop}"
    );

    // The flag is a control setting, and settings.get shows it.
    let set = c.call(
        "output.set_with_vertical",
        serde_json::json!({"kind": "record", "on": false}),
    );
    assert!(set["result"].is_object(), "{set}");
    next(&rx, |u| match u {
        Update::VerticalFlags(f) => (!f.record_with_main).then_some(()),
        _ => None,
    });
    let settings = c.call("settings.get", serde_json::json!({}));
    assert_eq!(
        settings["result"]["data"]["vertical"],
        serde_json::json!({"record_with_main": false, "stream_with_main": true}),
        "{settings}"
    );
    let bad = c.call(
        "output.set_with_vertical",
        serde_json::json!({"kind": "replay", "on": false}),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params", "{bad}");
    drop(host);
    drop(api);
}

/// ADR-0124, control API 1.61: a recording is paused and resumed through the control
/// API; its state stays `recording` with `paused` set, a second pause or a resume of
/// a running one is a conflict, and `canvas: "all"` reports each canvas.
#[test]
pub(super) fn a_control_api_client_pauses_and_resumes_a_recording() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    let (api, mut c) = api_client(&host);
    let sub = c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["record.state"]}),
    );
    assert!(sub["result"].is_object(), "{sub}");
    host.send(Command::SetProfile(record_profile("pause")));
    add_display(&host, &rx);
    let rec = c.call("record.start", serde_json::json!({"profile": "default"}));
    assert!(rec["result"].is_object(), "{rec}");
    let started = c.event("record.state", |d| d["state"] == "recording");
    assert_eq!(started["params"]["data"]["paused"], false, "{started}");

    let main = serde_json::json!({});
    let resume = c.call("record.resume", main.clone());
    assert_eq!(resume["error"]["data"]["kind"], "conflict", "{resume}");
    assert!(resume.to_string().contains("not paused"), "{resume}");
    let pause = c.call("record.pause", main.clone());
    assert!(pause["result"].is_object(), "{pause}");
    let paused = c.event("record.state", |d| d["paused"] == true);
    assert_eq!(paused["params"]["data"]["state"], "recording", "{paused}");
    let again = c.call("record.pause", main.clone());
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");
    let status = c.call("record.status", main.clone());
    assert_eq!(status["result"]["data"]["paused"], true, "{status}");

    // A toggle resumes a paused recording; with "all", each canvas is reported.
    let toggle = c.call("record.pause_toggle", main.clone());
    assert!(toggle["result"].is_object(), "{toggle}");
    c.event("record.state", |d| d["paused"] == false);
    let all = c.call("record.pause_toggle", serde_json::json!({"canvas": "all"}));
    assert_eq!(all["result"]["outputs"][0]["outcome"], "paused", "{all}");
    c.event("record.state", |d| d["paused"] == true);
    let all = c.call("record.resume", serde_json::json!({"canvas": "all"}));
    assert_eq!(all["result"]["outputs"][0]["outcome"], "resumed", "{all}");
    c.event("record.state", |d| d["paused"] == false);

    assert!(c.call("record.stop", main)["result"].is_object());
    drop(host);
    drop(api);
}

/// ADR-0124 from the window: Hold stream (the End stream button's second half,
/// Ctrl+Shift+H) holds the live stream and again resumes it; Break holds it too; with
/// nothing live, Hold says why.
#[test]
pub(super) fn the_window_holds_and_resumes_the_stream() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    add_display(&host, &rx);
    let held = |rx: &Receiver<Update>| {
        next(rx, |u| match u {
            Update::StreamHold(states) => Some(states.map(|s| s.map(|s| s.held))),
            _ => None,
        })
    };
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    host.send(Command::HoldStream);
    assert_eq!(held(&rx), [Some(true), None]);
    clock.advance(Duration::from_secs(1));
    host.send(Command::HoldStream);
    assert_eq!(held(&rx), [Some(false), None]);
    host.send(Command::Break);
    assert_eq!(held(&rx), [Some(true), None]);
    host.send(Command::Break);
    assert_eq!(held(&rx), [Some(false), None]);
    host.send(Command::EndStream);
    assert_eq!(ended(&rx), None);
    host.send(Command::HoldStream);
    let why = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert!(
        why.starts_with("The stream could not hold or resume"),
        "{why}"
    );
}

/// ADR-0124 decisions 12 and 15 through the control API (protocol 1.64): a live stream
/// is held and resumed, its state carries `held`, a second hold or a resume of a
/// stream not held is a conflict, and Break holds every stream and pauses every
/// recording, then resumes them all.
#[test]
pub(super) fn a_control_api_client_holds_a_stream_and_takes_a_break() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    let (api, mut c) = api_client(&host);
    let sub = c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["stream.state", "record.state"]}),
    );
    assert!(sub["result"].is_object(), "{sub}");
    add_display(&host, &rx);
    let main = serde_json::json!({});
    let none = c.call("stream.hold", main.clone());
    assert_eq!(none["error"]["data"]["kind"], "conflict", "{none}");
    let none = c.call("output.break_toggle", main.clone());
    assert_eq!(none["error"]["data"]["kind"], "conflict", "{none}");
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    let live = c.event("stream.state", |d| d["state"] == "live");
    assert_eq!(live["params"]["data"]["held"], false, "{live}");

    let resume = c.call("stream.resume", main.clone());
    assert_eq!(resume["error"]["data"]["kind"], "conflict", "{resume}");
    assert!(resume.to_string().contains("not on hold"), "{resume}");
    let bad = c.call("stream.hold", serde_json::json!({"picture": "slate"}));
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params", "{bad}");
    let bad = c.call(
        "stream.hold",
        serde_json::json!({"picture": "black", "sound": "scene"}),
    );
    assert_eq!(bad["error"]["data"]["kind"], "invalid_params", "{bad}");
    // A scene hold with no scene named or set (protocol 1.67).
    let unset = c.call("stream.hold", serde_json::json!({"picture": "scene"}));
    assert_eq!(unset["error"]["data"]["kind"], "conflict", "{unset}");
    assert!(unset.to_string().contains("no hold scene"), "{unset}");
    let hold = c.call("stream.hold", serde_json::json!({"picture": "black"}));
    assert!(hold["result"].is_object(), "{hold}");
    c.event("stream.state", |d| d["held"] == true);
    let again = c.call("stream.hold", serde_json::json!({"picture": "black"}));
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");
    clock.advance(Duration::from_secs(2));
    let toggle = c.call("stream.hold_toggle", main.clone());
    assert!(toggle["result"].is_object(), "{toggle}");
    let back = c.event("stream.state", |d| d["held"] == false);
    assert!(
        back["params"]["data"]["held_ms"].as_u64().unwrap() >= 2000,
        "{back}"
    );

    // Break: the stream holds and the recording pauses; again, both resume. The
    // recording shares the stream's encoders here, so the hold pauses it first and
    // the stream holds once that pause has begun (decision 14).
    host.send(Command::SetProfile(record_profile("break")));
    let rec = c.call("record.start", serde_json::json!({"profile": "default"}));
    assert!(rec["result"].is_object(), "{rec}");
    c.event("record.state", |d| d["state"] == "recording");
    let on = c.call("output.break_toggle", main.clone());
    let entries = &on["result"]["outputs"];
    assert_eq!(entries[0]["kind"], "stream", "{on}");
    assert_eq!(entries[0]["outcome"], "held", "{on}");
    assert_eq!(entries[1]["kind"], "record", "{on}");
    assert_eq!(entries[1]["outcome"], "paused", "{on}");
    // The recording pauses first, then the stream holds.
    c.event("record.state", |d| d["paused"] == true);
    c.event("stream.state", |d| d["held"] == true);
    let resume = c.call("record.resume", main.clone());
    assert_eq!(resume["error"]["data"]["kind"], "conflict", "{resume}");
    let off = c.call("output.break_toggle", main.clone());
    let entries = &off["result"]["outputs"];
    assert_eq!(entries[0]["outcome"], "resumed", "{off}");
    assert_eq!(entries[1]["outcome"], "resumed", "{off}");
    c.event("record.state", |d| d["paused"] == false);
    c.event("stream.state", |d| d["held"] == false);

    assert!(c.call("record.stop", main.clone())["result"].is_object());
    assert!(c.call("stream.stop", main)["result"].is_object());
    drop(host);
    drop(api);
}

/// ADR-0124 phase 2b through the control API (protocol 1.67): a live stream holds
/// with a scene named in the call, its state says which scene and sound, a scene not
/// in the collection is `not_found`, and the scene and sound set in the stream
/// settings are used when the call names none.
#[test]
pub(super) fn a_control_api_client_holds_a_stream_with_a_scene() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    let (api, mut c) = api_client(&host);
    let sub = c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["stream.state", "settings.changed"]}),
    );
    assert!(sub["result"].is_object(), "{sub}");
    add_display(&host, &rx);
    let added = c.call("scene.add", serde_json::json!({"name": "Break"}));
    let brk = added["result"]["scene"].as_u64().unwrap();
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    c.event("stream.state", |d| d["state"] == "live");

    let gone = c.call(
        "stream.hold",
        serde_json::json!({"picture": "scene", "scene": 9_999}),
    );
    assert_eq!(gone["error"]["data"]["kind"], "not_found", "{gone}");
    let hold = c.call(
        "stream.hold",
        serde_json::json!({"picture": "scene", "scene": brk, "sound": "scene"}),
    );
    assert!(hold["result"].is_object(), "{hold}");
    let held = c.event("stream.state", |d| d["held"] == true);
    let data = &held["params"]["data"];
    assert_eq!(data["hold_picture"], "scene", "{held}");
    assert_eq!(data["hold_scene"], brk, "{held}");
    assert_eq!(data["hold_sound"], "scene", "{held}");
    assert_eq!(data["hold_fallback"], false, "{held}");
    let other = c.call(
        "stream.hold",
        serde_json::json!({"picture": "scene", "scene": brk, "sound": "silence"}),
    );
    assert_eq!(other["error"]["data"]["kind"], "conflict", "{other}");
    // Another picture while held leaves the scene.
    let freeze = c.call("stream.hold", serde_json::json!({"picture": "freeze"}));
    assert!(freeze["result"].is_object(), "{freeze}");
    let frozen = c.event("stream.state", |d| d["hold_picture"] == "freeze");
    assert_eq!(
        frozen["params"]["data"]["hold_scene"],
        serde_json::Value::Null
    );
    let resume = c.call("stream.resume", serde_json::json!({}));
    assert!(resume["result"].is_object(), "{resume}");
    c.event("stream.state", |d| d["held"] == false);

    // The stream settings' hold scene and sound, used when the call names none.
    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    profile.key_saved = true;
    profile.hold_picture = rbs_studio_host::stream_profile::HoldPicture::Scene;
    profile.hold_scene = Some(brk as u32);
    profile.hold_sound = rbs_studio_host::stream_profile::HoldSound::Scene;
    host.send(Command::SetStreamProfile(profile));
    // The profile comes through the command channel, the call through the API: wait
    // until the host has taken it, or the call can be served first.
    c.event("settings.changed", |d| d["stream"]["hold_scene"] == brk);
    let got = c.call("settings.get", serde_json::json!({}));
    let stream = &got["result"]["data"]["stream"];
    assert_eq!(stream["hold_scene"], brk, "{got}");
    assert_eq!(stream["hold_sound"], "scene", "{got}");
    let toggle = c.call("stream.hold_toggle", serde_json::json!({}));
    assert!(toggle["result"].is_object(), "{toggle}");
    let held = c.event("stream.state", |d| d["held"] == true);
    assert_eq!(held["params"]["data"]["hold_scene"], brk, "{held}");
    assert_eq!(held["params"]["data"]["hold_sound"], "scene", "{held}");
    let toggle = c.call("stream.hold_toggle", serde_json::json!({}));
    assert!(toggle["result"].is_object(), "{toggle}");
    c.event("stream.state", |d| d["held"] == false);

    assert!(c.call("stream.stop", serde_json::json!({}))["result"].is_object());
    drop(host);
    drop(api);
}

/// ADR-0089 decision 3: with the profile's stream markers on, a recording that runs
/// while a stream starts and ends gets `stream_start` and `stream_end` markers at
/// those moments; with them off (the default) it gets none.
#[test]
pub(super) fn stream_markers_mark_a_running_recording_when_the_profile_asks() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    add_display(&host, &rx);
    let dir = std::env::temp_dir().join(format!("rbs-app-stream-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (name, on) in [("on.mkv", true), ("off.mkv", false)] {
        let markers = rbs_studio_host::record_profile::MarkerSettings {
            stream_events: on,
            ..Default::default()
        };
        host.send(Command::StartRecording(RecordRequest {
            path: dir.join(name),
            container: ContainerId::Matroska,
            encoder: None,
            video: Some(VideoSettings::default()),
            audio_bitrate_bps: 128_000,
            bit_depth: None,
            size: None,
            iso: false,
            editor_project: None,
            markers,
        }));
        assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
        clock.advance(Duration::from_secs(2));
        host.send(Command::StartStream(request()));
        assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
        clock.advance(Duration::from_secs(3));
        host.send(Command::EndStream);
        // The connection change can arrive after the end was asked: skipped.
        assert_eq!(ended(&rx), None);
        host.send(Command::StopRecording);
        let stopped = next_while(
            &rx,
            || clock.advance(END_TAIL_LIMIT),
            |u| match u {
                Update::Recording(r @ RecordingUpdate::Stopped { .. }) => Some(r),
                _ => None,
            },
        );
        assert!(
            matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
            "{stopped:?}"
        );
    }
    let text = std::fs::read_to_string(dir.join("on.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    let marks: Vec<_> = manifest
        .markers
        .iter()
        .map(|m| (m.kind, m.t_ns, m.label.as_str()))
        .collect();
    assert_eq!(
        marks,
        [
            (
                rbs_manifest::MarkerKind::StreamStart,
                2_000_000_000,
                "Stream started"
            ),
            (
                rbs_manifest::MarkerKind::StreamEnd,
                5_000_000_000,
                "Stream ended"
            ),
        ],
        "{text}"
    );
    // Off: no marker, so a plain recording writes no manifest.
    assert!(!dir.join("off.rbs.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
    drop(host);
}

/// ADR-0124 decision 10, control API 1.63: a marker added while a recording is paused
/// sits on the pause's join with `while_paused`, a later one is in file time, and the
/// manifest written at stop lists the pause.
#[test]
pub(super) fn markers_while_paused_sit_on_the_join_and_the_manifest_lists_the_pause() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let clock = TestClock::new();
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &clock);
    let (api, mut c) = api_client(&host);
    add_display(&host, &rx);
    let dir = std::env::temp_dir().join(format!("rbs-app-pause-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    host.send(Command::StartRecording(RecordRequest {
        path: dir.join("take.mkv"),
        container: ContainerId::Matroska,
        encoder: None,
        video: Some(VideoSettings::default()),
        audio_bitrate_bps: 128_000,
        bit_depth: None,
        size: None,
        iso: false,
        editor_project: None,
        markers: rbs_studio_host::record_profile::MarkerSettings::default(),
    }));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    let s = 1_000_000_000u64;
    clock.advance(Duration::from_secs(2));
    assert!(c.call("record.pause", serde_json::json!({}))["result"].is_object());
    clock.advance(Duration::from_secs(1));
    let during = c.call("marker.add", serde_json::json!({"label": "During"}));
    assert_eq!(
        during["result"]["markers"][0]["while_paused"], true,
        "{during}"
    );
    clock.advance(Duration::from_secs(2));
    assert!(c.call("record.resume", serde_json::json!({}))["result"].is_object());
    clock.advance(Duration::from_secs(1));
    let after = c.call("marker.add", serde_json::json!({"label": "After"}));
    assert!(
        after["result"]["markers"][0].get("while_paused").is_none(),
        "{after}"
    );
    host.send(Command::StopRecording);
    let stopped = next_while(
        &rx,
        || clock.advance(END_TAIL_LIMIT),
        |u| match u {
            Update::Recording(r @ RecordingUpdate::Stopped { .. }) => Some(r),
            _ => None,
        },
    );
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("take.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert_eq!(manifest.pauses.len(), 1, "{text}");
    let p = manifest.pauses[0];
    assert_eq!(p.at_file_ns, p.from_ns, "no earlier pause");
    assert!(
        p.from_ns >= 2 * s && p.to_ns >= 5 * s && p.to_ns < 6 * s,
        "{text}"
    );
    assert_eq!(manifest.paused_ns(), p.to_ns - p.from_ns);
    let [during, after] = &manifest.markers[..] else {
        panic!("{text}")
    };
    assert!(during.while_paused && !after.while_paused, "{text}");
    assert_eq!(during.t_ns, p.at_file_ns, "{text}");
    // Pressed 1 s after the resume was asked (a few ticks before the join): in file
    // time the pause is left out.
    let master = after.t_ns + manifest.paused_ns();
    assert!(master > p.to_ns && master <= p.to_ns + s, "{text}");
    assert_eq!(after.pressed_ns, Some(after.t_ns), "no lead");
    let _ = std::fs::remove_dir_all(&dir);
    drop(host);
    drop(api);
}

pub(super) const SECOND: &str = "rtmp://second.example.com/app";

/// The main canvas's stream with a second destination (its own key), a disabled third
/// and a fourth whose server refuses at once.
pub(super) fn destinations(
    keys: &MemoryKeyStore,
) -> rbs_studio_host::stream_destinations::Destinations {
    use rbs_studio_host::stream_destinations::{Canvas, Destinations};
    let mut d = Destinations::default();
    let second = d.add(Canvas::Main, "Second", "custom").unwrap();
    let e = d.entry_mut(second).unwrap();
    e.server = SECOND.into();
    e.key_saved = true;
    keys.write(
        &format!("{ACCOUNT}/dest-{second}"),
        &rbs_transport::Secret::new("second_key"),
    )
    .unwrap();
    let off = d.add(Canvas::Main, "Off", "custom").unwrap();
    let e = d.entry_mut(off).unwrap();
    e.server = "rtmp://off.example.com/app".into();
    e.key_saved = true;
    d.set_enabled(off, false);
    let refused = d.add(Canvas::Main, "Refused", "custom").unwrap();
    let e = d.entry_mut(refused).unwrap();
    e.server = "rtmp://refused.example.com/app".into();
    e.key_saved = true;
    keys.write(
        &format!("{ACCOUNT}/dest-{refused}"),
        &rbs_transport::Secret::new("refused_key"),
    )
    .unwrap();
    d
}

/// Control API 1.93 (ADR-0148 decision 10): every stream state says whether a
/// destination is on, a change publishes, and a canvas with none is refused first.
#[test]
pub(super) fn stream_available_follows_the_destinations() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys, Arc::clone(&server), &TestClock::new());
    let (api, mut c) = api_client(&host);
    c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["stream.state"]}),
    );
    // A new profile: no destination, so streaming is off.
    host.send(Command::SetStreamProfile(StreamProfile::new()));
    add_display(&host, &rx);
    c.event("stream.state", |d| d["available"] == false);
    let status = c.call("stream.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["available"], false, "{status}");
    // A first destination: available, published.
    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    host.send(Command::SetStreamProfile(profile));
    c.event("stream.state", |d| d["available"] == true);
    // Turned off in the list: off again, and stream.start is refused before a key is
    // read.
    let mut d = rbs_studio_host::stream_destinations::Destinations::default();
    assert!(d.set_enabled(0, false));
    host.send(Command::SetStreamDestinations(d.clone(), false));
    c.event("stream.state", |d| d["available"] == false);
    let refused = c.call("stream.start", serde_json::json!({}));
    assert_eq!(refused["error"]["data"]["kind"], "failed", "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Streaming is off"),
        "{refused}"
    );
    let all = c.call("stream.start", serde_json::json!({"canvas": "all"}));
    assert_eq!(all["error"]["data"]["kind"], "failed", "{all}");
    // A section that did not load counts as available.
    host.send(Command::SetStreamDestinations(d, true));
    c.event("stream.state", |d| d["available"] == true);
    assert!(server.sessions.lock().unwrap().is_empty());
    drop(api);
    drop(host);
}

#[test]
pub(super) fn go_live_sends_to_every_enabled_destination_with_its_own_key() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &TestClock::new());
    add_display(&host, &rx);
    host.send(Command::SetStreamDestinations(destinations(&keys), false));
    host.send(Command::StartStream(request()));
    match stream(&rx) {
        StreamUpdate::Started { url, .. } => assert_eq!(url, URL),
        other => panic!("{other:?}"),
    }
    // The refused destination is named; the others went live.
    let notice = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert!(notice.starts_with("Refused: "), "{notice}");
    // Each output opens its session on its own worker: in any order.
    let mut sessions = server.sessions.lock().unwrap().clone();
    sessions.sort();
    assert_eq!(
        sessions,
        [
            (URL.to_owned(), Some("live_abc".to_owned())),
            (SECOND.to_owned(), Some("second_key".to_owned())),
        ]
    );
    // Connected while one is; the connects of both are summed.
    link_where(&rx, |l| l.connected && l.connects == 2);
    host.send(Command::EndStream);
    next(&rx, |u| match u {
        Update::Stream(StreamUpdate::Ended { failure, .. }) => {
            assert_eq!(failure, None);
            Some(())
        }
        _ => None,
    });
    // Both were finished in the one stop.
    assert_eq!(*server.finished.lock().unwrap(), 2);
    drop(host);
}

#[test]
pub(super) fn a_destination_that_drops_ends_alone_and_the_stream_goes_on() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &TestClock::new());
    add_display(&host, &rx);
    host.send(Command::SetStreamDestinations(destinations(&keys), false));
    host.send(Command::StartStream(request()));
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    link_where(&rx, |l| l.connected && l.connects == 2);

    // The first destination (the stream's anchor) is refused by its server: it ends
    // alone, named, and the stream goes on with the second.
    server.events_for.lock().unwrap().push((
        URL.to_owned(),
        TransportEvent::Disconnected {
            error: TransportError::Rejected("unknown stream key".into()),
        },
    ));
    let notice = next(&rx, |u| match u {
        Update::Error(e) if e.contains("unknown stream key") => Some(e),
        Update::Stream(StreamUpdate::Ended { failure, .. }) => panic!("ended: {failure:?}"),
        _ => None,
    });
    assert!(notice.starts_with("Destination 0: "), "{notice}");
    // Then the second one: nothing is left, so the stream ends with its reason.
    server.events_for.lock().unwrap().push((
        SECOND.to_owned(),
        TransportEvent::Disconnected {
            error: TransportError::Rejected("second refused".into()),
        },
    ));
    let failure = next(&rx, |u| match u {
        Update::Stream(StreamUpdate::Ended { failure, .. }) => Some(failure),
        _ => None,
    });
    assert!(failure.unwrap().contains("second refused"));
    drop(host);
}

#[test]
pub(super) fn a_control_api_client_lists_starts_and_stops_destinations() {
    let Some(gpu) = gpu() else { return };
    let keys = Arc::new(MemoryKeyStore::new());
    keys.write(ACCOUNT, &rbs_transport::Secret::new("live_abc"))
        .unwrap();
    let server = Arc::new(Server::default());
    let (host, rx) = host(gpu, keys.clone(), Arc::clone(&server), &TestClock::new());
    let (api, mut c) = api_client(&host);
    c.call(
        "events.subscribe",
        serde_json::json!({"topics": ["stream.state"]}),
    );
    let mut profile = StreamProfile::new();
    profile.server = URL.into();
    profile.key_saved = true;
    host.send(Command::SetStreamProfile(profile));
    let saved = destinations(&keys);
    let [second, off, refused] = [2, 3, 4];
    assert_eq!(saved.get(off).map(|d| d.enabled), Some(false));
    host.send(Command::SetStreamDestinations(saved, false));
    add_display(&host, &rx);

    // The list: the first destination, then the others; never a server or a key.
    let list = c.call("stream.destinations", serde_json::json!({}));
    let rows = list["result"]["destinations"].as_array().unwrap().clone();
    let ids: Vec<_> = rows.iter().map(|r| r["id"].as_u64().unwrap()).collect();
    assert_eq!(ids, [0, 2, 3, 4], "{list}");
    assert_eq!(rows[1]["name"], "Second", "{list}");
    assert_eq!(rows[1]["configured"], true, "{list}");
    assert_eq!(rows[1]["encoding"], "shared", "{list}");
    assert_eq!(rows[2]["enabled"], false, "{list}");
    assert!(!list.to_string().contains("example.com"), "{list}");
    assert!(!list.to_string().contains("_key"), "{list}");
    let vertical = c.call(
        "stream.destinations",
        serde_json::json!({"canvas": "vertical"}),
    );
    assert_eq!(vertical["error"]["data"]["kind"], "not_found", "{vertical}");

    // Turning one on or off is saved by the UI; an unknown id is not found.
    let on = c.call(
        "stream.destination.set_enabled",
        serde_json::json!({"id": off, "enabled": true}),
    );
    assert!(on["result"].is_object(), "{on}");
    let d = next(&rx, |u| match u {
        Update::StreamDestinations(d) => Some(d),
        _ => None,
    });
    assert_eq!(d.get(off).map(|d| d.enabled), Some(true));
    c.call(
        "stream.destination.set_enabled",
        serde_json::json!({"id": off, "enabled": false}),
    );
    let unknown = c.call(
        "stream.destination.set_enabled",
        serde_json::json!({"id": 99, "enabled": true}),
    );
    assert_eq!(unknown["error"]["data"]["kind"], "not_found", "{unknown}");
    // A destination turned off is not started: turning it on stays a visible step.
    let disabled = c.call("stream.destination.start", serde_json::json!({"id": off}));
    assert_eq!(disabled["error"]["data"]["kind"], "conflict", "{disabled}");
    let idle_stop = c.call("stream.destination.stop", serde_json::json!({"id": second}));
    assert_eq!(
        idle_stop["error"]["data"]["kind"], "conflict",
        "{idle_stop}"
    );

    // Starting one while nothing streams goes live with it alone.
    let start = c.call(
        "stream.destination.start",
        serde_json::json!({"id": second}),
    );
    assert!(start["result"].is_object(), "{start}");
    assert!(matches!(stream(&rx), StreamUpdate::Started { .. }));
    assert_eq!(
        *server.sessions.lock().unwrap(),
        [(SECOND.to_owned(), Some("second_key".to_owned()))]
    );
    let live = c.stream_state(|d| d["state"] == "live" && d["destinations"][0]["state"] == "live");
    let rows = &live["params"]["data"]["destinations"];
    assert_eq!(rows.as_array().unwrap().len(), 1, "{live}");
    assert_eq!(rows[0]["id"], second, "{live}");
    assert!(!live.to_string().contains("example.com"), "{live}");
    let again = c.call(
        "stream.destination.start",
        serde_json::json!({"id": second}),
    );
    assert_eq!(again["error"]["data"]["kind"], "conflict", "{again}");

    // The first destination joins the running stream; a refused one fails alone.
    let join = c.call("stream.destination.start", serde_json::json!({"id": 0}));
    assert!(join["result"].is_object(), "{join}");
    c.stream_state(|d| {
        d["destinations"]
            .as_array()
            .is_some_and(|r| r.len() == 2 && r.iter().all(|r| r["state"] == "live"))
            && d["connects"] == 2
    });
    let no = c.call(
        "stream.destination.start",
        serde_json::json!({"id": refused}),
    );
    assert_eq!(no["error"]["data"]["kind"], "failed", "{no}");
    assert!(!no.to_string().contains("refused.example.com"), "{no}");

    // Stopping the anchor leaves the other on air; its transport is finished.
    let stop = c.call("stream.destination.stop", serde_json::json!({"id": second}));
    assert!(stop["result"].is_object(), "{stop}");
    assert_eq!(*server.finished.lock().unwrap(), 1);
    c.stream_state(|d| {
        d["state"] == "live"
            && d["destinations"].as_array().is_some_and(|r| {
                r.iter()
                    .any(|r| r["id"] == second && r["state"] == "stopped")
            })
    });
    // The last one ends the stream, as `stream.stop` does.
    let last = c.call("stream.destination.stop", serde_json::json!({"id": 0}));
    assert!(last["result"].is_object(), "{last}");
    let idle = c.stream_state(|d| d["state"] == "idle");
    assert_eq!(
        idle["params"]["data"]["last"]["failure"],
        serde_json::Value::Null
    );
    assert_eq!(*server.finished.lock().unwrap(), 2);
    drop(host);
    drop(api);
}
