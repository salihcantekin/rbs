//! The drawn context menu (ADR-0137): placement, the keyboard, closing and the focus
//! coming back, on the real library components in `MenuProbe`. With `RBS_SHOTS_DIR`
//! set, the ignored `menu_screenshots` writes Dark and Light pictures for the owner.

#![allow(clippy::expect_used, clippy::unwrap_used)] // Tests fail by panicking.

use std::cell::RefCell;
use std::rc::Rc;

use rbs_app_ui::{ContextMenus, MenuProbe as Probe, MenuRow, Overlays};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, VecModel};

fn row(kind: i32, title: &str, icon: &str, shortcut: &str, id: &str) -> MenuRow {
    MenuRow {
        kind,
        title: title.into(),
        icon: icon.into(),
        shortcut: shortcut.into(),
        id: id.into(),
        enabled: true,
        ..Default::default()
    }
}

/// The source menu as the registry makes it, shortened.
fn source_rows() -> Vec<MenuRow> {
    let mut paste = row(
        1,
        "Paste attributes…",
        "import",
        "Ctrl+Shift+V",
        "ui.source.paste_attributes",
    );
    paste.enabled = false;
    paste.reason = "Copy a source's attributes first.".into();
    let mut visible = row(1, "Visible", "eye", "", "item.visible.toggle?item=3");
    visible.checkable = true;
    visible.checked = true;
    vec![
        row(3, "Transform", "fit", "", ""),
        row(3, "Arrange", "chevron-up", "", ""),
        row(2, "", "", "", ""),
        visible,
        MenuRow {
            checkable: true,
            ..row(1, "Locked", "lock", "", "item.locked.toggle?item=3")
        },
        row(2, "", "", "", ""),
        row(1, "Rename", "pencil", "F2", "source.rename?source=2"),
        row(
            1,
            "Duplicate source",
            "copy",
            "Ctrl+D",
            "source.duplicate?source=2",
        ),
        paste,
        row(2, "", "", "", ""),
        row(1, "Remove from scene", "trash", "Del", "item.remove?item=3"),
    ]
}

fn transform_rows() -> Vec<MenuRow> {
    vec![
        MenuRow {
            checkable: true,
            checked: true,
            ..row(
                1,
                "Flip horizontally",
                "flip-h",
                "",
                "item.flip?axis=horizontal&item=3",
            )
        },
        MenuRow {
            checkable: true,
            checked: false,
            ..row(
                1,
                "Flip vertically",
                "flip-v",
                "",
                "item.flip?axis=vertical&item=3",
            )
        },
        row(
            1,
            "Rotate 90° clockwise",
            "rotate-cw",
            "",
            "item.rotate?item=3",
        ),
        row(
            1,
            "Fit to canvas",
            "fit",
            "Ctrl+F",
            "item.fit?item=3&mode=fit",
        ),
        row(
            1,
            "Reset transform",
            "reset",
            "Ctrl+R",
            "item.reset_transform?item=3",
        ),
    ]
}

struct Menu {
    p: Probe,
    ran: Rc<RefCell<Vec<String>>>,
}

fn setup() -> Menu {
    i_slint_backend_testing::init_no_event_loop();
    let p = Probe::new().expect("the probe window");
    install(&p)
}

fn install(p: &Probe) -> Menu {
    let g = p.global::<ContextMenus>();
    g.on_find(|rows, from, letter| rbs_app::menu_layout::find_row(&rows, from, &letter));
    g.on_place(|anchor, w, h, win_w, win_h, margin, beside| {
        rbs_app::menu_layout::place_cover(anchor, [w, h], [win_w, win_h], margin, beside)
    });
    let focus = Rc::new(RefCell::new(None));
    let weak = p.as_weak();
    let saved = focus.clone();
    g.on_rows(move |_, _| {
        if let Some(p) = weak.upgrade() {
            *saved.borrow_mut() = Some(rbs_app::popups::focused(p.window()));
            p.global::<ContextMenus>().set_has_reasons(true);
        }
        ModelRc::new(VecModel::from(source_rows()))
    });
    g.on_submenu(|i| {
        ModelRc::new(VecModel::from(if i == 0 {
            transform_rows()
        } else {
            vec![row(
                1,
                "Bring to front",
                "chevron-up",
                "",
                "item.move?direction=front&item=3",
            )]
        }))
    });
    let weak = p.as_weak();
    g.on_closed(move || {
        let Some(item) = focus.borrow_mut().take() else {
            return;
        };
        let weak = weak.clone();
        // As the app does: after the event that closed the menu.
        #[allow(clippy::disallowed_methods)]
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(p) = weak.upgrade() {
                rbs_app::popups::refocus(p.window(), &item);
            }
        });
    });
    let ran: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = ran.clone();
    g.on_run(move |id| {
        log.borrow_mut().push(id.to_string());
        true
    });
    p.show().expect("show");
    Menu {
        p: p.clone_strong(),
        ran,
    }
}

