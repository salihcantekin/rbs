//! User themes: overrides on a built-in theme, resolving and the theme rules.
//!
//! Spec: `docs/specs/themes.md`; decision: ADR-0057. A theme names a `base` (one of
//! the built-in themes) and sets only the tokens it changes. [`Tokens::resolve`]
//! applies the overrides to the base and runs every check of the built-in file plus
//! the theme rules (bounds, rules across tokens, distance from the live colour), and
//! reports every problem with the token it belongs to, so an editor can show it
//! beside that token.

use std::collections::BTreeMap;
use std::fmt;

use super::{
    CONTRAST_RULES, Color, Density, EFFECTS_KEY, EffectDraw, EffectValues, Effects, FIXED,
    LIVE_SURFACES, MIN_DELTA_E_FROM_LIVE, MIN_HUE_DISTANCE_FROM_LIVE, THEME_COLORS,
    THEME_CONTRAST_RULES, THEMES, TokenError, Tokens, TypeStyle, apply_density, checked,
    hue_distance, valid_name,
};

/// The font family token: a list of family names, the first installed one is used.
pub const FONT_FAMILY: &str = "font.family";

/// Largest theme file read (shared themes are untrusted input).
pub const MAX_THEME_FILE: u64 = 64 * 1024;

/// Most token names a theme may hold that this build does not know.
pub const MAX_UNKNOWN_TOKENS: usize = 256;

/// Longest `author` and `description`, in characters.
pub const MAX_AUTHOR: usize = 64;
/// See [`MAX_AUTHOR`].
pub const MAX_DESCRIPTION: usize = 200;

/// Most font family names in `font.family`, and the longest name in characters.
pub const MAX_FONT_FAMILIES: usize = 8;
/// See [`MAX_FONT_FAMILIES`].
pub const MAX_FONT_NAME: usize = 64;

/// The built-in theme a user theme starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Base {
    /// `dark`.
    #[default]
    Dark,
    /// `light`.
    Light,
}

impl Base {
    /// Every base, in [`THEMES`] order.
    pub const ALL: [Base; 2] = [Base::Dark, Base::Light];

    /// The name used in files and in [`THEMES`].
    pub fn name(self) -> &'static str {
        THEMES[self.index()]
    }

    /// The position in [`THEMES`] and [`Tokens::themes`].
    pub fn index(self) -> usize {
        match self {
            Base::Dark => 0,
            Base::Light => 1,
        }
    }

    /// The base named `name`. A retired base reads as the base that replaced it
    /// ([`RETIRED_BASES`]), so an old theme file or setting still opens.
    pub fn from_name(name: &str) -> Option<Base> {
        Base::ALL
            .into_iter()
            .find(|b| b.name() == name)
            .or_else(|| {
                RETIRED_BASES
                    .iter()
                    .find(|(old, _)| *old == name)
                    .map(|(_, b)| *b)
            })
    }
}

/// Bases that RBS no longer ships, and the base each one now reads as. `high-contrast`
/// was removed in ADR-0123: a theme or setting that names it shows on dark, and is
/// written back as `dark` the next time it is saved.
pub const RETIRED_BASES: &[(&str, Base)] = &[("high-contrast", Base::Dark)];

/// Where a token appears in the theme editor and in which table of a theme file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// `[colors]`.
    Colours,
    /// `[type]`.
    Text,
    /// `[font]`.
    Font,
    /// `[length]`: spacing, control heights, icons, menus.
    SpacingAndSize,
    /// `[length]`: radii and lines.
    Shapes,
    /// `[duration]`.
    Motion,
}

/// What a themeable token holds and its bounds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// An opaque `#rrggbb` colour.
    Color,
    /// Logical pixels within `min..=max`.
    Length {
        /// Smallest value.
        min: f32,
        /// Largest value.
        max: f32,
    },
    /// Milliseconds within `min..=max`.
    Duration {
        /// Smallest value.
        min: u32,
        /// Largest value.
        max: u32,
    },
    /// A whole text style.
    TypeStyle,
    /// A list of font family names.
    FontFamily,
}

/// A token a theme may set, with what the editor shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Themeable {
    /// Token name.
    pub name: &'static str,
    /// Name in words (English until Fluent strings exist).
    pub label: &'static str,
    /// Editor group and file table.
    pub group: Group,
    /// Value kind and bounds.
    pub kind: Kind,
}

const fn colour(name: &'static str, label: &'static str) -> Themeable {
    Themeable {
        name,
        label,
        group: Group::Colours,
        kind: Kind::Color,
    }
}

const fn length(
    name: &'static str,
    label: &'static str,
    group: Group,
    min: f32,
    max: f32,
) -> Themeable {
    Themeable {
        name,
        label,
        group,
        kind: Kind::Length { min, max },
    }
}

const fn text(name: &'static str, label: &'static str) -> Themeable {
    Themeable {
        name,
        label,
        group: Group::Text,
        kind: Kind::TypeStyle,
    }
}

const fn motion(name: &'static str, label: &'static str) -> Themeable {
    Themeable {
        name,
        label,
        group: Group::Motion,
        kind: Kind::Duration { min: 120, max: 200 },
    }
}

use Group::{Shapes, SpacingAndSize};

