//! Network access over TLS (ADR-0046): who gets plain WebSocket, who must use TLS, the
//! caps that keep remote peers from locking out local ones, and the key pin.
//!
//! "Remote" tests connect to this machine's own non-loopback address, which the server
//! cannot tell from another computer; where there is none they return early.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

mod common;

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use common::{Tls, WsClient, exchange, remote_address, upgrade_request};
use rbs_api::conformance::{Client, ConnectOptions, LIMIT};
use rbs_api::transport::ConnectionBudget;
use rbs_api::wake::Wake;
use rbs_api::{ControlTransport, PeerInfo, TransportEvent, TransportInbox, UpgradePolicy};
use rbs_api_ws::{TlsIdentity, WsConfig, WsTransport, tls};

fn identity() -> TlsIdentity {
    let g = tls::generate(&["studio.local".into()], &[], 1_800_000_000).unwrap();
    TlsIdentity::from_pem(&g.cert_pem, &g.key_pem).unwrap()
}

struct Rig {
    t: WsTransport,
    inbox: TransportInbox,
    wake: Arc<Wake>,
    pin: String,
    port: u16,
}

impl Rig {
    fn start(edit: impl FnOnce(&mut WsConfig)) -> Self {
        Self::start_with(4, 8, edit)
    }

    fn start_with(max: usize, quota: usize, edit: impl FnOnce(&mut WsConfig)) -> Self {
        let identity = identity();
        let pin = identity.key_pin().to_owned();
        let mut config = WsConfig::network(0, identity);
        config.policy = UpgradePolicy::default();
        edit(&mut config);
        let wake = Arc::new(Wake::new());
        let inbox = TransportInbox::new(
            wake.clone(),
            Arc::new(ConnectionBudget::new(max)),
            quota,
            4096,
        );
        let mut t = WsTransport::new(config);
        t.start(inbox.clone()).unwrap();
        let port = t.local_addrs()[0].port();
        Self {
            t,
            inbox,
            wake,
            pin,
            port,
        }
    }

    fn at(&self, ip: impl Into<IpAddr>) -> SocketAddr {
        SocketAddr::new(ip.into(), self.port)
    }

    fn tls(&self) -> Tls {
        Tls::pinned(&self.pin)
    }

    fn next(&self) -> TransportEvent {
        let deadline = std::time::Instant::now() + LIMIT;
        loop {
            if let Some(e) = self.inbox.pop() {
                return e;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "no transport event within {LIMIT:?}");
            self.wake.wait(Some(left));
        }
    }

    fn peer(&self) -> PeerInfo {
        match self.next() {
            TransportEvent::Connected { peer, .. } => peer,
            other => panic!("expected Connected, got {other:?}"),
        }
    }
}

/// The server hung up: end of stream or a reset, never a timeout.
fn hung_up(s: &mut TcpStream) -> bool {
    // macOS refuses socket options (EINVAL) once the peer has reset the connection.
    if s.set_read_timeout(Some(LIMIT)).is_err() {
        return true;
    }
    let mut b = [0u8; 64];
    loop {
        match s.read(&mut b) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e) => return !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
        }
    }
}

#[test]
fn the_network_listener_is_dual_stack_and_serves_loopback_plain_websocket() {
    let rig = Rig::start(|_| {});
    assert!(
        rig.t.local_addrs()[0].ip().is_unspecified(),
        "{:?}",
        rig.t.local_addrs()
    );
    // IPv4 loopback reaches the `[::]` socket as `::ffff:127.0.0.1`: still loopback.
    let _c = WsClient::open(rig.at([127, 0, 0, 1]), &ConnectOptions::default(), None).unwrap();
    let peer = rig.peer();
    assert!(peer.loopback && !peer.secure, "{peer:?}");
    assert_eq!(peer.address.as_deref(), Some("127.0.0.1"));
    if rig.t.local_addrs()[0].is_ipv6() {
        let _c = WsClient::open(
            rig.at(Ipv6Addr::LOCALHOST),
            &ConnectOptions::default(),
            None,
        )
        .unwrap();
        let peer = rig.peer();
        assert!(peer.loopback && !peer.secure, "{peer:?}");
    }
}

#[test]
fn loopback_may_use_tls_too_and_is_then_secure() {
    let rig = Rig::start(|_| {});
    let _c = WsClient::open(
        rig.at([127, 0, 0, 1]),
        &ConnectOptions::default(),
        Some(&rig.tls()),
    )
    .unwrap();
    let peer = rig.peer();
    assert!(peer.loopback && peer.secure, "{peer:?}");
}

