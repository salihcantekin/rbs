//! Theme effects (ADR-0120, ADR-0122): how lively a theme's controls look: Calm, Glow,
//! Glass or Tactile.
//!
//! The token file (format 4) holds one table of effect values per lively setting;
//! calm is all zero. A theme only chooses its setting. This module parses the tables,
//! checks their bounds and rules, and computes what a slot draws: values scaled into the
//! slot's extent, the per-kind face shading that the theme's contrast rules leave room
//! for, and every derived colour (ADR-0122 decision 4: Slint mixes nothing).

use std::collections::BTreeMap;

use super::{
    CONTRAST_RULES, Color, MIN_DELTA_E_FROM_LIVE, THEME_CONTRAST_RULES, TokenError, delta_e_2000,
};

/// The most an effect other than the halo may reach past a control's box, in logical
/// pixels (ADR-0120 decision 6). A constant, not a token.
pub const EFFECT_EXTENT_MAX: f32 = 4.0;

/// The theme key that holds the setting.
pub const EFFECTS_KEY: &str = "effects";

/// The name ADR-0120 gave Glow. A theme that stores it keeps it (ADR-0122 decision 1).
pub const VIVID_ALIAS: &str = "vivid";

/// The margin every checked ratio keeps, for Skia's rounding between gradient stops
/// (ADR-0122 decision 4).
pub const FACE_MARGIN: f64 = 0.05;

/// The lowest the gloss stop search may move the stop, as a fraction of the height
/// (ADR-0122 decision 5).
pub const GLOSS_STOP_MAX: f32 = 0.45;

/// Smallest CIEDE2000 difference between a rim and `color.accent`, so a rim never reads
/// as a checked button's outline (ADR-0122 decision 7).
pub const MIN_DELTA_E_RIM_FROM_ACCENT: f64 = 10.0;

/// How lively a theme's controls look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub enum Effects {
    /// Flat (the Tint look, ADR-0118): nothing is added.
    #[default]
    Calm,
    /// An outward glow (the halo) on the main action where the layout has room, and a
    /// coloured rim on the other buttons.
    Glow,
    /// A gloss at the top of each face and shading at its bottom, with a soft shadow.
    Glass,
    /// A darker edge under each button and a press.
    Tactile,
}

impl Effects {
    /// Every setting, in the editor's order.
    pub const ALL: [Effects; 4] = [
        Effects::Calm,
        Effects::Glow,
        Effects::Glass,
        Effects::Tactile,
    ];

    /// The settings with a table in the token file.
    pub const LIVELY: [Effects; 3] = [Effects::Glow, Effects::Glass, Effects::Tactile];

