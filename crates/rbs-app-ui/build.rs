//! Generates the token setters (`tokens_apply.rs`, rule U6) and picks where the UI types
//! come from (ADR-0138 decision 5): the split crates, or with the UI live preview the
//! single crate.

use std::path::PathBuf;
use std::time::SystemTime;

/// The `live-preview` feature and `SLINT_LIVE_PREVIEW` work together. The variable
/// alone would leave every UI crate empty, so it fails here with the fix. The feature
/// alone (`cargo doc --all-features`) only compiles the unused interpreter and an empty
/// single crate.
fn live_preview() -> bool {
    println!("cargo:rerun-if-env-changed=SLINT_LIVE_PREVIEW");
    println!("cargo::rustc-check-cfg=cfg(rbs_ui_live)");
    let feature = std::env::var_os("CARGO_FEATURE_LIVE_PREVIEW").is_some();
    let variable = std::env::var_os("SLINT_LIVE_PREVIEW").is_some();
    if variable && !feature {
        panic!(
            "SLINT_LIVE_PREVIEW is set but the `live-preview` feature is off; \
             use `cargo xtask ui-dev`"
        );
    }
    variable
}

fn main() {
    if live_preview() {
        println!("cargo::rustc-cfg=rbs_ui_live");
    }
    println!("cargo:rerun-if-changed=../rbs-ui/tokens.toml");
    let modified = std::fs::metadata("../rbs-ui/tokens.toml")
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    if let Err(e) = rbs_ui::tokens_slint::write_token_files(&out, modified) {
        panic!("{e}");
    }
}
