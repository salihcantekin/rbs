//! The WebSocket transport passes the transport conformance suite, through a minimal
//! blocking RFC 6455 client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

mod common;

use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use common::{Tls, WsClient};
use rbs_api::conformance::{
    self, ALLOWED_ORIGIN, Client, ClientEvent, ConnectOptions, HOST_NAME, Harness, LIMIT,
};
use rbs_api::transport::{ConnectionBudget, TransportError};
use rbs_api::wake::Wake;
use rbs_api::{ControlTransport, TransportEvent, TransportInbox, TransportKind, UpgradePolicy};
use rbs_api_ws::{WsConfig, WsTransport, tls};

/// Plain WebSocket on loopback, or with `tls` network access (every interface, TLS)
/// with a loopback client speaking TLS.
#[derive(Default)]
struct WsHarness {
    addr: Option<SocketAddr>,
    tls: Option<Tls>,
    identity: Option<rbs_api_ws::TlsIdentity>,
}

fn policy() -> UpgradePolicy {
    UpgradePolicy {
        allowed_origins: vec![ALLOWED_ORIGIN.into()],
        host_names: vec![HOST_NAME.into()],
        bound_addresses: Vec::new(),
    }
}

impl WsHarness {
    fn config() -> WsConfig {
        let mut config = WsConfig::on(vec![SocketAddr::from(([127, 0, 0, 1], 0))]);
        config.policy = policy();
        config
    }

    fn over_tls(tls12: bool) -> Self {
        let generated = tls::generate(&["studio.local".into()], &[], 1_800_000_000).unwrap();
        let identity =
            rbs_api_ws::TlsIdentity::from_pem(&generated.cert_pem, &generated.key_pem).unwrap();
        Self {
            addr: None,
            tls: Some(Tls {
                tls12,
                ..Tls::pinned(identity.key_pin())
            }),
            identity: Some(identity),
        }
    }
}

impl Harness for WsHarness {
    type Client = WsClient;

    fn kind(&self) -> TransportKind {
        TransportKind::WebSocket
    }

    fn checks_upgrade(&self) -> bool {
        true
    }

    fn start(
        &mut self,
        inbox: TransportInbox,
    ) -> Result<Box<dyn ControlTransport>, TransportError> {
        let config = match &self.identity {
            None => Self::config(),
            Some(identity) => WsConfig {
                policy: policy(),
                ..WsConfig::network(0, identity.clone())
            },
        };
        let mut t = WsTransport::new(config);
        t.start(inbox)?;
        // Network access listens on `[::]`; the client connects over IPv4 loopback.
        self.addr = t
            .local_addrs()
            .first()
            .map(|a| SocketAddr::from(([127, 0, 0, 1], a.port())));
        Ok(Box::new(t))
    }

    fn connect(&mut self, options: &ConnectOptions) -> Result<WsClient, String> {
        WsClient::open(self.addr.expect("started"), options, self.tls.as_ref())
    }
}

#[test]
fn the_websocket_transport_passes_the_conformance_suite() {
    conformance::run_all(&mut WsHarness::default());
}

#[test]
fn the_websocket_transport_passes_the_conformance_suite_over_tls_1_3() {
    conformance::run_all(&mut WsHarness::over_tls(false));
}

#[test]
fn the_websocket_transport_passes_the_conformance_suite_over_tls_1_2() {
    conformance::run_all(&mut WsHarness::over_tls(true));
}

fn rig(config: WsConfig) -> (WsTransport, TransportInbox, Arc<Wake>) {
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake.clone(), Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let mut t = WsTransport::new(config);
    t.start(inbox.clone()).unwrap();
    (t, inbox, wake)
}

