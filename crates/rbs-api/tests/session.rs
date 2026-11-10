//! The session state machine on a `TestClock` (control API spec, "Tests"): handshake,
//! authentication, scopes, limits, ordering, back-pressure, events and `seq`.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_api::control::EventPublisher;
use rbs_api::event::{EventPayload, Topic, TopicSet};
use rbs_api::session::{AuditEvent, Input};
use rbs_api::token::{Token, TokenStore};
use rbs_api::transport::{CloseReason, PeerInfo};
use rbs_api::types::RecordState;
use rbs_api::{Command, ScopeSet};
use serde_json::json;
use support::*;

#[test]
fn handshake_then_authenticate_returns_scopes_and_instance() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(conn, "session.hello", hello());
    let hello = h.one(conn);
    assert_eq!(hello["result"]["protocol"], rbs_api::PROTOCOL_VERSION);
    assert_eq!(hello["result"]["instance"], h.server.config().instance);
    assert!(hello["result"].get("scopes").is_none());

    let t = Token::from_bytes(&[PANEL_TOKEN_SEED; 32]);
    h.call(conn, "session.authenticate", json!({"token": t.expose()}));
    let auth = h.one(conn);
    assert_eq!(auth["result"]["scopes"], json!(["read", "control"]));
    assert_eq!(auth["result"]["server"]["name"], "rbs-test");
    assert_eq!(h.server.token_of(conn).as_deref(), Some("Stream Deck"));
}

#[test]
fn every_method_before_authentication_is_refused() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(conn, "record.status", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "not_authenticated");
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(conn, "studio.get", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "not_authenticated");
    assert!(h.forwarded.is_empty());
}

#[test]
fn unknown_and_wrong_tokens_fail_the_same_way_and_close() {
    let mut h = Harness::new();
    let mut answers = Vec::new();
    for token in [
        Token::from_bytes(&[9; 32]).expose().to_owned(),
        "not a token".to_owned(),
    ] {
        let conn = h.connect(PeerInfo::loopback_websocket());
        h.call(conn, "session.hello", hello());
        h.take(conn);
        h.call(conn, "session.authenticate", json!({"token": token}));
        let mut answer = h.one(conn);
        answer["id"] = json!(null);
        answers.push(answer);
        assert_eq!(h.closed(conn), Some(CloseReason::AuthFailed));
    }
    assert_eq!(answers[0], answers[1]);
    assert_eq!(error_kind(&answers[0]), "auth_failed");
}

#[test]
fn same_user_ipc_peer_is_authenticated_by_hello_with_all_scopes() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::same_user_ipc());
    h.call(conn, "session.hello", hello());
    let hello = h.one(conn);
    assert_eq!(
        hello["result"]["scopes"],
        json!(["read", "control", "admin", "plugins", "tokens"])
    );
    h.call(conn, "settings.get", json!({}));
    assert_eq!(h.forwarded.len(), 1);
}

#[test]
fn ipc_peer_of_another_user_needs_a_token() {
    let mut h = Harness::new();
    let mut peer = PeerInfo::same_user_ipc();
    peer.same_user = false;
    let conn = h.connect(peer);
    h.call(conn, "session.hello", hello());
    assert!(h.one(conn)["result"].get("scopes").is_none());
    h.call(conn, "studio.get", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "not_authenticated");
}

#[test]
fn unsupported_protocol_closes() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(
        conn,
        "session.hello",
        json!({"protocols": ["2.0"], "client": {"name": "x", "version": "1"}}),
    );
    assert_eq!(error_kind(&h.one(conn)), "unsupported_protocol");
    assert_eq!(h.closed(conn), Some(CloseReason::UnsupportedProtocol));
}

#[test]
fn handshake_times_out() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::loopback_websocket());
    let deadline = h.server.next_deadline().unwrap();
    assert_eq!(deadline, h.now() + Duration::from_secs(10));
    h.advance(Duration::from_secs(10) - Duration::from_nanos(1));
    assert_eq!(h.closed(conn), None);
    h.advance(Duration::from_nanos(1));
    assert_eq!(h.closed(conn), Some(CloseReason::HandshakeTimeout));
    assert_eq!(h.server.next_deadline(), None);
}

#[test]
fn an_authenticated_idle_session_sets_no_deadline() {
    let mut h = Harness::new();
    assert_eq!(h.server.next_deadline(), None);
    h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    assert_eq!(h.server.next_deadline(), None);
}

