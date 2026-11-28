//! Client-scoped command routing (ADR-0050). UI clients resolve commands; engine
//! operations return to the originating API session for fresh authorization.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::request::{Command, Response, UI_METHODS, method_scope};
use crate::{ApiError, Caller, ErrorKind, Scope};

#[cfg(test)]
mod tests;

/// A request for a UI client, separate from the engine command catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiRequest {
    /// Read the targeted client's command registry.
    List {
        /// Absent: the most recently focused registered desktop client.
        client: Option<String>,
    },
    /// Run an entry, including its encoded parameters.
    Run {
        /// Registry id.
        id: String,
        /// Absent: the most recently focused registered desktop client.
        client: Option<String>,
    },
}

impl UiRequest {
    /// Its client-scoped method name.
    pub fn method(&self) -> &'static str {
        match self {
            Self::List { .. } => "ui.command.list",
            Self::Run { .. } => "ui.command.run",
        }
    }

    pub(crate) fn client(&self) -> Option<&str> {
        match self {
            Self::List { client } | Self::Run { client, .. } => client.as_deref(),
        }
    }
}

/// `ui.command.list` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiList {
    /// Target UI client; absent means the last focused client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// `ui.command.run` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiRun {
    /// Registry id with encoded parameters.
    pub id: String,
    /// Target UI client; absent means the last focused client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// The authority a UI may inspect, but cannot widen or reuse after cancellation.
#[derive(Debug, Clone)]
pub struct UiTicket {
    pub(crate) serial: u64,
    pub(crate) conn: crate::transport::ConnectionId,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) cancellation: Arc<Cancellation>,
}

#[derive(Default)]
pub(crate) struct Cancellation(Mutex<Option<Box<dyn FnOnce() + Send>>>);

impl std::fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cancellation")
    }
}

impl UiTicket {
    /// Check immediately before changing UI state or opening a picker.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn cancel(&self) {
        self.active.store(false, Ordering::Release);
        let callback = self
            .cancellation
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(callback) = callback {
            callback();
        }
    }

    /// Install one non-blocking cancellation observer. Called outside the lock,
    /// including immediately when cancellation preceded registration.
    pub fn observe_cancellation(
        &self,
        callback: impl FnOnce() + Send + 'static,
    ) -> Result<(), ApiError> {
        let mut slot = self
            .cancellation
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !self.is_active() {
            drop(slot);
            callback();
        } else if slot.is_some() {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "a cancellation observer is already installed",
            ));
        } else {
            *slot = Some(Box::new(callback));
        }
        Ok(())
    }
}

impl PartialEq for UiTicket {
    fn eq(&self, other: &Self) -> bool {
        self.serial == other.serial
            && self.conn == other.conn
            && Arc::ptr_eq(&self.active, &other.active)
    }
}

/// A UI request sent asynchronously to exactly one registered client.
#[derive(Debug, Clone, PartialEq)]
pub struct UiForwarded {
    /// Cancellation-safe, single-use routing ticket.
    pub ticket: UiTicket,
    /// Effective caller permissions, never the UI client's permissions.
    pub caller: Caller,
    /// The request.
    pub request: UiRequest,
}

/// The UI either answers a UI-only operation or resolves an engine operation.
#[derive(Debug, Clone, PartialEq)]
pub enum UiResolution {
    /// No engine work: a registry listing, UI-only result or refusal.
    Complete(Result<Response, ApiError>),
    /// Must be admitted by the originating session, not dispatched by the UI.
    Engine(Command),
    /// An ordered UI gesture. Every operation is checked before any is forwarded
    /// and again against the live session before its turn.
    Batch(Vec<Command>),
}

/// A resolution carrying the unforgeable ticket received by the UI.
#[derive(Debug, Clone, PartialEq)]
pub struct UiResolved {
    pub(crate) ticket: UiTicket,
    pub(crate) resolution: UiResolution,
}