#[test]
fn a_peer_that_never_sends_its_upgrade_is_closed_at_the_deadline() {
    let mut config = WsHarness::config();
    config.upgrade_timeout = Duration::ZERO;
    let (t, inbox, _wake) = rig(config);
    let mut s = TcpStream::connect(t.local_addrs()[0]).unwrap();
    s.set_read_timeout(Some(LIMIT)).unwrap();
    let mut b = [0u8; 1];
    // EOF (or a reset), not a timeout: the server hung up.
    match s.read(&mut b) {
        Ok(0) => {}
        Err(e) => assert!(
            !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
            "{e}"
        ),
        Ok(n) => panic!("got {n} bytes"),
    }
    assert!(inbox.is_empty());
}

#[test]
fn a_ping_gets_a_pong_and_a_binary_message_closes_with_1003() {
    let (t, inbox, wake) = rig(WsHarness::config());
    let mut c = WsClient::open(t.local_addrs()[0], &ConnectOptions::default(), None).unwrap();
    wake.wait(Some(LIMIT));
    assert!(matches!(
        inbox.pop(),
        Some(TransportEvent::Connected { .. })
    ));
    c.frame(0x9, b"hi");
    assert_eq!(c.read_frame().unwrap(), (0xA, b"hi".to_vec()));
    c.frame(0x2, b"bin");
    let (op, payload) = c.read_frame().unwrap();
    assert_eq!(op, 0x8);
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1003);
}

#[test]
fn a_client_authenticates_with_the_api_server_over_websocket() {
    use rbs_api::request::ClientInfo;
    use rbs_api::{ApiOptions, ApiServer, ScopeSet, ServerConfig, Token, TokenEntry, TokenStore};

    let token = Token::from_bytes(&[7; 32]);
    let mut tokens = TokenStore::new();
    tokens.insert(TokenEntry {
        name: "Deck".into(),
        scopes: ScopeSet::PANEL,
        hash: token.hash(),
    });
    let config = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
    let (api, _endpoint) = ApiServer::start(config, tokens, ApiOptions::new(|| {})).unwrap();
    let mut t = WsTransport::new(WsHarness::config());
    t.start(api.inbox()).unwrap();
    let mut c = WsClient::open(t.local_addrs()[0], &ConnectOptions::default(), None).unwrap();
    let call = |c: &mut WsClient, text: String| -> serde_json::Value {
        c.send(&text);
        match c.recv(LIMIT) {
            Some(ClientEvent::Message(m)) => serde_json::from_str(&m).unwrap(),
            other => panic!("expected an answer, got {other:?}"),
        }
    };
    let hello = call(
        &mut c,
        r#"{"jsonrpc":"2.0","id":1,"method":"session.hello","params":{"protocols":["1.0"],"client":{"name":"t","version":"1"}}}"#.into(),
    );
    assert_eq!(
        hello["result"]["protocol"],
        rbs_api::session::PROTOCOL_VERSION,
        "{hello}"
    );
    let early = call(
        &mut c,
        r#"{"jsonrpc":"2.0","id":2,"method":"studio.get"}"#.into(),
    );
    assert_eq!(
        early["error"]["data"]["kind"], "not_authenticated",
        "{early}"
    );
    let auth = call(
        &mut c,
        format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"session.authenticate","params":{{"token":"{}"}}}}"#,
            token.expose()
        ),
    );
    assert_eq!(
        auth["result"]["scopes"],
        serde_json::json!(["read", "control"]),
        "{auth}"
    );
    t.stop();
    assert_eq!(
        c.recv(LIMIT),
        Some(ClientEvent::Closed(Some(
            rbs_api::CloseReason::ServerStopping
        )))
    );
    drop(api);
}

#[test]
fn a_taken_ipv6_loopback_port_fails_the_start() {
    // Someone else holds [::1]:port: starting on 127.0.0.1 alone would hand clients
    // that try ::1 first to that process.
    let Ok(squatter) = std::net::TcpListener::bind("[::1]:0") else {
        return; // No IPv6 here.
    };
    let port = squatter.local_addr().unwrap().port();
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake, Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let mut t = WsTransport::new(WsConfig::loopback(port));
    assert!(t.start(inbox).is_err());
}
