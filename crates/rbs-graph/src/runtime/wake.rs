//! The control thread a bypass unparks (ADR-0154).

use std::sync::atomic::{Ordering, fence};
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

/// Shared with every executor. The sleeper binds itself; a bypass unparks it.
///
/// The read after [`ControlWake::bind`] is wait-free. The worker never sets the cell
/// and never takes a lock.
#[derive(Debug)]
pub struct ControlWake {
    thread: OnceLock<Thread>,
}

impl ControlWake {
    /// A cell no thread has bound yet.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            thread: OnceLock::new(),
        })
    }

    /// The thread that will sleep binds itself, then fences before any load that
    /// decides whether to sleep (ADR-0154 decisions 1 and 2). Once only: a later bind
    /// does not replace the sleeper.
    pub fn bind(&self) {
        let _ = self.thread.set(std::thread::current());
        fence(Ordering::SeqCst);
    }

    /// Unparks the bound thread. No thread bound yet: does nothing. Does not allocate,
    /// lock or block.
    pub fn unpark(&self) {
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
    }
}