    /// The name in a theme document and the token file. Never `"vivid"`.
    pub fn name(self) -> &'static str {
        match self {
            Effects::Calm => "calm",
            Effects::Glow => "glow",
            Effects::Glass => "glass",
            Effects::Tactile => "tactile",
        }
    }

    /// The name the editor shows.
    pub fn label(self) -> &'static str {
        match self {
            Effects::Calm => "Calm",
            Effects::Glow => "Glow",
            Effects::Glass => "Glass",
            Effects::Tactile => "Tactile",
        }
    }

    /// The setting a name stands for. `"vivid"` is Glow (ADR-0122 decision 1).
    pub fn from_name(name: &str) -> Option<Effects> {
        if name == VIVID_ALIAS {
            return Some(Effects::Glow);
        }
        Effects::ALL.into_iter().find(|e| e.name() == name)
    }

    /// The position in [`Effects::ALL`].
    pub fn index(self) -> usize {
        match self {
            Effects::Calm => 0,
            Effects::Glow => 1,
            Effects::Glass => 2,
            Effects::Tactile => 3,
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

/// The effect tokens every lively table holds: name, kind and inclusive bounds
/// (ADR-0122 decision 3).
pub const EFFECT_TOKENS: &[(&str, EffectKind, f32, f32)] = &[
    ("effects.shadow.blur", EffectKind::Length, 0.0, 4.0),
    ("effects.shadow.offset", EffectKind::Length, 0.0, 2.0),
    ("effects.shadow.alpha.dark", EffectKind::Fraction, 0.0, 0.6),
    ("effects.shadow.alpha.light", EffectKind::Fraction, 0.0, 0.6),
    ("effects.halo.blur", EffectKind::Length, 0.0, 16.0),
    ("effects.halo.alpha", EffectKind::Fraction, 0.0, 0.8),
    ("effects.key.edge", EffectKind::Length, 0.0, 3.0),
    ("effects.key.shade", EffectKind::Fraction, 0.0, 0.6),
    ("effects.press", EffectKind::Length, 0.0, 3.0),
    ("effects.gloss", EffectKind::Fraction, 0.0, 0.4),
    ("effects.face.dark", EffectKind::Fraction, 0.0, 0.3),
    ("effects.rim.mix", EffectKind::Fraction, 0.0, 0.8),
    ("effects.inner.blur", EffectKind::Length, 0.0, 8.0),
    ("effects.inner.alpha", EffectKind::Fraction, 0.0, 0.4),
];

/// The tokens of a format 3 table (ADR-0120 decision 6), read only to carry an older
/// file over (ADR-0122 decision 2).
const EFFECT_TOKENS_V3: &[(&str, f32, f32)] = &[
    ("effects.shadow.blur", 0.0, 4.0),
    ("effects.shadow.offset", 0.0, 2.0),
    ("effects.shadow.alpha.dark", 0.0, 0.5),
    ("effects.shadow.alpha.light", 0.0, 0.5),
    ("effects.glow.blur", 0.0, 4.0),
    ("effects.glow.alpha", 0.0, 0.5),
    ("effects.sheen.alpha", 0.0, 0.3),
    ("effects.sheen.band", 0.0, 4.0),
    ("effects.key.edge", 0.0, 2.0),
    ("effects.key.shade", 0.0, 0.4),
    ("effects.press", 0.0, 2.0),
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
    /// Blur radius of the primary's outer glow.
    pub halo_blur: f32,
    /// Opacity of the halo at the box edge.
    pub halo_alpha: f32,
    /// Height of the darker edge under the face.
    pub key_edge: f32,
    /// How far the edge colour is mixed toward black, in encoded sRGB.
    pub key_shade: f32,
    /// How far the face moves down while pressed.
    pub press: f32,
    /// How far the top of a face is mixed toward white.
    pub gloss: f32,
    /// The most the bottom of a face is mixed toward black.
    pub face_dark: f32,
    /// How far the kind's rim colour is mixed into `color.border.subtle`.
    pub rim_mix: f32,
    /// Blur of the inner glow.
    pub inner_blur: f32,
    /// Opacity of the inner glow.
    pub inner_alpha: f32,
}

impl EffectValues {
    fn get_mut(&mut self, name: &str) -> Option<&mut f32> {
        Some(match name {
            "effects.shadow.blur" => &mut self.shadow_blur,
            "effects.shadow.offset" => &mut self.shadow_offset,
            "effects.shadow.alpha.dark" => &mut self.shadow_alpha_dark,
            "effects.shadow.alpha.light" => &mut self.shadow_alpha_light,
            "effects.halo.blur" | "effects.glow.blur" => &mut self.halo_blur,
            "effects.halo.alpha" | "effects.glow.alpha" => &mut self.halo_alpha,
            "effects.key.edge" => &mut self.key_edge,
            "effects.key.shade" => &mut self.key_shade,
            "effects.press" => &mut self.press,
            "effects.gloss" => &mut self.gloss,
            "effects.face.dark" => &mut self.face_dark,
            "effects.rim.mix" => &mut self.rim_mix,
            "effects.inner.blur" => &mut self.inner_blur,
            "effects.inner.alpha" => &mut self.inner_alpha,
            _ => return None,
        })
    }

    /// How far the values reach below the box, the halo aside: the key edge, then the
    /// shadow from there (ADR-0122 decision 3).
    pub fn reach(&self) -> f32 {
        self.key_edge + self.shadow_blur + self.shadow_offset
    }

    /// True when the values draw a halo.
    pub fn has_halo(&self) -> bool {
        self.halo_alpha > 0.0 && self.halo_blur > 0.0
    }

    /// True when nothing is drawn.
    pub fn is_calm(&self) -> bool {
        *self == EffectValues::default()
    }

    /// The rules across tokens (ADR-0122 decisions 3 and 8).
    fn check_rules(&self, setting: &str) -> Result<(), TokenError> {
        if self.halo_alpha > 0.0 && self.key_edge > 0.0 {
            return Err(TokenError(format!(
                "[effects.{setting}] effects.key.edge must be 0 while effects.halo.alpha is above 0"
            )));
        }
        if self.press > self.key_edge {
            return Err(TokenError(format!(
                "[effects.{setting}] effects.press must not be more than effects.key.edge"
            )));
        }
        if self.reach() > EFFECT_EXTENT_MAX {
            return Err(TokenError(format!(
                "[effects.{setting}] effects.key.edge + effects.shadow.offset + effects.shadow.blur is {}, more than {EFFECT_EXTENT_MAX}",
                self.reach()
            )));
        }
        Ok(())
    }

    /// Reads one `[effects.<setting>]` table of a format 4 file and checks its bounds
    /// and rules.
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
        v.check_rules(setting)?;
        Ok(v)
    }

    /// Reads one table of a format 3 file (ADR-0122 decision 2): `glow.*` becomes
    /// `halo.*`, `sheen.*` is dropped and every new token is 0. A table that has both a
    /// glow and a key edge loses the glow, because a halo needs `key.edge` 0.
    fn parse_v3(setting: &str, raw: &BTreeMap<String, f64>) -> Result<EffectValues, TokenError> {
        let mut v = EffectValues::default();
        for (name, _, _) in EFFECT_TOKENS_V3 {
            if !raw.contains_key(*name) {
                return Err(TokenError(format!("[effects.{setting}] is missing {name}")));
            }
        }
        for (name, value) in raw {
            let Some((_, min, max)) = EFFECT_TOKENS_V3.iter().find(|(n, ..)| n == name) else {
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
        if v.key_edge > 0.0 || !v.has_halo() {
            v.halo_blur = 0.0;
            v.halo_alpha = 0.0;
        }
        v.check_rules(setting)?;
        Ok(v)
    }
}

/// The effect values of the lively settings in a token file.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct EffectTables {
    /// `[effects.glow]` (format 3: `[effects.vivid]`).
    pub glow: EffectValues,
    /// `[effects.glass]`; `None` for a format 3 file, which has no Glass.
    pub glass: Option<EffectValues>,
    /// `[effects.tactile]`.
    pub tactile: EffectValues,
}

impl EffectTables {
    /// The values of `setting`; calm is all zero, and so is a setting the file lacks.
    pub fn values(&self, setting: Effects) -> EffectValues {
        match setting {
            Effects::Calm => EffectValues::default(),
            Effects::Glow => self.glow,
            Effects::Glass => self.glass.unwrap_or_default(),
            Effects::Tactile => self.tactile,
        }
    }

    /// True when the file has a table for `setting` (calm needs none).
    pub fn has(&self, setting: Effects) -> bool {
        setting != Effects::Glass || self.glass.is_some()
    }

    /// Reads the `[effects]` table of a format 4 file: exactly the lively settings.
    pub fn parse(raw: BTreeMap<String, BTreeMap<String, f64>>) -> Result<Self, TokenError> {
        let table = |e: Effects| {
            raw.get(e.name())
                .ok_or_else(|| TokenError(format!("[effects.{}] is missing", e.name())))
                .and_then(|t| EffectValues::parse(e.name(), t))
        };
        let out = EffectTables {
            glow: table(Effects::Glow)?,
            glass: Some(table(Effects::Glass)?),
            tactile: table(Effects::Tactile)?,
        };
        if let Some(extra) = raw
            .keys()
            .find(|k| !Effects::LIVELY.iter().any(|e| e.name() == k.as_str()))
        {
            return Err(TokenError(format!("unknown effects table {extra}")));
        }
        Ok(out)
    }

    /// Reads the `[effects]` table of a format 3 file (ADR-0122 decision 2):
    /// `[effects.vivid]` becomes Glow, `[effects.tactile]` stays, and there is no Glass.
    pub fn parse_v3(raw: BTreeMap<String, BTreeMap<String, f64>>) -> Result<Self, TokenError> {
        let table = |name: &str| {
            raw.get(name)
                .ok_or_else(|| TokenError(format!("[effects.{name}] is missing")))
                .and_then(|t| EffectValues::parse_v3(name, t))
        };
        let out = EffectTables {
            glow: table(VIVID_ALIAS)?,
            glass: None,
            tactile: table(Effects::Tactile.name())?,
        };
        if let Some(extra) = raw
            .keys()
            .find(|k| k.as_str() != VIVID_ALIAS && k.as_str() != Effects::Tactile.name())
        {
            return Err(TokenError(format!("unknown effects table {extra}")));
        }
        Ok(out)
    }
}

/// A kind of button face that effects shade (ADR-0122 decision 5's table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaceKind {
    /// The kind in words, for messages.
    pub name: &'static str,
    /// The fills at rest, under the pointer and pressed, as theme colour names. A kind
    /// with no pressed fill of its own repeats its hover fill.
    pub fills: [&'static str; 3],
    /// The inks drawn on the face.
    pub inks: &'static [&'static str],
    /// The focus ring is drawn over the face, so its pair is checked at the bottom edge.
    pub ring: bool,
    /// The colour of the kind's rim and inner glow, if it takes them (decision 7).
    pub rim: Option<&'static str>,
    /// The face sits inside a track one `size.focus-ring` from its edge (a
    /// SegmentedControl's chosen plate): its one height is `size.control.md` less two
    /// rings, instead of the three button heights.
    pub inset: bool,
}

/// Index of the primary in [`FACE_KINDS`] and every per-kind array.
pub const KIND_PRIMARY: usize = 0;
/// Index of the secondary (and framed ghost) kind.
pub const KIND_SECONDARY: usize = 1;
/// Index of a checked button.
pub const KIND_CHECKED: usize = 2;
/// Index of a danger button with text.
pub const KIND_DANGER: usize = 3;
/// Index of a SegmentedControl's chosen plate.
pub const KIND_PLATE: usize = 4;

/// How many kinds of face effects shade.
pub const KINDS: usize = 5;

/// The kinds whose faces are shaded, in the order of every per-kind array: primary,
/// secondary (and framed ghost), checked, danger with text, the chosen plate.
pub const FACE_KINDS: [FaceKind; KINDS] = [
    FaceKind {
        name: "primary",
        fills: ["color.accent", "color.accent.hover", "color.accent.pressed"],
        inks: &["color.text.inverse"],
        ring: false,
        rim: None,
        inset: false,
    },
    FaceKind {
        name: "secondary",
        fills: [
            "color.button.fill",
            "color.button.hover",
            "color.button.pressed",
        ],
        inks: &["color.text.primary", "color.text.secondary"],
        ring: true,
        rim: Some("color.accent"),
        inset: false,
    },
    FaceKind {
        name: "checked",
        fills: [
            "color.accent.subtle",
            "color.accent.subtle.hover",
            "color.accent.subtle.hover",
        ],
        inks: &["color.text.primary", "color.text.secondary", "color.accent"],
        ring: true,
        rim: None,
        inset: false,
    },
    FaceKind {
        name: "danger",
        fills: [
            "color.action.destructive.subtle",
            "color.action.destructive.subtle.hover",
            "color.action.destructive.subtle.hover",
        ],
        inks: &["color.state.error", "color.text.secondary"],
        ring: true,
        rim: Some("color.action.destructive"),
        inset: false,
    },
    // A SegmentedControl's chosen plate (ADR-0120 decision 14, owner 2026-11-16): it
    // neither hovers nor presses, so its three fills are one.
    FaceKind {
        name: "chosen plate",
        fills: [
            "color.surface.overlay",
            "color.surface.overlay",
            "color.surface.overlay",
        ],
        inks: &["color.text.primary"],
        ring: true,
        rim: None,
        inset: true,
    },
];

/// The resting fill of each kind of button that takes a key edge, in [`FACE_KINDS`]
/// order.
pub const KEY_FILLS: [&str; KINDS] = [
    FACE_KINDS[0].fills[0],
    FACE_KINDS[1].fills[0],
    FACE_KINDS[2].fills[0],
    FACE_KINDS[3].fills[0],
    FACE_KINDS[4].fills[0],
];

fn channel(c: u8, to: u8, f: f32) -> u8 {
    let f = f.clamp(0.0, 1.0);
    (f32::from(c) + (f32::from(to) - f32::from(c)) * f)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// `from` mixed toward `to` by `f` (0 none, 1 all), per channel in encoded sRGB,
/// rounded to nearest (ADR-0122 decision 4). Opaque.
pub fn mix(from: Color, to: Color, f: f32) -> Color {
    Color {
        r: channel(from.r, to.r, f),
        g: channel(from.g, to.g, f),
        b: channel(from.b, to.b, f),
        a: 255,
    }
}

/// `c` mixed toward white by `f`: `c + (255 − c)·f` per channel.
pub fn toward_white(c: Color, f: f32) -> Color {
    mix(c, WHITE, f)
}

/// `c` mixed toward black by `f`: `c·(1 − f)` per channel.
pub fn toward_black(c: Color, f: f32) -> Color {
    mix(c, BLACK, f)
}

/// `fill` mixed toward black by `shade`: the key edge colour.
pub fn key_colour(fill: Color, shade: f32) -> Color {
    toward_black(fill, shade)
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
    mix(ground, top, alpha)
}

/// The shading of one kind's face, as the theme's contrast rules allow it (ADR-0122
/// decision 5).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct FaceDraw {
    /// The gloss stop `s` as a fraction of the height. An instance's stop is
    /// `max(band top / height, s)`; 0 keeps the stop at the band top.
    pub stop: f32,
    /// How far the bottom is mixed toward black: the token scaled down for this kind.
    pub dark: f32,
    /// The top stop at rest, under the pointer and pressed.
    pub top: [Color; 3],
    /// The fill at rest, under the pointer and pressed (the gloss stop's colour).
    pub fill: [Color; 3],
    /// The bottom stop at rest, under the pointer and pressed.
    pub bottom: [Color; 3],
}

impl FaceDraw {
    /// True when every stop equals its fill: the face is a flat colour.
    pub fn is_flat(&self) -> bool {
        self.top == self.fill && self.bottom == self.fill
    }
}

/// What a slot draws: the setting's values scaled into the slot's extent, the faces and
/// the derived colours (ADR-0122 decisions 3 to 8). All zero and `on: false` for calm.
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
    /// Halo blur radius; 0 when the setting draws no halo. Not scaled by the extent.
    pub halo_blur: f32,
    /// Halo colour: the primary rest fill at the halo alpha.
    pub halo: Color,
    /// The clear zone a position that opts in keeps round a primary: `halo.blur + 1`
    /// when the setting draws a halo, else 0 (decision 8).
    pub halo_clear: f32,
    /// Key edge height.
    pub key_edge: f32,
    /// Press offset.
    pub press: f32,
    /// Key edge colours, per [`FACE_KINDS`].
    pub key: [Color; KINDS],
    /// The face shading, per [`FACE_KINDS`].
    pub faces: [FaceDraw; KINDS],
    /// The rim is drawn (the border of a secondary or danger button takes `rim`).
    pub rim_on: bool,
    /// Rim colours, per [`FACE_KINDS`]; a kind without a rim holds `color.border.subtle`.
    pub rim: [Color; KINDS],
    /// Inner glow blur radius.
    pub inner_blur: f32,
    /// Inner glow colours with their alpha, per [`FACE_KINDS`]; alpha 0 is none.
    pub inner: [Color; KINDS],
    /// The slot's extent.
    pub extent: f32,
    /// The chosen plate's shadow blur and offset: the shadow scaled into the track's
    /// room round the plate, one `size.focus-ring`.
    pub plate_shadow_blur: f32,
    /// See `plate_shadow_blur`.
    pub plate_shadow_offset: f32,
    /// The chosen plate's key edge, drawn inside the plate's bottom: the key edge, at
    /// most the plate's band top, so it never reaches the ink.
    pub plate_key_edge: f32,
    /// The chosen plate's glow in a setting that draws a halo (Glow): the `halo` colour
    /// with this blur, the track's room round the plate; 0 when the setting has no halo.
    /// It takes the place of the plate's shadow.
    pub plate_glow_blur: f32,
}

