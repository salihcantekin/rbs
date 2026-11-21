//! Recording tests, continued (markers onwards); split from record.rs by size.

use super::*;

/// ADR-0089: `marker.add` and `marker.remove` on the running recording; the markers
/// go into `<stem>.rbs.toml` beside a plain recording, and into the recording folder's
/// `recording.rbs.toml`, when it stops. No marker, no sidecar. The stop rewrite
/// completes the manifest of a take that kept its timing (separate files, or the
/// editor project setting), which writes its project or makes one with
/// `project.export`; a plain take without the setting stays incomplete.
#[test]
pub(super) fn markers_go_into_the_manifest_when_the_recording_stops() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, _display, mic) = marked(gpu, &clock, None);
    // `project.export` looks in the profile's recordings folder.
    let mut profile = rbs_studio_host::record_profile::RecordProfile::new();
    profile.folder = dir.clone();
    host.send(Command::SetProfile(profile));
    let mut api = ApiClient::attach(&host);

    let idle = api.call("marker.add", serde_json::json!({}));
    assert_eq!(error_kind(&idle), "conflict", "{idle}");
    // Every `marker.add` counts toward the session's two a second.
    api.reconnect();

    // A plain recording (`iso` off).
    let plain = dir.join("plain.mkv");
    host.send(Command::StartRecording(request(plain.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(3));
    let added = api.call(
        "marker.add",
        serde_json::json!({"label": "Good take", "color": "green"}),
    );
    assert_eq!(
        added["result"],
        serde_json::json!({"markers": [{"canvas": "main", "index": 1, "t_ns": 3_000_000_000u64}]}),
        "{added}"
    );
    let bad = api.call("marker.add", serde_json::json!({"color": "pink"}));
    assert_eq!(error_kind(&bad), "invalid_params", "{bad}");
    clock.advance(Duration::from_secs(1));
    let second = api.call("marker.add", serde_json::json!({}));
    assert_eq!(second["result"]["markers"][0]["index"], 2, "{second}");
    let removed = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "main", "index": 2}),
    );
    assert!(removed.get("error").is_none(), "{removed}");
    let again = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "main", "index": 2}),
    );
    assert_eq!(error_kind(&again), "not_found", "{again}");
    let vertical = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "vertical", "index": 1}),
    );
    assert_eq!(
        error_kind(&vertical),
        "not_found",
        "no vertical canvas: {vertical}"
    );
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("plain.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert_eq!(manifest.files.len(), 1);
    assert_eq!(manifest.files[0].file_name, "plain.mkv");
    assert_eq!(manifest.markers.len(), 1, "{text}");
    let m = &manifest.markers[0];
    assert_eq!(
        (m.index, m.t_ns, m.label.as_str(), m.color),
        (
            1,
            3_000_000_000,
            "Good take",
            rbs_manifest::MarkerColor::Green
        )
    );
    assert!(
        !text.contains(&dir.display().to_string()),
        "no path: {text}"
    );
    assert!(
        !manifest.is_complete(),
        "no timing without separate files or the editor project setting: {text}"
    );
    assert!(manifest.files[0].timing.is_none(), "{text}");
    api.reconnect();
    let refused = api.call(
        "project.export",
        serde_json::json!({"recording": "plain.mkv", "format": "fcpxml"}),
    );
    assert_eq!(error_kind(&refused), "failed", "{refused}");
    for (name, kind) in [
        ("../plain.mkv", "invalid_params"),
        ("quiet.mkv", "not_found"),
        ("nothing", "not_found"),
    ] {
        let answer = api.call(
            "project.export",
            serde_json::json!({"recording": name, "format": "fcpxml"}),
        );
        assert_eq!(error_kind(&answer), kind, "{name}: {answer}");
    }

    // No marker: no sidecar.
    let quiet = dir.join("quiet.mkv");
    host.send(Command::StartRecording(request(quiet.clone(), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    stop_recording(&host, &rx, &clock);
    assert!(quiet.is_file());
    assert!(!dir.join("quiet.rbs.toml").exists());

    // A recording folder: the markers join the start manifest.
    api.reconnect();
    let take = dir.join("take.mkv");
    host.send(Command::StartRecording(with_tracks(take, None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(2));
    let added = api.call("marker.add", serde_json::json!({"canvas": "main"}));
    assert_eq!(added["result"]["markers"][0]["index"], 1, "{added}");
    written_some(&rx, &clock);
    stop_recording(&host, &rx, &clock);
    let text = std::fs::read_to_string(dir.join("take").join("recording.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert_eq!(manifest.markers.len(), 1, "{text}");
    assert_eq!(manifest.markers[0].label, "Marker 1");
    assert!(
        manifest
            .files
            .iter()
            .any(|f| f.source_name.as_deref() == Some(mic.name.as_str())),
        "{text}"
    );
    assert!(manifest.markers[0].t_ns >= 2_000_000_000, "{text}");
    // The stop rewrite: complete, and every file placed from `t0` with its format.
    assert!(manifest.is_complete(), "{text}");
    let master = manifest.files.iter().find(|f| f.role == "master").unwrap();
    let timing = master.timing.unwrap_or_else(|| panic!("{text}"));
    assert!(timing.duration_ns > 0, "{text}");
    // Nothing renders on the still clock, so the first frame comes once the clock
    // moves (after the marker): seconds after `t0` here, a frame or so in real use.
    assert!(timing.start_offset_ns < 5_000_000_000, "{text}");
    assert!(timing.picture.is_some() && timing.sound.is_some(), "{text}");
    let audio = manifest.files.iter().find(|f| f.role == "audio").unwrap();
    let sound = audio.timing.unwrap_or_else(|| panic!("{text}"));
    assert!(sound.picture.is_none() && sound.sound.is_some(), "{text}");
    let done = manifest.completion.unwrap();
    assert_eq!(done.picture, timing.picture);
    assert!(
        done.duration_ns >= timing.start_offset_ns + timing.duration_ns,
        "{text}"
    );
    // `project.export` writes the project next to the manifest, never replacing one.
    api.reconnect();
    let first = api.call(
        "project.export",
        serde_json::json!({"recording": "take", "format": "fcpxml"}),
    );
    assert_eq!(
        first["result"],
        serde_json::json!({"file": "take.fcpxml", "missing": []}),
        "{first}"
    );
    let second = api.call(
        "project.export",
        serde_json::json!({"recording": "take", "format": "fcpxml"}),
    );
    assert_eq!(second["result"]["file"], "take 2.fcpxml", "{second}");
    let project = std::fs::read_to_string(dir.join("take").join("take.fcpxml")).unwrap();
    assert!(project.contains("<fcpxml version=\"1.9\">"), "{project}");
    assert!(project.contains("master.mkv"), "{project}");
    // FCP7 XML, the format DaVinci Resolve reads markers from.
    let xml = api.call(
        "project.export",
        serde_json::json!({"recording": "take", "format": "xmeml"}),
    );
    assert_eq!(xml["result"]["file"], "take.xml", "{xml}");
    let project = std::fs::read_to_string(dir.join("take").join("take.xml")).unwrap();
    assert!(project.contains("<xmeml version=\"5\">"), "{project}");
    assert!(project.contains("master.mkv"), "{project}");

    // The editor project setting: a plain take keeps its timing and writes its
    // project when it stops, with no marker.
    let mut with_project = request(dir.join("edit.mkv"), None);
    with_project.editor_project = Some(rbs_studio_host::record_profile::EditorProject::Fcpxml);
    host.send(Command::StartRecording(with_project));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    written_some(&rx, &clock);
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("edit.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert!(manifest.is_complete(), "{text}");
    assert!(manifest.markers.is_empty(), "{text}");
    assert!(
        manifest.files[0].timing.unwrap().start_offset_ns < 5_000_000_000,
        "{text}"
    );
    assert!(dir.join("edit.fcpxml").is_file());

    drop(api);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `record.estimate` (protocol 1.53, ADR-0087 step C2a): the plan of a start of the
/// saved profile next to what runs, its separate files listed with their cost, and
/// the live limit of separate video files (3 while the encoder's capacity is
/// unknown). It opens nothing.
#[test]
pub(super) fn the_estimate_lists_a_start_and_its_separate_files_without_opening_anything() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let (host, rx, display, mic) = marked(gpu, &clock, None);
    host.send(mark(
        display.id,
        RecordPatch {
            video: Some(true),
            ..RecordPatch::default()
        },
    ));
    rows_where(&rx, |r| {
        r.iter()
            .any(|r| r.id == display.id && r.record_separately.video)
    });
    let mut api = ApiClient::attach(&host);

    let answer = api.call("record.estimate", serde_json::json!({"profile": "default"}));
    let e = &answer["result"];
    assert!(answer.get("error").is_none(), "{answer}");
    assert!(
        ["fits", "heavy"].contains(&e["level"].as_str().unwrap()),
        "{answer}"
    );
    assert_eq!(e["max_tracks"]["video"]["at_most_now"], 3, "{answer}");
    let tracks: Vec<(u64, &str)> = e["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["source"].as_u64().unwrap(), t["role"].as_str().unwrap()))
        .collect();
    assert!(
        tracks.contains(&(u64::from(display.id.get()), "video")),
        "{answer}"
    );
    assert!(
        tracks.contains(&(u64::from(mic.id.get()), "audio")),
        "{answer}"
    );
    // The master and the screen's file, each one session on the test encoder; whole
    // numbers on the wire.
    let sessions = e["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["resource"] == "encoder_sessions")
        .unwrap_or_else(|| panic!("{answer}"));
    assert_eq!(sessions["need"], 2, "{answer}");
    assert!(sessions["scope"].is_string(), "{answer}");

    // Without the separate files: the master alone, no live limit.
    let plain = api.call(
        "record.estimate",
        serde_json::json!({"profile": "default", "iso": false}),
    );
    assert_eq!(plain["result"]["tracks"], serde_json::json!([]), "{plain}");
    assert!(
        plain["result"]["max_tracks"]["video"]
            .get("at_most_now")
            .is_none(),
        "{plain}"
    );
    // `all` without a vertical canvas is the main one; an explicit vertical is not
    // found; an unknown profile too.
    let all = api.call(
        "record.estimate",
        serde_json::json!({"profile": "default", "canvas": "all"}),
    );
    assert_eq!(all["result"]["tracks"], e["tracks"], "{all}");
    let vertical = api.call(
        "record.estimate",
        serde_json::json!({"profile": "default", "canvas": "vertical"}),
    );
    assert_eq!(error_kind(&vertical), "not_found", "{vertical}");
    let other = api.call("record.estimate", serde_json::json!({"profile": "other"}));
    assert_eq!(error_kind(&other), "not_found", "{other}");
    // Nothing started.
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");
}

/// Points the API's `default` profile at `folder`, and waits until the host has it.
pub(super) fn profile_in(host: &StudioHost, rx: &Receiver<Update>, folder: PathBuf) {
    let mut p = rbs_studio_host::record_profile::RecordProfile::new();
    p.folder = folder;
    host.send(Command::SetProfile(p));
    // The host takes commands in order: once this answer comes, the profile is set.
    host.send(Command::ListTargets(SourceKind::Screen));
    next(rx, |u| match u {
        Update::Targets(SourceKind::Screen, _) => Some(()),
        _ => None,
    });
}

/// The one take folder in `folder`, and its manifest.
pub(super) fn take_manifest(
    folder: &std::path::Path,
) -> (PathBuf, rbs_manifest::RecordingManifest) {
    let takes: Vec<PathBuf> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(takes.len(), 1, "{takes:?}");
    let text = std::fs::read_to_string(takes[0].join("recording.rbs.toml")).unwrap();
    (
        takes[0].clone(),
        rbs_manifest::RecordingManifest::from_toml(&text).unwrap(),
    )
}

/// A host with `n` screens, each marked to record its picture separately; their ids in
/// order.
pub(super) fn screens(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    n: usize,
) -> (StudioHost, Receiver<Update>, Vec<u32>) {
    let (host, rx) = host(gpu, clock);
    let ids = mark_screens(&host, &rx, n);
    (host, rx, ids)
}

/// Adds `n` screens to a host that just spawned, each marked to record its picture
/// separately; their ids in order.
pub(super) fn mark_screens(host: &StudioHost, rx: &Receiver<Update>, n: usize) -> Vec<u32> {
    next(rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    host.send(Command::ListTargets(SourceKind::Screen));
    let target = next(rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    for _ in 0..n {
        host.send(Command::AddSource {
            kind: SourceKind::Screen,
            target: target.clone(),
        });
    }
    let rows = rows_where(rx, |r| r.len() == n);
    for r in &rows {
        host.send(mark(r.id, RecordPatch::enabled(true)));
    }
    rows_where(rx, |r| {
        r.iter().filter(|r| r.record_separately.video).count() == n
    });
    let mut ids: Vec<u32> = rows.iter().map(|r| r.id.get()).collect();
    ids.sort_unstable();
    ids
}

/// Control API 1.54 (ADR-0087 decisions 5 and 7): with four screens marked and the
/// encoder's capacity unknown (cap 3), `ask` refuses before anything is on disk,
/// `skip` and an older request start with three files and list the fourth, also in
/// the manifest's `[[skipped]]`; `cpu` is refused.
#[test]
pub(super) fn the_start_gate_refuses_or_skips_video_files_past_the_encoder_cap() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-gate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx, ids) = screens(gpu, &clock, 4);
    let mut api = ApiClient::attach(&host);
    let fourth = serde_json::json!([{"source": ids[3], "role": "video"}]);

    profile_in(&host, &rx, dir.join("cpu"));
    let cpu = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "cpu"}),
    );
    assert_eq!(error_kind(&cpu), "invalid_params", "{cpu}");

    let ask_dir = dir.join("ask");
    profile_in(&host, &rx, ask_dir.clone());
    let ask = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "ask"}),
    );
    assert_eq!(ask["error"]["code"], -32040, "{ask}");
    assert_eq!(error_kind(&ask), "failed", "{ask}");
    assert_eq!(ask["error"]["data"]["reason"], "hardware_limit", "{ask}");
    assert_eq!(ask["error"]["data"]["tracks"], fourth, "{ask}");
    assert!(
        ask["error"]["data"]["estimate"]["level"].is_string(),
        "{ask}"
    );
    // Refused before the folder was made.
    assert!(
        std::fs::read_dir(&ask_dir).map_or(true, |mut d| d.next().is_none()),
        "nothing on disk"
    );
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");

    for (name, params) in [
        (
            "skip",
            serde_json::json!({"profile": "default", "on_hardware_limit": "skip"}),
        ),
        (
            "older",
            serde_json::json!({"profile": "default", "acknowledge_heavy": null, "on_hardware_limit": null}),
        ),
    ] {
        let folder = dir.join(name);
        profile_in(&host, &rx, folder.clone());
        let started = api.call("record.start", params);
        assert!(started.get("error").is_none(), "{name}: {started}");
        // The UI is told as if its own button started it.
        assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
        assert_eq!(
            started["result"]["skipped"],
            serde_json::json!([{"source": ids[3], "role": "video", "reason": "limit"}]),
            "{name}: {started}"
        );
        assert!(
            started["result"]["estimate"].is_object(),
            "{name}: {started}"
        );
        let (take, manifest) = take_manifest(&folder);
        let videos = manifest.files.iter().filter(|f| f.role == "video").count();
        assert_eq!(videos, 3, "{name}: {manifest:?}");
        assert_eq!(manifest.skipped.len(), 1, "{name}: {manifest:?}");
        assert_eq!(manifest.skipped[0].source.get(), ids[3]);
        assert_eq!(manifest.skipped[0].reason, "limit");
        assert_eq!(manifest.skipped[0].role, "video");
        match stop_recording(&host, &rx, &clock) {
            RecordingUpdate::Stopped { failure, .. } => assert_eq!(failure, None, "{name}"),
            other => panic!("{name}: {other:?}"),
        }
        // The stop rewrite keeps the skipped source.
        let text = std::fs::read_to_string(take.join("recording.rbs.toml")).unwrap();
        let after = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
        assert_eq!(after.skipped, manifest.skipped, "{name}");
    }

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control API 1.54 (ADR-0087 decisions 6 and 7), with a calibrated recording folder
/// whose write speed makes the master with one separate file heavy and the master
/// alone fit: a new request without `acknowledge_heavy` is refused (`heavy`), one with
/// it starts the file; an older request is never refused, trims the file and, with
/// nothing else marked, records one file as 1.46 does.
#[test]
pub(super) fn a_heavy_plan_is_refused_without_acknowledgement_and_an_older_start_trims_to_one_file()
{
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-heavy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let folder = dir.join("takes");
    let clock = TestClock::new();
    let disk_need = |answer: &serde_json::Value| {
        answer["result"]["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["resource"] == "disk_speed")
            .unwrap_or_else(|| panic!("{answer}"))["need"]
            .as_f64()
            .unwrap()
    };
    // What the start writes, from an uncalibrated host's estimate.
    let (full, alone) = {
        let (host, rx, _) = screens(gpu.clone(), &clock, 1);
        profile_in(&host, &rx, folder.clone());
        let mut api = ApiClient::attach(&host);
        let full = api.call("record.estimate", serde_json::json!({"profile": "default"}));
        let alone = api.call(
            "record.estimate",
            serde_json::json!({"profile": "default", "iso": false}),
        );
        (disk_need(&full), disk_need(&alone))
    };
    assert!(full > alone && alone > 0.0, "{full} {alone}");
    // The master with its file at 85 % of the disk: heavy; the master alone fits.
    let write_speed = (full / 0.85).ceil() as u64;
    assert!(alone < 0.7 * write_speed as f64, "{alone} of {write_speed}");
    let mut config = HostConfig::desktop();
    config
        .calibration
        .folders
        .push(rbs_studio_host::calibration_file::FolderEntry {
            path: folder.clone(),
            write_speed,
            measured_at: 0,
            partial: false,
        });
    let (host, rx) = spawn_host(gpu, &clock, config);
    let ids = mark_screens(&host, &rx, 1);
    profile_in(&host, &rx, folder.clone());
    let mut api = ApiClient::attach(&host);

    let heavy = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "skip"}),
    );
    assert_eq!(heavy["error"]["data"]["reason"], "heavy", "{heavy}");
    assert_eq!(
        heavy["error"]["data"]["estimate"]["gate_level"], "heavy",
        "{heavy}"
    );
    assert!(heavy["error"]["data"].get("tracks").is_none(), "{heavy}");
    assert!(
        std::fs::read_dir(&folder).map_or(true, |mut d| d.next().is_none()),
        "nothing on disk"
    );

    let acknowledged = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "acknowledge_heavy": true}),
    );
    assert!(acknowledged.get("error").is_none(), "{acknowledged}");
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    assert!(
        acknowledged["result"].get("skipped").is_none(),
        "{acknowledged}"
    );
    let (_, manifest) = take_manifest(&folder);
    assert_eq!(
        manifest.files.iter().filter(|f| f.role == "video").count(),
        1,
        "{manifest:?}"
    );
    stop_recording(&host, &rx, &clock);
    std::fs::remove_dir_all(&folder).unwrap();

    let older = api.call("record.start", serde_json::json!({"profile": "default"}));
    assert!(older.get("error").is_none(), "{older}");
    match recording(&rx) {
        RecordingUpdate::Started { path, separate, .. } => {
            // One file, not a folder (ADR-0087 decision 7).
            assert_eq!(path.parent(), Some(folder.as_path()), "{path:?}");
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("mkv"));
            assert!(separate.is_empty(), "{separate:?}");
        }
        other => panic!("{other:?}"),
    }
    let skipped = serde_json::json!([{"source": ids[0], "role": "video", "reason": "limit"}]);
    assert_eq!(older["result"]["skipped"], skipped, "{older}");
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["skipped"], skipped, "{status}");
    stop_recording(&host, &rx, &clock);

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Control API 1.54 (ADR-0087 decision 5, control API "Opening the files"): a file
/// whose hardware encoder refuses a session never falls to a software encoder. Under
/// `ask` the start is refused after the master ran, and what opened is stopped and
/// removed; an older request skips the file (`open_failed`) and keeps its folder.
#[test]
pub(super) fn a_file_whose_hardware_encoder_refuses_is_skipped_or_refuses_the_start_under_ask() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-hw-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = spawn_host_hw(gpu, &clock, HostConfig::desktop(), true, false);
    let ids = mark_screens(&host, &rx, 1);
    let mut api = ApiClient::attach(&host);
    let fallback = |answer: &serde_json::Value, at: &str| {
        answer[at]["estimate"]["reasons"]
            .as_array()
            .is_some_and(|r| {
                r.iter()
                    .any(|r| r["code"] == "software_fallback" && r["track"]["source"] == ids[0])
            })
    };

    let ask_dir = dir.join("ask");
    profile_in(&host, &rx, ask_dir.clone());
    let ask = api.call(
        "record.start",
        serde_json::json!({"profile": "default", "on_hardware_limit": "ask"}),
    );
    assert_eq!(ask["error"]["data"]["reason"], "hardware_limit", "{ask}");
    assert_eq!(
        ask["error"]["data"]["tracks"],
        serde_json::json!([{"source": ids[0], "role": "video"}]),
        "{ask}"
    );
    assert!(fallback(&ask["error"], "data"), "{ask}");
    // The master ran and was stopped; its folder is gone; nothing is recording.
    assert!(
        std::fs::read_dir(&ask_dir).map_or(true, |mut d| d.next().is_none()),
        "{:?}",
        std::fs::read_dir(&ask_dir).map(|d| d.map(|e| e.unwrap().path()).collect::<Vec<_>>())
    );
    let status = api.call("record.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"]["state"], "idle", "{status}");

    let older_dir = dir.join("older");
    profile_in(&host, &rx, older_dir.clone());
    let older = api.call("record.start", serde_json::json!({"profile": "default"}));
    assert!(older.get("error").is_none(), "{older}");
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    assert_eq!(
        older["result"]["skipped"],
        serde_json::json!([{"source": ids[0], "role": "video", "reason": "open_failed"}]),
        "{older}"
    );
    assert!(fallback(&older, "result"), "{older}");
    let (_, manifest) = take_manifest(&older_dir);
    assert_eq!(manifest.skipped.len(), 1, "{manifest:?}");
    assert_eq!(manifest.skipped[0].reason, "open_failed");
    stop_recording(&host, &rx, &clock);

    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------------
