//! Output profiles (glossary: Output profile): a saved set of output settings. The
//! profile is a set of named sections, each a TOML table owned by the code that uses it
//! (for example the app's `recording` and `stream` settings, each with its own format
//! version). Sections this build does not know are kept as they are, so a profile
//! written by a newer app or a plugin loses nothing when saved here.
//!
//! Secrets (stream keys, tokens) are never stored in a profile: they stay in the
//! operating system's credential store (ADR-0051), so a profile can be exported and
//! shared.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::document::{DataError, Document, FORMAT_KEY, KIND_KEY, NAME_KEY};

/// The key of a section's own format version. Each section owner bumps it whenever an
/// older build could not read the section (for example a new field, when the type
/// refuses unknown fields).
pub const SECTION_VERSION: &str = "format_version";

/// An output profile's sections.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputProfile {
    sections: toml::Table,
}

impl OutputProfile {
    /// A profile without sections: every setting at its default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Names of its sections, sorted.
    pub fn sections(&self) -> impl Iterator<Item = &str> {
        self.sections.keys().map(String::as_str)
    }

    /// A section, `Ok(None)` if absent; an error if it does not match `S`.
    pub fn get<S: DeserializeOwned>(&self, section: &str) -> Result<Option<S>, DataError> {
        let Some(value) = self.sections.get(section) else {
            return Ok(None);
        };
        value
            .clone()
            .try_into()
            .map(Some)
            .map_err(|e: toml::de::Error| {
                DataError::Invalid(format!("[{section}]: {}", e.message()))
            })
    }

    /// Sets a section. Its value must serialise to a table, and the names `format`,
    /// `kind` and `name` are reserved.
    ///
    /// A section written by a newer build (a higher integer `format_version` than the
    /// new value's) is refused with [`DataError::Newer`] and kept, so an older app
    /// never replaces settings it does not understand; [`OutputProfile::remove`] first
    /// when the user asks to reset them.
    pub fn set<S: Serialize>(&mut self, section: &str, value: &S) -> Result<(), DataError> {
        check_section(section)?;
        match toml::Value::try_from(value).map_err(|e| DataError::Invalid(e.to_string()))? {
            v @ toml::Value::Table(_) => {
                let version = |v: &toml::Value| match v.get(SECTION_VERSION) {
                    Some(toml::Value::Integer(n)) => u32::try_from(*n).ok(),
                    _ => None,
                };
                if let (Some(found), Some(new)) =
                    (self.sections.get(section).and_then(version), version(&v))
                    && found > new
                {
                    return Err(DataError::Newer {
                        found,
                        supported: new,
                    });
                }
                self.sections.insert(section.to_owned(), v);
                Ok(())
            }
            _ => Err(DataError::Invalid(format!(
                "section {section:?} must be a table"
            ))),
        }
    }

    /// Removes a section (its settings go back to their defaults).
    pub fn remove(&mut self, section: &str) {
        self.sections.remove(section);
    }
}

fn check_section(section: &str) -> Result<(), DataError> {
    let ok = !section.is_empty()
        && ![FORMAT_KEY, KIND_KEY, NAME_KEY].contains(&section)
        && section.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_' || b == b'.'
        });
    if ok {
        Ok(())
    } else {
        Err(DataError::Invalid(format!(
            "{section:?} is not a section name"
        )))
    }
}

impl Document for OutputProfile {
    const KIND: &'static str = "output-profile";
    const FORMAT: u32 = 1;

    fn from_body(body: toml::Table) -> Result<Self, String> {
        for (k, v) in &body {
            check_section(k).map_err(|e| e.to_string())?;
            if !v.is_table() {
                return Err(format!("{k:?} must be a section ([{k}])"));
            }
        }
        Ok(Self { sections: body })
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        Ok(self.sections.clone())
    }
}
