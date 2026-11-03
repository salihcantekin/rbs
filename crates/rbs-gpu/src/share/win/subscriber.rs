//! UI side of the cross-process preview on Windows: connects to the engine's pipe,
//! receives the shared texture handles and then frames (ADR-0036).

use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rbs_core::time::Timestamp;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::{
    GetNamedPipeServerProcessId, PIPE_READMODE_MESSAGE, SetNamedPipeHandleState, WaitNamedPipeW,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::PCWSTR;

use super::pipe::{self, Op, Owned, Poll};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::share::PreviewEndpoint;

/// Usage of the UI-side textures: what Slint's `Image::try_from(wgpu::Texture)` requires,
/// plus copies for tests.
pub const PREVIEW_TEXTURE_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::TEXTURE_BINDING
    .union(wgpu::TextureUsages::RENDER_ATTACHMENT)
    .union(wgpu::TextureUsages::COPY_SRC);

/// Format of the UI-side textures. The engine writes sRGB-encoded values into a
/// `R8G8B8A8_UNORM` texture (ADR-0036): flip-model and composition presentation take no
/// sRGB formats.
pub const PREVIEW_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// The pipe and its release writes, shared by the subscriber and its frames.
struct Link {
    pipe: Owned,
    generation: u32,
    /// Release writes; grows only if every one is still running (a stalled engine).
    #[allow(
        clippy::vec_box,
        reason = "the kernel holds pointers into each operation while it runs"
    )]
    writes: Mutex<Vec<Box<Op>>>,
}

impl Link {
    fn release(&self, slot: u32) {
        let Ok(mut writes) = self.writes.lock() else {
            return;
        };
        let pipe = self.pipe.raw();
        let i = match writes.iter().position(|w| !w.running()) {
            Some(i) => i,
            None => match Op::new() {
                Ok(op) => {
                    writes.push(op);
                    writes.len() - 1
                }
                Err(_) => return,
            },
        };
        let op = &mut writes[i];
        // SAFETY: the operation does not run (checked above); taking its old completion
        // does not wait.
        let _ = unsafe { op.poll(pipe) };
        let len = op.fill([pipe::ID_RELEASE, self.generation, slot]);
        // SAFETY: our open pipe; the boxed operation lives in `writes` until `Link` drops,
        // which cancels and waits first. A dead engine fails at once (nothing to release).
        let _ = unsafe { op.write(pipe, len) };
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Ok(writes) = self.writes.get_mut() {
            for w in writes.iter_mut() {
                // SAFETY: each write was started on our pipe; this waits until the kernel
                // no longer uses its memory.
                let _ = unsafe { w.cancel(self.pipe.raw()) };
            }
        }
    }
}

/// One frame of the engine's preview. Dropping it tells the engine that the UI no longer
/// reads its surface: drop it only after every read of the surface has completed (the
/// UI's own GPU work, or the desktop compositor's for a `PreviewLayer`).
pub struct PreviewFrame {
    link: Arc<Link>,
    slot: u32,
    sequence: u64,
    timestamp: Timestamp,
}

impl PreviewFrame {
    /// The surface (index into [`PreviewSubscriber::textures`]).
    pub fn slot(&self) -> usize {
        self.slot as usize
    }

    /// The engine's frame sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The engine time of the frame.
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }
}

impl Drop for PreviewFrame {
    fn drop(&mut self) {
        self.link.release(self.slot);
    }
}

impl std::fmt::Debug for PreviewFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewFrame")
            .field("slot", &self.slot)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// The UI's subscription to one engine preview. Lives in the UI process.
pub struct PreviewSubscriber {
    link: Arc<Link>,
    /// NT handles of the engine's textures, duplicated into this process by the engine.
    surfaces: Vec<Owned>,
    width: u32,
    height: u32,
    adapter_luid: u64,
    read: Box<Op>,
}

