//! Named-pipe messages and overlapped I/O of the Windows preview protocol (spec
//! `gpu-compositor.md`, "Preview export (cross-process)", ADR-0036).
//!
//! Marked low-level module (P11): every `unsafe` block states its invariant. Messages are
//! fixed-size arrays of little-endian 32-bit words in a message-mode pipe, so one read
//! returns one whole message and the layout is the same in every process that speaks
//! version [`VERSION`].

use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{Ordering, fence};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, HANDLE, HLOCAL, LocalFree, STATUS_PENDING, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, OpenProcessToken, ResetEvent,
};
use windows::core::{PCWSTR, PWSTR};

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
/// UI to engine after `SURFACES`: the UI owns the duplicated handles from now on.
pub(crate) const ID_ACCEPT: u32 = 0x5242_5005;

/// Words of each message.
pub(crate) const SUBSCRIBE_WORDS: usize = 4;
/// id, version, generation, width, height, count, adapter LUID (2), handles (2 each).
pub(crate) const SURFACES_WORDS: usize = 8 + 2 * MAX_SURFACES;
pub(crate) const FRAME_WORDS: usize = 7;
pub(crate) const RELEASE_WORDS: usize = 3;
/// id, generation.
pub(crate) const ACCEPT_WORDS: usize = 2;

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
///
/// The kernel writes `ov` (and `buf` for reads) while the operation runs, so both live in
/// `UnsafeCell`s and are reached only through raw pointers while issued; Rust code reads
/// or writes `buf` only while the operation is not issued.
pub(crate) struct Op {
    ov: UnsafeCell<OVERLAPPED>,
    event: Owned,
    buf: UnsafeCell<[u8; MSG_BYTES]>,
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
            ov: UnsafeCell::new(OVERLAPPED::default()),
            event: Owned(event),
            buf: UnsafeCell::new([0; MSG_BYTES]),
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
        if !self.issued {
            return false;
        }
        // SAFETY: `Internal` is written by the kernel while the operation runs; a volatile
        // read of an aligned usize through the cell's pointer is what
        // `HasOverlappedIoCompleted` does.
        let internal = unsafe { ptr::read_volatile(&raw const (*self.ov.get()).Internal) };
        if internal == STATUS_PENDING.0 as usize {
            return true;
        }
        // The kernel's writes to the buffer happen before its final status store.
        fence(Ordering::Acquire);
        false
    }

    /// The first `len` bytes of the buffer. Only while the operation is not issued.
    pub(crate) fn data(&self, len: usize) -> &[u8] {
        debug_assert!(!self.issued);
        // SAFETY: not issued, so the kernel does not write the buffer; `&self` keeps Rust
        // writers out.
        let all: &[u8; MSG_BYTES] = unsafe { &*self.buf.get() };
        &all[..len.min(MSG_BYTES)]
    }

    /// Encodes `words` into the buffer and returns the byte length. Only while the
    /// operation is not issued.
    pub(crate) fn fill<const N: usize>(&mut self, words: [u32; N]) -> usize {
        debug_assert!(!self.issued);
        encode(words, self.buf.get_mut())
    }

    /// Resets the operation for a call that takes an `OVERLAPPED` but no buffer
    /// (`ConnectNamedPipe`); call [`Op::mark_issued`] if it is pending afterwards.
    pub(crate) fn prepare_raw(&mut self) -> *mut OVERLAPPED {
        debug_assert!(!self.issued);
        self.prepare();
        self.ov.get()
    }

    /// Records that the call started with [`Op::prepare_raw`] is pending.
    pub(crate) fn mark_issued(&mut self) {
        self.issued = true;
    }

    fn prepare(&mut self) {
        *self.ov.get_mut() = OVERLAPPED {
            hEvent: self.event.raw(),
            ..OVERLAPPED::default()
        };
        // SAFETY: our own valid event handle.
        let _ = unsafe { ResetEvent(self.event.raw()) };
    }

    /// Starts reading one message into the buffer. `Err` when the read failed at once.
    ///
    /// # Safety
    ///
    /// `pipe` must be an open overlapped pipe handle that outlives the operation, and the
    /// operation must not be issued.
    pub(crate) unsafe fn read(&mut self, pipe: HANDLE) -> Result<(), WIN32_ERROR> {
        debug_assert!(!self.issued);
        self.prepare();
        let ov = self.ov.get();
        let buf = self.buf.get().cast::<u8>();
        // SAFETY: the caller guarantees the handle; `buf` and `ov` live in a box that
        // outlives the operation (its owner cancels and waits before dropping it) and are
        // reached only through these raw pointers until the completion is taken.
        let r = unsafe {
            ReadFile(
                pipe,
                Some(std::slice::from_raw_parts_mut(buf, MSG_BYTES)),
                None,
                Some(ov),
            )
        };
        self.started(r)
    }

    /// Starts writing the first `len` bytes of the buffer (see [`Op::fill`]) as one
    /// message.
    ///
    /// # Safety
    ///
    /// As [`Op::read`].
    pub(crate) unsafe fn write(&mut self, pipe: HANDLE, len: usize) -> Result<(), WIN32_ERROR> {
        debug_assert!(!self.issued);
        self.prepare();
        let ov = self.ov.get();
        let buf = self.buf.get().cast::<u8>().cast_const();
        // SAFETY: as in `read`; `len` is at most the buffer size; the kernel only reads it.
        let r = unsafe {
            WriteFile(
                pipe,
                Some(std::slice::from_raw_parts(buf, len.min(MSG_BYTES))),
                None,
                Some(ov),
            )
        };
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
        let r = unsafe { GetOverlappedResult(pipe, self.ov.get(), &mut n, false) };
        self.issued = false;
        match r {
            Ok(()) => Poll::Done(n),
            Err(e) => Poll::Failed(win32(&e)),
        }
    }

    /// Cancels the operation if it runs and waits until the kernel is done with it.
    /// Returns its completion (it may have finished before the cancel). Not for real-time
    /// threads (drop and give-up paths only).
    ///
    /// # Safety
    ///
    /// As [`Op::poll`].
    pub(crate) unsafe fn cancel(&mut self, pipe: HANDLE) -> Poll {
        if !self.issued {
            return Poll::Failed(WIN32_ERROR(0));
        }
        let ov = self.ov.get().cast_const();
        // SAFETY: cancels only this operation on its own handle.
        let _ = unsafe { CancelIoEx(pipe, Some(ov)) };
        let mut n = 0u32;
        // SAFETY: waits on this operation's own event until the kernel has completed it
        // (cancelled or not), after which its memory may be reused.
        let r = unsafe { GetOverlappedResult(pipe, ov, &mut n, true) };
        fence(Ordering::Acquire);
        self.issued = false;
        match r {
            Ok(()) => Poll::Done(n),
            Err(e) => Poll::Failed(win32(&e)),
        }
    }
}

