//! Native pictures: the decoder's `IOSurface`s handed out zero-copy
//! (ADR-0108 decision 4, capture port native surface rules).
//!
//! - Each picture's `CVPixelBuffer` is kept in a slot of [`Pool`] while a lease names
//!   the slot, so VideoToolbox does not reuse its `IOSurface` while the importer reads
//!   it. Slots are counted by `rbs_capture::handoff::Ring`: the decoder claims a slot
//!   (reference 1) and hands that reference to the frame's lease; the importer and the
//!   picture node release it. Neither `retain` nor `release` locks or frees.
//! - Buffers of free slots are dropped on the worker thread when it decodes the next
//!   picture. Their `IOSurface` stays retained for [`HANDLE_GRACE`] after that (in
//!   [`Seen`]), so a pointer value an importer may have cached is not reused.
//! - When the decoder is dropped, the pool and the seen list go to a graveyard: kept
//!   while any lease refers to a slot, then for the grace, then freed on the janitor
//!   thread, never on the render thread.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetIOSurface};
use objc2_io_surface::IOSurfaceRef;
use rbs_capture::handoff::{Grave, Graveyard, Ring};
use rbs_core::media::{Lease, LeaseOwner};

/// How long a handed-out surface stays retained after its last use (capture-port.md,
/// native surface rules: at least 10 s).
pub(crate) const HANDLE_GRACE: Duration = Duration::from_secs(10);

/// How often the graveyard's janitor checks stopped decoders.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// Slots of one decoder: more than the media source's surface budget (18), so a
/// claim fails only when consumers keep more pictures than the source allows.
pub(crate) const SLOTS: usize = 24;

/// Surfaces kept for their grace by a running decoder before older ones go to the
/// graveyard. VideoToolbox's pool holds a handful of surfaces per size.
const SEEN_MAX: usize = 64;

static GRAVEYARD: Graveyard = Graveyard::new("rbs-media-avf-janitor", HANDLE_GRACE, JANITOR_PERIOD);

/// A CoreFoundation object moved between the worker and the janitor.
///
/// `CVPixelBuffer` and `IOSurface` retain and release are thread-safe, and the adapter
/// never uses the wrapped object from two threads at once.
pub(crate) struct Sendable<T>(pub(crate) T);

// SAFETY: see the type's doc.
unsafe impl<T> Send for Sendable<T> {}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The slots and the pixel buffers they hold. Frames hold an `Arc` of it in their
/// lease.
pub(crate) struct Pool {
    ring: Ring,
    /// One entry per slot. Only the decoder's thread locks it, never a lease.
    buffers: Mutex<Vec<Option<Sendable<CFRetained<CVPixelBuffer>>>>>,
}

// SAFETY: delegates to `Ring`, whose counts are exact: the decoder claims a slot (one
// reference) and hands exactly that reference to one `Lease`; every clone retains and
// every drop releases once. A slot's pixel buffer (and so its IOSurface) stays in
// `buffers` while the slot is referenced: the decoder drops a buffer only for a free
// slot, and only the decoder can make a free slot referenced again (by `claim`). So the
// surface stays valid and unwritten by VideoToolbox while any lease names it. Neither
// method locks.
unsafe impl LeaseOwner for Pool {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// The decoder's end of the pool.
pub(crate) struct Surfaces {
    pool: Arc<Pool>,
    owner: Arc<dyn LeaseOwner>,
    seen: Seen,
}

impl Surfaces {
    pub(crate) fn new() -> Self {
        let pool = Arc::new(Pool {
            ring: Ring::new(&[0; SLOTS]),
            buffers: Mutex::new((0..SLOTS).map(|_| None).collect()),
        });
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&pool) as Arc<dyn LeaseOwner>;
        Self {
            pool,
            owner,
            seen: Seen::with_capacity(SEEN_MAX),
        }
    }

    /// Drops the buffers of slots nobody references any more; their surfaces wait out
    /// the grace in the seen list. Worker thread.
    fn recycle(&mut self, now: Instant) {
        let mut buffers = lock(&self.pool.buffers);
        for (slot, entry) in buffers.iter_mut().enumerate() {
            if entry.is_none() || !self.pool.ring.is_free(slot) {
                continue;
            }
            if let Some(Sendable(buffer)) = entry.take()
                && let Some(surface) = CVPixelBufferGetIOSurface(Some(&buffer))
            {
                self.seen.keep(surface, now);
            }
        }
        drop(buffers);
        self.seen.expire(now);
    }

    /// Keeps `buffer` in a free slot and returns its surface and the lease that names
    /// the slot, or `None` when every slot is referenced (the caller copies instead).
    pub(crate) fn hand_out(
        &mut self,
        buffer: CFRetained<CVPixelBuffer>,
    ) -> Option<(CFRetained<IOSurfaceRef>, Lease)> {
        self.recycle(Instant::now());
        let surface = CVPixelBufferGetIOSurface(Some(&buffer))?;
        let slot = self.pool.ring.claim()?;
        if let Some(entry) = lock(&self.pool.buffers).get_mut(slot) {
            *entry = Some(Sendable(buffer));
        } else {
            self.pool.ring.abandon(slot);
            return None;
        }
        // SAFETY: `claim` counted exactly one reference to `slot`, handed over here.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        Some((surface, lease))
    }
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        let seen = std::mem::replace(&mut self.seen, Seen::with_capacity(0));
        GRAVEYARD.bury(Box::new(PoolGrave {
            pool: Arc::clone(&self.pool),
            _seen: seen,
        }));
    }
}

/// Surfaces a picture was handed out with and that the decoder no longer holds, kept
/// for the grace so their pointer values are not reused while an importer may cache
/// them.
struct Seen {
    entries: Vec<(Sendable<CFRetained<IOSurfaceRef>>, Instant)>,
}

impl Seen {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
        }
    }

    /// Keeps `surface` from `now`. When full, the oldest entry goes to the graveyard,
    /// which keeps it for the rest of its grace.
    fn keep(&mut self, surface: CFRetained<IOSurfaceRef>, now: Instant) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|(s, _)| std::ptr::eq(&*s.0, &*surface))
        {
            entry.1 = now;
            return;
        }
        if self.entries.len() >= SEEN_MAX {
            let (oldest, since) = self.entries.remove(0);
            GRAVEYARD.bury(Box::new(SurfaceGrave {
                _surface: oldest,
                until: since + HANDLE_GRACE,
            }));
        }
        self.entries.push((Sendable(surface), now));
    }

    /// Releases surfaces whose grace has passed.
    fn expire(&mut self, now: Instant) {
        self.entries
            .retain(|(_, since)| now.saturating_duration_since(*since) < HANDLE_GRACE);
    }
}

/// One surface moved out of a full [`Seen`] list. Kept until its own grace ends.
struct SurfaceGrave {
    _surface: Sendable<CFRetained<IOSurfaceRef>>,
    until: Instant,
}

impl Grave for SurfaceGrave {
    fn is_idle(&self) -> bool {
        // Always true (`until - HANDLE_GRACE` lies in the past): the graveyard keeps the
        // surface a full grace from burial, which ends after `until`.
        Instant::now() + HANDLE_GRACE >= self.until
    }
    fn bytes(&self) -> u64 {
        0
    }
}

/// The pool of a dropped decoder. Kept while pictures reference it (so the last lease
/// never frees anything on the render thread) and for the grace after that.
struct PoolGrave {
    pool: Arc<Pool>,
    _seen: Seen,
}

impl Grave for PoolGrave {
    fn is_idle(&self) -> bool {
        self.pool.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        0
    }
}
