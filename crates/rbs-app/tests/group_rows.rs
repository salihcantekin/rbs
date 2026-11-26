//! The Sources list's group rows (ADR-0194) on the real panel in `SourcesProbe`: a
//! member row selects and removes only that member, the group row selects the group.

#![allow(clippy::expect_used)] // Tests fail by panicking.

use std::cell::RefCell;
use std::rc::Rc;

use rbs_app_ui::{SourceData, SourcesProbe, Studio};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, VecModel};

fn row(id: i32, name: &str, head: bool) -> SourceData {
    SourceData {
        id,
        name: name.into(),
        kind: "Colour".into(),
        icon: "palette".into(),
        status: "Running".into(),
        ok: true,
        picture: true,
        visible: true,
        grouped: true,
        group_head: head,
        group_open: true,
        group_id: 7,
        group_name: "Pair".into(),
        ..SourceData::default()
    }
}

fn key(w: &SourcesProbe, k: Key) {
    let s: slint::SharedString = k.into();
    w.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    w.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
}

fn click(w: &SourcesProbe, position: LogicalPosition) {
    let button = PointerEventButton::Left;
    w.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    w.window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    w.window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
    // Past the double-click time, so the next click is a click of its own.
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
}

/// The first place down the list whose click calls `want` (the rows' heights come
/// from the tokens, so the test does not hard-code them).
fn find(w: &SourcesProbe, calls: &RefCell<Vec<String>>, want: &str) -> LogicalPosition {
    (0..100)
        .map(|i| LogicalPosition::new(120.0, 2.0 + i as f32 * 3.0))
        .find(|at| {
            click(w, *at);
            let hit = calls.borrow().last().is_some_and(|c| c == want);
            calls.borrow_mut().clear();
            hit
        })
        .expect("a row that calls it")
}

#[test]
fn a_member_row_selects_and_removes_only_that_member() {
    i_slint_backend_testing::init_no_event_loop();
    let w = SourcesProbe::new().expect("the probe");
    w.window().set_size(slint::LogicalSize::new(340.0, 300.0));
    w.show().expect("show");
    let g = w.global::<Studio>();
    g.set_state(1);
    g.set_sources(ModelRc::new(VecModel::from(vec![
        row(1, "Logo", true),
        row(2, "Clock", false),
    ])));
    let calls = Rc::new(RefCell::new(Vec::<String>::new()));
    let log = |name: &'static str| {
        let calls = calls.clone();
        move |id: i32| calls.borrow_mut().push(format!("{name} {id}"))
    };
    g.on_select_source(log("source"));
    g.on_select_group(log("group"));
    g.on_remove_from_scene(log("remove"));
    g.on_delete_source(log("delete"));
    g.on_remove_group(log("remove group"));
    slint::platform::update_timers_and_animations();

    let group = find(&w, &calls, "group 7");
    let member = find(&w, &calls, "source 2");
    assert!(member.y > group.y, "the member sits under its group's row");

    click(&w, member);
    assert_eq!(*calls.borrow(), ["source 2"], "the member alone");
    // Delete asks on the row; Return confirms it for that member only.
    key(&w, Key::Delete);
    key(&w, Key::Return);
    assert_eq!(*calls.borrow(), ["source 2", "remove 2"]);

    click(&w, group);
    assert_eq!(*calls.borrow(), ["source 2", "remove 2", "group 7"]);
}
