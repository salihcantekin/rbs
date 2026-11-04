//! A minimal SRT listener for the transport's tests: the handshake version 5
//! listener side (induction with a cookie, conclusion with HSRSP or a rejection),
//! data reception in sequence order, periodic full ACKs and loss reports. Written
//! from the protocol description, independently of the adapter's code, so a shared
//! misreading would have to happen twice. Test support only.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)] // Not every test uses all.

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const MASK: u32 = 0x7FFF_FFFF;
const ACK_INTERVAL: Duration = Duration::from_millis(10);
/// Packets tracked beyond the first missing one (more than any flow window used).
const RING: u32 = 16384;

/// Listener behaviour.
#[derive(Clone)]
pub struct Behaviour {
    /// Count received payload without storing it (allocation-free after set-up).
    pub sink: bool,
    /// Flow window announced in the handshake (packets).
    pub flow_window: u32,
    /// While set, data is received but neither acknowledged nor reported lost.
    pub gate: Option<Arc<AtomicBool>>,
    /// Answer the conclusion with this rejection reason (sent as 1000 + reason, as
    /// libsrt does).
    pub reject: Option<u32>,
    /// Lose the first arrival of every n-th data packet (by sequence number).
    pub lose_every: Option<u32>,
    /// After this many data packets of the first connection, send a shutdown and
    /// forget that connection.
    pub close_after: Option<u64>,
    /// Latency (ms) of the listener; it answers with the larger of this and the
    /// caller's.
    pub latency_ms: u16,
    /// Never answer handshakes.
    pub silent: bool,
    /// Read nothing before the test also waits on this barrier (a listener that
    /// comes up after the caller started: its handshake waits).
    pub accept_gate: Option<Arc<std::sync::Barrier>>,
}

impl Default for Behaviour {
    fn default() -> Self {
        Self {
            sink: false,
            flow_window: 8192,
            gate: None,
            reject: None,
            lose_every: None,
            close_after: None,
            latency_ms: 120,
            silent: false,
            accept_gate: None,
        }
    }
}

/// What one connection delivered.
#[derive(Debug, Default, Clone)]
pub struct Session {
    pub stream_id: Option<String>,
    /// The caller's HSREQ: version, flags, delays.
    pub hsreq: Option<[u32; 3]>,
    pub isn: u32,
    /// Payload in sequence order, each packet once (empty in sink mode).
    pub payload: Vec<u8>,
    /// Message word (PP, O, KK, R bits and message number) and payload size of each
    /// packet, in sequence order (first arrival).
    pub packets: Vec<(u32, usize)>,
    /// Sequence numbers never received below the highest received one.
    pub missing: u64,
    pub received_bytes: u64,
    pub data_packets: u64,
    pub retransmitted: u64,
    pub ackacks: u64,
    pub keepalives: u64,
    pub dropreqs: Vec<(u32, u32)>,
    pub shutdown: bool,
}

struct Conn {
    caller_id: u32,
    own_id: u32,
    addr: SocketAddr,
    session: Session,
    /// Offset -> (message word, payload), non-sink mode.
    data: BTreeMap<u32, (u32, Vec<u8>)>,
    lost_once: Vec<u32>,
    /// Arrival flags of the offsets from `next` on, indexed modulo [`RING`].
    seen: Box<[bool]>,
    /// Offset (from the ISN) of the first packet not yet received: everything
    /// before it arrived.
    next: u32,
    /// Offset of the highest packet received.
    highest: Option<u32>,
    ack_no: u32,
    acked: Option<u32>,
    last_tick: Instant,
    closed: bool,
}

impl Conn {
    fn offset(&self, seq: u32) -> u32 {
        seq.wrapping_sub(self.session.isn) & MASK
    }

    fn seq(&self, offset: u32) -> u32 {
        self.session.isn.wrapping_add(offset) & MASK
    }
}

/// A local SRT listener on 127.0.0.1.
pub struct TestServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    sessions: Arc<Mutex<Vec<Session>>>,
}

impl TestServer {
    pub fn start(behaviour: Behaviour) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(2)))
            .unwrap();
        let addr = socket.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let stop = Arc::clone(&stop);
            let sessions = Arc::clone(&sessions);
            thread::spawn(move || serve(&socket, &behaviour, &stop, &sessions))
        };
        Self {
            addr,
            stop,
            thread: Some(thread),
            sessions,
        }
    }

    pub fn url(&self) -> String {
        format!("srt://{}", self.addr)
    }

    /// Stops the listener and returns the sessions in connection order.
    pub fn finish(mut self) -> Vec<Session> {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        std::mem::take(&mut *self.sessions.lock().unwrap())
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn word(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i * 4..i * 4 + 4].try_into().unwrap())
}

