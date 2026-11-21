//! Mach messages and port calls of the preview protocol (spec `gpu-compositor.md`,
//! "Preview export (cross-process)", ADR-0028).
//!
//! Marked low-level module (P11): every `unsafe` block states its invariant. Messages are
//! fixed-size `repr(C)` structs of 32-bit words, so no field needs packing and the layout
//! is the same in every process that speaks version [`VERSION`].

use std::ffi::CStr;
use std::mem::size_of;

use mach2::message::{
    mach_msg_body_t, mach_msg_header_t, mach_msg_port_descriptor_t, mach_msg_return_t,
};
use mach2::notify::{MACH_NOTIFY_DEAD_NAME, MACH_NOTIFY_NO_SENDERS};
use mach2::port::mach_port_t;

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
const RECV_SIZE: usize = size_of::<Surfaces>() + rbs_mach::TRAILER_ROOM;

/// A receive buffer, aligned for every message struct.
pub(super) type RecvBuf = rbs_mach::RecvBuf<RECV_SIZE>;

// The port calls live in `rbs-mach` (ADR-0167 decision 3), shared with the browser
// surface handover; this module keeps the preview messages and maps the errors.
pub(super) use rbs_mach::{bits, deallocate, destroy_receive, join, ok, random_bytes, split};

fn export(error: rbs_mach::MachError) -> GpuError {
    GpuError::Export(error.0)
}

/// A new receive right with the largest queue limit.
pub(super) fn new_receive() -> Result<mach_port_t, GpuError> {
    rbs_mach::new_receive().map_err(export)
}

/// Registers `name` in the bootstrap namespace (see [`rbs_mach::check_in`]).
pub(super) fn check_in(name: &CStr) -> Result<(mach_port_t, mach_port_t), GpuError> {
    rbs_mach::check_in(name).map_err(export)
}

/// Looks up `name` and returns a send right to it.
pub(super) fn look_up(name: &CStr) -> Result<mach_port_t, GpuError> {
    rbs_mach::look_up(name).map_err(|_| GpuError::Disconnected("preview service not found"))
}

/// Asks the kernel to send `ID_NO_SENDERS` to `port` once a send right to it has existed
/// and none is left.
pub(super) fn notify_no_senders(port: mach_port_t) -> Result<(), GpuError> {
    rbs_mach::notify_no_senders(port).map_err(export)
}

/// Asks the kernel to send `ID_DEAD_NAME` to `notify` when `name` dies.
pub(super) fn notify_dead_name(name: mach_port_t, notify: mach_port_t) -> Result<(), GpuError> {
    rbs_mach::notify_dead_name(name, notify)
        .map_err(|_| GpuError::Disconnected("preview service died before subscribing"))
}

/// Sends `msg`, waiting at most `timeout_ms` (0: never waits). See [`rbs_mach::send`].
///
/// # Safety
///
/// `msg` must be one of the message structs above with a filled header whose size is
/// `size_of::<T>()`, and its port names and dispositions must be rights this task holds.
pub(super) unsafe fn send<T>(msg: &mut T, timeout_ms: u32) -> mach_msg_return_t {
    // SAFETY: the caller upholds the same contract `rbs_mach::send` asks for.
    unsafe { rbs_mach::send(msg, timeout_ms) }
}

/// Receives one message on `port` into `buf`, waiting at most `timeout_ms`.
pub(super) fn receive(port: mach_port_t, buf: &mut RecvBuf, timeout_ms: u32) -> mach_msg_return_t {
    rbs_mach::receive(port, buf, timeout_ms)
}