/// Every token a theme may set, in editor order. Not here, refused in a theme: the
/// fixed live colours (rule U12), window sizes and keyboard steps.
pub const THEMEABLE: &[Themeable] = &[
    colour("color.surface.base", "Window background"),
    colour("color.surface.raised", "Panel background"),
    colour("color.surface.overlay", "Menu and popover background"),
    colour("color.surface.sunken", "Well background"),
    colour(
        "color.surface.checker",
        "Pattern behind transparent pictures",
    ),
    colour("color.scrollbar.thumb", "Scrollbar"),
    colour("color.scrollbar.thumb-hot", "Scrollbar under the pointer"),
    colour("color.toast.surface", "Notice background"),
    colour("color.toast.edge", "Notice outline"),
    colour("color.text.primary", "Text"),
    colour("color.text.secondary", "Secondary text"),
    colour("color.text.disabled", "Disabled text"),
    colour("color.text.inverse", "Text on accent"),
    colour("color.border.subtle", "Divider"),
    colour("color.border.strong", "Input border"),
    colour("color.border.focus", "Focus ring"),
    colour("color.accent", "Accent"),
    colour("color.accent.hover", "Accent under the pointer"),
    colour("color.accent.subtle", "Selection background"),
    colour("color.accent.subtle.hover", "Selection under the pointer"),
    colour("color.accent.pressed", "Accent while pressed"),
    colour("color.button.fill", "Button"),
    colour("color.button.hover", "Button under the pointer"),
    colour("color.button.pressed", "Button while pressed"),
    colour("color.button.chip", "Button icon chip"),
    colour("color.brand", "Logo accent"),
    colour("color.state.warning", "Warning"),
    colour("color.state.healthy", "Healthy"),
    colour("color.state.error", "Error"),
    colour("color.action.destructive.subtle", "Danger button"),
    colour(
        "color.action.destructive.subtle.hover",
        "Danger button under the pointer",
    ),
    colour("color.action.destructive", "Destructive action"),
    colour("color.meter.low", "Meter, low level"),
    colour("color.meter.mid", "Meter, middle level"),
    colour("color.meter.high", "Meter, high level"),
    colour("color.meter.clip", "Meter, clipped"),
    colour("color.loudness.fill", "Loudness bar"),
    colour("color.loudness.target", "Loudness target mark"),
    text("type.caption", "Caption"),
    text("type.body", "Body"),
    text("type.label", "Label"),
    text("type.title", "Title"),
    text("type.heading", "Heading"),
    Themeable {
        name: FONT_FAMILY,
        label: "Font families",
        group: Group::Font,
        kind: Kind::FontFamily,
    },
    length("space.1", "Spacing 1", SpacingAndSize, 0.0, 64.0),
    length("space.2", "Spacing 2", SpacingAndSize, 0.0, 64.0),
    length("space.3", "Spacing 3", SpacingAndSize, 0.0, 64.0),
    length("space.4", "Spacing 4", SpacingAndSize, 0.0, 64.0),
    length("space.6", "Spacing 6", SpacingAndSize, 0.0, 64.0),
    length("space.8", "Spacing 8", SpacingAndSize, 0.0, 64.0),
    length("size.panel.pad", "Panel padding", SpacingAndSize, 8.0, 64.0),
    length(
        "size.section.gap",
        "Gap between sections",
        SpacingAndSize,
        12.0,
        64.0,
    ),
    length(
        "size.field.gap",
        "Gap between fields",
        SpacingAndSize,
        6.0,
        64.0,
    ),
    length(
        "size.control.sm",
        "Small control height",
        SpacingAndSize,
        24.0,
        64.0,
    ),
    length(
        "size.control.md",
        "Control height",
        SpacingAndSize,
        24.0,
        64.0,
    ),
    length(
        "size.control.lg",
        "Large control height",
        SpacingAndSize,
        24.0,
        64.0,
    ),
    length("size.icon.box", "Icon size", SpacingAndSize, 12.0, 32.0),
    // Disjoint bounds keep thumb <= thumb-hot <= hit in every theme.
    length(
        "size.scrollbar.thumb",
        "Scrollbar width",
        SpacingAndSize,
        2.0,
        6.0,
    ),
    length(
        "size.scrollbar.thumb-hot",
        "Scrollbar width under the pointer",
        SpacingAndSize,
        6.0,
        10.0,
    ),
    length(
        "size.scrollbar.hit",
        "Scrollbar room",
        SpacingAndSize,
        10.0,
        16.0,
    ),
    length(
        "size.scrollbar.thumb-min",
        "Shortest scrollbar",
        SpacingAndSize,
        16.0,
        48.0,
    ),
    length(
        "size.list.cell-min-w",
        "Narrowest list cell",
        SpacingAndSize,
        80.0,
        240.0,
    ),
    length(
        "size.icon.stroke",
        "Icon line width",
        SpacingAndSize,
        1.0,
        3.0,
    ),
    length(
        "size.menu.sm",
        "Narrow menu width",
        SpacingAndSize,
        160.0,
        480.0,
    ),
    length("size.menu.md", "Menu width", SpacingAndSize, 160.0, 480.0),
    length(
        "size.select.list-h",
        "Drop-down list height",
        SpacingAndSize,
        160.0,
        640.0,
    ),
    length(
        "size.app-menu.w",
        "RBS menu width",
        SpacingAndSize,
        480.0,
        960.0,
    ),
    length(
        "size.app-menu.categories",
        "RBS menu category column width",
        SpacingAndSize,
        120.0,
        320.0,
    ),
    length(
        "size.app-menu.list-h",
        "RBS menu list height",
        SpacingAndSize,
        240.0,
        720.0,
    ),
    length("radius.sm", "Small corner radius", Shapes, 0.0, 32.0),
    length("radius.md", "Corner radius", Shapes, 0.0, 32.0),
    length("radius.lg", "Large corner radius", Shapes, 0.0, 32.0),
    length("radius.button", "Button corner radius", Shapes, 0.0, 32.0),
    length("size.button.chip", "Button icon chip", Shapes, 12.0, 32.0),
    length("size.border", "Border width", Shapes, 1.0, 4.0),
    length("size.focus-ring", "Focus ring width", Shapes, 2.0, 6.0),
    length(
        "size.toast.border",
        "Notice outline width",
        Shapes,
        1.0,
        4.0,
    ),
    motion("motion.fast", "Short animation"),
    motion("motion.normal", "Animation"),
];

/// The themeable token named `name`.
pub fn themeable(name: &str) -> Option<&'static Themeable> {
    THEMEABLE.iter().find(|t| t.name == name)
}

/// Text sizes, line heights and weights a theme may use.
pub const TYPE_SIZE: (u16, u16) = (11, 48);
/// See [`TYPE_SIZE`].
pub const TYPE_WEIGHT: (u16, u16) = (300, 900);

/// How much a problem matters (ADR-0072).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The theme is refused: not used, not saved.
    Error,
    /// A colour rule a user theme breaks: the theme is used and saved, with this shown.
    /// Built-in themes treat it as an error.
    Warning,
    /// Not a colour rule and never refusing: the theme's effects show as calm, and why
    /// (ADR-0120 decision 9). Its token is [`EFFECTS_KEY`].
    Notice,
}

/// The colour rule a warning comes from, so a UI can say it in its own words.
#[derive(Clone, Debug, PartialEq)]
pub enum ColorRule {
    /// `fg` drawn on `bg` has `ratio`, needs `need`. `fg` may be the fixed
    /// `color.state.live` (on a live surface).
    Contrast {
        /// The foreground token.
        fg: String,
        /// The background token.
        bg: String,
        /// The contrast found.
        ratio: f32,
        /// The contrast needed.
        need: f32,
    },
    /// The token's hue is too close to the live colour's (error, destructive, clip).
    LiveHue {
        /// The token.
        token: String,
    },
    /// The token looks like the live colour.
    LiveLook {
        /// The token.
        token: String,
        /// ΔE2000 found.
        difference: f32,
        /// ΔE2000 needed.
        need: f32,
    },
}

/// One thing wrong with a theme.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeProblem {
    /// The token it belongs to, if any (the editor shows it beside that token).
    pub token: Option<String>,
    /// What is wrong, in words (token names, for logs and files).
    pub reason: String,
    /// Whether it refuses the theme.
    pub severity: Severity,
    /// The colour rule, for a warning.
    pub rule: Option<ColorRule>,
}

impl ThemeProblem {
    fn at(token: &str, reason: String) -> ThemeProblem {
        ThemeProblem {
            token: Some(token.to_owned()),
            reason,
            severity: Severity::Error,
            rule: None,
        }
    }

    fn warn(token: &str, reason: String, rule: ColorRule) -> ThemeProblem {
        ThemeProblem {
            token: Some(token.to_owned()),
            reason,
            severity: Severity::Warning,
            rule: Some(rule),
        }
    }

    fn notice(reason: String) -> ThemeProblem {
        ThemeProblem {
            token: Some(EFFECTS_KEY.to_owned()),
            reason,
            severity: Severity::Notice,
            rule: None,
        }
    }

    /// True when it refuses the theme.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    /// True for a colour warning (ADR-0072), not an error or an effects notice.
    pub fn is_warning(&self) -> bool {
        self.severity == Severity::Warning
    }
}

impl fmt::Display for ThemeProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Every problem of a theme that is not used (at least one error); warnings too.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeProblems(pub Vec<ThemeProblem>);

impl fmt::Display for ThemeProblems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, p) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ThemeProblems {}

