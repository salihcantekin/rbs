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
    extra_methods: Vec<Value>,
    omitted_methods: Vec<&'static str>,
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
                "methods": rbs_api::request::METHODS.iter().filter(|m| !self.omitted_methods.contains(&m.name)).map(|m| json!({
                    "name": m.name, "scope": m.scope, "effect": m.effect
                })).chain(self.extra_methods.iter().cloned())
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
        extra_methods: vec![json!({"name": "future.thing", "scope": "read", "effect": "read"})],
        omitted_methods: Vec::new(),
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
fn tools_use_server_schemas_and_legacy_servers_keep_the_fallback() {
    let calls = Calls::default();
    let mut server = api(&calls, &[Scope::Read, Scope::Control], false);
    server.omitted_methods.push("scene.add");
    let pair = rbs_api::schema::builtin_schemas()
        .unwrap()
        .remove("scene.add")
        .unwrap();
    server.extra_methods.push(json!({
        "name": "scene.add", "scope": "control", "effect": "edit",
        "input_schema": pair.input_schema, "output_schema": pair.output_schema
    }));
    let mut r = rig(vec![Some(server)]);
    let list = r.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = list[0]["result"]["tools"].as_array().unwrap();
    let add = tools.iter().find(|t| t["name"] == "scene_add").unwrap();
    assert_eq!(add["inputSchema"], pair.input_schema);
    assert_eq!(add["outputSchema"], pair.output_schema);
    let legacy = tools.iter().find(|t| t["name"] == "scene_rename").unwrap();
    assert!(legacy.get("outputSchema").is_none());
    assert!(legacy["inputSchema"].is_object());
}

#[test]
fn rejects_partial_or_invalid_server_schema_metadata() {
    let calls = Calls::default();
    for (input, output) in [
        (json!({"type": "object"}), Value::Null),
        (Value::Null, json!({"type": "object"})),
        (json!({"type": "string"}), json!({"type": "object"})),
        (json!({"type": "object"}), json!("not a schema")),
        (
            json!({"type": "object", "properties": {"x": {"type": 42}}}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "properties": {"x": {"$ref": "https://example.test/schema"}}}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "properties": {"x": {"$ref": "#/$defs/missing"}}}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "default": {"type": 42}, "properties": {"x": {"$ref": "#/default"}}}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "$id": "https://example.test/"}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "$ref": "#"}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "$defs": {"node": {"type": "object", "properties": {"child": {"$ref": "#/$defs/node"}}}}, "properties": {"root": {"$ref": "#/$defs/node"}}}),
            json!({"type": "object"}),
        ),
        (
            json!({"type": "object", "$defs": {"a": {"$ref": "#/$defs/b"}, "b": {"$ref": "#/$defs/a"}}}),
            json!({"type": "object"}),
        ),
    ] {
        let mut server = api(&calls, &[Scope::Read, Scope::Control], false);
        server.extra_methods.push(json!({
            "name": "future.schema", "scope": "read", "effect": "read",
            "input_schema": input, "output_schema": output
        }));
        assert!(matches!(
            handshake(Box::new(server), &Token::from_bytes(&[7; 32])),
            Err(ConnectError::InvalidCatalog(_))
        ));
    }
}

#[test]
fn accepts_a_bounded_schema_with_a_local_reference() {
    let calls = Calls::default();
    let mut server = api(&calls, &[Scope::Read], false);
    server.extra_methods.push(json!({
        "name": "future.reference", "scope": "read", "effect": "read",
        "input_schema": {
            "type": "object",
            "$defs": {"value": {"type": "string"}},
            "properties": {"name": {"$ref": "#/$defs/value"}}
        },
        "output_schema": {"type": "object"}
    }));
    assert!(handshake(Box::new(server), &Token::from_bytes(&[7; 32])).is_ok());
}

#[test]
fn rejects_schemas_that_exceed_the_depth_or_size_bound() {
    let calls = Calls::default();
    let mut nested = json!({"type": "string"});
    for _ in 0..65 {
        nested = json!({"items": nested});
    }
    for input in [
        json!({"type": "object", "properties": {"x": nested}}),
        json!({"type": "object", "description": "x".repeat(65_536)}),
    ] {
        let mut server = api(&calls, &[Scope::Read], false);
        server.extra_methods.push(json!({
            "name": "future.bounded", "scope": "read", "effect": "read",
            "input_schema": input, "output_schema": {"type": "object"}
        }));
        assert!(matches!(
            handshake(Box::new(server), &Token::from_bytes(&[7; 32])),
            Err(ConnectError::InvalidCatalog(_))
        ));
    }
}

