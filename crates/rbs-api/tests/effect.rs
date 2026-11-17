//! Method effects and the irreversible-actions permission (ADR-0052 decisions 4
//! and 5, protocol 1.76), on the session state machine.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_api::ScopeSet;
use rbs_api::request::{Effect, METHODS, method_effect};
use rbs_api::session::AuditEvent;
use rbs_api::token::{Token, TokenStore};
use rbs_api::transport::{CloseReason, PeerInfo};
use serde_json::json;
use support::*;

const AGENT_SEED: u8 = 7;
const AGENT_VIEW_SEED: u8 = 8;

/// A harness whose store also holds a Control token and a View token without the
/// permission.
fn harness() -> Harness {
    let mut h = Harness::new();
    let mut tokens = TokenStore::new();
    tokens.insert(token(ALL_TOKEN_SEED, "admin console", ScopeSet::ALL).1);
    tokens.insert(token(PANEL_TOKEN_SEED, "Stream Deck", ScopeSet::PANEL).1);
    let mut agent = token(AGENT_SEED, "agent", ScopeSet::PANEL).1;
    agent.irreversible = false;
    tokens.insert(agent);
    let mut view = token(
        AGENT_VIEW_SEED,
        "viewer",
        ScopeSet::of([rbs_api::Scope::Read]),
    )
    .1;
    view.irreversible = false;
    tokens.insert(view);
    h.server.set_tokens(tokens);
    h
}

#[test]
fn every_method_has_an_effect_and_read_scope_methods_only_read() {
    for m in METHODS {
        assert_eq!(method_effect(m.name), Some(m.effect));
        if m.scope == Some(rbs_api::Scope::Read) {
            assert_eq!(m.effect, Effect::Read, "{} has scope read", m.name);
        }
    }
    for live in [
        "record.start",
        "record.start_to_file",
        "record.stop",
        "stream.start",
        "stream.stop",
        "vcam.start",
        "vcam.stop",
        "profile.switch",
    ] {
        assert_eq!(method_effect(live), Some(Effect::Live), "{live}");
    }
    for destructive in ["tokens.create", "tokens.revoke"] {
        assert_eq!(
            method_effect(destructive),
            Some(Effect::Destructive),
            "{destructive}"
        );
    }
    for edit in ["edit.undo", "edit.redo", "scene.switch", "settings.set"] {
        assert_eq!(method_effect(edit), Some(Effect::Edit), "{edit}");
    }
    assert_eq!(method_effect("no.such"), None);
}

#[test]
fn a_token_without_the_permission_is_refused_live_calls_and_audited() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), AGENT_SEED);
    h.call(conn, "record.start", json!({"profile": "default"}));
    let answer = h.one(conn);
    assert_eq!(error_kind(&answer), "not_permitted");
    assert_eq!(answer["error"]["code"], -32012);
    assert_eq!(answer["error"]["data"]["reason"], "irreversible");
    assert!(h.forwarded.is_empty());
    h.collect();
    assert!(h.audit.contains(&AuditEvent::NotPermitted {
        conn,
        token: Some("agent".into()),
        method: "record.start",
        reason: "irreversible",
    }));

    // Edits and reads still pass.
    h.call(conn, "scene.switch", json!({"scene": 1}));
    assert_eq!(h.forwarded.len(), 1);
}

#[test]
fn a_token_with_the_permission_passes_and_is_audited() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(conn, "record.start", json!({"profile": "default"}));
    assert_eq!(h.forwarded.len(), 1);
    h.collect();
    assert!(h.audit.contains(&AuditEvent::Irreversible {
        conn,
        token: Some("Stream Deck".into()),
        method: "record.start",
    }));
}

#[test]
fn the_scope_is_checked_before_the_permission() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), AGENT_VIEW_SEED);
    h.call(conn, "record.start", json!({"profile": "default"}));
    assert_eq!(error_kind(&h.one(conn)), "scope_denied");
}

#[test]
fn authenticate_reports_the_permission() {
    let mut h = harness();
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    let t = Token::from_bytes(&[AGENT_SEED; 32]);
    h.call(conn, "session.authenticate", json!({"token": t.expose()}));
    assert_eq!(h.one(conn)["result"]["irreversible"], false);
}

