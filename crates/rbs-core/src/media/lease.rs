//! Lifetime of pooled or producer-owned media memory.
//!
//! This is a marked low-level module (principle P11): it declares the `unsafe` lease
//! contract that raw native handles rely on.
//!
//! A [`Lease`] tells the owner of a buffer (a texture pool, a capture adapter's
//! surface ring, an audio buffer pool) when every consumer is done with it. The owner
//! keeps a reference count per slot; cloning a lease adds a reference, dropping one
//! removes it. Neither allocates. See `docs/specs/media-types.md#lifetime`.

use std::fmt;
use std::sync::Arc;

/// The owner side of a lease, implemented by pools and adapters.
///
/// Both methods are called from real-time threads (render loop, encoder workers,
/// audio thread). They must not allocate, block, take contended locks, log or panic
/// (a panic during unwinding aborts the process). A typical implementation is an
/// atomic counter per slot plus a lock-free free list. Tokens should carry a
/// generation in their upper bits so a stale release can be detected and ignored.
///
/// # Safety
///
/// Implementers guarantee that the memory or surface identified by a token stays valid
/// and is not written by the producer while its count of references is above zero,
/// and that `retain` and `release` keep that count exact. Frames built with
/// `VideoFrame::native` rely on this for the validity of raw handles.
///
/// The engine keeps its own `Arc` to every owner until the producer has been shut down
/// on a background thread, so the last `Arc` is never dropped on a real-time thread.
pub unsafe trait LeaseOwner: Send + Sync + 'static {
    /// Adds one reference to the slot identified by `token`.
    fn retain(&self, token: u64);

    /// Removes one reference. When the count reaches zero the slot may be reused or the
    /// surface returned to its producer.
    fn release(&self, token: u64);
}

/// One reference to a slot of a [`LeaseOwner`]. Dropping it releases the reference.
pub struct Lease {
    owner: Arc<dyn LeaseOwner>,
    token: u64,
}

impl Lease {
    /// Wraps a reference the owner has already counted for `token`.
    ///
    /// The owner creates leases; `owner` is typically an `Arc` the pool keeps and
    /// clones, so creating a lease does not allocate.
    ///
    /// # Safety
    ///
    /// The caller must hold exactly one counted reference to `token` in `owner` and
    /// hand it over to the lease, which releases it on drop.
    pub unsafe fn adopt(owner: Arc<dyn LeaseOwner>, token: u64) -> Self {
        Self { owner, token }
    }

    /// The owner-defined slot token.
    pub fn token(&self) -> u64 {
        self.token
    }
}

impl Clone for Lease {
    fn clone(&self) -> Self {
        self.owner.retain(self.token);
        Self {
            owner: Arc::clone(&self.owner),
            token: self.token,
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.owner.release(self.token);
    }
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease").field("token", &self.token).finish()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// A pool with per-slot counters, used by tests in this crate.
    #[derive(Default)]
    pub(crate) struct CountingOwner {
        pub(crate) counts: [AtomicI64; 4],
    }

    // SAFETY: counts are exact atomics; test slots own no memory.
    unsafe impl LeaseOwner for CountingOwner {
        fn retain(&self, token: u64) {
            self.counts[token as usize].fetch_add(1, Ordering::Relaxed);
        }
        fn release(&self, token: u64) {
            self.counts[token as usize].fetch_sub(1, Ordering::Relaxed);
        }
    }

    impl CountingOwner {
        pub(crate) fn lease(self: &Arc<Self>, token: u64) -> Lease {
            self.retain(token);
            // SAFETY: the reference counted just above is handed to the lease.
            unsafe { Lease::adopt(Arc::clone(self) as Arc<dyn LeaseOwner>, token) }
        }

        pub(crate) fn count(&self, token: u64) -> i64 {
            self.counts[token as usize].load(Ordering::Relaxed)
        }
    }

    #[test]
    fn clone_and_drop_balance() {
        let owner = Arc::new(CountingOwner::default());
        let a = owner.lease(1);
        assert_eq!(owner.count(1), 1);
        let b = a.clone();
        let c = b.clone();
        assert_eq!(owner.count(1), 3);
        assert_eq!(c.token(), 1);
        drop(a);
        drop(b);
        assert_eq!(owner.count(1), 1);
        drop(c);
        assert_eq!(owner.count(1), 0);
        assert_eq!(owner.count(0), 0);
    }
}
