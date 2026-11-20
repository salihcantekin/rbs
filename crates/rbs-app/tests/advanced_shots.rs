//! Screenshots of Settings › Advanced (ADR-0158 decision 8) in Dark and Light, for the
//! owner's approval of the visual change (CLAUDE.md rule 16). It writes raw RGBA files
//! to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and
//! run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test advanced_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app::diag_log::{AREA_NAMES, LogDetail};
use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{Experience, LogAreaRow, SettingsWindow, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn tokens(file: &TokenFile, base: Base, theme: &Theme<'_>, t: &Tokens<'_>) {
    let resolved = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&resolved, "").expect("a slot");
    apply_tokens(theme, t, file, [s.clone(), s]).expect("the tokens apply");
}

/// The rows as `advanced_page` builds them, with Stream at Everything.
fn areas() -> ModelRc<LogAreaRow> {
    let rows: Vec<LogAreaRow> = AREA_NAMES
        .iter()
        .map(|(area, label)| LogAreaRow {
            name: area.name().into(),
            label: (*label).into(),
            level: if area.name() == "stream" {
                LogDetail::Everything.index()
            } else {
                -1
            },
        })
        .collect();
    ModelRc::new(VecModel::from(rows))
}

fn page(file: &TokenFile, base: Base, pro: bool, name: &str, dir: &std::path::Path) {
    let w = SettingsWindow::new().expect("the settings window");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    let e = w.global::<Experience>();
    e.set_advanced(true);
    e.set_expert(pro);
    let g = w.global::<Studio>();
    g.set_log_detail(LogDetail::Detailed.index());
    g.set_log_detailed(true);
    g.set_log_back_to_normal(true);
    g.set_log_areas(areas());
    g.set_log_areas_changed(true);
    g.set_log_save_status("Saved 7 files in ~/Downloads/rbs-logs-20261120T171500Z.zip.".into());
    g.set_log_save_tone(1);
    w.set_section(13);
    w.window().set_size(slint::LogicalSize::new(
        960.0,
        if pro { 1480.0 } else { 820.0 },
    ));
    w.show().expect("show");
    let px = w.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn advanced_screenshots() {
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
        page(&file, base, false, &format!("{b}-1-standard"), &dir);
        page(&file, base, true, &format!("{b}-2-pro"), &dir);
    }
}
