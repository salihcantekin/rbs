//! Screenshots of a text source's Text tab in Properties (ADR-0129 decision 13), in
//! Dark and Light, for the owner's approval of the visual change (CLAUDE.md rule 16).
//! It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a
//! check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test text_props_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{PropertiesProbe, Sections, SourceData, Studio, TextData, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn colour(hex: u32) -> slint::Color {
    slint::Color::from_argb_encoded(0xff00_0000 | hex)
}

fn text(kind: i32) -> TextData {
    TextData {
        on: true,
        id: 7,
        kind,
        text: "Welcome to the show\nLive from Istanbul".into(),
        font: "".into(),
        size: 72.0,
        bold: true,
        color: "#ffffff".into(),
        swatch: colour(0xffffff),
        align: 1,
        format_default: match kind {
            1 => "HH:mm:ss",
            _ => "mm:ss",
        }
        .into(),
        minutes: 5,
        seconds: 0,
        end_text: if kind == 2 {
            "We start now".into()
        } else {
            "".into()
        },
        outline: kind == 0,
        outline_width: 4.0,
        outline_color: "#000000".into(),
        outline_swatch: colour(0x000000),
        shadow: false,
        shadow_color: "#00000099".into(),
        shadow_swatch: slint::Color::from_argb_u8(0x99, 0, 0, 0),
        shadow_x: 4.0,
        shadow_y: 4.0,
        shadow_blur: 6.0,
        background: false,
        background_color: "#00000099".into(),
        background_swatch: slint::Color::from_argb_u8(0x99, 0, 0, 0),
        padding: 16.0,
        ..Default::default()
    }
}

fn shot(file: &TokenFile, base: Base, name: &str, data: TextData, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let w = PropertiesProbe::new().expect("the probe window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    // Outline, Shadow and Background start closed in the app; the shots open the
    // groups that are on.
    let open = [
        ("properties.text-outline", data.outline),
        ("properties.text-shadow", data.shadow),
        ("properties.text-background", data.background),
    ];
    w.global::<Sections>()
        .on_closed(move |id, _| open.iter().any(|(k, on)| *k == id.as_str() && !on));
    let g = w.global::<Studio>();
    g.set_sources(ModelRc::new(VecModel::from(vec![SourceData {
        id: 7,
        name: "Title".into(),
        kind: "Text".into(),
        icon: "text".into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: -1,
        places: 1,
        ..Default::default()
    }])));
    g.set_properties_source(7);
    g.set_properties_tab(4);
    g.set_text_props(data);
    w.window().set_size(slint::LogicalSize::new(380.0, 1180.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn text_tab_screenshots() {
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
        shot(&file, base, &format!("text-{b}"), text(0), &dir);
        shot(&file, base, &format!("countdown-{b}"), text(2), &dir);
        let mut clock = text(1);
        clock.format = "HH:mm".into();
        clock.shadow = true;
        clock.background = true;
        shot(&file, base, &format!("clock-{b}"), clock, &dir);
    }
}
