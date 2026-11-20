//! Screenshots of the quit question in the window frame (ADR-0156 decision 1): while
//! streaming, while recording and while doing both, in Dark and Light, for the owner's
//! approval of the visual change (CLAUDE.md rule 16). It writes raw RGBA files to
//! `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and run
//! by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test quit_prompt_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, Shell, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn shot(file: &TokenFile, base: Base, ending: i32, name: &str, dir: &std::path::Path) {
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
    g.set_quit_ending(ending);
    g.set_quit_confirm(true);
    w.window().set_size(slint::LogicalSize::new(1280.0, 240.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn quit_prompt_screenshots() {
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
        for (ending, what) in [(1, "stream"), (2, "recording"), (3, "both")] {
            shot(&file, base, ending, &format!("{what}-{b}"), &dir);
        }
    }
}
