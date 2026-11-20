//! The platform's virtual camera (ADR-0066: Windows only so far).

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The platform's virtual camera adapter.
#[derive(Clone)]
pub struct VirtualCamera {
    /// The adapter.
    pub factory: Arc<dyn rbs_vcam::VirtualCameraFactory>,
    /// Settings › Virtual camera's "Remove completely": removes the camera and RBS's
    /// part from the OS, which may ask for administrator approval again. Blocks until
    /// the prompt is answered.
    pub remove_completely:
        Arc<dyn Fn() -> Result<rbs_vcam::InstallState, rbs_vcam::VcamError> + Send + Sync>,
}

/// Why this build has no virtual camera. The words for the user stay in the roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VcamUnavailable {
    /// No virtual camera adapter exists for this operating system yet.
    NotOnThisPlatform,
}

/// The file in the RBS settings folder `config` that keeps the camera's symbolic link,
/// shared by the app and `rbs-cli vcam`.
pub fn vcam_link_file(config: &Path) -> PathBuf {
    config.join("vcam-link.txt")
}

/// The virtual camera adapter of this platform, or why there is none. `config` is the
/// RBS settings folder; on Windows it is created if missing, so the link can be kept.
pub fn virtual_camera(config: &Path) -> Result<VirtualCamera, VcamUnavailable> {
    #[cfg(windows)]
    {
        // A missing folder only means the link is not kept; `status` then says not added.
        let _ = std::fs::create_dir_all(config);
        let links = rbs_vcam_mf::FileLinkStore::new(vcam_link_file(config));
        let mf = Arc::new(rbs_vcam_mf::MfVcamFactory::new(Box::new(links)));
        let remove = Arc::clone(&mf);
        Ok(VirtualCamera {
            factory: mf,
            remove_completely: Arc::new(move || remove.remove_completely()),
        })
    }
    #[cfg(not(windows))]
    {
        let _ = config;
        Err(VcamUnavailable::NotOnThisPlatform)
    }
}
