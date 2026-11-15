//! Theme effects (ADR-0120): how lively a theme's controls look, Calm, Vivid or Tactile.
//!
//! The token file (format 3) holds one table of effect values per lively setting;
//! calm is all zero. A theme only chooses its setting. This module parses the tables,
//! checks their bounds and the extent rule, resolves a theme's setting (high contrast
//! is always calm, a failed effect check drops to calm with a notice) and computes what
//! a slot draws: values scaled into the slot's extent and the derived colours.

use std::collections::BTreeMap;

use super::{Color, MIN_DELTA_E_FROM_LIVE, TokenError, delta_e_2000};

/// The most an effect may reach past a control's box, in logical pixels (ADR-0120
/// decision 6). A constant, not a token.
pub const EFFECT_EXTENT_MAX: f32 = 4.0;

/// The theme key that holds the setting.
pub const EFFECTS_KEY: &str = "effects";

/// How lively a theme's controls look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub enum Effects {
    /// Flat (the Tint look, ADR-0118): no shadow, glow, sheen, edge or press.
    #[default]
    Calm,
    /// Soft shadows, a glow under the main action and a sheen.
    Vivid,
    /// A darker edge under each button and a press.
    Tactile,
}

impl Effects {
    /// Every setting, in the editor's order.
    pub const ALL: [Effects; 3] = [Effects::Calm, Effects::Vivid, Effects::Tactile];

    /// The settings with a table in the token file.
    pub const LIVELY: [Effects; 2] = [Effects::Vivid, Effects::Tactile];

    /// The name in a theme document and the token file.
    pub fn name(self) -> &'static str {
        match self {
            Effects::Calm => "calm",
            Effects::Vivid => "vivid",
            Effects::Tactile => "tactile",
        }
    }

    /// The setting a name stands for.
    pub fn from_name(name: &str) -> Option<Effects> {
        Effects::ALL.into_iter().find(|e| e.name() == name)
    }

    /// The position in [`Effects::ALL`].
    pub fn index(self) -> usize {
        match self {
            Effects::Calm => 0,
            Effects::Vivid => 1,
            Effects::Tactile => 2,
        }
    }
}

/// The value kind of an effect token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    /// Logical pixels.
    Length,
    /// A fraction from 0 to 1 (a TOML float).
    Fraction,
}

/// The effect tokens every lively table holds: name, kind and inclusive bounds.
pub const EFFECT_TOKENS: &[(&str, EffectKind, f32, f32)] = &[
    ("effects.shadow.blur", EffectKind::Length, 0.0, 4.0),
    ("effects.shadow.offset", EffectKind::Length, 0.0, 2.0),
    ("effects.shadow.alpha.dark", EffectKind::Fraction, 0.0, 0.5),
    ("effects.shadow.alpha.light", EffectKind::Fraction, 0.0, 0.5),
    ("effects.glow.blur", EffectKind::Length, 0.0, 4.0),
    ("effects.glow.alpha", EffectKind::Fraction, 0.0, 0.5),
    ("effects.sheen.alpha", EffectKind::Fraction, 0.0, 0.3),
    ("effects.sheen.band", EffectKind::Length, 0.0, 4.0),
    ("effects.key.edge", EffectKind::Length, 0.0, 2.0),
    ("effects.key.shade", EffectKind::Fraction, 0.0, 0.4),
    ("effects.press", EffectKind::Length, 0.0, 2.0),
];

/// One setting's effect values; all zero is calm.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct EffectValues {
    /// Blur radius of the drop shadow.
    pub shadow_blur: f32,
    /// Downward offset of the drop shadow.
    pub shadow_offset: f32,
    /// Opacity of the black shadow on a dark base.
    pub shadow_alpha_dark: f32,
    /// Opacity of the black shadow on a light base.
    pub shadow_alpha_light: f32,
    /// Blur radius of the glow.
    pub glow_blur: f32,
    /// Opacity of the glow.
    pub glow_alpha: f32,
    /// Opacity of the white sheen.
    pub sheen_alpha: f32,
    /// Height of the sheen band at the top of the face, before its clamp.
    pub sheen_band: f32,
    /// Height of the darker edge under the face.
    pub key_edge: f32,
    /// How far the edge colour is mixed toward black, in encoded sRGB.
    pub key_shade: f32,
    /// How far the face moves down while pressed.
    pub press: f32,
}