#[test]
fn scopes_are_enforced_per_method() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(conn, "settings.get", json!({}));
    let denied = h.one(conn);
    assert_eq!(error_kind(&denied), "scope_denied");
    assert_eq!(denied["error"]["code"], -32010);
    h.call(
        conn,
        "record.start_to_file",
        json!({"profile": "default", "path": "/tmp/x.mkv"}),
    );
    assert_eq!(error_kind(&h.one(conn)), "scope_denied");
    // The monitor device is a saved setting of the computer (protocol 1.30).
    h.call(conn, "monitor.set_device", json!({"device": null}));
    assert_eq!(error_kind(&h.one(conn)), "scope_denied");
    h.call(
        conn,
        "events.subscribe",
        json!({"topics": ["settings.changed"]}),
    );
    assert_eq!(error_kind(&h.one(conn)), "scope_denied");
    h.call(conn, "record.start", json!({"profile": "default"}));
    assert_eq!(h.forwarded.len(), 1);
}

#[test]
fn remote_sessions_lose_admin_plugins_and_tokens() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::remote_websocket("192.168.1.20"));
    h.call(conn, "session.hello", hello());
    h.take(conn);
    let t = Token::from_bytes(&[ALL_TOKEN_SEED; 32]);
    h.call(conn, "session.authenticate", json!({"token": t.expose()}));
    assert_eq!(h.one(conn)["result"]["scopes"], json!(["read", "control"]));
    h.call(conn, "settings.get", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "scope_denied");

    let mut h = Harness::with_config(|c| c.allow_remote_admin = true);
    let conn = h.connect(PeerInfo::remote_websocket("192.168.1.20"));
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(conn, "session.authenticate", json!({"token": t.expose()}));
    assert_eq!(
        h.one(conn)["result"]["scopes"],
        json!(["read", "control", "admin"])
    );
}

#[test]
fn privileged_calls_are_audited_with_the_token_name() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), ALL_TOKEN_SEED);
    h.call(
        conn,
        "record.start_to_file",
        json!({"profile": "default", "path": "/videos/take1.mkv"}),
    );
    assert!(h.audit.contains(&AuditEvent::Privileged {
        conn,
        token: Some("admin console".into()),
        method: "record.start_to_file",
        detail: Some("/videos/take1.mkv".into()),
    }));
}

#[test]
fn pre_auth_messages_are_small() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::loopback_websocket());
    let big = "x".repeat(4 * 1024 + 1);
    h.call(
        conn,
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": big, "version": "1"}}),
    );
    assert_eq!(h.closed(conn), Some(CloseReason::MessageTooLarge));
}

#[test]
fn pending_handshakes_and_per_address_connections_are_capped() {
    let mut h = Harness::with_config(|c| {
        c.limits.max_pending_handshakes = 2;
        c.limits.max_connections_per_address = 3;
    });
    let a = h.connect(PeerInfo::remote_websocket("10.0.0.1"));
    let b = h.connect(PeerInfo::remote_websocket("10.0.0.2"));
    let c = h.connect(PeerInfo::remote_websocket("10.0.0.3"));
    assert_eq!(h.closed(a), None);
    assert_eq!(h.closed(b), None);
    assert_eq!(h.closed(c), Some(CloseReason::TooManyConnections));

    // Per address: authenticated remote sessions still count.
    let mut h = Harness::with_config(|c| c.limits.max_connections_per_address = 2);
    h.login(PeerInfo::remote_websocket("10.0.0.5"), PANEL_TOKEN_SEED);
    h.login(PeerInfo::remote_websocket("10.0.0.5"), PANEL_TOKEN_SEED);
    let third = h.connect(PeerInfo::remote_websocket("10.0.0.5"));
    assert_eq!(h.closed(third), Some(CloseReason::TooManyConnections));
    let other = h.connect(PeerInfo::remote_websocket("10.0.0.6"));
    assert_eq!(h.closed(other), None);
    // Loopback peers share one address and are not capped per address.
    for _ in 0..4 {
        let l = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
        assert_eq!(h.closed(l), None);
    }
}

