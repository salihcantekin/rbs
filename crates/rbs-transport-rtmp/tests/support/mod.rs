//! A minimal RTMP server for the adapter's tests, written separately from the
//! adapter's own chunk code: it accepts publishers, answers the publish sequence and
//! records every message. Behaviour switches let tests make it ping, ask for
//! acknowledgements, reject, drop the connection or send oversized messages.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Test support.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use rbs_media_format::amf0::{self, Decoded, Value};

/// A message received from the client.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub csid: u32,
    pub type_id: u8,
    pub timestamp: u32,
    pub stream_id: u32,
    pub body: Vec<u8>,
}

impl Msg {
    pub fn command(&self) -> Option<Vec<Decoded>> {
        (self.type_id == 20).then(|| amf0::decode_all(&self.body).unwrap())
    }

    pub fn command_name(&self) -> Option<String> {
        self.command()
            .and_then(|v| v.first().and_then(|n| n.as_str().map(str::to_owned)))
    }

    pub fn is_media(&self) -> bool {
        matches!(self.type_id, 8 | 9 | 18)
    }
}

/// What one connection delivered.
#[derive(Debug, Default, Clone)]
pub struct Session {
    pub messages: Vec<Msg>,
    /// Bytes read after the handshake while discarding (sink mode).
    pub discarded_bytes: u64,
    /// Whether the client closed the connection itself (EOF).
    pub clean_eof: bool,
}

impl Session {
    pub fn media(&self) -> Vec<&Msg> {
        self.messages.iter().filter(|m| m.is_media()).collect()
    }

    pub fn commands(&self) -> Vec<String> {
        self.messages.iter().filter_map(Msg::command_name).collect()
    }

    /// The media messages as an FLV file (header, tags), for byte comparisons.
    pub fn flv(&self) -> Vec<u8> {
        let mut out = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        for m in self.media() {
            let len = m.body.len() as u32;
            let ts = m.timestamp;
            out.push(m.type_id);
            out.extend_from_slice(&len.to_be_bytes()[1..]);
            out.extend_from_slice(&ts.to_be_bytes()[1..]);
            out.push((ts >> 24) as u8);
            out.extend_from_slice(&[0, 0, 0]);
            out.extend_from_slice(&m.body);
            out.extend_from_slice(&(len + 11).to_be_bytes());
        }
        out
    }
}

/// Server behaviour.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// Answer `publish` with this error status (level "error", code, description).
    pub reject_publish: Option<(String, String)>,
    /// On the first connection, close after this many media messages.
    pub drop_after_media: Option<usize>,
    /// After `publish`, send a ping request with this timestamp.
    pub ping: Option<u32>,
    /// Before `connect`'s answer, set this Window Acknowledgement Size; after
    /// `publish`, send enough small messages to cross it.
    pub window: Option<u32>,
    /// After `publish`, send a message header announcing this many bytes.
    pub oversized: Option<u32>,
    /// After `publish`, discard everything without parsing (allocation tests).
    pub sink: bool,
    /// Never answer `connect` (the client's setup times out).
    pub silent: bool,
    /// After answering `publish`, stop reading until the test also waits on this
    /// barrier (a stall in the middle of a stream).
    pub pause: Option<Arc<std::sync::Barrier>>,
}

pub struct TestServer {
    pub port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Vec<Session>>>,
}

impl TestServer {
    pub fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut sessions = Vec::new();
            for (n, conn) in listener.incoming().enumerate() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                sessions.push(serve(conn, &behaviour, n == 0));
            }
            sessions
        });
        Self {
            port,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("rtmp://127.0.0.1:{}/live", self.port)
    }

    /// Stops accepting and returns every connection's messages. Call after the
    /// client finished.
    pub fn finish(mut self) -> Vec<Session> {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        self.thread.take().unwrap().join().unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            let _ = thread.join();
        }
    }
}

#[derive(Default, Clone)]
struct Prev {
    timestamp: u32,
    delta: u32,
    len: usize,
    type_id: u8,
    stream_id: u32,
    extended: bool,
    partial: Vec<u8>,
}

struct Reader {
    conn: TcpStream,
    chunk_size: usize,
    prev: std::collections::HashMap<u32, Prev>,
}

impl Reader {
    fn exact(&mut self, n: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0; n];
        self.conn.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn u(&mut self, n: usize) -> Option<u32> {
        let b = self.exact(n)?;
        Some(b.iter().fold(0, |acc, &x| (acc << 8) | u32::from(x)))
    }

