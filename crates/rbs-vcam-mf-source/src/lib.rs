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

/// What the frame section's access list grants the user whose camera it is:
/// `READ_CONTROL | SECTION_MAP_READ | SECTION_MAP_WRITE`. The writer changes the slots'
/// state words, so it reads too, and it reads the section's owner to check that the
/// source made it (`READ_CONTROL`); it may not change the access list or the owner.
pub const SECTION_USER_ACCESS: u32 = 0x0002_0000 | 0x0004 | 0x0002;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{DllCanUnloadNow, DllGetClassObject};
