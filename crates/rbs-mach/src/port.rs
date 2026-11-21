//! Port rights, the bootstrap namespace, and sending and receiving messages.

use std::ffi::CStr;
use std::mem::size_of;

use mach2::bootstrap::{BOOTSTRAP_UNKNOWN_SERVICE, bootstrap_check_in, bootstrap_look_up};
use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_port::{
    mach_port_allocate, mach_port_deallocate, mach_port_insert_right, mach_port_mod_refs,
    mach_port_request_notification, mach_port_set_attributes,
};
use mach2::message::{
    MACH_MSG_SUCCESS, MACH_MSG_TYPE_MAKE_SEND, MACH_MSG_TYPE_MAKE_SEND_ONCE, MACH_RCV_MSG,
    MACH_RCV_TIMEOUT, MACH_SEND_INTERRUPTED, MACH_SEND_MSG, MACH_SEND_TIMED_OUT, MACH_SEND_TIMEOUT,
    mach_msg, mach_msg_destroy, mach_msg_header_t, mach_msg_return_t,
};
use mach2::notify::{MACH_NOTIFY_DEAD_NAME, MACH_NOTIFY_NO_SENDERS};
use mach2::port::{
    MACH_PORT_LIMITS_INFO, MACH_PORT_LIMITS_INFO_COUNT, MACH_PORT_NULL, MACH_PORT_QLIMIT_MAX,
    MACH_PORT_RIGHT_RECEIVE, mach_port_limits_t, mach_port_t,
};
use mach2::traps::mach_task_self;

use crate::MachError;

/// Room the kernel may append after a message: the largest trailer (`mach_msg_max_trailer_t`
/// is 68 bytes) rounded up.
pub const TRAILER_ROOM: usize = 128;

/// A receive buffer of `N` bytes, aligned for every message struct.
#[repr(C, align(8))]
pub struct RecvBuf<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> Default for RecvBuf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RecvBuf<N> {
    /// A zeroed buffer.
    pub fn new() -> Self {
        Self { bytes: [0; N] }
    }

    fn header_mut(&mut self) -> *mut mach_msg_header_t {
        self.bytes.as_mut_ptr().cast()
    }

    /// The header of the last received message.
    pub fn header(&self) -> mach_msg_header_t {
        const { assert!(N >= size_of::<mach_msg_header_t>()) };
        // SAFETY: the buffer is 8-byte aligned and at least a header long (checked at
        // compile time); any bit pattern is a valid header (plain integers).
        unsafe { *self.bytes.as_ptr().cast::<mach_msg_header_t>() }
    }

    /// The last received message as `T`, if it is at least as large as `T`.
    ///
    /// `T` must be a `repr(C)` message struct of plain integers (valid for any bit
    /// pattern, alignment at most 8).
    pub fn read<T: Copy>(&self) -> Option<T> {
        let size = self.header().msgh_size as usize;
        if size < size_of::<T>() || size_of::<T>() > N || align_of::<T>() > 8 {
            return None;
        }
        // SAFETY: `T` is a plain-integer message struct (the documented contract),
        // valid for any bit pattern; the buffer is 8-byte aligned and larger than `T`.
        Some(unsafe { std::ptr::read(self.bytes.as_ptr().cast::<T>()) })
    }

    /// The sender's pid from the audit trailer of the last message received with
    /// [`receive_audited`]; `None` when the trailer is missing or short.
    pub fn sender_pid(&self) -> Option<u32> {
        // `mach_msg_audit_trailer_t`: type, size, seqno, sender (2 words), audit token
        // (8 words). The pid is word 5 of the audit token (`audit_token_to_pid`).
        let at = (self.header().msgh_size as usize).next_multiple_of(4);
        let word = |i: usize| -> Option<u32> {
            let start = at.checked_add(4 * i)?;
            let bytes: [u8; 4] = self.bytes.get(start..start + 4)?.try_into().ok()?;
            Some(u32::from_ne_bytes(bytes))
        };
        let trailer_size = word(1)? as usize;
        if trailer_size < 4 * 13 {
            return None;
        }
        word(5 + 5)
    }