    /// Reads chunks until one message is complete. `None` at EOF.
    fn message(&mut self) -> Option<Msg> {
        loop {
            let b0 = self.u(1)?;
            let fmt = b0 >> 6;
            let csid = match b0 & 0x3f {
                0 => 64 + self.u(1)?,
                1 => {
                    let b = self.exact(2)?;
                    64 + u32::from(b[0]) + 256 * u32::from(b[1])
                }
                c => c,
            };
            let mut p = self.prev.get(&csid).cloned().unwrap_or_default();
            let starting = p.partial.is_empty();
            let mut field = None;
            if fmt <= 2 {
                field = Some(self.u(3)?);
            }
            if fmt <= 1 {
                p.len = self.u(3)? as usize;
                p.type_id = self.u(1)? as u8;
            }
            if fmt == 0 {
                let b = self.exact(4)?;
                p.stream_id = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            }
            let extended = match field {
                Some(f) => f == 0xff_ffff,
                None => p.extended,
            };
            let value = if extended { Some(self.u(4)?) } else { field };
            if fmt <= 2 {
                p.extended = extended;
            }
            if starting {
                match (fmt, value) {
                    (0, Some(v)) => {
                        p.timestamp = v;
                        p.delta = 0;
                    }
                    (1 | 2, Some(v)) => {
                        p.delta = v;
                        p.timestamp = p.timestamp.wrapping_add(v);
                    }
                    _ => p.timestamp = p.timestamp.wrapping_add(p.delta),
                }
            }
            let take = (p.len - p.partial.len()).min(self.chunk_size);
            let data = self.exact(take)?;
            p.partial.extend_from_slice(&data);
            let done = p.partial.len() == p.len;
            let msg = done.then(|| Msg {
                csid,
                type_id: p.type_id,
                timestamp: p.timestamp,
                stream_id: p.stream_id,
                body: std::mem::take(&mut p.partial),
            });
            self.prev.insert(csid, p);
            if let Some(msg) = msg {
                if msg.type_id == 1 {
                    self.chunk_size =
                        u32::from_be_bytes(msg.body[..4].try_into().unwrap()) as usize;
                }
                return Some(msg);
            }
        }
    }
}

/// Writes one message in 128-byte chunks on chunk stream `csid`.
pub fn write_msg(out: &mut Vec<u8>, csid: u8, type_id: u8, stream_id: u32, body: &[u8]) {
    let len = body.len() as u32;
    out.push(csid);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&len.to_be_bytes()[1..]);
    out.push(type_id);
    out.extend_from_slice(&stream_id.to_le_bytes());
    for (i, piece) in body.chunks(128).enumerate() {
        if i > 0 {
            out.push(0xc0 | csid);
        }
        out.extend_from_slice(piece);
    }
}

