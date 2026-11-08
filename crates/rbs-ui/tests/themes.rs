//! User themes: resolving on a base, theme rules and bounds (docs/specs/themes.md).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use rbs_ui::tokens::{
    BUILT_IN, Base, Color, ColorRule, FONT_FAMILY, Kind, MAX_UNKNOWN_TOKENS, Severity,
    THEME_COLORS, THEMEABLE, ThemeBody, Tokens, TypeStyle, delta_e_2000, themeable,
};

fn tokens() -> Tokens {
    Tokens::built_in().unwrap()
}

fn color(s: &str) -> Color {
    Color::parse(s).unwrap()
}

fn body(text: &str) -> ThemeBody {
    ThemeBody::from_body(toml::from_str(text).unwrap()).unwrap()
}

/// The reasons of a theme that fails or warns, or an empty list.
fn problems(theme: &ThemeBody) -> Vec<(Option<String>, String)> {
    match tokens().resolve(theme) {
        Ok(r) => r
            .warnings
            .into_iter()
            .map(|p| (p.token, p.reason))
            .collect(),
        Err(p) => p.0.into_iter().map(|p| (p.token, p.reason)).collect(),
    }
}

/// True when the theme is refused (an error, not only warnings).
fn refused(theme: &ThemeBody) -> bool {
    tokens().resolve(theme).is_err()
}

fn fails_at(theme: &ThemeBody, token: &str) -> bool {
    problems(theme)
        .iter()
        .any(|(t, _)| t.as_deref() == Some(token))
}

#[test]
fn every_base_resolves_with_no_overrides_to_its_built_in_values() {
    let t = tokens();
    for base in Base::ALL {
        let r = t.resolve(&ThemeBody::new(base)).unwrap();
        assert_eq!(r, t.built_in_theme(base));
        assert_eq!(r.colors, t.themes[base.index()].1);
        assert_eq!(r.high_contrast(), base == Base::HighContrast);
        assert!(r.font_family.is_empty());
    }
}

#[test]
fn every_group_of_overrides_applies() {
    let theme = body(
        r##"
        base = "dark"
        author = "Salih"
        description = "Deep blue with a warm accent"
        [colors]
        "color.surface.base" = "#0b1020"
        [font]
        "font.family" = ["Inter", "Segoe UI"]
        [type]
        "type.body" = { size = 14, line = 20, weight = 400 }
        "type.label" = { size = 14, line = 20, weight = 500 }
        [length]
        "radius.md" = 8
        "size.icon.stroke" = 1.25
        [duration]
        "motion.fast" = 150
        "##,
    );
    let r = tokens().resolve(&theme).unwrap();
    assert_eq!(r.colors["color.surface.base"], color("#0b1020"));
    assert_eq!(r.font_family, ["Inter", "Segoe UI"]);
    assert_eq!(
        r.type_styles["type.body"],
        TypeStyle {
            size: 14,
            line: 20,
            weight: 400
        }
    );
    assert_eq!(r.length["radius.md"], 8.0);
    assert_eq!(r.length["size.icon.stroke"], 1.25);
    assert_eq!(r.duration["motion.fast"], 150);
    // Untouched tokens keep the base's values.
    assert_eq!(
        r.colors["color.accent"],
        tokens().themes[0].1["color.accent"]
    );
    assert_eq!(theme.author.as_deref(), Some("Salih"));
}

#[test]
fn body_round_trips_with_unknown_names_kept_in_their_tables() {
    let text = r##"
        base = "light"
        [colors]
        "color.accent" = "#1f6feb"
        "color.sparkle" = "#123456"
        [length]
        "radius.md" = 8
        "size.future" = 3
        [duration]
        "motion.fast" = "quick"
        "##;
    let theme = body(text);
    assert_eq!(theme.unknown_names(), ["color.sparkle", "size.future"]);
    let again = ThemeBody::from_body(theme.to_body()).unwrap();
    assert_eq!(again, theme);
    let rendered = toml::to_string(&theme.to_body()).unwrap();
    assert_eq!(rendered, toml::to_string(&again.to_body()).unwrap());
    // Unknown names are reported, not applied; an invalid value fails the theme.
    let p = problems(&theme);
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!(p[0].0.as_deref(), Some("motion.fast"));
}