    /// Destroys the rights carried by the last received message (unknown or rejected
    /// messages, so a stranger cannot leak port names into this process).
    pub fn destroy(&mut self) {
        // SAFETY: the buffer holds a message received by `receive`; `mach_msg_destroy`
        // releases exactly the rights the kernel moved into this task with it.
        unsafe { mach_msg_destroy(self.header_mut()) }
    }
}

/// Mach header bits: remote and local dispositions.
pub const fn bits(remote: u32, local: u32) -> u32 {
    remote | (local << 8)
}

fn task() -> mach_port_t {
    // SAFETY: reads the task's own port name, set by libSystem before `main`.
    unsafe { mach_task_self() }
}

/// Drops one user reference of a send right or dead name. Errors are ignored: the name
/// was already gone.
pub fn deallocate(name: mach_port_t) {
    if name != MACH_PORT_NULL {
        // SAFETY: `name` is a send right or dead name this task owns one reference of.
        let _ = unsafe { mach_port_deallocate(task(), name) };
    }
}

/// Destroys a receive right owned by this task.
pub fn destroy_receive(name: mach_port_t) {
    if name != MACH_PORT_NULL {
        // SAFETY: `name` is a receive right this task owns; dropping it wakes senders
        // with dead names.
        let _ = unsafe { mach_port_mod_refs(task(), name, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

fn check(kr: i32, what: &'static str) -> Result<(), MachError> {
    if kr == KERN_SUCCESS {
        Ok(())
    } else {
        Err(MachError(what))
    }
}

/// A new receive right with the largest queue limit.
pub fn new_receive() -> Result<mach_port_t, MachError> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: `port` is a valid out pointer for the new name.
    let kr = unsafe { mach_port_allocate(task(), MACH_PORT_RIGHT_RECEIVE, &mut port) };
    check(kr, "mach_port_allocate failed")?;
    raise_qlimit(port);
    Ok(port)
}

/// Raises the queue limit, so releases never fill the queue (best effort).
fn raise_qlimit(port: mach_port_t) {
    let mut limits = mach_port_limits_t {
        mpl_qlimit: MACH_PORT_QLIMIT_MAX,
    };
    // SAFETY: `limits` is the `MACH_PORT_LIMITS_INFO` flavour, `COUNT` words long.
    let _ = unsafe {
        mach_port_set_attributes(
            task(),
            port,
            MACH_PORT_LIMITS_INFO,
            (&raw mut limits).cast(),
            MACH_PORT_LIMITS_INFO_COUNT,
        )
    };
}

/// Registers `name` in the bootstrap namespace and returns its receive right, plus the
/// send right `bootstrap_register` needed (or `MACH_PORT_NULL`).
///
/// `bootstrap_check_in` creates a dynamic service on current macOS. Where it reports an
/// unknown service, the older `bootstrap_register` is used with a port of our own; the
/// returned send right must then be dropped with the receive right.
pub fn check_in(name: &CStr) -> Result<(mach_port_t, mach_port_t), MachError> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: `name` is NUL-terminated; `port` is a valid out pointer. `bootstrap_port`
    // is set by libSystem before `main` and never written afterwards.
    let kr =
        unsafe { bootstrap_check_in(mach2::bootstrap::bootstrap_port, name.as_ptr(), &mut port) };
    if kr == KERN_SUCCESS {
        raise_qlimit(port);
        return Ok((port, MACH_PORT_NULL));
    }
    if kr != BOOTSTRAP_UNKNOWN_SERVICE {
        return Err(MachError("bootstrap_check_in failed"));
    }
    let port = new_receive()?;
    // SAFETY: `port` is our receive right; this adds one send right under the same name.
    let kr = unsafe { mach_port_insert_right(task(), port, port, MACH_MSG_TYPE_MAKE_SEND) };
    if kr != KERN_SUCCESS {
        destroy_receive(port);
        return Err(MachError("mach_port_insert_right failed"));
    }
    // SAFETY: `name` is NUL-terminated and `port` is a send right we own; the call copies it.
    let kr = unsafe {
        #[allow(deprecated)]
        mach2::bootstrap::bootstrap_register(
            mach2::bootstrap::bootstrap_port,
            name.as_ptr().cast_mut(),
            port,
        )
    };
    if kr != KERN_SUCCESS {
        deallocate(port);
        destroy_receive(port);
        return Err(MachError("bootstrap_register failed"));
    }
    Ok((port, port))
}

/// Looks up `name` and returns a send right to it.
pub fn look_up(name: &CStr) -> Result<mach_port_t, MachError> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: as in `check_in`.
    let kr =
        unsafe { bootstrap_look_up(mach2::bootstrap::bootstrap_port, name.as_ptr(), &mut port) };
    if kr == KERN_SUCCESS {
        Ok(port)
    } else {
        Err(MachError("service not found"))
    }
}

/// Asks the kernel to send `MACH_NOTIFY_NO_SENDERS` to `port` once a send right to it
/// has existed and none is left.
pub fn notify_no_senders(port: mach_port_t) -> Result<(), MachError> {
    let mut previous = MACH_PORT_NULL;
    // SAFETY: `port` is a receive right of this task. `sync = 1` arms the notification
    // only after the first send right is made. The kernel makes a send-once right to
    // `port` for it; send-once rights do not count as senders.
    let kr = unsafe {
        mach_port_request_notification(
            task(),
            port,
            MACH_NOTIFY_NO_SENDERS,
            1,
            port,
            MACH_MSG_TYPE_MAKE_SEND_ONCE,
            &mut previous,
        )
    };
    deallocate(previous);
    check(kr, "no-senders notification request failed")
}

/// Fills `buf` with random bytes from the system CSPRNG.
pub fn random_bytes(buf: &mut [u8]) {
    unsafe extern "C" {
        fn arc4random_buf(buf: *mut std::ffi::c_void, nbytes: usize);
    }
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes; `arc4random_buf` (libSystem)
    // writes exactly that many and cannot fail.
    unsafe { arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

/// Asks the kernel to send `MACH_NOTIFY_DEAD_NAME` to `notify` when `name` dies.
pub fn notify_dead_name(name: mach_port_t, notify: mach_port_t) -> Result<(), MachError> {
    let mut previous = MACH_PORT_NULL;
    // SAFETY: `name` is a send right and `notify` a receive right of this task; the kernel
    // makes a send-once right to `notify` for the notification.
    let kr = unsafe {
        mach_port_request_notification(
            task(),
            name,
            MACH_NOTIFY_DEAD_NAME,
            0,
            notify,
            MACH_MSG_TYPE_MAKE_SEND_ONCE,
            &mut previous,
        )
    };
    deallocate(previous);
    check(kr, "dead-name notification request failed")
}

/// Sends `msg`, waiting at most `timeout_ms` (0: never waits).
///
/// When the send times out or is interrupted after the kernel copied the message in, the
/// kernel returns its rights to this task (pseudo-receive); they are destroyed here, so a
/// failed send never leaks references. On other errors the rights were not copied in and
/// stay as they were.
///
/// # Safety
///
/// `msg` must be a `repr(C)` message struct with a filled header whose size is
/// `size_of::<T>()`, and its port names and dispositions must be rights this task holds.
pub unsafe fn send<T>(msg: &mut T, timeout_ms: u32) -> mach_msg_return_t {
    let header: *mut mach_msg_header_t = std::ptr::from_mut(msg).cast();
    // SAFETY: the caller upholds the message contract; the size matches the struct.
    let ret = unsafe {
        mach_msg(
            header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            size_of::<T>() as u32,
            0,
            MACH_PORT_NULL,
            timeout_ms,
            MACH_PORT_NULL,
        )
    };
    if ret == MACH_SEND_TIMED_OUT || ret == MACH_SEND_INTERRUPTED {
        // SAFETY: on these two errors the buffer holds the message with pseudo-received
        // rights: the destination in `msgh_remote_port`, the reply right in
        // `msgh_local_port` and the descriptors. `mach_msg_destroy` releases the
        // destination and the descriptors but not the reply right, which is dropped here.
        unsafe {
            let local = (*header).msgh_local_port;
            let local_bits = ((*header).msgh_bits >> 8) & 0x1f;
            mach_msg_destroy(header);
            if local_bits != 0 {
                deallocate(local);
            }
        }
    }
    ret
}

fn receive_with<const N: usize>(
    port: mach_port_t,
    buf: &mut RecvBuf<N>,
    timeout_ms: u32,
    extra: i32,
) -> mach_msg_return_t {
    // SAFETY: `buf` is `N` writable bytes, aligned for a header; `port` is a receive
    // right of this task. Larger messages are destroyed by the kernel.
    unsafe {
        mach_msg(
            buf.header_mut(),
            MACH_RCV_MSG | MACH_RCV_TIMEOUT | extra,
            0,
            N as u32,
            port,
            timeout_ms,
            MACH_PORT_NULL,
        )
    }
}

/// Receives one message on `port` into `buf`, waiting at most `timeout_ms`.
pub fn receive<const N: usize>(
    port: mach_port_t,
    buf: &mut RecvBuf<N>,
    timeout_ms: u32,
) -> mach_msg_return_t {
    receive_with(port, buf, timeout_ms, 0)
}

/// `MACH_RCV_TRAILER_TYPE(MACH_MSG_TRAILER_FORMAT_0) |
/// MACH_RCV_TRAILER_ELEMENTS(MACH_RCV_TRAILER_AUDIT)` (`<mach/message.h>`).
const RCV_AUDIT_TRAILER: i32 = 3 << 24;

/// As [`receive`], with the audit trailer, so [`RecvBuf::sender_pid`] names the
/// sender. `N` must leave [`TRAILER_ROOM`] after the largest expected message.
pub fn receive_audited<const N: usize>(
    port: mach_port_t,
    buf: &mut RecvBuf<N>,
    timeout_ms: u32,
) -> mach_msg_return_t {
    receive_with(port, buf, timeout_ms, RCV_AUDIT_TRAILER)
}

/// `true` for a successful `mach_msg`.
pub fn ok(ret: mach_msg_return_t) -> bool {
    ret == MACH_MSG_SUCCESS
}

/// Splits a `u64` into two words (low first).
pub fn split(v: u64) -> [u32; 2] {
    [v as u32, (v >> 32) as u32]
}

/// Joins two words (low first).
pub fn join(w: [u32; 2]) -> u64 {
    u64::from(w[0]) | (u64::from(w[1]) << 32)
}

/// The kernel's dead-name and no-senders notification ids.
pub const ID_DEAD_NAME: i32 = MACH_NOTIFY_DEAD_NAME;
/// See [`ID_DEAD_NAME`].
pub const ID_NO_SENDERS: i32 = MACH_NOTIFY_NO_SENDERS;

pub use mach2::message::MACH_MSG_TYPE_MAKE_SEND_ONCE as MAKE_SEND_ONCE;
/// The Mach types the protocols use, re-exported so users need no `mach2` of their own.
pub use mach2::message::{
    MACH_MSG_PORT_DESCRIPTOR, MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND,
    MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS_COMPLEX, MACH_RCV_TIMED_OUT, mach_msg_body_t,
    mach_msg_port_descriptor_t,
};
pub use mach2::message::{MACH_MSG_TYPE_MAKE_SEND as MAKE_SEND, mach_msg_header_t as Header};
pub use mach2::port::{MACH_PORT_NULL as PORT_NULL, mach_port_t as Port};