// ADR-0087 step D1: *Test this computer* (control API 1.55).

/// The calibration run's clock moves only through its pacer and sink.
pub(super) struct CalPacer(TestClock);

impl rbs_record::calibration::Pacer for CalPacer {
    fn yield_now(&self) {
        self.0.advance(Duration::from_micros(50));
    }
    fn wait_until(&self, t: rbs_core::time::Timestamp) {
        use rbs_core::time::Clock as _;
        if self.0.now() < t {
            self.0.set(t);
        }
    }
}

/// A write sink in memory; `create` waits on `gate` when one is given.
pub(super) struct CalSink {
    pub(super) clock: TestClock,
    pub(super) gate: Option<Arc<std::sync::Mutex<Receiver<()>>>>,
}

impl rbs_record::calibration::WriteSink for CalSink {
    fn list(&mut self, _: &std::path::Path) -> std::io::Result<Vec<(String, u64)>> {
        Ok(Vec::new())
    }
    fn remove(&mut self, _: &std::path::Path) -> std::io::Result<()> {
        Ok(())
    }
    fn create(&mut self, _: &std::path::Path) -> std::io::Result<()> {
        if let Some(g) = &self.gate {
            let _ = g.lock().unwrap().recv();
        }
        Ok(())
    }
    fn write(&mut self, _: &[u8]) -> std::io::Result<()> {
        self.clock.advance(Duration::from_millis(10));
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        self.clock.advance(Duration::from_millis(10));
        Ok(())
    }
}