impl EffectDraw {
    /// True when no face is shaded.
    pub fn faces_are_flat(&self) -> bool {
        self.faces.iter().all(FaceDraw::is_flat)
    }
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

/// The control heights a button can have, after density.
fn control_heights(length: &BTreeMap<String, f32>) -> [f32; 3] {
    ["size.control.sm", "size.control.md", "size.control.lg"]
        .map(|n| length.get(n).copied().unwrap_or(0.0))
}

/// The heights a face of `kind` can have, after density: the three button heights, or
/// the plate's one height for an inset kind (0 is skipped).
pub fn kind_heights(kind: &FaceKind, length: &BTreeMap<String, f32>) -> [f32; 3] {
    if kind.inset {
        [plate_height(length), 0.0, 0.0]
    } else {
        control_heights(length)
    }
}

/// The one height of an inset face (a SegmentedControl's chosen plate), after density:
/// `size.control.md − 2 · size.focus-ring`.
pub fn plate_height(length: &BTreeMap<String, f32>) -> f32 {
    let get = |n: &str| length.get(n).copied().unwrap_or(0.0);
    (get("size.control.md") - 2.0 * get("size.focus-ring")).max(0.0)
}

/// The top of the ink band of a button `height` tall: `(height − label line) / 2`.
pub fn band_top(height: f32, label_line: f32) -> f32 {
    ((height - label_line) / 2.0).max(0.0)
}

/// The gloss stop of an instance `height` tall for a kind whose searched stop is `s`,
/// as a fraction of the height: `max(band top / height, s)` (decision 5).
pub fn gloss_stop(height: f32, label_line: f32, s: f32) -> f32 {
    if height <= 0.0 {
        return s;
    }
    (band_top(height, label_line) / height).max(s)
}

/// The colour of a three-stop face at `t` (0 top, 1 bottom) with the gloss stop at `p`.
pub fn face_colour(top: Color, fill: Color, bottom: Color, p: f32, t: f32) -> Color {
    if t <= p {
        if p <= 0.0 {
            fill
        } else {
            mix(top, fill, t / p)
        }
    } else if p >= 1.0 {
        fill
    } else {
        mix(fill, bottom, (t - p) / (1.0 - p))
    }
}

/// The minimum of the contrast rule for `fg` drawn on `bg`, if there is one.
fn rule(fg: &str, bg: &str) -> Option<f64> {
    CONTRAST_RULES
        .iter()
        .chain(THEME_CONTRAST_RULES)
        .find(|(f, b, ..)| *f == fg && *b == bg)
        .map(|(_, _, min)| *min)
}

/// One pair of decision 5's table for one state of a kind: the ink colour and the
/// ratio it must reach, `min(rule + margin, its calm ratio)`.
struct Pair {
    /// The ink's relative luminance.
    ink: f64,
    bar: f64,
}

impl Pair {
    /// True when the ink keeps its bar on a face of relative luminance `face`.
    fn holds(&self, face: f64) -> bool {
        (self.ink.max(face) + 0.05) / (self.ink.min(face) + 0.05) >= self.bar
    }
}

/// The resolved colours of one kind: fills per state, the pairs per state and the ring
/// bar per state.
struct KindColours {
    fills: [Color; 3],
    pairs: [Vec<Pair>; 3],
    ring: [Option<Pair>; 3],
}

impl KindColours {
    fn of(kind: &FaceKind, colors: &BTreeMap<String, Color>) -> KindColours {
        let colour = |n: &str| colors.get(n).copied().unwrap_or(BLACK);
        // A pair whose calm ratio is under its rule is skipped: ADR-0072 already warns
        // about it, and the theme does not also lose its effects.
        let pair = |fg: &str, bg: &str| {
            let need = rule(fg, bg)?;
            let calm = colour(fg).contrast(colour(bg));
            (calm >= need).then(|| Pair {
                ink: colour(fg).luminance(),
                bar: (need + FACE_MARGIN).min(calm),
            })
        };
        let fills = kind.fills.map(colour);
        let pairs = kind.fills.map(|bg| {
            kind.inks
                .iter()
                .filter_map(|fg| pair(fg, bg))
                .collect::<Vec<_>>()
        });
        let ring = kind
            .fills
            .map(|bg| kind.ring.then(|| pair("color.border.focus", bg)).flatten());
        KindColours { fills, pairs, ring }
    }

