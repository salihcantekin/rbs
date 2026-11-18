//! Add-ons: what RBS can use but does not ship (ADR-0145, [add-ons spec]).
//!
//! An add-on has three independent parts: an *origin* (today only the built-in
//! catalog, [`catalog`]), a *transfer* (download, size and hash checks, safe unpack:
//! [`AddOns::install`]) and a *kind* (today only [`Kind::Runtime`]).
//!
//! This crate holds every rule and no I/O of its own: HTTP comes in through
//! [`Fetch`], decompression through [`Unpack`] and file system operations through
//! [`AddOnFs`]. `rbs-add-ons-sys` implements them; tests use in-memory versions.
//!
//! Nothing here starts a thread, a timer or a watcher (P13). [`AddOns::install`]
//! runs on a thread the caller makes for that one install.
//!
//! [add-ons spec]: https://github.com/salihcantekin/rbs/blob/main/docs/specs/add-ons.md

mod archive;
pub mod catalog;
mod fetch;
mod fs;
mod id;
mod names;
mod status;
mod store;
mod version;

pub use archive::{ArchiveEntry, EntryKind, Format, Unpack, UnpackError};
pub use catalog::{Artifact, CatalogError, Entry, Kind, parse_entry};
pub use fetch::{Fetch, FetchError, Response};
pub use fs::{AddOnFs, FsError, LockGuard};
pub use id::AddOnId;
pub use names::RelPath;
pub use status::{FailReason, InstallError, Status};
pub use store::{AddOns, InstallControl, Installed, MARKER};
pub use version::Version;
