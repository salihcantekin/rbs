//! One accepted connection: a reader thread that pushes whole messages into the
//! transport inbox, and a writer thread that drains the connection's bounded output
//! queue. Both block in the OS (a read, a write, or a condition variable) until there
//! is something to do.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use rbs_api::transport::{ConnectionHandle, ConnectionSender, SendError};
use rbs_api::{CloseReason, PeerInfo, TransportInbox};

use crate::frame::{self, Header, PREAMBLE};
use crate::{close_code, close_text};

/// A connected byte stream, shared by its reader and writer threads.
pub(crate) trait Stream: Send + Sync + 'static {
    /// Reads at most `buf.len()` bytes; 0 at the end of the stream. Called by the
    /// reader thread only.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    /// Writes all of `buf`. Called by one thread at a time (the writer, or the accept
    /// thread before the writer exists).
    fn write_all(&self, buf: &[u8]) -> io::Result<()>;
    /// Ends the connection both ways; a read or write blocked on another thread
    /// returns. Data already written still reaches the peer.
    fn shutdown(&self);
    /// Both threads have ended: release the OS handle now (the peer sees the end of
    /// the stream), not when the connection's bookkeeping is dropped.
    fn finish(&self) {}
}

/// How long `stop` lets the writers send their close frames before cutting the
/// connections: a peer that does not read must not hold the transport's stop.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(1);

/// A write that makes no progress for this long ends the connection: a peer that does
/// not read cannot keep its connection (and its slot) forever, also after the server
/// closed it.
pub(crate) const WRITE_STALL: Duration = Duration::from_secs(10);

struct Out {
    queue: VecDeque<Arc<str>>,
    bytes: usize,
    /// `try_send` answered `Full`; `writable` is reported once half the queue drained.
    full: bool,
    /// Close with this reason once the queue is written.
    close: Option<CloseReason>,
    /// The connection is over: nothing more is read or written.
    done: bool,
    /// The writer thread has ended.
    writer_ended: bool,
}

pub(crate) struct Shared {
    stream: Box<dyn Stream>,
    out: Mutex<Out>,
    changed: Condvar,
    reader: OnceLock<Thread>,
    max_queued: usize,
    /// Threads still using the stream; the last one to end calls `finish`.
    threads: AtomicUsize,
}

impl Shared {
    fn thread_ended(&self) {
        if self.threads.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.stream.finish();
        }
    }

    fn out(&self) -> MutexGuard<'_, Out> {
        self.out.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wake_reader(&self) {
        if let Some(t) = self.reader.get() {
            t.unpark();
        }
    }

    /// The connection is over: wakes both threads and unblocks their I/O.
    fn end(&self) {
        self.out().done = true;
        self.changed.notify_all();
        self.wake_reader();
        self.stream.shutdown();
    }

    /// The transport stops: what is queued is dropped, the close frame is sent.
    pub(crate) fn stop(&self, reason: CloseReason) {
        {
            let mut o = self.out();
            o.queue.clear();
            o.bytes = 0;
            if o.close.is_none() {
                o.close = Some(reason);
            }
        }
        self.changed.notify_all();
        self.wake_reader();
    }
}

struct Sender(Arc<Shared>);

impl ConnectionSender for Sender {
    fn try_send(&mut self, text: &Arc<str>) -> Result<(), SendError> {
        let mut o = self.0.out();
        if o.done || o.close.is_some() {
            return Err(SendError::Closed);
        }
        if !o.queue.is_empty() && o.bytes + text.len() > self.0.max_queued {
            o.full = true;
            return Err(SendError::Full);
        }
        o.bytes += text.len();
        o.queue.push_back(text.clone());
        drop(o);
        self.0.changed.notify_all();
        Ok(())
    }

    fn close(&mut self, reason: CloseReason) {
        {
            let mut o = self.0.out();
            if o.done || o.close.is_some() {
                return;
            }
            o.close = Some(reason);
        }
        self.0.changed.notify_all();
    }
}

