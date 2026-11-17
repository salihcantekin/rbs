//! The control API session: one request at a time over local IPC, and the handshake
//! that narrows it to the token (mcp-server spec, "Process and connection"). It fails
//! closed: nothing is sent after `session.hello` until `session.authenticate` answers.

use std::collections::BTreeSet;
use std::time::Duration;

use rbs_api::request::Effect;
use rbs_api::{Scope, Token};
use rbs_api_ipc::{Endpoint, IpcClient, Received};
use serde_json::{Value, json};

use crate::token::TokenSource;

/// The first protocol minor that narrows a same-user IPC session to a token.
pub const NARROWING_MINOR: u32 = 73;

/// How long a connection attempt may take before it counts as unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Hang guard for one answer: long enough for any control thread answer.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a call failed.
#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    /// The API answered with an error: its `data.kind`, message and `data`.
    Api {
        /// `scope_denied`, `not_permitted`, ...
        kind: String,
        /// The API's message.
        message: String,
        /// The wire's `error.data` (with `kind`, and `reason` where there is one).
        data: Value,
    },
    /// The connection is gone, or the answer was not understood.
    Lost(String),
}

/// One control API connection.
pub trait Api {
    /// Sends `method` with `params` and waits for its answer.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError>;
}

/// Why no session was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// Nothing serves the endpoint: RBS is not running, or AI tools are stopped.
    Unreachable(String),
    /// No usable token (missing or unreadable file, no token in it).
    NoToken(String),
    /// The token was refused: `auth_failed`, or `not_permitted` with `ai_tools_off`.
    Refused(String),
    /// The server's advertised method catalog cannot safely be used.
    InvalidCatalog(String),
    /// The server is older than narrowing: `rbs-mcp` ends (ADR-0052 decision 3).
    TooOld(String),
}

/// A session narrowed to the token.
pub struct Session {
    /// The connection.
    pub api: Box<dyn Api>,
    /// The scopes `session.authenticate` granted.
    pub scopes: Vec<Scope>,
    /// "Allow actions that cannot be undone".
    pub irreversible: bool,
    /// What `api.describe` listed. Schemas are present only for methods whose schemas
    /// were asked for (or that an older server sent unasked).
    pub methods: Vec<Method>,
    /// Methods whose schemas were already requested, so an absent one is not asked again.
    asked: BTreeSet<String>,
}

/// Most names one `api.describe` selection may hold (ADR-0196).
const DESCRIBE_MAX_NAMES: usize = 64;

impl Session {
    /// Fetches the schemas of `names` (methods) that have none yet, in selections the
    /// server accepts, validating each answer like the catalog.
    pub fn load_schemas(&mut self, names: &[String]) -> Result<(), CallError> {
        let missing: Vec<String> = names
            .iter()
            .filter(|n| {
                !self.asked.contains(n.as_str())
                    && self
                        .methods
                        .iter()
                        .any(|m| &m.name == *n && m.input_schema.is_none())
            })
            .cloned()
            .collect();
        for chunk in missing.chunks(DESCRIBE_MAX_NAMES) {
            let described = self.api.call(
                "api.describe",
                json!({ "detail": "schemas", "methods": chunk }),
            )?;
            self.asked.extend(chunk.iter().cloned());
            self.merge(&described)?;
        }
        Ok(())
    }

    /// Fetches the schemas of every method of `namespaces` in one selection per 64
    /// namespaces.
    pub fn load_namespace_schemas(&mut self, namespaces: &[String]) -> Result<(), CallError> {
        for chunk in namespaces.chunks(DESCRIBE_MAX_NAMES) {
            let described = self.api.call(
                "api.describe",
                json!({ "detail": "schemas", "namespaces": chunk }),
            )?;
            self.merge(&described)?;
            for m in &self.methods {
                if chunk
                    .iter()
                    .any(|n| Some(n.as_str()) == m.name.split('.').next())
                {
                    self.asked.insert(m.name.clone());
                }
            }
        }
        Ok(())
    }

    /// Validates a described answer and copies its schemas to the methods that have none.
    fn merge(&mut self, described: &Value) -> Result<(), CallError> {
        let got = methods(described).map_err(|e| CallError::Api {
            kind: "invalid_catalog".into(),
            message: match e {
                ConnectError::InvalidCatalog(w) => w,
                _ => "RBS sent an invalid method catalog".into(),
            },
            data: Value::Null,
        })?;
        for new in got {
            if let Some(old) = self.methods.iter_mut().find(|m| m.name == new.name) {
                if old.input_schema.is_none() {
                    old.input_schema = new.input_schema;
                    old.output_schema = new.output_schema;
                }
            }
        }
        Ok(())
    }
}

