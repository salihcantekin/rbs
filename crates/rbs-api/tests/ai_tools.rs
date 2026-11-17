//! The AI tools token in the session state machine on a `TestClock` (ADR-0134
//! decisions 2 and 3): accepted only on same-user IPC while AI tools are started,
//! `ai_tools_off` only after its hash matched, Stop closing its sessions and finishing
//! a forwarded request, and token-only IPC while only AI tools are started.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_api::request::{Empty, Response};
use rbs_api::token::{Token, TokenEntry, TokenKind, TokenSlot, TokenStore};
use rbs_api::transport::{CloseReason, PeerInfo};
use rbs_api::{ScopeSet, session::AuditEvent};
use serde_json::json;
use support::*;

const AI_SEED: u8 = 7;

fn ai_entry() -> (Token, TokenEntry) {
    let (t, mut e) = token(AI_SEED, rbs_api::AI_TOOLS_NAME, ScopeSet::PANEL);
    e.kind = TokenKind::AiTools;
    e.irreversible = false;
    (t, e)
}

fn slot(started: bool, tokenless: bool) -> TokenSlot {
    let mut store = TokenStore::new();
    store.insert(token(PANEL_TOKEN_SEED, "Stream Deck", ScopeSet::PANEL).1);
    store.insert(ai_entry().1);
    TokenSlot {
        store,
        ai_tools_started: started,
        tokenless_ipc: tokenless,
    }
}

fn authenticate(
    h: &mut Harness,
    peer: PeerInfo,
    token: &Token,
) -> (rbs_api::transport::ConnectionId, serde_json::Value) {
    let conn = h.connect(peer);
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(
        conn,
        "session.authenticate",
        json!({"token": token.expose()}),
    );
    let answer = h.one(conn);
    (conn, answer)
}

#[test]
fn the_ai_tools_token_works_only_on_same_user_ipc_while_started() {
    let mut h = Harness::new();
    h.server.set_slot(slot(true, true));
    let (t, _) = ai_entry();
    let (conn, auth) = authenticate(&mut h, PeerInfo::same_user_ipc(), &t);
    assert_eq!(
        auth["result"]["scopes"],
        json!(["read", "control"]),
        "{auth}"
    );
    assert_eq!(auth["result"]["irreversible"], false);
    assert!(h.audit.contains(&AuditEvent::Authenticated {
        conn,
        token: Some(rbs_api::AI_TOOLS_NAME.into()),
        kind: Some(TokenKind::AiTools),
        client: Some("test".into()),
        scopes: ScopeSet::PANEL,
    }));
    // On WebSocket it fails like an unknown token.
    let (conn, auth) = authenticate(&mut h, PeerInfo::loopback_websocket(), &t);
    assert_eq!(error_kind(&auth), "auth_failed");
    assert!(auth["error"]["data"].get("reason").is_none());
    assert_eq!(h.closed(conn), Some(CloseReason::AuthFailed));
}

#[test]
fn stopped_ai_tools_answer_ai_tools_off_only_after_a_hash_match_on_ipc() {
    let mut h = Harness::new();
    h.server.set_slot(slot(false, true));
    let (t, _) = ai_entry();
    let (conn, auth) = authenticate(&mut h, PeerInfo::same_user_ipc(), &t);
    assert_eq!(error_kind(&auth), "not_permitted");
    assert_eq!(auth["error"]["data"]["reason"], "ai_tools_off");
    assert_eq!(h.closed(conn), Some(CloseReason::AiToolsStopped));
    // A wrong token on IPC: the uniform failure.
    let (_, auth) = authenticate(
        &mut h,
        PeerInfo::same_user_ipc(),
        &Token::from_bytes(&[9; 32]),
    );
    assert_eq!(error_kind(&auth), "auth_failed");
    // The same token on WebSocket: the uniform failure, nothing about AI tools.
    let (_, auth) = authenticate(&mut h, PeerInfo::loopback_websocket(), &t);
    assert_eq!(error_kind(&auth), "auth_failed");
}

