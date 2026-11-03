//! The thread policy port (ADR-0030): how the engine asks the platform for a
//! scheduling class on its worker threads, without naming an OS API.
//!
//! The engine describes what a thread does ([`ThreadClass`]); an adapter maps that to
//! the platform (MMCSS and timer resolution on Windows, time-constraint policy on
//! macOS, `SCHED_FIFO` or rtkit and timer slack on Linux). The policy is called on the
//! thread itself before its first tick and undone through the returned [`ThreadGuard`]
//! on the same thread after its last one. Without a policy nothing is called and
//! threads run at the platform's default class (ADR-0013).

use std::fmt;
use std::marker::PhantomData;
use std::time::Duration;

/// What a thread does, so that a policy can pick its class.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadClass {
    /// Work once per `period` that must finish well inside it: the render executor and
    /// encoders at frame cadence. Build it with [`ThreadClass::periodic`]; more fields
    /// (a compute budget) may be added.
    #[non_exhaustive]
    Periodic {
        /// The time between two runs.
        period: Duration,
    },
    /// Event-driven work that should run soon after a wake-up but has no deadline per
    /// period: outputs, muxers, network sends.
    Responsive,
}

impl ThreadClass {
    /// Periodic work once per `period`.
    pub const fn periodic(period: Duration) -> Self {
        ThreadClass::Periodic { period }
    }
}

/// A request for one thread, made on that thread.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadRequest<'a> {
    /// The thread's name (for logs and diagnostics).
    pub name: &'a str,
    /// What the thread does.
    pub class: ThreadClass,
}

impl<'a> ThreadRequest<'a> {
    /// A request for the calling thread.
    pub const fn new(name: &'a str, class: ThreadClass) -> Self {
        Self { name, class }
    }
}

/// What a policy obtained for a thread.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadGrant {
    /// Nothing changed; the thread runs at the default class.
    Unchanged,
    /// A higher priority or a better timer, but no real-time class.
    Raised,
    /// A real-time or time-constrained class.
    RealTime,
}

/// Why a policy could not raise a thread. The thread keeps running at its default
/// class either way.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadPolicyError {
    /// The platform refused (missing privilege, a limit, a service that said no).
    Denied(&'static str),
    /// The policy has nothing for this platform or class.
    Unsupported,
    /// An OS call failed with this code.
    Os(i32),
}

impl fmt::Display for ThreadPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThreadPolicyError::Denied(why) => write!(f, "thread class denied: {why}"),
            ThreadPolicyError::Unsupported => f.write_str("thread class not supported here"),
            ThreadPolicyError::Os(code) => write!(f, "thread class: OS error {code}"),
        }
    }
}

impl std::error::Error for ThreadPolicyError {}

/// Undoes what a policy did for a thread when dropped. Must be dropped on the thread
/// it was made for; it is neither `Send` nor `Sync`, so it cannot leave that thread:
///
/// ```compile_fail
/// use rbs_core::thread::{ThreadGrant, ThreadGuard};
/// fn send<T: Send>(_: T) {}
/// send(ThreadGuard::new(ThreadGrant::Unchanged));
/// ```
#[must_use = "dropping the guard undoes the thread's class at once"]
pub struct ThreadGuard {
    grant: ThreadGrant,
    restore: Option<Box<dyn FnOnce()>>,
    /// Keeps the guard `!Send` and `!Sync` whatever `restore` holds.
    _thread_bound: PhantomData<*const ()>,
}

impl ThreadGuard {
    /// A guard that undoes nothing (the policy changed nothing, or nothing needs undoing).
    pub fn new(grant: ThreadGrant) -> Self {
        Self {
            grant,
            restore: None,
            _thread_bound: PhantomData,
        }
    }

    /// A guard that calls `restore` when dropped, on the same thread.
    pub fn restoring(grant: ThreadGrant, restore: impl FnOnce() + 'static) -> Self {
        Self {
            grant,
            restore: Some(Box::new(restore)),
            _thread_bound: PhantomData,
        }
    }

    /// What the policy obtained.
    pub fn grant(&self) -> ThreadGrant {
        self.grant
    }
}

impl fmt::Debug for ThreadGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadGuard")
            .field("grant", &self.grant)
            .field("restores", &self.restore.is_some())
            .finish()
    }
}

impl Drop for ThreadGuard {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

/// Raises the scheduling class of a thread. Implemented by OS adapter crates; the
/// composition root hands one to the engine.
pub trait ThreadPolicy: Send + Sync + 'static {
    /// Called on the thread the request is for, before its real-time work starts. Not
    /// real-time: may allocate and make system calls, but must return within about a
    /// quarter second whatever the system does (bound every IPC call with a timeout): it
    /// delays the thread's first tick, and a call that never returns stalls the engine.
    /// On error the thread runs at its default class. The guard's restore must not panic;
    /// callers catch it anyway.
    fn enter(&self, request: &ThreadRequest<'_>) -> Result<ThreadGuard, ThreadPolicyError>;
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn a_guard_restores_once_on_drop() {
        let restored = Rc::new(Cell::new(0));
        let r = Rc::clone(&restored);
        let guard = ThreadGuard::restoring(ThreadGrant::RealTime, move || r.set(r.get() + 1));
        assert_eq!(guard.grant(), ThreadGrant::RealTime);
        assert_eq!(restored.get(), 0);
        drop(guard);
        assert_eq!(restored.get(), 1);
        drop(ThreadGuard::new(ThreadGrant::Unchanged));
    }
}
