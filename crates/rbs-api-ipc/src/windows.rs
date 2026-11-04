//! Named pipe endpoint (`\\.\pipe\rbs-control-<user SID>`): created with a DACL that
//! lets only the current user open it, `PIPE_REJECT_REMOTE_CLIENTS`, and
//! `FILE_FLAG_FIRST_PIPE_INSTANCE` so a program that took the name first is noticed.
//! Each client's process is checked to run as the current user; the client checks the
//! server's process the same way. Overlapped I/O, so a connection's reader and writer
//! threads use one handle at once and both can be stopped.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    HANDLE, HLOCAL, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, PSID, RevertToSelf, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, ImpersonateNamedPipeClient,
    PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT, WaitNamedPipeW,
};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, INFINITE, OpenProcess, OpenProcessToken,
    OpenThreadToken, PROCESS_QUERY_LIMITED_INFORMATION, ResetEvent, SetEvent,
    WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use crate::conn::{Stream, WRITE_STALL};

/// Pipe buffer sizes (a hint to the system).
const BUFFER: u32 = 64 * 1024;

/// A Win32 error as an `io::Error` with its OS code.
fn os(e: windows::core::Error) -> io::Error {
    let h = e.code().0 as u32;
    if h & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((h & 0xFFFF) as i32)
    } else {
        io::Error::other(e)
    }
}

fn is(e: &windows::core::Error, code: windows::Win32::Foundation::WIN32_ERROR) -> bool {
    e.code() == code.to_hresult()
}

/// A handle closed when dropped.
struct Owned(HANDLE);

// SAFETY: a kernel handle may be used and closed from any thread; the wrapper only
// closes it once, in `drop`.
unsafe impl Send for Owned {}
// SAFETY: as above; shared use goes through thread-safe kernel calls.
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle is owned by this wrapper and not used after this.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A manual-reset event.
fn event() -> io::Result<Owned> {
    // SAFETY: no attributes and no name; the returned handle is owned by `Owned`.
    unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map(Owned)
        .map_err(os)
}

/// Runs one overlapped operation on `handle` and waits for it, at most `timeout_ms`, or
/// until `cancel` is set. `Ok(None)` when the time ran out (the operation is
/// cancelled); an error when `cancel` was set.
fn overlapped(
    handle: HANDLE,
    event: &Owned,
    cancel: Option<&Owned>,
    timeout_ms: u32,
    start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
) -> io::Result<Option<u32>> {
    let mut ov = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    // SAFETY: the event handle is valid while `event` lives.
    unsafe { ResetEvent(event.0) }.map_err(os)?;
    match start(&raw mut ov) {
        Ok(()) => {}
        // `ConnectNamedPipe`: the client connected before the call; nothing started.
        Err(e) if is(&e, ERROR_PIPE_CONNECTED) => return Ok(Some(0)),
        Err(e) if is(&e, ERROR_IO_PENDING) => {
            let waits: Vec<HANDLE> = std::iter::once(event.0)
                .chain(cancel.map(|c| c.0))
                .collect();
            // SAFETY: the handles are valid while `event` and `cancel` live.
            let w = unsafe { WaitForMultipleObjects(&waits, false, timeout_ms) };
            if w != WAIT_OBJECT_0 {
                // Timed out or cancelled: end this operation before `ov` goes away.
                let mut n = 0;
                // SAFETY: `ov` is the operation started above and lives until it ends,
                // which `GetOverlappedResult` with `bwait` waits for.
                let done = unsafe {
                    let _ = CancelIoEx(handle, Some(&raw const ov));
                    GetOverlappedResult(handle, &raw const ov, &mut n, true)
                };
                return match done {
                    Ok(()) => Ok(Some(n)),
                    Err(_) if w == WAIT_TIMEOUT => Ok(None),
                    Err(_) => Err(io::ErrorKind::ConnectionAborted.into()),
                };
            }
        }
        Err(e) => return Err(os(e)),
    }
    let mut n = 0;
    // SAFETY: `ov` belongs to the operation started above, which has completed.
    unsafe { GetOverlappedResult(handle, &raw const ov, &mut n, true) }.map_err(os)?;
    Ok(Some(n))
}

fn ended(e: &io::Error) -> bool {
    [ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED, ERROR_NO_DATA]
        .iter()
        .any(|c| e.raw_os_error() == Some(c.0 as i32))
}

