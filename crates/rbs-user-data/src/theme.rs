//! User themes (`themes/<id>.toml`, `kind = "theme"`) and the appearance choice
//! (`appearance.toml`, `kind = "appearance"`). Spec: `docs/specs/themes.md`; decision:
//! ADR-0057.
//!
//! The theme body and its checks belong to `rbs_ui::tokens` ([`ThemeBody`],
//! `Tokens::resolve`); this module makes it a document and keeps the appearance choice.

use std::fmt;
use std::path::Path;

use rbs_ui::tokens::{Base, MAX_THEME_FILE, ThemeBody};
use serde::{Deserialize, Serialize};

use crate::document::{self, Document};
use crate::library::EntryId;

impl Document for ThemeBody {
    const KIND: &'static str = "theme";
    const FORMAT: u32 = 1;
    const MAX_SIZE: u64 = MAX_THEME_FILE;

    fn from_body(body: toml::Table) -> Result<Self, String> {
        ThemeBody::from_body(body)
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        Ok(ThemeBody::to_body(self))
    }
}

/// Prefix of the ids of built-in themes (`builtin:dark`); `:` is never in a library id.
pub const BUILT_IN_PREFIX: &str = "builtin:";

/// A theme: one of the built-in ones or a user theme in the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeId {
    /// `builtin:<base>`.
    BuiltIn(Base),
    /// A library entry.
    User(EntryId),
}

impl ThemeId {
    /// Parses `builtin:<base>` or a library id.
    pub fn parse(s: &str) -> Option<ThemeId> {
        match s.strip_prefix(BUILT_IN_PREFIX) {
            Some(b) => Base::from_name(b).map(ThemeId::BuiltIn),
            None => EntryId::parse(s).map(ThemeId::User),
        }
    }
}

impl fmt::Display for ThemeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeId::BuiltIn(b) => write!(f, "{BUILT_IN_PREFIX}{}", b.name()),
            ThemeId::User(id) => f.write_str(id.as_str()),
        }
    }
}

/// How the theme in use is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppearanceMode {
    /// A theme for the OS's dark scheme and one for its light scheme.
    #[default]
    System,
    /// One theme whatever the OS scheme.
    Fixed,
}

/// The appearance choice: which themes are used. Per user, not per output profile.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Appearance {
    /// System or fixed.
    pub mode: AppearanceMode,
    /// Used while the OS scheme is dark (`System`).
    pub dark: ThemeId,
    /// Used while the OS scheme is light (`System`).
    pub light: ThemeId,
    /// Used in `Fixed` mode.
    pub theme: ThemeId,
    /// Compact uses the next smaller spacing and control size (format 2).
    pub density: rbs_ui::tokens::Density,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            mode: AppearanceMode::System,
            dark: ThemeId::BuiltIn(Base::Dark),
            light: ThemeId::BuiltIn(Base::Light),
            theme: ThemeId::BuiltIn(Base::Dark),
            density: rbs_ui::tokens::Density::Regular,
        }
    }
}

impl Appearance {
    /// The given choice.
    pub fn new(mode: AppearanceMode, dark: ThemeId, light: ThemeId, theme: ThemeId) -> Self {
        Appearance {
            mode,
            dark,
            light,
            theme,
            density: rbs_ui::tokens::Density::Regular,
        }
    }

    /// The themes for the two slots (OS dark, OS light): in `Fixed` mode both are
    /// `theme`.
    pub fn slots(&self) -> [&ThemeId; 2] {
        match self.mode {
            AppearanceMode::System => [&self.dark, &self.light],
            AppearanceMode::Fixed => [&self.theme, &self.theme],
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AppearanceFile {
    #[serde(default = "system")]
    mode: String,
    #[serde(default = "dark")]
    dark: String,
    #[serde(default = "light")]
    light: String,
    #[serde(default = "dark")]
    theme: String,
    #[serde(default = "regular")]
    density: String,
}

fn regular() -> String {
    "regular".into()
}

fn system() -> String {
    "system".into()
}
fn dark() -> String {
    format!("{BUILT_IN_PREFIX}dark")
}
fn light() -> String {
    format!("{BUILT_IN_PREFIX}light")
}

impl Document for Appearance {
    const KIND: &'static str = "appearance";
    const FORMAT: u32 = 2;

    fn upgrade(from: u32, body: &mut toml::Table) -> Result<(), String> {
        match from {
            1 => {
                body.entry("density")
                    .or_insert(toml::Value::String("regular".into()));
                Ok(())
            }
            other => Err(format!("format {other} cannot be read")),
        }
    }

    fn from_body(body: toml::Table) -> Result<Self, String> {
        let f: AppearanceFile = document::from_body(body)?;
        let mode = match f.mode.as_str() {
            "system" => AppearanceMode::System,
            "fixed" => AppearanceMode::Fixed,
            m => return Err(format!("mode {m:?} is not system or fixed")),
        };
        let id = |s: &str, what: &str| {
            ThemeId::parse(s).ok_or_else(|| format!("{what} {s:?} is not a theme id"))
        };
        let density = match f.density.as_str() {
            "regular" => rbs_ui::tokens::Density::Regular,
            "compact" => rbs_ui::tokens::Density::Compact,
            d => return Err(format!("density {d:?} is not regular or compact")),
        };
        Ok(Appearance {
            mode,
            dark: id(&f.dark, "dark")?,
            light: id(&f.light, "light")?,
            theme: id(&f.theme, "theme")?,
            density,
        })
    }

    fn to_body(&self) -> Result<toml::Table, String> {
        document::body_of(&AppearanceFile {
            mode: match self.mode {
                AppearanceMode::System => "system",
                AppearanceMode::Fixed => "fixed",
            }
            .into(),
            dark: self.dark.to_string(),
            light: self.light.to_string(),
            theme: self.theme.to_string(),
            density: match self.density {
                rbs_ui::tokens::Density::Regular => "regular",
                rbs_ui::tokens::Density::Compact => "compact",
            }
            .into(),
        })
    }
}

/// The `base` of a theme file whose body may not load, read from the file's text alone
/// (for the fallback of ADR-0057 decision 5); `None` if it cannot be found.
pub fn read_base(path: &Path) -> Option<Base> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.len() as u64 > MAX_THEME_FILE {
        return None;
    }
    if let Ok(table) = toml::from_str::<toml::Table>(&text) {
        return table.get("base")?.as_str().and_then(Base::from_name);
    }
    // Text this build cannot parse: the first top-level `base = "..."` line.
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            return None;
        }
        if let Some(rest) = line.strip_prefix("base")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return Base::from_name(value.trim().trim_matches('"'));
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::document::{self, Document};

    #[test]
    fn a_format_1_appearance_file_is_regular_spacing() {
        let text = "\
format = 1
kind = \"appearance\"
mode = \"fixed\"
dark = \"builtin:dark\"
light = \"builtin:light\"
theme = \"builtin:light\"
";
        let read = document::parse::<Appearance>(text).unwrap();
        assert_eq!(read.format, 1);
        assert_eq!(read.document.density, rbs_ui::tokens::Density::Regular);
        assert_eq!(Appearance::FORMAT, 2);
    }
}