#[test]
fn structural_errors_refuse_the_body() {
    let refused = |text: &str| ThemeBody::from_body(toml::from_str(text).unwrap()).unwrap_err();
    assert!(refused("author = \"x\"").contains("base is missing"));
    assert!(refused("base = \"sepia\"").contains("sepia"));
    assert!(refused("base = \"dark\"\nextra = 1").contains("unknown key extra"));
    assert!(refused("base = \"dark\"\ncolors = 3").contains("[colors]"));
    assert!(
        refused("base = \"dark\"\n[length]\n\"color.accent\" = 3").contains("belongs in [colors]")
    );
    assert!(refused("base = \"dark\"\n[colors]\n\"Accent\" = \"#ffffff\"").contains("invalid"));
    assert!(refused("base = \"dark\"\nauthor = \"a\\nb\"").contains("control"));
    assert!(refused("base = \"dark\"\nauthor = \"\u{202E}x\"").contains("direction"));
    let long = format!("base = \"dark\"\ndescription = \"{}\"", "x".repeat(201));
    assert!(refused(&long).contains("200"));
}

#[test]
fn fixed_and_non_themeable_tokens_are_refused() {
    let mut theme = ThemeBody::new(Base::Dark);
    theme
        .colors
        .insert("color.state.live".into(), color("#00aa00"));
    theme.length.insert("size.window.w".into(), 1600.0);
    theme.length.insert("step.nudge".into(), 4.0);
    let p = problems(&theme);
    assert!(
        p.iter()
            .any(|(t, r)| t.as_deref() == Some("color.state.live") && r.contains("U12"))
    );
    assert!(fails_at(&theme, "size.window.w"));
    assert!(fails_at(&theme, "step.nudge"));
}

#[test]
fn every_bound_holds_at_its_edges_and_fails_beyond() {
    for t in THEMEABLE {
        match t.kind {
            Kind::Length { min, max } => {
                for (v, ok) in [
                    (min, true),
                    (max, true),
                    (min - 0.5, false),
                    (max + 0.5, false),
                ] {
                    let mut theme = ThemeBody::new(Base::Dark);
                    theme.length.insert(t.name.into(), v);
                    // Keep the rules across tokens out of the way: move what this
                    // token is compared with along with it where needed.
                    let p = problems(&theme);
                    let bound_problem = p.iter().any(|(tok, r)| {
                        tok.as_deref() == Some(t.name) && r.contains("must be from")
                    });
                    assert_eq!(bound_problem, !ok, "{} = {v}: {p:?}", t.name);
                }
            }
            Kind::Duration { min, max } => {
                for (v, ok) in [(min, true), (max, true), (min - 1, false), (max + 1, false)] {
                    let mut theme = ThemeBody::new(Base::Dark);
                    theme.duration.insert(t.name.into(), v);
                    assert_eq!(fails_at(&theme, t.name), !ok, "{} = {v}", t.name);
                }
            }
            _ => {}
        }
    }
    let style = |size, line, weight| TypeStyle { size, line, weight };
    for (s, ok) in [
        (style(12, 16, 300), true),
        (style(12, 24, 900), true),
        (style(10, 16, 400), false),
        (style(12, 11, 400), false),
        (style(12, 25, 400), false),
        (style(12, 16, 299), false),
        (style(12, 16, 901), false),
    ] {
        let mut theme = ThemeBody::new(Base::Dark);
        theme.type_styles.insert("type.caption".into(), s);
        assert_eq!(fails_at(&theme, "type.caption"), !ok, "{s:?}");
    }
}

#[test]
fn font_family_names_are_checked() {
    let with = |names: Vec<&str>| {
        let mut theme = ThemeBody::new(Base::Light);
        theme.font_family = Some(names.into_iter().map(str::to_owned).collect());
        theme
    };
    assert!(!fails_at(&with(vec!["Inter", "Segoe UI"]), FONT_FAMILY));
    assert!(fails_at(&with(vec![" "]), FONT_FAMILY));
    assert!(fails_at(&with(vec!["Inter\u{2066}"]), FONT_FAMILY));
    assert!(fails_at(&with(vec!["x"; 9]), FONT_FAMILY));
    let long = "x".repeat(65);
    assert!(fails_at(&with(vec![long.as_str()]), FONT_FAMILY));
    let t = body("base = \"dark\"\n[font]\n\"font.family\" = [1]");
    assert!(fails_at(&t, FONT_FAMILY));
}

