//! Screenshots of Settings › Guests in Dark and Light for the owner's approval
//! (CLAUDE.md rule 16). Raw RGBA files go to `RBS_SHOTS_DIR` with Skia's real pixels; this
//! is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test guests_settings_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{SettingsWindow, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn guests_settings_screenshots() {
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
        for (name, sharing) in [("idle", false), ("sharing", true)] {
            let w = SettingsWindow::new().expect("the settings window");
            let resolved = file.resolve(&ThemeBody::new(base)).expect("resolves");
            let s = slot(&resolved, "").expect("a slot");
            apply_tokens(
                &w.global::<Theme>(),
                &w.global::<Tokens>(),
                &file,
                [s.clone(), s],
            )
            .expect("the tokens apply");
            let labels: Vec<SharedString> = rbs_app::guest_settings::lifetime_labels()
                .into_iter()
                .map(Into::into)
                .collect();
            w.set_guest_lifetimes(ModelRc::new(VecModel::from(labels)));
            w.set_guest_sharing(sharing);
            w.set_guest_share_text(if sharing {
                "Sharing the program with one guest.".into()
            } else {
                "Not sharing.".into()
            });
            w.set_guest_share_state(if sharing { "Live".into() } else { "".into() });
            w.set_guest_add_text("Adding a guest needs a later version of RBS.".into());
            w.set_section(14);
            w.window().set_size(slint::LogicalSize::new(960.0, 820.0));
            w.show().expect("show");
            let px = w.window().take_snapshot().expect("a snapshot");
            let path = dir.join(format!(
                "guests-{name}-{b}-{}x{}.rgba",
                px.width(),
                px.height()
            ));
            std::fs::write(path, px.as_bytes()).expect("written");
            w.hide().expect("hide");
        }
    }
}
