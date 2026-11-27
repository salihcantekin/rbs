//! Routed commands retain their original session's authority and queue position.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
use rbs_api::ui::{UiResolution, authorize_engine, authorize_method};
use rbs_api::{Caller, Command, Input, PeerInfo, ScopeSet};
use serde_json::json;
use support::*;

fn resolve(h: &mut Harness, call: &rbs_api::ui::UiForwarded, command: Command) {
    h.server.handle(
        Input::Ui(call.resolve(UiResolution::Engine(command))),
        h.now(),
    );
    h.collect();
}

#[test]
fn a_control_caller_cannot_run_an_admin_registry_entry() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let mut caller = Caller::local();
    caller.scopes = ScopeSet::EMPTY.with(rbs_api::Scope::Read);
    assert_eq!(
        authorize_method(&caller, "ui.command.run")
            .unwrap_err()
            .kind,
        rbs_api::ErrorKind::ScopeDenied
    );
    h.call(
        conn,
        "ui.command.run",
        json!({"id": "profile.select?id=profile-a", "client": "desktop-a"}),
    );
    assert_eq!(h.ui.len(), 1);
    let call = h.ui.pop().unwrap();
    assert_eq!(call.caller.scopes, ScopeSet::PANEL);
    resolve(
        &mut h,
        &call,
        Command::ProfileSwitch(rbs_api::request::EntryRef {
            id: "profile-a".into(),
        }),
    );
    assert_eq!(h.one(conn)["error"]["data"]["kind"], "scope_denied");
    assert!(h.forwarded.is_empty());
}

#[test]
fn a_resolution_preserves_the_caller_and_precedes_queued_requests() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::remote_websocket("192.0.2.1"), PANEL_TOKEN_SEED);
    let id = h.call(
        conn,
        "ui.command.run",
        json!({"id": "stream.toggle", "client": "desktop"}),
    );
    let call = h.ui.pop().unwrap();
    h.call(conn, "source.list", json!({}));
    resolve(
        &mut h,
        &call,
        Command::StreamStop(rbs_api::request::OnCanvases::default()),
    );
    assert_eq!(h.forwarded.len(), 1);
    let forwarded = &h.forwarded[0];
    assert_eq!(forwarded.id, rbs_api::RequestId::Number(id));
    assert_eq!(forwarded.caller, call.caller);
    assert!(forwarded.caller.remote);
    // Replaying a UI answer cannot replace the engine call or submit another one.
    resolve(&mut h, &call, Command::SourceList);
    assert_eq!(h.forwarded.len(), 1);
}

#[test]
fn disconnect_and_revocation_cancel_a_delayed_picker() {
    for revoke in [false, true] {
        let mut h = Harness::new();
        let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
        h.call(
            conn,
            "ui.command.run",
            json!({"id": "source.add?kind=camera"}),
        );
        let call = h.ui.pop().unwrap();
        if revoke {
            h.server.set_tokens(rbs_api::TokenStore::new());
        } else {
            h.transport_closed(conn);
        }
        assert!(!call.ticket.is_active());
        resolve(&mut h, &call, Command::SourceList);
        assert!(h.forwarded.is_empty());
    }
}

#[test]
fn a_listing_cannot_be_used_to_execute_and_is_not_in_the_engine_catalogue() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(conn, "ui.command.list", json!({"client": "desktop"}));
    let call = h.ui.pop().unwrap();
    resolve(&mut h, &call, Command::SourceList);
    assert_eq!(h.one(conn)["error"]["data"]["kind"], "invalid_request");
    assert!(h.forwarded.is_empty());
    assert!(
        !rbs_api::request::METHODS
            .iter()
            .any(|method| method.name.starts_with("ui."))
    );
}

#[test]
fn resolved_destructive_and_network_commands_keep_permission_refusals() {
    let mut caller = Caller::local();
    caller.irreversible = false;
    for (command, reason) in [
        (Command::TokensRevoke(rbs_api::request::TokenRef { name: "panel".into() }), "irreversible"),
        (serde_json::from_value::<rbs_api::Request>(json!({
            "method": "source.add", "params": {"kind": "browser", "url": "https://example.invalid"}
        })).map(|request| match request { rbs_api::Request::Command(command) => command, _ => unreachable!() }).unwrap(), "network"),
    ] {
        let error = authorize_engine(&caller, &command).unwrap_err();
        assert_eq!(error.kind, rbs_api::ErrorKind::NotPermitted);
        assert_eq!(error.data.unwrap()["reason"], reason);
    }
}

#[test]
fn a_read_only_session_is_refused_before_any_ui_dispatch() {
    let mut h = Harness::new();
    let mut store = rbs_api::TokenStore::new();
    store.insert(token(3, "observer", ScopeSet::EMPTY.with(rbs_api::Scope::Read)).1);
    h.server.set_tokens(store);
    let conn = h.login(PeerInfo::loopback_websocket(), 3);
    for (method, params) in [
        ("ui.command.list", json!({})),
        ("ui.command.run", json!({"id": "stream.toggle"})),
    ] {
        h.call(conn, method, params);
        assert_eq!(h.one(conn)["error"]["data"]["kind"], "scope_denied");
        assert!(h.ui.is_empty());
        assert!(h.forwarded.is_empty());
    }
}

