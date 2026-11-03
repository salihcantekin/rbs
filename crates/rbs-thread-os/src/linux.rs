//! Linux: `SCHED_FIFO` when the process may (`CAP_SYS_NICE` or `RLIMIT_RTPRIO`), else
//! rtkit, else a lower nice value; per-thread timer slack of 1 ns so `park_timeout`
//! wakes on time.

use rbs_core::thread::{ThreadClass, ThreadGrant, ThreadGuard, ThreadPolicyError, ThreadRequest};

use crate::OsThreadConfig;

type Undo = Box<dyn FnOnce()>;

pub(crate) fn enter(
    config: &OsThreadConfig,
    request: &ThreadRequest<'_>,
) -> Result<ThreadGuard, ThreadPolicyError> {
    let mut undo: Vec<Undo> = Vec::new();
    let mut grant = ThreadGrant::Unchanged;
    if let Some(restore) = timer_slack(1) {
        undo.push(restore);
        grant = ThreadGrant::Raised;
    }
    let periodic = matches!(request.class, ThreadClass::Periodic { .. });
    let mut realtime = None;
    if periodic && config.realtime {
        let priority = i32::from(config.linux_rt_priority.clamp(1, 99));
        realtime = sched_fifo(priority).or_else(|| rtkit(priority));
    }
    match realtime {
        Some(restore) => {
            undo.push(restore);
            grant = ThreadGrant::RealTime;
        }
        None => {
            if let Some(restore) = nice(if periodic { -10 } else { -5 }) {
                undo.push(restore);
                grant = ThreadGrant::Raised;
            }
        }
    }
    if grant == ThreadGrant::Unchanged {
        return Err(ThreadPolicyError::Denied(
            "no real-time class, nice value or timer slack allowed",
        ));
    }
    Ok(ThreadGuard::restoring(grant, move || {
        for undo in undo.into_iter().rev() {
            undo();
        }
    }))
}

/// Sets this thread's timer slack in ns; returns how to restore the old one.
fn timer_slack(ns: libc::c_ulong) -> Option<Undo> {
    const ZERO: libc::c_ulong = 0;
    // SAFETY: PR_GET_TIMERSLACK takes no pointers and only reads the calling thread's
    // slack; the unused arguments are zero `unsigned long`s as the man page asks.
    let old = unsafe { libc::prctl(libc::PR_GET_TIMERSLACK, ZERO, ZERO, ZERO, ZERO) };
    if old < 0 {
        return None;
    }
    // SAFETY: PR_SET_TIMERSLACK takes the slack by value and only affects the calling
    // thread.
    let set = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, ns, ZERO, ZERO, ZERO) };
    if set != 0 {
        return None;
    }
    // `old` is non-negative here, so the cast keeps its value.
    let old = old as libc::c_ulong;
    Some(Box::new(move || {
        // SAFETY: as above; runs on the same thread (the guard is not `Send`).
        let _ = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, old, ZERO, ZERO, ZERO) };
    }))
}

/// The calling thread's scheduling policy and parameters.
fn sched_get() -> Option<(libc::c_int, libc::sched_param)> {
    let mut policy: libc::c_int = 0;
    let mut param = libc::sched_param { sched_priority: 0 };
    // SAFETY: `pthread_self()` is the calling thread, alive for the call; both out
    // pointers point to initialised locals that outlive the call.
    let rc = unsafe { libc::pthread_getschedparam(libc::pthread_self(), &mut policy, &mut param) };
    (rc == 0).then_some((policy, param))
}

/// Sets the calling thread's scheduling policy and priority.
fn sched_set(policy: libc::c_int, priority: libc::c_int) -> bool {
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // SAFETY: `pthread_self()` is the calling thread; `param` is a valid, initialised
    // struct that outlives the call and is only read.
    unsafe { libc::pthread_setschedparam(libc::pthread_self(), policy, &param) == 0 }
}

/// A restore that puts back `old` (lowering a class needs no privilege).
fn restore_sched(old: (libc::c_int, libc::sched_param)) -> Undo {
    Box::new(move || {
        let _ = sched_set(old.0, old.1.sched_priority);
    })
}

fn sched_fifo(priority: libc::c_int) -> Option<Undo> {
    let old = sched_get()?;
    sched_set(libc::SCHED_FIFO, priority).then(|| restore_sched(old))
}

/// The kernel id of the calling thread.
fn tid() -> libc::pid_t {
    // SAFETY: `gettid` takes no arguments and cannot fail.
    unsafe { libc::gettid() }
}

