//! `cargo xtask verify`: the one local gate before a push (owner decision D3,
//! 2026-11-20; quality plan WP-03, ADR-0144).
//!
//! The quick part runs every time and needs no build of the workspace: `cargo fmt
//! --check`, `check-layers`, `check-ui` and `check-size`. The affected part builds
//! only what the change can break: `cargo xtask affected clippy` and `affected test`.
//! It also checks the control API's generated schema snapshot with the opt-in feature.
//! The affected part is skipped when a quick check failed, so a formatting slip is
//! reported in seconds. One result line closes the run.

use std::path::Path;
use std::process::ExitCode;

pub const USAGE: &str = "\
usage: cargo xtask verify [--quick] [--base <rev>]
  --quick  only fmt, check-layers, check-ui and check-size (the merger's check of a
           merge result before main is pushed)
  --base   compare the affected part against this revision (default: the merge base
           with origin/main)";

/// Runs `cargo xtask verify` with the arguments after `verify`.
pub fn run(root: &Path, args: &[String]) -> Result<ExitCode, String> {
    let mut quick = false;
    let mut base: Vec<String> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--base" => {
                let rev = iter.next().ok_or("--base needs a revision")?;
                base = vec!["--base".to_owned(), rev.clone()];
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    let mut results: Vec<(&str, bool)> = Vec::new();

    let fmt_ok = check_format(root)?;
    results.push(("fmt", fmt_ok));
    results.push(("layers", report("check-layers", crate::check_workspace())));
    results.push(("ui", report("check-ui", crate::check_ui::check(root))));
    results.push(("size", report("check-size", crate::check_size::check(root))));

    let quick_ok = results.iter().all(|(_, ok)| *ok);
    if !quick && quick_ok {
        for (name, action) in [("clippy", "clippy"), ("test", "test")] {
            let mut args = base.clone();
            args.push(action.to_owned());
            let code = crate::affected::run(root, &args)?;
            results.push((name, code == ExitCode::SUCCESS));
            if code != ExitCode::SUCCESS {
                break;
            }
        }
        if results.iter().all(|(_, ok)| *ok) {
            let (_, mut cargo) = crate::cargo_command();
            let schema_ok = cargo
                .current_dir(root)
                .args([
                    "test",
                    "-p",
                    "rbs-api",
                    "--features",
                    "schema",
                    "--test",
                    "catalog",
                    "generated_schemas_match_the_checked_in_snapshot",
                ])
                .status()
                .map_err(|e| format!("run control API schema snapshot test: {e}"))?
                .success();
            results.push(("schema", schema_ok));
        }
    }
    let line: Vec<String> = results
        .iter()
        .map(|(name, ok)| format!("{name} {}", if *ok { "ok" } else { "FAILED" }))
        .collect();
    let all_ok = results.iter().all(|(_, ok)| *ok);
    let skipped = if !quick && !quick_ok {
        ", clippy, test and schema skipped"
    } else if quick {
        " (quick)"
    } else {
        ""
    };
    eprintln!(
        "verify: {}: {}{skipped}",
        if all_ok { "passed" } else { "FAILED" },
        line.join(", ")
    );
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[cfg(not(windows))]
fn check_format(root: &Path) -> Result<bool, String> {
    let (_, mut fmt) = crate::cargo_command();
    fmt.current_dir(root)
        .args(["fmt", "--all", "--", "--check"])
        .status()
        .map(|status| status.success())
        .map_err(|e| format!("run cargo fmt: {e}"))
}

#[cfg(windows)]
fn check_format(root: &Path) -> Result<bool, String> {
    let (_, mut cargo) = crate::cargo_command();
    let output = cargo
        .current_dir(root)
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()
        .map_err(|e| format!("run cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let packages = workspace_packages(&output.stdout)?;
    // cargo-fmt passes every target to rustfmt in one command, exceeding the
    // Windows command-line limit for this workspace. Check the same targets in batches.
    for batch in packages.chunks(8) {
        let (_, mut fmt) = crate::cargo_command();
        fmt.current_dir(root).arg("fmt");
        for package in batch {
            fmt.args(["-p", package]);
        }
        let status = fmt
            .args(["--", "--check"])
            .status()
            .map_err(|e| format!("run cargo fmt: {e}"))?;
        if !status.success() {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(any(windows, test))]
fn workspace_packages(metadata: &[u8]) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_slice(metadata).map_err(|e| format!("parse cargo metadata: {e}"))?;
    let members = value["workspace_members"]
        .as_array()
        .ok_or("cargo metadata has no workspace_members")?;
    let packages = value["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages")?;
    let mut names = Vec::new();
    for member in members {
        let id = member.as_str().ok_or("invalid workspace member id")?;
        let package = packages
            .iter()
            .find(|package| package["id"].as_str() == Some(id))
            .ok_or_else(|| format!("workspace member missing from cargo metadata: {id}"))?;
        names.push(
            package["name"]
                .as_str()
                .ok_or("workspace package has no name")?
                .to_owned(),
        );
    }
    if names.is_empty() {
        return Err("cargo metadata has no workspace members".into());
    }
    Ok(names)
}

fn report(name: &str, result: Result<(), Vec<String>>) -> bool {
    match result {
        Ok(()) => true,
        Err(errors) => {
            for error in errors {
                eprintln!("{name}: {error}");
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::workspace_packages;

    #[test]
    fn formats_only_workspace_members() {
        let metadata = br#"{
            "workspace_members": ["a 1.0", "b 1.0"],
            "packages": [
                {"id": "a 1.0", "name": "a"},
                {"id": "external 1.0", "name": "external"},
                {"id": "b 1.0", "name": "b"}
            ]
        }"#;
        assert_eq!(workspace_packages(metadata).unwrap(), ["a", "b"]);
    }

    #[test]
    fn rejects_missing_workspace_members() {
        let metadata = br#"{"workspace_members": ["a 1.0"], "packages": []}"#;
        assert!(workspace_packages(metadata).is_err());
    }
}