    /// True when every pair holds on a face shaded with `gloss`, `dark` and the stop
    /// `s`, for every state and control height. `inner` is an inner glow's colour, alpha
    /// and blur, blended into the face where the ink band comes nearest the edge.
    fn passes(
        &self,
        gloss: f32,
        dark: f32,
        s: f32,
        inner: Option<(Color, f32, f32)>,
        heights: [f32; 3],
        label_line: f32,
    ) -> bool {
        for state in 0..3 {
            let fill = self.fills[state];
            let top = toward_white(fill, gloss);
            let bottom = toward_black(fill, dark);
            if let Some(ring) = &self.ring[state]
                && !ring.holds(bottom.luminance())
            {
                return false;
            }
            if self.pairs[state].is_empty() {
                continue;
            }
            for (i, h) in heights.into_iter().enumerate() {
                if h <= 0.0 || heights[..i].contains(&h) {
                    continue;
                }
                let band = band_top(h, label_line);
                let p = gloss_stop(h, label_line, s);
                // The band ends, and the stop when it lies inside the band: the gradient
                // is monotonic per channel within each segment, so these bound every row
                // of ink.
                let ends = [band / h, (h - band) / h];
                let inside = (p > ends[0] && p < ends[1]).then_some(p);
                for t in ends.into_iter().chain(inside) {
                    let mut face = face_colour(top, fill, bottom, p, t);
                    if let Some((glow, alpha, blur)) = inner {
                        // The Gaussian tail: evaluated half a pixel nearer the edge.
                        let edge = (t * h).min(h - t * h);
                        let a = residual(alpha, 0.0, blur, (edge - 0.5).max(0.0));
                        face = over(glow, a, face);
                    }
                    let face = face.luminance();
                    if self.pairs[state].iter().any(|pair| !pair.holds(face)) {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Counts down from `from` in steps of 0.01 to the largest value `ok` accepts; 0 when
/// none is accepted.
fn largest(from: f32, ok: impl Fn(f32) -> bool) -> f32 {
    let mut n = (from * 100.0).round() as i32;
    while n > 0 {
        let v = n as f32 / 100.0;
        if ok(v) {
            return v;
        }
        n -= 1;
    }
    0.0
}

/// What `values` draw on a slot with `colors` (a resolved theme's), `length` (after
/// density) and `label_line` (the label style's line height); `dark` picks the
/// shadow alpha. Runs the searches of ADR-0122 decisions 5 to 7.
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
    let accent = colour("color.accent");
    let subtle = colour("color.border.subtle");
    let plate = plate_height(length);
    let mut faces = [FaceDraw::default(); KINDS];
    let mut rim = [subtle; KINDS];
    let mut inner = [with_alpha(BLACK, 0.0); KINDS];
    let mut rim_on = false;
    for (i, kind) in FACE_KINDS.iter().enumerate() {
        let k = KindColours::of(kind, colors);
        let heights = kind_heights(kind, length);
        // 1. The bottom shading: the largest value every pair of the kind allows.
        let face_dark = largest(values.face_dark, |d| {
            k.passes(0.0, d, 0.0, None, heights, label_line)
        });
        // 2. The gloss stop: down from the band top while every pair still holds.
        let mut stop = 0.0;
        if values.gloss > 0.0 {
            let mut n = 1;
            while n as f32 / 100.0 <= GLOSS_STOP_MAX + f32::EPSILON
                && k.passes(
                    values.gloss,
                    face_dark,
                    n as f32 / 100.0,
                    None,
                    heights,
                    label_line,
                )
            {
                stop = n as f32 / 100.0;
                n += 1;
            }
            // A stop above every height's band top changes no instance: the band top.
            if heights
                .iter()
                .all(|h| *h <= 0.0 || stop <= band_top(*h, label_line) / *h)
            {
                stop = 0.0;
            }
        }
        faces[i] = FaceDraw {
            stop,
            dark: face_dark,
            top: k.fills.map(|f| toward_white(f, values.gloss)),
            fill: k.fills,
            bottom: k.fills.map(|f| toward_black(f, face_dark)),
        };
        let Some(rim_name) = kind.rim else {
            continue;
        };
        let rim_colour = colour(rim_name);
        // The rim must not read as a checked button's accent outline.
        let rim_mix = largest(values.rim_mix, |m| {
            delta_e_2000(mix(subtle, rim_colour, m), accent) >= MIN_DELTA_E_RIM_FROM_ACCENT
        });
        if rim_mix > 0.0 {
            rim[i] = mix(subtle, rim_colour, rim_mix);
            rim_on = true;
        }
        // The inner glow lies under the ink band's ends: scaled down like the shading.
        if values.inner_blur > 0.0 {
            let inner_alpha = largest(values.inner_alpha, |a| {
                k.passes(
                    values.gloss,
                    face_dark,
                    stop,
                    Some((rim_colour, a, values.inner_blur)),
                    heights,
                    label_line,
                )
            });
            inner[i] = with_alpha(rim_colour, inner_alpha);
        }
    }
    let halo = values.has_halo();
    // The plate's shadow stays inside the track's room round it.
    let room = length
        .get("size.focus-ring")
        .copied()
        .unwrap_or(0.0)
        .max(0.0);
    let plate_reach = (values.shadow_blur + values.shadow_offset) * scale;
    let plate_scale = if plate_reach > room && plate_reach > 0.0 {
        room / plate_reach
    } else {
        1.0
    };
    EffectDraw {
        on: true,
        shadow_blur: values.shadow_blur * scale,
        shadow_offset: values.shadow_offset * scale,
        shadow: with_alpha(BLACK, alpha),
        halo_blur: if halo { values.halo_blur } else { 0.0 },
        halo: with_alpha(accent, if halo { values.halo_alpha } else { 0.0 }),
        halo_clear: if halo { values.halo_blur + 1.0 } else { 0.0 },
        key_edge: values.key_edge * scale,
        press: values.press * scale,
        key: KEY_FILLS.map(|f| key_colour(colour(f), values.key_shade)),
        faces,
        rim_on,
        rim,
        inner_blur: values.inner_blur,
        inner,
        extent,
        plate_shadow_blur: values.shadow_blur * scale * plate_scale,
        plate_shadow_offset: values.shadow_offset * scale * plate_scale,
        plate_key_edge: (values.key_edge * scale).min(band_top(plate, label_line)),
        plate_glow_blur: if halo { room } else { 0.0 },
    }
}

/// The effect check that failed ([`checked`]); its text is the reason shown.
#[derive(Clone, Debug, PartialEq)]
pub enum EffectCheckError {
    /// A derived colour is too close to the live colour.
    LooksLive {
        /// The part, as a phrase ("the glow").
        what: String,
        /// Its CIEDE2000 difference from the live colour.
        difference: f64,
    },
    /// The focus ring beside an effect falls under 3:1 contrast.
    FocusContrast {
        /// The contrast ratio it has.
        ratio: f64,
    },
}

impl std::fmt::Display for EffectCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EffectCheckError::LooksLive { what, difference } => write!(
                f,
                "{what} looks like the live colour (difference {difference:.1}, needs {MIN_DELTA_E_FROM_LIVE})"
            ),
            EffectCheckError::FocusContrast { ratio } => write!(
                f,
                "the focus ring beside an effect has contrast {ratio:.2}, needs 3"
            ),
        }
    }
}

impl std::error::Error for EffectCheckError {}

/// What `values` draw, as [`draw`], after the effect checks that turn a setting off
/// (ADR-0120 decision 9, ADR-0122 decision 6): `Err` holds the failed check. The face
/// pairs never fail here: their searches scale the shading down instead.
pub fn checked(
    values: &EffectValues,
    colors: &BTreeMap<String, Color>,
    length: &BTreeMap<String, f32>,
    label_line: f32,
    dark: bool,
    live: Color,
) -> Result<EffectDraw, EffectCheckError> {
    let d = draw(values, colors, length, label_line, dark);
    if !d.on {
        return Ok(d);
    }
    // Every derived colour against the live colour: nothing reads as "live red".
    let mut derived: Vec<(String, Color)> = Vec::new();
    if d.key_edge > 0.0 {
        for (fill, key) in KEY_FILLS.iter().zip(d.key) {
            derived.push((format!("the darker edge under {fill}"), key));
        }
    }
    if d.halo.a > 0 {
        derived.push(("the glow".into(), Color { a: 255, ..d.halo }));
    }
    for (i, kind) in FACE_KINDS.iter().enumerate() {
        let face = &d.faces[i];
        for state in 0..3 {
            if face.top[state] != face.fill[state] {
                derived.push((
                    format!("the gloss on {}", kind.fills[state]),
                    face.top[state],
                ));
            }
            if face.bottom[state] != face.fill[state] {
                derived.push((
                    format!("the shading on {}", kind.fills[state]),
                    face.bottom[state],
                ));
            }
        }
        if d.rim_on && kind.rim.is_some() {
            derived.push((format!("the rim of a {} button", kind.name), d.rim[i]));
        }
        if d.inner[i].a > 0 {
            derived.push((
                format!("the inner glow of a {} button", kind.name),
                Color {
                    a: 255,
                    ..d.inner[i]
                },
            ));
        }
    }
    for (what, c) in derived {
        let diff = delta_e_2000(c, live);
        if diff < MIN_DELTA_E_FROM_LIVE {
            return Err(EffectCheckError::LooksLive {
                what,
                difference: diff,
            });
        }
    }
    // The clamp: a neighbour's strongest shadow at the outer edge of the extent, blended
    // into the toolbar ground, must keep the focus ring at 3:1. The halo is not part of
    // this: its clear zone keeps it off every neighbour (decision 8).
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
        let mut grounds = vec![*ground];
        if shadow > 0.0 {
            grounds.push(over(BLACK, shadow, *ground));
        }
        for g in grounds {
            let ratio = focus.contrast(g);
            if ratio < 3.0 {
                return Err(EffectCheckError::FocusContrast { ratio });
            }
        }
    }
    Ok(d)
}

/// The failed check of [`checked`], or `None` when `values` may be drawn.
pub fn check(
    values: &EffectValues,
    colors: &BTreeMap<String, Color>,
    length: &BTreeMap<String, f32>,
    label_line: f32,
    dark: bool,
    live: Color,
) -> Option<EffectCheckError> {
    checked(values, colors, length, label_line, dark, live).err()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use crate::tokens::{Base, Density, Tokens, apply_density};

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

    fn colour(s: &str) -> Color {
        Color::parse(s).unwrap()
    }

    /// The built-in theme `base` drawn with `setting` at `density`.
    fn built_in(base: Base, setting: Effects, density: Density, steps: u8) -> EffectDraw {
        let t = Tokens::built_in().unwrap();
        let theme = t.built_in_theme(base);
        let mut length = theme.length.clone();
        apply_density(&mut length, density, steps);
        let values = t.effects.unwrap().values(setting);
        draw(&values, &theme.colors, &length, 20.0, base == Base::Dark)
    }

    #[test]
    fn names_round_trip_and_vivid_is_glow() {
        for e in Effects::ALL {
            assert_eq!(Effects::from_name(e.name()), Some(e));
            assert_eq!(Effects::ALL[e.index()], e);
            assert_ne!(e.name(), VIVID_ALIAS);
        }
        assert_eq!(Effects::from_name("vivid"), Some(Effects::Glow));
        assert_eq!(Effects::from_name("glossy"), None);
    }

    #[test]
    fn bounds_press_extent_and_the_halo_rule_are_checked() {
        assert!(EffectValues::parse("glow", &table(&[])).unwrap().is_calm());
        assert!(EffectValues::parse("glow", &table(&[("effects.shadow.blur", 4.5)])).is_err());
        assert!(EffectValues::parse("glow", &table(&[("effects.gloss", -0.1)])).is_err());
        assert!(EffectValues::parse("glow", &table(&[("effects.halo.blur", 16.5)])).is_err());
        assert!(EffectValues::parse("glow", &table(&[("effects.press", 1.0)])).is_err());
        // 3 + 1 + 1 = 5 > 4.
        let wide = [
            ("effects.key.edge", 3.0),
            ("effects.shadow.blur", 1.0),
            ("effects.shadow.offset", 1.0),
        ];
        assert!(EffectValues::parse("glow", &table(&wide)).is_err());
        let fits = [
            ("effects.key.edge", 3.0),
            ("effects.press", 2.0),
            ("effects.shadow.blur", 1.0),
        ];
        assert_eq!(
            EffectValues::parse("tactile", &table(&fits))
                .unwrap()
                .reach(),
            4.0
        );
        // The halo is outside the extent rule, but needs key.edge 0.
        let halo = [("effects.halo.blur", 16.0), ("effects.halo.alpha", 0.8)];
        assert!(
            EffectValues::parse("glow", &table(&halo))
                .unwrap()
                .has_halo()
        );
        let halo_on_an_edge = [
            ("effects.halo.blur", 12.0),
            ("effects.halo.alpha", 0.6),
            ("effects.key.edge", 1.0),
        ];
        let e = EffectValues::parse("glow", &table(&halo_on_an_edge)).unwrap_err();
        assert!(e.0.contains("effects.key.edge must be 0"), "{e}");
        // Unknown and removed tokens, and a missing one.
        for name in ["effects.bevel", "effects.sheen.alpha", "effects.glow.blur"] {
            let mut extra = table(&[]);
            extra.insert(name.into(), 0.0);
            assert!(EffectValues::parse("glow", &extra).is_err(), "{name}");
        }
        let mut missing = table(&[]);
        missing.remove("effects.press");
        assert!(EffectValues::parse("glow", &missing).is_err());
    }

    #[test]
    fn format_4_needs_exactly_glow_glass_and_tactile() {
        let all = |names: &[&str]| {
            names
                .iter()
                .map(|n| ((*n).to_owned(), table(&[])))
                .collect::<BTreeMap<_, _>>()
        };
        assert!(EffectTables::parse(all(&["glow", "glass", "tactile"])).is_ok());
        assert!(EffectTables::parse(all(&["glow", "tactile"])).is_err());
        assert!(EffectTables::parse(all(&["glow", "glass", "tactile", "vivid"])).is_err());
        assert!(EffectTables::parse(all(&["vivid", "glass", "tactile"])).is_err());
    }

    #[test]
    fn a_format_3_table_is_carried_over() {
        let v3 = |pairs: &[(&str, f64)]| {
            let mut t: BTreeMap<String, f64> = EFFECT_TOKENS_V3
                .iter()
                .map(|(n, ..)| ((*n).to_owned(), 0.0))
                .collect();
            for (n, v) in pairs {
                t.insert((*n).to_owned(), *v);
            }
            t
        };
        let mut raw = BTreeMap::new();
        raw.insert(
            "vivid".to_owned(),
            v3(&[
                ("effects.shadow.blur", 3.0),
                ("effects.glow.blur", 4.0),
                ("effects.glow.alpha", 0.5),
                ("effects.sheen.alpha", 0.3),
                ("effects.sheen.band", 4.0),
            ]),
        );
        raw.insert(
            "tactile".to_owned(),
            v3(&[
                ("effects.key.edge", 2.0),
                ("effects.press", 2.0),
                ("effects.glow.blur", 2.0),
                ("effects.glow.alpha", 0.5),
            ]),
        );
        let t = EffectTables::parse_v3(raw.clone()).unwrap();
        // Vivid is Glow: its glow is the halo, its sheen is gone, every new token is 0.
        assert_eq!(
            t.glow,
            EffectValues {
                shadow_blur: 3.0,
                halo_blur: 4.0,
                halo_alpha: 0.5,
                ..EffectValues::default()
            }
        );
        // A glow on a key edge cannot be a halo.
        assert_eq!(
            t.tactile,
            EffectValues {
                key_edge: 2.0,
                press: 2.0,
                ..EffectValues::default()
            }
        );
        assert_eq!(t.glass, None);
        assert!(!t.has(Effects::Glass));
        assert!(t.values(Effects::Glass).is_calm());
        // A format 3 file has no other table and no format 4 token.
        raw.insert("glass".to_owned(), v3(&[]));
        assert!(EffectTables::parse_v3(raw.clone()).is_err());
        raw.remove("glass");
        if let Some(t) = raw.get_mut("vivid") {
            t.insert("effects.gloss".into(), 0.1);
        }
        assert!(EffectTables::parse_v3(raw).is_err());
    }

    #[test]
    fn mixing_is_per_channel_in_encoded_srgb_rounded_to_nearest() {
        let c = colour("#646464");
        assert_eq!(toward_black(c, 0.0), c);
        assert_eq!(toward_black(c, 0.4).to_string(), "#3c3c3c");
        assert_eq!(toward_white(c, 0.3).to_string(), "#939393");
        assert_eq!(key_colour(c, 0.55).to_string(), "#2d2d2d");
        assert_eq!(
            mix(colour("#000000"), colour("#ff8001"), 0.5).to_string(),
            "#804001"
        );
        assert_eq!(
            over(colour("#ffffff"), 0.25, colour("#000000")).to_string(),
            "#404040"
        );
    }

    #[test]
    fn values_scale_into_a_smaller_extent_but_the_halo_does_not() {
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
            ("size.control.md", 24.0),
            ("size.control.lg", 24.0),
        ] {
            length.insert(n.to_owned(), x);
        }
        let d = draw(&v, &BTreeMap::new(), &length, 20.0, true);
        assert_eq!(d.extent, 2.0);
        assert!(d.key_edge + d.shadow_blur + d.shadow_offset <= d.extent);
        assert!(d.press <= d.key_edge);
        assert_eq!((d.halo_blur, d.halo.a, d.halo_clear), (0.0, 0, 0.0));
        let halo = EffectValues {
            halo_blur: 12.0,
            halo_alpha: 0.6,
            shadow_blur: 3.0,
            shadow_offset: 1.0,
            ..EffectValues::default()
        };
        let d = draw(&halo, &BTreeMap::new(), &length, 20.0, true);
        assert_eq!((d.shadow_blur, d.shadow_offset), (1.5, 0.5));
        assert_eq!((d.halo_blur, d.halo_clear), (12.0, 13.0));
        length.insert("space.1".into(), 0.0);
        let d = draw(&v, &BTreeMap::new(), &length, 20.0, true);
        assert_eq!(
            (d.extent, d.key_edge, d.press, d.shadow_blur),
            (0.0, 0.0, 0.0, 0.0)
        );
    }

