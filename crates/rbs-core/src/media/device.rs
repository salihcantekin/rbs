//! GPU device identity shared by capture and encoder ports (ADR-0147 decision 1).

/// A GPU device, identified the way OS interop APIs identify it.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GpuDeviceId {
    /// A Windows adapter LUID (DXGI, D3D11, D3D12, Vulkan `deviceLUID`): the bytes of the
    /// `LUID` struct in memory order (`LowPart` then `HighPart`, little-endian). Read as a
    /// little-endian `u64` it is the `adapter_luid` of a `NativeSurface`.
    Luid([u8; 8]),
    /// A Vulkan or CUDA device UUID.
    Uuid([u8; 16]),
    /// A Linux DRM render node, as `(major, minor)` device numbers.
    DrmRenderNode {
        /// Device major number.
        major: u32,
        /// Device minor number.
        minor: u32,
    },
    /// A macOS Metal device registry id.
    MetalRegistryId(u64),
}
