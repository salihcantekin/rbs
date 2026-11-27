//! The main canvas's buses through the control API (protocol 1.115, ADR-0170
//! decision 9): adding, renaming and removing a bus, sends, the loaded profile's
//! references that follow a rename, a bus kept while a track names it, and undo.

use rbs_studio_host::record_profile::RecordProfile;
use rbs_studio_host::studio_host::Command;

use super::*;

fn buses(c: &mut Client) -> Value {
    ok(&c.call("bus.list", json!({})))["buses"].clone()
}

fn reason(answer: &Value) -> &Value {
    &answer["error"]["data"]["reason"]
}

/// Sends the window's recording settings with these tracks and waits until the
/// control API shows them.
fn set_tracks(host: &StudioHost, c: &mut Client, tracks: &[&str]) {
    let mut profile = RecordProfile::new();
    profile.audio_tracks = tracks.iter().map(|t| (*t).to_owned()).collect();
    host.send(Command::SetProfile(profile));
    let want: Value = if tracks == ["main"] {
        Value::Null
    } else {
        json!(tracks)
    };
    c.event("settings.changed", |d| {
        d["profiles"][0]["audio_tracks"] == want
    });
}

fn sends_of(c: &mut Client, source: u64) -> Value {
    sources(c)
        .into_iter()
        .find(|s| s["id"] == source)
        .map(|s| s["sends"].clone())
        .unwrap()
}

#[test]
fn a_bus_is_added_renamed_with_its_references_and_kept_while_a_track_names_it() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call(
        "events.subscribe",
        json!({"topics": ["bus.changed", "source.changed", "settings.changed"]}),
    ));
    assert_eq!(buses(&mut c), json!([]));

    // Added with a trimmed name; taken, empty and reserved names are refused.
    ok(&c.call("bus.add", json!({"name": " Music "})));
    c.event("bus.changed", |d| d["buses"][0]["name"] == "Music");
    let taken = c.call("bus.add", json!({"name": "Music"}));
    assert_eq!(kind(&taken), "conflict");
    assert_eq!(reason(&taken), "name_taken");
    for name in ["main", " "] {
        let bad = c.call("bus.add", json!({"name": name}));
        assert_eq!(kind(&bad), "invalid_params", "{name:?}: {bad}");
    }
    for n in 2..=5 {
        ok(&c.call("bus.add", json!({"name": format!("Bus {n}")})));
    }
    let full = c.call("bus.add", json!({"name": "Bus 6"}));
    assert_eq!(reason(&full), "too_many_buses", "{full}");
    for n in 2..=5 {
        ok(&c.call("bus.remove", json!({"name": format!("Bus {n}")})));
    }

    // Sends: only levels other than the default are in the row.
    let mic = add(&mut c, "audio_input", "Mic");
    ok(&c.call(
        "source.set_sends",
        json!({"source": mic, "sends": [{"bus": "Music", "level": 0.5}, {"bus": "main", "level": 0.0}]}),
    ));
    let want = json!([{"bus": "main", "level": 0.0}, {"bus": "Music", "level": 0.5}]);
    c.event("source.changed", |d| {
        d.as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == mic && s["sends"] == want)
    });
    for (sends, k) in [
        (json!([]), "invalid_params"),
        (json!([{"bus": "Nope", "level": 0.5}]), "not_found"),
        (json!([{"bus": "Music", "level": 1.5}]), "invalid_params"),
        (
            json!([{"bus": "Music", "level": 0.1}, {"bus": " Music", "level": 0.2}]),
            "invalid_params",
        ),
    ] {
        let bad = c.call("source.set_sends", json!({"source": mic, "sends": sends}));
        assert_eq!(kind(&bad), k, "{bad}");
    }
    assert_eq!(sends_of(&mut c, mic), want);

    // A recording track names the bus: it is kept.
    set_tracks(&host, &mut c, &["main", "Music"]);
    let used = c.call("bus.remove", json!({"name": "Music"}));
    assert_eq!(reason(&used), "bus_in_use", "{used}");
    assert_eq!(used["error"]["data"]["uses"], json!(["recording track 2"]));

    // A rename takes the profile's references along, and undo takes them back.
    ok(&c.call("bus.set", json!({"name": "Music", "rename": "Songs"})));
    let saved = next(&updates, |u| match u {
        Update::Profile(p) => Some(p.audio_tracks),
        _ => None,
    });
    assert_eq!(saved, ["main", "Songs"]);
    assert_eq!(buses(&mut c)[0]["name"], "Songs");
    assert_eq!(sends_of(&mut c, mic)[1]["bus"], "Songs");
    ok(&c.call("edit.undo", json!({})));
    c.event("settings.changed", |d| {
        d["profiles"][0]["audio_tracks"] == json!(["main", "Music"])
    });
    assert_eq!(buses(&mut c)[0]["name"], "Music");

    // A name the profile already names is refused, so a missing name never finds a
    // different bus.
    set_tracks(&host, &mut c, &["main", "Music", "Ghost"]);
    let ghost = c.call("bus.set", json!({"name": "Music", "rename": "Ghost"}));
    assert_eq!(reason(&ghost), "name_in_profile", "{ghost}");

    // Volume and mute are live; a request with nothing to set, or out of range, is not.
    ok(&c.call(
        "bus.set",
        json!({"name": "Music", "volume": 0.25, "muted": true}),
    ));
    c.event("bus.changed", |d| {
        d["buses"][0]["volume"] == 0.25 && d["buses"][0]["muted"] == true
    });
    // The window's Mixer gets the track with its fader and mute (ADR-0170 decision 9).
    let track = next(&updates, |u| match u {
        Update::Tracks(t) if t.first().is_some_and(|t| t.muted) => t.into_iter().next(),
        _ => None,
    });
    assert_eq!((track.name.as_str(), track.volume), ("Music", 0.25));
    for bad in [
        json!({"name": "Music"}),
        json!({"name": "Music", "volume": 2.0}),
        json!({"name": "main", "muted": true}),
    ] {
        assert_eq!(kind(&c.call("bus.set", bad)), "invalid_params");
    }
    assert_eq!(
        kind(&c.call("bus.set", json!({"name": "Nope", "muted": true}))),
        "not_found"
    );

    // Once nothing names it, it goes with its sends; undo brings both back, and a
    // redo is refused while a track names it again.
    set_tracks(&host, &mut c, &["main"]);
    ok(&c.call("bus.remove", json!({"name": "Music"})));
    assert_eq!(buses(&mut c), json!([]));
    assert_eq!(
        sends_of(&mut c, mic),
        json!([{"bus": "main", "level": 0.0}])
    );
    ok(&c.call("edit.undo", json!({})));
    assert_eq!(buses(&mut c)[0]["name"], "Music");
    assert_eq!(sends_of(&mut c, mic), want);
    set_tracks(&host, &mut c, &["main", "Music"]);
    assert_eq!(kind(&c.call("edit.redo", json!({}))), "undo_conflict");
    assert_eq!(buses(&mut c)[0]["name"], "Music");
    drop(host);
}

