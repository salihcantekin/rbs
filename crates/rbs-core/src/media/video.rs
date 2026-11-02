//! Video formats and frames.
//!
//! A [`VideoFrame`] is a small value: a format, a master-clock timestamp, and a
//! [`FramePayload`] that names where the pixels are. Pixels are never inside the
//! frame value itself, so moving a frame through a queue copies a few dozen bytes.
//! See `docs/specs/media-types.md`.

use std::fmt;

use bytes::Bytes;

use super::MediaError;
use super::lease::Lease;
use crate::time::Timestamp;

/// Width and height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Size {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Size {
    /// Creates a size.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// True when either side is zero.
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Pixel layout of a frame or texture.
///
/// The `*Srgb` variants are 8-bit formats whose GPU views decode sRGB to linear on
/// sampling. Colour meaning (primaries, transfer, matrix, range) is carried separately
/// in [`ColorSpace`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit B, G, R, A, one plane.
    Bgra8,
    /// 8-bit B, G, R, A with sRGB-encoded colour channels.
    Bgra8Srgb,
    /// 8-bit R, G, B, A, one plane.
    Rgba8,
    /// 8-bit R, G, B, A with sRGB-encoded colour channels.
    Rgba8Srgb,
    /// 10-bit R, G, B and 2-bit A packed in 32 bits, one plane.
    Rgb10a2,
    /// 16-bit float R, G, B, A, one plane. The canvas working format (linear light).
    Rgba16Float,
    /// 8-bit 4:2:0: a Y plane and an interleaved UV plane. Encoder input.
    Nv12,
    /// 10-bit 4:2:0 in 16-bit words (data in the high bits): Y plane and UV plane.
    P010,
    /// 8-bit 4:2:0 with separate Y, U and V planes.
    I420,
    /// 8-bit 4:4:4 with separate Y, U and V planes.
    I444,
    /// 8-bit 4:2:2 packed as Y0 U Y1 V, one plane. Common for webcams.
    Yuy2,
    /// 8-bit 4:2:2 packed as U Y0 V Y1, one plane.
    Uyvy,
}

impl PixelFormat {
    /// Stable lowercase name, used in capability values and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            PixelFormat::Bgra8 => "bgra8",
            PixelFormat::Bgra8Srgb => "bgra8-srgb",
            PixelFormat::Rgba8 => "rgba8",
            PixelFormat::Rgba8Srgb => "rgba8-srgb",
            PixelFormat::Rgb10a2 => "rgb10a2",
            PixelFormat::Rgba16Float => "rgba16f",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::P010 => "p010",
            PixelFormat::I420 => "i420",
            PixelFormat::I444 => "i444",
            PixelFormat::Yuy2 => "yuy2",
            PixelFormat::Uyvy => "uyvy",
        }
    }

    /// True for Y'CbCr formats, which need a matrix and range to interpret.
    pub const fn is_yuv(self) -> bool {
        matches!(
            self,
            PixelFormat::Nv12
                | PixelFormat::P010
                | PixelFormat::I420
                | PixelFormat::I444
                | PixelFormat::Yuy2
                | PixelFormat::Uyvy
        )
    }

    /// Bits of precision per colour component.
    pub const fn bit_depth(self) -> u8 {
        match self {
            PixelFormat::Rgb10a2 | PixelFormat::P010 => 10,
            PixelFormat::Rgba16Float => 16,
            _ => 8,
        }
    }

    /// Number of memory planes.
    pub const fn plane_count(self) -> usize {
        match self {
            PixelFormat::Nv12 | PixelFormat::P010 => 2,
            PixelFormat::I420 | PixelFormat::I444 => 3,
            _ => 1,
        }
    }

    /// The smallest number of bytes in one row of `plane` for an image `width` pixels
    /// wide, or `None` if the plane does not exist. Chroma sizes round up.
    pub const fn min_row_bytes(self, plane: usize, width: u32) -> Option<u64> {
        if plane >= self.plane_count() {
            return None;
        }
        let w = width as u64;
        let half = w.div_ceil(2);
        Some(match self {
            PixelFormat::Bgra8
            | PixelFormat::Bgra8Srgb
            | PixelFormat::Rgba8
            | PixelFormat::Rgba8Srgb
            | PixelFormat::Rgb10a2 => w * 4,
            PixelFormat::Rgba16Float => w * 8,
            // Packed 4:2:2: four bytes per pair of pixels.
            PixelFormat::Yuy2 | PixelFormat::Uyvy => half * 4,
            PixelFormat::Nv12 => {
                if plane == 0 {
                    w
                } else {
                    half * 2
                }
            }
            PixelFormat::P010 => {
                if plane == 0 {
                    w * 2
                } else {
                    half * 4
                }
            }
            PixelFormat::I420 => {
                if plane == 0 {
                    w
                } else {
                    half
                }
            }
            PixelFormat::I444 => w,
        })
    }

    /// The number of rows in `plane` for an image `height` pixels high, or `None` if
    /// the plane does not exist.
    pub const fn plane_rows(self, plane: usize, height: u32) -> Option<u32> {
        if plane >= self.plane_count() {
            return None;
        }
        let subsampled = plane > 0
            && matches!(
                self,
                PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::I420
            );
        Some(if subsampled {
            height.div_ceil(2)
        } else {
            height
        })
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Colour primaries (the gamut).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorPrimaries {
    /// ITU-R BT.709, also used by sRGB.
    Bt709,
    /// ITU-R BT.601 625-line (PAL).
    Bt601Pal,
    /// ITU-R BT.601 525-line (SMPTE 170M, NTSC).
    Bt601Ntsc,
    /// ITU-R BT.2020 / BT.2100.
    Bt2020,
    /// Display P3 (DCI-P3 primaries, D65 white).
    DisplayP3,
}

/// Transfer function (how stored values map to light).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferFunction {
    /// Linear light. Used for composition.
    Linear,
    /// IEC 61966-2-1 sRGB.
    Srgb,
    /// ITU-R BT.709 / BT.1886 (also used by BT.601 and SDR BT.2020).
    Bt709,
    /// SMPTE ST 2084 perceptual quantiser (HDR10).
    Pq,
    /// ARIB STD-B67 hybrid log-gamma.
    Hlg,
}

