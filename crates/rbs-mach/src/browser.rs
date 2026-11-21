//! The browser surface handover (ADR-0167 decision 3).
//!
//! The studio checks in a service with a random name for one spawn ([`Handover`]); the
//! helper looks it up once ([`fetch`]), sends one request with the spawn's token and
//! a reply port, and receives a send right to each slot surface. The studio answers
//! only a request with the right token from the spawned helper's pid, then removes the
//! service. Its message ids and version are its own, distinct from the preview
//! protocol's (ADR-0028), so neither service parses the other's messages.

use std::ffi::CString;
use std::mem::size_of;

use mach2::message::mach_msg_header_t;
use mach2::port::{MACH_PORT_NULL, mach_port_t};

use crate::{
    MACH_MSG_PORT_DESCRIPTOR, MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE,
    MACH_MSGH_BITS_COMPLEX, MAKE_SEND_ONCE, MachError, RecvBuf, TRAILER_ROOM, bits, check_in,
    deallocate, destroy_receive, look_up, mach_msg_body_t, mach_msg_port_descriptor_t,
    new_receive, ok, random_bytes, receive, receive_audited, send,
};

/// Protocol version carried by both messages.
pub const VERSION: u32 = 1;
/// Most surfaces one handover carries (the frame section's `MAX_GPU_SLOTS`).
pub const MAX_SURFACES: usize = 8;
/// Message ids: "RBB" (RBS browser) and the message number.
pub const ID_REQUEST: i32 = 0x5242_6201;
/// See [`ID_REQUEST`].
pub const ID_SURFACES: i32 = 0x5242_6202;
/// Messages the studio takes from the queue in one [`Handover::poll`], so a flood
/// cannot hold the supervisor's pass.
pub const DRAIN_PER_POLL: usize = 8;
/// Bytes of the spawn token.
pub const TOKEN_LEN: usize = 16;

/// Helper to studio: version and token; the reply right is in the header.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Request {
    header: mach_msg_header_t,
    version: u32,
    token: [u8; TOKEN_LEN],
}

/// Studio to helper: one copied send right per surface (null for unused descriptors).
#[repr(C)]
#[derive(Clone, Copy)]
struct Surfaces {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    ports: [mach_msg_port_descriptor_t; MAX_SURFACES],
    version: u32,
    count: u32,
}

const STUDIO_BUF: usize = size_of::<Request>() + TRAILER_ROOM;
const HELPER_BUF: usize = size_of::<Surfaces>() + TRAILER_ROOM;

fn descriptor(name: mach_port_t) -> mach_msg_port_descriptor_t {
    mach_msg_port_descriptor_t::new(name, MACH_MSG_TYPE_COPY_SEND)
}

/// Where a handover stands after [`Handover::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No valid request yet.
    Waiting,
    /// The surfaces went to the helper; the service is gone.
    Done,
    /// The reply could not be sent; the service is gone.
    Failed,
}

/// The studio side of one spawn's handover. Never waits.
pub struct Handover {
    name: CString,
    token: [u8; TOKEN_LEN],
    pid: u32,
    service: mach_port_t,
    registered: mach_port_t,
    surfaces: Vec<mach_port_t>,
    state: State,
    buf: Box<RecvBuf<STUDIO_BUF>>,
}

// SAFETY: the port names are plain integers owned by this value; the one thread that
// holds the `Handover` at a time is the only one that uses or drops them.
unsafe impl Send for Handover {}

