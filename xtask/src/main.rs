//! Layer check for principle P1 (`docs/architecture/overview.md`, Layers).
//!
//! `cargo xtask check-layers` fails when a crate depends on a layer it may not see.
//! The allowlists are the current tree. A new crate, or a new dependency that crosses
//! a layer, is classified here in the same change as the overview table.

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use toml::Value;

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
        _ => {
            eprintln!("usage: cargo xtask check-layers");
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
        Class::AppUi => deny_named(
            to,
            to_class,
            &["rbs-ui", "slint", "i-slint-core"],
            "generated UI code depends on the UI model and Slint",
        ),
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
            | Class::Tool
            | Class::Plugin
            | Class::PluginSdk,
        ) => Some("the engine never depends on an adapter, the control API or the UI".to_owned()),
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
            | Class::Tool
            | Class::Plugin
            | Class::PluginSdk,
        ) => Some("an adapter sees its port, foundation and its support crate".to_owned()),
        Some(Class::Core | Class::Helper | Class::Support) => None,
        None if is_ui_toolkit(to) => Some("an adapter does not use a UI toolkit".to_owned()),
        None => None,
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
        | "rbs-mux" | "rbs-transport" | "rbs-vcam" => Class::Core,
        "rbs-params" | "rbs-media-format" | "rbs-crash" | "rbs-vcam-ring" => Class::Helper,
        "rbs-codec-hw" | "rbs-vcam-mf-source" => Class::Support,
        "rbs-gpu" | "rbs-audio" | "rbs-audio-filters" | "rbs-engine" | "rbs-nodes"
        | "rbs-sources" | "rbs-record" | "rbs-filters" => Class::Engine,
        "rbs-api" => Class::Control,
        "rbs-api-ws" | "rbs-api-ipc" => Class::Transport,
        "rbs-ui" => Class::Ui,
        "rbs-app-ui" => Class::AppUi,
        "rbs-remote-settings" => Class::Settings,
        "rbs-user-data" => Class::UserData,
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
        | "rbs-mux-mpegts"
        | "rbs-mux-flv"
        | "rbs-mux-fmp4"
        | "rbs-mux-matroska"
        | "rbs-transport-file"
        | "rbs-transport-rtmp"
        | "rbs-transport-srt"
        | "rbs-thread-os"
        | "rbs-vcam-mf"
        | "rbs-vcam-test" => Class::Adapter,
        other if other.starts_with("rbs-") => return None,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn an_unclassified_rbs_crate_is_refused() {
        assert!(deny("rbs-core", "rbs-plugin-sdk").is_some());
    }
}
