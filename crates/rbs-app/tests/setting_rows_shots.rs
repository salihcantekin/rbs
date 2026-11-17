//! Screenshots of the Recording settings page (Separate files, Markers and project) and
//! the replay buffer card, in Dark and Light, for the owner's approval of the row
//! alignment change (CLAUDE.md rule 16). Writes raw RGBA files to `RBS_SHOTS_DIR` with
//! Skia's real pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test setting_rows_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{ReplayBuffer, SettingsWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn setting_rows_screenshots() {
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
        let w = SettingsWindow::new().expect("the settings window");
        let theme = file
            .resolve(&ThemeBody::new(base))
            .expect("the theme resolves");
        let s = slot(&theme, "").expect("a slot");
        apply_tokens(
            &w.global::<Theme>(),
            &w.global::<Tokens>(),
            &file,
            [s.clone(), s],
        )
        .expect("the tokens apply");
        let st = w.global::<Studio>();
        st.set_rec_separate_names("Shure".into());
        st.set_rec_separate_counts("1 sound".into());
        st.set_rec_separate_limit(3);
        st.set_rec_project(1);
        st.set_rec_marker_scenes(true);
        let r = w.global::<ReplayBuffer>();
        r.set_can_arm(true);
        r.set_status("Not armed. Nothing is kept.".into());
        r.set_saved_name("RBS Replay 2026-11-17 14-09-32.mkv".into());
        w.set_section(1);
        w.window().set_size(slint::LogicalSize::new(960.0, 5200.0));
        w.show().expect("show");
        let px = w.window().take_snapshot().expect("a snapshot");
        let path = dir.join(format!("recording-{b}-{}x{}.rgba", px.width(), px.height()));
        std::fs::write(&path, px.as_bytes()).expect("written");
        w.hide().expect("hide");
    }
}