fn header(out: &mut Vec<u8>, kind: u16, info: u32, dest: u32) {
    out.clear();
    out.extend_from_slice(&(0x8000_0000 | (u32::from(kind) << 16)).to_be_bytes());
    out.extend_from_slice(&info.to_be_bytes());
    out.extend_from_slice(&0_u32.to_be_bytes());
    out.extend_from_slice(&dest.to_be_bytes());
}

fn words(out: &mut Vec<u8>, ws: &[u32]) {
    for w in ws {
        out.extend_from_slice(&w.to_be_bytes());
    }
}

fn serve(socket: &UdpSocket, b: &Behaviour, stop: &AtomicBool, sessions: &Mutex<Vec<Session>>) {
    let mut conns: Vec<Conn> = Vec::with_capacity(8);
    let mut buf = vec![0_u8; 2048];
    let mut out = Vec::with_capacity(2048);
    let mut next_id = 1000_u32;
    let mut ever_connected = 0_usize;
    if let Some(gate) = &b.accept_gate {
        gate.wait();
    }
    loop {
        // Once stopped, the packets already received are still read (a client's last
        // shutdown may be queued when `finish` is called); the loop ends at the first
        // read that finds none.
        let stopping = stop.load(Ordering::SeqCst);
        let received = socket.recv_from(&mut buf);
        if stopping && received.is_err() {
            break;
        }
        if let Ok((n, from)) = received {
            let p = &buf[..n];
            if n >= 16 && p[0] & 0x80 != 0 {
                let kind = ((word(p, 0) >> 16) & 0x7FFF) as u16;
                let dest = word(p, 3);
                if kind == 0 && dest == 0 && n >= 64 && !b.silent {
                    handshake(
                        socket,
                        b,
                        p,
                        from,
                        &mut conns,
                        &mut out,
                        &mut next_id,
                        &mut ever_connected,
                    );
                } else if let Some(c) = conns.iter_mut().find(|c| c.own_id == dest && !c.closed) {
                    match kind {
                        1 => c.session.keepalives += 1,
                        5 => c.session.shutdown = true,
                        6 => c.session.ackacks += 1,
                        7 if n >= 24 => c
                            .session
                            .dropreqs
                            .push((word(p, 4) & MASK, word(p, 5) & MASK)),
                        _ => {}
                    }
                }
            } else if n >= 16 {
                let dest = word(p, 3);
                if let Some(c) = conns.iter_mut().find(|c| c.own_id == dest && !c.closed) {
                    data(socket, b, c, p, &mut out);
                }
            }
        }
        let gated = b.gate.as_ref().is_some_and(|g| g.load(Ordering::SeqCst));
        for c in conns.iter_mut().filter(|c| !c.closed) {
            if !gated && c.last_tick.elapsed() >= ACK_INTERVAL {
                c.last_tick = Instant::now();
                tick(socket, b, c, &mut out);
            }
        }
    }
    let mut done = sessions.lock().unwrap();
    for c in conns {
        let mut s = c.session;
        if !b.sink {
            let mut expected = 0;
            for (k, (msg, v)) in &c.data {
                s.missing += u64::from(k - expected);
                expected = k + 1;
                s.payload.extend_from_slice(v);
                s.packets.push((*msg, v.len()));
            }
        }
        done.push(s);
    }
}

