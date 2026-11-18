//! Screenshots of Settings › Stream's Destinations card and the main window's frame
//! group, in Dark and Light, for the owner's approval of the visual change (CLAUDE.md
//! rule 16, ADR-0148). It writes raw RGBA files to `RBS_SHOTS_DIR` with Skia's real
//! pixels; it is not a check, so it is ignored and run by hand:
//!
//! `SLINT_EMIT_DEBUG_INFO=1 RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test destinations_shots -- --ignored`
//!
//! (`SLINT_EMIT_DEBUG_INFO` lets the test find the switch it turns off for the
//! last-destination question.)

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{
    DestinationRow, MainWindow, SettingsWindow, Shell, Studio, Theme, Tokens, ToolbarData,
};
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::ComponentHandle;

fn row(id: i32, name: &str, note: &str, on: bool) -> DestinationRow {
    DestinationRow {
        id,
        name: name.into(),
        note: note.into(),
        on,
        warn: false,
        removable: true,
    }
}

fn save(px: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, dir: &std::path::Path, name: &str) {
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

fn themed<T: ComponentHandle>(w: &T, file: &TokenFile, base: Base)
where
    for<'a> Theme<'a>: slint::Global<'a, T>,
    for<'a> Tokens<'a>: slint::Global<'a, T>,
{
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    apply_tokens(
        &w.global::<Theme>(),
        &w.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
}

/// `state`: "empty", "all-off" or "last-on" (one on, the callout of turning it off).
fn settings(file: &TokenFile, base: Base, state: &str, name: &str, dir: &std::path::Path) {
    let w = SettingsWindow::new().expect("the settings window");
    themed(&w, file, base);
    let g = w.global::<Studio>();
    let note = "RTMP · Key saved · Same as the stream";
    let yt = "YouTube · RTMP · Key saved · Same as the stream";
    let rows = match state {
        "empty" => vec![],
        "all-off" => vec![
            row(2, "Live Push", note, false),
            row(3, "YouTube", yt, false),
        ],
        _ => vec![
            row(2, "Live Push", note, true),
            row(3, "YouTube", yt, false),
        ],
    };
    let on = rows.iter().filter(|r| r.on).count();
    let n = rows.len();
    g.set_st_dests(std::rc::Rc::new(slint::VecModel::from(rows)).into());
    g.set_st_dest_on_count(i32::try_from(on).unwrap());
    g.set_st_available(on > 0);
    g.set_st_dest_count(format!("{n} of 6").into());
    g.set_st_dest_can_add(true);
    g.set_st_dest_editable(true);
    g.set_st_form_idle(true);
    if on > 0 {
        g.set_st_dest_upload("Upload needed: about 18 Mbit/s for 1 destination.".into());
    }
    // Settings › Stream (`main.rs` SECTION_STREAM).
    w.set_section(2);
    w.window().set_size(slint::LogicalSize::new(960.0, 520.0));
    w.show().expect("show");
    if state == "last-on" {
        // A first frame makes the list's rows.
        let _ = w.window().take_snapshot();
        let toggle = i_slint_backend_testing::ElementQuery::from_root(&w)
            .match_descendants()
            .match_accessible_role(i_slint_backend_testing::AccessibleRole::Switch)
            .find_all()
            .into_iter()
            .find(|e| {
                e.accessible_label()
                    .is_some_and(|l| l.contains("Live Push"))
            })
            .expect("the switch (build with SLINT_EMIT_DEBUG_INFO=1)");
        toggle.invoke_accessible_default_action();
    }
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

fn cell(id: &str, title: &str, icon: &str) -> ToolbarData {
    ToolbarData {
        id: id.into(),
        title: title.into(),
        kind: 0,
        enabled: true,
        checked: false,
        live: false,
        icon: icon.into(),
        icon_only: false,
    }
}

/// The frame's fixed group with and without Go live.
fn frame(file: &TokenFile, base: Base, stream: bool, name: &str, dir: &std::path::Path) {
    let w = MainWindow::new().expect("the main window");
    themed(&w, file, base);
    let mut cells = Vec::new();
    if stream {
        cells.push(cell("stream.toggle", "Go live", "bolt"));
    }
    cells.push(cell("record.toggle", "Record", "dot"));
    w.global::<Shell>()
        .set_toolbar_fixed(std::rc::Rc::new(slint::VecModel::from(cells)).into());
    // The status text names the stream only while a canvas streams (ADR-0148).
    w.global::<Shell>().set_streaming(stream);
    w.window().set_size(slint::LogicalSize::new(1280.0, 720.0));
    w.show().expect("show");
    save(&w.window().take_snapshot().expect("a snapshot"), dir, name);
    w.hide().expect("hide");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn destinations_screenshots() {
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
        for state in ["empty", "all-off", "last-on"] {
            settings(&file, base, state, &format!("stream-{state}-{b}"), &dir);
        }
        frame(&file, base, false, &format!("frame-off-{b}"), &dir);
        frame(&file, base, true, &format!("frame-on-{b}"), &dir);
    }
}
