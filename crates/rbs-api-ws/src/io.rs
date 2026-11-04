//! The I/O thread: one readiness poller for the listeners and every connection.
//!
//! Sources are registered in oneshot mode: after each event the source is armed again
//! with the interest its state needs now (read unless paused, write while output
//! waits). The API thread queues output and frees paused connections through shared
//! per-connection state and wakes the poller; each wake services every connection
//! (at most the connection cap plus the upgrade cap).

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use polling::{Event, Events, Poller};
use rbs_api::transport::SendError;
use rbs_api::{
    CloseReason, ConnectionHandle, ConnectionSender, PeerInfo, ReadState, TransportInbox,
    TransportKind, UpgradePolicy,
};

use crate::frame::{self, FrameReader, Incoming};
use crate::handshake::{self, MAX_REQUEST, UNAVAILABLE};
use crate::{close_code, close_text};

/// How long a closing connection may take to receive its close frame and hang up.
const LINGER: Duration = Duration::from_secs(2);
/// Bytes read per `read` call.
const CHUNK: usize = 16 * 1024;
/// Chunks read from one connection per wake, so one busy peer cannot starve others.
const CHUNKS_PER_WAKE: usize = 4;
/// An idle output buffer larger than this is released.
const KEEP_CAPACITY: usize = 64 * 1024;

pub(crate) struct Settings {
    pub policy: UpgradePolicy,
    pub upgrade_timeout: Duration,
    pub max_upgrading: usize,
    pub max_queued_bytes: usize,
}

/// Output and flags shared between the API thread (sender, resume) and the I/O
/// thread.
#[derive(Default)]
struct Out {
    buf: Vec<u8>,
    /// A close frame is queued: nothing more is sent.
    closing: bool,
    /// The I/O thread dropped the connection.
    gone: bool,
    /// `try_send` answered `Full`: report `Writable` once the queue drained.
    full: bool,
    /// The inbox freed quota for a paused connection.
    resume: bool,
}

struct Shared {
    out: Mutex<Out>,
    max: usize,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Out> {
        self.out.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a close frame unless one is queued already.
    fn close(&self, code: u16, text: &str) {
        let mut out = self.lock();
        if !out.closing && !out.gone {
            frame::encode_close(code, text, &mut out.buf);
            out.closing = true;
        }
    }
}

/// The connection's sender, used on the API thread.
struct WsSender {
    shared: Arc<Shared>,
    poller: Arc<Poller>,
}

impl ConnectionSender for WsSender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        {
            let mut out = self.shared.lock();
            if out.closing || out.gone {
                return Err(SendError::Closed);
            }
            if !out.buf.is_empty() && out.buf.len() + text.len() + 10 > self.shared.max {
                out.full = true;
                return Err(SendError::Full);
            }
            frame::encode_text(text, &mut out.buf);
        }
        // A failed notification leaves the output for the next wake.
        let _ = self.poller.notify();
        Ok(())
    }

    fn close(&mut self, reason: CloseReason) {
        self.shared.close(close_code(reason), close_text(reason));
        let _ = self.poller.notify();
    }
}

enum State {
    /// Waiting for the HTTP upgrade request.
    Upgrading { buf: Vec<u8>, deadline: Instant },
    /// Exchanging messages.
    Open {
        handle: ConnectionHandle,
        reader: FrameReader,
        shared: Arc<Shared>,
        paused: bool,
    },
    /// A close frame is queued or sent; the inbox already has `Closed`. Waits for the
    /// peer to hang up, at most until `deadline`.
    Lingering {
        shared: Arc<Shared>,
        deadline: Instant,
        write_shut: bool,
    },
}

struct Conn {
    stream: TcpStream,
    peer: SocketAddr,
    state: State,
    /// The interest the source is armed with; `None` after its event fired.
    armed: Option<(bool, bool)>,
    poller: Arc<Poller>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Keeps the registration invariant of `Poller::add` (see `register`).
        let _ = self.poller.delete(&self.stream);
        match &self.state {
            State::Open { shared, .. } | State::Lingering { shared, .. } => {
                shared.lock().gone = true;
            }
            State::Upgrading { .. } => {}
        }
    }
}

