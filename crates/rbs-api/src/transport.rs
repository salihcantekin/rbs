//! The transport port (ADR-0044 decision 4). A [`ControlTransport`] adapter accepts
//! connections, exchanges whole UTF-8 messages and reports the peer. It runs its own
//! I/O (threads or an async runtime) and pushes into the bounded [`TransportInbox`]
//! that `rbs-api` gives it; `rbs-api` answers through each connection's
//! [`ConnectionSender`].
//!
//! Adapter contract (checked by the transport conformance suite):
//!
//! - Call [`TransportInbox::connect`] for each accepted connection after any checks that
//!   must happen before it is accepted (WebSocket: [`UpgradePolicy`](crate::UpgradePolicy)
//!   before the upgrade completes). A refused connection is closed without a session.
//! - Push each whole received message with [`ConnectionHandle::push_message`]. When it
//!   returns [`ReadState::Pause`], stop reading that connection until the `resume`
//!   callback given to `connect` runs (it can run before the adapter has paused, so it
//!   must leave a permit: `Thread::unpark`, a `Waker`, a flag). This is the
//!   per-connection back-pressure that keeps one client from filling the inbox.
//! - [`ConnectionSender::try_send`] never blocks. When it returns [`SendError::Full`],
//!   call [`ConnectionHandle::writable`] once the connection can take messages again.
//! - Drop the [`ConnectionHandle`] (or call [`ConnectionHandle::closed`]) exactly once
//!   when the connection is gone, whoever closed it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::wake::Wake;

/// Identifies a connection for the server's lifetime. Never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(u64);

impl ConnectionId {
    /// An id from its raw value (tests driving the session state machine directly).
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "connection#{}", self.0)
    }
}

/// How a peer is connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransportKind {
    /// WebSocket (`rbs-api-ws`).
    WebSocket,
    /// Unix domain socket or named pipe (`rbs-api-ipc`).
    Ipc,
}

/// What the transport knows about a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// How it is connected.
    pub kind: TransportKind,
    /// The peer address is a loopback address (always true for IPC).
    pub loopback: bool,
    /// The peer's address without port (`192.168.1.20`), for per-address caps and
    /// back-off; `None` for IPC.
    pub address: Option<String>,
    /// The `Origin` header, when present.
    pub origin: Option<String>,
    /// The `Host` header, when present.
    pub host: Option<String>,
    /// IPC only: the OS verified that the peer runs as the same user.
    pub same_user: bool,
}

impl PeerInfo {
    /// A loopback WebSocket peer without `Origin`.
    pub fn loopback_websocket() -> Self {
        Self {
            kind: TransportKind::WebSocket,
            loopback: true,
            address: Some("127.0.0.1".into()),
            origin: None,
            host: Some("localhost".into()),
            same_user: false,
        }
    }

    /// A same-user IPC peer.
    pub fn same_user_ipc() -> Self {
        Self {
            kind: TransportKind::Ipc,
            loopback: true,
            address: None,
            origin: None,
            host: None,
            same_user: true,
        }
    }

    /// A WebSocket peer from another machine at `address`.
    pub fn remote_websocket(address: &str) -> Self {
        Self {
            kind: TransportKind::WebSocket,
            loopback: false,
            address: Some(address.into()),
            origin: None,
            host: Some("studio.local".into()),
            same_user: false,
        }
    }

    /// Loopback or IPC: may use `admin`, `plugins` and `tokens`.
    pub fn is_local(&self) -> bool {
        self.loopback || self.kind == TransportKind::Ipc
    }

    /// An IPC peer the OS identified as the same user.
    pub fn is_same_user_ipc(&self) -> bool {
        self.kind == TransportKind::Ipc && self.same_user
    }
}

/// Why the server closes a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseReason {
    /// The server stops.
    ServerStopping,
    /// Not authenticated within the handshake timeout.
    HandshakeTimeout,
    /// The token was refused.
    AuthFailed,
    /// No common protocol version.
    UnsupportedProtocol,
    /// A connection, pending-handshake or per-address cap was reached.
    TooManyConnections,
    /// A message before authentication was larger than allowed.
    MessageTooLarge,
    /// The session's token was revoked.
    Revoked,
    /// The peer does not read its answers: more than the limit wait.
    TooSlow,
}

/// Why a message was not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The connection's write queue is full; the adapter calls
    /// [`ConnectionHandle::writable`] when it drained.
    Full,
    /// The connection is gone.
    Closed,
}

/// Sends to one connection. Called only on the API thread; must not block.
pub trait ConnectionSender: Send {
    /// Queues a whole message.
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError>;
    /// Closes the connection. The adapter still drops the [`ConnectionHandle`] when it
    /// is gone.
    fn close(&mut self, reason: CloseReason);
}

/// What adapters push into the inbox.
pub enum TransportEvent {
    /// A connection was accepted.
    Connected {
        /// Its id.
        id: ConnectionId,
        /// Who it is.
        peer: PeerInfo,
        /// How to answer it.
        sender: Box<dyn ConnectionSender>,
    },
    /// A whole message arrived.
    Message {
        /// The connection.
        id: ConnectionId,
        /// The message.
        text: String,
    },
    /// The connection can take messages again after [`SendError::Full`].
    Writable {
        /// The connection.
        id: ConnectionId,
    },
    /// The connection is gone.
    Closed {
        /// The connection.
        id: ConnectionId,
    },
}

