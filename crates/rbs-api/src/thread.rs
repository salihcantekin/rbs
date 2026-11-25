//! The API thread (ADR-0044 decision 5, ADR-0134 decision 2): exists only while remote
//! control is on or AI tools are started, owns
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
use crate::token::{TokenSlot, TokenStore};
use crate::transport::{ControlTransport, TransportError, TransportInbox};
use crate::wake::Wake;

/// What the API thread needs besides the configuration.
pub struct ApiOptions {
    /// Registered desktop UI clients. Default: no UI routing destination.
    pub ui: crate::ui::UiRouter,
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
            ui: crate::ui::UiRouter::default(),
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
    tokens: Mutex<Option<TokenSlot>>,
    /// A new "allow remote admin", taken like `tokens`.
    remote_admin: Mutex<Option<bool>>,
    rounds: AtomicU64,
}

impl Shared {
    /// Applies what was set from other threads since the last call.
    fn apply_pending(&self, server: &mut Server) {
        let admin = self
            .remote_admin
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(a) = admin {
            server.set_allow_remote_admin(a);
        }
        let tokens = self
            .tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(t) = tokens {
            server.set_slot(t);
        }
    }
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
    /// Replaces the token store and the AI tools flags (ADR-0134 decision 2). A store
    /// alone (`store.into()`) means AI tools stopped and tokenless IPC.
    pub fn set(&self, slot: impl Into<TokenSlot>) {
        *self.0.tokens.lock().unwrap_or_else(PoisonError::into_inner) = Some(slot.into());
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
    ui: crate::ui::UiRouter,
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
        tokens: impl Into<TokenSlot>,
        options: ApiOptions,
    ) -> Result<(ApiServer, ControlEndpoint), TransportError> {
        let ApiOptions {
            ui,
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
        let (ui_tx, ui_rx) = mpsc::channel();
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
            remote_admin: Mutex::new(None),
            rounds: AtomicU64::new(0),
        });
        let thread = {
            let ui = ui.clone();
            let shared = shared.clone();
            let inbox = inbox.clone();
            std::thread::Builder::new()
                .name("rbs-api".into())
                .spawn(move || {
                    let mut dispatch = |server: &mut Server| {
                        while let Some(action) = server.next_action() {
                            match action {
                                Action::Ui(call) => {
                                    let sender = ui_tx.clone();
                                    let wake = shared.wake.clone();
                                    ui.route(
                                        call,
                                        crate::ui::UiResponder {
                                            send: Arc::new(move |reply| {
                                                if sender.send(reply).is_ok() {
                                                    wake.signal();
                                                }
                                            }),
                                            sent: Arc::new(AtomicBool::new(false)),
                                        },
                                    );
                                }
                                Action::Forward(f) => {
                                    // The control thread may be gone at shutdown.
                                    if forward_tx.send(f).is_ok() {
                                        control_waker();
                                    }
                                }
                                Action::SetMask(mask) => events.set_mask(mask),
                                Action::SetSampled(mask) => {
                                    events.set_sampled(mask);
                                    control_waker();
                                }
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
                        shared.apply_pending(&mut server);
                        while let Some(event) = inbox.pop() {
                            // A store set before a client connected must be in place
                            // before that client's events are handled: replies go out
                            // during this loop, so a client can connect, say hello and
                            // authenticate within one round. Checking after each pop
                            // orders the replacement before every event enqueued
                            // after it (`set` happens before the event was sent).
                            shared.apply_pending(&mut server);
                            server.handle(Input::Transport(event), now);
                        }
                        while let Ok(reply) = reply_rx.try_recv() {
                            server.handle(Input::Reply(reply), now);
                        }
                        while let Ok(resolved) = ui_rx.try_recv() {
                            shared.apply_pending(&mut server);
                            server.handle(Input::Ui(resolved), now);
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
            ui,
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

    /// Replaces the token store (AI tools stopped, tokenless IPC); sessions of
    /// revoked tokens are closed.
    pub fn set_tokens(&self, tokens: TokenStore) {
        self.token_handle().set(tokens);
    }

    /// The registration handle for the desktop clients of this server.
    pub fn ui_router(&self) -> crate::ui::UiRouter {
        self.ui.clone()
    }

    /// Replaces the token store and the AI tools flags.
    pub fn set_slot(&self, slot: TokenSlot) {
        self.token_handle().set(slot);
    }

    /// Sets "allow remote admin" for sessions that authenticate from now on (a
    /// transport restarted after this call accepts nothing before it applies).
    /// Sessions already authenticated keep their scopes.
    pub fn set_allow_remote_admin(&self, allow: bool) {
        *self
            .shared
            .remote_admin
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(allow);
        self.shared.wake.signal();
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
