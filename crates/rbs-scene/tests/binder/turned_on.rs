//! `BindReport::turned_on_filters` (ADR-0199).

use super::*;

#[test]
fn a_filter_turned_on_while_its_source_is_drawn_is_reported_but_a_scene_switch_is_not() {
    let mut s = setup();
    let (f, _) = s
        .col
        .add_filter(s.cam, ty("org.rbs.filter.lut"), Settings::new())
        .unwrap();
    s.col.set_filter_enabled(s.cam, f, false).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(
        report.turned_on_filters.is_empty(),
        "first sync: nothing was drawn"
    );
    let node = s.binder.filter_node(s.cam, f).unwrap();

    // Turned on while the camera is on screen: listed once.
    s.col.set_filter_enabled(s.cam, f, true).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert_eq!(report.turned_on_filters, [(f, node)]);
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.turned_on_filters.is_empty());

    // A scene without the camera, then back: the filter joins again, but the camera
    // was not on screen just before, so it is not listed.
    let (other, _) = s.col.add_scene("Other", SceneScope::Shared).unwrap();
    s.col
        .add_item(other, ItemContent::Source(s.screen), full(), None)
        .unwrap();
    s.col.set_program(s.wide, Some(other)).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    s.col.set_program(s.wide, Some(s.live)).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(
        report.turned_on_filters.is_empty(),
        "a scene switch is not a turn-on"
    );

    // Turned on while the camera is not drawn: not listed either.
    s.col.set_filter_enabled(s.cam, f, false).unwrap();
    s.col.set_program(s.wide, Some(other)).unwrap();
    s.binder.sync(&s.col, &mut s.graph).unwrap();
    s.col.set_filter_enabled(s.cam, f, true).unwrap();
    let report = s.binder.sync(&s.col, &mut s.graph).unwrap();
    assert!(report.turned_on_filters.is_empty());
}
