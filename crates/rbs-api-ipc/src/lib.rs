//! Local IPC transport for the RBS control API (ADR-0044 decisions 4 and 11,
//! `docs/specs/control-api.md`, "Local IPC"): a Unix domain socket in a user-private
//! directory, or on Windows a named pipe that only the current user can open and that
//! refuses remote clients.
//!
//! - Peers are identified by the OS: a peer that does not run as the current user is
//!   refused before it reaches the API, so every accepted peer is reported as a
//!   same-user IPC peer (`PeerInfo::same_user`), which `session.hello` authenticates.
//! - Framing ([`frame`]): the server opens an accepted connection with the 4-byte
//!   preamble [`PREAMBLE`]; then each message is a 4-byte big-endian header (bit 31
//!   clear, the low 31 bits the length) and that many bytes of UTF-8 JSON. A header
//!   with bit 31 set is a close frame: a big-endian `u16` code (the WebSocket
//!   transport's codes, [`close_code`]) and an optional UTF-8 reason. A refused
//!   connection gets a close frame instead of the preamble.
//! - A message is never buffered past the connection's current limit (4 KiB before
//!   authentication): a longer one is skipped as it arrives and reported as
//!   `Oversized`, and the connection keeps working.
//! - Threads: one accept thread while started, and a reader and a writer per
//!   connection, each blocked in the OS until there is something to do: no periodic
//!   wake-up. A connection that used its inbox quota is not read until the API thread
//!   drains it; output to a peer that does not read is bounded and answers `Full`.
//!   Nothing exists until [`ControlTransport::start`] (ADR-0013).
//!
//! [`IpcClient`] is the matching blocking client (`rbs-cli`), which checks that the
//! server runs as the same user before it sends anything.

mod client;
mod conn;
pub mod frame;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

use rbs_api::transport::TransportError;
use rbs_api::{CloseReason, ControlTransport, TransportInbox};

pub use client::{ClientError, IpcClient, Received};
pub use frame::PREAMBLE;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

/// Where the endpoint is: a socket path (Unix) or a pipe name (Windows,
/// `\\.\pipe\...`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(pub PathBuf);

impl Endpoint {
    /// The current user's endpoint: on Linux `$XDG_RUNTIME_DIR/rbs/control.sock` (or
    /// `/tmp/rbs-<uid>/control.sock` without it), on macOS
    /// `$TMPDIR/rbs/control.sock`, on Windows `\\.\pipe\rbs-control-<user SID>`.
    pub fn for_current_user() -> Result<Endpoint, String> {
        platform::default_endpoint().map(Endpoint)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}

/// Settings of the IPC transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcConfig {
    /// The endpoint to serve.
    pub endpoint: Endpoint,
    /// Output bytes queued per connection before `try_send` answers `Full` (one message
    /// is always taken when nothing is queued).
    pub max_queued_bytes: usize,
    /// How long `stop` lets the writers send their close frames (all together) before
    /// cutting the connections.
    pub stop_grace: std::time::Duration,
}

impl IpcConfig {
    /// Serves `endpoint` with the default limits.
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            max_queued_bytes: 1024 * 1024,
            stop_grace: conn::STOP_GRACE,
        }
    }
}

/// The IPC transport adapter.
pub struct IpcTransport {
    config: IpcConfig,
    running: Option<Running>,
}

struct Running {
    stop: Arc<AtomicBool>,
    listener: Arc<platform::Listener>,
    accept: JoinHandle<()>,
    conns: Arc<Mutex<Vec<conn::Live>>>,
}

impl IpcTransport {
    /// A transport that serves `config` once started.
    pub fn new(config: IpcConfig) -> Self {
        Self {
            config,
            running: None,
        }
    }

    /// The endpoint it serves.
    pub fn endpoint(&self) -> &Endpoint {
        &self.config.endpoint
    }
}

