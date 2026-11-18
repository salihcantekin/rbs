//! Screenshots of Settings › Shortcuts with the category filter row, in Dark and Light,
//! for the owner's approval (CLAUDE.md rule 16). Writes raw RGBA files to
//! `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is ignored and run
//! by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test shortcut_filter_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{SettingsWindow, ShortcutChip, ShortcutRow, Shortcuts, Theme, Tokens};
use rbs_ui::command::Category;
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

fn row(group: &str, title: &str, chip: Option<(&str, bool)>) -> ShortcutRow {
    ShortcutRow {
        group: group.into(),
        command: SharedString::new(),
        title: title.into(),
        chips: ModelRc::new(VecModel::from(
            chip.map(|(label, global)| ShortcutChip {
                label: label.into(),
                keys: label.into(),
                global,
            })
            .into_iter()
            .collect::<Vec<_>>(),
        )),
        changed: false,
        problem: SharedString::new(),
        note: SharedString::new(),
        off: false,
    }
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn shortcut_filter_screenshots() {
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
    let names: Vec<SharedString> = std::iter::once("All")
        .chain(Category::ALL.iter().map(|c| c.title()))
        .map(SharedString::from)
        .collect();
    // (file name, chosen category, with a shortcut only, rows)
    let views: Vec<(&str, i32, bool, Vec<ShortcutRow>)> = vec![
        (
            "scene",
            1,
            false,
            vec![
                row("SCENE", "Scene: Intro", None),
                row("", "Scene: Main camera", None),
                row("", "Scene 1", Some(("Ctrl+1", false))),
                row("", "Scene 2", Some(("Ctrl+2", false))),
                row("", "Next scene", Some(("Ctrl+PageDown", false))),
                row("", "Previous scene", Some(("Ctrl+PageUp", false))),
                row("", "New scene...", None),
                row("", "Duplicate scene", None),
                row("OUTPUT", "Studio mode", None),
                row("", "Transition", Some(("Ctrl+Enter", false))),
            ],
        ),
        (
            "assigned",
            0,
            true,
            vec![
                row("EDIT", "Undo", Some(("Ctrl+Z", false))),
                row("", "Redo", Some(("Ctrl+Shift+Z", false))),
                row("SCENE", "Scene 1", Some(("Ctrl+1", false))),
                row("", "Scene 2", Some(("Ctrl+2", false))),
                row(
                    "OUTPUT",
                    "Start or end the stream",
                    Some(("Ctrl+Alt+F9", true)),
                ),
                row("", "Start or stop recording", Some(("Ctrl+Alt+F10", true))),
            ],
        ),
        // Streaming is off (ADR-0148): the stream commands keep their keys, dimmed.
        (
            "streaming-off",
            0,
            true,
            vec![
                row("EDIT", "Undo", Some(("Ctrl+Z", false))),
                ShortcutRow {
                    note: "Streaming is off".into(),
                    off: true,
                    ..row(
                        "OUTPUT",
                        "Start or end the stream",
                        Some(("Ctrl+Alt+F9", true)),
                    )
                },
                ShortcutRow {
                    note: "Streaming is off".into(),
                    off: true,
                    ..row("", "Hold stream", Some(("Ctrl+Alt+H", false)))
                },
                row("", "Start or stop recording", Some(("Ctrl+Alt+F10", true))),
            ],
        ),
    ];
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        for (name, category, assigned, rows) in &views {
            let w = SettingsWindow::new().expect("the settings window");
            let theme = file
                .resolve(&ThemeBody::new(base))
                .expect("the theme resolves");
            let s = slot(&theme, "").expect("a slot");
            apply_tokens(
                &w.global::<Theme>(),
                &w.global::<Tokens>(),
                &file,
                [s.clone(), s],
            )
            .expect("the tokens apply");
            let g = w.global::<Shortcuts>();
            g.set_hint("Shortcuts run commands while an RBS window is in front. Global shortcuts also work while another app is in front, or RBS has no window open; they start and stop outputs without asking.".into());
            g.set_categories(ModelRc::new(VecModel::from(names.clone())));
            g.set_category(*category);
            g.set_assigned_only(*assigned);
            g.set_filtered(true);
            g.set_rows(ModelRc::new(VecModel::from(rows.clone())));
            w.set_section(6);
            w.window().set_size(slint::LogicalSize::new(926.0, 720.0));
            w.show().expect("show");
            let px = w.window().take_snapshot().expect("a snapshot");
            let path = dir.join(format!("{name}-{b}-{}x{}.rgba", px.width(), px.height()));
            std::fs::write(&path, px.as_bytes()).expect("written");
            w.hide().expect("hide");
        }
    }
}
