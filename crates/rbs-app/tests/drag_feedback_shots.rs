//! Screenshots of the panel drag feedback and of a full-height side area, in Dark and
//! Light, for the owner's approval of ADR-0197 (CLAUDE.md rule 16). Writes raw RGBA
//! files to `RBS_SHOTS_DIR` with Skia's real pixels; it is not a check, so it is
//! ignored and run by hand:
//!
//! `RBS_SHOTS_DIR=/tmp/shots cargo test -p rbs-app --test drag_feedback_shots -- --ignored`

#![allow(clippy::expect_used, clippy::unwrap_used)] // A tool run by hand.

use rbs_app_ui::tokens_apply::{apply_tokens, slot};
use rbs_app_ui::{Cover, MainWindow, Shell, StackData, TabData, Theme, Tokens};
use rbs_ui::builtin;
use rbs_ui::frame::flatten_window;
use rbs_ui::layout::Rect;
use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, VecModel};

const W: f32 = 1280.0;
const H: f32 = 720.0;

fn title(id: &str) -> &'static str {
    match id {
        "rbs.scenes" => "Scenes",
        "rbs.sources" => "Sources",
        "rbs.properties" => "Properties",
        "rbs.mixer" => "Mixer",
        "rbs.preview" => "Preview",
        _ => "Panel",
    }
}

fn stacks(full_right: bool) -> Vec<StackData> {
    let mut ws = builtin::edit();
    if let Some(m) = ws.main_layout_mut() {
        m.right.full_height = full_right;
    }
    let flat = flatten_window(&ws.windows[0], Rect::sized(W, H), false);
    flat.stacks
        .into_iter()
        .map(|s| StackData {
            window: 0,
            stack: s.stack as i32,
            x: s.rect.x,
            y: s.rect.y,
            w: s.rect.w,
            h: s.rect.h,
            tabs: ModelRc::new(VecModel::from(
                s.tabs
                    .iter()
                    .map(|t| TabData {
                        id: t.as_str().into(),
                        title: title(t.as_str()).into(),
                        known: true,
                        icon: "".into(),
                    })
                    .collect::<Vec<_>>(),
            )),
            active: s.active as i32,
            collapsed: s.collapsed,
            can_collapse: true,
            can_hide: true,
            takes_drop: s.stack < rbs_ui::layout::PICTURES_PREVIEW_STACK,
        })
        .collect()
}

fn window(file: &TokenFile, base: Base) -> MainWindow {
    let theme = file
        .resolve(&ThemeBody::new(base))
        .expect("the theme resolves");
    let s = slot(&theme, "").expect("a slot");
    let m = MainWindow::new().expect("the main window");
    apply_tokens(
        &m.global::<Theme>(),
        &m.global::<Tokens>(),
        file,
        [s.clone(), s],
    )
    .expect("the tokens apply");
    m
}

fn snap(m: &MainWindow, name: &str, dir: &std::path::Path) {
    let px = m.window().take_snapshot().expect("a snapshot");
    let path = dir.join(format!("{name}-{}x{}.rgba", px.width(), px.height()));
    std::fs::write(&path, px.as_bytes()).expect("written");
}

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn drag_feedback_screenshots() {
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
    let origin_y: f32 = std::env::var("RBS_DOCK_Y")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(96.0);
    for (base, b) in [(Base::Dark, "dark"), (Base::Light, "light")] {
        for full in [false, true] {
            let m = window(&file, base);
            m.set_stacks(ModelRc::new(VecModel::from(stacks(full))));
            m.window()
                .set_size(slint::LogicalSize::new(W, H + origin_y));
            m.show().expect("show");
            snap(
                &m,
                &format!("layout-{}-{b}", if full { "full" } else { "plain" }),
                &dir,
            );
            m.hide().expect("hide");
        }
        // A tab dragged: press on the Sources tab, move far, then over the preview.
        let m = window(&file, base);
        let all = stacks(false);
        m.set_stacks(ModelRc::new(VecModel::from(all.clone())));
        m.window()
            .set_size(slint::LogicalSize::new(W, H + origin_y));
        m.show().expect("show");
        let src = all
            .iter()
            .find(|s| {
                use slint::Model;
                s.tabs.iter().any(|t| t.id == "rbs.sources")
            })
            .expect("sources stack");
        let _ = src;
        // The Sources tab in window coordinates (read from the layout shot).
        let down = LogicalPosition::new(44.0, 294.0);
        let g = m.global::<Shell>();
        g.on_panel_data(|id| {
            let mut d = slint::DataTransfer::default();
            d.set_plain_text(id);
            d
        });
        g.on_is_panel(|_| true);
        let win = m.window();
        win.dispatch_event(WindowEvent::PointerMoved { position: down });
        win.dispatch_event(WindowEvent::PointerPressed {
            position: down,
            button: PointerEventButton::Left,
        });
        let away = LogicalPosition::new(down.x + 40.0, down.y + 40.0);
        win.dispatch_event(WindowEvent::PointerMoved { position: away });
        let over = LogicalPosition::new(W / 2.0 - 60.0, 330.0);
        win.dispatch_event(WindowEvent::PointerMoved { position: over });
        // What Rust answers with while the pointer is over that stack.
        let g = m.global::<Shell>();
        g.set_drop_window(0);
        g.set_drop_guide(Cover {
            x: 272.0 - 6.0,
            y: 0.0,
            width: 374.0,
            height: 514.0,
        });
        snap(&m, &format!("dragging-{b}"), &dir);
        win.dispatch_event(WindowEvent::PointerReleased {
            position: over,
            button: PointerEventButton::Left,
        });
        m.hide().expect("hide");
    }
}
