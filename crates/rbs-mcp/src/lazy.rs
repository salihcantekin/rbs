//! The default tool set (ADR-0196): a small, fixed list through which an agent finds
//! methods, loads the schemas it needs and calls them, instead of one tool and one
//! schema per method. Every check that applied to a generated tool applies here,
//! in the same order, so a hidden method is never confirmed by an error message.

use std::collections::BTreeMap;

use rbs_api::request::Effect;
use serde_json::{Map, Value, json};

use crate::catalog;
use crate::link::{Method, Session};

/// Lists namespaces, or the methods of one.
pub const INDEX: &str = "api_index";
/// Input and output schemas of named methods.
pub const SCHEMA: &str = "api_schema";
/// Calls a method of effect `read`.
pub const READ: &str = "api_read";
/// Calls a method of effect `edit`.
pub const EDIT: &str = "api_edit";
/// Calls a method of effect `live` or `destructive` (or none).
pub const LIVE: &str = "api_live";

/// Most methods one `api_schema` call may name.
pub const MAX_SCHEMA_METHODS: usize = 16;

/// Whether `name` is one of these tools.
pub fn is_lazy_tool(name: &str) -> bool {
    matches!(name, INDEX | SCHEMA | READ | EDIT | LIVE)
}

/// The tool a method is called through. A missing or unknown effect is handled as
/// `destructive`.
fn call_tool(method: &Method) -> &'static str {
    match method.effect {
        Some(Effect::Read) => READ,
        Some(Effect::Edit) => EDIT,
        _ => LIVE,
    }
}

/// The tools listed for `session`: the call tool of an effect class is listed only when
/// the session may call a method of it.
pub fn tool_list(session: &Session, callable: &[&Method]) -> Vec<Value> {
    let mut list = vec![
        json!({
            "name": INDEX,
            "description": "Find RBS control API methods. Without `namespace`: every namespace with its method count. With `namespace` (for example `source`): its methods, each with a one-line description, its effect and the tool to call it through. Call this, then api_schema, then the call tool.",
            "inputSchema": {
                "type": "object",
                "properties": { "namespace": { "type": "string" } },
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true }
        }),
        json!({
            "name": SCHEMA,
            "description": format!("The input and output JSON Schema of up to {MAX_SCHEMA_METHODS} methods named by api_index, loaded on demand."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "methods": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "maxItems": MAX_SCHEMA_METHODS
                    }
                },
                "required": ["methods"],
                "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true }
        }),
    ];
    let has = |tool: &str| callable.iter().any(|m| call_tool(m) == tool);
    let call_schema = json!({
        "type": "object",
        "properties": {
            "method": { "type": "string", "description": "A method name from api_index, such as `scene.list`." },
            "params": { "type": "object", "description": "The method's parameters, as api_schema describes them." }
        },
        "required": ["method"],
        "additionalProperties": false
    });
    if has(READ) {
        list.push(json!({
            "name": READ,
            "description": "Call a control API method whose effect is `read` (it changes nothing).",
            "inputSchema": call_schema,
            "annotations": { "readOnlyHint": true }
        }));
    }
    if has(EDIT) {
        list.push(json!({
            "name": EDIT,
            "description": "Call a control API method whose effect is `edit` (a change the user can undo in RBS).",
            "inputSchema": call_schema,
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": true }
        }));
    }
    if has(LIVE) && session.irreversible {
        list.push(json!({
            "name": LIVE,
            "description": "Call a control API method whose effect is `live` (recording, streaming, switching) or `destructive`. These cannot be undone.",
            "inputSchema": call_schema,
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": true }
        }));
    }
    list
}

/// What a lazy tool answers: `Ok` is the JSON result, `Err` a message for a tool error.
pub type Answer = Result<Value, String>;

fn unknown_method(name: &str) -> String {
    format!("There is no method {name:?} for this session. Use {INDEX} to see what is available.")
}

/// `api_index`.
pub fn index(callable: &[&Method], arguments: &Value) -> Answer {
    match arguments.get("namespace").and_then(Value::as_str) {
        None => {
            let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
            for m in callable {
                *counts
                    .entry(m.name.split('.').next().unwrap_or(""))
                    .or_default() += 1;
            }
            Ok(json!({
                "namespaces": counts
                    .into_iter()
                    .map(|(name, methods)| json!({ "name": name, "methods": methods }))
                    .collect::<Vec<_>>()
            }))
        }
        Some(namespace) => {
            let methods: Vec<Value> = callable
                .iter()
                .filter(|m| m.name.split('.').next() == Some(namespace))
                .map(|m| {
                    json!({
                        "method": m.name,
                        "description": catalog::summary(&m.name),
                        "effect": m.effect,
                        "tool": call_tool(m),
                    })
                })
                .collect();
            if methods.is_empty() {
                Err(format!(
                    "There is no namespace {namespace:?} for this session. Call {INDEX} without arguments to list them."
                ))
            } else {
                Ok(json!({ "namespace": namespace, "methods": methods }))
            }
        }
    }
}

/// The method names `api_schema` was asked about, checked and ready to load.
pub fn schema_names(callable: &[&Method], arguments: &Value) -> Result<Vec<String>, String> {
    let names: Vec<&str> = arguments
        .get("methods")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if names.is_empty() || names.len() > MAX_SCHEMA_METHODS {
        return Err(format!(
            "Name between 1 and {MAX_SCHEMA_METHODS} methods in `methods`."
        ));
    }
    let mut unique: Vec<String> = Vec::new();
    for name in names {
        if !callable.iter().any(|m| m.name == name) {
            return Err(unknown_method(name));
        }
        if !unique.iter().any(|n| n == name) {
            unique.push(name.to_owned());
        }
    }
    Ok(unique)
}

/// `api_schema`, once `names` are loaded into `callable`.
pub fn schemas(callable: &[&Method], names: &[String]) -> Value {
    let mut out = Map::new();
    for name in names {
        let Some(m) = callable.iter().find(|m| &m.name == name) else {
            continue;
        };
        let entry = catalog::entry(&m.name);
        let mut o = Map::new();
        o.insert("description".into(), entry.description.into());
        o.insert("effect".into(), json!(m.effect));
        o.insert("tool".into(), call_tool(m).into());
        o.insert(
            "input_schema".into(),
            m.input_schema.clone().unwrap_or(entry.input_schema),
        );
        if let Some(output) = &m.output_schema {
            o.insert("output_schema".into(), output.clone());
        }
        out.insert(name.clone(), Value::Object(o));
    }
    json!({ "methods": out })
}

/// A call tool's target: the method to call, after the order of checks of ADR-0196.
pub fn call_target<'a>(
    tool: &str,
    callable: &[&'a Method],
    arguments: &Value,
) -> Result<&'a Method, String> {
    let name = arguments
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or("");
    // 1. Hidden methods are unknown, whatever the tool.
    let Some(method) = callable.iter().find(|m| m.name == name) else {
        return Err(unknown_method(name));
    };
    // 2. Then the effect class.
    let right = call_tool(method);
    if right != tool {
        return Err(format!("{name} is called through {right}, not {tool}."));
    }
    Ok(method)
}
