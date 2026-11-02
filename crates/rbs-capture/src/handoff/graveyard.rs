//! Deferred release of native surfaces.
//!
//! Importers cache opened surfaces by their handle or pointer value, so a producer must
//! not let a value it handed out name another object while an importer may still cache
//! it (capture-port.md, "Native surface rules"). And the render thread must never free
//! a surface, which would happen if it dropped the last lease of a stopped session.
//!
//! A [`Graveyard`] solves both: adapters bury surfaces they no longer use. A grave is
//! kept while [`Grave::is_idle`] is false (frames still reference it) and for a grace
//! period after it became idle, then dropped on the graveyard's janitor thread. The
//! janitor exists only while the graveyard holds something (P13).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Something buried in a [`Graveyard`]. Its `Drop` releases the OS objects.
pub trait Grave: Send {
    /// Whether nothing references the buried objects any more. Checked by the janitor;
    /// must not block.
    fn is_idle(&self) -> bool {
        true
    }

    /// Bytes of memory the grave holds, counted in [`Graveyard::bytes`] until it is
    /// released.
    fn bytes(&self) -> u64 {
        0
    }
}

struct Entry {
    grave: Box<dyn Grave>,
    bytes: u64,
    idle_since: Option<Instant>,
}

/// Graves waiting out their grace period. Usually a `static` of the adapter crate.
pub struct Graveyard {
    grace: Duration,
    period: Duration,
    entries: Mutex<Vec<Entry>>,
    bytes: AtomicU64,
    janitor: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Graveyard {
    /// A graveyard that keeps graves for `grace` after they become idle; its janitor
    /// checks every `period`.
    pub const fn new(grace: Duration, period: Duration) -> Self {
        Self {
            grace,
            period,
            entries: Mutex::new(Vec::new()),
            bytes: AtomicU64::new(0),
            janitor: AtomicBool::new(false),
        }
    }

    /// The grace period.
    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Bytes held by graves not yet released.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }

    /// Number of graves not yet released.
    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    /// True when nothing waits for release.
    pub fn is_empty(&self) -> bool {
        lock(&self.entries).is_empty()
    }

    /// Adds a grave without starting the janitor; the caller calls [`Graveyard::reap`].
    /// `now` starts the grace if the grave is already idle.
    pub fn push(&self, grave: Box<dyn Grave>, now: Instant) {
        let bytes = grave.bytes();
        let idle_since = grave.is_idle().then_some(now);
        self.bytes.fetch_add(bytes, Ordering::AcqRel);
        lock(&self.entries).push(Entry {
            grave,
            bytes,
            idle_since,
        });
    }

    /// Releases graves whose grace has passed at `now`, outside the lock. Returns
    /// whether the graveyard is empty afterwards.
    pub fn reap(&self, now: Instant) -> bool {
        let expired: Vec<Entry> = {
            let mut entries = lock(&self.entries);
            for entry in entries.iter_mut() {
                if entry.grave.is_idle() {
                    entry.idle_since.get_or_insert(now);
                } else {
                    entry.idle_since = None;
                }
            }
            let grace = self.grace;
            let (expired, kept): (Vec<Entry>, Vec<Entry>) =
                std::mem::take(&mut *entries).into_iter().partition(|e| {
                    e.idle_since
                        .is_some_and(|t| now.saturating_duration_since(t) >= grace)
                });
            *entries = kept;
            expired
        };
        for entry in expired {
            let bytes = entry.bytes;
            drop(entry);
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        }
        self.is_empty()
    }

    /// Adds a grave and makes sure the janitor thread runs. If the thread cannot be
    /// started, the grave stays (valid) until the next `bury` starts one.
    pub fn bury(&'static self, grave: Box<dyn Grave>) {
        self.push(grave, Instant::now());
        if self.janitor.swap(true, Ordering::AcqRel) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("rbs-capture-janitor".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(self.period);
                    if self.reap(Instant::now()) {
                        self.janitor.store(false, Ordering::Release);
                        // A grave added after the check would otherwise wait for the
                        // next `bury`: take the job back if so.
                        if self.is_empty() || self.janitor.swap(true, Ordering::AcqRel) {
                            return;
                        }
                    }
                }
            });
        if spawned.is_err() {
            self.janitor.store(false, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Probe {
        idle: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
        bytes: u64,
    }

    impl Grave for Probe {
        fn is_idle(&self) -> bool {
            self.idle.load(Ordering::Acquire)
        }
        fn bytes(&self) -> u64 {
            self.bytes
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    fn probe(idle: bool, bytes: u64) -> (Box<Probe>, Arc<AtomicBool>, Arc<AtomicBool>) {
        let idle = Arc::new(AtomicBool::new(idle));
        let dropped = Arc::new(AtomicBool::new(false));
        let grave = Box::new(Probe {
            idle: Arc::clone(&idle),
            dropped: Arc::clone(&dropped),
            bytes,
        });
        (grave, idle, dropped)
    }

    const GRACE: Duration = Duration::from_secs(10);

    #[test]
    fn idle_grave_is_released_after_the_grace() {
        let yard = Graveyard::new(GRACE, Duration::from_secs(1));
        let t0 = Instant::now();
        let (grave, _, dropped) = probe(true, 100);
        yard.push(grave, t0);
        assert_eq!(yard.bytes(), 100);
        assert!(!yard.reap(t0 + GRACE - Duration::from_millis(1)));
        assert!(!dropped.load(Ordering::Acquire));
        assert!(yard.reap(t0 + GRACE));
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(yard.bytes(), 0);
    }

    #[test]
    fn grace_starts_when_the_grave_becomes_idle() {
        let yard = Graveyard::new(GRACE, Duration::from_secs(1));
        let t0 = Instant::now();
        let (grave, idle, dropped) = probe(false, 0);
        yard.push(grave, t0);
        // Held for a long time: never released.
        assert!(!yard.reap(t0 + GRACE * 3));
        idle.store(true, Ordering::Release);
        let t1 = t0 + GRACE * 4;
        assert!(!yard.reap(t1));
        assert!(!yard.reap(t1 + GRACE - Duration::from_millis(1)));
        assert!(!dropped.load(Ordering::Acquire));
        assert!(yard.reap(t1 + GRACE));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn a_grave_referenced_again_restarts_its_grace() {
        let yard = Graveyard::new(GRACE, Duration::from_secs(1));
        let t0 = Instant::now();
        let (grave, idle, dropped) = probe(true, 0);
        yard.push(grave, t0);
        idle.store(false, Ordering::Release);
        assert!(!yard.reap(t0 + GRACE));
        idle.store(true, Ordering::Release);
        assert!(!yard.reap(t0 + GRACE * 2 - Duration::from_millis(1)));
        assert!(!dropped.load(Ordering::Acquire));
        // Idle again since the previous reap (t0 + 2*GRACE - 1 ms).
        assert!(yard.reap(t0 + GRACE * 3));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn janitor_releases_and_exits() {
        static YARD: Graveyard = Graveyard::new(Duration::ZERO, Duration::from_millis(1));
        let (grave, _, dropped) = probe(true, 7);
        YARD.bury(grave);
        // The janitor runs on its own thread; wait for it without a fixed sleep.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dropped.load(Ordering::Acquire) || YARD.bytes() != 0 {
            assert!(
                Instant::now() < deadline,
                "janitor did not release the grave"
            );
            std::thread::yield_now();
        }
        assert!(YARD.is_empty());
    }
}