fn key(p: &Probe, k: Key) {
    let s: slint::SharedString = k.into();
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
}

fn text(p: &Probe, t: &str) {
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: t.into() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: t.into() });
}

fn press(p: &Probe, x: f32, y: f32, button: PointerEventButton) {
    let position = LogicalPosition::new(x, y);
    p.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    p.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    p.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
}

/// Runs pending timers and the submenu delay.
fn settle() {
    slint::platform::update_timers_and_animations();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
}

#[test]
fn a_menu_opens_at_the_point_and_flips_upward_near_the_bottom() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    assert!(g.get_open());
    let c = g.get_cover();
    assert_eq!((c.x, c.y), (100.0, 60.0), "under and right of the point");
    g.invoke_close();
    m.p.invoke_open_at(600.0, 400.0);
    settle();
    let c = g.get_cover();
    assert!(
        (c.y + c.height - 400.0).abs() < 0.5,
        "it ends at the point: {c:?}"
    );
    assert!((c.x + c.width - 600.0).abs() < 0.5, "and left of it: {c:?}");
}

#[test]
fn a_button_menu_near_the_bottom_opens_over_the_button() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_open_bottom();
    settle();
    let c = g.get_cover();
    let button_top = g.get_y() - g.get_above();
    assert!(
        (c.y + c.height - button_top).abs() < 0.5,
        "{c:?} ends at {button_top}"
    );
}

#[test]
fn keys_move_over_separators_run_and_give_the_focus_back() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_focus_row();
    assert!(m.p.get_row_focused());
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    key(&m.p, Key::UpArrow); // the last row: Remove from scene
    key(&m.p, Key::UpArrow); // over the separator: Paste (disabled)
    key(&m.p, Key::Return);
    assert!(
        g.get_open() && m.ran.borrow().is_empty(),
        "a disabled row does nothing"
    );
    key(&m.p, Key::UpArrow);
    key(&m.p, Key::Return);
    assert!(!g.get_open());
    assert_eq!(m.ran.borrow().as_slice(), ["source.duplicate?source=2"]);
    settle();
    assert!(m.p.get_row_focused(), "the focus is back on the row");
}

#[test]
fn a_letter_jumps_and_escape_closes_the_submenu_then_the_menu() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_focus_row();
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    text(&m.p, "t");
    key(&m.p, Key::RightArrow);
    assert!(g.get_sub_open(), "Transform opens");
    let (menu, sub) = (g.get_cover(), g.get_sub_cover());
    assert!(
        (sub.x - (menu.x + menu.width)).abs() < 0.5,
        "beside the menu: {menu:?} {sub:?}"
    );
    key(&m.p, Key::DownArrow);
    key(&m.p, Key::DownArrow);
    key(&m.p, Key::Escape);
    assert!(!g.get_sub_open() && g.get_open());
    key(&m.p, Key::RightArrow);
    key(&m.p, Key::Return);
    assert!(g.get_open(), "a switch in a submenu changes in place");
    key(&m.p, Key::End);
    key(&m.p, Key::Return);
    assert!(!g.get_open());
    assert_eq!(
        m.ran.borrow().as_slice(),
        [
            "item.flip?axis=horizontal&item=3",
            "item.reset_transform?item=3"
        ]
    );
    settle();
    assert!(m.p.get_row_focused(), "back on the row after a pick too");
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    key(&m.p, Key::Escape);
    assert!(!g.get_open());
    settle();
    assert!(m.p.get_row_focused());
}

