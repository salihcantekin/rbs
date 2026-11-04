//! The I/O thread: one readiness poller for the listeners and every connection.
//!
//! Sources are registered in oneshot mode: after each event the source is armed again
//! with the interest its state needs now (read unless paused, write while output
//! waits). The API thread queues output and frees paused connections through shared
//! per-connection state and wakes the poller; each wake services every connection
//! (at most the socket cap).
//!
//! With network access on (ADR-0046), a connection's first byte decides its wire:
//! `0x16` starts TLS (rustls, without I/O of its own: socket bytes pass through
//! `read_tls`/`write_tls` here), anything else is plain HTTP, accepted only from a
//! loopback peer. Every write goes through the wire, and plaintext rustls decrypted
//! already is read before the thread sleeps, since it raises no readiness event.

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, TcpListener, TcpStream};
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
use crate::handshake::{self, MAX_REQUEST, UNAVAILABLE, USE_TLS};
use crate::{close_code, close_text};

/// How long a closing connection may take to receive its close frame and hang up.
const LINGER: Duration = Duration::from_secs(2);
/// Bytes read per `read` call.
const CHUNK: usize = 16 * 1024;
/// Chunks read from one connection per wake, so one busy peer cannot starve others.
const CHUNKS_PER_WAKE: usize = 4;
/// An idle output buffer larger than this is released.
const KEEP_CAPACITY: usize = 64 * 1024;
/// How long accepting pauses after an accept error other than `WouldBlock` (out of
/// file descriptors), so the listener does not fire in a busy loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(250);
/// rustls' limit on output it buffers (records and plaintext not yet sent).
const TLS_BUFFER: usize = 16 * 1024;
/// A connection whose message limit is at most this has not authenticated: its queued
/// output is capped at [`PRE_AUTH_OUTPUT`], so pongs it never reads stay small.
const PRE_AUTH_LIMIT: usize = 16 * 1024;
const PRE_AUTH_OUTPUT: usize = 16 * 1024;

pub(crate) struct Settings {
    pub policy: UpgradePolicy,
    pub upgrade_timeout: Duration,
    /// Remote connections still upgrading (TLS handshake or HTTP upgrade).
    pub max_upgrading: usize,
    pub max_queued_bytes: usize,
    pub max_sockets: usize,
    /// Sockets only loopback peers can take.
    pub loopback_reserve: usize,
    /// Sockets per remote address (IPv6: per /64).
    pub max_per_address: usize,
    /// TLS for network access, or `None` (loopback only, plain).
    pub tls: Option<Arc<rustls::ServerConfig>>,
}

/// How a connection's bytes travel.
enum Wire {
    /// Network access is on and the first byte has not arrived.
    Unknown,
    Plain,
    Tls(Box<rustls::ServerConnection>),
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
        /// `None` for a refused upgrade, whose answer was written already.
        shared: Option<Arc<Shared>>,
        deadline: Instant,
        write_shut: bool,
    },
}

