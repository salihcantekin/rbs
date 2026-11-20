//! Windows backend of the cross-process preview (ADR-0036): shared D3D12 textures whose NT
//! handles the engine duplicates into the UI process, the same four messages as on macOS
//! over a message-mode named pipe, and the UI-side layer on the composition swapchain.

#[cfg(feature = "preview-layer")]
mod layer;
mod pipe;
mod publisher;
#[cfg(feature = "preview-layer")]
mod solid;
mod subscriber;

#[cfg(feature = "preview-layer")]
pub use layer::{LayerRect, PreviewLayer};
pub use publisher::PreviewPublisher;
#[cfg(feature = "preview-layer")]
pub use solid::{Solid, SolidGroup, SolidLayer};
pub use subscriber::{
    PREVIEW_TEXTURE_FORMAT, PREVIEW_TEXTURE_USAGE, PreviewFrame, PreviewSubscriber,
};