/// The body of a theme document (`kind = "theme"`): everything but the envelope.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ThemeBody {
    /// The built-in theme it starts from.
    pub base: Base,
    /// Display text for sharing.
    pub author: Option<String>,
    /// Display text for sharing.
    pub description: Option<String>,
    /// `[colors]` overrides.
    pub colors: BTreeMap<String, Color>,
    /// `[font]` `font.family`, when set.
    pub font_family: Option<Vec<String>>,
    /// `[type]` overrides.
    pub type_styles: BTreeMap<String, TypeStyle>,
    /// `[length]` overrides, logical pixels.
    pub length: BTreeMap<String, f32>,
    /// `[duration]` overrides, milliseconds.
    pub duration: BTreeMap<String, u32>,
    /// The `effects` key as stored (ADR-0120 decision 1): `None` is calm. A value this
    /// build does not know is kept as it is and reads as calm.
    pub effects: Option<String>,
    /// Known tokens whose value is of the wrong kind, with the reason; reported by
    /// [`Tokens::resolve`] and kept on save.
    pub invalid: BTreeMap<String, (String, toml::Value, String)>,
    /// Valid token names this build does not know, per table; kept on save, not applied.
    pub unknown: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

const TABLES: [&str; 5] = ["colors", "font", "type", "length", "duration"];

fn table_of(t: &Themeable) -> &'static str {
    match t.group {
        Group::Colours => "colors",
        Group::Text => "type",
        Group::Font => "font",
        Group::SpacingAndSize | Group::Shapes => "length",
        Group::Motion => "duration",
    }
}

/// The table a known but not themeable token would be in.
fn known_table(name: &str) -> Option<&'static str> {
    if let Some(t) = themeable(name) {
        return Some(table_of(t));
    }
    if FIXED.iter().any(|(n, _)| *n == name) {
        return Some("colors");
    }
    if super::LENGTHS.contains(&name) {
        return Some("length");
    }
    None
}

/// Why display text is refused ([`plain_line`]); `what` names the text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineError {
    /// More than `max` characters.
    TooLong {
        /// The text, as a phrase ("author").
        what: &'static str,
        /// The limit in characters.
        max: usize,
    },
    /// A control character or a text direction mark.
    ControlCharacter {
        /// The text, as a phrase ("author").
        what: &'static str,
    },
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineError::TooLong { what, max } => {
                write!(f, "{what} is longer than {max} characters")
            }
            LineError::ControlCharacter { what } => {
                write!(f, "{what} has a control character or text direction mark")
            }
        }
    }
}

impl std::error::Error for LineError {}

/// Why [`ThemeBody::from_body`] refuses a body. The text names the key or token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThemeBodyError {
    /// No `base` key.
    BaseMissing,
    /// `base` is not a string.
    BaseNotString,
    /// `base` names no built-in theme.
    UnknownBase(String),
    /// `author`, `description` or `effects` is not a string.
    NotString(&'static str),
    /// `author` or `description` is not plain display text.
    Text(LineError),
    /// `effects` is not a valid name.
    InvalidEffects(String),
    /// A token table (`[colors]`, ...) is not a table.
    NotTable(&'static str),
    /// A top-level key the format does not have.
    UnknownKey(String),
    /// A token name that is not valid, in `table`.
    InvalidName {
        /// The name as written.
        name: String,
        /// The table it is in.
        table: &'static str,
    },
    /// A known token in the wrong table.
    WrongTable {
        /// The token.
        name: String,
        /// The table it belongs in.
        home: &'static str,
        /// The table it is in.
        table: &'static str,
    },
}

impl fmt::Display for ThemeBodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ThemeBodyError::BaseMissing => f.write_str("base is missing"),
            ThemeBodyError::BaseNotString => f.write_str("base must be a string"),
            ThemeBodyError::UnknownBase(b) => {
                write!(f, "base {b:?} is not one of {}", THEMES.join(", "))
            }
            ThemeBodyError::NotString(key) => write!(f, "{key} must be a string"),
            ThemeBodyError::Text(e) => e.fmt(f),
            ThemeBodyError::InvalidEffects(e) => write!(f, "effects {e:?} is not a valid name"),
            ThemeBodyError::NotTable(table) => write!(f, "[{table}] must be a table"),
            ThemeBodyError::UnknownKey(key) => write!(f, "unknown key {key}"),
            ThemeBodyError::InvalidName { name, table } => {
                write!(f, "invalid token name {name:?} in [{table}]")
            }
            ThemeBodyError::WrongTable { name, home, table } => {
                write!(f, "{name} belongs in [{home}], not [{table}]")
            }
        }
    }
}

impl std::error::Error for ThemeBodyError {}

/// Display text: one line, no control characters or text direction marks.
fn plain_line(s: &str, max: usize, what: &'static str) -> Result<(), LineError> {
    if s.chars().count() > max {
        return Err(LineError::TooLong { what, max });
    }
    if s.chars().any(|c| c.is_control() || is_direction_mark(c)) {
        return Err(LineError::ControlCharacter { what });
    }
    Ok(())
}