    #[test]
    fn the_halo_clear_zone_is_the_blur_plus_one_only_where_a_halo_draws() {
        for base in [Base::Dark, Base::Light] {
            for (density, steps) in [(Density::Regular, 1), (Density::Compact, 3)] {
                let glow = built_in(base, Effects::Glow, density, steps);
                assert_eq!((glow.halo_blur, glow.halo_clear), (12.0, 13.0));
                assert!((f32::from(glow.halo.a) / 255.0 - 0.6).abs() < 0.01);
                for e in [Effects::Glass, Effects::Tactile] {
                    let d = built_in(base, e, density, steps);
                    assert_eq!((d.halo_blur, d.halo.a, d.halo_clear), (0.0, 0, 0.0));
                }
            }
        }
        assert_eq!(EffectDraw::default().halo_clear, 0.0);
    }

    #[test]
    fn residual_falls_to_nothing_at_the_reach() {
        assert_eq!(residual(0.4, 1.0, 3.0, 4.0), 0.0);
        assert!(residual(0.4, 1.0, 3.0, 2.0) > 0.0);
        assert_eq!(residual(0.4, 0.0, 0.0, 0.0), 0.0);
        assert_eq!(residual(0.4, 0.0, 6.0, 1.5), 0.3);
    }

    #[test]
    fn the_gloss_stop_is_never_above_the_band_top() {
        // 4 of 28 px, 2 of 24 and 6 of 32 (ADR-0122 decision 5).
        assert_eq!(band_top(28.0, 20.0), 4.0);
        assert_eq!(band_top(24.0, 20.0), 2.0);
        assert_eq!(band_top(32.0, 20.0), 6.0);
        assert_eq!(band_top(16.0, 20.0), 0.0);
        for (h, top) in [(24.0, 2.0), (28.0, 4.0), (32.0, 6.0)] {
            assert_eq!(gloss_stop(h, 20.0, 0.0), top / h);
            assert_eq!(gloss_stop(h, 20.0, 0.45), 0.45);
            assert_eq!(gloss_stop(h, 20.0, 0.1), (top / h).max(0.1));
        }
        // The face is the fill at the stop, the top above it and the bottom below.
        let (top, fill, bottom) = (colour("#ffffff"), colour("#808080"), colour("#000000"));
        assert_eq!(face_colour(top, fill, bottom, 0.25, 0.0), top);
        assert_eq!(face_colour(top, fill, bottom, 0.25, 0.25), fill);
        assert_eq!(face_colour(top, fill, bottom, 0.25, 1.0), bottom);
        assert_eq!(face_colour(top, fill, bottom, 0.0, 0.0), fill);
    }