impl EffectValues {
    fn get_mut(&mut self, name: &str) -> Option<&mut f32> {
        Some(match name {
            "effects.shadow.blur" => &mut self.shadow_blur,
            "effects.shadow.offset" => &mut self.shadow_offset,
            "effects.shadow.alpha.dark" => &mut self.shadow_alpha_dark,
            "effects.shadow.alpha.light" => &mut self.shadow_alpha_light,
            "effects.glow.blur" => &mut self.glow_blur,
            "effects.glow.alpha" => &mut self.glow_alpha,
            "effects.sheen.alpha" => &mut self.sheen_alpha,
            "effects.sheen.band" => &mut self.sheen_band,
            "effects.key.edge" => &mut self.key_edge,
            "effects.key.shade" => &mut self.key_shade,
            "effects.press" => &mut self.press,
            _ => return None,
        })
    }

    /// How far the values reach below the box: the key edge, then the farther of the
    /// shadow and the glow from there (ADR-0120 decision 6).
    pub fn reach(&self) -> f32 {
        self.key_edge + (self.shadow_blur + self.shadow_offset).max(self.glow_blur)
    }

    /// True when nothing is drawn.
    pub fn is_calm(&self) -> bool {
        *self == EffectValues::default()
    }

    /// Reads one `[effects.<setting>]` table and checks its bounds, `press ≤ key.edge`
    /// and the extent rule.
    pub fn parse(setting: &str, raw: &BTreeMap<String, f64>) -> Result<EffectValues, TokenError> {
        let mut v = EffectValues::default();
        for (name, _, _, _) in EFFECT_TOKENS {
            if !raw.contains_key(*name) {
                return Err(TokenError(format!("[effects.{setting}] is missing {name}")));
            }
        }
        for (name, value) in raw {
            let Some((_, _, min, max)) = EFFECT_TOKENS.iter().find(|(n, ..)| n == name) else {
                return Err(TokenError(format!(
                    "[effects.{setting}] has unknown token {name}"
                )));
            };
            let value = *value as f32;
            if !value.is_finite() || !(*min..=*max).contains(&value) {
                return Err(TokenError(format!(
                    "[effects.{setting}] {name} must be from {min} to {max}, not {value}"
                )));
            }
            if let Some(slot) = v.get_mut(name) {
                *slot = value;
            }
        }
        if v.press > v.key_edge {
            return Err(TokenError(format!(
                "[effects.{setting}] effects.press must not be more than effects.key.edge"
            )));
        }
        if v.reach() > EFFECT_EXTENT_MAX {
            return Err(TokenError(format!(
                "[effects.{setting}] effects.key.edge + max(effects.shadow.blur + effects.shadow.offset, effects.glow.blur) is {}, more than {EFFECT_EXTENT_MAX}",
                v.reach()
            )));
        }
        Ok(v)
    }
}

/// The effect values of the lively settings in a token file.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct EffectTables {
    /// `[effects.vivid]`.
    pub vivid: EffectValues,
    /// `[effects.tactile]`.
    pub tactile: EffectValues,
}

impl EffectTables {
    /// The values of `setting`; calm is all zero.
    pub fn values(&self, setting: Effects) -> EffectValues {
        match setting {
            Effects::Calm => EffectValues::default(),
            Effects::Vivid => self.vivid,
            Effects::Tactile => self.tactile,
        }
    }

    /// Reads the `[effects]` table of a format 3 file: exactly the lively settings.
    pub fn parse(raw: BTreeMap<String, BTreeMap<String, f64>>) -> Result<Self, TokenError> {
        let mut out = EffectTables::default();
        for e in Effects::LIVELY {
            let table = raw
                .get(e.name())
                .ok_or_else(|| TokenError(format!("[effects.{}] is missing", e.name())))?;
            let values = EffectValues::parse(e.name(), table)?;
            match e {
                Effects::Vivid => out.vivid = values,
                _ => out.tactile = values,
            }
        }
        if let Some(extra) = raw
            .keys()
            .find(|k| !Effects::LIVELY.iter().any(|e| e.name() == k.as_str()))
        {
            return Err(TokenError(format!("unknown effects table {extra}")));
        }
        Ok(out)
    }
}