#[test]
fn plain_http_from_another_computer_is_told_to_use_tls() {
    let Some(ip) = remote() else { return };
    let rig = Rig::start(|_| {});
    let addr = rig.at(ip);
    let answer = exchange(
        addr,
        None,
        &upgrade_request(addr, &ConnectOptions::default()),
    )
    .unwrap();
    assert!(answer.starts_with("HTTP/1.1 426 "), "{answer}");
    assert!(answer.ends_with("must use wss:// (TLS).\n"), "{answer}");
    assert!(rig.inbox.is_empty());
}

#[test]
fn another_computer_connects_over_tls_and_is_secure_and_not_loopback() {
    let Some(ip) = remote() else { return };
    let rig = Rig::start(|_| {});
    let mut c = WsClient::open(rig.at(ip), &ConnectOptions::default(), Some(&rig.tls())).unwrap();
    let mut peer = rig.peer();
    assert!(!peer.loopback && peer.secure, "{peer:?}");
    assert_eq!(peer.address, Some(ip.to_string()));
    // The `Host` the client sent was the address it connected to.
    assert_eq!(peer.host.take(), Some(rig.at(ip).to_string()));
    c.send("hi");
    match rig.next() {
        TransportEvent::Message { text, .. } => assert_eq!(&*text, "hi"),
        other => panic!("expected Message, got {other:?}"),
    }
}

#[test]
fn a_host_naming_another_address_is_refused_with_a_readable_answer_over_tls() {
    let Some(ip) = remote() else { return };
    let rig = Rig::start(|_| {});
    let addr = rig.at(ip);
    let options = ConnectOptions {
        host: Some(format!("evil.example:{}", rig.port)),
        ..ConnectOptions::default()
    };
    let answer = exchange(addr, Some(&rig.tls()), &upgrade_request(addr, &options)).unwrap();
    assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
    // `[::]` itself is never a valid `Host`.
    let options = ConnectOptions {
        host: Some(format!("[::]:{}", rig.port)),
        ..ConnectOptions::default()
    };
    let answer = exchange(addr, Some(&rig.tls()), &upgrade_request(addr, &options)).unwrap();
    assert!(answer.starts_with("HTTP/1.1 403 "), "{answer}");
    assert!(rig.inbox.is_empty());
}

#[test]
fn a_client_with_the_wrong_pin_gives_up_before_sending_anything() {
    let rig = Rig::start(|_| {});
    let other = identity();
    let wrong = Tls::pinned(other.key_pin());
    let err = WsClient::open(
        rig.at([127, 0, 0, 1]),
        &ConnectOptions::default(),
        Some(&wrong),
    )
    .err()
    .expect("refused");
    assert!(err.contains("pin"), "{err}");
    assert!(rig.inbox.is_empty());
}

#[test]
fn a_client_sending_one_byte_at_a_time_completes_the_handshake() {
    let rig = Rig::start(|_| {});
    let tls = Tls {
        trickle: true,
        ..rig.tls()
    };
    let mut c = WsClient::open(
        rig.at([127, 0, 0, 1]),
        &ConnectOptions::default(),
        Some(&tls),
    )
    .unwrap();
    assert!(rig.peer().secure);
    c.send("slow");
    match rig.next() {
        TransportEvent::Message { text, .. } => assert_eq!(&*text, "slow"),
        other => panic!("expected Message, got {other:?}"),
    }
}

#[test]
fn a_stalled_tls_handshake_is_closed_at_the_deadline() {
    let rig = Rig::start(|c| c.upgrade_timeout = Duration::ZERO);
    let mut s = TcpStream::connect(rig.at([127, 0, 0, 1])).unwrap();
    // The first bytes of a ClientHello, and no more.
    s.write_all(&[0x16, 0x03, 0x01]).unwrap();
    assert!(hung_up(&mut s));
    assert!(rig.inbox.is_empty());
}

#[test]
fn a_burst_over_tls_is_delivered_whole_across_pauses() {
    const QUOTA: usize = 2;
    // About 120 KiB in one write: several TLS records and several read passes, with
    // the connection paused on its quota again and again.
    const SENT: usize = 200;
    let rig = Rig::start_with(4, QUOTA, |_| {});
    let mut c = WsClient::open(
        rig.at([127, 0, 0, 1]),
        &ConnectOptions::default(),
        Some(&rig.tls()),
    )
    .unwrap();
    let _ = rig.peer();
    let text = |i: usize| format!("m{i:04}{}", "x".repeat(600));
    let mut out = Vec::new();
    for i in 0..SENT {
        WsClient::encode(true, 0x1, text(i).as_bytes(), &mut out);
    }
    c.write(&out);
    for i in 0..SENT {
        // Reading the inbox resumes the connection; what rustls or the frame reader
        // already holds raises no socket event and must be read from there.
        match rig.next() {
            TransportEvent::Message { text: t, .. } => assert_eq!(t, text(i)),
            other => panic!("expected Message, got {other:?}"),
        }
    }
}

