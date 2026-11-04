//! WebSocket transport for the RBS control API (ADR-0044, `docs/specs/control-api.md`).
//!
//! One I/O thread, started by [`ControlTransport::start`] and joined by
//! [`ControlTransport::stop`], accepts connections, completes the HTTP upgrade after
//! the `Origin` and `Host` check ([`rbs_api::UpgradePolicy`]), reads RFC 6455 frames
//! and writes answers and events. It sleeps in the OS readiness poller (epoll, kqueue,
//! IOCP) until a socket is ready, the API thread queued output or freed a paused
//! connection, or an upgrade's deadline passes: no periodic wake-up. Nothing exists
//! until `start` (ADR-0013).
//!
//! - Loopback by default ([`WsConfig::loopback`]). Network access ([`WsConfig::network`],
//!   ADR-0046) listens on all interfaces at the same port and needs a [`TlsIdentity`]:
//!   each connection's first byte decides between TLS and plain HTTP, and plain HTTP is
//!   served only to loopback peers (others get `426`). Without TLS, an address that is
//!   not loopback is refused at `start`.
//! - Remote peers cannot lock out local ones: the last sockets are kept for loopback
//!   peers, and remote connections still upgrading and sockets per remote address are
//!   capped at accept.
//! - A message is never buffered past the connection's current limit (4 KiB before
//!   authentication): a longer one is skipped as it arrives and reported as
//!   `Oversized`.
//! - A connection that used its inbox quota is not read until the API thread drains
//!   it; output to a peer that does not read is bounded and answers `Full`.
//! - Close codes: see [`close_code`].

mod frame;
mod handshake;
mod io;
pub mod tls;

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use polling::Poller;
use rbs_api::transport::TransportError;
use rbs_api::{CloseReason, ControlTransport, TransportInbox, UpgradePolicy};

pub use handshake::{SUBPROTOCOL, accept_key};
pub use tls::TlsIdentity;

/// Settings of the WebSocket transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsConfig {
    /// Addresses to listen on. Without `tls` each must be loopback. An unspecified
    /// IPv6 address (`[::]`) is bound dual-stack, so it also takes IPv4; where IPv6 is
    /// off, `0.0.0.0` is bound instead.
    pub addresses: Vec<SocketAddr>,
    /// The `Origin` allow list and host names. The bound addresses are added at start.
    pub policy: UpgradePolicy,
    /// How long a connection may take to send its upgrade request.
    pub upgrade_timeout: Duration,
    /// Remote connections that may be in their TLS handshake or upgrade request at
    /// once; more are closed at accept. Loopback peers are not counted.
    pub max_upgrading: usize,
    /// Output bytes queued per connection before `try_send` answers `Full` (one message
    /// is always taken when nothing is queued). A connection with this much queued is
    /// not read until it drains, so a peer that never reads cannot grow it with pings.
    pub max_queued_bytes: usize,
    /// Sockets open at once, in any state (upgrading, open, closing); more are closed
    /// at accept. Set above the API's connection cap plus `max_upgrading`.
    pub max_sockets: usize,
    /// Of `max_sockets`, how many only loopback peers can take.
    pub loopback_reserve: usize,
    /// Sockets per remote address (IPv6: per /64), in any state.
    pub max_per_address: usize,
    /// TLS for network access (ADR-0046); `None` serves plain WebSocket on loopback.
    pub tls: Option<TlsIdentity>,
}

impl WsConfig {
    /// Listens on `127.0.0.1` and `::1` at `port` (`::1` is skipped where IPv6 is off).
    /// Give a fixed port: with 0 the two addresses get different ports.
    pub fn loopback(port: u16) -> Self {
        Self::on(vec![
            SocketAddr::from(([127, 0, 0, 1], port)),
            SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        ])
    }

    /// Network access (ADR-0046): listens on every interface at `port` (one dual-stack
    /// socket), TLS with `identity` for every peer, plain WebSocket for loopback peers.
    pub fn network(port: u16, identity: TlsIdentity) -> Self {
        Self {
            tls: Some(identity),
            ..Self::on(vec![SocketAddr::from((
                std::net::Ipv6Addr::UNSPECIFIED,
                port,
            ))])
        }
    }

    /// Listens on `addresses`.
    pub fn on(addresses: Vec<SocketAddr>) -> Self {
        Self {
            addresses,
            policy: UpgradePolicy::default(),
            upgrade_timeout: Duration::from_secs(10),
            max_upgrading: 16,
            max_queued_bytes: 1024 * 1024,
            max_sockets: 128,
            loopback_reserve: 32,
            max_per_address: 8,
            tls: None,
        }
    }
}

/// The WebSocket transport adapter.
pub struct WsTransport {
    config: WsConfig,
    running: Option<Running>,
    local: Vec<SocketAddr>,
}

struct Running {
    poller: Arc<Poller>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl WsTransport {
    /// An adapter that listens once started.
    pub fn new(config: WsConfig) -> Self {
        Self {
            config,
            running: None,
            local: Vec::new(),
        }
    }

    /// The addresses it listens on (with the ports the OS chose for port 0), while
    /// running.
    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.local
    }
}

