//! Theme documents, the theme library and the appearance choice (themes spec).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod common;

use common::TempDir;
use rbs_ui::tokens::{Base, Color, MAX_THEME_FILE, ThemeBody, Tokens};
use rbs_user_data::document::{self, Document};
use rbs_user_data::theme::read_base;
use rbs_user_data::{
    APPEARANCE_FILE, Appearance, AppearanceMode, DataError, EntryId, THEMES_DIR, ThemeId, UserData,
};

const MIDNIGHT: &str = r##"format = 1
kind = "theme"
name = "Midnight"
base = "dark"
author = "Salih"
description = "Deep blue with a warm accent"

[colors]
"color.accent" = "#ff9f43"
"color.sparkle" = "#123456"
"color.surface.base" = "#0b1020"

[font]
"font.family" = ["Inter", "Segoe UI"]

[length]
"radius.md" = 8
"##;

#[test]
fn a_theme_document_round_trips_with_stable_text() {
    let read = document::parse::<ThemeBody>(MIDNIGHT).unwrap();
    assert_eq!(read.name.as_deref(), Some("Midnight"));
    let theme = read.document;
    assert_eq!(theme.base, Base::Dark);
    assert_eq!(
        theme.colors["color.accent"],
        Color::parse("#ff9f43").unwrap()
    );
    assert_eq!(theme.unknown_names(), ["color.sparkle"]);
    let text = document::render(&theme, Some("Midnight")).unwrap();
    let again = document::parse::<ThemeBody>(&text).unwrap().document;
    assert_eq!(again, theme);
    assert_eq!(document::render(&again, Some("Midnight")).unwrap(), text);
    // Overrides are written sorted by name, unknown names in their table.
    let accent = text.find("\"color.accent\"").unwrap();
    let sparkle = text.find("\"color.sparkle\"").unwrap();
    let surface = text.find("\"color.surface.base\"").unwrap();
    assert!(accent < sparkle && sparkle < surface, "{text}");
    // And it resolves.
    let resolved = Tokens::built_in().unwrap().resolve(&theme).unwrap();
    assert_eq!(resolved.font_family, ["Inter", "Segoe UI"]);
}

#[test]
fn refused_theme_documents() {
    let refused = |text: &str| document::parse::<ThemeBody>(text).unwrap_err();
    let head = "format = 1\nkind = \"theme\"\nname = \"x\"\n";
    for (body, why) in [
        ("", "base is missing"),
        ("base = \"sepia\"\n", "sepia"),
        ("base = \"dark\"\nshade = 1\n", "unknown key"),
        (
            "base = \"dark\"\n[length]\n\"color.accent\" = 1\n",
            "belongs in [colors]",
        ),
        ("base = \"dark\"\nauthor = \"a\\u0007\"\n", "control"),
    ] {
        match refused(&format!("{head}{body}")) {
            DataError::Invalid(r) => assert!(r.contains(why), "{r}"),
            e => panic!("{e:?}"),
        }
    }
    assert!(matches!(
        refused("format = 2\nkind = \"theme\"\nbase = \"dark\"\n"),
        DataError::Newer { found: 2, .. }
    ));
    assert!(matches!(
        refused("format = 1\nkind = \"output-profile\"\n"),
        DataError::WrongKind { .. }
    ));
}

#[test]
fn theme_files_over_64_kib_are_not_read_or_written() {
    let d = TempDir::new("theme-size");
    let path = d.path().join("big.toml");
    let filler = format!("# {}\n", "x".repeat(200));
    let mut text = MIDNIGHT.to_owned();
    while (text.len() as u64) <= MAX_THEME_FILE {
        text.push_str(&filler);
    }
    std::fs::write(&path, &text).unwrap();
    assert!(matches!(
        document::read::<ThemeBody>(&path),
        Err(DataError::Invalid(_))
    ));
    // A body that would render larger is never written.
    let mut theme = ThemeBody::new(Base::Light);
    let unknown = theme.unknown.entry("length".into()).or_default();
    for i in 0..4000 {
        unknown.insert(format!("size.future-number-{i}"), toml::Value::Integer(1));
    }
    assert!(document::render(&theme, Some("Big")).is_err());
    assert_eq!(<ThemeBody as Document>::MAX_SIZE, MAX_THEME_FILE);
}

