//! The API thread end to end: an in-memory transport, the API thread and a control
//! thread with a fake target. Waits on channel messages with a generous hang guard,
//! never for a duration (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rbs_api::request::{ClientInfo, Empty, RecordStarted, Response};
use rbs_api::session::ServerConfig;
use rbs_api::thread::ApiOptions;
use rbs_api::transport::{
    CloseReason, ConnectionHandle, ConnectionSender, ControlTransport, PeerInfo, SendError,
    TransportError, TransportInbox,
};
use rbs_api::types::RecordState;
use rbs_api::types::Snapshot;
use rbs_api::{
    ApiError, ApiServer, Command, ControlTarget, EventPayload, EventPublisher, ScopeSet, StateKey,
    Token, TokenEntry, TokenStore, Topic,
};
use serde_json::{Value, json};

/// Hang guard only: every wait ends as soon as its message arrives.
const GUARD: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
struct MemoryTransport {
    inbox: Arc<Mutex<Option<TransportInbox>>>,
    stops: Arc<AtomicUsize>,
}

impl ControlTransport for MemoryTransport {
    fn start(&mut self, inbox: TransportInbox) -> Result<(), TransportError> {
        *self.inbox.lock().unwrap() = Some(inbox);
        Ok(())
    }

    fn stop(&mut self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }
}

enum Out {
    Text(Value),
    Closed(CloseReason),
}

struct ChannelSender(Sender<Out>);

impl ConnectionSender for ChannelSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        self.0
            .send(Out::Text(serde_json::from_str(text).unwrap()))
            .map_err(|_| SendError::Closed)
    }

    fn close(&mut self, reason: CloseReason) {
        let _ = self.0.send(Out::Closed(reason));
    }
}

struct Client {
    handle: ConnectionHandle,
    rx: Receiver<Out>,
    next: i64,
}