#[test]
fn failed_authentication_from_an_address_backs_off() {
    let mut h = Harness::new();
    let bad = Token::from_bytes(&[9; 32]);
    let good = Token::from_bytes(&[PANEL_TOKEN_SEED; 32]);
    let remote = || PeerInfo::remote_websocket("203.0.113.7");

    let conn = h.connect(remote());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(conn, "session.authenticate", json!({"token": bad.expose()}));
    assert_eq!(error_kind(&h.one(conn)), "auth_failed");

    // The next attempt from that address waits 500 ms, even with the right token.
    let conn = h.connect(remote());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(
        conn,
        "session.authenticate",
        json!({"token": good.expose()}),
    );
    assert!(h.take(conn).is_empty());
    h.advance(Duration::from_millis(499));
    assert!(h.take(conn).is_empty());
    h.advance(Duration::from_millis(1));
    assert!(h.one(conn)["result"]["scopes"].is_array());

    // Loopback failures do not delay other loopback clients (no lock-out).
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(conn, "session.hello", hello());
    h.take(conn);
    h.call(conn, "session.authenticate", json!({"token": bad.expose()}));
    h.take(conn);
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    assert_eq!(h.closed(conn), None);
}

#[test]
fn back_off_doubles_per_failure_up_to_the_limit() {
    let mut h = Harness::with_config(|c| c.limits.handshake_timeout = Duration::from_secs(600));
    let bad = Token::from_bytes(&[9; 32]);
    let mut waits = Vec::new();
    for _ in 0..8 {
        let conn = h.connect(PeerInfo::remote_websocket("203.0.113.8"));
        h.call(conn, "session.hello", hello());
        h.take(conn);
        let start = h.now();
        h.call(conn, "session.authenticate", json!({"token": bad.expose()}));
        while h.take(conn).is_empty() {
            let next = h.server.next_deadline().unwrap();
            h.advance(next.saturating_since(h.now()));
        }
        waits.push(h.now().saturating_since(start).as_millis());
        h.transport_closed(conn);
    }
    assert_eq!(waits, vec![0, 500, 1000, 2000, 4000, 8000, 16000, 30000]);
}

#[test]
fn requests_beyond_the_rate_are_refused() {
    let mut h = Harness::with_config(|c| {
        c.limits.request_rate = 2;
        c.limits.request_burst = 3;
    });
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(conn, "session.hello", hello());
    h.call(
        conn,
        "session.authenticate",
        json!({"token": Token::from_bytes(&[PANEL_TOKEN_SEED; 32]).expose()}),
    );
    h.call(conn, "api.describe", json!({}));
    h.take(conn);
    h.call(conn, "api.describe", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "rate_limited");
    h.advance(Duration::from_millis(500));
    h.call(conn, "api.describe", json!({}));
    assert!(h.one(conn)["result"]["methods"].is_array());
}

