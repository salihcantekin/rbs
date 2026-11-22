//! Device creation and adapter selection by capability (principle P3).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rbs_core::capability::{CapabilityKey, CapabilitySet, CapabilityValue};

use crate::error::GpuError;

/// `gpu.software`: the adapter renders on the CPU (WARP, lavapipe, llvmpipe).
pub const CAP_SOFTWARE: CapabilityKey = CapabilityKey::new_static("gpu.software");
/// `gpu.max_texture_dimension`: largest 2D texture side in texels.
pub const CAP_MAX_TEXTURE_DIMENSION: CapabilityKey =
    CapabilityKey::new_static("gpu.max_texture_dimension");
/// `gpu.timestamp_query`: GPU timestamps are available for per-pass timing.
pub const CAP_TIMESTAMP_QUERY: CapabilityKey = CapabilityKey::new_static("gpu.timestamp_query");

/// `gpu.nv12_surface`: the converter can write one multi-planar NV12 texture for encoders.
pub const CAP_NV12_SURFACE: CapabilityKey = CapabilityKey::new_static("gpu.nv12_surface");
/// `gpu.p010`: `R16Unorm` and `Rg16Unorm` render targets, so the converter can write P010
/// planes.
pub const CAP_P010: CapabilityKey = CapabilityKey::new_static("gpu.p010");
/// `gpu.p010_surface`: the converter can write one multi-planar P010 texture for encoders.
pub const CAP_P010_SURFACE: CapabilityKey = CapabilityKey::new_static("gpu.p010_surface");

/// `gpu.import.dmabuf`: the importer can alias Linux DMA-BUFs (Vulkan external memory with
/// DRM format modifiers).
pub const CAP_IMPORT_DMABUF: CapabilityKey = CapabilityKey::new_static("gpu.import.dmabuf");
/// `gpu.import.sync_file`: the importer can make the GPU wait on a Linux sync file
/// (`SurfaceSync::SyncFile`) before reading a DMA-BUF (Vulkan `VK_KHR_external_semaphore_fd`
/// with `SYNC_FD` import).
pub const CAP_IMPORT_SYNC_FILE: CapabilityKey = CapabilityKey::new_static("gpu.import.sync_file");

/// Which kind of adapter to prefer when several satisfy the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerPreference {
    /// Prefer integrated adapters.
    LowPower,
    /// Prefer discrete adapters.
    #[default]
    HighPerformance,
}

/// What the engine needs from a GPU adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuRequest {
    /// Which kind of adapter to prefer.
    pub power: PowerPreference,
    /// Whether CPU adapters (WARP, lavapipe, llvmpipe) are acceptable.
    pub allow_software: bool,
}

impl Default for GpuRequest {
    fn default() -> Self {
        Self {
            power: PowerPreference::HighPerformance,
            allow_software: false,
        }
    }
}

/// Counters shared with the device and readers on other threads.
#[derive(Debug, Default)]
pub struct GpuMetrics {
    frames_cpu_copied: AtomicU64,
    surfaces_opened: AtomicU64,
    device_lost: AtomicBool,
}

impl GpuMetrics {
    /// Number of textures copied to the CPU. Must stay 0 on the default path (P4).
    pub fn frames_cpu_copied(&self) -> u64 {
        self.frames_cpu_copied.load(Ordering::Relaxed)
    }

    /// Whether the device reported itself lost.
    pub fn device_lost(&self) -> bool {
        self.device_lost.load(Ordering::Relaxed)
    }

    /// Native surfaces opened by importers (first sight of a producer surface).
    pub fn surfaces_opened(&self) -> u64 {
        self.surfaces_opened.load(Ordering::Relaxed)
    }

