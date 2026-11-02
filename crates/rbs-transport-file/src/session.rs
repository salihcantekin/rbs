//! The file transport session: producer side (the output worker) and I/O thread.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rbs_core::media::{Bytes, ChunkKind, StreamDescriptor};
use rbs_transport::{
    QueueLimits, SendOutcome, Transport, TransportError, TransportEvent, TransportItem,
    TransportStats,
};

/// Slots in the hand-off ring; the byte and duration limits usually bind first.
const RING_SLOTS: usize = 1024;
/// Write buffer of the I/O thread.
const WRITE_BUFFER: usize = 256 * 1024;
/// The I/O thread wakes at least this often while idle, so a missed wake-up costs at
/// most this much latency.
const IDLE_WAIT: Duration = Duration::from_millis(50);
const NO_TIMESTAMP: u64 = u64::MAX;
/// After a timed-out `finish` or a `Drop`, how long to wait for the thread to notice
/// the abort before detaching it.
const ABORT_GRACE: Duration = Duration::from_secs(1);

struct Item {
    data: Bytes,
    /// `Some(offset)` for a patch.
    patch: Option<u64>,
    /// Master time of a `Media` chunk, in nanoseconds, or `NO_TIMESTAMP`.
    timestamp: u64,
}

/// State shared with the I/O thread: atomics and set-once cells only (P6).
#[derive(Default)]
struct Shared {
    queued_bytes: AtomicU64,
    bytes_sent: AtomicU64,
    /// Timestamp of the last `Media` chunk written, or `NO_TIMESTAMP`.
    written_timestamp: AtomicU64,
    opened: AtomicBool,
    sleeping: AtomicBool,
    /// Finish: write what is queued, then exit.
    stop: AtomicBool,
    /// Give up: discard what is queued and exit after the current write.
    abort: AtomicBool,
    error: OnceLock<String>,
}

struct Running {
    producer: rtrb::Producer<Item>,
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
    done: mpsc::Receiver<()>,
}

enum State {
    Created,
    Running(Running),
    Finished,
}

pub(crate) struct FileTransport {
    path: PathBuf,
    limits: QueueLimits,
    state: State,
    newest_timestamp: Option<u64>,
    reported_connected: bool,
    reported_error: bool,
}

impl FileTransport {
    pub(crate) fn new(path: PathBuf, limits: QueueLimits) -> Self {
        Self {
            path,
            limits,
            state: State::Created,
            newest_timestamp: None,
            reported_connected: false,
            reported_error: false,
        }
    }
}

fn io_error(shared: &Shared, what: &str, e: &std::io::Error) {
    // The path is not included: it is the user's own and already known to the caller.
    let _ = shared.error.set(format!("{what}: {e}"));
}

/// The I/O thread: opens the file, writes items in order, applies patches.
fn run_io(
    path: PathBuf,
    mut consumer: rtrb::Consumer<Item>,
    shared: Arc<Shared>,
    done: mpsc::Sender<()>,
) {
    let file = match File::create(&path) {
        Ok(file) => file,
        Err(e) => {
            io_error(&shared, "open", &e);
            discard(&mut consumer, &shared);
            let _ = done.send(());
            return;
        }
    };
    shared.opened.store(true, Ordering::Release);
    let mut out = BufWriter::with_capacity(WRITE_BUFFER, file);
    let mut failed = false;
    loop {
        if shared.abort.load(Ordering::Acquire) {
            discard(&mut consumer, &shared);
            break;
        }
        match consumer.pop() {
            Ok(item) => {
                let len = item.data.len() as u64;
                if !failed {
                    if let Err(e) = write_item(&mut out, &item) {
                        io_error(&shared, "write", &e);
                        failed = true;
                    } else {
                        shared.bytes_sent.fetch_add(len, Ordering::Relaxed);
                        if item.timestamp != NO_TIMESTAMP {
                            shared
                                .written_timestamp
                                .store(item.timestamp, Ordering::Release);
                        }
                    }
                }
                shared.queued_bytes.fetch_sub(len, Ordering::AcqRel);
            }
            Err(_) => {
                if shared.stop.load(Ordering::Acquire) {
                    break;
                }
                // Announce the nap, then look again, so a producer that pushed in
                // between either sees `sleeping` and unparks us, or we see its item.
                shared.sleeping.store(true, Ordering::SeqCst);
                if consumer.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                    thread::park_timeout(IDLE_WAIT);
                }
                shared.sleeping.store(false, Ordering::SeqCst);
            }
        }
    }
    if !failed && !shared.abort.load(Ordering::Acquire) {
        let flushed = out.flush().and_then(|()| match out.get_ref().sync_data() {
            // Pipes and some special files cannot be synced; nothing is lost.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => Ok(()),
            other => other,
        });
        if let Err(e) = flushed {
            io_error(&shared, "flush", &e);
        }
    }
    let _ = done.send(());
}

fn write_item(out: &mut BufWriter<File>, item: &Item) -> std::io::Result<()> {
    match item.patch {
        None => out.write_all(&item.data),
        Some(offset) => {
            out.flush()?;
            let file = out.get_mut();
            let end = file.stream_position()?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&item.data)?;
            file.seek(SeekFrom::Start(end))?;
            Ok(())
        }
    }
}

fn discard(consumer: &mut rtrb::Consumer<Item>, shared: &Shared) {
    while let Ok(item) = consumer.pop() {
        shared
            .queued_bytes
            .fetch_sub(item.data.len() as u64, Ordering::AcqRel);
    }
}

impl Running {
    fn wake(&self) {
        if self.shared.sleeping.load(Ordering::SeqCst) {
            self.thread.thread().unpark();
        }
    }

