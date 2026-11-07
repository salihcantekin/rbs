//! The I/O thread: connects as an SRT caller, sends queued chunks as live-mode data
//! packets, answers acknowledgements and loss reports, and reconnects by the
//! session's policy.
//!
//! One thread owns the UDP socket. Sends never wait for the peer (a UDP send only
//! waits for room in the local socket buffer), and every receive waits at most
//! [`SLICE`], so the thread sees `stop` and `abort` within one slice; no socket
//! timeouts on sends and no shutdown are needed. Between bursts of at most [`BURST`]
//! packets the thread reads what the peer sent without waiting (at most
//! [`POLL_READS`] datagrams), so a flooding peer cannot starve sending.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rbs_core::media::Bytes;
use rbs_transport::{ReconnectPolicy, TransportError};

use crate::packet::{self, Handshake, ctrl, hs};
use crate::url::SrtUrl;

/// Longest single blocking receive, so `stop` and `abort` are seen promptly. It is
/// also how long a newly queued chunk may wait before the idle thread takes it.
pub(crate) const SLICE: Duration = Duration::from_millis(5);
/// From the first induction to the listener's conclusion.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// Handshake packets are sent again this often until answered.
const HANDSHAKE_RESEND: Duration = Duration::from_millis(250);
/// A keep-alive is sent after this long without sending anything.
const KEEPALIVE: Duration = Duration::from_secs(1);
/// The connection is lost after this long without hearing from the peer (libsrt's
/// default peer idle timeout).
const PEER_IDLE: Duration = Duration::from_secs(5);
/// Payload bytes per data packet: seven 188-byte TS packets, libsrt's live-mode
/// default. A chunk's last packet may be shorter; packets never span two chunks.
pub(crate) const PAYLOAD: usize = 1316;
/// Announced maximum transmission unit (bytes, IP and UDP headers included).
const MTU: u32 = 1500;
/// Announced flow window (packets).
const FLOW_WINDOW: u32 = 8192;
/// Most packets sent and not yet acknowledged, whatever the peer announces.
const MAX_FLIGHT: u32 = 8192;
/// Packets sent between two looks at the socket.
const BURST: usize = 32;
/// Most datagrams read per look at the socket while busy.
const POLL_READS: usize = 16;
/// Most loss ranges remembered; later reports are ignored until there is room (the
/// receiver reports losses again).
const MAX_LOSS_RANGES: usize = 256;
/// A sent packet is kept for retransmission until acknowledged, or for the peer's
/// latency (at least [`DROP_FLOOR`]) plus [`DROP_MARGIN`] after its timestamp: after
/// that the receiver would drop it as too late (libsrt's sender drop threshold).
const DROP_FLOOR: Duration = Duration::from_secs(1);
const DROP_MARGIN: Duration = Duration::from_millis(20);
/// Round-trip time and its variance assumed before the first ACK (libsrt's).
const INITIAL_RTT: Duration = Duration::from_millis(100);
const INITIAL_RTT_VAR: Duration = Duration::from_millis(50);
/// The retransmission timeout is RTT + 4 x variance + this (two ACK intervals).
const RTO_MARGIN: Duration = Duration::from_millis(20);
/// Most doubling of the retransmission timeout while nothing is acknowledged.
const MAX_RTO_BACKOFF: u32 = 16;
/// Receive buffer: larger than any datagram a peer sends at our MTU.
const RECV_BUF: usize = 2048;

/// What a queued chunk is, for the I/O thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A `Header` chunk: kept and sent again first on every new connection.
    Header,
    /// A `Media` chunk marked `keyframe`: a connection may start here.
    Key,
    /// Any other `Media` chunk.
    Inter,
    /// A `Trailer` chunk.
    Trailer,
}

/// One chunk committed to the hand-off.
pub(crate) struct Item {
    pub data: Bytes,
    pub kind: Kind,
    /// When `send` took it: the source time of its packets (TSBPD).
    pub at: Instant,
    /// Media time in ns of a `Media` chunk (its timestamp); 0 for others.
    pub ts: u64,
}

/// Written by the I/O thread, read by the producer; on its own cache line.
#[derive(Default)]
#[repr(align(128))]
pub(crate) struct Progress {
    /// Datagram bytes sent (headers and retransmissions included).
    pub bytes_sent: AtomicU64,
    /// Items taken from the hand-off and finished with (sent or discarded).
    pub done_items: AtomicU64,
    /// Items taken from the hand-off and discarded (before a keyframe chunk after a
    /// (re)connect, cut by a lost connection, or when the session ended).
    pub discarded: AtomicU64,
    /// Successful handshakes (1 after the first connection).
    pub connects: AtomicU32,
    /// Reconnect attempts started since the session began.
    pub attempts: AtomicU32,
    /// Number of the current reconnect attempt (1 for the first after a loss).
    pub attempt: AtomicU32,
    /// The last round-trip time the peer reported, in microseconds; 0 when unknown.
    pub rtt_us: AtomicU32,
}

/// The media time in ns of the newest media `send` took. Written by the producer on every
/// `send`, read by the I/O thread only when a connection starts; on its own cache
/// line, so the stores do not contend with the flags the I/O thread polls.
#[derive(Default)]
#[repr(align(128))]
pub(crate) struct Newest(pub AtomicU64);

