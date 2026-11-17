#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::collections::VecDeque;
use std::sync::Mutex;

use rbs_api::{Scope, Token};
use rbs_core::time::TestClock;

use super::*;
use crate::link::{Api, handshake};

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// Answers like a server: the handshake, `api.describe` with the built-in methods,
/// and `{}` (or a set error) for any other method.
struct FakeApi {
    calls: Calls,
    scopes: Vec<Scope>,
    irreversible: bool,
    auth_error: Option<(&'static str, Value)>,
    lose_after: Option<usize>,
    protocol: &'static str,
}

impl Api for FakeApi {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let mut calls = self.calls.lock().unwrap();
        calls.push((method.to_owned(), params));
        if self.lose_after.is_some_and(|n| calls.len() > n) {
            return Err(CallError::Lost("gone".into()));
        }
        match method {
            "session.hello" => Ok(json!({ "protocol": self.protocol, "scopes": null })),
            "session.authenticate" => match &self.auth_error {
                Some((kind, data)) => Err(CallError::Api {
                    kind: (*kind).into(),
                    message: "no".into(),
                    data: data.clone(),
                }),
                None => Ok(json!({ "scopes": self.scopes, "irreversible": self.irreversible })),
            },
            "api.describe" => Ok(json!({
                "protocol": "1.84",
                "methods": rbs_api::request::METHODS.iter().map(|m| json!({
                    "name": m.name, "scope": m.scope, "effect": m.effect
                })).chain([json!({"name": "future.thing", "scope": "read", "effect": "read"})])
                .collect::<Vec<_>>(),
            })),
            "scene.remove" => Err(CallError::Api {
                kind: "not_permitted".into(),
                message: "on air".into(),
                data: json!({ "kind": "not_permitted", "reason": "on_air" }),
            }),
            _ => Ok(json!({ "ok": method })),
        }
    }
}

/// Each `connect` takes the next planned result; `None` means unreachable.
struct FakeConnector {
    plan: VecDeque<Option<FakeApi>>,
    attempts: Arc<Mutex<usize>>,
    token: Token,
}

impl Connect for FakeConnector {
    fn connect(&mut self) -> Result<Session, ConnectError> {
        *self.attempts.lock().unwrap() += 1;
        match self.plan.pop_front().flatten() {
            Some(api) => handshake(Box::new(api), &self.token),
            None => Err(ConnectError::Unreachable("no endpoint".into())),
        }
    }
}

fn api(calls: &Calls, scopes: &[Scope], irreversible: bool) -> FakeApi {
    FakeApi {
        calls: calls.clone(),
        scopes: scopes.to_vec(),
        irreversible,
        auth_error: None,
        lose_after: None,
        protocol: "1.84",
    }
}

struct Rig {
    mcp: Mcp<FakeConnector>,
    clock: Arc<TestClock>,
    attempts: Arc<Mutex<usize>>,
    out: Vec<String>,
    token: Token,
}