struct Listener {
    listener: TcpListener,
    poller: Arc<Poller>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Keeps the registration invariant of `Poller::add` (see `register`).
        let _ = self.poller.delete(&self.listener);
    }
}

/// Registers a source the caller owns in a struct whose `Drop` deletes it.
fn register(
    poller: &Poller,
    source: impl polling::AsRawSource,
    event: Event,
) -> std::io::Result<()> {
    // SAFETY: `Poller::add` requires the source to be deleted from the poller before
    // it is dropped. Every source registered here is owned by a `Conn` or `Listener`,
    // whose `Drop` deletes it from this same poller first; the poller (an `Arc` they
    // hold) outlives them.
    unsafe { poller.add(source, event) }
}

pub(crate) struct Io {
    poller: Arc<Poller>,
    stop: Arc<AtomicBool>,
    listeners: Vec<Listener>,
    conns: HashMap<usize, Conn>,
    next_key: usize,
    inbox: TransportInbox,
    settings: Settings,
    scratch: Vec<u8>,
}

impl Io {
    pub(crate) fn new(
        poller: Arc<Poller>,
        stop: Arc<AtomicBool>,
        listeners: Vec<TcpListener>,
        inbox: TransportInbox,
        settings: Settings,
    ) -> std::io::Result<Self> {
        let mut owned = Vec::with_capacity(listeners.len());
        for (key, listener) in listeners.into_iter().enumerate() {
            register(&poller, &listener, Event::readable(key))?;
            owned.push(Listener {
                listener,
                poller: poller.clone(),
            });
        }
        let next_key = owned.len();
        Ok(Self {
            poller,
            stop,
            listeners: owned,
            conns: HashMap::new(),
            next_key,
            inbox,
            settings,
            scratch: vec![0; CHUNK],
        })
    }

