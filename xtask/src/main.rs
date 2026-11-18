//! Layer check for principle P1 (`docs/architecture/overview.md`, Layers).
//!
//! `cargo xtask check-layers` fails when a crate depends on a layer it may not see.
//! The allowlists are the current tree. A new crate, or a new dependency that crosses
//! a layer, is classified here in the same change as the overview table.
//!
//! `cargo xtask check-ui` runs the counted UI checks of ADR-0115 (`check_ui`).
//!
//! `cargo xtask affected` checks, lints or tests only the crates a change can break
//! (`affected`, `docs/specs/testing-strategy.md`).
//!
//! `cargo xtask ui-dev` runs the app with the UI live preview (`ui_dev`).

mod affected;
mod check_ui;
mod ui_dev;
mod ui_split;

use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use toml::Value;

/// A `cargo` command without the package variables `cargo run` set for xtask itself.
///
/// `cargo xtask` runs this binary through `cargo run`, which sets `CARGO_MANIFEST_DIR`
/// and `CARGO_PKG_*` to xtask's. A child `cargo` passed them on to build scripts:
/// `ring`'s reruns when `CARGO_MANIFEST_DIR` changes, so every switch between
/// `cargo xtask affected test` and a plain `cargo test` rebuilt `ring`, `rustls` and
/// every crate above them, `rbs-app` included (about 200 s on 4 cores).
pub(crate) fn cargo_command() -> (String, Command) {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let mut command = Command::new(&cargo);
    for (name, _) in env::vars_os() {
        if name.to_str().is_some_and(is_package_variable) {
            command.env_remove(name);
        }
    }
    (cargo, command)
}

/// Whether `cargo run` sets this variable for the package it runs.
fn is_package_variable(name: &str) -> bool {
    matches!(
        name,
        "CARGO_MANIFEST_DIR"
            | "CARGO_MANIFEST_PATH"
            | "CARGO_PRIMARY_PACKAGE"
            | "CARGO_CRATE_NAME"
            | "CARGO_BIN_NAME"
    ) || name.starts_with("CARGO_PKG_")
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("check-layers") if args.next().is_none() => match check_workspace() {
            Ok(()) => ExitCode::SUCCESS,
            Err(errors) => {
                for error in errors {
                    eprintln!("check-layers: {error}");
                }
                ExitCode::from(1)
            }
        },
        Some("check-ui") => {
            let rest: Vec<String> = args.collect();
            match check_ui::run(&workspace_root(), &rest) {
                Ok(()) => ExitCode::SUCCESS,
                Err(errors) => {
                    for error in errors {
                        eprintln!("check-ui: {error}");
                    }
                    // An agent's edit hook treats exit code 2 as "refused, read stderr".
                    let hook = rest.first().is_some_and(|arg| arg == "--hook");
                    ExitCode::from(if hook { 2 } else { 1 })
                }
            }
        }
        Some("affected") => {
            let rest: Vec<String> = args.collect();
            match affected::run(&workspace_root(), &rest) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("affected: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Some("ui-dev") => {
            let rest: Vec<String> = args.collect();
            match ui_dev::run(&workspace_root(), &rest) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("ui-dev: {error}");
                    ExitCode::from(2)
                }
            }
        }
        _ => {
            eprintln!(
                "usage: cargo xtask check-layers\n{}\n{}\n{}",
                check_ui::USAGE,
                affected::USAGE,
                ui_dev::USAGE
            );
            ExitCode::from(2)
        }
    }
}