fn is_direction_mark(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

fn number(v: &toml::Value) -> Option<f64> {
    match v {
        toml::Value::Integer(i) => Some(*i as f64),
        toml::Value::Float(f) => Some(*f),
        _ => None,
    }
}

impl ThemeBody {
    /// A theme on `base` with no overrides.
    pub fn new(base: Base) -> ThemeBody {
        ThemeBody {
            base,
            ..ThemeBody::default()
        }
    }

    /// Reads a body. Refuses a missing or unknown base, unknown top-level keys, a table
    /// that is not a table, an invalid token name, a known token in the wrong table
    /// and invalid display text. A known token with a value of the wrong kind is kept
    /// in [`ThemeBody::invalid`] for [`Tokens::resolve`] to report.
    pub fn from_body(mut body: toml::Table) -> Result<ThemeBody, ThemeBodyError> {
        let base = match body.remove("base") {
            Some(toml::Value::String(b)) => {
                Base::from_name(&b).ok_or(ThemeBodyError::UnknownBase(b))?
            }
            Some(_) => return Err(ThemeBodyError::BaseNotString),
            None => return Err(ThemeBodyError::BaseMissing),
        };
        let mut theme = ThemeBody::new(base);
        for (key, max) in [("author", MAX_AUTHOR), ("description", MAX_DESCRIPTION)] {
            match body.remove(key) {
                Some(toml::Value::String(s)) => {
                    plain_line(&s, max, key).map_err(ThemeBodyError::Text)?;
                    let slot = if key == "author" {
                        &mut theme.author
                    } else {
                        &mut theme.description
                    };
                    *slot = Some(s);
                }
                Some(_) => return Err(ThemeBodyError::NotString(key)),
                None => {}
            }
        }
        match body.remove(EFFECTS_KEY) {
            Some(toml::Value::String(e)) => {
                if !valid_name(&e) {
                    return Err(ThemeBodyError::InvalidEffects(e));
                }
                // Calm is the same as no key, and is written as no key.
                if e != Effects::Calm.name() {
                    theme.effects = Some(e);
                }
            }
            Some(_) => return Err(ThemeBodyError::NotString(EFFECTS_KEY)),
            None => {}
        }
        for table in TABLES {
            let entries = match body.remove(table) {
                Some(toml::Value::Table(t)) => t,
                Some(_) => return Err(ThemeBodyError::NotTable(table)),
                None => continue,
            };
            for (name, value) in entries {
                theme.read_entry(table, name, value)?;
            }
        }
        if let Some(extra) = body.keys().next() {
            return Err(ThemeBodyError::UnknownKey(extra.clone()));
        }
        Ok(theme)
    }

    fn read_entry(
        &mut self,
        table: &'static str,
        name: String,
        value: toml::Value,
    ) -> Result<(), ThemeBodyError> {
        if !valid_name(&name) {
            return Err(ThemeBodyError::InvalidName { name, table });
        }
        let Some(home) = known_table(&name) else {
            self.unknown
                .entry(table.to_owned())
                .or_default()
                .insert(name, value);
            return Ok(());
        };
        if home != table {
            return Err(ThemeBodyError::WrongTable { name, home, table });
        }
        let wrong = |why: &str| (table.to_owned(), value.clone(), why.to_owned());
        let bad = match table {
            "colors" => match value.as_str().and_then(opaque) {
                Some(c) => {
                    self.colors.insert(name, c);
                    return Ok(());
                }
                None => wrong("is not a #rrggbb colour"),
            },
            "font" => match font_list(&value) {
                Some(list) => {
                    self.font_family = Some(list);
                    return Ok(());
                }
                None => wrong("is not a list of font family names"),
            },
            "type" => match value.clone().try_into::<TypeStyle>() {
                Ok(style) => {
                    self.type_styles.insert(name, style);
                    return Ok(());
                }
                Err(_) => wrong("is not { size, line, weight } with whole numbers"),
            },
            "length" => match number(&value) {
                Some(v) if v.is_finite() => {
                    self.length.insert(name, v as f32);
                    return Ok(());
                }
                _ => wrong("is not a number"),
            },
            _ => match value.as_integer().and_then(|v| u32::try_from(v).ok()) {
                Some(v) => {
                    self.duration.insert(name, v);
                    return Ok(());
                }
                None => wrong("is not a whole number of milliseconds"),
            },
        };
        self.invalid.insert(name, bad);
        Ok(())
    }

    /// The body as written: `base`, the display text, then each table with its
    /// overrides, invalid values and unknown names, sorted by name.
    pub fn to_body(&self) -> toml::Table {
        let mut body = toml::Table::new();
        body.insert("base".into(), self.base.name().into());
        if let Some(a) = &self.author {
            body.insert("author".into(), a.clone().into());
        }
        if let Some(d) = &self.description {
            body.insert("description".into(), d.clone().into());
        }
        if let Some(e) = self.stored_effects() {
            body.insert(EFFECTS_KEY.into(), e.into());
        }
        let mut tables: BTreeMap<&str, toml::Table> = BTreeMap::new();
        for (n, c) in &self.colors {
            tables
                .entry("colors")
                .or_default()
                .insert(n.clone(), c.to_string().into());
        }
        if let Some(f) = &self.font_family {
            let list = f.iter().map(|s| toml::Value::from(s.clone())).collect();
            tables
                .entry("font")
                .or_default()
                .insert(FONT_FAMILY.into(), toml::Value::Array(list));
        }
        for (n, t) in &self.type_styles {
            let mut style = toml::Table::new();
            style.insert("size".into(), i64::from(t.size).into());
            style.insert("line".into(), i64::from(t.line).into());
            style.insert("weight".into(), i64::from(t.weight).into());
            tables
                .entry("type")
                .or_default()
                .insert(n.clone(), toml::Value::Table(style));
        }
        for (n, v) in &self.length {
            let value = if v.fract() == 0.0 && v.abs() < 1e9 {
                toml::Value::Integer(*v as i64)
            } else {
                toml::Value::Float(f64::from(*v))
            };
            tables.entry("length").or_default().insert(n.clone(), value);
        }
        for (n, v) in &self.duration {
            tables
                .entry("duration")
                .or_default()
                .insert(n.clone(), i64::from(*v).into());
        }
        for (n, (table, value, _)) in &self.invalid {
            if let Some(t) = TABLES.iter().find(|t| **t == table.as_str()) {
                tables
                    .entry(t)
                    .or_default()
                    .insert(n.clone(), value.clone());
            }
        }
        for (table, entries) in &self.unknown {
            if let Some(t) = TABLES.iter().find(|t| **t == table.as_str()) {
                let target = tables.entry(t).or_default();
                for (n, v) in entries {
                    target.insert(n.clone(), v.clone());
                }
            }
        }
        for table in TABLES {
            if let Some(t) = tables.remove(table) {
                body.insert(table.into(), toml::Value::Table(t));
            }
        }
        body
    }

    /// The stored `effects` value that is not calm, if any: what is written, and what
    /// makes the document need format 2 (ADR-0120 decision 2).
    pub fn stored_effects(&self) -> Option<&str> {
        self.effects
            .as_deref()
            .filter(|e| *e != Effects::Calm.name())
    }

    /// The setting the editor shows: the stored one, or calm for none or an unknown one.
    pub fn effects_choice(&self) -> Effects {
        self.effects
            .as_deref()
            .and_then(Effects::from_name)
            .unwrap_or_default()
    }

    /// Stores `setting` (calm as no key).
    pub fn set_effects(&mut self, setting: Effects) {
        self.effects = (setting != Effects::Calm).then(|| setting.name().to_owned());
    }

    /// Names of tokens this build does not know, sorted.
    pub fn unknown_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .unknown
            .values()
            .flat_map(|t| t.keys().cloned())
            .collect();
        names.sort();
        names
    }
}

fn opaque(s: &str) -> Option<Color> {
    Color::parse(s).filter(|c| c.a == 255 && s.len() == 7)
}

fn font_list(v: &toml::Value) -> Option<Vec<String>> {
    v.as_array()?
        .iter()
        .map(|n| n.as_str().map(str::to_owned))
        .collect()
}

/// A theme ready to apply: its base with the overrides, with no error (it may have
/// colour warnings, ADR-0072).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedTheme {
    /// The base it was resolved on.
    pub base: Base,
    /// Every theme colour ([`THEME_COLORS`]).
    pub colors: BTreeMap<String, Color>,
    /// The fixed colours (the same in every theme).
    pub fixed: BTreeMap<String, Color>,
    /// Every length, themeable or not.
    pub length: BTreeMap<String, f32>,
    /// Every duration.
    pub duration: BTreeMap<String, u32>,
    /// Every text style.
    pub type_styles: BTreeMap<String, TypeStyle>,
    /// Font families in order of preference; empty: the OS UI font.
    pub font_family: Vec<String>,
    /// Token names in the theme that this build does not know (not applied).
    pub unknown: Vec<String>,
    /// Colour rules the theme breaks (ADR-0072), and the effects notice (ADR-0120,
    /// [`Severity::Notice`]); empty for a built-in theme.
    pub warnings: Vec<ThemeProblem>,
    /// The effects drawn (ADR-0120): calm unless the theme asks for more and may have it.
    pub effects: Effects,
    /// The values of [`ResolvedTheme::effects`]; all zero for calm.
    pub effect_values: EffectValues,
}

impl ResolvedTheme {
    /// What a slot draws for the theme's effects, on its lengths as they are now (after
    /// density, ADR-0120 decision 11): the faces and derived colours of the density in
    /// use (ADR-0122 decision 6). Calm when the effect checks fail at this density.
    pub fn effect_draw(&self) -> EffectDraw {
        let Some(live) = self.fixed.get("color.state.live") else {
            return EffectDraw::default();
        };
        checked(
            &self.effect_values,
            &self.colors,
            &self.length,
            self.label_line(),
            self.base == Base::Dark,
            *live,
        )
        .unwrap_or_default()
    }

