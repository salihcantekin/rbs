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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorRange {
    /// Full range (0-255 for 8 bits).
    Full,
    /// Limited / video range (16-235 luma for 8 bits).
    Limited,
}

/// The complete colour description of a frame or link.
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
    /// sRGB, as produced by desktop capture and images.
    pub const SRGB: Self = Self {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Srgb,
        matrix: MatrixCoefficients::Rgb,
        range: ColorRange::Full,
    };
    /// Linear light with BT.709 primaries: the SDR canvas working space.
    pub const LINEAR_BT709: Self = Self {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Linear,
        matrix: MatrixCoefficients::Rgb,
        range: ColorRange::Full,
    };
    /// Linear light with BT.2020 primaries: the HDR canvas working space.
    pub const LINEAR_BT2020: Self = Self {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferFunction::Linear,
        matrix: MatrixCoefficients::Rgb,
        range: ColorRange::Full,
    };
    /// BT.709 limited range Y'CbCr, the usual SDR encoder output.
    pub const BT709_LIMITED: Self = Self {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        matrix: MatrixCoefficients::Bt709,
        range: ColorRange::Limited,
    };
    /// BT.709 full range Y'CbCr.
    pub const BT709_FULL: Self = Self {
        range: ColorRange::Full,
        ..Self::BT709_LIMITED
    };
    /// BT.2100 PQ limited range Y'CbCr (HDR10).
    pub const BT2100_PQ: Self = Self {
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferFunction::Pq,
        matrix: MatrixCoefficients::Bt2020Ncl,
        range: ColorRange::Limited,
    };
    /// BT.2100 HLG limited range Y'CbCr.
    pub const BT2100_HLG: Self = Self {
        transfer: TransferFunction::Hlg,
        ..Self::BT2100_PQ
    };
}

/// The picture format of a frame: size, pixel layout and colour.
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
    /// CUDA device memory.
    Cuda,
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
            MemoryKind::Cuda => "cuda",
        }
    }
}

impl fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Opaque id of a texture in an `rbs-gpu` pool: a slab slot plus a generation that
/// detects use of a slot after it was recycled. Only `rbs-gpu` gives these meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureId {
    /// Slab slot index.
    pub slot: u32,
    /// Incremented each time the slot is reused.
    pub generation: u32,
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
    /// DRM fourcc code of the native layout.
    pub fourcc: u32,
    /// DRM format modifier (tiling / compression).
    pub modifier: u64,
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

    /// The planes in use.
    pub fn planes(&self) -> &[DmaBufPlane] {
        &self.planes[..usize::from(self.plane_count)]
    }
}

/// A producer-owned native GPU surface, described as plain data.
///
/// Values are raw handle and pointer values exactly as the platform API returns them.
/// Core never dereferences them; `rbs-gpu` imports them (in its `unsafe` interop
/// module), and the producer keeps them valid until the frame's [`Lease`] is released.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeSurface {
    /// Windows: an `ID3D11Texture2D*` and its array slice, on a device that shares
    /// the engine adapter.
    D3d11Texture {
        /// The interface pointer value.
        texture: usize,
        /// Array slice to read.
        array_index: u32,
    },
    /// Windows: a shareable Direct3D resource handle.
    D3dSharedHandle {
        /// The handle value.
        handle: u64,
        /// True for an NT handle (`CreateSharedHandle`), false for a legacy handle.
        nt: bool,
        /// The API the resource was created with.
        api: D3dApi,
    },
    /// Windows: an `ID3D12Resource*`.
    D3d12Resource {
        /// The interface pointer value.
        resource: usize,
    },
    /// macOS: an `IOSurfaceRef`.
    IoSurface {
        /// The reference value.
        surface: usize,
    },
    /// macOS: an `MTLTexture` object pointer.
    MetalTexture {
        /// The object pointer value.
        texture: usize,
    },
    /// A `VkImage` handle on a device that shares the engine adapter.
    VulkanImage {
        /// The handle value.
        image: u64,
    },
    /// Linux: DMA-BUF planes.
    DmaBuf(DmaBufSurface),
    /// CUDA device memory.
    Cuda {
        /// Device pointer value.
        device_ptr: u64,
        /// Bytes per row.
        pitch: u32,
    },
}

impl NativeSurface {
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
            NativeSurface::VulkanImage { .. } => MemoryKind::Vulkan,
            NativeSurface::DmaBuf(_) => MemoryKind::DmaBuf,
            NativeSurface::Cuda { .. } => MemoryKind::Cuda,
        }
    }
}

/// How a consumer must synchronise with the producer before reading a surface.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SurfaceSync {
    /// The surface is ready when the frame is delivered.
    Ready,
    /// DXGI keyed mutex: acquire with `acquire_key`, release with `release_key`.
    KeyedMutex {
        /// Key to acquire before reading.
        acquire_key: u64,
        /// Key to release after reading.
        release_key: u64,
    },
    /// A shared timeline fence (D3D12 or Vulkan timeline semaphore): wait until it
    /// reaches `value`.
    TimelineFence {
        /// Shared handle value of the fence.
        handle: u64,
        /// Value to wait for.
        value: u64,
    },
    /// A Linux sync file descriptor to wait on.
    SyncFile {
        /// The file descriptor; owned by the producer.
        fd: i32,
    },
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

