//! The OS side of add-ons (ADR-0145): [`SysFetch`] (HTTPS), [`SysUnpack`] (tar.gz,
//! tar.bz2) and [`SysFs`] (locks, link-safe creation, sync, free space).
//!
//! Every rule lives in `rbs-add-ons`; this crate only transfers, decodes and
//! performs. Only composition roots link it.

mod fetch;
mod fs;
mod unpack;

pub use fetch::SysFetch;
pub use fs::SysFs;
pub use unpack::SysUnpack;