pub(super) fn calibration_setup(
    clock: &TestClock,
    gate: Option<Receiver<()>>,
) -> rbs_studio_host::studio_host::CalibrationSetup {
    let gate = gate.map(|g| Arc::new(std::sync::Mutex::new(g)));
    let sink_clock = clock.clone();
    rbs_studio_host::studio_host::CalibrationSetup {
        clock: Arc::new(clock.clone()),
        pacer: Arc::new(CalPacer(clock.clone())),
        sink: Arc::new(move || {
            Box::new(CalSink {
                clock: sink_clock.clone(),
                gate: gate.clone(),
            })
        }),
        write_bytes: 64 * 1024 * 1024,
        now_unix: Arc::new(|| 1_000_000),
        export_rig: fake_export_rig(clock, true),
    }
}

/// A scripted export rig on the run's clock (step D2): each tick costs 400 µs of the
/// render thread and each conversion 300 µs of the GPU; `works: false` has no device.
pub(super) fn fake_export_rig(
    clock: &TestClock,
    works: bool,
) -> rbs_studio_host::studio_host::ExportRigMaker {
    use rbs_record::calibration::{ExportClass, ExportCounters, ExportRig, RigError};
    struct Rig {
        clock: TestClock,
        counters: ExportCounters,
        in_flight: u64,
    }
    impl ExportRig for Rig {
        fn begin(&mut self, _: &ExportClass) -> Result<(), RigError> {
            Ok(())
        }
        fn tick(&mut self) -> Result<(), RigError> {
            self.counters.frames += self.in_flight;
            self.in_flight = 1;
            self.clock.advance(Duration::from_micros(400));
            Ok(())
        }
        fn wait(&mut self, _: Duration) -> Result<bool, RigError> {
            self.clock.advance(Duration::from_micros(300));
            Ok(true)
        }
        fn counters(&self) -> ExportCounters {
            self.counters
        }
        fn end(&mut self) {}
    }
    let clock = clock.clone();
    Arc::new(move |_, _| {
        if works {
            Ok(Box::new(Rig {
                clock: clock.clone(),
                counters: ExportCounters::default(),
                in_flight: 0,
            }) as Box<dyn ExportRig>)
        } else {
            Err(RigError::Failed)
        }
    })
}

