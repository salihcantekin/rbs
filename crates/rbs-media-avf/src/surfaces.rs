//! Native pictures: the decoder's `IOSurface`s handed out zero-copy
//! (ADR-0108 decision 4, capture port native surface rules).
//!
//! - Each picture's `CVPixelBuffer` is kept in a slot of [`Pool`] while a lease names
//!   the slot, so VideoToolbox does not reuse its `IOSurface` while the importer reads
//!   it. Slots are counted by `rbs_capture::handoff::Ring`: the decoder claims a slot
//!   (reference 1) and hands that reference to the frame's lease; the importer and the
//!   picture node release it. Neither `retain` nor `release` locks or frees.
//! - Buffers of free slots are dropped on the worker thread when it decodes the next
//!   picture. No handle grace is needed: the Metal importer's texture retains the
//!   `IOSurface` while the importer caches it (`rbs-gpu` `interop/metal.rs`), so its
//!   address cannot name another surface while it is a cache key.
//! - When the decoder is dropped, the pool goes to a graveyard: kept while any lease
//!   refers to a slot, then freed on the janitor thread, never on the render thread.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetIOSurface};
use objc2_io_surface::IOSurfaceRef;
use rbs_capture::handoff::{Grave, Graveyard, Ring};
use rbs_core::media::{Lease, LeaseOwner};

/// How often the graveyard's janitor checks stopped decoders.
const JANITOR_PERIOD: Duration = Duration::from_secs(1);

/// Slots of one decoder: more than the media source's surface budget (18), so a
/// claim fails only when consumers keep more pictures than the source allows.
pub(crate) const SLOTS: usize = 24;

/// Pools of dropped decoders, released once no lease names a slot (no grace: see the
/// module doc).
static GRAVEYARD: Graveyard =
    Graveyard::new("rbs-media-avf-janitor", Duration::ZERO, JANITOR_PERIOD);

/// A decoded pixel buffer moved between the worker and the janitor.
struct SendableBuffer {
    _buffer: CFRetained<CVPixelBuffer>,
}

// SAFETY: `CVPixelBuffer` retain and release are thread-safe, and the adapter reads a
// buffer only on the worker; the janitor only releases it.
unsafe impl Send for SendableBuffer {}

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
    buffers: Mutex<Vec<Option<SendableBuffer>>>,
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
}

impl Surfaces {
    pub(crate) fn new() -> Self {
        let pool = Arc::new(Pool {
            ring: Ring::new(&[0; SLOTS]),
            buffers: Mutex::new((0..SLOTS).map(|_| None).collect()),
        });
        let owner: Arc<dyn LeaseOwner> = Arc::clone(&pool) as Arc<dyn LeaseOwner>;
        Self { pool, owner }
    }

    /// Drops the buffers of slots nobody references any more, so VideoToolbox can
    /// reuse their surfaces. Worker thread.
    fn recycle(&mut self) {
        let mut buffers = lock(&self.pool.buffers);
        for (slot, entry) in buffers.iter_mut().enumerate() {
            if entry.is_some() && self.pool.ring.is_free(slot) {
                *entry = None;
            }
        }
    }

    /// Keeps `buffer` in a free slot and returns its surface and the lease that names
    /// the slot, or `None` when every slot is referenced (the caller skips the
    /// picture).
    pub(crate) fn hand_out(
        &mut self,
        buffer: CFRetained<CVPixelBuffer>,
    ) -> Option<(CFRetained<IOSurfaceRef>, Lease)> {
        self.recycle();
        let surface = CVPixelBufferGetIOSurface(Some(&buffer))?;
        let slot = self.pool.ring.claim()?;
        if let Some(entry) = lock(&self.pool.buffers).get_mut(slot) {
            *entry = Some(SendableBuffer { _buffer: buffer });
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
        self.recycle();
        GRAVEYARD.bury(Box::new(PoolGrave(Arc::clone(&self.pool))));
    }
}

/// The pool of a dropped decoder. Kept while pictures reference it, so the last lease
/// never frees anything on the render thread.
struct PoolGrave(Arc<Pool>);

impl Grave for PoolGrave {
    fn is_idle(&self) -> bool {
        self.0.ring.is_idle()
    }
    fn bytes(&self) -> u64 {
        0
    }
}
