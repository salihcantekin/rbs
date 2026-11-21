//! The built-in add-on catalog and where add-ons are kept (ADR-0145, add-ons spec):
//! the studio host's part (`rbs_studio_host::add_ons`) and the OS's HTTPS, unpack and
//! file system, which stay in this composition root (ADR-0159 decision 2).

use std::sync::Arc;

pub use rbs_studio_host::add_ons::*;

/// The built-in catalog with the OS's HTTPS, unpack and file system, under
/// [`default_root`]. Opens nothing: `SysFetch` builds its agent here, which makes no
/// connection. A free function, as it stays in the app when [`AddOnsSetup`] moves to
/// the studio host crate (ADR-0159 decision 2).
pub fn system_setup() -> Option<AddOnsSetup> {
    Some(AddOnsSetup {
        catalog: catalog(target()),
        root: default_root()?,
        fs: Arc::new(rbs_add_ons_sys::SysFs),
        fetch: Arc::new(rbs_add_ons_sys::SysFetch::new()),
        unpack: Arc::new(rbs_add_ons_sys::SysUnpack),
        clock: Arc::new(rbs_core::time::MonotonicClock::new()),
    })
}
