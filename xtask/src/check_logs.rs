//! Part of `check-layers`: no diagnostics log call in code that is hot as a whole, and
//! the log macros are always called by path (ADR-0158 decisions 1 and 3; spec
//! `diagnostics-log.md`, "Hot-path rule"). Files with hot and cold code are checked by
//! the `hot-path-review` checklist instead. Keep this list in step with the spec and
//! `docs/architecture/hot-paths.md`.
//!
//! It also keeps library code off standard output and error and off other log
//! facades (spec, "Where lines go"): outside tests, `print!`, `println!`, `eprint!`,
//! `eprintln!`, `dbg!`, `rbs_crash::errln!`/`outln!` and the `log` and `tracing`
//! macros appear only where [`OUTPUT_ALLOWED`] says, since a line written there is
//! lost in an app started from Finder or the Start menu.

use std::fs;
use std::path::{Path, PathBuf};

/// Files and folders (ending in `/`) under `crates/` that are hot as a whole.
pub const HOT: &[&str] = &[
    "rbs-audio/src/input.rs",
    "rbs-audio/src/resample.rs",
    "rbs-audio/src/mixer.rs",
    "rbs-audio/src/meter.rs",
    "rbs-audio/src/sync_probe.rs",
    "rbs-audio/src/sink.rs",
    "rbs-audio/src/output.rs",
    "rbs-audio/src/encoder.rs",
    "rbs-audio/src/loudness/",
    "rbs-audio-filters/src/",
    "rbs-denoise/src/",
    "rbs-audio-io/src/ring.rs",
    "rbs-audio-io/src/edge.rs",
    "rbs-graph/src/runtime/",
    "rbs-engine/src/worker.rs",
    "rbs-filters/src/node.rs",
    "rbs-sources/src/browser_sound.rs",
    "rbs-browser-shm/src/sound.rs",
    "rbs-nodes/src/media/picture.rs",
    "rbs-nodes/src/media/sound.rs",
    "rbs-nodes/src/media/control.rs",
    "rbs-nodes/src/preview.rs",
    "rbs-nodes/src/text.rs",
    "rbs-nodes/src/sync_probe.rs",
    "rbs-gpu/src/yuv.rs",
    "rbs-gpu/src/download.rs",
    "rbs-gpu/src/upload.rs",
    "rbs-gpu/src/budget.rs",
    "rbs-gpu/src/rgba.rs",
    "rbs-gpu/src/motion.rs",
    "rbs-gpu/src/share/",
    "rbs-vcam/src/handoff.rs",
    "rbs-vcam/src/mailbox.rs",
    "rbs-media-format/src/",
    "rbs-core/src/time.rs",
    "rbs-core/src/timing.rs",
    "rbs-core/src/media/",
];

/// Crates and files (under `crates/`) that may write to standard output or error:
/// the CLI front ends answering their user, the macros' own crate, the log writer's
/// echo, the app's note when no log could start, the UI measure mode's results
/// (read by scripts, ADR-0120 decision 13), small standalone tools, the test
/// helper binary, and the CEF helper, whose standard error the studio logs
/// (`rbs_browser_runner`, `Area::Browser`).
pub const OUTPUT_ALLOWED: &[&str] = &[
    "rbs-cli/",
    "rbs-mcp/",
    "rbs-crash/",
    "rbs-log/",
    "rbs-app/src/diag_log.rs",
    "rbs-app/src/measure_ui.rs",
    "rbs-browser-runner/src/main.rs",
    "rbs-browser-cef/",
];

/// Writes to standard output or error, and other log facades.
const OUTPUT: [&str; 9] = [
    "print!(",
    "println!(",
    "eprint!(",
    "eprintln!(",
    "dbg!(",
    "errln!(",
    "outln!(",
    "log::",
    "tracing::",
];

const MACROS: [&str; 6] = ["error!", "warn!", "info!", "debug!", "trace!", "log_at!"];

