//! UI side of the cross-process preview: looks up the engine's surfaces and receives frames.

use std::ffi::CString;
use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;

use mach2::message::{
    MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND, MACH_MSG_TYPE_MOVE_SEND,
    MACH_MSGH_BITS_COMPLEX, MACH_RCV_TIMED_OUT, mach_msg_header_t,
};
use mach2::port::{MACH_PORT_NULL, mach_port_t};
use objc2_core_foundation::CFRetained;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{MTLPixelFormat, MTLTextureUsage};
use rbs_core::time::Timestamp;

use super::mach::{self, RecvBuf};
use super::{FOURCC_RGBA, PreviewEndpoint};
use crate::context::GpuContext;
use crate::error::GpuError;
use crate::interop::metal::{PlaneTexture, alias_plane};

/// Usage of the UI-side textures: what Slint's `Image::try_from(wgpu::Texture)` requires,
/// plus copies for tests.
pub const PREVIEW_TEXTURE_USAGE: wgpu::TextureUsages = wgpu::TextureUsages::TEXTURE_BINDING
    .union(wgpu::TextureUsages::RENDER_ATTACHMENT)
    .union(wgpu::TextureUsages::COPY_SRC);

/// Format of the UI-side textures.
pub const PREVIEW_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// The Mach rights a subscriber and its frames share.
struct Link {
    /// Send right to the engine's service.
    service: mach_port_t,
    /// Our receive right (frames, surfaces, dead-name notification).
    port: mach_port_t,
    /// Send right to the engine's session port for this subscription; releases go here.
    /// The engine sees "no senders" once it is dropped, which ends the subscription.
    session: mach_port_t,
    generation: u32,
}

impl Link {
    fn release(&self, slot: u32) {
        let mut msg = mach::Release {
            header: mach_msg_header_t {
                msgh_bits: mach::bits(MACH_MSG_TYPE_COPY_SEND, 0),
                msgh_size: size_of::<mach::Release>() as u32,
                msgh_remote_port: self.session,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: mach::ID_RELEASE,
            },
            generation: self.generation,
            slot,
        };
        // SAFETY: a filled `Release` of its own size; the session send right is copied.
        // Zero timeout: only this UI can send to the session port, it sends at most one
        // release per surface, and the port's queue limit is the maximum, so the queue is
        // never full; a dead engine fails at once (nothing to release then).
        let _ = unsafe { mach::send(&mut msg, 0) };
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        mach::deallocate(self.session);
        mach::deallocate(self.service);
        mach::destroy_receive(self.port);
    }
}

/// A retained IOSurface of the engine's ring.
struct SharedSurface(CFRetained<IOSurfaceRef>);

// SAFETY: IOSurface objects are thread-safe reference-counted objects; the subscriber
// only reads their properties and aliases them as textures.
unsafe impl Send for SharedSurface {}
// SAFETY: see `Send`.
unsafe impl Sync for SharedSurface {}

/// One frame of the engine's preview. Dropping it tells the engine that the UI no longer
/// reads its surface: drop it only after the UI's GPU work that reads the surface has
/// completed.
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
    surfaces: Vec<SharedSurface>,
    width: u32,
    height: u32,
    buf: Box<RecvBuf>,
}

impl PreviewSubscriber {
    /// Looks up the service, subscribes with the endpoint's token and receives the
    /// surfaces. Blocks up to `timeout`; not for real-time threads.
    ///
    /// `GpuError::Disconnected` when the service does not exist, does not answer (for
    /// example a wrong token) or dies.
    pub fn connect(endpoint: &PreviewEndpoint, timeout: Duration) -> Result<Self, GpuError> {
        let name = CString::new(endpoint.service.as_str())
            .map_err(|_| GpuError::Disconnected("service name contains NUL"))?;
        let service = mach::look_up(&name)?;
        let port = match mach::new_receive() {
            Ok(p) => p,
            Err(e) => {
                mach::deallocate(service);
                return Err(e);
            }
        };
        let mut link = Link {
            service,
            port,
            session: MACH_PORT_NULL,
            generation: 0,
        };
        mach::notify_dead_name(service, port)?;
        let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        let mut msg = mach::Subscribe {
            header: mach_msg_header_t {
                msgh_bits: mach::bits(MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND),
                msgh_size: size_of::<mach::Subscribe>() as u32,
                msgh_remote_port: service,
                msgh_local_port: port,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: mach::ID_SUBSCRIBE,
            },
            version: mach::VERSION,
            token: mach::split(endpoint.token),
        };
        // SAFETY: a filled `Subscribe` of its own size; the service send right is copied
        // and a send right to our receive port is made for the engine.
        if !mach::ok(unsafe { mach::send(&mut msg, ms) }) {
            return Err(GpuError::Disconnected("preview service does not accept"));
        }
        let mut buf = Box::new(RecvBuf::new());
        loop {
            let ret = mach::receive(port, &mut buf, ms);
            if ret == MACH_RCV_TIMED_OUT {
                return Err(GpuError::Disconnected("preview service did not answer"));
            }
            if !mach::ok(ret) {
                return Err(GpuError::Disconnected("receive failed"));
            }
            let header = buf.header();
            if header.msgh_id == mach::ID_DEAD_NAME {
                dead_name(&mut buf);
                return Err(GpuError::Disconnected("preview service died"));
            }
            if header.msgh_id != mach::ID_SURFACES || header.msgh_bits & MACH_MSGH_BITS_COMPLEX == 0
            {
                buf.destroy();
                continue;
            }
            let Some(s) = buf.read::<mach::Surfaces>() else {
                buf.destroy();
                continue;
            };
            // Takes every received right first, so an error below leaks none.
            let (session, surfaces) = take_rights(&s);
            link.session = session;
            if s.version != mach::VERSION {
                return Err(GpuError::Disconnected("preview protocol version mismatch"));
            }
            let surfaces = surfaces?;
            if link.session == MACH_PORT_NULL {
                return Err(GpuError::Disconnected("preview session port missing"));
            }
            link.generation = s.generation;
            return Ok(Self {
                link: Arc::new(link),
                surfaces,
                width: s.width,
                height: s.height,
                buf,
            });
        }
    }