/// Matrix from R'G'B' to Y'CbCr. `Rgb` for non-YUV formats.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatrixCoefficients {
    /// No matrix: the data is RGB.
    Rgb,
    /// ITU-R BT.601.
    Bt601,
    /// ITU-R BT.709.
    Bt709,
    /// ITU-R BT.2020 non-constant luminance.
    Bt2020Ncl,
}

/// Quantisation range of stored values.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorRange {
    /// Full range (0-255 for 8 bits).
    Full,
    /// Limited / video range (16-235 luma for 8 bits).
    Limited,
}

/// The complete colour description of a frame or link.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorSpace {
    /// Gamut.
    pub primaries: ColorPrimaries,
    /// Transfer function.
    pub transfer: TransferFunction,
    /// Y'CbCr matrix, or `Rgb`.
    pub matrix: MatrixCoefficients,
    /// Quantisation range.
    pub range: ColorRange,
}

impl ColorSpace {
    /// Creates a colour description.
    pub const fn new(
        primaries: ColorPrimaries,
        transfer: TransferFunction,
        matrix: MatrixCoefficients,
        range: ColorRange,
    ) -> Self {
        Self {
            primaries,
            transfer,
            matrix,
            range,
        }
    }

    /// sRGB, as produced by desktop capture and images.
    pub const SRGB: Self = Self::new(
        ColorPrimaries::Bt709,
        TransferFunction::Srgb,
        MatrixCoefficients::Rgb,
        ColorRange::Full,
    );
    /// Linear light with BT.709 primaries: the SDR canvas working space.
    pub const LINEAR_BT709: Self = Self::new(
        ColorPrimaries::Bt709,
        TransferFunction::Linear,
        MatrixCoefficients::Rgb,
        ColorRange::Full,
    );
    /// Linear light with BT.2020 primaries: the HDR canvas working space.
    pub const LINEAR_BT2020: Self = Self::new(
        ColorPrimaries::Bt2020,
        TransferFunction::Linear,
        MatrixCoefficients::Rgb,
        ColorRange::Full,
    );
    /// BT.709 limited range Y'CbCr, the usual SDR encoder output.
    pub const BT709_LIMITED: Self = Self::new(
        ColorPrimaries::Bt709,
        TransferFunction::Bt709,
        MatrixCoefficients::Bt709,
        ColorRange::Limited,
    );
    /// BT.709 full range Y'CbCr.
    pub const BT709_FULL: Self = Self::new(
        ColorPrimaries::Bt709,
        TransferFunction::Bt709,
        MatrixCoefficients::Bt709,
        ColorRange::Full,
    );
    /// BT.2100 PQ limited range Y'CbCr (HDR10).
    pub const BT2100_PQ: Self = Self::new(
        ColorPrimaries::Bt2020,
        TransferFunction::Pq,
        MatrixCoefficients::Bt2020Ncl,
        ColorRange::Limited,
    );
    /// BT.2100 HLG limited range Y'CbCr.
    pub const BT2100_HLG: Self = Self::new(
        ColorPrimaries::Bt2020,
        TransferFunction::Hlg,
        MatrixCoefficients::Bt2020Ncl,
        ColorRange::Limited,
    );
}

