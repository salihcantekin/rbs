//! Screenshots of a camera's Source settings window with the colour space and range
//! rows (ADR-0175), in Dark and Light, for the owner's approval of the visual change
//! (CLAUDE.md rule 16). It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real
//! pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test camera_colour_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{ModeData, SourceSettingsData, SourceSettingsWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn strings(v: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        v.iter().map(|s| SharedString::from(*s)).collect::<Vec<_>>(),
    ))
}

fn camera(range: i32) -> SourceSettingsData {
    let rates = strings(&["24 fps", "25 fps", "30 fps", "50 fps", "60 fps"]);
    SourceSettingsData {
        id: 3,
        name: "Cam Link 4K".into(),
        kind_code: 2,
        modes: ModelRc::new(VecModel::from(vec![
            ModeData {
                label: "1920 \u{d7} 1080".into(),
                rates: rates.clone(),
            },
            ModeData {
                label: "1280 \u{d7} 720".into(),
                rates: rates.clone(),
            },
        ])),
        mode_labels: strings(&["1920 \u{d7} 1080", "1280 \u{d7} 720"]),
        mode_index: 0,
        rates,
        rate_index: 2,
        cursor: true,
        has_color: true,
        color_matrix: 0,
        color_range: range,
        ..Default::default()
    }
}

fn shot(file: &TokenFile, base: Base, name: &str, range: i32, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = SourceSettingsWindow::new().expect("the window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    w.global::<Studio>()
        .set_source_settings(ModelRc::new(VecModel::from(vec![camera(range)])));
    w.window().set_size(slint::LogicalSize::new(440.0, 560.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn camera_colour_screenshots() {
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
        shot(&file, base, &format!("camera-auto-{b}"), 0, &dir);
        shot(&file, base, &format!("camera-full-{b}"), 2, &dir);
    }
}
