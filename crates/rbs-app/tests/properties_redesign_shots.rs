//! Screenshots of the redesigned Properties panel (fixed header and tabs, Source options
//! under the body, feature rows with summaries, two columns when wide), in Dark and
//! Light, for the owner's approval of the visual change (CLAUDE.md rule 16). It writes
//! raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it
//! is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test properties_redesign_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    FilterData, FilterFieldData, PropertiesProbe, Sections, SourceData, Studio, TextData, Theme,
    Tokens,
};
use rbs_ui::tokens::{Base, Effects, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn colour(hex: u32) -> slint::Color {
    slint::Color::from_argb_encoded(0xff00_0000 | hex)
}

fn source(text: bool) -> SourceData {
    SourceData {
        id: 7,
        name: if text { "Ticker" } else { "Sunset 2" }.into(),
        kind: if text { "Text" } else { "Image" }.into(),
        icon: if text { "text" } else { "image" }.into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: -1,
        places: 2,
        x: 0.0,
        y: 0.0,
        w: 2560.0,
        h: 1440.0,
        pic_w: 2560,
        pic_h: 1440,
        record_separately: !text,
        audio: !text,
        volume: 0.7,
        volume_text: "-6.0 dB".into(),
        filters: ModelRc::new(VecModel::from(vec![FilterData {
            id: 3,
            name: "Colour correction".into(),
            enabled: true,
            known: true,
            fields: ModelRc::new(VecModel::from(vec![
                FilterFieldData {
                    label: "Brightness".into(),
                    kind: 0,
                    position: 0.5,
                    value_text: "0.00".into(),
                    ..Default::default()
                },
                FilterFieldData {
                    label: "Mode".into(),
                    kind: 1,
                    dropdown: true,
                    options: ModelRc::new(VecModel::from(vec!["Soft".into(), "Hard".into()])),
                    ..Default::default()
                },
            ])),
            ..Default::default()
        }])),
        selected_filter: 0,
        show_for_ms: 5000,
        item_id: 7,
        border_color: "#ffffff".into(),
        border_swatch: colour(0xffffff),
        border_width: 4.0,
        ..Default::default()
    }
}

fn text() -> TextData {
    TextData {
        on: true,
        id: 7,
        text: "Breaking news".into(),
        size: 102.0,
        color: "#ffffff".into(),
        swatch: colour(0xffffff),
        align: 2,
        outline_color: "#000000".into(),
        outline_swatch: colour(0x000000),
        shadow: true,
        shadow_color: "#f31affb3".into(),
        shadow_swatch: colour(0xf31aff),
        shadow_x: 9.0,
        shadow_y: 15.0,
        shadow_blur: 4.0,
        background_color: "#00000099".into(),
        padding: 16.0,
        ..Default::default()
    }
}

struct Look {
    name: &'static str,
    text: bool,
    tab: i32,
    options_open: bool,
    width: f32,
    height: f32,
}

fn shot(file: &TokenFile, base: Base, effects: Effects, look: &Look, dir: &std::path::Path) {
    let mut body = ThemeBody::new(base);
    body.set_effects(effects);
    let theme = file
        .resolve(&body)
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
    let options_open = look.options_open;
    // The effects and Scroll start closed, as in the app; the closed Shadow shows its summary.
    w.global::<Sections>()
        .on_closed(move |id, _| match id.as_str() {
            "properties.options" => !options_open,
            "properties.text-outline"
            | "properties.text-shadow"
            | "properties.text-background"
            | "properties.scroll" => true,
            _ => false,
        });
    let g = w.global::<Studio>();
    g.set_sources(ModelRc::new(VecModel::from(vec![source(look.text)])));
    g.set_properties_source(7);
    g.set_properties_tab(look.tab);
    if look.text {
        g.set_text_props(text());
    }
    w.window()
        .set_size(slint::LogicalSize::new(look.width, look.height));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{}-{}x{}.rgba", look.name, px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn properties_redesign_screenshots() {
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
    let looks = [
        ("image-closed", false, 0, false, 260.0, 720.0),
        ("image-open", false, 0, true, 260.0, 720.0),
        ("text-closed", true, 4, false, 260.0, 720.0),
        ("image-wide", false, 0, true, 720.0, 640.0),
        ("image-short", false, 0, true, 260.0, 420.0),
        ("text-tall", true, 4, false, 260.0, 1150.0),
        ("image-tall", false, 0, false, 260.0, 1500.0),
        ("audio", false, 2, false, 260.0, 900.0),
        ("filters", false, 1, false, 260.0, 900.0),
    ];
    let fx = [
        (Effects::Calm, "calm"),
        (Effects::Glow, "glow"),
        (Effects::Glass, "glass"),
        (Effects::Tactile, "tactile"),
    ];
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
      for (effects, e) in fx {
        for (n, text, tab, open, width, height) in looks {
            if e != "calm" && n != "text-tall" {
                continue;
            }
            let name = format!("{n}-{b}-{e}");
            let look = Look {
                name: Box::leak(name.into_boxed_str()),
                text,
                tab,
                options_open: open,
                width,
                height,
            };
            shot(&file, base, effects, &look, &dir);
        }
      }
    }
}
