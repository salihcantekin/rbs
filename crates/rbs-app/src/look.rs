//! The look of every window: the token file in use, copied into each window's `Theme`
//! and `Tokens` globals when the window opens (design system spec, "Tokens").
//!
//! The values are run-time properties, so a theme can change without a new build. Until
//! user themes have their own format and setting, the file in use is the built-in one,
//! or the file named by `RBS_TOKENS` (a development aid to try token values).

use std::path::Path;

use rbs_ui::tokens::Tokens as TokenFile;

use crate::ui::{Theme, ThemeColors, Tokens};

include!(concat!(env!("OUT_DIR"), "/tokens_apply.rs"));

/// The token file to use and, when `RBS_TOKENS` names one that cannot be used, why the
/// built-in file is used instead. `None` keeps the values compiled into `tokens.slint`,
/// which are the built-in file's: the build refuses a built-in file that does not parse,
/// so that happens only if parsing changed after the build.
pub fn load() -> (Option<TokenFile>, Option<String>) {
    match std::env::var_os("RBS_TOKENS") {
        Some(path) if !path.is_empty() => match read(Path::new(&path)) {
            Ok(file) => (Some(file), None),
            Err(why) => (TokenFile::built_in().ok(), Some(why)),
        },
        _ => (TokenFile::built_in().ok(), None),
    }
}

fn read(path: &Path) -> Result<TokenFile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("The token file {} cannot be read: {e}", path.display()))?;
    TokenFile::parse(&text)
        .map_err(|e| format!("The token file {} is not used: {e}", path.display()))
}

/// The theme the OS or `RBS_THEME` asks for: -1 follows the OS.
fn choice() -> i32 {
    match std::env::var("RBS_THEME").as_deref() {
        Ok("dark") => 0,
        Ok("light") => 1,
        Ok("high-contrast") => 2,
        _ => -1,
    }
}

/// Gives a newly opened window the look in use.
pub fn apply(theme: &Theme<'_>, tokens: &Tokens<'_>, file: Option<&TokenFile>) {
    theme.set_choice(choice());
    let Some(file) = file else {
        return;
    };
    if let Err(why) = apply_tokens(theme, tokens, file) {
        crate::studio_ui::log(&format!("token file not applied: {why}"));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)] // Tests fail by panicking.

    use rbs_ui::tokens::{BUILT_IN, Color, Tokens as TokenFile};
    use slint::{ComponentHandle, Model};

    use super::apply_tokens;
    use crate::ui::{MainWindow, Theme, Tokens};

    fn window() -> MainWindow {
        i_slint_backend_testing::init_no_event_loop();
        MainWindow::new().expect("a headless window")
    }

    fn argb(c: Color) -> slint::Color {
        slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)
    }

    /// The built-in file applied at run time gives the values compiled into
    /// `tokens.slint`: the defaults and the generated setters agree.
    #[test]
    fn built_in_file_matches_the_compiled_defaults() {
        let w = window();
        let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
        let before = (
            theme.get_themes().iter().collect::<Vec<_>>(),
            tokens.get_space_4(),
            tokens.get_motion_fast(),
            tokens.get_type_title_size(),
            tokens.get_type_title_weight(),
        );
        let file = TokenFile::built_in().expect("built-in tokens");
        apply_tokens(&theme, &tokens, &file).expect("applied");
        let after = (
            theme.get_themes().iter().collect::<Vec<_>>(),
            tokens.get_space_4(),
            tokens.get_motion_fast(),
            tokens.get_type_title_size(),
            tokens.get_type_title_weight(),
        );
        assert_eq!(before, after);
        assert_eq!(after.0.len(), rbs_ui::tokens::THEMES.len());
    }

    /// Another token file changes colours, lengths, durations and text styles at run
    /// time, in the theme the window shows.
    #[test]
    fn another_file_changes_the_window_at_run_time() {
        let text = BUILT_IN
            .replace("\"space.4\" = 16", "\"space.4\" = 20")
            .replace("\"motion.fast\" = ", "\"motion.fast\" = 1")
            .replacen(
                "\"color.surface.base\" = \"#121417\"",
                "\"color.surface.base\" = \"#101215\"",
                1,
            );
        let file = TokenFile::parse(&text).expect("valid tokens");
        let w = window();
        let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
        let fast = tokens.get_motion_fast();
        theme.set_choice(0);
        apply_tokens(&theme, &tokens, &file).expect("applied");
        assert_eq!(tokens.get_space_4(), 20.0);
        assert_eq!(
            tokens.get_motion_fast(),
            i64::from(file.duration["motion.fast"])
        );
        assert_ne!(tokens.get_motion_fast(), fast);
        let dark = &file.themes[0].1;
        assert_eq!(theme.get_surface_base(), argb(dark["color.surface.base"]));
        assert_eq!(
            theme.get_surface_base(),
            slint::Color::from_rgb_u8(0x10, 0x12, 0x15)
        );
        theme.set_choice(1);
        let light = &file.themes[1].1;
        assert_eq!(theme.get_surface_base(), argb(light["color.surface.base"]));
    }

    /// A file without a token the app uses changes nothing.
    #[test]
    fn incomplete_file_changes_nothing() {
        let mut file = TokenFile::parse(&BUILT_IN.replace("\"space.4\" = 16", "\"space.4\" = 20"))
            .expect("valid tokens");
        file.type_styles.remove("type.title");
        let w = window();
        let (theme, tokens) = (w.global::<Theme>(), w.global::<Tokens>());
        assert!(apply_tokens(&theme, &tokens, &file).is_err());
        assert_eq!(tokens.get_space_4(), 16.0);
    }
}
