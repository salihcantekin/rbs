//! The Windows parts: the factory, the camera, the ring mapping and the setup tool.

mod camera;
mod elevate;
mod factory;
mod ring;
#[doc(hidden)]
pub mod setup;
mod sys;

pub use factory::{MfSession, MfVcamFactory};
#[cfg(test)]
mod tests;