impl fmt::Debug for TransportEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportEvent::Connected { id, peer, .. } => {
                write!(f, "Connected({id}, {peer:?})")
            }
            TransportEvent::Message { id, text } => write!(f, "Message({id}, {} B)", text.len()),
            TransportEvent::Writable { id } => write!(f, "Writable({id})"),
            TransportEvent::Closed { id } => write!(f, "Closed({id})"),
        }
    }
}

/// Why a connection was refused at `connect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The total connection cap is reached.
    TooManyConnections,
    /// The server stopped.
    Stopped,
}

/// Counts connections from `connect` until the server has forgotten them (the
/// adapter dropped the handle and no request of theirs is still in flight).
#[derive(Debug)]
pub struct ConnectionBudget {
    open: AtomicUsize,
    max: usize,
}

impl ConnectionBudget {
    /// A budget of `max` connections.
    pub fn new(max: usize) -> Self {
        Self {
            open: AtomicUsize::new(0),
            max,
        }
    }

    /// Connections counted now.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    fn try_take(&self) -> bool {
        self.open
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max).then_some(n + 1)
            })
            .is_ok()
    }

    pub(crate) fn release(&self) {
        let _ = self
            .open
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
    }
}

/// Whether the adapter may keep reading a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadState {
    /// Keep reading.
    Continue,
    /// The connection used its quota: stop reading until `resume` runs.
    Pause,
}

struct Quota {
    queued: AtomicUsize,
    max: usize,
    resume: Box<dyn Fn() + Send + Sync>,
}

struct Inner {
    queue: Mutex<VecDeque<(TransportEvent, Option<Arc<Quota>>)>>,
    wake: Arc<Wake>,
    budget: Arc<ConnectionBudget>,
    next_id: AtomicU64,
    quota: usize,
    stopped: std::sync::atomic::AtomicBool,
}

/// The bounded inbox adapters push into: at most the connection cap times
/// (per-connection quota + 3) events.
#[derive(Clone)]
pub struct TransportInbox {
    inner: Arc<Inner>,
}

impl TransportInbox {
    /// An inbox that signals `wake`, counts connections in `budget` and lets each
    /// connection have `quota` unread messages.
    pub fn new(wake: Arc<Wake>, budget: Arc<ConnectionBudget>, quota: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(VecDeque::new()),
                wake,
                budget,
                next_id: AtomicU64::new(1),
                quota: quota.max(1),
                stopped: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// Registers an accepted connection. `resume` runs on the API thread when a paused
    /// connection may be read again; it must not block.
    pub fn connect(
        &self,
        peer: PeerInfo,
        sender: Box<dyn ConnectionSender>,
        resume: Box<dyn Fn() + Send + Sync>,
    ) -> Result<ConnectionHandle, Refused> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(Refused::Stopped);
        }
        if !self.inner.budget.try_take() {
            return Err(Refused::TooManyConnections);
        }
        let id = ConnectionId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let quota = Arc::new(Quota {
            queued: AtomicUsize::new(0),
            max: self.inner.quota,
            resume,
        });
        self.push(TransportEvent::Connected { id, peer, sender }, None);
        Ok(ConnectionHandle {
            id,
            inbox: self.clone(),
            quota,
            closed: false,
        })
    }

    /// Takes the oldest event. Runs a paused connection's `resume` when its quota
    /// frees.
    pub fn pop(&self) -> Option<TransportEvent> {
        let (event, quota) = self
            .inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()?;
        if let Some(q) = quota {
            let before = q.queued.fetch_sub(1, Ordering::AcqRel);
            if before == q.max {
                (q.resume)();
            }
        }
        Some(event)
    }

    /// Events waiting.
    pub fn len(&self) -> usize {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// True when no event waits.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Refuses new connections from now on.
    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::Release);
    }

    fn push(&self, event: TransportEvent, quota: Option<Arc<Quota>>) {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back((event, quota));
        self.inner.wake.signal();
    }
}

/// An adapter's handle to one accepted connection. Dropping it reports the
/// connection closed.
pub struct ConnectionHandle {
    id: ConnectionId,
    inbox: TransportInbox,
    quota: Arc<Quota>,
    closed: bool,
}

impl ConnectionHandle {
    /// The connection's id.
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Pushes a whole received message.
    pub fn push_message(&self, text: String) -> ReadState {
        let queued = self.quota.queued.fetch_add(1, Ordering::AcqRel) + 1;
        self.inbox.push(
            TransportEvent::Message { id: self.id, text },
            Some(self.quota.clone()),
        );
        if queued >= self.quota.max {
            ReadState::Pause
        } else {
            ReadState::Continue
        }
    }

    /// True when the connection has quota left (check after `resume` ran).
    pub fn can_read(&self) -> bool {
        self.quota.queued.load(Ordering::Acquire) < self.quota.max
    }

    /// The connection can take messages again after [`SendError::Full`].
    pub fn writable(&self) {
        self.inbox
            .push(TransportEvent::Writable { id: self.id }, None);
    }

    /// The connection is gone (the same as dropping the handle).
    pub fn closed(self) {
        drop(self);
    }
}

impl Drop for ConnectionHandle {
    fn drop(&mut self) {
        if !self.closed {
            self.closed = true;
            self.inbox
                .push(TransportEvent::Closed { id: self.id }, None);
        }
    }
}

/// Why a transport could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

/// A transport adapter (WebSocket, IPC). Only `rbs-app` and `rbs-cli` choose which
/// run.
pub trait ControlTransport: Send {
    /// Starts accepting connections into `inbox` (binds, spawns its I/O).
    fn start(&mut self, inbox: TransportInbox) -> Result<(), TransportError>;
    /// Stops accepting, closes every connection and joins its I/O. After it returns,
    /// the adapter pushes nothing more.
    fn stop(&mut self);
}
