//! Design tokens: parsing and checking `tokens.toml`.
//!
//! Spec: `docs/specs/ui-design-system.md`, "Tokens". Every theme defines the
//! same colour names; live colours are fixed outside the themes (rule U12);
//! text and surface pairs pass the contrast check of rule U7.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

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
/// values. A custom token file must repeat them unchanged (rule U12).
pub const FIXED: &[(&str, &str)] = &[
    ("color.state.live", "#c4262e"),
    ("color.text.on-live", "#ffffff"),
];

/// Length tokens every file defines (logical pixels).
pub const LENGTHS: &[&str] = &[
    "space.1",
    "space.2",
    "space.3",
    "space.4",
    "space.6",
    "space.8",
    "size.control.sm",
    "size.control.md",
    "size.control.lg",
    "radius.sm",
    "radius.md",
    "radius.lg",
];

/// Duration tokens every file defines (milliseconds).
pub const DURATIONS: &[&str] = &["motion.fast", "motion.normal"];

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
    ("color.state.warning", "color.surface.raised", 3.0, 3.0),
    ("color.state.healthy", "color.surface.raised", 3.0, 3.0),
    ("color.state.error", "color.surface.raised", 3.0, 3.0),
    ("color.action.destructive", "color.surface.raised", 3.0, 3.0),
    ("color.meter.low", "color.surface.sunken", 3.0, 3.0),
    ("color.meter.mid", "color.surface.sunken", 3.0, 3.0),
    ("color.meter.high", "color.surface.sunken", 3.0, 3.0),
    ("color.meter.clip", "color.surface.sunken", 3.0, 3.0),
];

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
        if raw.format != 1 {
            return Err(TokenError(format!("unsupported format {}", raw.format)));
        }
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
        let tokens = Tokens {
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
        for (theme, c) in &self.themes {
            let hc = theme == "high-contrast";
            for (fg, bg, min, min_hc) in CONTRAST_RULES {
                let need = if hc { *min_hc } else { *min };
                let ratio = c[*fg].contrast(c[*bg]);
                if ratio < need {
                    return Err(TokenError(format!(
                        "theme {theme}: {fg} on {bg} has contrast {ratio:.2}, needs {need}"
                    )));
                }
            }
            for t in [
                "color.state.error",
                "color.action.destructive",
                "color.meter.clip",
            ] {
                if hue_distance(c[t], live) < MIN_HUE_DISTANCE_FROM_LIVE {
                    return Err(TokenError(format!(
                        "theme {theme}: {t} is too close in hue to color.state.live"
                    )));
                }
            }
            if live.contrast(c["color.surface.base"]) < 3.0 {
                return Err(TokenError(format!(
                    "theme {theme}: color.state.live on color.surface.base needs contrast 3"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

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

        let alpha = base.replacen("\"#121417\"", "\"#12141780\"", 1);
        assert!(Tokens::parse(&alpha).unwrap_err().0.contains("#rrggbb"));

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
