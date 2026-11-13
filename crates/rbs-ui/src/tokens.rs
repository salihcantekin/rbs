//! Design tokens: parsing and checking `tokens.toml`.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Tokens". Every theme defines the
//! same colour names; live colours are fixed outside the themes (rule U12);
//! text and surface pairs pass the contrast check of rule U7.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

mod theme;
pub use theme::*;

/// The token file shipped with RBS.
pub const BUILT_IN: &str = include_str!("../tokens.toml");

/// Themes every token file defines.
pub const THEMES: [&str; 3] = ["dark", "light", "high-contrast"];

/// Colour tokens every theme defines.
pub const THEME_COLORS: &[&str] = &[
    "color.surface.base",
    "color.surface.raised",
    "color.surface.overlay",
    "color.surface.sunken",
    "color.toast.surface",
    "color.toast.edge",
    "color.text.primary",
    "color.text.secondary",
    "color.text.disabled",
    "color.text.inverse",
    "color.border.subtle",
    "color.border.strong",
    "color.border.focus",
    "color.accent",
    "color.accent.hover",
    "color.accent.subtle",
    "color.brand",
    "color.state.warning",
    "color.state.healthy",
    "color.state.error",
    "color.action.destructive",
    "color.meter.low",
    "color.meter.mid",
    "color.meter.high",
    "color.meter.clip",
];

/// Colour tokens that are the same in every theme, with their only allowed
/// values. A custom token file must repeat them unchanged: the live colours (rule
/// U12) and the Windows close button's hover, as the OS draws it (ADR-0071).
pub const FIXED: &[(&str, &str)] = &[
    ("color.state.live", "#c4262e"),
    ("color.text.on-live", "#ffffff"),
    ("color.caption.close-hover", "#c42b1c"),
    ("color.caption.close-text", "#ffffff"),
];

/// Lengths a token file may not set below these values: the focus ring must stay
/// visible and icons legible (rule U7), and the main window must fit the 1280 x 720 minimum (rule U11).
pub const MINIMUMS: &[(&str, f32, &str)] = &[
    ("size.focus-ring", 2.0, "rule U7"),
    ("size.toast.border", 2.0, "the notice stays visible"),
    ("size.toast.rail", 4.0, "the notice stays visible"),
    ("size.panel.pad", 8.0, "ADR-0076"),
    ("size.section.gap", 12.0, "ADR-0076"),
    ("size.field.gap", 6.0, "ADR-0076"),
    ("size.tab.indicator", 2.0, "rule U7"),
    ("size.icon.box", 12.0, "rule U7"),
    ("size.icon.stroke", 1.0, "rule U7"),
    ("size.window.min-w", 1280.0, "rule U11"),
    ("size.window.min-h", 720.0, "rule U11"),
];

/// Length tokens every file defines (logical pixels).
pub const LENGTHS: &[&str] = &[
    "space.1",
    "space.2",
    "space.3",
    "space.4",
    "space.6",
    "space.8",
    "size.panel.pad",
    "size.section.gap",
    "size.field.gap",
    "size.field.compact",
    "size.tab.indicator",
    "size.control.sm",
    "size.control.md",
    "size.control.lg",
    "size.color.code",
    "radius.sm",
    "radius.md",
    "radius.lg",
    "size.border",
    "size.focus-ring",
    "size.icon.box",
    "size.icon.stroke",
    "size.menu.sm",
    "size.menu.md",
    "size.row.stack-below",
    "size.row.picture.sm",
    "size.row.picture.md",
    "size.row.picture.lg",
    "size.select.list-h",
    "size.app-menu.w",
    "size.app-menu.categories",
    "size.app-menu.list-h",
    "size.caption.w",
    "size.window.w",
    "size.window.h",
    "size.window.min-w",
    "size.window.min-h",
    "size.floating.w",
    "size.floating.h",
    "size.settings.w",
    "size.settings.h",
    "step.nudge",
    "step.nudge-large",
    "size.tracking.caps",
    "size.meter.peak",
    "size.meter.bar",
    "size.meter.scale",
    "size.meter.scale-line",
    "size.meter.readout",
    "size.mixer.strip",
    "size.mixer.splitter",
    "size.mixer.controls",
    "size.mixer.min-h",
    "size.transition.narrow",
    "size.transition.short",
    "size.transition.between",
    "size.transition.icons",
    "size.transition.below",
    "size.transition.min-picture",
    "size.transition.min-h",
    "size.transition.type",
    "size.transition.type-min",
    "size.transition.duration",
    "size.transition.compact",
    "size.library.thumb-w",
    "size.library.thumb-h",
    "size.target.thumb-w",
    "size.target.thumb-h",
    "size.toast.border",
    "size.toast.rail",
    "size.toast.max",
];

