//! The transport conformance suite (control-api spec, "Testing"): every
//! [`ControlTransport`] adapter runs it from its own tests with a [`Harness`] that
//! starts the adapter and opens real client connections.
//!
//! Each check drives a fresh [`TransportInbox`] directly, without the API thread, and
//! waits on its [`Wake`] with a hang guard only (ADR-0032): no check sleeps for timing.
//!
//! Enabled with the `conformance` feature.

#![allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)] // A failed check panics.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::transport::{
    CloseReason, ConnectionBudget, ConnectionSender, ControlTransport, PeerInfo, SendError,
    TransportError, TransportEvent, TransportInbox, TransportKind,
};
use crate::wake::Wake;

/// Hang guard for every wait in the suite.
pub const LIMIT: Duration = Duration::from_secs(20);

/// The origin a harness puts on its adapter's allow list.
pub const ALLOWED_ORIGIN: &str = "https://panel.example";

/// The host name a harness configures for its adapter.
pub const HOST_NAME: &str = "studio.local";

/// The limit before authentication the suite gives the inbox.
pub const PRE_AUTH_BYTES: usize = 4096;

/// Upgrade headers a client sends (WebSocket); other transports ignore them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectOptions {
    /// The `Origin` header, if any.
    pub origin: Option<String>,
    /// The `Host` header; `None` lets the client use the address it connects to.
    pub host: Option<String>,
    /// Leave the `Host` header out.
    pub no_host: bool,
}

/// What a client sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEvent {
    /// A whole message.
    Message(String),
    /// The server closed the connection, with the reason when the transport carries one.
    Closed(Option<CloseReason>),
}

/// A client connection the harness opened.
pub trait Client {
    /// Sends one whole message.
    fn send(&mut self, text: &str);
    /// The next message or the close, waiting at most `timeout`; `None` when nothing
    /// came.
    fn recv(&mut self, timeout: Duration) -> Option<ClientEvent>;
    /// Closes the connection from the client side.
    fn close(&mut self);
}

/// Starts the adapter under test and opens client connections to it.
pub trait Harness {
    /// The harness's client.
    type Client: Client;
    /// The transport kind the adapter reports.
    fn kind(&self) -> TransportKind;
    /// True for adapters that run the `Origin` and `Host` check before the upgrade
    /// (WebSocket). Their allow list holds [`ALLOWED_ORIGIN`] and their host names
    /// [`HOST_NAME`].
    fn checks_upgrade(&self) -> bool;
    /// A fresh adapter, started into `inbox`.
    fn start(&mut self, inbox: TransportInbox)
    -> Result<Box<dyn ControlTransport>, TransportError>;
    /// Opens a connection. `Ok` once the transport accepted it (for WebSocket: the
    /// upgrade completed); `Err` with the refusal's description when it was refused
    /// before completion.
    fn connect(&mut self, options: &ConnectOptions) -> Result<Self::Client, String>;
}

/// Runs every check.
pub fn run_all<H: Harness>(h: &mut H) {
    messages_round_trip(h);
    oversized_messages_are_reported_not_buffered(h);
    the_server_closes_with_a_reason(h);
    a_client_close_is_reported(h);
    a_paused_connection_is_not_read(h);
    a_full_connection_reports_writable(h);
    a_refused_connection_never_reaches_the_inbox(h);
    stop_closes_everything(h);
    if h.checks_upgrade() {
        origin_and_host_are_checked_before_the_upgrade(h);
    }
}

/// An inbox and the adapter running into it.
pub struct Rig {
    /// The inbox the adapter pushes into.
    pub inbox: TransportInbox,
    wake: Arc<Wake>,
    transport: Option<Box<dyn ControlTransport>>,
}

impl Rig {
    /// Starts the harness's adapter on an inbox with `max` connections and `quota`
    /// unread messages per connection.
    pub fn start<H: Harness>(h: &mut H, max: usize, quota: usize) -> Self {
        let wake = Arc::new(Wake::new());
        let budget = Arc::new(ConnectionBudget::new(max));
        let inbox = TransportInbox::new(wake.clone(), budget, quota, PRE_AUTH_BYTES);
        let transport = h.start(inbox.clone()).expect("the transport starts");
        Self {
            inbox,
            wake,
            transport: Some(transport),
        }
    }

    /// The next inbox event, waiting at most [`LIMIT`].
    pub fn next(&self) -> TransportEvent {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(e) = self.inbox.pop() {
                return e;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no transport event within {LIMIT:?}");
            self.wake.wait(Some(left));
        }
    }

    /// Waits for the next `Connected` event and returns its parts.
    pub fn connected(&self) -> Connected {
        match self.next() {
            TransportEvent::Connected {
                id, peer, sender, ..
            } => Connected {
                id: id.get(),
                peer,
                sender,
            },
            other => panic!("expected Connected, got {other:?}"),
        }
    }

