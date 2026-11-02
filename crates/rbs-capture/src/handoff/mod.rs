//! Building blocks for hardware adapters: the lock-free hand-off of captured surfaces
//! from an OS callback thread to the render thread ([`Ring`]), and deferred release of
//! surfaces whose handle values may still be cached by an importer ([`Graveyard`]).
//! See `docs/specs/capture-port.md`, "Hand-off helpers".
//!

mod graveyard;
mod ring;

pub use graveyard::{Grave, Graveyard};
pub use ring::{FrameMeta, Ring};
