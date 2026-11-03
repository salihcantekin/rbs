//! macOS: `THREAD_TIME_CONSTRAINT_POLICY` for periodic threads (the class Core Audio
//! and display-linked renderers use), QoS user-interactive for responsive ones. Timers
//! on macOS are already precise; nothing to change there.

use std::time::Duration;

use rbs_core::thread::{ThreadClass, ThreadGrant, ThreadGuard, ThreadPolicyError, ThreadRequest};

use crate::OsThreadConfig;

/// `mach_timebase_info_data_t` from <mach/mach_time.h> (deprecated in `libc`, which
/// points to `mach2`; the layout is two `u32`s).
#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

// Not in `libc` (or deprecated there): <pthread/qos.h> and <mach/mach_time.h>, both
// part of libSystem.
unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    fn pthread_set_qos_class_self_np(qos_class: u32, relative_priority: libc::c_int)
    -> libc::c_int;
    fn pthread_get_qos_class_np(
        thread: libc::pthread_t,
        qos_class: *mut u32,
        relative_priority: *mut libc::c_int,
    ) -> libc::c_int;
}

/// `QOS_CLASS_USER_INTERACTIVE` from <sys/qos.h>.
const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
/// `QOS_CLASS_DEFAULT`.
const QOS_CLASS_DEFAULT: u32 = 0x15;
/// `QOS_CLASS_UNSPECIFIED`: not accepted by `pthread_set_qos_class_self_np`.
const QOS_CLASS_UNSPECIFIED: u32 = 0;

/// The shortest computation Apple silicon accepts for a time-constraint thread.
const MIN_COMPUTATION: Duration = Duration::from_micros(50);
/// The longest computation the kernel accepts.
const MAX_COMPUTATION: Duration = Duration::from_millis(50);

pub(crate) fn enter(
    config: &OsThreadConfig,
    request: &ThreadRequest<'_>,
) -> Result<ThreadGuard, ThreadPolicyError> {
    match request.class {
        // Like the other platforms, a refused real-time class falls back to a raised one.
        ThreadClass::Periodic { period, .. } if config.realtime && !period.is_zero() => {
            time_constraint(period).or_else(|_| user_interactive())
        }
        _ => user_interactive(),
    }
}

/// Mach absolute time units in `d`, saturating.
fn to_abs(d: Duration) -> u32 {
    let mut base = Timebase { numer: 0, denom: 0 };
    // SAFETY: `base` is an initialised `#[repr(C)]` struct with the C layout that
    // outlives the call; the call only writes it.
    let rc = unsafe { mach_timebase_info(&mut base) };
    if rc != libc::KERN_SUCCESS || base.numer == 0 {
        return u32::MAX;
    }
    let abs = d.as_nanos() * u128::from(base.denom) / u128::from(base.numer);
    u32::try_from(abs).unwrap_or(u32::MAX)
}

fn time_constraint(period: Duration) -> Result<ThreadGuard, ThreadPolicyError> {
    let computation = (period / 2).clamp(MIN_COMPUTATION, MAX_COMPUTATION);
    let mut policy = libc::thread_time_constraint_policy {
        period: to_abs(period),
        computation: to_abs(computation),
        constraint: to_abs(period.max(computation)),
        preemptible: 1,
    };
    // SAFETY: `pthread_self()` is the calling thread; the port it maps to stays valid
    // while the thread lives.
    let thread = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
    // SAFETY: `policy` is an initialised `thread_time_constraint_policy` that outlives
    // the call, and the count is its size in `integer_t`s, as the flavour requires.
    let rc = unsafe {
        libc::thread_policy_set(
            thread,
            libc::THREAD_TIME_CONSTRAINT_POLICY as libc::thread_policy_flavor_t,
            (&mut policy as *mut libc::thread_time_constraint_policy).cast(),
            libc::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    if rc != libc::KERN_SUCCESS {
        return Err(ThreadPolicyError::Os(rc));
    }
    Ok(ThreadGuard::restoring(ThreadGrant::RealTime, move || {
        // The standard policy takes no data (count 0); the pointer is not read.
        let mut none: libc::integer_t = 0;
        // SAFETY: runs on the same thread (the guard is not `Send`), so `thread` is still
        // its port; `none` outlives the call and is not read with count 0.
        let _ = unsafe {
            libc::thread_policy_set(
                thread,
                libc::THREAD_STANDARD_POLICY as libc::thread_policy_flavor_t,
                &mut none,
                libc::THREAD_STANDARD_POLICY_COUNT as libc::mach_msg_type_number_t,
            )
        };
    }))
}

fn user_interactive() -> Result<ThreadGuard, ThreadPolicyError> {
    let (mut old, mut relative) = (0u32, 0 as libc::c_int);
    // SAFETY: `pthread_self()` is the calling thread; both out pointers are initialised
    // locals that outlive the call.
    let rc = unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut old, &mut relative) };
    if rc != 0 {
        return Err(ThreadPolicyError::Os(rc));
    }
    // SAFETY: plain value arguments; changes only the calling thread.
    let rc = unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    if rc != 0 {
        return Err(ThreadPolicyError::Os(rc));
    }
    // A thread that never had a class reports "unspecified", which cannot be set back;
    // the default class is what it ran with.
    let old = if old == QOS_CLASS_UNSPECIFIED {
        QOS_CLASS_DEFAULT
    } else {
        old
    };
    Ok(ThreadGuard::restoring(ThreadGrant::Raised, move || {
        // SAFETY: as above, on the same thread.
        let _ = unsafe { pthread_set_qos_class_self_np(old, relative) };
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use super::*;

    fn qos() -> u32 {
        let (mut class, mut relative) = (0u32, 0 as libc::c_int);
        // SAFETY: as in `user_interactive`.
        let rc =
            unsafe { pthread_get_qos_class_np(libc::pthread_self(), &mut class, &mut relative) };
        assert_eq!(rc, 0);
        class
    }

    #[test]
    fn a_responsive_guard_raises_and_restores_the_qos_class() {
        std::thread::spawn(|| {
            let before = qos();
            let guard = enter(
                &OsThreadConfig::default(),
                &ThreadRequest::new("t", ThreadClass::Responsive),
            )
            .unwrap();
            assert_eq!(qos(), QOS_CLASS_USER_INTERACTIVE);
            drop(guard);
            let after = qos();
            assert!(
                after == before || (before == QOS_CLASS_UNSPECIFIED && after == QOS_CLASS_DEFAULT),
                "{before:#x} -> {after:#x}"
            );
        })
        .join()
        .unwrap();
    }
}