/// The picture format of a frame: size, pixel layout and colour.
///
/// Non-exhaustive so that later fields (for example HDR metadata or a visible
/// rectangle) can be added without breaking users; build it with [`VideoFormat::new`].
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VideoFormat {
    /// Size in pixels.
    pub size: Size,
    /// Pixel layout.
    pub pixel_format: PixelFormat,
    /// Colour description.
    pub color: ColorSpace,
}

impl VideoFormat {
    /// Creates a format.
    pub const fn new(size: Size, pixel_format: PixelFormat, color: ColorSpace) -> Self {
        Self {
            size,
            pixel_format,
            color,
        }
    }
}

/// Where the pixels of a frame live. Used in capability values (`input.memory`) and to
/// make CPU copies visible (principle P4).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryKind {
    /// System memory (CPU). The fallback path; its use is counted in metrics.
    System,
    /// A texture of the engine's own GPU device, from an `rbs-gpu` pool.
    Gpu,
    /// A Direct3D 11 resource.
    D3d11,
    /// A Direct3D 12 resource.
    D3d12,
    /// A Vulkan image.
    Vulkan,
    /// A Metal texture or IOSurface.
    Metal,
    /// Linux DMA-BUF file descriptors.
    DmaBuf,
}

impl MemoryKind {
    /// Stable lowercase name, used in capability values and logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            MemoryKind::System => "system",
            MemoryKind::Gpu => "gpu",
            MemoryKind::D3d11 => "d3d11",
            MemoryKind::D3d12 => "d3d12",
            MemoryKind::Vulkan => "vulkan",
            MemoryKind::Metal => "metal",
            MemoryKind::DmaBuf => "dmabuf",
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opaque id of a texture in an `rbs-gpu` pool: a slab slot plus a generation that
/// detects use of a slot after it was recycled. Only `rbs-gpu` gives these meaning,
/// and it rejects ids whose generation does not match the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId {
    slot: u32,
    generation: u32,
}

impl TextureId {
    /// Creates an id. Called by `rbs-gpu` pools.
    pub const fn new(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    /// Slab slot index.
    pub const fn slot(self) -> u32 {
        self.slot
    }

    /// Incremented each time the slot is reused.
    pub const fn generation(self) -> u32 {
        self.generation
    }
}

/// Which Direct3D API a shared handle was created for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum D3dApi {
    /// Direct3D 11.
    D3d11,
    /// Direct3D 12.
    D3d12,
}

/// One plane of a DMA-BUF surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DmaBufPlane {
    /// File descriptor. Owned by the producer; valid while the frame's lease lives.
    pub fd: i32,
    /// Byte offset of the plane in the buffer.
    pub offset: u32,
    /// Bytes per row.
    pub stride: u32,
}

/// Maximum planes of a DMA-BUF surface.
pub const MAX_DMABUF_PLANES: usize = 4;

/// A Linux DMA-BUF surface as delivered by PipeWire, V4L2 or a Wayland compositor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DmaBufSurface {
    fourcc: u32,
    modifier: u64,
    planes: [DmaBufPlane; MAX_DMABUF_PLANES],
    plane_count: u8,
}

