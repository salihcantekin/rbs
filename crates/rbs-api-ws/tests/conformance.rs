//! The WebSocket transport passes the transport conformance suite, through a minimal
//! blocking RFC 6455 client.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use rbs_api::conformance::{
    self, ALLOWED_ORIGIN, Client, ClientEvent, ConnectOptions, HOST_NAME, Harness, LIMIT,
};
use rbs_api::transport::{ConnectionBudget, TransportError};
use rbs_api::wake::Wake;
use rbs_api::{ControlTransport, TransportEvent, TransportInbox, TransportKind, UpgradePolicy};
use rbs_api_ws::{SUBPROTOCOL, WsConfig, WsTransport, accept_key, close_reason};

struct WsClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl WsClient {
    fn open(addr: SocketAddr, options: &ConnectOptions) -> Result<Self, String> {
        let stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let mut req = format!(
            "GET / HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {SUBPROTOCOL}\r\n"
        );
        if !options.no_host {
            let host = options.host.clone().unwrap_or_else(|| addr.to_string());
            req.push_str(&format!("Host: {host}\r\n"));
        }
        if let Some(o) = &options.origin {
            req.push_str(&format!("Origin: {o}\r\n"));
        }
        req.push_str("\r\n");
        writer
            .write_all(req.as_bytes())
            .map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).map_err(|e| e.to_string())?;
        let mut accept = None;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).map_err(|e| e.to_string())?;
            if n == 0 || line == "\r\n" {
                break;
            }
            if let Some(v) = line.strip_prefix("Sec-WebSocket-Accept: ") {
                accept = Some(v.trim().to_owned());
            }
        }
        if !status.starts_with("HTTP/1.1 101") {
            return Err(status);
        }
        assert_eq!(accept.as_deref(), Some(accept_key(key).as_str()));
        Ok(Self { reader, writer })
    }

    fn frame(&mut self, opcode: u8, payload: &[u8]) {
        let mask = [1u8, 2, 3, 4];
        let mut out = vec![0x80 | opcode];
        let len = payload.len();
        if len < 126 {
            out.push(0x80 | len as u8);
        } else if len <= 0xFFFF {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            out.push(0x80 | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        // The server may already have closed (a test of its close); that is not this
        // client's failure.
        let _ = self.writer.write_all(&out);
    }

    fn read_frame(&mut self) -> std::io::Result<(u8, Vec<u8>)> {
        let mut h = [0u8; 2];
        self.reader.read_exact(&mut h)?;
        assert_eq!(h[1] & 0x80, 0, "server frames are not masked");
        let len = match h[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                self.reader.read_exact(&mut b)?;
                u64::from(u16::from_be_bytes(b))
            }
            127 => {
                let mut b = [0u8; 8];
                self.reader.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => u64::from(n),
        };
        let mut payload = vec![0; usize::try_from(len).unwrap()];
        self.reader.read_exact(&mut payload)?;
        Ok((h[0] & 0x0F, payload))
    }
}

impl Client for WsClient {
    fn send(&mut self, text: &str) {
        self.frame(0x1, text.as_bytes());
    }

    fn recv(&mut self, timeout: Duration) -> Option<ClientEvent> {
        self.reader
            .get_ref()
            .set_read_timeout(Some(timeout))
            .unwrap();
        loop {
            match self.read_frame() {
                Ok((0x1, payload)) => {
                    return Some(ClientEvent::Message(String::from_utf8(payload).unwrap()));
                }
                Ok((0x8, payload)) => {
                    let code =
                        (payload.len() >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]));
                    return Some(ClientEvent::Closed(code.and_then(close_reason)));
                }
                Ok((0xA, _)) => {}
                Ok((op, _)) => panic!("unexpected opcode {op}"),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return None;
                }
                Err(_) => return Some(ClientEvent::Closed(None)),
            }
        }
    }

    fn close(&mut self) {
        self.frame(0x8, &1000u16.to_be_bytes());
        let _ = self.writer.shutdown(Shutdown::Write);
    }
}

#[derive(Default)]
struct WsHarness {
    addr: Option<SocketAddr>,
}

impl WsHarness {
    fn config() -> WsConfig {
        let mut config = WsConfig::on(vec![SocketAddr::from(([127, 0, 0, 1], 0))]);
        config.policy = UpgradePolicy {
            allowed_origins: vec![ALLOWED_ORIGIN.into()],
            host_names: vec![HOST_NAME.into()],
            bound_addresses: Vec::new(),
        };
        config
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
        let mut t = WsTransport::new(Self::config());
        t.start(inbox)?;
        self.addr = t.local_addrs().first().copied();
        Ok(Box::new(t))
    }

    fn connect(&mut self, options: &ConnectOptions) -> Result<WsClient, String> {
        WsClient::open(self.addr.expect("started"), options)
    }
}

#[test]
fn the_websocket_transport_passes_the_conformance_suite() {
    conformance::run_all(&mut WsHarness::default());
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
    let mut c = WsClient::open(t.local_addrs()[0], &ConnectOptions::default()).unwrap();
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
    let mut c = WsClient::open(t.local_addrs()[0], &ConnectOptions::default()).unwrap();
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
    assert_eq!(hello["result"]["protocol"], "1.0", "{hello}");
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