impl std::fmt::Debug for Handover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handover")
            .field("name", &self.name)
            .field("pid", &self.pid)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl Handover {
    /// Checks in a service with a fresh random name for the helper `pid`. Takes over
    /// `surfaces`, one send right per slot (`IOSurfaceCreateMachPort`), at most
    /// [`MAX_SURFACES`]; they are dropped with the handover.
    pub fn new(surfaces: Vec<mach_port_t>, pid: u32) -> Result<Self, MachError> {
        if surfaces.is_empty() || surfaces.len() > MAX_SURFACES {
            surfaces.into_iter().for_each(deallocate);
            return Err(MachError("bad surface count"));
        }
        let mut id = [0u8; 8];
        random_bytes(&mut id);
        let mut token = [0u8; TOKEN_LEN];
        random_bytes(&mut token);
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        let Ok(name) = CString::new(format!("org.rbs.browser-surfaces.{hex}")) else {
            surfaces.into_iter().for_each(deallocate);
            return Err(MachError("bad service name"));
        };
        let (service, registered) = match check_in(&name) {
            Ok(ports) => ports,
            Err(error) => {
                surfaces.into_iter().for_each(deallocate);
                return Err(error);
            }
        };
        Ok(Self {
            name,
            token,
            pid,
            service,
            registered,
            surfaces,
            state: State::Waiting,
            buf: Box::new(RecvBuf::new()),
        })
    }

    /// The service name the helper looks up.
    pub fn name(&self) -> &str {
        self.name.to_str().unwrap_or_default()
    }

    /// The spawn's token the helper must send.
    pub fn token(&self) -> [u8; TOKEN_LEN] {
        self.token
    }

    /// How many surfaces the handover carries.
    pub fn count(&self) -> u32 {
        self.surfaces.len() as u32
    }

    /// Where the handover stands.
    pub fn state(&self) -> State {
        self.state
    }

    /// Takes at most [`DRAIN_PER_POLL`] queued requests with zero-timeout calls and
    /// answers the first valid one. Never waits.
    pub fn poll(&mut self) -> State {
        for _ in 0..DRAIN_PER_POLL {
            if self.state != State::Waiting {
                break;
            }
            if !ok(receive_audited(self.service, &mut self.buf, 0)) {
                break;
            }
            let header = self.buf.header();
            let valid = header.msgh_id == ID_REQUEST
                && self.buf.sender_pid() == Some(self.pid)
                && self
                    .buf
                    .read::<Request>()
                    .is_some_and(|r| r.version == VERSION && r.token == self.token)
                && header.msgh_remote_port != MACH_PORT_NULL;
            if !valid {
                self.buf.destroy();
                continue;
            }
            self.state = if self.reply(header.msgh_remote_port) {
                State::Done
            } else {
                State::Failed
            };
            self.remove_service();
        }
        self.state
    }

    fn reply(&mut self, to: mach_port_t) -> bool {
        let mut ports = [descriptor(MACH_PORT_NULL); MAX_SURFACES];
        for (slot, &port) in ports.iter_mut().zip(&self.surfaces) {
            *slot = descriptor(port);
        }
        let mut msg = Surfaces {
            header: mach_msg_header_t {
                msgh_bits: bits(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0) | MACH_MSGH_BITS_COMPLEX,
                msgh_size: size_of::<Surfaces>() as u32,
                msgh_remote_port: to,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: ID_SURFACES,
            },
            body: mach_msg_body_t {
                msgh_descriptor_count: MAX_SURFACES as u32,
            },
            ports,
            version: VERSION,
            count: self.surfaces.len() as u32,
        };
        // SAFETY: a filled `Surfaces` of its own size; the reply right is the send-once
        // right the request moved to us, and the surface rights are copied. On a
        // timeout `send` destroys the returned rights.
        let ret = unsafe { send(&mut msg, 0) };
        if !ok(ret) {
            // A failed send that was not copied in leaves the send-once right with us.
            deallocate(to);
            return false;
        }
        true
    }

    /// Removes the service now (also on a deadline); a waiting handover fails.
    pub fn close(&mut self) {
        if self.state == State::Waiting {
            self.state = State::Failed;
        }
        self.remove_service();
    }

    fn remove_service(&mut self) {
        deallocate(self.registered);
        self.registered = MACH_PORT_NULL;
        destroy_receive(self.service);
        self.service = MACH_PORT_NULL;
    }
}

impl Drop for Handover {
    fn drop(&mut self) {
        self.remove_service();
        for port in self.surfaces.drain(..) {
            deallocate(port);
        }
    }
}