impl Client {
    fn connect(transport: &MemoryTransport) -> Client {
        let inbox = transport.inbox.lock().unwrap().clone().unwrap();
        let (tx, rx) = mpsc::channel();
        let handle = inbox
            .connect(
                PeerInfo::loopback_websocket(),
                Box::new(ChannelSender(tx)),
                Box::new(|| {}),
            )
            .unwrap();
        Client {
            handle,
            rx,
            next: 1,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next;
        self.next += 1;
        let text = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.handle.push_message(text.to_string());
        id
    }

    fn recv(&self) -> Out {
        self.rx.recv_timeout(GUARD).expect("nothing arrived")
    }

    fn text(&self) -> Value {
        match self.recv() {
            Out::Text(v) => v,
            Out::Closed(r) => panic!("closed: {r:?}"),
        }
    }
}

struct Studio {
    recording: bool,
}

impl ControlTarget for Studio {
    fn handle(
        &mut self,
        command: Command,
        events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        match command {
            Command::RecordStart(_) => {
                self.recording = true;
                events.changed(StateKey::topic(Topic::RecordState), || {
                    EventPayload::RecordState(RecordState::Recording {
                        profile: Some("default".into()),
                        video_encoder: "Test".into(),
                        audio_encoder: None,
                        tracks: vec![],
                    })
                });
                Ok(Response::RecordStarted(RecordStarted {
                    video_encoder: "Test".into(),
                    audio_encoder: None,
                }))
            }
            Command::RecordStatus(_) => Ok(Response::RecordStatus(Snapshot {
                seq: events.seq_map([Topic::RecordState]),
                data: RecordState::Idle { last: None },
            })),
            _ => Ok(Response::Done(Empty {})),
        }
    }
}

enum Ctl {
    Wake,
    Stop,
}

#[test]
fn a_client_authenticates_subscribes_records_and_gets_the_event() {
    let token = Token::from_bytes(&[5; 32]);
    let mut tokens = TokenStore::new();
    tokens.insert(TokenEntry {
        name: "Deck".into(),
        scopes: ScopeSet::PANEL,
        hash: token.hash(),
    });
    let config = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
    let transport = MemoryTransport::default();
    let (ctl_tx, ctl_rx) = mpsc::channel();
    let waker = Mutex::new(ctl_tx.clone());
    let mut options = ApiOptions::new(move || {
        let _ = waker.lock().unwrap().send(Ctl::Wake);
    });
    options.transports.push(Box::new(transport.clone()));
    let (api, endpoint) = ApiServer::start(config, tokens, options).unwrap();

    // The control thread: drains forwarded commands whenever woken.
    let control = std::thread::spawn(move || {
        let mut studio = Studio { recording: false };
        let mut events = EventPublisher::new();
        events.attach(endpoint.event_link());
        while let Ok(Ctl::Wake) = ctl_rx.recv() {
            while endpoint.serve_one(&mut studio, &mut events) {}
        }
        studio.recording
    });

    let mut c = Client::connect(&transport);
    c.call(
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": "t", "version": "1"}}),
    );
    assert_eq!(c.text()["result"]["protocol"], rbs_api::PROTOCOL_VERSION);
    c.call("session.authenticate", json!({"token": token.expose()}));
    assert_eq!(c.text()["result"]["scopes"], json!(["read", "control"]));
    c.call("events.subscribe", json!({"topics": ["record.state"]}));
    assert_eq!(c.text()["result"]["seq"]["record.state"], 0);

    let start = c.call("record.start", json!({"profile": "default"}));
    let (mut answer, mut event) = (None, None);
    while answer.is_none() || event.is_none() {
        let v = c.text();
        if v["id"] == start {
            answer = Some(v);
        } else {
            event = Some(v);
        }
    }
    assert_eq!(answer.unwrap()["result"]["video_encoder"], "Test");
    let event = event.unwrap();
    assert_eq!(event["params"]["topic"], "record.state");
    assert_eq!(event["params"]["seq"], 1);

    c.call("record.status", json!({}));
    assert_eq!(c.text()["result"]["seq"]["record.state"], 1);

    drop(api);
    assert!(matches!(c.recv(), Out::Closed(CloseReason::ServerStopping)));
    assert_eq!(transport.stops.load(Ordering::SeqCst), 1);
    ctl_tx.send(Ctl::Stop).unwrap();
    assert!(control.join().unwrap());
}

#[test]
fn the_inbox_pauses_a_connection_at_its_quota_and_resumes_it() {
    use rbs_api::transport::{ConnectionBudget, ReadState, TransportEvent};
    use rbs_api::wake::Wake;

    let wake = Arc::new(Wake::new());
    let budget = Arc::new(ConnectionBudget::new(1));
    let inbox = TransportInbox::new(wake.clone(), budget.clone(), 2, 4096);
    let resumed = Arc::new(AtomicUsize::new(0));
    let r = resumed.clone();
    let (tx, _rx) = mpsc::channel();
    let conn = inbox
        .connect(
            PeerInfo::loopback_websocket(),
            Box::new(ChannelSender(tx.clone())),
            Box::new(move || {
                r.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .unwrap();
    assert!(wake.wait(Some(Duration::ZERO)));
    // The budget of one connection is used.
    assert!(
        inbox
            .connect(
                PeerInfo::loopback_websocket(),
                Box::new(ChannelSender(tx)),
                Box::new(|| {})
            )
            .is_err()
    );
    assert_eq!(conn.push_message("a".into()), ReadState::Continue);
    assert_eq!(conn.push_message("b".into()), ReadState::Pause);
    assert!(!conn.can_read());
    assert!(matches!(
        inbox.pop(),
        Some(TransportEvent::Connected { .. })
    ));
    assert!(matches!(inbox.pop(), Some(TransportEvent::Message { .. })));
    assert_eq!(resumed.load(Ordering::SeqCst), 1);
    assert!(conn.can_read());
    assert!(matches!(inbox.pop(), Some(TransportEvent::Message { .. })));
    assert_eq!(resumed.load(Ordering::SeqCst), 1);
    // Before authentication a message over 4 KiB is dropped and reported, not queued.
    assert_eq!(conn.max_message_bytes(), 4096);
    conn.push_message("x".repeat(4097));
    assert!(matches!(
        inbox.pop(),
        Some(TransportEvent::Oversized { len: 4097, .. })
    ));
    drop(conn);
    assert!(matches!(inbox.pop(), Some(TransportEvent::Closed { .. })));
    assert!(inbox.is_empty());
}

#[test]
fn remote_peers_cannot_take_the_local_reserve() {
    use rbs_api::transport::ConnectionBudget;
    use rbs_api::wake::Wake;

    let budget = Arc::new(ConnectionBudget::with_local_reserve(3, 1));
    let inbox = TransportInbox::new(Arc::new(Wake::new()), budget.clone(), 2, 4096);
    let (tx, _rx) = mpsc::channel();
    let connect =
        |peer: PeerInfo| inbox.connect(peer, Box::new(ChannelSender(tx.clone())), Box::new(|| {}));
    let _r1 = connect(PeerInfo::remote_websocket("10.0.0.1")).unwrap();
    let _r2 = connect(PeerInfo::remote_websocket("10.0.0.2")).unwrap();
    assert!(connect(PeerInfo::remote_websocket("10.0.0.3")).is_err());
    let _local = connect(PeerInfo::same_user_ipc()).unwrap();
    assert_eq!(budget.open(), 3);
    assert!(connect(PeerInfo::loopback_websocket()).is_err());
}

/// A control thread that keeps the tokens itself, as an app answering `tokens.*` does.
struct TokenKeeper {
    store: TokenStore,
    handle: rbs_api::TokenHandle,
}

impl ControlTarget for TokenKeeper {
    fn handle(
        &mut self,
        command: Command,
        _events: &mut EventPublisher,
    ) -> Result<Response, ApiError> {
        use rbs_api::request::{TokenCreated, TokenRef};
        match command {
            Command::TokensCreate(p) => {
                let token = Token::from_bytes(&[7; 32]);
                self.store.insert(TokenEntry {
                    name: p.name.clone(),
                    scopes: p.scopes,
                    hash: token.hash(),
                });
                // Set before the reply: the token works for anything sent after it.
                self.handle.set(self.store.clone());
                Ok(Response::TokenCreated(TokenCreated {
                    name: p.name,
                    scopes: p.scopes,
                    token: token.expose().to_owned(),
                }))
            }
            Command::TokensRevoke(TokenRef { name }) => {
                self.store.revoke(&name);
                self.handle.set(self.store.clone());
                Ok(Response::Done(Empty {}))
            }
            _ => Ok(Response::Done(Empty {})),
        }
    }
}

#[test]
fn a_token_set_before_the_reply_works_for_the_next_message_and_revoking_closes_its_sessions() {
    let config = ServerConfig::new(ClientInfo::new("rbs", "test")).unwrap();
    let transport = MemoryTransport::default();
    let (ctl_tx, ctl_rx) = mpsc::channel();
    let waker = Mutex::new(ctl_tx.clone());
    let mut options = ApiOptions::new(move || {
        let _ = waker.lock().unwrap().send(Ctl::Wake);
    });
    options.transports.push(Box::new(transport.clone()));
    let (api, endpoint) = ApiServer::start(config, TokenStore::new(), options).unwrap();
    let handle = api.token_handle();
    let control = std::thread::spawn(move || {
        let mut keeper = TokenKeeper {
            store: TokenStore::new(),
            handle,
        };
        let mut events = EventPublisher::new();
        while let Ok(Ctl::Wake) = ctl_rx.recv() {
            while endpoint.serve_one(&mut keeper, &mut events) {}
        }
    });

    // A same-user IPC peer: authenticated by hello, with the `tokens` scope.
    let inbox = transport.inbox.lock().unwrap().clone().unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = inbox
        .connect(
            PeerInfo::same_user_ipc(),
            Box::new(ChannelSender(tx)),
            Box::new(|| {}),
        )
        .unwrap();
    let mut cli = Client {
        handle,
        rx,
        next: 1,
    };
    cli.call(
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": "cli", "version": "1"}}),
    );
    assert!(
        cli.text()["result"]["scopes"]
            .as_array()
            .unwrap()
            .contains(&json!("tokens"))
    );
    cli.call(
        "tokens.create",
        json!({"name": "Deck", "scopes": ["read", "control"]}),
    );
    let created = cli.text();
    let token = created["result"]["token"].as_str().unwrap().to_owned();

    let mut deck = Client::connect(&transport);
    deck.call(
        "session.hello",
        json!({"protocols": ["1.0"], "client": {"name": "deck", "version": "1"}}),
    );
    deck.text();
    deck.call("session.authenticate", json!({ "token": token }));
    assert_eq!(deck.text()["result"]["scopes"], json!(["read", "control"]));
    // A panel token cannot manage tokens.
    deck.call("tokens.list", json!({}));
    assert_eq!(deck.text()["error"]["data"]["kind"], "scope_denied");

    cli.call("tokens.revoke", json!({"name": "Deck"}));
    assert_eq!(cli.text()["result"], json!({}));
    assert!(matches!(deck.recv(), Out::Closed(CloseReason::Revoked)));

    drop(api);
    ctl_tx.send(Ctl::Stop).unwrap();
    control.join().unwrap();
}

#[test]
fn a_plain_remote_websocket_peer_is_refused_by_the_inbox() {
    use rbs_api::transport::{ConnectionBudget, Refused};
    use rbs_api::wake::Wake;

    let inbox = TransportInbox::new(
        Arc::new(Wake::new()),
        Arc::new(ConnectionBudget::new(4)),
        2,
        4096,
    );
    let (tx, _rx) = mpsc::channel();
    let connect =
        |peer: PeerInfo| inbox.connect(peer, Box::new(ChannelSender(tx.clone())), Box::new(|| {}));
    let plain = PeerInfo {
        secure: false,
        ..PeerInfo::remote_websocket("10.0.0.1")
    };
    assert!(matches!(connect(plain), Err(Refused::Insecure)));
    assert!(connect(PeerInfo::remote_websocket("10.0.0.1")).is_ok());
    assert!(connect(PeerInfo::loopback_websocket()).is_ok());
}