/// The SID of a process's user, in the buffer `GetTokenInformation` filled.
struct UserSid(Vec<u64>);

impl UserSid {
    fn of_process(process: HANDLE) -> io::Result<UserSid> {
        let mut token = HANDLE::default();
        // SAFETY: `process` is a valid process handle; `token` receives a handle owned
        // below.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.map_err(os)?;
        let token = Owned(token);
        Self::of_token(token.0)
    }

    fn of_token(token: HANDLE) -> io::Result<UserSid> {
        let mut len = 0u32;
        // SAFETY: a size query: no buffer, `len` receives the size needed.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        // SAFETY: `buf` is at least `len` bytes and 8-byte aligned (`TOKEN_USER` needs
        // pointer alignment).
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                len,
                &mut len,
            )
        }
        .map_err(os)?;
        Ok(UserSid(buf))
    }

    fn current() -> io::Result<UserSid> {
        // SAFETY: the pseudo handle of this process needs no closing.
        Self::of_process(unsafe { GetCurrentProcess() })
    }

    fn of_pid(pid: u32) -> io::Result<UserSid> {
        // SAFETY: opens a handle owned below; fails for a process that is gone.
        let p =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map_err(os)?;
        let p = Owned(p);
        Self::of_process(p.0)
    }

    fn psid(&self) -> PSID {
        // SAFETY: the buffer holds a `TOKEN_USER` written by `GetTokenInformation`,
        // whose `Sid` points into the same buffer.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }

    fn same(&self, other: &UserSid) -> bool {
        // SAFETY: both SIDs are valid while their buffers live.
        unsafe { EqualSid(self.psid(), other.psid()) }.is_ok()
    }

    fn string(&self) -> io::Result<String> {
        let mut p = PWSTR::null();
        // SAFETY: the SID is valid; `p` receives a string the system allocated, freed
        // below.
        unsafe { ConvertSidToStringSidW(self.psid(), &mut p) }.map_err(os)?;
        // SAFETY: `p` is a NUL-terminated string from the call above.
        let s = unsafe { p.to_string() }.map_err(io::Error::other);
        // SAFETY: `p` was allocated with `LocalAlloc` by the call above.
        unsafe { LocalFree(Some(HLOCAL(p.0.cast()))) };
        s
    }
}

/// True when the client of the connected `pipe` runs as the current user, from the
/// connection itself: this thread briefly takes the client's identity (it opens the
/// pipe with `SECURITY_IDENTIFICATION`, enough to read its token, not to act as it),
/// reads its token's user and returns to its own.
fn client_is_current_user(pipe: HANDLE) -> bool {
    let Ok(me) = UserSid::current() else {
        return false;
    };
    // SAFETY: `pipe` is a connected server instance; the impersonation is undone
    // below before anything else runs on this thread.
    if unsafe { ImpersonateNamedPipeClient(pipe) }.is_err() {
        return false;
    }
    let mut token = HANDLE::default();
    // SAFETY: reads this thread's impersonation token; `OpenAsSelf` checks access as
    // the process, not as the client. The handle is owned below.
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, true, &mut token) };
    // SAFETY: ends the impersonation started above.
    if unsafe { RevertToSelf() }.is_err() {
        // Running on as someone else is never acceptable.
        std::process::abort();
    }
    if opened.is_err() {
        return false;
    }
    let token = Owned(token);
    UserSid::of_token(token.0).is_ok_and(|them| me.same(&them))
}

/// True when process `pid` runs as the current user.
fn same_user(pid: u32) -> bool {
    match (UserSid::current(), UserSid::of_pid(pid)) {
        (Ok(me), Ok(them)) => me.same(&them),
        _ => false,
    }
}

pub(crate) fn default_endpoint() -> Result<PathBuf, String> {
    let sid = UserSid::current()
        .and_then(|s| s.string())
        .map_err(|e| format!("cannot read the current user: {e}"))?;
    Ok(PathBuf::from(format!(r"\\.\pipe\rbs-control-{sid}")))
}

fn wide(path: &Path) -> Vec<u16> {
    OsStr::new(path).encode_wide().chain(Some(0)).collect()
}