/// The helper side: looks `name` up, sends `token` and waits at most `timeout_ms` for
/// the surfaces. Returns one send right per surface, in slot order; the caller opens
/// each (`IOSurfaceLookupFromMachPort`) and deallocates it. Waits: call it outside
/// any CEF callback.
pub fn fetch(
    name: &str,
    token: [u8; TOKEN_LEN],
    timeout_ms: u32,
) -> Result<Vec<mach_port_t>, MachError> {
    let name = CString::new(name).map_err(|_| MachError("bad service name"))?;
    let service = look_up(&name)?;
    let reply = match new_receive() {
        Ok(port) => port,
        Err(error) => {
            deallocate(service);
            return Err(error);
        }
    };
    let mut request = Request {
        header: mach_msg_header_t {
            msgh_bits: bits(MACH_MSG_TYPE_COPY_SEND, MAKE_SEND_ONCE),
            msgh_size: size_of::<Request>() as u32,
            msgh_remote_port: service,
            msgh_local_port: reply,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: ID_REQUEST,
        },
        version: VERSION,
        token,
    };
    // SAFETY: a filled `Request` of its own size; the service send right is copied and
    // a send-once right to our reply port is made.
    let sent = unsafe { send(&mut request, timeout_ms) };
    deallocate(service);
    if !ok(sent) {
        destroy_receive(reply);
        return Err(MachError("request not sent"));
    }
    let mut buf = Box::new(RecvBuf::<HELPER_BUF>::new());
    let received = receive(reply, &mut buf, timeout_ms);
    destroy_receive(reply);
    if !ok(received) {
        return Err(MachError("no surfaces in time"));
    }
    let header = buf.header();
    let reply_msg = (header.msgh_id == ID_SURFACES
        && header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0)
        .then(|| buf.read::<Surfaces>())
        .flatten();
    let Some(msg) = reply_msg.filter(|m| {
        m.version == VERSION
            && m.count as usize <= MAX_SURFACES
            && m.count > 0
            && m.body.msgh_descriptor_count as usize == MAX_SURFACES
    }) else {
        buf.destroy();
        return Err(MachError("bad surfaces reply"));
    };
    let mut ports = Vec::with_capacity(msg.count as usize);
    for (i, d) in msg.ports.iter().enumerate() {
        if i < msg.count as usize && d.type_ == MACH_MSG_PORT_DESCRIPTOR as u8 {
            ports.push(d.name);
        } else {
            deallocate(d.name);
        }
    }
    if ports.len() != msg.count as usize || ports.contains(&MACH_PORT_NULL) {
        ports.into_iter().for_each(deallocate);
        return Err(MachError("missing surface right"));
    }
    Ok(ports)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::{MAKE_SEND, Port};

    /// A receive right with one send right, standing in for a surface port.
    fn fake_surface() -> Port {
        let port = new_receive().unwrap();
        // SAFETY: test-only: adds a send right to our own receive right.
        let kr = unsafe {
            mach2::mach_port::mach_port_insert_right(
                mach2::traps::mach_task_self(),
                port,
                port,
                MAKE_SEND,
            )
        };
        assert_eq!(kr, 0);
        port
    }

    fn run_fetch(name: String, token: [u8; TOKEN_LEN]) -> std::thread::JoinHandle<Result<usize, MachError>> {
        std::thread::spawn(move || {
            let ports = fetch(&name, token, 2_000)?;
            let n = ports.len();
            ports.into_iter().for_each(deallocate);
            Ok(n)
        })
    }

    fn poll_until_not_waiting(handover: &mut Handover, guard: std::time::Duration) -> State {
        let start = std::time::Instant::now();
        loop {
            let state = handover.poll();
            if state != State::Waiting || start.elapsed() > guard {
                return state;
            }
            std::thread::yield_now();
        }
    }

    /// Polls until the helper's fetch has returned (hang guard 5 s).
    fn poll_until_fetch_ends(
        handover: &mut Handover,
        fetched: &std::thread::JoinHandle<Result<usize, MachError>>,
    ) -> State {
        let start = std::time::Instant::now();
        while !fetched.is_finished() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "hang guard");
            handover.poll();
            std::thread::yield_now();
        }
        handover.state()
    }

    #[test]
    fn the_helper_gets_every_surface_and_the_service_is_gone() {
        let surfaces: Vec<Port> = (0..4).map(|_| fake_surface()).collect();
        let mut handover = Handover::new(surfaces, std::process::id()).unwrap();
        let name = handover.name().to_owned();
        let fetched = run_fetch(name.clone(), handover.token());
        assert_eq!(
            poll_until_not_waiting(&mut handover, std::time::Duration::from_secs(5)),
            State::Done
        );
        assert_eq!(fetched.join().unwrap(), Ok(4));
        // Removed after one reply: a second look-up fails.
        assert!(look_up(&CString::new(name).unwrap()).is_err());
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let surfaces: Vec<Port> = (0..2).map(|_| fake_surface()).collect();
        let mut handover = Handover::new(surfaces, std::process::id()).unwrap();
        let mut token = handover.token();
        token[0] ^= 1;
        let fetched = run_fetch(handover.name().to_owned(), token);
        // The request is dropped, which destroys its reply right: the helper's wait
        // ends at once without surfaces, and the handover still waits.
        assert_eq!(poll_until_fetch_ends(&mut handover, &fetched), State::Waiting);
        assert!(fetched.join().unwrap().is_err());
        handover.close();
        assert_eq!(handover.state(), State::Failed);
    }

    #[test]
    fn a_request_from_another_pid_is_refused() {
        let surfaces: Vec<Port> = (0..2).map(|_| fake_surface()).collect();
        // The handover expects another process; this test's own request must fail.
        let mut handover = Handover::new(surfaces, std::process::id() + 1).unwrap();
        let fetched = run_fetch(handover.name().to_owned(), handover.token());
        assert_eq!(poll_until_fetch_ends(&mut handover, &fetched), State::Waiting);
        assert!(fetched.join().unwrap().is_err());
        handover.close();
    }

    #[test]
    fn a_poll_with_no_request_returns_at_once() {
        let surfaces: Vec<Port> = vec![fake_surface()];
        let mut handover = Handover::new(surfaces, std::process::id()).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            assert_eq!(handover.poll(), State::Waiting);
        }
        // Hang guard only: zero-timeout calls.
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        handover.close();
        assert!(look_up(&CString::new(handover.name()).unwrap()).is_err());
    }
}
