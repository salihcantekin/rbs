//! Vulkan backend of the importer: single-plane Linux DMA-BUFs and sync-file waits.
//!
//! Marked low-level module (P11). Every `unsafe` block states its invariant.
//!
//! Sync files (`SurfaceSync::SyncFile`) are waited on by the GPU, never by the CPU (P6):
//! an already signalled file needs no work; otherwise a duplicate of the file is imported
//! into a pooled binary semaphore (`VK_KHR_external_semaphore_fd`, `SYNC_FD`, temporary
//! import) and staged as a wait of the queue's next submission, normally the render that
//! reads the frame. The extension is not enabled by wgpu, so devices that can use it are
//! opened through the HAL with it added ([`request_device`]).
//!
//! The import goes through wgpu-hal's `texture_from_dmabuf_fd`
//! (`VK_EXT_external_memory_dma_buf` with `VK_EXT_image_drm_format_modifier`). The cache key
//! is the DMA-BUF's inode: unique per buffer while the buffer lives, and the imported memory
//! keeps the buffer alive while the entry is cached, so a key never names another buffer.
//! File descriptor numbers are not used as keys because the OS reuses them.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use ash::{khr, vk};
use rbs_core::media::{DmaBufSurface, SurfaceSync};

use super::{Opened, USAGE};
use crate::context::{CAP_IMPORT_DMABUF, CAP_IMPORT_SYNC_FILE, GpuContext};
use crate::error::GpuError;
use crate::pool::TextureDesc;

type Vulkan = wgpu::hal::api::Vulkan;

/// DRM fourcc `AR24` (`DRM_FORMAT_ARGB8888`): bytes B, G, R, A in memory.
const DRM_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// DRM fourcc `AB24` (`DRM_FORMAT_ABGR8888`): bytes R, G, B, A in memory.
const DRM_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");
/// DRM fourcc `XR24` (`DRM_FORMAT_XRGB8888`): as `AR24` with an undefined fourth byte.
const DRM_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
/// DRM fourcc `XB24` (`DRM_FORMAT_XBGR8888`): as `AB24` with an undefined fourth byte.
const DRM_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");

/// Checks the surface and sync kind, and returns the cache key (the buffer's inode).
pub(super) fn validate(
    ctx: &GpuContext,
    surface: &DmaBufSurface,
    sync: SurfaceSync,
) -> Result<u64, GpuError> {
    if !ctx.has(&CAP_IMPORT_DMABUF) {
        return Err(GpuError::Unsupported(
            "DMA-BUF import needs a Vulkan device with external DMA-BUF memory",
        ));
    }
    match sync {
        SurfaceSync::Ready => {}
        SurfaceSync::SyncFile { fd, .. } => {
            if !ctx.has(&CAP_IMPORT_SYNC_FILE) {
                return Err(GpuError::Unsupported(
                    "sync files need VK_KHR_external_semaphore_fd with SYNC_FD import",
                ));
            }
            if fd < 0 {
                return Err(GpuError::Import("negative sync file descriptor".into()));
            }
        }
        _ => {
            return Err(GpuError::Unsupported(
                "sync kind on Vulkan; deliver Ready or SyncFile",
            ));
        }
    }
    let [plane] = surface.planes() else {
        return Err(GpuError::Unsupported("multi-plane DMA-BUF"));
    };
    map_format(surface.fourcc())?;
    if plane.fd < 0 {
        return Err(GpuError::Import("negative DMA-BUF file descriptor".into()));
    }
    // SAFETY: the producer keeps `fd` open while its lease is held (VideoFrame::native
    // contract) and the importer holds the lease. The `File` is never dropped, so the
    // descriptor stays owned by the producer; `metadata` only calls fstat.
    let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(plane.fd) });
    let meta = file
        .metadata()
        .map_err(|e| GpuError::Import(e.to_string()))?;
    Ok(meta.ino())
}

