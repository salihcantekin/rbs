//! MCP over stdio for one RBS control API token (ADR-0052, ADR-0134, mcp-server spec).
//!
//! [`Mcp`] answers one MCP message at a time. Connected, it lists one tool per control
//! API method the token may use (generated from `api.describe`) and `studio_summary`.
//! Not connected (RBS is not running, AI tools are stopped, the token file is missing
//! or the token was refused), it waits in a hint state: only `studio_summary` is
//! listed, and it says to press Start in RBS. It tries again on a tool call, at most
//! once every [`RETRY_EVERY`] (an injected clock), and sends
//! `notifications/tools/list_changed` when the tools change. Nothing here reaches RBS
//! except through the control API.

pub mod catalog;
pub mod lazy;
pub mod link;
pub mod token;

use std::sync::Arc;
use std::time::Duration;

use rbs_api::request::Effect;
use rbs_core::time::{Clock, Timestamp};
use serde_json::{Map, Value, json};

use crate::link::{CallError, Connect, ConnectError, Method, Session, needs_permission};

/// The least time between two connection attempts (ADR-0134 decision 6).
pub const RETRY_EVERY: Duration = Duration::from_secs(2);

/// What `studio_summary` answers while not connected.
pub const HINT: &str = "RBS is not connected. Open RBS and press Start in Settings, AI tools.";

const INVALID_CATALOG_HINT: &str =
    "RBS sent an invalid method catalog. Check the rbs-mcp diagnostic output.";

/// MCP protocol versions this server speaks, newest first.
const MCP_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Namespaces never offered, whatever the token's scopes (mcp-server spec).
const NEVER_NAMESPACES: &[&str] = &["session", "tokens", "events", "plugin", "plugins"];

/// Methods never offered: the undo stack is shared with the user.
const NEVER_METHODS: &[&str] = &["edit.undo", "edit.redo"];

/// Methods that reach the network or add a source that may (`openWorldHint`).
const OPEN_WORLD: &[&str] = &[
    "stream.start",
    "stream.destination.start",
    "source.add",
    "source.set_settings",
    "source.retarget",
    "addon.install",
    "addon.update",
    "record.start_to_file",
    "collection.set_stinger_file",
    "settings.set",
];

/// The reads `studio_summary` combines, under these keys.
const SUMMARY: &[(&str, &str)] = &[
    ("studio", "studio.get"),
    ("scenes", "scene.list"),
    ("sources", "source.list"),
    ("profiles", "profile.list"),
    ("collections", "collection.list"),
    ("recording", "record.status"),
    ("stream", "stream.status"),
];

/// Which tools `rbs-mcp` lists (ADR-0196).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolSet {
    /// The small fixed set of [`lazy`]: index, schema and call tools (the default).
    #[default]
    Lazy,
    /// The fixed set plus one tool per method of these namespaces (`--tools a,b`).
    Namespaces(Vec<String>),
    /// One tool per allowed method, no fixed set (`--tools all`).
    All,
}

impl ToolSet {
    /// Parses the value of `--tools`.
    pub fn parse(list: &str) -> Self {
        let names: Vec<String> = list
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if names.iter().any(|n| n == "all") {
            Self::All
        } else if names.is_empty() {
            Self::Lazy
        } else {
            Self::Namespaces(names)
        }
    }

    fn namespaces(&self) -> Option<&[String]> {
        match self {
            Self::Namespaces(list) => Some(list),
            _ => None,
        }
    }

    fn lazy(&self) -> bool {
        !matches!(self, Self::All)
    }

    fn direct(&self) -> bool {
        !matches!(self, Self::Lazy)
    }
}

/// What handling one message produced.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// Lines to write to stdout, in order.
    pub out: Vec<String>,
    /// Set when `rbs-mcp` must end with a non-zero exit, with the reason for stderr.
    pub exit: Option<String>,
}

enum State {
    /// Not connected; the reason, for stderr only.
    Hint {
        reason: String,
        catalog_error: bool,
    },
    Ready(Ready),
}

struct Ready {
    session: Session,
    /// Tool name → method name.
    tools: Vec<(String, Method)>,
}

/// The MCP server.
pub struct Mcp<C: Connect> {
    connector: C,
    clock: Arc<dyn Clock>,
    tools: ToolSet,
    state: State,
    last_try: Option<Timestamp>,
    initialized: bool,
}

