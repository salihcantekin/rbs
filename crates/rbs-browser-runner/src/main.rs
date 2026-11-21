//! Tree-root stand-in. A real CEF build replaces this executable and is a process tree.
//! This binary is not CEF and does not load CEF.

use rbs_browser_runner::HelperMode;

/// The engine version the test double says it was built for.
const TEST_MAJOR: u32 = 156;
const TEST_MINOR: u32 = 0;
const TEST_PATCH: u32 = 30;

fn main() {
    if std::env::args().any(|arg| arg == "--exit-immediately") {
        std::process::exit(2);
    }
    // Acts as the CEF runner does before it loads the engine (add-ons spec "Browser
    // engine consumer"): no runtime, or a damaged one, exits before the first beat;
    // a runtime whose marker names another version exits as a mismatch.
    if std::env::args().any(|arg| arg == "--needs-runtime") {
        use rbs_browser_ipc::exit;
        match rbs_browser_ipc::runtime::from_env() {
            Err(_) => std::process::exit(exit::RUNTIME_MISSING),
            Ok(rt) if !rt.library_dir.is_dir() => std::process::exit(exit::RUNTIME_MISSING),
            Ok(rt) if !rt.is_version(TEST_MAJOR, TEST_MINOR, TEST_PATCH) => {
                std::process::exit(exit::RUNTIME_MISMATCH)
            }
            Ok(_) => {}
        }
    }
    let mode = if std::env::args().any(|arg| arg == "--hold") {
        HelperMode::Run
    } else if std::env::args().any(|arg| arg == "--hang") {
        HelperMode::Hang
    } else if std::env::args().any(|arg| arg == "--still") {
        HelperMode::Still
    } else {
        HelperMode::Once
    };
    if std::env::args().any(|arg| arg == "--say") {
        // Lines the studio forwards to its log (ADR-0158).
        rbs_crash::errln!("rbs-browser-helper: started");
        rbs_crash::errln!("engine noise");
    }
    let tone = std::env::args().any(|arg| arg == "--tone");
    if rbs_browser_runner::helper_main_with(mode, tone).is_err() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod built {
    #[test]
    fn the_helper_binary_links() {}
}