#[test]
fn a_switch_row_changes_in_place_and_the_menu_stays_open() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    text(&m.p, "v");
    key(&m.p, Key::Return);
    assert!(g.get_open(), "the menu stays open");
    assert!(
        !slint::Model::row_data(&g.get_shown(), 3).unwrap().checked,
        "Visible is off now"
    );
    text(&m.p, " ");
    assert!(slint::Model::row_data(&g.get_shown(), 3).unwrap().checked);
    assert_eq!(
        m.ran.borrow().as_slice(),
        ["item.visible.toggle?item=3", "item.visible.toggle?item=3"]
    );
    key(&m.p, Key::RightArrow);
    key(&m.p, Key::Escape);
    assert!(!g.get_open());
}

#[test]
fn a_press_or_the_wheel_outside_closes_it_and_tab_too() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    press(&m.p, 620.0, 20.0, PointerEventButton::Right);
    assert!(!g.get_open() && m.ran.borrow().is_empty());
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    m.p.window().dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(620.0, 20.0),
        delta_x: 0.0,
        delta_y: -20.0,
    });
    assert!(!g.get_open());
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    key(&m.p, Key::Tab);
    assert!(!g.get_open());
}

#[test]
fn hovering_a_submenu_row_opens_it_after_the_delay_and_a_click_runs() {
    let m = setup();
    let g = m.p.global::<ContextMenus>();
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    let c = g.get_cover();
    // The second row (Arrange), in its middle.
    let (x, y) = (c.x + 40.0, c.y + 4.0 + 28.0 + 14.0);
    m.p.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(x, y),
    });
    slint::platform::update_timers_and_animations();
    assert!(!g.get_sub_open(), "not at once");
    settle();
    assert!(g.get_sub_open());
    let s = g.get_sub_cover();
    press(&m.p, s.x + 40.0, s.y + 4.0 + 14.0, PointerEventButton::Left);
    assert_eq!(
        m.ran.borrow().as_slice(),
        ["item.move?direction=front&item=3"]
    );
    assert!(!g.get_open());
}

#[test]
fn opening_a_menu_closes_the_list_of_a_select_and_a_select_opens_its_list() {
    let m = setup();
    let o = m.p.global::<Overlays>();
    m.p.invoke_focus_select();
    key(&m.p, Key::Return);
    assert!(
        o.get_select_open(),
        "a short Select opens the drawn list too"
    );
    m.p.invoke_open_at(100.0, 60.0);
    settle();
    assert!(!o.get_select_open());
}

/// A screenshot's name and what it does to the window first.
type Shot<'a> = (&'a str, &'a dyn Fn(&Probe));

#[test]
#[ignore = "a tool: writes screenshots for the owner's approval"]
fn menu_screenshots() {
    use rbs_app_ui::tokens_apply::{apply_tokens, slot};
    use rbs_app_ui::{Theme, Tokens};
    use rbs_ui::tokens::{Base, ThemeBody, Tokens as TokenFile};
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
        let theme = file
            .resolve(&ThemeBody::new(base))
            .expect("the theme resolves");
        let s = slot(&theme, "").expect("a slot");
        let shots: [Shot; 3] = [
            ("submenu", &|p| {
                p.invoke_open_at(150.0, 40.0);
                slint::platform::update_timers_and_animations();
                key(p, Key::DownArrow);
                key(p, Key::RightArrow);
                key(p, Key::DownArrow);
            }),
            ("flipped-reason", &|p| {
                p.invoke_open_at(610.0, 405.0);
                slint::platform::update_timers_and_animations();
                key(p, Key::UpArrow);
                key(p, Key::UpArrow);
            }),
            ("select", &|p| {
                p.invoke_focus_select();
                key(p, Key::Return);
                slint::platform::update_timers_and_animations();
            }),
        ];
        for (name, act) in shots {
            let p = Probe::new().expect("the probe window");
            apply_tokens(
                &p.global::<Theme>(),
                &p.global::<Tokens>(),
                &file,
                [s.clone(), s.clone()],
            )
            .expect("the tokens apply");
            let m = install(&p);
            act(&m.p);
            slint::platform::update_timers_and_animations();
            let px = m.p.window().take_snapshot().expect("a snapshot");
            let path = dir.join(format!(
                "menu-{name}-{b}-{}x{}.rgba",
                px.width(),
                px.height()
            ));
            std::fs::write(&path, px.as_bytes()).expect("written");
            m.p.hide().expect("hide");
        }
    }
}
