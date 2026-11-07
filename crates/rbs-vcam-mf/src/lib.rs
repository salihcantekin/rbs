//! The RBS virtual camera on Windows (ADR-0066, ADR-0070; spec
//! `docs/specs/virtual-camera.md`).
//!
//! - [`sender`] writes frames into the shared ring the media source
//!   (`rbs-vcam-mf-source`) reads. It is platform-neutral and tested everywhere.
//! - The factory (Windows only) adds the camera with one elevated step
//!   (`rbs-vcam-setup.exe`), creates it without elevation, changes its format by
//!   writing the user's format file, and opens sessions whose sender thread maps the
//!   ring.

pub mod sender;

/// Adapter id.
pub const ID: &str = "org.rbs.vcam.mf";

#[cfg(windows)]
mod win;
#[cfg(windows)]
#[doc(hidden)]
pub use win::setup;
#[cfg(windows)]
pub use win::{MfSession, MfVcamFactory};

/// Where the factory keeps the camera's symbolic link between runs (the app stores it
/// with its other virtual camera settings, ADR-0070 decision 5).
pub trait LinkStore: Send + Sync {
    /// The stored link, if any.
    fn load(&self) -> Option<String>;
    /// Stores `link`, or forgets it.
    fn save(&self, link: Option<&str>);
}

/// A [`LinkStore`] in one small text file.
#[derive(Debug, Clone)]
pub struct FileLinkStore {
    path: std::path::PathBuf,
}

impl FileLinkStore {
    /// A store in the file at `path`.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl LinkStore for FileLinkStore {
    fn load(&self) -> Option<String> {
        let s = std::fs::read_to_string(&self.path).ok()?;
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_owned())
    }

    fn save(&self, link: Option<&str>) {
        let _ = match link {
            Some(l) => std::fs::write(&self.path, l),
            None => match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                r => r,
            },
        };
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_file_link_store_round_trips_and_forgets() {
        let dir = std::env::temp_dir().join(format!("rbs-vcam-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = FileLinkStore::new(dir.join("link"));
        assert_eq!(store.load(), None);
        store.save(Some("\\\\?\\swd#vcam"));
        assert_eq!(store.load().as_deref(), Some("\\\\?\\swd#vcam"));
        store.save(None);
        store.save(None);
        assert_eq!(store.load(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
