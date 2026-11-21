//! Screenshots of Properties' activity rows (ADR-0152, owner approval 2026-11-21):
//! "Runs" of a display with its warm-up hint, "Keep running for" of a media file, a
//! web page and a display, the media file's third "When it comes back" choice and a
//! scroll's "When it comes back", in Dark and Light, for the owner's approval of the
//! visual change (CLAUDE.md rule 16). It writes raw RGBA files to `RBS_SHOTS_DIR`
//! with Skia's real pixels; it is not a check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test activity_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{BrowserData, PropertiesProbe, SourceData, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn film() -> SourceData {
    SourceData {
        id: 7,
        name: "Intro clip".into(),
        kind: "Media file".into(),
        icon: "media".into(),
        status: "Running".into(),
        ok: true,
        picture: true,
        audio: true,
        visible: true,
        kind_code: -1,
        places: 1,
        media: true,
        media_path: "/Users/me/Movies/intro.mp4".into(),
        media_loop: true,
        act_on: true,
        act_run: 0,
        act_return: 2,
        act_linger: 2,
        media_end_ms: -1,
        media_state: 1,
        media_pos_ms: 41_300,
        media_len_ms: 95_000,
        ..Default::default()
    }
}

fn display(warm_up: &str) -> SourceData {
    SourceData {
        id: 7,
        name: "Main display".into(),
        kind: "Display".into(),
        icon: "display".into(),
        status: "Running".into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: 0,
        places: 1,
        act_on: true,
        act_run: 1,
        act_return: -1,
        act_linger: 3,
        act_warm_up: warm_up.into(),
        ..Default::default()
    }
}

fn ticker() -> SourceData {
    SourceData {
        id: 7,
        name: "News ticker".into(),
        kind: "Text".into(),
        icon: "text".into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: -1,
        places: 1,
        w: 1920.0,
        h: 80.0,
        y: 1000.0,
        scroll_on: true,
        scroll_speed: 120.0,
        scroll_gap: 64.0,
        scroll_return: 1,
        ..Default::default()
    }
}

fn page() -> BrowserData {
    BrowserData {
        on: true,
        id: 7,
        url: "https://streamelements.com/overlay/5f2c/chat".into(),
        width: 1280,
        height: 720,
        canvas_w: 1920,
        canvas_h: 1080,
        fps: 30,
        zoom: 100,
        transparent: true,
        run: 1,
        on_return: 0,
        linger: 4,
        running: true,
        ..Default::default()
    }
}

fn browser_row() -> SourceData {
    SourceData {
        id: 7,
        name: "Chat".into(),
        kind: "Browser".into(),
        browser: true,
        icon: "globe".into(),
        ok: true,
        picture: true,
        visible: true,
        kind_code: -1,
        places: 1,
        act_on: true,
        ..Default::default()
    }
}

struct Shot {
    name: &'static str,
    row: SourceData,
    tab: i32,
    page: Option<BrowserData>,
    height: f32,
}

fn shot(file: &TokenFile, base: Base, b: &str, s: Shot, dir: &std::path::Path) {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let slot = slot(&theme, "").expect("a slot");
    let w = PropertiesProbe::new().expect("the probe window");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [slot.clone(), slot],
    )
    .expect("the tokens apply");
    let g = w.global::<Studio>();
    g.set_sources(ModelRc::new(VecModel::from(vec![s.row])));
    g.set_properties_source(7);
    g.set_properties_tab(s.tab);
    if let Some(p) = s.page {
        g.set_browser_props(p);
    }
    w.window()
        .set_size(slint::LogicalSize::new(380.0, s.height));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!(
        "{}-{b}-{}x{}.rgba",
        s.name,
        px.width(),
        px.height()
    ));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn activity_screenshots() {
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
        let shots = [
            Shot {
                name: "media",
                row: film(),
                tab: 3,
                page: None,
                height: 900.0,
            },
            Shot {
                name: "display",
                row: display("0.6"),
                tab: 3,
                page: None,
                height: 560.0,
            },
            Shot {
                name: "page",
                row: browser_row(),
                tab: 5,
                page: Some(page()),
                height: 1700.0,
            },
            Shot {
                name: "scroll",
                row: ticker(),
                tab: 0,
                page: None,
                height: 1700.0,
            },
        ];
        for s in shots {
            shot(&file, base, b, s, &dir);
        }
    }
}
