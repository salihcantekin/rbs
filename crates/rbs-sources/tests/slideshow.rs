//! The slide show hub and its decode thread (ADR-0171 decisions 4, 6 and 8): settings,
//! first picture, prefetch, commands, loop end, shuffle and the thread's lifetime.
//! Waits are for conditions, bounded only as hang guards (ADR-0032).

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use rbs_graph::runtime::param::{ParamReceiver, channel};
use rbs_nodes::{SlideFrame, SlideshowShared};
use rbs_scene::model::{SettingValue, Settings, SourceId};
use rbs_sources::slideshow::{
    self, FIELDS, SlideCommand, SlideshowHub, check_settings, read_settings,
};

const HANG_GUARD: Duration = Duration::from_secs(10);

fn folder(name: &str, files: usize) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rbs-slideshow-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/plain-4x2.jpg");
    for i in 1..=files {
        std::fs::copy(&src, dir.join(format!("{i}.jpg"))).unwrap();
    }
    dir
}

fn settings(dir: &Path, pairs: &[(&str, SettingValue)]) -> Settings {
    let mut s = Settings::new();
    s.insert(
        slideshow::FOLDER.into(),
        SettingValue::Text(dir.display().to_string()),
    );
    for (k, v) in pairs {
        s.insert((*k).into(), v.clone());
    }
    s
}

struct Rig {
    hub: SlideshowHub,
    shared: Arc<SlideshowShared>,
    rx: ParamReceiver<Option<SlideFrame>>,
    id: SourceId,
}

impl Rig {
    fn new(s: &Settings) -> Self {
        let mut hub = SlideshowHub::new();
        let shared = Arc::new(SlideshowShared::default());
        let (tx, rx) = channel(None);
        let id = SourceId::from_raw(7);
        hub.attach(id, read_settings(s), (8, 4), Arc::clone(&shared), tx);
        shared.warm.store(true, Ordering::Release);
        hub.service();
        Self {
            hub,
            shared,
            rx,
            id,
        }
    }

    /// Waits for a frame with a serial above `after`, as the node's `update` would.
    fn next_frame(&mut self, after: u64) -> (u64, bool, bool) {
        let start = Instant::now();
        loop {
            self.rx.update();
            if let Some(f) = self.rx.get() {
                if f.serial > after {
                    return (f.serial, f.now, f.cut);
                }
            }
            assert!(start.elapsed() < HANG_GUARD, "no frame after {after}");
            std::thread::yield_now();
        }
    }

    /// Plays the node: shows `serial` and wakes the thread through a command-free path.
    fn show(&mut self, serial: u64) {
        self.shared.shown.store(serial, Ordering::Release);
        self.hub.nudge();
    }

    fn index(&self) -> Option<usize> {
        self.hub.state(self.id).unwrap().index
    }
}

#[test]
fn the_schema_is_valid_and_settings_read_leniently() {
    rbs_params::check_fields(FIELDS, rbs_params::MAX_FIELDS).unwrap();
    let dir = PathBuf::from("/nowhere");
    let s = read_settings(&settings(&dir, &[]));
    assert_eq!(s.interval_ms, Some(5000));
    assert_eq!(s.transition, Some(rbs_gpu::SlideKind::Fade));
    assert_eq!(s.transition_ms, 700);
    assert!(s.looped && !s.shuffle && !s.watch);
    let manual = read_settings(&settings(
        &dir,
        &[("advance", SettingValue::Text("manual".into()))],
    ));
    assert_eq!(manual.interval_ms, None);
    // Out of range in a document: clamped. In an edit: refused.
    let far = settings(&dir, &[("interval_s", SettingValue::Float(99_999.0))]);
    assert_eq!(read_settings(&far).interval_ms, Some(3_600_000));
    assert!(check_settings(&far).is_err());
    assert!(
        check_settings(&settings(
            &dir,
            &[("fit", SettingValue::Text("zoom".into()))]
        ))
        .is_err()
    );
    assert!(check_settings(&settings(&dir, &[("width", SettingValue::Int(10))])).is_ok());
}

#[test]
fn no_thread_until_a_node_is_warm_and_none_after() {
    let dir = folder("life", 2);
    let mut hub = SlideshowHub::new();
    let shared = Arc::new(SlideshowShared::default());
    let (tx, _rx) = channel(None);
    hub.attach(
        SourceId::from_raw(1),
        read_settings(&settings(&dir, &[])),
        (8, 4),
        Arc::clone(&shared),
        tx,
    );
    hub.service();
    assert!(!hub.running(), "Standby: no thread");
    shared.warm.store(true, Ordering::Release);
    hub.service();
    assert!(hub.running());
    shared.warm.store(false, Ordering::Release);
    let start = Instant::now();
    while hub.running() {
        hub.nudge();
        hub.service();
        assert!(start.elapsed() < HANG_GUARD);
        std::thread::yield_now();
    }
}