/// The resting fill of each kind of button that takes a key edge, as theme colour
/// names: primary, secondary (and framed ghost), danger, checked.
pub const KEY_FILLS: [&str; 4] = [
    "color.accent",
    "color.button.fill",
    "color.action.destructive.subtle",
    "color.accent.subtle",
];

/// `fill` mixed toward black by `shade` (0 none, 1 black), in encoded sRGB channels.
pub fn key_colour(fill: Color, shade: f32) -> Color {
    let keep = 1.0 - shade.clamp(0.0, 1.0);
    let ch = |c: u8| (f32::from(c) * keep).round().clamp(0.0, 255.0) as u8;
    Color {
        r: ch(fill.r),
        g: ch(fill.g),
        b: ch(fill.b),
        a: 255,
    }
}

/// `c` at opacity `alpha` (0 to 1).
pub fn with_alpha(c: Color, alpha: f32) -> Color {
    Color {
        a: (alpha.clamp(0.0, 1.0) * 255.0).round() as u8,
        ..c
    }
}

const BLACK: Color = Color {
    r: 0,
    g: 0,
    b: 0,
    a: 255,
};
const WHITE: Color = Color {
    r: 255,
    g: 255,
    b: 255,
    a: 255,
};

/// The opacity a blurred shadow still has at `distance` past its box, as the checks
/// model it: full at the offset, falling linearly to none at offset + blur. Linear
/// overstates a Gaussian's tail, so the check errs on the safe side.
pub fn residual(alpha: f32, offset: f32, blur: f32, distance: f32) -> f32 {
    if blur <= 0.0 {
        return if distance < offset { alpha } else { 0.0 };
    }
    alpha * ((offset + blur - distance) / blur).clamp(0.0, 1.0)
}

/// `top` at opacity `alpha` over the opaque `ground`.
pub fn over(top: Color, alpha: f32, ground: Color) -> Color {
    let a = alpha.clamp(0.0, 1.0);
    let ch = |t: u8, g: u8| (f32::from(t) * a + f32::from(g) * (1.0 - a)).round() as u8;
    Color {
        r: ch(top.r, ground.r),
        g: ch(top.g, ground.g),
        b: ch(top.b, ground.b),
        a: 255,
    }
}

/// What a slot draws: the setting's values scaled into the slot's extent, and the
/// derived colours (ADR-0120 decisions 7 and 11). All zero and `on: false` for calm.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct EffectDraw {
    /// Any effect is drawn.
    pub on: bool,
    /// Shadow blur radius.
    pub shadow_blur: f32,
    /// Shadow offset downward.
    pub shadow_offset: f32,
    /// Shadow colour: black at the base's alpha.
    pub shadow: Color,
    /// Glow blur radius.
    pub glow_blur: f32,
    /// Glow colour: the primary rest fill at the glow alpha.
    pub glow: Color,
    /// Sheen colour: white at the sheen alpha.
    pub sheen: Color,
    /// Sheen band height, clamped clear of all ink.
    pub sheen_band: f32,
    /// Key edge height.
    pub key_edge: f32,
    /// Press offset.
    pub press: f32,
    /// Key edge colours, per [`KEY_FILLS`].
    pub key: [Color; 4],
    /// The slot's extent.
    pub extent: f32,
}

/// The slot's extent: `max(0, min(EFFECT_EXTENT_MAX, space.1, space.2 − size.focus-ring))`
/// on the lengths after density (ADR-0120 decision 11).
pub fn extent(length: &BTreeMap<String, f32>) -> f32 {
    let get = |n: &str| length.get(n).copied().unwrap_or(0.0);
    EFFECT_EXTENT_MAX
        .min(get("space.1"))
        .min(get("space.2") - get("size.focus-ring"))
        .max(0.0)
}

/// The sheen band clamp: `max(0, (size.control.sm − max(size.button.chip,
/// size.icon.box, type.label line)) / 2)`, so the band never reaches ink.
pub fn sheen_clamp(length: &BTreeMap<String, f32>, label_line: f32) -> f32 {
    let get = |n: &str| length.get(n).copied().unwrap_or(0.0);
    let ink = get("size.button.chip")
        .max(get("size.icon.box"))
        .max(label_line);
    ((get("size.control.sm") - ink) / 2.0).max(0.0)
}