/// State shared with the I/O thread: atomics and set-once cells only (P6).
#[derive(Default)]
pub(crate) struct Shared {
    pub progress: Progress,
    /// Finish: send what is queued, wait for it to be acknowledged, close and exit.
    /// No new connection attempt starts after a failure once this is set.
    pub stop: AtomicBool,
    /// Give up now: discard what is queued and exit.
    pub abort: AtomicBool,
    /// `finish` was called: a connection that fails from now on is not tried again,
    /// and a wait before the next attempt ends at once (nobody receives the stream,
    /// so the tail is not worth the wait).
    pub ending: AtomicBool,
    /// Ask the encoder for a keyframe (set when a connection is lost).
    pub keyframe_wanted: AtomicBool,
    /// A connection is up and takes media: the producer moves staged items to the
    /// hand-off only while this is set, so nothing waits there for a connection.
    pub linked: AtomicBool,
    /// Written by the producer on every `send`; on its own cache line.
    pub newest: Newest,
    /// The error that ended the session for good.
    pub error: OnceLock<TransportError>,
    /// The I/O thread has ended: nothing will take items from the hand-off again.
    pub exited: AtomicBool,
}

impl Shared {
    /// Sets the abort flag. The I/O thread sees it within one receive slice; only a
    /// DNS lookup can keep it longer.
    pub(crate) fn abort(&self) {
        self.abort.store(true, Ordering::SeqCst);
    }
}

/// What the I/O thread needs. The stream id goes on the wire only; nothing the
/// thread reports contains it.
pub(crate) struct IoConfig {
    pub url: SrtUrl,
    pub stream_id: Option<Vec<u8>>,
    pub latency_ms: u16,
    pub reconnect: ReconnectPolicy,
    /// The queue's duration limit in ns: at the start of a connection, committed media
    /// chunks this much older than the newest queued one are discarded.
    pub stale_ns: u64,
}

#[derive(Debug)]
enum Fail {
    Aborted,
    Error(TransportError),
}

fn connection(what: &str, e: &std::io::Error) -> Fail {
    Fail::Error(TransportError::Connection(format!("{what}: {e}")))
}

fn lost(what: &str) -> Fail {
    Fail::Error(TransportError::Connection(what.into()))
}

/// A read that found nothing within its time. Windows sometimes reports a blocking
/// `recv` whose `SO_RCVTIMEO` expired as `ERROR_IO_PENDING` (997, "Overlapped I/O
/// operation is in progress") instead of `WSAETIMEDOUT`; no datagram was consumed,
/// so it is a timeout too.
fn is_timeout(e: &std::io::Error) -> bool {
    const IO_PENDING: i32 = 997;
    matches!(
        e.kind(),
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    ) || (cfg!(windows) && e.raw_os_error() == Some(IO_PENDING))
}

/// Errors a connected UDP socket reports for an ICMP message about an earlier
/// datagram. They say nothing certain about the peer now; silence is detected by
/// the handshake and idle timeouts instead.
fn is_icmp_echo(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset
    )
}

/// A send that found the local interface queue full: `WouldBlock`, or `ENOBUFS`
/// (BSD and macOS return it for UDP instead of blocking; Windows `WSAENOBUFS`).
fn is_buffer_full(e: &std::io::Error) -> bool {
    const ENOBUFS: i32 = if cfg!(windows) {
        10055
    } else if cfg!(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    )) {
        55
    } else {
        105
    };
    e.kind() == ErrorKind::WouldBlock || e.raw_os_error() == Some(ENOBUFS)
}

/// A receive of a datagram larger than the buffer: Windows reports `WSAEMSGSIZE`
/// (the datagram is dropped); elsewhere it is truncated silently.
fn is_oversized(e: &std::io::Error) -> bool {
    cfg!(windows) && e.raw_os_error() == Some(10040)
}

