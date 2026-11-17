//! Screenshots of the controls modernised on 2026-11-17 (fields, Checkbox, Slider,
//! cards, section list, panel tabs, ChoiceCard, colour swatch) in Dark and Light, in
//! every theme effects setting, for the owner's approval (CLAUDE.md rule 16). Writes raw
//! RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is
//! ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test modernize_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, SettingsWindow, StackData, TabData, Theme, Tokens};
use rbs_ui::tokens::{Base, Effects, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn modernize_screenshots() {
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
    let only = std::env::var("RBS_SHOTS_SECTIONS").unwrap_or_else(|_| "4,2,5".into());
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        for (fx, f) in [
            (Effects::Calm, "calm"),
            (Effects::Glow, "glow"),
            (Effects::Glass, "glass"),
            (Effects::Tactile, "tactile"),
        ] {
            let mut body = ThemeBody::new(base);
            body.set_effects(fx);
            let theme = file.resolve(&body).expect("the theme resolves");
            let s = slot(&theme, "").expect("a slot");
            for section in only.split(',').filter_map(|v| v.parse::<i32>().ok()) {
                let w = SettingsWindow::new().expect("the settings window");
                apply_tokens(
                    &w.global::<Theme>(),
                    &w.global::<Tokens>(),
                    &file,
                    [s.clone(), s.clone()],
                )
                .expect("the tokens apply");
                w.set_section(section);
                w.window()
                    .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                        scale_factor: 2.0,
                    });
                w.window().set_size(slint::PhysicalSize::new(1920, 2800));
                w.show().expect("show");
                let px = w.window().take_snapshot().expect("a snapshot");
                let path = dir.join(format!(
                    "settings{section}-{b}-{f}-{}x{}.rgba",
                    px.width(),
                    px.height()
                ));
                std::fs::write(&path, px.as_bytes()).expect("written");
                w.hide().expect("hide");
            }
            let m = MainWindow::new().expect("the main window");
            apply_tokens(
                &m.global::<Theme>(),
                &m.global::<Tokens>(),
                &file,
                [s.clone(), s],
            )
            .expect("the tokens apply");
            // Two panel cards: one with three tabs (the second shown), one alone.
            let tab = |id: &str, title: &str| TabData {
                id: id.into(),
                title: title.into(),
                known: true,
                icon: "".into(),
            };
            let stack = |x: f32, tabs: Vec<TabData>, active: i32| StackData {
                window: 0,
                stack: 0,
                x,
                y: 48.0,
                w: 400.0,
                h: 240.0,
                tabs: slint::ModelRc::new(slint::VecModel::from(tabs)),
                active,
                collapsed: false,
                can_collapse: true,
                can_hide: true,
            };
            m.set_stacks(slint::ModelRc::new(slint::VecModel::from(vec![
                stack(
                    16.0,
                    vec![
                        tab("scenes", "Scenes"),
                        tab("sources", "Sources"),
                        tab("mixer", "Mixer"),
                    ],
                    1,
                ),
                stack(432.0, vec![tab("stats", "Stats")], 0),
            ])));
            m.window()
                .dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
                    scale_factor: 2.0,
                });
            m.window().set_size(slint::PhysicalSize::new(1720, 640));
            m.show().expect("show");
            let px = m.window().take_snapshot().expect("a snapshot");
            let path = dir.join(format!("main-{b}-{f}-{}x{}.rgba", px.width(), px.height()));
            std::fs::write(&path, px.as_bytes()).expect("written");
            m.hide().expect("hide");
        }
    }
}
