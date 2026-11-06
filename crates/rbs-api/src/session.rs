//! The session state machine (ADR-0044 decisions 5, 9-13): handshake, authentication,
//! scopes, limits, per-session queues, subscriptions and event coalescing. It is pure:
//! [`Server::handle`] and [`Server::advance`] take the time as an argument and
//! [`Server::next_deadline`] says when to call again, so tests drive it with a
//! `TestClock` and the API thread only wraps it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use rbs_core::time::Timestamp;

use crate::codec::{Codec, JsonCodec, RequestId};
use crate::control::{Forwarded, Reply};
use crate::error::{ApiError, ErrorKind};
use crate::event::{Event, EventPayload, StateKey, Topic, TopicSet};
use crate::request::{
    AuthResult, ClientInfo, Command, Description, Empty, HelloResult, METHODS, MethodInfoOwned,
    Request, Response, topic_scope,
};
use crate::scope::{Scope, ScopeSet};
use crate::token::{TokenHash, TokenStore};
use crate::transport::{
    CloseReason, ConnectionBudget, ConnectionId, ConnectionSender, MessageLimit, PeerInfo,
    SendError, TransportEvent,
};

/// The protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "1.3";

/// Caps and timeouts (ADR-0044 decision 12). The defaults are the version 1 values in
/// the control API spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Connections at once, including closed ones whose request is still in flight.
    pub max_connections: usize,
    /// Of `max_connections`, how many only loopback and IPC peers can take.
    pub local_reserved_connections: usize,
    /// Remote connections not yet authenticated.
    pub max_pending_handshakes: usize,
    /// Connections per peer address (not applied on loopback or IPC).
    pub max_connections_per_address: usize,
    /// Time from connect to authenticated.
    pub handshake_timeout: Duration,
    /// Largest message before authentication.
    pub pre_auth_max_bytes: usize,
    /// Largest message after authentication.
    pub max_message_bytes: usize,
    /// Requests per second, sustained.
    pub request_rate: u32,
    /// Requests in a burst.
    pub request_burst: u32,
    /// Requests waiting behind the one in flight (`busy` beyond).
    pub queue_len: usize,
    /// Answers waiting while the connection does not take messages; beyond, the
    /// connection is closed as too slow.
    pub max_pending_answers: usize,
    /// Unread messages per connection in the transport inbox.
    pub inbox_quota: usize,
    /// Delay after the first failed authentication from an address; doubles per
    /// failure.
    pub backoff_base: Duration,
    /// Longest delay.
    pub backoff_max: Duration,
    /// Addresses remembered for back-off.
    pub backoff_addresses: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 64,
            local_reserved_connections: 8,
            max_pending_handshakes: 16,
            max_connections_per_address: 8,
            handshake_timeout: Duration::from_secs(10),
            pre_auth_max_bytes: 4 * 1024,
            max_message_bytes: 1024 * 1024,
            request_rate: 50,
            request_burst: 100,
            queue_len: 16,
            max_pending_answers: 64,
            inbox_quota: 8,
            backoff_base: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            backoff_addresses: 1024,
        }
    }
}

/// How the server runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    /// Caps and timeouts.
    pub limits: Limits,
    /// The visible "Allow remote admin" setting: remote tokens keep `admin`.
    pub allow_remote_admin: bool,
    /// The server's name and version, sent after authentication.
    pub server: ClientInfo,
    /// A random id of this server run; `seq` counters restart with it.
    pub instance: String,
}

impl ServerConfig {
    /// Default limits, remote admin off, and a random instance id.
    pub fn new(server: ClientInfo) -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        let instance = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Self {
            limits: Limits::default(),
            allow_remote_admin: false,
            server,
            instance,
        })
    }
}

/// Something the server logs: connections, authentication and privileged calls, with
/// the token name (ADR-0044, "Exposure and limits"). Never contains a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditEvent {
    /// A connection was accepted.
    Connected {
        /// The connection.
        conn: ConnectionId,
        /// Who it is.
        peer: PeerInfo,
    },
    /// A connection was authenticated.
    Authenticated {
        /// The connection.
        conn: ConnectionId,
        /// The token's name; `None` for a same-user IPC peer.
        token: Option<String>,
        /// What it may do.
        scopes: ScopeSet,
    },
    /// An authentication failed.
    AuthFailed {
        /// The connection.
        conn: ConnectionId,
        /// The peer's address, when it has one.
        address: Option<String>,
    },
    /// A call with `admin`, `plugins` or `tokens`.
    Privileged {
        /// The connection.
        conn: ConnectionId,
        /// The token's name; `None` for a same-user IPC peer.
        token: Option<String>,
        /// The method.
        method: &'static str,
        /// The explicit file path, for `record.start_to_file`.
        detail: Option<String>,
    },
    /// The server closed a connection.
    Closed {
        /// The connection.
        conn: ConnectionId,
        /// Why.
        reason: CloseReason,
    },
    /// A connection went away.
    Disconnected {
        /// The connection.
        conn: ConnectionId,
    },
}