/// Opens `count` TCP connections from this machine's non-loopback address, each
/// starting a TLS handshake it never finishes, so an admitted one stays upgrading
/// (the deadline is far away). Returns the first `count - 1` and whether the last was
/// closed at accept: closed without a byte. The earlier ones must still be open:
/// the server accepts in order, so it has seen them by the time it closed the last.
fn remote_until_refused(rig: &Rig, ip: IpAddr, count: usize) -> (Vec<TcpStream>, bool) {
    let mut kept = Vec::new();
    for _ in 0..count {
        let mut s = TcpStream::connect(rig.at(ip)).unwrap();
        let _ = s.write_all(&[0x16, 0x03, 0x01]);
        kept.push(s);
    }
    let mut last = kept.pop().unwrap();
    let refused = closed_without_answer(&mut last);
    if refused {
        for s in &mut kept {
            s.set_nonblocking(true).unwrap();
            let mut b = [0u8; 1];
            match s.read(&mut b) {
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                other => panic!("an admitted socket was closed or answered: {other:?}"),
            }
        }
    }
    (kept, refused)
}

/// The server closed `s` without sending anything; false when it answered or kept
/// it open.
fn closed_without_answer(s: &mut TcpStream) -> bool {
    // macOS refuses socket options (EINVAL) once the peer has reset the connection.
    if s.set_read_timeout(Some(LIMIT)).is_err() {
        return true;
    }
    let mut b = [0u8; 64];
    match s.read(&mut b) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
    }
}

/// This machine's non-loopback address, or `None` with a note: the test then has
/// nothing to connect from.
fn remote() -> Option<IpAddr> {
    let ip = remote_address();
    if ip.is_none() {
        eprintln!("skipped: this machine has no non-loopback address to connect from");
    }
    ip
}

#[test]
fn remote_peers_cannot_lock_out_loopback() {
    let Some(ip) = remote() else { return };
    // Room for 4 sockets, 2 of them for loopback only.
    let rig = Rig::start(|c| {
        c.max_sockets = 4;
        c.loopback_reserve = 2;
    });
    let (held, refused) = remote_until_refused(&rig, ip, 3);
    assert!(refused, "the third remote socket was taken");
    assert_eq!(held.len(), 2);
    let _a = WsClient::open(rig.at([127, 0, 0, 1]), &ConnectOptions::default(), None).unwrap();
    let _b = WsClient::open(rig.at([127, 0, 0, 1]), &ConnectOptions::default(), None).unwrap();
    assert!(rig.peer().loopback);
    assert!(rig.peer().loopback);
}

#[test]
fn remote_peers_are_capped_per_address_and_while_upgrading() {
    let Some(ip) = remote() else { return };
    let rig = Rig::start(|c| c.max_per_address = 2);
    let (_held, refused) = remote_until_refused(&rig, ip, 3);
    assert!(refused, "a third socket from one address was taken");
    drop(rig);

    let rig = Rig::start(|c| c.max_upgrading = 2);
    let (_held, refused) = remote_until_refused(&rig, ip, 3);
    assert!(refused, "a third upgrading remote socket was taken");
    // Loopback peers are not counted.
    let _c = WsClient::open(rig.at([127, 0, 0, 1]), &ConnectOptions::default(), None).unwrap();
    assert!(rig.peer().loopback);
}

#[test]
fn without_tls_only_loopback_is_served_and_a_tls_client_gets_no_handshake() {
    let mut config = WsConfig::loopback(0);
    config.addresses.truncate(1);
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake, Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let mut t = WsTransport::new(config);
    t.start(inbox.clone()).unwrap();
    let tls = Tls::pinned(identity().key_pin());
    assert!(WsClient::open(t.local_addrs()[0], &ConnectOptions::default(), Some(&tls)).is_err());
    assert!(inbox.is_empty());

    let mut config = WsConfig::network(0, identity());
    config.tls = None;
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake, Arc::new(ConnectionBudget::new(4)), 8, 4096);
    assert!(WsTransport::new(config).start(inbox).is_err());
}
