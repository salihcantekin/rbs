//! The Page tab of a browser source in Properties and in the Source settings window
//! (ADR-0109, ADR-0132, ADR-0149): what it shows, read from the source's stored
//! settings, and the setting patches its edits send. Pure functions; the studio sends
//! each patch as one undo step.

use rbs_scene::model::{SettingValue, Settings};
use rbs_sources::types::keys;
use rbs_sources::{
    BROWSER_CAP, BROWSER_DEFAULT_FPS, BROWSER_DEFAULT_SIZE, BROWSER_NO_ADDRESS,
    BROWSER_RUNTIME_MISMATCH, BROWSER_RUNTIME_MISSING, DEFAULT_ZOOM, browser_scheme,
};
use serde_json::Value;

use super::text_props::Patch;
use crate::ui::BrowserData;

/// The Page tab's `state` (types.slint `BrowserData`).
pub mod state {
    /// The page runs, or will when shown.
    pub const OK: i32 = 0;
    /// The browser engine is not installed (or does not match).
    pub const ENGINE: i32 = 1;
    /// Both page slots are taken.
    pub const CAP: i32 = 2;
    /// The page failed; `status` says why.
    pub const FAILED: i32 = 3;
    /// Hidden, so not running (ADR-0107).
    pub const HIDDEN: i32 = 4;
    /// The page runs, drawn without the graphics card (ADR-0157 decision 9).
    pub const GPU_FALLBACK: i32 = 5;
}

/// How the source runs, as the Page tab words it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Run {
    /// The page runs now.
    pub running: bool,
    /// The control-API reason it does not, when known.
    pub reason: Option<&'static str>,
    /// Why it failed otherwise; empty when it did not.
    pub failed: String,
}

fn text_of(settings: &Settings, key: &str) -> String {
    match settings.get(key) {
        Some(SettingValue::Text(t)) => t.clone(),
        _ => String::new(),
    }
}

fn int_of(settings: &Settings, key: &str, default: i64) -> i32 {
    match settings.get(key) {
        Some(SettingValue::Int(i)) => i32::try_from(*i).unwrap_or(i32::MAX),
        _ => i32::try_from(default).unwrap_or(0),
    }
}

fn flag_of(settings: &Settings, key: &str, default: bool) -> bool {
    match settings.get(key) {
        Some(SettingValue::Bool(on)) => *on,
        _ => default,
    }
}

/// True when the page has an address to load; a page without one draws nothing, so
/// editing pictures show a placeholder outline where it sits.
pub fn has_address(settings: &Settings) -> bool {
    !text_of(settings, keys::URL).trim().is_empty()
}

/// True for a `file:` address.
fn is_file(url: &str) -> bool {
    url.trim().to_ascii_lowercase().starts_with("file:")
}

/// The file name of a `file:` address, without its folder.
fn file_name(url: &str) -> String {
    let path = url.trim();
    let path = path.get("file://".len()..).unwrap_or(path);
    path.rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .replace("%20", " ")
}