    pub(crate) fn run(mut self) {
        let mut events = Events::new();
        let mut keys = Vec::new();
        loop {
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            let timeout = self
                .next_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()));
            events.clear();
            if let Err(e) = self.poller.wait(&mut events, timeout)
                && e.kind() != ErrorKind::Interrupted
            {
                break;
            }
            if self.stop.load(Ordering::Acquire) {
                break;
            }
            let mut readable = Vec::new();
            for ev in events.iter() {
                if ev.key < self.listeners.len() {
                    self.accept(ev.key);
                } else if let Some(c) = self.conns.get_mut(&ev.key) {
                    c.armed = None;
                    if ev.readable {
                        readable.push(ev.key);
                    }
                }
            }
            let now = Instant::now();
            keys.clear();
            keys.extend(self.conns.keys().copied());
            for &key in &keys {
                let keep = self.service(key, readable.contains(&key), now);
                if !keep {
                    self.conns.remove(&key);
                }
            }
        }
        self.shutdown();
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.conns
            .values()
            .filter_map(|c| match &c.state {
                State::Upgrading { deadline, .. } | State::Lingering { deadline, .. } => {
                    Some(*deadline)
                }
                State::Open { .. } => None,
            })
            .min()
    }

    fn accept(&mut self, index: usize) {
        loop {
            let Some(l) = self.listeners.get(index) else {
                return;
            };
            match l.listener.accept() {
                Ok((stream, peer)) => {
                    let upgrading = self
                        .conns
                        .values()
                        .filter(|c| matches!(c.state, State::Upgrading { .. }))
                        .count();
                    if upgrading >= self.settings.max_upgrading
                        || stream.set_nonblocking(true).is_err()
                    {
                        continue; // Dropping the stream closes it.
                    }
                    let _ = stream.set_nodelay(true);
                    let key = self.next_key;
                    self.next_key += 1;
                    if register(&self.poller, &stream, Event::readable(key)).is_err() {
                        continue;
                    }
                    self.conns.insert(
                        key,
                        Conn {
                            stream,
                            peer,
                            state: State::Upgrading {
                                buf: Vec::new(),
                                deadline: Instant::now() + self.settings.upgrade_timeout,
                            },
                            armed: Some((true, false)),
                            poller: self.poller.clone(),
                        },
                    );
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                // `WouldBlock`, or an error a later accept may not have: arm again.
                Err(_) => break,
            }
        }
        if let Some(l) = self.listeners.get(index) {
            let _ = self.poller.modify(&l.listener, Event::readable(index));
        }
    }

    /// Services one connection; false when it is done.
    fn service(&mut self, key: usize, readable: bool, now: Instant) -> bool {
        let Some(mut conn) = self.conns.remove(&key) else {
            return false;
        };
        let keep = self.step(&mut conn, readable, now);
        if keep {
            let want = match &conn.state {
                State::Upgrading { .. } => (true, false),
                State::Open { shared, paused, .. } => (!*paused, !shared.lock().buf.is_empty()),
                State::Lingering { shared, .. } => (true, !shared.lock().buf.is_empty()),
            };
            if conn.armed != Some(want) {
                if self
                    .poller
                    .modify(&conn.stream, Event::new(key, want.0, want.1))
                    .is_err()
                {
                    return false;
                }
                conn.armed = Some(want);
            }
            self.conns.insert(key, conn);
        }
        keep
    }

    fn step(&mut self, conn: &mut Conn, readable: bool, now: Instant) -> bool {
        if let State::Upgrading { .. } = conn.state {
            match self.upgrade(conn, readable, now) {
                Some(true) => {}
                Some(false) => return false,
                // Still waiting for the request.
                None => return true,
            }
        }
        if let State::Open { .. } = conn.state
            && !self.open(conn, readable)
        {
            return false;
        }
        if let State::Lingering { .. } = conn.state {
            return linger(conn, readable, now, &mut self.scratch);
        }
        true
    }

    /// Reads the upgrade request. `Some(true)`: the connection is open now;
    /// `Some(false)`: refused or gone; `None`: still waiting.
    fn upgrade(&mut self, conn: &mut Conn, readable: bool, now: Instant) -> Option<bool> {
        let State::Upgrading { buf, deadline } = &mut conn.state else {
            return Some(false);
        };
        if readable {
            let room = MAX_REQUEST.saturating_sub(buf.len()).min(CHUNK);
            if room > 0 {
                match conn.stream.read(&mut self.scratch[..room]) {
                    Ok(0) => return Some(false),
                    Ok(n) => buf.extend_from_slice(&self.scratch[..n]),
                    Err(e) if is_transient(&e) => {}
                    Err(_) => return Some(false),
                }
            }
        }
        let upgrade = match handshake::parse(buf, &self.settings.policy) {
            Ok(Some(u)) => u,
            Ok(None) if now >= *deadline => return Some(false),
            Ok(None) => return None,
            Err(refusal) => {
                write_small(&mut conn.stream, refusal.response());
                return Some(false);
            }
        };
        let shared = Arc::new(Shared {
            out: Mutex::new(Out::default()),
            max: self.settings.max_queued_bytes,
        });
        let ip = conn.peer.ip().to_canonical();
        let peer = PeerInfo {
            kind: TransportKind::WebSocket,
            loopback: ip.is_loopback(),
            address: Some(ip.to_string()),
            origin: upgrade.origin.clone(),
            host: upgrade.host.clone(),
            same_user: false,
        };
        let sender = WsSender {
            shared: shared.clone(),
            poller: self.poller.clone(),
        };
        let resume = {
            let shared = shared.clone();
            let poller = self.poller.clone();
            move || {
                shared.lock().resume = true;
                let _ = poller.notify();
            }
        };
        let handle = match self.inbox.connect(peer, Box::new(sender), Box::new(resume)) {
            Ok(h) => h,
            Err(_) => {
                write_small(&mut conn.stream, UNAVAILABLE);
                return Some(false);
            }
        };
        if !write_small(&mut conn.stream, &handshake::accept_response(&upgrade)) {
            return Some(false); // Dropping the handle reports the connection closed.
        }
        let mut reader = FrameReader::default();
        reader.feed(&buf[upgrade.rest..]);
        conn.state = State::Open {
            handle,
            reader,
            shared,
            paused: false,
        };
        Some(true)
    }

    /// Reads, writes and closes an open connection; false when it is gone.
    fn open(&mut self, conn: &mut Conn, readable: bool) -> bool {
        let State::Open {
            handle,
            reader,
            shared,
            paused,
        } = &mut conn.state
        else {
            return false;
        };
        let resumed = std::mem::take(&mut shared.lock().resume);
        if resumed {
            *paused = false;
        }
        if !*paused {
            process(handle, reader, shared, paused);
        }
        if readable || resumed {
            for _ in 0..CHUNKS_PER_WAKE {
                if *paused || shared.lock().closing {
                    break;
                }
                match conn.stream.read(&mut self.scratch) {
                    Ok(0) => return false,
                    Ok(n) => {
                        reader.feed(&self.scratch[..n]);
                        process(handle, reader, shared, paused);
                    }
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return false,
                }
            }
        }
        if flush(&mut conn.stream, shared).is_err() {
            return false;
        }
        let (closing, writable) = {
            let mut out = shared.lock();
            let writable = out.full && out.buf.len() <= shared.max / 2;
            if writable {
                out.full = false;
            }
            (out.closing, writable)
        };
        if writable {
            handle.writable();
        }
        if closing {
            // Replacing the state drops the handle: the inbox gets `Closed` now, the
            // close frame is still flushed.
            conn.state = State::Lingering {
                shared: shared.clone(),
                deadline: Instant::now() + LINGER,
                write_shut: false,
            };
        }
        true
    }

    /// Closes every connection with `server_stopping` (best effort, without waiting
    /// for slow peers) and releases everything; `Closed` is pushed for each open one.
    fn shutdown(&mut self) {
        for conn in self.conns.values_mut() {
            if let State::Open { shared, .. } | State::Lingering { shared, .. } = &conn.state {
                shared.close(
                    close_code(CloseReason::ServerStopping),
                    close_text(CloseReason::ServerStopping),
                );
                let _ = flush(&mut conn.stream, shared);
                let _ = conn.stream.shutdown(Shutdown::Write);
            }
        }
        self.conns.clear();
        self.listeners.clear();
    }
}