impl ControlTransport for IpcTransport {
    fn start(&mut self, inbox: TransportInbox) -> Result<(), TransportError> {
        if self.running.is_some() {
            return Err(TransportError("already running".into()));
        }
        let listener = Arc::new(
            platform::Listener::bind(&self.config.endpoint.0)
                .map_err(|e| TransportError(format!("{}: {e}", self.config.endpoint)))?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let conns = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let (stop, listener, conns) = (stop.clone(), listener.clone(), conns.clone());
            let max_queued = self.config.max_queued_bytes;
            std::thread::Builder::new()
                .name("rbs-api-ipc accept".into())
                .spawn(move || accept_loop(&listener, &stop, &inbox, &conns, max_queued))
                .map_err(|e| TransportError(format!("cannot start the IPC thread: {e}")))?
        };
        self.running = Some(Running {
            stop,
            listener,
            accept,
            conns,
        });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(r) = self.running.take() else {
            return;
        };
        r.stop.store(true, Ordering::Release);
        r.listener.wake();
        let _ = r.accept.join();
        // Nothing is accepted any more: close what is open and wait for its threads.
        let live = std::mem::take(&mut *r.conns.lock().unwrap_or_else(PoisonError::into_inner));
        for c in &live {
            c.stop(CloseReason::ServerStopping);
        }
        // One grace period for all of them, not one each.
        let deadline = std::time::Instant::now() + self.config.stop_grace;
        for c in live {
            c.join(deadline);
        }
        r.listener.remove();
    }
}

impl Drop for IpcTransport {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(
    listener: &platform::Listener,
    stop: &AtomicBool,
    inbox: &TransportInbox,
    conns: &Mutex<Vec<conn::Live>>,
    max_queued: usize,
) {
    loop {
        let accepted = listener.accept();
        if stop.load(Ordering::Acquire) {
            return;
        }
        let stream = match accepted {
            Ok(Some(s)) => s,
            // A peer of another user, or one that left at once: refused, keep going.
            Ok(None) => continue,
            Err(_) => {
                // The listener is broken (out of descriptors, ...). Wait for the next
                // peer rather than spin; `stop` wakes the accept.
                if listener.wait_after_error() {
                    continue;
                }
                return;
            }
        };
        let mut list = conns.lock().unwrap_or_else(PoisonError::into_inner);
        list.retain(|c| !c.finished());
        if let Some(live) = conn::open(stream, inbox, max_queued) {
            list.push(live);
        }
    }
}

/// The close code for a reason the server closes with: the WebSocket transport's
/// codes, so clients of both share one table.
pub const fn close_code(reason: CloseReason) -> u16 {
    match reason {
        CloseReason::ServerStopping => 1001,
        CloseReason::MessageTooLarge => 1009,
        CloseReason::HandshakeTimeout => 4001,
        CloseReason::AuthFailed => 4002,
        CloseReason::UnsupportedProtocol => 4003,
        CloseReason::TooManyConnections => 4004,
        CloseReason::Revoked => 4005,
        CloseReason::TooSlow => 4006,
    }
}

/// The reason for a close code from the server, if it is one of [`close_code`]'s.
pub const fn close_reason(code: u16) -> Option<CloseReason> {
    Some(match code {
        1001 => CloseReason::ServerStopping,
        1009 => CloseReason::MessageTooLarge,
        4001 => CloseReason::HandshakeTimeout,
        4002 => CloseReason::AuthFailed,
        4003 => CloseReason::UnsupportedProtocol,
        4004 => CloseReason::TooManyConnections,
        4005 => CloseReason::Revoked,
        4006 => CloseReason::TooSlow,
        _ => return None,
    })
}

/// The reason text sent with a close code (stable, for logs).
pub(crate) const fn close_text(reason: CloseReason) -> &'static str {
    match reason {
        CloseReason::ServerStopping => "server_stopping",
        CloseReason::MessageTooLarge => "message_too_large",
        CloseReason::HandshakeTimeout => "handshake_timeout",
        CloseReason::AuthFailed => "auth_failed",
        CloseReason::UnsupportedProtocol => "unsupported_protocol",
        CloseReason::TooManyConnections => "too_many_connections",
        CloseReason::Revoked => "revoked",
        CloseReason::TooSlow => "too_slow",
    }
}

/// The close code a peer sends when it closes normally.
pub const NORMAL_CLOSE: u16 = 1000;
