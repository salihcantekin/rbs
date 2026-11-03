//! Mach messages and port calls of the preview protocol (spec `gpu-compositor.md`,
//! "Preview export (cross-process)", ADR-0028).
//!
//! Marked low-level module (P11): every `unsafe` block states its invariant. Messages are
//! fixed-size `repr(C)` structs of 32-bit words, so no field needs packing and the layout
//! is the same in every process that speaks version [`VERSION`].

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
    mach_msg, mach_msg_body_t, mach_msg_destroy, mach_msg_header_t, mach_msg_port_descriptor_t,
    mach_msg_return_t,
};
use mach2::notify::{MACH_NOTIFY_DEAD_NAME, MACH_NOTIFY_NO_SENDERS};
use mach2::port::{
    MACH_PORT_LIMITS_INFO, MACH_PORT_LIMITS_INFO_COUNT, MACH_PORT_NULL, MACH_PORT_QLIMIT_MAX,
    MACH_PORT_RIGHT_RECEIVE, mach_port_limits_t, mach_port_t,
};
use mach2::traps::mach_task_self;

use crate::error::GpuError;

/// Protocol version carried by `SUBSCRIBE` and `SURFACES`.
pub(super) const VERSION: u32 = 1;
/// Most surfaces one publisher shares (fixed descriptor count of `SURFACES`).
pub(super) const MAX_SURFACES: usize = super::MAX_PREVIEW_SURFACES;

/// Message ids: "RBP" (RBS preview) and the message number.
pub(super) const ID_SUBSCRIBE: i32 = 0x5242_5001;
pub(super) const ID_SURFACES: i32 = 0x5242_5002;
pub(super) const ID_FRAME: i32 = 0x5242_5003;
pub(super) const ID_RELEASE: i32 = 0x5242_5004;
/// Sent by the kernel to the UI when the engine's service port dies.
pub(super) const ID_DEAD_NAME: i32 = MACH_NOTIFY_DEAD_NAME;
/// Sent by the kernel to the engine's session port when the UI's last send right to it is
/// gone (the UI dropped its subscriber and every frame, or exited).
pub(super) const ID_NO_SENDERS: i32 = MACH_NOTIFY_NO_SENDERS;

/// UI to engine: version, token, and a send right to the UI's port in the header.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Subscribe {
    pub header: mach_msg_header_t,
    pub version: u32,
    pub token: [u32; 2],
}

/// Engine to UI: a send right to the subscription's session port (descriptor 0, where
/// the UI sends its releases), then one copied send right per surface (null for unused
/// descriptors).
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Surfaces {
    pub header: mach_msg_header_t,
    pub body: mach_msg_body_t,
    pub ports: [mach_msg_port_descriptor_t; MAX_SURFACES + 1],
    pub version: u32,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub count: u32,
}

/// Engine to UI: surface `slot` holds frame `sequence`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Frame {
    pub header: mach_msg_header_t,
    pub generation: u32,
    pub slot: u32,
    pub sequence: [u32; 2],
    pub timestamp: [u32; 2],
}

/// UI to engine, on the session port: the UI no longer reads surface `slot`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Release {
    pub header: mach_msg_header_t,
    pub generation: u32,
    pub slot: u32,
}

/// `mach_dead_name_notification_t` without its trailer.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct DeadName {
    pub header: mach_msg_header_t,
    pub ndr: [u8; 8],
    pub name: mach_port_t,
}

/// Room for the largest message plus the largest trailer the kernel may append.
const RECV_SIZE: usize = size_of::<Surfaces>() + 128;

/// A receive buffer, aligned for every message struct.
#[repr(C, align(8))]
pub(super) struct RecvBuf {
    bytes: [u8; RECV_SIZE],
}

impl RecvBuf {
    pub fn new() -> Self {
        Self {
            bytes: [0; RECV_SIZE],
        }
    }

    fn header_mut(&mut self) -> *mut mach_msg_header_t {
        self.bytes.as_mut_ptr().cast()
    }

    /// The header of the last received message.
    pub fn header(&self) -> mach_msg_header_t {
        // SAFETY: the buffer is 8-byte aligned and larger than a header; any bit pattern
        // is a valid header (plain integers).
        unsafe { *self.bytes.as_ptr().cast::<mach_msg_header_t>() }
    }

