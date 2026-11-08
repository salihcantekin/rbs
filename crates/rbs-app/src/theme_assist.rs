//! Help for editing theme colours (themes spec, theme editor): every contrast problem
//! in words with the colours' names, a "Fix" that moves one colour's lightness just
//! far enough to pass every rule it takes part in, and the full palette worked out
//! from three main colours (background, text, accent). Toolkit-free and pure; the
//! rules themselves stay in `rbs_ui::tokens`, which still decides what can be used.

use std::collections::{BTreeMap, BTreeSet};

use rbs_ui::tokens::{
    CONTRAST_RULES, Color, LIVE_SURFACES, MIN_DELTA_E_FROM_LIVE, THEME_COLORS,
    THEME_CONTRAST_RULES, delta_e_2000, themeable,
};

/// Colour name → value, every theme colour.
pub type Colors = BTreeMap<String, Color>;

/// The three main colours, in the order the editor shows them.
pub const MAIN: [&str; 3] = ["color.surface.base", "color.text.primary", "color.accent"];

/// The main colours' names in the editor.
pub fn main_label(name: &str) -> Option<&'static str> {
    match name {
        "color.surface.base" => Some("Background"),
        "color.text.primary" => Some("Text"),
        "color.accent" => Some("Accent"),
        _ => None,
    }
}

/// A token's name in words (its editor label), or the token name itself.
pub fn label(name: &str) -> &str {
    themeable(name).map_or(name, |t| t.label)
}

/// What the rules ask of one colour against the others.
struct Rules {
    high_contrast: bool,
    live: Color,
}

/// One rule a colour pair breaks.
#[derive(Debug, Clone, PartialEq)]
pub enum Issue {
    /// `fg` drawn on `bg` has too little contrast.
    Contrast {
        fg: String,
        bg: String,
        ratio: f64,
        need: f64,
    },
    /// The fixed live red on `surface` has too little contrast.
    LiveOn { surface: String, ratio: f64 },
    /// `token` looks like the live red.
    LooksLive { token: String },
}

impl Issue {
    /// The colours this issue is about; changing any of them can fix it.
    pub fn tokens(&self) -> Vec<&str> {
        match self {
            Issue::Contrast { fg, bg, .. } => vec![fg, bg],
            Issue::LiveOn { surface, .. } => vec![surface],
            Issue::LooksLive { token } => vec![token],
        }
    }

    /// The issue in words, as seen from the row of `token`.
    pub fn describe(&self, token: &str) -> String {
        match self {
            Issue::Contrast {
                fg,
                bg,
                ratio,
                need,
            } if fg == token => format!(
                "Hard to read on {} ({ratio:.1}:1, needs {need}:1)",
                label(bg)
            ),
            Issue::Contrast {
                fg, ratio, need, ..
            } => format!(
                "{} is hard to read on it ({ratio:.1}:1, needs {need}:1)",
                label(fg)
            ),
            Issue::LiveOn { ratio, .. } => {
                format!("The live red is hard to see on it ({ratio:.1}:1, needs 3:1)")
            }
            Issue::LooksLive { .. } => "Looks too much like the live red".to_owned(),
        }
    }
}

impl Rules {
    fn need(&self, min: f64, min_hc: f64) -> f64 {
        if self.high_contrast { min_hc } else { min }
    }

    /// Every contrast and live-colour issue of `colors`.
    fn issues(&self, colors: &Colors) -> Vec<Issue> {
        let mut out = Vec::new();
        for (fg, bg, min, min_hc) in CONTRAST_RULES.iter().chain(THEME_CONTRAST_RULES) {
            let need = self.need(*min, *min_hc);
            if let (Some(f), Some(b)) = (colors.get(*fg), colors.get(*bg)) {
                let ratio = f.contrast(*b);
                if ratio < need {
                    out.push(Issue::Contrast {
                        fg: (*fg).to_owned(),
                        bg: (*bg).to_owned(),
                        ratio,
                        need,
                    });
                }
            }
        }
        for s in LIVE_SURFACES {
            if let Some(b) = colors.get(*s) {
                let ratio = self.live.contrast(*b);
                if ratio < 3.0 {
                    out.push(Issue::LiveOn {
                        surface: (*s).to_owned(),
                        ratio,
                    });
                }
            }
        }
        for t in THEME_COLORS {
            if colors
                .get(*t)
                .is_some_and(|c| delta_e_2000(*c, self.live) < MIN_DELTA_E_FROM_LIVE)
            {
                out.push(Issue::LooksLive {
                    token: (*t).to_owned(),
                });
            }
        }
        out
    }