/// Sends one datagram. The socket blocks, but a UDP send waits only for room in the
/// local buffer, never for the peer.
fn send_datagram(socket: &UdpSocket, data: &[u8], shared: &Shared) -> Result<(), Fail> {
    loop {
        match socket.send(data) {
            Ok(n) => {
                shared
                    .progress
                    .bytes_sent
                    .fetch_add(n as u64, Ordering::Relaxed);
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            // The datagram is lost but kept for retransmission: a loss report
            // recovers it.
            Err(e) if is_icmp_echo(&e) || is_buffer_full(&e) => return Ok(()),
            Err(e) => return Err(connection("send", &e)),
        }
    }
}

/// A pseudo-random 32-bit value for socket ids and initial sequence numbers. Not
/// for security: SRT uses them to tell connections apart.
fn random_u32() -> u32 {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let calls = CALLS.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let mut x = time ^ calls.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ u64::from(std::process::id());
    x |= 1;
    for _ in 0..4 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    (x >> 16) as u32
}

/// One sent packet, kept until acknowledged or too old.
struct Sent {
    seq: u32,
    msg: u32,
    ts: u32,
    data: Bytes,
}

/// What is being cut into packets: a chunk from the hand-off, or a saved header sent
/// again on a new connection.
struct Current {
    data: Bytes,
    offset: usize,
    at: Instant,
    /// Taken from the hand-off: counts as done when its last packet is sent.
    from_handoff: bool,
    /// A `Header` chunk (saved for later connections).
    header: bool,
}

/// One connection to the listener.
struct Conn {
    socket: UdpSocket,
    /// Time zero of packet timestamps.
    start: Instant,
    own_id: u32,
    peer_id: u32,
    next_seq: u32,
    next_msg: u32,
    /// Everything before this sequence number was acknowledged.
    acked: u32,
    /// Timestamp of the last data packet, so timestamps never go back.
    last_ts: u32,
    /// Most packets in flight: the peer's flow window, capped.
    window: u32,
    /// Free packets in the receiver's buffer, from its last ACK.
    available: u32,
    /// How long a sent packet is kept for retransmission.
    keep_for: Duration,
    /// The listener's latency: it delivers a packet this long after its timestamp.
    latency: Duration,
    last_sent: Instant,
    last_heard: Instant,
    /// The peer's last reported round-trip time and variance.
    rtt: Duration,
    rtt_var: Duration,
    /// When unacknowledged packets are sent again if no ACK advanced meanwhile:
    /// a receiver cannot report the loss of the last packets it was sent.
    rexmit_due: Instant,
    rexmit_backoff: u32,
    flight: VecDeque<Sent>,
    loss: VecDeque<(u32, u32)>,
    buf: Box<[u8]>,
    out: Vec<u8>,
}

impl Conn {
    fn now_ts(&self, now: Instant) -> u32 {
        // Microseconds since the connection started, wrapping as SRT defines.
        now.saturating_duration_since(self.start).as_micros() as u32
    }

    fn send_out(&mut self, shared: &Shared) -> Result<(), Fail> {
        send_datagram(&self.socket, &self.out, shared)?;
        self.last_sent = Instant::now();
        Ok(())
    }

    /// Sends the shutdown, the connection's last packet; the socket is closed next.
    /// On Windows a send returns once the datagram is in the socket's send buffer,
    /// and closing the socket right after can discard it: under load the listener
    /// then never got the shutdown. Without a send buffer the send returns only
    /// once the datagram has left (and every earlier one before it), so closing
    /// cannot lose it.
    fn send_shutdown(&mut self, shared: &Shared) -> Result<(), Fail> {
        #[cfg(windows)]
        {
            // Best effort: if the option is refused, the send is as before.
            let _ = socket2::SockRef::from(&self.socket).set_send_buffer_size(0);
        }
        self.send_control(ctrl::SHUTDOWN, 0, shared)
    }

    fn send_control(&mut self, kind: u16, info: u32, shared: &Shared) -> Result<(), Fail> {
        let ts = self.now_ts(Instant::now());
        self.out.clear();
        packet::write_short_control(&mut self.out, kind, info, ts, self.peer_id);
        self.send_out(shared)
    }

    /// Sends the packets the peer reported lost (at most [`BURST`]), with the
    /// retransmission flag; for packets no longer kept, a DROPREQ so the receiver
    /// stops asking.
    fn retransmit(&mut self, shared: &Shared) -> Result<(), Fail> {
        let mut budget = BURST;
        while budget > 0 {
            let Some(&(first, last)) = self.loss.front() else {
                return Ok(());
            };
            let front = self.flight.front().map_or(self.next_seq, |s| s.seq);
            if packet::seq_diff(last, first) < 0 || packet::seq_diff(first, self.next_seq) >= 0 {
                // Malformed, or not sent yet.
                self.loss.pop_front();
                continue;
            }
            if packet::seq_diff(first, self.acked) < 0 {
                // Acknowledged since it was reported.
                self.advance_loss(self.acked);
                continue;
            }
            if packet::seq_diff(first, front) < 0 {
                // Dropped as too late: tell the receiver.
                let end = if packet::seq_diff(last, front) < 0 {
                    last
                } else {
                    packet::seq_add(front, packet::SEQ_MASK)
                };
                let ts = self.now_ts(Instant::now());
                self.out.clear();
                packet::write_dropreq(&mut self.out, first, end, ts, self.peer_id);
                self.send_out(shared)?;
                self.advance_loss(front);
                budget -= 1;
                continue;
            }
            let index = packet::seq_diff(first, front) as usize;
            if let Some(sent) = self.flight.get(index) {
                self.out.clear();
                packet::write_data(
                    &mut self.out,
                    sent.seq,
                    sent.msg,
                    true,
                    sent.ts,
                    self.peer_id,
                    &sent.data,
                );
                self.send_out(shared)?;
            }
            self.advance_loss(packet::seq_add(first, 1));
            budget -= 1;
        }
        Ok(())
    }

    /// Removes sequence numbers before `seq` from the front loss range.
    fn advance_loss(&mut self, seq: u32) {
        if let Some(range) = self.loss.front_mut() {
            if packet::seq_diff(range.1, seq) < 0 {
                self.loss.pop_front();
            } else if packet::seq_diff(range.0, seq) < 0 {
                range.0 = seq;
            }
        }
    }

    /// Forgets sent packets older than the drop threshold (the receiver would
    /// discard them as too late anyway).
    fn expire(&mut self, now: Instant) {
        let now_ts = self.now_ts(now);
        let keep_us = self.keep_for.as_micros() as u32;
        while let Some(front) = self.flight.front() {
            if now_ts.wrapping_sub(front.ts) <= keep_us {
                break;
            }
            self.flight.pop_front();
        }
    }

    /// The timestamp of a data packet whose chunk was taken at `at`: never before
    /// the previous one, compared in wrapping order (timestamps wrap every 71.6
    /// minutes, and the next one is "later" across the wrap).
    fn data_ts(&self, at: Instant) -> u32 {
        let ts = self.now_ts(at);
        if (ts.wrapping_sub(self.last_ts) as i32) < 0 {
            self.last_ts
        } else {
            ts
        }
    }

    /// Whether the listener has delivered the last data packet: its latency (plus a
    /// margin) has passed since that packet's timestamp.
    fn delivered(&self, now: Instant) -> bool {
        let since = self.now_ts(now).wrapping_sub(self.last_ts);
        Duration::from_micros(u64::from(since)) >= self.latency + DROP_MARGIN
    }

    /// The retransmission timeout.
    fn rto(&self) -> Duration {
        self.rtt + 4 * self.rtt_var + RTO_MARGIN
    }

    /// When no ACK advanced for a retransmission timeout and no loss is reported,
    /// queues every unacknowledged packet for retransmission (libsrt's "late
    /// retransmission" in live mode): the receiver cannot see the loss of the last
    /// packets of a burst. The timeout doubles (up to 16 times) until an ACK
    /// advances, so a stalled receiver is not flooded.
    fn rexmit_timer(&mut self, now: Instant) {
        if now < self.rexmit_due || !self.loss.is_empty() {
            return;
        }
        if let (Some(first), Some(last)) = (self.flight.front(), self.flight.back()) {
            self.loss.push_back((first.seq, last.seq));
        }
        self.rexmit_backoff = (self.rexmit_backoff * 2).min(MAX_RTO_BACKOFF);
        self.rexmit_due = now + self.rto() * self.rexmit_backoff;
    }

    /// Room for another new packet.
    fn window_open(&self) -> bool {
        (self.flight.len() as u32) < self.window.min(self.available).max(1)
    }

    /// Reads one datagram, waiting at most [`SLICE`] when `wait`; `false` when none
    /// arrived.
    fn read(&mut self, shared: &Shared) -> Result<bool, Fail> {
        match self.socket.recv(&mut self.buf) {
            Ok(n) => {
                self.handle(n, shared)?;
                Ok(true)
            }
            Err(e) if is_timeout(&e) || is_icmp_echo(&e) || is_oversized(&e) => Ok(false),
            Err(e) => Err(connection("receive", &e)),
        }
    }

    /// Reads what arrived without waiting, at most [`POLL_READS`] datagrams.
    fn poll(&mut self, shared: &Shared) -> Result<(), Fail> {
        self.socket
            .set_nonblocking(true)
            .map_err(|e| connection("socket", &e))?;
        let mut result = Ok(());
        for _ in 0..POLL_READS {
            match self.read(shared) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.socket
            .set_nonblocking(false)
            .map_err(|e| connection("socket", &e))?;
        result
    }

    /// Handles a received datagram of `n` bytes in `buf`.
    fn handle(&mut self, n: usize, shared: &Shared) -> Result<(), Fail> {
        let Some(control) = packet::parse_control(&self.buf[..n]) else {
            // A sender ignores data packets.
            return Ok(());
        };
        if control.dest != self.own_id {
            return Ok(());
        }
        self.last_heard = Instant::now();
        match control.kind {
            ctrl::ACK => {
                let Some(ack) = packet::parse_ack(control.cif) else {
                    return Ok(());
                };
                let info = control.info;
                if packet::seq_diff(ack.seq, self.next_seq) > 0 {
                    // Acknowledges what was never sent: ignore it.
                    return Ok(());
                }
                if let Some(rtt) = ack.rtt_us.filter(|r| *r > 0) {
                    self.rtt = Duration::from_micros(u64::from(rtt));
                    self.rtt_var =
                        Duration::from_micros(u64::from(ack.rtt_var_us.unwrap_or_default()));
                }
                if packet::seq_diff(ack.seq, self.acked) > 0 {
                    self.acked = ack.seq;
                    self.rexmit_backoff = 1;
                    self.rexmit_due = Instant::now() + self.rto();
                }
                while self
                    .flight
                    .front()
                    .is_some_and(|s| packet::seq_diff(s.seq, ack.seq) < 0)
                {
                    self.flight.pop_front();
                }
                if let Some(rtt) = ack.rtt_us.filter(|r| *r > 0) {
                    shared.progress.rtt_us.store(rtt, Ordering::Relaxed);
                }
                if let Some(available) = ack.available {
                    self.available = available;
                }
                if !ack.light {
                    self.send_control(ctrl::ACKACK, info, shared)?;
                }
            }
            ctrl::NAK => {
                for range in packet::nak_ranges(control.cif) {
                    if self.loss.len() >= MAX_LOSS_RANGES {
                        break;
                    }
                    self.loss.push_back(range);
                }
            }
            ctrl::SHUTDOWN => return Err(lost("the peer closed the connection")),
            // Keep-alives, repeated handshake answers and anything else need nothing.
            _ => {}
        }
        Ok(())
    }
}

/// The I/O thread's state.
pub(crate) struct Io {
    config: IoConfig,
    shared: Arc<Shared>,
    consumer: rtrb::Consumer<Item>,
    /// `Header` chunks received so far, sent again first on every new connection.
    headers: Vec<Bytes>,
    current: Option<Current>,
    /// A connection attempt or connection has failed in this session.
    failed_before: bool,
    /// No media chunk was sent on this connection yet.
    fresh_start: bool,
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
            headers: Vec::new(),
            current: None,
            failed_before: false,
            fresh_start: true,
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
            let mut connected = false;
            let result = self.connect().and_then(|mut conn| {
                connected = true;
                let result = self.stream(&mut conn);
                drop(conn);
                result
            });
            self.drop_current();
            if connected {
                attempt = 0;
                delay = initial;
            }
            let error = match result {
                Ok(()) | Err(Fail::Aborted) => break,
                Err(Fail::Error(_)) if self.shared.abort.load(Ordering::Acquire) => break,
                Err(Fail::Error(e)) => e,
            };
            self.failed_before = true;
            let retry = error.is_recoverable() && attempts.is_none_or(|n| attempt < n);
            if !retry || self.finishing() || self.shared.ending.load(Ordering::Acquire) {
                // Also while finishing with nothing queued: no reason to reconnect,
                // but what was in flight may be lost, so `finish` reports the error.
                let _ = self.shared.error.set(error);
                break;
            }
            attempt += 1;
            let progress = &self.shared.progress;
            progress.attempt.store(attempt, Ordering::Relaxed);
            progress.attempts.fetch_add(1, Ordering::Release);
            self.shared.keyframe_wanted.store(true, Ordering::Release);
            if !self.back_off(delay) {
                if !self.shared.abort.load(Ordering::Acquire) {
                    // Ended while waiting to try again: what was queued is lost.
                    let _ = self.shared.error.set(error);
                }
                break;
            }
            delay = delay.saturating_mul(2).min(max.max(initial));
        }
        self.discard_all();
        self.shared.exited.store(true, Ordering::Release);
        let _ = done.send(());
    }

    /// `finish` was called and nothing is left to send.
    fn finishing(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire) && self.consumer.is_empty()
    }

    /// Waits `delay` before the next attempt; `false` if the session is aborted, or
    /// finishing with nothing left to deliver, meanwhile.
    fn back_off(&self, delay: Duration) -> bool {
        let now = Instant::now();
        let until = now
            .checked_add(delay)
            .unwrap_or(now + Duration::from_secs(86_400));
        loop {
            if self.shared.abort.load(Ordering::Acquire)
                || self.shared.ending.load(Ordering::Acquire)
                || self.finishing()
            {
                return false;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            thread::park_timeout(left);
        }
    }

    fn finish_item(&self, discarded: bool) {
        let progress = &self.shared.progress;
        if discarded {
            progress.discarded.fetch_add(1, Ordering::Relaxed);
        }
        progress.done_items.fetch_add(1, Ordering::Release);
    }

    /// A chunk cut by a lost connection: its delivery is incomplete, count it lost.
    /// A header is not lost: it was saved and goes first on the next connection.
    fn drop_current(&mut self) {
        if let Some(current) = self.current.take() {
            if current.from_handoff {
                self.finish_item(!current.header);
            }
        }
    }

    fn discard_all(&mut self) {
        while let Ok(item) = self.consumer.pop() {
            drop(item);
            self.finish_item(true);
        }
    }

    /// Resolves the listener, binds a socket and runs the caller handshake
    /// (induction, then conclusion with the HSREQ and stream id extensions).
    fn connect(&mut self) -> Result<Conn, Fail> {
        let url = &self.config.url;
        let addr = (url.host.as_str(), url.port)
            .to_socket_addrs()
            .map_err(|e| connection("resolve", &e))?
            .next()
            .ok_or_else(|| lost("the host has no address"))?;
        if self.shared.abort.load(Ordering::Acquire) {
            return Err(Fail::Aborted);
        }
        let local: SocketAddr = match addr {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let setup = |e: std::io::Error| connection("socket", &e);
        let socket = UdpSocket::bind(local).map_err(setup)?;
        socket.connect(addr).map_err(setup)?;
        socket.set_read_timeout(Some(SLICE)).map_err(setup)?;
        let now = Instant::now();
        let isn = random_u32() & packet::SEQ_MASK;
        let mut conn = Conn {
            socket,
            start: now,
            // Below 2^30: libsrt reserves the next bit for group ids.
            own_id: (random_u32() & 0x3FFF_FFFF).max(1),
            peer_id: 0,
            next_seq: isn,
            next_msg: 1,
            acked: isn,
            last_ts: 0,
            window: 0,
            available: 0,
            keep_for: DROP_FLOOR,
            latency: Duration::ZERO,
            last_sent: now,
            last_heard: now,
            rtt: INITIAL_RTT,
            rtt_var: INITIAL_RTT_VAR,
            rexmit_due: now,
            rexmit_backoff: 1,
            flight: VecDeque::new(),
            loss: VecDeque::with_capacity(MAX_LOSS_RANGES),
            buf: vec![0; RECV_BUF].into_boxed_slice(),
            out: Vec::with_capacity(RECV_BUF),
        };
        let deadline = now + HANDSHAKE_TIMEOUT;
        let mut hs = Handshake {
            version: 4,
            encryption: 0,
            extension: hs::UDT_DGRAM,
            isn,
            mtu: MTU,
            flow_window: FLOW_WINDOW,
            kind: hs::INDUCTION,
            socket_id: conn.own_id,
            cookie: 0,
            peer_ip: packet::peer_ip(addr.ip()),
        };
        let (answer, _) = self.exchange(&mut conn, &hs, deadline, |h| {
            h.kind == hs::INDUCTION || packet::is_rejection(h.kind)
        })?;
        if answer.kind != hs::INDUCTION {
            return Err(Fail::Error(TransportError::Rejected(packet::rejection(
                answer.kind,
            ))));
        }
        if answer.version < 5 || answer.extension != hs::SRT_MAGIC {
            return Err(Fail::Error(TransportError::Rejected(
                "the listener does not speak SRT handshake version 5".into(),
            )));
        }
        hs.version = 5;
        hs.kind = hs::CONCLUSION;
        hs.cookie = answer.cookie;
        hs.extension = hs::EXT_HSREQ;
        if self.config.stream_id.is_some() {
            hs.extension |= hs::EXT_CONFIG;
        }
        let (answer, delay) = self.exchange(&mut conn, &hs, deadline, |h| {
            h.kind == hs::CONCLUSION || packet::is_rejection(h.kind)
        })?;
        if answer.kind != hs::CONCLUSION {
            return Err(Fail::Error(TransportError::Rejected(packet::rejection(
                answer.kind,
            ))));
        }
        let latency = delay.unwrap_or(self.config.latency_ms);
        conn.peer_id = answer.socket_id;
        conn.window = answer.flow_window.clamp(1, MAX_FLIGHT);
        conn.available = conn.window;
        conn.latency = Duration::from_millis(u64::from(latency));
        conn.keep_for = conn.latency.max(DROP_FLOOR) + DROP_MARGIN;
        conn.flight = VecDeque::with_capacity(conn.window as usize);
        conn.last_heard = Instant::now();
        Ok(conn)
    }

    /// Sends handshake `hs` (with our extensions when it is a conclusion) until the
    /// listener answers with a handshake that `accept` takes, or `deadline`. Returns
    /// the answer and the receiver delay of its HSRSP extension.
    fn exchange(
        &self,
        conn: &mut Conn,
        hs: &Handshake,
        deadline: Instant,
        accept: impl Fn(&Handshake) -> bool,
    ) -> Result<(Handshake, Option<u16>), Fail> {
        let shared = &*self.shared;
        let mut resend_at = Instant::now();
        loop {
            if shared.abort.load(Ordering::Acquire) {
                return Err(Fail::Aborted);
            }
            if self.finishing() {
                // Nothing queued any more: no reason to connect. If an earlier
                // connection failed, what it had in flight may be lost, so `finish`
                // reports that; otherwise nothing was ever queued and all is well.
                return Err(if self.failed_before {
                    lost("finished before the connection was made again")
                } else {
                    Fail::Aborted
                });
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(lost("no handshake answer from the SRT listener"));
            }
            if now >= resend_at {
                resend_at = now + HANDSHAKE_RESEND;
                conn.out.clear();
                let ts = conn.now_ts(now);
                packet::write_control(&mut conn.out, ctrl::HANDSHAKE, 0, ts, 0);
                hs.write(&mut conn.out);
                if hs.kind == hs::CONCLUSION {
                    packet::write_hsreq(&mut conn.out, self.config.latency_ms);
                    if let Some(sid) = &self.config.stream_id {
                        packet::write_stream_id(&mut conn.out, sid);
                    }
                }
                conn.send_out(shared)?;
            }
            let n = match conn.socket.recv(&mut conn.buf) {
                Ok(n) => n,
                Err(e) if is_timeout(&e) || is_icmp_echo(&e) || is_oversized(&e) => continue,
                Err(e) => return Err(connection("handshake", &e)),
            };
            let Some(control) = packet::parse_control(&conn.buf[..n]) else {
                continue;
            };
            if control.kind != ctrl::HANDSHAKE || (control.dest != conn.own_id && control.dest != 0)
            {
                continue;
            }
            let Some(answer) = Handshake::parse(control.cif) else {
                continue;
            };
            if !accept(&answer) {
                continue;
            }
            let delay = packet::extensions(control.cif)
                .find(|(kind, _)| *kind == hs::CMD_HSRSP)
                .and_then(|(_, content)| packet::hsrsp_receiver_delay(content));
            return Ok((answer, delay));
        }
    }

    /// Takes the next thing to send: a saved header (on a new connection), else the
    /// next chunk of the hand-off. `false` when there is nothing.
    fn next_source(&mut self, headers_sent: &mut usize, waiting_for_key: &mut bool) -> bool {
        while self.current.is_none() {
            if let Some(header) = self.headers.get(*headers_sent) {
                *headers_sent += 1;
                if !header.is_empty() {
                    self.current = Some(Current {
                        data: header.clone(),
                        offset: 0,
                        at: Instant::now(),
                        from_handoff: false,
                        header: true,
                    });
                }
                continue;
            }
            let Ok(item) = self.consumer.pop() else {
                return false;
            };
            match item.kind {
                Kind::Header => {
                    // Kept for later connections; this one sends it now.
                    self.headers.push(item.data.clone());
                    *headers_sent = self.headers.len();
                }
                // Committed while no connection took it: older than the queue
                // limit allows by now (the chunk of a keyframe included).
                Kind::Key | Kind::Inter
                    if self.fresh_start
                        && self
                            .shared
                            .newest
                            .0
                            .load(Ordering::Relaxed)
                            .saturating_sub(item.ts)
                            > self.config.stale_ns =>
                {
                    self.finish_item(true);
                    continue;
                }
                Kind::Inter if *waiting_for_key => {
                    self.finish_item(true);
                    continue;
                }
                Kind::Key => {
                    *waiting_for_key = false;
                    self.fresh_start = false;
                }
                Kind::Inter | Kind::Trailer => {}
            }
            if item.data.is_empty() {
                self.finish_item(false);
                continue;
            }
            self.current = Some(Current {
                header: item.kind == Kind::Header,
                data: item.data,
                offset: 0,
                at: item.at,
                from_handoff: true,
            });
        }
        true
    }

    /// Sends new packets while the window is open, at most [`BURST`].
    fn send_new(
        &mut self,
        conn: &mut Conn,
        headers_sent: &mut usize,
        waiting_for_key: &mut bool,
    ) -> Result<(), Fail> {
        let shared = Arc::clone(&self.shared);
        for _ in 0..BURST {
            if !conn.window_open() || !self.next_source(headers_sent, waiting_for_key) {
                break;
            }
            let Some(current) = self.current.as_mut() else {
                break;
            };
            let end = (current.offset + PAYLOAD).min(current.data.len());
            let payload = current.data.slice(current.offset..end);
            current.offset = end;
            let ts = conn.data_ts(current.at);
            let complete = end == current.data.len();
            let from_handoff = current.from_handoff;
            conn.last_ts = ts;
            let (seq, msg) = (conn.next_seq, conn.next_msg);
            conn.out.clear();
            packet::write_data(&mut conn.out, seq, msg, false, ts, conn.peer_id, &payload);
            conn.next_seq = packet::seq_add(seq, 1);
            conn.next_msg = packet::msg_next(msg);
            if conn.flight.is_empty() {
                conn.rexmit_backoff = 1;
                conn.rexmit_due = Instant::now() + conn.rto();
            }
            conn.flight.push_back(Sent {
                seq,
                msg,
                ts,
                data: payload,
            });
            if complete {
                self.current = None;
                if from_handoff {
                    self.finish_item(false);
                }
            }
            conn.send_out(&shared)?;
        }
        Ok(())
    }

    /// Sends queued chunks until the session stops or the connection fails. The link
    /// is set before the connection is counted, so a producer that sees `Connected`
    /// also sees the link.
    fn stream(&mut self, conn: &mut Conn) -> Result<(), Fail> {
        self.shared.linked.store(true, Ordering::Release);
        let result = self.stream_linked(conn);
        self.shared.linked.store(false, Ordering::Release);
        result
    }

    fn stream_linked(&mut self, conn: &mut Conn) -> Result<(), Fail> {
        let shared = Arc::clone(&self.shared);
        shared.progress.connects.fetch_add(1, Ordering::Release);
        // Each connection sends the saved headers first and starts at a keyframe, with
        // nothing older than the queue's duration limit.
        self.fresh_start = true;
        let mut headers_sent = 0;
        let mut waiting_for_key = true;
        loop {
            if shared.abort.load(Ordering::Acquire) {
                // Best effort, so the listener does not wait for its idle timeout.
                let _ = conn.send_shutdown(&shared);
                return Err(Fail::Aborted);
            }
            conn.retransmit(&shared)?;
            self.send_new(conn, &mut headers_sent, &mut waiting_for_key)?;
            let now = Instant::now();
            conn.expire(now);
            conn.rexmit_timer(now);
            if now.saturating_duration_since(conn.last_sent) >= KEEPALIVE {
                conn.send_control(ctrl::KEEPALIVE, 0, &shared)?;
            }
            if now.saturating_duration_since(conn.last_heard) >= PEER_IDLE {
                return Err(lost("the peer stopped answering"));
            }
            // `stop` first: `finish` pushes the last chunks before it sets `stop`, so
            // an empty hand-off seen after `stop` is really empty.
            let stopping = shared.stop.load(Ordering::Acquire);
            let idle = self.current.is_none()
                && headers_sent == self.headers.len()
                && self.consumer.is_empty();
            if stopping && idle && conn.flight.is_empty() && conn.delivered(now) {
                // Everything was acknowledged (or expired) and the listener has had
                // the time to deliver it: close. libsrt discards what it holds back
                // for its latency once the peer closes.
                return conn.send_shutdown(&shared);
            }
            let busy = !conn.loss.is_empty() || (!idle && conn.window_open());
            if busy {
                conn.poll(&shared)?;
            } else {
                conn.read(&shared)?;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn timeouts_include_windows_pending_reads() {
        assert!(is_timeout(&std::io::Error::from(ErrorKind::WouldBlock)));
        assert!(is_timeout(&std::io::Error::from(ErrorKind::TimedOut)));
        assert_eq!(
            is_timeout(&std::io::Error::from_raw_os_error(997)),
            cfg!(windows)
        );
        assert!(!is_timeout(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    fn conn() -> Conn {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let now = Instant::now();
        Conn {
            socket,
            start: now,
            own_id: 7,
            peer_id: 9,
            next_seq: packet::SEQ_MASK - 1,
            next_msg: 1,
            acked: packet::SEQ_MASK - 1,
            last_ts: 0,
            window: 8,
            available: 8,
            keep_for: DROP_FLOOR,
            latency: Duration::ZERO,
            last_sent: now,
            last_heard: now,
            rtt: INITIAL_RTT,
            rtt_var: INITIAL_RTT_VAR,
            rexmit_due: now,
            rexmit_backoff: 1,
            flight: VecDeque::new(),
            loss: VecDeque::new(),
            buf: vec![0; RECV_BUF].into_boxed_slice(),
            out: Vec::new(),
        }
    }

    fn sent(conn: &mut Conn, count: u32) {
        for _ in 0..count {
            conn.flight.push_back(Sent {
                seq: conn.next_seq,
                msg: conn.next_msg,
                ts: 0,
                data: Bytes::from_static(b"x"),
            });
            conn.next_seq = packet::seq_add(conn.next_seq, 1);
            conn.next_msg = packet::msg_next(conn.next_msg);
        }
    }

    #[test]
    fn acks_release_packets_across_the_sequence_wrap() {
        let mut c = conn();
        sent(&mut c, 5);
        let mut ack = Vec::new();
        packet::write_control(&mut ack, ctrl::ACK, 1, 0, 7);
        ack.extend_from_slice(&1_u32.to_be_bytes());
        c.buf[..ack.len()].copy_from_slice(&ack);
        let shared = Shared::default();
        c.handle(ack.len(), &shared).unwrap();
        assert_eq!(c.flight.len(), 2, "seqs MAX-1, MAX, 0 acknowledged");
        assert_eq!(c.flight.front().unwrap().seq, 1);
        // An ACK for packets never sent is ignored.
        let mut ack = Vec::new();
        packet::write_control(&mut ack, ctrl::ACK, 2, 0, 7);
        ack.extend_from_slice(&100_u32.to_be_bytes());
        c.buf[..ack.len()].copy_from_slice(&ack);
        c.handle(ack.len(), &shared).unwrap();
        assert_eq!(c.flight.len(), 2);
    }

    #[test]
    fn timestamps_keep_advancing_across_their_wrap() {
        let mut c = conn();
        // 2^32 microseconds (71.6 minutes) plus a little after the start.
        let Some(start) = Instant::now().checked_sub(Duration::from_micros((1 << 32) + 500)) else {
            return; // The monotonic clock is younger than that (a fresh boot).
        };
        c.start = start;
        c.last_ts = u32::MAX - 1000;
        let ts = c.data_ts(Instant::now());
        assert!(ts < 1_000_000, "wrapped forward to {ts}, not frozen");
        // A packet stamped just before the wrap is still kept for retransmission.
        c.flight.push_back(Sent {
            seq: 1,
            msg: 1,
            ts: u32::MAX - 1000,
            data: Bytes::from_static(b"x"),
        });
        c.expire(Instant::now());
        assert_eq!(c.flight.len(), 1);
    }

    #[test]
    fn loss_reports_resend_kept_packets_and_drop_the_rest() {
        let mut c = conn();
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        c.socket.connect(peer.local_addr().unwrap()).unwrap();
        sent(&mut c, 6);
        // The first two are gone (too late), not acknowledged.
        c.flight.pop_front();
        c.flight.pop_front();
        let first = packet::SEQ_MASK - 1;
        c.loss.push_back((first, packet::seq_add(first, 3)));
        let shared = Shared::default();
        c.retransmit(&shared).unwrap();
        assert!(c.loss.is_empty());
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut buf = [0_u8; 64];
        let n = peer.recv(&mut buf).unwrap();
        let drop = packet::parse_control(&buf[..n]).unwrap();
        assert_eq!(drop.kind, ctrl::DROPREQ);
        assert_eq!(drop.cif[..4], first.to_be_bytes());
        assert_eq!(drop.cif[4..8], packet::SEQ_MASK.to_be_bytes());
        for seq in [0_u32, 1] {
            let n = peer.recv(&mut buf).unwrap();
            assert!(packet::parse_control(&buf[..n]).is_none());
            assert_eq!(buf[..4], seq.to_be_bytes());
            assert_eq!(buf[4] & 0x04, 0x04, "retransmission flag");
        }
    }
}