/// What the API thread does for the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send this command to the control thread (and wake it).
    Forward(Forwarded),
    /// Store this subscriber mask where the control thread reads it. Comes before the
    /// `Forward` of the subscribe that needs it.
    SetMask(TopicSet),
    /// Log this.
    Audit(AuditEvent),
}

/// Reads a latest-value topic (`audio.levels`) from read-only, lock-free handles the
/// composition root provides. Runs on the API thread; never wakes the control thread.
pub trait Sampler: Send {
    /// The topic's current value, or `None` when there is nothing to send.
    fn sample(&mut self, topic: Topic) -> Option<EventPayload>;
}

/// Server input.
#[derive(Debug)]
pub enum Input {
    /// From a transport.
    Transport(TransportEvent),
    /// From the control thread.
    Reply(Reply),
    /// From the event slots.
    Events(Vec<Event>),
}

#[derive(Debug, Clone)]
enum Phase {
    AwaitHello,
    AwaitAuth,
    Ready {
        scopes: ScopeSet,
        /// The token's name, hash and stored scopes (before channel limits).
        token: Option<(String, TokenHash, ScopeSet)>,
    },
    Closed,
}

enum Queued {
    Answer(RequestId, Result<Response, ApiError>),
    Local(RequestId, Request),
    Forward(RequestId, Command),
}

struct PendingAuth {
    id: RequestId,
    token: String,
    not_before: Timestamp,
}

/// Token bucket in millionths of a request.
struct Bucket {
    level: u64,
    at: Timestamp,
}

#[derive(Default)]
struct Outgoing {
    blocked: bool,
    answers: VecDeque<Arc<str>>,
    events: BTreeMap<StateKey, Arc<str>>,
}

struct Sampling {
    interval: Duration,
    next: Timestamp,
}

struct Session {
    peer: PeerInfo,
    /// Back-off and per-address key: the address, or its /64 for IPv6.
    address_key: Option<String>,
    sender: Box<dyn ConnectionSender>,
    limit: MessageLimit,
    phase: Phase,
    handshake_deadline: Timestamp,
    transport_closed: bool,
    pending_auth: Option<PendingAuth>,
    queue: VecDeque<Queued>,
    in_flight: bool,
    bucket: Bucket,
    out: Outgoing,
    topics: TopicSet,
    sampled: BTreeMap<Topic, Sampling>,
}

impl Session {
    fn is_handshaking(&self) -> bool {
        matches!(self.phase, Phase::AwaitHello | Phase::AwaitAuth)
    }

    fn token_name(&self) -> Option<String> {
        match &self.phase {
            Phase::Ready { token, .. } => token.as_ref().map(|(n, _, _)| n.clone()),
            _ => None,
        }
    }
}

struct Backoff {
    failures: u32,
    until: Timestamp,
}

const MICRO: u64 = 1_000_000;

/// The control API server's sessions. One per API thread.
pub struct Server {
    config: ServerConfig,
    codec: Box<dyn Codec>,
    tokens: TokenStore,
    sessions: BTreeMap<ConnectionId, Session>,
    budget: Arc<ConnectionBudget>,
    backoff: HashMap<String, Backoff>,
    mask: TopicSet,
    sample_seq: u64,
    sampler: Option<Box<dyn Sampler>>,
    actions: VecDeque<Action>,
}

impl Server {
    /// A server with the JSON codec and no sampler.
    pub fn new(config: ServerConfig, tokens: TokenStore) -> Self {
        let budget = Arc::new(ConnectionBudget::with_local_reserve(
            config.limits.max_connections,
            config.limits.local_reserved_connections,
        ));
        Self {
            config,
            codec: Box::new(JsonCodec),
            tokens,
            sessions: BTreeMap::new(),
            budget,
            backoff: HashMap::new(),
            mask: TopicSet::EMPTY,
            sample_seq: 0,
            sampler: None,
            actions: VecDeque::new(),
        }
    }