/// Duration tokens every file defines (milliseconds).
pub const DURATIONS: &[&str] = &[
    "motion.fast",
    "motion.normal",
    "delay.repeat-start",
    "delay.repeat",
    "delay.tooltip",
    "delay.toast",
    "delay.toast-tick",
];

/// Text styles every file defines.
pub const TYPE_STYLES: &[&str] = &[
    "type.caption",
    "type.body",
    "type.label",
    "type.title",
    "type.heading",
];

/// An sRGB colour with alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
    /// Alpha, 255 is opaque.
    pub a: u8,
}

impl Color {
    /// Parse `#rrggbb` or `#rrggbbaa`. Token files accept only `#rrggbb`,
    /// because contrast is checked on opaque colours.
    pub fn parse(s: &str) -> Option<Color> {
        let hex = s.strip_prefix('#')?;
        if !(hex.len() == 6 || hex.len() == 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Color {
            r: byte(0)?,
            g: byte(2)?,
            b: byte(4)?,
            a: if hex.len() == 8 { byte(6)? } else { 255 },
        })
    }

    /// WCAG 2 relative luminance (alpha ignored).
    pub fn luminance(self) -> f64 {
        let ch = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(self.r) + 0.7152 * ch(self.g) + 0.0722 * ch(self.b)
    }

    /// WCAG 2 contrast ratio between two opaque colours, 1 to 21.
    pub fn contrast(self, other: Color) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)?;
        if self.a != 255 {
            write!(f, "{:02x}", self.a)?;
        }
        Ok(())
    }
}

/// A text style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeStyle {
    /// Font size in logical pixels.
    pub size: u16,
    /// Line height in logical pixels.
    pub line: u16,
    /// Font weight, 100 to 900.
    pub weight: u16,
}

/// A parsed and checked token file.
#[derive(Clone, Debug, PartialEq)]
pub struct Tokens {
    /// Font families in order of preference; the first installed one is used, and
    /// none (the built-in value) means the OS UI font. Format 1 files have none.
    pub font_family: Vec<String>,
    /// Colours that no theme can change.
    pub fixed: BTreeMap<String, Color>,
    /// Colours per theme, in [`THEMES`] order.
    pub themes: Vec<(String, BTreeMap<String, Color>)>,
    /// Lengths in logical pixels.
    pub length: BTreeMap<String, f32>,
    /// Durations in milliseconds.
    pub duration: BTreeMap<String, u32>,
    /// Text styles.
    pub type_styles: BTreeMap<String, TypeStyle>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTokens {
    format: u32,
    fixed: BTreeMap<String, String>,
    themes: BTreeMap<String, BTreeMap<String, String>>,
    length: BTreeMap<String, f32>,
    duration: BTreeMap<String, u32>,
    #[serde(rename = "type")]
    type_styles: BTreeMap<String, TypeStyle>,
    font: Option<BTreeMap<String, Vec<String>>>,
}

/// A problem in a token file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenError(pub String);

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "design tokens: {}", self.0)
    }
}

impl std::error::Error for TokenError {}

fn colors(
    raw: BTreeMap<String, String>,
    expected: &[&str],
    what: &str,
) -> Result<BTreeMap<String, Color>, TokenError> {
    for name in expected {
        if !raw.contains_key(*name) {
            return Err(TokenError(format!("{what} is missing {name}")));
        }
    }
    raw.into_iter()
        .map(|(k, v)| {
            if !expected.contains(&k.as_str()) {
                return Err(TokenError(format!("{what} has unknown colour {k}")));
            }
            let c = Color::parse(&v)
                .filter(|c| c.a == 255 && v.len() == 7)
                .ok_or_else(|| TokenError(format!("{what}: {k} = {v:?} is not #rrggbb")))?;
            Ok((k, c))
        })
        .collect()
}