/// One method `api.describe` listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    /// `namespace.verb`.
    pub name: String,
    /// The scope it needs; `None` for the handshake or a scope this build does not know.
    pub scope: Option<Scope>,
    /// What it does; `None` when missing or unknown (handled as `destructive`).
    pub effect: Option<Effect>,
    /// Schema of request parameters supplied by the server, when available.
    pub input_schema: Option<Value>,
    /// Schema of a successful result supplied by the server, when available.
    pub output_schema: Option<Value>,
}

fn supported_schema(schema: &Value) -> bool {
    fn references_are_local(value: &Value, root: &Value, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match value {
            Value::Object(object) => object.iter().all(|(key, value)| {
                (match key.as_str() {
                    "$id" | "$dynamicRef" | "$recursiveRef" => false,
                    "$ref" => value
                        .as_str()
                        .and_then(|reference| reference.strip_prefix('#'))
                        .is_some_and(|pointer| {
                            (pointer.is_empty() || pointer.starts_with('/'))
                                && root.pointer(pointer).is_some_and(|target| {
                                    (target.is_object() || target.is_boolean())
                                        && jsonschema::meta::is_valid(target)
                                })
                        }),
                    _ => true,
                }) && references_are_local(value, root, depth + 1)
            }),
            Value::Array(values) => values
                .iter()
                .all(|value| references_are_local(value, root, depth + 1)),
            _ => true,
        }
    }
    fn acyclic(
        value: &Value,
        root: &Value,
        active: &mut BTreeSet<usize>,
        done: &mut BTreeSet<usize>,
        depth: usize,
    ) -> bool {
        if depth > 64 {
            return false;
        }
        let id = std::ptr::from_ref(value) as usize;
        if done.contains(&id) {
            return true;
        }
        if !active.insert(id) {
            return false;
        }
        let valid = match value {
            Value::Object(object) => {
                let reference_ok = object.get("$ref").is_none_or(|reference| {
                    reference
                        .as_str()
                        .and_then(|reference| reference.strip_prefix('#'))
                        .and_then(|pointer| root.pointer(pointer))
                        .is_some_and(|target| acyclic(target, root, active, done, depth + 1))
                });
                reference_ok
                    && object
                        .values()
                        .all(|child| acyclic(child, root, active, done, depth + 1))
            }
            Value::Array(values) => values
                .iter()
                .all(|child| acyclic(child, root, active, done, depth + 1)),
            _ => true,
        };
        active.remove(&id);
        if valid {
            done.insert(id);
        }
        valid
    }
    serde_json::to_vec(schema).is_ok_and(|bytes| bytes.len() <= 65_536)
        && references_are_local(schema, schema, 0)
        && acyclic(
            schema,
            schema,
            &mut BTreeSet::new(),
            &mut BTreeSet::new(),
            0,
        )
        && jsonschema::meta::is_valid(schema)
}

/// Validate tool names before accepting the catalog. Unknown scope and effect values
/// remain unknown and are handled conservatively when generating tools.
fn methods(described: &Value) -> Result<Vec<Method>, ConnectError> {
    let Some(list) = described.get("methods").and_then(Value::as_array) else {
        return Err(ConnectError::InvalidCatalog(
            "RBS sent an invalid method catalog".into(),
        ));
    };
    let mut names = BTreeSet::from(["studio_summary".to_owned()]);
    let mut methods = Vec::with_capacity(list.len());
    for m in list {
        let Some(name) = m.get("name").and_then(Value::as_str) else {
            return Err(ConnectError::InvalidCatalog(
                "RBS sent an invalid method catalog".into(),
            ));
        };
        let tool = crate::tool_name(name);
        if !name.contains('.')
            || name.split('.').any(str::is_empty)
            || tool.len() > 64
            || !tool
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(ConnectError::InvalidCatalog(
                "RBS sent a method with an invalid tool name".into(),
            ));
        }
        if !names.insert(tool) {
            return Err(ConnectError::InvalidCatalog(
                "RBS sent conflicting method tool names".into(),
            ));
        }
        let scope = m
            .get("scope")
            .cloned()
            .and_then(|s| serde_json::from_value(s).ok());
        let effect = m
            .get("effect")
            .cloned()
            .and_then(|e| serde_json::from_value(e).ok());
        let input_schema = m.get("input_schema").filter(|v| !v.is_null()).cloned();
        let output_schema = m.get("output_schema").filter(|v| !v.is_null()).cloned();
        match (&input_schema, &output_schema) {
            (None, None) => {}
            (Some(input), Some(output))
                if input.get("type") == Some(&json!("object"))
                    && output.is_object()
                    && supported_schema(input)
                    && supported_schema(output) => {}
            _ => {
                return Err(ConnectError::InvalidCatalog(
                    "RBS sent invalid method schema metadata".into(),
                ));
            }
        }
        methods.push(Method {
            name: name.to_owned(),
            scope,
            effect,
            input_schema,
            output_schema,
        });
    }
    Ok(methods)
}