    /// Samples latest-value topics with `sampler`.
    pub fn with_sampler(mut self, sampler: Box<dyn Sampler>) -> Self {
        self.sampler = Some(sampler);
        self
    }

    /// The connection budget to give the transport inbox.
    pub fn budget(&self) -> Arc<ConnectionBudget> {
        self.budget.clone()
    }

    /// The configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Sessions the server still holds (open, or closed with a request in flight).
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The union of state topics API sessions subscribe to.
    pub fn mask(&self) -> TopicSet {
        self.mask
    }

    /// The next action for the API thread.
    pub fn next_action(&mut self) -> Option<Action> {
        self.actions.pop_front()
    }

    /// Replaces the token store. Sessions of tokens no longer in it are closed.
    pub fn set_tokens(&mut self, tokens: TokenStore) {
        self.tokens = tokens;
        let revoked: Vec<ConnectionId> = self
            .sessions
            .iter()
            .filter(|(_, s)| match &s.phase {
                // Revoked, renamed or re-scoped: the session ends; the client
                // reconnects with what the token allows now.
                Phase::Ready {
                    token: Some((name, hash, scopes)),
                    ..
                } => !self
                    .tokens
                    .entries()
                    .iter()
                    .any(|e| e.hash == *hash && e.name == *name && e.scopes == *scopes),
                _ => false,
            })
            .map(|(c, _)| *c)
            .collect();
        for conn in revoked {
            self.close(conn, CloseReason::Revoked);
        }
    }

    /// Closes every connection (the server stops).
    pub fn shutdown(&mut self) {
        let conns: Vec<ConnectionId> = self.sessions.keys().copied().collect();
        for conn in conns {
            self.close(conn, CloseReason::ServerStopping);
        }
    }

    /// When [`Server::advance`] must run next: the earliest handshake timeout, delayed
    /// authentication or sampling time; `None` when nothing is due (no wake-up).
    pub fn next_deadline(&self) -> Option<Timestamp> {
        let mut next: Option<Timestamp> = None;
        let mut take = |t: Timestamp| next = Some(next.map_or(t, |n| n.min(t)));
        for s in self.sessions.values() {
            if s.is_handshaking() {
                take(s.handshake_deadline);
            }
            if let Some(p) = &s.pending_auth {
                take(p.not_before);
            }
            if matches!(s.phase, Phase::Ready { .. }) && self.sampler.is_some() {
                for sampling in s.sampled.values() {
                    take(sampling.next);
                }
            }
        }
        next
    }

