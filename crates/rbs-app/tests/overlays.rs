//! The info hint popover and the tooltip (ADR-0094): keyboard and pointer behaviour of
//! the real library components in a probe window, with a mocked clock (no sleeping).

#![allow(clippy::expect_used)] // Tests fail by panicking.

use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

use rbs_app_ui::{OverlayProbe as Probe, Overlays};

fn key(p: &Probe, k: Key) {
    let s: slint::SharedString = k.into();
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
}

fn point(p: &Probe, x: f32, y: f32) {
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x, y),
    });
}

/// Runs what Slint defers to the next frame: `changed` handlers and timers.
fn settle() {
    slint::platform::update_timers_and_animations();
}

fn setup() -> Probe {
    i_slint_backend_testing::init_no_event_loop();
    let p = Probe::new().expect("the probe window");
    p.show().expect("show");
    p
}

#[test]
fn the_hint_opens_with_return_and_escape_returns_the_focus() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    key(&p, Key::Return);
    assert!(o.get_hint_open());
    assert_eq!(
        o.get_hint_text().as_str(),
        "Sound below this level is muted."
    );
    key(&p, Key::Escape);
    assert!(!o.get_hint_open());
    assert!(p.get_hint_focused(), "the focus is back on the info button");
}

#[test]
fn opening_a_hint_shows_no_tooltip_and_a_click_outside_closes_it() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    key(&p, Key::Return);
    assert!(o.get_hint_open() && !o.get_tip_shown());
    // A click far from the popover closes it.
    point(&p, 380.0, 280.0);
    p.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(380.0, 280.0),
        button: slint::platform::PointerEventButton::Left,
    });
    p.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(380.0, 280.0),
        button: slint::platform::PointerEventButton::Left,
    });
    assert!(!o.get_hint_open());
}

#[test]
fn a_tooltip_shows_at_once_for_the_keyboard_and_after_the_delay_for_the_pointer() {
    let p = setup();
    let o = p.global::<Overlays>();
    // Keyboard focus: at once, with the button's name; Escape dismisses it.
    p.invoke_focus_button();
    settle();
    assert!(o.get_tip_shown(), "keyboard focus shows it at once");
    assert_eq!(o.get_tip_text().as_str(), "Flip horizontally");
    key(&p, Key::Escape);
    settle();
    assert!(!o.get_tip_shown());

    // Pointer: waits for the delay, then shows; leaving hides it. The keyboard focus
    // moves away first, since a focused button counts as pointed at.
    p.invoke_focus_hint();
    settle();
    point(&p, 300.0, 250.0);
    point(&p, 110.0, 20.0);
    settle();
    assert!(!o.get_tip_shown() && o.get_tip_waiting());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(o.get_tip_shown());
    point(&p, 300.0, 250.0);
    settle();
    assert!(!o.get_tip_shown());
}

#[test]
fn a_feature_section_keeps_its_settings_when_off_and_folds_by_id() {
    use rbs_app_ui::Sections;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;

    let p = setup();
    let shut: Rc<RefCell<HashSet<String>>> = Rc::default();
    let s = p.global::<Sections>();
    let read = shut.clone();
    s.on_closed(move |id, _| read.borrow().contains(id.as_str()));
    let write = shut.clone();
    s.on_set_closed(move |id, closed| {
        let mut set = write.borrow_mut();
        if closed {
            set.insert(id.to_string());
        } else {
            set.remove(id.as_str());
        }
    });
    settle();
    let open_height = p.get_feature_height();
    assert!(p.get_feature_on());
    // Switching it off leaves the settings in place.
    p.invoke_flip_feature();
    settle();
    assert!(!p.get_feature_on());
    assert_eq!(p.get_feature_height(), open_height);
    // Folding by id takes their room.
    s.invoke_set_closed("probe.feature".into(), true);
    s.set_rev(s.get_rev() + 1);
    settle();
    assert!(
        p.get_feature_height() < open_height,
        "{:?} vs {:?}",
        p.get_feature_height(),
        open_height
    );
}