#[allow(clippy::too_many_arguments)]
fn handshake(
    socket: &UdpSocket,
    b: &Behaviour,
    p: &[u8],
    from: SocketAddr,
    conns: &mut Vec<Conn>,
    out: &mut Vec<u8>,
    next_id: &mut u32,
    ever: &mut usize,
) {
    let cif = &p[16..];
    let (version, isn, kind, caller_id, cookie) = (
        word(cif, 0),
        word(cif, 2),
        word(cif, 5),
        word(cif, 6),
        word(cif, 7),
    );
    let expected_cookie = 0x5EED_0000 ^ caller_id;
    if kind == 1 {
        assert_eq!(version, 4, "the caller's induction is version 4");
        header(out, 0, 0, caller_id);
        words(
            out,
            &[
                5,
                0x4A17,
                isn,
                1500,
                b.flow_window,
                1,
                0,
                expected_cookie,
                0,
                0,
                0,
                0,
            ],
        );
        socket.send_to(out, from).unwrap();
        return;
    }
    if kind != 0xFFFF_FFFF || cookie != expected_cookie || version != 5 {
        return;
    }
    if let Some(code) = b.reject {
        header(out, 0, 0, caller_id);
        words(
            out,
            &[
                5,
                0,
                isn,
                1500,
                b.flow_window,
                1000 + code,
                0,
                cookie,
                0,
                0,
                0,
                0,
            ],
        );
        socket.send_to(out, from).unwrap();
        return;
    }
    let mut hsreq = None;
    let mut stream_id = None;
    let mut rest = &cif[48..];
    while rest.len() >= 4 {
        let head = word(rest, 0);
        let len = (head & 0xFFFF) as usize * 4;
        let content = &rest[4..4 + len];
        match head >> 16 {
            1 => hsreq = Some([word(content, 0), word(content, 1), word(content, 2)]),
            5 => {
                let mut sid = Vec::new();
                for w in content.chunks(4) {
                    sid.extend(w.iter().rev());
                }
                while sid.last() == Some(&0) {
                    sid.pop();
                }
                stream_id = Some(String::from_utf8(sid).unwrap());
            }
            _ => {}
        }
        rest = &rest[4 + len..];
    }
    let hsreq = hsreq.expect("a conclusion carries HSREQ");
    let latency = b.latency_ms.max((hsreq[2] >> 16) as u16);
    let own_id = match conns.iter().find(|c| c.caller_id == caller_id) {
        Some(c) => c.own_id,
        None => {
            *next_id += 1;
            *ever += 1;
            conns.push(Conn {
                caller_id,
                own_id: *next_id,
                addr: from,
                session: Session {
                    stream_id,
                    hsreq: Some(hsreq),
                    isn,
                    ..Session::default()
                },
                data: BTreeMap::new(),
                lost_once: Vec::new(),
                seen: vec![false; RING as usize].into_boxed_slice(),
                next: 0,
                highest: None,
                ack_no: 0,
                acked: None,
                last_tick: Instant::now(),
                closed: false,
            });
            *next_id
        }
    };
    header(out, 0, 0, caller_id);
    words(
        out,
        &[
            5,
            1,
            isn,
            1500,
            b.flow_window,
            0xFFFF_FFFF,
            own_id,
            cookie,
            0,
            0,
            0,
            0,
        ],
    );
    words(
        out,
        &[
            (2 << 16) | 3,
            0x0001_0500,
            0x3F,
            (u32::from(latency) << 16) | u32::from(latency),
        ],
    );
    socket.send_to(out, from).unwrap();
}

fn data(socket: &UdpSocket, b: &Behaviour, c: &mut Conn, p: &[u8], out: &mut Vec<u8>) {
    let seq = word(p, 0) & MASK;
    let msg = word(p, 1);
    let offset = c.offset(seq);
    if let Some(n) = b.lose_every {
        if offset % n == n - 1 && !c.lost_once.contains(&offset) {
            c.lost_once.push(offset);
            return;
        }
    }
    c.session.data_packets += 1;
    if msg & (1 << 26) != 0 {
        c.session.retransmitted += 1;
    }
    let slot = (offset % RING) as usize;
    if offset < c.next || offset - c.next >= RING || c.seen[slot] {
        // A duplicate, or beyond what is tracked.
        return;
    }
    c.seen[slot] = true;
    c.session.received_bytes += (p.len() - 16) as u64;
    if c.highest.is_none_or(|h| offset > h) {
        c.highest = Some(offset);
    }
    if !b.sink {
        c.data.insert(offset, (msg, p[16..].to_vec()));
    }
    while c.seen[(c.next % RING) as usize] {
        c.seen[(c.next % RING) as usize] = false;
        c.next += 1;
    }
    if let Some(limit) = b.close_after {
        if c.session.data_packets >= limit && c.own_id == 1001 {
            header(out, 5, 0, c.caller_id);
            words(out, &[0]);
            socket.send_to(out, c.addr).unwrap();
            c.closed = true;
        }
    }
}

/// Every [`ACK_INTERVAL`]: a full ACK when something new arrived, and a loss report
/// for the gaps below the highest received packet.
fn tick(socket: &UdpSocket, b: &Behaviour, c: &mut Conn, out: &mut Vec<u8>) {
    let Some(highest) = c.highest else {
        return;
    };
    let expected = c.seq(c.next);
    if c.acked != Some(expected) {
        c.acked = Some(expected);
        c.ack_no += 1;
        header(out, 2, c.ack_no, c.caller_id);
        words(
            out,
            &[expected, 2000, 500, b.flow_window, 100, 1000, 1_000_000],
        );
        socket.send_to(out, c.addr).unwrap();
    }
    // Loss list: ranges of missing offsets between `next` and `highest`.
    header(out, 3, 0, c.caller_id);
    let mut gaps = 0;
    let mut offset = c.next;
    while offset < highest && gaps < 32 {
        if c.seen[(offset % RING) as usize] {
            offset += 1;
            continue;
        }
        let first = offset;
        while offset < highest && !c.seen[(offset % RING) as usize] {
            offset += 1;
        }
        let last = offset - 1;
        if first == last {
            words(out, &[c.seq(first)]);
        } else {
            words(out, &[c.seq(first) | 0x8000_0000, c.seq(last)]);
        }
        gaps += 1;
    }
    if gaps > 0 {
        socket.send_to(out, c.addr).unwrap();
    }
}