impl DmaBufSurface {
    /// Creates a surface description with 1 to [`MAX_DMABUF_PLANES`] planes.
    pub fn new(fourcc: u32, modifier: u64, planes: &[DmaBufPlane]) -> Result<Self, MediaError> {
        if planes.is_empty() || planes.len() > MAX_DMABUF_PLANES {
            return Err(MediaError::PlaneCount {
                expected: MAX_DMABUF_PLANES,
                actual: planes.len(),
            });
        }
        let mut stored = [DmaBufPlane::default(); MAX_DMABUF_PLANES];
        stored[..planes.len()].copy_from_slice(planes);
        Ok(Self {
            fourcc,
            modifier,
            planes: stored,
            plane_count: planes.len() as u8,
        })
    }

    /// DRM fourcc code of the native layout.
    pub fn fourcc(&self) -> u32 {
        self.fourcc
    }

    /// DRM format modifier (tiling / compression).
    pub fn modifier(&self) -> u64 {
        self.modifier
    }

    /// The planes in use.
    pub fn planes(&self) -> &[DmaBufPlane] {
        &self.planes[..usize::from(self.plane_count)]
    }
}

/// A producer-owned native GPU surface, described as plain data.
///
/// Values are raw handle and pointer values exactly as the platform API returns them.
/// Core never dereferences them. A frame carrying one can only be built with the
/// `unsafe` [`VideoFrame::native`], whose caller guarantees validity for the life of
/// the frame's [`Lease`]; `rbs-gpu` imports it exactly once (see
/// `docs/specs/media-types.md#native-surfaces`).
///
/// Variants are non-exhaustive: build them with the constructor functions so fields can
/// be added later (for example a Vulkan memory handle) without breaking producers.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeSurface {
    /// Windows: an `ID3D11Texture2D*` and its array slice.
    #[non_exhaustive]
    D3d11Texture {
        /// The interface pointer value.
        texture: usize,
        /// Array slice to read.
        array_index: u32,
        /// LUID of the adapter the texture lives on; must match the engine adapter.
        adapter_luid: u64,
    },
    /// Windows: a shareable Direct3D resource handle.
    #[non_exhaustive]
    D3dSharedHandle {
        /// The handle value.
        handle: u64,
        /// True for an NT handle (`CreateSharedHandle`), false for a legacy handle.
        nt: bool,
        /// The API the resource was created with.
        api: D3dApi,
        /// LUID of the adapter the resource lives on.
        adapter_luid: u64,
    },
    /// Windows: an `ID3D12Resource*`.
    #[non_exhaustive]
    D3d12Resource {
        /// The interface pointer value.
        resource: usize,
        /// LUID of the adapter the resource lives on; must match the engine adapter.
        adapter_luid: u64,
    },
    /// macOS: an `IOSurfaceRef`.
    #[non_exhaustive]
    IoSurface {
        /// The reference value.
        surface: usize,
    },
    /// macOS: an `MTLTexture` object pointer.
    #[non_exhaustive]
    MetalTexture {
        /// The object pointer value.
        texture: usize,
        /// Registry id of the `MTLDevice` it was created on.
        device_registry_id: u64,
    },
    /// Linux: DMA-BUF planes.
    DmaBuf(DmaBufSurface),
}

impl NativeSurface {
    /// A Direct3D 11 texture.
    pub const fn d3d11_texture(texture: usize, array_index: u32, adapter_luid: u64) -> Self {
        Self::D3d11Texture {
            texture,
            array_index,
            adapter_luid,
        }
    }

    /// A shareable Direct3D resource handle.
    pub const fn d3d_shared_handle(handle: u64, nt: bool, api: D3dApi, adapter_luid: u64) -> Self {
        Self::D3dSharedHandle {
            handle,
            nt,
            api,
            adapter_luid,
        }
    }

    /// A Direct3D 12 resource.
    pub const fn d3d12_resource(resource: usize, adapter_luid: u64) -> Self {
        Self::D3d12Resource {
            resource,
            adapter_luid,
        }
    }

    /// An IOSurface.
    pub const fn io_surface(surface: usize) -> Self {
        Self::IoSurface { surface }
    }

    /// A Metal texture.
    pub const fn metal_texture(texture: usize, device_registry_id: u64) -> Self {
        Self::MetalTexture {
            texture,
            device_registry_id,
        }
    }