/// The Page tab's data for the browser source `id` with `settings`. `canvas` is the
/// page size "Use canvas size" sets.
pub fn browser_data(id: i32, settings: &Settings, run: &Run, canvas: (i64, i64)) -> BrowserData {
    let url = text_of(settings, keys::URL);
    let file = is_file(&url);
    let (default_w, default_h) = BROWSER_DEFAULT_SIZE;
    let state = match run.reason {
        Some(r) if r == BROWSER_RUNTIME_MISSING || r == BROWSER_RUNTIME_MISMATCH => state::ENGINE,
        Some(r) if r == BROWSER_CAP => state::CAP,
        Some(r) if r == BROWSER_NO_ADDRESS => state::OK,
        Some(r) if r == rbs_app::browser_tree::GPU_FALLBACK && run.running => state::GPU_FALLBACK,
        _ if run.running => state::OK,
        _ if !run.failed.is_empty() => state::FAILED,
        _ => state::HIDDEN,
    };
    BrowserData {
        on: true,
        id,
        file,
        file_name: if file && url.len() > "file://".len() {
            file_name(&url).into()
        } else {
            Default::default()
        },
        url: url.into(),
        width: int_of(settings, keys::WIDTH, i64::from(default_w)),
        height: int_of(settings, keys::HEIGHT, i64::from(default_h)),
        canvas_w: i32::try_from(canvas.0).unwrap_or(0),
        canvas_h: i32::try_from(canvas.1).unwrap_or(0),
        fps: int_of(settings, keys::FPS, i64::from(BROWSER_DEFAULT_FPS)),
        zoom: int_of(settings, keys::ZOOM_PERCENT, i64::from(DEFAULT_ZOOM)),
        transparent: flag_of(settings, keys::TRANSPARENT, false),
        css: text_of(settings, keys::CSS).into(),
        reload_on_file_change: flag_of(settings, keys::RELOAD_ON_FILE_CHANGE, false),
        keep_signed_in: flag_of(settings, keys::KEEP_SIGNED_IN, false),
        reload_every: int_of(settings, keys::RELOAD_EVERY_S, 0),
        script: text_of(settings, keys::SCRIPT).into(),
        script_on_load: flag_of(settings, keys::SCRIPT_ON_LOAD, false),
        script_every: int_of(settings, keys::SCRIPT_EVERY_S, 0),
        // Filled in from the source's activity policy by the caller (ADR-0152).
        run: -1,
        on_return: -1,
        linger: -1,
        retry_on_error: flag_of(settings, keys::RETRY_ON_ERROR, true),
        running: run.running,
        state,
        status: if state == state::FAILED {
            run.failed.as_str().into()
        } else {
            Default::default()
        },
    }
}

/// A text setting of the Page tab. `url` must be an accepted address (an invalid one
/// sends nothing, so the field shows the stored one again); an empty `css` or
/// `script` removes the key.
pub fn value_patch(key: &str, value: &str) -> Patch {
    let mut patch = Patch::new();
    match key {
        keys::URL => {
            let url = value.trim();
            if browser_scheme(url).is_ok() {
                patch.insert(key.to_owned(), Value::from(url));
            } else if !url.is_empty() && !url.contains("://") {
                // A bare host name is a web address (U3: fewer steps).
                patch.insert(key.to_owned(), Value::from(format!("https://{url}")));
            }
        }
        keys::CSS | keys::SCRIPT => {
            let v = if value.is_empty() {
                Value::Null
            } else {
                Value::from(value)
            };
            patch.insert(key.to_owned(), v);
        }
        _ => {}
    }
    patch
}

/// A number of the Page tab, already clamped by the field. A default value removes
/// the key, so a document stays at the oldest format that holds it.
pub fn number_patch(key: &str, value: f64) -> Patch {
    let mut patch = Patch::new();
    let n = value.round() as i64;
    let v = match key {
        keys::WIDTH | keys::HEIGHT | keys::FPS => Value::from(n),
        keys::ZOOM_PERCENT if n == i64::from(DEFAULT_ZOOM) => Value::Null,
        keys::RELOAD_EVERY_S | keys::SCRIPT_EVERY_S if n == 0 => Value::Null,
        keys::ZOOM_PERCENT | keys::RELOAD_EVERY_S | keys::SCRIPT_EVERY_S => Value::from(n),
        _ => return patch,
    };
    patch.insert(key.to_owned(), v);
    patch
}

/// A switch of the Page tab. Off (on for `retry_on_error`, its default) removes the key.
pub fn flag_patch(key: &str, on: bool) -> Patch {
    let mut patch = Patch::new();
    let default = key == keys::RETRY_ON_ERROR;
    if ![
        keys::TRANSPARENT,
        keys::RELOAD_ON_FILE_CHANGE,
        keys::KEEP_SIGNED_IN,
        keys::SCRIPT_ON_LOAD,
        keys::RETRY_ON_ERROR,
    ]
    .contains(&key)
    {
        return patch;
    }
    let v = if on == default {
        Value::Null
    } else {
        Value::from(on)
    };
    patch.insert(key.to_owned(), v);
    patch
}