impl PreviewSubscriber {
    /// Opens the engine's pipe, subscribes with the endpoint's token and receives the
    /// surfaces. Blocks up to `timeout`, plus up to 1 s to finish the handshake once the
    /// surfaces have arrived; not for real-time threads.
    ///
    /// `GpuError::Disconnected` when the pipe does not exist, belongs to another process
    /// than the one its name says, does not answer (for example a wrong token) or closes.
    pub fn connect(endpoint: &PreviewEndpoint, timeout: Duration) -> Result<Self, GpuError> {
        let deadline = Instant::now() + timeout;
        let path = pipe::pipe_path(&endpoint.service);
        let pipe = loop {
            // SAFETY: `path` is NUL-terminated UTF-16. Identification-level impersonation
            // only: the engine may learn who we are but cannot act as us.
            let r = unsafe {
                CreateFileW(
                    PCWSTR(path.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            match r {
                Ok(h) => break Owned(h),
                Err(e) if pipe::win32(&e) == ERROR_PIPE_BUSY => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(GpuError::Disconnected("preview pipe is busy"));
                    }
                    let ms = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
                    // SAFETY: as above; waits at most `ms` for a free instance.
                    let _ = unsafe { WaitNamedPipeW(PCWSTR(path.as_ptr()), ms.max(1)) };
                }
                Err(e) if pipe::win32(&e) == ERROR_FILE_NOT_FOUND => {
                    return Err(GpuError::Disconnected("preview pipe does not exist"));
                }
                Err(_) => return Err(GpuError::Disconnected("cannot open the preview pipe")),
            }
        };
        let mode = PIPE_READMODE_MESSAGE;
        // SAFETY: our open pipe handle; only the read mode is set.
        unsafe { SetNamedPipeHandleState(pipe.raw(), Some(&mode), None, None) }
            .map_err(|_| GpuError::Disconnected("cannot set the pipe's message mode"))?;
        // The name carries the engine's process id: a pipe of another process under that
        // name is a squatter.
        let mut server = 0u32;
        // SAFETY: our open pipe; `server` is a valid out pointer.
        unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut server) }
            .map_err(|_| GpuError::Disconnected("cannot identify the preview engine"))?;
        if !name_has_pid(&endpoint.service, server) {
            return Err(GpuError::Disconnected(
                "preview pipe belongs to another process",
            ));
        }

        let mut write = Op::new().map_err(|_| GpuError::Disconnected("CreateEventW failed"))?;
        let token = pipe::split(endpoint.token);
        let len = write.fill([pipe::ID_SUBSCRIBE, pipe::VERSION, token[0], token[1]]);
        // SAFETY: our open pipe; `write` lives until it is waited for below.
        unsafe { write.write(pipe.raw(), len) }
            .map_err(|_| GpuError::Disconnected("preview engine does not accept"))?;
        if !matches!(wait(&mut write, pipe.raw(), deadline), Some(Poll::Done(_))) {
            // SAFETY: the write was started on `pipe`; waits until the kernel is done.
            let _ = unsafe { write.cancel(pipe.raw()) };
            return Err(GpuError::Disconnected("preview engine does not accept"));
        }