    /// The line height of the label style, which sets a button's ink band.
    fn label_line(&self) -> f32 {
        self.type_styles
            .get("type.label")
            .map_or(0.0, |s| f32::from(s.line))
    }

    /// The colour warnings (ADR-0072), without the effects notice.
    pub fn colour_warnings(&self) -> impl Iterator<Item = &ThemeProblem> {
        self.warnings.iter().filter(|p| p.is_warning())
    }

    /// The effects notice, if the theme's effects show as calm, or without shading, for
    /// a reason.
    pub fn effects_notice(&self) -> Option<&ThemeProblem> {
        self.warnings
            .iter()
            .find(|p| p.severity == Severity::Notice)
    }

    /// True when docked panels and Settings cards draw their outline: the panel colour
    /// (`color.surface.raised`) is closer than [`PANEL_OUTLINE_BELOW`] to the window ground
    /// (`color.surface.base`), so colour alone would not separate them (ADR-0112
    /// decision 3). Also true when either colour is missing.
    pub fn panel_outline(&self) -> bool {
        match (
            self.colors.get("color.surface.raised"),
            self.colors.get("color.surface.base"),
        ) {
            (Some(panel), Some(ground)) => panel_outline(*panel, *ground),
            _ => true,
        }
    }

    /// True when secondary and danger buttons draw their `color.border.subtle` outline:
    /// a resting fill ([`BUTTON_FILLS`]) is closer than [`PANEL_OUTLINE_BELOW`] to one of
    /// the surfaces a button sits on, so the fill alone would not show the button's edge
    /// (ADR-0118). Also true when a colour is missing.
    pub fn button_outline(&self) -> bool {
        let get = |n: &str| self.colors.get(n).copied();
        BUTTON_FILLS.iter().any(|f| match get(f) {
            Some(fill) => BUTTON_SURFACES
                .iter()
                .map(|n| get(n))
                .any(|s| s.is_none_or(|s| button_outline(fill, s))),
            None => true,
        })
    }
}

/// The resting fills of buttons that may draw an outline: secondary and danger.
pub const BUTTON_FILLS: [&str; 2] = ["color.button.fill", "color.action.destructive.subtle"];

/// The surfaces a button sits on: the window ground, panels, wells and popups.
pub const BUTTON_SURFACES: [&str; 4] = [
    "color.surface.base",
    "color.surface.raised",
    "color.surface.sunken",
    "color.surface.overlay",
];

/// Whether a button filled with `fill` on a surface of colour `surface` needs its outline
/// (ADR-0118, the same threshold as a panel's, ADR-0112 decision 3).
pub fn button_outline(fill: Color, surface: Color) -> bool {
    fill.contrast(surface) < PANEL_OUTLINE_BELOW
}

/// Below this contrast between a panel and the window ground, the panel keeps its outline
/// (ADR-0112 decision 3). The built-in Dark and Light themes are above it.
pub const PANEL_OUTLINE_BELOW: f64 = 1.15;

/// Whether a panel of colour `panel` on a ground of colour `ground` needs its outline.
pub fn panel_outline(panel: Color, ground: Color) -> bool {
    panel.contrast(ground) < PANEL_OUTLINE_BELOW
}

/// Length chains a theme keeps in order, smallest first (`check_across`).
const LENGTH_CHAINS: &[&[&str]] = &[
    &["size.control.sm", "size.control.md", "size.control.lg"],
    &[
        "space.1", "space.2", "space.3", "space.4", "space.6", "space.8",
    ],
    &["size.field.gap", "size.section.gap"],
    &["size.icon.box", "size.button.chip", "size.control.sm"],
    &["radius.sm", "radius.md", "radius.lg"],
];

/// Text size chains a theme keeps in order, smallest first (`check_across`).
const SIZE_CHAINS: &[&[&str]] = &[
    &["type.caption", "type.body", "type.title", "type.heading"],
    &["type.body", "type.label", "type.title"],
];

/// Text styles whose line height must fit in `size.control.sm`.
const LINES_IN_SMALL_CONTROL: &[&str] = &["type.caption", "type.body", "type.label"];