    /// Handles one input at time `now`.
    pub fn handle(&mut self, input: Input, now: Timestamp) {
        match input {
            Input::Transport(TransportEvent::Connected {
                id,
                peer,
                sender,
                limit,
            }) => {
                self.connected(id, peer, sender, limit, now);
            }
            Input::Transport(TransportEvent::Message { id, text }) => self.message(id, &text, now),
            Input::Transport(TransportEvent::Oversized { id, .. }) => self.oversized(id),
            Input::Transport(TransportEvent::Writable { id }) => {
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.out.blocked = false;
                }
                self.flush(id);
            }
            Input::Transport(TransportEvent::Closed { id }) => self.transport_closed(id),
            Input::Reply(reply) => self.reply(reply, now),
            Input::Events(events) => self.events(&events),
        }
    }

    /// Runs what is due at `now`: handshake timeouts, delayed authentications,
    /// sampling.
    pub fn advance(&mut self, now: Timestamp) {
        let conns: Vec<ConnectionId> = self.sessions.keys().copied().collect();
        for conn in conns {
            let Some(s) = self.sessions.get_mut(&conn) else {
                continue;
            };
            if s.is_handshaking() && now >= s.handshake_deadline {
                self.close(conn, CloseReason::HandshakeTimeout);
                continue;
            }
            if s.pending_auth.as_ref().is_some_and(|p| now >= p.not_before) {
                // Another attempt from the address may have failed meanwhile: then this
                // one waits for the new delay, so parallel attempts gain nothing.
                let until = s
                    .address_key
                    .as_ref()
                    .and_then(|a| self.backoff.get(a))
                    .map(|b| b.until);
                match until {
                    Some(until) if until > now => {
                        if let Some(p) = s.pending_auth.as_mut() {
                            p.not_before = until;
                        }
                    }
                    _ => {
                        if let Some(p) = s.pending_auth.take() {
                            self.authenticate(conn, p.id, &p.token, now);
                        }
                    }
                }
            }
        }
        self.sample(now);
    }

    fn connected(
        &mut self,
        conn: ConnectionId,
        peer: PeerInfo,
        sender: Box<dyn ConnectionSender>,
        limit: MessageLimit,
        now: Timestamp,
    ) {
        let limits = &self.config.limits;
        let address_key = peer
            .address
            .as_deref()
            .filter(|_| !peer.is_local())
            .map(address_key);
        // The pending-handshake cap counts and refuses remote peers only, so remote
        // clients cannot lock out loopback and IPC clients (decision 12). Local peers
        // are bounded by the connection budget, which remote peers cannot fill without
        // a token: at most this many of its slots are unauthenticated remote ones.
        let remote_pending = self
            .sessions
            .values()
            .filter(|s| s.is_handshaking() && !s.peer.is_local())
            .count();
        let same_address = match &address_key {
            Some(key) => self
                .sessions
                .values()
                .filter(|s| !matches!(s.phase, Phase::Closed))
                .filter(|s| s.address_key.as_ref() == Some(key))
                .count(),
            None => 0,
        };
        let refuse = !peer.is_local()
            && (remote_pending >= limits.max_pending_handshakes
                || same_address >= limits.max_connections_per_address);
        self.actions.push_back(Action::Audit(AuditEvent::Connected {
            conn,
            peer: peer.clone(),
        }));
        self.sessions.insert(
            conn,
            Session {
                peer,
                address_key,
                sender,
                limit,
                phase: Phase::AwaitHello,
                handshake_deadline: now + limits.handshake_timeout,
                transport_closed: false,
                pending_auth: None,
                queue: VecDeque::new(),
                in_flight: false,
                bucket: Bucket {
                    level: u64::from(limits.request_burst) * MICRO,
                    at: now,
                },
                out: Outgoing::default(),
                topics: TopicSet::EMPTY,
                sampled: BTreeMap::new(),
            },
        );
        if refuse {
            self.close(conn, CloseReason::TooManyConnections);
        }
    }

    fn message(&mut self, conn: ConnectionId, text: &str, now: Timestamp) {
        let limits = self.config.limits.clone();
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        let handshaking = s.is_handshaking();
        match s.phase {
            Phase::Closed => return,
            _ if handshaking && text.len() > limits.pre_auth_max_bytes => {
                self.close(conn, CloseReason::MessageTooLarge);
                return;
            }
            _ if text.len() > limits.max_message_bytes => {
                let e = ApiError::new(ErrorKind::MessageTooLarge, "message too large");
                self.send_now(conn, None, Err(e));
                return;
            }
            _ => {}
        }
        let decoded = self.codec.decode(text);
        let id = match &decoded {
            Ok(i) => Some(i.id.clone()),
            Err(m) => m.id.clone(),
        };
        if !self.take_request(conn, now) {
            let e = ApiError::new(ErrorKind::RateLimited, "too many requests");
            self.send_now(conn, id.as_ref(), Err(e));
            return;
        }
        let incoming = match decoded {
            Ok(i) => i,
            Err(mut m) => {
                let handshake_method = m
                    .method
                    .as_deref()
                    .is_some_and(|name| name.starts_with("session."));
                if handshaking && m.method.is_some() && !handshake_method {
                    // Before authentication only the handshake is revealed (decision 9):
                    // no method list, no parameter schema.
                    m.error = ApiError::new(
                        ErrorKind::NotAuthenticated,
                        "send session.hello, then session.authenticate",
                    );
                }
                let ready = self
                    .sessions
                    .get(&conn)
                    .is_some_and(|s| matches!(s.phase, Phase::Ready { .. }));
                match (ready, m.id) {
                    // An answer with an id waits its turn behind the request in flight.
                    (true, Some(id)) => self.enqueue(conn, Queued::Answer(id, Err(m.error)), now),
                    (_, id) => self.send_now(conn, id.as_ref(), Err(m.error)),
                }
                return;
            }
        };
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        match (s.phase.clone(), incoming.request) {
            (Phase::AwaitHello, Request::Hello(hello)) => {
                let speaks_v1 = hello
                    .protocols
                    .iter()
                    .any(|p| p == "1" || p.starts_with("1."));
                if !speaks_v1 {
                    let e = ApiError::new(
                        ErrorKind::UnsupportedProtocol,
                        format!("this server speaks protocol {PROTOCOL_VERSION}"),
                    );
                    self.send_now(conn, Some(&incoming.id), Err(e));
                    self.close(conn, CloseReason::UnsupportedProtocol);
                    return;
                }
                let same_user = s.peer.is_same_user_ipc();
                let scopes = same_user.then_some(ScopeSet::ALL);
                if let Some(scopes) = scopes {
                    s.phase = Phase::Ready {
                        scopes,
                        token: None,
                    };
                    s.limit.set(limits.max_message_bytes);
                    self.actions
                        .push_back(Action::Audit(AuditEvent::Authenticated {
                            conn,
                            token: None,
                            scopes,
                        }));
                } else {
                    s.phase = Phase::AwaitAuth;
                }
                let result = HelloResult {
                    protocol: PROTOCOL_VERSION.into(),
                    instance: self.config.instance.clone(),
                    scopes,
                };
                self.send_now(conn, Some(&incoming.id), Ok(Response::Hello(result)));
            }
            (Phase::AwaitAuth, Request::Authenticate(auth)) => {
                if s.pending_auth.is_some() {
                    let e = ApiError::new(ErrorKind::Busy, "authentication in progress");
                    self.send_now(conn, Some(&incoming.id), Err(e));
                    return;
                }
                let not_before = s
                    .address_key
                    .as_ref()
                    .and_then(|a| self.backoff.get(a))
                    .map_or(now, |b| b.until);
                if now >= not_before {
                    self.authenticate(conn, incoming.id, &auth.token, now);
                } else if let Some(s) = self.sessions.get_mut(&conn) {
                    s.pending_auth = Some(PendingAuth {
                        id: incoming.id,
                        token: auth.token,
                        not_before,
                    });
                }
            }
            (Phase::AwaitHello | Phase::AwaitAuth, _) => {
                let e = ApiError::new(
                    ErrorKind::NotAuthenticated,
                    "send session.hello, then session.authenticate",
                );
                self.send_now(conn, Some(&incoming.id), Err(e));
            }
            (Phase::Ready { scopes, token }, request) => {
                if s.queue.len() >= limits.queue_len {
                    let e = ApiError::new(ErrorKind::Busy, "request queue full");
                    self.send_now(conn, Some(&incoming.id), Err(e));
                    return;
                }
                let queued = admit(incoming.id, request, scopes);
                if let Queued::Forward(_, command) = &queued {
                    let scope = method_scope_of(command.method());
                    if matches!(scope, Some(Scope::Admin | Scope::Plugins | Scope::Tokens)) {
                        let detail = match command {
                            Command::RecordStartToFile(p) => Some(p.path.clone()),
                            Command::TokensCreate(p) => Some(p.name.clone()),
                            Command::TokensRevoke(p) => Some(p.name.clone()),
                            _ => None,
                        };
                        self.actions
                            .push_back(Action::Audit(AuditEvent::Privileged {
                                conn,
                                token: token.map(|(n, _, _)| n),
                                method: command.method(),
                                detail,
                            }));
                    }
                }
                self.enqueue(conn, queued, now);
            }
            (Phase::Closed, _) => {}
        }
    }

    /// Queues an answer or request behind the one in flight (`busy` when full).
    fn enqueue(&mut self, conn: ConnectionId, queued: Queued, now: Timestamp) {
        let max = self.config.limits.queue_len;
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        if s.queue.len() >= max {
            let id = match &queued {
                Queued::Answer(id, _) | Queued::Local(id, _) | Queued::Forward(id, _) => id.clone(),
            };
            let e = ApiError::new(ErrorKind::Busy, "request queue full");
            self.send_now(conn, Some(&id), Err(e));
            return;
        }
        s.queue.push_back(queued);
        self.pump(conn, now);
    }

    /// A message over the connection's limit, dropped by the transport.
    fn oversized(&mut self, conn: ConnectionId) {
        let Some(s) = self.sessions.get(&conn) else {
            return;
        };
        match s.phase {
            Phase::Closed => {}
            Phase::AwaitHello | Phase::AwaitAuth => self.close(conn, CloseReason::MessageTooLarge),
            Phase::Ready { .. } => {
                let e = ApiError::new(ErrorKind::MessageTooLarge, "message too large");
                self.send_now(conn, None, Err(e));
            }
        }
    }

    fn take_request(&mut self, conn: ConnectionId, now: Timestamp) -> bool {
        let limits = &self.config.limits;
        let Some(s) = self.sessions.get_mut(&conn) else {
            return false;
        };
        let elapsed = now.saturating_since(s.bucket.at);
        let nanos = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let refill = nanos.saturating_mul(u64::from(limits.request_rate)) / 1000;
        let cap = u64::from(limits.request_burst) * MICRO;
        s.bucket.level = s.bucket.level.saturating_add(refill).min(cap);
        s.bucket.at = now;
        if s.bucket.level >= MICRO {
            s.bucket.level -= MICRO;
            true
        } else {
            false
        }
    }

    fn authenticate(&mut self, conn: ConnectionId, id: RequestId, token: &str, now: Timestamp) {
        let allow_remote_admin = self.config.allow_remote_admin;
        let max_message = self.config.limits.max_message_bytes;
        let entry = self
            .tokens
            .verify(token)
            .map(|e| (e.name.clone(), e.scopes, e.hash));
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        match entry {
            Some((name, stored, hash)) => {
                let scopes = stored.for_channel(s.peer.is_local(), allow_remote_admin);
                s.phase = Phase::Ready {
                    scopes,
                    token: Some((name.clone(), hash, stored)),
                };
                s.limit.set(max_message);
                self.actions
                    .push_back(Action::Audit(AuditEvent::Authenticated {
                        conn,
                        token: Some(name),
                        scopes,
                    }));
                let result = AuthResult {
                    scopes,
                    server: self.config.server.clone(),
                };
                self.send_now(conn, Some(&id), Ok(Response::Authenticated(result)));
            }
            None => {
                let address = s.peer.address.clone();
                if let Some(key) = s.address_key.clone() {
                    self.record_failure(key, now);
                }
                self.actions
                    .push_back(Action::Audit(AuditEvent::AuthFailed { conn, address }));
                let e = ApiError::new(ErrorKind::AuthFailed, "authentication failed");
                self.send_now(conn, Some(&id), Err(e));
                self.close(conn, CloseReason::AuthFailed);
            }
        }
    }

    fn record_failure(&mut self, address: String, now: Timestamp) {
        let limits = &self.config.limits;
        // Forget an address once its delay ended longer ago than the longest delay.
        let quiet = limits.backoff_max;
        self.backoff
            .retain(|_, b| now.saturating_since(b.until) <= quiet);
        if self.backoff.len() >= limits.backoff_addresses && !self.backoff.contains_key(&address) {
            if let Some(oldest) = self
                .backoff
                .iter()
                .min_by_key(|(_, b)| b.until)
                .map(|(a, _)| a.clone())
            {
                self.backoff.remove(&oldest);
            }
        }
        let b = self.backoff.entry(address).or_insert(Backoff {
            failures: 0,
            until: now,
        });
        b.failures = b.failures.saturating_add(1);
        let factor = 1u32 << (b.failures - 1).min(16);
        let delay = limits
            .backoff_base
            .saturating_mul(factor)
            .min(limits.backoff_max);
        b.until = now + delay;
    }

    /// Answers queued requests in order until one is forwarded and in flight.
    fn pump(&mut self, conn: ConnectionId, now: Timestamp) {
        loop {
            let Some(s) = self.sessions.get_mut(&conn) else {
                return;
            };
            if s.in_flight || !matches!(s.phase, Phase::Ready { .. }) {
                return;
            }
            let Some(next) = s.queue.pop_front() else {
                return;
            };
            match next {
                Queued::Answer(id, result) => self.send_now(conn, Some(&id), result),
                Queued::Local(id, Request::Unsubscribe(u)) => {
                    for t in u.topics {
                        s.topics = s.topics.without(t);
                        s.sampled.remove(&t);
                    }
                    self.update_mask();
                    self.send_now(conn, Some(&id), Ok(Response::Done(Empty {})));
                }
                Queued::Local(id, _) => {
                    let d = Description {
                        protocol: PROTOCOL_VERSION.into(),
                        methods: METHODS
                            .iter()
                            .map(|m| MethodInfoOwned {
                                name: m.name.into(),
                                scope: m.scope,
                            })
                            .collect(),
                        topics: Topic::ALL.to_vec(),
                    };
                    self.send_now(conn, Some(&id), Ok(Response::Describe(d)));
                }
                Queued::Forward(id, command) => {
                    if let Command::EventsSubscribe(sub) = &command {
                        for t in &sub.topics {
                            s.topics = s.topics.with(*t);
                            if t.is_sampled() {
                                let hz = sub
                                    .rates
                                    .get(t)
                                    .copied()
                                    .unwrap_or(t.max_rate_hz())
                                    .clamp(1, t.max_rate_hz());
                                let interval = Duration::from_secs(1) / hz;
                                s.sampled.insert(
                                    *t,
                                    Sampling {
                                        interval,
                                        next: now + interval,
                                    },
                                );
                            }
                        }
                        self.update_mask();
                    }
                    if let Some(s) = self.sessions.get_mut(&conn) {
                        s.in_flight = true;
                    }
                    self.actions
                        .push_back(Action::Forward(Forwarded { conn, id, command }));
                    return;
                }
            }
        }
    }

    fn reply(&mut self, reply: Reply, now: Timestamp) {
        let Reply { conn, id, result } = reply;
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        s.in_flight = false;
        if matches!(s.phase, Phase::Closed) {
            self.forget_if_done(conn);
            return;
        }
        self.send_now(conn, Some(&id), result);
        self.pump(conn, now);
    }

    fn events(&mut self, events: &[Event]) {
        for event in events {
            let key = event.key();
            let text: Arc<str> = self.codec.encode_event(event).into();
            let conns: Vec<ConnectionId> = self
                .sessions
                .iter()
                .filter(|(_, s)| matches!(s.phase, Phase::Ready { .. }))
                .filter(|(_, s)| s.topics.contains(key.topic))
                .map(|(c, _)| *c)
                .collect();
            for conn in conns {
                self.send_event(conn, key, &text);
            }
        }
    }

    fn sample(&mut self, now: Timestamp) {
        let Some(mut sampler) = self.sampler.take() else {
            return;
        };
        for topic in Topic::ALL.into_iter().filter(|t| t.is_sampled()) {
            let due: Vec<ConnectionId> = self
                .sessions
                .iter()
                .filter(|(_, s)| matches!(s.phase, Phase::Ready { .. }))
                .filter(|(_, s)| s.sampled.get(&topic).is_some_and(|x| now >= x.next))
                .map(|(c, _)| *c)
                .collect();
            if due.is_empty() {
                continue;
            }
            for conn in &due {
                if let Some(x) = self
                    .sessions
                    .get_mut(conn)
                    .and_then(|s| s.sampled.get_mut(&topic))
                {
                    x.next = x.next + x.interval;
                    if x.next <= now {
                        x.next = now + x.interval;
                    }
                }
            }
            let Some(payload) = sampler.sample(topic) else {
                continue;
            };
            self.sample_seq += 1;
            let event = Event {
                object: None,
                seq: self.sample_seq,
                payload,
            };
            let text: Arc<str> = self.codec.encode_event(&event).into();
            for conn in due {
                self.send_event(conn, StateKey::topic(topic), &text);
            }
        }
        self.sampler = Some(sampler);
    }

    fn update_mask(&mut self) {
        let mask = self
            .sessions
            .values()
            .filter(|s| matches!(s.phase, Phase::Ready { .. }))
            .fold(TopicSet::EMPTY, |m, s| m.union(s.topics));
        let mask = TopicSet::of(mask.iter().filter(|t| !t.is_sampled()));
        if mask != self.mask {
            self.mask = mask;
            self.actions.push_back(Action::SetMask(mask));
        }
    }

    fn send_now(
        &mut self,
        conn: ConnectionId,
        id: Option<&RequestId>,
        result: Result<Response, ApiError>,
    ) {
        let text: Arc<str> = self.codec.encode_response(id, &result).into();
        let max = self.config.limits.max_pending_answers;
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        if matches!(s.phase, Phase::Closed) {
            return;
        }
        if !s.out.blocked {
            match s.sender.try_send(&text) {
                Ok(()) | Err(SendError::Closed) => return,
                Err(SendError::Full) => s.out.blocked = true,
            }
        }
        s.out.answers.push_back(text);
        if s.out.answers.len() > max {
            self.close(conn, CloseReason::TooSlow);
        }
    }

    fn send_event(&mut self, conn: ConnectionId, key: StateKey, text: &Arc<str>) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        if s.out.blocked {
            // Only the latest event per state key waits: bounded by the live keys.
            s.out.events.insert(key, text.clone());
            return;
        }
        if let Err(SendError::Full) = s.sender.try_send(text) {
            s.out.blocked = true;
            s.out.events.insert(key, text.clone());
        }
    }

    fn flush(&mut self, conn: ConnectionId) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        while !s.out.blocked {
            if let Some(text) = s.out.answers.pop_front() {
                if let Err(SendError::Full) = s.sender.try_send(&text) {
                    s.out.blocked = true;
                    s.out.answers.push_front(text);
                }
            } else if let Some((key, text)) = s.out.events.pop_first() {
                if let Err(SendError::Full) = s.sender.try_send(&text) {
                    s.out.blocked = true;
                    s.out.events.insert(key, text);
                }
            } else {
                break;
            }
        }
    }

    /// Closes a connection from the server side. The session stays until the transport
    /// reports it gone and its in-flight request is answered.
    fn close(&mut self, conn: ConnectionId, reason: CloseReason) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        if matches!(s.phase, Phase::Closed) {
            return;
        }
        s.phase = Phase::Closed;
        s.queue.clear();
        s.pending_auth = None;
        s.topics = TopicSet::EMPTY;
        s.sampled.clear();
        s.out = Outgoing::default();
        if !s.transport_closed {
            s.sender.close(reason);
        }
        self.actions
            .push_back(Action::Audit(AuditEvent::Closed { conn, reason }));
        self.update_mask();
    }

    fn transport_closed(&mut self, conn: ConnectionId) {
        let Some(s) = self.sessions.get_mut(&conn) else {
            return;
        };
        s.transport_closed = true;
        if !matches!(s.phase, Phase::Closed) {
            s.phase = Phase::Closed;
            s.queue.clear();
            s.pending_auth = None;
            s.topics = TopicSet::EMPTY;
            s.sampled.clear();
            s.out = Outgoing::default();
            self.update_mask();
        }
        self.actions
            .push_back(Action::Audit(AuditEvent::Disconnected { conn }));
        self.forget_if_done(conn);
    }

    /// Forgets a session once the transport dropped it and no request of it is in
    /// flight; only then its connection slot is free.
    fn forget_if_done(&mut self, conn: ConnectionId) {
        let done = self
            .sessions
            .get(&conn)
            .is_some_and(|s| s.transport_closed && !s.in_flight);
        if done {
            self.sessions.remove(&conn);
            self.budget.release();
        }
    }

    /// The token name of a session (tests and logs).
    pub fn token_of(&self, conn: ConnectionId) -> Option<String> {
        self.sessions.get(&conn).and_then(Session::token_name)
    }
}

