//! With `SLINT_LIVE_PREVIEW` set, compiles `crates/rbs-app/ui/roots/single.slint` (every
//! window and shared type, no library) into the live preview's wrappers; otherwise
//! generates nothing (ADR-0138 decision 5).

use std::path::PathBuf;
use std::time::SystemTime;

fn main() {
    println!("cargo:rerun-if-env-changed=SLINT_LIVE_PREVIEW");
    println!("cargo::rustc-check-cfg=cfg(rbs_ui_skip)");
    if std::env::var_os("SLINT_LIVE_PREVIEW").is_none() {
        println!("cargo::rustc-cfg=rbs_ui_skip");
        return;
    }
    println!("cargo:rerun-if-changed=../rbs-ui/tokens.toml");
    let modified = std::fs::metadata("../rbs-ui/tokens.toml")
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    if let Err(e) = rbs_ui::tokens_slint::write_token_files(&out, modified) {
        panic!("{e}");
    }
    let config = slint_build::CompilerConfiguration::new().with_include_paths(vec![out]);
    if let Err(e) = slint_build::compile_with_config("../rbs-app/ui/roots/single.slint", config) {
        panic!("{e}");
    }
}