/// What `values` draw on a slot with `colors` (a resolved theme's), `length` (after
/// density) and `label_line` (the label style's line height); `dark` picks the
/// shadow alpha.
pub fn draw(
    values: &EffectValues,
    colors: &BTreeMap<String, Color>,
    length: &BTreeMap<String, f32>,
    label_line: f32,
    dark: bool,
) -> EffectDraw {
    if values.is_calm() {
        return EffectDraw::default();
    }
    let extent = extent(length);
    let reach = values.reach();
    let scale = if reach > extent && reach > 0.0 {
        extent / reach
    } else {
        1.0
    };
    let colour = |n: &str| colors.get(n).copied().unwrap_or(BLACK);
    let alpha = if dark {
        values.shadow_alpha_dark
    } else {
        values.shadow_alpha_light
    };
    EffectDraw {
        on: true,
        shadow_blur: values.shadow_blur * scale,
        shadow_offset: values.shadow_offset * scale,
        shadow: with_alpha(BLACK, alpha),
        glow_blur: values.glow_blur * scale,
        glow: with_alpha(colour("color.accent"), values.glow_alpha),
        sheen: with_alpha(WHITE, values.sheen_alpha),
        sheen_band: values
            .sheen_band
            .min(sheen_clamp(length, label_line))
            .max(0.0),
        key_edge: values.key_edge * scale,
        press: values.press * scale,
        key: KEY_FILLS.map(|f| key_colour(colour(f), values.key_shade)),
        extent,
    }
}