/// Pixels in system memory: one shared buffer and the layout of each plane in it.
///
/// This is the fallback path (software encoders, webcams that deliver CPU buffers,
/// readback). Its use is counted in metrics (principle P4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemFrame {
    data: Bytes,
    planes: [PlaneLayout; MAX_PLANES],
    plane_count: u8,
}

impl SystemFrame {
    /// Creates a system-memory frame after checking that `planes` matches `format`
    /// and that every row fits in `data`.
    pub fn new(
        format: &VideoFormat,
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
            data,
            planes: stored,
            plane_count: planes.len() as u8,
        })
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramePayload {
    /// A texture of the engine GPU device, from an `rbs-gpu` pool. The default path.
    Texture(TextureId),
    /// A producer-owned native surface that `rbs-gpu` imports.
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
/// A frame passed to a call (`&VideoFrame`) is lent for that call only. To keep it
/// longer (for example in an encoder queue), clone it: the clone holds its own
/// [`Lease`], so the producer cannot reuse the memory until it is dropped.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct VideoFrame {
    /// Picture format.
    pub format: VideoFormat,
    /// Presentation time in master-clock time (ADR-0004).
    pub timestamp: Timestamp,
    /// Per-producer counter, incremented for every frame the producer emits, so gaps
    /// show dropped frames.
    pub sequence: u64,
    /// The producer's own timestamp in its clock domain, in nanoseconds, for
    /// diagnostics only. Never used for timing.
    pub source_time: Option<u64>,
    /// Where the pixels are.
    pub payload: FramePayload,
    /// Keeps the pixel memory alive and returns it to its owner when the last clone
    /// is dropped. `None` when the payload needs no lifetime tracking.
    pub lease: Option<Lease>,
}

impl VideoFrame {
    /// Creates a frame with sequence 0, no source time and no lease.
    pub fn new(format: VideoFormat, timestamp: Timestamp, payload: FramePayload) -> Self {
        Self {
            format,
            timestamp,
            sequence: 0,
            source_time: None,
            payload,
            lease: None,
        }
    }

    /// Sets the sequence number.
    pub fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = sequence;
        self
    }

    /// Sets the producer-domain timestamp.
    pub fn with_source_time(mut self, source_nanos: u64) -> Self {
        self.source_time = Some(source_nanos);
        self
    }

    /// Attaches the lease that keeps the pixel memory alive.
    pub fn with_lease(mut self, lease: Lease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Where the pixels live.
    pub const fn memory_kind(&self) -> MemoryKind {
        self.payload.memory_kind()
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
        let Ok(frame) = SystemFrame::new(&format, data.clone(), &planes) else {
            panic!("valid layout");
        };
        assert_eq!(frame.planes().len(), 2);
        assert_eq!(
            frame.plane(1).map(|(bytes, stride)| (bytes.len(), stride)),
            Some((uv, 2048))
        );
        assert!(frame.plane(2).is_none());

        assert_eq!(
            SystemFrame::new(&format, data.clone(), &planes[..1]),
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
            SystemFrame::new(&format, data.clone(), &narrow),
            Err(MediaError::StrideTooSmall { plane: 1, .. })
        ));
        let short = data.slice(..y + 539 * 2048 + 1919);
        assert!(matches!(
            SystemFrame::new(&format, short, &planes),
            Err(MediaError::BufferTooSmall { plane: 1, .. })
        ));
        let empty = VideoFormat::new(Size::new(0, 1080), PixelFormat::Nv12, ColorSpace::SRGB);
        assert_eq!(
            SystemFrame::new(&empty, data, &planes),
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
        assert!(DmaBufSurface::new(0, 0, &[]).is_err());
        assert!(DmaBufSurface::new(0, 0, &[plane; 5]).is_err());
        assert_eq!(
            NativeSurface::DmaBuf(surface).memory_kind(),
            MemoryKind::DmaBuf
        );
    }

    #[test]
    fn frames_report_memory_kind_and_hold_leases() {
        let owner = Arc::new(CountingOwner::default());
        let gpu = VideoFrame::new(
            nv12_1080p(),
            Timestamp::from_nanos(1),
            FramePayload::Texture(TextureId {
                slot: 3,
                generation: 1,
            }),
        )
        .with_sequence(9)
        .with_source_time(77)
        .with_lease(owner.lease(2));
        assert_eq!(gpu.memory_kind(), MemoryKind::Gpu);
        assert_eq!((gpu.sequence, gpu.source_time), (9, Some(77)));

        let queued = gpu.clone();
        assert_eq!(owner.count(2), 2);
        drop(gpu);
        assert_eq!(owner.count(2), 1);
        drop(queued);
        assert_eq!(owner.count(2), 0);

        let native = FramePayload::Native {
            surface: NativeSurface::D3dSharedHandle {
                handle: 0x44,
                nt: true,
                api: D3dApi::D3d12,
            },
            sync: SurfaceSync::KeyedMutex {
                acquire_key: 1,
                release_key: 0,
            },
        };
        assert_eq!(native.memory_kind(), MemoryKind::D3d12);
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
