//! A new layer list waits for the schedule that links it (ADR-0199 decision 5).

use super::*;

const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];

/// A green source in the rig, not linked to the compositor yet.
fn green(rig: &mut Rig) -> NodeId {
    rig.add(Box::new(Solid::new([0, 255, 0, 255])))
}

#[test]
fn a_list_naming_a_producer_not_linked_yet_waits_for_its_link() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scene(4, |red, _| vec![full(red, 0)]);
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);

    // The list goes out before its apply, as the studio sends it.
    let green = green(&mut rig);
    let _ = scene.sender.send(vec![full(green, 2)]);
    rig.run(&scene, 3);
    assert_close(rig.pixel(&scene, 8, 16), RED, 0.01);

    rig.graph
        .connect(green, "out", scene.comp, "layers")
        .unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 8, 16), GREEN, 0.01);
}

#[test]
fn a_waiting_list_is_taken_after_the_hold_limit() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scene(4, |red, _| vec![full(red, 0)]);
    rig.run(&scene, 2);
    let green = green(&mut rig);
    let _ = scene
        .sender
        .send(vec![full(green, 2), right_half(scene.blue, 1)]);
    // One second at 60 fps, less one tick.
    rig.run(&scene, 59);
    assert_close(rig.pixel(&scene, 56, 16), RED, 0.01);
    rig.run(&scene, 2);
    assert_close(rig.pixel(&scene, 56, 16), BLUE, 0.01);
}

#[test]
fn a_producer_unlinked_in_the_current_list_too_does_not_hold_a_list() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scene(4, |red, _| vec![full(red, 0)]);
    let (red, blue, green) = (scene.red, scene.blue, green(&mut rig));
    // Green never links (a source that failed): its list is taken at the limit.
    let _ = scene.sender.send(vec![full(red, 0), full(green, 2)]);
    rig.run(&scene, 62);
    // The next list keeps green: taken at once.
    let _ = scene
        .sender
        .send(vec![full(red, 0), right_half(blue, 1), full(green, 2)]);
    rig.run(&scene, 1);
    assert_close(rig.pixel(&scene, 56, 16), BLUE, 0.01);
}