/// The effect checks of ADR-0120 decision 9 on `values` for a theme with `colors`:
/// `None` when they pass, or the failed check in words.
pub fn check(
    values: &EffectValues,
    colors: &BTreeMap<String, Color>,
    length: &BTreeMap<String, f32>,
    label_line: f32,
    dark: bool,
    live: Color,
) -> Option<String> {
    if values.is_calm() {
        return None;
    }
    let d = draw(values, colors, length, label_line, dark);
    // Derived colours against the live colour: no edge or glow reads as "live red".
    if d.key_edge > 0.0 {
        for (fill, key) in KEY_FILLS.iter().zip(d.key) {
            let diff = delta_e_2000(key, live);
            if diff < MIN_DELTA_E_FROM_LIVE {
                return Some(format!(
                    "the darker edge under {fill} looks like the live colour (difference {diff:.1}, needs {MIN_DELTA_E_FROM_LIVE})"
                ));
            }
        }
    }
    if d.glow_blur > 0.0 && d.glow.a > 0 {
        let glow = Color { a: 255, ..d.glow };
        let diff = delta_e_2000(glow, live);
        if diff < MIN_DELTA_E_FROM_LIVE {
            return Some(format!(
                "the glow looks like the live colour (difference {diff:.1}, needs {MIN_DELTA_E_FROM_LIVE})"
            ));
        }
    }
    // The clamp: a neighbour's strongest shadow or glow at the outer edge of the extent,
    // blended into the toolbar ground, must keep the focus ring at 3:1.
    if let (Some(focus), Some(ground)) = (
        colors.get("color.border.focus"),
        colors.get("color.surface.base"),
    ) {
        let shadow = residual(
            f32::from(d.shadow.a) / 255.0,
            d.shadow_offset,
            d.shadow_blur,
            d.extent,
        );
        let glow = residual(f32::from(d.glow.a) / 255.0, 0.0, d.glow_blur, d.extent);
        let mut grounds = vec![*ground];
        if shadow > 0.0 {
            grounds.push(over(BLACK, shadow, *ground));
        }
        if glow > 0.0 {
            grounds.push(over(Color { a: 255, ..d.glow }, glow, *ground));
        }
        for g in grounds {
            let ratio = focus.contrast(g);
            if ratio < 3.0 {
                return Some(format!(
                    "the focus ring beside an effect has contrast {ratio:.2}, needs 3"
                ));
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    fn table(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        let mut t: BTreeMap<String, f64> = EFFECT_TOKENS
            .iter()
            .map(|(n, ..)| ((*n).to_owned(), 0.0))
            .collect();
        for (n, v) in pairs {
            t.insert((*n).to_owned(), *v);
        }
        t
    }

    #[test]
    fn names_round_trip() {
        for e in Effects::ALL {
            assert_eq!(Effects::from_name(e.name()), Some(e));
            assert_eq!(Effects::ALL[e.index()], e);
        }
        assert_eq!(Effects::from_name("glossy"), None);
    }

    #[test]
    fn bounds_press_and_extent_are_checked() {
        assert!(EffectValues::parse("vivid", &table(&[])).unwrap().is_calm());
        assert!(EffectValues::parse("vivid", &table(&[("effects.shadow.blur", 4.5)])).is_err());
        assert!(EffectValues::parse("vivid", &table(&[("effects.sheen.alpha", -0.1)])).is_err());
        assert!(EffectValues::parse("vivid", &table(&[("effects.press", 1.0)])).is_err());
        // 2 + max(1 + 2, 0) = 5 > 4.
        let wide = [
            ("effects.key.edge", 2.0),
            ("effects.shadow.blur", 2.0),
            ("effects.shadow.offset", 1.0),
        ];
        assert!(EffectValues::parse("vivid", &table(&wide)).is_err());
        let fits = [
            ("effects.key.edge", 2.0),
            ("effects.press", 2.0),
            ("effects.shadow.blur", 1.0),
            ("effects.shadow.offset", 1.0),
            ("effects.glow.blur", 2.0),
        ];
        assert_eq!(
            EffectValues::parse("vivid", &table(&fits)).unwrap().reach(),
            4.0
        );
        let mut extra = table(&[]);
        extra.insert("effects.bevel".into(), 1.0);
        assert!(EffectValues::parse("vivid", &extra).is_err());
        let mut missing = table(&[]);
        missing.remove("effects.press");
        assert!(EffectValues::parse("vivid", &missing).is_err());
    }

    #[test]
    fn values_scale_into_a_smaller_extent_and_keep_press_within_the_edge() {
        let v = EffectValues {
            key_edge: 2.0,
            press: 2.0,
            shadow_blur: 1.0,
            shadow_offset: 1.0,
            ..EffectValues::default()
        };
        let mut length = BTreeMap::new();
        for (n, x) in [
            ("space.1", 4.0),
            ("space.2", 4.0),
            ("size.focus-ring", 2.0),
            ("size.control.sm", 24.0),
            ("size.button.chip", 20.0),
            ("size.icon.box", 16.0),
        ] {
            length.insert(n.to_owned(), x);
        }
        let d = draw(&v, &BTreeMap::new(), &length, 20.0, true);
        assert_eq!(d.extent, 2.0);
        assert!(d.key_edge + (d.shadow_blur + d.shadow_offset).max(d.glow_blur) <= d.extent);
        assert!(d.press <= d.key_edge);
        assert_eq!(d.sheen_band, 0.0);
        length.insert("space.1".into(), 0.0);
        let d = draw(&v, &BTreeMap::new(), &length, 20.0, true);
        assert_eq!(
            (d.extent, d.key_edge, d.press, d.shadow_blur),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn the_sheen_band_is_clamped_clear_of_ink() {
        let mut length = BTreeMap::new();
        for (n, x) in [
            ("size.control.sm", 24.0),
            ("size.button.chip", 20.0),
            ("size.icon.box", 16.0),
        ] {
            length.insert(n.to_owned(), x);
        }
        assert_eq!(sheen_clamp(&length, 20.0), 2.0);
        assert_eq!(sheen_clamp(&length, 30.0), 0.0);
    }

    #[test]
    fn residual_falls_to_nothing_at_the_reach() {
        assert_eq!(residual(0.4, 1.0, 3.0, 4.0), 0.0);
        assert!(residual(0.4, 1.0, 3.0, 2.0) > 0.0);
        assert_eq!(residual(0.4, 0.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn key_colour_mixes_toward_black() {
        let c = Color::parse("#646464").unwrap();
        assert_eq!(key_colour(c, 0.0), c);
        assert_eq!(key_colour(c, 0.4).to_string(), "#3c3c3c");
    }
}