impl<C: Connect> Mcp<C> {
    /// A server that connects with `connector`; `tools` says which tools are listed
    /// (`--tools`).
    pub fn new(connector: C, clock: Arc<dyn Clock>, tools: ToolSet) -> Self {
        Self {
            connector,
            clock,
            tools,
            state: State::Hint {
                reason: "not tried yet".into(),
                catalog_error: false,
            },
            last_try: None,
            initialized: false,
        }
    }

    /// The first connection attempt. `Err` (for stderr) when the server is too old,
    /// which ends `rbs-mcp`; any other failure leaves it in the hint state.
    pub fn start(&mut self) -> Result<(), String> {
        match self.try_connect() {
            Err(ConnectError::TooOld(why)) => Err(why),
            _ => Ok(()),
        }
    }

    /// True while connected.
    pub fn connected(&self) -> bool {
        matches!(self.state, State::Ready(_))
    }

    /// Why it is not connected, for stderr (never the token).
    pub fn hint_reason(&self) -> Option<&str> {
        match &self.state {
            State::Hint { reason, .. } => Some(reason),
            State::Ready(_) => None,
        }
    }

    fn try_connect(&mut self) -> Result<(), ConnectError> {
        self.last_try = Some(self.clock.now());
        match self.connector.connect() {
            Ok(mut session) => {
                let tools = if self.tools.direct() {
                    let mut spaces: Vec<String> = generated(&session, self.tools.namespaces())
                        .iter()
                        .filter_map(|(_, m)| m.name.split('.').next().map(str::to_owned))
                        .collect();
                    spaces.sort();
                    spaces.dedup();
                    if let Err(e) = session.load_namespace_schemas(&spaces) {
                        let e = match e {
                            CallError::Lost(why) => ConnectError::Unreachable(why),
                            CallError::Api { message, .. } => ConnectError::InvalidCatalog(message),
                        };
                        return self.fail(e);
                    }
                    // Clone after loading: the tools carry the schemas.
                    generated(&session, self.tools.namespaces())
                } else {
                    Vec::new()
                };
                self.state = State::Ready(Ready { session, tools });
                Ok(())
            }
            Err(e) => self.fail(e),
        }
    }

    fn fail(&mut self, e: ConnectError) -> Result<(), ConnectError> {
        let catalog_error = matches!(e, ConnectError::InvalidCatalog(_));
        let reason = match &e {
            ConnectError::Unreachable(w)
            | ConnectError::NoToken(w)
            | ConnectError::Refused(w)
            | ConnectError::InvalidCatalog(w)
            | ConnectError::TooOld(w) => w.clone(),
        };
        self.state = State::Hint {
            reason,
            catalog_error,
        };
        Err(e)
    }

    /// Tries again when not connected and the last try is [`RETRY_EVERY`] ago.
    /// Returns a too-old server's reason.
    fn retry(&mut self, out: &mut Vec<String>) -> Option<String> {
        if self.connected() {
            return None;
        }
        let now = self.clock.now();
        if self
            .last_try
            .is_some_and(|t| now.saturating_since(t) < RETRY_EVERY)
        {
            return None;
        }
        match self.try_connect() {
            Ok(()) => {
                if self.initialized {
                    out.push(list_changed());
                }
                None
            }
            Err(ConnectError::TooOld(why)) => Some(why),
            Err(_) => None,
        }
    }