    /// The memory kind of this surface.
    pub const fn memory_kind(&self) -> MemoryKind {
        match self {
            NativeSurface::D3d11Texture { .. } => MemoryKind::D3d11,
            NativeSurface::D3dSharedHandle { api, .. } => match api {
                D3dApi::D3d11 => MemoryKind::D3d11,
                D3dApi::D3d12 => MemoryKind::D3d12,
            },
            NativeSurface::D3d12Resource { .. } => MemoryKind::D3d12,
            NativeSurface::IoSurface { .. } | NativeSurface::MetalTexture { .. } => {
                MemoryKind::Metal
            }
            NativeSurface::DmaBuf(_) => MemoryKind::DmaBuf,
        }
    }
}

/// How the single importer of a native surface (`rbs-gpu`) waits for the producer
/// before reading it. Release back to the producer is signalled by dropping the lease
/// after the importer's own GPU work on the surface has completed.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceSync {
    /// The surface is ready when the frame is delivered.
    Ready,
    /// DXGI keyed mutex: acquire with `acquire_key`, release with `release_key`.
    #[non_exhaustive]
    KeyedMutex {
        /// Key to acquire before reading.
        acquire_key: u64,
        /// Key to release after reading.
        release_key: u64,
    },
    /// A shared timeline fence (D3D12 or Vulkan timeline semaphore): wait until it
    /// reaches `value`.
    #[non_exhaustive]
    TimelineFence {
        /// Shared handle value of the fence.
        handle: u64,
        /// Value to wait for.
        value: u64,
    },
    /// A Linux sync file descriptor to wait on.
    #[non_exhaustive]
    SyncFile {
        /// The file descriptor; owned by the producer.
        fd: i32,
    },
}

impl SurfaceSync {
    /// A keyed mutex.
    pub const fn keyed_mutex(acquire_key: u64, release_key: u64) -> Self {
        Self::KeyedMutex {
            acquire_key,
            release_key,
        }
    }

    /// A shared timeline fence.
    pub const fn timeline_fence(handle: u64, value: u64) -> Self {
        Self::TimelineFence { handle, value }
    }

    /// A sync file.
    pub const fn sync_file(fd: i32) -> Self {
        Self::SyncFile { fd }
    }
}

/// Byte layout of one plane inside a [`SystemFrame`] buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PlaneLayout {
    /// Byte offset of the first row in the buffer.
    pub offset: usize,
    /// Bytes from the start of one row to the next.
    pub stride: usize,
}

/// Maximum planes of a pixel format.
pub const MAX_PLANES: usize = 3;

/// Pixels in system memory: one shared buffer, the layout of each plane in it, and the
/// format the layout was checked against.
///
/// This is the fallback path (software encoders, webcams that deliver CPU buffers,
/// readback). Its use is counted in metrics (principle P4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFrame {
    format: VideoFormat,
    data: Bytes,
    planes: [PlaneLayout; MAX_PLANES],
    plane_count: u8,
}

impl SystemFrame {
    /// Creates a system-memory frame after checking that `planes` matches `format`
    /// and that every row fits in `data`.
    pub fn new(
        format: VideoFormat,
        data: Bytes,
        planes: &[PlaneLayout],
    ) -> Result<Self, MediaError> {
        let pf = format.pixel_format;
        if format.size.is_empty() {
            return Err(MediaError::EmptySize);
        }
        if planes.len() != pf.plane_count() {
            return Err(MediaError::PlaneCount {
                expected: pf.plane_count(),
                actual: planes.len(),
            });
        }
        let mut stored = [PlaneLayout::default(); MAX_PLANES];
        for (index, layout) in planes.iter().enumerate() {
            let row = pf.min_row_bytes(index, format.size.width).unwrap_or(0);
            let rows = pf.plane_rows(index, format.size.height).unwrap_or(0);
            if (layout.stride as u64) < row {
                return Err(MediaError::StrideTooSmall {
                    plane: index,
                    stride: layout.stride,
                    min: row,
                });
            }
            // Last byte needed: offset + (rows - 1) * stride + row.
            let needed = (layout.offset as u64)
                .saturating_add(u64::from(rows - 1).saturating_mul(layout.stride as u64))
                .saturating_add(row);
            if needed > data.len() as u64 {
                return Err(MediaError::BufferTooSmall {
                    plane: index,
                    needed,
                    actual: data.len(),
                });
            }
            stored[index] = *layout;
        }
        Ok(Self {
            format,
            data,
            planes: stored,
            plane_count: planes.len() as u8,
        })
    }

