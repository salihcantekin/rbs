//! The network check of the control API for source settings (ADR-0052 decision 4,
//! `not_permitted` with reason `network`): a caller without "Allow actions that cannot
//! be undone" may not make RBS open a connection to a host of its choosing.
//!
//! The API thread checks what a request says by itself (`Command::reaches_network`).
//! A source's settings and target mean what its kind says, so they are checked here:
//! a browser source's address may not change, an image or media source's path must be
//! a local absolute path (`rbs_api::location::is_local_path`), and a source of a type
//! this build does not know (a plugin type) counts as network, so it fails closed.

use rbs_api::location::is_local_path;
use rbs_api::{ApiError, Command, ErrorKind};
use rbs_scene::model::SourceId;
use rbs_sources::types::keys;

use super::{Host, SourceKind};

/// True when a settings patch for a source of `kind` could make it reach a host.
fn reaches_network<'a>(
    kind: Option<SourceKind>,
    mut patch: impl Iterator<Item = (&'a str, &'a serde_json::Value)>,
) -> bool {
    match kind {
        // A type this build does not know: it may open anything.
        None => true,
        // The address, CSS and the user's script reach hosts; keeping a hidden page
        // running starts a process nothing shows (ADR-0132 decision 4, ADR-0149
        // decisions 3 and 5).
        Some(SourceKind::Browser) => patch.any(|(k, v)| {
            rbs_sources::BROWSER_NETWORK_KEYS.contains(&k)
                && (k != keys::KEEP_RUNNING || v.as_bool() == Some(true))
        }),
        Some(SourceKind::Image | SourceKind::Media) => {
            patch.any(|(k, v)| k == keys::PATH && v.as_str().is_some_and(|p| !is_local_path(p)))
        }
        // Capture targets are listed by their adapters; colours and texts name no
        // location the API may set (a text source's file is refused anyway).
        Some(_) => false,
    }
}

fn refusal(method: &str) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), "network".into());
    ApiError::with_data(
        ErrorKind::NotPermitted,
        format!(
            "{method} could connect to another computer, and this token is not allowed actions that cannot be undone"
        ),
        data,
    )
}

/// True when a browser source with `current` settings, after `patch` (a null removes
/// a key), would run the user's script inside a signed-in account: a non-empty
/// `script` with a run setting on and `keep_signed_in` true (ADR-0149 decision 3).
/// Only when the patch touches one of the keys that decide it, so no order of writes
/// gets around the check.
fn script_in_signed_in_account(
    current: &rbs_scene::model::Settings,
    patch: &std::collections::BTreeMap<String, serde_json::Value>,
) -> bool {
    use rbs_scene::model::SettingValue;
    const DECIDING: [&str; 5] = [
        keys::SCRIPT,
        keys::SCRIPT_ON_LOAD,
        keys::SCRIPT_EVERY_S,
        keys::KEEP_SIGNED_IN,
        keys::URL,
    ];
    if !patch.keys().any(|k| DECIDING.contains(&k.as_str())) {
        return false;
    }
    let value = |key: &str| -> Option<serde_json::Value> {
        match patch.get(key) {
            Some(v) if v.is_null() => None,
            Some(v) => Some(v.clone()),
            None => match current.get(key) {
                Some(SettingValue::Text(t)) => Some(t.clone().into()),
                Some(SettingValue::Bool(b)) => Some((*b).into()),
                Some(SettingValue::Int(i)) => Some((*i).into()),
                _ => None,
            },
        }
    };
    let on = |key: &str| value(key).and_then(|v| v.as_bool()) == Some(true);
    let script = value(keys::SCRIPT)
        .and_then(|v| v.as_str().map(|s| !s.is_empty()))
        .unwrap_or(false);
    let every = value(keys::SCRIPT_EVERY_S)
        .and_then(|v| v.as_i64())
        .is_some_and(|n| n > 0);
    script && (on(keys::SCRIPT_ON_LOAD) || every) && on(keys::KEEP_SIGNED_IN)
}

fn local_only(method: &str) -> ApiError {
    let mut data = serde_json::Map::new();
    data.insert("reason".into(), "local_only".into());
    ApiError::with_data(
        ErrorKind::NotPermitted,
        format!(
            "{method} would run a script inside a signed-in page, which only the person at this computer may set up"
        ),
        data,
    )
}

impl Host {
    /// Refuses a token a write that would leave a browser source running the user's
    /// script inside a signed-in account (`local_only`, ADR-0149 decision 3). The
    /// desktop app and a same-user IPC peer without a token may.
    pub(super) fn check_local_only(&self, token: bool, command: &Command) -> Result<(), ApiError> {
        if !token {
            return Ok(());
        }
        let signed_in_browser = |id: u32| {
            self.collection
                .source(SourceId::from_raw(id))
                .filter(|s| SourceKind::of(&s.type_id) == Some(SourceKind::Browser))
        };
        let refused = match command {
            Command::SourceSetSettings(p) => signed_in_browser(p.source)
                .is_some_and(|s| script_in_signed_in_account(&s.settings, &p.settings)),
            // A script of its own inside a signed-in page; the saved one was set up
            // by the local user.
            Command::BrowserRunScript(p) => {
                p.script.is_some()
                    && signed_in_browser(p.source).is_some_and(|s| {
                        matches!(
                            s.settings.get(keys::KEEP_SIGNED_IN),
                            Some(rbs_scene::model::SettingValue::Bool(true))
                        )
                    })
            }
            _ => false,
        };
        if refused {
            return Err(local_only(command.method()));
        }
        Ok(())
    }

