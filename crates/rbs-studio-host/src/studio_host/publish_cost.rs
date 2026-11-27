//! What a control round's [`Host::publish`] costs when nothing changed (F-10): the
//! allocations it makes on the control thread, counted per thread, and its time.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::cell::Cell;
use std::time::Instant;

use rbs_audio_io_test::TestAudioBackend;
use rbs_capture_test::TestCaptureFactory;
use rbs_core::time::TestClock;
use rbs_engine::EngineConfig;
use rbs_gpu::{GpuError, GpuRequest};
use rbs_scene::model::{ItemContent, Layout, SceneScope, Settings};

use super::*;

/// Counts the allocations of the thread that makes them, so tests running beside
/// this one do not add to its count.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: every call is forwarded unchanged to `System`; the counter is a
// thread-local `Cell` with no destructor, so it never allocates or re-enters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: AllocLayout) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract for `alloc` is passed on as it is.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: AllocLayout) {
        // SAFETY: `ptr` came from `System` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: AllocLayout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract for `realloc` is passed on as it is.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

fn gpu() -> Option<Arc<GpuContext>> {
    match GpuContext::new(&GpuRequest {
        allow_software: true,
        ..GpuRequest::default()
    }) {
        Ok(ctx) => Some(Arc::new(ctx)),
        Err(GpuError::NoAdapter) => {
            assert!(
                std::env::var_os("RBS_REQUIRE_GPU").is_none_or(|v| v.is_empty()),
                "RBS_REQUIRE_GPU is set but no adapter was found"
            );
            rbs_crash::errln!("SKIPPED: no GPU adapter");
            None
        }
        Err(e) => panic!("{e}"),
    }
}

/// One canvas whose program scene places `n` sources: half microphones (one device,
/// so device peers are looked up), half colours.
fn collection(n: usize) -> SceneCollection {
    let mut col = SceneCollection::new();
    let (canvas, _) = col
        .add_canvas("Main", Size::new(1280, 720), FrameRate::FPS_30)
        .unwrap();
    let (scene, _) = col.add_scene("Live", SceneScope::Shared).unwrap();
    for i in 0..n {
        let (kind, settings) = if i % 2 == 0 {
            (types::AUDIO_INPUT_SOURCE, Settings::new())
        } else {
            (types::COLOR_SOURCE, Settings::new())
        };
        let (id, _) = col
            .add_source(
                &format!("Source {i}"),
                NodeTypeId::new_static(kind),
                settings,
            )
            .unwrap();
        col.add_item(
            scene,
            ItemContent::Source(id),
            Layout::new([0.0, 0.0], [0.1, 0.1]),
            None,
        )
        .unwrap();
    }
    col.set_program(canvas, Some(scene)).unwrap();
    col
}

pub(super) fn host(n: usize) -> Option<Host> {
    host_with_clock(n, Arc::new(TestClock::new()))
}

pub(super) fn host_with_clock(n: usize, clock: Arc<TestClock>) -> Option<Host> {
    host_with_engine(n, move || {
        Engine::with_clock(
            Arc::clone(&clock) as Arc<dyn Clock>,
            EngineConfig::default().swap_timeout(Duration::from_secs(20)),
        )
    })
}

pub(super) fn host_with_engine(n: usize, engine: impl Fn() -> Engine + 'static) -> Option<Host> {
    host_with_document(n, |_| {}, engine)
}

pub(super) fn host_with_document(
    n: usize,
    configure: impl FnOnce(&mut SceneCollection),
    engine: impl Fn() -> Engine + 'static,
) -> Option<Host> {
    let gpu = gpu()?;
    let mut config = HostConfig::canvas(Size::new(1280, 720), FrameRate::FPS_30);
    config.preview = None;
    let mut document = collection(n);
    configure(&mut document);
    config.collection = Some(document);
    let (host, _) = start(
        config,
        || {
            let mut a = RecordAdapters::new();
            a.sources
                .register_capture(Arc::new(TestCaptureFactory::new()))
                .unwrap();
            a.sources
                .register_audio(Arc::new(TestAudioBackend::new()))
                .unwrap();
            (a, Vec::new())
        },
        move || Ok(gpu),
        engine,
        crate::levels::LevelFeed::new(),
    )
    .unwrap();
    Some(host)
}

