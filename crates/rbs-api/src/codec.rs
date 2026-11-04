//! The wire codec: JSON-RPC 2.0 over UTF-8 JSON text (ADR-0044 decision 7), behind the
//! [`Codec`] trait so another encoding can be negotiated later.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::{ApiError, ErrorKind};
use crate::event::Event;
use crate::request::{Request, Response, method_scope};

/// A JSON-RPC request id: a number or a string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    /// A number.
    Number(i64),
    /// A string.
    Text(String),
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequestId::Number(n) => write!(f, "{n}"),
            RequestId::Text(s) => write!(f, "{s:?}"),
        }
    }
}

/// A decoded request.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    /// Its id.
    pub id: RequestId,
    /// The request.
    pub request: Request,
}

/// A message that is not a valid request: the error to answer with, and the request id
/// when one could be read (`null` on the wire otherwise).
#[derive(Debug, Clone, PartialEq)]
pub struct Malformed {
    /// The request's id, when it could be read.
    pub id: Option<RequestId>,
    /// Why it is not valid.
    pub error: ApiError,
}

/// Turns messages into requests and answers and events into messages.
pub trait Codec: Send {
    /// Reads one request.
    fn decode(&self, text: &str) -> Result<Incoming, Malformed>;
    /// Writes an answer to request `id` (`None`: the id could not be read).
    fn encode_response(
        &self,
        id: Option<&RequestId>,
        result: &Result<Response, ApiError>,
    ) -> String;
    /// Writes an event notification.
    fn encode_event(&self, event: &Event) -> String;
}

/// JSON-RPC 2.0 with JSON text.
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonCodec;

impl Codec for JsonCodec {
    fn decode(&self, text: &str) -> Result<Incoming, Malformed> {
        let value: Value = serde_json::from_str(text).map_err(|e| Malformed {
            id: None,
            error: ApiError::new(ErrorKind::ParseError, e.to_string()),
        })?;
        let mut object = match value {
            Value::Object(o) => o,
            Value::Array(_) => {
                return Err(malformed(
                    None,
                    ErrorKind::BatchUnsupported,
                    "batch requests are not supported in protocol version 1",
                ));
            }
            _ => return Err(malformed(None, ErrorKind::InvalidRequest, "not an object")),
        };
        let id = match object.get("id") {
            Some(Value::Number(n)) => n.as_i64().map(RequestId::Number),
            Some(Value::String(s)) => Some(RequestId::Text(s.clone())),
            _ => None,
        };
        let Some(id) = id else {
            return Err(malformed(
                None,
                ErrorKind::InvalidRequest,
                "a request needs an integer or string id",
            ));
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(malformed(
                Some(id),
                ErrorKind::InvalidRequest,
                "jsonrpc must be \"2.0\"",
            ));
        }
        let Some(Value::String(method)) = object.remove("method") else {
            return Err(malformed(
                Some(id),
                ErrorKind::InvalidRequest,
                "method must be a string",
            ));
        };
        if method_scope(&method).is_none() {
            return Err(malformed(
                Some(id),
                ErrorKind::MethodNotFound,
                format!("no method {method:?}"),
            ));
        }
        let params = match object.remove("params") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(p @ Value::Object(_)) => p,
            Some(_) => {
                return Err(malformed(
                    Some(id),
                    ErrorKind::InvalidParams,
                    "params must be an object",
                ));
            }
        };
        match serde_json::from_value::<Request>(json!({ "method": method, "params": params })) {
            Ok(request) => Ok(Incoming { id, request }),
            Err(e) => Err(malformed(Some(id), ErrorKind::InvalidParams, e.to_string())),
        }
    }

    fn encode_response(
        &self,
        id: Option<&RequestId>,
        result: &Result<Response, ApiError>,
    ) -> String {
        let id = id.map_or(Value::Null, |id| match id {
            RequestId::Number(n) => Value::from(*n),
            RequestId::Text(s) => Value::from(s.as_str()),
        });
        let body = match result.as_ref().map(serde_json::to_value) {
            Ok(Ok(result)) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Ok(Err(e)) => error_body(
                &id,
                &ApiError::new(ErrorKind::Internal, format!("result not encodable: {e}")),
            ),
            Err(error) => error_body(&id, error),
        };
        body.to_string()
    }

    fn encode_event(&self, event: &Event) -> String {
        match serde_json::to_value(event) {
            Ok(params) => json!({ "jsonrpc": "2.0", "method": "event", "params": params }),
            // Event types only hold strings, numbers and lists, which always encode; a
            // failure here is a bug, reported as a notification rather than lost.
            Err(e) => json!({
                "jsonrpc": "2.0",
                "method": "event",
                "params": { "error": e.to_string() },
            }),
        }
        .to_string()
    }
}

fn error_body(id: &Value, error: &ApiError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": error.kind.code(),
            "message": error.message,
            "data": { "kind": error.kind.name() },
        },
    })
}

fn malformed(id: Option<RequestId>, kind: ErrorKind, message: impl Into<String>) -> Malformed {
    Malformed {
        id,
        error: ApiError::new(kind, message),
    }
}