/// A token name that maps to an identifier in every toolkit: lowercase words
/// of letters and digits joined by `.` or `-`.
fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.split(['.', '-']).all(|w| {
            !w.is_empty()
                && w.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// Contrast rules of rule U7: (foreground, background, minimum ratio, high-contrast minimum).
pub const CONTRAST_RULES: &[(&str, &str, f64, f64)] = &[
    ("color.text.primary", "color.surface.base", 4.5, 7.0),
    ("color.text.primary", "color.surface.raised", 4.5, 7.0),
    ("color.text.primary", "color.surface.overlay", 4.5, 7.0),
    ("color.text.primary", "color.surface.sunken", 4.5, 7.0),
    ("color.text.secondary", "color.surface.base", 4.5, 7.0),
    ("color.text.secondary", "color.surface.raised", 4.5, 7.0),
    ("color.text.secondary", "color.surface.overlay", 4.5, 7.0),
    ("color.text.secondary", "color.surface.sunken", 4.5, 7.0),
    ("color.text.inverse", "color.accent", 4.5, 7.0),
    ("color.text.primary", "color.accent.subtle", 4.5, 7.0),
    ("color.border.focus", "color.surface.base", 3.0, 3.0),
    ("color.border.focus", "color.surface.raised", 3.0, 3.0),
    ("color.border.strong", "color.surface.raised", 3.0, 3.0),
    ("color.state.warning", "color.surface.raised", 4.5, 7.0),
    ("color.state.healthy", "color.surface.raised", 3.0, 3.0),
    ("color.state.error", "color.surface.raised", 3.0, 3.0),
    ("color.action.destructive", "color.surface.raised", 3.0, 3.0),
    ("color.meter.low", "color.surface.sunken", 3.0, 3.0),
    ("color.meter.mid", "color.surface.sunken", 3.0, 3.0),
    ("color.meter.high", "color.surface.sunken", 3.0, 3.0),
    ("color.meter.clip", "color.surface.sunken", 3.0, 3.0),
];

/// Contrast rules that exist because themes are untrusted input (ADR-0057 decision 3);
/// the built-in file passes them too: (foreground, background, minimum ratio,
/// high-contrast minimum).
pub const THEME_CONTRAST_RULES: &[(&str, &str, f64, f64)] = &[
    ("color.border.focus", "color.surface.overlay", 3.0, 3.0),
    ("color.border.focus", "color.surface.sunken", 3.0, 3.0),
    ("color.text.inverse", "color.accent.hover", 4.5, 7.0),
    ("color.accent", "color.surface.base", 3.0, 3.0),
    ("color.accent", "color.surface.raised", 3.0, 3.0),
    ("color.accent", "color.surface.overlay", 3.0, 3.0),
    ("color.accent", "color.surface.sunken", 3.0, 3.0),
    ("color.text.secondary", "color.accent.subtle", 4.5, 7.0),
    // Pairs the desktop UI draws (`rbs-app` test `every_drawn_pair_has_a_contrast_rule`).
    // Error messages are text, so they need the text minimum.
    ("color.state.error", "color.surface.base", 4.5, 7.0),
    ("color.state.error", "color.surface.raised", 4.5, 7.0),
    ("color.state.error", "color.surface.overlay", 4.5, 7.0),
    ("color.state.error", "color.surface.sunken", 4.5, 7.0),
    ("color.state.error", "color.accent.subtle", 4.5, 7.0),
    ("color.border.focus", "color.accent.subtle", 3.0, 3.0),
    ("color.border.strong", "color.surface.base", 3.0, 3.0),
    ("color.border.strong", "color.surface.sunken", 3.0, 3.0),
    // A Checkbox on a hovered or selected row (Properties, Filters tab; ADR-0062).
    ("color.border.strong", "color.surface.overlay", 3.0, 3.0),
    ("color.border.strong", "color.accent.subtle", 3.0, 3.0),
    ("color.accent", "color.accent.subtle", 3.0, 3.0),
    // The warning mark of an unavailable source on a hovered or selected row (ADR-0077).
    ("color.state.warning", "color.surface.overlay", 3.0, 3.0),
    ("color.state.warning", "color.accent.subtle", 3.0, 3.0),
    // The same mark of a ListRow in the searchable picker's well (ADR-0114).
    ("color.state.warning", "color.surface.base", 3.0, 3.0),
    // The brand mark in the RBS menu button, resting (raised) and under the pointer (overlay).
    ("color.brand", "color.surface.raised", 3.0, 3.0),
    ("color.brand", "color.surface.overlay", 3.0, 3.0),
    // The notice card (toast and error bar): text on its fill, its edge and the
    // controls on it, and the edge against the window and the panels it floats over.
    ("color.text.primary", "color.toast.surface", 4.5, 7.0),
    ("color.text.secondary", "color.toast.surface", 4.5, 7.0),
    // The outline, the side mark and the notice icon are the edge colour on the fill.
    ("color.toast.edge", "color.toast.surface", 4.5, 7.0),
    // The countdown chip is filled with the edge colour; the seconds and the hourglass
    // are the surface colour, and the seconds are text.
    ("color.toast.surface", "color.toast.edge", 4.5, 7.0),
    ("color.border.focus", "color.toast.surface", 3.0, 3.0),
    ("color.border.strong", "color.toast.surface", 3.0, 3.0),
    ("color.accent", "color.toast.surface", 3.0, 3.0),
    ("color.toast.edge", "color.surface.base", 3.0, 3.0),
    ("color.toast.edge", "color.surface.raised", 3.0, 3.0),
    ("color.toast.edge", "color.surface.overlay", 3.0, 3.0),
    ("color.toast.edge", "color.surface.sunken", 3.0, 3.0),
];

/// Surfaces `color.state.live` must stand out from (3:1). Not `surface.raised` or
/// `.overlay`: the fixed live colour is not a theme's to fix (ADR-0057 review, item 1).
pub const LIVE_SURFACES: &[&str] = &["color.surface.base", "color.surface.sunken"];

/// Smallest CIEDE2000 difference between any theme colour and `color.state.live`, so
/// no other colour reads as "live red" (rule U12, ADR-0057).
pub const MIN_DELTA_E_FROM_LIVE: f64 = 15.0;

/// Hue distance in degrees below which an error colour reads as "live red" (rule U12).
pub const MIN_HUE_DISTANCE_FROM_LIVE: f64 = 45.0;

fn hue(c: Color) -> f64 {
    let (r, g, b) = (f64::from(c.r), f64::from(c.g), f64::from(c.b));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d == 0.0 {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    h * 60.0
}

fn hue_distance(a: Color, b: Color) -> f64 {
    let d = (hue(a) - hue(b)).abs();
    d.min(360.0 - d)
}

impl Tokens {
    /// Parse a token file and run every check.
    pub fn parse(text: &str) -> Result<Tokens, TokenError> {
        let raw: RawTokens = toml::from_str(text).map_err(|e| TokenError(e.to_string()))?;
        let font_family = match (raw.format, raw.font) {
            (1, None) => Vec::new(),
            (1, Some(_)) => return Err(TokenError("[font] needs format 2".into())),
            (2, Some(mut font)) => {
                let family = font
                    .remove(FONT_FAMILY)
                    .ok_or_else(|| TokenError(format!("[font] is missing {FONT_FAMILY}")))?;
                if let Some(extra) = font.keys().next() {
                    return Err(TokenError(format!("[font] has unknown token {extra}")));
                }
                family
            }
            (2, None) => return Err(TokenError("[font] is missing".into())),
            (f, _) => return Err(TokenError(format!("unsupported format {f}"))),
        };
        let fixed_names: Vec<&str> = FIXED.iter().map(|(n, _)| *n).collect();
        let fixed = colors(raw.fixed, &fixed_names, "[fixed]")?;
        for (name, value) in FIXED {
            if Color::parse(value) != Some(fixed[*name]) {
                return Err(TokenError(format!(
                    "{name} is fixed at {value}; a token file cannot change it"
                )));
            }
        }
        let require = |have: Vec<&String>, need: &[&str], what: &str| match need
            .iter()
            .find(|n| !have.iter().any(|h| h == *n))
        {
            Some(n) => Err(TokenError(format!("[{what}] is missing {n}"))),
            None => Ok(()),
        };
        require(raw.length.keys().collect(), LENGTHS, "length")?;
        require(raw.duration.keys().collect(), DURATIONS, "duration")?;
        require(raw.type_styles.keys().collect(), TYPE_STYLES, "type")?;
        let mut raw_themes = raw.themes;
        let mut themes = Vec::new();
        for t in THEMES {
            let c = raw_themes
                .remove(t)
                .ok_or_else(|| TokenError(format!("theme {t} is missing")))?;
            themes.push((
                t.to_owned(),
                colors(c, THEME_COLORS, &format!("theme {t}"))?,
            ));
        }
        if let Some(extra) = raw_themes.keys().next() {
            return Err(TokenError(format!("unknown theme {extra}")));
        }
        let names = raw
            .length
            .keys()
            .chain(raw.duration.keys())
            .chain(raw.type_styles.keys());
        for n in names {
            if !valid_name(n) {
                return Err(TokenError(format!("invalid token name {n:?}")));
            }
        }
        if raw.length.values().any(|v| !v.is_finite() || *v < 0.0) {
            return Err(TokenError("lengths must be non-negative numbers".into()));
        }
        if raw
            .type_styles
            .values()
            .any(|t| t.size == 0 || t.line < t.size || !(100..=900).contains(&t.weight))
        {
            return Err(TokenError(
                "text styles need a size, a line height at least the size, and a weight of 100 to 900"
                    .into(),
            ));
        }
        for (name, min, rule) in MINIMUMS {
            if raw.length.get(*name).is_some_and(|v| v < min) {
                return Err(TokenError(format!(
                    "{name} must be at least {min} ({rule})"
                )));
            }
        }
        let tokens = Tokens {
            font_family,
            fixed,
            themes,
            length: raw.length,
            duration: raw.duration,
            type_styles: raw.type_styles,
        };
        tokens.check_contrast()?;
        Ok(tokens)
    }

    /// The tokens shipped with RBS.
    pub fn built_in() -> Result<Tokens, TokenError> {
        Tokens::parse(BUILT_IN)
    }

    fn check_contrast(&self) -> Result<(), TokenError> {
        let live = self.fixed["color.state.live"];
        let on_live = self.fixed["color.text.on-live"];
        if live.contrast(on_live) < 4.5 {
            return Err(TokenError(format!(
                "color.text.on-live on color.state.live has contrast {:.2}, needs 4.5",
                live.contrast(on_live)
            )));
        }
        let close = self.fixed["color.caption.close-hover"];
        let close_text = self.fixed["color.caption.close-text"];
        if close.contrast(close_text) < 4.5 {
            return Err(TokenError(format!(
                "color.caption.close-text on color.caption.close-hover has contrast {:.2}, needs 4.5",
                close.contrast(close_text)
            )));
        }
        for (theme, colors) in &self.themes {
            let look = Look {
                colors,
                live,
                high_contrast: theme == "high-contrast",
                length: &self.length,
                duration: &self.duration,
                type_styles: &self.type_styles,
                font_family: &self.font_family,
            };
            if let Some(p) = look.check().into_iter().next() {
                return Err(TokenError(format!("theme {theme}: {}", p.reason)));
            }
        }
        Ok(())
    }
}

/// How tightly spacing and controls are packed. Regular is the token file as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    /// The lengths in the token file.
    #[default]
    Regular,
    /// Spacing, control size and the panel gaps step down. The smallest step is
    /// unchanged. No new pixel value is introduced except a token's floor.
    Compact,
    /// The default control takes the large control height and the three panel gaps
    /// take the next larger spacing step (ADR-0112 decision 9). `space.*`,
    /// `size.control.sm` and `size.control.lg` are not touched.
    Roomy,
}

/// The tightest compact stop. Stop 1 is the next smaller step.
pub const COMPACT_STEPS_MAX: u8 = 3;

const SPACE_STEPS: &[&str] = &[
    "space.1", "space.2", "space.3", "space.4", "space.6", "space.8",
];
const CONTROL_STEPS: &[&str] = &["size.control.sm", "size.control.md", "size.control.lg"];
const PANEL_STEPS: &[&str] = &["size.panel.pad", "size.section.gap", "size.field.gap"];

/// Applies `density` to a freshly resolved length map. `steps` is 1, 2 or 3 while
/// compact (other values are clamped). Compact reads that map and steps it down;
/// calling it again on the result steps it down again, so the caller resolves first.
pub fn apply_density(
    length: &mut std::collections::BTreeMap<String, f32>,
    density: Density,
    steps: u8,
) {
    if density == Density::Roomy {
        apply_roomy(length);
        return;
    }
    if density != Density::Compact {
        return;
    }
    let steps = steps.clamp(1, COMPACT_STEPS_MAX);
    let original = length.clone();
    for _ in 0..steps {
        step_down(length, SPACE_STEPS);
        step_down(length, CONTROL_STEPS);
    }
    for key in PANEL_STEPS {
        let Some(floor) = floor_of(key) else {
            continue;
        };
        let Some(value) = original.get(*key).copied() else {
            continue;
        };
        length.insert(
            (*key).to_owned(),
            nth_smaller_space(&original, value, floor, steps),
        );
    }
    if let (Some(field), Some(section)) = (
        length.get("size.field.gap").copied(),
        length.get("size.section.gap").copied(),
    ) && field > section
    {
        length.insert("size.field.gap".to_owned(), section);
    }
}

/// Roomy (ADR-0112 decision 9): `size.control.md` takes the resolved `size.control.lg`,
/// and each panel gap takes the next larger resolved `space.*` value (unchanged when
/// none is larger). Larger values keep every "must not be smaller than" rule.
fn apply_roomy(length: &mut std::collections::BTreeMap<String, f32>) {
    if let Some(lg) = length.get("size.control.lg").copied() {
        length.insert("size.control.md".to_owned(), lg);
    }
    let mut spaces: Vec<f32> = SPACE_STEPS
        .iter()
        .filter_map(|key| length.get(*key).copied())
        .collect();
    spaces.sort_by(f32::total_cmp);
    for key in PANEL_STEPS {
        if let Some(value) = length.get(*key).copied()
            && let Some(larger) = spaces.iter().find(|step| **step > value)
        {
            length.insert((*key).to_owned(), *larger);
        }
    }
}

fn floor_of(name: &str) -> Option<f32> {
    MINIMUMS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, floor, _)| *floor)
}

