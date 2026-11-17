//! Screenshots of the OS permissions UI (ADR-0141): Settings › Permissions, a refused
//! camera in Properties' Source tab, and the first-run page's access step, in Dark and
//! Light, for the owner's approval of the visual change (CLAUDE.md rule 16). It writes
//! raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it
//! is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test permissions_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app::permission_text::{self, Action, Os, Tone};
use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    Overlays, PermissionRowData, PropertiesProbe, SettingsWindow, SourceData, Studio, Theme, Tokens,
};
use rbs_sources::permissions::{PermissionInfo, PermissionKind, PermissionStatus as S};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, VecModel};

fn info(status: S, can_prompt: bool, restart: bool) -> PermissionInfo {
    PermissionInfo {
        status,
        can_prompt,
        restart,
    }
}

/// The rows as `permissions_page` builds them, for a Mac started from Terminal.
fn rows(keep: &[PermissionKind]) -> ModelRc<PermissionRowData> {
    let all = [
        (PermissionKind::Camera, info(S::NotAsked, true, false)),
        (PermissionKind::Microphone, info(S::Allowed, false, false)),
        (PermissionKind::Screen, info(S::Blocked, false, true)),
        (PermissionKind::SystemSound, info(S::Unknown, false, false)),
    ];
    let data: Vec<PermissionRowData> = all
        .into_iter()
        .filter(|(k, _)| keep.contains(k))
        .map(|(kind, info)| {
            let r = permission_text::row(kind, info, false, true, Os::Mac, None);
            PermissionRowData {
                kind: permission_text::kind_code(kind),
                label: r.label.into(),
                status: r.status.into(),
                tone: match r.tone {
                    Tone::Plain => 0,
                    Tone::Warning => 1,
                    Tone::Problem => 2,
                },
                value: r.value.into(),
                action: r.action.code(),
                action_text: match r.action {
                    Action::Allow => "Allow".into(),
                    Action::OpenSettings => "Open system settings".into(),
                    Action::None => "".into(),
                },
            }
        })
        .collect();
    ModelRc::new(VecModel::from(data))
}

fn tokens(file: &TokenFile, base: Base, theme: &Theme<'_>, t: &Tokens<'_>) {
    let resolved = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&resolved, "").expect("a slot");
    apply_tokens(theme, t, file, [s.clone(), s]).expect("the tokens apply");
}

fn save(w: &slint::Window, dir: &std::path::Path, name: &str) {
    let px = w.take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

fn page(file: &TokenFile, base: Base, name: &str, dir: &std::path::Path) {
    let w = SettingsWindow::new().expect("the settings window");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    let g = w.global::<Studio>();
    g.set_permission_rows(rows(&PermissionKind::ALL));
    g.set_permission_holder("Terminal".into());
    g.set_permission_holder_path("/Users/me/rbs/target/release/rbs".into());
    g.set_permission_holder_note(
        "RBS was started from Terminal, so macOS most likely gives these permissions to \
         Terminal, not to RBS."
            .into(),
    );
    g.set_permission_holder_warn(true);
    w.set_section(11);
    w.window().set_size(slint::LogicalSize::new(960.0, 760.0));
    w.show().expect("show");
    save(w.window(), dir, name);
    w.hide().expect("hide");
}

fn source_tab(file: &TokenFile, base: Base, name: &str, action: i32, dir: &std::path::Path) {
    let w = PropertiesProbe::new().expect("the probe window");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    let g = w.global::<Studio>();
    g.set_sources(ModelRc::new(VecModel::from(vec![SourceData {
        id: 7,
        name: "FaceTime HD Camera".into(),
        kind: "Camera".into(),
        icon: "camera".into(),
        status: "Camera access denied".into(),
        ok: false,
        picture: true,
        visible: true,
        // Camera (`SourceKind::ALL` order).
        kind_code: 2,
        places: 1,
        perm_kind: 0,
        perm_action: action,
        ..Default::default()
    }])));
    g.set_properties_source(7);
    g.set_properties_tab(3);
    w.window().set_size(slint::LogicalSize::new(380.0, 560.0));
    w.show().expect("show");
    save(w.window(), dir, name);
    w.hide().expect("hide");
}

fn first_run(file: &TokenFile, base: Base, name: &str, dir: &std::path::Path) {
    let w = PropertiesProbe::new().expect("the probe window");
    tokens(file, base, &w.global::<Theme>(), &w.global::<Tokens>());
    w.global::<Studio>().set_permission_rows(rows(&[
        PermissionKind::Camera,
        PermissionKind::Microphone,
        PermissionKind::Screen,
    ]));
    let o = w.global::<Overlays>();
    o.set_first_run_step(1);
    o.set_first_run_access_to_do(true);
    o.set_first_run_open(true);
    w.window().set_size(slint::LogicalSize::new(960.0, 640.0));
    w.show().expect("show");
    save(w.window(), dir, name);
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn permissions_screenshots() {
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
        page(&file, base, &format!("page-{b}"), &dir);
        source_tab(&file, base, &format!("source-allow-{b}"), 1, &dir);
        source_tab(&file, base, &format!("source-fix-{b}"), 2, &dir);
        first_run(&file, base, &format!("first-run-{b}"), &dir);
    }
}