    /// Preview size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Surfaces in the engine's ring.
    pub fn surfaces(&self) -> usize {
        self.surfaces.len()
    }

    /// The surfaces in slot order, retained (for [`super::PreviewLayer`]).
    #[cfg(target_os = "macos")]
    pub(super) fn io_surfaces(&self) -> Vec<CFRetained<IOSurfaceRef>> {
        self.surfaces.iter().map(|s| s.0.clone()).collect()
    }

    /// Global IOSurface ID of surface `slot` (diagnostics and tests).
    pub fn surface_id(&self, slot: usize) -> Option<u32> {
        self.surfaces.get(slot).map(|s| s.0.id())
    }

    /// The next frame, waiting up to `timeout` (zero only polls). `Ok(None)` on timeout,
    /// `GpuError::Disconnected` once the engine is gone.
    ///
    /// A UI that only shows the latest frame drops older ones as soon as their successor
    /// is shown and its own reads are done.
    pub fn recv(&mut self, timeout: Duration) -> Result<Option<PreviewFrame>, GpuError> {
        let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        loop {
            let ret = mach::receive(self.link.port, &mut self.buf, ms);
            if ret == MACH_RCV_TIMED_OUT {
                return Ok(None);
            }
            if !mach::ok(ret) {
                return Err(GpuError::Disconnected("receive failed"));
            }
            let header = self.buf.header();
            if header.msgh_id == mach::ID_DEAD_NAME {
                dead_name(&mut self.buf);
                return Err(GpuError::Disconnected("preview service died"));
            }
            if header.msgh_id == mach::ID_FRAME
                && header.msgh_bits & MACH_MSGH_BITS_COMPLEX == 0
                && let Some(f) = self.buf.read::<mach::Frame>()
                && f.generation == self.link.generation
                && (f.slot as usize) < self.surfaces.len()
            {
                return Ok(Some(PreviewFrame {
                    link: Arc::clone(&self.link),
                    slot: f.slot,
                    sequence: mach::join(f.sequence),
                    timestamp: Timestamp::from_nanos(mach::join(f.timestamp)),
                }));
            }
            self.buf.destroy();
        }
    }

    /// Aliases every surface on `ctx`'s device as a [`PREVIEW_TEXTURE_FORMAT`] texture
    /// with [`PREVIEW_TEXTURE_USAGE`], in slot order. No copy: the textures are the
    /// engine's surfaces. Call once after connecting; not for real-time threads.
    pub fn textures(&self, ctx: &GpuContext) -> Result<Vec<wgpu::Texture>, GpuError> {
        self.surfaces
            .iter()
            .map(|s| {
                alias_plane(
                    ctx,
                    &s.0,
                    0,
                    PlaneTexture {
                        format: PREVIEW_TEXTURE_FORMAT,
                        mtl_format: MTLPixelFormat::RGBA8Unorm_sRGB,
                        width: self.width,
                        height: self.height,
                        mtl_usage: MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget,
                        usage: PREVIEW_TEXTURE_USAGE,
                    },
                )
            })
            .collect()
    }
}

/// Drops the extra dead-name reference a dead-name notification carries.
fn dead_name(buf: &mut RecvBuf) {
    if let Some(n) = buf.read::<mach::DeadName>() {
        mach::deallocate(n.name);
    }
}

/// Takes the session send right of a `SURFACES` message (or null), looks up its surfaces
/// and drops the surfaces' received send rights.
fn take_rights(s: &mach::Surfaces) -> (mach_port_t, Result<Vec<SharedSurface>, GpuError>) {
    let d = s.ports[0];
    let session = if u32::from(d.disposition) == MACH_MSG_TYPE_MOVE_SEND {
        d.name
    } else {
        mach::deallocate(d.name);
        MACH_PORT_NULL
    };
    (session, take_surfaces(s))
}

/// Looks up the surfaces of a `SURFACES` message and drops the received send rights.
fn take_surfaces(s: &mach::Surfaces) -> Result<Vec<SharedSurface>, GpuError> {
    let count = s.count as usize;
    let mut out = Vec::with_capacity(count.min(mach::MAX_SURFACES));
    let mut failed = false;
    for (i, d) in s.ports[1..].iter().enumerate() {
        let name = d.name;
        if name == MACH_PORT_NULL {
            continue;
        }
        if i < count && u32::from(d.disposition) == MACH_MSG_TYPE_MOVE_SEND && !failed {
            match IOSurfaceRef::lookup_from_mach_port(name) {
                Some(io)
                    if io.pixel_format() == FOURCC_RGBA
                        && io.width() == s.width as usize
                        && io.height() == s.height as usize =>
                {
                    out.push(SharedSurface(io));
                }
                _ => failed = true,
            }
        }
        // Every received right is ours to drop; the IOSurface keeps its own reference.
        mach::deallocate(name);
    }
    if failed || out.len() != count || count < 2 {
        return Err(GpuError::Disconnected("preview surfaces are invalid"));
    }
    Ok(out)
}
