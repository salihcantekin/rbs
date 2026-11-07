//! The API thread (ADR-0044 decision 5): exists only while remote control is on, owns
//! the [`Server`], and sleeps on one [`Wake`] shared by the transport inbox, the reply
//! channel and the event slots until the server's next deadline. No periodic wake-up:
//! with no session and nothing due it sleeps until signalled.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use rbs_core::time::{Clock, MonotonicClock};

use crate::control::{ControlEndpoint, EventShared};
use crate::session::{Action, AuditEvent, Input, Sampler, Server, ServerConfig};
use crate::token::TokenStore;
use crate::transport::{ControlTransport, TransportError, TransportInbox};
use crate::wake::Wake;

/// What the API thread needs besides the configuration.
pub struct ApiOptions {
    /// Transports to start (WebSocket, IPC).
    pub transports: Vec<Box<dyn ControlTransport>>,
    /// Samples latest-value topics; `None`: `audio.levels` sends nothing.
    pub sampler: Option<Box<dyn Sampler>>,
    /// Wakes the control thread after a command was forwarded to it. Must not block.
    pub control_waker: Box<dyn Fn() + Send + Sync>,
    /// Logs connections, authentication and privileged calls. Runs on the API thread.
    pub audit: Box<dyn FnMut(AuditEvent) + Send>,
    /// The clock deadlines are measured in (tests pass a `TestClock`).
    pub clock: Arc<dyn Clock>,
}

impl ApiOptions {
    /// No transport, no sampler, the monotonic clock; `control_waker` wakes the
    /// control thread and audit records are dropped.
    pub fn new(control_waker: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            transports: Vec::new(),
            sampler: None,
            control_waker: Box::new(control_waker),
            audit: Box::new(|_| {}),
            clock: Arc::new(MonotonicClock::new()),
        }
    }
}

struct Shared {
    wake: Arc<Wake>,
    stop: AtomicBool,
    tokens: Mutex<Option<TokenStore>>,
    rounds: AtomicU64,
}

/// Replaces the token store of a running [`ApiServer`] ([`ApiServer::token_handle`]).
/// The API thread takes a new store at the start of a round, before it reads input,
/// and writes the control thread's replies after reading input. So when the control
/// thread sets the store before it replies, every message any client sends after that
/// reply arrived is checked against the new store, and the sessions of revoked tokens
/// are closed before such a message is read. Setting it after the server stopped does
/// nothing.
#[derive(Clone)]
pub struct TokenHandle(Arc<Shared>);

impl TokenHandle {
    /// Replaces the token store.
    pub fn set(&self, tokens: TokenStore) {
        *self.0.tokens.lock().unwrap_or_else(PoisonError::into_inner) = Some(tokens);
        self.0.wake.signal();
    }
}

impl std::fmt::Debug for TokenHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenHandle")
    }
}

/// A running control API server. Dropping it stops the transports, closes every
/// session and joins the API thread.
pub struct ApiServer {
    shared: Arc<Shared>,
    inbox: TransportInbox,
    transports: Vec<Box<dyn ControlTransport>>,
    thread: Option<JoinHandle<()>>,
}