impl Tokens {
    /// Carries the neighbours of `edited` along so `body` keeps the order rules of
    /// "Across tokens" (themes spec, "Editing"). `before` is the body before the edit:
    /// when the edit raised `edited`, every token that now breaks an order is raised to
    /// the value it must reach (the next one in its chain, `space.1` to the focus ring,
    /// `size.control.sm` to a text line); when it lowered `edited`, every token is
    /// lowered instead (the previous one in its chain, the focus ring to `space.1`, text
    /// lines to `size.control.sm`, never under their size). `edited` itself never moves,
    /// except `radius.button`, which is held at half of `size.control.sm` (a pill). A
    /// carried token stays within its own bounds; where that is not enough the rule
    /// still fails and `resolve` reports it. A carried value equal to the base's is
    /// written as no override.
    pub fn carry(&self, before: &ThemeBody, body: &mut ThemeBody, edited: &str) {
        let base = self.built_in_theme(body.base);
        let len = |b: &ThemeBody, n: &str| {
            b.length
                .get(n)
                .or_else(|| base.length.get(n))
                .copied()
                .unwrap_or(0.0)
        };
        let style = |b: &ThemeBody, n: &str| {
            b.type_styles
                .get(n)
                .or_else(|| base.type_styles.get(n))
                .copied()
        };
        let set_len = |b: &mut ThemeBody, n: &str, v: f32| {
            let v = match themeable(n).map(|t| t.kind) {
                Some(Kind::Length { min, max }) => v.clamp(min, max),
                _ => v,
            };
            if base.length.get(n) == Some(&v) {
                b.length.remove(n);
            } else {
                b.length.insert(n.to_owned(), v);
            }
        };
        let set_style = |b: &mut ThemeBody, n: &str, st: TypeStyle| {
            if base.type_styles.get(n) == Some(&st) {
                b.type_styles.remove(n);
            } else {
                b.type_styles.insert(n.to_owned(), st);
            }
        };
        // A text size within the bounds, with its line height kept from the size to
        // twice the size.
        let sized = |st: TypeStyle, size: u16| {
            let size = size.clamp(TYPE_SIZE.0, TYPE_SIZE.1);
            TypeStyle {
                size,
                line: st.line.clamp(size, size.saturating_mul(2)),
                weight: st.weight,
            }
        };
        let up = match (style(before, edited), style(body, edited)) {
            (Some(old), Some(new)) => new.size > old.size || new.line > old.line,
            _ => len(body, edited) > len(before, edited),
        };
        // Each pass moves values one way only, so a few passes settle every chain.
        for _ in 0..16 {
            let start = body.clone();
            for chain in LENGTH_CHAINS {
                for w in chain.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if len(body, a) > len(body, b) {
                        if up {
                            let v = len(body, a);
                            set_len(body, b, v);
                        } else {
                            let v = len(body, b);
                            set_len(body, a, v);
                        }
                    }
                }
            }
            for chain in SIZE_CHAINS {
                for w in chain.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    let (Some(sa), Some(sb)) = (style(body, a), style(body, b)) else {
                        continue;
                    };
                    if sa.size > sb.size {
                        if up {
                            set_style(body, b, sized(sb, sa.size));
                        } else {
                            set_style(body, a, sized(sa, sb.size));
                        }
                    }
                }
            }
            // The focus ring fits in space.1.
            let (ring, s1) = (len(body, "size.focus-ring"), len(body, "space.1"));
            if ring > s1 {
                if up {
                    set_len(body, "space.1", ring);
                } else {
                    set_len(body, "size.focus-ring", s1);
                }
            }
            // Text lines fit in a small control (a line never goes under its size).
            let sm = len(body, "size.control.sm");
            for n in LINES_IN_SMALL_CONTROL {
                if let Some(st) = style(body, n)
                    && f32::from(st.line) > sm
                {
                    if up {
                        set_len(body, "size.control.sm", f32::from(st.line));
                    } else {
                        let line = (sm as u16).max(st.size);
                        set_style(body, n, TypeStyle { line, ..st });
                    }
                }
            }
            // A button's corners are at most a pill: half of a small control.
            let half = len(body, "size.control.sm") / 2.0;
            if len(body, "radius.button") > half {
                set_len(body, "radius.button", half);
            }
            if *body == start {
                break;
            }
        }
    }

    /// The built-in theme `base` as a resolved theme.
    pub fn built_in_theme(&self, base: Base) -> ResolvedTheme {
        ResolvedTheme {
            base,
            colors: self.themes[base.index()].1.clone(),
            fixed: self.fixed.clone(),
            length: self.length.clone(),
            duration: self.duration.clone(),
            type_styles: self.type_styles.clone(),
            font_family: self.font_family.clone(),
            unknown: Vec::new(),
            warnings: Vec::new(),
            effects: Effects::Calm,
            effect_values: EffectValues::default(),
        }
    }

    /// The effects `theme` resolves to, with the notice when it asks for more than it
    /// gets (ADR-0120 decisions 2, 3, 6 and 9). `out` is the theme resolved so far.
    fn resolve_effects(
        &self,
        theme: &ThemeBody,
        out: &ResolvedTheme,
    ) -> (Effects, EffectValues, Option<ThemeProblem>) {
        let calm = (Effects::Calm, EffectValues::default());
        let Some(name) = theme.stored_effects() else {
            return (calm.0, calm.1, None);
        };
        let Some(setting) = Effects::from_name(name) else {
            return (
                calm.0,
                calm.1,
                Some(ThemeProblem::notice(
                    "This theme uses effects this version does not have; it shows as Calm".into(),
                )),
            );
        };
        let Some(tables) = &self.effects else {
            return (
                calm.0,
                calm.1,
                Some(ThemeProblem::notice(
                    "This build's token file has no effects".into(),
                )),
            );
        };
        if !tables.has(setting) {
            return (
                calm.0,
                calm.1,
                Some(ThemeProblem::notice(format!(
                    "This build's token file has no {} effects",
                    setting.label()
                ))),
            );
        }
        let values = tables.values(setting);
        let notice = effects_notice(setting, &values, out, &out.length);
        // Calm only when no density may draw the setting; otherwise the setting stays
        // and `effect_draw` gives calm at the densities that fail.
        if let Some(n) = &notice
            && n.everywhere
            && n.calm
        {
            return (calm.0, calm.1, Some(ThemeProblem::notice(n.to_string())));
        }
        if let Some(n) = notice {
            return (setting, values, Some(ThemeProblem::notice(n.to_string())));
        }
        (setting, values, None)
    }

    /// Applies `theme`'s overrides to its base and runs every check and theme rule.
    /// Returns every problem at once.
    pub fn resolve(&self, theme: &ThemeBody) -> Result<ResolvedTheme, ThemeProblems> {
        let mut problems = Vec::new();
        let mut out = self.built_in_theme(theme.base);
        let not_themeable = |name: &str, problems: &mut Vec<ThemeProblem>| {
            let why = if FIXED.iter().any(|(n, _)| *n == name) {
                format!(
                    "{name} is the same in every theme; no theme can change it (rule U12, ADR-0071)"
                )
            } else {
                format!("{name} is not part of a theme")
            };
            problems.push(ThemeProblem::at(name, why));
        };
        for (name, c) in &theme.colors {
            if themeable(name).is_some() {
                out.colors.insert(name.clone(), *c);
            } else {
                not_themeable(name, &mut problems);
            }
        }
        for (name, v) in &theme.length {
            if themeable(name).is_some() {
                out.length.insert(name.clone(), *v);
            } else {
                not_themeable(name, &mut problems);
            }
        }
        for (name, v) in &theme.duration {
            if themeable(name).is_some() {
                out.duration.insert(name.clone(), *v);
            } else {
                not_themeable(name, &mut problems);
            }
        }
        for (name, v) in &theme.type_styles {
            if themeable(name).is_some() {
                out.type_styles.insert(name.clone(), *v);
            } else {
                not_themeable(name, &mut problems);
            }
        }
        if let Some(f) = &theme.font_family {
            out.font_family = f.clone();
        }
        for (name, (_, _, why)) in &theme.invalid {
            problems.push(ThemeProblem::at(name, format!("{name} {why}")));
        }
        out.unknown = theme.unknown_names();
        if out.unknown.len() > MAX_UNKNOWN_TOKENS {
            problems.push(ThemeProblem {
                token: None,
                reason: format!(
                    "the theme has {} token names this version does not know; at most {MAX_UNKNOWN_TOKENS} are kept",
                    out.unknown.len()
                ),
                severity: Severity::Error,
                rule: None,
            });
        }
        let look = Look {
            colors: &out.colors,
            live: self.fixed["color.state.live"],
            length: &out.length,
            duration: &out.duration,
            type_styles: &out.type_styles,
            font_family: &out.font_family,
        };
        problems.extend(look.check());
        let (effects, values, notice) = self.resolve_effects(theme, &out);
        out.effects = effects;
        out.effect_values = values;
        problems.extend(notice);
        // Colour rules only warn on a user theme (ADR-0072); any other problem refuses it.
        if problems.iter().any(ThemeProblem::is_error) {
            Err(ThemeProblems(problems))
        } else {
            out.warnings = problems;
            Ok(out)
        }
    }
}

/// The densities the effect checks run for, with the name a notice uses (ADR-0122
/// decision 6): regular, roomy and compact steps 1 to 3.
pub const EFFECT_DENSITIES: [(Density, u8, &str); 5] = [
    (Density::Regular, 1, "Regular"),
    (Density::Roomy, 1, "Roomy"),
    (Density::Compact, 1, "Compact 1"),
    (Density::Compact, 2, "Compact 2"),
    (Density::Compact, 3, "Compact 3"),
];

/// Why a setting does not draw all it could (ADR-0122 decisions 5 and 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectsNotice {
    /// The reason in words.
    pub reason: String,
    /// The setting shows as calm there (a failed check), not only without shading.
    pub calm: bool,
    /// It applies to every density.
    pub everywhere: bool,
    /// The densities it applies to, by name.
    pub densities: Vec<&'static str>,
}

impl fmt::Display for EffectsNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.everywhere {
            write!(f, "{}", self.reason)
        } else {
            write!(
                f,
                "{} (density: {})",
                self.reason,
                self.densities.join(", ")
            )
        }
    }
}

