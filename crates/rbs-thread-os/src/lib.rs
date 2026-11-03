//! OS adapter for the thread policy port (`rbs_core::thread`, ADR-0030).
//!
//! [`OsThreadPolicy`] maps the engine's thread classes to the platform:
//!
//! | Class | Windows | macOS | Linux |
//! | --- | --- | --- | --- |
//! | `Periodic { period }` | MMCSS task "Games" at high priority, 1 ms timer resolution | `THREAD_TIME_CONSTRAINT_POLICY` (computation half the period, constraint the period) | `SCHED_FIFO`, else rtkit (`SCHED_RR`), else a lower nice value; 1 ns timer slack |
//! | `Responsive` | `THREAD_PRIORITY_ABOVE_NORMAL`, 1 ms timer resolution | QoS class user-interactive | a lower nice value if allowed; 1 ns timer slack |
//!
//! Every change is undone by the returned guard on the same thread. When the platform
//! refuses everything, `enter` returns an error and the thread runs at its default
//! class. The composition root (`rbs-app`, `rbs-cli`) hands the policy to
//! `rbs_engine::Engine::set_thread_policy`; nothing else names this crate.
//!
//! Unsafe code: every OS call sits in this crate's per-OS module with a `// SAFETY:`
//! comment.

use rbs_core::thread::{ThreadGuard, ThreadPolicy, ThreadPolicyError, ThreadRequest};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// Settings of [`OsThreadPolicy`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsThreadConfig {
    /// Ask for real-time classes for periodic threads. Off: only raise priorities and
    /// timer resolution.
    pub realtime: bool,
    /// Linux `SCHED_FIFO` / rtkit priority (1..=99; rtkit caps it at its own maximum).
    /// Low on purpose: audio servers and the kernel's own threads stay above.
    pub linux_rt_priority: u8,
}

impl Default for OsThreadConfig {
    fn default() -> Self {
        Self {
            realtime: true,
            linux_rt_priority: 10,
        }
    }
}

/// The platform's thread policy.
#[derive(Debug, Clone, Default)]
pub struct OsThreadPolicy {
    config: OsThreadConfig,
}

impl OsThreadPolicy {
    /// The policy with [`OsThreadConfig::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The policy with `config`.
    pub fn with_config(config: OsThreadConfig) -> Self {
        Self { config }
    }

    /// The settings.
    pub fn config(&self) -> OsThreadConfig {
        self.config
    }
}

impl ThreadPolicy for OsThreadPolicy {
    fn enter(&self, request: &ThreadRequest<'_>) -> Result<ThreadGuard, ThreadPolicyError> {
        #[cfg(target_os = "linux")]
        return linux::enter(&self.config, request);
        #[cfg(target_os = "macos")]
        return macos::enter(&self.config, request);
        #[cfg(windows)]
        return windows::enter(&self.config, request);
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let _ = request;
            Err(ThreadPolicyError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use std::time::Duration;

    use rbs_core::thread::{ThreadClass, ThreadGrant};

    use super::*;

    /// Runs `enter` and the guard's drop on a fresh thread, as a worker does. CI
    /// runners usually may not take a real-time class; then the policy still raises
    /// what it can or reports an error, and must never panic.
    fn enter_on_new_thread(class: ThreadClass) -> Result<ThreadGrant, ThreadPolicyError> {
        std::thread::Builder::new()
            .name("rbs-thread-os-test".into())
            .spawn(move || {
                let policy = OsThreadPolicy::new();
                let guard = policy.enter(&ThreadRequest::new("rbs-thread-os-test", class))?;
                let grant = guard.grant();
                drop(guard);
                Ok(grant)
            })
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn periodic_and_responsive_requests_are_granted_or_refused_cleanly() {
        let periodic = enter_on_new_thread(ThreadClass::periodic(Duration::from_nanos(16_666_667)));
        let responsive = enter_on_new_thread(ThreadClass::Responsive);
        // On Windows and macOS these calls need no privilege.
        #[cfg(any(windows, target_os = "macos"))]
        {
            assert!(periodic.is_ok(), "{periodic:?}");
            assert!(responsive.is_ok(), "{responsive:?}");
        }
        // On Linux the timer slack alone always succeeds, so the thread is at least
        // raised.
        #[cfg(target_os = "linux")]
        {
            assert!(
                matches!(periodic, Ok(ThreadGrant::Raised | ThreadGrant::RealTime)),
                "{periodic:?}"
            );
            assert!(
                matches!(responsive, Ok(ThreadGrant::Raised)),
                "{responsive:?}"
            );
        }
        let _ = (periodic, responsive);
    }
}
