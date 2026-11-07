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
    CONTRAST_RULES, Color, FIXED, LIVE_SURFACES, MIN_DELTA_E_FROM_LIVE, MIN_HUE_DISTANCE_FROM_LIVE,
    THEME_COLORS, THEME_CONTRAST_RULES, THEMES, TokenError, Tokens, TypeStyle, hue_distance,
    valid_name,
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
    /// `high-contrast`: contrast rules use the high-contrast thresholds.
    HighContrast,
}

impl Base {
    /// Every base, in [`THEMES`] order.
    pub const ALL: [Base; 3] = [Base::Dark, Base::Light, Base::HighContrast];

    /// The name used in files and in [`THEMES`].
    pub fn name(self) -> &'static str {
        THEMES[self.index()]
    }

    /// The position in [`THEMES`] and [`Tokens::themes`].
    pub fn index(self) -> usize {
        match self {
            Base::Dark => 0,
            Base::Light => 1,
            Base::HighContrast => 2,
        }
    }

    /// The base named `name`.
    pub fn from_name(name: &str) -> Option<Base> {
        Base::ALL.into_iter().find(|b| b.name() == name)
    }
}

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
    colour("color.state.warning", "Warning"),
    colour("color.state.healthy", "Healthy"),
    colour("color.state.error", "Error"),
    colour("color.action.destructive", "Destructive action"),
    colour("color.meter.low", "Meter, low level"),
    colour("color.meter.mid", "Meter, middle level"),
    colour("color.meter.high", "Meter, high level"),
    colour("color.meter.clip", "Meter, clipped"),
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
    length("size.border", "Border width", Shapes, 1.0, 4.0),
    length("size.focus-ring", "Focus ring width", Shapes, 2.0, 6.0),
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

/// One thing wrong with a theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeProblem {
    /// The token it belongs to, if any (the editor shows it beside that token).
    pub token: Option<String>,
    /// What is wrong, in words.
    pub reason: String,
}

impl ThemeProblem {
    fn at(token: &str, reason: String) -> ThemeProblem {
        ThemeProblem {
            token: Some(token.to_owned()),
            reason,
        }
    }
}

impl fmt::Display for ThemeProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Every problem of a theme that is not used; never empty.
#[derive(Clone, Debug, PartialEq, Eq)]
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

/// Display text: one line, no control characters or text direction marks.
fn plain_line(s: &str, max: usize, what: &str) -> Result<(), String> {
    if s.chars().count() > max {
        return Err(format!("{what} is longer than {max} characters"));
    }
    if s.chars().any(|c| c.is_control() || is_direction_mark(c)) {
        return Err(format!(
            "{what} has a control character or text direction mark"
        ));
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
    pub fn from_body(mut body: toml::Table) -> Result<ThemeBody, String> {
        let base = match body.remove("base") {
            Some(toml::Value::String(b)) => Base::from_name(&b)
                .ok_or_else(|| format!("base {b:?} is not one of dark, light, high-contrast"))?,
            Some(_) => return Err("base must be a string".into()),
            None => return Err("base is missing".into()),
        };
        let mut theme = ThemeBody::new(base);
        for (key, max) in [("author", MAX_AUTHOR), ("description", MAX_DESCRIPTION)] {
            match body.remove(key) {
                Some(toml::Value::String(s)) => {
                    plain_line(&s, max, key)?;
                    let slot = if key == "author" {
                        &mut theme.author
                    } else {
                        &mut theme.description
                    };
                    *slot = Some(s);
                }
                Some(_) => return Err(format!("{key} must be a string")),
                None => {}
            }
        }
        for table in TABLES {
            let entries = match body.remove(table) {
                Some(toml::Value::Table(t)) => t,
                Some(_) => return Err(format!("[{table}] must be a table")),
                None => continue,
            };
            for (name, value) in entries {
                theme.read_entry(table, name, value)?;
            }
        }
        if let Some(extra) = body.keys().next() {
            return Err(format!("unknown key {extra}"));
        }
        Ok(theme)
    }

    fn read_entry(&mut self, table: &str, name: String, value: toml::Value) -> Result<(), String> {
        if !valid_name(&name) {
            return Err(format!("invalid token name {name:?} in [{table}]"));
        }
        let Some(home) = known_table(&name) else {
            self.unknown
                .entry(table.to_owned())
                .or_default()
                .insert(name, value);
            return Ok(());
        };
        if home != table {
            return Err(format!("{name} belongs in [{home}], not [{table}]"));
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

/// A theme ready to apply: its base with the overrides, every check passed.
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
}

impl ResolvedTheme {
    /// True when the base is `high-contrast`.
    pub fn high_contrast(&self) -> bool {
        self.base == Base::HighContrast
    }
}

impl Tokens {
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
        }
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
            });
        }
        let look = Look {
            colors: &out.colors,
            live: self.fixed["color.state.live"],
            high_contrast: out.high_contrast(),
            length: &out.length,
            duration: &out.duration,
            type_styles: &out.type_styles,
            font_family: &out.font_family,
        };
        problems.extend(look.check());
        if problems.is_empty() {
            Ok(out)
        } else {
            Err(ThemeProblems(problems))
        }
    }
}

/// One complete set of look values: what every theme, built-in or resolved, is
/// checked on.
pub(super) struct Look<'a> {
    pub colors: &'a BTreeMap<String, Color>,
    pub live: Color,
    pub high_contrast: bool,
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
        for (fg, bg, min, min_hc) in CONTRAST_RULES.iter().chain(THEME_CONTRAST_RULES) {
            let need = if self.high_contrast { *min_hc } else { *min };
            if let (Some(f), Some(b)) = (c(fg), c(bg)) {
                let ratio = f.contrast(b);
                if ratio < need {
                    p.push(ThemeProblem::at(
                        fg,
                        format!("{fg} on {bg} has contrast {ratio:.2}, needs {need}"),
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
                p.push(ThemeProblem::at(
                    t,
                    format!("{t} is too close in hue to color.state.live (rule U12)"),
                ));
            }
        }
        for s in LIVE_SURFACES {
            if let Some(b) = c(s) {
                let ratio = self.live.contrast(b);
                if ratio < 3.0 {
                    p.push(ThemeProblem::at(
                        s,
                        format!("color.state.live on {s} has contrast {ratio:.2}, needs 3"),
                    ));
                }
            }
        }
        for name in THEME_COLORS {
            if let Some(v) = c(name) {
                let d = delta_e_2000(v, self.live);
                if d < MIN_DELTA_E_FROM_LIVE {
                    p.push(ThemeProblem::at(
                        name,
                        format!(
                            "{name} looks like the live colour (difference {d:.1}, needs {MIN_DELTA_E_FROM_LIVE}; rule U12)"
                        ),
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
                            p.push(ThemeProblem::at(t.name, why));
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
