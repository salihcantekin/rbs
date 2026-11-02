//! The file transport session: producer side (the output worker) and I/O thread.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rbs_core::media::{Bytes, ChunkKind, StreamDescriptor};
use rbs_transport::{
    QueueLimits, SendOutcome, Transport, TransportError, TransportEvent, TransportItem,
    TransportStats,
};

/// Slots in the hand-off ring; the byte and duration limits usually bind first.
const RING_SLOTS: usize = 1024;
/// Write buffer of the I/O thread.
const WRITE_BUFFER: usize = 256 * 1024;
/// Safety net for the park/unpark handshake (which is fenced and does not lose
/// wake-ups): the idle I/O thread still looks at the ring this often.
const IDLE_WAIT: Duration = Duration::from_millis(500);
const NO_TIMESTAMP: u64 = u64::MAX;
/// Share of the `finish` timeout spent waiting for a clean end; the rest is the grace
/// period after the abort, so `finish` never blocks longer than its timeout.
const CLEAN_SHARE: u32 = 4; // of 5
/// How long `Drop` waits for the aborted thread before detaching it.
const DROP_GRACE: Duration = Duration::from_millis(200);

struct Item {
    data: Bytes,
    /// `Some(offset)` for a patch.
    patch: Option<u64>,
    /// Master time of a `Media` chunk, in nanoseconds, or `NO_TIMESTAMP`.
    timestamp: u64,
}

/// Written by the I/O thread only, on its own cache line so the producer's stores do
/// not invalidate it.
#[derive(Default)]
#[repr(align(128))]
struct Progress {
    bytes_sent: AtomicU64,
    /// Timestamp of the last `Media` chunk written, or `NO_TIMESTAMP`.
    written_timestamp: AtomicU64,
    opened: AtomicBool,
    sleeping: AtomicBool,
}

/// State shared with the I/O thread: atomics and set-once cells only (P6).
#[derive(Default)]
struct Shared {
    /// Added by the producer before a push, subtracted by the I/O thread after a pop.
    queued_bytes: AtomicU64,
    progress: Progress,
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
    /// The counters as they were when the thread ended.
    Finished(TransportStats),
}

pub(crate) struct FileTransport {
    path: PathBuf,
    limits: QueueLimits,
    state: State,
    /// Timestamp of the first `Media` chunk queued; the duration limit counts from it
    /// until a chunk has been written.
    first_timestamp: Option<u64>,
    reported_connected: bool,
    reported_error: bool,
    /// The write error was returned by `send` once; later calls return `Closed`
    /// without building another message.
    returned_error: bool,
}

impl FileTransport {
    pub(crate) fn new(path: PathBuf, limits: QueueLimits) -> Self {
        Self {
            path,
            limits,
            state: State::Created,
            first_timestamp: None,
            reported_connected: false,
            reported_error: false,
            returned_error: false,
        }
    }
}

fn io_error(shared: &Shared, what: &str, e: &std::io::Error) {
    // The path is not included: it is the user's own and already known to the caller.
    let _ = shared.error.set(format!("{what}: {e}"));
}