/// The `steps`th smaller original `space.*` value that is still at or above `floor`.
/// The tightest such value when fewer remain. The floor when none remain and it is
/// below `value`.
fn nth_smaller_space(
    original: &std::collections::BTreeMap<String, f32>,
    value: f32,
    floor: f32,
    steps: u8,
) -> f32 {
    let mut smaller: Vec<f32> = SPACE_STEPS
        .iter()
        .filter_map(|key| original.get(*key).copied())
        .filter(|step| *step < value && *step >= floor)
        .collect();
    smaller.sort_by(|a, b| b.total_cmp(a));
    smaller.dedup_by(|a, b| a.total_cmp(b).is_eq());
    if let Some(found) = smaller
        .get(usize::from(steps) - 1)
        .or_else(|| smaller.last())
    {
        *found
    } else if floor < value {
        floor
    } else {
        value
    }
}

fn step_down(length: &mut std::collections::BTreeMap<String, f32>, keys: &[&str]) {
    let current: Vec<Option<f32>> = keys.iter().map(|k| length.get(*k).copied()).collect();
    for (i, key) in keys.iter().enumerate().skip(1) {
        if let Some(smaller) = current[i - 1] {
            length.insert((*key).to_owned(), smaller);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn roomy_enlarges_the_default_control_and_the_panel_gaps_only() {
        let file = Tokens::built_in().expect("built-in tokens");
        let mut length = file.length.clone();
        apply_density(&mut length, Density::Roomy, 1);
        assert_eq!(length["size.control.md"], file.length["size.control.lg"]);
        assert_eq!(length["size.panel.pad"], 16.0);
        assert_eq!(length["size.section.gap"], 24.0);
        assert_eq!(length["size.field.gap"], 12.0);
        for key in SPACE_STEPS
            .iter()
            .chain(["size.control.sm", "size.control.lg"].iter())
        {
            assert_eq!(length[*key], file.length[*key], "{key} must not change");
        }
        let changed: Vec<&String> = length
            .keys()
            .filter(|k| length[*k] != file.length[*k])
            .collect();
        assert_eq!(
            changed,
            [
                "size.control.md",
                "size.field.gap",
                "size.panel.pad",
                "size.section.gap"
            ]
        );
        // A gap already at the largest step stays there.
        let mut top = file.length.clone();
        top.insert("size.panel.pad".to_owned(), 32.0);
        apply_density(&mut top, Density::Roomy, 1);
        assert_eq!(top["size.panel.pad"], 32.0);
    }

    #[test]
    fn compact_uses_the_next_smaller_spacing_and_control_size() {
        let file = Tokens::built_in().expect("built-in tokens");
        let regular_space = file.length["space.4"];
        let smaller_space = file.length["space.3"];
        let smaller_control = file.length["size.control.md"];
        let smallest = file.length["space.1"];
        let mut length = file.length.clone();
        apply_density(&mut length, Density::Regular, 3);
        assert_eq!(length["space.4"], regular_space);
        apply_density(&mut length, Density::Compact, 1);
        assert_eq!(length["space.4"], smaller_space);
        assert_eq!(length["space.1"], smallest);
        assert_eq!(length["size.control.lg"], smaller_control);
        assert_eq!(length["size.control.sm"], file.length["size.control.sm"]);
        assert_eq!(length["size.panel.pad"], file.length["space.2"]);
        assert_eq!(length["size.section.gap"], file.length["space.3"]);
        assert_eq!(length["size.field.gap"], 6.0);
        assert_eq!(length["radius.md"], file.length["radius.md"]);

        let mut stop3 = file.length.clone();
        apply_density(&mut stop3, Density::Compact, 3);
        assert_eq!(stop3["space.4"], file.length["space.1"]);
        assert_eq!(stop3["space.6"], file.length["space.2"]);
        assert_eq!(stop3["space.8"], file.length["space.3"]);
        assert_eq!(stop3["size.control.lg"], file.length["size.control.sm"]);
        assert_eq!(stop3["size.control.md"], file.length["size.control.sm"]);
        assert_eq!(stop3["size.panel.pad"], 8.0);
        assert_eq!(stop3["size.section.gap"], 12.0);
        assert!(stop3["size.field.gap"] <= stop3["size.section.gap"]);

        let mut again = file.length.clone();
        apply_density(&mut again, Density::Compact, 2);
        let mut matched = file.length.clone();
        apply_density(&mut matched, Density::Compact, 2);
        assert_eq!(again["space.4"], matched["space.4"]);
        assert_eq!(again["space.4"], file.length["space.2"]);
    }

    #[test]
    fn compact_keeps_field_gap_at_or_below_section_gap() {
        let file = Tokens::built_in().expect("built-in tokens");
        let mut length = file.length.clone();
        length.insert("size.field.gap".into(), 18.0);
        length.insert("size.section.gap".into(), 16.0);
        apply_density(&mut length, Density::Compact, 1);
        assert!(length["size.field.gap"] <= length["size.section.gap"]);
        assert_eq!(length["size.section.gap"], 12.0);
        assert_eq!(length["size.field.gap"], 12.0);
    }

    #[test]
    fn built_in_tokens_pass_every_check() {
        let t = Tokens::built_in().unwrap();
        assert_eq!(t.themes.len(), 3);
        assert_eq!(t.length["size.control.md"], 28.0);
        assert_eq!(t.type_styles["type.body"].size, 13);
    }

    #[test]
    fn contrast_math() {
        let black = Color::parse("#000000").unwrap();
        let white = Color::parse("#ffffff").unwrap();
        assert!((black.contrast(white) - 21.0).abs() < 1e-9);
        assert_eq!(Color::parse("#12345678").map(|c| c.a), Some(0x78));
        assert_eq!(Color::parse("#1234567").map(|c| c.a), None);
        assert_eq!(Color::parse("#c4262e").unwrap().to_string(), "#c4262e");
        assert!(
            hue_distance(
                Color::parse("#ff0000").unwrap(),
                Color::parse("#ff00ff").unwrap()
            ) == 60.0
        );
    }

    #[test]
    fn checks_fail_loudly() {
        // Git may check the file out with CRLF line ends (Windows).
        let base = BUILT_IN.replace("\r\n", "\n");
        let low_contrast = base.replacen(
            "\"color.text.secondary\" = \"#a8afbb\"",
            "\"color.text.secondary\" = \"#30343a\"",
            1,
        );
        let e = Tokens::parse(&low_contrast).unwrap_err().0;
        assert!(e.contains("color.text.secondary"), "{e}");

        let red_error = base.replacen(
            "\"color.state.error\" = \"#c38bf0\"",
            "\"color.state.error\" = \"#ff5555\"",
            1,
        );
        assert!(Tokens::parse(&red_error).unwrap_err().0.contains("hue"));

        let live_in_theme = base.replacen(
            "[themes.light]\n",
            "[themes.light]\n\"color.state.live\" = \"#00ff00\"\n",
            1,
        );
        assert!(
            Tokens::parse(&live_in_theme)
                .unwrap_err()
                .0
                .contains("unknown colour")
        );

        let other_live = base.replacen("#c4262e", "#ff0000", 1);
        assert!(Tokens::parse(&other_live).unwrap_err().0.contains("fixed"));

        let alpha = base.replacen("\"#0b0b0d\"", "\"#0b0b0d80\"", 1);
        assert!(Tokens::parse(&alpha).unwrap_err().0.contains("#rrggbb"));

        let thin_ring = base.replacen("\"size.focus-ring\" = 2", "\"size.focus-ring\" = 0", 1);
        assert!(Tokens::parse(&thin_ring).unwrap_err().0.contains("U7"));
        for (name, min, _) in MINIMUMS {
            if let Some(t) = crate::tokens::themeable(name)
                && let crate::tokens::theme::Kind::Length { min: bound, .. } = &t.kind
            {
                assert_eq!(bound, min, "{name}: editor bound and MINIMUMS floor differ");
            }
        }
        // ADR-0076: the panel spacing tokens keep a floor, whatever space.* says.
        for (from, to) in [
            ("\"size.panel.pad\" = 12", "\"size.panel.pad\" = 2"),
            ("\"size.section.gap\" = 16", "\"size.section.gap\" = 4"),
            ("\"size.field.gap\" = 8", "\"size.field.gap\" = 0"),
        ] {
            let e = Tokens::parse(&base.replacen(from, to, 1)).unwrap_err().0;
            assert!(e.contains("ADR-0076"), "{e}");
        }
        let no_icon = base.replacen("\"size.icon.box\" = 16", "\"size.icon.box\" = 0", 1);
        let e = Tokens::parse(&no_icon).unwrap_err().0;
        assert!(e.contains("size.icon.box") && e.contains("U7"), "{e}");
        let hairline = base.replacen(
            "\"size.icon.stroke\" = 1.5",
            "\"size.icon.stroke\" = 0.5",
            1,
        );
        let e = Tokens::parse(&hairline).unwrap_err().0;
        assert!(e.contains("size.icon.stroke") && e.contains("U7"), "{e}");
        let small = base.replacen(
            "\"size.window.min-w\" = 1280",
            "\"size.window.min-w\" = 800",
            1,
        );
        assert!(Tokens::parse(&small).unwrap_err().0.contains("U11"));

        let no_motion = base.replacen("\"motion.fast\" = 120\n", "", 1);
        assert!(
            Tokens::parse(&no_motion)
                .unwrap_err()
                .0
                .contains("motion.fast")
        );

        let missing = base.replacen("\"color.meter.clip\" = \"#e9a6ff\"\n", "", 1);
        assert!(
            Tokens::parse(&missing)
                .unwrap_err()
                .0
                .contains("missing color.meter.clip")
        );

        let bad_name = base.replacen("\"space.1\" = 4", "\"space.1\" = 4\n\"Space 1\" = 4", 1);
        assert!(
            Tokens::parse(&bad_name)
                .unwrap_err()
                .0
                .contains("invalid token name")
        );
    }
}