fn rig(plan: Vec<Option<FakeApi>>) -> Rig {
    let clock = Arc::new(TestClock::new());
    let attempts = Arc::new(Mutex::new(0));
    let token = Token::from_bytes(&[7; 32]);
    let connector = FakeConnector {
        plan: plan.into(),
        attempts: attempts.clone(),
        token: token.clone(),
    };
    let mut mcp = Mcp::new(connector, clock.clone(), None);
    mcp.start().unwrap();
    let mut r = Rig {
        mcp,
        clock,
        attempts,
        out: Vec::new(),
        token,
    };
    r.send(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#);
    r.send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    r
}

impl Rig {
    fn send(&mut self, line: &str) -> Vec<Value> {
        let o = self.mcp.handle(line);
        assert_eq!(o.exit, None);
        self.out.extend(o.out.iter().cloned());
        o.out
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn tools(&mut self) -> Vec<String> {
        let out = self.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let answer = out.iter().find(|v| v.get("id").is_some()).unwrap();
        answer["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect()
    }

    fn call(&mut self, name: &str, args: Value) -> (Value, bool) {
        let line = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": name, "arguments": args}})
        .to_string();
        let out = self.send(&line);
        let changed = out
            .iter()
            .any(|v| v["method"] == "notifications/tools/list_changed");
        let answer = out.into_iter().find(|v| v.get("id").is_some()).unwrap();
        (answer["result"].clone(), changed)
    }

    fn attempts(&self) -> usize {
        *self.attempts.lock().unwrap()
    }
}

#[test]
fn unreachable_lists_only_studio_summary_with_the_start_hint() {
    let mut r = rig(vec![None]);
    assert_eq!(r.tools(), ["studio_summary"]);
    let (result, _) = r.call("studio_summary", json!({}));
    assert_eq!(result["isError"], false);
    assert_eq!(result["content"][0]["text"], HINT);
    assert!(HINT.contains("press Start in Settings, AI tools"));
    let (result, _) = r.call("scene_list", json!({}));
    assert_eq!(result["isError"], true);
}

#[test]
fn retries_are_spaced_and_a_late_start_lists_the_tools() {
    let calls = Calls::default();
    let mut r = rig(vec![
        None,
        None,
        Some(api(&calls, &[Scope::Read, Scope::Control], false)),
    ]);
    assert_eq!(r.attempts(), 1);
    // An AI tool calling in a loop: no new attempt within RETRY_EVERY.
    for _ in 0..50 {
        r.call("studio_summary", json!({}));
    }
    assert_eq!(r.attempts(), 1);
    r.clock.advance(RETRY_EVERY);
    let (_, changed) = r.call("studio_summary", json!({}));
    assert_eq!(r.attempts(), 2);
    assert!(!changed, "still unreachable");
    r.clock.advance(RETRY_EVERY / 2);
    r.call("studio_summary", json!({}));
    assert_eq!(r.attempts(), 2);
    r.clock.advance(RETRY_EVERY);
    let (result, changed) = r.call("studio_summary", json!({}));
    assert_eq!(r.attempts(), 3);
    assert!(changed, "tools/list_changed after the handshake");
    assert_eq!(result["isError"], false);
    assert!(
        result["structuredContent"]["studio"].is_object(),
        "{result}"
    );
    assert!(r.tools().contains(&"scene_switch".to_owned()));
}

#[test]
fn tools_follow_the_scopes_and_the_permission() {
    let calls = Calls::default();
    // View only.
    let mut r = rig(vec![Some(api(&calls, &[Scope::Read], true))]);
    let tools = r.tools();
    assert!(tools.contains(&"scene_list".to_owned()));
    assert!(!tools.contains(&"scene_add".to_owned()));
    for t in &tools {
        let method = rbs_api::request::METHODS
            .iter()
            .find(|m| tool_name(m.name) == *t);
        if let Some(m) = method {
            assert_eq!(m.effect, Effect::Read, "{t}");
        }
    }
    // Control without the permission: edits, no live or destructive tool.
    let mut r = rig(vec![Some(api(
        &calls,
        &[Scope::Read, Scope::Control],
        false,
    ))]);
    let tools = r.tools();
    assert!(tools.contains(&"scene_add".to_owned()));
    assert!(!tools.contains(&"record_start".to_owned()));
    assert!(!tools.contains(&"source_reset".to_owned()));
    // Every scope with the permission: still never tokens, sessions, events, undo.
    let mut r = rig(vec![Some(api(
        &calls,
        &[
            Scope::Read,
            Scope::Control,
            Scope::Admin,
            Scope::Plugins,
            Scope::Tokens,
        ],
        true,
    ))]);
    let tools = r.tools();
    assert!(tools.contains(&"record_start".to_owned()));
    for t in &tools {
        assert!(!t.starts_with("tokens_"), "{t}");
        assert!(!t.starts_with("session_"), "{t}");
        assert!(!t.starts_with("events_"), "{t}");
        assert!(t != "edit_undo" && t != "edit_redo", "{t}");
        assert!(t != "future_thing", "only methods built into rbs-api");
    }
}

#[test]
fn tool_names_are_valid_and_unique() {
    let mut seen = std::collections::BTreeSet::new();
    for m in rbs_api::request::METHODS {
        let name = tool_name(m.name);
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "{name}"
        );
        assert!(seen.insert(name.clone()), "two methods map to {name}");
    }
}

#[test]
fn annotations_follow_the_effect_and_an_unknown_one_is_destructive() {
    let method = |effect| Method {
        name: "x.y".into(),
        scope: Some(Scope::Read),
        effect,
    };
    assert_eq!(
        annotations(&method(Some(Effect::Read)))["readOnlyHint"],
        true
    );
    assert_eq!(
        annotations(&method(Some(Effect::Edit)))["destructiveHint"],
        false
    );
    assert_eq!(
        annotations(&method(Some(Effect::Live)))["destructiveHint"],
        true
    );
    assert_eq!(annotations(&method(None))["destructiveHint"], true);
    let stream = Method {
        name: "stream.start".into(),
        scope: Some(Scope::Control),
        effect: Some(Effect::Live),
    };
    assert_eq!(annotations(&stream)["openWorldHint"], true);
}

#[test]
fn a_call_is_forwarded_and_an_api_error_is_a_tool_error() {
    let calls = Calls::default();
    let mut r = rig(vec![Some(api(
        &calls,
        &[Scope::Read, Scope::Control],
        false,
    ))]);
    let (result, _) = r.call("scene_rename", json!({"scene": 2, "name": "Intro"}));
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["ok"], "scene.rename");
    let last = calls.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        last,
        ("scene.rename".into(), json!({"scene": 2, "name": "Intro"}))
    );
    let (result, _) = r.call("scene_remove", json!({"scene": 2}));
    assert_eq!(result["isError"], true);
    assert_eq!(
        result["structuredContent"]["error"]["kind"],
        "not_permitted"
    );
    assert_eq!(result["structuredContent"]["error"]["reason"], "on_air");
    let (result, _) = r.call("tokens_list", json!({}));
    assert_eq!(result["isError"], true, "not a tool");
}