/// Prints the allocations and time of one unchanged `publish`, for 10 and 50 sources.
#[test]
#[ignore = "slow: measurement for F-10, prints numbers"]
fn slow_publish_cost_when_nothing_changed() {
    for n in [10, 50] {
        let Some(mut host) = host(n) else { return };
        let quiet = |_: Update| {};
        host.publish(&quiet);
        host.publish(&quiet);
        const ROUNDS: u32 = 2000;
        let before = allocations();
        let started = Instant::now();
        for _ in 0..ROUNDS {
            host.publish(&quiet);
        }
        let took = started.elapsed();
        let made = allocations() - before;
        rbs_crash::errln!(
            "F-10 publish, {n} sources: {:.2} allocations and {:.1} µs per round",
            made as f64 / f64::from(ROUNDS),
            took.as_secs_f64() * 1e6 / f64::from(ROUNDS)
        );
        // A fader step: one volume edit, then the round that publishes it.
        let source = host.collection.sources().next().map(|s| s.id).unwrap();
        const STEPS: u32 = 200;
        let before = allocations();
        let started = Instant::now();
        for i in 0..STEPS {
            let volume = f64::from(i % 100) as f32 / 100.0;
            host.collection.set_source_volume(source, volume).unwrap();
            host.publish(&quiet);
        }
        let took = started.elapsed();
        rbs_crash::errln!(
            "F-10 fader step, {n} sources: {:.0} allocations and {:.1} µs per step",
            (allocations() - before) as f64 / f64::from(STEPS),
            took.as_secs_f64() * 1e6 / f64::from(STEPS)
        );
        // A master fader step.
        let canvas = host.canvas;
        let before = allocations();
        let started = Instant::now();
        for i in 0..STEPS {
            let volume = f64::from(i % 100) as f32 / 100.0;
            host.collection
                .set_canvas_master_volume(canvas, volume)
                .unwrap();
            host.publish(&quiet);
        }
        let took = started.elapsed();
        rbs_crash::errln!(
            "F-10 master step, {n} sources: {:.0} allocations and {:.1} µs per step",
            (allocations() - before) as f64 / f64::from(STEPS),
            took.as_secs_f64() * 1e6 / f64::from(STEPS)
        );
    }
}

/// Every update a call of `publish` sends, by its variant name.
fn sent(host: &mut Host) -> Vec<String> {
    let seen = std::cell::RefCell::new(Vec::new());
    host.publish(&|u: Update| {
        let text = format!("{u:?}");
        let name = text
            .split(['(', ' ', '{'])
            .next()
            .unwrap_or_default()
            .to_owned();
        seen.borrow_mut().push(name);
    });
    seen.into_inner()
}

/// F-10: a round in which nothing changed sends nothing and builds no row, and
/// allocates nothing on the control thread.
#[test]
fn an_unchanged_round_sends_nothing_and_allocates_nothing() {
    let Some(mut host) = host(10) else { return };
    let first = sent(&mut host);
    assert!(first.iter().any(|u| u == "Sources"), "{first:?}");
    // The first rounds may still settle (sizes, levels); then it is quiet.
    let _ = sent(&mut host);
    assert_eq!(sent(&mut host), Vec::<String>::new());
    let quiet = |_: Update| {};
    let before = allocations();
    for _ in 0..10 {
        host.publish(&quiet);
    }
    assert_eq!(allocations() - before, 0);
}

/// F-10: the time left on a running Show cue travels on its own; the rows are not
/// rebuilt for it.
#[test]
fn a_running_show_cue_sends_its_time_and_not_the_rows() {
    let Some(mut host) = host(2) else { return };
    let item = host
        .collection
        .scene(host.scene)
        .and_then(|s| s.items.first())
        .map(|i| i.id)
        .unwrap();
    host.collection.set_item_visible(item, false).unwrap();
    host.collection
        .set_item_show_for(item, Some(5_000))
        .unwrap();
    let _ = sent(&mut host);
    let _ = sent(&mut host);
    assert!(host.show_item(item).unwrap());
    let updates = sent(&mut host);
    assert!(updates.iter().any(|u| u == "Cues"), "{updates:?}");
    assert_eq!(host.cues_sent.len(), 1);
    assert_eq!(host.cues_sent[0].0, item);
    assert!(host.cues_sent[0].1 > 0 && host.cues_sent[0].1 <= 5_000);
    // The engine's clock does not move here: the next round has nothing new.
    assert_eq!(sent(&mut host), Vec::<String>::new());
}

/// F-10: a fader step or a mute sends the new audio alone, and the rows it patches
/// equal rows built from scratch. A sync offset still sends the rows: it changes the
/// device peers of other sources.
#[test]
fn a_fader_step_patches_the_rows_instead_of_sending_them() {
    let Some(mut host) = host(4) else { return };
    let _ = sent(&mut host);
    let _ = sent(&mut host);
    let mic = host
        .collection
        .sources()
        .find(|s| s.type_id.as_str() == types::AUDIO_INPUT_SOURCE)
        .map(|s| s.id)
        .unwrap();
    host.collection.set_source_volume(mic, 0.25).unwrap();
    assert_eq!(sent(&mut host), vec!["SourceAudio".to_owned()]);
    host.collection.set_source_muted(mic, true).unwrap();
    assert_eq!(sent(&mut host), vec!["SourceAudio".to_owned()]);
    assert_eq!(host.rows, host.rows());
    assert_eq!(host.heard, host.rows_of(host.scene));
    assert_eq!(host.globals_shown, host.global_rows());
    let row = host.heard.iter().find(|r| r.id == mic).unwrap();
    assert_eq!((row.audio.volume, row.audio.muted), (0.25, true));
    // The canvas master patches the scene list only.
    let canvas = host.canvas;
    host.collection
        .set_canvas_master_volume(canvas, 0.5)
        .unwrap();
    assert_eq!(sent(&mut host), vec!["Master".to_owned()]);
    assert_eq!(host.scenes_shown.as_ref(), Some(&host.scene_list()));
    host.collection.set_source_sync_offset(mic, 40).unwrap();
    let updates = sent(&mut host);
    assert!(updates.iter().any(|u| u == "Sources"), "{updates:?}");
    assert_eq!(host.rows, host.rows());
}
