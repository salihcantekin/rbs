//! `cargo xtask verify`: the one local gate before a push (owner decision D3,
//! 2026-11-20; quality plan WP-03, ADR-0144).
//!
//! The quick part runs every time and needs no build of the workspace: `cargo fmt
//! --check`, `check-layers`, `check-ui` and `check-size`. The affected part builds
//! only what the change can break: `cargo xtask affected clippy` and `affected test`.
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

    let (_, mut fmt) = crate::cargo_command();
    let fmt_ok = fmt
        .current_dir(root)
        .args(["fmt", "--all", "--", "--check"])
        .status()
        .map_err(|e| format!("run cargo fmt: {e}"))?
        .success();
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
    }
    let line: Vec<String> = results
        .iter()
        .map(|(name, ok)| format!("{name} {}", if *ok { "ok" } else { "FAILED" }))
        .collect();
    let all_ok = results.iter().all(|(_, ok)| *ok);
    let skipped = if !quick && !quick_ok {
        ", clippy and test skipped"
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