impl UiForwarded {
    /// Return this request's resolution to the API thread.
    pub fn resolve(&self, resolution: UiResolution) -> UiResolved {
        UiResolved {
            ticket: self.ticket.clone(),
            resolution,
        }
    }
}

/// Check a resolved method against the caller, including client-scoped methods.
/// Unknown methods fail closed rather than inheriting `control`.
pub fn authorize_method(caller: &Caller, method: &str) -> Result<(), ApiError> {
    let scope = method_scope(method).or_else(|| {
        UI_METHODS.contains(&method).then_some(Some(
            if method.starts_with("ui.profile.") || method.starts_with("ui.guest.") {
                Scope::Admin
            } else {
                Scope::Control
            },
        ))
    });
    let Some(scope) = scope else {
        return Err(ApiError::new(ErrorKind::MethodNotFound, "unknown method"));
    };
    if let Some(scope) = scope
        && !caller.scopes.contains(scope)
    {
        return Err(ApiError::new(
            ErrorKind::ScopeDenied,
            format!("needs scope {scope}"),
        ));
    }
    Ok(())
}

/// The same parameter-dependent admission checks used for external engine calls.
pub fn authorize_engine(caller: &Caller, command: &Command) -> Result<(), ApiError> {
    authorize_method(caller, command.method())?;
    let method = command.method();
    let refused = if command.effect().is_irreversible() && !caller.irreversible {
        Some((
            "irreversible",
            format!(
                "{method} cannot be undone, and this token is not allowed actions that cannot be undone"
            ),
        ))
    } else if let Command::TokensCreate(p) = command
        && p.scopes.iter().any(|scope| !caller.scopes.contains(scope))
    {
        Some((
            "scopes",
            format!("{method} cannot give scopes this session does not hold"),
        ))
    } else if !caller.irreversible && command.reaches_network() {
        Some((
            "network",
            format!(
                "{method} could connect to another computer, and this token is not allowed actions that cannot be undone"
            ),
        ))
    } else {
        None
    };
    if let Some((reason, message)) = refused {
        let mut data = serde_json::Map::new();
        data.insert("reason".into(), reason.into());
        return Err(ApiError::with_data(ErrorKind::NotPermitted, message, data));
    }
    Ok(())
}

/// Check an operation performed by the UI itself before executing it.
pub fn authorize_ui(caller: &Caller, method: &str) -> Result<(), ApiError> {
    authorize_method(caller, method)?;
    if !UI_METHODS.contains(&method) {
        return Err(ApiError::new(
            ErrorKind::MethodNotFound,
            "engine methods must return to the API session",
        ));
    }
    let destructive = method == "ui.profile.delete" || method == "ui.collection.delete";
    if destructive && !caller.irreversible {
        let mut data = serde_json::Map::new();
        data.insert("reason".into(), "irreversible".into());
        return Err(ApiError::with_data(
            ErrorKind::NotPermitted,
            "this action cannot be undone",
            data,
        ));
    }
    Ok(())
}

/// One registry row. Disabled entries include a reason for this caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiCommandInfo {
    /// Registry id, not an engine method.
    pub id: String,
    /// Client's displayed title.
    pub title: String,
    /// `action`, `picker` or `toggle`.
    pub kind: String,
    /// Whether this caller can run it in the current client state.
    pub enabled: bool,
    /// Disabled reason.
    pub reason: Option<String>,
    /// Toggle state, if applicable.
    pub checked: Option<bool>,
}

type Sink = Arc<dyn Fn(UiForwarded, UiResponder) + Send + Sync>;

struct Client {
    name: String,
    focused: u64,
    sink: Sink,
    pending: Vec<(UiForwarded, UiResponder)>,
}

#[derive(Default)]
struct Clients {
    entries: BTreeMap<String, Client>,
    focus: u64,
}

/// A UI's non-blocking reply path. Repeated or cancelled answers are ignored.
#[derive(Clone)]
pub struct UiResponder {
    pub(crate) send: Arc<dyn Fn(UiResolved) + Send + Sync>,
    pub(crate) sent: Arc<AtomicBool>,
}