        let mut read = Op::new().map_err(|_| GpuError::Disconnected("CreateEventW failed"))?;
        // SAFETY: our open pipe; the boxed read is cancelled before it drops.
        unsafe { read.read(pipe.raw()) }
            .map_err(|_| GpuError::Disconnected("preview engine closed the pipe"))?;
        let n = match wait(&mut read, pipe.raw(), deadline) {
            Some(Poll::Done(n)) => n as usize,
            Some(_) => return Err(GpuError::Disconnected("preview engine closed the pipe")),
            None => {
                // Giving up before `ACCEPT` leaves the handle values to the engine: it
                // closes them in this process once the pipe closes, so nothing leaks even
                // if `SURFACES` arrived just now.
                // SAFETY: as above.
                let _ = unsafe { read.cancel(pipe.raw()) };
                return Err(GpuError::Disconnected("preview engine did not answer"));
            }
        };
        // Nothing here closes a received handle value before `ACCEPT` is written: until
        // then the engine owns them (it closes them if this process leaves without
        // accepting), so an invalid or foreign message cannot make us close unrelated
        // handles.
        let Some(w) = pipe::decode::<{ pipe::SURFACES_WORDS }>(read.data(n)) else {
            return Err(GpuError::Disconnected("preview surfaces are invalid"));
        };
        if w[0] != pipe::ID_SURFACES {
            return Err(GpuError::Disconnected("preview surfaces are invalid"));
        }
        if w[1] != pipe::VERSION {
            return Err(GpuError::Disconnected("preview protocol version mismatch"));
        }
        let count = w[5] as usize;
        let values: Vec<u64> = w[8..]
            .chunks_exact(2)
            .take(count.min(pipe::MAX_SURFACES))
            .map(|p| pipe::join([p[0], p[1]]))
            .collect();
        if !(2..=pipe::MAX_SURFACES).contains(&count)
            || w[3] == 0
            || w[4] == 0
            || values.contains(&0)
        {
            return Err(GpuError::Disconnected("preview surfaces are invalid"));
        }
        let generation = w[2];
        let len = write.fill([pipe::ID_ACCEPT, generation]);
        // SAFETY: our open pipe; the last write has completed, so `write` is not issued.
        let started = unsafe { write.write(pipe.raw(), len) }.is_ok();
        // The accept is tiny and the pipe buffer empty, so it completes at once; a short
        // grace past the deadline keeps a deadline that just ran out from losing it.
        let grace = deadline.max(Instant::now() + Duration::from_secs(1));
        let accepted = started
            && match wait(&mut write, pipe.raw(), grace) {
                Some(Poll::Done(_)) => true,
                Some(_) => false,
                // SAFETY: as above; the completion tells whether it was written.
                None => matches!(unsafe { write.cancel(pipe.raw()) }, Poll::Done(_)),
            };
        if !accepted {
            // Not accepted: the handles stay the engine's to close (or, if the engine is
            // gone, stay open until this process exits).
            return Err(GpuError::Disconnected("preview engine closed the pipe"));
        }
        // Accepted: the handle values are ours from here on and close with the subscriber.
        let surfaces: Vec<Owned> = values
            .into_iter()
            .map(|v| Owned(HANDLE(v as usize as *mut c_void)))
            .collect();
        Ok(Self {
            link: Arc::new(Link {
                pipe,
                generation,
                writes: Mutex::new(vec![write]),
            }),
            surfaces,
            width: w[3],
            height: w[4],
            adapter_luid: pipe::join([w[6], w[7]]),
            read,
        })
    }

    /// Preview size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Surfaces in the engine's ring.
    pub fn surfaces(&self) -> usize {
        self.surfaces.len()
    }

    /// LUID of the engine's adapter; the surfaces can be opened only on it.
    pub fn adapter_luid(&self) -> u64 {
        self.adapter_luid
    }

    /// NT handle of surface `slot` in this process (diagnostics and
    /// `PreviewLayer`); stays owned by the subscriber.
    pub fn surface_handle(&self, slot: usize) -> Option<u64> {
        self.surfaces.get(slot).map(|h| h.raw().0 as usize as u64)
    }

    /// The next frame, waiting up to `timeout` (zero only polls). `Ok(None)` on timeout,
    /// `GpuError::Disconnected` once the engine is gone.
    ///
    /// A UI that only shows the latest frame drops older ones as soon as their successor
    /// is shown and nothing reads them any more.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<PreviewFrame>, GpuError> {
        let deadline = Instant::now() + timeout;
        let pipe = self.link.pipe.raw();
        loop {
            if !self.read.issued() {
                // SAFETY: our open pipe; the read is not issued; the box lives in `self`,
                // whose drop cancels and waits first.
                unsafe { self.read.read(pipe) }
                    .map_err(|_| GpuError::Disconnected("preview engine closed the pipe"))?;
            }
            match wait(&mut self.read, pipe, deadline) {
                None => return Ok(None),
                Some(Poll::Pending) => return Ok(None),
                Some(Poll::Failed(e)) if e == ERROR_MORE_DATA => {
                    // Not a message of this protocol version; the rest is read next and
                    // ignored as well.
                    continue;
                }
                Some(Poll::Failed(_)) => {
                    return Err(GpuError::Disconnected("preview engine closed the pipe"));
                }
                Some(Poll::Done(n)) => {
                    let bytes = self.read.data(n as usize);
                    if let Some([pipe::ID_FRAME, generation, slot, s0, s1, t0, t1]) =
                        pipe::decode::<{ pipe::FRAME_WORDS }>(bytes)
                        && generation == self.link.generation
                        && (slot as usize) < self.surfaces.len()
                    {
                        return Ok(Some(PreviewFrame {
                            link: Arc::clone(&self.link),
                            slot,
                            sequence: pipe::join([s0, s1]),
                            timestamp: Timestamp::from_nanos(pipe::join([t0, t1])),
                        }));
                    }
                }
            }
        }
    }

    /// Opens every surface on `ctx`'s device as a [`PREVIEW_TEXTURE_FORMAT`] texture with
    /// [`PREVIEW_TEXTURE_USAGE`], in slot order. No copy: the textures are the engine's
    /// surfaces. `ctx` must be D3D12 on the engine's adapter. Call once after connecting;
    /// not for real-time threads.
    pub fn textures(&self, ctx: &GpuContext) -> Result<Vec<wgpu::Texture>, GpuError> {
        if ctx.adapter_luid() != Some(self.adapter_luid) {
            return Err(GpuError::WrongAdapter);
        }
        self.surfaces
            .iter()
            .map(|h| {
                let t = crate::interop::open_d3d12_shared(
                    ctx,
                    h.raw().0 as usize as u64,
                    PREVIEW_TEXTURE_USAGE,
                )?;
                if t.format() != PREVIEW_TEXTURE_FORMAT
                    || t.width() != self.width
                    || t.height() != self.height
                {
                    return Err(GpuError::Disconnected("preview surfaces are invalid"));
                }
                Ok(t)
            })
            .collect()
    }
}