/// The key for per-address caps and back-off: the address, or for IPv6 its /64, since
/// one host usually holds a whole /64.
fn address_key(address: &str) -> String {
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        Ok(std::net::IpAddr::V6(v6)) => v6
            .to_ipv4_mapped()
            .map_or_else(|| address.to_owned(), |v4| v4.to_string()),
        _ => address.to_owned(),
    }
}

fn method_scope_of(method: &str) -> Option<Scope> {
    crate::request::method_scope(method).flatten()
}

/// Checks an authenticated request and decides how it is answered: in order, either
/// locally, by the control thread, or with an error.
fn admit(id: RequestId, request: Request, scopes: ScopeSet) -> Queued {
    let denied = |scope: Scope| {
        Queued::Answer(
            id.clone(),
            Err(ApiError::new(
                ErrorKind::ScopeDenied,
                format!("needs scope {scope}"),
            )),
        )
    };
    if let Some(scope) = request.scope() {
        if !scopes.contains(scope) {
            return denied(scope);
        }
    }
    match request {
        Request::Hello(_) | Request::Authenticate(_) => Queued::Answer(
            id,
            Err(ApiError::new(
                ErrorKind::InvalidRequest,
                "the session is already authenticated",
            )),
        ),
        Request::Describe | Request::Unsubscribe(_) => Queued::Local(id, request),
        Request::Command(Command::EventsSubscribe(sub)) => {
            if let Some(t) = sub
                .topics
                .iter()
                .find(|t| !scopes.contains(topic_scope(**t)))
            {
                return denied(topic_scope(*t));
            }
            if let Some((t, _)) = sub
                .rates
                .iter()
                .find(|(t, hz)| !t.is_sampled() || **hz == 0)
            {
                return Queued::Answer(
                    id,
                    Err(ApiError::new(
                        ErrorKind::InvalidParams,
                        format!("no sampling rate for {t}: only latest-value topics, above 0 Hz"),
                    )),
                );
            }
            Queued::Forward(id, Command::EventsSubscribe(sub))
        }
        Request::Command(command) => Queued::Forward(id, command),
    }
}
