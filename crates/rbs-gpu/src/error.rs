//! Errors returned by `rbs-gpu`.
//!
//! Variants used on the render path carry only `Copy` data, so building them does
//! not allocate (hot-path rule, `docs/specs/gpu-compositor.md`).

use std::fmt;

use crate::pool::TextureId;

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
            Self::InvalidSize { width, height } => write!(f, "invalid size {width}x{height}"),
            Self::Readback(msg) => write!(f, "readback failed: {msg}"),
        }
    }
}

impl std::error::Error for GpuError {}