fn check_workspace() -> Result<(), Vec<String>> {
    let root = workspace_root();
    let mut edges = Vec::new();
    let crates = root.join("crates");
    let entries =
        fs::read_dir(&crates).map_err(|e| vec![format!("read {}: {e}", crates.display())])?;
    for entry in entries {
        let entry = entry.map_err(|e| vec![e.to_string()])?;
        let manifest = entry.path().join("Cargo.toml");
        if manifest.is_file() {
            edges.extend(read_edges(&manifest)?);
        }
    }
    let own = root.join("xtask/Cargo.toml");
    edges.extend(read_edges(&own)?);
    let mut errors = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for (from, to) in &edges {
        seen.insert(from.clone());
        if let Some(error) = deny(from, to) {
            errors.push(format!("{from} must not depend on {to} ({error})"));
        }
    }
    // A crate with no dependencies is still classified.
    if let Ok(entries) = fs::read_dir(root.join("crates")) {
        for entry in entries.flatten() {
            if let Some(name) = package_name(&entry.path().join("Cargo.toml")) {
                seen.insert(name);
            }
        }
    }
    for name in seen {
        if name.starts_with("rbs-") && class(&name).is_none() {
            errors.push(format!(
                "{name} is not in the layer table (xtask/src/main.rs and docs/architecture/overview.md)"
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read_edges(manifest: &Path) -> Result<Vec<(String, String)>, Vec<String>> {
    let text = fs::read_to_string(manifest)
        .map_err(|e| vec![format!("read {}: {e}", manifest.display())])?;
    let value: Value = text
        .parse()
        .map_err(|e| vec![format!("parse {}: {e}", manifest.display())])?;
    let name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| vec![format!("{} has no package name", manifest.display())])?
        .to_owned();
    let mut edges = Vec::new();
    if let Some(deps) = value.get("dependencies").and_then(Value::as_table) {
        for dep in deps.keys() {
            edges.push((name.clone(), dep.clone()));
        }
    }
    if let Some(targets) = value.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            if let Some(deps) = target.get("dependencies").and_then(Value::as_table) {
                for dep in deps.keys() {
                    edges.push((name.clone(), dep.clone()));
                }
            }
        }
    }
    Ok(edges)
}

/// Why `from` may not depend on `to`, or `None` when the edge is allowed.
fn package_name(manifest: &Path) -> Option<String> {
    let text = fs::read_to_string(manifest).ok()?;
    let value: Value = text.parse().ok()?;
    value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn deny(from: &str, to: &str) -> Option<String> {
    if to.starts_with("rbs-") && class(to).is_none() {
        return Some(
            "that crate is not in the layer table (xtask/src/main.rs and docs/architecture/overview.md)"
                .to_owned(),
        );
    }
    let from_class = class(from)?;
    let to_class = class(to);
    match from_class {
        Class::Core => deny_core(to, to_class),
        Class::Helper => deny_helper(from, to, to_class),
        Class::Support => deny_support(from, to, to_class),
        Class::Engine => deny_engine(from, to, to_class),
        Class::Adapter => deny_adapter(from, to, to_class),
        Class::Control => deny_named(
            to,
            to_class,
            &[
                "rbs-core",
                "serde",
                "serde_json",
                "sha2",
                "subtle",
                "getrandom",
            ],
            "the control API stays free of the engine, adapters and transports",
        ),
        Class::Transport => {
            if to_class.is_some() && to != "rbs-api" {
                Some("a transport depends only on rbs-api".to_owned())
            } else if is_ui_or_gpu(to) {
                Some("a transport does not draw or render".to_owned())
            } else {
                None
            }
        }
        Class::Ui => {
            if to_class.is_some() || is_ui_or_gpu(to) || is_os_media(to) {
                Some("rbs-ui is toolkit-neutral and does not see the engine".to_owned())
            } else {
                None
            }
        }
        Class::AppUi => deny_app_ui(from, to, to_class),
        Class::Settings => deny_named(
            to,
            to_class,
            &["rbs-api", "serde", "toml"],
            "remote-control settings are shared by the app and the cli",
        ),
        Class::UserData => deny_named(
            to,
            to_class,
            &[
                "rbs-core",
                "rbs-graph",
                "rbs-scene",
                "rbs-ui",
                "serde",
                "toml",
            ],
            "user data sees the scene model, not an adapter or a toolkit",
        ),
        Class::AddOns => deny_named(
            to,
            to_class,
            &["rbs-core", "serde", "sha2", "toml", "unicode-normalization"],
            "add-ons hold the rules; HTTP, archives and OS file operations come through rbs-add-ons-sys (ADR-0145)",
        ),
        Class::Import => deny_named(
            to,
            to_class,
            &[
                "rbs-core",
                "rbs-graph",
                "rbs-scene",
                "rbs-params",
                "rbs-ui",
                "serde",
                "serde_json",
            ],
            "an importer reads files into the scene model; only composition roots write (ADR-0131)",
        ),
        Class::PluginSdk => deny_named(
            to,
            to_class,
            &["serde", "toml"],
            "the plugin SDK is the manifest types and does not see the engine",
        ),
        Class::Plugin => deny_named(
            to,
            to_class,
            &["rbs-plugin-sdk"],
            "the plugin host reads manifests and does not load libraries or see the engine",
        ),
        Class::App | Class::Tool => None,
    }
}

fn deny_core(to: &str, to_class: Option<Class>) -> Option<String> {
    match to_class {
        Some(Class::Core) => None,
        Some(_) => Some("a core or port crate only sees other core and port crates".to_owned()),
        None if to == "bytes" => None,
        None => Some("a core or port crate's only external crate is bytes".to_owned()),
    }
}

fn deny_helper(from: &str, to: &str, to_class: Option<Class>) -> Option<String> {
    if from == "rbs-media-format" && to == "rbs-core" {
        return None;
    }
    Some(format!(
        "{from} is a helper with no further crate ({to}, {to_class:?})"
    ))
}

fn deny_support(from: &str, to: &str, to_class: Option<Class>) -> Option<String> {
    if from == "rbs-codec-hw" && matches!(to, "rbs-codec" | "rbs-core" | "rbs-media-format") {
        return None;
    }
    if from == "rbs-vcam-mf-source" && (to == "rbs-vcam-ring" || is_os_media(to)) {
        return None;
    }
    // OS shared memory for browser frames (ADR-0109 decision 8; owner's choice
    // 2026-11-16 to keep `rbs-browser-ipc` free of OS bindings).
    if from == "rbs-browser-shm" && matches!(to, "libc" | "windows") {
        return None;
    }
    let _ = to_class;
    Some("adapter support never depends on an adapter or a vendor SDK".to_owned())
}

fn deny_engine(from: &str, to: &str, to_class: Option<Class>) -> Option<String> {
    if from == "rbs-engine" && !matches!(to, "rbs-core" | "rbs-graph") {
        return Some("rbs-engine depends only on rbs-core and rbs-graph".to_owned());
    }
    match to_class {
        Some(
            Class::Adapter
            | Class::App
            | Class::AppUi
            | Class::Control
            | Class::Transport
            | Class::Ui
            | Class::Settings
            | Class::UserData
            | Class::AddOns
            | Class::Import
            | Class::Tool
            | Class::Plugin
            | Class::PluginSdk,
        ) => Some("the engine never depends on an adapter, the control API or the UI".to_owned()),
        // Support crates that carry OS bindings stay with their adapters.
        Some(Class::Support) if matches!(to, "rbs-browser-shm" | "rbs-vcam-mf-source") => {
            Some("the engine never reaches OS bindings through a support crate".to_owned())
        }
        Some(Class::Core | Class::Helper | Class::Support | Class::Engine) => None,
        None if is_ui_toolkit(to) || is_ffmpeg(to) || is_os_media(to) && from != "rbs-gpu" => Some(
            "OS media and UI toolkits stay in adapters; wgpu stays in the GPU crates".to_owned(),
        ),
        None => None,
    }
}

fn deny_adapter(from: &str, to: &str, to_class: Option<Class>) -> Option<String> {
    match to_class {
        Some(Class::Adapter) => {
            if (from.starts_with("rbs-codec-") && to == "rbs-codec-hw")
                || (from == "rbs-vcam-mf" && to == "rbs-vcam-mf-source")
            {
                None
            } else {
                Some("adapters do not depend on each other".to_owned())
            }
        }
        Some(
            Class::Engine
            | Class::App
            | Class::AppUi
            | Class::Control
            | Class::Transport
            | Class::Ui
            | Class::Settings
            | Class::UserData
            | Class::AddOns
            | Class::Import
            | Class::Tool
            | Class::Plugin
            | Class::PluginSdk,
        ) => Some("an adapter sees its port, foundation and its support crate".to_owned()),
        Some(Class::Core | Class::Helper | Class::Support) => None,
        None if is_ui_toolkit(to) => Some("an adapter does not use a UI toolkit".to_owned()),
        None => None,
    }
}

/// The crates of the generated UI code (ADR-0138): the UI model, Slint, and each other
/// only from the facade `rbs-app-ui` or towards the base library.
fn deny_app_ui(from: &str, to: &str, to_class: Option<Class>) -> Option<String> {
    if to_class == Some(Class::AppUi) {
        if from == "rbs-app-ui" || to == "rbs-app-ui-base" {
            None
        } else {
            Some(
                "a UI crate depends on another only from the facade rbs-app-ui or on rbs-app-ui-base (ADR-0138)"
                    .to_owned(),
            )
        }
    } else {
        deny_named(
            to,
            to_class,
            &["rbs-ui", "slint", "i-slint-core"],
            "generated UI code depends on the UI model and Slint",
        )
    }
}

fn deny_named(to: &str, to_class: Option<Class>, allowed: &[&str], why: &str) -> Option<String> {
    if allowed.contains(&to) {
        None
    } else {
        let _ = to_class;
        Some(why.to_owned())
    }
}

fn is_ui_toolkit(name: &str) -> bool {
    name == "slint" || name.starts_with("i-slint") || name.starts_with("i_slint")
}

fn is_ffmpeg(name: &str) -> bool {
    name.starts_with("ffmpeg")
}

fn is_os_media(name: &str) -> bool {
    name.starts_with("objc")
        || name == "windows"
        || name.starts_with("windows-")
        || name.starts_with("pipewire")
        || name == "ash"
        || name == "dbus"
        || name == "zbus"
        || name.starts_with("coreaudio")
        || name == "block2"
        || name == "dispatch2"
        || name == "mach2"
}

fn is_ui_or_gpu(name: &str) -> bool {
    is_ui_toolkit(name) || name == "wgpu" || name.starts_with("wgpu-")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Core,
    Helper,
    Support,
    Engine,
    Adapter,
    Control,
    Transport,
    Ui,
    AppUi,
    Settings,
    UserData,
    AddOns,
    Import,
    App,
    Tool,
    PluginSdk,
    Plugin,
}

/// `None` for a crate from outside the workspace. A workspace crate that is missing
/// from this match is reported by `check_workspace`, which fails the build.
fn class(name: &str) -> Option<Class> {
    Some(match name {
        "rbs-core" | "rbs-graph" | "rbs-scene" | "rbs-capture" | "rbs-audio-io" | "rbs-codec"
        | "rbs-mux" | "rbs-transport" | "rbs-vcam" | "rbs-media-decode" | "rbs-text" => Class::Core,
        "rbs-params" | "rbs-media-format" | "rbs-crash" | "rbs-vcam-ring" | "rbs-browser-ipc" => {
            Class::Helper
        }
        "rbs-codec-hw" | "rbs-vcam-mf-source" | "rbs-browser-shm" => Class::Support,
        "rbs-gpu" | "rbs-audio" | "rbs-audio-filters" | "rbs-engine" | "rbs-nodes"
        | "rbs-sources" | "rbs-record" | "rbs-filters" => Class::Engine,
        "rbs-api" => Class::Control,
        "rbs-api-ws" | "rbs-api-ipc" => Class::Transport,
        "rbs-ui" => Class::Ui,
        "rbs-app-ui" | "rbs-app-ui-base" | "rbs-app-ui-main" | "rbs-app-ui-settings"
        | "rbs-app-ui-dialogs" | "rbs-app-ui-probes" | "rbs-app-ui-single" => Class::AppUi,
        "rbs-remote-settings" => Class::Settings,
        "rbs-user-data" => Class::UserData,
        "rbs-add-ons" => Class::AddOns,
        "rbs-import-obs" => Class::Import,
        "rbs-app" | "rbs-cli" | "rbs-mcp" => Class::App,
        "rbs-plugin-sdk" => Class::PluginSdk,
        "rbs-plugin-host" => Class::Plugin,
        "xtask" => Class::Tool,
        "rbs-capture-wgc"
        | "rbs-capture-sck"
        | "rbs-capture-avf"
        | "rbs-capture-mf"
        | "rbs-capture-pipewire"
        | "rbs-capture-test"
        | "rbs-audio-io-coreaudio"
        | "rbs-audio-io-pipewire"
        | "rbs-audio-io-wasapi"
        | "rbs-audio-io-test"
        | "rbs-codec-ffmpeg"
        | "rbs-codec-amf"
        | "rbs-codec-nvenc"
        | "rbs-codec-qsv"
        | "rbs-codec-vaapi"
        | "rbs-codec-videotoolbox"
        | "rbs-codec-audiotoolbox"
        | "rbs-codec-mf"
        | "rbs-codec-pcm"
        | "rbs-codec-flac"
        | "rbs-mux-mpegts"
        | "rbs-mux-wav"
        | "rbs-mux-flac"
        | "rbs-mux-flv"
        | "rbs-mux-fmp4"
        | "rbs-mux-matroska"
        | "rbs-transport-file"
        | "rbs-transport-rtmp"
        | "rbs-transport-srt"
        | "rbs-thread-os"
        | "rbs-system-probe"
        | "rbs-vcam-mf"
        | "rbs-vcam-test"
        | "rbs-media-decode-test"
        | "rbs-text-test"
        | "rbs-text-parley"
        | "rbs-browser-runner"
        // Outside the workspace (ADR-0132 decision 1), so check-layers never walks
        // it; `the_cef_runner_depends_only_on_its_allow_list` checks its manifest.
        | "rbs-browser-cef" => Class::Adapter,
        other if other.starts_with("rbs-") => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn child_cargo_drops_only_the_package_variables() {
        assert!(super::is_package_variable("CARGO_MANIFEST_DIR"));
        assert!(super::is_package_variable("CARGO_PKG_NAME"));
        assert!(!super::is_package_variable("CARGO_TARGET_DIR"));
        assert!(!super::is_package_variable("CARGO_HOME"));
        assert!(!super::is_package_variable("CARGO"));
    }

    use super::*;

    /// ADR-0132 decision 1: no workspace command may resolve, download or build
    /// CEF. The CEF runner is excluded and keeps its own lock file.
    #[test]
    #[allow(clippy::expect_used)]
    fn the_workspace_lock_file_has_no_cef() {
        let lock = fs::read_to_string(workspace_root().join("Cargo.lock")).expect("Cargo.lock");
        for name in ["cef-dll-sys", "cef", "rbs-browser-cef"] {
            let entry = format!("name = \"{name}\"\n");
            assert!(
                !lock.contains(&entry),
                "{name} is in the workspace Cargo.lock"
            );
        }
        let manifest = fs::read_to_string(workspace_root().join("Cargo.toml")).expect("Cargo.toml");
        assert!(
            manifest.contains("\"crates/rbs-browser-cef\""),
            "crates/rbs-browser-cef must stay in the workspace exclude list"
        );
    }

    #[test]
    fn core_cannot_see_an_adapter_or_wgpu() {
        assert!(deny("rbs-graph", "rbs-codec-nvenc").is_some());
        assert!(deny("rbs-capture", "wgpu").is_some());
        assert!(deny("rbs-core", "bytes").is_none());
        assert!(deny("rbs-scene", "rbs-graph").is_none());
    }

    #[test]
    fn the_engine_cannot_see_an_adapter() {
        assert!(deny("rbs-nodes", "rbs-codec-nvenc").is_some());
        assert!(deny("rbs-engine", "rbs-gpu").is_some());
        assert!(deny("rbs-gpu", "wgpu").is_none());
        assert!(deny("rbs-nodes", "rbs-codec").is_none());
    }

    #[test]
    fn adapters_do_not_depend_on_each_other() {
        assert!(deny("rbs-codec-nvenc", "rbs-codec-amf").is_some());
        assert!(deny("rbs-codec-nvenc", "rbs-codec-hw").is_none());
        assert!(deny("rbs-vcam-mf", "rbs-vcam-mf-source").is_none());
    }

    #[test]
    fn browser_shared_memory_stays_out_of_the_ipc_helper() {
        assert!(deny("rbs-browser-ipc", "libc").is_some());
        assert!(deny("rbs-browser-ipc", "windows").is_some());
        assert!(deny("rbs-browser-shm", "libc").is_none());
        assert!(deny("rbs-browser-shm", "rbs-browser-runner").is_some());
        assert!(deny("rbs-browser-runner", "rbs-browser-shm").is_none());
        assert!(deny("rbs-sources", "rbs-browser-shm").is_some());
        assert!(deny("rbs-app", "rbs-browser-shm").is_none());
    }

    /// The CEF runner is outside the workspace, so check its manifest here.
    #[test]
    fn the_cef_runner_depends_only_on_its_allow_list() {
        let manifest = workspace_root().join("crates/rbs-browser-cef/Cargo.toml");
        let edges = read_edges(&manifest).unwrap_or_default();
        assert!(!edges.is_empty(), "rbs-browser-cef has dependencies");
        for (_, to) in edges {
            let allowed = matches!(to.as_str(), "cef" | "rbs-browser-ipc" | "rbs-browser-shm")
                || to.starts_with("objc2");
            assert!(allowed, "rbs-browser-cef must not depend on {to}");
        }
    }

    #[test]
    fn an_unclassified_rbs_crate_is_refused() {
        assert!(deny("rbs-core", "rbs-plugin-sdk").is_some());
    }
}
