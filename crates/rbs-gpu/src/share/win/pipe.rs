//! Named-pipe messages and overlapped I/O of the Windows preview protocol (spec
//! `gpu-compositor.md`, "Preview export (cross-process)", ADR-0035).
//!
//! Marked low-level module (P11): every `unsafe` block states its invariant. Messages are
//! fixed-size arrays of little-endian 32-bit words in a message-mode pipe, so one read
//! returns one whole message and the layout is the same in every process that speaks
//! version [`VERSION`].

use std::ptr;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, HANDLE, STATUS_PENDING, WIN32_ERROR,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent};

use crate::error::GpuError;

/// Protocol version carried by `SUBSCRIBE` and `SURFACES`.
pub(crate) const VERSION: u32 = 1;
/// Most surfaces one publisher shares.
pub(crate) const MAX_SURFACES: usize = crate::share::MAX_PREVIEW_SURFACES;

/// Message ids: "RBP" (RBS preview) and the message number, as on macOS.
pub(crate) const ID_SUBSCRIBE: u32 = 0x5242_5001;
pub(crate) const ID_SURFACES: u32 = 0x5242_5002;
pub(crate) const ID_FRAME: u32 = 0x5242_5003;
pub(crate) const ID_RELEASE: u32 = 0x5242_5004;

/// Words of each message.
pub(crate) const SUBSCRIBE_WORDS: usize = 4;
/// id, version, generation, width, height, count, adapter LUID (2), handles (2 each).
pub(crate) const SURFACES_WORDS: usize = 8 + 2 * MAX_SURFACES;
pub(crate) const FRAME_WORDS: usize = 7;
pub(crate) const RELEASE_WORDS: usize = 3;

/// Largest message in bytes; buffers are this size (a longer message is a protocol error).
pub(crate) const MSG_BYTES: usize = SURFACES_WORDS * 4;

/// Pipe buffer sizes: room for a ring's worth of frames and releases several times over.
pub(crate) const PIPE_BUFFER: u32 = 4096;

/// `\\.\pipe\` + the service name, NUL-terminated UTF-16.
pub(crate) fn pipe_path(service: &str) -> Vec<u16> {
    r"\\.\pipe\"
        .encode_utf16()
        .chain(service.encode_utf16())
        .chain(Some(0))
        .collect()
}

pub(crate) fn encode<const N: usize>(words: [u32; N], out: &mut [u8; MSG_BYTES]) -> usize {
    for (chunk, w) in out.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&w.to_le_bytes());
    }
    N * 4
}