/// Imports the DMA-BUF on the render device without a copy.
pub(super) fn open(
    ctx: &GpuContext,
    surface: &DmaBufSurface,
    (width, height): (u32, u32),
) -> Result<Opened, GpuError> {
    let [plane] = surface.planes() else {
        return Err(GpuError::Unsupported("multi-plane DMA-BUF"));
    };
    let format = map_format(surface.fourcc())?;
    if width == 0 || height == 0 || width.max(height) > ctx.max_texture_dimension() {
        return Err(GpuError::InvalidSize { width, height });
    }
    // SAFETY: as in `validate`, the descriptor is open for this call. The duplicate is
    // ours: wgpu-hal takes ownership of it and closes it on failure.
    let fd = unsafe { BorrowedFd::borrow_raw(plane.fd) }
        .try_clone_to_owned()
        .map_err(|e| GpuError::Import(e.to_string()))?;

    let size = wgpu::Extent3d {
        width,
        height,
        depth_or_array_layers: 1,
    };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("rbs-gpu imported"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let hal_texture = {
        // SAFETY: the HAL device is only borrowed to create one texture; wgpu state is not
        // changed and the guard is dropped before the texture is registered.
        let hal = unsafe { ctx.device().as_hal::<Vulkan>() }
            .ok_or(GpuError::Unsupported("render device is not Vulkan"))?;
        // SAFETY: the device has VULKAN_EXTERNAL_MEMORY_DMA_BUF (checked in `validate`);
        // `fd` is a DMA-BUF holding one plane of `format` at `width x height` with the
        // producer's modifier, stride and offset (VideoFrame::native contract).
        unsafe {
            hal.texture_from_dmabuf_fd(
                fd,
                &hal_desc,
                surface.modifier(),
                u64::from(plane.stride),
                u64::from(plane.offset),
            )
        }
        .map_err(|e| GpuError::Import(e.to_string()))?
    };
    // SAFETY: the HAL texture was created on this device from `hal_desc`, which matches
    // the descriptor below. Its contents come from the producer; the image starts in
    // UNDEFINED layout, which UNINITIALIZED states (wgpu does not clear textures created
    // from HAL), as is usual for DMA-BUF imports.
    let texture = unsafe {
        ctx.device().create_texture_from_hal::<Vulkan>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("rbs-gpu imported"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: USAGE,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    };
    Ok(Opened {
        texture,
        chroma: None,
        desc: TextureDesc {
            width,
            height,
            format,
            usage: USAGE,
        },
    })
}

fn map_format(fourcc: u32) -> Result<wgpu::TextureFormat, GpuError> {
    match fourcc {
        // X formats alias the same layout; the source draws them with `Layer::opaque`.
        DRM_ARGB8888 | DRM_XRGB8888 => Ok(wgpu::TextureFormat::Bgra8Unorm),
        DRM_ABGR8888 | DRM_XBGR8888 => Ok(wgpu::TextureFormat::Rgba8Unorm),
        _ => Err(GpuError::Unsupported(
            "DMA-BUF format (ARGB8888, XRGB8888, ABGR8888, XBGR8888)",
        )),
    }
}

/// Most sync-file waits in flight at once (one per frame whose producer fence was still
/// pending at import, until the GPU passed the wait). A full pool rejects the frame.
pub(crate) const MAX_SYNC_WAITS: usize = 16;

/// Pooled binary semaphores for sync-file waits. Empty until the first pending sync file
/// (P13).
#[derive(Default)]
pub(super) struct SyncFiles {
    state: Option<SyncState>,
}

struct SyncState {
    /// Keeps the device alive so the semaphores can be destroyed on drop.
    device: wgpu::Device,
    loader: khr::external_semaphore_fd::Device,
    slots: Vec<Slot>,
    /// Slots not in use. Capacity [`MAX_SYNC_WAITS`], so pushing never allocates.
    free: Vec<usize>,
    /// Busy slots, oldest first. Waits complete in submission order, so only the oldest is
    /// polled. Capacity [`MAX_SYNC_WAITS`].
    busy: VecDeque<usize>,
    /// Slots whose wait is staged on the queue but may not have been submitted yet.
    staged: Vec<usize>,
}

struct Slot {
    semaphore: vk::Semaphore,
    /// A submission at or after the one that consumed the wait. Free once it completed.
    done_after: Option<wgpu::SubmissionIndex>,
}

impl SyncFiles {
    /// Makes the queue's next submission wait for `sync`. `Ready` and signalled sync files
    /// cost one `poll` system call at most and no GPU work.
    ///
    /// The wait is consumed by the next queue submission, normally the render that reads
    /// the imported frame, so the reading batch itself waits on the producer's fence.
    pub(super) fn wait(&mut self, ctx: &GpuContext, sync: SurfaceSync) -> Result<(), GpuError> {
        let SurfaceSync::SyncFile { fd, .. } = sync else {
            return Ok(());
        };
        if is_signalled(fd)? {
            return Ok(());
        }
        let state = match &mut self.state {
            Some(state) => state,
            empty => empty.insert(SyncState::new(ctx)?),
        };
        let slot = state.free_slot(ctx)?;
        let semaphore = state.slots[slot].semaphore;
        // SAFETY: the HAL queue is only borrowed to stage one wait; it is taken before the
        // import so a non-Vulkan queue leaves no payload behind.
        let Some(queue) = (unsafe { ctx.queue().as_hal::<Vulkan>() }) else {
            state.free.push(slot);
            return Err(GpuError::Unsupported("render queue is not Vulkan"));
        };

        // SAFETY: the producer keeps `fd` open while its lease is held (VideoFrame::native
        // contract) and the importer holds the lease. The duplicate is ours.
        let dup: OwnedFd = match unsafe { BorrowedFd::borrow_raw(fd) }.try_clone_to_owned() {
            Ok(dup) => dup,
            Err(e) => {
                state.free.push(slot);
                return Err(GpuError::Import(e.to_string()));
            }
        };
        let raw = dup.into_raw_fd();
        let info = vk::ImportSemaphoreFdInfoKHR::default()
            .semaphore(semaphore)
            .flags(vk::SemaphoreImportFlags::TEMPORARY)
            .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD)
            .fd(raw);
        // SAFETY: `semaphore` is a binary semaphore of this device with no pending signal or
        // wait (its last consuming submission completed, or it is new). SYNC_FD imports must
        // be temporary. On success Vulkan owns `raw`; on failure it stays ours.
        if let Err(e) = unsafe { state.loader.import_semaphore_fd(&info) } {
            // SAFETY: the import failed, so `raw` is still the duplicate created above.
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
            state.free.push(slot);
            return Err(GpuError::Import(format!("sync file import: {e}")));
        }
        queue.add_wait_semaphore(semaphore, None, vk::PipelineStageFlags::ALL_COMMANDS);
        state.staged.push(slot);
        Ok(())
    }

    /// Once per render tick, after the tick's submissions: moves staged waits that a
    /// submission consumed to the busy list. One empty submission gives them an index to
    /// poll (it only tracks completion; the ordering came from the consuming batch).
    pub(super) fn retire(&mut self, ctx: &GpuContext) {
        let Some(state) = &mut self.state else {
            return;
        };
        if state.staged.is_empty() {
            return;
        }
        // SAFETY: the HAL queue is only borrowed to look at the staged wait list.
        let Some(queue) = (unsafe { ctx.queue().as_hal::<Vulkan>() }) else {
            return;
        };
        // Consumed waits move to the end of `busy` (no allocation: capacity
        // MAX_SYNC_WAITS); unconsumed ones go back on the queue's list.
        let first = state.busy.len();
        let mut i = 0;
        while i < state.staged.len() {
            let slot = state.staged[i];
            let semaphore = state.slots[slot].semaphore;
            if queue.remove_wait_semaphore(semaphore) {
                // Not submitted yet (the frame was not rendered this tick): stage it again.
                // Only the render thread submits frames that read imported surfaces.
                queue.add_wait_semaphore(semaphore, None, vk::PipelineStageFlags::ALL_COMMANDS);
                i += 1;
            } else {
                state.busy.push_back(slot);
                state.staged.swap_remove(i);
            }
        }
        drop(queue);
        if state.busy.len() == first {
            return;
        }
        let index = ctx.queue().submit(std::iter::empty());
        for k in first..state.busy.len() {
            let slot = state.busy[k];
            state.slots[slot].done_after = Some(index.clone());
        }
    }
}

impl SyncState {
    fn new(ctx: &GpuContext) -> Result<Self, GpuError> {
        // SAFETY: the HAL device is only borrowed to load the extension's function table.
        let hal = unsafe { ctx.device().as_hal::<Vulkan>() }
            .ok_or(GpuError::Unsupported("render device is not Vulkan"))?;
        let instance = hal.shared_instance().raw_instance();
        // The extension was enabled at device creation (`CAP_IMPORT_SYNC_FILE`, checked by
        // `validate`), so its functions load.
        let loader = khr::external_semaphore_fd::Device::new(instance, hal.raw_device());
        Ok(Self {
            device: ctx.device().clone(),
            loader,
            slots: Vec::with_capacity(MAX_SYNC_WAITS),
            free: Vec::with_capacity(MAX_SYNC_WAITS),
            busy: VecDeque::with_capacity(MAX_SYNC_WAITS),
            staged: Vec::with_capacity(MAX_SYNC_WAITS),
        })
    }

    /// A free slot: a never-used one, the oldest busy one if its submission completed, or
    /// a new one while fewer than [`MAX_SYNC_WAITS`] exist. At most one poll per call.
    fn free_slot(&mut self, ctx: &GpuContext) -> Result<usize, GpuError> {
        if let Some(slot) = self.free.pop() {
            return Ok(slot);
        }
        if let Some(&oldest) = self.busy.front() {
            let done = self.slots[oldest].done_after.as_ref().is_some_and(|index| {
                ctx.device()
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index.clone()),
                        timeout: Some(Duration::ZERO),
                    })
                    .is_ok()
            });
            if done {
                self.busy.pop_front();
                self.slots[oldest].done_after = None;
                return Ok(oldest);
            }
        }
        if self.slots.len() >= MAX_SYNC_WAITS {
            return Err(GpuError::Unsupported(
                "more pending sync files in flight than MAX_SYNC_WAITS",
            ));
        }
        // SAFETY: the HAL device is only borrowed to create one semaphore.
        let hal = unsafe { ctx.device().as_hal::<Vulkan>() }
            .ok_or(GpuError::Unsupported("render device is not Vulkan"))?;
        // SAFETY: a plain binary semaphore on a live device; destroyed in `Drop`.
        let semaphore = unsafe {
            hal.raw_device()
                .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
        }
        .map_err(|e| GpuError::Import(format!("semaphore: {e}")))?;
        self.slots.push(Slot {
            semaphore,
            done_after: None,
        });
        Ok(self.slots.len() - 1)
    }
}

