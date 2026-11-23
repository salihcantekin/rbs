//! The glide of a program change (ADR-0100) in the running studio: how long it runs.
//! Reference adapters on a `TestClock`; nothing waits for a duration.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use std::time::Duration;

use rbs_core::time::Clock;
use rbs_record::{Studio, StudioSettings};
use rbs_scene::model::{ItemContent, Layout, SceneScope};
use support::*;

#[test]
fn a_longer_source_glide_sets_how_long_the_glide_runs() {
    // ADR-0100 decision 5: the glide lasts as long as its longest slide. The camera
    // slides from the camera scene back to its box with an 800 ms override while the
    // collection glide is 200 ms; nothing leaves, so this is the glide alone.
    use rbs_scene::{
        CollectionGlide, CollectionTransition, GlideEasing, SourceGlide, TransitionKind,
    };

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, canvas) = scene();
    let live = col.canvas(canvas).unwrap().program.unwrap();
    let camera = col.sources().find(|s| s.name == "Camera").unwrap().id;
    let (other, _) = col.add_scene("Camera only", SceneScope::Shared).unwrap();
    col.add_item(
        other,
        ItemContent::Source(camera),
        Layout::new([0.0, 0.0], [W as f32, H as f32]),
        None,
    )
    .unwrap();
    col.set_transition(CollectionTransition {
        kind: TransitionKind::Move,
        direct: TransitionKind::Move,
        ..CollectionTransition::default()
    })
    .unwrap();
    col.set_glide(CollectionGlide {
        duration_ms: 200,
        easing: GlideEasing::EaseInOut,
    })
    .unwrap();
    col.set_source_glide(
        camera,
        Some(SourceGlide {
            duration_ms: 800,
            easing: GlideEasing::Linear,
        }),
    )
    .unwrap();
    col.set_program(canvas, Some(other)).unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();

    col.set_program(canvas, Some(live)).unwrap();
    let at = clock.now();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert_eq!(
        studio.glide_end(),
        Some(at + Duration::from_millis(800)),
        "the camera's override, not the collection's 200 ms"
    );
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
