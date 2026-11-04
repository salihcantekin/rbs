//! Remote control as the app runs it (ADR-0044): a token made in the settings works over
//! the WebSocket transport on loopback, a revoked one disconnects its app, and turning
//! remote control off tells connected apps the server is going away. No studio is
//! needed: the handshake is answered by the API thread. Waits end when the answer
//! arrives; the read timeout is only a hang guard (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use rbs_api_ws::{SUBPROTOCOL, close_code};
use rbs_app::remote_control::{RemoteOptions, RemoteServer, StartError};
use rbs_remote_settings::{Access, RemoteProfile};
use serde_json::{Value, json};

/// Hang guard only.
const LIMIT: Duration = Duration::from_secs(20);

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

enum Got {
    Text(Value),
    Closed(Option<u16>),
}

impl Client {
    fn open(addr: SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        let mut writer = stream.try_clone().unwrap();
        write!(
            writer,
            "GET / HTTP/1.1\r\nHost: localhost:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: {SUBPROTOCOL}\r\n\r\n",
            addr.port()
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.starts_with("HTTP/1.1 101"), "{status}");
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                break;
            }
        }
        Client { reader, writer }
    }

    fn send(&mut self, v: &Value) {
        let payload = v.to_string().into_bytes();
        let mask = [9u8, 8, 7, 6];
        let mut out = vec![0x81];
        if payload.len() < 126 {
            out.push(0x80 | payload.len() as u8);
        } else {
            out.push(0x80 | 126);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        out.extend_from_slice(&mask);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.writer.write_all(&out).unwrap();
    }

    fn recv(&mut self) -> Got {
        let mut h = [0u8; 2];
        if self.reader.read_exact(&mut h).is_err() {
            return Got::Closed(None);
        }
        let len = match h[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                self.reader.read_exact(&mut b).unwrap();
                usize::from(u16::from_be_bytes(b))
            }
            127 => panic!("unexpectedly large frame"),
            n => usize::from(n),
        };
        let mut payload = vec![0; len];
        self.reader.read_exact(&mut payload).unwrap();
        match h[0] & 0x0F {
            0x1 => Got::Text(serde_json::from_slice(&payload).unwrap()),
            0x8 => Got::Closed((len >= 2).then(|| u16::from_be_bytes([payload[0], payload[1]]))),
            op => panic!("unexpected opcode {op}"),
        }
    }

    fn call(&mut self, v: Value) -> Value {
        self.send(&v);
        match self.recv() {
            Got::Text(v) => v,
            Got::Closed(code) => panic!("closed with {code:?}"),
        }
    }

    /// Hello, then authenticate with `token`; returns the authenticate answer.
    fn login(&mut self, token: &str) -> Value {
        let hello = self.call(json!({"jsonrpc": "2.0", "id": 1, "method": "session.hello",
            "params": {"protocols": ["1.0"], "client": {"name": "test", "version": "1"}}}));
        assert_eq!(hello["result"]["protocol"], "1.0", "{hello}");
        self.call(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session.authenticate",
            "params": {"token": token}}),
        )
    }

    /// Reads until the server closes; returns the close code.
    fn closed(&mut self) -> Option<u16> {
        loop {
            if let Got::Closed(code) = self.recv() {
                return code;
            }
        }
    }
}

fn start(profile: &RemoteProfile) -> RemoteServer {
    let (server, _endpoint) = RemoteServer::start(
        vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        profile.store(),
        RemoteOptions {
            control_waker: Box::new(|| {}),
            audit: Box::new(|_| {}),
        },
    )
    .unwrap();
    server
}

#[test]
fn a_token_from_the_settings_works_and_revoking_it_disconnects_the_app() {
    let mut profile = RemoteProfile::new();
    let deck = profile
        .create_token("Stream Deck", Access::Control)
        .unwrap();
    let viewer = profile.create_token("Viewer", Access::View).unwrap();
    let server = start(&profile);
    let addr = server.local_addrs()[0];

    let mut c = Client::open(addr);
    let auth = c.login(deck.expose());
    assert_eq!(
        auth["result"]["scopes"],
        json!(["read", "control"]),
        "{auth}"
    );
    let mut v = Client::open(addr);
    let auth = v.login(viewer.expose());
    assert_eq!(auth["result"]["scopes"], json!(["read"]), "{auth}");

    // Revoking "Stream Deck" closes its session only.
    let revoked = profile.revoke(0).unwrap();
    server.set_tokens(profile.store());
    assert_eq!(c.closed(), Some(close_code(rbs_api::CloseReason::Revoked)));

    // A revoked token no longer authenticates.
    let mut again = Client::open(addr);
    again.send(
        &json!({"jsonrpc": "2.0", "id": 1, "method": "session.hello",
        "params": {"protocols": ["1.0"], "client": {"name": "test", "version": "1"}}}),
    );
    let Got::Text(_) = again.recv() else {
        panic!("no hello answer")
    };
    again.send(
        &json!({"jsonrpc": "2.0", "id": 2, "method": "session.authenticate",
        "params": {"token": deck.expose()}}),
    );
    let mut refused = false;
    loop {
        match again.recv() {
            Got::Text(v) if v["error"]["data"]["kind"] == "auth_failed" => refused = true,
            Got::Text(v) => panic!("unexpected answer {v}"),
            Got::Closed(code) => {
                assert_eq!(code, Some(close_code(rbs_api::CloseReason::AuthFailed)));
                break;
            }
        }
    }
    assert!(refused);

    // Undoing the revoke makes the same token work again.
    profile.restore(0, revoked).unwrap();
    server.set_tokens(profile.store());
    let mut back = Client::open(addr);
    let auth = back.login(deck.expose());
    assert_eq!(
        auth["result"]["scopes"],
        json!(["read", "control"]),
        "{auth}"
    );

    // Turning remote control off tells the remaining app the server is going away.
    drop(server);
    assert_eq!(
        v.closed(),
        Some(close_code(rbs_api::CloseReason::ServerStopping))
    );
}

#[test]
fn a_taken_port_fails_the_start_with_a_reason() {
    let first = start(&RemoteProfile::new());
    let taken = first.local_addrs()[0];
    let result = RemoteServer::start(
        vec![taken],
        RemoteProfile::new().store(),
        RemoteOptions {
            control_waker: Box::new(|| {}),
            audit: Box::new(|_| {}),
        },
    );
    let Err(why) = result else {
        panic!("a second server started on a taken port")
    };
    assert_eq!(why, StartError::PortInUse(taken.port()));
    assert_eq!(
        why.to_string(),
        format!("another app is using port {}", taken.port())
    );
}
