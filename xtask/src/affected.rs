//! `cargo xtask affected`: build, lint and test only the crates a change can break.
//!
//! A change is the diff between the merge base with `origin/main` (or `--base <rev>`)
//! and the working tree, untracked files included. A changed file selects:
//!
//! - its workspace package, for a file under `crates/<dir>/` or `xtask/`;
//! - every workspace package whose Rust code (comment lines left out) names the file's
//!   path (a `build.rs` that compiles `../rbs-app/ui/app.slint`, a test that includes a
//!   spec from `docs/`);
//! - every package of `Cargo.lock` whose entry changed, for a lock file change;
//! - the whole workspace, for a build-wide file (`Cargo.toml` at the root, `.cargo/`,
//!   `.config/`, a toolchain file) or any root file it does not know.
//!
//! Documentation (`docs/`, `.claude/`, Markdown at the root), `.github/`, `prototypes/`
//! and the CEF runner (outside the workspace, ADR-0132) select nothing unless a source
//! names them. The selection is then closed over reverse dependencies, normal, build
//! and dev, as `Cargo.lock` records them, so a crate's dependents are retested too.
//! See `docs/specs/testing-strategy.md`, "Which tests to run".

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use toml::Value;

/// Usage text, printed by `cargo xtask` on a wrong command line.
pub const USAGE: &str = "\
usage: cargo xtask affected [--base <rev>] [--head <rev>] [list|check|clippy|test] [-- <extra args>]
  --base  compare against this revision (default: the merge base with origin/main)
  --head  compare up to this commit instead of the working tree (to see what a pull
          request selects; sources are still read from the working tree)
  list    print the affected packages, one per line (default)
  check   cargo check --all-targets on them
  clippy  cargo clippy --all-targets -- -D warnings on them
  test    cargo nextest run on them (cargo test when nextest is not installed); when a
          UI crate is selected, also checks the UI live preview build (ADR-0138)";

