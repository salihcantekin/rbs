//! Drives the session state machine directly: fake connections record what the server
//! sends, a `TestClock` gives the time, and a fake control target answers forwarded
//! commands on the test's thread (ADR-0032: no threads, no sleeps).

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use rbs_api::control::{EventPublisher, Reply};
use rbs_api::request::{Command, Empty, ProfileList, Response};
use rbs_api::session::{Action, AuditEvent, Input, Server, ServerConfig};
use rbs_api::token::{Token, TokenEntry, TokenStore};
use rbs_api::transport::{
    CloseReason, ConnectionId, ConnectionSender, PeerInfo, SendError, TransportEvent,
};
use rbs_api::types::{RecordState, Snapshot};
use rbs_api::{ApiError, Forwarded, ScopeSet, StateKey, Topic};
use rbs_core::time::{Clock, TestClock};
use serde_json::Value;

/// What one fake connection saw.
#[derive(Default)]
pub struct Wire {
    pub sent: Vec<Value>,
    pub closed: Option<CloseReason>,
    pub full: bool,
}

pub struct FakeSender(pub Arc<Mutex<Wire>>);

impl ConnectionSender for FakeSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        let mut w = self.0.lock().unwrap();
        if w.closed.is_some() {
            return Err(SendError::Closed);
        }
        if w.full {
            return Err(SendError::Full);
        }
        w.sent.push(serde_json::from_str(text).unwrap());
        Ok(())
    }

    fn close(&mut self, reason: CloseReason) {
        self.0.lock().unwrap().closed = Some(reason);
    }
}

/// A token for tests and its entry.
pub fn token(seed: u8, name: &str, scopes: ScopeSet) -> (Token, TokenEntry) {
    let t = Token::from_bytes(&[seed; 32]);
    let e = TokenEntry {
        name: name.into(),
        scopes,
        hash: t.hash(),
    };
    (t, e)
}

pub const ALL_TOKEN_SEED: u8 = 1;
pub const PANEL_TOKEN_SEED: u8 = 2;

pub struct Harness {
    pub server: Server,
    pub clock: TestClock,
    pub wires: Vec<(ConnectionId, Arc<Mutex<Wire>>)>,
    pub next_id: i64,
    pub forwarded: Vec<Forwarded>,
    pub audit: Vec<AuditEvent>,
    pub masks: Vec<rbs_api::TopicSet>,
    pub inbox: rbs_api::TransportInbox,
    pub handles: Vec<rbs_api::ConnectionHandle>,
}

impl Harness {
    pub fn new() -> Self {
        Self::with_config(|_| {})
    }

    pub fn with_config(edit: impl FnOnce(&mut ServerConfig)) -> Self {
        let mut config =
            ServerConfig::new(rbs_api::request::ClientInfo::new("rbs-test", "0.0.1")).unwrap();
        edit(&mut config);
        let mut tokens = TokenStore::new();
        tokens.insert(token(ALL_TOKEN_SEED, "admin console", ScopeSet::ALL).1);
        tokens.insert(token(PANEL_TOKEN_SEED, "Stream Deck", ScopeSet::PANEL).1);
        let server = Server::new(config, tokens);
        let inbox = rbs_api::TransportInbox::new(
            Arc::new(rbs_api::wake::Wake::new()),
            server.budget(),
            8,
            4096,
        );
        Self {
            inbox,
            handles: Vec::new(),
            server,
            clock: TestClock::new(),
            wires: Vec::new(),
            next_id: 1,
            forwarded: Vec::new(),
            audit: Vec::new(),
            masks: Vec::new(),
        }
    }

    pub fn now(&self) -> rbs_core::time::Timestamp {
        self.clock.now()
    }

    /// Connects through a real `TransportInbox`, so the connection budget is taken as
    /// in production; the handle is kept until the harness drops.
    pub fn connect(&mut self, peer: PeerInfo) -> ConnectionId {
        let wire = Arc::new(Mutex::new(Wire::default()));
        let handle = self
            .inbox
            .connect(peer, Box::new(FakeSender(wire.clone())), Box::new(|| {}))
            .expect("budget");
        let id = handle.id();
        self.handles.push(handle);
        self.wires.push((id, wire));
        let now = self.now();
        while let Some(event) = self.inbox.pop() {
            self.server.handle(Input::Transport(event), now);
        }
        self.collect();
        id
    }

    pub fn wire(&self, conn: ConnectionId) -> Arc<Mutex<Wire>> {
        self.wires
            .iter()
            .find(|(c, _)| *c == conn)
            .map(|(_, w)| w.clone())
            .unwrap()
    }

