//! Screenshots of the silent output question and Settings › Audio's Silence check, in
//! Dark and Light, for the owner's approval of the visual change (CLAUDE.md rule 16).
//! It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a
//! check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test silent_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, SettingsWindow, Shell, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn save(px: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, dir: &std::path::Path, name: &str) {
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

fn main_shot(
    file: &TokenFile,
    base: Base,
    name: &str,
    kind: i32,
    reason: &str,
    dir: &std::path::Path,
) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = MainWindow::new().expect("the main window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    let g = w.global::<Shell>();
    g.set_silent_confirm(kind);
    g.set_silent_reason(reason.into());
    w.window().set_size(slint::LogicalSize::new(1280.0, 420.0));
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

fn settings_shot(file: &TokenFile, base: Base, name: &str, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = SettingsWindow::new().expect("the settings window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    w.global::<Studio>().set_warn_silent(true);
    // Settings › Audio (`main.rs` SECTION_AUDIO).
    w.set_section(9);
    w.window().set_size(slint::LogicalSize::new(960.0, 900.0));
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn silent_output_screenshots() {
    let dir = std::path::PathBuf::from(std::env::var_os("RBS_SHOTS_DIR").expect("RBS_SHOTS_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let backend = i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            renderer_name: Some("skia".into()),
            ..Default::default()
        },
    );
    slint::platform::set_platform(Box::new(backend)).expect("the testing platform");
    let file = TokenFile::built_in().unwrap();
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        main_shot(
            &file,
            base,
            &format!("record-{b}"),
            1,
            "No microphone is in this scene.",
            &dir,
        );
        main_shot(&file, base, &format!("live-{b}"), 2, "Mic is muted.", &dir);
        settings_shot(&file, base, &format!("settings-{b}"), &dir);
    }
}
