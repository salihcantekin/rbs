//! `cargo xtask ui-dev`: runs the desktop app with the UI live preview
//! (`docs/specs/testing-strategy.md`, "UI live preview").
//!
//! The UI is interpreted at run time instead of compiled to Rust, and the open windows
//! reload when a `.slint` file under `crates/rbs-app/ui` is saved. A UI edit then needs
//! no Rust build at all. Development only: the interpreter is slower than the generated
//! code, so measurements and builds that ship never use it.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub const USAGE: &str = "\
usage: cargo xtask ui-dev [<cargo run args>] [-- <app args>]
  builds and runs `rbs` with the `ui-live-preview` feature and SLINT_LIVE_PREVIEW=1;
  saving a .slint file reloads the open windows (e.g. `cargo xtask ui-dev --features demo-sources`)";

/// The `cargo run` command line for `extra` (everything after `ui-dev`).
fn command_args(extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "run".to_owned(),
        "--package".to_owned(),
        "rbs-app".to_owned(),
        "--features".to_owned(),
        "ui-live-preview".to_owned(),
    ];
    args.extend(extra.iter().cloned());
    args
}

/// The target directory of the live preview: `live-preview/` under `CARGO_TARGET_DIR`
/// when it is set, else under the checkout's `target/`. Switching modes in one directory
/// would rerun every UI build script and rebuild the window crates (ADR-0138 decision 5).
pub fn target_dir(root: &Path, cargo_target_dir: Option<&std::ffi::OsStr>) -> PathBuf {
    cargo_target_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
        .join("live-preview")
}

pub fn run(root: &Path, extra: &[String]) -> Result<ExitCode, String> {
    if extra.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(&cargo)
        .current_dir(root)
        .args(command_args(extra))
        .env("SLINT_LIVE_PREVIEW", "1")
        .env(
            "CARGO_TARGET_DIR",
            target_dir(root, std::env::var_os("CARGO_TARGET_DIR").as_deref()),
        )
        .status()
        .map_err(|e| format!("run {cargo}: {e}"))?;
    Ok(if status.success() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{command_args, target_dir};

    #[test]
    fn the_live_preview_builds_beside_the_normal_target() {
        let root = Path::new("/repo");
        assert_eq!(
            target_dir(root, None),
            Path::new("/repo/target/live-preview")
        );
        assert_eq!(
            target_dir(root, Some("/shared/target".as_ref())),
            Path::new("/shared/target/live-preview")
        );
    }

    #[test]
    fn the_feature_comes_first_and_extra_arguments_follow() {
        let extra = ["--features".to_owned(), "demo-sources".to_owned()];
        assert_eq!(
            command_args(&extra),
            [
                "run",
                "--package",
                "rbs-app",
                "--features",
                "ui-live-preview",
                "--features",
                "demo-sources"
            ]
        );
    }
}
