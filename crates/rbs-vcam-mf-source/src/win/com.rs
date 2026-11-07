//! Panic containment and the DLL's object count.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Foundation::E_UNEXPECTED;
use windows_core::Result;

/// Runs a COM entry point's body; a panic becomes `E_UNEXPECTED` instead of unwinding
/// into Frame Server (ADR-0066 decision 9, "Containment"). Every method of every object
/// this DLL hands out goes through it.
pub(crate) fn guard<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| Err(E_UNEXPECTED.into()))
}

/// Objects alive; `DllCanUnloadNow` answers from it and [`LOCKS`].
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// Server locks (`IClassFactory::LockServer`), counted apart so an unbalanced unlock
/// never takes a live object's count.
static LOCKS: AtomicUsize = AtomicUsize::new(0);

/// Counts one live object until dropped.
#[derive(Debug)]
pub(crate) struct Alive(());

impl Alive {
    pub(crate) fn new() -> Self {
        LIVE.fetch_add(1, Ordering::AcqRel);
        Self(())
    }
}

impl Drop for Alive {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Adds or removes a server lock (`IClassFactory::LockServer`).
pub(crate) fn lock_server(lock: bool) {
    if lock {
        LOCKS.fetch_add(1, Ordering::AcqRel);
    } else {
        // Never below zero, whatever a caller does.
        let mut n = LOCKS.load(Ordering::Acquire);
        while let Some(less) = n.checked_sub(1) {
            match LOCKS.compare_exchange_weak(n, less, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => n = now,
            }
        }
    }
}

/// Whether nothing of this DLL is in use.
pub(crate) fn idle() -> bool {
    LIVE.load(Ordering::Acquire) == 0 && LOCKS.load(Ordering::Acquire) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_an_hresult() {
        let r: Result<()> = guard(|| panic!("injected"));
        assert_eq!(r.err().map(|e| e.code()), Some(E_UNEXPECTED));
        assert_eq!(guard(|| Ok(7)).ok(), Some(7));
    }
}
