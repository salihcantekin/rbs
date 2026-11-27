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

#[test]
fn a_fade_in_a_waiting_list_starts_when_the_list_is_taken() {
    let Some(mut rig) = Rig::new() else { return };
    let mut scene = rig.scene(4, |red, _| vec![full(red, 0)]);
    rig.run(&scene, 2);
    let green = green(&mut rig);
    // Sent with a 200 ms fade starting at the next tick, as a scene change fades in.
    let mut layer = full(green, 2);
    layer.fade = Some(Fade {
        from: 0.0,
        start: Timestamp::from_nanos(rig.tick * 1_000_000_000 / 60),
        duration: Duration::from_millis(200),
        easing: rbs_gpu::node::Easing::Linear,
    });
    let _ = scene.sender.send(vec![full(scene.red, 0), layer]);
    // About 100 ms of waiting: without the delay the fade would be half way.
    rig.run(&scene, 6);
    rig.graph
        .connect(green, "out", scene.comp, "layers")
        .unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    // Green's first picture comes a tick after it joins: one or two ticks into the
    // fade, not seven.
    rig.run(&scene, 2);
    let [r, g, _, _] = rig.pixel(&scene, 8, 16);
    assert!(g < 0.2 && r > 0.8, "fade delayed by the wait: {r} {g}");
}

#[test]
fn a_ready_counter_is_not_marked_while_a_list_waits() {
    let Some(mut rig) = Rig::new() else { return };
    let red = rig.add(Box::new(Solid::new([255, 0, 0, 255])));
    let ready = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let canvas = CanvasDesc {
        width: W,
        height: H,
    };
    let (node, mut sender) = CompositorNode::new(canvas, 4, vec![full(red, 0)]);
    let comp = rig.add(Box::new(node.with_ready(Arc::clone(&ready))));
    let probe = rig.add(Box::new(Probe {
        desc: NodeDescriptor::new(
            NodeTypeId::new_static("org.rbs.test.probe"),
            NodeKind::Output,
            vec![Pin::one("in", LinkKind::Video)],
            vec![],
        ),
        last: Arc::new(Mutex::new(None)),
    }));
    rig.graph.connect(red, "out", comp, "layers").unwrap();
    rig.graph.connect(comp, "canvas", probe, "in").unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    for _ in 0..3 {
        rig.tick();
    }
    assert!(ready.load(Ordering::Acquire) > 0, "every layer drawn");

    let green = green(&mut rig);
    let _ = sender.send(vec![full(green, 1)]);
    ready.store(0, Ordering::Release);
    for _ in 0..3 {
        rig.tick();
    }
    assert_eq!(
        ready.load(Ordering::Acquire),
        0,
        "the drawn list is not the one sent"
    );

    rig.ctl.collect().unwrap();
    rig.graph.connect(green, "out", comp, "layers").unwrap();
    rig.ctl.apply(&rig.graph).unwrap();
    for _ in 0..2 {
        rig.tick();
    }
    assert!(ready.load(Ordering::Acquire) > 0);
}