#[test]
fn a_lost_connection_goes_back_to_the_hint_and_reconnects() {
    let calls = Calls::default();
    let mut first = api(&calls, &[Scope::Read, Scope::Control], false);
    first.lose_after = Some(3);
    let second = api(&calls, &[Scope::Read], false);
    let mut r = rig(vec![Some(first), Some(second)]);
    assert!(r.mcp.connected());
    let (result, changed) = r.call("scene_list", json!({}));
    assert_eq!(result["isError"], true);
    assert!(changed);
    assert!(!r.mcp.connected());
    assert_eq!(r.tools(), ["studio_summary"]);
    r.clock.advance(RETRY_EVERY);
    let (_, changed) = r.call("scene_list", json!({}));
    assert!(changed);
    assert!(
        !r.tools().contains(&"scene_add".to_owned()),
        "the new session's scopes"
    );
}

#[test]
fn a_stopped_or_rotated_token_waits_in_the_hint_state() {
    let calls = Calls::default();
    let mut off = api(&calls, &[Scope::Read], false);
    off.auth_error = Some((
        "not_permitted",
        json!({"kind": "not_permitted", "reason": "ai_tools_off"}),
    ));
    let mut wrong = api(&calls, &[Scope::Read], false);
    wrong.auth_error = Some(("auth_failed", json!({"kind": "auth_failed"})));
    let mut r = rig(vec![Some(off), Some(wrong)]);
    assert_eq!(r.mcp.hint_reason(), Some("AI tools are stopped in RBS"));
    r.clock.advance(RETRY_EVERY);
    r.call("studio_summary", json!({}));
    assert_eq!(r.mcp.hint_reason(), Some("RBS did not accept the token"));
    assert_eq!(r.tools(), ["studio_summary"]);
}

#[test]
fn an_older_server_ends_rbs_mcp() {
    let calls = Calls::default();
    let mut old = api(&calls, &[Scope::Read], false);
    old.protocol = "1.72";
    let mut no_narrowing = api(&calls, &[Scope::Read], false);
    no_narrowing.auth_error = Some(("invalid_request", json!({"kind": "invalid_request"})));
    for a in [old, no_narrowing] {
        let connector = FakeConnector {
            plan: vec![Some(a)].into(),
            attempts: Arc::default(),
            token: Token::from_bytes(&[1; 32]),
        };
        let mut mcp = Mcp::new(connector, Arc::new(TestClock::new()), None);
        assert!(mcp.start().is_err());
    }
    // Nothing after hello when the protocol is too old.
    let methods: Vec<String> = calls.lock().unwrap().iter().map(|c| c.0.clone()).collect();
    assert_eq!(
        methods,
        ["session.hello", "session.hello", "session.authenticate"]
    );
}

#[test]
fn the_token_is_never_in_the_output() {
    let calls = Calls::default();
    let mut r = rig(vec![Some(api(
        &calls,
        &[Scope::Read, Scope::Control],
        true,
    ))]);
    r.tools();
    r.call("studio_summary", json!({}));
    r.call("scene_list", json!({}));
    let token = r.token.expose().to_owned();
    for line in &r.out {
        assert!(!line.contains(&token));
    }
    // It went to the server, once, in session.authenticate.
    let sent: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.1.to_string().contains(&token))
        .map(|c| c.0.clone())
        .collect();
    assert_eq!(sent, ["session.authenticate"]);
}

#[test]
fn namespaces_limit_the_generated_tools() {
    let calls = Calls::default();
    let clock = Arc::new(TestClock::new());
    let connector = FakeConnector {
        plan: vec![Some(api(&calls, &[Scope::Read, Scope::Control], false))].into(),
        attempts: Arc::default(),
        token: Token::from_bytes(&[1; 32]),
    };
    let mut mcp = Mcp::new(connector, clock, Some(vec!["scene".into()]));
    mcp.start().unwrap();
    let o = mcp.handle(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let v: Value = serde_json::from_str(&o.out[0]).unwrap();
    for t in v["result"]["tools"].as_array().unwrap() {
        let name = t["name"].as_str().unwrap();
        assert!(
            name == "studio_summary" || name.starts_with("scene_"),
            "{name}"
        );
    }
}