    /// Stops the adapter (and joins its I/O).
    pub fn stop(&mut self) {
        if let Some(mut t) = self.transport.take() {
            t.stop();
        }
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The parts of a `Connected` event.
pub struct Connected {
    /// The connection's raw id.
    pub id: u64,
    /// What the adapter reported about the peer.
    pub peer: PeerInfo,
    /// The connection's sender.
    pub sender: Box<dyn ConnectionSender>,
}

fn recv<C: Client>(c: &mut C) -> ClientEvent {
    c.recv(LIMIT)
        .expect("the client got something within the limit")
}

fn send(sender: &mut dyn ConnectionSender, text: &str) {
    let text: Arc<str> = Arc::from(text);
    sender
        .try_send(&text)
        .expect("an idle connection takes a message");
}

/// A message goes each way intact, including multi-byte UTF-8 and an empty message,
/// and the peer is reported as the harness's transport kind.
pub fn messages_round_trip<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 4, 8);
    let mut c = h.connect(&ConnectOptions::default()).expect("accepted");
    let mut conn = rig.connected();
    assert_eq!(conn.peer.kind, h.kind());
    assert!(
        conn.peer.is_local(),
        "a test client is local: {:?}",
        conn.peer
    );
    for text in ["{\"jsonrpc\":\"2.0\"}", "çağrı ✓ 🎥", ""] {
        c.send(text);
        match rig.next() {
            TransportEvent::Message { id, text: got } => {
                assert_eq!(id.get(), conn.id);
                assert_eq!(got, text);
            }
            other => panic!("expected Message, got {other:?}"),
        }
        send(conn.sender.as_mut(), text);
        assert_eq!(recv(&mut c), ClientEvent::Message(text.to_owned()));
    }
}

/// A message longer than the connection's limit arrives as `Oversized`, and the
/// connection keeps working: the next message arrives whole.
pub fn oversized_messages_are_reported_not_buffered<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 4, 8);
    let mut c = h.connect(&ConnectOptions::default()).expect("accepted");
    let _conn = rig.connected();
    let long = "x".repeat(PRE_AUTH_BYTES + 1);
    c.send(&long);
    match rig.next() {
        TransportEvent::Oversized { len, .. } => assert!(len > PRE_AUTH_BYTES, "{len}"),
        other => panic!("expected Oversized, got {other:?}"),
    }
    let exact = "y".repeat(PRE_AUTH_BYTES);
    c.send(&exact);
    match rig.next() {
        TransportEvent::Message { text, .. } => assert_eq!(text, exact),
        other => panic!("expected Message, got {other:?}"),
    }
}

/// `ConnectionSender::close` reaches the client with its reason, after the messages
/// sent before it, and the adapter reports the connection closed.
pub fn the_server_closes_with_a_reason<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 4, 8);
    for reason in [CloseReason::AuthFailed, CloseReason::MessageTooLarge] {
        let mut c = h.connect(&ConnectOptions::default()).expect("accepted");
        let mut conn = rig.connected();
        send(conn.sender.as_mut(), "last words");
        conn.sender.close(reason);
        assert_eq!(recv(&mut c), ClientEvent::Message("last words".into()));
        match recv(&mut c) {
            ClientEvent::Closed(Some(r)) => assert_eq!(r, reason),
            ClientEvent::Closed(None) if h.kind() == TransportKind::Ipc => {}
            other => panic!("expected the close with {reason:?}, got {other:?}"),
        }
        match rig.next() {
            TransportEvent::Closed { id } => assert_eq!(id.get(), conn.id),
            other => panic!("expected Closed, got {other:?}"),
        }
        let text: Arc<str> = Arc::from("late");
        assert_eq!(conn.sender.try_send(&text), Err(SendError::Closed));
    }
}

/// A client that closes is reported closed.
pub fn a_client_close_is_reported<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 4, 8);
    let mut c = h.connect(&ConnectOptions::default()).expect("accepted");
    let conn = rig.connected();
    c.close();
    match rig.next() {
        TransportEvent::Closed { id } => assert_eq!(id.get(), conn.id),
        other => panic!("expected Closed, got {other:?}"),
    }
}

/// A connection that used its quota is not read until the inbox drains: the inbox
/// never holds more of its messages than the quota, and every message still arrives,
/// in order.
pub fn a_paused_connection_is_not_read<H: Harness>(h: &mut H) {
    const QUOTA: usize = 2;
    const SENT: usize = 7;
    let rig = Rig::start(h, 4, QUOTA);
    let mut c = h.connect(&ConnectOptions::default()).expect("accepted");
    let _conn = rig.connected();
    for i in 0..SENT {
        c.send(&format!("m{i}"));
    }
    for i in 0..SENT {
        assert!(
            rig.inbox.len() <= QUOTA,
            "{} messages unread, quota {QUOTA}",
            rig.inbox.len()
        );
        match rig.next() {
            TransportEvent::Message { text, .. } => assert_eq!(text, format!("m{i}")),
            other => panic!("expected Message, got {other:?}"),
        }
    }
}