#[test]
fn stop_closes_ai_tools_sessions_and_finishes_a_forwarded_request() {
    let mut h = Harness::new();
    h.server.set_slot(slot(true, true));
    let (t, _) = ai_entry();
    let (ai, _) = authenticate(&mut h, PeerInfo::same_user_ipc(), &t);
    let deck = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(ai, "scene.add", json!({}));
    h.call(ai, "scene.add", json!({}));
    assert_eq!(h.forwarded.len(), 1, "one in flight, one queued");
    let in_flight = h.forwarded.remove(0);

    // Stop.
    h.server.set_slot(slot(false, true));
    h.collect();
    assert_eq!(h.closed(ai), Some(CloseReason::AiToolsStopped));
    assert_eq!(h.closed(deck), None, "other sessions stay");
    // The forwarded request finishes; its answer is discarded, the queued one dropped.
    h.reply(&in_flight, Ok(Response::Done(Empty {})));
    assert!(h.take(ai).is_empty());
    assert!(h.forwarded.is_empty());

    // Start again: the token works.
    h.server.set_slot(slot(true, true));
    let (_, auth) = authenticate(&mut h, PeerInfo::same_user_ipc(), &t);
    assert!(auth["result"].is_object(), "{auth}");
}

#[test]
fn token_only_ipc_while_only_ai_tools_are_started() {
    let mut h = Harness::new();
    // Remote control on: a tokenless IPC session.
    h.server.set_slot(slot(true, true));
    let tokenless = h.connect(PeerInfo::same_user_ipc());
    h.call(tokenless, "session.hello", hello());
    assert!(h.one(tokenless)["result"]["scopes"].is_array());

    // Remote control off, AI tools started: tokenless sessions close.
    h.server.set_slot(slot(true, false));
    h.collect();
    assert_eq!(h.closed(tokenless), Some(CloseReason::ServerStopping));

    // Hello grants nothing; authenticate grants the token's scopes.
    let conn = h.connect(PeerInfo::same_user_ipc());
    h.call(conn, "session.hello", hello());
    let answer = h.one(conn);
    assert!(answer["result"].get("scopes").is_none(), "{answer}");
    h.call(conn, "settings.get", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "not_authenticated");
    let deck = Token::from_bytes(&[PANEL_TOKEN_SEED; 32]);
    h.call(
        conn,
        "session.authenticate",
        json!({"token": deck.expose()}),
    );
    assert_eq!(h.one(conn)["result"]["scopes"], json!(["read", "control"]));

    // Remote control back on: a new tokenless session works again; the AI tools
    // session stays.
    let (ai, _) = authenticate(&mut h, PeerInfo::same_user_ipc(), &ai_entry().0);
    h.server.set_slot(slot(true, true));
    h.collect();
    assert_eq!(h.closed(ai), None);
    let again = h.connect(PeerInfo::same_user_ipc());
    h.call(again, "session.hello", hello());
    assert!(h.one(again)["result"]["scopes"].is_array());
}

#[test]
fn a_store_change_keeps_ai_tools_sessions_when_the_flags_stay() {
    let mut h = Harness::new();
    h.server.set_slot(slot(true, false));
    let (ai, _) = authenticate(&mut h, PeerInfo::same_user_ipc(), &ai_entry().0);
    // tokens.create elsewhere: the composed slot keeps the entry and the flag.
    let mut next = slot(true, false);
    next.store.insert(token(3, "New panel", ScopeSet::PANEL).1);
    h.server.set_slot(next);
    h.collect();
    assert_eq!(h.closed(ai), None);
    // The access changed on the page: the session ends and reconnects.
    let mut changed = slot(true, false);
    let mut e = ai_entry().1;
    e.scopes = ScopeSet::EMPTY.with(rbs_api::Scope::Read);
    changed.store.revoke_hash(&e.hash);
    changed.store.insert(e);
    h.server.set_slot(changed);
    h.collect();
    assert_eq!(h.closed(ai), Some(CloseReason::Revoked));
}

#[test]
fn the_kind_is_saved_only_for_ai_tools_entries() {
    let (_, user) = token(1, "Deck", ScopeSet::PANEL);
    let text = serde_json::to_string(&user).unwrap();
    assert!(!text.contains("kind"), "{text}");
    let (_, ai) = ai_entry();
    let text = serde_json::to_string(&ai).unwrap();
    assert!(text.contains(r#""kind":"ai_tools""#), "{text}");
    let back: TokenEntry = serde_json::from_str(&text).unwrap();
    assert_eq!(back, ai);
}