#[test]
fn non_object_output_schema_is_not_exposed_as_an_mcp_output_schema() {
    let calls = Calls::default();
    let mut server = api(&calls, &[Scope::Read], false);
    server.extra_methods.push(json!({
        "name": "future.scalar", "scope": "read", "effect": "read",
        "input_schema": {"type": "object"}, "output_schema": {"type": "string"}
    }));
    let mut r = rig(vec![Some(server)]);
    let list = r.send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
    let tools = list[0]["result"]["tools"].as_array().unwrap();
    let scalar = tools.iter().find(|t| t["name"] == "future_scalar").unwrap();
    assert!(scalar.get("outputSchema").is_none());
}

#[test]
fn nullable_object_output_schema_matches_structured_content() {
    let catalog = rbs_api::schema::builtin_schemas().unwrap();
    let schema = &catalog["source.attributes_clipboard"].output_schema;
    let output = mcp_output_schema(schema).unwrap();
    let clipboard = json!({"source": 1, "name": "Camera", "type": "camera", "groups": []});
    for (value, valid) in [
        (clipboard.clone(), true),
        (json!({"result": null}), true),
        (json!({"result": clipboard}), false),
        (json!({"result": null, "unexpected": true}), false),
        (json!({"source": -1}), false),
        (Value::Null, false),
    ] {
        assert_eq!(jsonschema::is_valid(&output, &value), valid, "{value}");
    }
    let unsupported = json!({
        "$defs": {"value": {"type": "object"}},
        "anyOf": [{"$ref": "#/$defs/value"}, {"type": "null"}],
        "not": {"required": ["result"]}
    });
    assert!(mcp_output_schema(&unsupported).is_none());
    let ref_to_replaced_branch = json!({
        "$defs": {"value": {
            "type": "object",
            "properties": {"field": {"$ref": "#/anyOf/1"}}
        }},
        "anyOf": [{"$ref": "#/$defs/value"}, {"type": "null"}]
    });
    assert!(mcp_output_schema(&ref_to_replaced_branch).is_none());
    let ignored_ref_sibling = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "$defs": {
            "value": {"$ref": "#/$defs/scalar", "type": "object"},
            "scalar": {"type": "string"}
        },
        "anyOf": [{"$ref": "#/$defs/value"}, {"type": "null"}]
    });
    assert!(mcp_output_schema(&ignored_ref_sibling).is_none());
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
        assert!(
            ![
                "addon_install",
                "addon_update",
                "addon_cancel",
                "addon_clean",
                "addon_remove"
            ]
            .contains(&t.as_str()),
            "{t}"
        );
        assert!(t != "edit_undo" && t != "edit_redo", "{t}");
    }
}

#[test]
fn malformed_or_colliding_server_catalogs_fail_closed() {
    let calls = Calls::default();
    for extra in [
        json!({"name": "scene_list", "scope": "read", "effect": "read"}),
        json!({"name": "scene.list", "scope": "read", "effect": "read"}),
        json!({"name": "studio.summary", "scope": "read", "effect": "read"}),
        json!({"name": "bad.!", "scope": "read", "effect": "read"}),
        json!({"name": format!("long.{}", "a".repeat(60)), "scope": "read", "effect": "read"}),
        json!({"scope": "read", "effect": "read"}),
        json!({"name": "future..bad", "scope": "read", "effect": "read"}),
    ] {
        let mut server = api(&calls, &[Scope::Read], true);
        server.extra_methods = vec![extra];
        let mut r = rig(vec![Some(server)]);
        assert_eq!(r.tools(), ["studio_summary"]);
        assert!(r.mcp.hint_reason().unwrap().contains("RBS sent"));
        let (answer, _) = r.call("studio_summary", json!({}));
        assert_eq!(answer["isError"], true);
        assert_eq!(answer["content"][0]["text"], INVALID_CATALOG_HINT);
    }
    // Both names are individually valid but map to the same MCP name.
    let mut server = api(&calls, &[Scope::Read], true);
    server.extra_methods = vec![
        json!({"name": "new.method_name", "scope": "read", "effect": "read"}),
        json!({"name": "new.method.name", "scope": "read", "effect": "read"}),
    ];
    let mut r = rig(vec![Some(server)]);
    assert_eq!(r.tools(), ["studio_summary"]);
    assert!(r.mcp.hint_reason().unwrap().contains("conflicting"));
}

