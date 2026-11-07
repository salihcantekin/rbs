//! The look of every window: the themes in use, resolved and copied into each window's
//! `Theme` and `Tokens` globals (themes spec, "Applying"; ADR-0057).
//!
//! Two slots: slot 0 is used while the OS scheme is dark, slot 1 while it is light. In
//! `system` mode they hold the appearance's `dark` and `light` themes, in `fixed` mode
//! both hold its `theme`. A user theme that cannot be used falls back to the built-in
//! theme of its base, so a high-contrast user keeps high contrast. `RBS_TOKENS=<file>`
//! (another complete token file) and `RBS_THEME=<dark|light|high-contrast>` are
//! development aids that win over the appearance choice.

use std::path::Path;

use rbs_ui::tokens::{Base, ResolvedTheme, Tokens as TokenFile};
use rbs_user_data::theme::read_base;
use rbs_user_data::{Appearance, ThemeId, UserData};

use crate::ui::{Theme, ThemeSlot, Tokens};

include!(concat!(env!("OUT_DIR"), "/tokens_apply.rs"));

/// Text a font must draw for the UI language (English until Fluent strings exist):
/// a family without these glyphs is skipped (rule U8).
pub const SAMPLE_TEXT: &str = "AaBbCcGgQqXxYyZz0123456789";

/// Which font families are installed and draw a text.
pub trait FontCatalog {
    /// True when `family` is installed and its default face maps every character of
    /// `sample`.
    fn has_family(&mut self, family: &str, sample: &str) -> bool;
}

/// The fonts of this computer, read only when a theme names a family (P13).
#[derive(Default)]
pub struct SystemFonts(Option<fontique::Collection>);

impl FontCatalog for SystemFonts {
    fn has_family(&mut self, family: &str, sample: &str) -> bool {
        let fonts = self.0.get_or_insert_with(|| {
            fontique::Collection::new(fontique::CollectionOptions::default())
        });
        let Some(info) = fonts.family_by_name(family) else {
            return false;
        };
        let Some(face) = info.default_font() else {
            return false;
        };
        let Some(data) = face.load(None) else {
            return false;
        };
        let Some(map) = face.charmap_index().charmap(data.as_ref()) else {
            return false;
        };
        sample.chars().all(|c| map.map(c).is_some_and(|g| g != 0))
    }
}

/// The first family of `families` the catalogue has with `sample`; empty (the OS UI
/// font) if none.
pub fn choose_font(families: &[String], fonts: &mut dyn FontCatalog, sample: &str) -> String {
    families
        .iter()
        .find(|f| fonts.has_family(f, sample))
        .cloned()
        .unwrap_or_default()
}

/// The look in use: the token file the themes resolve on and the two slots.
pub struct Look {
    /// The built-in token file, or the `RBS_TOKENS` one.
    pub file: TokenFile,
    /// The resolved themes of the two slots (OS dark, OS light).
    pub themes: [ResolvedTheme; 2],
    /// The window font of each slot (empty: the OS UI font).
    pub fonts: [String; 2],
}

/// Reads the look: the token file, the appearance choice and the themes it names.
/// `None` keeps the values compiled into `tokens.slint` (only if the built-in file
/// stopped parsing after the build). The `String` says what could not be used.
pub fn load(data: &UserData, fonts: &mut dyn FontCatalog) -> (Option<Look>, Option<String>) {
    let mut notes = Vec::new();
    let file = match std::env::var_os("RBS_TOKENS") {
        Some(path) if !path.is_empty() => match read(Path::new(&path)) {
            Ok(file) => Some(file),
            Err(why) => {
                notes.push(why);
                TokenFile::built_in().ok()
            }
        },
        _ => TokenFile::built_in().ok(),
    };
    let Some(file) = file else {
        return (None, join(notes));
    };
    let appearance = match dev_theme() {
        Some(base) => Appearance::new(
            rbs_user_data::AppearanceMode::Fixed,
            ThemeId::BuiltIn(Base::Dark),
            ThemeId::BuiltIn(Base::Light),
            ThemeId::BuiltIn(base),
        ),
        None => {
            let (a, err) = data.appearance();
            if let Some(e) = err {
                notes.push(format!("The appearance settings cannot be read: {e}"));
            }
            a
        }
    };
    let [dark, light] = appearance.slots();
    let themes = [
        resolve(&file, data, dark, Base::Dark, &mut notes),
        resolve(&file, data, light, Base::Light, &mut notes),
    ];
    let fonts = [
        choose_font(&themes[0].font_family, fonts, SAMPLE_TEXT),
        choose_font(&themes[1].font_family, fonts, SAMPLE_TEXT),
    ];
    (
        Some(Look {
            file,
            themes,
            fonts,
        }),
        join(notes),
    )
}