    /// Handles one line from stdin.
    pub fn handle(&mut self, line: &str) -> Outcome {
        let mut outcome = Outcome::default();
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            outcome
                .out
                .push(error_response(Value::Null, -32700, "parse error"));
            return outcome;
        };
        let id = value.get("id").cloned();
        let method = value.get("method").and_then(Value::as_str).unwrap_or("");
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str);
                let version = asked
                    .filter(|v| MCP_VERSIONS.contains(v))
                    .unwrap_or(MCP_VERSIONS[0]);
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": true } },
                    "serverInfo": { "name": "rbs", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "RBS is a live production engine (scenes, sources, recording, streaming). Call studio_summary first, then api_index to find methods and api_schema to learn a method's parameters before calling it. Edits are undoable in RBS; live actions (recording, streaming) are not."
                })
            }
            "notifications/initialized" => {
                self.initialized = true;
                return outcome;
            }
            "ping" => json!({}),
            "tools/list" => {
                outcome.exit = self.retry(&mut outcome.out);
                json!({ "tools": self.tool_list() })
            }
            "tools/call" => {
                outcome.exit = self.retry(&mut outcome.out);
                self.call(&params, &mut outcome.out)
            }
            _ => {
                if let Some(id) = id {
                    outcome
                        .out
                        .push(error_response(id, -32601, "method not found"));
                }
                return outcome;
            }
        };
        if let Some(id) = id {
            outcome
                .out
                .push(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string());
        }
        outcome
    }

    fn tool_list(&self) -> Vec<Value> {
        let mut list = vec![json!({
            "name": "studio_summary",
            "description": "What the studio is doing: canvas, scenes with their items, sources, profiles and scene collections, recording and stream state (`stream.available` is false while no stream destination is on, so Go live is off), in one JSON value. Call it first.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true }
        })];
        if let State::Ready(ready) = &self.state {
            if self.tools.lazy() {
                list.extend(lazy::tool_list(&ready.session, &callable(&ready.session)));
            }
            for (tool, method) in &ready.tools {
                let entry = catalog::entry(&method.name);
                let mut tool = json!({
                    "name": tool,
                    "description": entry.description,
                    "inputSchema": method.input_schema.as_ref().unwrap_or(&entry.input_schema),
                    "annotations": annotations(method),
                });
                if let Some(schema) = method.output_schema.as_ref().and_then(mcp_output_schema) {
                    tool["outputSchema"] = schema;
                }
                list.push(tool);
            }
        }
        list
    }

    fn call(&mut self, params: &Value, out: &mut Vec<String>) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let lazy_on = self.tools.lazy();
        let State::Ready(ready) = &mut self.state else {
            let hint = if matches!(
                self.state,
                State::Hint {
                    catalog_error: true,
                    ..
                }
            ) {
                INVALID_CATALOG_HINT
            } else {
                HINT
            };
            return if name == "studio_summary" {
                text_result(hint, hint == INVALID_CATALOG_HINT)
            } else {
                text_result(hint, true)
            };
        };
        let answer = if name == "studio_summary" {
            summary(ready)
        } else if lazy_on && lazy::is_lazy_tool(name) {
            match lazy_call(ready, name, &arguments) {
                Ok(answer) => answer,
                Err(text) => return text_result(&text, true),
            }
        } else {
            let Some((_, method)) = ready.tools.iter().find(|(t, _)| t == name) else {
                return text_result(&format!("There is no tool {name:?}."), true);
            };
            let method = method.name.clone();
            ready.session.api.call(&method, arguments)
        };
        match answer {
            Ok(value) => structured(value),
            Err(CallError::Api {
                kind,
                message,
                data,
            }) => {
                let mut error = Map::new();
                error.insert("kind".into(), kind.into());
                error.insert("message".into(), message.into());
                if let Some(reason) = data.get("reason") {
                    error.insert("reason".into(), reason.clone());
                }
                let error = Value::Object(error);
                json!({
                    "content": [{ "type": "text", "text": error.to_string() }],
                    "structuredContent": { "error": error },
                    "isError": true
                })
            }
            Err(CallError::Lost(why)) => {
                self.state = State::Hint {
                    reason: why,
                    catalog_error: false,
                };
                // Try again on the next call, not at once.
                self.last_try = Some(self.clock.now());
                if self.initialized {
                    out.push(list_changed());
                }
                text_result(
                    "The connection to RBS was lost. Check that RBS is open and AI tools are started, then try again.",
                    true,
                )
            }
        }
    }
}

/// One of the fixed tools of [`lazy`]. `Err` is a message for a tool error; `Ok` the
/// API's answer.
fn lazy_call(
    ready: &mut Ready,
    name: &str,
    arguments: &Value,
) -> Result<Result<Value, CallError>, String> {
    match name {
        lazy::INDEX => lazy::index(&callable(&ready.session), arguments).map(Ok),
        lazy::SCHEMA => {
            let names = lazy::schema_names(&callable(&ready.session), arguments)?;
            match ready.session.load_schemas(&names) {
                Ok(()) => {}
                Err(CallError::Api { message, .. }) => return Err(message),
                Err(lost) => return Ok(Err(lost)),
            }
            Ok(Ok(lazy::schemas(&callable(&ready.session), &names)))
        }
        _ => {
            let method = lazy::call_target(name, &callable(&ready.session), arguments)?
                .name
                .clone();
            let params = arguments
                .get("params")
                .cloned()
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            Ok(ready.session.api.call(&method, params))
        }
    }
}