#[test]
fn an_ipc_session_narrowed_to_a_token_without_the_permission_is_bound_by_it() {
    let mut h = harness();
    let conn = h.connect(PeerInfo::same_user_ipc());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    let t = Token::from_bytes(&[AGENT_SEED; 32]);
    h.call(conn, "session.authenticate", json!({"token": t.expose()}));
    assert_eq!(h.one(conn)["result"]["irreversible"], false);
    h.call(conn, "stream.start", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "not_permitted");
    assert!(h.forwarded.is_empty());
}

#[test]
fn a_same_user_ipc_session_without_a_token_keeps_the_permission() {
    let mut h = harness();
    let conn = h.connect(PeerInfo::same_user_ipc());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(conn, "tokens.revoke", json!({"name": "agent"}));
    assert_eq!(h.forwarded.len(), 1);
}

#[test]
fn changing_a_tokens_permission_closes_its_sessions() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let mut tokens = TokenStore::new();
    let mut panel = token(PANEL_TOKEN_SEED, "Stream Deck", ScopeSet::PANEL).1;
    panel.irreversible = false;
    tokens.insert(panel);
    h.server.set_tokens(tokens);
    assert_eq!(h.closed(conn), Some(CloseReason::Revoked));
}

#[test]
fn describe_lists_each_methods_effect() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), AGENT_SEED);
    h.call(conn, "api.describe", json!({}));
    let answer = h.one(conn);
    let methods = answer["result"]["methods"].as_array().unwrap();
    let start = methods
        .iter()
        .find(|m| m["name"] == "stream.start")
        .unwrap();
    assert_eq!(start["effect"], "live");
    assert_eq!(methods.len(), METHODS.len());
    assert!(methods.iter().all(|m| m["effect"].is_string()));
}

#[test]
fn stored_entries_without_the_field_keep_the_permission() {
    let entry: rbs_api::token::TokenEntry = serde_json::from_value(json!({
        "name": "old",
        "scopes": ["read"],
        "hash": "73aa00dce5b7afd18306d160c3de0e6e56db9ade3300340da67c2021c402f04b",
    }))
    .unwrap();
    assert!(entry.irreversible);
}

#[test]
fn reviewed_methods_that_lose_data_are_destructive_or_live() {
    for name in [
        "source.set_sync_offset",
        "source.paste_attributes",
        "source.reset",
        "loudness.set_enabled",
        "canvas.remove_vertical",
    ] {
        assert_eq!(method_effect(name), Some(Effect::Destructive), "{name}");
    }
    assert_eq!(method_effect("collection.switch"), Some(Effect::Live));
}

#[test]
fn a_canvas_change_through_settings_set_is_live() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), AGENT_SEED);
    // A Control token lacks `admin`; the permission check needs a full token.
    h.call(conn, "settings.set", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "scope_denied");
    let mut tokens = TokenStore::new();
    let mut full = token(ALL_TOKEN_SEED, "admin console", ScopeSet::ALL).1;
    full.irreversible = false;
    tokens.insert(full);
    h.server.set_tokens(tokens);
    let conn = h.login(PeerInfo::loopback_websocket(), ALL_TOKEN_SEED);
    h.call(
        conn,
        "settings.set",
        json!({"canvas": {"width": 1280, "height": 720, "fps_num": 30, "fps_den": 1}}),
    );
    assert_eq!(error_kind(&h.one(conn)), "not_permitted");
    let before = h.forwarded.len();
    h.call(conn, "settings.set", json!({}));
    assert_eq!(h.forwarded.len(), before + 1);
}

#[test]
fn the_caller_goes_with_the_command_and_on_air_refusals_are_audited() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), AGENT_SEED);
    h.call(conn, "scene.switch", json!({"scene": 1}));
    let f = h.forwarded.pop().unwrap();
    assert_eq!(
        f.caller,
        rbs_api::Caller {
            token: Some("agent".into()),
            irreversible: false,
        }
    );
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), "on_air".into());
    let e = rbs_api::ApiError::with_data(rbs_api::ErrorKind::NotPermitted, "on air", data);
    h.reply(&f, Err(e));
    assert_eq!(h.one(conn)["error"]["data"]["reason"], "on_air");
    assert!(h.audit.contains(&AuditEvent::NotPermitted {
        conn,
        token: Some("agent".into()),
        method: "scene.switch",
        reason: "on_air",
    }));
}