fn join(notes: Vec<String>) -> Option<String> {
    (!notes.is_empty()).then(|| notes.join(" "))
}

/// The theme `id` resolved on `file`; when it cannot be used, the built-in theme of
/// its base if the base can be read, else `fallback` (ADR-0057 decision 5).
pub fn resolve(
    file: &TokenFile,
    data: &UserData,
    id: &ThemeId,
    fallback: Base,
    notes: &mut Vec<String>,
) -> ResolvedTheme {
    let user = match id {
        ThemeId::BuiltIn(base) => return file.built_in_theme(*base),
        ThemeId::User(user) => user,
    };
    let themes = data.themes();
    let (base, why) = match themes.load(user) {
        Ok(entry) => match file.resolve(&entry.document) {
            Ok(theme) => return theme,
            Err(problems) => (
                entry.document.base,
                format!("The theme \"{}\" is not used: {problems}.", entry.name),
            ),
        },
        Err(e) => (
            read_base(&themes.path(user)).unwrap_or(fallback),
            format!("The theme \"{user}\" cannot be read: {e}."),
        ),
    };
    notes.push(format!(
        "{why} The built-in {} theme is used instead.",
        match base {
            Base::Dark => "Dark",
            Base::Light => "Light",
            Base::HighContrast => "High contrast",
        }
    ));
    file.built_in_theme(base)
}

fn read(path: &Path) -> Result<TokenFile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("The token file {} cannot be read: {e}", path.display()))?;
    TokenFile::parse(&text)
        .map_err(|e| format!("The token file {} is not used: {e}", path.display()))
}

/// The theme `RBS_THEME` asks for.
pub fn dev_theme() -> Option<Base> {
    std::env::var("RBS_THEME")
        .ok()
        .and_then(|n| Base::from_name(&n))
}

/// Gives a window the look in use.
pub fn apply(theme: &Theme<'_>, tokens: &Tokens<'_>, look: Option<&Look>) {
    let Some(look) = look else {
        return;
    };
    let result = slot(&look.themes[0], &look.fonts[0])
        .and_then(|dark| Ok([dark, slot(&look.themes[1], &look.fonts[1])?]))
        .and_then(|slots| apply_tokens(theme, tokens, &look.file, slots));
    if let Err(why) = result {
        crate::studio_ui::log(&format!("token file not applied: {why}"));
    }
}