impl ControlTransport for WsTransport {
    fn start(&mut self, inbox: TransportInbox) -> Result<(), TransportError> {
        if self.running.is_some() {
            return Err(TransportError("already running".into()));
        }
        if self.config.tls.is_none()
            && let Some(a) = self.config.addresses.iter().find(|a| !a.ip().is_loopback())
        {
            return Err(TransportError(format!(
                "{a} is not a loopback address; network access needs TLS"
            )));
        }
        let mut listeners = Vec::new();
        let mut v6_error = None;
        for addr in &self.config.addresses {
            match bind(addr) {
                Ok(l) => listeners.push(l),
                // `::1` where IPv6 is off: fine as long as another address is bound.
                // Any other error fails, above all `AddrInUse`: a process holding
                // `[::1]:port` would otherwise get the tokens of clients that try
                // `localhost` over IPv6 first.
                Err(e) if addr.is_ipv6() && ipv6_unavailable(&e) => {
                    v6_error = Some(format!("{addr}: {e}"));
                    // No IPv6: all interfaces over IPv4.
                    if addr.ip().is_unspecified() {
                        let v4 = SocketAddr::from(([0, 0, 0, 0], addr.port()));
                        listeners.push(
                            TcpListener::bind(v4).map_err(|e| {
                                TransportError(format!("cannot listen on {v4}: {e}"))
                            })?,
                        );
                    }
                }
                Err(e) => return Err(TransportError(format!("cannot listen on {addr}: {e}"))),
            }
        }
        if listeners.is_empty() {
            return Err(TransportError(format!(
                "no address to listen on{}",
                v6_error.map(|e| format!(" ({e})")).unwrap_or_default()
            )));
        }
        let mut local = Vec::new();
        for l in &listeners {
            l.set_nonblocking(true)
                .map_err(|e| TransportError(format!("listener: {e}")))?;
            local.push(
                l.local_addr()
                    .map_err(|e| TransportError(format!("listener: {e}")))?,
            );
        }
        let mut policy = self.config.policy.clone();
        // An unspecified address is never a `Host`; each connection's own local
        // address is checked instead.
        policy.bound_addresses.extend(
            local
                .iter()
                .map(SocketAddr::ip)
                .filter(|ip| !ip.is_unspecified()),
        );
        let poller = Arc::new(Poller::new().map_err(|e| TransportError(format!("poller: {e}")))?);
        let stop = Arc::new(AtomicBool::new(false));
        let io = io::Io::new(
            poller.clone(),
            stop.clone(),
            listeners,
            inbox,
            io::Settings {
                policy,
                upgrade_timeout: self.config.upgrade_timeout,
                max_upgrading: self.config.max_upgrading,
                max_queued_bytes: self.config.max_queued_bytes,
                max_sockets: self.config.max_sockets,
                loopback_reserve: self.config.loopback_reserve,
                max_per_address: self.config.max_per_address,
                tls: self.config.tls.as_ref().map(TlsIdentity::server_config),
            },
        )
        .map_err(|e| TransportError(format!("poller: {e}")))?;
        let thread = std::thread::Builder::new()
            .name("rbs-api-ws".into())
            .spawn(move || io.run())
            .map_err(|e| TransportError(format!("I/O thread: {e}")))?;
        self.local = local;
        self.running = Some(Running {
            poller,
            stop,
            thread,
        });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(r) = self.running.take() else {
            return;
        };
        r.stop.store(true, Ordering::Release);
        // A poller that cannot be notified is broken; the thread then still sees the
        // flag at its next event or deadline.
        let _ = r.poller.notify();
        let _ = r.thread.join();
        self.local.clear();
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Binds `addr`; an unspecified IPv6 address dual-stack (`IPV6_V6ONLY` off, which std
/// cannot set before bind, and which Windows turns on by default).
fn bind(addr: &SocketAddr) -> std::io::Result<TcpListener> {
    if !(addr.is_ipv6() && addr.ip().is_unspecified()) {
        return TcpListener::bind(addr);
    }
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    #[cfg(not(windows))]
    socket.set_reuse_address(true)?;
    socket.bind(&(*addr).into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

/// True for a bind error that means the system has no IPv6.
fn ipv6_unavailable(e: &std::io::Error) -> bool {
    // EAFNOSUPPORT: 97 on Linux, 47 on macOS and the BSDs; WSAEAFNOSUPPORT: 10047.
    const NO_FAMILY: [i32; 3] = [97, 47, 10047];
    matches!(
        e.kind(),
        std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::Unsupported
    ) || e.raw_os_error().is_some_and(|c| NO_FAMILY.contains(&c))
}

/// The WebSocket close code for a reason the server closes with: 1001 (going away)
/// when the server stops, 1009 (message too big) for a message before authentication
/// over the limit, and codes in the private range 4000-4999 for the others. The close
/// frame's reason text is the reason's name in `snake_case`.
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

/// The reason for a close code from [`close_code`].
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

/// The close frame's reason text.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_map_back() {
        for r in [
            CloseReason::ServerStopping,
            CloseReason::MessageTooLarge,
            CloseReason::HandshakeTimeout,
            CloseReason::AuthFailed,
            CloseReason::UnsupportedProtocol,
            CloseReason::TooManyConnections,
            CloseReason::Revoked,
            CloseReason::TooSlow,
        ] {
            assert_eq!(close_reason(close_code(r)), Some(r));
        }
        assert_eq!(close_reason(1000), None);
    }

    #[test]
    fn a_network_address_is_refused_without_tls() {
        let mut t = WsTransport::new(WsConfig::on(vec![
            "0.0.0.0:0"
                .parse()
                .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0))),
        ]));
        let wake = Arc::new(rbs_api::wake::Wake::new());
        let budget = Arc::new(rbs_api::transport::ConnectionBudget::new(1));
        let inbox = TransportInbox::new(wake, budget, 1, 4096);
        let err = t.start(inbox);
        assert!(err.is_err());
    }
}