/// Runs `cargo xtask affected` with the arguments after `affected`.
pub fn run(root: &Path, args: &[String]) -> Result<ExitCode, String> {
    let mut base = None;
    let mut head: Option<String> = None;
    let mut action = "list".to_owned();
    let mut extra = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--base" => base = Some(iter.next().ok_or("--base needs a revision")?.clone()),
            "--head" => head = Some(iter.next().ok_or("--head needs a revision")?.clone()),
            "--" => {
                extra = iter.by_ref().cloned().collect();
            }
            "list" | "check" | "clippy" | "test" => action = arg.clone(),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    let base = match base {
        Some(base) => base,
        None => git(root, &["merge-base", "HEAD", "origin/main"])
            .map_err(|e| format!("{e}\n(no origin/main? pass --base <rev>)"))?
            .trim()
            .to_owned(),
    };
    let (changed, lock, old_lock) = changes(root, &base, head.as_deref())?;
    let workspace = Workspace::read(root, &lock)?;
    let selection = workspace.select(&changed, &old_lock, &lock)?;
    match action.as_str() {
        "list" => {
            match &selection {
                Selection::All => println!("(whole workspace)"),
                Selection::Some(packages) => {
                    for package in packages {
                        println!("{package}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => cargo(root, &action, &selection, &extra),
    }
}

/// The changed paths, the current lock file and the base's lock file (empty when the
/// lock did not change).
fn changes(
    root: &Path,
    base: &str,
    head: Option<&str>,
) -> Result<(BTreeSet<String>, String, String), String> {
    let mut changed: BTreeSet<String> = match head {
        Some(head) => git(root, &["diff", "--name-only", base, head])?,
        None => git(root, &["diff", "--name-only", base])?,
    }
    .lines()
    .map(str::to_owned)
    .collect();
    let lock = match head {
        Some(head) => git(root, &["show", &format!("{head}:Cargo.lock")])?,
        None => {
            changed.extend(
                git(root, &["ls-files", "--others", "--exclude-standard"])?
                    .lines()
                    .map(str::to_owned),
            );
            fs::read_to_string(root.join("Cargo.lock"))
                .map_err(|e| format!("read Cargo.lock: {e}"))?
        }
    };
    let old_lock = if changed.contains("Cargo.lock") {
        git(root, &["show", &format!("{base}:Cargo.lock")]).unwrap_or_default()
    } else {
        String::new()
    };
    Ok((changed, lock, old_lock))
}

/// What a change selects.
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    /// A build-wide file changed: everything.
    All,
    /// These workspace packages, possibly none.
    Some(BTreeSet<String>),
}

/// The workspace packages, their directories, sources and reverse dependencies.
struct Workspace {
    /// Directory relative to the root (`crates/rbs-core`, `xtask`) to package name.
    dirs: BTreeMap<String, String>,
    /// Package name to the text of its Rust sources (for path references).
    sources: BTreeMap<String, String>,
    /// Package name to the packages that depend on it (every package in the lock).
    dependents: BTreeMap<String, BTreeSet<String>>,
}

impl Workspace {
    fn read(root: &Path, lock: &str) -> Result<Self, String> {
        let mut dirs = BTreeMap::new();
        let mut sources = BTreeMap::new();
        let mut candidates: Vec<PathBuf> = vec![root.join("xtask")];
        let crates = root.join("crates");
        let entries =
            fs::read_dir(&crates).map_err(|e| format!("read {}: {e}", crates.display()))?;
        for entry in entries.flatten() {
            candidates.push(entry.path());
        }
        let members = lock_members(lock)?;
        for dir in candidates {
            let Some(name) = crate::package_name(&dir.join("Cargo.toml")) else {
                continue;
            };
            // Only workspace members are in this lock without a source; the CEF runner
            // has its own lock file and is left out.
            if !members.contains(&name) {
                continue;
            }
            let relative = dir
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let mut text = String::new();
            collect_rust(&dir, &mut text);
            sources.insert(name.clone(), text);
            dirs.insert(relative, name);
        }
        Ok(Self {
            dirs,
            sources,
            dependents: lock_dependents(lock)?,
        })
    }

    fn select(
        &self,
        changed: &BTreeSet<String>,
        old_lock: &str,
        lock: &str,
    ) -> Result<Selection, String> {
        let mut seeds = BTreeSet::new();
        for path in changed {
            match classify(path) {
                Kind::BuildWide => return Ok(Selection::All),
                Kind::Lock => seeds.extend(changed_lock_packages(old_lock, lock)?),
                Kind::Ignored => {}
                Kind::Source => {
                    if let Some(package) = self.owner(path) {
                        seeds.insert(package.clone());
                    }
                }
            }
            seeds.extend(self.referencing(path));
        }
        // Close over reverse dependencies, then keep workspace members.
        let mut seen = BTreeSet::new();
        let mut stack: Vec<String> = seeds.into_iter().collect();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some(users) = self.dependents.get(&name) {
                stack.extend(users.iter().cloned());
            }
        }
        let members: BTreeSet<&String> = self.dirs.values().collect();
        Ok(Selection::Some(
            seen.into_iter().filter(|n| members.contains(n)).collect(),
        ))
    }

    /// The workspace package whose directory holds `path`.
    fn owner(&self, path: &str) -> Option<&String> {
        self.dirs
            .iter()
            .filter(|(dir, _)| path.starts_with(&format!("{dir}/")))
            .max_by_key(|(dir, _)| dir.len())
            .map(|(_, name)| name)
    }

    /// Packages whose Rust sources name `path` or a directory it is in, for files a
    /// build script or test reads from outside its own crate.
    fn referencing(&self, path: &str) -> BTreeSet<String> {
        let needles = needles(path);
        self.sources
            .iter()
            .filter(|(_, text)| needles.iter().any(|n| text.contains(n.as_str())))
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// How a changed path affects the build.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// Changes how every crate builds or tests.
    BuildWide,
    /// `Cargo.lock`: the changed packages and their dependents.
    Lock,
    /// Inside a crate (or another file a source may name).
    Source,
    /// Never compiled; selects only crates whose sources name it.
    Ignored,
}

fn classify(path: &str) -> Kind {
    if path == "Cargo.lock" {
        return Kind::Lock;
    }
    let first = path.split('/').next().unwrap_or(path);
    let is_root_file = !path.contains('/');
    if is_root_file {
        if path.ends_with(".md") || path == "LICENSE" || path.starts_with(".git") {
            return Kind::Ignored;
        }
        return Kind::BuildWide;
    }
    match first {
        "crates" | "xtask" => Kind::Source,
        "docs" | ".claude" | ".github" | "prototypes" | "tools" => Kind::Ignored,
        ".cargo" | ".config" => Kind::BuildWide,
        _ => Kind::BuildWide,
    }
}

/// Strings that name `path` in a source: its full path and its path without the first
/// directory (`rbs-app/ui/app.slint`, `ui/baseline.toml`), and the same for each of its
/// directories with at least two components (`crates/rbs-app/ui`, `rbs-app/ui`).
fn needles(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').collect();
    let mut out = Vec::new();
    for end in (2..=parts.len()).rev() {
        for start in 0..2 {
            if end - start >= 2 {
                out.push(parts[start..end].join("/"));
            }
        }
    }
    // A directory of its own crate, such as `crates/rbs-core/src`, is named everywhere in
    // comments; only keep directories below a crate's top level and whole file paths.
    out.retain(|n| n.contains('.') || n.split('/').count() >= 2 && !is_crate_top(n));
    out.sort();
    out.dedup();
    out
}

fn is_crate_top(needle: &str) -> bool {
    let parts: Vec<&str> = needle.split('/').collect();
    matches!(
        parts.as_slice(),
        ["crates", _]
            | ["crates", _, "src" | "tests" | "benches" | "examples"]
            | [_, "src" | "tests" | "benches" | "examples"]
            | ["docs", _]
    )
}

fn collect_rust(dir: &Path, out: &mut String) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_rust(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = fs::read_to_string(&path) {
                // Comments cite specs and other crates' files everywhere; only code that
                // reads a file counts.
                for line in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
}

/// Workspace members: lock packages without a `source`.
fn lock_members(lock: &str) -> Result<BTreeSet<String>, String> {
    Ok(lock_packages(lock)?
        .into_iter()
        .filter(|p| p.source.is_none())
        .map(|p| p.name)
        .collect())
}

fn lock_dependents(lock: &str) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for package in lock_packages(lock)? {
        for dep in &package.dependencies {
            // "name", "name version" or "name version (source)".
            let dep_name = dep.split(' ').next().unwrap_or(dep).to_owned();
            map.entry(dep_name)
                .or_default()
                .insert(package.name.clone());
        }
    }
    Ok(map)
}

/// Lock packages added, removed or changed between two lock files, by name.
fn changed_lock_packages(old: &str, new: &str) -> Result<BTreeSet<String>, String> {
    if old.is_empty() {
        // The base has no lock file (or it could not be read): every package changed.
        return Ok(lock_packages(new)?.into_iter().map(|p| p.name).collect());
    }
    let old: BTreeSet<LockPackage> = lock_packages(old)?.into_iter().collect();
    let new: BTreeSet<LockPackage> = lock_packages(new)?.into_iter().collect();
    Ok(old
        .symmetric_difference(&new)
        .map(|p| p.name.clone())
        .collect())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    dependencies: Vec<String>,
}

fn lock_packages(lock: &str) -> Result<Vec<LockPackage>, String> {
    let value: Value = lock.parse().map_err(|e| format!("parse Cargo.lock: {e}"))?;
    let packages = value
        .get("package")
        .and_then(Value::as_array)
        .ok_or("Cargo.lock has no packages")?;
    let text = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_owned);
    Ok(packages
        .iter()
        .filter_map(|p| {
            Some(LockPackage {
                name: text(p, "name")?,
                version: text(p, "version").unwrap_or_default(),
                source: text(p, "source"),
                dependencies: p
                    .get("dependencies")
                    .and_then(Value::as_array)
                    .map(|deps| {
                        deps.iter()
                            .filter_map(|d| d.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect())
}

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("run git: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether the selection holds a crate of the generated UI code (ADR-0138).
fn selects_ui(selection: &Selection) -> bool {
    match selection {
        Selection::All => true,
        Selection::Some(set) => set.iter().any(|p| p.starts_with("rbs-app-ui")),
    }
}

/// The package arguments for `cargo nextest run` when a selection holding `rbs-app` is
/// better built as the whole workspace: `--workspace` plus a filter that runs only the
/// selected packages' tests. `-p` subsets resolve features over the subset alone, so
/// third-party crates and about 46 workspace crates are compiled a second time with
/// other features (measured 248 s and 7 GB more for `-p rbs-app` beside a workspace
/// build). `None` keeps `-p`: no `rbs-app`, the whole workspace already, or a filter
/// of the caller's own (nextest joins several filters with "or").
fn nextest_packages(selection: &Selection, extra: &[String]) -> Option<Vec<String>> {
    let Selection::Some(set) = selection else {
        return None;
    };
    let own_filter = extra
        .iter()
        .any(|a| a == "-E" || a.starts_with("--filterset") || a.starts_with("--filter-expr"));
    if !set.contains("rbs-app") || own_filter {
        return None;
    }
    let filter = set
        .iter()
        .map(|p| format!("package(={p})"))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(vec!["--workspace".to_owned(), "-E".to_owned(), filter])
}

fn cargo(
    root: &Path,
    action: &str,
    selection: &Selection,
    extra: &[String],
) -> Result<ExitCode, String> {
    let packages: Vec<String> = match selection {
        Selection::All => vec!["--workspace".to_owned()],
        Selection::Some(set) if set.is_empty() => {
            eprintln!("affected: the change touches no crate; nothing to {action}");
            return Ok(ExitCode::SUCCESS);
        }
        Selection::Some(set) => set
            .iter()
            .flat_map(|p| ["-p".to_owned(), p.clone()])
            .collect(),
    };
    let (cargo, mut command) = crate::cargo_command();
    command.current_dir(root);
    match action {
        "check" => {
            command
                .arg("check")
                .args(&packages)
                .arg("--all-targets")
                .args(extra);
        }
        "clippy" => {
            command
                .arg("clippy")
                .args(&packages)
                .arg("--all-targets")
                .args(extra);
            if !extra.iter().any(|a| a == "--") {
                command.args(["--", "-D", "warnings"]);
            }
        }
        _ => {
            let nextest = crate::cargo_command()
                .1
                .args(["nextest", "--version"])
                .output()
                .is_ok_and(|o| o.status.success());
            if nextest {
                command
                    .args(["nextest", "run"])
                    .args(nextest_packages(selection, extra).unwrap_or(packages))
                    .args(extra);
            } else {
                eprintln!("affected: cargo-nextest is not installed; using cargo test");
                command.arg("test").args(&packages).args(extra);
            }
        }
    }
    let count = match selection {
        Selection::All => "the whole workspace".to_owned(),
        Selection::Some(set) => format!("{} package(s)", set.len()),
    };
    eprintln!("affected: {action} on {count}");
    let status = command.status().map_err(|e| format!("run {cargo}: {e}"))?;
    if !status.success() {
        return Ok(ExitCode::from(1));
    }
    if action == "test" && selects_ui(selection) {
        // The live preview's single build must keep compiling with the split one
        // (ADR-0138 decision 5); its own target directory keeps the normal build intact.
        eprintln!("affected: checking the UI live preview build");
        let target =
            crate::ui_dev::target_dir(root, std::env::var_os("CARGO_TARGET_DIR").as_deref());
        let live = crate::cargo_command()
            .1
            .current_dir(root)
            .args(["check", "-p", "rbs-app", "--features", "ui-live-preview"])
            .env("SLINT_LIVE_PREVIEW", "1")
            .env("CARGO_TARGET_DIR", target)
            .status()
            .map_err(|e| format!("run {cargo}: {e}"))?;
        if !live.success() {
            return Ok(ExitCode::from(1));
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "core"
version = "0.0.1"

[[package]]
name = "graph"
version = "0.0.1"
dependencies = ["core"]

[[package]]
name = "app"
version = "0.0.1"
dependencies = ["graph", "serde 1.0.1"]

[[package]]
name = "lone"
version = "0.0.1"
dependencies = ["serde 1.0.1 (registry+https://github.com/rust-lang/crates.io-index)"]

[[package]]
name = "serde"
version = "1.0.1"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#;

    fn workspace() -> Workspace {
        let mut dirs = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for (dir, name, text) in [
            ("crates/core", "core", ""),
            ("crates/graph", "graph", ""),
            ("crates/app", "app", ""),
            (
                "crates/lone",
                "lone",
                "include_str!(\"../../../docs/specs/lone.md\")",
            ),
            ("xtask", "xtask", "const UI: &str = \"crates/app/ui\";"),
        ] {
            dirs.insert(dir.to_owned(), name.to_owned());
            sources.insert(name.to_owned(), text.to_owned());
        }
        let dependents = lock_dependents(LOCK).unwrap_or_default();
        Workspace {
            dirs,
            sources,
            dependents,
        }
    }

    fn select(paths: &[&str]) -> Selection {
        let changed = paths.iter().map(|p| (*p).to_owned()).collect();
        workspace()
            .select(&changed, "", LOCK)
            .unwrap_or(Selection::All)
    }

    fn some(names: &[&str]) -> Selection {
        Selection::Some(names.iter().map(|n| (*n).to_owned()).collect())
    }

    #[test]
    fn a_crate_change_selects_the_crate_and_its_dependents() {
        assert_eq!(
            select(&["crates/core/src/lib.rs"]),
            some(&["app", "core", "graph"])
        );
        assert_eq!(select(&["crates/app/src/main.rs"]), some(&["app"]));
    }

    #[test]
    fn documentation_selects_only_crates_that_read_it() {
        assert_eq!(select(&["docs/adr/0001-x.md", "README.md"]), some(&[]));
        assert_eq!(select(&["docs/specs/lone.md"]), some(&["lone"]));
    }

    #[test]
    fn a_file_read_from_another_crate_selects_the_reader() {
        assert_eq!(
            select(&["crates/app/ui/main.slint"]),
            some(&["app", "xtask"])
        );
    }

    #[test]
    fn build_wide_files_select_everything() {
        assert_eq!(select(&["Cargo.toml"]), Selection::All);
        assert_eq!(select(&[".config/nextest.toml"]), Selection::All);
        assert_eq!(select(&["rust-toolchain.toml"]), Selection::All);
    }

    #[test]
    fn a_lock_change_selects_the_users_of_the_changed_packages() {
        let old = LOCK.replace("version = \"1.0.1\"\nsource", "version = \"1.0.0\"\nsource");
        let changed = ["Cargo.lock".to_owned()].into_iter().collect();
        let selection = workspace()
            .select(&changed, &old, LOCK)
            .unwrap_or(Selection::All);
        assert_eq!(selection, some(&["app", "lone"]));
    }

    #[test]
    fn a_selection_with_the_app_builds_the_workspace_and_filters_tests() {
        let selection = some(&["rbs-app", "rbs-gpu"]);
        assert_eq!(
            nextest_packages(&selection, &[]),
            Some(vec![
                "--workspace".to_owned(),
                "-E".to_owned(),
                "package(=rbs-app) | package(=rbs-gpu)".to_owned(),
            ])
        );
        assert_eq!(nextest_packages(&some(&["rbs-gpu"]), &[]), None);
        assert_eq!(nextest_packages(&Selection::All, &[]), None);
        let own = ["-E".to_owned(), "test(x)".to_owned()];
        assert_eq!(nextest_packages(&selection, &own), None);
    }

    #[test]
    fn crate_top_directories_are_not_needles() {
        let n = needles("crates/rbs-core/src/time.rs");
        assert!(n.contains(&"rbs-core/src/time.rs".to_owned()));
        assert!(!n.contains(&"crates/rbs-core/src".to_owned()));
        assert!(!n.contains(&"crates/rbs-core".to_owned()));
        assert!(needles("crates/rbs-app/ui/app.slint").contains(&"rbs-app/ui".to_owned()));
    }
}