/// A security descriptor from `LocalAlloc`, freed when dropped.
struct Descriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is read-only after creation and freed once, in `drop`.
unsafe impl Send for Descriptor {}
// SAFETY: as above.
unsafe impl Sync for Descriptor {}

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by `ConvertStringSecurityDescriptorToSecurityDescriptorW`.
        unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

pub(crate) struct Listener {
    name: Vec<u16>,
    descriptor: Descriptor,
    /// The instance waiting for the next client: one always exists while serving, so
    /// the name stays ours.
    pending: Mutex<Option<Owned>>,
    connected: Owned,
    stop: Owned,
}

impl Listener {
    pub(crate) fn bind(path: &Path) -> Result<Listener, String> {
        let sid = UserSid::current()
            .and_then(|s| s.string())
            .map_err(|e| format!("cannot read the current user: {e}"))?;
        // Protected DACL: full access for the current user only.
        let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is NUL-terminated; `sd` receives an allocation owned by
        // `Descriptor`.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| format!("cannot make the pipe's access list: {e}"))?;
        let l = Listener {
            name: wide(path),
            descriptor: Descriptor(sd),
            pending: Mutex::new(None),
            connected: event().map_err(|e| e.to_string())?,
            stop: event().map_err(|e| e.to_string())?,
        };
        let first = l.instance(true).map_err(|e| {
            if e.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) {
                "another program already serves this endpoint".to_owned()
            } else {
                e.to_string()
            }
        })?;
        *l.pending() = Some(first);
        Ok(l)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Option<Owned>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn instance(&self, first: bool) -> io::Result<Owned> {
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0.0,
            bInheritHandle: false.into(),
        };
        let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
        if first {
            mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }
        // SAFETY: `name` is NUL-terminated; `sa` and the descriptor live for the call.
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(self.name.as_ptr()),
                mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                BUFFER,
                BUFFER,
                0,
                Some(&raw const sa),
            )
        };
        if h.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(Owned(h))
    }

    /// The next client: `Ok(None)` for one of another user, or when woken to stop.
    pub(crate) fn accept(&self) -> io::Result<Option<Box<dyn Stream>>> {
        let pipe = match self.pending().take() {
            Some(p) => p,
            None => self.instance(false)?,
        };
        let connect = overlapped(pipe.0, &self.connected, Some(&self.stop), INFINITE, |ov| {
            // SAFETY: `pipe` is a server instance; `ov` lives until the operation ends.
            unsafe { ConnectNamedPipe(pipe.0, Some(ov)) }
        });
        // The next instance before anything else, so the name stays ours.
        if let Ok(next) = self.instance(false) {
            *self.pending() = Some(next);
        }
        match connect {
            Ok(Some(_)) => {}
            // Woken to stop, or the client left at once.
            Ok(None) | Err(_) => return Ok(None),
        }
        if !client_is_current_user(pipe.0) {
            return Ok(None);
        }
        Ok(Some(Box::new(PipeIo::new(pipe)?)))
    }

    /// Makes a blocked `accept` return (the stop flag is set first).
    pub(crate) fn wake(&self) {
        // SAFETY: the event lives as long as the listener.
        let _ = unsafe { SetEvent(self.stop.0) };
    }

    /// After an accept error: pause briefly instead of spinning; `wake` ends the pause.
    pub(crate) fn wait_after_error(&self) -> bool {
        // SAFETY: the event lives as long as the listener.
        let _ = unsafe { WaitForSingleObject(self.stop.0, 100) };
        true
    }

    /// Closes the waiting instance: the name is gone once the connections are.
    pub(crate) fn remove(&self) {
        self.pending().take();
    }
}

/// One connected pipe, server or client side.
struct PipeIo {
    /// Closed by `finish` or, if that never ran, by `drop`: exactly once.
    pipe: std::mem::ManuallyDrop<Owned>,
    read_event: Owned,
    write_event: Owned,
    cancel: Owned,
    finished: AtomicBool,
}

impl PipeIo {
    fn new(pipe: Owned) -> io::Result<PipeIo> {
        Ok(PipeIo {
            pipe: std::mem::ManuallyDrop::new(pipe),
            read_event: event()?,
            write_event: event()?,
            cancel: event()?,
            finished: AtomicBool::new(false),
        })
    }