    pub(crate) fn count_surface_opened(&self) {
        // Relaxed: a statistic.
        self.surfaces_opened.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_cpu_copy(&self) {
        // Relaxed: a statistic, no ordering with other memory is implied.
        self.frames_cpu_copied.fetch_add(1, Ordering::Relaxed);
    }
}

/// A GPU device with the objects every pass shares.
///
/// Created when the first canvas becomes Active; an engine with no active canvas
/// holds none (P13).
pub struct GpuContext {
    /// Keeps the device in the exit registry until the context drops (see `exit`).
    #[cfg(target_os = "linux")]
    pub(crate) _exit_drain: crate::exit::ExitDrain,
    /// Unique in the process. Every context has its own `wgpu::Instance`, and wgpu
    /// compares devices only by their id inside that instance, so devices of two
    /// contexts can compare equal. A cache shared between contexts keys on this.
    uid: u64,
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    capabilities: CapabilitySet,
    adapter_name: String,
    adapter_luid: Option<u64>,
    metrics: Arc<GpuMetrics>,
    pub(crate) sampled_layout: wgpu::BindGroupLayout,
    pub(crate) linear_sampler: wgpu::Sampler,
    /// A 4:2:0 picture's planes for the compositor's YUV pipelines (perf audit R-11 step
    /// b): Y' (binding 0), the linear clamp sampler (1) and CbCr (2).
    pub(crate) yuv_layout: wgpu::BindGroupLayout,
    /// The compositor's group 0 (layers, styles, 4:2:0 colour records), shared by every
    /// compositor of this device and by the wrap pipelines (ADR-0150 decision 7).
    pub(crate) layers_layout: wgpu::BindGroupLayout,
    /// The scrolling pipelines, built once on the first [`GpuContext::prepare_scrolling`]
    /// (ADR-0150 decision 7). Empty on a device where nothing ever scrolled (P13).
    pub(crate) wrap: std::sync::OnceLock<crate::compositor::WrapSet>,
}

impl GpuContext {
    /// Picks an adapter that satisfies `request` and creates a device on it.
    ///
    /// Returns [`GpuError::NoAdapter`] when nothing fits.
    pub fn new(request: &GpuRequest) -> Result<Self, GpuError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        // GLES has no storage buffers in the vertex stage on many drivers; the compositor needs them.
        desc.backends = desc.backends.intersection(wgpu::Backends::PRIMARY);
        let instance = wgpu::Instance::new(desc);
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::PRIMARY));

        let adapter = adapters
            .into_iter()
            .filter(|a| request.allow_software || !is_software(&a.get_info()))
            .filter(|a| {
                a.get_downlevel_capabilities()
                    .flags
                    .contains(wgpu::DownlevelFlags::VERTEX_STORAGE)
            })
            .min_by_key(|a| {
                let info = a.get_info();
                (rank(&info, request.power), backend_rank(info.backend))
            })
            .ok_or(GpuError::NoAdapter)?;
        let info = adapter.get_info();
        let context = Self::on_adapter(instance, adapter)?;
        // The adapter chosen, once per context (ADR-0158 spec, "What is logged").
        let level = if is_software(&info) {
            rbs_core::log::Level::Warn
        } else {
            rbs_core::log::Level::Info
        };
        // Metal reports no driver name or version: the part is left out then.
        let driver = [info.driver.as_str(), info.driver_info.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        let driver = if driver.is_empty() {
            String::new()
        } else {
            format!(", driver {driver}")
        };
        rbs_core::log_at!(
            level,
            rbs_core::log::Area::Gpu,
            "adapter {} ({:?}, {:?}{driver})",
            info.name,
            info.backend,
            info.device_type
        );
        Ok(context)
    }

    /// A second, independent device and queue on this context's adapter, with the same
    /// features and limits: the throwaway context of *Test this computer* (ADR-0087
    /// decision 9), so a measurement never touches the live device's pool or queue.
    ///
    /// On some backends (D3D12) both share one native device, so a device removal
    /// reaches both. Dropping it waits for its queue without a bound: a caller that
    /// cannot rule out a hung driver hands it to a thread it can abandon.
    pub fn sibling(&self) -> Result<Self, GpuError> {
        Self::on_adapter(self.instance.clone(), self.adapter.clone())
    }

    /// Creates the device on `adapter` and the objects every pass shares.
    fn on_adapter(instance: wgpu::Instance, adapter: wgpu::Adapter) -> Result<Self, GpuError> {
        let info = adapter.get_info();
        // Optional features that cost nothing until used.
        let features = adapter.features()
            & (wgpu::Features::TEXTURE_FORMAT_NV12
                | wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
                | wgpu::Features::TEXTURE_FORMAT_P010
                | wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES
                | wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF);
        // P010 planes are rendered as `R16Unorm` / `Rg16Unorm`. The 16-bit normalized feature
        // alone guarantees only storage use; render targets come from the adapter's own
        // format features, which validation honours only with the adapter-specific feature.
        let renderable = |format| {
            adapter
                .get_texture_format_features(format)
                .allowed_usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT)
        };
        let norm16 = features.contains(
            wgpu::Features::TEXTURE_FORMAT_16BIT_NORM
                | wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES,
        ) && renderable(wgpu::TextureFormat::R16Unorm)
            && renderable(wgpu::TextureFormat::Rg16Unorm);
        let limits = adapter.limits();
        let device_desc = wgpu::DeviceDescriptor {
            label: Some("rbs-gpu"),
            required_features: features,
            required_limits: limits.clone(),
            ..Default::default()
        };
        let (device, queue, sync_file) = request_device(&adapter, &device_desc)?;
        // Called once, by wgpu, when the device goes: lost (a driver reset, a removed
        // GPU) or destroyed when the context is dropped.
        device.set_device_lost_callback(|reason, message| match reason {
            wgpu::DeviceLostReason::Destroyed => {
                rbs_core::debug!(rbs_core::log::Area::Gpu, "device released");
            }
            _ => rbs_core::error!(rbs_core::log::Area::Gpu, "device lost: {message}"),
        });
        let capabilities = CapabilitySet::new()
            .with(CAP_SOFTWARE, CapabilityValue::Flag(is_software(&info)))
            .with(
                CAP_MAX_TEXTURE_DIMENSION,
                CapabilityValue::Int(i64::from(limits.max_texture_dimension_2d)),
            )
            .with(
                CAP_TIMESTAMP_QUERY,
                CapabilityValue::Flag(adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY)),
            )
            .with(
                CAP_IMPORT_DMABUF,
                CapabilityValue::Flag(
                    features.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
                ),
            )
            .with(
                CAP_IMPORT_SYNC_FILE,
                CapabilityValue::Flag(
                    sync_file && features.contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF),
                ),
            )
            .with(
                CAP_NV12_SURFACE,
                CapabilityValue::Flag(features.contains(wgpu::Features::TEXTURE_FORMAT_NV12)),
            )
            .with(CAP_P010, CapabilityValue::Flag(norm16))
            .with(
                CAP_P010_SURFACE,
                // The plane views of a P010 texture are 16-bit normalized formats.
                CapabilityValue::Flag(
                    norm16 && features.contains(wgpu::Features::TEXTURE_FORMAT_P010),
                ),
            );

        let metrics = Arc::new(GpuMetrics::default());
        let lost = Arc::clone(&metrics);
        device.set_device_lost_callback(move |_reason, _message| {
            lost.device_lost.store(true, Ordering::Relaxed);
        });

        let sampled_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu sampled texture"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    // Compute: the sync motion probe samples pool textures too (ADR-0126).
                    visibility: wgpu::ShaderStages::FRAGMENT.union(wgpu::ShaderStages::COMPUTE),
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT.union(wgpu::ShaderStages::COMPUTE),
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let plane = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let yuv_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rbs-gpu sampled 4:2:0 planes"),
            entries: &[
                plane(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                plane(2),
            ],
        });
        let linear_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rbs-gpu linear clamp"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let layers_layout = crate::compositor::layers_layout(&device);

        let adapter_luid = crate::interop::adapter_luid(&device);
        Ok(Self {
            #[cfg(target_os = "linux")]
            _exit_drain: crate::exit::ExitDrain::register(&device),
            uid: {
                static NEXT_UID: AtomicU64 = AtomicU64::new(0);
                // Relaxed: only uniqueness is needed, no ordering with other memory.
                NEXT_UID.fetch_add(1, Ordering::Relaxed)
            },
            instance,
            adapter,
            device,
            queue,
            capabilities,
            adapter_name: info.name,
            adapter_luid,
            metrics,
            sampled_layout,
            linear_sampler,
            yuv_layout,
            layers_layout,
            wrap: std::sync::OnceLock::new(),
        })
    }

    /// What the selected adapter can do.
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// An opaque key of the adapter and its driver: numeric vendor and device ids, the
    /// backend and the driver strings (empty on Metal), never the name. It changes
    /// when the GPU or its driver changes, so a measurement on it can be dropped
    /// (ADR-0087 step D2). Never branch on its content (P3).
    pub fn adapter_key(&self) -> String {
        let i = self.adapter.get_info();
        format!(
            "{:04x}:{:04x}:{:?}:{}:{}",
            i.vendor, i.device, i.backend, i.driver, i.driver_info
        )
    }

    /// The adapter's name, for diagnostics only. Never branch on it (P3).
    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    /// The Direct3D adapter LUID (`HighPart << 32 | LowPart`) when the device runs on
    /// D3D12. Producers create shared surfaces on this adapter.
    pub fn adapter_luid(&self) -> Option<u64> {
        self.adapter_luid
    }

    /// Shared counters.
    pub fn metrics(&self) -> &Arc<GpuMetrics> {
        &self.metrics
    }

    /// The wgpu instance the device was created from. A UI process hands it, with
    /// [`Self::adapter`], [`Self::device`] and [`Self::queue`], to its toolkit so that both
    /// render on one device (ADR-0028, decision 6).
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    /// Process-unique id of this context (see the field).
    pub(crate) fn uid(&self) -> u64 {
        self.uid
    }

    /// The selected adapter.
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// The wgpu device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// The wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// The bind group layout of a sampled texture (binding 0, filterable float 2D) and
    /// its sampler (binding 1), which every pool texture's
    /// [`PooledTexture::sampled_bind_group`](crate::PooledTexture::sampled_bind_group)
    /// matches. Pipelines that sample pool textures put it in their pipeline layout.
    pub fn sampled_layout(&self) -> &wgpu::BindGroupLayout {
        &self.sampled_layout
    }

    /// Builds the scrolling pipelines of this device if they do not exist yet (ADR-0150
    /// decision 7). Compiles shaders: call from the control thread before the first
    /// layer list with a scrolling layer is sent, never from `Node::process`. Cheap
    /// after the first call.
    pub fn prepare_scrolling(&self) {
        self.wrap
            .get_or_init(|| crate::compositor::WrapSet::new(self));
    }

    /// Whether [`GpuContext::prepare_scrolling`] has built the scrolling pipelines.
    pub fn scrolling_ready(&self) -> bool {
        self.wrap.get().is_some()
    }

    /// Lets the device retire finished work without blocking. Call once per render tick.
    pub fn poll(&self) {
        // A poll error only means the device is lost, which the lost callback reports.
        let _ = self.device.poll(wgpu::PollType::Poll);
    }

    /// Waits at most `timeout` for the work submitted so far to finish. `Ok(true)` when it
    /// finished, `Ok(false)` on timeout. Not for real-time threads.
    pub fn wait_until(&self, timeout: std::time::Duration) -> Result<bool, GpuError> {
        match self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(timeout),
        }) {
            Ok(_) => Ok(true),
            Err(wgpu::PollError::Timeout) => Ok(false),
            Err(e) => Err(GpuError::Readback(e.to_string())),
        }
    }

    /// Blocks until all submitted work finished. Not for real-time threads.
    pub fn wait_idle(&self) -> Result<(), GpuError> {
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .map(|_| ())
            .map_err(|e| GpuError::Readback(e.to_string()))
    }

    pub(crate) fn has(&self, key: &CapabilityKey) -> bool {
        self.capabilities.get(key) == Some(&CapabilityValue::Flag(true))
    }

    pub(crate) fn max_texture_dimension(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
}