/// Runs the effect checks of `values` (the table of `setting`) on `theme`'s colours for
/// every density of `length` (lengths before density): the first reason found and the
/// densities it or any other reason applies to, or `None` when every density draws the
/// setting in full.
pub fn effects_notice(
    setting: Effects,
    values: &EffectValues,
    theme: &ResolvedTheme,
    length: &BTreeMap<String, f32>,
) -> Option<EffectsNotice> {
    let live = *theme.fixed.get("color.state.live")?;
    let mut reason = None;
    let mut calm = true;
    let mut densities = Vec::new();
    for (density, steps, name) in EFFECT_DENSITIES {
        let mut l = length.clone();
        apply_density(&mut l, density, steps);
        let why = match checked(
            values,
            &theme.colors,
            &l,
            theme.label_line(),
            theme.base == Base::Dark,
            live,
        ) {
            Err(why) => Some((format!("The effects show as Calm: {why}"), true)),
            Ok(d) if setting == Effects::Glass && d.faces_are_flat() => Some((
                "This theme's colours leave no room for Glass shading".to_owned(),
                false,
            )),
            Ok(_) => None,
        };
        match why {
            Some((why, is_calm)) => {
                calm &= is_calm;
                reason.get_or_insert(why);
                densities.push(name);
            }
            None => calm = false,
        }
    }
    reason.map(|reason| EffectsNotice {
        reason,
        calm,
        everywhere: densities.len() == EFFECT_DENSITIES.len(),
        densities,
    })
}

/// One complete set of look values: what every theme, built-in or resolved, is
/// checked on.
pub(super) struct Look<'a> {
    pub colors: &'a BTreeMap<String, Color>,
    pub live: Color,
    pub length: &'a BTreeMap<String, f32>,
    pub duration: &'a BTreeMap<String, u32>,
    pub type_styles: &'a BTreeMap<String, TypeStyle>,
    pub font_family: &'a [String],
}

impl Look<'_> {
    /// Every problem, in a stable order: contrast, hue, distance from live, bounds,
    /// rules across tokens, font names.
    pub fn check(&self) -> Vec<ThemeProblem> {
        let mut p = Vec::new();
        let c = |n: &str| self.colors.get(n).copied();
        // A hover or menu plate that matches the panel is invisible in every list
        // that only changes its background (the light theme used to do this).
        if let (Some(overlay), Some(raised)) =
            (c("color.surface.overlay"), c("color.surface.raised"))
            && overlay == raised
        {
            p.push(ThemeProblem::at(
                "color.surface.overlay",
                "color.surface.overlay must differ from color.surface.raised, so a row under the pointer is visible".into(),
            ));
        }
        for &(fg, bg, need) in CONTRAST_RULES.iter().chain(THEME_CONTRAST_RULES) {
            if let (Some(f), Some(b)) = (c(fg), c(bg)) {
                let ratio = f.contrast(b);
                if ratio < need {
                    p.push(ThemeProblem::warn(
                        fg,
                        format!("{fg} on {bg} has contrast {ratio:.2}, needs {need}"),
                        ColorRule::Contrast {
                            fg: (*fg).to_owned(),
                            bg: (*bg).to_owned(),
                            ratio: ratio as f32,
                            need: need as f32,
                        },
                    ));
                }
            }
        }
        for t in [
            "color.state.error",
            "color.action.destructive",
            "color.meter.clip",
        ] {
            if c(t).is_some_and(|v| hue_distance(v, self.live) < MIN_HUE_DISTANCE_FROM_LIVE) {
                p.push(ThemeProblem::warn(
                    t,
                    format!("{t} is too close in hue to color.state.live (rule U12)"),
                    ColorRule::LiveHue {
                        token: t.to_owned(),
                    },
                ));
            }
        }
        for s in LIVE_SURFACES {
            if let Some(b) = c(s) {
                let ratio = self.live.contrast(b);
                if ratio < 3.0 {
                    p.push(ThemeProblem::warn(
                        s,
                        format!("color.state.live on {s} has contrast {ratio:.2}, needs 3"),
                        ColorRule::Contrast {
                            fg: "color.state.live".to_owned(),
                            bg: (*s).to_owned(),
                            ratio: ratio as f32,
                            need: 3.0,
                        },
                    ));
                }
            }
        }
        for name in THEME_COLORS {
            if let Some(v) = c(name) {
                let d = delta_e_2000(v, self.live);
                if d < MIN_DELTA_E_FROM_LIVE {
                    p.push(ThemeProblem::warn(
                        name,
                        format!(
                            "{name} looks like the live colour (difference {d:.1}, needs {MIN_DELTA_E_FROM_LIVE}; rule U12)"
                        ),
                        ColorRule::LiveLook {
                            token: (*name).to_owned(),
                            difference: d as f32,
                            need: MIN_DELTA_E_FROM_LIVE as f32,
                        },
                    ));
                }
            }
        }
        self.check_bounds(&mut p);
        self.check_across(&mut p);
        p
    }

    fn check_bounds(&self, p: &mut Vec<ThemeProblem>) {
        for t in THEMEABLE {
            match t.kind {
                Kind::Length { min, max } => {
                    if let Some(v) = self.length.get(t.name)
                        && !(min..=max).contains(v)
                    {
                        p.push(ThemeProblem::at(
                            t.name,
                            format!("{} must be from {min} to {max}, not {v}", t.name),
                        ));
                    }
                }
                Kind::Duration { min, max } => {
                    if let Some(v) = self.duration.get(t.name)
                        && !(min..=max).contains(v)
                    {
                        p.push(ThemeProblem::at(
                            t.name,
                            format!(
                                "{} must be from {min} to {max} ms (rule U12), not {v}",
                                t.name
                            ),
                        ));
                    }
                }
                Kind::TypeStyle => {
                    if let Some(s) = self.type_styles.get(t.name) {
                        let (lo, hi) = TYPE_SIZE;
                        let (wlo, whi) = TYPE_WEIGHT;
                        if !(lo..=hi).contains(&s.size)
                            || s.line < s.size
                            || u32::from(s.line) > 2 * u32::from(s.size)
                            || !(wlo..=whi).contains(&s.weight)
                        {
                            p.push(ThemeProblem::at(
                                t.name,
                                format!(
                                    "{} needs a size from {lo} to {hi}, a line height from the size to twice the size, and a weight from {wlo} to {whi}",
                                    t.name
                                ),
                            ));
                        }
                    }
                }
                Kind::FontFamily => {
                    if self.font_family.len() > MAX_FONT_FAMILIES {
                        p.push(ThemeProblem::at(
                            t.name,
                            format!("{} names more than {MAX_FONT_FAMILIES} families", t.name),
                        ));
                    }
                    for name in self.font_family {
                        if name.trim().is_empty() {
                            p.push(ThemeProblem::at(
                                t.name,
                                "a font family name is empty".into(),
                            ));
                        } else if let Err(why) =
                            plain_line(name, MAX_FONT_NAME, "a font family name")
                        {
                            p.push(ThemeProblem::at(t.name, why.to_string()));
                        }
                    }
                }
                Kind::Color => {}
            }
        }
    }

    fn check_across(&self, p: &mut Vec<ThemeProblem>) {
        let len = |n: &str| self.length.get(n).copied();
        let size = |n: &str| self.type_styles.get(n).map(|s| f32::from(s.size));
        if let Some(sm) = len("size.control.sm") {
            for t in ["type.caption", "type.body", "type.label"] {
                if let Some(s) = self.type_styles.get(t)
                    && f32::from(s.line) > sm
                {
                    p.push(ThemeProblem::at(
                        t,
                        format!("the line height of {t} must fit in size.control.sm ({sm})"),
                    ));
                }
            }
            if len("size.icon.box").is_some_and(|v| v > sm) {
                p.push(ThemeProblem::at(
                    "size.icon.box",
                    format!("size.icon.box must fit in size.control.sm ({sm})"),
                ));
            }
        }
        let mut ascending = |names: &[&str], get: &dyn Fn(&str) -> Option<f32>| {
            for pair in names.windows(2) {
                if let (Some(a), Some(b)) = (get(pair[0]), get(pair[1]))
                    && a > b
                {
                    p.push(ThemeProblem::at(
                        pair[1],
                        format!("{} must not be smaller than {}", pair[1], pair[0]),
                    ));
                }
            }
        };
        ascending(
            &["size.control.sm", "size.control.md", "size.control.lg"],
            &len,
        );
        ascending(
            &[
                "space.1", "space.2", "space.3", "space.4", "space.6", "space.8",
            ],
            &len,
        );
        ascending(&["size.field.gap", "size.section.gap"], &len);
        // A button's icon fits its chip, and the chip fits a dense button (ADR-0118).
        ascending(
            &["size.icon.box", "size.button.chip", "size.control.sm"],
            &len,
        );
        ascending(&["radius.sm", "radius.md", "radius.lg"], &len);
        ascending(
            &["type.caption", "type.body", "type.title", "type.heading"],
            &size,
        );
        ascending(&["type.body", "type.label", "type.title"], &size);
        if let (Some(ring), Some(space)) = (len("size.focus-ring"), len("space.1"))
            && ring > space
        {
            p.push(ThemeProblem::at(
                "size.focus-ring",
                format!("size.focus-ring must not be wider than space.1 ({space})"),
            ));
        }
        if let (Some(r), Some(sm)) = (len("radius.button"), len("size.control.sm"))
            && r > sm / 2.0
        {
            p.push(ThemeProblem::at(
                "radius.button",
                format!("radius.button must not be more than half of size.control.sm ({sm})"),
            ));
        }
    }
}

