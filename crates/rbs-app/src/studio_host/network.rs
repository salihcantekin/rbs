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

impl Host {
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
