//! The matte a session produces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rbs_core::media::{Size, SurfaceSync};
use rbs_core::time::Timestamp;

/// A release signal from the consumer of a GPU matte back to the session: the consumer
/// calls [`MatteRelease::release`] after the GPU work that reads the texture has been
/// submitted, and the session reuses the texture only after [`MatteRelease::is_released`]
/// (ADR-0179, spec rule 6).
#[derive(Debug, Clone, Default)]
pub struct MatteRelease(Arc<AtomicBool>);

impl MatteRelease {
    /// A new, unreleased signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the texture as no longer read by the consumer.
    pub fn release(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// True once the consumer released the texture.
    pub fn is_released(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// A matte held in a texture of the session's ring. The session owns the texture; the
/// consumer waits for `ready` in GPU queue order, never on the CPU, and signals
/// `release` when it is done.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct GpuMatte {
    /// An opaque handle of the host's texture pool slot the matte lives in.
    pub texture: u64,
    /// How the consumer waits for the producer.
    pub ready: SurfaceSync,
    /// The consumer's release signal.
    pub release: MatteRelease,
}

impl GpuMatte {
    /// Creates a GPU matte.
    pub fn new(texture: u64, ready: SurfaceSync, release: MatteRelease) -> Self {
        Self {
            texture,
            ready,
            release,
        }
    }
}

/// The pixels of a matte.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum MattePlane {
    /// One byte per pixel, 0 for background and 255 for the person, rows packed. The
    /// consumer may keep the `Arc` as long as it likes.
    Cpu(Arc<[u8]>),
    /// A texture of the session's ring.
    Gpu(GpuMatte),
}

/// One finished matte: one channel, values 0..1, `size` fixed per session and level and
/// with the aspect of the input.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct Matte {
    /// The master-clock time of the frame this matte was made from.
    pub timestamp: Timestamp,
    /// The matte's size in pixels.
    pub size: Size,
    /// The pixels.
    pub plane: MattePlane,
}

impl Matte {
    /// Creates a matte.
    pub fn new(timestamp: Timestamp, size: Size, plane: MattePlane) -> Self {
        Self {
            timestamp,
            size,
            plane,
        }
    }
}