#[test]
fn the_name_prompt_accepts_with_return_and_cancels_with_escape() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let p = setup();
    let o = p.global::<Overlays>();
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let accepted = log.clone();
    o.on_name_accepted(move |n| accepted.borrow_mut().push(format!("ok {n}")));
    let cancelled = log.clone();
    o.on_name_cancelled(move || cancelled.borrow_mut().push("cancel".to_owned()));
    o.set_name_value("Show".into());
    o.set_name_open(true);
    settle();
    // The field starts with the value and has the focus: Return accepts it.
    key(&p, Key::Return);
    assert_eq!(log.borrow().as_slice(), ["ok Show"]);
    key(&p, Key::Escape);
    assert_eq!(log.borrow().as_slice(), ["ok Show", "cancel"]);
}

fn picker_row(name: &str, index: i32) -> rbs_app_ui::PickerRow {
    rbs_app_ui::PickerRow {
        name: name.into(),
        note: "".into(),
        index,
    }
}

#[test]
fn the_picker_moves_with_the_arrows_and_chooses_by_the_rows_listing_index() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let p = setup();
    let o = p.global::<Overlays>();
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let chosen = log.clone();
    o.on_picker_chosen(move |i| chosen.borrow_mut().push(format!("chose {i}")));
    let filtered = log.clone();
    o.on_picker_filtered(move |t| filtered.borrow_mut().push(format!("filter {t}")));
    // A filter hid rows 0 and 1: the visible rows keep their listing indexes 2 and 5.
    o.set_picker_rows(slint::ModelRc::new(slint::VecModel::from(vec![
        picker_row("Capture card", 2),
        picker_row("Virtual cam", 5),
    ])));
    o.set_picker_open(true);
    settle();
    // The filter field has the focus: typing reaches the app, Down moves, Return chooses.
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: "c".into() });
    key(&p, Key::DownArrow);
    key(&p, Key::Return);
    assert_eq!(log.borrow().last().map(String::as_str), Some("chose 5"));
    key(&p, Key::UpArrow);
    key(&p, Key::Return);
    assert_eq!(log.borrow().last().map(String::as_str), Some("chose 2"));
}

#[test]
fn escape_closes_the_picker_above_the_name_prompt_and_nothing_else() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let p = setup();
    let o = p.global::<Overlays>();
    let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
    let name = log.clone();
    o.on_name_cancelled(move || name.borrow_mut().push("name"));
    let picker = log.clone();
    o.on_picker_cancelled(move || picker.borrow_mut().push("picker"));
    o.set_name_open(true);
    settle();
    o.set_picker_open(true);
    settle();
    key(&p, Key::Escape);
    assert_eq!(
        log.borrow().as_slice(),
        ["picker"],
        "only the topmost overlay"
    );
}

#[test]
fn the_colour_popover_is_a_layer_and_a_pick_reaches_only_the_swatch_that_opened_it() {
    let p = setup();
    let o = p.global::<Overlays>();
    // The swatch is at (200, 10), 28 px or so wide: a click opens the popover.
    p.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(210.0, 20.0),
    });
    p.window().dispatch_event(WindowEvent::PointerPressed {
        position: slint::LogicalPosition::new(210.0, 20.0),
        button: slint::platform::PointerEventButton::Left,
    });
    p.window().dispatch_event(WindowEvent::PointerReleased {
        position: slint::LogicalPosition::new(210.0, 20.0),
        button: slint::platform::PointerEventButton::Left,
    });
    settle();
    assert!(o.get_color_open(), "the swatch opens the popover");
    assert_eq!(o.get_color_label().as_str(), "Border colour");
    // A pick (as the popover publishes it) comes back through the swatch.
    o.set_color_result("#445566".into());
    o.set_color_serial(o.get_color_serial() + 1);
    settle();
    assert_eq!(p.get_last_pick().as_str(), "#445566");
    // Another owner's pick is not ours.
    o.set_color_owner(o.get_color_owner() + 100);
    o.set_color_result("#778899".into());
    o.set_color_serial(o.get_color_serial() + 1);
    settle();
    assert_eq!(p.get_last_pick().as_str(), "#445566");
    o.set_color_owner(o.get_color_owner() - 100);
    key(&p, Key::Escape);
    settle();
    assert!(!o.get_color_open());
    assert!(p.get_colour_focused(), "the focus is back on the swatch");
}