    /// True when `token` set to `c` passes every rule it takes part in, the other
    /// colours as they are. A small margin keeps a passing value passing after it
    /// is rounded to whole sRGB steps.
    fn passes(&self, colors: &Colors, token: &str, c: Color) -> bool {
        const MARGIN: f64 = 0.02;
        for (fg, bg, min, min_hc) in CONTRAST_RULES.iter().chain(THEME_CONTRAST_RULES) {
            let need = self.need(*min, *min_hc) + MARGIN;
            let other = if *fg == token {
                colors.get(*bg)
            } else if *bg == token {
                colors.get(*fg)
            } else {
                continue;
            };
            if other.is_some_and(|o| c.contrast(*o) < need) {
                return false;
            }
        }
        if LIVE_SURFACES.contains(&token) && self.live.contrast(c) < 3.0 + MARGIN {
            return false;
        }
        delta_e_2000(c, self.live) >= MIN_DELTA_E_FROM_LIVE + MARGIN
    }

    /// `token` with its lightness moved as little as possible so it passes every rule
    /// it takes part in; hue and chroma stay where sRGB allows. `None`: no lightness
    /// passes (the other colours leave no room).
    fn fix(&self, colors: &Colors, token: &str) -> Option<Color> {
        let now = *colors.get(token)?;
        if self.passes(colors, token, now) {
            return Some(now);
        }
        let (l, a, b) = lab(now);
        // Half a step of CIE lightness at a time, nearest first: at most 200 tries,
        // each a few dozen contrast checks.
        for i in 1..=200 {
            let d = f64::from(i) * 0.5;
            for nl in [l - d, l + d] {
                if !(0.0..=100.0).contains(&nl) {
                    continue;
                }
                let c = from_lab(nl, a, b);
                if self.passes(colors, token, c) {
                    return Some(c);
                }
            }
        }
        None
    }