    /// The format the layout was validated against.
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// The whole buffer.
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// The plane layouts in use.
    pub fn planes(&self) -> &[PlaneLayout] {
        &self.planes[..usize::from(self.plane_count)]
    }

    /// The bytes of plane `index` from its first row to the end of the buffer, and its
    /// stride. `None` if the plane does not exist.
    pub fn plane(&self, index: usize) -> Option<(&[u8], usize)> {
        let layout = self.planes().get(index)?;
        Some((self.data.get(layout.offset..)?, layout.stride))
    }
}

/// Where the pixels of a [`VideoFrame`] are.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
pub enum FramePayload {
    /// A texture of the engine GPU device, from an `rbs-gpu` pool. The default path.
    Texture(TextureId),
    /// A producer-owned native surface that `rbs-gpu` imports exactly once.
    #[non_exhaustive]
    Native {
        /// The surface.
        surface: NativeSurface,
        /// How to synchronise before reading it.
        sync: SurfaceSync,
    },
    /// System memory.
    System(SystemFrame),
}

impl FramePayload {
    /// Where the pixels live.
    pub const fn memory_kind(&self) -> MemoryKind {
        match self {
            FramePayload::Texture(_) => MemoryKind::Gpu,
            FramePayload::Native { surface, .. } => surface.memory_kind(),
            FramePayload::System(_) => MemoryKind::System,
        }
    }
}

/// One video frame: format, master-clock time and where its pixels are.
///
/// Fields are private so the format always matches the payload and a native surface is
/// never separated from the lease that keeps it valid.
///
/// A frame passed to a call (`&VideoFrame`) is lent for that call only. To keep a GPU
/// or system-memory frame longer (for example in an encoder queue), use
/// [`VideoFrame::share`]: the copy holds its own [`Lease`] reference. Frames with a
/// native payload cannot be shared; `rbs-gpu` imports them once and shares the result.
#[derive(Debug)]
pub struct VideoFrame {
    format: VideoFormat,
    timestamp: Timestamp,
    sequence: u64,
    source_time: Option<u64>,
    payload: FramePayload,
    lease: Option<Lease>,
}

impl VideoFrame {
    /// A frame in an `rbs-gpu` pool texture. `lease` keeps the slot from being reused;
    /// a stale id is still detected by its generation.
    pub fn texture(
        format: VideoFormat,
        timestamp: Timestamp,
        texture: TextureId,
        lease: Option<Lease>,
    ) -> Self {
        Self {
            format,
            timestamp,
            sequence: 0,
            source_time: None,
            payload: FramePayload::Texture(texture),
            lease,
        }
    }

    /// A frame in system memory. The format is the one the layout was validated
    /// against.
    pub fn system(timestamp: Timestamp, frame: SystemFrame, lease: Option<Lease>) -> Self {
        Self {
            format: frame.format,
            timestamp,
            sequence: 0,
            source_time: None,
            payload: FramePayload::System(frame),
            lease,
        }
    }

    /// A frame in a producer-owned native surface.
    ///
    /// # Safety
    ///
    /// The caller guarantees that every handle and pointer value in `surface` (and in
    /// `sync`) refers to a live object of the described kind and `format`, on the
    /// adapter it names, and stays valid until `lease` and every reference it hands
    /// out are released. The producer must not write to the surface during that time.
    pub unsafe fn native(
        format: VideoFormat,
        timestamp: Timestamp,
        surface: NativeSurface,
        sync: SurfaceSync,
        lease: Lease,
    ) -> Self {
        Self {
            format,
            timestamp,
            sequence: 0,
            source_time: None,
            payload: FramePayload::Native { surface, sync },
            lease: Some(lease),
        }
    }