#[test]
fn all_batch_permissions_are_checked_before_the_first_engine_operation() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(
        conn,
        "ui.command.run",
        json!({"id": "source.group.set_visible?group=1"}),
    );
    let call = h.ui.pop().unwrap();
    h.server.handle(
        Input::Ui(call.resolve(UiResolution::Batch(vec![
            Command::StreamStop(rbs_api::request::OnCanvases::default()),
            Command::ProfileSwitch(rbs_api::request::EntryRef {
                id: "profile-a".into(),
            }),
        ]))),
        h.now(),
    );
    h.collect();
    assert_eq!(h.one(conn)["error"]["data"]["kind"], "scope_denied");
    assert!(h.forwarded.is_empty());
}

#[test]
fn batch_refusals_stop_the_gesture_and_preserve_the_host_error() {
    for kind in [
        rbs_api::ErrorKind::NotPermitted,
        rbs_api::ErrorKind::Conflict,
    ] {
        let mut h = Harness::new();
        let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
        let id = h.call(
            conn,
            "ui.command.run",
            json!({"id": "source.group.set_visible?group=1"}),
        );
        let call = h.ui.pop().unwrap();
        let queued = h.call(conn, "studio.get", json!({}));
        h.server.handle(
            Input::Ui(call.resolve(UiResolution::Batch(vec![
                Command::SourceList,
                Command::SourceList,
            ]))),
            h.now(),
        );
        h.collect();
        let first = h.forwarded.pop().unwrap();
        let mut data = serde_json::Map::new();
        data.insert("reason".into(), "on_air".into());
        let error = rbs_api::ApiError::with_data(kind, "host refused the change", data);
        h.reply(&first, Err(error.clone()));
        let answer = h.one(conn);
        assert_eq!(answer["id"], id);
        assert_eq!(answer["error"]["message"], error.message);
        assert_eq!(answer["error"]["data"]["reason"], "on_air");
        assert_eq!(h.forwarded.len(), 1);
        assert_eq!(h.forwarded[0].id, rbs_api::RequestId::Number(queued));
        assert_eq!(h.forwarded[0].command, Command::StudioGet);
    }
}

#[test]
fn a_batch_answers_only_after_the_last_operation_and_keeps_the_original_caller() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::remote_websocket("192.0.2.2"), PANEL_TOKEN_SEED);
    let id = h.call(conn, "ui.command.run", json!({"id": "mixer.hide_all"}));
    let call = h.ui.pop().unwrap();
    h.server.handle(
        Input::Ui(call.resolve(UiResolution::Batch(vec![
            Command::SourceList,
            Command::StudioGet,
        ]))),
        h.now(),
    );
    h.collect();
    for command in [Command::SourceList, Command::StudioGet] {
        let forwarded = h.forwarded.pop().unwrap();
        assert_eq!(forwarded.command, command);
        assert_eq!(forwarded.caller, call.caller);
        assert!(h.take(conn).is_empty());
        h.reply(
            &forwarded,
            Ok(rbs_api::Response::Done(rbs_api::request::Empty {})),
        );
    }
    assert_eq!(h.one(conn)["id"], id);
    assert!(h.forwarded.is_empty());
}

#[test]
fn revocation_between_batch_operations_cancels_the_remaining_work() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(conn, "ui.command.run", json!({"id": "mixer.hide_all"}));
    let call = h.ui.pop().unwrap();
    h.server.handle(
        Input::Ui(call.resolve(UiResolution::Batch(vec![
            Command::SourceList,
            Command::StudioGet,
        ]))),
        h.now(),
    );
    h.collect();
    let first = h.forwarded.pop().unwrap();
    assert!(call.ticket.is_active());
    h.server.set_tokens(rbs_api::TokenStore::new());
    assert!(!call.ticket.is_active());
    h.reply(
        &first,
        Ok(rbs_api::Response::Done(rbs_api::request::Empty {})),
    );
    assert!(h.forwarded.is_empty());
}

#[test]
fn resolved_calls_share_marker_rate_limits_and_privileged_audits() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), ALL_TOKEN_SEED);
    h.audit.clear();
    h.call(
        conn,
        "ui.command.run",
        json!({"id": "profile.select?id=profile-a"}),
    );
    let call = h.ui.pop().unwrap();
    resolve(
        &mut h,
        &call,
        Command::ProfileSwitch(rbs_api::request::EntryRef {
            id: "profile-a".into(),
        }),
    );
    assert!(h.audit.iter().any(|audit| matches!(
        audit,
        rbs_api::AuditEvent::Privileged {
            method: "profile.switch",
            ..
        }
    )));
    assert!(h.audit.iter().any(|audit| matches!(
        audit,
        rbs_api::AuditEvent::Irreversible {
            method: "profile.switch",
            ..
        }
    )));
    let first = h.forwarded.pop().unwrap();
    h.reply(
        &first,
        Ok(rbs_api::Response::Done(rbs_api::request::Empty {})),
    );
    h.one(conn);
    let rbs_api::Request::Command(marker) = serde_json::from_value::<rbs_api::Request>(json!({
        "method": "marker.add", "params": {},
    }))
    .unwrap() else {
        panic!("marker must be an engine command")
    };
    for index in 0..3 {
        h.call(conn, "ui.command.run", json!({"id": "record.marker"}));
        let call = h.ui.pop().unwrap();
        resolve(&mut h, &call, marker.clone());
        if index < 2 {
            let first = h.forwarded.pop().unwrap();
            h.reply(
                &first,
                Ok(rbs_api::Response::Done(rbs_api::request::Empty {})),
            );
            h.one(conn);
        } else {
            assert_eq!(h.one(conn)["error"]["data"]["kind"], "rate_limited");
            assert!(h.forwarded.is_empty());
        }
    }
}