#[test]
fn every_contrast_rule_reports_its_token() {
    // The text on the accent button under the pointer, too light on a light accent.
    let mut theme = ThemeBody::new(Base::Dark);
    theme
        .colors
        .insert("color.accent.hover".into(), color("#1a2a40"));
    let p = problems(&theme);
    assert!(
        p.iter()
            .any(|(t, r)| t.as_deref() == Some("color.text.inverse") && r.contains("accent.hover")),
        "{p:?}"
    );
    // Several problems at once.
    theme
        .colors
        .insert("color.border.focus".into(), color("#252931"));
    theme
        .colors
        .insert("color.text.secondary".into(), color("#30343a"));
    let p = problems(&theme);
    for token in [
        "color.text.inverse",
        "color.border.focus",
        "color.text.secondary",
    ] {
        assert!(
            p.iter().any(|(t, _)| t.as_deref() == Some(token)),
            "{token}: {p:?}"
        );
    }
}

#[test]
fn high_contrast_base_uses_the_stricter_thresholds() {
    // 5.0:1 passes on dark (4.5) but not on high contrast (7).
    let grey = "#8c8c8c";
    assert!((4.5..7.0).contains(&color(grey).contrast(color("#141414"))));
    for (base, fails) in [(Base::Dark, false), (Base::HighContrast, true)] {
        let mut theme = ThemeBody::new(base);
        theme
            .colors
            .insert("color.surface.overlay".into(), color("#141414"));
        theme
            .colors
            .insert("color.text.secondary".into(), color(grey));
        let on_overlay = problems(&theme)
            .iter()
            .any(|(_, r)| r.starts_with("color.text.secondary on color.surface.overlay"));
        assert_eq!(on_overlay, fails, "{base:?}");
    }
}

#[test]
fn no_theme_colour_may_look_like_live() {
    let live = color("#c4262e");
    for name in ["color.accent", "color.meter.high", "color.state.warning"] {
        let mut theme = ThemeBody::new(Base::Dark);
        theme.colors.insert(name.into(), live);
        let p = problems(&theme);
        assert!(
            p.iter()
                .any(|(t, r)| t.as_deref() == Some(name) && r.contains("live colour")),
            "{name}: {p:?}"
        );
    }
    assert!(delta_e_2000(live, live) < 1e-9);
    // The built-in themes keep their distance (closest: light meter.high, about 16.2).
    let t = tokens();
    let closest = t
        .themes
        .iter()
        .flat_map(|(_, c)| THEME_COLORS.iter().map(|n| delta_e_2000(c[*n], live)))
        .fold(f64::MAX, f64::min);
    assert!((16.0..16.5).contains(&closest), "{closest}");
}

#[test]
fn rules_across_tokens() {
    let style = |size, line| TypeStyle {
        size,
        line,
        weight: 400,
    };
    let mut theme = ThemeBody::new(Base::Dark);
    theme.type_styles.insert("type.label".into(), style(20, 30));
    assert!(
        fails_at(&theme, "type.label"),
        "line must fit in size.control.sm"
    );

    let mut theme = ThemeBody::new(Base::Dark);
    theme.length.insert("size.icon.box".into(), 28.0);
    assert!(fails_at(&theme, "size.icon.box"));

    let mut theme = ThemeBody::new(Base::Dark);
    theme.length.insert("size.control.md".into(), 40.0);
    assert!(fails_at(&theme, "size.control.lg"), "sm <= md <= lg");

    let mut theme = ThemeBody::new(Base::Dark);
    theme.length.insert("space.3".into(), 20.0);
    assert!(fails_at(&theme, "space.4"));

    let mut theme = ThemeBody::new(Base::Dark);
    theme.length.insert("radius.sm".into(), 12.0);
    assert!(fails_at(&theme, "radius.md"));

    let mut theme = ThemeBody::new(Base::Dark);
    theme.type_styles.insert("type.title".into(), style(24, 30));
    assert!(fails_at(&theme, "type.heading"));

    let mut theme = ThemeBody::new(Base::Dark);
    theme.type_styles.insert("type.label".into(), style(12, 16));
    assert!(fails_at(&theme, "type.label"), "body <= label");

    let mut theme = ThemeBody::new(Base::Dark);
    theme.length.insert("size.focus-ring".into(), 6.0);
    assert!(fails_at(&theme, "size.focus-ring"), "ring <= space.1");
    // ... and passes once the spacing makes room.
    theme.length.insert("space.1".into(), 6.0);
    assert!(problems(&theme).is_empty(), "{:?}", problems(&theme));
}

