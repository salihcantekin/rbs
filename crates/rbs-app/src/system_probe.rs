//! The operating system's probe for recording protection (ADR-0090 decision 11),
//! handed to the studio host as [`HostConfig::system_probe`] so the host does not name
//! `rbs-system-probe` (ADR-0159 decision 4).
//!
//! [`HostConfig::system_probe`]: crate::studio_host::HostConfig::system_probe

use std::path::Path;

use rbs_record::protection::{Probed, SystemProbe, VolumeKey, VolumeSpace};

use crate::studio_host::protection::SystemProbeFactory;

/// The operating system's probe (`rbs-system-probe`).
pub fn factory() -> SystemProbeFactory {
    SystemProbeFactory::new(|| Box::new(OsProbe))
}

/// The operating system's name and version for the calibration key, if it can be read.
pub fn os_version() -> Option<String> {
    rbs_system_probe::os_version().ok()
}

/// `rbs-system-probe` behind the port (ADR-0090 decision 11).
struct OsProbe;

impl SystemProbe for OsProbe {
    fn volume_space(&mut self, folder: &Path) -> Probed<VolumeSpace> {
        match rbs_system_probe::volume_space(folder) {
            Ok(s) => Probed::Value(VolumeSpace {
                volume: VolumeKey(s.volume),
                free_bytes: s.free_bytes,
            }),
            Err(rbs_system_probe::ProbeError::Unsupported) => Probed::Unsupported,
            Err(rbs_system_probe::ProbeError::Failed(_)) => Probed::Unknown,
        }
    }

    fn available_memory(&mut self) -> Probed<u64> {
        match rbs_system_probe::available_memory() {
            Ok(b) => Probed::Value(b),
            Err(rbs_system_probe::ProbeError::Unsupported) => Probed::Unsupported,
            Err(rbs_system_probe::ProbeError::Failed(_)) => Probed::Unknown,
        }
    }
}
