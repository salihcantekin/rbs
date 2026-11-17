//! Waits for submitted GPU work before the process exits (Linux).
//!
//! A [`GpuContext`](crate::GpuContext) that is never dropped (one kept in a `static`, or
//! alive when `main` returns or `std::process::exit` runs) can still have work in flight
//! when the C runtime runs its exit handlers. Mesa's software Vulkan driver (lavapipe)
//! compiles shader variants with LLVM on its own queue thread while it executes a
//! submission, and LLVM's static destructors run among those exit handlers: the queue
//! thread then uses freed LLVM state and the process ends with a segfault or a
//! "double free" after the work it was asked to do succeeded. Seen in about 1 run in
//! 100 of `rbs-gpu` tests, which keep one context per test binary in a `static`.
//!
//! Every live device is registered here, and one `atexit` handler, installed after
//! the first device exists (so after the driver and LLVM registered theirs; exit
//! handlers run in reverse order), waits a bounded time for each registered device's
//! queue. Dropping a context removes its device, so the registry never extends a
//! device's life. Low-level module (P11): the only `unsafe` is the `atexit` call.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, Once, PoisonError};
use std::time::Duration;

/// How long the exit handler waits for each device. A hung or lost device must not
/// hold the process forever; a healthy queue drains in well under this.
const EXIT_WAIT: Duration = Duration::from_secs(5);

static LIVE: Mutex<Vec<(u64, wgpu::Device)>> = Mutex::new(Vec::new());
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
static HANDLER: Once = Once::new();

/// Keeps a device in the exit registry while the owning context lives.
pub(crate) struct ExitDrain {
    id: u64,
}

impl ExitDrain {
    /// Registers `device`; the first call installs the exit handler.
    pub(crate) fn register(device: &wgpu::Device) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        LIVE.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, device.clone()));
        HANDLER.call_once(|| {
            // SAFETY: `drain_at_exit` is an `extern "C" fn()` with no arguments that
            // never unwinds (it catches panics). `atexit` only stores the pointer; a
            // non-zero result means it was not stored, and then exit simply does not
            // wait, which is the behaviour without this module.
            let _ = unsafe { libc::atexit(drain_at_exit) };
        });
        Self { id }
    }
}

impl Drop for ExitDrain {
    fn drop(&mut self) {
        LIVE.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}

extern "C" fn drain_at_exit() {
    // A panic must not unwind into the C runtime; there is nothing to report at exit.
    let _ = std::panic::catch_unwind(|| {
        // Clone the handles so no lock is held while waiting: a context dropped on
        // another thread during exit only blocks for the clone.
        let devices: Vec<wgpu::Device> = LIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(_, device)| device.clone())
            .collect();
        for device in devices {
            // A lost device or a timeout ends the wait; either way the process exits.
            let _ = device.poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: Some(EXIT_WAIT),
            });
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::panic)] // Tests fail by panicking.

    use super::LIVE;
    use crate::{GpuContext, GpuError, GpuRequest};
    use std::sync::PoisonError;

    fn listed(id: u64) -> bool {
        LIVE.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(listed, _)| *listed == id)
    }

    /// The only test in this binary that opens a device (software drivers crash with
    /// several devices created at once).
    #[test]
    fn a_context_is_listed_for_the_exit_wait_until_it_drops() {
        let ctx = match GpuContext::new(&GpuRequest {
            allow_software: true,
            ..GpuRequest::default()
        }) {
            Ok(ctx) => ctx,
            Err(GpuError::NoAdapter) => {
                assert!(
                    std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                    "RBS_REQUIRE_GPU is set but no GPU adapter was found"
                );
                eprintln!("SKIPPED: no GPU adapter");
                return;
            }
            Err(e) => panic!("GPU context failed: {e}"),
        };
        let id = ctx._exit_drain.id;
        assert!(listed(id));
        let sibling = ctx.sibling().expect("sibling");
        assert!(listed(sibling._exit_drain.id));
        drop(sibling);
        assert!(
            listed(id),
            "dropping a sibling keeps the first context listed"
        );
        drop(ctx);
        assert!(!listed(id));
    }
}