fn click(p: &Probe, x: f32, y: f32) {
    point(p, x, y);
    for pressed in [true, false] {
        let position = slint::LogicalPosition::new(x, y);
        let button = slint::platform::PointerEventButton::Left;
        p.window().dispatch_event(if pressed {
            WindowEvent::PointerPressed { position, button }
        } else {
            WindowEvent::PointerReleased { position, button }
        });
    }
}

#[test]
fn one_click_on_a_tab_selects_it_while_the_focus_is_elsewhere() {
    let p = setup();
    // The focus is on another control, as after a click in another panel.
    p.invoke_focus_button();
    settle();
    // The Tabs bar spans x 10..310 at y 240; three tabs of 100 each.
    click(&p, 160.0, 250.0);
    assert_eq!(p.get_tab_index(), 1, "the first click chooses Filters");
    click(&p, 260.0, 250.0);
    assert_eq!(p.get_tab_index(), 2);
}

/// Another window taking the focus closes the tooltip and keeps it closed until the
/// window is active again; coming back shows no tooltip for the focus alone.
#[test]
fn losing_the_window_activation_closes_the_tooltip() {
    let p = setup();
    rbs_app::window_events::install(&p);
    let o = p.global::<Overlays>();
    let activate = |on: bool| {
        p.window()
            .dispatch_event(WindowEvent::WindowActiveChanged(on));
        settle();
    };
    activate(true);

    // Keyboard focus shows it; the window losing the activation closes it.
    p.invoke_focus_button();
    settle();
    assert!(o.get_tip_shown());
    activate(false);
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());
    // Back with the window: the focus returns, the tooltip does not.
    activate(true);
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());

    // The pointer resting on the button while the window is inactive starts nothing.
    p.invoke_focus_hint();
    settle();
    point(&p, 390.0, 290.0);
    settle();
    activate(false);
    point(&p, 110.0, 20.0);
    settle();
    assert!(!o.get_tip_waiting());
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(!o.get_tip_shown());

    // A tooltip waiting for the delay is dropped when the window loses the activation.
    activate(true);
    point(&p, 390.0, 290.0);
    point(&p, 110.0, 20.0);
    settle();
    assert!(o.get_tip_waiting());
    activate(false);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(!o.get_tip_shown());
}

/// The pointer moving off the control (onto a control without a tooltip, or where a
/// removed control was) closes the tooltip; on the control or the tooltip it stays.
#[test]
fn the_pointer_off_the_control_closes_the_tooltip() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    settle();
    point(&p, 390.0, 290.0);
    point(&p, 110.0, 20.0);
    settle();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(o.get_tip_shown());
    // Still on the control, then on the tooltip itself: it stays.
    o.invoke_pointer_at(112.0, 22.0);
    assert!(o.get_tip_shown());
    let cover = o.get_tip_cover();
    o.invoke_pointer_at(cover.x + cover.width / 2.0, cover.y + cover.height / 2.0);
    assert!(o.get_tip_shown());
    // Elsewhere in the window: closed, without the control hearing the pointer leave.
    o.invoke_pointer_at(390.0, 290.0);
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());
}

/// A button clicked with the pointer keeps the focus, but not its tooltip once the
/// pointer moves away.
#[test]
fn a_clicked_button_does_not_keep_its_tooltip() {
    let p = setup();
    let o = p.global::<Overlays>();
    p.invoke_focus_hint();
    settle();
    click(&p, 110.0, 20.0);
    settle();
    point(&p, 390.0, 290.0);
    settle();
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
    assert!(!o.get_tip_shown() && !o.get_tip_waiting());
}

#[test]
fn tab_into_a_row_below_the_view_shows_the_whole_row() {
    i_slint_backend_testing::init_no_event_loop();
    let p = rbs_app_ui::FocusRevealProbe::new().expect("the probe window");
    p.show().expect("show");
    p.invoke_focus_first();
    settle();
    assert_eq!(p.get_offset(), 0.0, "the first row is in view");
    let s: slint::SharedString = Key::Tab.into();
    p.window()
        .dispatch_event(WindowEvent::KeyPressed { text: s.clone() });
    p.window()
        .dispatch_event(WindowEvent::KeyReleased { text: s });
    settle();
    let shown_to = p.get_offset() + p.get_view_height();
    assert!(
        shown_to >= p.get_row_bottom(),
        "the row's help is in view: shown to {shown_to}, row ends at {}",
        p.get_row_bottom()
    );
}