impl Drop for PreviewSubscriber {
    fn drop(&mut self) {
        // SAFETY: the read was started on the link's pipe, which outlives this call (the
        // link is dropped after the fields); this waits until the kernel is done with it.
        let _ = unsafe { self.read.cancel(self.link.pipe.raw()) };
    }
}

/// Waits for `op` until `deadline`. `None` on timeout (the operation keeps running).
fn wait(op: &mut Op, pipe: HANDLE, deadline: Instant) -> Option<Poll> {
    let left = deadline.saturating_duration_since(Instant::now());
    let ms = left.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    // SAFETY: the operation's own event, valid while `op` lives.
    let r = unsafe { WaitForSingleObject(op.event(), ms) };
    if r != WAIT_OBJECT_0 {
        return None;
    }
    // SAFETY: `op` was started on `pipe`.
    match unsafe { op.poll(pipe) } {
        Poll::Pending => None,
        done => Some(done),
    }
}

/// Whether `service` (`org.rbs.preview.<pid>.<n>.<nonce>`) names process `pid`.
fn name_has_pid(service: &str, pid: u32) -> bool {
    service
        .strip_prefix("org.rbs.preview.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|p| p.parse::<u32>().ok())
        == Some(pid)
}

#[cfg(test)]
mod tests {
    use super::name_has_pid;

    #[test]
    fn service_name_carries_the_pid() {
        assert!(name_has_pid("org.rbs.preview.42.0.deadbeef", 42));
        assert!(!name_has_pid("org.rbs.preview.43.0.deadbeef", 42));
        assert!(!name_has_pid("evil.42.0", 42));
    }
}
