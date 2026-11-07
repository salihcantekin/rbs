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
        assert_eq!(hello["result"]["protocol"], "1.4", "{hello}");
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
            ipc: None,
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
            ipc: None,
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

/// A fresh IPC endpoint per test: tests run in parallel. Unix socket paths are short.
fn ipc_endpoint(name: &str) -> rbs_api_ipc::Endpoint {
    let id = format!("{name}-{}", std::process::id());
    if cfg!(windows) {
        rbs_api_ipc::Endpoint(format!(r"\\.\pipe\rbs-app-test-{id}").into())
    } else {
        let dir = std::env::temp_dir().join(format!("rbs-app-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        rbs_api_ipc::Endpoint(dir.join("c.sock"))
    }
}

/// `rbs-cli token` over local IPC: the studio's control thread answers `tokens.*` from
/// the settings file, a new token works at once over WebSocket, the UI is told, and a
/// revoke disconnects the app using it. The studio itself failed to start here (no
/// GPU needed): token management does not depend on it.
#[test]
fn tokens_changed_over_local_ipc_take_effect_at_once() {
    use rbs_api_ipc::{IpcClient, Received};
    use rbs_app::studio_host::{HostConfig, StudioHost, Update};

    let config = std::env::temp_dir().join(format!("rbs-app-config-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&config);
    let profile = RemoteProfile {
        enabled: true,
        ..RemoteProfile::new()
    };
    rbs_remote_settings::save(&config, &profile).unwrap();

    let (tx, updates) = std::sync::mpsc::channel();
    let host = StudioHost::spawn(
        HostConfig::desktop(),
        || (rbs_record::RecordAdapters::new(), Vec::new()),
        || Err("no GPU in this test".into()),
        rbs_engine::Engine::new,
        move |u| {
            let _ = tx.send(u);
        },
    )
    .unwrap();
    let next = |want: fn(&Update) -> bool| loop {
        let u = updates.recv_timeout(LIMIT).expect("the host answered");
        if want(&u) {
            return u;
        }
    };
    next(|u| matches!(u, Update::Failed(_)));

    let endpoint = ipc_endpoint("tokens");
    let (server, api) = RemoteServer::start(
        vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        profile.store(),
        RemoteOptions {
            control_waker: Box::new(host.api_waker()),
            audit: Box::new(|_| {}),
            ipc: Some(endpoint.clone()),
        },
    )
    .unwrap();
    assert_eq!(server.ipc_problem(), None);
    host.attach_api(api, Some(server.api_tokens(config.clone())));

    let mut cli = IpcClient::connect(&endpoint, LIMIT).unwrap();
    let mut call = |id: u32, method: &str, params: Value| -> Value {
        cli.send(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        )
        .unwrap();
        match cli.recv(LIMIT).unwrap() {
            Some(Received::Message(m)) => serde_json::from_str(&m).unwrap(),
            other => panic!("no answer: {other:?}"),
        }
    };
    let hello = call(
        1,
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": "rbs-cli", "version": "1"}}),
    );
    assert!(
        hello["result"]["scopes"]
            .as_array()
            .unwrap()
            .contains(&json!("tokens")),
        "{hello}"
    );
    let created = call(
        2,
        "tokens.create",
        json!({"name": "Deck", "scopes": ["read", "control"]}),
    );
    let token = created["result"]["token"].as_str().unwrap().to_owned();
    next(|u| matches!(u, Update::TokensChanged));
    let saved = rbs_remote_settings::read(&config).unwrap().unwrap();
    assert!(saved.enabled, "other settings are kept");
    assert!(saved.store().verify(&token).is_some());

    // Works at once over WebSocket, without the settings window.
    let mut deck = Client::open(server.local_addrs()[0]);
    let auth = deck.login(&token);
    assert_eq!(
        auth["result"]["scopes"],
        json!(["read", "control"]),
        "{auth}"
    );
    // A panel token cannot manage tokens (no token the app makes has `tokens`).
    let denied = deck.call(json!({"jsonrpc": "2.0", "id": 3, "method": "tokens.list"}));
    assert_eq!(denied["error"]["data"]["kind"], "scope_denied", "{denied}");

    let list = call(3, "tokens.list", json!({}));
    assert_eq!(
        list["result"]["tokens"],
        json!([{"name": "Deck", "scopes": ["read", "control"]}])
    );
    let taken = call(
        4,
        "tokens.create",
        json!({"name": "Deck", "scopes": ["read"]}),
    );
    assert_eq!(taken["error"]["data"]["kind"], "conflict", "{taken}");

    let revoked = call(5, "tokens.revoke", json!({"name": "Deck"}));
    assert_eq!(revoked["result"], json!({}), "{revoked}");
    assert_eq!(
        deck.closed(),
        Some(close_code(rbs_api::CloseReason::Revoked))
    );
    next(|u| matches!(u, Update::TokensChanged));
    assert!(
        rbs_remote_settings::read(&config)
            .unwrap()
            .unwrap()
            .tokens
            .is_empty()
    );

    host.detach_api();
    drop(server);
    drop(host);
    let _ = std::fs::remove_dir_all(&config);
    if !cfg!(windows) {
        let _ = std::fs::remove_dir_all(endpoint.0.parent().unwrap());
    }
}

#[test]
fn with_network_access_apps_on_this_computer_still_connect_without_tls() {
    let mut profile = RemoteProfile::new();
    let deck = profile.create_token("Stream Deck", Access::Full).unwrap();
    let g = rbs_api_ws::tls::generate(&["localhost".into()], &[], 1_800_000_000).unwrap();
    let identity = rbs_api_ws::TlsIdentity::from_pem(&g.cert_pem, &g.key_pem).unwrap();
    let (server, _endpoint) = RemoteServer::start_network(
        0,
        rbs_app::remote_control::NetworkOptions {
            identity,
            host_names: vec!["studio.local".into()],
            allow_remote_admin: false,
        },
        profile.store(),
        RemoteOptions {
            control_waker: Box::new(|| {}),
            audit: Box::new(|_| {}),
            ipc: None,
        },
    )
    .unwrap();
    let bound = server.local_addrs()[0];
    assert!(bound.ip().is_unspecified(), "{bound}");
    let mut c = Client::open(SocketAddr::from(([127, 0, 0, 1], bound.port())));
    let auth = c.login(deck.expose());
    // Loopback keeps `admin`; "Allow remote admin" only matters for other computers.
    assert_eq!(
        auth["result"]["scopes"],
        json!(["read", "control", "admin"]),
        "{auth}"
    );
}
