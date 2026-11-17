//! Screenshots of the theme editor's Shapes group after an edit that carries a
//! neighbour along (themes spec, "Editing"), in Dark and Light, for the owner's approval
//! of the visual change (CLAUDE.md rule 16). It writes raw RGBA files to
//! `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and run
//! by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test theme_editor_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{Appearance, SettingsWindow, Theme, ThemeOption, ThemeTokenRow, Tokens};
use rbs_ui::tokens::{Base, Group, Kind, THEMEABLE, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn shot(file: &TokenFile, base: Base, name: &str, dir: &std::path::Path) {
    // The user raised Corner radius to 16 px: Large corner radius came along.
    let before = ThemeBody::new(base);
    let mut body = before.clone();
    body.length.insert("radius.md".into(), 16.0);
    file.carry(&before, &mut body, "radius.md");
    let theme = file.resolve(&body).expect("the carried theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = SettingsWindow::new().expect("the settings window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    let a = w.global::<Appearance>();
    let c = |n: &str| {
        let c = theme.colors[n];
        slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)
    };
    a.set_themes(ModelRc::new(VecModel::from(vec![ThemeOption {
        name: "Rounder".into(),
        user: true,
        usable: true,
        surface: c("color.surface.base"),
        text: c("color.text.primary"),
        accent: c("color.accent"),
        ..Default::default()
    }])));
    a.set_selected(0);
    a.set_editing(true);
    a.set_editing_in_use(true);
    a.set_editor_name("Rounder".into());
    a.set_groups(ModelRc::new(VecModel::from(
        [
            "Colours",
            "Text",
            "Font",
            "Spacing and size",
            "Shapes",
            "Motion",
        ]
        .map(SharedString::from)
        .to_vec(),
    )));
    a.set_group(4);
    a.set_changed(true);
    a.set_can_save(true);
    let rows: Vec<ThemeTokenRow> = THEMEABLE
        .iter()
        .filter(|t| t.group == Group::Shapes)
        .filter_map(|t| match t.kind {
            Kind::Length { .. } => Some(ThemeTokenRow {
                name: t.name.into(),
                label: t.label.into(),
                kind: 1,
                number: theme.length[t.name],
                step: 1.0,
                unit: "px".into(),
                overridden: body.length.contains_key(t.name),
                hint: rbs_app::theme_hints::shapes(t.name).into(),
                ..Default::default()
            }),
            _ => None,
        })
        .collect();
    a.set_rows(ModelRc::new(VecModel::from(rows)));
    w.set_section(5);
    w.window().set_size(slint::LogicalSize::new(960.0, 1500.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn theme_editor_shapes_screenshots() {
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
        shot(&file, base, &format!("theme-editor-shapes-{b}"), &dir);
    }
}
