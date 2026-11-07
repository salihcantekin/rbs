//! The Media Foundation media source behind the RBS virtual camera on Windows
//! (ADR-0066 decision 9, ADR-0070; spec `docs/specs/virtual-camera.md`).
//!
//! Windows' Camera Frame Server loads this DLL in its own process (session 0,
//! `LocalService`) when an app opens "RBS Virtual Camera". The source announces one NV12
//! format, creates the shared frame section, and answers each sample request with the
//! newest frame RBS wrote into the ring, the last one again, or the still picture
//! "RBS VIRTUAL CAMERA IS OFF".
//!
//! - [`announced`], [`still`] and [`pump`] are platform-neutral and tested everywhere.
//! - The COM objects (Windows only) catch every panic at their entry points and return
//!   an `HRESULT`: a crash here would take Frame Server, and every camera of every app,
//!   down (ADR-0066 decision 9, "Containment").

pub mod announced;
pub mod clsid;
pub mod pump;
pub mod still;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{DllCanUnloadNow, DllGetClassObject};