    /// Fixes issues by moving one colour per issue, never one of `keep`; returns the
    /// colours that changed. A few rounds, as one fix can affect another pair.
    fn fix_all(&self, colors: &mut Colors, keep: &BTreeSet<&str>) -> BTreeSet<String> {
        let mut changed = BTreeSet::new();
        for _ in 0..8 {
            let issues = self.issues(colors);
            if issues.is_empty() {
                break;
            }
            let mut moved = false;
            for issue in issues {
                // The text, border or mark first (the foreground); the surface when
                // the foreground is kept or no lightness of it passes.
                let fixed = issue
                    .tokens()
                    .into_iter()
                    .filter(|t| !keep.contains(t))
                    .find_map(|t| self.fix(colors, t).map(|c| (t.to_owned(), c)));
                if let Some((target, c)) = fixed
                    && colors.get(&target) != Some(&c)
                {
                    colors.insert(target.clone(), c);
                    changed.insert(target);
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        changed
    }
}

/// The colour helper for one theme: its base's colours and rules.
pub struct Assist {
    rules: Rules,
}

impl Assist {
    /// For a theme with high-contrast thresholds or not; `live` is the fixed live red.
    pub fn new(high_contrast: bool, live: Color) -> Self {
        Assist {
            rules: Rules {
                high_contrast,
                live,
            },
        }
    }

    /// Every contrast and live-colour issue of `colors`.
    pub fn issues(&self, colors: &Colors) -> Vec<Issue> {
        self.rules.issues(colors)
    }

    /// `token` moved just far enough in lightness to pass its rules; `None` if no
    /// lightness passes.
    pub fn fix(&self, colors: &Colors, token: &str) -> Option<Color> {
        self.rules.fix(colors, token)
    }

    /// Fixes every issue it can without changing the colours in `keep`; returns the
    /// colours it changed.
    pub fn fix_all(&self, colors: &mut Colors, keep: &[&str]) -> BTreeSet<String> {
        self.rules
            .fix_all(colors, &keep.iter().copied().collect::<BTreeSet<_>>())
    }

    /// The full palette for new main colours. `base` is the built-in theme the theme
    /// starts from; `main` the three main colours ([`MAIN`] order). Each other colour
    /// keeps its relation to the main colours it belongs with in `base` (a panel a
    /// bit lighter than the window, secondary text between text and background, the
    /// selection a tint of the accent), then every issue is fixed without touching
    /// `changed`, the main colour the user just set.
    pub fn derive(&self, base: &Colors, main: [Color; 3], changed: &str) -> Colors {
        let [s, t, a] = main;
        let get = |n: &str| base.get(n).copied().unwrap_or(s);
        let (bs, bt, ba) = (
            get("color.surface.base"),
            get("color.text.primary"),
            get("color.accent"),
        );
        let mut out = base.clone();
        out.insert("color.surface.base".into(), s);
        out.insert("color.text.primary".into(), t);
        out.insert("color.accent".into(), a);
        // Surfaces: the same lightness and tint step from the window background.
        for n in [
            "color.surface.raised",
            "color.surface.overlay",
            "color.surface.sunken",
        ] {
            out.insert(n.into(), shift(s, bs, get(n)));
        }
        // Between background and text: the same place on the way from one to the other.
        for n in [
            "color.text.secondary",
            "color.text.disabled",
            "color.border.subtle",
            "color.border.strong",
        ] {
            out.insert(n.into(), mix(s, t, place(bs, bt, get(n))));
        }
        // The accent's family: the same step from the accent, the selection the same
        // place between background and accent.
        for n in ["color.accent.hover", "color.border.focus"] {
            out.insert(n.into(), shift(a, ba, get(n)));
        }
        out.insert(
            "color.accent.subtle".into(),
            mix(s, a, place(bs, ba, get("color.accent.subtle"))),
        );
        // Text on the accent: black or white, whichever reads better on it.
        let black = Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        };
        let white = Color {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        let hover = out["color.accent.hover"];
        let score = |c: Color| c.contrast(a).min(c.contrast(hover));
        out.insert(
            "color.text.inverse".into(),
            if score(black) >= score(white) {
                black
            } else {
                white
            },
        );
        self.fix_all(&mut out, &[changed]);
        out
    }
}

/// `to` moved from `from_base` the way `x` lies from `from_base` in the base theme:
/// the same Lab difference.
fn shift(to: Color, from_base: Color, x: Color) -> Color {
    let (l0, a0, b0) = lab(from_base);
    let (l1, a1, b1) = lab(x);
    let (l, a, b) = lab(to);
    from_lab(l + (l1 - l0), a + (a1 - a0), b + (b1 - b0))
}

/// Where `x` lies between `a` (0) and `b` (1), by lightness.
fn place(a: Color, b: Color, x: Color) -> f64 {
    let (la, lb, lx) = (lab(a).0, lab(b).0, lab(x).0);
    if (lb - la).abs() < 1e-6 {
        0.5
    } else {
        ((lx - la) / (lb - la)).clamp(0.0, 1.0)
    }
}

/// `a` mixed with `b` in Lab, `f` of the way to `b`.
fn mix(a: Color, b: Color, f: f64) -> Color {
    let (l0, a0, b0) = lab(a);
    let (l1, a1, b1) = lab(b);
    from_lab(l0 + (l1 - l0) * f, a0 + (a1 - a0) * f, b0 + (b1 - b0) * f)
}

const WHITE: (f64, f64, f64) = (0.95047, 1.0, 1.08883);

fn to_linear(v: u8) -> f64 {
    let v = f64::from(v) / 255.0;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn from_linear(v: f64) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0).round().clamp(0.0, 255.0) as u8
}

/// CIE Lab (D65) of an opaque sRGB colour.
pub fn lab(c: Color) -> (f64, f64, f64) {
    let (r, g, b) = (to_linear(c.r), to_linear(c.g), to_linear(c.b));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / WHITE.0;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / WHITE.2;
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

/// The opaque sRGB colour nearest to CIE Lab `(l, a, b)` (channels clamped).
pub fn from_lab(l: f64, a: f64, b: f64) -> Color {
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let inv = |t: f64| {
        if t.powi(3) > 0.008856 {
            t.powi(3)
        } else {
            (t - 16.0 / 116.0) / 7.787
        }
    };
    let (x, y, z) = (inv(fx) * WHITE.0, inv(fy) * WHITE.1, inv(fz) * WHITE.2);
    let r = 3.2406 * x - 1.5372 * y - 0.4986 * z;
    let g = -0.9689 * x + 1.8758 * y + 0.0415 * z;
    let bl = 0.0557 * x - 0.2040 * y + 1.0570 * z;
    Color {
        r: from_linear(r),
        g: from_linear(g),
        b: from_linear(bl),
        a: 255,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

    use super::*;
    use rbs_ui::tokens::{Base, Tokens};

    fn file() -> Tokens {
        Tokens::built_in().unwrap()
    }

    fn hex(s: &str) -> Color {
        Color::parse(s).unwrap()
    }

    #[test]
    fn lab_round_trips_within_one_step() {
        for c in [
            "#000000", "#ffffff", "#3b82f6", "#c4262e", "#1f2937", "#7f7f7f",
        ] {
            let c = hex(c);
            let (l, a, b) = lab(c);
            let back = from_lab(l, a, b);
            for (x, y) in [(c.r, back.r), (c.g, back.g), (c.b, back.b)] {
                assert!(x.abs_diff(y) <= 1, "{c} -> {back}");
            }
        }
    }

    #[test]
    fn built_in_themes_have_no_issues() {
        let f = file();
        let live = f.fixed["color.state.live"];
        for base in Base::ALL {
            let t = f.built_in_theme(base);
            let a = Assist::new(t.high_contrast(), live);
            assert!(a.issues(&t.colors).is_empty(), "{base:?}");
        }
    }

    #[test]
    fn issues_name_both_sides_in_words() {
        let f = file();
        let t = f.built_in_theme(Base::Dark);
        let a = Assist::new(false, f.fixed["color.state.live"]);
        let mut c = t.colors.clone();
        c.insert("color.text.primary".into(), c["color.surface.base"]);
        let issue = a
            .issues(&c)
            .into_iter()
            .find(|i| {
                matches!(i, Issue::Contrast { fg, bg, .. }
                    if fg == "color.text.primary" && bg == "color.surface.base")
            })
            .unwrap();
        assert!(
            issue
                .describe("color.text.primary")
                .starts_with("Hard to read on Window background")
        );
        assert!(
            issue
                .describe("color.surface.base")
                .starts_with("Text is hard to read on it")
        );
    }

    #[test]
    fn fix_moves_only_lightness_until_every_rule_of_the_colour_passes() {
        let f = file();
        let t = f.built_in_theme(Base::Dark);
        let a = Assist::new(false, f.fixed["color.state.live"]);
        let mut c = t.colors.clone();
        // A mid blue window background: text and the live red are hard to read on it.
        c.insert("color.surface.base".into(), hex("#4a7bd0"));
        assert!(!a.issues(&c).is_empty());
        let fixed = a.fix(&c, "color.surface.base").unwrap();
        c.insert("color.surface.base".into(), fixed);
        assert!(
            a.issues(&c)
                .iter()
                .all(|i| !i.tokens().contains(&"color.surface.base")),
            "{:?}",
            a.issues(&c)
        );
        // Still blue: the hue did not turn.
        assert!(fixed.b > fixed.r, "{fixed}");
    }

    #[test]
    fn derived_palettes_pass_every_rule_for_light_and_dark_choices() {
        let f = file();
        let live = f.fixed["color.state.live"];
        let cases = [
            (
                Base::Dark,
                ["#101828", "#f2f4f7", "#2e90fa"],
                "color.surface.base",
            ),
            (
                Base::Dark,
                ["#f7f7f2", "#1d1d1b", "#0e7c66"],
                "color.surface.base",
            ),
            (
                Base::Light,
                ["#ffffff", "#111111", "#7a5af8"],
                "color.accent",
            ),
            (
                Base::Light,
                ["#1b1b2f", "#e0e0e0", "#f79009"],
                "color.text.primary",
            ),
            (
                Base::HighContrast,
                ["#000000", "#ffffff", "#36bffa"],
                "color.accent",
            ),
        ];
        for (base, main, changed) in cases {
            let t = f.built_in_theme(base);
            let a = Assist::new(t.high_contrast(), live);
            let out = a.derive(&t.colors, main.map(hex), changed);
            assert_eq!(
                out[changed],
                hex(main[MAIN.iter().position(|m| *m == changed).unwrap()])
            );
            let mut body = rbs_ui::tokens::ThemeBody::new(base);
            body.colors = out.clone();
            assert!(
                f.resolve(&body).is_ok(),
                "{base:?} {main:?}: {:?}",
                f.resolve(&body).err()
            );
        }
    }

    #[test]
    fn a_light_background_turns_light_text_dark() {
        let f = file();
        let t = f.built_in_theme(Base::Dark);
        let a = Assist::new(false, f.fixed["color.state.live"]);
        let main = [
            hex("#fafafa"),
            t.colors["color.text.primary"],
            t.colors["color.accent"],
        ];
        let out = a.derive(&t.colors, main, "color.surface.base");
        assert!(
            lab(out["color.text.primary"]).0 < 50.0,
            "{}",
            out["color.text.primary"]
        );
    }
}