    /// Stops the thread: `abort` discards what is queued. Waits at most `timeout`
    /// for a clean end; after that it aborts and joins.
    fn stop(self, timeout: Duration, abort: bool) -> Result<(), TransportError> {
        if abort {
            self.shared.abort.store(true, Ordering::Release);
        }
        self.shared.stop.store(true, Ordering::SeqCst);
        self.thread.thread().unpark();
        let mut timed_out = false;
        if self.done.recv_timeout(timeout).is_err() {
            timed_out = true;
            self.shared.abort.store(true, Ordering::Release);
            self.thread.thread().unpark();
            // The thread exits after its current write. If the OS keeps it blocked (a
            // hung network drive), it is detached rather than blocking the caller; it
            // exits once that call returns.
            if self.done.recv_timeout(ABORT_GRACE).is_err() {
                return Err(TransportError::Timeout);
            }
        }
        let panicked = self.thread.join().is_err();
        if panicked {
            return Err(TransportError::Io("I/O thread panicked".into()));
        }
        if timed_out {
            return Err(TransportError::Timeout);
        }
        match self.shared.error.get() {
            Some(e) if !abort => Err(TransportError::Io(e.clone())),
            _ => Ok(()),
        }
    }
}

impl Transport for FileTransport {
    fn start(&mut self, _streams: &[StreamDescriptor]) -> Result<(), TransportError> {
        match self.state {
            State::Created => {}
            State::Running(_) => return Err(TransportError::Rejected("already started".into())),
            State::Finished => return Err(TransportError::Closed),
        }
        let (producer, consumer) = rtrb::RingBuffer::new(RING_SLOTS);
        let shared = Arc::new(Shared {
            written_timestamp: AtomicU64::new(NO_TIMESTAMP),
            ..Shared::default()
        });
        let (done_tx, done) = mpsc::channel();
        let path = self.path.clone();
        let io_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("rbs-file-transport".into())
            .spawn(move || run_io(path, consumer, io_shared, done_tx))
            .map_err(|e| TransportError::Io(format!("cannot start the I/O thread: {e}")))?;
        self.state = State::Running(Running {
            producer,
            shared,
            thread,
            done,
        });
        Ok(())
    }

    fn send(&mut self, item: TransportItem<'_>) -> Result<SendOutcome, TransportError> {
        let running = match &mut self.state {
            State::Created => return Err(TransportError::NotStarted),
            State::Finished => return Err(TransportError::Closed),
            State::Running(running) => running,
        };
        let TransportItem::Chunk(chunk) = item else {
            return Err(TransportError::InvalidInput(
                "the file transport takes muxer chunks".into(),
            ));
        };
        if let Some(e) = running.shared.error.get() {
            return Err(TransportError::Io(e.clone()));
        }
        let (patch, timestamp) = match chunk.kind {
            ChunkKind::Patch { offset } => (Some(offset), NO_TIMESTAMP),
            ChunkKind::Media { timestamp, .. } => (None, timestamp.as_nanos()),
            _ => (None, NO_TIMESTAMP),
        };
        let len = chunk.data.len() as u64;
        let queued = running.shared.queued_bytes.load(Ordering::Acquire);
        if queued > 0 && queued + len > self.limits.max_bytes as u64 {
            return Err(TransportError::QueueFull);
        }
        if timestamp != NO_TIMESTAMP && queued > 0 {
            let written = running.shared.written_timestamp.load(Ordering::Acquire);
            let oldest = if written == NO_TIMESTAMP {
                self.newest_timestamp.unwrap_or(timestamp)
            } else {
                written
            };
            let held = Duration::from_nanos(timestamp.saturating_sub(oldest));
            if held > self.limits.max_duration {
                return Err(TransportError::QueueFull);
            }
        }
        // Count the bytes before the push, so the I/O side never subtracts first.
        running.shared.queued_bytes.fetch_add(len, Ordering::AcqRel);
        let pushed = running.producer.push(Item {
            data: chunk.data.clone(),
            patch,
            timestamp,
        });
        if pushed.is_err() {
            running.shared.queued_bytes.fetch_sub(len, Ordering::AcqRel);
            return Err(TransportError::QueueFull);
        }
        if timestamp != NO_TIMESTAMP && self.newest_timestamp.is_none() {
            self.newest_timestamp = Some(timestamp);
        }
        running.wake();
        Ok(SendOutcome::Queued)
    }

    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        let State::Running(running) = &self.state else {
            return;
        };
        if !self.reported_connected && running.shared.opened.load(Ordering::Acquire) {
            self.reported_connected = true;
            out.push(TransportEvent::Connected);
        }
        if !self.reported_error {
            if let Some(e) = running.shared.error.get() {
                self.reported_error = true;
                out.push(TransportEvent::Disconnected {
                    error: TransportError::Io(e.clone()),
                });
            }
        }
    }

    fn stats(&self) -> TransportStats {
        let mut stats = TransportStats::default();
        if let State::Running(running) = &self.state {
            stats.bytes_sent = running.shared.bytes_sent.load(Ordering::Relaxed);
            stats.queued_bytes = running.shared.queued_bytes.load(Ordering::Relaxed);
        }
        stats
    }

    fn finish(&mut self, timeout: Duration) -> Result<(), TransportError> {
        match std::mem::replace(&mut self.state, State::Finished) {
            State::Running(running) => running.stop(timeout, false),
            State::Created => Ok(()),
            State::Finished => Err(TransportError::Closed),
        }
    }
}

impl Drop for FileTransport {
    fn drop(&mut self) {
        if let State::Running(running) = std::mem::replace(&mut self.state, State::Finished) {
            // Dropped without `finish`: keep what was written, stop promptly.
            let _ = running.stop(Duration::ZERO, true);
        }
    }
}
