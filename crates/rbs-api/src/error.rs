//! Typed errors. `kind` is the stable machine-readable error (`error.data.kind` on the
//! wire); `message` is for logs. Clients treat unknown kinds generically.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The stable error kinds of protocol version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ErrorKind {
    /// The message is not JSON.
    ParseError,
    /// The message is JSON but not a JSON-RPC 2.0 request.
    InvalidRequest,
    /// No method of this name.
    MethodNotFound,
    /// The method's parameters do not match its schema.
    InvalidParams,
    /// A server bug; the message says what.
    Internal,
    /// A JSON array of requests (refused in version 1).
    BatchUnsupported,
    /// None of the client's protocol versions is supported.
    UnsupportedProtocol,
    /// The method needs an authenticated session.
    NotAuthenticated,
    /// The token is unknown or wrong (the same for both).
    AuthFailed,
    /// The method needs a scope the session does not have.
    ScopeDenied,
    /// The session's request queue is full.
    Busy,
    /// The session sends requests faster than its rate limit.
    RateLimited,
    /// The message is larger than allowed.
    MessageTooLarge,
    /// The object named does not exist.
    NotFound,
    /// The request conflicts with the current state (for example a recording runs).
    Conflict,
    /// An undo record was used out of order.
    UndoConflict,
    /// An explicit file already exists; it is never overwritten.
    FileExists,
    /// The request was valid but could not be carried out; the message says why.
    Failed,
}

impl ErrorKind {
    /// Its name on the wire.
    pub const fn name(self) -> &'static str {
        match self {
            ErrorKind::ParseError => "parse_error",
            ErrorKind::InvalidRequest => "invalid_request",
            ErrorKind::MethodNotFound => "method_not_found",
            ErrorKind::InvalidParams => "invalid_params",
            ErrorKind::Internal => "internal",
            ErrorKind::BatchUnsupported => "batch_unsupported",
            ErrorKind::UnsupportedProtocol => "unsupported_protocol",
            ErrorKind::NotAuthenticated => "not_authenticated",
            ErrorKind::AuthFailed => "auth_failed",
            ErrorKind::ScopeDenied => "scope_denied",
            ErrorKind::Busy => "busy",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::MessageTooLarge => "message_too_large",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Conflict => "conflict",
            ErrorKind::UndoConflict => "undo_conflict",
            ErrorKind::FileExists => "file_exists",
            ErrorKind::Failed => "failed",
        }
    }

    /// The JSON-RPC error code: the standard codes for JSON-RPC errors, and fixed
    /// codes in the server range for the rest.
    pub const fn code(self) -> i32 {
        match self {
            ErrorKind::ParseError => -32700,
            ErrorKind::InvalidRequest => -32600,
            ErrorKind::MethodNotFound => -32601,
            ErrorKind::InvalidParams => -32602,
            ErrorKind::Internal => -32603,
            ErrorKind::NotAuthenticated => -32001,
            ErrorKind::AuthFailed => -32002,
            ErrorKind::UnsupportedProtocol => -32003,
            ErrorKind::ScopeDenied => -32010,
            ErrorKind::Busy => -32020,
            ErrorKind::RateLimited => -32021,
            ErrorKind::MessageTooLarge => -32022,
            ErrorKind::BatchUnsupported => -32023,
            ErrorKind::NotFound => -32030,
            ErrorKind::Conflict => -32031,
            ErrorKind::UndoConflict => -32032,
            ErrorKind::FileExists => -32033,
            ErrorKind::Failed => -32040,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// An error answer to a request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// What went wrong, machine-readable.
    pub kind: ErrorKind,
    /// What went wrong, in words for logs.
    pub message: String,
}

impl ApiError {
    /// An error of `kind`.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ApiError {}