#[test]
fn one_request_in_flight_answers_in_order_and_busy_beyond_the_queue() {
    let mut h = Harness::with_config(|c| c.limits.queue_len = 2);
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let first = h.call(conn, "record.status", json!({}));
    let second = h.call(conn, "api.describe", json!({}));
    let third = h.call(conn, "settings.get", json!({}));
    let fourth = h.call(conn, "source.list", json!({}));
    // Only the first is forwarded; the queue holds two; the fourth is refused at once.
    assert_eq!(h.forwarded.len(), 1);
    let busy = h.one(conn);
    assert_eq!(busy["id"], fourth);
    assert_eq!(error_kind(&busy), "busy");

    let mut target = FakeTarget::default();
    h.serve(&mut target);
    let answers = h.take(conn);
    let ids: Vec<_> = answers.iter().map(|a| a["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, vec![first, second, third]);
    assert_eq!(error_kind(&answers[2]), "scope_denied");
}

#[test]
fn a_closed_session_keeps_its_slot_until_its_reply_is_drained() {
    let mut h = Harness::new();
    let budget = h.server.budget();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    assert_eq!(budget.open(), 1);
    h.call(conn, "record.status", json!({}));
    let f = h.forwarded.pop().unwrap();
    h.transport_closed(conn);
    assert_eq!(h.server.session_count(), 1);
    assert_eq!(budget.open(), 1);
    h.reply(&f, Ok(rbs_api::Response::Done(rbs_api::request::Empty {})));
    assert_eq!(h.server.session_count(), 0);
    assert_eq!(budget.open(), 0);

    // A refused connection frees its slot once the transport drops it.
    let mut h = Harness::with_config(|c| c.limits.max_pending_handshakes = 0);
    let budget = h.server.budget();
    let refused = h.connect(PeerInfo::remote_websocket("10.1.1.1"));
    assert_eq!(h.closed(refused), Some(CloseReason::TooManyConnections));
    assert_eq!(budget.open(), 1);
    h.transport_closed(refused);
    assert_eq!(budget.open(), 0);
    assert!(h.take(conn).is_empty());
}

#[test]
fn subscribe_sets_the_mask_before_forwarding() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(
        conn,
        "events.subscribe",
        json!({"topics": ["record.state", "audio.levels"]}),
    );
    // The mask holds state topics only; sampled topics never reach the control thread.
    assert_eq!(h.masks, vec![TopicSet::of([Topic::RecordState])]);
    assert!(matches!(
        h.forwarded[0].command,
        Command::EventsSubscribe(_)
    ));
    h.call(
        conn,
        "events.unsubscribe",
        json!({"topics": ["record.state"]}),
    );
    let mut target = FakeTarget::default();
    h.serve(&mut target);
    assert_eq!(h.masks.last(), Some(&TopicSet::EMPTY));
    let answers = h.take(conn);
    assert_eq!(answers[0]["result"]["seq"]["record.state"], 0);
    assert_eq!(answers[1]["result"], json!({}));
}

#[test]
fn events_reach_subscribers_with_seq_and_coalesce_while_blocked() {
    let mut h = Harness::new();
    let sub = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let other = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(sub, "events.subscribe", json!({"topics": ["record.state"]}));
    let mut target = FakeTarget::default();
    h.serve(&mut target);
    h.take(sub);

    let event = |seq, recording| rbs_api::Event {
        object: None,
        seq,
        payload: EventPayload::RecordState(if recording {
            RecordState::Recording {
                profile: None,
                video_encoder: "Test".into(),
                audio_encoder: None,
            }
        } else {
            RecordState::Idle { last: None }
        }),
    };
    let now = h.now();
    h.server.handle(Input::Events(vec![event(1, true)]), now);
    let got = h.one(sub);
    assert_eq!(got["method"], "event");
    assert_eq!(got["params"]["topic"], "record.state");
    assert_eq!(got["params"]["seq"], 1);
    assert_eq!(got["params"]["data"]["state"], "recording");
    assert!(h.take(other).is_empty());

    // While the connection takes nothing, only the latest event per key waits.
    h.set_full(sub, true);
    for seq in 2..10 {
        h.server
            .handle(Input::Events(vec![event(seq, seq % 2 == 0)]), now);
    }
    h.set_full(sub, false);
    let got = h.one(sub);
    assert_eq!(got["params"]["seq"], 9);
}

#[test]
fn replies_are_kept_in_order_while_the_connection_is_full() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.set_full(conn, true);
    let a = h.call(conn, "api.describe", json!({}));
    let b = h.call(conn, "settings.get", json!({}));
    h.set_full(conn, false);
    let ids: Vec<_> = h.take(conn).iter().map(|m| m["id"].clone()).collect();
    assert_eq!(ids, vec![json!(a), json!(b)]);
}

#[test]
fn a_peer_that_never_reads_is_closed() {
    let mut h = Harness::with_config(|c| {
        c.limits.max_pending_answers = 4;
        c.limits.request_burst = 1000;
    });
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.set_full(conn, true);
    for _ in 0..5 {
        h.call(conn, "api.describe", json!({}));
    }
    assert_eq!(h.closed(conn), Some(CloseReason::TooSlow));
}

#[test]
fn revoking_a_token_closes_its_sessions() {
    let mut h = Harness::new();
    let panel = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let admin = h.login(PeerInfo::loopback_websocket(), ALL_TOKEN_SEED);
    let mut tokens = TokenStore::new();
    tokens.insert(token(ALL_TOKEN_SEED, "admin console", ScopeSet::ALL).1);
    h.server.set_tokens(tokens);
    assert_eq!(h.closed(panel), Some(CloseReason::Revoked));
    assert_eq!(h.closed(admin), None);
}

