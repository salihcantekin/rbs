//! Tree-root stand-in. A real CEF build replaces this executable and is a process tree.
//! This binary is not CEF and does not load CEF.

fn main() {
    let hold = std::env::args().any(|arg| arg == "--hold");
    if std::env::args().any(|arg| arg == "--exit-immediately") {
        std::process::exit(2);
    }
    if let Err(error) = rbs_browser_runner::helper_main(hold) {
        let _ = error;
        std::process::exit(1);
    }
}

#[cfg(test)]
mod built {
    #[test]
    fn the_helper_binary_links() {}
}