/// CIE Lab (D65) of an sRGB colour.
fn lab(c: Color) -> (f64, f64, f64) {
    let lin = |v: u8| {
        let v = f64::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(c.r), lin(c.g), lin(c.b));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f64| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    (
        116.0 * f(y) - 16.0,
        500.0 * (f(x) - f(y)),
        200.0 * (f(y) - f(z)),
    )
}

/// CIEDE2000 colour difference.
pub fn delta_e_2000(a: Color, b: Color) -> f64 {
    let (l1, a1, b1) = lab(a);
    let (l2, a2, b2) = lab(b);
    let pow7 = |v: f64| v.powi(7);
    let c_bar = (a1.hypot(b1) + a2.hypot(b2)) / 2.0;
    let g = 0.5 * (1.0 - (pow7(c_bar) / (pow7(c_bar) + pow7(25.0))).sqrt());
    let (a1p, a2p) = ((1.0 + g) * a1, (1.0 + g) * a2);
    let (c1p, c2p) = (a1p.hypot(b1), a2p.hypot(b2));
    let hue = |b: f64, a: f64| b.atan2(a).to_degrees().rem_euclid(360.0);
    let (h1, h2) = (hue(b1, a1p), hue(b2, a2p));
    let dl = l2 - l1;
    let dc = c2p - c1p;
    let mut dh = h2 - h1;
    if c1p * c2p == 0.0 {
        dh = 0.0;
    } else if dh > 180.0 {
        dh -= 360.0;
    } else if dh < -180.0 {
        dh += 360.0;
    }
    let dh_big = 2.0 * (c1p * c2p).sqrt() * (dh.to_radians() / 2.0).sin();
    let l_bar = (l1 + l2) / 2.0;
    let cp_bar = (c1p + c2p) / 2.0;
    let h_bar = if c1p * c2p == 0.0 {
        h1 + h2
    } else if (h1 - h2).abs() > 180.0 {
        if h1 + h2 < 360.0 {
            (h1 + h2 + 360.0) / 2.0
        } else {
            (h1 + h2 - 360.0) / 2.0
        }
    } else {
        (h1 + h2) / 2.0
    };
    let cos = |deg: f64| deg.to_radians().cos();
    let t =
        1.0 - 0.17 * cos(h_bar - 30.0) + 0.24 * cos(2.0 * h_bar) + 0.32 * cos(3.0 * h_bar + 6.0)
            - 0.20 * cos(4.0 * h_bar - 63.0);
    let d_theta = 30.0 * (-((h_bar - 275.0) / 25.0).powi(2)).exp();
    let rc = 2.0 * (pow7(cp_bar) / (pow7(cp_bar) + pow7(25.0))).sqrt();
    let sl = 1.0 + 0.015 * (l_bar - 50.0).powi(2) / (20.0 + (l_bar - 50.0).powi(2)).sqrt();
    let sc = 1.0 + 0.045 * cp_bar;
    let sh = 1.0 + 0.015 * cp_bar * t;
    let rt = -(2.0 * d_theta).to_radians().sin() * rc;
    ((dl / sl).powi(2) + (dc / sc).powi(2) + (dh_big / sh).powi(2) + rt * (dc / sc) * (dh_big / sh))
        .sqrt()
}

impl From<ThemeProblems> for TokenError {
    fn from(p: ThemeProblems) -> TokenError {
        TokenError(p.to_string())
    }
}

/// A colour close to `token`'s current one that clears every colour warning of the
/// token (ADR-0072): the hue and saturation stay, the lightness moves the least that
/// is enough. `None` when the token has no warning or no lightness helps.
pub fn suggest_colour(tokens: &Tokens, theme: &ThemeBody, token: &str) -> Option<Color> {
    let resolved = |t: &ThemeBody| tokens.resolve(t).ok();
    let now = resolved(theme)?;
    let start = *now.colors.get(token)?;
    let concerns = |r: &ResolvedTheme| {
        r.warnings.iter().any(|w| match &w.rule {
            Some(ColorRule::Contrast { fg, bg, .. }) => fg == token || bg == token,
            Some(ColorRule::LiveHue { token: t }) | Some(ColorRule::LiveLook { token: t, .. }) => {
                t == token
            }
            None => false,
        })
    };
    if !concerns(&now) {
        return None;
    }
    // Lightness cannot change a hue or a look, so only a contrast warning has a fix.
    let contrast = |r: &ResolvedTheme| {
        r.warnings.iter().any(|w| match &w.rule {
            Some(ColorRule::Contrast { fg, bg, .. }) => fg == token || bg == token,
            _ => false,
        })
    };
    if !contrast(&now) {
        return None;
    }
    let (h, sat, l) = to_hsl(start);
    // 1 % steps of lightness, nearest first, darker and lighter alternating (at most
    // 200 resolves, only when a row with a contrast warning is shown).
    for step in 1..=100 {
        let d = f64::from(step) * 0.01;
        for candidate in [l - d, l + d] {
            if !(0.0..=1.0).contains(&candidate) {
                continue;
            }
            let c = from_hsl(h, sat, candidate);
            let mut t = theme.clone();
            t.colors.insert(token.to_owned(), c);
            if let Some(r) = resolved(&t)
                && !concerns(&r)
            {
                return Some(c);
            }
        }
    }
    None
}

fn to_hsl(c: Color) -> (f64, f64, f64) {
    let (r, g, b) = (
        f64::from(c.r) / 255.0,
        f64::from(c.g) / 255.0,
        f64::from(c.b) / 255.0,
    );
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d == 0.0 {
        return (0.0, 0.0, l);
    }
    let s = d / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h, s, l)
}

fn from_hsl(h: f64, s: f64, l: f64) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h / 60.0) as u32 % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let byte = |v: f64| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    Color {
        r: byte(r),
        g: byte(g),
        b: byte(b),
        a: 255,
    }
}