/// Creates the device. Vulkan devices that can wait on sync files are opened through the
/// HAL so that extension can be enabled; the flag says whether it was.
fn request_device(
    adapter: &wgpu::Adapter,
    desc: &wgpu::DeviceDescriptor<'_>,
) -> Result<(wgpu::Device, wgpu::Queue, bool), GpuError> {
    #[cfg(target_os = "linux")]
    if let Some(opened) = crate::interop::vulkan_request_device(adapter, desc) {
        return opened;
    }
    let (device, queue) = pollster::block_on(adapter.request_device(desc))
        .map_err(|e| GpuError::DeviceRequest(e.to_string()))?;
    Ok((device, queue, false))
}

fn is_software(info: &wgpu::AdapterInfo) -> bool {
    info.device_type == wgpu::DeviceType::Cpu
}

/// Among adapters of the same rank, the backend whose native interop RBS implements comes
/// first: on Windows every shared surface (import, encoder export, cross-process preview)
/// is a D3D12 object, so D3D12 wins over Vulkan on the same GPU.
fn backend_rank(backend: wgpu::Backend) -> u8 {
    match backend {
        wgpu::Backend::Dx12 if cfg!(windows) => 0,
        _ => 1,
    }
}

fn rank(info: &wgpu::AdapterInfo, power: PowerPreference) -> u8 {
    use wgpu::DeviceType as D;
    match (power, info.device_type) {
        (PowerPreference::HighPerformance, D::DiscreteGpu) => 0,
        (PowerPreference::HighPerformance, D::IntegratedGpu) => 1,
        (PowerPreference::LowPower, D::IntegratedGpu) => 0,
        (PowerPreference::LowPower, D::DiscreteGpu) => 1,
        (_, D::VirtualGpu) => 2,
        (_, D::Other) => 3,
        (_, D::Cpu) => 4,
    }
}
