//! The studio lends the pointer to pointer filters (spec `video-filters.md`, "Binding
//! and the studio"; ADR-0165 decisions 3 and 5): with an input monitor, a scheduled
//! click-rings filter subscribes and turns its source's mapping switch on; disabled,
//! it lets both go. Without a monitor the switch stays off. Skips without a GPU.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

mod support;

use rbs_core::capability::{CapabilitySet, CapabilityValue};
use rbs_core::media::ContainerId;
use rbs_graph::node::NodeTypeId;
use rbs_input::{EventMask, InputBackend, InputError, InputMonitor, InputSink, RunningInput, caps};
use rbs_record::{OutputSettings, Studio, StudioSettings};
use rbs_scene::model::Settings;
use rbs_sources::types;
use support::*;

struct Backend;
struct Running;

impl RunningInput for Running {
    fn set_mask(&mut self, _: EventMask) {}
    fn stop(self: Box<Self>) {}
}

impl InputBackend for Backend {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new()
            .with(caps::POINTER, CapabilityValue::Flag(true))
            .with(caps::BUTTONS, CapabilityValue::Flag(true))
    }
    fn start(&self, _: EventMask, _: InputSink) -> Result<Box<dyn RunningInput>, InputError> {
        Ok(Box::new(Running))
    }
}

/// Runs a studio with a click-rings filter on the display; `monitor` is lent when set.
/// Returns whether the switch was on and the monitor ran while it was scheduled, and
/// both after the filter was disabled.
fn run(monitor: Option<&InputMonitor>) -> Option<[(bool, bool); 2]> {
    let gpu = gpu()?;
    let mut r = rig();
    if let Some(m) = monitor {
        r.adapters.sources.set_input(m.clone());
    }
    let (mut col, canvas) = scene();
    let display = col
        .sources()
        .find(|s| s.type_id.as_str() == types::DISPLAY_SOURCE)
        .map(|s| s.id)
        .unwrap();
    let (rings, _) = col
        .add_filter(
            display,
            NodeTypeId::new_static(rbs_filters::CLICK_RINGS),
            Settings::new(),
        )
        .unwrap();
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://pointer", ContainerId::Matroska),
        )
        .unwrap();
    let frames = |s: &Studio| s.filter_stats(rings).map_or(0, |st| st.frames());
    assert!(run_until(&mut studio, &clock, |s| frames(s) >= 3));
    let running = || monitor.is_some_and(InputMonitor::is_running);
    let on = (studio.pointer_mapping_on(display), running());
    assert_eq!(studio.filter_stats(rings).unwrap().pointer_faults(), 0);

    col.set_filter_enabled(display, rings, false).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(run_until(&mut studio, &clock, |_| !running()));
    let off = (studio.pointer_mapping_on(display), running());

    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
    Some([on, off])
}

#[test]
fn a_scheduled_pointer_filter_is_lent_the_monitor_and_turns_the_mapping_on() {
    let monitor = InputMonitor::new(Box::new(Backend));
    let Some([on, off]) = run(Some(&monitor)) else {
        return;
    };
    assert_eq!(on, (true, true), "scheduled: switch on, subscribed");
    assert_eq!(off, (false, false), "disabled: switch off, unsubscribed");
}

#[test]
fn without_a_monitor_the_mapping_stays_off() {
    let Some([on, off]) = run(None) else {
        return;
    };
    assert_eq!(on, (false, false));
    assert_eq!(off, (false, false));
}

#[test]
fn the_switch_stays_on_while_another_pointer_filter_of_the_source_runs() {
    let Some(gpu) = gpu() else { return };
    let monitor = InputMonitor::new(Box::new(Backend));
    let mut r = rig();
    r.adapters.sources.set_input(monitor.clone());
    let (mut col, canvas) = scene();
    let display = col
        .sources()
        .find(|s| s.type_id.as_str() == types::DISPLAY_SOURCE)
        .map(|s| s.id)
        .unwrap();
    let add = |col: &mut rbs_scene::SceneCollection, id: &str| {
        col.add_filter(display, NodeTypeId::new(id), Settings::new())
            .unwrap()
            .0
    };
    let rings = add(&mut col, rbs_filters::CLICK_RINGS);
    let halo = add(&mut col, rbs_filters::POINTER_HALO);
    let (engine, clock) = engine_at_start();
    let mut studio = Studio::start(
        engine,
        gpu,
        &col,
        &r.adapters.sources,
        &StudioSettings::new(canvas),
    )
    .unwrap();
    let out = studio
        .start_output(
            &r.adapters,
            &OutputSettings::new("mem://pointer-two", ContainerId::Matroska),
        )
        .unwrap();
    let frames = |s: &Studio| s.filter_stats(halo).map_or(0, |st| st.frames());
    assert!(run_until(&mut studio, &clock, |s| frames(s) >= 3));
    assert!(studio.pointer_mapping_on(display));

    // One leaves (removed): the other still wants the mapping.
    col.remove_filter(display, rings).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(studio.filter_stats(rings).is_none());
    assert!(studio.pointer_mapping_on(display), "the halo still runs");

    // The last one leaves: off, and the monitor stops.
    col.remove_filter(display, halo).unwrap();
    studio.update(&col, &r.adapters.sources).unwrap();
    assert!(!studio.pointer_mapping_on(display));
    assert!(run_until(&mut studio, &clock, |_| !monitor.is_running()));

    studio.stop_output(out).unwrap();
    let (_, errors) = studio.shutdown();
    assert!(errors.is_empty(), "{errors:?}");
}