    /// ADR-0122 decision 5's table: the built-in colours at regular density.
    #[test]
    fn the_built_in_faces_resolve_to_the_decided_values() {
        let stops = |d: &EffectDraw| d.faces.map(|f| (f.dark, f.stop));
        let glass = built_in(Base::Dark, Effects::Glass, Density::Regular, 1);
        assert_eq!(
            stops(&glass)[..4],
            [(0.09, 0.45), (0.3, 0.0), (0.3, 0.1), (0.3, 0.0)]
        );
        let glass = built_in(Base::Light, Effects::Glass, Density::Regular, 1);
        assert_eq!(
            stops(&glass)[..4],
            [(0.3, 0.0), (0.03, 0.45), (0.03, 0.45), (0.02, 0.45)]
        );
        // Tactile has a gloss and no bottom shading.
        let tactile = built_in(Base::Dark, Effects::Tactile, Density::Regular, 1);
        assert_eq!(
            stops(&tactile)[..4],
            [(0.0, 0.45), (0.0, 0.0), (0.0, 0.14), (0.0, 0.0)]
        );
        let tactile = built_in(Base::Light, Effects::Tactile, Density::Regular, 1);
        assert_eq!(tactile.faces[KIND_SECONDARY].stop, 0.45);
        assert_eq!(tactile.faces[KIND_CHECKED].stop, 0.45);
        assert_eq!(tactile.faces[KIND_DANGER].stop, 0.45);
        // Light's primary keeps its gloss within a fraction of a pixel of the band top.
        assert!(tactile.faces[KIND_PRIMARY].stop < 0.1);
        // The chosen plate: shaded in Glass, a gloss in Tactile, flat in Glow.
        let plate = |b, e| built_in(b, e, Density::Regular, 1).faces[KIND_PLATE];
        assert_eq!(
            (
                plate(Base::Dark, Effects::Glass).dark,
                plate(Base::Dark, Effects::Glass).stop
            ),
            (0.3, 0.45)
        );
        assert_eq!(
            (
                plate(Base::Light, Effects::Glass).dark,
                plate(Base::Light, Effects::Glass).stop
            ),
            (0.06, 0.45)
        );
        assert_eq!(plate(Base::Dark, Effects::Tactile).stop, 0.45);
        assert_eq!(plate(Base::Light, Effects::Tactile).stop, 0.45);
        // Glow shades no face.
        for base in [Base::Dark, Base::Light] {
            assert!(built_in(base, Effects::Glow, Density::Regular, 1).faces_are_flat());
        }
    }

