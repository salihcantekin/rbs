//! The IPC transport passes the transport conformance suite through its own client,
//! and a same-user client is authenticated by `session.hello` without a token.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // Tests fail by panicking.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rbs_api::conformance::{self, Client, ClientEvent, ConnectOptions, Harness, LIMIT};
use rbs_api::transport::{ConnectionBudget, TransportError};
use rbs_api::wake::Wake;
use rbs_api::{ControlTransport, TransportEvent, TransportInbox, TransportKind};
use rbs_api_ipc::{ClientError, Endpoint, IpcClient, IpcConfig, IpcTransport, Received};

/// Removes a test endpoint's directory (Unix) when dropped.
struct Cleanup(Option<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(d) = self.0.take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

fn cleanup(e: &Endpoint) -> Cleanup {
    Cleanup((!cfg!(windows)).then(|| e.0.parent().unwrap().to_owned()))
}

/// A fresh endpoint per use: tests run in parallel.
fn endpoint() -> Endpoint {
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let id = format!("{}-{n}", std::process::id());
    if cfg!(windows) {
        Endpoint(PathBuf::from(format!(r"\\.\pipe\rbs-ipc-test-{id}")))
    } else {
        // Short: socket paths are limited to about 100 bytes.
        let dir = std::env::temp_dir().join(format!("rbs-ipc-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        Endpoint(dir.join("c.sock"))
    }
}

struct IpcTestClient(IpcClient);

impl Client for IpcTestClient {
    fn send(&mut self, text: &str) {
        // The server may already have closed (a test of its close).
        let _ = self.0.send(text);
    }

    fn recv(&mut self, timeout: Duration) -> Option<ClientEvent> {
        match self.0.recv(timeout) {
            Ok(Some(Received::Message(m))) => Some(ClientEvent::Message(m)),
            Ok(Some(Received::Closed(r))) => Some(ClientEvent::Closed(r)),
            Ok(None) => None,
            Err(_) => Some(ClientEvent::Closed(None)),
        }
    }

    fn close(&mut self) {
        self.0.close();
    }
}

#[derive(Default)]
struct IpcHarness {
    endpoint: Option<Endpoint>,
    cleanups: Vec<Cleanup>,
}

impl Harness for IpcHarness {
    type Client = IpcTestClient;

    fn kind(&self) -> TransportKind {
        TransportKind::Ipc
    }

    fn checks_upgrade(&self) -> bool {
        false
    }

    fn start(
        &mut self,
        inbox: TransportInbox,
    ) -> Result<Box<dyn ControlTransport>, TransportError> {
        let e = endpoint();
        let mut t = IpcTransport::new(IpcConfig::new(e.clone()));
        t.start(inbox)?;
        self.cleanups.push(cleanup(&e));
        self.endpoint = Some(e);
        Ok(Box::new(t))
    }

    fn connect(&mut self, _options: &ConnectOptions) -> Result<IpcTestClient, String> {
        IpcClient::connect(self.endpoint.as_ref().expect("started"), LIMIT)
            .map(IpcTestClient)
            .map_err(|e| e.to_string())
    }
}

#[test]
fn the_ipc_transport_passes_the_conformance_suite() {
    conformance::run_all(&mut IpcHarness::default());
}

/// A started transport. Bind the cleanup first, so it is dropped last.
fn rig() -> (Cleanup, IpcTransport, TransportInbox, Arc<Wake>) {
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake.clone(), Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let e = endpoint();
    let mut t = IpcTransport::new(IpcConfig::new(e.clone()));
    t.start(inbox.clone()).unwrap();
    (cleanup(&e), t, inbox, wake)
}

fn next(inbox: &TransportInbox, wake: &Wake) -> TransportEvent {
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        if let Some(e) = inbox.pop() {
            return e;
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!left.is_zero(), "no transport event within {LIMIT:?}");
        wake.wait(Some(left));
    }
}

#[test]
fn a_second_server_on_the_same_endpoint_is_refused() {
    let (_c, t, _inbox, _wake) = rig();
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake, Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let mut second = IpcTransport::new(IpcConfig::new(t.endpoint().clone()));
    let err = second.start(inbox).unwrap_err();
    assert!(err.0.contains("already serves"), "{err}");
}

#[test]
fn a_client_without_a_server_is_told_rbs_is_not_running() {
    let e = endpoint();
    let _c = cleanup(&e);
    match IpcClient::connect(&e, LIMIT) {
        Err(ClientError::NotRunning(_)) => {}
        Err(other) => panic!("expected NotRunning, got {other}"),
        Ok(_) => panic!("connected to nothing"),
    }
}

#[test]
fn a_message_that_is_not_utf8_ends_the_connection() {
    let (_c, t, inbox, wake) = rig();
    let mut c = IpcClient::connect(t.endpoint(), LIMIT).unwrap();
    assert!(matches!(
        next(&inbox, &wake),
        TransportEvent::Connected { .. }
    ));
    c.send_raw(&[0, 0, 0, 2, 0xFF, 0xFE]).unwrap();
    assert!(matches!(next(&inbox, &wake), TransportEvent::Closed { .. }));
    assert_eq!(c.recv(LIMIT).unwrap(), Some(Received::Closed(None)));
}

#[cfg(unix)]
#[test]
fn a_stale_socket_is_replaced_and_the_socket_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let e = endpoint();
    let _c = cleanup(&e);
    let dir = e.0.parent().unwrap();
    std::fs::create_dir(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // A socket file nobody listens on, as a crashed process leaves it.
    drop(std::os::unix::net::UnixListener::bind(&e.0).unwrap());
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake, Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let mut t = IpcTransport::new(IpcConfig::new(e.clone()));
    t.start(inbox).unwrap();
    let mode = std::fs::metadata(&e.0).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let _c = IpcClient::connect(&e, LIMIT).unwrap();
    t.stop();
    assert!(!e.0.exists(), "the socket is removed at stop");
}

#[cfg(unix)]
#[test]
fn a_directory_others_can_enter_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let e = endpoint();
    let _c = cleanup(&e);
    let dir = e.0.parent().unwrap();
    std::fs::create_dir(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake, Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let err = IpcTransport::new(IpcConfig::new(e))
        .start(inbox)
        .unwrap_err();
    assert!(err.0.contains("other users"), "{err}");
}

#[test]
fn a_same_user_client_is_authenticated_by_hello() {
    use rbs_api::request::ClientInfo;
    use rbs_api::{ApiOptions, ApiServer, ServerConfig, TokenStore};

    let config = ServerConfig::new(ClientInfo::new("RBS", "test")).unwrap();
    let (api, _endpoint) = ApiServer::start(config, TokenStore::new(), ApiOptions::new(|| {}))
        .expect("the API thread starts");
    let e = endpoint();
    let _c = cleanup(&e);
    let mut t = IpcTransport::new(IpcConfig::new(e.clone()));
    t.start(api.inbox()).unwrap();
    let mut c = IpcClient::connect(&e, LIMIT).unwrap();
    c.send(
        r#"{"jsonrpc":"2.0","id":1,"method":"session.hello","params":{"protocols":["1.0"],"client":{"name":"test","version":"1"}}}"#,
    )
    .unwrap();
    let Some(Received::Message(m)) = c.recv(LIMIT).unwrap() else {
        panic!("no answer to hello");
    };
    let v: serde_json::Value = serde_json::from_str(&m).unwrap();
    let scopes = &v["result"]["scopes"];
    assert!(
        scopes
            .as_array()
            .is_some_and(|s| s.iter().any(|x| x == "tokens")),
        "{m}"
    );
    t.stop();
    drop(api);
}

/// `stop` with peers that never read returns after one grace period for all of them,
/// not one each, and is not held by their stalled writes. The bound checked is only a
/// hang guard.
#[test]
fn stop_is_not_held_by_peers_that_do_not_read() {
    use rbs_api::transport::SendError;
    let wake = Arc::new(Wake::new());
    let inbox = TransportInbox::new(wake.clone(), Arc::new(ConnectionBudget::new(4)), 8, 4096);
    let e = endpoint();
    let _c = cleanup(&e);
    let mut config = IpcConfig::new(e);
    config.stop_grace = Duration::from_millis(20);
    let mut t = IpcTransport::new(config);
    t.start(inbox.clone()).unwrap();
    let chunk: Arc<str> = Arc::from("z".repeat(256 * 1024));
    let mut clients = Vec::new();
    let mut senders = Vec::new();
    for _ in 0..3 {
        clients.push(IpcClient::connect(t.endpoint(), LIMIT).unwrap());
        let TransportEvent::Connected { mut sender, .. } = next(&inbox, &wake) else {
            panic!("expected Connected");
        };
        // Fill the queue until it refuses. `Full` is not final: the writer thread can
        // still move a message into the socket buffer and make room until the peer's
        // buffer is full too, so a second `try_send` may succeed. The peers never read,
        // so the writes end up stalled whatever the timing; `stop` must not wait for them.
        assert!(
            std::iter::repeat_with(|| sender.try_send(&chunk))
                .take(1024)
                .any(|r| r == Err(SendError::Full)),
            "the queue never filled"
        );
        senders.push(sender);
    }
    let started = std::time::Instant::now();
    t.stop();
    assert!(
        started.elapsed() < LIMIT,
        "stop took {:?}",
        started.elapsed()
    );
}

/// `stop` does not depend on the socket file: it returns after the file was deleted.
#[cfg(unix)]
#[test]
fn stop_works_after_the_socket_file_was_deleted() {
    let (_c, mut t, _inbox, _wake) = rig();
    std::fs::remove_file(&t.endpoint().0).unwrap();
    t.stop();
}