    /// Refuses, without the permission, a source change that could reach a host.
    pub(super) fn check_network(
        &self,
        irreversible: bool,
        command: &Command,
    ) -> Result<(), ApiError> {
        if irreversible {
            return Ok(());
        }
        // A missing source passes: the command itself answers `not_found`.
        let reaches = match command {
            Command::SourceSetSettings(p) => self.source_kind(p.source).is_some_and(|kind| {
                reaches_network(kind, p.settings.iter().map(|(k, v)| (k.as_str(), v)))
            }),
            // The adapter and target are a capture device's; a type this build does
            // not know fails closed.
            Command::SourceRetarget(p) => self.source_kind(p.source).is_some_and(|k| k.is_none()),
            // A click or a key can do anything in a page, a sign-up or a purchase
            // included (ADR-0132 decision 9).
            Command::BrowserInput(p) => self.source_kind(p.source).is_some(),
            _ => false,
        };
        if reaches {
            return Err(refusal(command.method()));
        }
        Ok(())
    }

    /// The source's kind: `None` when it does not exist, `Some(None)` when its type is
    /// not one this build knows.
    fn source_kind(&self, source: u32) -> Option<Option<SourceKind>> {
        self.collection
            .source(SourceId::from_raw(source))
            .map(|s| SourceKind::of(&s.type_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn patch(pairs: &[(&'static str, Value)]) -> Vec<(&'static str, Value)> {
        pairs.to_vec()
    }

    fn check(kind: Option<SourceKind>, pairs: &[(&'static str, Value)]) -> bool {
        let p = patch(pairs);
        reaches_network(kind, p.iter().map(|(k, v)| (*k, v)))
    }

    #[test]
    fn a_browser_address_and_unknown_types_reach_the_network() {
        assert!(check(
            Some(SourceKind::Browser),
            &[(keys::URL, json!("https://example.com"))]
        ));
        assert!(!check(
            Some(SourceKind::Browser),
            &[(keys::TRANSPARENT, json!(true))]
        ));
        for key in [
            keys::CSS,
            keys::SCRIPT,
            keys::SCRIPT_ON_LOAD,
            keys::SCRIPT_EVERY_S,
        ] {
            assert!(
                check(Some(SourceKind::Browser), &[(key, json!("x"))]),
                "{key}"
            );
        }
        assert!(check(
            Some(SourceKind::Browser),
            &[(keys::KEEP_RUNNING, json!(true))]
        ));
        assert!(!check(
            Some(SourceKind::Browser),
            &[(keys::KEEP_RUNNING, json!(false))]
        ));
        assert!(!check(
            Some(SourceKind::Browser),
            &[(keys::ZOOM_PERCENT, json!(150))]
        ));
        assert!(check(None, &[]));
    }

    #[test]
    fn a_script_in_a_signed_in_page_is_found_for_every_order_of_writes() {
        use rbs_scene::model::{SettingValue, Settings};
        use std::collections::BTreeMap;
        let settings = |pairs: &[(&str, SettingValue)]| -> Settings {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        let patch = |pairs: &[(&str, Value)]| -> BTreeMap<String, Value> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect()
        };
        let script = (keys::SCRIPT, SettingValue::Text("go()".into()));
        let on_load = (keys::SCRIPT_ON_LOAD, SettingValue::Bool(true));
        let signed = (keys::KEEP_SIGNED_IN, SettingValue::Bool(true));
        // Script first, then sign-in.
        let has_script = settings(&[script.clone(), on_load.clone()]);
        assert!(script_in_signed_in_account(
            &has_script,
            &patch(&[(keys::KEEP_SIGNED_IN, json!(true))])
        ));
        // Sign-in first, then the script.
        let has_sign_in = settings(std::slice::from_ref(&signed));
        assert!(script_in_signed_in_account(
            &has_sign_in,
            &patch(&[
                (keys::SCRIPT, json!("go()")),
                (keys::SCRIPT_EVERY_S, json!(30))
            ])
        ));
        // A token moving such a page elsewhere.
        let both = settings(&[script.clone(), on_load.clone(), signed.clone()]);
        assert!(script_in_signed_in_account(
            &both,
            &patch(&[(keys::URL, json!("https://other.example/"))])
        ));
        // Turning it off, or keys that do not decide it, pass.
        assert!(!script_in_signed_in_account(
            &both,
            &patch(&[(keys::KEEP_SIGNED_IN, Value::Null)])
        ));
        assert!(!script_in_signed_in_account(
            &both,
            &patch(&[(keys::ZOOM_PERCENT, json!(150))])
        ));
        // A script with no run setting does not run by itself.
        assert!(!script_in_signed_in_account(
            &has_sign_in,
            &patch(&[(keys::SCRIPT, json!("go()"))])
        ));
    }

    #[test]
    fn an_image_or_media_path_must_be_local() {
        for kind in [SourceKind::Image, SourceKind::Media] {
            assert!(!check(Some(kind), &[(keys::PATH, json!("/home/a/b.png"))]));
            assert!(check(
                Some(kind),
                &[(keys::PATH, json!("\\\\server\\share\\b.png"))]
            ));
            assert!(check(Some(kind), &[(keys::PATH, json!("rtsp://cam/s"))]));
        }
    }

    #[test]
    fn capture_colour_and_text_settings_do_not() {
        assert!(!check(
            Some(SourceKind::Camera),
            &[(keys::TARGET, json!("usb:1"))]
        ));
        assert!(!check(
            Some(SourceKind::Color),
            &[(keys::COLOR, json!("#ffffff"))]
        ));
        assert!(!check(
            Some(SourceKind::Text),
            &[(keys::TEXT, json!("see https://example.com"))]
        ));
    }
}