/// Problems found under `root/crates`.
pub fn run(root: &Path) -> Vec<String> {
    let crates = root.join("crates");
    let mut errors = Vec::new();
    for hot in HOT {
        let path = crates.join(hot);
        if hot.ends_with('/') {
            visit(&path, &mut |file, text| hot_file(file, text, &mut errors));
        } else if let Ok(text) = fs::read_to_string(&path) {
            hot_file(&path, &text, &mut errors);
        } else {
            errors.push(format!(
                "{hot} is in the hot-path log list (xtask/src/check_logs.rs) but does not exist"
            ));
        }
    }
    output(&crates, &mut errors);
    visit(&crates, &mut |file, text| {
        for (n, line) in text.lines().enumerate() {
            if imports_a_macro(line) {
                errors.push(format!(
                    "{}:{}: call the log macros by path (rbs_core::info!), do not import them (ADR-0158)",
                    file.display(),
                    n + 1
                ));
            }
        }
    });
    errors
}

/// Stray writes to standard output or error in each crate's `src/` outside tests,
/// `src/bin/` and [`OUTPUT_ALLOWED`].
fn output(crates: &Path, errors: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(crates) else {
        return;
    };
    let mut dirs: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    dirs.sort();
    for dir in dirs {
        let src = dir.join("src");
        let mut files = Vec::new();
        visit(&src, &mut |file, text| {
            files.push((file.to_path_buf(), text.to_owned()))
        });
        let tests: Vec<PathBuf> = files.iter().flat_map(|(f, t)| test_modules(f, t)).collect();
        for (file, text) in &files {
            let rel = file.strip_prefix(crates).unwrap_or(file);
            let rel = rel.to_string_lossy().replace('\\', "/");
            if OUTPUT_ALLOWED.iter().any(|a| rel.starts_with(a))
                || rel.contains("/src/bin/")
                || tests.iter().any(|t| file.starts_with(t))
            {
                continue;
            }
            for (n, line) in outside_tests(text) {
                if writes_output(line) {
                    errors.push(format!(
                        "{}:{}: no print!, eprintln!, dbg!, errln! or log/tracing macro in library code; use rbs_core::info!(Area::..., ...) (ADR-0158)",
                        file.display(),
                        n + 1
                    ));
                }
            }
        }
    }
}

/// A write to standard output or error, or a `log`/`tracing` macro, in the code part
/// of `line`.
fn writes_output(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or("");
    if code.trim_start().starts_with("use log") || code.trim_start().starts_with("use tracing") {
        return true;
    }
    OUTPUT.iter().any(|m| {
        code.match_indices(m).any(|(at, _)| {
            let before = code[..at].chars().next_back();
            let bare = !before.is_some_and(|c| c.is_alphanumeric() || c == '_');
            if m.ends_with("::") {
                // `log::warn!` but not `rbs_core::log::Area` or `catalog::`.
                bare && code[at + m.len()..]
                    .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '!'))
                    .next()
                    .is_some_and(|name| name.ends_with('!'))
            } else {
                bare || code[..at].ends_with("::")
            }
        })
    })
}

/// The numbered lines of `text` before its first inline `#[cfg(test)] mod ... {`.
fn outside_tests(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let lines: Vec<&str> = text.lines().collect();
    let end = (0..lines.len())
        .find(|&i| {
            lines[i].trim() == "#[cfg(test)]"
                && lines[i + 1..]
                    .iter()
                    .find(|l| !l.trim_start().starts_with("#["))
                    .is_some_and(|l| l.contains("mod ") && l.trim_end().ends_with('{'))
        })
        .unwrap_or(lines.len());
    lines.into_iter().take(end).enumerate()
}

/// The files and folders of the modules `file` declares as `#[cfg(test)] mod x;`.
fn test_modules(file: &Path, text: &str) -> Vec<PathBuf> {
    let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned());
    let parent = file.parent().unwrap_or(file);
    let dir = match stem.as_deref() {
        Some("lib" | "main" | "mod") | None => parent.to_path_buf(),
        Some(s) => parent.join(s),
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[cfg(test)]" {
            continue;
        }
        let Some(next) = lines[i + 1..]
            .iter()
            .find(|l| !l.trim_start().starts_with("#["))
        else {
            continue;
        };
        let next = next.trim();
        let next = next.strip_prefix("pub(crate) ").unwrap_or(next);
        let next = next.strip_prefix("pub ").unwrap_or(next);
        if let Some(name) = next.strip_prefix("mod ").and_then(|r| r.strip_suffix(';')) {
            out.push(dir.join(format!("{name}.rs")));
            out.push(dir.join(name));
        }
    }
    out
}