    #[test]
    fn glow_resolves_its_rim_and_scales_its_inner_glow() {
        let alpha = |c: Color| (f32::from(c.a) / 255.0 * 100.0).round() / 100.0;
        // Dark: text.secondary on button.pressed and the danger pairs leave no room.
        let dark = built_in(Base::Dark, Effects::Glow, Density::Regular, 1);
        assert_eq!(dark.inner.map(|c| c.a), [0; KINDS]);
        // Light: 0.19 for secondary and 0.05 for danger, of the token's 0.25.
        let light = built_in(Base::Light, Effects::Glow, Density::Regular, 1);
        assert_eq!(light.inner.map(alpha), [0.0, 0.19, 0.0, 0.05, 0.0]);
        let t = Tokens::built_in().unwrap();
        for (base, d) in [(Base::Dark, dark), (Base::Light, light)] {
            let c = &t.built_in_theme(base).colors;
            let (accent, subtle) = (c["color.accent"], c["color.border.subtle"]);
            assert!(d.rim_on);
            // The rim is the full 0.55, and far enough from a checked outline.
            assert_eq!(d.rim[KIND_SECONDARY], mix(subtle, accent, 0.55));
            assert_eq!(
                d.rim[KIND_DANGER],
                mix(subtle, c["color.action.destructive"], 0.55)
            );
            for kind in [KIND_SECONDARY, KIND_DANGER] {
                assert!(delta_e_2000(d.rim[kind], accent) >= MIN_DELTA_E_RIM_FROM_ACCENT);
            }
            // Primary and checked take no rim.
            assert_eq!(d.rim[KIND_PRIMARY], subtle);
            assert_eq!(d.rim[KIND_CHECKED], subtle);
        }
    }

