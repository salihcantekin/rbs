//! Native pictures: the decoder's IOSurface-backed pixel buffers handed out zero-copy
//! (backend-ports.md, live video decode invariants 8 and 9).
//!
//! - The adapter keeps a slot table of retained `CVPixelBuffer`s, separate from the
//!   reference pictures VideoToolbox decodes against: while a lease names a slot its
//!   buffer stays retained, so VideoToolbox never writes that `IOSurface` again
//!   (VideoToolbox does not reuse a buffer it handed out while it is retained). Slots
//!   are counted by `rbs_capture::handoff::Ring`: the worker claims a free slot (one
//!   reference) and hands that reference to the frame's lease. With every slot leased
//!   the picture is dropped; the adapter never allocates beyond and never waits.
//!   Neither `retain` nor `release` locks or frees.
//! - Buffers of free slots are dropped on the worker when it hands out the next
//!   picture, so VideoToolbox can reuse their surfaces.
//! - When the decoder is dropped, a pool some picture used goes to a graveyard: kept
//!   while a lease refers to a slot, then for [`HANDLE_GRACE`] (the file port's native
//!   surface rules), then freed on the janitor thread, never on the render thread. The
//!   janitor starts on the first burial.
//!
//! Low-level module (principle P11): every `unsafe` block states its invariant.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetIOSurface};
use objc2_io_surface::IOSurfaceRef;
use rbs_capture::handoff::{Grave, Graveyard, Ring};
use rbs_core::media::{Lease, LeaseOwner};

/// How long a dropped decoder's surfaces stay after their last reference.
const HANDLE_GRACE: Duration = Duration::from_secs(10);

/// How often the graveyard's janitor checks dropped decoders.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// Pools of dropped decoders.
static GRAVEYARD: Graveyard = Graveyard::new(
    "rbs-codec-videotoolbox-janitor",
    HANDLE_GRACE,
    JANITOR_PERIOD,
);

/// A decoded pixel buffer moved between the worker and the janitor.
struct SendableBuffer {
    _buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: `CVPixelBuffer` retain and release are thread-safe; the adapter reads a
// buffer only on the worker, and the janitor only releases it.
unsafe impl Send for SendableBuffer {}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The slots and the pixel buffers they hold. Frames hold an `Arc` of it in their lease.
struct Pool {
    ring: Ring,
    /// One entry per slot. Only the worker (and the janitor, after the last lease)
    /// locks it, never a lease.
    buffers: Mutex<Vec<Option<SendableBuffer>>>,
}

// SAFETY: delegates to `Ring`, whose counts are exact: the worker claims a slot (one
// reference) and hands exactly that reference to one `Lease`; every clone retains and
// every drop releases once. A slot's pixel buffer (and so its IOSurface) stays in
// `buffers` while the slot is referenced: the worker drops a buffer only for a free
// slot, and only the worker can make a free slot referenced again (by `claim`). So the
// surface stays valid and unwritten while any lease names it. Neither method locks.
unsafe impl LeaseOwner for Pool {
    fn retain(&self, token: u64) {
        self.ring.retain(token);
    }

    fn release(&self, token: u64) {
        self.ring.release(token);
    }
}

/// The worker's end of the pool.
pub(super) struct Slots {
    pool: Arc<Pool>,
    owner: Arc<dyn LeaseOwner>,
    count: usize,
    used: bool,
}

impl Slots {
    /// A pool of `count` slots. Allocates the table only; no buffer, no thread.
    pub(super) fn new(count: usize) -> Self {
        let pool = Arc::new(Pool {
            ring: Ring::new(&vec![0; count]),
            buffers: Mutex::new((0..count).map(|_| None).collect()),
        });
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&pool) as Arc<dyn LeaseOwner>;
        Self {
            pool,
            owner,
            count,
            used: false,
        }
    }

    /// Slots of the pool (`max_surfaces`).
    pub(super) fn count(&self) -> usize {
        self.count
    }

    /// Drops the buffers of slots nobody references any more.
    fn recycle(&mut self) {
        let mut buffers = lock(&self.pool.buffers);
        for (slot, entry) in buffers.iter_mut().enumerate() {
            if entry.is_some() && self.pool.ring.is_free(slot) {
                *entry = None;
            }
        }
    }

    /// Keeps `buffer` in a free slot and returns its surface and the lease that names
    /// the slot, or `None` when every slot is referenced (the caller drops the
    /// picture), or the buffer back when it has no IOSurface.
    pub(super) fn hand_out(&mut self, buffer: CFRetained<CVPixelBuffer>) -> HandOut {
        self.recycle();
        let Some(surface) = CVPixelBufferGetIOSurface(Some(&buffer)) else {
            return HandOut::NoSurface(buffer);
        };
        let Some(slot) = self.pool.ring.claim() else {
            return HandOut::Full;
        };
        if let Some(entry) = lock(&self.pool.buffers).get_mut(slot) {
            *entry = Some(SendableBuffer { _buffer: buffer });
        } else {
            self.pool.ring.abandon(slot);
            return HandOut::Full;
        }
        self.used = true;
        // SAFETY: `claim` counted exactly one reference to `slot`, handed over here.
        let lease = unsafe { Lease::adopt(Arc::clone(&self.owner), slot as u64) };
        HandOut::Leased(surface, lease)
    }
}

/// What [`Slots::hand_out`] made of a buffer.
pub(super) enum HandOut {
    /// The buffer is kept in a slot named by the lease.
    Leased(CFRetained<IOSurfaceRef>, Lease),
    /// Every slot is leased.
    Full,
    /// The buffer is not IOSurface-backed; given back.
    NoSurface(CFRetained<CVPixelBuffer>),
}

impl Drop for Slots {
    fn drop(&mut self) {
        self.recycle();
        // A never-used pool has no buffer and no handed-out surface.
        if self.used {
            GRAVEYARD.bury(Box::new(PoolGrave(Arc::clone(&self.pool))));
        }
    }
}

/// The pool of a dropped decoder. Kept while pictures reference it, so the last lease
/// never frees anything on the render thread, and for the grace after that.
struct PoolGrave(Arc<Pool>);

impl Grave for PoolGrave {
    fn is_idle(&self) -> bool {
        self.0.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        0
    }
}
