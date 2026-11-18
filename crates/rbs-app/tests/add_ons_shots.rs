//! Screenshots of Settings › Add-ons (ADR-0145 decision 9) in each row state, in Dark
//! and Light, for the owner's approval of the visual change (CLAUDE.md rule 16). It
//! writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check,
//! so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test add_ons_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_api::types::{AddOnFailReason, AddOnInfo, AddOnStatus};
use rbs_app::add_on_text;
use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{AddOnRowData, SettingsWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn engine(status: AddOnStatus) -> AddOnInfo {
    AddOnInfo {
        id: "org.rbs.browser-engine".into(),
        kind: "runtime".into(),
        name: "Browser engine".into(),
        summary: "Lets browser sources show web pages.".into(),
        version: "156.0.30".into(),
        size: Some(175_176_258),
        unpacked_size: Some(444_764_149),
        available: true,
        status,
    }
}

/// The row as `add_ons_page` builds it.
fn rows(status: AddOnStatus) -> ModelRc<AddOnRowData> {
    let r = add_on_text::row(&engine(status));
    ModelRc::new(VecModel::from(vec![AddOnRowData {
        id: r.id.into(),
        label: r.label.into(),
        help: r.help.into(),
        problem: r.problem,
        value: r.value.into(),
        action: r.action.code(),
        action_text: r.action.text().into(),
    }]))
}

fn tokens(file: &TokenFile, base: Base, theme: &Theme<'_>, t: &Tokens<'_>) {
    let resolved = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&resolved, "").expect("a slot");
    apply_tokens(theme, t, file, [s.clone(), s]).expect("the tokens apply");
}

fn page(file: &TokenFile, base: Base, status: AddOnStatus, name: &str, dir: &std::path::Path) {
    let w = SettingsWindow::new().expect("the settings window");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    w.global::<Studio>().set_add_on_rows(rows(status));
    w.set_section(12);
    w.window().set_size(slint::LogicalSize::new(960.0, 440.0));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn add_ons_screenshots() {
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
        for (status, s) in [
            (AddOnStatus::NotInstalled, "1-not-installed"),
            (
                AddOnStatus::Downloading {
                    bytes: 42_000_000,
                    total: 175_176_258,
                },
                "2-downloading",
            ),
            (
                AddOnStatus::Installed {
                    version: "156.0.30".into(),
                },
                "3-installed",
            ),
            (
                AddOnStatus::Failed {
                    reason: AddOnFailReason::Network,
                },
                "4-failed",
            ),
        ] {
            page(&file, base, status, &format!("{b}-{s}"), &dir);
        }
    }
}
