//! Screenshots of the notice cards for the owner's approval of the visual change
//! (CLAUDE.md rule 16): the error bar with a short and a long (two-line) message, the
//! error bar just after a copy, and the delete undo toast at 3 s, in Dark and Light.
//! It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a
//! check, so it is ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test toast_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use i_slint_backend_testing::ElementHandle;
use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{MainWindow, Shell, Studio, Theme, Tokens};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

const SHORT: &str = "the camera is in use by another app";
const LONG: &str = "source.set_companion_audio refused: the companion audio device \
    \"MacBook Pro Microphone\" cannot follow this capture because its sample rate \
    of 44100 Hz does not match the studio rate of 48000 Hz";

enum Case {
    Error(&'static str, bool),
    Deleted,
}

fn shot(file: &TokenFile, base: Base, case: &Case, name: &str, dir: &std::path::Path) {
    let theme = file.resolve(&ThemeBody::new(base)).unwrap();
    let s = slot(&theme, "").unwrap();
    let w = MainWindow::new().unwrap();
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .unwrap();
    match case {
        Case::Error(detail, copy) => {
            let g = w.global::<Shell>();
            g.set_error_detail((*detail).into());
            g.set_error_kind(2);
            g.set_error_generation(1);
            if *copy {
                let shown = format!("That did not work: {detail}");
                ElementHandle::find_by_accessible_label(&w, &shown)
                    .next()
                    .expect("the words")
                    .invoke_accessible_default_action();
            }
        }
        Case::Deleted => {
            let st = w.global::<Studio>();
            st.set_deleted("Camera".into());
            st.set_deleted_generation(1);
        }
    }
    let h = if matches!(case, Case::Deleted) {
        720.0
    } else {
        200.0
    };
    w.window().set_size(slint::LogicalSize::new(1280.0, h));
    w.show().unwrap();
    let px = w.window().take_snapshot().unwrap();
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).unwrap();
    w.hide().unwrap();
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn toast_screenshots() {
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
    let cases = [
        (Case::Error(SHORT, false), "error-short"),
        (Case::Error(LONG, false), "error-long"),
        (Case::Error(SHORT, true), "error-copied"),
        (Case::Deleted, "deleted"),
    ];
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        for (case, what) in &cases {
            shot(&file, base, case, &format!("{what}-{b}"), &dir);
        }
    }
}
