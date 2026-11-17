//! Compiles `crates/rbs-app/ui/roots/main.slint` against the shared library `rbsui`
//! (ADR-0138 decisions 1 to 3).

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=SLINT_LIVE_PREVIEW");
    println!("cargo::rustc-check-cfg=cfg(rbs_ui_skip)");
    // The live preview generates code that cannot use a library: the single crate builds
    // everything then (ADR-0138 decision 5).
    if std::env::var_os("SLINT_LIVE_PREVIEW").is_some() {
        println!("cargo::rustc-cfg=rbs_ui_skip");
        return;
    }
    // The base's own `tokens.slint`: a second copy would be a second `Theme` (decision 3).
    let tokens = PathBuf::from(std::env::var_os("DEP_RBSUI_TOKENS_DIR").unwrap_or_default());
    let config = slint_build::CompilerConfiguration::new().with_include_paths(vec![tokens]);
    if let Err(e) = slint_build::compile_with_config("../rbs-app/ui/roots/main.slint", config) {
        panic!("{e}");
    }
}