/// The generated tools of a session: methods advertised by the connected RBS
/// server that the session's scopes and permission allow, minus those never offered.
fn generated(session: &Session, namespaces: Option<&[String]>) -> Vec<(String, Method)> {
    callable(session)
        .into_iter()
        .filter(|m| {
            let ns = m.name.split('.').next().unwrap_or("");
            namespaces.is_none_or(|list| list.iter().any(|n| n == ns))
        })
        .map(|m| (tool_name(&m.name), m.clone()))
        .collect()
}

/// The methods this session may call through any tool: not a never-offered one, and
/// allowed by the token's scopes and permission.
fn callable(session: &Session) -> Vec<&Method> {
    session
        .methods
        .iter()
        .filter(|m| {
            let ns = m.name.split('.').next().unwrap_or("");
            !NEVER_NAMESPACES.contains(&ns) && !NEVER_METHODS.contains(&m.name.as_str())
        })
        .filter(|m| allowed(session, m))
        .collect()
}

fn allowed(session: &Session, method: &Method) -> bool {
    method.scope.is_some_and(|scope| {
        !matches!(scope, rbs_api::Scope::Plugins | rbs_api::Scope::Tokens)
            && session.scopes.contains(&scope)
    }) && (session.irreversible || !needs_permission(method.effect))
}

/// `source.add` → `source_add`.
pub fn tool_name(method: &str) -> String {
    method.replace('.', "_")
}

fn annotations(method: &Method) -> Value {
    let mut a = match method.effect {
        Some(Effect::Read) => json!({ "readOnlyHint": true }),
        Some(Effect::Edit) => json!({ "readOnlyHint": false, "destructiveHint": false }),
        _ => json!({ "readOnlyHint": false, "destructiveHint": true }),
    };
    if method.name == "settings.set" {
        a["destructiveHint"] = json!(true);
    }
    if OPEN_WORLD.contains(&method.name.as_str())
        || !rbs_api::request::METHODS
            .iter()
            .any(|known| known.name == method.name)
    {
        a["openWorldHint"] = json!(true);
    }
    a
}

fn summary(ready: &mut Ready) -> Result<Value, CallError> {
    let mut out = Map::new();
    for (key, method) in SUMMARY {
        if !ready
            .session
            .methods
            .iter()
            .any(|m| m.name == *method && allowed(&ready.session, m))
        {
            continue;
        }
        match ready.session.api.call(method, json!({})) {
            Ok(v) => {
                out.insert((*key).into(), v);
            }
            Err(CallError::Api { kind, .. }) => {
                out.insert((*key).into(), json!({ "error": kind }));
            }
            Err(lost) => return Err(lost),
        }
    }
    Ok(Value::Object(out))
}

fn structured(value: Value) -> Value {
    let text = value.to_string();
    let object = if value.is_object() {
        value
    } else {
        json!({ "result": value })
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": object,
        "isError": false
    })
}

fn mcp_output_schema(schema: &Value) -> Option<Value> {
    fn references_stay_in_defs(value: &Value) -> bool {
        match value {
            Value::Object(object) => object.iter().all(|(key, value)| {
                (key != "$ref"
                    || value
                        .as_str()
                        .is_some_and(|reference| reference.starts_with("#/$defs/")))
                    && references_stay_in_defs(value)
            }),
            Value::Array(values) => values.iter().all(references_stay_in_defs),
            _ => true,
        }
    }

    let root = schema.as_object()?;
    if root.get("type") == Some(&json!("object")) {
        return Some(schema.clone());
    }
    if root.keys().any(|key| {
        !matches!(
            key.as_str(),
            "$schema" | "$defs" | "title" | "description" | "anyOf"
        )
    }) || !references_stay_in_defs(schema)
    {
        return None;
    }
    let branches = root.get("anyOf")?.as_array()?;
    let [object, null] = branches.as_slice() else {
        return None;
    };
    let reference = object.as_object()?;
    if reference.len() != 1 {
        return None;
    }
    let pointer = reference.get("$ref")?.as_str()?.strip_prefix('#')?;
    let target = schema.pointer(pointer)?;
    if target.get("type") != Some(&json!("object"))
        || target.get("$ref").is_some()
        || null != &json!({"type": "null"})
    {
        return None;
    }
    let mut result = schema.clone();
    result["type"] = json!("object");
    result["anyOf"][1] = json!({
        "type": "object",
        "properties": {"result": {"type": "null"}},
        "required": ["result"],
        "additionalProperties": false
    });
    Some(result)
}

fn text_result(text: &str, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": is_error })
}

fn list_changed() -> String {
    json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests;
