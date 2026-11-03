//! Panel ids and the registry of built-in panels.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Panel model".

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum length of a panel id in bytes, parameters included.
pub const MAX_PANEL_ID_LEN: usize = 128;

/// A stable panel id, optionally with parameters: `rbs.mixer`,
/// `rbs.program?canvas=vertical`, `<plugin-id>.<panel>`.
///
/// The base is lowercase ASCII letters, digits, `.`, `-` and `_`, and contains at
/// least one `.`. Parameters are `key=value` pairs joined by `&` with the same
/// characters. Ids that no registry knows are valid: they are kept in workspace
/// files and shown as placeholders (an uninstalled plugin's panel, for example).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PanelId(String);

/// Why a string is not a valid panel id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelIdError {
    /// The rejected input.
    pub input: String,
    /// What is wrong with it.
    pub reason: &'static str,
}

impl fmt::Display for PanelIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid panel id {:?}: {}", self.input, self.reason)
    }
}

impl std::error::Error for PanelIdError {}

fn valid_word(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

impl PanelId {
    /// Parse and validate a panel id.
    pub fn parse(s: &str) -> Result<Self, PanelIdError> {
        let err = |reason| PanelIdError {
            input: s.to_owned(),
            reason,
        };
        if s.len() > MAX_PANEL_ID_LEN {
            return Err(err("longer than 128 bytes"));
        }
        let (base, params) = match s.split_once('?') {
            Some((b, p)) => (b, Some(p)),
            None => (s, None),
        };
        if !valid_word(base) || !base.contains('.') || base.starts_with('.') || base.ends_with('.')
        {
            return Err(err(
                "the base must be `namespace.panel` in lowercase letters, digits, `.`, `-` and `_`",
            ));
        }
        if let Some(params) = params {
            let mut keys: Vec<&str> = Vec::new();
            for pair in params.split('&') {
                let Some((k, v)) = pair.split_once('=') else {
                    return Err(err("parameters must be `key=value` pairs joined by `&`"));
                };
                if !valid_word(k) || !valid_word(v) {
                    return Err(err(
                        "parameter keys and values use lowercase letters, digits, `.`, `-` and `_`",
                    ));
                }
                if keys.contains(&k) {
                    return Err(err("a parameter key appears twice"));
                }
                keys.push(k);
            }
        }
        Ok(PanelId(s.to_owned()))
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id without parameters: `rbs.program` for `rbs.program?canvas=vertical`.
    pub fn base(&self) -> &str {
        self.0.split_once('?').map_or(&self.0, |(b, _)| b)
    }

    /// The value of parameter `key`, if present.
    pub fn param(&self, key: &str) -> Option<&str> {
        let (_, params) = self.0.split_once('?')?;
        params
            .split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

impl fmt::Display for PanelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PanelId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PanelId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PanelId::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// How often a visible panel needs to be redrawn without user input.
///
/// The shell runs at most one timer, at the fastest rate any visible panel
/// needs, and no timer when every visible panel is [`UpdateRate::OnChange`].
/// Hidden panels never count (ADR-0013). Lower rates for meters and text are
/// part of the UI CPU budget (`docs/specs/performance-budgets.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UpdateRate {
    /// Only when its data or the user changes something.
    OnChange,
    /// At most this many times per second (meters, statistics).
    Hz(u16),
    /// Every display refresh (preview and program surfaces).
    Display,
}

impl UpdateRate {
    /// The faster of two rates.
    pub fn max(self, other: UpdateRate) -> UpdateRate {
        Ord::max(self, other)
    }
}

/// Static description of a built-in panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelInfo {
    /// Base id, without parameters.
    pub id: &'static str,
    /// English title from the glossary. The shell translates it.
    pub title: &'static str,
    /// Redraw need while visible.
    pub rate: UpdateRate,
    /// Closing it while live shows an undo toast (UI rule U21, "Safe while live").
    pub guarded_while_live: bool,
    /// Can be opened once per canvas with a `canvas` parameter.
    pub per_canvas: bool,
}

/// Built-in panels, in Panels menu order.
pub const BUILT_IN_PANELS: &[PanelInfo] = &[
    info("rbs.scenes", "Scenes", UpdateRate::OnChange, false, false),
    info("rbs.sources", "Sources", UpdateRate::OnChange, false, false),
    info("rbs.preview", "Preview", UpdateRate::Display, false, true),
    info("rbs.program", "Program", UpdateRate::Display, true, true),
    info(
        "rbs.properties",
        "Properties",
        UpdateRate::OnChange,
        false,
        false,
    ),
    info("rbs.mixer", "Mixer", UpdateRate::Hz(30), true, false),
    info(
        "rbs.live-controls",
        "Live controls",
        UpdateRate::OnChange,
        true,
        false,
    ),
    info(
        "rbs.transition",
        "Transition",
        UpdateRate::OnChange,
        false,
        false,
    ),
    info("rbs.outputs", "Outputs", UpdateRate::OnChange, false, false),
    info("rbs.stats", "Stats", UpdateRate::Hz(1), false, false),
    info(
        "rbs.status-bar",
        "Status bar",
        UpdateRate::Hz(1),
        false,
        false,
    ),
];

const fn info(
    id: &'static str,
    title: &'static str,
    rate: UpdateRate,
    guarded_while_live: bool,
    per_canvas: bool,
) -> PanelInfo {
    PanelInfo {
        id,
        title,
        rate,
        guarded_while_live,
        per_canvas,
    }
}

/// Look up a built-in panel by id (parameters are ignored). `None` for plugin
/// panels and unknown ids.
pub fn built_in(id: &PanelId) -> Option<&'static PanelInfo> {
    BUILT_IN_PANELS.iter().find(|p| p.id == id.base())
}

/// A panel id from a literal known to be valid (built-in ids; a test checks them).
pub(crate) fn id(base: &'static str) -> PanelId {
    PanelId(base.to_owned())
}

/// The fastest update rate needed by any of `visible`. Unknown panels count as
/// [`UpdateRate::OnChange`] (they show a static placeholder).
pub fn needed_rate<'a>(visible: impl IntoIterator<Item = &'a PanelId>) -> UpdateRate {
    visible
        .into_iter()
        .filter_map(built_in)
        .fold(UpdateRate::OnChange, |r, p| r.max(p.rate))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn built_in_ids_are_valid_and_unique() {
        for (i, p) in BUILT_IN_PANELS.iter().enumerate() {
            assert!(PanelId::parse(p.id).is_ok(), "{}", p.id);
            assert!(BUILT_IN_PANELS[..i].iter().all(|q| q.id != p.id));
        }
    }

    #[test]
    fn parses_parameters() {
        let p = PanelId::parse("rbs.program?canvas=vertical").unwrap();
        assert_eq!(p.base(), "rbs.program");
        assert_eq!(p.param("canvas"), Some("vertical"));
        assert_eq!(p.param("other"), None);
        assert!(built_in(&p).is_some_and(|i| i.per_canvas));
        let plugin = PanelId::parse("com.example.chat.dock").unwrap();
        assert!(built_in(&plugin).is_none());
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in [
            "",
            "mixer",
            ".rbs",
            "rbs.",
            "RBS.mixer",
            "rbs mixer",
            "rbs.program?canvas",
            "rbs.program?canvas=a&canvas=b",
            "rbs.program?=x",
            "rbs.program?Canvas=x",
        ] {
            assert!(PanelId::parse(bad).is_err(), "{bad}");
        }
        assert!(PanelId::parse(&format!("rbs.{}", "a".repeat(200))).is_err());
    }

    #[test]
    fn rate_is_the_fastest_visible() {
        let ids = [id("rbs.scenes"), id("rbs.stats")];
        assert_eq!(needed_rate(&ids), UpdateRate::Hz(1));
        let ids = [id("rbs.mixer"), id("rbs.program"), id("rbs.stats")];
        assert_eq!(needed_rate(&ids), UpdateRate::Display);
        assert_eq!(needed_rate(&[]), UpdateRate::OnChange);
        let unknown = PanelId::parse("com.example.chat").unwrap();
        assert_eq!(needed_rate([&unknown]), UpdateRate::OnChange);
        assert!(UpdateRate::Hz(30) > UpdateRate::Hz(1));
    }
}