#[test]
fn a_same_user_ipc_session_is_a_permitted_caller() {
    let mut h = harness();
    let conn = h.connect(PeerInfo::same_user_ipc());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(conn, "scene.switch", json!({"scene": 1}));
    assert_eq!(h.forwarded[0].caller, rbs_api::Caller::local());
}

#[test]
fn a_browser_source_reaches_the_network_and_is_refused_without_the_permission() {
    let mut h = harness();
    let conn = h.login(PeerInfo::loopback_websocket(), AGENT_SEED);
    h.call(
        conn,
        "source.add",
        json!({"kind": "browser", "adapter": "cef", "target": "x"}),
    );
    let answer = h.one(conn);
    assert_eq!(error_kind(&answer), "not_permitted");
    assert_eq!(answer["error"]["data"]["reason"], "network");
    assert!(h.forwarded.is_empty());
    h.collect();
    assert!(h.audit.contains(&AuditEvent::NotPermitted {
        conn,
        token: Some("agent".into()),
        method: "source.add",
        reason: "network",
    }));

    // Other kinds pass to the control thread.
    h.call(
        conn,
        "source.add",
        json!({"kind": "camera", "adapter": "test", "target": "0"}),
    );
    assert_eq!(h.forwarded.len(), 1);
}

#[test]
fn a_recording_folder_on_another_computer_is_refused_without_the_permission() {
    let mut h = harness();
    let mut tokens = TokenStore::new();
    let mut full = token(ALL_TOKEN_SEED, "admin console", ScopeSet::ALL).1;
    full.irreversible = false;
    tokens.insert(full);
    h.server.set_tokens(tokens);
    let conn = h.login(PeerInfo::loopback_websocket(), ALL_TOKEN_SEED);
    let profile = |folder: &str| {
        let mut p = serde_json::to_value(json_profile()).unwrap();
        p["folder"] = folder.into();
        json!({ "profiles": [p] })
    };
    h.call(conn, "settings.set", profile("\\\\nas\\share\\rec"));
    let answer = h.one(conn);
    assert_eq!(error_kind(&answer), "not_permitted", "{answer}");
    assert_eq!(answer["error"]["data"]["reason"], "network");
    h.call(conn, "settings.set", profile("/home/me/Videos"));
    assert_eq!(h.forwarded.len(), 1);
}

fn json_profile() -> rbs_api::types::RecordingProfile {
    use rbs_api::types::*;
    RecordingProfile {
        id: "default".into(),
        name: "Default".into(),
        folder: String::new(),
        container: Container::Mkv,
        encoder: None,
        video_codec: VideoCodec::H264,
        rate_control: RateMode::Vbr,
        video_kbps: 12_000,
        max_video_kbps: None,
        quality: 23,
        keyframe_seconds: 2,
        profile: None,
        preset: Preset::Balanced,
        b_frames: 2,
        cbr_padding: false,
        audio_kbps: 160,
        bit_depth: 16,
        delivery: Delivery::Video,
        open_shadows: false,
        output_size: None,
        editor_project: None,
        markers: Default::default(),
    }
}

#[test]
fn tokens_create_never_gives_more_scopes_than_the_caller_holds() {
    let mut h = harness();
    let mut tokens = TokenStore::new();
    tokens.insert(
        token(
            ALL_TOKEN_SEED,
            "token keeper",
            ScopeSet::of([rbs_api::Scope::Read, rbs_api::Scope::Tokens]),
        )
        .1,
    );
    h.server.set_tokens(tokens);
    let conn = h.login(PeerInfo::loopback_websocket(), ALL_TOKEN_SEED);
    h.call(
        conn,
        "tokens.create",
        json!({"name": "panel", "scopes": ["read", "control"]}),
    );
    let answer = h.one(conn);
    assert_eq!(error_kind(&answer), "not_permitted", "{answer}");
    assert_eq!(answer["error"]["data"]["reason"], "scopes");
    assert!(h.forwarded.is_empty());
    h.call(
        conn,
        "tokens.create",
        json!({"name": "viewer", "scopes": ["read"], "irreversible": false}),
    );
    assert_eq!(h.forwarded.len(), 1);
}
