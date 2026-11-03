//! The I/O thread: connects, publishes, writes queued packets and answers the
//! server's control messages, reconnecting by the session's policy.
//!
//! One thread owns the socket and is its only writer. The socket is blocking with
//! short timeouts ([`SLICE`]), so every wait re-checks the abort flag. Server
//! messages are read when the thread is idle and at least every [`READ_INTERVAL`]
//! while it is busy, with the socket switched to non-blocking for that read.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rbs_core::media::EncodedPacket;
use rbs_media_format::amf0::Decoded;
use rbs_media_format::flv::{BodyPlan, FlvTracks, TrackKind};
use rbs_transport::{ReconnectPolicy, TransportError};

use crate::chunk::{self, ChunkReader};
use crate::command::{self, Reply};
use crate::url::RtmpUrl;

/// Longest single blocking socket call, so aborts are seen promptly.
pub(crate) const SLICE: Duration = Duration::from_millis(50);
/// Per address, for the TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// From the TCP connection to `NetStream.Publish.Start`.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a clean close waits for the server to close after our FIN.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Safety net of the park/unpark handshake, and the idle read interval.
const IDLE_WAIT: Duration = Duration::from_millis(100);
/// Server messages are read at least this often while sending.
const READ_INTERVAL: Duration = Duration::from_millis(100);
/// Chunk size announced with Set Chunk Size and used for every message after it.
pub(crate) const CHUNK_SIZE: usize = 4096;
/// Largest message accepted from the server, checked from the chunk header before
/// the body is stored or AMF0-decoded. Servers send only small control messages.
pub(crate) const MAX_MESSAGE: usize = 64 * 1024;
const HANDSHAKE_LEN: usize = 1536;
const RTMP_VERSION: u8 = 3;

/// One packet committed to the hand-off: validated, timed and sized by the producer.
pub(crate) struct Item {
    pub packet: EncodedPacket,
    pub plan: BodyPlan,
    /// RTMP timestamp (decode time) in milliseconds, wrapping at 2^32.
    pub ms: u32,
    /// Composition offset in milliseconds.
    pub cts: i64,
}

/// Written by the I/O thread, read by the producer; on its own cache line.
#[derive(Default)]
#[repr(align(128))]
pub(crate) struct Progress {
    pub bytes_sent: AtomicU64,
    /// Items taken from the hand-off and finished with (sent or discarded).
    pub done_items: AtomicU64,
    /// Items taken from the hand-off and discarded (before a keyframe after a
    /// (re)connect, or when the session ended).
    pub discarded: AtomicU64,
    /// Successful publishes (1 after the first connection).
    pub publishes: AtomicU32,
    /// Reconnect attempts started since the session began.
    pub attempts: AtomicU32,
    /// Number of the current reconnect attempt (1 for the first after a loss).
    pub attempt: AtomicU32,
    pub sleeping: AtomicBool,
}

/// State shared with the I/O thread: atomics and set-once cells only (P6).
#[derive(Default)]
pub(crate) struct Shared {
    pub progress: Progress,
    /// Finish: send what is queued, close cleanly and exit. No new connection
    /// attempt starts after a failure once this is set.
    pub stop: AtomicBool,
    /// Give up now: discard what is queued and exit.
    pub abort: AtomicBool,
    /// Ask the encoder for a keyframe (set when a connection is lost).
    pub keyframe_wanted: AtomicBool,
    /// The error that ended the session for good.
    pub error: OnceLock<TransportError>,
}

/// What the I/O thread needs. The key is the stream key in clear text: it goes on
/// the wire only, and is removed from every server text that becomes an error.
pub(crate) struct IoConfig {
    pub url: RtmpUrl,
    pub key: String,
    pub reconnect: ReconnectPolicy,
    pub tracks: FlvTracks,
}

enum Fail {
    Aborted,
    Error(TransportError),
}

impl From<TransportError> for Fail {
    fn from(e: TransportError) -> Self {
        Fail::Error(e)
    }
}

fn connection(what: &str, e: &std::io::Error) -> Fail {
    Fail::Error(TransportError::Connection(format!("{what}: {e}")))
}

fn protocol(what: &str) -> Fail {
    Fail::Error(TransportError::Connection(format!(
        "protocol error: {what}"
    )))
}