#[test]
fn themes_live_in_their_own_library() {
    let d = TempDir::new("theme-library");
    let data = UserData::new(d.path());
    let lock = data.try_lock().unwrap().unwrap();
    let themes = data.themes();
    let theme = document::parse::<ThemeBody>(MIDNIGHT).unwrap().document;
    let id = themes.create(&lock, "Midnight", &theme).unwrap();
    assert_eq!(id.as_str(), "midnight");
    assert!(d.path().join(THEMES_DIR).join("midnight.toml").exists());
    let copy = themes.duplicate(&lock, &id, "Midnight copy").unwrap();
    assert_eq!(themes.load(&copy).unwrap().document, theme);

    // Export and import into another user's data: one file, nothing scrubbed.
    let out = d.path().join("shared-midnight.toml");
    themes.export(&id, &out).unwrap();
    let other = TempDir::new("theme-library-other");
    let other_data = UserData::new(other.path());
    let other_lock = other_data.try_lock().unwrap().unwrap();
    let (new_id, name) = other_data.themes().import(&other_lock, &out).unwrap();
    assert_eq!(name, "Midnight");
    assert_eq!(other_data.themes().load(&new_id).unwrap().document, theme);
    // A profile is not imported as a theme.
    let profile = d.path().join("profile.toml");
    std::fs::write(
        &profile,
        "format = 1\nkind = \"output-profile\"\nname = \"P\"\n",
    )
    .unwrap();
    assert!(matches!(
        other_data.themes().import(&other_lock, &profile),
        Err(DataError::WrongKind { .. })
    ));
}

#[test]
fn appearance_defaults_round_trip_and_unreadable_file() {
    let d = TempDir::new("appearance");
    let data = UserData::new(d.path());
    let (a, err) = data.appearance();
    assert_eq!(a, Appearance::default());
    assert!(err.is_none());
    assert_eq!(
        a.slots(),
        [
            &ThemeId::BuiltIn(Base::Dark),
            &ThemeId::BuiltIn(Base::Light)
        ]
    );

    let lock = data.try_lock().unwrap().unwrap();
    let midnight = ThemeId::User(EntryId::parse("midnight").unwrap());
    let chosen = Appearance::new(
        AppearanceMode::Fixed,
        midnight.clone(),
        ThemeId::BuiltIn(Base::Light),
        ThemeId::BuiltIn(Base::HighContrast),
    );
    data.set_appearance(&lock, &chosen).unwrap();
    let (read, err) = data.appearance();
    assert!(err.is_none());
    assert_eq!(read, chosen);
    assert_eq!(
        chosen.slots(),
        [
            &ThemeId::BuiltIn(Base::HighContrast),
            &ThemeId::BuiltIn(Base::HighContrast)
        ]
    );
    let text = std::fs::read_to_string(d.path().join(APPEARANCE_FILE)).unwrap();
    assert!(text.contains("dark = \"midnight\""), "{text}");
    assert!(text.contains("theme = \"builtin:high-contrast\""), "{text}");

    // Missing keys take their defaults.
    std::fs::write(
        d.path().join(APPEARANCE_FILE),
        "format = 1\nkind = \"appearance\"\nmode = \"fixed\"\n",
    )
    .unwrap();
    let (a, err) = data.appearance();
    assert!(err.is_none());
    assert_eq!(a.mode, AppearanceMode::Fixed);
    assert_eq!(a.theme, ThemeId::BuiltIn(Base::Dark));

    // An unreadable file gives the defaults and the reason, and is left as it is.
    let broken = "format = 1\nkind = \"appearance\"\nmode = \"sepia\"\n";
    std::fs::write(d.path().join(APPEARANCE_FILE), broken).unwrap();
    let (a, err) = data.appearance();
    assert_eq!(a, Appearance::default());
    assert!(matches!(err, Some(DataError::Invalid(_))));
    assert_eq!(
        std::fs::read_to_string(d.path().join(APPEARANCE_FILE)).unwrap(),
        broken
    );
}

#[test]
fn theme_ids() {
    assert_eq!(
        ThemeId::parse("builtin:light"),
        Some(ThemeId::BuiltIn(Base::Light))
    );
    assert_eq!(ThemeId::parse("builtin:sepia"), None);
    assert_eq!(
        ThemeId::parse("midnight"),
        Some(ThemeId::User(EntryId::parse("midnight").unwrap()))
    );
    assert_eq!(ThemeId::parse("Mid Night"), None);
    assert_eq!(
        ThemeId::BuiltIn(Base::HighContrast).to_string(),
        "builtin:high-contrast"
    );
}

#[test]
fn the_base_of_a_broken_theme_is_still_found() {
    let d = TempDir::new("theme-base");
    let p = d.path().join("t.toml");
    std::fs::write(
        &p,
        "format = 1\nkind = \"theme\"\nbase = \"high-contrast\"\nbogus = 1\n",
    )
    .unwrap();
    assert!(document::read::<ThemeBody>(&p).is_err());
    assert_eq!(read_base(&p), Some(Base::HighContrast));
    // Syntax from a newer RBS this build cannot parse.
    std::fs::write(
        &p,
        "format = 9\nkind = \"theme\"\nbase = \"light\"\nx = <new>\n",
    )
    .unwrap();
    assert_eq!(read_base(&p), Some(Base::Light));
    std::fs::write(&p, "format = 1\nkind = \"theme\"\n").unwrap();
    assert_eq!(read_base(&p), None);
    assert_eq!(read_base(&d.path().join("missing.toml")), None);
}