/// ADR-0087 step D2 (control API 1.57): with a screen marked, *Test this computer*
/// measures the export of its size class first, saves it, and the estimate's render
/// share row no longer asks for a test (the previous tick, step E, is still unknown).
/// Without a throwaway device the class is skipped and the row stops asking.
#[test]
pub(super) fn test_this_computer_measures_the_export_of_separate_files() {
    for works in [true, false] {
        let Some(gpu) = gpu() else { return };
        let clock = TestClock::new();
        let cal = TestClock::new();
        let dir = temp_dir(if works { "export" } else { "export-none" });
        let mut config = HostConfig::desktop();
        config.config_dir = Some(dir.join("config"));
        let mut setup = calibration_setup(&cal, None);
        setup.export_rig = fake_export_rig(&cal, works);
        config.calibration_setup = Some(setup);
        let (host, rx) = spawn_host(gpu, &clock, config);
        mark_screens(&host, &rx, 1);
        profile_in(&host, &rx, dir.join("rec"));
        let mut api = ApiClient::attach(&host);
        api.call(
            "events.subscribe",
            serde_json::json!({"topics": ["calibration.state"]}),
        );
        let share = |api: &mut ApiClient| {
            let e = api.call("record.estimate", serde_json::json!({"profile": "default"}));
            e["result"]["resources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["resource"] == "iso_render_share")
                .cloned()
                .unwrap_or_else(|| panic!("{e}"))
        };
        let before = share(&mut api);
        assert_eq!(before["level"], "unknown", "{before}");
        assert_eq!(before["calibrate"], true, "{before}");

        let run = api.call("calibration.run", serde_json::json!({"profile": "default"}));
        assert!(run.get("error").is_none(), "{run}");
        let done = api.event("calibration.state", |d| d["state"] == "done");
        let after = share(&mut api);
        assert_eq!(after["level"], "unknown", "{after}");
        assert_eq!(after["calibrate"], false, "{after}");
        if works {
            let exports = done["exports"].as_array().unwrap();
            assert_eq!(exports.len(), 1, "{done}");
            assert_eq!(exports[0]["format"], "nv12", "{done}");
            assert_eq!(exports[0]["time_us"], 400, "{done}");
            assert!(
                done["exports_skipped"].as_array().unwrap().is_empty(),
                "{done}"
            );
            // The export part leaves the 1.55 list as it was.
            assert!(
                done["skipped"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|s| s.get("size").is_none()),
                "{done}"
            );
            let text =
                std::fs::read_to_string(dir.join("config").join("calibration.toml")).unwrap();
            assert!(
                text.contains("[[exports]]") && text.contains("time_us = 400"),
                "{text}"
            );
            assert!(after["need"].as_f64().unwrap() > 0.0, "{after}");
        } else {
            assert_eq!(
                done["exports_skipped"][0]["code"], "gpu_unavailable",
                "{done}"
            );
            assert!(done["exports"].as_array().unwrap().is_empty(), "{done}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

pub(super) fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rbs-cal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(super) fn calibration_host(
    gpu: Arc<GpuContext>,
    clock: &TestClock,
    cal_clock: &TestClock,
    gate: Option<Receiver<()>>,
    name: &str,
) -> (StudioHost, Receiver<Update>, PathBuf) {
    let dir = temp_dir(name);
    let mut config = HostConfig::desktop();
    config.config_dir = Some(dir.join("config"));
    config.calibration_setup = Some(calibration_setup(cal_clock, gate));
    let (host, rx) = spawn_host(gpu, clock, config);
    profile_in(&host, &rx, dir.join("rec"));
    (host, rx, dir)
}

#[test]
pub(super) fn test_this_computer_measures_saves_and_the_estimate_uses_it() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let cal = TestClock::new();
    let (host, _rx, dir) = calibration_host(gpu, &clock, &cal, None, "measure");
    let mut api = ApiClient::attach(&host);
    let sub = api.call(
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    assert!(sub.get("error").is_none(), "{sub}");
    let idle = api.call("calibration.status", serde_json::json!({}));
    assert_eq!(idle["result"]["data"]["state"], "idle", "{idle}");

    // One canvas per run; an unknown profile is not found.
    let all = api.call(
        "calibration.run",
        serde_json::json!({"profile": "default", "canvas": "all"}),
    );
    assert_eq!(error_kind(&all), "invalid_params", "{all}");
    let other = api.call("calibration.run", serde_json::json!({"profile": "other"}));
    assert_eq!(error_kind(&other), "not_found", "{other}");

    let run = api.call("calibration.run", serde_json::json!({"profile": "default"}));
    assert!(run.get("error").is_none(), "{run}");
    let done = api.event("calibration.state", |d| d["state"] == "done");
    assert_eq!(done["saved"], true, "{done}");
    assert_eq!(done["cancelled"], false, "{done}");
    // The working fake keeps up to the ceiling; the broken one is unsupported.
    let encoders = done["encoders"].as_array().unwrap();
    let working = encoders
        .iter()
        .find(|e| e["adapter"] == WORKING)
        .unwrap_or_else(|| panic!("{done}"));
    assert_eq!(working["stopped_by"], "ceiling", "{done}");
    assert_eq!(
        working["sessions"],
        rbs_record::calibration::SESSION_CEILING,
        "{done}"
    );
    assert!(
        done["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["code"] == "unsupported" && s["adapter"] == BROKEN),
        "{done}"
    );
    assert!(done["write_speed"].as_u64().unwrap() > 0, "{done}");
    // No path in the state.
    assert!(!done.to_string().contains("rbs-cal-"), "{done}");
    let text = std::fs::read_to_string(dir.join("config").join("calibration.toml")).unwrap();
    assert!(text.contains("stopped_by = \"ceiling\""), "{text}");

    // The next estimate reads the measurement.
    let e = api.call("record.estimate", serde_json::json!({"profile": "default"}));
    // The likely master encoder is the broken fake (unsupported, so unmeasured);
    // the folder speed is measured either way.
    let disk = e["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["resource"] == "disk_speed")
        .cloned()
        .unwrap_or_else(|| panic!("{e}"));
    assert_eq!(disk["measured"], true, "{e}");
    assert!(text.contains(WORKING), "{text}");
    let status = api.call("calibration.status", serde_json::json!({}));
    assert_eq!(status["result"]["data"], done, "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
pub(super) fn starts_are_refused_while_the_test_runs_and_a_cancel_ends_it() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let cal = TestClock::new();
    let (gate_tx, gate_rx) = mpsc::channel();
    let (host, rx, dir) = calibration_host(gpu, &clock, &cal, Some(gate_rx), "refuse");
    let mut api = ApiClient::attach(&host);
    api.call(
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    let run = api.call("calibration.run", serde_json::json!({"profile": "default"}));
    assert!(run.get("error").is_none(), "{run}");
    // The write part waits on the gate: the run stays active.
    api.event("calibration.state", |d| {
        d["state"] == "running" && d["part"] == "write"
    });
    for (method, params) in [
        ("record.start", serde_json::json!({"profile": "default"})),
        ("stream.start", serde_json::json!({})),
        ("replay.arm", serde_json::json!({})),
        ("calibration.run", serde_json::json!({"profile": "default"})),
    ] {
        let a = api.call(method, params);
        assert_eq!(error_kind(&a), "conflict", "{method}: {a}");
    }
    // The desktop's Record says why.
    let path = dir.join("rec").join("x.mkv");
    host.send(Command::StartRecording(request(path, None)));
    match recording(&rx) {
        RecordingUpdate::NotStarted(why) => {
            assert!(why.contains("Test this computer is running"), "{why}");
        }
        other => panic!("{other:?}"),
    }

    let cancel = api.call("calibration.cancel", serde_json::json!({}));
    assert!(cancel.get("error").is_none(), "{cancel}");
    let cancelling = api.call("calibration.status", serde_json::json!({}));
    assert_eq!(
        cancelling["result"]["data"]["state"], "running",
        "{cancelling}"
    );
    assert_eq!(
        cancelling["result"]["data"]["cancelling"], true,
        "{cancelling}"
    );
    gate_tx.send(()).unwrap();
    let done = api.event("calibration.state", |d| d["state"] == "done");
    assert_eq!(done["cancelled"], true, "{done}");
    assert_eq!(done["partial"], true, "{done}");
    // Starts are allowed again.
    let status = api.call("record.status", serde_json::json!({}));
    assert!(status.get("error").is_none(), "{status}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
pub(super) fn a_run_that_does_not_answer_ends_at_the_host_deadline() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let cal = TestClock::new();
    let (_gate_tx, gate_rx) = mpsc::channel::<()>();
    let (host, _rx, dir) = calibration_host(gpu, &clock, &cal, Some(gate_rx), "hung");
    let mut api = ApiClient::attach(&host);
    api.call(
        "events.subscribe",
        serde_json::json!({"topics": ["calibration.state"]}),
    );
    api.call("calibration.run", serde_json::json!({"profile": "default"}));
    api.event("calibration.state", |d| {
        d["state"] == "running" && d["part"] == "write"
    });
    // The write never returns; past the limit and the grace the host ends the run
    // with what it received.
    cal.advance(Duration::from_secs(70));
    let done = api.event("calibration.state", |d| d["state"] == "done");
    assert_eq!(done["partial"], true, "{done}");
    assert!(!done["encoders"].as_array().unwrap().is_empty(), "{done}");
    let a = api.call("record.status", serde_json::json!({}));
    assert!(a.get("error").is_none(), "{a}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0089 decisions 3 and 4: scene changes mark a running recording (on by
/// default) with the new program scene's name, the lead moves a manual marker
/// earlier and keeps the press time, and with scene markers off a switch adds none.
#[test]
pub(super) fn scene_markers_and_the_marker_lead_follow_the_request() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-auto-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new();
    let (host, rx, _display, _mic) = marked(gpu, &clock, None);
    let mut api = ApiClient::attach(&host);
    let list = api.call("scene.list", serde_json::json!({}));
    let first = list["result"]["data"]["program"].clone();
    assert!(first.is_u64(), "{list}");
    let added = api.call("scene.add", serde_json::json!({"name": "Break"}));
    let second = added["result"]["scene"].clone();
    assert!(second.is_u64(), "{added}");

    let mut with_lead = request(dir.join("auto.mkv"), None);
    with_lead.markers.lead_ms = 2_000;
    host.send(Command::StartRecording(with_lead));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(5));
    let switched = api.call("scene.switch", serde_json::json!({"scene": second}));
    assert!(switched.get("error").is_none(), "{switched}");
    clock.advance(Duration::from_secs(1));
    api.reconnect();
    let late = api.call("marker.add", serde_json::json!({"label": "Late"}));
    assert_eq!(
        late["result"]["markers"][0]["t_ns"], 4_000_000_000u64,
        "{late}"
    );
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );
    let text = std::fs::read_to_string(dir.join("auto.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    let marks: Vec<_> = manifest
        .markers
        .iter()
        .map(|m| (m.kind, m.t_ns, m.pressed_ns, m.label.as_str()))
        .collect();
    assert_eq!(
        marks,
        [
            (
                rbs_manifest::MarkerKind::Scene,
                5_000_000_000,
                None,
                "Break"
            ),
            (
                rbs_manifest::MarkerKind::Manual,
                4_000_000_000,
                Some(6_000_000_000),
                "Late"
            ),
        ],
        "{text}"
    );

    // Scene markers off: a switch marks nothing, so the plain take writes no manifest.
    let mut quiet = request(dir.join("quiet.mkv"), None);
    quiet.markers.scene_changes = false;
    host.send(Command::StartRecording(quiet));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(1));
    let back = api.call("scene.switch", serde_json::json!({"scene": first}));
    assert!(back.get("error").is_none(), "{back}");
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(matches!(
        stopped,
        RecordingUpdate::Stopped { failure: None, .. }
    ));
    assert!(!dir.join("quiet.rbs.toml").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0089 decision 10: the window adds a marker with `Command::AddMarker` and
/// takes it back with `Command::RemoveMarkers`; each answers with the manual marker
/// counts, and an add names the marker for the notice. Automatic markers do not count.
#[test]
pub(super) fn the_window_adds_and_takes_back_a_marker() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-ui-markers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new();
    let (host, rx, _display, _mic) = marked(gpu, &clock, None);
    let markers = |rx: &Receiver<Update>| {
        next(rx, |u| match u {
            Update::Markers { counts, placed } => Some((counts, placed)),
            _ => None,
        })
    };

    host.send(Command::StartRecording(request(dir.join("ui.mkv"), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(3));
    host.send(Command::AddMarker);
    let (counts, placed) = markers(&rx);
    assert_eq!(counts, [Some(1), None]);
    let placed = placed.expect("an add names its marker");
    assert_eq!(placed.label, "Marker 1");
    assert_eq!(placed.t_ns, 3_000_000_000);
    assert_eq!(placed.placed.len(), 1);

    host.send(Command::RemoveMarkers(placed.placed));
    let (counts, placed) = markers(&rx);
    assert_eq!(counts, [Some(0), None]);
    assert!(placed.is_none());

    let stopped = stop_recording(&host, &rx, &clock);
    assert!(matches!(
        stopped,
        RecordingUpdate::Stopped { failure: None, .. }
    ));
    // The marker was taken back, so the plain take writes no manifest.
    assert!(!dir.join("ui.rbs.toml").exists());

    // Nothing records: the add is refused in words.
    host.send(Command::AddMarker);
    let why = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert!(why.contains("no recording"), "{why}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0089 decision 5: a marker not yet on disk is written into the manifest 5
/// seconds later, while the recording runs, so a crash keeps it; taking every marker
/// back removes a plain recording's manifest again.
#[test]
pub(super) fn markers_reach_the_manifest_while_recording() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-marker-flush-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new();
    let (host, rx, _display, _mic) = marked(gpu, &clock, None);
    let mut api = ApiClient::attach(&host);
    let manifest = dir.join("flush.rbs.toml");
    // Two requests: the second is served in a later round, so the first round's
    // timers have run by its answer.
    let settle = |api: &mut ApiClient| {
        for _ in 0..2 {
            let status = api.call("record.status", serde_json::json!({}));
            assert!(status.get("error").is_none(), "{status}");
        }
    };

    host.send(Command::StartRecording(request(
        dir.join("flush.mkv"),
        None,
    )));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(1));
    let added = api.call("marker.add", serde_json::json!({"label": "Keep"}));
    assert!(added.get("error").is_none(), "{added}");
    // The timer arms in the round that served it, on the clock as it is now.
    settle(&mut api);
    clock.advance(Duration::from_secs(4));
    settle(&mut api);
    assert!(!manifest.exists(), "written before its 5 seconds");
    clock.advance(Duration::from_secs(1));
    settle(&mut api);
    let text = std::fs::read_to_string(&manifest).unwrap();
    let flushed = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert!(!flushed.is_complete(), "{text}");
    let labels: Vec<_> = flushed.markers.iter().map(|m| m.label.as_str()).collect();
    assert_eq!(labels, ["Keep"], "{text}");

    // Taken back: the next flush removes the plain recording's manifest.
    api.reconnect();
    let removed = api.call(
        "marker.remove",
        serde_json::json!({"canvas": "main", "index": 1}),
    );
    assert!(removed.get("error").is_none(), "{removed}");
    // The timer arms in the round that served it, on the clock as it is now.
    settle(&mut api);
    clock.advance(Duration::from_secs(5));
    settle(&mut api);
    assert!(!manifest.exists());

    let stopped = stop_recording(&host, &rx, &clock);
    assert!(matches!(
        stopped,
        RecordingUpdate::Stopped { failure: None, .. }
    ));
    assert!(!manifest.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The 8-byte unsigned integer after the first `pattern` (an element id and its
/// 1-byte size 8) at or after `from`, and where it ends.
pub(super) fn uint_after(bytes: &[u8], from: usize, pattern: &[u8]) -> Option<(u64, usize)> {
    let at = from
        + bytes[from..]
            .windows(pattern.len())
            .position(|w| w == pattern)?;
    let start = at + pattern.len();
    let value = bytes[start..start + 8]
        .iter()
        .fold(0u64, |v, b| (v << 8) | u64::from(*b));
    Some((value, start + 8))
}

/// ADR-0089 decision 14: a Matroska master whose timing is known gets its markers as
/// chapters at a clean stop, counted from its first sample; one past the end is left
/// out.
#[test]
#[ignore = "slow: about 20 s, the clock moves 1 ms per step for the real muxer"]
pub(super) fn slow_a_matroska_master_gets_its_markers_as_chapters() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-chapters-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new();
    let (host, rx) = spawn_host_hw(gpu, &clock, HostConfig::desktop(), false, true);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    host.send(Command::ListTargets(SourceKind::Screen));
    let target = next(&rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target,
    });
    rows_where(&rx, |r| r.len() == 1);
    let mut api = ApiClient::attach(&host);

    let path = dir.join("chapters.mkv");
    let mut r = request(path.clone(), None);
    // The editor project setting keeps the master's timing.
    r.editor_project = Some(rbs_studio_host::record_profile::EditorProject::Xmeml);
    host.send(Command::StartRecording(r));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    // The real muxer refuses a stream more than its interleaving bound (1 s) behind
    // the other: the clock moves slowly enough for the software GPU's video to keep up
    // with the audio, which is made on demand.
    let packets = |n: u64| {
        next_while(
            &rx,
            || clock.advance(Duration::from_millis(1)),
            |u| match u {
                Update::Stats(s) if s.recording.is_some_and(|r| r.packets >= n) => Some(()),
                _ => None,
            },
        )
    };
    packets(4);
    let added = api.call("marker.add", serde_json::json!({"label": "Chapter A"}));
    assert!(added.get("error").is_none(), "{added}");
    // More frames after the marker, so it is inside the master.
    packets(20);
    let stopped = stop_recording(&host, &rx, &clock);
    assert!(
        matches!(stopped, RecordingUpdate::Stopped { failure: None, .. }),
        "{stopped:?}"
    );

    let text = std::fs::read_to_string(dir.join("chapters.rbs.toml")).unwrap();
    let manifest = rbs_manifest::RecordingManifest::from_toml(&text).unwrap();
    assert!(manifest.is_complete(), "{text}");
    let timing = manifest.files[0].timing.unwrap();
    let marker = &manifest.markers[0];
    let bytes = std::fs::read(&path).unwrap();
    // The earliest Cluster timestamp (ms): the master's first sample. Audio arriving
    // after the first video keyframe can start a later Cluster earlier.
    let first_ms = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, w)| *w == [0x1f, 0x43, 0xb6, 0x75])
        .filter_map(|(at, _)| uint_after(&bytes, at, &[0xe7, 0x88]).map(|(t, _)| t))
        .min()
        .unwrap();
    let chapters = bytes
        .windows(4)
        .rposition(|w| w == [0x10, 0x43, 0xa7, 0x70])
        .expect("a Chapters element");
    let (start, end) = uint_after(&bytes, chapters, &[0x91, 0x88]).unwrap();
    assert_eq!(
        start,
        first_ms * 1_000_000 + marker.t_ns - timing.start_offset_ns,
        "{text}"
    );
    assert!(
        bytes[end..].windows(9).any(|w| w == b"Chapter A"),
        "the label is the title"
    );
    assert!(
        uint_after(&bytes, end, &[0x91, 0x88]).is_none(),
        "one chapter"
    );

    drop(api);
    drop(host);
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0124: the window pauses and resumes the running recordings with
/// `Command::PauseRecording`; each answers with every recording's pause state. With
/// nothing recording the toggle is refused in a sentence for the toast.
#[test]
pub(super) fn the_window_pauses_and_resumes_the_recording() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-ui-pause-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let clock = TestClock::new();
    let (host, rx, _display, _mic) = marked(gpu, &clock, None);
    let paused = |rx: &Receiver<Update>| {
        next(rx, |u| match u {
            Update::RecordPause(states) => Some(states.map(|s| s.map(|s| s.paused))),
            _ => None,
        })
    };

    host.send(Command::StartRecording(request(
        dir.join("pause.mkv"),
        None,
    )));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));
    clock.advance(Duration::from_secs(2));
    host.send(Command::PauseRecording);
    assert_eq!(paused(&rx), [Some(true), None]);
    clock.advance(Duration::from_secs(1));
    host.send(Command::PauseRecording);
    assert_eq!(paused(&rx), [Some(false), None]);

    let stopped = stop_recording(&host, &rx, &clock);
    assert!(matches!(
        stopped,
        RecordingUpdate::Stopped { failure: None, .. }
    ));
    host.send(Command::PauseRecording);
    let why = next(&rx, |u| match u {
        Update::Error(e) => Some(e),
        _ => None,
    });
    assert!(
        why.starts_with("The recording could not pause or resume"),
        "{why}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0052 decision 4 through the real target: while a recording runs, an IPC
/// session narrowed to a token without "Allow actions that cannot be undone" may not
/// change what is on air (`not_permitted`, reason `on_air`), may still edit what is
/// off air, and is refused live calls; once the recording stops, the same edit passes.
#[test]
pub(super) fn a_token_without_the_permission_cannot_change_what_is_on_air() {
    let Some(gpu) = gpu() else { return };
    let dir = std::env::temp_dir().join(format!("rbs-app-on-air-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let clock = TestClock::new();
    let (host, rx) = host(gpu, &clock);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    host.send(Command::ListTargets(SourceKind::Screen));
    let display = next(&rx, |u| match u {
        Update::Targets(SourceKind::Screen, t) => Some(t[0].clone()),
        _ => None,
    });
    host.send(Command::AddSource {
        kind: SourceKind::Screen,
        target: display,
    });
    let source = rows_where(&rx, |r| r.len() == 1)[0].id.get();

    let token = rbs_api::token::Token::from_bytes(&[7; 32]);
    let mut tokens = rbs_api::TokenStore::new();
    tokens.insert(rbs_api::token::TokenEntry {
        kind: rbs_api::TokenKind::User,
        irreversible: false,
        name: "agent".into(),
        scopes: rbs_api::ScopeSet::PANEL,
        hash: token.hash(),
    });
    let mut api = ApiClient::attach_with(&host, tokens);
    let auth = api.call(
        "session.authenticate",
        serde_json::json!({"token": token.expose()}),
    );
    assert_eq!(auth["result"]["irreversible"], false, "{auth}");

    // Nothing runs: an edit of the program scene's source passes.
    let quiet = api.call(
        "source.set_volume",
        serde_json::json!({"source": source, "volume": 0.8}),
    );
    assert!(quiet.get("error").is_none(), "{quiet}");

    host.send(Command::StartRecording(request(dir.join("take.mkv"), None)));
    assert!(matches!(recording(&rx), RecordingUpdate::Started { .. }));

    let louder = api.call(
        "source.set_volume",
        serde_json::json!({"source": source, "volume": 0.5}),
    );
    assert_eq!(error_kind(&louder), "not_permitted", "{louder}");
    assert_eq!(louder["error"]["data"]["reason"], "on_air", "{louder}");
    let switch = api.call("canvas.transition", serde_json::json!({}));
    assert_eq!(error_kind(&switch), "not_permitted", "{switch}");

    // Off air: a new scene, renamed.
    let added = api.call("scene.add", serde_json::json!({"name": "Spare"}));
    let spare = added["result"]["scene"].as_u64().unwrap();
    let renamed = api.call(
        "scene.rename",
        serde_json::json!({"scene": spare, "name": "Next"}),
    );
    assert!(renamed.get("error").is_none(), "{renamed}");
    // Placing the on-air source there still touches it.
    let placed = api.call(
        "item.add",
        serde_json::json!({"scene": spare, "source": source}),
    );
    assert_eq!(error_kind(&placed), "not_permitted", "{placed}");

    let stop = api.call("record.stop", serde_json::json!({}));
    assert_eq!(error_kind(&stop), "not_permitted", "{stop}");
    assert_eq!(stop["error"]["data"]["reason"], "irreversible", "{stop}");

    stop_recording(&host, &rx, &clock);
    let after = api.call(
        "source.set_volume",
        serde_json::json!({"source": source, "volume": 0.5}),
    );
    assert!(after.get("error").is_none(), "{after}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR-0052 decision 4, reason `network`: without the permission an image's path must
/// be a local absolute path, through the real target.
#[test]
pub(super) fn a_token_without_the_permission_cannot_point_a_source_at_another_computer() {
    let Some(gpu) = gpu() else { return };
    let clock = TestClock::new();
    let (host, rx) = host(gpu, &clock);
    next(&rx, |u| match u {
        Update::Started { .. } => Some(()),
        Update::Failed(e) => panic!("{e}"),
        _ => None,
    });
    let token = rbs_api::token::Token::from_bytes(&[7; 32]);
    let mut tokens = rbs_api::TokenStore::new();
    tokens.insert(rbs_api::token::TokenEntry {
        kind: rbs_api::TokenKind::User,
        irreversible: false,
        name: "agent".into(),
        scopes: rbs_api::ScopeSet::PANEL,
        hash: token.hash(),
    });
    let mut api = ApiClient::attach_with(&host, tokens);
    api.call(
        "session.authenticate",
        serde_json::json!({"token": token.expose()}),
    );
    let added = api.call("source.add_sample", serde_json::json!({"sample": "aurora"}));
    let image = added["result"]["source"].as_u64().expect("an image source");

    let remote = api.call(
        "source.set_settings",
        serde_json::json!({"source": image, "settings": {"path": "\\\\nas\\share\\a.png"}}),
    );
    assert_eq!(error_kind(&remote), "not_permitted", "{remote}");
    assert_eq!(remote["error"]["data"]["reason"], "network", "{remote}");

    let local = api.call(
        "source.set_settings",
        serde_json::json!({"source": image, "settings": {"path": "/no/such/picture.png"}}),
    );
    assert_ne!(error_kind(&local), "not_permitted", "{local}");
}