fn command(name: &str, txn: f64, info: &[(&str, Value<'_>)], extra: Option<f64>) -> Vec<u8> {
    let mut body = Vec::new();
    amf0::string(&mut body, name);
    amf0::number(&mut body, txn);
    amf0::null(&mut body);
    if !info.is_empty() {
        amf0::object(&mut body, info);
    }
    if let Some(v) = extra {
        amf0::number(&mut body, v);
    }
    body
}

fn serve(mut conn: TcpStream, b: &Behaviour, first: bool) -> Session {
    let mut session = Session::default();
    conn.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    // Handshake: C0 C1 in, S0 S1 S2 out, C2 in.
    let mut c0c1 = vec![0; 1537];
    if conn.read_exact(&mut c0c1).is_err() {
        return session;
    }
    assert_eq!(c0c1[0], 3, "RTMP version");
    let mut s = vec![3_u8];
    s.extend((0..1536).map(|i| (i % 7) as u8));
    s.extend_from_slice(&c0c1[1..]);
    conn.write_all(&s).unwrap();
    let mut c2 = vec![0; 1536];
    conn.read_exact(&mut c2).unwrap();
    assert_eq!(&c2[..], &s[1..1537], "C2 echoes S1");
    let mut reader = Reader {
        conn: conn.try_clone().unwrap(),
        chunk_size: 128,
        prev: Default::default(),
    };
    let mut media = 0;
    let mut published = false;
    loop {
        if published && b.sink {
            let mut buf = [0_u8; 64 * 1024];
            loop {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        session.clean_eof = true;
                        return session;
                    }
                    Ok(n) => session.discarded_bytes += n as u64,
                }
            }
        }
        let Some(msg) = reader.message() else {
            session.clean_eof = true;
            return session;
        };
        let name = msg.command_name();
        let is_media = msg.is_media();
        let txn = msg
            .command()
            .and_then(|v| v.get(1).and_then(Decoded::as_number))
            .unwrap_or(0.0);
        session.messages.push(msg);
        let mut out = Vec::new();
        match name.as_deref() {
            Some("connect") if b.silent => {}
            Some("connect") => {
                if let Some(w) = b.window {
                    write_msg(&mut out, 2, 5, 0, &w.to_be_bytes());
                }
                write_msg(&mut out, 2, 6, 0, &[0, 0x26, 0x25, 0xa0, 2]);
                let body = command(
                    "_result",
                    txn,
                    &[("code", Value::String("NetConnection.Connect.Success"))],
                    None,
                );
                write_msg(&mut out, 3, 20, 0, &body);
                // Servers send onBWDone and other noise; the client must ignore it.
                write_msg(&mut out, 3, 20, 0, &command("onBWDone", 0.0, &[], None));
            }
            Some("releaseStream") => {
                // Some servers answer releaseStream with an error; harmless.
                let body = command(
                    "_error",
                    txn,
                    &[("code", Value::String("NetConnection.Call.Failed"))],
                    None,
                );
                write_msg(&mut out, 3, 20, 0, &body);
            }
            Some("createStream") => {
                let mut body = Vec::new();
                amf0::string(&mut body, "_result");
                amf0::number(&mut body, txn);
                amf0::null(&mut body);
                amf0::number(&mut body, 1.0);
                write_msg(&mut out, 3, 20, 0, &body);
            }
            Some("publish") => {
                let (level, code, description) = match &b.reject_publish {
                    Some((code, description)) => ("error", code.as_str(), description.as_str()),
                    None => ("status", "NetStream.Publish.Start", "Start publishing"),
                };
                let body = command(
                    "onStatus",
                    0.0,
                    &[
                        ("level", Value::String(level)),
                        ("code", Value::String(code)),
                        ("description", Value::String(description)),
                    ],
                    None,
                );
                write_msg(&mut out, 5, 20, 1, &body);
                published = true;
                if let Some(ts) = b.ping {
                    let mut body = vec![0, 6];
                    body.extend_from_slice(&ts.to_be_bytes());
                    write_msg(&mut out, 2, 4, 0, &body);
                }
                if let Some(w) = b.window {
                    // Stream Begin events, enough bytes to cross the window.
                    let count = (w as usize).div_ceil(18) + 1;
                    for _ in 0..count {
                        write_msg(&mut out, 2, 4, 0, &[0, 0, 0, 0, 0, 1]);
                    }
                }
                if let Some(len) = b.oversized {
                    out.push(3);
                    out.extend_from_slice(&[0, 0, 0]);
                    out.extend_from_slice(&len.to_be_bytes()[1..]);
                    out.extend_from_slice(&[20, 0, 0, 0, 0]);
                }
            }
            _ => {}
        }
        if !out.is_empty() && conn.write_all(&out).is_err() {
            return session;
        }
        if name.as_deref() == Some("publish") {
            if let Some(pause) = &b.pause {
                pause.wait();
            }
        }
        if is_media {
            media += 1;
            if first && b.drop_after_media == Some(media) {
                let _ = conn.shutdown(Shutdown::Both);
                return session;
            }
        }
    }
}

/// A listener that accepts connections in the kernel backlog and never reads.
pub struct Stalled {
    listener: TcpListener,
}

impl Stalled {
    pub fn new() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").unwrap(),
        }
    }

    pub fn url(&self) -> String {
        format!(
            "rtmp://127.0.0.1:{}/live",
            self.listener.local_addr().unwrap().port()
        )
    }
}

/// Real-looking H.264 parameter sets (x264, 320x240, constrained baseline).
pub const SPS: &[u8] = &[
    0x67, 0x42, 0xc0, 0x0d, 0xd9, 0x01, 0x41, 0xfb, 0x01, 0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00,
    0x00, 0x03, 0x03, 0xc0, 0xf1, 0x42, 0xa4, 0x80,
];
pub const PPS: &[u8] = &[0x68, 0xcb, 0x83, 0xcb, 0x20];