#[test]
fn the_publisher_counts_seq_and_builds_payloads_only_for_subscribers() {
    let mut publisher = EventPublisher::new();
    let mut built = 0;
    let key = FakeTarget::key();
    for _ in 0..3 {
        publisher.changed(key, || {
            built += 1;
            EventPayload::RecordState(RecordState::Idle { last: None })
        });
    }
    assert_eq!(built, 0);
    assert_eq!(publisher.seq(key), 3);

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    publisher.set_local(TopicSet::of([Topic::RecordState]), move |e| {
        sink.lock().unwrap().push(e.seq);
    });
    publisher.changed(key, || {
        EventPayload::RecordState(RecordState::Idle { last: None })
    });
    assert_eq!(*seen.lock().unwrap(), vec![4]);
}

#[test]
fn sampled_topics_follow_the_client_rate_capped() {
    struct Levels;
    impl rbs_api::Sampler for Levels {
        fn sample(&mut self, _topic: Topic) -> Option<EventPayload> {
            Some(EventPayload::AudioLevels(vec![]))
        }
    }
    let mut h = Harness::new();
    let config = h.server.config().clone();
    let mut tokens = TokenStore::new();
    tokens.insert(token(PANEL_TOKEN_SEED, "Stream Deck", ScopeSet::PANEL).1);
    h.server = rbs_api::Server::new(config, tokens).with_sampler(Box::new(Levels));
    h.inbox = rbs_api::TransportInbox::new(
        std::sync::Arc::new(rbs_api::wake::Wake::new()),
        h.server.budget(),
        8,
        4096,
    );
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(
        conn,
        "events.subscribe",
        json!({"topics": ["audio.levels"], "rates": {"audio.levels": 1000}}),
    );
    let mut target = FakeTarget::default();
    h.serve(&mut target);
    h.take(conn);
    // 1000 Hz is capped at 30 Hz: one sample per 33.3 ms.
    let interval = Duration::from_secs(1) / 30;
    assert_eq!(h.server.next_deadline(), Some(h.now() + interval));
    let mut samples = 0;
    for _ in 0..30 {
        h.advance(interval);
        samples += h.take(conn).len();
    }
    assert_eq!(samples, 30);

    h.call(
        conn,
        "events.subscribe",
        json!({"topics": ["record.state"], "rates": {"record.state": 5}}),
    );
    assert_eq!(error_kind(&h.one(conn)), "invalid_params");
}

#[test]
fn remote_handshakes_cannot_lock_out_local_clients() {
    let mut h = Harness::with_config(|c| c.limits.max_pending_handshakes = 2);
    let r1 = h.connect(PeerInfo::remote_websocket("10.0.0.1"));
    let r2 = h.connect(PeerInfo::remote_websocket("10.0.0.2"));
    let r3 = h.connect(PeerInfo::remote_websocket("10.0.0.3"));
    assert_eq!(h.closed(r1), None);
    assert_eq!(h.closed(r2), None);
    assert_eq!(h.closed(r3), Some(CloseReason::TooManyConnections));
    for _ in 0..4 {
        let local = h.connect(PeerInfo::loopback_websocket());
        assert_eq!(h.closed(local), None);
        let ipc = h.connect(PeerInfo::same_user_ipc());
        assert_eq!(h.closed(ipc), None);
    }
}

#[test]
fn ipv6_peers_are_grouped_by_their_64_prefix() {
    let mut h = Harness::with_config(|c| c.limits.max_connections_per_address = 2);
    h.connect(PeerInfo::remote_websocket("2001:db8:1:2::10"));
    h.connect(PeerInfo::remote_websocket("2001:db8:1:2::11"));
    let third = h.connect(PeerInfo::remote_websocket("2001:db8:1:2:ffff::1"));
    assert_eq!(h.closed(third), Some(CloseReason::TooManyConnections));
    let other_net = h.connect(PeerInfo::remote_websocket("2001:db8:1:3::1"));
    assert_eq!(h.closed(other_net), None);
}

