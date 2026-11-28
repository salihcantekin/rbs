//! The guest source's saved settings (spec `guest-source.md`, "Registration"): an
//! `rbs-params` schema plus the guest's name as text. A pairing is never saved.

use std::time::Duration;

use rbs_params::{Field, Schema, Values};
use rbs_scene::model::{SettingValue, Settings};

/// Settings key: the guest's name, shown in the studio (text, not in the schema: the
/// schema has no text field).
pub const NAME: &str = "name";

/// The longest name kept.
pub const MAX_NAME_CHARS: usize = 64;

/// The schema, in display order.
pub const FIELDS: &[Field] = &[
    Field::number(
        "jitter_ms",
        "Jitter buffer",
        40.0,
        1000.0,
        150.0,
        10.0,
        "ms",
    )
    .with_help("How long pictures and sound wait to smooth out the network."),
    Field::toggle("check_phrase", "Check phrase", true)
        .with_help("Both sides compare a phrase before the guest is shown."),
    Field::toggle("local_only", "Local network only", false)
        .with_help("Connect only over this network, never through the internet."),
];

mod at {
    pub const JITTER_MS: usize = 0;
    pub const CHECK_PHRASE: usize = 1;
    pub const LOCAL_ONLY: usize = 2;
}

/// Every key the type uses.
pub const KEYS: &[&str] = &["jitter_ms", "check_phrase", "local_only", NAME];

/// The option keys (ADR-0133 decision 2): the schema keys. The name is identity.
pub const OPTION_KEYS: &[&str] = &["jitter_ms", "check_phrase", "local_only"];

/// The guest source's settings, read leniently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSettings {
    /// The guest's name, if set.
    pub name: Option<String>,
    /// The jitter target.
    pub jitter: Duration,
    /// Compare the check phrase before the guest is shown.
    pub check_phrase: bool,
    /// Host candidates only (no STUN).
    pub local_only: bool,
}

fn schema_part(settings: &Settings) -> Settings {
    settings
        .iter()
        .filter(|(k, _)| FIELDS.iter().any(|f| f.key == k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Checks the settings of an edit: only [`KEYS`], schema values in range, the name as
/// text of at most [`MAX_NAME_CHARS`]. `(key, expected)` on a refusal.
pub fn validate(settings: &Settings) -> Result<(), (&'static str, &'static str)> {
    if settings.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return Err(("settings", "only keys this source type uses"));
    }
    if crate::filters::filter_values(&FIELDS, &schema_part(settings)).is_err() {
        return Err(("settings", "values within the guest source's ranges"));
    }
    match settings.get(NAME) {
        None => Ok(()),
        Some(SettingValue::Text(t)) if t.chars().count() <= MAX_NAME_CHARS => Ok(()),
        Some(_) => Err((NAME, "text of at most 64 characters")),
    }
}

/// The schema values, read leniently (the Source tab's rows).
pub fn values(settings: &Settings) -> Values {
    crate::filters::filter_values_lenient(&FIELDS, &schema_part(settings)).0
}

/// Reads the settings leniently (a value out of range is clamped).
pub fn read_settings(settings: &Settings) -> GuestSettings {
    let v = values(settings);
    let name = match settings.get(NAME) {
        Some(SettingValue::Text(t)) if !t.trim().is_empty() => {
            Some(t.chars().take(MAX_NAME_CHARS).collect())
        }
        _ => None,
    };
    GuestSettings {
        name,
        jitter: Duration::from_millis(v.number(at::JITTER_MS) as u64),
        check_phrase: v.toggle(at::CHECK_PHRASE),
        local_only: v.toggle(at::LOCAL_ONLY),
    }
}

/// The schema as a trait object, for hosts that draw it.
pub fn schema() -> &'static dyn Schema {
    &FIELDS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(pairs: &[(&str, SettingValue)]) -> Settings {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn defaults_match_the_spec() {
        let s = read_settings(&Settings::default());
        assert_eq!(s.jitter, Duration::from_millis(150));
        assert!(s.check_phrase);
        assert!(!s.local_only);
        assert_eq!(s.name, None);
    }

    #[test]
    fn edits_out_of_range_or_unknown_are_refused_and_documents_are_clamped() {
        let low = with(&[("jitter_ms", SettingValue::Int(10))]);
        assert!(validate(&low).is_err());
        assert_eq!(read_settings(&low).jitter, Duration::from_millis(40));
        assert!(validate(&with(&[("url", SettingValue::Text("x".into()))])).is_err());
        let long = "x".repeat(MAX_NAME_CHARS + 1);
        assert!(validate(&with(&[(NAME, SettingValue::Text(long))])).is_err());
        let ok = with(&[
            ("jitter_ms", SettingValue::Int(300)),
            ("local_only", SettingValue::Bool(true)),
            (NAME, SettingValue::Text("Ayşe".into())),
        ]);
        assert_eq!(validate(&ok), Ok(()));
        let s = read_settings(&ok);
        assert_eq!(s.jitter, Duration::from_millis(300));
        assert!(s.local_only);
        assert_eq!(s.name.as_deref(), Some("Ayşe"));
    }
}