/// Makes sessions (the real one over IPC, a fake in tests).
pub trait Connect {
    /// A new session, with the token as it is now.
    fn connect(&mut self) -> Result<Session, ConnectError>;
}

/// Hello, authenticate with `token`, then `api.describe`.
pub fn handshake(mut api: Box<dyn Api>, token: &Token) -> Result<Session, ConnectError> {
    let lost = |e: CallError| match e {
        CallError::Lost(why) => ConnectError::Unreachable(why),
        CallError::Api { message, .. } => ConnectError::Refused(message),
    };
    let hello = api
        .call(
            "session.hello",
            json!({
                "protocols": ["1.0"],
                "client": { "name": "rbs-mcp", "version": env!("CARGO_PKG_VERSION") }
            }),
        )
        .map_err(lost)?;
    let minor = hello
        .get("protocol")
        .and_then(Value::as_str)
        .and_then(|p| p.strip_prefix("1."))
        .and_then(|m| m.parse::<u32>().ok())
        .unwrap_or(0);
    if minor < NARROWING_MINOR {
        return Err(ConnectError::TooOld(format!(
            "this RBS speaks protocol 1.{minor}; rbs-mcp needs 1.{NARROWING_MINOR} or newer. Update RBS."
        )));
    }
    let auth = match api.call("session.authenticate", json!({ "token": token.expose() })) {
        Ok(a) => a,
        Err(CallError::Api { kind, data, .. }) => {
            return Err(match kind.as_str() {
                "invalid_request" => ConnectError::TooOld(
                    "this RBS cannot narrow a local session to a token. Update RBS.".into(),
                ),
                "not_permitted" if data.get("reason") == Some(&json!("ai_tools_off")) => {
                    ConnectError::Refused("AI tools are stopped in RBS".into())
                }
                "auth_failed" => ConnectError::Refused("RBS did not accept the token".into()),
                other => ConnectError::Refused(format!("RBS refused the session ({other})")),
            });
        }
        Err(CallError::Lost(why)) => return Err(ConnectError::Unreachable(why)),
    };
    let scopes: Vec<Scope> = auth
        .get("scopes")
        .cloned()
        .and_then(|s| serde_json::from_value(s).ok())
        .ok_or_else(|| ConnectError::Refused("RBS sent no scopes".into()))?;
    let irreversible = auth
        .get("irreversible")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let described = api.call("api.describe", json!({})).map_err(lost)?;
    let methods = methods(&described)?;
    Ok(Session {
        api,
        scopes,
        irreversible,
        methods,
        asked: BTreeSet::new(),
    })
}

/// Whether a method of `effect` may run without "Allow actions that cannot be undone".
/// A missing effect is handled as `destructive`.
pub fn needs_permission(effect: Option<Effect>) -> bool {
    !matches!(effect, Some(Effect::Read | Effect::Edit))
}

/// The real session: local IPC to the RBS of this user.
pub struct IpcConnector {
    /// Where the token comes from.
    pub token: TokenSource,
    /// The endpoint; `None` uses the current user's.
    pub endpoint: Option<Endpoint>,
}

impl Connect for IpcConnector {
    fn connect(&mut self) -> Result<Session, ConnectError> {
        let token = self.token.read().map_err(ConnectError::NoToken)?;
        let endpoint = match &self.endpoint {
            Some(e) => e.clone(),
            None => Endpoint::for_current_user().map_err(ConnectError::Unreachable)?,
        };
        let client = IpcClient::connect(&endpoint, CONNECT_TIMEOUT)
            .map_err(|e| ConnectError::Unreachable(format!("{e}; looked at {endpoint}")))?;
        handshake(Box::new(IpcApi { client, next: 1 }), &token)
    }
}

struct IpcApi {
    client: IpcClient,
    next: u64,
}

impl Api for IpcApi {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next;
        self.next += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.client
            .send(&request.to_string())
            .map_err(|e| CallError::Lost(e.kind().to_string()))?;
        loop {
            let message = match self.client.recv(ANSWER_TIMEOUT) {
                Ok(Some(Received::Message(m))) => m,
                Ok(Some(Received::Closed(reason))) => {
                    return Err(CallError::Lost(format!(
                        "RBS closed the connection ({reason:?})"
                    )));
                }
                Ok(None) => return Err(CallError::Lost("RBS did not answer".into())),
                Err(e) => return Err(CallError::Lost(e.kind().to_string())),
            };
            let Ok(value) = serde_json::from_str::<Value>(&message) else {
                return Err(CallError::Lost(
                    "RBS sent something that is not JSON".into(),
                ));
            };
            // Events and answers to other ids are not ours.
            if value.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let data = error.get("data").cloned().unwrap_or(Value::Null);
                return Err(CallError::Api {
                    kind: data
                        .get("kind")
                        .and_then(Value::as_str)
                        .unwrap_or("internal")
                        .to_owned(),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    data,
                });
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}