/// The first `N` words of `bytes`, if it holds exactly `N` words.
pub(crate) fn decode<const N: usize>(bytes: &[u8]) -> Option<[u32; N]> {
    if bytes.len() != N * 4 {
        return None;
    }
    let mut out = [0u32; N];
    for (w, chunk) in out.iter_mut().zip(bytes.chunks_exact(4)) {
        *w = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    Some(out)
}

pub(crate) fn split(v: u64) -> [u32; 2] {
    [v as u32, (v >> 32) as u32]
}

pub(crate) fn join(w: [u32; 2]) -> u64 {
    u64::from(w[0]) | (u64::from(w[1]) << 32)
}

/// Fills `out` from the system's preferred CSPRNG.
pub(crate) fn random_bytes(out: &mut [u8]) -> Result<(), GpuError> {
    // SAFETY: `out` is a valid writable buffer; no algorithm handle is needed with
    // BCRYPT_USE_SYSTEM_PREFERRED_RNG.
    let status = unsafe { BCryptGenRandom(None, out, BCRYPT_USE_SYSTEM_PREFERRED_RNG) };
    if status.is_err() {
        return Err(GpuError::Export("BCryptGenRandom failed"));
    }
    Ok(())
}

/// The Win32 error of a failed call.
pub(crate) fn win32(e: &windows::core::Error) -> WIN32_ERROR {
    WIN32_ERROR::from_error(e).unwrap_or(WIN32_ERROR(u32::MAX))
}

/// An owned kernel handle, closed on drop.
#[derive(Debug)]
pub(crate) struct Owned(pub HANDLE);

impl Owned {
    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() && !self.0.0.is_null() {
            // SAFETY: we own the handle and close it once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// SAFETY: a kernel handle is a process-wide value; any thread may use or close it.
unsafe impl Send for Owned {}
// SAFETY: see `Send`; the wrapper only reads the value.
unsafe impl Sync for Owned {}

/// State of an overlapped operation.
pub(crate) enum Poll {
    /// Still running.
    Pending,
    /// Done: bytes transferred.
    Done(u32),
    /// Done with an error.
    Failed(WIN32_ERROR),
}

/// One overlapped read or write with its own buffer and event. Boxed by its owner so the
/// kernel's pointers to it stay valid while it runs.
pub(crate) struct Op {
    ov: OVERLAPPED,
    event: Owned,
    pub(crate) buf: [u8; MSG_BYTES],
    /// Started and its completion not yet taken.
    issued: bool,
}

impl Op {
    /// A new idle operation (allocates its event; not for real-time threads).
    pub(crate) fn new() -> Result<Box<Self>, GpuError> {
        // SAFETY: a manual-reset, unnamed event with default security.
        let event = unsafe { CreateEventW(None, true, false, None) }
            .map_err(|_| GpuError::Export("CreateEventW failed"))?;
        Ok(Box::new(Self {
            ov: OVERLAPPED::default(),
            event: Owned(event),
            buf: [0; MSG_BYTES],
            issued: false,
        }))
    }

    pub(crate) fn event(&self) -> HANDLE {
        self.event.raw()
    }

    /// Whether the operation was started and its completion not taken yet.
    pub(crate) fn issued(&self) -> bool {
        self.issued
    }

    /// Whether the kernel still runs it (a memory read, no system call).
    pub(crate) fn running(&self) -> bool {
        // SAFETY: `Internal` is written by the kernel while the operation runs; a volatile
        // read of an aligned usize is what `HasOverlappedIoCompleted` does.
        self.issued && unsafe { ptr::read_volatile(&self.ov.Internal) } == STATUS_PENDING.0 as usize
    }

    /// Resets the operation for a call that takes an `OVERLAPPED` but no buffer
    /// (`ConnectNamedPipe`); call [`Op::mark_issued`] if it is pending afterwards.
    pub(crate) fn prepare_raw(&mut self) -> *mut OVERLAPPED {
        debug_assert!(!self.issued);
        self.prepare();
        &mut self.ov
    }

    /// Records that the call started with [`Op::prepare_raw`] is pending.
    pub(crate) fn mark_issued(&mut self) {
        self.issued = true;
    }

    fn prepare(&mut self) {
        self.ov = OVERLAPPED {
            hEvent: self.event.raw(),
            ..OVERLAPPED::default()
        };
        // SAFETY: our own valid event handle.
        let _ = unsafe { ResetEvent(self.event.raw()) };
    }

    /// Starts reading one message into `buf`. `Err` when the read failed at once.
    ///
    /// # Safety
    ///
    /// `pipe` must be an open overlapped pipe handle that outlives the operation, and the
    /// operation must not be issued.
    pub(crate) unsafe fn read(&mut self, pipe: HANDLE) -> Result<(), WIN32_ERROR> {
        debug_assert!(!self.issued);
        self.prepare();
        let ov: *mut OVERLAPPED = &mut self.ov;
        // SAFETY: the caller guarantees the handle; `buf` and `ov` live in a box that
        // outlives the operation (its owner cancels and waits before dropping it).
        let r = unsafe { ReadFile(pipe, Some(&mut self.buf), None, Some(ov)) };
        self.started(r)
    }

    /// Starts writing the first `len` bytes of `buf` as one message.
    ///
    /// # Safety
    ///
    /// As [`Op::read`].
    pub(crate) unsafe fn write(&mut self, pipe: HANDLE, len: usize) -> Result<(), WIN32_ERROR> {
        debug_assert!(!self.issued);
        self.prepare();
        let ov: *mut OVERLAPPED = &mut self.ov;
        // SAFETY: as in `read`; `len` is at most the buffer size.
        let r = unsafe { WriteFile(pipe, Some(&self.buf[..len.min(MSG_BYTES)]), None, Some(ov)) };
        self.started(r)
    }

    fn started(&mut self, r: windows::core::Result<()>) -> Result<(), WIN32_ERROR> {
        match r {
            Ok(()) => {
                self.issued = true;
                Ok(())
            }
            Err(e) if win32(&e) == ERROR_IO_PENDING => {
                self.issued = true;
                Ok(())
            }
            Err(e) => Err(win32(&e)),
        }
    }

    /// Takes the completion if the operation has finished. Never waits.
    ///
    /// # Safety
    ///
    /// `pipe` must be the handle the operation was started on.
    pub(crate) unsafe fn poll(&mut self, pipe: HANDLE) -> Poll {
        if !self.issued {
            return Poll::Failed(WIN32_ERROR(0));
        }
        if self.running() {
            return Poll::Pending;
        }
        let mut n = 0u32;
        // SAFETY: the operation has finished (checked above), so this does not wait; `ov`
        // is the one the operation was started with.
        let r = unsafe { GetOverlappedResult(pipe, &self.ov, &mut n, false) };
        self.issued = false;
        match r {
            Ok(()) => Poll::Done(n),
            Err(e) => Poll::Failed(win32(&e)),
        }
    }

    /// Cancels the operation if it runs and waits until the kernel is done with it.
    /// Not for real-time threads (drop paths only).
    ///
    /// # Safety
    ///
    /// As [`Op::poll`].
    pub(crate) unsafe fn cancel(&mut self, pipe: HANDLE) {
        if !self.issued {
            return;
        }
        let ov: *const OVERLAPPED = &self.ov;
        // SAFETY: cancels only this operation on its own handle.
        let _ = unsafe { CancelIoEx(pipe, Some(ov)) };
        let mut n = 0u32;
        // SAFETY: waits on this operation's own event until the kernel has completed it
        // (cancelled or not), after which its memory may be reused.
        let _ = unsafe { GetOverlappedResult(pipe, &self.ov, &mut n, true) };
        self.issued = false;
    }
}

// SAFETY: an `Op` is used by one thread at a time (its owner's); the kernel holds raw
// pointers into its box only while it is issued, and every owner cancels before dropping.
unsafe impl Send for Op {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip() {
        let mut buf = [0u8; MSG_BYTES];
        let n = encode([ID_FRAME, 7, 3, 1, 2, 3, 4], &mut buf);
        assert_eq!(n, FRAME_WORDS * 4);
        assert_eq!(
            decode::<FRAME_WORDS>(&buf[..n]),
            Some([ID_FRAME, 7, 3, 1, 2, 3, 4])
        );
        assert_eq!(decode::<RELEASE_WORDS>(&buf[..n]), None);
        assert_eq!(join(split(0x0123_4567_89ab_cdef)), 0x0123_4567_89ab_cdef);
        assert_eq!(MSG_BYTES, 96);
    }

    #[test]
    fn pipe_path_is_nul_terminated() {
        let p = pipe_path("org.rbs.preview.1.0.ab");
        assert_eq!(p.last(), Some(&0));
        assert_eq!(
            String::from_utf16_lossy(&p[..p.len() - 1]),
            r"\\.\pipe\org.rbs.preview.1.0.ab"
        );
    }
}