impl ApiServer {
    /// Starts the API thread and the transports. Returns the server and the endpoint
    /// the control thread drains.
    pub fn start(
        config: ServerConfig,
        tokens: TokenStore,
        options: ApiOptions,
    ) -> Result<(ApiServer, ControlEndpoint), TransportError> {
        let ApiOptions {
            transports,
            sampler,
            control_waker,
            mut audit,
            clock,
        } = options;
        let wake = Arc::new(Wake::new());
        let quota = config.limits.inbox_quota;
        let pre_auth_bytes = config.limits.pre_auth_max_bytes;
        let mut server = Server::new(config, tokens);
        if let Some(s) = sampler {
            server = server.with_sampler(s);
        }
        let inbox = TransportInbox::new(wake.clone(), server.budget(), quota, pre_auth_bytes);
        // Both channels are bounded by construction: a session has at most one command
        // in flight, and sessions (including closed ones whose command is still in
        // flight) are limited by the connection budget. So the control thread's reply
        // send never fails or blocks while the API thread runs.
        let (forward_tx, forward_rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        let events = Arc::new(EventShared::new(wake.clone()));
        let endpoint = ControlEndpoint {
            requests: forward_rx,
            replies: reply_tx,
            wake: wake.clone(),
            events: events.clone(),
        };
        let shared = Arc::new(Shared {
            wake,
            stop: AtomicBool::new(false),
            tokens: Mutex::new(None),
            rounds: AtomicU64::new(0),
        });
        let thread = {
            let shared = shared.clone();
            let inbox = inbox.clone();
            std::thread::Builder::new()
                .name("rbs-api".into())
                .spawn(move || {
                    let mut dispatch = |server: &mut Server| {
                        while let Some(action) = server.next_action() {
                            match action {
                                Action::Forward(f) => {
                                    // The control thread may be gone at shutdown.
                                    if forward_tx.send(f).is_ok() {
                                        control_waker();
                                    }
                                }
                                Action::SetMask(mask) => events.set_mask(mask),
                                Action::Audit(a) => audit(a),
                            }
                        }
                    };
                    loop {
                        shared.rounds.fetch_add(1, Ordering::Relaxed);
                        if shared.stop.load(Ordering::Acquire) {
                            server.shutdown();
                            dispatch(&mut server);
                            break;
                        }
                        let now = clock.now();
                        let tokens = shared
                            .tokens
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .take();
                        if let Some(t) = tokens {
                            server.set_tokens(t);
                        }
                        while let Some(event) = inbox.pop() {
                            // A store set before a client connected must be in place
                            // before that client's events are handled: replies go out
                            // during this loop, so a client can connect, say hello and
                            // authenticate within one round. Checking after each pop
                            // orders the replacement before every event enqueued
                            // after it (`set` happens before the event was sent).
                            let tokens = shared
                                .tokens
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .take();
                            if let Some(t) = tokens {
                                server.set_tokens(t);
                            }
                            server.handle(Input::Transport(event), now);
                        }
                        while let Ok(reply) = reply_rx.try_recv() {
                            server.handle(Input::Reply(reply), now);
                        }
                        let drained = events.drain();
                        if !drained.is_empty() {
                            server.handle(Input::Events(drained), now);
                        }
                        server.advance(clock.now());
                        dispatch(&mut server);
                        let timeout = server
                            .next_deadline()
                            .map(|d| d.saturating_since(clock.now()));
                        shared.wake.wait(timeout);
                    }
                })
                .map_err(|e| TransportError(format!("cannot start the API thread: {e}")))?
        };
        let mut api = ApiServer {
            shared,
            inbox,
            transports: Vec::new(),
            thread: Some(thread),
        };
        for mut t in transports {
            t.start(api.inbox.clone())?;
            api.transports.push(t);
        }
        Ok((api, endpoint))
    }

    /// Replaces the token store; sessions of revoked tokens are closed.
    pub fn set_tokens(&self, tokens: TokenStore) {
        self.token_handle().set(tokens);
    }

    /// A handle that replaces the token store from another thread, such as the
    /// control thread answering `tokens.*`.
    pub fn token_handle(&self) -> TokenHandle {
        TokenHandle(self.shared.clone())
    }

    /// The inbox, for transports started outside [`ApiServer::start`] and for tests.
    pub fn inbox(&self) -> TransportInbox {
        self.inbox.clone()
    }

    /// Rounds the API thread has run (idle-cost check: no rounds without input).
    pub fn rounds(&self) -> u64 {
        self.shared.rounds.load(Ordering::Relaxed)
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        for t in &mut self.transports {
            t.stop();
        }
        self.inbox.stop();
        self.shared.stop.store(true, Ordering::Release);
        self.shared.wake.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