/// The I/O thread: opens the file, writes items in order, applies patches. It signals
/// `done` only after the file is closed, so a caller that saw `done` can join without
/// waiting on the OS.
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
    // Pipes and devices cannot be synced (and some cannot seek); only regular files
    // are synced at the end.
    let regular = file.metadata().is_ok_and(|m| m.is_file());
    shared.progress.opened.store(true, Ordering::Release);
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
                        shared.progress.bytes_sent.fetch_add(len, Ordering::Relaxed);
                        if item.timestamp != NO_TIMESTAMP {
                            shared
                                .progress
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
                // Announce the nap, then look again. The fence pairs with the one in
                // `Running::wake`: either the producer sees `sleeping` and unparks us,
                // or we see its item.
                shared.progress.sleeping.store(true, Ordering::SeqCst);
                fence(Ordering::SeqCst);
                if consumer.is_empty() && !shared.stop.load(Ordering::SeqCst) {
                    thread::park_timeout(IDLE_WAIT);
                }
                shared.progress.sleeping.store(false, Ordering::SeqCst);
            }
        }
    }
    if failed || shared.abort.load(Ordering::Acquire) {
        // Discard the buffer: flushing it could block on a stalled target again.
        let (file, _unwritten) = out.into_parts();
        drop(file);
    } else {
        let flushed = out.flush().and_then(|()| {
            if regular {
                out.get_ref().sync_data()
            } else {
                Ok(())
            }
        });
        if let Err(e) = flushed {
            io_error(&shared, "flush", &e);
        }
        let (file, _unwritten) = out.into_parts();
        drop(file);
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
            // A patch only rewrites bytes already written; one past the end would leave
            // a hole and later chunks would land inside it.
            if offset.saturating_add(item.data.len() as u64) > end {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "patch beyond the end of the file",
                ));
            }
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
        // Pairs with the fence in `run_io`: orders the push before the load of
        // `sleeping` (a release store and a later load may otherwise be reordered).
        fence(Ordering::SeqCst);
        if self.shared.progress.sleeping.load(Ordering::SeqCst) {
            self.thread.thread().unpark();
        }
    }

    /// Stops the thread and returns the final counters with the result. With `abort`
    /// the queue is discarded at once and `budget` is the grace period; otherwise four
    /// fifths of `budget` are spent waiting for a clean end, then the thread is aborted
    /// and gets the rest. A thread still blocked in the OS after that (a hung network
    /// drive or a stalled pipe) is detached; it exits when that call returns.
    fn stop(self, budget: Duration, abort: bool) -> (Result<(), TransportError>, TransportStats) {
        // Both waits count against one deadline, so they cannot add up to more.
        let start = Instant::now();
        let deadline = start + budget;
        let clean = if abort {
            self.shared.abort.store(true, Ordering::Release);
            start
        } else {
            start + budget * CLEAN_SHARE / 5
        };
        self.shared.stop.store(true, Ordering::SeqCst);
        self.thread.thread().unpark();
        let mut timed_out = false;
        let clean_wait = clean.saturating_duration_since(Instant::now());
        if abort || self.done.recv_timeout(clean_wait).is_err() {
            timed_out = !abort;
            self.shared.abort.store(true, Ordering::Release);
            self.thread.thread().unpark();
            let grace = deadline.saturating_duration_since(Instant::now());
            if self.done.recv_timeout(grace).is_err() {
                return (Err(TransportError::Timeout), self.stats());
            }
        }
        let stats = self.stats();
        if self.thread.join().is_err() {
            return (Err(TransportError::Io("I/O thread panicked".into())), stats);
        }
        let result = match self.shared.error.get() {
            _ if timed_out => Err(TransportError::Timeout),
            Some(e) if !abort => Err(TransportError::Io(e.clone())),
            _ => Ok(()),
        };
        (result, stats)
    }

    fn stats(&self) -> TransportStats {
        let mut stats = TransportStats::default();
        stats.bytes_sent = self.shared.progress.bytes_sent.load(Ordering::Relaxed);
        stats.queued_bytes = self.shared.queued_bytes.load(Ordering::Relaxed);
        stats
    }
}

impl Transport for FileTransport {
    fn start(&mut self, _streams: &[StreamDescriptor]) -> Result<(), TransportError> {
        match self.state {
            State::Created => {}
            State::Running(_) => return Err(TransportError::Rejected("already started".into())),
            State::Finished(_) => return Err(TransportError::Closed),
        }
        let (producer, consumer) = rtrb::RingBuffer::new(RING_SLOTS);
        let shared = Arc::new(Shared {
            progress: Progress {
                written_timestamp: AtomicU64::new(NO_TIMESTAMP),
                ..Progress::default()
            },
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
            State::Finished(_) => return Err(TransportError::Closed),
            State::Running(running) => running,
        };
        let TransportItem::Chunk(chunk) = item else {
            return Err(TransportError::InvalidInput(
                "the file transport takes muxer chunks".into(),
            ));
        };
        if let Some(e) = running.shared.error.get() {
            if self.returned_error {
                return Err(TransportError::Closed);
            }
            self.returned_error = true;
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
        // Duration held: from the last chunk written (or the first queued, before any
        // write) to this one. That is one chunk interval stricter than counting from
        // the oldest queued chunk, and needs no per-item bookkeeping.
        if timestamp != NO_TIMESTAMP && queued > 0 {
            let written = running
                .shared
                .progress
                .written_timestamp
                .load(Ordering::Acquire);
            let oldest = if written == NO_TIMESTAMP {
                self.first_timestamp.unwrap_or(timestamp)
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
        if timestamp != NO_TIMESTAMP && self.first_timestamp.is_none() {
            self.first_timestamp = Some(timestamp);
        }
        running.wake();
        Ok(SendOutcome::Queued)
    }

    fn poll_events(&mut self, out: &mut Vec<TransportEvent>) {
        let State::Running(running) = &self.state else {
            return;
        };
        if !self.reported_connected && running.shared.progress.opened.load(Ordering::Acquire) {
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
        match &self.state {
            State::Created => TransportStats::default(),
            State::Running(running) => running.stats(),
            State::Finished(stats) => *stats,
        }
    }

    fn finish(&mut self, timeout: Duration) -> Result<(), TransportError> {
        let state = std::mem::replace(&mut self.state, State::Finished(TransportStats::default()));
        match state {
            State::Running(running) => {
                let (result, stats) = running.stop(timeout, false);
                self.state = State::Finished(stats);
                result
            }
            State::Created => Ok(()),
            State::Finished(stats) => {
                self.state = State::Finished(stats);
                Err(TransportError::Closed)
            }
        }
    }
}

impl Drop for FileTransport {
    fn drop(&mut self) {
        let state = std::mem::replace(&mut self.state, State::Finished(TransportStats::default()));
        if let State::Running(running) = state {
            // Dropped without `finish`: keep what was written, stop promptly.
            let _ = running.stop(DROP_GRACE, true);
        }
    }
}