// SAFETY: an `Op` is used by one thread at a time (its owner's); the kernel holds raw
// pointers into its box only while it is issued, and every owner cancels before dropping.
unsafe impl Send for Op {}

/// A security descriptor whose DACL grants the current process's user, and nobody else,
/// full access (the pipe's DACL: the default one lets Everyone connect for reading).
pub(crate) struct UserOnly {
    sd: PSECURITY_DESCRIPTOR,
}

impl UserOnly {
    /// Builds `D:P(A;;GA;;;<user SID>)S:(ML;;NWNRNX;;;ME)` for the process token's user:
    /// only that user, and only at medium integrity or above (a low-integrity sandbox of
    /// the same user can neither read nor write the pipe, so it cannot occupy it). Not for
    /// real-time threads.
    pub(crate) fn new() -> Result<Self, GpuError> {
        let fail = |what| move |_: windows::core::Error| GpuError::Export(what);
        let mut token = HANDLE::default();
        // SAFETY: opens our own process token for querying.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(fail("OpenProcessToken failed"))?;
        let token = Owned(token);
        // TOKEN_USER plus its SID; 256 bytes hold any SID (at most 68 bytes) and the header.
        let mut info = [0u64; 32];
        let mut len = 0u32;
        // SAFETY: `info` is a writable, 8-aligned buffer of the size given.
        unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                Some(info.as_mut_ptr().cast()),
                std::mem::size_of_val(&info) as u32,
                &mut len,
            )
        }
        .map_err(fail("GetTokenInformation failed"))?;
        // SAFETY: the call filled `info` with a TOKEN_USER whose SID points into `info`.
        let sid = unsafe { (*info.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        let mut text = PWSTR::null();
        // SAFETY: `sid` is valid while `info` lives; the string is freed with LocalFree.
        unsafe { ConvertSidToStringSidW(sid, &mut text) }
            .map_err(fail("ConvertSidToStringSidW failed"))?;
        // SAFETY: a NUL-terminated string allocated by the call above.
        let sid_text = unsafe { text.to_string() }.unwrap_or_default();
        // SAFETY: frees the string allocated by ConvertSidToStringSidW.
        let _ = unsafe { LocalFree(Some(HLOCAL(text.0.cast()))) };
        if sid_text.is_empty() {
            return Err(GpuError::Export("empty user SID"));
        }
        // Protected DACL (no inherited entries), generic all for the user only; mandatory
        // label medium, no read up, no write up and no execute up.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid_text})S:(ML;;NWNRNX;;;ME)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated; the descriptor is freed with LocalFree on drop.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(fail(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW failed",
        ))?;
        Ok(Self { sd })
    }

    /// Attributes for a create call; valid while `self` lives.
    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.sd.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for UserOnly {
    fn drop(&mut self) {
        // SAFETY: frees the descriptor allocated by
        // ConvertStringSecurityDescriptorToSecurityDescriptorW.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.sd.0))) };
    }
}

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