struct Conn {
    stream: TcpStream,
    /// The peer's address, canonical (IPv4-mapped IPv6 as IPv4).
    peer: IpAddr,
    /// The address the connection was accepted on, canonical.
    local: Option<IpAddr>,
    wire: Wire,
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
            State::Open { shared, .. }
            | State::Lingering {
                shared: Some(shared),
                ..
            } => {
                shared.lock().gone = true;
            }
            State::Upgrading { .. } | State::Lingering { shared: None, .. } => {}
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
    /// Accepting resumes at this time after an accept error.
    accept_paused: Option<Instant>,
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
            accept_paused: None,
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
            if let Some(at) = self.accept_paused
                && Instant::now() >= at
            {
                self.accept_paused = None;
                for (key, l) in self.listeners.iter().enumerate() {
                    let _ = self.poller.modify(&l.listener, Event::readable(key));
                }
            }
            let mut readable = HashSet::new();
            for ev in events.iter() {
                if ev.key < self.listeners.len() {
                    self.accept(ev.key);
                } else if let Some(c) = self.conns.get_mut(&ev.key) {
                    c.armed = None;
                    if ev.readable {
                        readable.insert(ev.key);
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
        let conns = self
            .conns
            .values()
            .filter_map(|c| match &c.state {
                State::Upgrading { deadline, .. } | State::Lingering { deadline, .. } => {
                    Some(*deadline)
                }
                State::Open { .. } => None,
            })
            .min();
        match (conns, self.accept_paused) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn accept(&mut self, index: usize) {
        loop {
            let Some(l) = self.listeners.get(index) else {
                return;
            };
            match l.listener.accept() {
                Ok((stream, peer)) => {
                    let peer = peer.ip().to_canonical();
                    if !self.admits(peer) || stream.set_nonblocking(true).is_err() {
                        continue; // Dropping the stream closes it.
                    }
                    let local = stream.local_addr().ok().map(|a| a.ip().to_canonical());
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
                            local,
                            wire: if self.settings.tls.is_some() {
                                Wire::Unknown
                            } else {
                                Wire::Plain
                            },
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
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // Out of file descriptors or similar: the connection stays in the
                // backlog and the listener would fire again at once. Pause accepting.
                Err(_) => {
                    self.accept_paused = Some(Instant::now() + ACCEPT_BACKOFF);
                    return;
                }
            }
        }
        if let Some(l) = self.listeners.get(index) {
            let _ = self.poller.modify(&l.listener, Event::readable(index));
        }
    }

    /// Whether a new connection from `peer` is taken: the socket cap, with the last
    /// slots kept for loopback peers, and for remote peers a cap on connections still
    /// upgrading and on sockets per address, so remote peers cannot lock out local
    /// ones (ADR-0046 decision 2).
    fn admits(&self, peer: IpAddr) -> bool {
        let s = &self.settings;
        let total = self.conns.len();
        if total >= s.max_sockets {
            return false;
        }
        if peer.is_loopback() {
            return true;
        }
        if total >= s.max_sockets.saturating_sub(s.loopback_reserve) {
            return false;
        }
        let remote = || self.conns.values().filter(|c| !c.peer.is_loopback());
        let upgrading = remote()
            .filter(|c| matches!(c.state, State::Upgrading { .. }))
            .count();
        let key = address_key(peer);
        let same = remote().filter(|c| address_key(c.peer) == key).count();
        upgrading < s.max_upgrading && same < s.max_per_address
    }

    /// Services one connection; false when it is done.
    fn service(&mut self, key: usize, readable: bool, now: Instant) -> bool {
        let Some(mut conn) = self.conns.remove(&key) else {
            return false;
        };
        let keep = self.step(&mut conn, readable, now);
        if keep {
            let tls_out = wants_write(&conn.wire);
            let want = match &conn.state {
                State::Upgrading { .. } => (true, tls_out),
                State::Open {
                    handle,
                    shared,
                    paused,
                    ..
                } => {
                    let queued = shared.lock().buf.len();
                    (
                        may_read(*paused, queued, read_cap(handle, shared)),
                        queued > 0 || tls_out,
                    )
                }
                State::Lingering { shared, .. } => (
                    true,
                    tls_out || shared.as_ref().is_some_and(|s| !s.lock().buf.is_empty()),
                ),
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

    /// Reads the upgrade request. `Some(true)`: the connection is open now, or refused
    /// and lingering until its answer is read; `Some(false)`: gone; `None`: still
    /// waiting.
    fn upgrade(&mut self, conn: &mut Conn, readable: bool, now: Instant) -> Option<bool> {
        let State::Upgrading { deadline, .. } = &conn.state else {
            return Some(false);
        };
        if now >= *deadline {
            return Some(false);
        }
        if let Wire::Unknown = conn.wire {
            if !readable {
                return None;
            }
            let mut first = [0u8; 1];
            match conn.stream.peek(&mut first) {
                Ok(0) => return Some(false),
                Ok(_) => {}
                Err(e) if is_transient(&e) => return None,
                Err(_) => return Some(false),
            }
            if first[0] == 0x16 {
                let Some(config) = &self.settings.tls else {
                    return Some(false);
                };
                let Ok(mut tls) = rustls::ServerConnection::new(config.clone()) else {
                    return Some(false);
                };
                tls.set_buffer_limit(Some(TLS_BUFFER));
                conn.wire = Wire::Tls(Box::new(tls));
            } else if conn.peer.is_loopback() {
                conn.wire = Wire::Plain;
            } else {
                // Plain HTTP from another computer: told to use TLS, never served.
                conn.wire = Wire::Plain;
                refuse(conn, USE_TLS, now);
                return Some(true);
            }
        }
        let State::Upgrading { buf, deadline } = &mut conn.state else {
            return Some(false);
        };
        // Until the request is whole or would block: rustls may hold decrypted bytes
        // that raise no readiness event.
        loop {
            let room = MAX_REQUEST.saturating_sub(buf.len()).min(CHUNK);
            if room == 0 {
                break;
            }
            match read_plain(
                &mut conn.stream,
                &mut conn.wire,
                &mut self.scratch[..room],
                readable,
            ) {
                Ok(0) => return Some(false),
                Ok(n) => buf.extend_from_slice(&self.scratch[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    let _ = flush_wire(&mut conn.stream, &mut conn.wire);
                    return Some(false);
                }
            }
        }
        // The TLS handshake's own records.
        if flush_wire(&mut conn.stream, &mut conn.wire).is_err() {
            return Some(false);
        }
        let upgrade = match handshake::parse(buf, &self.settings.policy, conn.local) {
            Ok(Some(u)) => u,
            Ok(None) if now >= *deadline => return Some(false),
            Ok(None) => return None,
            Err(refusal) => {
                refuse(conn, refusal.response(), now);
                return Some(true);
            }
        };
        let shared = Arc::new(Shared {
            out: Mutex::new(Out::default()),
            max: self.settings.max_queued_bytes,
        });
        let ip = conn.peer;
        let peer = PeerInfo {
            kind: TransportKind::WebSocket,
            loopback: ip.is_loopback(),
            address: Some(ip.to_string()),
            origin: upgrade.origin.clone(),
            host: upgrade.host.clone(),
            same_user: false,
            secure: matches!(conn.wire, Wire::Tls(_)),
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
                refuse(conn, UNAVAILABLE, now);
                return Some(true);
            }
        };
        if !write_small(
            &mut conn.stream,
            &mut conn.wire,
            &handshake::accept_response(&upgrade),
        ) {
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
        // TLS: plaintext rustls already decrypted is read even without an event.
        let buffered = matches!(conn.wire, Wire::Tls(_));
        if readable || resumed || buffered {
            for _ in 0..CHUNKS_PER_WAKE {
                {
                    // A peer that does not read its answers (pongs, too) is not read
                    // either, so its output stays bounded. A paused connection is not
                    // read at all: rustls refuses records once its plaintext is full.
                    let out = shared.lock();
                    if out.closing || !may_read(*paused, out.buf.len(), read_cap(handle, shared)) {
                        break;
                    }
                }
                match read_plain(
                    &mut conn.stream,
                    &mut conn.wire,
                    &mut self.scratch,
                    readable || resumed,
                ) {
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
        if flush(&mut conn.stream, &mut conn.wire, shared).is_err() {
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
                shared: Some(shared.clone()),
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
            if let State::Open { shared, .. }
            | State::Lingering {
                shared: Some(shared),
                ..
            } = &conn.state
            {
                shared.close(
                    close_code(CloseReason::ServerStopping),
                    close_text(CloseReason::ServerStopping),
                );
                let _ = flush(&mut conn.stream, &mut conn.wire, shared);
                if let Wire::Tls(tls) = &mut conn.wire {
                    tls.send_close_notify();
                    let _ = flush_tls(&mut conn.stream, tls);
                }
                let _ = conn.stream.shutdown(Shutdown::Write);
            }
        }
        self.conns.clear();
        self.listeners.clear();
    }
}

/// Whether an open connection is read: not while paused by the inbox, and not while
/// its own output is backed up, so a peer that sends pings but never reads cannot grow
/// its pong queue (it is bounded by `max` plus one wake's worth of reads).
fn may_read(paused: bool, queued: usize, max: usize) -> bool {
    !paused && queued < max
}

/// The queued output above which an open connection is not read: small before
/// authentication (ADR-0046 decision 6).
fn read_cap(handle: &ConnectionHandle, shared: &Shared) -> usize {
    if handle.max_message_bytes() <= PRE_AUTH_LIMIT {
        shared.max.min(PRE_AUTH_OUTPUT)
    } else {
        shared.max
    }
}

/// The key per-address caps group by: the address, or for IPv6 its /64.
fn address_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::from([s[0], s[1], s[2], s[3], 0, 0, 0, 0])
        }
    }
}

/// Reads plaintext: first what rustls decrypted already, then, when `from_socket`,
/// from the socket. `Ok(0)` when the peer closed.
fn read_plain(
    stream: &mut TcpStream,
    wire: &mut Wire,
    buf: &mut [u8],
    from_socket: bool,
) -> std::io::Result<usize> {
    match wire {
        Wire::Unknown | Wire::Plain => {
            if !from_socket {
                return Err(ErrorKind::WouldBlock.into());
            }
            stream.read(buf)
        }
        Wire::Tls(tls) => loop {
            match tls.reader().read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            if !from_socket {
                return Err(ErrorKind::WouldBlock.into());
            }
            if tls.read_tls(stream)? == 0 {
                return Ok(0);
            }
            if let Err(e) = tls.process_new_packets() {
                // The alert rustls queued goes out before the connection is dropped.
                let _ = tls.write_tls(stream);
                return Err(std::io::Error::new(ErrorKind::InvalidData, e));
            }
        },
    }
}

/// Writes as much of `bytes` as the wire takes now (TLS: into rustls' bounded buffer,
/// then records to the socket until it would block).
fn write_plain(stream: &mut TcpStream, wire: &mut Wire, bytes: &[u8]) -> std::io::Result<usize> {
    match wire {
        Wire::Unknown | Wire::Plain => stream.write(bytes),
        Wire::Tls(tls) => {
            let n = tls.writer().write(bytes)?;
            flush_tls(stream, tls)?;
            if n == 0 && !bytes.is_empty() {
                return Err(ErrorKind::WouldBlock.into());
            }
            Ok(n)
        }
    }
}

/// Sends the TLS records rustls holds until the socket would block.
fn flush_tls(stream: &mut TcpStream, tls: &mut rustls::ServerConnection) -> std::io::Result<()> {
    while tls.wants_write() {
        match tls.write_tls(stream) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn flush_wire(stream: &mut TcpStream, wire: &mut Wire) -> std::io::Result<()> {
    match wire {
        Wire::Tls(tls) => flush_tls(stream, tls),
        Wire::Unknown | Wire::Plain => Ok(()),
    }
}

/// TLS records wait to be sent.
fn wants_write(wire: &Wire) -> bool {
    matches!(wire, Wire::Tls(tls) if tls.wants_write())
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
    if now >= *deadline {
        return false;
    }
    if let Some(shared) = shared
        && flush(&mut conn.stream, &mut conn.wire, shared).is_err()
    {
        return false;
    }
    if !*write_shut && shared.as_ref().is_none_or(|s| s.lock().buf.is_empty()) {
        // TLS: `close_notify` first, and the socket shut once rustls sent everything.
        let sent = match &mut conn.wire {
            Wire::Tls(tls) => {
                tls.send_close_notify();
                if flush_tls(&mut conn.stream, tls).is_err() {
                    return false;
                }
                !tls.wants_write()
            }
            Wire::Unknown | Wire::Plain => true,
        };
        if sent {
            let _ = conn.stream.shutdown(Shutdown::Write);
            *write_shut = true;
        }
    }
    if readable {
        for _ in 0..CHUNKS_PER_WAKE {
            match read_plain(&mut conn.stream, &mut conn.wire, scratch, true) {
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

/// Answers a refused upgrade and lingers, so the peer reads the answer before the
/// socket closes (closing with its request unread would reset the connection). Over
/// TLS the answer is encrypted, and lingering sends it and `close_notify`.
fn refuse(conn: &mut Conn, answer: &[u8], now: Instant) {
    let written = write_small(&mut conn.stream, &mut conn.wire, answer);
    let tls = matches!(conn.wire, Wire::Tls(_));
    if !tls {
        let _ = conn.stream.shutdown(Shutdown::Write);
    }
    conn.state = State::Lingering {
        shared: None,
        deadline: if written { now + LINGER } else { now },
        write_shut: !tls,
    };
}

/// Writes queued output until the socket would block.
fn flush(stream: &mut TcpStream, wire: &mut Wire, shared: &Shared) -> std::io::Result<()> {
    let mut out = shared.lock();
    while !out.buf.is_empty() {
        match write_plain(stream, wire, &out.buf) {
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
    drop(out);
    flush_wire(stream, wire)
}

/// Writes a short answer to a fresh connection; false when it did not all go out.
/// Over TLS it is taken whole into rustls (far below its buffer limit) and sent as the
/// socket allows; what remains goes out with the next flush.
fn write_small(stream: &mut TcpStream, wire: &mut Wire, mut bytes: &[u8]) -> bool {
    if let Wire::Tls(tls) = wire {
        return tls.writer().write_all(bytes).is_ok() && flush_tls(stream, tls).is_ok();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backed_up_or_paused_connection_is_not_read() {
        assert!(may_read(false, 0, 1024));
        assert!(may_read(false, 1023, 1024));
        assert!(!may_read(false, 1024, 1024));
        assert!(!may_read(true, 0, 1024));
    }
}