#[test]
fn too_many_unknown_names_fail_the_theme() {
    let mut theme = ThemeBody::new(Base::Dark);
    let unknown = theme.unknown.entry("length".into()).or_default();
    for i in 0..=MAX_UNKNOWN_TOKENS {
        unknown.insert(format!("size.future-{i}"), toml::Value::Integer(1));
    }
    let p = problems(&theme);
    assert!(
        p.iter()
            .any(|(t, r)| t.is_none() && r.contains("does not know"))
    );
}

#[test]
fn format_1_token_files_still_parse_with_the_os_font() {
    let base = BUILT_IN.replace("\r\n", "\n");
    let v1 = base
        .replacen("format = 2", "format = 1", 1)
        .split("\n[font]")
        .next()
        .unwrap()
        .to_owned();
    let t = Tokens::parse(&v1).unwrap();
    assert!(t.font_family.is_empty());
    assert!(Tokens::parse(&base.replacen("format = 2", "format = 1", 1)).is_err());
    let no_font = base.split("\n[font]").next().unwrap();
    assert!(Tokens::parse(no_font).is_err());
}

#[test]
fn theme_rules_bind_the_built_in_file_too() {
    let base = BUILT_IN.replace("\r\n", "\n");
    let slow = base.replacen("\"motion.normal\" = 180", "\"motion.normal\" = 400", 1);
    assert!(
        Tokens::parse(&slow)
            .unwrap_err()
            .0
            .contains("motion.normal")
    );
    let red_accent = base.replacen(
        "\"color.accent\" = \"#5b9cff\"",
        "\"color.accent\" = \"#d0303a\"",
        1,
    );
    let e = Tokens::parse(&red_accent).unwrap_err().0;
    assert!(e.contains("color.accent"), "{e}");
}

#[test]
fn themeable_metadata_is_complete() {
    let t = tokens();
    for name in THEME_COLORS {
        assert!(
            matches!(themeable(name).map(|m| m.kind), Some(Kind::Color)),
            "{name}"
        );
    }
    for m in THEMEABLE {
        let known = match m.kind {
            Kind::Color => t.themes[0].1.contains_key(m.name),
            Kind::Length { .. } => t.length.contains_key(m.name),
            Kind::Duration { .. } => t.duration.contains_key(m.name),
            Kind::TypeStyle => t.type_styles.contains_key(m.name),
            Kind::FontFamily => m.name == FONT_FAMILY,
        };
        assert!(known, "{} is not a token of the built-in file", m.name);
        assert!(!m.label.is_empty());
    }
}