#[test]
fn new_method_metadata_fails_closed_for_unknown_scope_and_effect() {
    let calls = Calls::default();
    let mut server = api(
        &calls,
        &[Scope::Read, Scope::Control, Scope::Plugins],
        false,
    );
    server.extra_methods = vec![
        json!({"name": "future.read", "scope": "read", "effect": "read"}),
        json!({"name": "future.unknown_scope", "scope": "new_scope", "effect": "read"}),
        json!({"name": "future.unknown_effect", "scope": "control", "effect": "future"}),
        json!({"name": "future.missing_effect", "scope": "control"}),
        json!({"name": "future.plugin", "scope": "plugins", "effect": "read"}),
    ];
    let mut r = rig(vec![Some(server)]);
    let tools = r.tools();
    assert!(tools.contains(&"future_read".to_owned()));
    assert!(!tools.contains(&"future_unknown_scope".to_owned()));
    assert!(!tools.contains(&"future_unknown_effect".to_owned()));
    assert!(!tools.contains(&"future_missing_effect".to_owned()));
    assert!(!tools.contains(&"future_plugin".to_owned()));
    let list = r.send(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
    let method = list[0]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "future_read")
        .unwrap();
    assert_eq!(method["inputSchema"], json!({"type": "object"}));
    assert_eq!(method["annotations"]["openWorldHint"], true);
    assert_eq!(method["description"], "The control API method future.read.");

    let mut server = api(&calls, &[Scope::Read, Scope::Control], true);
    server.extra_methods = vec![
        json!({"name": "future.unknown_effect", "scope": "control", "effect": "future"}),
        json!({"name": "future.missing_effect", "scope": "control"}),
        json!({"name": "future.missing_scope", "effect": "read"}),
    ];
    let mut r = rig(vec![Some(server)]);
    let tools = r.tools();
    assert!(tools.contains(&"future_unknown_effect".to_owned()));
    assert!(tools.contains(&"future_missing_effect".to_owned()));
    assert!(!tools.contains(&"future_missing_scope".to_owned()));
    let list = r.send(r#"{"jsonrpc":"2.0","id":9,"method":"tools/list"}"#);
    for name in ["future_unknown_effect", "future_missing_effect"] {
        let method = list[0]["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap();
        assert_eq!(method["annotations"]["destructiveHint"], true);
    }
}

#[test]
fn summary_applies_the_same_scope_and_effect_checks_as_generated_tools() {
    for (scope, effect) in [("plugins", "read"), ("read", "future")] {
        let calls = Calls::default();
        let mut server = api(&calls, &[Scope::Read, Scope::Plugins], false);
        server.omitted_methods.push("scene.list");
        server
            .extra_methods
            .push(json!({"name": "scene.list", "scope": scope, "effect": effect}));
        let mut r = rig(vec![Some(server)]);
        assert!(!r.tools().contains(&"scene_list".to_owned()));
        let (answer, _) = r.call("studio_summary", json!({}));
        assert_eq!(answer["isError"], false);
        assert!(answer["structuredContent"].get("scenes").is_none());
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|(method, _)| method == "scene.list")
        );
    }
}

#[test]
fn tools_use_the_connected_servers_catalog_without_a_local_method_whitelist() {
    let calls = Calls::default();
    let mut r = rig(vec![Some(api(&calls, &[Scope::Read], true))]);

    assert!(r.tools().contains(&"future_thing".to_owned()));
    let (result, _) = r.call("future_thing", json!({}));
    assert_eq!(result["isError"], false);
    assert!(
        calls
            .lock()
            .unwrap()
            .iter()
            .any(|(method, _)| method == "future.thing")
    );
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
        input_schema: None,
        output_schema: None,
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
        input_schema: None,
        output_schema: None,
    };
    assert_eq!(annotations(&stream)["openWorldHint"], true);
}

#[test]
fn parameter_dependent_actions_have_conservative_annotations() {
    let method = |name: &str| Method {
        name: name.into(),
        scope: Some(Scope::Admin),
        effect: Some(Effect::Edit),
        input_schema: None,
        output_schema: None,
    };
    assert_eq!(
        annotations(&method("settings.set"))["destructiveHint"],
        true
    );
    for name in [
        "source.add",
        "source.retarget",
        "source.set_settings",
        "stream.start",
        "stream.destination.start",
        "addon.install",
        "addon.update",
        "record.start_to_file",
        "collection.set_stinger_file",
        "settings.set",
    ] {
        assert_eq!(annotations(&method(name))["openWorldHint"], true, "{name}");
    }
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