/// The `file:` address of a picked page file.
pub fn file_url(path: &std::path::Path) -> String {
    let mut text = String::new();
    for c in path.to_string_lossy().chars() {
        match c {
            '\\' => text.push('/'),
            ' ' => text.push_str("%20"),
            '#' => text.push_str("%23"),
            '?' => text.push_str("%3F"),
            '%' => text.push_str("%25"),
            c => text.push(c),
        }
    }
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// The browser sources of `rows`, by id: their stored settings and how they run.
pub fn rows(
    rows: &[rbs_app::studio_host::SourceRow],
) -> std::collections::BTreeMap<i32, (Settings, Run)> {
    use rbs_app::studio_host::SourceStatus;
    rows.iter()
        .filter_map(|r| {
            let run = Run {
                running: matches!(r.status, SourceStatus::Running),
                reason: r.browser_reason,
                failed: match &r.status {
                    SourceStatus::Failed(why) => why.clone(),
                    SourceStatus::Unavailable { detail, .. } => detail.clone(),
                    _ => String::new(),
                },
            };
            Some((r.id.get() as i32, (r.browser.clone()?, run)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str) -> Settings {
        let mut s = Settings::new();
        s.insert(keys::URL.into(), SettingValue::Text(url.into()));
        s
    }

    #[test]
    fn the_tab_reads_defaults_and_the_state() {
        let run = Run {
            running: true,
            ..Run::default()
        };
        let d = browser_data(3, &page("https://example.com/"), &run, (1920, 1080));
        assert!(d.on && !d.file && d.running);
        assert_eq!((d.width, d.height, d.fps, d.zoom), (1920, 1080, 30, 100));
        assert!(d.retry_on_error);
        assert_eq!(d.state, state::OK);
        let missing = Run {
            reason: Some(BROWSER_RUNTIME_MISSING),
            ..Run::default()
        };
        assert_eq!(
            browser_data(3, &page("https://a/"), &missing, (0, 0)).state,
            state::ENGINE
        );
        let failed = Run {
            failed: "no answer".into(),
            ..Run::default()
        };
        let d = browser_data(3, &page("https://a/"), &failed, (0, 0));
        assert_eq!((d.state, d.status.as_str()), (state::FAILED, "no answer"));
        let fell_back = Run {
            running: true,
            reason: Some(rbs_app::browser_tree::GPU_FALLBACK),
            ..Run::default()
        };
        let d = browser_data(3, &page("https://a/"), &fell_back, (0, 0));
        assert_eq!(d.state, state::GPU_FALLBACK);
        let d = browser_data(
            3,
            &page("file:///C:/overlays/score.html"),
            &Run::default(),
            (0, 0),
        );
        assert!(d.file);
        assert_eq!(d.file_name.as_str(), "score.html");
        assert_eq!(d.state, state::HIDDEN);
    }

    #[test]
    fn edits_send_minimal_patches() {
        assert_eq!(
            value_patch(keys::URL, " example.com/chat ")[keys::URL],
            Value::from("https://example.com/chat")
        );
        assert!(value_patch(keys::URL, "ftp://x").is_empty());
        assert_eq!(value_patch(keys::CSS, "")[keys::CSS], Value::Null);
        assert_eq!(
            number_patch(keys::ZOOM_PERCENT, 100.0)[keys::ZOOM_PERCENT],
            Value::Null
        );
        assert_eq!(
            number_patch(keys::ZOOM_PERCENT, 150.0)[keys::ZOOM_PERCENT],
            Value::from(150)
        );
        assert_eq!(
            number_patch(keys::RELOAD_EVERY_S, 0.0)[keys::RELOAD_EVERY_S],
            Value::Null
        );
        assert!(number_patch("profile_id", 1.0).is_empty());
        assert_eq!(
            flag_patch(keys::RETRY_ON_ERROR, true)[keys::RETRY_ON_ERROR],
            Value::Null
        );
        assert_eq!(
            flag_patch(keys::RETRY_ON_ERROR, false)[keys::RETRY_ON_ERROR],
            Value::from(false)
        );
        // "Runs" replaced the switch (ADR-0152): not a Page tab flag any more.
        assert!(flag_patch(keys::KEEP_RUNNING, true).is_empty());
        assert!(flag_patch(keys::PROFILE_ID, true).is_empty());
        assert_eq!(
            file_url(std::path::Path::new("C:\\o\\a b.html")),
            "file:///C:/o/a%20b.html"
        );
        assert_eq!(
            file_url(std::path::Path::new("/o/a.html")),
            "file:///o/a.html"
        );
    }
}