impl Drop for SyncState {
    /// Destroys the semaphores whose waits have passed, after a bounded wait for busy ones.
    /// Importers are dropped off the real-time path (source stop, shutdown). A semaphore
    /// the GPU may still use, or whose wait was never submitted, is leaked instead.
    fn drop(&mut self) {
        // SAFETY: the HAL device and queue are only borrowed to clean up objects created
        // on them.
        let Some(hal) = (unsafe { self.device.as_hal::<Vulkan>() }) else {
            return;
        };
        for (i, slot) in self.slots.iter().enumerate() {
            if self.staged.contains(&i) {
                // Possibly still on the queue's wait list: never destroy it.
                continue;
            }
            let idle = slot.done_after.as_ref().is_none_or(|index| {
                self.device
                    .poll(wgpu::PollType::Wait {
                        submission_index: Some(index.clone()),
                        timeout: Some(Duration::from_secs(1)),
                    })
                    .is_ok()
            });
            if idle {
                // SAFETY: created on this device in `free_slot`; not staged, and every
                // submission that waited on it has completed (checked just above).
                unsafe { hal.raw_device().destroy_semaphore(slot.semaphore, None) };
            }
        }
    }
}

/// Whether the sync file's fences have all signalled, without blocking (`poll` with a zero
/// timeout; a sync file is readable once signalled).
fn is_signalled(fd: i32) -> Result<bool, GpuError> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is a valid array of one entry for the call; `fd` is a descriptor the
    // producer keeps open (see `SyncFiles::wait`). A zero timeout never blocks.
    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
    if n < 0 {
        return Err(GpuError::Import(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(GpuError::Import("invalid sync file descriptor".into()));
    }
    Ok(pfd.revents & libc::POLLIN != 0)
}

/// Opens the device through the HAL, adding `VK_KHR_external_semaphore_fd`, when the adapter
/// can import sync files into semaphores and DMA-BUF import is requested. `None` otherwise
/// (the caller then uses `request_device`).
pub(super) fn request_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Option<Result<(wgpu::Device, wgpu::Queue, bool), GpuError>> {
    let (open, sync_file) = {
        // SAFETY: the HAL adapter is only borrowed to query support and open the device;
        // the guard is dropped before wgpu wraps the device.
        let hal = unsafe { adapter.as_hal::<Vulkan>() }?;
        // Only where the extension is useful: every other device takes wgpu's own
        // `request_device`, which also applies wgpu's strict-compliance filtering.
        if !desc
            .required_features
            .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
            || !sync_file_importable(&hal)
        {
            return None;
        }
        let callback = move |args: wgpu::hal::vulkan::CreateDeviceCallbackArgs<'_, '_, '_>| {
            if !args.extensions.contains(&khr::external_semaphore_fd::NAME) {
                args.extensions.push(khr::external_semaphore_fd::NAME);
            }
        };
        // SAFETY: the callback only adds an extension the physical device supports
        // (checked in `sync_file_importable`); it removes nothing and changes no feature.
        // Features and limits are the adapter's own, as `request_device` would pass.
        let open = unsafe {
            hal.open_with_callback(
                desc.required_features,
                &desc.required_limits,
                &desc.memory_hints,
                Some(Box::new(callback)),
            )
        };
        (open, true)
    };
    let open = match open {
        Ok(open) => open,
        Err(e) => return Some(Err(GpuError::DeviceRequest(e.to_string()))),
    };
    // SAFETY: `open` was created from this adapter's HAL adapter with the descriptor's
    // features and limits.
    let result = unsafe { adapter.create_device_from_hal::<Vulkan>(open, desc) }
        .map(|(device, queue)| (device, queue, sync_file))
        .map_err(|e| GpuError::DeviceRequest(e.to_string()));
    Some(result)
}