    #[test]
    fn a_rim_too_close_to_the_accent_scales_down() {
        let t = Tokens::built_in().unwrap();
        let theme = t.built_in_theme(Base::Dark);
        let values = EffectValues {
            rim_mix: 0.8,
            ..t.effects.unwrap().glow
        };
        let d = draw(&values, &theme.colors, &theme.length, 20.0, true);
        let (accent, subtle) = (
            theme.colors["color.accent"],
            theme.colors["color.border.subtle"],
        );
        assert!(d.rim_on);
        assert_ne!(d.rim[KIND_SECONDARY], mix(subtle, accent, 0.8));
        assert!(delta_e_2000(d.rim[KIND_SECONDARY], accent) >= MIN_DELTA_E_RIM_FROM_ACCENT);
        assert!(delta_e_2000(mix(subtle, accent, 0.8), accent) < MIN_DELTA_E_RIM_FROM_ACCENT);
    }

    /// Every pair of decision 5's table holds on every drawn face, in every density.
    #[test]
    fn every_face_pair_holds_at_the_ink_band_in_every_density() {
        let t = Tokens::built_in().unwrap();
        for base in [Base::Dark, Base::Light] {
            let theme = t.built_in_theme(base);
            for setting in Effects::LIVELY {
                for (density, steps) in [
                    (Density::Regular, 1),
                    (Density::Roomy, 1),
                    (Density::Compact, 1),
                    (Density::Compact, 2),
                    (Density::Compact, 3),
                ] {
                    let mut length = theme.length.clone();
                    apply_density(&mut length, density, steps);
                    let values = t.effects.unwrap().values(setting);
                    let d = draw(&values, &theme.colors, &length, 20.0, base == Base::Dark);
                    for (kind, face) in FACE_KINDS.iter().zip(d.faces) {
                        let k = KindColours::of(kind, &theme.colors);
                        assert!(
                            k.passes(
                                values.gloss,
                                face.dark,
                                face.stop,
                                None,
                                kind_heights(kind, &length),
                                20.0
                            ),
                            "{base:?} {setting:?} {density:?} {steps} {}",
                            kind.name
                        );
                        // One more step of shading would break a pair, or the ceiling
                        // is reached: the search found the limit.
                        if face.dark + 0.01 <= values.face_dark {
                            assert!(!k.passes(
                                0.0,
                                face.dark + 0.01,
                                0.0,
                                None,
                                kind_heights(kind, &length),
                                20.0
                            ));
                        }
                    }
                }
            }
        }
    }

    /// A SegmentedControl's chosen plate (ADR-0120 decision 14): its shadow stays in the
    /// track's room of one focus ring and its key edge stays below the ink band, in
    /// every lively setting and density; Tactile still draws an edge.
    #[test]
    fn the_plate_keeps_its_effects_inside_the_track() {
        let t = Tokens::built_in().unwrap();
        for base in [Base::Dark, Base::Light] {
            let theme = t.built_in_theme(base);
            for setting in Effects::LIVELY {
                for (density, steps) in [
                    (Density::Regular, 1),
                    (Density::Roomy, 1),
                    (Density::Compact, 1),
                    (Density::Compact, 2),
                    (Density::Compact, 3),
                ] {
                    let mut length = theme.length.clone();
                    apply_density(&mut length, density, steps);
                    let values = t.effects.unwrap().values(setting);
                    let d = draw(&values, &theme.colors, &length, 20.0, base == Base::Dark);
                    let why = format!("{base:?} {setting:?} {density:?} {steps}");
                    let ring = length["size.focus-ring"];
                    assert!(
                        d.plate_shadow_blur + d.plate_shadow_offset <= ring + 1e-4,
                        "{why}"
                    );
                    let plate = plate_height(&length);
                    assert!(d.plate_key_edge <= band_top(plate, 20.0) + 1e-4, "{why}");
                    assert!(d.plate_key_edge <= d.key_edge + 1e-4, "{why}");
                    assert!(d.plate_glow_blur <= ring + 1e-4, "{why}");
                    assert_eq!(d.plate_glow_blur > 0.0, setting == Effects::Glow, "{why}");
                    if setting == Effects::Tactile && density == Density::Regular {
                        assert!(d.plate_key_edge > 0.0, "{why}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_pair_that_fails_in_calm_is_skipped() {
        let t = Tokens::built_in().unwrap();
        let mut colors = t.built_in_theme(Base::Dark).colors;
        // text.secondary on the button fills is under its rule: ADR-0072 warns, and the
        // search ignores the pair instead of flattening the secondary.
        colors.insert("color.text.secondary".into(), colour("#55585e"));
        let k = KindColours::of(&FACE_KINDS[KIND_SECONDARY], &colors);
        assert!(k.pairs.iter().all(|p| p.len() == 1));
        let length = t.built_in_theme(Base::Dark).length;
        let glass = t.effects.unwrap().values(Effects::Glass);
        let d = draw(&glass, &colors, &length, 20.0, true);
        assert_eq!(d.faces[KIND_SECONDARY].dark, 0.3);
    }

    #[test]
    fn every_derived_colour_is_checked_against_the_live_colour() {
        let t = Tokens::built_in().unwrap();
        let theme = t.built_in_theme(Base::Dark);
        let live = t.fixed["color.state.live"];
        let tables = t.effects.unwrap();
        let check = |setting: Effects, colors: &BTreeMap<String, Color>| {
            check(
                &tables.values(setting),
                colors,
                &theme.length,
                20.0,
                true,
                live,
            )
        };
        for e in Effects::LIVELY {
            assert_eq!(check(e, &theme.colors), None);
        }
        // An accent like the live colour: the halo and the rim would spread "live red".
        let mut red = theme.colors.clone();
        red.insert("color.accent".into(), colour("#c62a30"));
        let why = check(Effects::Glow, &red).unwrap();
        assert!(
            matches!(&why, EffectCheckError::LooksLive { what, .. } if what == "the glow"),
            "{why}"
        );
        // A destructive colour like it: the danger rim.
        let mut danger = theme.colors.clone();
        danger.insert("color.action.destructive".into(), colour("#c62a30"));
        let why = check(Effects::Glow, &danger).unwrap().to_string();
        assert!(why.contains("the rim of a danger button"), "{why}");
        // A fill whose gloss or key edge lands on it.
        let mut fill = theme.colors.clone();
        fill.insert("color.button.fill".into(), colour("#a80010"));
        let why = check(Effects::Glass, &fill).unwrap().to_string();
        assert!(why.contains("color.button.fill"), "{why}");
        assert!(check(Effects::Tactile, &fill).is_some());
    }
}
