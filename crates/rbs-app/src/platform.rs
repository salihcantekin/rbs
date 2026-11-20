//! The platform's adapters for the app: the shared list in `rbs-platform` (ADR-0160),
//! which `rbs-cli` uses too. Order is preference: a registry tries hardware adapters
//! first, then registration order.

use rbs_record::RecordAdapters;

pub use rbs_platform::VirtualCamera;

/// Every adapter this build has for the platform, not probed yet. Registration errors
/// (a duplicate id) are returned as messages; they mean a bug in the list.
/// `RBS_DEMO_SOURCES=1` adds the reference adapters in builds with `demo-sources`.
pub fn adapters() -> (RecordAdapters, Vec<String>) {
    let demo = std::env::var_os("RBS_DEMO_SOURCES").is_some_and(|v| v == "1");
    let registered =
        rbs_platform::adapters(&rbs_platform::Options::default().with_demo_sources(demo));
    (registered.adapters, registered.errors)
}

/// Fills the studio host's adapter seams (ADR-0159 decision 4): what the host would
/// otherwise name an adapter or an OS binding for.
pub fn host_seams(config: &mut crate::studio_host::HostConfig) {
    config.system_probe = Some(crate::studio_host::protection::SystemProbeFactory::os());
    config.chapters = Some(std::sync::Arc::new(crate::chapters::MatroskaChapters));
}

/// The virtual camera adapter of this platform, or why there is none, in the words
/// Settings › Virtual camera shows. `config` is the RBS settings folder, where the
/// camera's symbolic link is kept in the file `rbs-cli vcam` uses too.
pub fn virtual_camera(config: &std::path::Path) -> Result<VirtualCamera, &'static str> {
    rbs_platform::virtual_camera(config).map_err(|why| match why {
        rbs_platform::VcamUnavailable::NotOnThisPlatform if cfg!(target_os = "macos") => {
            "Not available on macOS yet"
        }
        _ => "Not available on Linux yet",
    })
}
