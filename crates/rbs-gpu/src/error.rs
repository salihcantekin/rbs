//! Errors returned by `rbs-gpu`.
//!
//! Variants used on the render path carry only `Copy` data, so building them does
//! not allocate (hot-path rule, `docs/specs/gpu-compositor.md`).

use std::fmt;

use rbs_core::media::TextureId;

/// Every error `rbs-gpu` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuError {
    /// No adapter satisfies the request. Tests treat this as "hardware absent".
    NoAdapter,
    /// The adapter refused to create a device.
    DeviceRequest(String),
    /// The id refers to a texture that was released (its slot generation changed).
    StaleTexture(TextureId),
    /// The id was never handed out by this pool.
    UnknownTexture(TextureId),
    /// A texture does not have the format, size or usage the call needs.
    TargetMismatch(TextureId),
    /// More layers than the compositor reserved with `reserve_layers`.
    TooManyLayers {
        /// Layers passed to `render`.
        requested: u32,
        /// Layers reserved.
        reserved: u32,
    },
    /// The surface kind, sync kind or format cannot be imported on this backend.
    Unsupported(&'static str),
    /// The native surface lives on another adapter than the render device.
    WrongAdapter,
    /// Opening a native surface or fence failed (first sight only, not per frame).
    Import(String),
    /// Every surface of an exporter is still pending or held by its encoder.
    ExportBusy {
        /// Surfaces in the exporter's ring.
        surfaces: u32,
    },
    /// The device was lost; work on it is discarded.
    DeviceLost,
    /// Creating a native export surface failed (at creation only, not per frame).
    Export(&'static str),
    /// The pool has no free slot for a new texture.
    PoolFull {
        /// Slots the pool was created with.
        capacity: u32,
    },
    /// A size is zero, odd where it must be even, or above the device limit.
    InvalidSize {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
    },
    /// Copying a texture to the CPU failed.
    Readback(String),
}

impl fmt::Display for GpuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter => f.write_str("no GPU adapter satisfies the request"),
            Self::DeviceRequest(msg) => write!(f, "device request failed: {msg}"),
            Self::StaleTexture(id) => write!(f, "texture {id:?} was released"),
            Self::UnknownTexture(id) => write!(f, "texture {id:?} is not from this pool"),
            Self::TargetMismatch(id) => {
                write!(f, "texture {id:?} has the wrong format, size or usage")
            }
            Self::TooManyLayers {
                requested,
                reserved,
            } => write!(f, "{requested} layers requested, {reserved} reserved"),
            Self::Unsupported(what) => write!(f, "unsupported for native import: {what}"),
            Self::WrongAdapter => f.write_str("native surface is on another adapter"),
            Self::Import(msg) => write!(f, "native import failed: {msg}"),
            Self::ExportBusy { surfaces } => {
                write!(f, "all {surfaces} export surfaces are in use")
            }
            Self::DeviceLost => f.write_str("the GPU device was lost"),
            Self::Export(msg) => write!(f, "native export failed: {msg}"),
            Self::PoolFull { capacity } => write!(f, "texture pool full ({capacity} slots)"),
            Self::InvalidSize { width, height } => write!(f, "invalid size {width}x{height}"),
            Self::Readback(msg) => write!(f, "readback failed: {msg}"),
        }
    }
}

impl std::error::Error for GpuError {}
