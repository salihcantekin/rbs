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
    config.system_probe = Some(crate::system_probe::factory());
    config.os_version = crate::system_probe::os_version();
    config.chapters = Some(std::sync::Arc::new(crate::chapters::MatroskaChapters));
    config.main_display = Some(main_display);
}

/// The Core Graphics framebuffer and point width; it does not list screen targets.
#[cfg(target_os = "macos")]
fn main_display(_: &rbs_sources::SourceTypes) -> Option<crate::studio_host::MainDisplay> {
    rbs_capture_sck::main_display().map(|(width, height, scale)| crate::studio_host::MainDisplay {
        size: (width, height),
        scale,
    })
}

#[cfg(not(target_os = "macos"))]
use crate::studio_host::display_from_default_screen as main_display;

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