/// Server text without the stream key, and bounded in length.
fn redact(text: &str, key: &str) -> String {
    let mut text = if key.is_empty() {
        text.to_owned()
    } else {
        text.replace(key, "<redacted>")
    };
    if text.len() > 256 {
        let mut end = 256;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn rejected(what: &str, status: &command::Status, key: &str) -> Fail {
    let text = format!("{what}: {} {}", status.code, status.description);
    Fail::Error(TransportError::Rejected(redact(text.trim_end(), key)))
}

/// One connection to the server.
struct Conn {
    stream: TcpStream,
    reader: ChunkReader,
    /// Message stream id from `createStream`.
    stream_id: u32,
    bytes_in: u64,
    acked: u64,
    /// The server's Window Acknowledgement Size: acknowledge every this many bytes.
    window: u32,
    /// The last Window Acknowledgement Size we sent.
    sent_window: u32,
    replies: VecDeque<Reply>,
    /// Control replies (acknowledgements, pongs), written between messages.
    control: Vec<u8>,
    read_buf: Box<[u8]>,
}

impl Conn {
    fn write_all(&mut self, data: &[u8], shared: &Shared) -> Result<(), Fail> {
        write_all(&mut self.stream, data, shared)
    }

    /// Reads what the server sent without waiting; handles control messages and
    /// queues command replies.
    fn poll_reads(&mut self, shared: &Shared, key: &str) -> Result<(), Fail> {
        self.stream
            .set_nonblocking(true)
            .map_err(|e| connection("socket", &e))?;
        let result = loop {
            match self.stream.read(&mut self.read_buf) {
                Ok(0) => break Err(closed()),
                Ok(n) => {
                    if let Err(e) = self.received(n, shared, key) {
                        break Err(e);
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break Ok(()),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => break Err(connection("receive", &e)),
            }
        };
        self.stream
            .set_nonblocking(false)
            .map_err(|e| connection("socket", &e))?;
        result
    }

    /// One blocking read of at most [`SLICE`]; `Ok` also when nothing arrived.
    fn read_once(&mut self, shared: &Shared, key: &str) -> Result<(), Fail> {
        match self.stream.read(&mut self.read_buf) {
            Ok(0) => Err(closed()),
            Ok(n) => self.received(n, shared, key),
            Err(e) if is_timeout(&e) => Ok(()),
            Err(e) => Err(connection("receive", &e)),
        }
    }

    fn received(&mut self, n: usize, shared: &Shared, key: &str) -> Result<(), Fail> {
        self.bytes_in += n as u64;
        self.reader.feed(&self.read_buf[..n]);
        loop {
            let action = match self.reader.next().map_err(protocol)? {
                None => break,
                Some(m) => classify(m.type_id, m.body)?,
            };
            match action {
                Action::None => {}
                Action::ChunkSize(size) => self.reader.set_chunk_size(size).map_err(protocol)?,
                Action::Abort(csid) => self.reader.abort(csid),
                Action::Window(size) => self.window = size,
                Action::PeerBandwidth(size) => {
                    if size != self.sent_window {
                        self.sent_window = size;
                        chunk::write_control(&mut self.control, chunk::MSG_WINDOW_ACK_SIZE, size);
                    }
                }
                Action::Ping(ts) => chunk::write_ping_response(&mut self.control, ts),
                Action::Reply(reply) => {
                    match &reply {
                        Reply::Status(s) if s.is_error() => {
                            return Err(rejected("server status", s, key));
                        }
                        _ => {}
                    }
                    if self.replies.len() >= 16 {
                        self.replies.pop_front();
                    }
                    self.replies.push_back(reply);
                }
            }
        }
        if self.window > 0 && self.bytes_in - self.acked >= u64::from(self.window) {
            self.acked = self.bytes_in;
            // The sequence number is the byte count, wrapping at 2^32.
            chunk::write_control(&mut self.control, chunk::MSG_ACK, self.bytes_in as u32);
        }
        if !self.control.is_empty() {
            let control = std::mem::take(&mut self.control);
            let written = write_all(&mut self.stream, &control, shared);
            self.control = control;
            self.control.clear();
            written?;
        }
        Ok(())
    }

    /// Waits for a reply that `pick` accepts, up to `deadline`.
    fn wait_reply<T>(
        &mut self,
        shared: &Shared,
        key: &str,
        deadline: Instant,
        what: &str,
        mut pick: impl FnMut(Reply) -> Option<Result<T, Fail>>,
    ) -> Result<T, Fail> {
        loop {
            while let Some(reply) = self.replies.pop_front() {
                if let Some(result) = pick(reply) {
                    return result;
                }
            }
            if shared.abort.load(Ordering::Acquire) {
                return Err(Fail::Aborted);
            }
            if Instant::now() >= deadline {
                return Err(Fail::Error(TransportError::Connection(format!(
                    "no answer to {what}"
                ))));
            }
            self.read_once(shared, key)?;
        }
    }
}

fn closed() -> Fail {
    Fail::Error(TransportError::Connection(
        "the server closed the connection".into(),
    ))
}

fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

/// Writes all of `data`, in calls of at most [`SLICE`] each, re-checking the abort
/// flag between them. Partial writes continue where they stopped, so a slow peer
/// never tears a message.
fn write_all(stream: &mut TcpStream, mut data: &[u8], shared: &Shared) -> Result<(), Fail> {
    while !data.is_empty() {
        if shared.abort.load(Ordering::Acquire) {
            return Err(Fail::Aborted);
        }
        match stream.write(data) {
            Ok(0) => return Err(closed()),
            Ok(n) => {
                data = &data[n..];
                shared
                    .progress
                    .bytes_sent
                    .fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) if is_timeout(&e) => {}
            Err(e) => return Err(connection("send", &e)),
        }
    }
    Ok(())
}

enum Action {
    None,
    ChunkSize(u32),
    Abort(u32),
    Window(u32),
    PeerBandwidth(u32),
    Ping([u8; 4]),
    Reply(Reply),
}

fn be_u32(body: &[u8]) -> Option<u32> {
    body.get(..4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn classify(type_id: u8, body: &[u8]) -> Result<Action, Fail> {
    let value = || be_u32(body).ok_or_else(|| protocol("short control message"));
    Ok(match type_id {
        chunk::MSG_SET_CHUNK_SIZE => Action::ChunkSize(value()?),
        chunk::MSG_ABORT => Action::Abort(value()?),
        chunk::MSG_WINDOW_ACK_SIZE => Action::Window(value()?),
        chunk::MSG_SET_PEER_BANDWIDTH => Action::PeerBandwidth(value()?),
        chunk::MSG_USER_CONTROL => match body {
            [e1, e0, t3, t2, t1, t0, ..]
                if u16::from_be_bytes([*e1, *e0]) == chunk::UC_PING_REQUEST =>
            {
                Action::Ping([*t3, *t2, *t1, *t0])
            }
            _ => Action::None,
        },
        chunk::MSG_COMMAND_AMF0 | chunk::MSG_COMMAND_AMF3 => {
            // An AMF3 command message starts with a format byte, then AMF0 values.
            let body = match (type_id, body) {
                (chunk::MSG_COMMAND_AMF3, [0, rest @ ..]) => rest,
                _ => body,
            };
            Action::Reply(command::parse_reply(body).map_err(protocol)?)
        }
        _ => Action::None,
    })
}

/// Appends a command message (its body built by `build` in `body`) to `out`.
fn push_command(
    body: &mut Vec<u8>,
    out: &mut Vec<u8>,
    stream_id: u32,
    build: impl FnOnce(&mut Vec<u8>),
) {
    body.clear();
    build(body);
    chunk::write_message(
        out,
        chunk::CSID_COMMAND,
        0,
        chunk::MSG_COMMAND_AMF0,
        stream_id,
        body,
        CHUNK_SIZE,
    );
}

/// The I/O thread's state.
pub(crate) struct Io {
    config: IoConfig,
    shared: Arc<Shared>,
    consumer: rtrb::Consumer<Item>,
    /// Message bodies are built here, then chunked into `out`; both are reused.
    body: Vec<u8>,
    out: Vec<u8>,
}

impl Io {
    pub(crate) fn new(
        config: IoConfig,
        shared: Arc<Shared>,
        consumer: rtrb::Consumer<Item>,
    ) -> Self {
        Self {
            config,
            shared,
            consumer,
            body: Vec::with_capacity(64 * 1024),
            out: Vec::with_capacity(64 * 1024),
        }
    }

    /// The thread body. Signals `done` only after the socket is closed.
    pub(crate) fn run(mut self, done: mpsc::Sender<()>) {
        let (initial, max, attempts) = match self.config.reconnect {
            ReconnectPolicy::Backoff {
                initial,
                max,
                attempts,
            } => (initial, max, attempts),
            _ => (Duration::ZERO, Duration::ZERO, Some(0)),
        };
        let mut attempt = 0_u32;
        let mut delay = initial;
        loop {
            let mut published = false;
            let result = self.connect().and_then(|conn| {
                published = true;
                self.stream(conn)
            });
            if published {
                attempt = 0;
                delay = initial;
            }
            let error = match result {
                Ok(()) | Err(Fail::Aborted) => break,
                Err(Fail::Error(e)) => e,
            };
            let retry = error.is_recoverable() && attempts.is_none_or(|n| attempt < n);
            if !retry {
                let _ = self.shared.error.set(error);
                break;
            }
            if self.finishing() {
                // `finish` with nothing left to deliver: no reason to reconnect.
                break;
            }
            attempt += 1;
            let progress = &self.shared.progress;
            progress.attempt.store(attempt, Ordering::Relaxed);
            progress.attempts.fetch_add(1, Ordering::Release);
            self.shared.keyframe_wanted.store(true, Ordering::Release);
            if !self.back_off(delay) {
                break;
            }
            delay = (delay * 2).min(max.max(initial));
        }
        self.discard_all();
        let _ = done.send(());
    }

    /// `finish` was called and nothing is left in the hand-off.
    fn finishing(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire) && self.consumer.is_empty()
    }

    /// Waits `delay` before the next attempt; `false` if the session is aborted, or
    /// finishing with nothing left to deliver, meanwhile.
    fn back_off(&self, delay: Duration) -> bool {
        let until = Instant::now() + delay;
        loop {
            if self.shared.abort.load(Ordering::Acquire) || self.finishing() {
                return false;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            thread::park_timeout(left);
        }
    }

    fn discard_all(&mut self) {
        let progress = &self.shared.progress;
        while let Ok(item) = self.consumer.pop() {
            drop(item);
            progress.discarded.fetch_add(1, Ordering::Relaxed);
            progress.done_items.fetch_add(1, Ordering::Release);
        }
    }

    /// TCP connection, handshake and the publish command sequence.
    fn connect(&mut self) -> Result<Conn, Fail> {
        let url = &self.config.url;
        let addrs = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| connection("resolve", &e))?;
        let mut last = None;
        let mut stream = None;
        for addr in addrs {
            if self.shared.abort.load(Ordering::Acquire) {
                return Err(Fail::Aborted);
            }
            match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let stream = match (stream, last) {
            (Some(s), _) => s,
            (None, Some(e)) => return Err(connection("connect", &e)),
            (None, None) => {
                return Err(Fail::Error(TransportError::Connection(
                    "the host has no address".into(),
                )));
            }
        };
        let deadline = Instant::now() + SETUP_TIMEOUT;
        let setup = |e: std::io::Error| connection("socket", &e);
        stream.set_nodelay(true).map_err(setup)?;
        stream.set_read_timeout(Some(SLICE)).map_err(setup)?;
        stream.set_write_timeout(Some(SLICE)).map_err(setup)?;
        let mut conn = Conn {
            stream,
            reader: ChunkReader::new(MAX_MESSAGE),
            stream_id: 0,
            bytes_in: 0,
            acked: 0,
            window: 0,
            sent_window: 0,
            replies: VecDeque::new(),
            control: Vec::with_capacity(64),
            read_buf: vec![0; 16 * 1024].into_boxed_slice(),
        };
        self.handshake(&mut conn, deadline)?;
        let shared = Arc::clone(&self.shared);
        let key = self.config.key.clone();
        let key = key.as_str();

        self.out.clear();
        chunk::write_control(&mut self.out, chunk::MSG_SET_CHUNK_SIZE, CHUNK_SIZE as u32);
        push_command(&mut self.body, &mut self.out, 0, |out| {
            command::connect(out, &self.config.url.app, &self.config.url.tc_url)
        });
        conn.write_all(&self.out, &shared)?;
        conn.wait_reply(&shared, key, deadline, "connect", |reply| match reply {
            Reply::Result { txn, .. } if txn == command::TXN_CONNECT => Some(Ok(())),
            Reply::Error { txn, info } if txn == command::TXN_CONNECT => {
                Some(Err(rejected("connect", &info, key)))
            }
            _ => None,
        })?;

        self.out.clear();
        push_command(&mut self.body, &mut self.out, 0, |out| {
            command::release_stream(out, key)
        });
        push_command(&mut self.body, &mut self.out, 0, |out| {
            command::fc_publish(out, key)
        });
        push_command(&mut self.body, &mut self.out, 0, command::create_stream);
        conn.write_all(&self.out, &shared)?;
        conn.stream_id =
            conn.wait_reply(
                &shared,
                key,
                deadline,
                "createStream",
                |reply| match reply {
                    Reply::Result { txn, value } if txn == command::TXN_CREATE_STREAM => {
                        Some(match value.as_ref().and_then(Decoded::as_number) {
                            Some(id) if id >= 0.0 && id <= f64::from(u32::MAX) => Ok(id as u32),
                            _ => Err(protocol("createStream without a stream id")),
                        })
                    }
                    Reply::Error { txn, info } if txn == command::TXN_CREATE_STREAM => {
                        Some(Err(rejected("createStream", &info, key)))
                    }
                    _ => None,
                },
            )?;

        self.out.clear();
        push_command(&mut self.body, &mut self.out, conn.stream_id, |out| {
            command::publish(out, key)
        });
        conn.write_all(&self.out, &shared)?;
        conn.wait_reply(&shared, key, deadline, "publish", |reply| match reply {
            Reply::Status(s) if s.code == "NetStream.Publish.Start" => Some(Ok(())),
            Reply::Status(s) if s.is_error() => Some(Err(rejected("publish", &s, key))),
            Reply::Error { info, .. } => Some(Err(rejected("publish", &info, key))),
            _ => None,
        })?;
        Ok(conn)
    }

    /// The simple handshake: C0+C1, read S0+S1+S2, C2 echoes S1.
    fn handshake(&mut self, conn: &mut Conn, deadline: Instant) -> Result<(), Fail> {
        let shared = &*self.shared;
        let mut c0c1 = vec![0_u8; 1 + HANDSHAKE_LEN];
        c0c1[0] = RTMP_VERSION;
        // Time and zero fields stay 0; the rest is arbitrary (no digest: the simple
        // handshake that ingest servers accept from encoders).
        let mut seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            | 1;
        for b in &mut c0c1[9..] {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = seed as u8;
        }
        conn.write_all(&c0c1, shared)?;
        let mut s = vec![0_u8; 1 + 2 * HANDSHAKE_LEN];
        let mut have = 0;
        while have < s.len() {
            if shared.abort.load(Ordering::Acquire) {
                return Err(Fail::Aborted);
            }
            if Instant::now() >= deadline {
                return Err(Fail::Error(TransportError::Connection(
                    "handshake timed out".into(),
                )));
            }
            match conn.stream.read(&mut s[have..]) {
                Ok(0) => return Err(closed()),
                Ok(n) => {
                    if have == 0 && s[0] != RTMP_VERSION {
                        return Err(protocol("the server does not speak RTMP version 3"));
                    }
                    have += n;
                }
                Err(e) if is_timeout(&e) => {}
                Err(e) => return Err(connection("handshake", &e)),
            }
        }
        conn.bytes_in += s.len() as u64;
        conn.write_all(&s[1..=HANDSHAKE_LEN], shared)
    }

    /// Sends queued packets until the session stops or the connection fails.
    fn stream(&mut self, mut conn: Conn) -> Result<(), Fail> {
        let shared = Arc::clone(&self.shared);
        let progress = &shared.progress;
        progress.publishes.fetch_add(1, Ordering::Release);
        let key = std::mem::take(&mut self.config.key);
        let result = self.stream_loop(&mut conn, &shared, &key);
        self.config.key = key;
        result
    }

    fn stream_loop(&mut self, conn: &mut Conn, shared: &Shared, key: &str) -> Result<(), Fail> {
        let progress = &shared.progress;
        // Each connection starts at a keyframe, after the headers.
        let mut waiting_for_key = self.config.tracks.has_video();
        let mut headers_sent = false;
        let mut next_read = Instant::now() + READ_INTERVAL;
        loop {
            if shared.abort.load(Ordering::Acquire) {
                return Err(Fail::Aborted);
            }
            match self.consumer.pop() {
                Ok(item) => {
                    let skip = waiting_for_key
                        && !(item.plan.kind == TrackKind::Video && item.packet.keyframe());
                    let result = if skip {
                        progress.discarded.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    } else {
                        waiting_for_key = false;
                        self.send_item(conn, &item, &mut headers_sent, shared)
                    };
                    drop(item);
                    progress.done_items.fetch_add(1, Ordering::Release);
                    result?;
                    let now = Instant::now();
                    if now >= next_read {
                        next_read = now + READ_INTERVAL;
                        conn.poll_reads(shared, key)?;
                    }
                }
                Err(_) => {
                    if shared.stop.load(Ordering::Acquire) && self.consumer.is_empty() {
                        return self.close(conn, shared, key);
                    }
                    // Announce the nap, then look again. The fence pairs with the one in
                    // the producer's `wake`: either it sees `sleeping` and unparks us, or
                    // we see its item.
                    progress.sleeping.store(true, Ordering::SeqCst);
                    fence(Ordering::SeqCst);
                    if self.consumer.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                        thread::park_timeout(IDLE_WAIT);
                    }
                    progress.sleeping.store(false, Ordering::SeqCst);
                    next_read = Instant::now() + READ_INTERVAL;
                    conn.poll_reads(shared, key)?;
                }
            }
        }
    }

    fn send_item(
        &mut self,
        conn: &mut Conn,
        item: &Item,
        headers_sent: &mut bool,
        shared: &Shared,
    ) -> Result<(), Fail> {
        self.out.clear();
        if !*headers_sent {
            *headers_sent = true;
            self.headers(conn.stream_id, item.ms);
        }
        self.body.clear();
        self.config
            .tracks
            .write_body(item.plan, &item.packet, item.cts, &mut self.body);
        let (csid, type_id) = match item.plan.kind {
            TrackKind::Video => (chunk::CSID_VIDEO, chunk::MSG_VIDEO),
            TrackKind::Audio => (chunk::CSID_AUDIO, chunk::MSG_AUDIO),
        };
        self.out
            .reserve(chunk::message_size(self.body.len(), CHUNK_SIZE, item.ms));
        chunk::write_message(
            &mut self.out,
            csid,
            item.ms,
            type_id,
            conn.stream_id,
            &self.body,
            CHUNK_SIZE,
        );
        conn.write_all(&self.out, shared)
    }

    /// Appends `@setDataFrame onMetaData` and the sequence headers, stamped with the
    /// time of the first packet that follows them.
    fn headers(&mut self, stream_id: u32, ms: u32) {
        let tracks = &self.config.tracks;
        let mut message = |body: &mut Vec<u8>, csid, type_id| {
            chunk::write_message(
                &mut self.out,
                csid,
                ms,
                type_id,
                stream_id,
                body,
                CHUNK_SIZE,
            );
            body.clear();
        };
        self.body.clear();
        command::set_data_frame(&mut self.body, |out| tracks.metadata_body(out));
        message(&mut self.body, chunk::CSID_DATA, chunk::MSG_DATA_AMF0);
        if tracks.video_sequence_header(&mut self.body) {
            message(&mut self.body, chunk::CSID_VIDEO, chunk::MSG_VIDEO);
        }
        if tracks.audio_sequence_header(&mut self.body) {
            message(&mut self.body, chunk::CSID_AUDIO, chunk::MSG_AUDIO);
        }
    }

    /// `FCUnpublish` and `deleteStream`, then FIN, then waits briefly for the server
    /// to close, so unread server data cannot turn the close into a reset that
    /// discards what we sent.
    fn close(&mut self, conn: &mut Conn, shared: &Shared, key: &str) -> Result<(), Fail> {
        self.out.clear();
        push_command(&mut self.body, &mut self.out, 0, |out| {
            command::fc_unpublish(out, key)
        });
        let stream_id = conn.stream_id;
        push_command(&mut self.body, &mut self.out, 0, |out| {
            command::delete_stream(out, stream_id)
        });
        conn.write_all(&self.out, shared)?;
        let _ = conn.stream.shutdown(Shutdown::Write);
        let until = Instant::now() + CLOSE_WAIT;
        while Instant::now() < until && !shared.abort.load(Ordering::Acquire) {
            match conn.stream.read(&mut conn.read_buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if is_timeout(&e) => {}
                Err(_) => break,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_the_key_and_bounds_server_text() {
        let text = redact("stream live_abc is already publishing", "live_abc");
        assert_eq!(text, "stream <redacted> is already publishing");
        assert_eq!(redact(&"é".repeat(300), "").len(), 256);
    }
}