/// Whether the adapter supports `VK_KHR_external_semaphore_fd` and can import `SYNC_FD`
/// payloads into semaphores (Vulkan 1.1 instance and device).
fn sync_file_importable(hal: &wgpu::hal::vulkan::Adapter) -> bool {
    let caps = hal.physical_device_capabilities();
    let shared = hal.shared_instance();
    if !caps.supports_extension(khr::external_semaphore_fd::NAME)
        || shared.instance_api_version() < vk::API_VERSION_1_1
        || caps.properties().api_version < vk::API_VERSION_1_1
    {
        return false;
    }
    let info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::SYNC_FD);
    let mut props = vk::ExternalSemaphoreProperties::default();
    // SAFETY: Vulkan 1.1 core query (versions checked above) on the adapter's own physical
    // device with valid input and output structures.
    unsafe {
        shared
            .raw_instance()
            .get_physical_device_external_semaphore_properties(
                hal.raw_physical_device(),
                &info,
                &mut props,
            );
    }
    props
        .external_semaphore_features
        .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests fail by panicking.

    use std::io::Write;
    use std::os::fd::AsRawFd;

    use super::is_signalled;

    /// A sync file is readable once signalled; a pipe has the same `poll` behaviour.
    #[test]
    fn readable_descriptor_counts_as_signalled() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        assert!(!is_signalled(reader.as_raw_fd()).unwrap());
        writer.write_all(b"x").unwrap();
        assert!(is_signalled(reader.as_raw_fd()).unwrap());
    }

    #[test]
    fn closed_descriptor_is_an_error() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let fd = reader.as_raw_fd();
        drop(reader);
        assert!(is_signalled(fd).is_err());
    }
}