#[test]
fn parallel_attempts_from_one_address_share_the_back_off() {
    let mut h = Harness::new();
    let bad = Token::from_bytes(&[9; 32]);
    let remote = || PeerInfo::remote_websocket("203.0.113.9");
    let first = h.connect(remote());
    h.call(first, "session.hello", hello());
    h.take(first);
    h.call(
        first,
        "session.authenticate",
        json!({"token": bad.expose()}),
    );
    h.take(first);
    // Two attempts wait for the same 500 ms delay.
    let (a, b) = (h.connect(remote()), h.connect(remote()));
    for c in [a, b] {
        h.call(c, "session.hello", hello());
        h.take(c);
        h.call(c, "session.authenticate", json!({"token": bad.expose()}));
    }
    h.advance(Duration::from_millis(500));
    // The first released attempt fails; the second now waits for the new 1 s delay.
    let answered: Vec<_> = [a, b]
        .into_iter()
        .filter(|c| !h.take(*c).is_empty())
        .collect();
    assert_eq!(answered.len(), 1);
    let waiting = if answered[0] == a { b } else { a };
    h.advance(Duration::from_millis(999));
    assert!(h.take(waiting).is_empty());
    h.advance(Duration::from_millis(1));
    assert_eq!(error_kind(&h.one(waiting)), "auth_failed");
}

#[test]
fn before_authentication_only_the_handshake_is_revealed() {
    let mut h = Harness::new();
    let conn = h.connect(PeerInfo::loopback_websocket());
    h.call(conn, "studio.explode", json!({}));
    assert_eq!(error_kind(&h.one(conn)), "not_authenticated");
    h.call(conn, "source.delete", json!({"source": "x"}));
    assert_eq!(error_kind(&h.one(conn)), "not_authenticated");
    // The handshake's own schema is public.
    h.call(conn, "session.hello", json!({"protocols": 1}));
    assert_eq!(error_kind(&h.one(conn)), "invalid_params");
}

#[test]
fn decoding_errors_with_an_id_wait_behind_the_request_in_flight() {
    let mut h = Harness::new();
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let first = h.call(conn, "record.status", json!({}));
    let second = h.call(conn, "source.delete", json!({"source": "x"}));
    assert!(h.take(conn).is_empty());
    let mut target = FakeTarget::default();
    h.serve(&mut target);
    let answers = h.take(conn);
    assert_eq!(answers[0]["id"], first);
    assert_eq!(answers[1]["id"], second);
    assert_eq!(error_kind(&answers[1]), "invalid_params");
}

#[test]
fn changing_a_tokens_scopes_closes_its_sessions() {
    let mut h = Harness::new();
    let panel = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    let mut tokens = TokenStore::new();
    tokens.insert(token(ALL_TOKEN_SEED, "admin console", ScopeSet::ALL).1);
    tokens.insert(
        token(
            PANEL_TOKEN_SEED,
            "Stream Deck",
            ScopeSet::of([rbs_api::Scope::Read]),
        )
        .1,
    );
    h.server.set_tokens(tokens);
    assert_eq!(h.closed(panel), Some(CloseReason::Revoked));
}

#[test]
fn oversized_messages_close_before_and_are_refused_after_authentication() {
    use rbs_api::transport::TransportEvent;
    let mut h = Harness::new();
    let early = h.connect(PeerInfo::loopback_websocket());
    let now = h.now();
    h.server.handle(
        Input::Transport(TransportEvent::Oversized {
            id: early,
            len: 5000,
        }),
        now,
    );
    h.collect();
    assert_eq!(h.closed(early), Some(CloseReason::MessageTooLarge));

    let late = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.server.handle(
        Input::Transport(TransportEvent::Oversized {
            id: late,
            len: 5_000_000,
        }),
        now,
    );
    h.collect();
    assert_eq!(error_kind(&h.one(late)), "message_too_large");
    assert_eq!(h.closed(late), None);
}

#[test]
fn a_slow_peer_closed_with_a_request_in_flight_keeps_its_slot_until_the_reply() {
    let mut h = Harness::with_config(|c| c.limits.max_pending_answers = 1);
    let conn = h.login(PeerInfo::loopback_websocket(), PANEL_TOKEN_SEED);
    h.call(conn, "record.status", json!({}));
    let f = h.forwarded.pop().unwrap();
    h.set_full(conn, true);
    h.call(conn, "api.describe", json!({}));
    h.call(conn, "api.describe", json!({}));
    for _ in 0..16 {
        h.call(conn, "nope", json!({}));
    }
    assert_eq!(h.closed(conn), Some(CloseReason::TooSlow));
    h.transport_closed(conn);
    assert_eq!(h.server.session_count(), 1);
    h.reply(&f, Ok(rbs_api::Response::Done(rbs_api::request::Empty {})));
    assert_eq!(h.server.session_count(), 0);
}