#[test]
fn the_first_picture_is_a_cut_and_the_next_is_prefetched() {
    let dir = folder("first", 3);
    let mut rig = Rig::new(&settings(&dir, &[]));
    let (first, now, cut) = rig.next_frame(0);
    assert!(now && cut);
    rig.show(first);
    let (second, now, cut) = rig.next_frame(first);
    assert!(!now && !cut, "prefetched, waits for the node's timer");
    rig.show(second);
    assert_eq!(rig.hub.state(rig.id).unwrap().count, 3);
    let third = rig.next_frame(second).0;
    rig.show(third);
    assert_eq!(rig.index(), Some(2));
}

#[test]
fn next_and_previous_show_at_once() {
    let dir = folder("cmd", 3);
    let manual = [("advance", SettingValue::Text("manual".into()))];
    let mut rig = Rig::new(&settings(&dir, &manual));
    let first = rig.next_frame(0).0;
    rig.show(first);
    let pre = rig.next_frame(first).0;
    assert!(rig.hub.command(rig.id, SlideCommand::Previous));
    let (prev, now, cut) = rig.next_frame(pre);
    assert!(now && !cut);
    rig.show(prev);
    // Looping: before the first is the last.
    let start = Instant::now();
    while rig.index() != Some(2) {
        assert!(start.elapsed() < HANG_GUARD);
        rig.hub.nudge();
        std::thread::yield_now();
    }
    assert!(rig.hub.command(rig.id, SlideCommand::Restart));
    let restart = rig.next_frame(prev).0;
    rig.show(restart);
    assert_eq!(rig.index(), Some(0));
    assert!(!rig.hub.command(SourceId::from_raw(99), SlideCommand::Next));
}

#[test]
fn without_loop_the_last_picture_stays() {
    let dir = folder("end", 2);
    let mut rig = Rig::new(&settings(&dir, &[("loop", SettingValue::Bool(false))]));
    let a = rig.next_frame(0).0;
    rig.show(a);
    let b = rig.next_frame(a).0;
    rig.show(b);
    let start = Instant::now();
    while !rig.hub.state(rig.id).unwrap().ended {
        assert!(start.elapsed() < HANG_GUARD);
        rig.hub.command(rig.id, SlideCommand::Next);
        rig.hub.nudge();
        std::thread::yield_now();
    }
    rig.rx.update();
    assert_eq!(rig.rx.get().as_ref().unwrap().serial, b);
}

#[test]
fn a_missing_folder_is_reported_and_sends_nothing() {
    let mut rig = Rig::new(&settings(Path::new("/nowhere/at/all"), &[]));
    let start = Instant::now();
    while !rig.hub.state(rig.id).unwrap().folder_error {
        assert!(start.elapsed() < HANG_GUARD);
        rig.hub.nudge();
        std::thread::yield_now();
    }
    rig.rx.update();
    assert!(rig.rx.get().is_none());
}

#[test]
fn a_folder_of_broken_files_stops_instead_of_spinning() {
    let dir = folder("broken", 0);
    for name in ["a.jpg", "b.png"] {
        std::fs::write(dir.join(name), b"not a picture").unwrap();
    }
    let mut rig = Rig::new(&settings(&dir, &[]));
    let start = Instant::now();
    while rig.hub.state(rig.id).unwrap().count != 2 {
        assert!(start.elapsed() < HANG_GUARD);
        rig.hub.nudge();
        std::thread::yield_now();
    }
    // Every file fails once, then the thread rests: nothing was sent.
    for _ in 0..50 {
        rig.hub.nudge();
        std::thread::yield_now();
    }
    rig.rx.update();
    assert!(rig.rx.get().is_none());
}

#[test]
fn the_type_is_registered_with_its_keys_and_activity() {
    use rbs_graph::node::NodeTypeId;
    use rbs_scene::activity::{OnReturn, Run};
    use rbs_sources::types::{SLIDESHOW_SOURCE, SourceClass, class_of};
    let t = NodeTypeId::new(SLIDESHOW_SOURCE);
    assert_eq!(class_of(&t), Some(SourceClass::Slideshow));
    let types = rbs_sources::SourceTypes::new();
    assert!(types.is_installed(&t));
    assert!(types.installed().contains(&t));
    let a = rbs_sources::activity_traits(&t);
    assert_eq!(a.default_run, Run::Shown);
    assert_eq!(a.default_on_return, Some(OnReturn::Resume));
    assert!(!rbs_sources::is_still(&t, &Settings::new()));
    let dir = PathBuf::from("/x");
    assert!(rbs_sources::validate_settings(&t, &settings(&dir, &[])).is_ok());
    let mut bad = settings(&dir, &[]);
    bad.insert("url".into(), SettingValue::Text("x".into()));
    assert!(rbs_sources::validate_settings(&t, &bad).is_err());
    let mut half = settings(&dir, &[]);
    half.insert("width".into(), SettingValue::Int(100));
    assert!(rbs_sources::validate_settings(&t, &half).is_err());
    assert_eq!(
        slideshow::output_size(&Settings::new(), Some((1280, 720))),
        (1280, 720)
    );
    assert_eq!(slideshow::output_size(&Settings::new(), None), (1920, 1080));
    let source = slideshow::instantiate(&settings(&dir, &[]), Some((640, 360)));
    assert_eq!(source.size, (640, 360));
    assert_eq!(source.shared.interval_ms.load(Ordering::Relaxed), 5000);
}