fn hot_file(file: &Path, text: &str, errors: &mut Vec<String>) {
    for (n, line) in text.lines().enumerate() {
        if logs(line) {
            errors.push(format!(
                "{}:{}: no diagnostics log call on a hot path; count the event and summarise it on a cold thread (ADR-0158 decision 3)",
                file.display(),
                n + 1
            ));
        }
    }
}

/// A call of an RBS log macro (`rbs_core::` or, inside `rbs-core`, `crate::`), a
/// `log` crate macro, or an import of the `log` crate (whose macros could then be
/// called bare).
fn logs(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or("");
    let trimmed = code.trim_start();
    if trimmed.starts_with("use log")
        || trimmed.starts_with("use ::log")
        || trimmed.contains("#[macro_use]")
    {
        return true;
    }
    MACROS.iter().any(|m| {
        ["rbs_core::", "crate::", "log::"]
            .iter()
            .any(|p| code.contains(&format!("{p}{m}")))
    })
}

/// `use rbs_core::{info, ...}`, `pub use rbs_core::info;` or `use rbs_core::info as i;`
/// on one line. A `use` list spread over several lines is not seen (known limit).
fn imports_a_macro(line: &str) -> bool {
    let code = line.trim_start();
    let code = code.strip_prefix("pub ").unwrap_or(code);
    let code = code.strip_prefix("pub(crate) ").unwrap_or(code);
    let Some(rest) = code.strip_prefix("use rbs_core::") else {
        return false;
    };
    let rest = rest.trim_end_matches(';');
    let names: Vec<&str> = match rest.strip_prefix('{') {
        Some(list) => list
            .trim_end_matches('}')
            .split(',')
            .map(str::trim)
            .collect(),
        None => vec![rest.trim()],
    };
    names.iter().any(|n| {
        let n = n.split(" as ").next().unwrap_or(n).trim();
        MACROS.iter().any(|m| m.trim_end_matches('!') == n)
    })
}

fn visit(dir: &Path, f: &mut dyn FnMut(&Path, &str)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name == "target" || name.to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            visit(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = fs::read_to_string(&path)
        {
            f(&path, &text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_calls_are_found() {
        assert!(logs("    rbs_core::warn!(Area::Audio, \"late\");"));
        assert!(logs("log::debug!(\"x\")"));
        assert!(!logs("// rbs_core::warn!(Area::Audio, \"late\");"));
        assert!(!logs("let warn = 1;"));
        assert!(logs("    crate::warn!(Area::Audio, \"late\");"));
        assert!(logs("use log::warn;"));
    }

    #[test]
    fn stray_output_is_found() {
        assert!(writes_output("    eprintln!(\"x\");"));
        assert!(writes_output("println!(\"{x}\")"));
        assert!(writes_output("let v = dbg!(x);"));
        assert!(writes_output("rbs_crash::errln!(\"x\");"));
        assert!(writes_output("    log::warn!(\"x\");"));
        assert!(writes_output("tracing::info!(\"x\");"));
        assert!(writes_output("use log::warn;"));
        assert!(!writes_output("rbs_core::info!(Area::App, \"x\");"));
        assert!(!writes_output("use rbs_core::log::{Area, Level};"));
        assert!(!writes_output("rbs_core::log::Area::Gpu,"));
        assert!(!writes_output("let c = catalog::load();"));
        assert!(!writes_output("// eprintln!(\"x\");"));
        assert!(!writes_output("fn reprint!(x)"));
    }

    #[test]
    fn test_code_is_skipped() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    eprintln!(\"x\");\n}\n";
        assert_eq!(outside_tests(text).count(), 1);
        let decl = "#[cfg(test)]\nmod cost;\nmod live;\n";
        let found = test_modules(Path::new("a/src/host.rs"), decl);
        assert_eq!(
            found,
            [
                PathBuf::from("a/src/host/cost.rs"),
                PathBuf::from("a/src/host/cost")
            ]
        );
    }

    #[test]
    fn macro_imports_are_found() {
        assert!(imports_a_macro("use rbs_core::info;"));
        assert!(imports_a_macro("use rbs_core::{log, warn};"));
        assert!(imports_a_macro("pub use rbs_core::info as say;"));
        assert!(!imports_a_macro("use rbs_core::log::{self, Area};"));
        assert!(!imports_a_macro("use rbs_core::time::Clock;"));
    }
}
