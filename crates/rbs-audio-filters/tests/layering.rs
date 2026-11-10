//! Layering (ADR-0082, Tests): the schema and audio filter crates depend on neither the
//! GPU crate nor `wgpu`, so audio never pulls graphics in.

#![allow(clippy::expect_used)]

use std::path::Path;

fn manifest(crate_dir: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_dir)
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&path).expect("manifest");
    // Comments may name what the crate does not use.
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn audio_filters_and_params_do_not_depend_on_the_gpu() {
    for dir in ["rbs-audio-filters", "rbs-params"] {
        let text = manifest(dir);
        for forbidden in ["rbs-gpu", "wgpu", "rbs-audio ", "rbs-audio=", "rbs-filters"] {
            assert!(
                !text.contains(forbidden),
                "{dir} must not depend on {forbidden}"
            );
        }
    }
}