#[test]
fn a_stream_bus_follows_a_rename_and_an_undo_may_not_remove_a_used_bus() {
    let Some((host, updates, _api, mut c)) = start() else {
        return;
    };
    ok(&c.call(
        "events.subscribe",
        json!({"topics": ["bus.changed", "settings.changed"]}),
    ));
    ok(&c.call("bus.add", json!({"name": "Music"})));
    let mut stream = rbs_studio_host::stream_profile::StreamProfile::new();
    stream.audio_bus = "Music".to_owned();
    host.send(Command::SetStreamProfile(stream));
    c.event("settings.changed", |d| d["stream"]["audio_bus"] == "Music");

    // Undo of the add would remove a bus the stream names.
    assert_eq!(kind(&c.call("edit.undo", json!({}))), "undo_conflict");
    assert_eq!(buses(&mut c)[0]["name"], "Music");
    let used = c.call("bus.remove", json!({"name": "Music"}));
    assert_eq!(used["error"]["data"]["uses"], json!(["stream"]), "{used}");

    // The stream's bus follows a rename, sent to the window as stream settings.
    ok(&c.call("bus.set", json!({"name": "Music", "rename": "VOD"})));
    let saved = next(&updates, |u| match u {
        Update::StreamProfile(p) => Some(p.audio_bus),
        Update::Profile(_) | Update::VerticalProfile(_) | Update::VerticalStreamProfile(_) => {
            panic!("only the stream settings changed")
        }
        _ => None,
    });
    assert_eq!(saved, "VOD");
    c.event("bus.changed", |d| d["buses"][0]["name"] == "VOD");
    drop(host);
}