/// Property: any set of overrides either resolves to a theme that passes every rule
/// (checked again here as a built-in-style token file) or is refused with at least one
/// problem; nothing panics. Deterministic: a fixed-seed generator, no sleeping.
#[test]
fn any_overrides_resolve_to_a_passing_theme_or_are_refused() {
    let t = tokens();
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let (mut passed, mut warned, mut refused) = (0, 0, 0);
    for _ in 0..2000 {
        let base = Base::ALL[(next() % 3) as usize];
        let mut theme = ThemeBody::new(base);
        for _ in 0..(next() % 4) {
            let m = &THEMEABLE[(next() % THEMEABLE.len() as u64) as usize];
            match m.kind {
                Kind::Color => {
                    let c = Color {
                        r: next() as u8,
                        g: next() as u8,
                        b: next() as u8,
                        a: 255,
                    };
                    theme.colors.insert(m.name.into(), c);
                }
                Kind::Length { min, max } => {
                    let v = min - 4.0 + (next() % 1000) as f32 / 1000.0 * (max - min + 8.0);
                    theme.length.insert(m.name.into(), v);
                }
                Kind::Duration { min, max } => {
                    let v = min - 20 + (next() % u64::from(max - min + 40)) as u32;
                    theme.duration.insert(m.name.into(), v);
                }
                Kind::TypeStyle => {
                    let size = 8 + (next() % 50) as u16;
                    let line = size - 2 + (next() % 30) as u16;
                    let weight = 100 * (1 + (next() % 9) as u16);
                    theme
                        .type_styles
                        .insert(m.name.into(), TypeStyle { size, line, weight });
                }
                Kind::FontFamily => {
                    let n = (next() % 10) as usize;
                    theme.font_family = Some((0..n).map(|i| format!("Font {i}")).collect());
                }
            }
        }
        match t.resolve(&theme) {
            Ok(r) if !r.warnings.is_empty() => {
                // Only colour rules warn (ADR-0072); the theme is used as it is.
                warned += 1;
                assert!(
                    r.warnings
                        .iter()
                        .all(|w| w.severity == Severity::Warning && w.rule.is_some()),
                    "{:?}",
                    r.warnings
                );
            }
            Ok(r) => {
                passed += 1;
                // The resolved values, written as a token file, pass the file checks.
                let mut file = toml::Table::new();
                file.insert("format".into(), 2.into());
                let fixed: toml::Table = r
                    .fixed
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_string().into()))
                    .collect();
                file.insert("fixed".into(), fixed.into());
                let mut themes = toml::Table::new();
                for b in Base::ALL {
                    let colors: toml::Table = if b == base {
                        r.colors
                            .iter()
                            .map(|(k, v)| (k.clone(), v.to_string().into()))
                            .collect()
                    } else {
                        t.themes[b.index()]
                            .1
                            .iter()
                            .map(|(k, v)| (k.clone(), v.to_string().into()))
                            .collect()
                    };
                    themes.insert(b.name().into(), colors.into());
                }
                file.insert("themes".into(), themes.into());
                let length: toml::Table = r
                    .length
                    .iter()
                    .map(|(k, v)| (k.clone(), f64::from(*v).into()))
                    .collect();
                file.insert("length".into(), length.into());
                let duration: toml::Table = r
                    .duration
                    .iter()
                    .map(|(k, v)| (k.clone(), i64::from(*v).into()))
                    .collect();
                file.insert("duration".into(), duration.into());
                let type_styles: toml::Table = r
                    .type_styles
                    .iter()
                    .map(|(k, s)| {
                        let mut st = toml::Table::new();
                        st.insert("size".into(), i64::from(s.size).into());
                        st.insert("line".into(), i64::from(s.line).into());
                        st.insert("weight".into(), i64::from(s.weight).into());
                        (k.clone(), st.into())
                    })
                    .collect();
                file.insert("type".into(), type_styles.into());
                let mut font = toml::Table::new();
                font.insert(
                    FONT_FAMILY.into(),
                    toml::Value::Array(r.font_family.iter().map(|f| f.clone().into()).collect()),
                );
                file.insert("font".into(), font.into());
                let text = toml::to_string(&file).unwrap();
                if let Err(e) = Tokens::parse(&text) {
                    panic!("resolved theme fails the file checks: {e}\n{theme:?}");
                }
            }
            Err(p) => {
                refused += 1;
                assert!(p.0.iter().any(|p| p.severity == Severity::Error), "{p:?}");
            }
        }
    }
    assert!(
        passed > 100 && warned > 100 && refused > 100,
        "{passed} passed, {warned} warned, {refused} refused"
    );
}