    fn read_for(&self, buf: &mut [u8], timeout_ms: u32) -> io::Result<Option<usize>> {
        let h = self.pipe.0;
        match overlapped(h, &self.read_event, Some(&self.cancel), timeout_ms, |ov| {
            // SAFETY: `buf` and `ov` live until the operation ends (`overlapped`
            // waits for it).
            unsafe { ReadFile(h, Some(buf), None, Some(ov)) }
        }) {
            Ok(n) => Ok(n.map(|n| n as usize)),
            Err(e) if ended(&e) => Ok(Some(0)),
            Err(e) => Err(e),
        }
    }

    fn write_all_now(&self, mut buf: &[u8]) -> io::Result<()> {
        let h = self.pipe.0;
        while !buf.is_empty() {
            let part = &buf[..buf.len().min(u32::MAX as usize)];
            let stall = WRITE_STALL.as_millis() as u32;
            let n = overlapped(h, &self.write_event, Some(&self.cancel), stall, |ov| {
                // SAFETY: `part` and `ov` live until the operation ends.
                unsafe { WriteFile(h, Some(part), None, Some(ov)) }
            })?
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))? as usize;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[n..];
        }
        Ok(())
    }
}

impl Stream for PipeIo {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_for(buf, INFINITE).map(|n| n.unwrap_or(0))
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        self.write_all_now(buf)
    }

    fn shutdown(&self) {
        // SAFETY: the event lives as long as `self`.
        let _ = unsafe { SetEvent(self.cancel.0) };
    }

    fn finish(&self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            // Both threads have ended: close the pipe now, so the client reads what
            // is left and then the end (`Drop` then skips it).
            // SAFETY: no thread uses the handle any more (the caller is the last).
            let _ = unsafe { CloseHandle(self.pipe.0) };
        }
    }
}

impl Drop for PipeIo {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::Acquire) {
            // SAFETY: `finish` did not close it, and nothing uses it after this.
            unsafe { std::mem::ManuallyDrop::drop(&mut self.pipe) };
        }
    }
}

/// The client side: a connection to the pipe whose server runs as the current user.
pub(crate) struct ClientIo(PipeIo);

impl ClientIo {
    pub(crate) fn connect(path: &Path, timeout: Duration) -> Result<ClientIo, crate::ClientError> {
        let name = wide(path);
        let flags: FILE_FLAGS_AND_ATTRIBUTES =
            FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION;
        let deadline = std::time::Instant::now() + timeout;
        let h = loop {
            // SAFETY: `name` is NUL-terminated; the handle is owned below.
            let opened = unsafe {
                CreateFileW(
                    PCWSTR(name.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    flags,
                    None,
                )
            };
            match opened {
                Ok(h) => break h,
                // Every instance is taken for a moment (the server makes the next one
                // right after a connect): wait for one, within the time given.
                Err(e) if is(&e, ERROR_PIPE_BUSY) => {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX - 1);
                    if ms == 0 {
                        return Err(crate::ClientError::NotRunning(os(e)));
                    }
                    // SAFETY: `name` is NUL-terminated.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), ms) };
                }
                Err(e) => return Err(crate::ClientError::NotRunning(os(e))),
            }
        };
        let pipe = Owned(h);
        let mut pid = 0;
        // SAFETY: `pipe` is a connected client handle.
        let known = unsafe { GetNamedPipeServerProcessId(pipe.0, &mut pid) }.is_ok();
        if !known || !same_user(pid) {
            return Err(crate::ClientError::WrongServer);
        }
        PipeIo::new(pipe)
            .map(ClientIo)
            .map_err(crate::ClientError::Io)
    }

    pub(crate) fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        self.0.write_all_now(buf)
    }

    /// Reads what is there, waiting at most `timeout` (`None`: no limit). `Ok(None)`
    /// when nothing came in time.
    pub(crate) fn read(
        &self,
        buf: &mut [u8],
        timeout: Option<Duration>,
    ) -> io::Result<Option<usize>> {
        let ms = timeout.map_or(INFINITE, |t| {
            u32::try_from(t.as_millis()).unwrap_or(INFINITE - 1).max(1)
        });
        self.0.read_for(buf, ms)
    }

    pub(crate) fn shutdown_write(&self) {
        // Pipes have no half-close; the close frame already said it.
    }
}
