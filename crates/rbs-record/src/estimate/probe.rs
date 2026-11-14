//! The disk probe of the estimate (ADR-0087 decision 1, spec
//! `docs/specs/output-estimate.md#disk-probe`).
//!
//! [`DiskProbe`] keeps the free space of recording folders for [`CACHE_FOR`] on the
//! caller's clock and reads a missing or stale folder on a short-lived thread, one at
//! a time. [`DiskProbe::free_space`] never waits: a folder with no fresh reading is
//! unknown for this estimate, and the reading is there for the next one. A read that
//! takes longer than [`PROBE_TIMEOUT`] makes the folder unknown, with a reason, until
//! a later read answers. No thread runs between requests (P13).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use rbs_core::time::Timestamp;

/// How long one free-space read may take before the folder counts as unknown.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a reading of a folder is used.
pub const CACHE_FOR: Duration = Duration::from_secs(60);

/// Reads the bytes the current user can still write in a folder. May block (a network
/// share); it runs only on the probe's own thread. Wired in by a composition root
/// (`rbs-system-probe`); tests use a fake.
pub type ReadFreeSpace = Arc<dyn Fn(&Path) -> Result<u64, String> + Send + Sync>;

/// What is known about a folder's free space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FreeSpace {
    /// Bytes free, read less than [`CACHE_FOR`] ago.
    Known(u64),
    /// No fresh reading yet; one is on its way.
    Pending,
    /// The last read failed or took longer than [`PROBE_TIMEOUT`].
    Unknown(String),
}

impl FreeSpace {
    /// The bytes, when known.
    pub fn bytes(&self) -> Option<u64> {
        match self {
            Self::Known(b) => Some(*b),
            Self::Pending | Self::Unknown(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    at: Timestamp,
    value: Result<u64, String>,
}

struct InFlight {
    folder: PathBuf,
    asked_at: Timestamp,
    answer: Receiver<Result<u64, String>>,
    timed_out: bool,
}

/// Free space per recording folder, read on demand.
pub struct DiskProbe {
    read: ReadFreeSpace,
    cache: HashMap<PathBuf, Entry>,
    in_flight: Option<InFlight>,
    timeout: Duration,
    cache_for: Duration,
}

impl std::fmt::Debug for DiskProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskProbe")
            .field("cached", &self.cache.len())
            .field("busy", &self.in_flight.is_some())
            .finish()
    }
}

impl DiskProbe {
    /// A probe with [`PROBE_TIMEOUT`] and [`CACHE_FOR`]. Starts no thread.
    pub fn new(read: ReadFreeSpace) -> Self {
        Self::with_timing(read, PROBE_TIMEOUT, CACHE_FOR)
    }

    /// [`DiskProbe::new`] with another timeout and cache time.
    pub fn with_timing(read: ReadFreeSpace, timeout: Duration, cache_for: Duration) -> Self {
        Self {
            read,
            cache: HashMap::new(),
            in_flight: None,
            timeout,
            cache_for,
        }
    }

    /// Takes an answer that arrived and marks a read that took too long. Never waits.
    pub fn poll(&mut self, now: Timestamp) {
        let Some(f) = &mut self.in_flight else { return };
        match f.answer.try_recv() {
            Ok(value) => {
                let folder = f.folder.clone();
                self.in_flight = None;
                self.cache.insert(folder, Entry { at: now, value });
            }
            Err(TryRecvError::Disconnected) => {
                let folder = f.folder.clone();
                self.in_flight = None;
                self.cache.insert(
                    folder,
                    Entry {
                        at: now,
                        value: Err("the probe stopped".to_owned()),
                    },
                );
            }
            Err(TryRecvError::Empty) => {
                if !f.timed_out && now.saturating_since(f.asked_at) >= self.timeout {
                    f.timed_out = true;
                    let folder = f.folder.clone();
                    let reason = format!(
                        "reading the free space took longer than {} s",
                        self.timeout.as_secs()
                    );
                    self.cache.insert(
                        folder,
                        Entry {
                            at: now,
                            value: Err(reason),
                        },
                    );
                }
            }
        }
    }

    /// The free space of `folder` as known at `now`. A missing or stale reading starts
    /// a read for the next estimate, unless one is already running (a slow drive keeps
    /// one thread at most), and answers [`FreeSpace::Pending`] or the last failure.
    pub fn free_space(&mut self, folder: &Path, now: Timestamp) -> FreeSpace {
        self.poll(now);
        let entry = self.cache.get(folder).cloned();
        let fresh = entry
            .as_ref()
            .is_some_and(|e| now.saturating_since(e.at) < self.cache_for);
        if !fresh {
            self.ask(folder, now);
        }
        match entry {
            Some(Entry { value: Ok(b), .. }) if fresh => FreeSpace::Known(b),
            Some(Entry { value: Err(r), .. }) => FreeSpace::Unknown(r),
            _ => FreeSpace::Pending,
        }
    }

    /// A read is running.
    pub fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    fn ask(&mut self, folder: &Path, now: Timestamp) {
        if self.in_flight.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let read = Arc::clone(&self.read);
        let path = folder.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("rbs-estimate-probe".into())
            .spawn(move || {
                let _ = tx.send(read(&path));
            });
        match spawned {
            // The thread ends after its one read; it is not joined, so a hung drive
            // never holds the caller.
            Ok(_) => {
                self.in_flight = Some(InFlight {
                    folder: folder.to_path_buf(),
                    asked_at: now,
                    answer: rx,
                    timed_out: false,
                });
            }
            Err(e) => {
                self.cache.insert(
                    folder.to_path_buf(),
                    Entry {
                        at: now,
                        value: Err(format!("the probe could not start: {e}")),
                    },
                );
            }
        }
    }

    /// Blocks up to `limit` of real time until the running read has answered. For
    /// tests (as a hang guard) and nothing else: the control thread never waits.
    pub fn wait_for_answer(&mut self, limit: Duration) -> bool {
        let Some(f) = &self.in_flight else {
            return false;
        };
        match f.answer.recv_timeout(limit) {
            Ok(value) => {
                let (folder, at) = (f.folder.clone(), f.asked_at);
                self.in_flight = None;
                self.cache.insert(folder, Entry { at, value });
                true
            }
            Err(_) => false,
        }
    }
}