#[test]
fn colour_rules_warn_and_the_theme_is_still_used() {
    // ADR-0072: a mid blue window background breaks the live and text contrast rules,
    // yet the theme resolves with the colour and lists the warnings.
    let mut theme = ThemeBody::new(Base::Dark);
    let blue = color("#5b8de0");
    theme.colors.insert("color.surface.base".into(), blue);
    let r = tokens()
        .resolve(&theme)
        .expect("warnings never refuse a theme");
    assert_eq!(r.colors["color.surface.base"], blue);
    assert!(r.warnings.iter().all(|w| w.severity == Severity::Warning));
    assert!(
        r.warnings.iter().any(|w| matches!(
            &w.rule,
            Some(ColorRule::Contrast { fg, bg, .. })
                if fg == "color.state.live" && bg == "color.surface.base"
        )),
        "{:?}",
        r.warnings
    );
    assert!(
        r.warnings.iter().any(|w| matches!(
            &w.rule,
            Some(ColorRule::Contrast { fg, bg, ratio, need })
                if fg == "color.text.primary" && bg == "color.surface.base" && ratio < need
        )),
        "{:?}",
        r.warnings
    );
    // A colour like live warns; a hue like live warns.
    let mut theme = ThemeBody::new(Base::Dark);
    theme.colors.insert("color.accent".into(), color("#c4262e"));
    theme
        .colors
        .insert("color.state.error".into(), color("#d02040"));
    let r = tokens().resolve(&theme).unwrap();
    assert!(r.warnings.iter().any(
        |w| matches!(&w.rule, Some(ColorRule::LiveLook { token, .. }) if token == "color.accent")
    ));
    assert!(r.warnings.iter().any(
        |w| matches!(&w.rule, Some(ColorRule::LiveHue { token }) if token == "color.state.error")
    ));
    // An error with warnings still refuses, and lists both.
    theme.length.insert("space.1".into(), -5.0);
    let p = tokens().resolve(&theme).unwrap_err();
    assert!(p.0.iter().any(|p| p.severity == Severity::Error));
    assert!(p.0.iter().any(|p| p.severity == Severity::Warning));
    // Built-in themes resolve with no warning.
    for base in Base::ALL {
        assert!(
            tokens()
                .resolve(&ThemeBody::new(base))
                .unwrap()
                .warnings
                .is_empty()
        );
    }
}

#[test]
fn a_colour_warning_alone_never_refuses() {
    let mut theme = ThemeBody::new(Base::Light);
    theme
        .colors
        .insert("color.text.primary".into(), color("#f0f0f0"));
    assert!(!refused(&theme));
    assert!(fails_at(&theme, "color.text.primary"));
}

#[test]
fn the_suggested_colour_clears_the_warnings_of_its_token() {
    let t = tokens();
    let mut theme = ThemeBody::new(Base::Dark);
    theme
        .colors
        .insert("color.surface.base".into(), color("#5b8de0"));
    let fixed = rbs_ui::tokens::suggest_colour(&t, &theme, "color.surface.base")
        .expect("a lightness that works exists");
    theme.colors.insert("color.surface.base".into(), fixed);
    let r = t.resolve(&theme).unwrap();
    assert!(
        r.warnings.iter().all(|w| w.token.as_deref() != Some("color.surface.base")
            && !matches!(&w.rule, Some(ColorRule::Contrast { bg, .. }) if bg == "color.surface.base")),
        "{:?}",
        r.warnings
    );
    // Nothing to fix: nothing suggested.
    assert!(
        rbs_ui::tokens::suggest_colour(&t, &ThemeBody::new(Base::Dark), "color.accent").is_none()
    );
}

#[test]
fn only_contrast_warnings_have_a_suggested_colour() {
    let t = tokens();
    // A hue warning cannot be fixed by lightness.
    let mut theme = ThemeBody::new(Base::Dark);
    theme
        .colors
        .insert("color.state.error".into(), color("#d02040"));
    assert!(rbs_ui::tokens::suggest_colour(&t, &theme, "color.state.error").is_none());
    // Any single colour: a suggestion never panics and, when given, is an opaque colour.
    for name in THEME_COLORS {
        let mut theme = ThemeBody::new(Base::Light);
        theme.colors.insert((*name).into(), color("#808080"));
        if let Some(c) = rbs_ui::tokens::suggest_colour(&t, &theme, name) {
            assert_eq!(c.a, 255, "{name}");
        }
    }
}