/// A connection whose peer does not read fills up: `try_send` answers `Full` instead
/// of blocking, and once the peer reads, the adapter reports `Writable`.
pub fn a_full_connection_reports_writable<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 4, 8);
    let mut c = h.connect(&ConnectOptions::default()).expect("accepted");
    let mut conn = rig.connected();
    let chunk: Arc<str> = Arc::from("z".repeat(64 * 1024));
    let mut sent = 0usize;
    loop {
        match conn.sender.try_send(&chunk) {
            Ok(()) => sent += 1,
            Err(SendError::Full) => break,
            Err(SendError::Closed) => panic!("closed while filling"),
        }
        assert!(sent < 4096, "never full after {sent} messages of 64 KiB");
    }
    for _ in 0..sent {
        match recv(&mut c) {
            ClientEvent::Message(m) => assert_eq!(m.len(), chunk.len()),
            other => panic!("expected a message, got {other:?}"),
        }
    }
    match rig.next() {
        TransportEvent::Writable { id } => assert_eq!(id.get(), conn.id),
        other => panic!("expected Writable, got {other:?}"),
    }
}

/// A connection over the inbox's connection cap is refused before it completes and
/// leaves nothing in the inbox.
pub fn a_refused_connection_never_reaches_the_inbox<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 1, 8);
    let _first = h.connect(&ConnectOptions::default()).expect("accepted");
    let _conn = rig.connected();
    assert!(h.connect(&ConnectOptions::default()).is_err());
    assert!(rig.inbox.is_empty(), "a refused connection pushed an event");
}

/// `stop` closes every connection with `ServerStopping`, reports each closed, and
/// pushes nothing after it returns; new connections are refused.
pub fn stop_closes_everything<H: Harness>(h: &mut H) {
    let mut rig = Rig::start(h, 4, 8);
    let mut a = h.connect(&ConnectOptions::default()).expect("accepted");
    let mut b = h.connect(&ConnectOptions::default()).expect("accepted");
    let first = rig.connected();
    let second = rig.connected();
    rig.stop();
    for c in [&mut a, &mut b] {
        match recv(c) {
            ClientEvent::Closed(Some(r)) => assert_eq!(r, CloseReason::ServerStopping),
            ClientEvent::Closed(None) if h.kind() == TransportKind::Ipc => {}
            other => panic!("expected the stop, got {other:?}"),
        }
    }
    let mut closed = 0;
    while let Some(e) = rig.inbox.pop() {
        match e {
            TransportEvent::Closed { id } => {
                assert!(id.get() == first.id || id.get() == second.id);
                closed += 1;
            }
            other => panic!("unexpected {other:?} after stop"),
        }
    }
    assert_eq!(closed, 2);
    assert!(h.connect(&ConnectOptions::default()).is_err());
    assert!(rig.inbox.is_empty());
}

/// `Origin` and `Host` are checked before the upgrade completes: a refused request
/// never reaches the inbox, an accepted one reports both headers.
pub fn origin_and_host_are_checked_before_the_upgrade<H: Harness>(h: &mut H) {
    let rig = Rig::start(h, 8, 8);
    let refused = [
        ConnectOptions {
            origin: Some("null".into()),
            ..ConnectOptions::default()
        },
        ConnectOptions {
            origin: Some("https://evil.example".into()),
            ..ConnectOptions::default()
        },
        ConnectOptions {
            host: Some("evil.example".into()),
            ..ConnectOptions::default()
        },
        ConnectOptions {
            host: Some("10.255.255.1".into()),
            ..ConnectOptions::default()
        },
        ConnectOptions {
            no_host: true,
            ..ConnectOptions::default()
        },
    ];
    for options in &refused {
        assert!(h.connect(options).is_err(), "accepted {options:?}");
        assert!(rig.inbox.is_empty(), "{options:?} reached the inbox");
    }
    let accepted = [
        (
            Some(ALLOWED_ORIGIN.to_owned()),
            Some("localhost:1".to_owned()),
        ),
        (
            Some(ALLOWED_ORIGIN.to_ascii_uppercase()),
            Some("127.0.0.1".to_owned()),
        ),
        (None, Some("[::1]:80".to_owned())),
        (None, Some(format!("{HOST_NAME}:4455"))),
    ];
    for (origin, host) in accepted {
        let options = ConnectOptions {
            origin: origin.clone(),
            host: host.clone(),
            no_host: false,
        };
        let _c = h
            .connect(&options)
            .unwrap_or_else(|e| panic!("refused {options:?}: {e}"));
        let conn = rig.connected();
        assert_eq!(conn.peer.origin, origin);
        assert_eq!(conn.peer.host, host);
        drop(conn);
        drop(_c);
        match rig.next() {
            TransportEvent::Closed { .. } => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }
}