/// A connection's threads, for `stop` and for pruning finished ones. It does not keep
/// the connection alive: once its threads ended and the API dropped its sender, the
/// stream is closed.
pub(crate) struct Live {
    shared: Weak<Shared>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Live {
    pub(crate) fn finished(&self) -> bool {
        [&self.reader, &self.writer]
            .into_iter()
            .flatten()
            .all(JoinHandle::is_finished)
    }

    /// The transport stops: queued output is dropped and the close frame sent.
    pub(crate) fn stop(&self, reason: CloseReason) {
        if let Some(s) = self.shared.upgrade() {
            s.stop(reason);
        }
    }

    /// Waits until `deadline` for the writer to send its close frame, then ends the
    /// connection and joins both threads.
    pub(crate) fn join(mut self, deadline: Instant) {
        if let Some(s) = self.shared.upgrade() {
            {
                let o = s.out();
                let left = deadline.saturating_duration_since(Instant::now());
                let _ = s.changed.wait_timeout_while(o, left, |o| !o.writer_ended);
            }
            s.end();
        }
        for t in [self.reader.take(), self.writer.take()]
            .into_iter()
            .flatten()
        {
            let _ = t.join();
        }
    }
}

/// Registers an accepted, same-user stream with the inbox and starts its threads.
/// `None` when it was refused (it got a close frame) or is already gone.
pub(crate) fn open(
    stream: Box<dyn Stream>,
    inbox: &TransportInbox,
    max_queued: usize,
) -> Option<Live> {
    let shared = Arc::new(Shared {
        stream,
        out: Mutex::new(Out {
            queue: VecDeque::new(),
            bytes: 0,
            full: false,
            close: None,
            done: false,
            writer_ended: false,
        }),
        changed: Condvar::new(),
        reader: OnceLock::new(),
        max_queued,
        threads: AtomicUsize::new(2),
    });
    let resume = {
        let s = Arc::downgrade(&shared);
        Box::new(move || {
            if let Some(s) = s.upgrade() {
                s.wake_reader();
            }
        })
    };
    let handle = match inbox.connect(
        PeerInfo::same_user_ipc(),
        Box::new(Sender(shared.clone())),
        resume,
    ) {
        Ok(h) => h,
        Err(_) => {
            let reason = CloseReason::TooManyConnections;
            let _ = shared
                .stream
                .write_all(&frame::close(close_code(reason), close_text(reason)));
            shared.stream.shutdown();
            shared.stream.finish();
            return None;
        }
    };
    // Before the writer exists, so the preamble comes first.
    if shared.stream.write_all(&PREAMBLE).is_err() {
        shared.end();
        shared.stream.finish();
        return None;
    }
    let handle = Arc::new(handle);
    let reader = {
        let (s, h) = (shared.clone(), handle.clone());
        std::thread::Builder::new()
            .name("rbs-api-ipc read".into())
            .spawn(move || read_loop(&s, &h))
    };
    let writer = {
        let (s, h) = (shared.clone(), handle);
        std::thread::Builder::new()
            .name("rbs-api-ipc write".into())
            .spawn(move || write_loop(&s, &h))
    };
    for t in [&reader, &writer] {
        if t.is_err() {
            shared.end();
            shared.thread_ended();
        }
    }
    Some(Live {
        shared: Arc::downgrade(&shared),
        reader: reader.ok(),
        writer: writer.ok(),
    })
}

fn read_exact(stream: &dyn Stream, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        match stream.read(buf) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Reads and drops `len` bytes: a message over the limit is never held.
fn discard(stream: &dyn Stream, mut len: usize) -> io::Result<()> {
    let mut scratch = [0u8; 8192];
    while len > 0 {
        let n = len.min(scratch.len());
        read_exact(stream, &mut scratch[..n])?;
        len -= n;
    }
    Ok(())
}

fn read_loop(shared: &Shared, handle: &ConnectionHandle) {
    // Registered before the first quota check, so a `resume` is never lost.
    let _ = shared.reader.set(std::thread::current());
    let stream = shared.stream.as_ref();
    let mut header = [0u8; 4];
    'read: loop {
        while !handle.can_read() {
            {
                let o = shared.out();
                if o.done {
                    break 'read;
                }
                if o.close.is_some() {
                    // The server closes: nothing more is read, and the writer sends
                    // what is queued and the close frame, then ends the stream.
                    drop(o);
                    shared.thread_ended();
                    return;
                }
            }
            std::thread::park();
        }
        if read_exact(stream, &mut header).is_err() {
            break;
        }
        match Header::parse(header) {
            // The peer closes; nothing after its close frame matters.
            Header::Close(_) => break,
            Header::Message(len) => {
                let len = len as usize;
                if len > handle.max_message_bytes() {
                    handle.push_oversized(len);
                    if discard(stream, len).is_err() {
                        break;
                    }
                    continue;
                }
                let mut buf = vec![0u8; len];
                if read_exact(stream, &mut buf).is_err() {
                    break;
                }
                match String::from_utf8(buf) {
                    Ok(text) => {
                        handle.push_message(text);
                    }
                    // Not UTF-8: the peer does not speak this protocol.
                    Err(_) => break,
                }
            }
        }
    }
    shared.end();
    shared.thread_ended();
}

enum Item {
    Message(Arc<str>, bool),
    Close(CloseReason),
}

fn write_loop(shared: &Shared, handle: &ConnectionHandle) {
    let stream = shared.stream.as_ref();
    loop {
        let item = {
            let mut o = shared.out();
            loop {
                if o.done {
                    break None;
                }
                if let Some(m) = o.queue.pop_front() {
                    o.bytes -= m.len();
                    let writable = o.full && o.bytes <= shared.max_queued / 2;
                    if writable {
                        o.full = false;
                    }
                    break Some(Item::Message(m, writable));
                }
                if let Some(r) = o.close {
                    break Some(Item::Close(r));
                }
                o = shared
                    .changed
                    .wait(o)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        match item {
            None => break,
            Some(Item::Message(m, writable)) => {
                let sent = frame::message_header(m.len())
                    .and_then(|h| stream.write_all(&h))
                    .and_then(|()| stream.write_all(m.as_bytes()));
                if sent.is_err() {
                    break;
                }
                if writable {
                    handle.writable();
                }
            }
            Some(Item::Close(r)) => {
                let _ = stream.write_all(&frame::close(close_code(r), close_text(r)));
                break;
            }
        }
    }
    shared.out().writer_ended = true;
    shared.end();
    shared.thread_ended();
}
