//! Source activity in the running studio (ADR-0152): a capture that runs only when
//! shown closes off screen and opens on the cut (phase 4, audit F-07), and a
//! background page runs hidden while placed.

use std::sync::atomic::Ordering;

use rbs_core::media::ContainerId;
use rbs_graph::node::NodeTypeId;
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::model::{ItemContent, Layout, Settings};
use rbs_sources::types;

use crate::support::*;

#[test]
fn a_camera_that_runs_only_when_shown_closes_off_screen_and_opens_on_the_cut() {
    // ADR-0152 phase 4, audit F-07: a capture with `run: shown` holds no session while
    // no shown scene draws it, and opens again when its scene goes on air. A camera
    // with the default (`placed`) next to it stays open on the hidden scene.
    use rbs_scene::activity::{Run, SourceActivity};
    use rbs_scene::model::SceneScope;

    let Some(gpu) = gpu() else { return };
    let r = rig();
    let (mut col, main) = scene();
    let live = col.canvas(main).unwrap().program.unwrap();
    let (other, _) = col.add_scene("Other", SceneScope::Shared).unwrap();
    let small = Layout::new([0.0, 0.0], [20.0, 11.0]);
    let add_camera = |col: &mut rbs_scene::SceneCollection, name: &str| {
        let (id, _) = col
            .add_source(
                name,
                NodeTypeId::new_static(types::CAMERA_SOURCE),
                Settings::new(),
            )
            .unwrap();
        col.add_item(other, ItemContent::Source(id), small, None)
            .unwrap();
        id
    };
    let shy = add_camera(&mut col, "Shy camera");
    let warm = add_camera(&mut col, "Warm camera");
    col.set_source_activity(shy, SourceActivity::new(Some(Run::Shown), None, None))
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut settings = StudioSettings::new(main);
    settings.audio = false;
    let mut studio = Studio::start(engine, gpu, &col, &r.adapters.sources, &settings).unwrap();
    let names = |s: &Studio| -> Vec<String> { s.sources().into_iter().map(|x| x.name).collect() };
    let mut rec = OutputSettings::new("mem://main", ContainerId::Matroska);
    rec.audio = None;
    studio.start_output(&r.adapters, &rec).unwrap();
    let running = names(&studio);
    assert!(running.iter().any(|n| n == "Warm camera"), "{running:?}");
    assert!(!running.iter().any(|n| n == "Shy camera"), "{running:?}");
    let reached = run_until(&mut studio, &clock, |s| s.picture_size(warm).is_some());
    assert!(reached, "the warm camera opened");
    assert_eq!(studio.picture_size(shy), None);
    let buffers = r.capture.live_buffers();

    // On air: the shy camera opens.
    col.set_program(main, Some(other)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(names(&studio).iter().any(|n| n == "Shy camera"));
    let reached = run_until(&mut studio, &clock, |s| s.picture_size(shy).is_some());
    assert!(reached, "the shy camera opened when shown");
    assert!(r.capture.live_buffers() > buffers);

    // Off air again, once the outgoing scene's transition ended: it closes; the warm
    // one stays.
    col.set_program(main, Some(live)).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    studio.end_transitions();
    studio.update(&col, &r.adapters.sources).unwrap();
    let running = names(&studio);
    assert!(!running.iter().any(|n| n == "Shy camera"), "{running:?}");
    assert!(running.iter().any(|n| n == "Warm camera"), "{running:?}");
    let reached = run_until(&mut studio, &clock, |_| r.capture.live_buffers() <= buffers);
    assert!(reached, "the shy camera is still open");
    assert_eq!(studio.picture_size(shy), None);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn a_background_page_runs_hidden_while_placed_and_stops_when_it_no_longer_is_one() {
    // ADR-0149 decision 5, ADR-0152 decisions 4 and 7: `run: placed` keeps the page's
    // helper running, hidden, while its picture node is not scheduled; shown, the same
    // helper paints at full rate; back to the type's default, it stops.
    use std::sync::Arc;

    use crate::fake_browser::FakeTree;
    use rbs_browser_ipc::control::Control;
    use rbs_core::media::video::Size;
    use rbs_scene::SceneCollection;
    use rbs_scene::activity::{Run, SourceActivity};
    use rbs_scene::model::{SceneScope, SettingValue};
    use rbs_sources::BrowserTree;
    use rbs_sources::types::keys;

    let Some(gpu) = gpu() else {
        return;
    };
    let r = rig();
    let mut col = SceneCollection::new();
    let (canvas, _) = col.add_canvas("Main", Size::new(W, H), FPS).unwrap();
    let (live, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    let (shelf, _) = col.add_scene("Shelf", SceneScope::Shared).unwrap();
    let mut settings = Settings::new();
    settings.insert(
        keys::URL.to_owned(),
        SettingValue::Text("https://example.com/".to_owned()),
    );
    settings.insert(keys::WIDTH.to_owned(), SettingValue::Int(64));
    settings.insert(keys::HEIGHT.to_owned(), SettingValue::Int(36));
    let (page, _) = col
        .add_source(
            "Page",
            NodeTypeId::new_static(types::BROWSER_SOURCE),
            settings,
        )
        .unwrap();
    let full = Layout::new([0.0, 0.0], [W as f32, H as f32]);
    col.add_item(shelf, ItemContent::Source(page), full, None)
        .unwrap();
    col.set_source_activity(page, SourceActivity::new(Some(Run::Placed), None, None))
        .unwrap();
    col.set_program(canvas, Some(live)).unwrap();

    let tree = FakeTree::new();
    let sent = || tree.sent.lock().unwrap().clone();

    let (engine, _clock) = engine_at_start();
    let mut studio = Studio::start_with_tree(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
        Some(Arc::clone(&tree) as Arc<dyn BrowserTree>),
    )
    .unwrap();
    // Placed on a scene nothing shows: started by the studio, hidden.
    assert!(studio.browser_running(page));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    assert_eq!(sent(), vec![Control::Shown(false)]);

    // On the program: the same helper, shown.
    let item = col
        .add_item(live, ItemContent::Source(page), full, None)
        .unwrap()
        .0;
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.browser_running(page));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    assert_eq!(sent(), vec![Control::Shown(false), Control::Shown(true)]);

    // Off the program again: the node leaves the schedule, the helper stays, hidden.
    col.remove_item(item).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.browser_running(page));
    assert_eq!(tree.starts.load(Ordering::Relaxed), 1);
    assert_eq!(tree.stops.load(Ordering::Relaxed), 0);
    assert_eq!(sent().last(), Some(&Control::Shown(false)));

    // Back to the type's default (`run: shown`): nothing shows it, so it stops.
    col.set_source_activity(page, SourceActivity::default())
        .unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(!studio.browser_running(page));
    assert_eq!(tree.stops.load(Ordering::Relaxed), 1);
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