    /// Sets the sequence number.
    pub fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = sequence;
        self
    }

    /// Sets the producer-domain timestamp (diagnostics only).
    pub fn with_source_time(mut self, source_nanos: u64) -> Self {
        self.source_time = Some(source_nanos);
        self
    }

    /// Picture format.
    pub fn format(&self) -> &VideoFormat {
        &self.format
    }

    /// Presentation time in master-clock time (ADR-0004).
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    /// Per-producer counter, incremented for every frame the producer emits, so gaps
    /// show dropped frames.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The producer's own timestamp in its clock domain, in nanoseconds, for
    /// diagnostics only. Never used for timing.
    pub fn source_time(&self) -> Option<u64> {
        self.source_time
    }

    /// Where the pixels are.
    pub fn payload(&self) -> &FramePayload {
        &self.payload
    }

    /// The lease keeping the pixel memory alive, if any.
    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    /// Where the pixels live.
    pub const fn memory_kind(&self) -> MemoryKind {
        self.payload.memory_kind()
    }

    /// A second reference to the same pixels, holding its own lease reference.
    /// `None` for native payloads, which have exactly one consumer (`rbs-gpu`).
    pub fn share(&self) -> Option<Self> {
        let payload = match &self.payload {
            FramePayload::Texture(id) => FramePayload::Texture(*id),
            FramePayload::System(frame) => FramePayload::System(frame.clone()),
            FramePayload::Native { .. } => return None,
        };
        Some(Self {
            format: self.format,
            timestamp: self.timestamp,
            sequence: self.sequence,
            source_time: self.source_time,
            payload,
            lease: self.lease.clone(),
        })
    }

    /// Splits a native frame into its surface, sync and lease, for the importer
    /// (`rbs-gpu`). Returns `None` (dropping the frame) if the payload is not native;
    /// check [`VideoFrame::payload`] first.
    pub fn into_native(self) -> Option<(NativeSurface, SurfaceSync, Lease)> {
        match (self.payload, self.lease) {
            (FramePayload::Native { surface, sync }, Some(lease)) => Some((surface, sync, lease)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::media::lease::tests::CountingOwner;

    fn nv12_1080p() -> VideoFormat {
        VideoFormat::new(
            Size::new(1920, 1080),
            PixelFormat::Nv12,
            ColorSpace::BT709_LIMITED,
        )
    }

    #[test]
    fn plane_geometry() {
        assert_eq!(PixelFormat::Nv12.min_row_bytes(0, 1921), Some(1921));
        assert_eq!(PixelFormat::Nv12.min_row_bytes(1, 1921), Some(1922));
        assert_eq!(PixelFormat::Nv12.plane_rows(1, 1081), Some(541));
        assert_eq!(PixelFormat::Nv12.min_row_bytes(2, 1920), None);
        assert_eq!(PixelFormat::P010.min_row_bytes(1, 1920), Some(3840));
        assert_eq!(PixelFormat::I420.min_row_bytes(2, 1920), Some(960));
        assert_eq!(PixelFormat::I444.plane_rows(2, 1080), Some(1080));
        assert_eq!(PixelFormat::Yuy2.min_row_bytes(0, 641), Some(1284));
        assert_eq!(PixelFormat::Rgba16Float.min_row_bytes(0, 10), Some(80));
        assert!(PixelFormat::P010.is_yuv());
        assert!(!PixelFormat::Bgra8Srgb.is_yuv());
        assert_eq!(PixelFormat::P010.bit_depth(), 10);
        assert_eq!(PixelFormat::Rgba16Float.to_string(), "rgba16f");
    }

    #[test]
    fn color_constants_are_consistent() {
        assert_eq!(ColorSpace::BT709_FULL.range, ColorRange::Full);
        assert_eq!(ColorSpace::BT709_FULL.matrix, MatrixCoefficients::Bt709);
        assert_eq!(ColorSpace::BT2100_HLG.transfer, TransferFunction::Hlg);
        assert_eq!(ColorSpace::BT2100_HLG.primaries, ColorPrimaries::Bt2020);
    }

    #[test]
    fn system_frame_validates_layout() {
        let format = nv12_1080p();
        let y = 2048 * 1080;
        let uv = 2048 * 540;
        let data = Bytes::from(vec![0_u8; y + uv]);
        let planes = [
            PlaneLayout {
                offset: 0,
                stride: 2048,
            },
            PlaneLayout {
                offset: y,
                stride: 2048,
            },
        ];
        let Ok(frame) = SystemFrame::new(format, data.clone(), &planes) else {
            panic!("valid layout");
        };
        assert_eq!(frame.planes().len(), 2);
        assert_eq!(
            frame.plane(1).map(|(bytes, stride)| (bytes.len(), stride)),
            Some((uv, 2048))
        );
        assert!(frame.plane(2).is_none());

        let video = VideoFrame::system(Timestamp::from_nanos(4), frame, None);
        assert_eq!(video.format(), &format);
        assert_eq!(video.memory_kind(), MemoryKind::System);

        assert_eq!(
            SystemFrame::new(format, data.clone(), &planes[..1]),
            Err(MediaError::PlaneCount {
                expected: 2,
                actual: 1
            })
        );
        let narrow = [
            planes[0],
            PlaneLayout {
                offset: y,
                stride: 100,
            },
        ];
        assert!(matches!(
            SystemFrame::new(format, data.clone(), &narrow),
            Err(MediaError::StrideTooSmall { plane: 1, .. })
        ));
        let short = data.slice(..y + 539 * 2048 + 1919);
        assert!(matches!(
            SystemFrame::new(format, short, &planes),
            Err(MediaError::BufferTooSmall { plane: 1, .. })
        ));
        let empty = VideoFormat::new(Size::new(0, 1080), PixelFormat::Nv12, ColorSpace::SRGB);
        assert_eq!(
            SystemFrame::new(empty, data, &planes),
            Err(MediaError::EmptySize)
        );
    }

    #[test]
    fn dmabuf_surface_plane_limits() {
        let plane = DmaBufPlane {
            fd: 3,
            offset: 0,
            stride: 7680,
        };
        let Ok(surface) = DmaBufSurface::new(0x3432_5241, 0, &[plane, plane]) else {
            panic!("two planes are valid");
        };
        assert_eq!(surface.planes().len(), 2);
        assert_eq!(surface.fourcc(), 0x3432_5241);
        assert!(DmaBufSurface::new(0, 0, &[]).is_err());
        assert!(DmaBufSurface::new(0, 0, &[plane; 5]).is_err());
        assert_eq!(
            NativeSurface::DmaBuf(surface).memory_kind(),
            MemoryKind::DmaBuf
        );
    }

    #[test]
    fn shared_frames_hold_their_own_lease() {
        let owner = Arc::new(CountingOwner::default());
        let gpu = VideoFrame::texture(
            nv12_1080p(),
            Timestamp::from_nanos(1),
            TextureId::new(3, 1),
            Some(owner.lease(2)),
        )
        .with_sequence(9)
        .with_source_time(77);
        assert_eq!(gpu.memory_kind(), MemoryKind::Gpu);
        assert_eq!((gpu.sequence(), gpu.source_time()), (9, Some(77)));

        let Some(queued) = gpu.share() else {
            panic!("texture frames can be shared");
        };
        assert_eq!(queued.payload(), gpu.payload());
        assert_eq!(owner.count(2), 2);
        drop(gpu);
        assert_eq!(owner.count(2), 1);
        drop(queued);
        assert_eq!(owner.count(2), 0);
    }

    #[test]
    fn native_frames_have_one_consumer() {
        let owner = Arc::new(CountingOwner::default());
        let surface = NativeSurface::d3d_shared_handle(0x44, true, D3dApi::D3d12, 7);
        // SAFETY: test only; the handle is never dereferenced.
        let frame = unsafe {
            VideoFrame::native(
                nv12_1080p(),
                Timestamp::from_nanos(1),
                surface,
                SurfaceSync::keyed_mutex(1, 0),
                owner.lease(1),
            )
        };
        assert_eq!(frame.memory_kind(), MemoryKind::D3d12);
        assert!(frame.share().is_none());
        let Some((s, sync, lease)) = frame.into_native() else {
            panic!("frame is native");
        };
        assert_eq!(s, surface);
        assert_eq!(sync, SurfaceSync::keyed_mutex(1, 0));
        assert_eq!(owner.count(1), 1);
        drop(lease);
        assert_eq!(owner.count(1), 0);

        let texture =
            VideoFrame::texture(nv12_1080p(), Timestamp::ZERO, TextureId::new(0, 0), None);
        assert!(texture.into_native().is_none());
    }

    #[test]
    fn video_frame_is_small() {
        // Frames move through queues by value; keep them within three cache lines.
        assert!(
            std::mem::size_of::<VideoFrame>() <= 192,
            "{}",
            std::mem::size_of::<VideoFrame>()
        );
    }
}
