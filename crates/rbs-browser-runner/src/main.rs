//! Tree-root stand-in. A real CEF build replaces this executable and is a process tree.
//! This binary is not CEF and does not load CEF.

use rbs_browser_runner::HelperMode;

fn main() {
    if std::env::args().any(|arg| arg == "--exit-immediately") {
        std::process::exit(2);
    }
    let mode = if std::env::args().any(|arg| arg == "--hold") {
        HelperMode::Run
    } else if std::env::args().any(|arg| arg == "--hang") {
        HelperMode::Hang
    } else {
        HelperMode::Once
    };
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