impl UiResponder {
    /// Send a resolution without waiting for the API or control thread.
    pub fn send(&self, resolution: UiResolved) {
        if !self.sent.swap(true, Ordering::AcqRel) {
            (self.send)(resolution);
        }
    }
}

/// Registration and focused-client routing. No extra thread or polling.
#[derive(Clone, Default)]
pub struct UiRouter(Arc<Mutex<Clients>>);

impl UiRouter {
    /// Register one desktop client. Duplicate ids are refused, not replaced.
    pub fn register(
        &self,
        id: String,
        name: String,
        sink: impl Fn(UiForwarded, UiResponder) + Send + Sync + 'static,
    ) -> Result<(), ApiError> {
        if id.is_empty() || id.len() > 128 || name.is_empty() || name.len() > 128 {
            return Err(ApiError::new(
                ErrorKind::InvalidParams,
                "invalid UI client id or name",
            ));
        }
        let mut clients = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if clients.entries.contains_key(&id) {
            return Err(ApiError::new(
                ErrorKind::Conflict,
                "UI client already registered",
            ));
        }
        if clients.entries.len() >= 16 {
            return Err(ApiError::new(ErrorKind::Busy, "UI client limit reached"));
        }
        clients.entries.insert(
            id,
            Client {
                name,
                focused: 0,
                sink: Arc::new(sink),
                pending: Vec::new(),
            },
        );
        Ok(())
    }

    /// A focus event, not a request, chooses the default destination.
    pub fn focus(&self, id: &str) -> Result<(), ApiError> {
        let mut clients = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !clients.entries.contains_key(id) {
            return Err(ApiError::new(
                ErrorKind::NotFound,
                "UI client is not registered",
            ));
        }
        clients.focus = clients.focus.saturating_add(1);
        let focused = clients.focus;
        if let Some(client) = clients.entries.get_mut(id) {
            client.focused = focused;
        }
        Ok(())
    }

    /// Remove a closed client. A targeted request never falls back to another one.
    pub fn unregister(&self, id: &str) {
        let client = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .remove(id);
        if let Some(client) = client {
            for (call, reply) in client.pending {
                call.ticket.cancel();
                reply.send(call.resolve(UiResolution::Complete(Err(ApiError::new(
                    ErrorKind::Conflict,
                    "target UI client closed",
                )))));
            }
        }
    }

    /// Registered ids and display names, without UI state.
    pub fn clients(&self) -> Vec<(String, String)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .iter()
            .map(|(id, client)| (id.clone(), client.name.clone()))
            .collect()
    }

    pub(crate) fn route(&self, call: UiForwarded, reply: UiResponder) {
        if !call.ticket.is_active() {
            return;
        }
        let mut full = false;
        let sink = {
            let mut clients = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let id = match call.request.client() {
                Some(id) => Some(id.to_owned()),
                None => clients
                    .entries
                    .iter()
                    .filter(|(_, client)| client.focused > 0)
                    .max_by_key(|(_, client)| client.focused)
                    .map(|(id, _)| id.clone()),
            };
            id.and_then(|id| clients.entries.get_mut(&id))
                .and_then(|client| {
                    client.pending.retain(|(call, reply)| {
                        call.ticket.is_active() || !reply.sent.load(Ordering::Acquire)
                    });
                    if client.pending.len() >= 128 {
                        full = true;
                        return None;
                    }
                    client.pending.push((call.clone(), reply.clone()));
                    Some(Arc::clone(&client.sink))
                })
        };
        match sink {
            Some(sink) if call.ticket.is_active() => sink(call, reply),
            Some(_) => reply.send(call.resolve(UiResolution::Complete(Err(ApiError::new(
                ErrorKind::Conflict,
                "the UI request was cancelled",
            ))))),
            None => reply.send(call.resolve(UiResolution::Complete(Err(if full {
                ApiError::new(ErrorKind::Busy, "target UI client queue is full")
            } else {
                ApiError::new(ErrorKind::NotFound, "target UI client is not available")
            })))),
        }
    }
}