    /// Sends raw text.
    pub fn send_text(&mut self, conn: ConnectionId, text: &str) {
        let now = self.now();
        self.server.handle(
            Input::Transport(TransportEvent::Message {
                id: conn,
                text: text.into(),
            }),
            now,
        );
        self.collect();
    }

    /// Sends a request and returns its id.
    pub fn call(&mut self, conn: ConnectionId, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        let text =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_text(conn, &text.to_string());
        id
    }

    /// Everything sent to `conn` since the last call.
    pub fn take(&mut self, conn: ConnectionId) -> Vec<Value> {
        std::mem::take(&mut self.wire(conn).lock().unwrap().sent)
    }

    /// The single message sent to `conn`.
    pub fn one(&mut self, conn: ConnectionId) -> Value {
        let mut all = self.take(conn);
        assert_eq!(all.len(), 1, "expected one message, got {all:?}");
        all.remove(0)
    }

    pub fn closed(&self, conn: ConnectionId) -> Option<CloseReason> {
        self.wire(conn).lock().unwrap().closed
    }

    /// Drops the connection's handle as an adapter does when the peer is gone.
    pub fn transport_closed(&mut self, conn: ConnectionId) {
        self.handles.retain(|h| h.id() != conn);
        let now = self.now();
        while let Some(event) = self.inbox.pop() {
            self.server.handle(Input::Transport(event), now);
        }
        self.collect();
    }

    pub fn set_full(&mut self, conn: ConnectionId, full: bool) {
        self.wire(conn).lock().unwrap().full = full;
        if !full {
            let now = self.now();
            self.server
                .handle(Input::Transport(TransportEvent::Writable { id: conn }), now);
            self.collect();
        }
    }

    pub fn advance(&mut self, by: std::time::Duration) {
        self.clock.advance(by);
        let now = self.now();
        self.server.advance(now);
        self.collect();
    }

    pub fn collect(&mut self) {
        while let Some(a) = self.server.next_action() {
            match a {
                Action::Forward(f) => self.forwarded.push(f),
                Action::Audit(e) => self.audit.push(e),
                Action::SetMask(m) => self.masks.push(m),
            }
        }
    }

    pub fn reply(&mut self, f: &Forwarded, result: Result<Response, ApiError>) {
        let now = self.now();
        self.server.handle(
            Input::Reply(Reply {
                conn: f.conn,
                id: f.id.clone(),
                result,
            }),
            now,
        );
        self.collect();
    }

    /// Answers every forwarded command with `target`.
    pub fn serve(&mut self, target: &mut FakeTarget) {
        let pending = std::mem::take(&mut self.forwarded);
        for f in pending {
            let result = target.answer(&f.command);
            self.reply(&f, result);
        }
    }

    /// hello + authenticate with the token of `seed`.
    pub fn login(&mut self, peer: PeerInfo, seed: u8) -> ConnectionId {
        let conn = self.connect(peer);
        self.call(conn, "session.hello", hello());
        let hello = self.one(conn);
        assert!(hello.get("result").is_some(), "{hello}");
        let t = Token::from_bytes(&[seed; 32]);
        self.call(
            conn,
            "session.authenticate",
            serde_json::json!({"token": t.expose()}),
        );
        let auth = self.one(conn);
        assert!(auth.get("result").is_some(), "{auth}");
        conn
    }
}

pub fn hello() -> Value {
    serde_json::json!({"protocols": ["1.0"], "client": {"name": "test", "version": "1"}})
}

pub fn error_kind(msg: &Value) -> &str {
    msg["error"]["data"]["kind"].as_str().unwrap_or("<none>")
}

/// A control target with a recording state and a publisher, like the studio's.
#[derive(Default)]
pub struct FakeTarget {
    pub events: EventPublisher,
    pub recording: bool,
    pub commands: Vec<Command>,
}

impl FakeTarget {
    pub fn answer(&mut self, command: &Command) -> Result<Response, ApiError> {
        self.commands.push(command.clone());
        match command {
            Command::OutputProfiles => Ok(Response::Profiles(ProfileList { profiles: vec![] })),
            Command::RecordStatus(_) => Ok(Response::RecordStatus(Snapshot {
                seq: self.events.seq_map([Topic::RecordState]),
                data: self.state(),
            })),
            Command::EventsSubscribe(s) => Ok(Response::Subscribed(rbs_api::request::Subscribed {
                seq: self.events.seq_map(s.topics.iter().copied()),
            })),
            _ => Ok(Response::Done(Empty {})),
        }
    }

    pub fn state(&self) -> RecordState {
        if self.recording {
            RecordState::Recording {
                profile: Some("default".into()),
                video_encoder: "Test".into(),
                audio_encoder: None,
            }
        } else {
            RecordState::Idle { last: None }
        }
    }

    pub fn key() -> StateKey {
        StateKey::topic(Topic::RecordState)
    }
}