/// Lowers this thread's nice value to `value`; returns how to restore the old one.
fn nice(value: libc::c_int) -> Option<Undo> {
    let tid = tid();
    // On Linux `PRIO_PROCESS` with a thread id applies to that thread only.
    // `getpriority` may return -1 as a value, so errno tells failures apart.
    // SAFETY: `__errno_location` returns the calling thread's errno slot, valid for the
    // thread's lifetime.
    unsafe { *libc::__errno_location() = 0 };
    // SAFETY: plain value arguments; reads the priority of this process's own thread.
    let old = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid as libc::id_t) };
    // SAFETY: as above.
    if old == -1 && unsafe { *libc::__errno_location() } != 0 {
        return None;
    }
    if old <= value {
        return None;
    }
    // SAFETY: plain value arguments; changes only this process's own thread.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, value) } != 0 {
        return None;
    }
    Some(Box::new(move || {
        // SAFETY: as above; raising the nice value back needs no privilege.
        let _ = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, old) };
    }))
}

/// The longest an rtkit call may take before the worker gives up on it.
#[cfg(feature = "rtkit")]
const RTKIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// One system-bus connection for the process, made on first use (`None` if there is no
/// bus); every call on it is bounded by `RTKIT_TIMEOUT`.
#[cfg(feature = "rtkit")]
fn system_bus() -> Option<&'static zbus::blocking::Connection> {
    static BUS: std::sync::OnceLock<Option<zbus::blocking::Connection>> =
        std::sync::OnceLock::new();
    BUS.get_or_init(|| {
        zbus::blocking::connection::Builder::system()
            .ok()?
            .method_timeout(RTKIT_TIMEOUT)
            .build()
            .ok()
    })
    .as_ref()
}

/// Asks rtkit to make the calling thread `SCHED_RR` (feature `rtkit`). rtkit only grants
/// threads of a process with `RLIMIT_RTTIME` set, so this lowers it to rtkit's maximum,
/// process-wide and for good, once rtkit answered: the soft limit is half the hard one,
/// so an overrun first gets `SIGXCPU` (whose default action still ends the process; the
/// composition root that enables this feature must handle it) before `SIGKILL`.
#[cfg(feature = "rtkit")]
fn rtkit(priority: libc::c_int) -> Option<Undo> {
    use zbus::blocking::Proxy;

    let old = sched_get()?;
    let connection = system_bus()?;
    let kit = Proxy::new(
        connection,
        "org.freedesktop.RealtimeKit1",
        "/org/freedesktop/RealtimeKit1",
        "org.freedesktop.RealtimeKit1",
    )
    .ok()?;
    let max_priority: i32 = kit.get_property("MaxRealtimePriority").ok()?;
    let max_rttime: i64 = kit.get_property("RTTimeUSecMax").ok()?;
    let max_rttime = libc::rlim_t::try_from(max_rttime).ok()?;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is an initialised local that outlives the call.
    if unsafe { libc::getrlimit(libc::RLIMIT_RTTIME, &mut limit) } != 0 {
        return None;
    }
    if limit.rlim_max == libc::RLIM_INFINITY || limit.rlim_max > max_rttime {
        let lowered = libc::rlimit {
            rlim_cur: max_rttime / 2,
            rlim_max: max_rttime,
        };
        // SAFETY: `lowered` is a valid struct that outlives the call; lowering a limit
        // needs no privilege.
        if unsafe { libc::setrlimit(libc::RLIMIT_RTTIME, &lowered) } != 0 {
            return None;
        }
    }
    let priority = u32::try_from(priority.min(max_priority).max(1)).ok()?;
    let thread = u64::try_from(tid()).ok()?;
    kit.call_method("MakeThreadRealtime", &(thread, priority))
        .ok()?;
    Some(restore_sched(old))
}

#[cfg(not(feature = "rtkit"))]
fn rtkit(_: libc::c_int) -> Option<Undo> {
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use super::*;

    fn slack() -> libc::c_int {
        const ZERO: libc::c_ulong = 0;
        // SAFETY: as in `timer_slack`.
        unsafe { libc::prctl(libc::PR_GET_TIMERSLACK, ZERO, ZERO, ZERO, ZERO) }
    }

    #[test]
    fn the_guard_restores_slack_policy_and_nice_value() {
        std::thread::spawn(|| {
            let (slack_before, sched_before) = (slack(), sched_get().unwrap());
            // SAFETY: plain value arguments; reads this thread's own nice value.
            let nice_before = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid() as libc::id_t) };
            let guard = enter(
                &OsThreadConfig::default(),
                &ThreadRequest::new(
                    "t",
                    ThreadClass::periodic(std::time::Duration::from_millis(16)),
                ),
            )
            .unwrap();
            // Real-time threads have no slack at all on recent kernels.
            assert!(slack() <= 1, "{}", slack());
            drop(guard);
            assert_eq!(slack(), slack_before);
            let sched_after = sched_get().unwrap();
            assert_eq!(sched_after.0, sched_before.0);
            assert_eq!(sched_after.1.sched_priority, sched_before.1.sched_priority);
            // SAFETY: as above.
            let nice_after = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid() as libc::id_t) };
            assert_eq!(nice_after, nice_before);
        })
        .join()
        .unwrap();
    }
}
