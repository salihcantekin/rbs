//! Layer check for principle P1 (`docs/architecture/overview.md`, Layers).
//!
//! `cargo xtask check-layers` fails when a crate depends on a layer it may not see.
//! Dev-dependencies are checked for adapters and vendor SDKs. An engine crate may
//! name those in a dev-dependency for an example harness. A normal dependency may not.
//! The allowlists are the current tree. A new crate, or a new dependency that crosses
//! a layer, is classified here in the same change as the overview table.
//!
//! `cargo xtask check-ui` runs the counted UI checks of ADR-0115 (`check_ui`).
//!
//! `cargo xtask affected` checks, lints or tests only the crates a change can break
//! (`affected`, `docs/specs/testing-strategy.md`).
//!
//! `cargo xtask ui-dev` runs the app with the UI live preview (`ui_dev`).
//!
//! `cargo xtask check-size` keeps long files and functions from growing (`check_size`).
//!
//! `cargo xtask device-check <scenario>` runs the checks only a real machine can run
//! and summarises their logs (`device_check`).
//!
//! `cargo xtask verify` runs all of the above that a push needs, in one command
//! (`verify`, owner decision D3 of 2026-11-20).

mod affected;
mod check_logs;
mod check_size;
mod check_ui;
mod device_check;
mod ui_dev;
mod ui_split;
mod verify;

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
        Some("check-size") => {
            let rest: Vec<String> = args.collect();
            match check_size::run(&workspace_root(), &rest) {
                Ok(()) => ExitCode::SUCCESS,
                Err(errors) => {
                    for error in errors {
                        eprintln!("check-size: {error}");
                    }
                    ExitCode::from(1)
                }
            }
        }
        Some("verify") => {
            let rest: Vec<String> = args.collect();
            match verify::run(&workspace_root(), &rest) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("verify: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Some("device-check") => {
            let rest: Vec<String> = args.collect();
            match device_check::run(&workspace_root(), &rest) {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("device-check: {error}");
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
                "usage: cargo xtask check-layers\n{}\n{}\n{}\n{}\n{}\n{}",
                verify::USAGE,
                device_check::USAGE,
                check_ui::USAGE,
                check_size::USAGE,
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
    for (from, to, dev) in &edges {
        seen.insert(from.clone());
        if let Some(error) = denied(from, to, *dev) {
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
    errors.extend(check_logs::run(&root));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn read_edges(manifest: &Path) -> Result<Vec<(String, String, bool)>, Vec<String>> {
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
    push_dep_table(&mut edges, &name, value.get("dependencies"), false);
    push_dep_table(&mut edges, &name, value.get("dev-dependencies"), true);
    if let Some(targets) = value.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            push_dep_table(&mut edges, &name, target.get("dependencies"), false);
            push_dep_table(&mut edges, &name, target.get("dev-dependencies"), true);
        }
    }
    Ok(edges)
}

fn push_dep_table(
    edges: &mut Vec<(String, String, bool)>,
    name: &str,
    table: Option<&Value>,
    dev: bool,
) {
    let Some(deps) = table.and_then(Value::as_table) else {
        return;
    };
    for dep in deps.keys() {
        edges.push((name.to_owned(), dep.clone(), dev));
    }
}

/// `dev` marks a dev-dependency. Tests and benches may name criterion. What a
/// dev-dependency must not hide is an adapter or a vendor SDK behind a core,
/// helper, support or engine crate. An engine crate may name those for an
/// example (`rbs-nodes` `examples/record.rs` and `native_encode.rs`).
fn denied(from: &str, to: &str, dev: bool) -> Option<String> {
    // Before the dev-edge shortcut: no crate may reach every adapter through a
    // dev-dependency on the platform list either (ADR-0160 decision 5).
    if let Some(why) = deny_platform_user(from, to).or_else(|| deny_host_user(from, to)) {
        return Some(why);
    }
    if !dev {
        return deny(from, to);
    }
    let from_class = class(from);
    // Before the early return below: the host's tests may name adapters, but never a
    // root, an app UI crate or a toolkit (ADR-0159 decision 3).
    if from_class == Some(Class::StudioHost) {
        return deny_host_dev(from, to);
    }
    let watched = matches!(
        from_class,
        Some(Class::Core | Class::Helper | Class::Support | Class::Engine)
    );
    if !watched {
        return None;
    }
    let adapterish = matches!(class(to), Some(Class::Adapter)) || is_ffmpeg(to) || is_os_media(to);
    if !adapterish {
        return None;
    }
    if from_class == Some(Class::Engine)
        && (matches!(class(to), Some(Class::Adapter)) || matches!(to, "windows" | "libc"))
    {
        return None;
    }
    deny(from, to)
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
    // The log file writer serves composition roots and the browser helpers only
    // (ADR-0158 decision 4).
    if to == "rbs-log" && !(class(from) == Some(Class::App) || from == "rbs-browser-runner") {
        return Some("only composition roots and the browser helpers use rbs-log".to_owned());
    }
    if let Some(why) = deny_platform_user(from, to).or_else(|| deny_host_user(from, to)) {
        return Some(why);
    }
    let from_class = class(from)?;
    let to_class = class(to);
    match from_class {
        Class::Core => deny_core(to, to_class),
        Class::Helper => deny_helper(from, to, to_class),
        Class::Diagnostics => deny_named(
            to,
            to_class,
            &["rbs-core", "rbs-crash", "log"],
            "rbs-log depends on rbs-core, rbs-crash and log only (ADR-0158)",
        ),
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
        Class::Denoise => deny_named(
            to,
            to_class,
            &["realfft"],
            "the speech denoiser is pure DSP on realfft (ADR-0130)",
        ),
        Class::RecordingModel => deny_named(
            to,
            to_class,
            &["rbs-core", "toml"],
            "the recording model sees no engine, port, adapter or UI crate (ADR-0089)",
        ),
        Class::EditorProject => deny_named(
            to,
            to_class,
            &["rbs-core", "rbs-manifest"],
            "an editor project is written from the recording manifest only (ADR-0089)",
        ),
        Class::Plugin => deny_named(
            to,
            to_class,
            &["rbs-plugin-sdk"],
            "the plugin host reads manifests and does not load libraries or see the engine",
        ),
        Class::Platform => deny_platform(to, to_class),
        Class::StudioHost => deny_studio_host(to, to_class),
        Class::App | Class::Tool => None,
    }
}

/// Only the composition roots that register built-in adapters use the platform list
/// (ADR-0160 decision 5); `rbs-mcp` uses no adapter (ADR-0052 decision 2).
fn deny_platform_user(from: &str, to: &str) -> Option<String> {
    if to == "rbs-platform" && !matches!(from, "rbs-app" | "rbs-cli") {
        Some("only rbs-app and rbs-cli use the platform adapter list (ADR-0160)".to_owned())
    } else {
        None
    }
}

/// Only composition roots use the studio host; its own test edge is exempt
/// (ADR-0159 decision 3).
fn deny_host_user(from: &str, to: &str) -> Option<String> {
    if to == "rbs-studio-host" && from != to && class(from) != Some(Class::App) {
        Some("only composition roots use the studio host (ADR-0159)".to_owned())
    } else {
        None
    }
}

/// The studio host (ADR-0159 decision 3): foundation and ports, helpers, engine
/// crates, the control API, the UI model, settings, user data, add-ons, importers,
/// the recording model, editor projects and four plain data crates. No adapter,
/// support crate, control transport, rbs-log, app UI crate, root or toolkit.
fn deny_studio_host(to: &str, to_class: Option<Class>) -> Option<String> {
    let allowed = match to_class {
        Some(
            Class::Core
            | Class::Helper
            | Class::Engine
            | Class::Control
            | Class::Ui
            | Class::Settings
            | Class::UserData
            | Class::AddOns
            | Class::Import
            | Class::RecordingModel
            | Class::EditorProject,
        ) => true,
        Some(_) => false,
        None => matches!(to, "serde" | "serde_json" | "toml" | "chrono"),
    };
    if allowed {
        None
    } else {
        Some(
            "the studio host sees no adapter, toolkit or OS binding; adapters come through HostConfig (ADR-0159)"
                .to_owned(),
        )
    }
}

/// The studio host's dev-dependencies: itself, adapters for tests and what the host
/// may use, but no composition root, app UI crate or toolkit (ADR-0159 decision 3).
fn deny_host_dev(from: &str, to: &str) -> Option<String> {
    let to_class = class(to);
    if to == from || to_class == Some(Class::Adapter) {
        return None;
    }
    if matches!(to_class, Some(Class::App | Class::AppUi)) || is_ui_toolkit(to) || to == "winit" {
        return Some(
            "the studio host's tests link no root, app UI or toolkit (ADR-0159)".to_owned(),
        );
    }
    if to_class.is_some() {
        return deny_studio_host(to, to_class);
    }
    None
}

/// The platform adapter list (ADR-0160 decision 5): ports, helpers, support crates
/// without OS bindings, engine crates, adapters other than the two that stay in
/// rbs-app, and chrono for the text clock.
fn deny_platform(to: &str, to_class: Option<Class>) -> Option<String> {
    let allowed = match to_class {
        Some(Class::Core | Class::Helper | Class::Engine) => true,
        Some(Class::Support) => !matches!(
            to,
            "rbs-browser-shm" | "rbs-vcam-mf-source" | "rbs-system-probe"
        ),
        Some(Class::Adapter) => !matches!(to, "rbs-add-ons-sys" | "rbs-browser-runner"),
        Some(_) => false,
        None => to == "chrono",
    };
    if allowed {
        None
    } else {
        Some(
            "the platform list sees ports, engine crates and adapters only; OS bindings come through an adapter"
                .to_owned(),
        )
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
    // Mach ports and messages for cross-process surfaces: the preview export and the
    // browser surface handover (ADR-0028, ADR-0167 decision 3). macOS only.
    if from == "rbs-mach" && to == "mach2" {
        return None;
    }
    // Read-only OS information shared by adapters and composition roots: free space,
    // memory and the app identity that holds the OS privacy grants (ADR-0090, ADR-0141).
    // It never changes OS state; anything that does belongs in an adapter.
    if from == "rbs-system-probe" && matches!(to, "libc" | "windows-sys") {
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
            | Class::PluginSdk
            | Class::Diagnostics
            | Class::EditorProject
            | Class::Platform
            | Class::StudioHost,
        ) => Some("the engine never depends on an adapter, the control API or the UI".to_owned()),
        // Support crates that carry OS bindings stay with their adapters.
        Some(Class::Support)
            if matches!(
                to,
                "rbs-browser-shm" | "rbs-vcam-mf-source" | "rbs-system-probe"
            ) =>
        {
            Some("the engine never reaches OS bindings through a support crate".to_owned())
        }
        Some(
            Class::Core
            | Class::Helper
            | Class::Support
            | Class::Engine
            | Class::Denoise
            | Class::RecordingModel,
        ) => None,
        None if is_ui_toolkit(to) || is_ffmpeg(to) || is_os_media(to) && from != "rbs-gpu" => Some(
            "OS media and UI toolkits stay in adapters; wgpu stays in the GPU crates".to_owned(),
        ),
        None => None,
    }
}

fn deny_adapter(from: &str, to: &str, to_class: Option<Class>) -> Option<String> {
    // The add-ons adapter implements the traits of rbs-add-ons, its port (ADR-0145).
    if from == "rbs-add-ons-sys" && to == "rbs-add-ons" {
        return None;
    }
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
            | Class::PluginSdk
            | Class::Denoise
            | Class::RecordingModel
            | Class::EditorProject
            | Class::Platform
            | Class::StudioHost,
        ) => Some("an adapter sees its port, foundation and its support crate".to_owned()),
        // Diagnostics: only rbs-browser-runner gets here (the rbs-log rule in `deny`).
        Some(Class::Core | Class::Helper | Class::Support | Class::Diagnostics) => None,
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
    /// `rbs-denoise`: the speech denoiser (ADR-0130).
    Denoise,
    /// `rbs-manifest`: the recording model (ADR-0089).
    RecordingModel,
    /// `rbs-log`: the diagnostics log file (ADR-0158).
    Diagnostics,
    /// `rbs-project`: editor projects, used only by composition roots (ADR-0089).
    EditorProject,
    /// `rbs-platform`: the built-in adapter list of rbs-app and rbs-cli (ADR-0160).
    Platform,
    /// `rbs-studio-host`: the studio host, free of toolkits and adapters (ADR-0159).
    StudioHost,
}

/// `None` for a crate from outside the workspace. A workspace crate that is missing
/// from this match is reported by `check_workspace`, which fails the build.
fn class(name: &str) -> Option<Class> {
    Some(match name {
        "rbs-core" | "rbs-graph" | "rbs-scene" | "rbs-capture" | "rbs-audio-io" | "rbs-input" | "rbs-codec"
        | "rbs-mux" | "rbs-transport" | "rbs-vcam" | "rbs-media-decode" | "rbs-text" | "rbs-segment" => Class::Core,
        "rbs-params" | "rbs-media-format" | "rbs-crash" | "rbs-vcam-ring" | "rbs-browser-ipc" => {
            Class::Helper
        }
        "rbs-codec-hw" | "rbs-vcam-mf-source" | "rbs-browser-shm" | "rbs-system-probe"
        | "rbs-mach" => Class::Support,
        "rbs-gpu" | "rbs-audio" | "rbs-audio-filters" | "rbs-engine" | "rbs-nodes"
        | "rbs-sources" | "rbs-record" | "rbs-filters" => Class::Engine,
        "rbs-log" => Class::Diagnostics,
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
        "rbs-platform" => Class::Platform,
        "rbs-studio-host" => Class::StudioHost,
        "rbs-plugin-sdk" => Class::PluginSdk,
        "rbs-plugin-host" => Class::Plugin,
        "xtask" => Class::Tool,
        "rbs-denoise" => Class::Denoise,
        "rbs-manifest" => Class::RecordingModel,
        "rbs-project" => Class::EditorProject,
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
        | "rbs-input-mac"
        | "rbs-input-win"
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
        | "rbs-vcam-mf"
        | "rbs-vcam-test"
        | "rbs-media-decode-test"
        | "rbs-media-anim"
        | "rbs-media-avf"
        | "rbs-media-mf"
        | "rbs-text-test"
        | "rbs-text-parley"
        | "rbs-browser-runner"
        | "rbs-add-ons-sys"
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

    /// ADR-0132 decision 2 and ADR-0145 decision 3: the browser engine's pin names
    /// the CEF version the runner's `cef` crate is built against (the build metadata
    /// of its version in the runner's own lock file, `156.6.0+156.0.30` -> `156.0.30`).
    #[test]
    #[allow(clippy::expect_used)]
    fn the_cef_pin_matches_the_runner_lock_file() {
        let dir = workspace_root().join("crates/rbs-browser-cef");
        let pin: Value = fs::read_to_string(dir.join("cef-pin.toml"))
            .expect("cef-pin.toml")
            .parse()
            .expect("cef-pin.toml is TOML");
        let pinned = pin.get("version").and_then(Value::as_str).expect("version");
        let lock: Value = fs::read_to_string(dir.join("Cargo.lock"))
            .expect("runner Cargo.lock")
            .parse()
            .expect("runner Cargo.lock is TOML");
        let cef = lock
            .get("package")
            .and_then(Value::as_array)
            .and_then(|packages| {
                packages
                    .iter()
                    .find(|p| p.get("name").and_then(Value::as_str) == Some("cef"))
            })
            .and_then(|p| p.get("version"))
            .and_then(Value::as_str)
            .expect("the cef crate in the runner lock file");
        let built = cef
            .split_once('+')
            .map(|(_, cef)| cef)
            .expect("cef version has +CEF");
        assert_eq!(
            pinned, built,
            "cef-pin.toml pins CEF {pinned}, the runner's cef crate {cef} is built against {built}"
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
    fn an_engine_example_may_name_an_adapter() {
        assert!(denied("rbs-nodes", "rbs-codec-nvenc", true).is_none());
        assert!(denied("rbs-nodes", "windows", true).is_none());
        assert!(denied("rbs-nodes", "rbs-codec-nvenc", false).is_some());
        assert!(denied("rbs-core", "rbs-codec-nvenc", true).is_some());
        assert!(denied("rbs-graph", "criterion", true).is_none());
        assert!(denied("rbs-transport-srt", "rbs-mux-mpegts", true).is_none());
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

    #[test]
    fn adapters_share_os_facts_through_the_system_probe() {
        assert!(deny("rbs-capture-sck", "rbs-system-probe").is_none());
        assert!(deny("rbs-system-probe", "windows-sys").is_none());
        assert!(deny("rbs-system-probe", "rbs-capture-sck").is_some());
        assert!(deny("rbs-record", "rbs-system-probe").is_some());
    }

    #[test]
    fn the_recording_model_and_editor_projects_stay_small() {
        assert!(deny("rbs-record", "rbs-manifest").is_none());
        assert!(deny("rbs-manifest", "rbs-graph").is_some());
        assert!(deny("rbs-project", "rbs-manifest").is_none());
        assert!(deny("rbs-record", "rbs-project").is_some());
        assert!(deny("rbs-cli", "rbs-project").is_none());
        assert!(deny("rbs-audio-filters", "rbs-denoise").is_none());
        assert!(deny("rbs-denoise", "rbs-core").is_some());
    }

    #[test]
    fn only_the_adapter_roots_use_the_platform_list() {
        assert!(deny("rbs-app", "rbs-platform").is_none());
        assert!(deny("rbs-cli", "rbs-platform").is_none());
        assert!(deny("rbs-mcp", "rbs-platform").is_some());
        assert!(deny("rbs-record", "rbs-platform").is_some());
        assert!(denied("rbs-mcp", "rbs-platform", true).is_some());
        assert!(denied("rbs-record", "rbs-platform", true).is_some());
        assert!(denied("rbs-app", "rbs-platform", true).is_none());
    }

    #[test]
    fn the_studio_host_sees_no_adapter_or_toolkit() {
        assert!(deny("rbs-studio-host", "rbs-record").is_none());
        assert!(deny("rbs-studio-host", "rbs-api").is_none());
        assert!(deny("rbs-studio-host", "chrono").is_none());
        assert!(deny("rbs-studio-host", "rbs-mux-matroska").is_some());
        assert!(deny("rbs-studio-host", "rbs-system-probe").is_some());
        assert!(deny("rbs-studio-host", "rbs-api-ws").is_some());
        assert!(deny("rbs-studio-host", "rbs-log").is_some());
        assert!(deny("rbs-studio-host", "slint").is_some());
        assert!(deny("rbs-studio-host", "keyring").is_some());
        // Dev edges: adapters and the self edge are allowed, a toolkit or root is not.
        assert!(denied("rbs-studio-host", "rbs-mux-matroska", true).is_none());
        assert!(denied("rbs-studio-host", "rbs-studio-host", true).is_none());
        assert!(denied("rbs-studio-host", "slint", true).is_some());
        assert!(denied("rbs-studio-host", "rbs-app", true).is_some());
        // Only roots use it.
        assert!(deny("rbs-app", "rbs-studio-host").is_none());
        assert!(deny("rbs-cli", "rbs-studio-host").is_none());
        assert!(deny("rbs-record", "rbs-studio-host").is_some());
        assert!(denied("rbs-ui", "rbs-studio-host", true).is_some());
    }

    #[test]
    fn the_platform_list_sees_adapters_but_no_os_bindings() {
        assert!(deny("rbs-platform", "rbs-codec-nvenc").is_none());
        assert!(deny("rbs-platform", "rbs-core").is_none());
        assert!(deny("rbs-platform", "rbs-record").is_none());
        assert!(deny("rbs-platform", "chrono").is_none());
        assert!(deny("rbs-platform", "rbs-api").is_some());
        assert!(deny("rbs-platform", "rbs-log").is_some());
        assert!(deny("rbs-platform", "rbs-system-probe").is_some());
        assert!(deny("rbs-platform", "rbs-add-ons-sys").is_some());
        assert!(deny("rbs-platform", "slint").is_some());
        assert!(deny("rbs-platform", "windows").is_some());
    }

    /// The CEF runner is outside the workspace, so check its manifest here.
    #[test]
    fn the_cef_runner_depends_only_on_its_allow_list() {
        let manifest = workspace_root().join("crates/rbs-browser-cef/Cargo.toml");
        let edges = read_edges(&manifest).unwrap_or_default();
        assert!(!edges.is_empty(), "rbs-browser-cef has dependencies");
        for (_, to, _) in edges {
            // `windows-sys` loads the downloaded engine on Windows (add-ons spec,
            // "Browser engine consumer"); `windows` copies the page into the studio's
            // shared textures with Direct3D 11 (ADR-0109 decision 8). `libc` checks on
            // Unix whether the owner of a stale temporary profile still runs (scene
            // sources spec, browser runner; `windows` does it on Windows). On macOS
            // `rbs-mach` fetches the studio's IOSurfaces and `block2` carries Metal's
            // completed handler (ADR-0167 decisions 3 and 4).
            let allowed = matches!(
                to.as_str(),
                "cef"
                    | "rbs-browser-ipc"
                    | "rbs-browser-shm"
                    | "rbs-mach"
                    | "block2"
                    | "windows-sys"
                    | "windows"
                    | "libc"
            ) || to.starts_with("objc2");
            assert!(allowed, "rbs-browser-cef must not depend on {to}");
        }
    }

    #[test]
    fn an_unclassified_rbs_crate_is_refused() {
        assert!(deny("rbs-core", "rbs-plugin-sdk").is_some());
    }
}