    /// The last received message as `T`, if it is at least as large as `T`.
    pub fn read<T: Copy>(&self) -> Option<T> {
        let size = self.header().msgh_size as usize;
        if size < size_of::<T>() || size_of::<T>() > RECV_SIZE {
            return None;
        }
        // SAFETY: `T` is one of the `repr(C)` message structs above: plain integers, valid
        // for any bit pattern, alignment at most 4. The buffer is larger than `T`.
        Some(unsafe { std::ptr::read(self.bytes.as_ptr().cast::<T>()) })
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
pub(super) const fn bits(remote: u32, local: u32) -> u32 {
    remote | (local << 8)
}

fn task() -> mach_port_t {
    // SAFETY: reads the task's own port name, set by libSystem before `main`.
    unsafe { mach_task_self() }
}

/// Drops one user reference of a send right or dead name. Errors are ignored: the name
/// was already gone.
pub(super) fn deallocate(name: mach_port_t) {
    if name != MACH_PORT_NULL {
        // SAFETY: `name` is a send right or dead name this task owns one reference of.
        let _ = unsafe { mach_port_deallocate(task(), name) };
    }
}

/// Destroys a receive right owned by this task.
pub(super) fn destroy_receive(name: mach_port_t) {
    if name != MACH_PORT_NULL {
        // SAFETY: `name` is a receive right this task owns; dropping it wakes senders
        // with dead names.
        let _ = unsafe { mach_port_mod_refs(task(), name, MACH_PORT_RIGHT_RECEIVE, -1) };
    }
}

fn check(kr: i32, what: &'static str) -> Result<(), GpuError> {
    if kr == KERN_SUCCESS {
        Ok(())
    } else {
        Err(GpuError::Export(what))
    }
}

/// A new receive right with the largest queue limit.
pub(super) fn new_receive() -> Result<mach_port_t, GpuError> {
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

/// Registers `name` in the bootstrap namespace and returns its receive right.
///
/// `bootstrap_check_in` creates a dynamic service on current macOS. Where it reports an
/// unknown service, the older `bootstrap_register` is used with a port of our own; the
/// returned send right must then be dropped with the receive right.
pub(super) fn check_in(name: &CStr) -> Result<(mach_port_t, mach_port_t), GpuError> {
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
        return Err(GpuError::Export("bootstrap_check_in failed"));
    }
    let port = new_receive()?;
    // SAFETY: `port` is our receive right; this adds one send right under the same name.
    let kr = unsafe { mach_port_insert_right(task(), port, port, MACH_MSG_TYPE_MAKE_SEND) };
    if kr != KERN_SUCCESS {
        destroy_receive(port);
        return Err(GpuError::Export("mach_port_insert_right failed"));
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
        return Err(GpuError::Export("bootstrap_register failed"));
    }
    Ok((port, port))
}

/// Looks up `name` and returns a send right to it.
pub(super) fn look_up(name: &CStr) -> Result<mach_port_t, GpuError> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: as in `check_in`.
    let kr =
        unsafe { bootstrap_look_up(mach2::bootstrap::bootstrap_port, name.as_ptr(), &mut port) };
    if kr == KERN_SUCCESS {
        Ok(port)
    } else {
        Err(GpuError::Disconnected("preview service not found"))
    }
}

/// Asks the kernel to send `ID_NO_SENDERS` to `port` once a send right to it has existed
/// and none is left.
pub(super) fn notify_no_senders(port: mach_port_t) -> Result<(), GpuError> {
    let mut previous = MACH_PORT_NULL;
    // SAFETY: `port` is a receive right of this task. `sync = 1` arms the notification
    // only after the first send right is made (the one sent in `SURFACES`). The kernel
    // makes a send-once right to `port` for it; send-once rights do not count as senders.
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
pub(super) fn random_bytes(buf: &mut [u8]) {
    unsafe extern "C" {
        fn arc4random_buf(buf: *mut std::ffi::c_void, nbytes: usize);
    }
    // SAFETY: `buf` is valid for writes of `buf.len()` bytes; `arc4random_buf` (libSystem)
    // writes exactly that many and cannot fail.
    unsafe { arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) }
}

/// Asks the kernel to send `ID_DEAD_NAME` to `notify` when `name` dies.
pub(super) fn notify_dead_name(name: mach_port_t, notify: mach_port_t) -> Result<(), GpuError> {
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
    if kr == KERN_SUCCESS {
        Ok(())
    } else {
        Err(GpuError::Disconnected(
            "preview service died before subscribing",
        ))
    }
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
/// `msg` must be one of the message structs above with a filled header whose size is
/// `size_of::<T>()`, and its port names and dispositions must be rights this task holds.
pub(super) unsafe fn send<T>(msg: &mut T, timeout_ms: u32) -> mach_msg_return_t {
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

/// Receives one message on `port` into `buf`, waiting at most `timeout_ms`.
pub(super) fn receive(port: mach_port_t, buf: &mut RecvBuf, timeout_ms: u32) -> mach_msg_return_t {
    // SAFETY: `buf` is `RECV_SIZE` writable bytes, aligned for a header; `port` is a
    // receive right of this task. Larger messages are destroyed by the kernel.
    unsafe {
        mach_msg(
            buf.header_mut(),
            MACH_RCV_MSG | MACH_RCV_TIMEOUT,
            0,
            RECV_SIZE as u32,
            port,
            timeout_ms,
            MACH_PORT_NULL,
        )
    }
}

/// `true` for a successful `mach_msg`.
pub(super) fn ok(ret: mach_msg_return_t) -> bool {
    ret == MACH_MSG_SUCCESS
}

/// Splits a `u64` into two words (low first).
pub(super) fn split(v: u64) -> [u32; 2] {
    [v as u32, (v >> 32) as u32]
}

/// Joins two words (low first).
pub(super) fn join(w: [u32; 2]) -> u64 {
    u64::from(w[0]) | (u64::from(w[1]) << 32)
}