#[cfg(test)]
mod drawn;

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use rbs_ui::tokens::{BUILT_IN, Base, Color, ThemeBody, Tokens as TokenFile, TypeStyle};
    use rbs_user_data::{Appearance, AppearanceMode, EntryId, ThemeId, UserData};
    use slint::{ComponentHandle, Model};

    use super::{FontCatalog, Look, SLOT_TOKENS, apply_tokens, choose_font, resolve, slot};
    use crate::ui::{MainWindow, Theme, Tokens};

    fn window() -> MainWindow {
        i_slint_backend_testing::init_no_event_loop();
        MainWindow::new().expect("a headless window")
    }

    fn argb(c: Color) -> slint::Color {
        slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let d =
                std::env::temp_dir().join(format!("rbs-app-look-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn slots(a: &ResolvedPair) -> [crate::ui::ThemeSlot; 2] {
        [slot(&a.0, "").unwrap(), slot(&a.1, "").unwrap()]
    }
    type ResolvedPair = (rbs_ui::tokens::ResolvedTheme, rbs_ui::tokens::ResolvedTheme);

    fn built_in_pair(file: &TokenFile) -> ResolvedPair {
        (
            file.built_in_theme(Base::Dark),
            file.built_in_theme(Base::Light),
        )
    }

    /// The built-in themes applied at run time give the values compiled into
    /// `tokens.slint`: the defaults and the generated setters agree.
    #[test]
    fn built_in_file_matches_the_compiled_defaults() {
        let w = window();
        let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
        let before = (
            theme.get_slots().iter().collect::<Vec<_>>(),
            tokens.get_space_4(),
            tokens.get_motion_fast(),
            tokens.get_type_title_size(),
            tokens.get_type_title_weight(),
            tokens.get_size_window_min_w(),
        );
        let file = TokenFile::built_in().unwrap();
        apply_tokens(&theme, &tokens, &file, slots(&built_in_pair(&file))).unwrap();
        let after = (
            theme.get_slots().iter().collect::<Vec<_>>(),
            tokens.get_space_4(),
            tokens.get_motion_fast(),
            tokens.get_type_title_size(),
            tokens.get_type_title_weight(),
            tokens.get_size_window_min_w(),
        );
        assert_eq!(before, after);
        assert_eq!(after.0.len(), 2);
        assert_eq!(theme.get_font_family(), "");
    }

    /// Every themeable token is in the slot, and nothing else is (golden list).
    #[test]
    fn every_themeable_token_has_a_slot_field() {
        let file = TokenFile::built_in().unwrap();
        let mut expected: BTreeSet<&str> = rbs_ui::tokens::THEMEABLE
            .iter()
            .map(|t| t.name)
            .filter(|n| *n != rbs_ui::tokens::FONT_FAMILY)
            .collect();
        for n in file.themes[0].1.keys() {
            expected.insert(n);
        }
        let got: BTreeSet<&str> = SLOT_TOKENS.iter().copied().collect();
        assert_eq!(got, expected);
    }

    /// A user theme changes colours, lengths, durations, text styles and the font of
    /// an open window and of a window opened later; slot 1 holds the light theme.
    #[test]
    fn a_resolved_theme_changes_the_window_at_run_time() {
        let file = TokenFile::built_in().unwrap();
        let mut body = ThemeBody::new(Base::Dark);
        body.colors.insert(
            "color.surface.base".into(),
            Color::parse("#0b1020").unwrap(),
        );
        body.length.insert("radius.md".into(), 8.0);
        body.duration.insert("motion.fast".into(), 150);
        body.type_styles.insert(
            "type.title".into(),
            TypeStyle {
                size: 16,
                line: 22,
                weight: 700,
            },
        );
        let dark = file.resolve(&body).unwrap();
        let light = file.built_in_theme(Base::Light);
        let pair = [slot(&dark, "Inter").unwrap(), slot(&light, "").unwrap()];
        let first = window();
        for w in [first, MainWindow::new().expect("a window opened later")] {
            let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
            apply_tokens(&theme, &tokens, &file, pair.clone()).unwrap();
            // The test backend reports no light scheme: slot 0 is active.
            assert_eq!(theme.get_active(), 0);
            assert_eq!(
                theme.get_surface_base(),
                slint::Color::from_rgb_u8(0x0b, 0x10, 0x20)
            );
            assert_eq!(tokens.get_radius_md(), 8.0);
            assert_eq!(tokens.get_motion_fast(), 150);
            assert_eq!(tokens.get_type_title_size(), 16.0);
            assert_eq!(tokens.get_type_title_weight(), 700);
            assert_eq!(theme.get_font_family(), "Inter");
            assert!(!theme.get_high_contrast());
            let slots: Vec<_> = theme.get_slots().iter().collect();
            assert_eq!(
                slots[1].surface_base,
                argb(light.colors["color.surface.base"])
            );
        }
    }

    /// A high-contrast theme in a slot sets `Theme.high-contrast` while that slot shows.
    #[test]
    fn high_contrast_follows_the_slot() {
        let file = TokenFile::built_in().unwrap();
        let hc = file.built_in_theme(Base::HighContrast);
        let w = window();
        let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
        let pair = [slot(&hc, "").unwrap(), slot(&hc, "").unwrap()];
        apply_tokens(&theme, &tokens, &file, pair).unwrap();
        assert!(theme.get_high_contrast());
        assert_eq!(
            theme.get_surface_base(),
            argb(hc.colors["color.surface.base"])
        );
    }

    /// A token file without a single value the app uses changes nothing.
    #[test]
    fn incomplete_file_changes_nothing() {
        let mut file =
            TokenFile::parse(&BUILT_IN.replace("\"step.nudge\" = 8", "\"step.nudge\" = 10"))
                .unwrap();
        let pair = slots(&built_in_pair(&file));
        file.length.remove("size.window.w");
        let w = window();
        let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
        assert!(apply_tokens(&theme, &tokens, &file, pair).is_err());
        assert_eq!(tokens.get_step_nudge(), 8.0);
    }

    struct Fake(&'static [&'static str]);
    impl FontCatalog for Fake {
        fn has_family(&mut self, family: &str, sample: &str) -> bool {
            assert!(!sample.is_empty());
            self.0.contains(&family)
        }
    }

    #[test]
    fn the_first_installed_family_is_chosen() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let mut fonts = Fake(&["Segoe UI", "Helvetica Neue"]);
        assert_eq!(
            choose_font(
                &names(&["Inter", "Segoe UI", "Helvetica Neue"]),
                &mut fonts,
                "Aa"
            ),
            "Segoe UI"
        );
        assert_eq!(choose_font(&names(&["Inter"]), &mut fonts, "Aa"), "");
        assert_eq!(choose_font(&[], &mut fonts, "Aa"), "");
    }

    #[test]
    fn unusable_themes_fall_back_to_their_base() {
        let d = TempDir::new("fallback");
        let data = UserData::new(&d.0);
        let lock = data.try_lock().unwrap().unwrap();
        let file = TokenFile::built_in().unwrap();
        let mut notes = Vec::new();

        // A usable user theme resolves.
        let mut good = ThemeBody::new(Base::Light);
        good.length.insert("radius.md".into(), 8.0);
        let id = data.themes().create(&lock, "Soft", &good).unwrap();
        let r = resolve(&file, &data, &ThemeId::User(id), Base::Dark, &mut notes);
        assert_eq!(r.length["radius.md"], 8.0);
        assert!(notes.is_empty());

        // One that fails a rule: its base's built-in theme (high contrast kept).
        let mut failing = ThemeBody::new(Base::HighContrast);
        failing
            .colors
            .insert("color.accent".into(), Color::parse("#db2d35").unwrap());
        let id = data.themes().create(&lock, "Loud", &failing).unwrap();
        let r = resolve(&file, &data, &ThemeId::User(id), Base::Dark, &mut notes);
        assert_eq!(r, file.built_in_theme(Base::HighContrast));
        assert!(
            notes[0].contains("Loud") && notes[0].contains("High contrast"),
            "{notes:?}"
        );

        // One that does not load: the base read from its text.
        let broken = data.themes().path(&EntryId::parse("broken").unwrap());
        std::fs::write(
            &broken,
            "format = 1\nkind = \"theme\"\nbase = \"light\"\nbogus = 1\n",
        )
        .unwrap();
        let r = resolve(
            &file,
            &data,
            &ThemeId::User(EntryId::parse("broken").unwrap()),
            Base::Dark,
            &mut notes,
        );
        assert_eq!(r, file.built_in_theme(Base::Light));

        // A missing one, and a newer one: the slot's own default.
        let missing = ThemeId::User(EntryId::parse("gone").unwrap());
        assert_eq!(
            resolve(&file, &data, &missing, Base::Light, &mut notes),
            file.built_in_theme(Base::Light)
        );
        let newer = data.themes().path(&EntryId::parse("future").unwrap());
        std::fs::write(
            &newer,
            "format = 7\nkind = \"theme\"\nname = \"F\"\nx = <new>\n",
        )
        .unwrap();
        let future = ThemeId::User(EntryId::parse("future").unwrap());
        assert_eq!(
            resolve(&file, &data, &future, Base::Dark, &mut notes),
            file.built_in_theme(Base::Dark)
        );
        assert_eq!(notes.len(), 4);
    }

    #[test]
    fn appearance_modes_fill_the_slots() {
        let d = TempDir::new("modes");
        let data = UserData::new(&d.0);
        let lock = data.try_lock().unwrap().unwrap();
        let mut warm = ThemeBody::new(Base::Light);
        warm.length.insert("radius.md".into(), 9.0);
        let id = data.themes().create(&lock, "Warm", &warm).unwrap();
        let mut fonts = Fake(&[]);

        let system = Appearance::new(
            AppearanceMode::System,
            ThemeId::BuiltIn(Base::HighContrast),
            ThemeId::User(id.clone()),
            ThemeId::BuiltIn(Base::Dark),
        );
        data.set_appearance(&lock, &system).unwrap();
        let (look, note) = super::load(&data, &mut fonts);
        let Look { themes, .. } = look.unwrap();
        assert!(note.is_none(), "{note:?}");
        assert!(themes[0].high_contrast());
        assert_eq!(themes[1].length["radius.md"], 9.0);

        let fixed = Appearance::new(
            AppearanceMode::Fixed,
            ThemeId::BuiltIn(Base::Dark),
            ThemeId::BuiltIn(Base::Light),
            ThemeId::User(id),
        );
        data.set_appearance(&lock, &fixed).unwrap();
        let themes = super::load(&data, &mut fonts).0.unwrap().themes;
        assert_eq!(themes[0], themes[1]);
        assert_eq!(themes[0].length["radius.md"], 9.0);
    }
}