/// Hands the reader's messages to the inbox until it needs more bytes, the
/// connection must pause, or it closes.
fn process(
    handle: &ConnectionHandle,
    reader: &mut FrameReader,
    shared: &Shared,
    paused: &mut bool,
) {
    loop {
        if shared.lock().closing {
            return;
        }
        let state = match reader.next(handle.max_message_bytes()) {
            Ok(None) => return,
            Ok(Some(Incoming::Text(text))) => handle.push_message(text),
            Ok(Some(Incoming::Oversized(len))) => handle.push_oversized(len),
            Ok(Some(Incoming::Ping(payload))) => {
                frame::encode_pong(&payload, &mut shared.lock().buf);
                ReadState::Continue
            }
            Ok(Some(Incoming::Close(_))) => {
                shared.close(frame::NORMAL, "");
                return;
            }
            Err(code) => {
                shared.close(code, "protocol error");
                return;
            }
        };
        if state == ReadState::Pause {
            *paused = true;
            return;
        }
    }
}

/// Flushes the close frame, then waits for the peer to hang up; false when done.
fn linger(conn: &mut Conn, readable: bool, now: Instant, scratch: &mut [u8]) -> bool {
    let State::Lingering {
        shared,
        deadline,
        write_shut,
    } = &mut conn.state
    else {
        return false;
    };
    if now >= *deadline || flush(&mut conn.stream, shared).is_err() {
        return false;
    }
    if !*write_shut && shared.lock().buf.is_empty() {
        let _ = conn.stream.shutdown(Shutdown::Write);
        *write_shut = true;
    }
    if readable {
        loop {
            match conn.stream.read(scratch) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
    }
    true
}

/// Writes queued output until the socket would block.
fn flush(stream: &mut TcpStream, shared: &Shared) -> std::io::Result<()> {
    let mut out = shared.lock();
    while !out.buf.is_empty() {
        match stream.write(&out.buf) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => {
                out.buf.drain(..n);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    if out.buf.is_empty() && out.buf.capacity() > KEEP_CAPACITY {
        out.buf = Vec::new();
    }
    Ok(())
}

/// Writes a short answer to a fresh socket; false when it did not all go out.
fn write_small(stream: &mut TcpStream, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        match stream.write(bytes) {
            Ok(0) => return false,
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    true
}

fn is_transient(e: &std::io::Error) -> bool {
    matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
}
